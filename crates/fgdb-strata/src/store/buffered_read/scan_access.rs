//! The buffered store's access seam for ordered edge scans. Refaults still use
//! the admitted extent identity/checksum and the ordinary canonical decoders.

use super::*;
use crate::tiered::edge_scan::{
    BufferedEdgeDirection, BufferedEdgeScan, BufferedEdgeScanEvent, EdgePropertyAccess,
    EdgeScanWork,
};

pub(super) mod native_runs;
mod retained;
mod sealed;

pub(crate) use super::block_directory::Candidates as BufferedIncidenceBlocks;
use super::block_directory::RelationBlocks;
pub(crate) use super::block_directory::RootCandidates as BufferedRootBlocks;

pub(crate) type BufferedBlockImage = (Vec<AdjacencyEntry>, Option<BlockProps>);
type BufferedBlockKeys = (Vec<AdjacencyEntry>, Option<Vec<u8>>);

impl<V: Vfs> BufferedPartition<V> {
    /// Open a pull-driven scan in stable EId order at one retained cut. Root
    /// head capacity and payload reads are deferred until actual root demand.
    /// Initial authenticated admission supplied each block's minimum EId, not
    /// its first adjacency-ordered entry. The cursor never builds an edge table.
    pub fn edge_scan(
        &mut self,
        cx: &QueryCx,
        as_of: CommitSeq,
    ) -> Result<BufferedEdgeScan<'_, V>, BufferedReadError> {
        self.begin(cx, as_of)?;
        let blocks = self.blocks.len();
        let maximum = self.limits.max_work;
        BufferedEdgeScan::new(self, cx, as_of, blocks, maximum, None)
    }

    /// Scan one relation in stable EId order without faulting other relations.
    /// Initial root admission still authenticates every block and property
    /// patch and proves immutable endpoint/relation identity across histories.
    /// The admitted block summaries therefore select complete EId histories;
    /// filtering never substitutes for admission or the selected-row decoders.
    ///
    /// Opening allocates and reserves no root workspace. First demand seeks
    /// the relation's admitted descriptor range, reserves its exact head count
    /// through cache-aware admission, and seeds only matching blocks. Every
    /// seek comparison and selected descriptor consumes the ordinary work limit.
    /// Unrelated histories add logarithmic lookup work, never payload faults.
    pub fn edge_scan_relation(
        &mut self,
        cx: &QueryCx,
        as_of: CommitSeq,
        relation: RelationId,
    ) -> Result<BufferedEdgeScan<'_, V>, BufferedReadError> {
        self.begin(cx, as_of)?;
        let blocks = self.blocks.len();
        let maximum = self.limits.max_work;
        BufferedEdgeScan::new(self, cx, as_of, blocks, maximum, Some(relation))
    }

    pub(crate) fn edge_scan_first(
        &self,
        at: usize,
        relation: Option<RelationId>,
    ) -> Option<(EId, CommitSeq)> {
        self.blocks[at]
            .summary
            .filter(|summary| relation.is_none_or(|relation| summary.relation == relation))
            .map(|summary| summary.first_edge)
    }

    pub(crate) fn edge_scan_relation_blocks<C>(
        &self,
        relation: RelationId,
        cx: &QueryCx,
        work: &mut EdgeScanWork,
        observe: &mut impl FnMut(BufferedEdgeScanEvent) -> Result<(), C>,
    ) -> Result<RelationBlocks, BufferedScanError<C>> {
        self.block_directory
            .relation_blocks(relation, &mut || work.step(cx, observe))
    }

    pub(crate) fn edge_scan_next_relation_block<C>(
        &self,
        blocks: &mut RelationBlocks,
        cx: &QueryCx,
        work: &mut EdgeScanWork,
        observe: &mut impl FnMut(BufferedEdgeScanEvent) -> Result<(), C>,
    ) -> Result<Option<usize>, BufferedScanError<C>> {
        self.block_directory
            .next_relation(blocks, &mut || work.step(cx, observe))
    }

    pub(crate) fn edge_scan_incidence_blocks(
        &self,
        endpoint: VId,
        relation: Option<RelationId>,
        direction: BufferedEdgeDirection,
    ) -> BufferedIncidenceBlocks {
        self.block_directory
            .candidates(endpoint, relation, direction)
    }

    pub(crate) fn edge_scan_next_incidence_block<C>(
        &self,
        candidates: &mut BufferedIncidenceBlocks,
        cx: &QueryCx,
        work: &mut EdgeScanWork,
        observe: &mut impl FnMut(BufferedEdgeScanEvent) -> Result<(), C>,
    ) -> Result<Option<usize>, BufferedScanError<C>> {
        self.block_directory
            .next(candidates, &mut || work.step(cx, observe))
    }

    pub(crate) fn edge_scan_root_blocks<'a>(
        &self,
        vertices: &'a [VId],
        relation: Option<RelationId>,
        direction: BufferedEdgeDirection,
    ) -> BufferedRootBlocks<'a> {
        self.block_directory
            .root_candidates(vertices, relation, direction)
    }

    pub(crate) fn edge_scan_next_root_block<C>(
        &self,
        candidates: &mut BufferedRootBlocks<'_>,
        cx: &QueryCx,
        work: &mut EdgeScanWork,
        observe: &mut impl FnMut(BufferedEdgeScanEvent) -> Result<(), C>,
    ) -> Result<Option<usize>, BufferedScanError<C>> {
        self.block_directory
            .next_root(candidates, &mut || work.step(cx, observe))
    }

    pub(crate) fn reserve_scan_bytes(
        &mut self,
        cx: &QueryCx,
        bytes: usize,
    ) -> Result<MemoryCharge, BufferedReadError> {
        Ok(self.buffer.reserve_scratch(cx, bytes)?)
    }

    pub(crate) fn reserve_scan_workspace(
        &mut self,
        cx: &QueryCx,
    ) -> Result<MemoryCharge, BufferedReadError> {
        self.reserve_scan_bytes(cx, OBJECT_WORKSPACE_BYTES)
    }

    // Refault the admitted topology and its positional locators without
    // fetching unrelated property values. Initial root admission already proved
    // the block/patch bijection and joint logical digest; immutable extent
    // checksums preserve that proof on every later fault.
    pub(super) async fn block_keys(
        &mut self,
        cx: &QueryCx,
        at: usize,
        admission: Admission,
    ) -> Result<BufferedBlockKeys, BufferedReadError> {
        let descriptor = self.blocks[at];
        let bytes = self
            .pin(
                cx,
                crate::store::StoredObjectKind::Block,
                descriptor.block,
                admission,
            )
            .await?;
        let (entries, patch) = crate::decode_block_with_properties(bytes.as_ref())
            .map_err(StoreError::Malformed)
            .map_err(BufferedReadError::from)?;
        drop(bytes);
        if entries.len() != descriptor.rows
            || BlockSummary::from_entries(&entries) != descriptor.summary
        {
            return Err(BufferError::InvalidLoad.into());
        }
        let locators = match (patch, descriptor.properties) {
            (Some((id, locators)), Some(key)) if id == key.object() => Some(locators),
            (None, None) => None,
            _ => return Err(BufferError::InvalidLoad.into()),
        };
        Ok((entries, locators))
    }

    async fn block_property_patch(
        &mut self,
        cx: &QueryCx,
        at: usize,
        admission: Admission,
    ) -> Result<Vec<EdgePropertyRow>, BufferedReadError> {
        let key = self.blocks[at].properties.ok_or(BufferError::InvalidLoad)?;
        let bytes = self
            .pin(
                cx,
                crate::store::StoredObjectKind::EdgePropertyPatch,
                key,
                admission,
            )
            .await?;
        crate::edge_props::read_property_patch_inner(
            self.store.k_oid.expose(),
            self.store.namespace,
            bytes.as_ref(),
            crate::edge_props::EdgePropertyPatchVersion(key.object()),
            self.store.decode_resolver(),
        )
        .map_err(StoreError::MalformedEdgePropertyPatch)
        .map_err(BufferedReadError::from)
    }

    async fn block_property_columns(
        &mut self,
        cx: &QueryCx,
        at: usize,
        keys: PropertySelection,
        admission: Admission,
    ) -> Result<Vec<EdgePropertyRow>, BufferedReadError> {
        let key = self.blocks[at].properties.ok_or(BufferError::InvalidLoad)?;
        let bytes = self
            .pin(
                cx,
                crate::store::StoredObjectKind::EdgePropertyPatch,
                key,
                admission,
            )
            .await?;
        crate::edge_props::read_admitted_property_columns(
            self.store.k_oid.expose(),
            self.store.namespace,
            bytes.as_ref(),
            crate::edge_props::EdgePropertyPatchVersion(key.object()),
            self.store.decode_resolver(),
            keys,
        )
        .map_err(StoreError::MalformedEdgePropertyPatch)
        .map_err(BufferedReadError::from)
    }

    pub(super) async fn block_property_row(
        &mut self,
        cx: &QueryCx,
        at: usize,
        locator: u8,
        keys: Option<PropertySelection>,
        admission: Admission,
    ) -> Result<EdgePropertyRow, BufferedReadError> {
        let row = usize::from(locator)
            .checked_sub(1)
            .ok_or(BufferError::InvalidLoad)?;
        let key = self.blocks[at].properties.ok_or(BufferError::InvalidLoad)?;
        let bytes = self
            .pin(
                cx,
                crate::store::StoredObjectKind::EdgePropertyPatch,
                key,
                admission,
            )
            .await?;
        crate::edge_props::read_admitted_property_row(
            self.store.k_oid.expose(),
            self.store.namespace,
            bytes.as_ref(),
            crate::edge_props::EdgePropertyPatchVersion(key.object()),
            self.store.decode_resolver(),
            row,
            keys,
        )
        .map_err(StoreError::MalformedEdgePropertyPatch)
        .map_err(BufferedReadError::from)?
        .ok_or_else(|| BufferError::InvalidLoad.into())
    }

    pub(super) async fn block_property_value(
        &mut self,
        cx: &QueryCx,
        at: usize,
        locator: u8,
        property: PropertyKeyId,
        admission: Admission,
    ) -> Result<Option<CanonicalPropertyValue>, BufferedReadError> {
        let row = usize::from(locator)
            .checked_sub(1)
            .ok_or(BufferError::InvalidLoad)?;
        let key = self.blocks[at].properties.ok_or(BufferError::InvalidLoad)?;
        let bytes = self
            .pin(
                cx,
                crate::store::StoredObjectKind::EdgePropertyPatch,
                key,
                admission,
            )
            .await?;
        crate::edge_props::read_admitted_property_value(
            self.store.k_oid.expose(),
            self.store.namespace,
            bytes.as_ref(),
            crate::edge_props::EdgePropertyPatchVersion(key.object()),
            self.store.decode_resolver(),
            row,
            property,
        )
        .map_err(StoreError::MalformedEdgePropertyPatch)
        .map_err(BufferedReadError::from)?
        .ok_or_else(|| BufferError::InvalidLoad.into())
    }

    // Streaming callers still receive the complete native block image. Both
    // lanes share the exact same topology/property decoders and reservations.
    // The hosted property's additional visit is admitted before its pin.
    pub(super) async fn block_controlled<C: Send>(
        &mut self,
        cx: &QueryCx,
        at: usize,
        admission: Admission,
        observe: &mut (impl FnMut() -> Result<(), BufferedScanError<C>> + Send),
    ) -> Result<BufferedBlockImage, BufferedScanError<C>> {
        let (entries, locators) = self.block_keys(cx, at, admission).await?;
        let properties = if let Some(locators) = locators {
            observe()?;
            let rows = self.block_property_patch(cx, at, admission).await?;
            crate::edge_props::validate_block_patch_consistency(&locators, rows.len())
                .map_err(StoreError::MalformedEdgePropertyPatch)
                .map_err(BufferedReadError::from)?;
            Some(BlockProps { locators, rows })
        } else {
            None
        };
        Ok((entries, properties))
    }

    pub(crate) async fn edge_scan_block<C: Send>(
        &mut self,
        cx: &QueryCx,
        at: usize,
        properties: EdgePropertyAccess,
        work: &mut EdgeScanWork,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<BufferedValue<BufferedBlockImage>, BufferedScanError<C>> {
        let descriptor = self.blocks[at];
        work.step(cx, observe)?;
        for _ in 0..descriptor.rows {
            work.step(cx, observe)?;
        }
        let charge = self.reserve_scan_workspace(cx)?;
        let image = match properties {
            EdgePropertyAccess::Read => {
                self.block_controlled(cx, at, Admission::ScanBypass, &mut || {
                    work.step(cx, observe)
                })
                .await?
            }
            EdgePropertyAccess::Properties(keys) => {
                let (entries, locators) = self.block_keys(cx, at, Admission::ScanBypass).await?;
                let properties = if let Some(locators) = locators {
                    // The same sidecar visit is admitted before its pin.
                    work.step(cx, observe)?;
                    let rows = self
                        .block_property_columns(cx, at, keys, Admission::ScanBypass)
                        .await?;
                    crate::edge_props::validate_block_patch_consistency(&locators, rows.len())
                        .map_err(StoreError::MalformedEdgePropertyPatch)
                        .map_err(BufferedReadError::from)?;
                    Some(BlockProps { locators, rows })
                } else {
                    None
                };
                (entries, properties)
            }
            EdgePropertyAccess::Skip => {
                // Admission proved the joint block/property digest. Refault the
                // authenticated topology and verify its locator identity, then
                // drop those locators without fetching any property sidecar.
                let (entries, _) = self.block_keys(cx, at, Admission::ScanBypass).await?;
                (entries, None)
            }
        };
        cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
        // Decoder temporaries are gone. Retain the immutable block image's
        // ownership, not a full decoder workspace throughout the traversal.
        retained::block(BufferedValue::from_reserved(image, charge), &mut || {
            work.step(cx, observe)
        })
    }

    // Resolve an explicitly requested EId without constructing an all-edge
    // merge. The winner helper is shared with ordinary point reads; its
    // original topology ordinal remains the sole selected-payload locator.
    pub(crate) async fn edge_scan_point_topology<C: Send>(
        &mut self,
        cx: &QueryCx,
        eid: EId,
        as_of: CommitSeq,
        work: &mut EdgeScanWork,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<(BufferedValue<BufferedEdge>, usize, usize)>, BufferedScanError<C>> {
        let winner = self
            .edge_point_winner_with(cx, eid, as_of, Admission::ScanBypass, &mut |event| {
                work.step(cx, observe)?;
                if let PointReadEvent::Decode(rows) = event {
                    for _ in 0..rows {
                        work.step(cx, observe)?;
                    }
                }
                Ok::<_, BufferedScanError<C>>(())
            })
            .await?;
        let Some(winner) = winner else {
            return Ok(None);
        };
        let charge = self.reserve_scan_workspace(cx)?;
        let edge = BufferedValue::from_reserved(
            BufferedEdge {
                entry: winner.entry,
                props: Vec::new(),
            },
            charge,
        )
        .retain_scan_edge(&mut || work.step(cx, observe))?;
        Ok(Some((edge, winner.block, winner.original)))
    }

    /// Materialize only one already admitted winning row. Its original
    /// topology ordinal selects the authenticated sidecar locator; EId merge
    /// order is never reused as a property-row position. Hidden sibling rows
    /// remain framed and authenticated without materializing their values.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn edge_scan_selected_row<C: Send>(
        &mut self,
        cx: &QueryCx,
        at: usize,
        original: usize,
        expected: AdjacencyEntry,
        keys: PropertySelection,
        work: &mut EdgeScanWork,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<BufferedValue<BufferedEdge>, BufferedScanError<C>> {
        work.step(cx, observe)?;
        let descriptor = self
            .blocks
            .get(at)
            .ok_or(BufferedReadError::Buffer(BufferError::InvalidLoad))?;
        if original >= descriptor.rows {
            return Err(BufferedReadError::from(BufferError::InvalidLoad).into());
        }
        for _ in 0..descriptor.rows {
            work.step(cx, observe)?;
        }
        let charge = self.reserve_scan_workspace(cx)?;
        let (entries, locators) = self.block_keys(cx, at, Admission::ScanBypass).await?;
        let entry = *entries
            .get(original)
            .ok_or(BufferedReadError::Buffer(BufferError::InvalidLoad))?;
        if entry != expected {
            return Err(BufferedReadError::from(BufferError::InvalidLoad).into());
        }
        let locator = locators
            .as_ref()
            .map(|locators| {
                locators
                    .get(original)
                    .copied()
                    .ok_or(BufferedReadError::Buffer(BufferError::InvalidLoad))
            })
            .transpose()?
            .unwrap_or(0);
        drop(entries);
        drop(locators);
        let props = if keys.is_empty() || locator == 0 {
            Vec::new()
        } else {
            work.step(cx, observe)?;
            self.block_property_row(cx, at, locator, Some(keys), Admission::ScanBypass)
                .await?
        };
        cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
        BufferedValue::from_reserved(BufferedEdge { entry, props }, charge)
            .retain_scan_edge(&mut || work.step(cx, observe))
    }

    // Endpoint resolution shares the edge cursor's allowance. Its immutable
    // interval directory prunes unrelated metadata and payloads, while every
    // possibly matching history still enters the ordinary same-cut resolver.
    pub(crate) async fn edge_scan_vertex<C: Send>(
        &mut self,
        cx: &QueryCx,
        vid: VId,
        as_of: CommitSeq,
        work: &mut EdgeScanWork,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedValue<VertexRow>>, BufferedScanError<C>> {
        self.edge_scan_vertex_with_access(
            cx,
            vid,
            as_of,
            VertexPointAccess::Properties(None),
            work,
            observe,
        )
        .await
    }

    // This projection is available only after complete root admission. The
    // opaque owner cannot expose skipped properties as a complete VertexRow.
    pub(crate) async fn edge_scan_vertex_topology<C: Send>(
        &mut self,
        cx: &QueryCx,
        vid: VId,
        as_of: CommitSeq,
        work: &mut EdgeScanWork,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedVertexTopology>, BufferedScanError<C>> {
        self.edge_scan_vertex_with_access(
            cx,
            vid,
            as_of,
            VertexPointAccess::Topology,
            work,
            observe,
        )
        .await
        .map(|row| row.map(BufferedVertexTopology))
    }

    // Select history before decoding these keys. The partial owner preserves
    // both winner metadata and absent/NULL semantics without a complete map.
    pub(crate) async fn edge_scan_vertex_properties<C: Send>(
        &mut self,
        cx: &QueryCx,
        vid: VId,
        keys: PropertySelection,
        as_of: CommitSeq,
        work: &mut EdgeScanWork,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedVertexProperties>, BufferedScanError<C>> {
        let access = if keys.is_empty() {
            VertexPointAccess::Topology
        } else {
            VertexPointAccess::Properties(Some(keys))
        };
        self.edge_scan_vertex_with_access(cx, vid, as_of, access, work, observe)
            .await
            .map(|row| row.map(BufferedVertexProperties))
    }

    async fn edge_scan_vertex_with_access<C: Send>(
        &mut self,
        cx: &QueryCx,
        vid: VId,
        as_of: CommitSeq,
        access: VertexPointAccess,
        work: &mut EdgeScanWork,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedValue<VertexRow>>, BufferedScanError<C>> {
        let mut output_charge = None;
        let row = {
            let mut control = |event| {
                // Keep the source's existing directory/row/completion charges
                // and admit the complete bounded decode before every parse,
                // including a deferred winning row's materialization.
                work.step(cx, observe)?;
                if let PointReadEvent::Decode(rows) = event {
                    for _ in 0..rows {
                        work.step(cx, observe)?;
                    }
                }
                Ok::<_, BufferedScanError<C>>(())
            };
            let Some(winner) = self
                .vertex_point_winner_with(
                    cx,
                    VertexPointRead {
                        vid,
                        as_of,
                        access,
                        admission: Admission::ScanBypass,
                    },
                    Some(&mut output_charge),
                    &mut control,
                )
                .await?
            else {
                return Ok(None);
            };
            let key = match access {
                VertexPointAccess::Properties(key) => key,
                VertexPointAccess::Topology => None,
            };
            self.vertex_point_row_with(cx, winner, key, Admission::ScanBypass, &mut control)
                .await?
        };
        let charge = output_charge.ok_or(BufferedReadError::Buffer(BufferError::InvalidLoad))?;
        // The one reserved result workspace spans all historical candidates
        // and winning-value decoding; retain only its exact output ownership.
        BufferedValue::from_reserved(row, charge)
            .retain_scan_vertex(&mut || work.step(cx, observe))
            .map(Some)
    }
}
