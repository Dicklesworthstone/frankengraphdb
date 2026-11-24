//! Staged selected-field points sharing an existing cold source owner. A
//! point never advances or seeds the edge root; detaching the cursor retains
//! its cut, source identity, cumulative work and terminal lifecycle.

use super::*;
use crate::store::{BufferedVertexCandidate, BufferedVertexProperties};
use fgdb_delta_types::LabelId;
use fgdb_types::VId;

impl<V: Vfs> BufferedEdgeScan<'_, V> {
    /// Resolve an arbitrary vertex at this source's exact cut, admit its
    /// topology, then materialize only the selected keys. A carried identity
    /// is not evidence of visibility. A denied or absent vertex returns no
    /// row without decoding its property values.
    ///
    /// Points share this cursor's cumulative physical allowance and do not
    /// construct a root heap. Failure or dropping a polled pending read closes
    /// the complete cursor. Detaching/resuming preserves that state and budget.
    pub async fn vertex_admitted_properties_candidate_with<C: Send>(
        &mut self,
        cx: &QueryCx,
        vid: VId,
        keys: PropertySelection,
        admit: &mut (impl FnMut(VId, &[LabelId]) -> Result<bool, C> + Send),
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedVertexCandidate<BufferedVertexProperties>>, BufferedScanError<C>>
    {
        let Some(state) = self.state.take() else {
            return Ok(None);
        };
        let topology = self
            .partition
            .edge_scan_vertex_topology(cx, vid, self.as_of, &mut self.work, observe)
            .await?;
        let row = if let Some(topology) = topology {
            let admitted = admit(vid, topology.labels()).map_err(BufferedScanError::Control)?;
            cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
            if admitted {
                // Both lookups use the same immutable publication and cut.
                // The canonical resolver still selects the winner before its
                // requested values; a newer absent key cannot fall back.
                drop(topology);
                Some(
                    self.partition
                        .edge_scan_vertex_properties(
                            cx,
                            vid,
                            keys,
                            self.as_of,
                            &mut self.work,
                            observe,
                        )
                        .await?
                        .ok_or_else(invalid_load)?,
                )
            } else {
                None
            }
        } else {
            None
        };
        self.state = Some(state);
        Ok(Some(BufferedVertexCandidate { vid, row }))
    }

    /// Resolve an arbitrary edge's authoritative topology and both same-cut
    /// endpoint topologies before the caller admits any selected property.
    /// The requested EId is sought through the admitted interval directory,
    /// never by constructing a complete edge table or scanning a root prefix.
    ///
    /// Visibility callbacks precede all sidecar reads. A selected value uses
    /// the winning statement's original stored ordinal. Points share root work
    /// and retain the same close/error/pending-drop contract as vertex points.
    pub async fn edge_admitted_properties_candidate_with<C: Send>(
        &mut self,
        cx: &QueryCx,
        eid: EId,
        keys: PropertySelection,
        admit: &mut (impl FnMut(&BufferedEdgeTopologyEndpoints) -> Result<bool, C> + Send),
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedEdgeCandidate<BufferedEdgePropertiesEndpoints>>, BufferedScanError<C>>
    {
        let Some(state) = self.state.take() else {
            return Ok(None);
        };
        let selected = self
            .partition
            .edge_scan_point_topology(cx, eid, self.as_of, &mut self.work, observe)
            .await?;
        let row = if let Some((edge, block, original)) = selected {
            let expected = edge.entry;
            let (source, target) = self
                .resolve_topology_vertices(cx, &expected, observe)
                .await?;
            let topology = BufferedEdgeTopologyEndpoints {
                edge: BufferedEdgeTopology(edge),
                source,
                target,
            };
            let admitted = admit(&topology).map_err(BufferedScanError::Control)?;
            cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
            if admitted {
                let BufferedEdgeTopologyEndpoints {
                    edge,
                    source,
                    target,
                } = topology;
                let edge = if keys.is_empty() {
                    edge.0
                } else {
                    drop(edge);
                    self.partition
                        .edge_scan_selected_row(
                            cx,
                            block,
                            original,
                            expected,
                            keys,
                            &mut self.work,
                            observe,
                        )
                        .await?
                };
                Some(BufferedEdgePropertiesEndpoints {
                    edge: BufferedEdgeProperties(edge),
                    source,
                    target,
                    source_properties: None,
                    target_properties: None,
                })
            } else {
                None
            }
        } else {
            None
        };
        self.state = Some(state);
        Ok(Some(BufferedEdgeCandidate { eid, row }))
    }
}
