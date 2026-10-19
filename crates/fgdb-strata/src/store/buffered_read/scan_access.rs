//! The buffered store's access seam for ordered edge scans. Refaults still use
//! the admitted extent identity/checksum and the ordinary canonical decoders.

use super::*;
use crate::tiered::edge_scan::{
    BufferedEdgeDirection, BufferedEdgeScan, BufferedEdgeScanEvent, EdgeScanWork,
};

pub(crate) use super::block_directory::Candidates as BufferedIncidenceBlocks;

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
        BufferedEdgeScan::new(self, cx, as_of, blocks, maximum)
    }

    pub(crate) fn edge_scan_first(&self, at: usize) -> Option<(EId, CommitSeq)> {
        self.blocks[at].summary.map(|summary| summary.first_edge)
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
        let bytes = self.pin(cx, descriptor.block, admission).await?;
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
        let bytes = self.pin(cx, key, admission).await?;
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

    pub(super) async fn block_property_row(
        &mut self,
        cx: &QueryCx,
        at: usize,
        locator: u8,
        admission: Admission,
    ) -> Result<EdgePropertyRow, BufferedReadError> {
        let row = usize::from(locator)
            .checked_sub(1)
            .ok_or(BufferError::InvalidLoad)?;
        self.block_property_patch(cx, at, admission)
            .await?
            .get(row)
            .cloned()
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
        work: &mut EdgeScanWork,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<BufferedValue<BufferedBlockImage>, BufferedScanError<C>> {
        let descriptor = self.blocks[at];
        work.step(cx, observe)?;
        for _ in 0..descriptor.rows {
            work.step(cx, observe)?;
        }
        let charge = self.reserve_scan_workspace(cx)?;
        let image = self
            .block_controlled(cx, at, Admission::ScanBypass, &mut || {
                work.step(cx, observe)
            })
            .await?;
        cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
        Ok(BufferedValue::from_reserved(image, charge))
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
        let mut result: Option<BufferedValue<VertexRow>> = None;
        let mut winner_at = 0;
        let mut candidates = self.patch_directory.candidates(vid);
        while let Some(at) = self
            .patch_directory
            .next(&mut candidates, &mut || work.step(cx, observe))?
        {
            let descriptor = self.patches[at];
            if self.root.vertex_patches[at].first_seq > as_of || !descriptor.may_contain(vid) {
                continue;
            }
            // Charge the complete bounded decode before fetching a patch.
            work.step(cx, observe)?;
            for _ in 0..descriptor.rows {
                work.step(cx, observe)?;
            }
            let _workspace = self.reserve_scan_workspace(cx)?;
            let bytes = self
                .pin(cx, descriptor.extent, Admission::ScanBypass)
                .await?;
            let rows = crate::root::resolve_patch_ref(
                self.store.k_oid.expose(),
                self.store.namespace,
                at,
                &self.root.vertex_patches[at],
                bytes.as_ref(),
                self.store.decode_resolver(),
            )
            .map_err(StoreError::MalformedRoot)
            .map_err(BufferedReadError::from)?;
            if rows.len() != descriptor.rows {
                return Err(BufferedReadError::Buffer(BufferError::InvalidLoad).into());
            }
            for row in &rows {
                work.step(cx, observe)?;
                if row.vid == vid
                    && row.created_at <= as_of
                    && result
                        .as_ref()
                        .is_none_or(|old| (old.created_at, winner_at) <= (row.created_at, at))
                {
                    match result.as_mut() {
                        Some(old) => old.value = row.clone(),
                        None => {
                            let charge = self.reserve_scan_workspace(cx)?;
                            result = Some(BufferedValue::from_reserved(row.clone(), charge));
                        }
                    }
                    winner_at = at;
                }
            }
        }
        work.step(cx, observe)?;
        Ok(result.filter(|row| row.visible_at(as_of)))
    }
}
