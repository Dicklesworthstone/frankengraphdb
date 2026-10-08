//! The buffered store's access seam for ordered edge scans. Refaults still use
//! the admitted extent identity/checksum and the ordinary canonical decoders.

use super::*;
use crate::tiered::edge_scan::{BufferedEdgeScan, BufferedEdgeScanEvent, EdgeScanWork};

pub(crate) type BufferedBlockImage = (Vec<AdjacencyEntry>, Option<BlockProps>);

impl<V: Vfs> BufferedPartition<V> {
    /// Open a pull-driven scan in stable EId order at one retained cut. Only
    /// the head capacity is reserved here; no payload is read before demand.
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
        self.blocks[at].first_edge
    }

    pub(crate) fn reserve_scan_bytes(
        &self,
        cx: &QueryCx,
        bytes: usize,
    ) -> Result<MemoryCharge, BufferedReadError> {
        Ok(self.buffer.reserve_scratch(cx, bytes)?)
    }

    pub(crate) fn reserve_scan_workspace(
        &self,
        cx: &QueryCx,
    ) -> Result<MemoryCharge, BufferedReadError> {
        self.reserve_scan_bytes(cx, OBJECT_WORKSPACE_BYTES)
    }

    // One refault decoder for point/adjacency reads and streaming reads.
    // The caller admits block I/O, decoded rows and the workspace. The hosted
    // property's additional visit is admitted HERE, immediately before its pin.
    pub(super) async fn block_controlled<C: Send>(
        &mut self,
        cx: &QueryCx,
        at: usize,
        admission: Admission,
        observe: &mut (impl FnMut() -> Result<(), BufferedScanError<C>> + Send),
    ) -> Result<BufferedBlockImage, BufferedScanError<C>> {
        let descriptor = self.blocks[at];
        let bytes = self.pin(cx, descriptor.block, admission).await?;
        let (entries, patch) = crate::decode_block_with_properties(bytes.as_ref())
            .map_err(StoreError::Malformed)
            .map_err(BufferedReadError::from)?;
        drop(bytes);
        let properties = match (patch, descriptor.properties) {
            (Some((_, locators)), Some(key)) => {
                observe()?;
                let bytes = self.pin(cx, key, admission).await?;
                let rows = crate::edge_props::read_property_patch_inner(
                    self.store.k_oid.expose(),
                    self.store.namespace,
                    bytes.as_ref(),
                    crate::edge_props::EdgePropertyPatchVersion(key.object()),
                    self.store.decode_resolver(),
                )
                .map_err(StoreError::MalformedEdgePropertyPatch)
                .map_err(BufferedReadError::from)?;
                Some(BlockProps { locators, rows })
            }
            (None, None) => None,
            _ => return Err(BufferedReadError::Buffer(BufferError::InvalidLoad).into()),
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
        let image = self.block_controlled(
            cx, at, Admission::ScanBypass, &mut || work.step(cx, observe),
        ).await?;
        if image.0.len() != descriptor.rows
            || image.0.iter().map(|row| (row.eid, row.created_at)).min() != descriptor.first_edge
        {
            return Err(BufferedReadError::Buffer(BufferError::InvalidLoad).into());
        }
        cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
        Ok(BufferedValue::from_reserved(image, charge))
    }

    // Endpoint resolution shares the edge cursor's allowance. Range metadata
    // excludes unrelated payloads, not versions of the requested identity.
    // This is still a descriptor scan, not an indexed vertex point-lookup claim.
    pub(crate) async fn edge_scan_vertex<C: Send>(
        &mut self,
        cx: &QueryCx,
        vid: VId,
        as_of: CommitSeq,
        work: &mut EdgeScanWork,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedValue<VertexRow>>, BufferedScanError<C>> {
        let mut result: Option<BufferedValue<VertexRow>> = None;
        for at in 0..self.patches.len() {
            work.step(cx, observe)?;
            let descriptor = self.patches[at];
            if self.root.vertex_patches[at].first_seq > as_of
                || descriptor.first.is_none_or(|(first, _)| vid < first)
                || descriptor.last.is_none_or(|last| vid > last)
            {
                continue;
            }
            // Charge the complete bounded decode before fetching a patch.
            work.step(cx, observe)?;
            for _ in 0..descriptor.rows {
                work.step(cx, observe)?;
            }
            let _workspace = self.reserve_scan_workspace(cx)?;
            let bytes = self.pin(cx, descriptor.extent, Admission::ScanBypass).await?;
            let rows = crate::root::resolve_patch_ref(
                self.store.k_oid.expose(), self.store.namespace, at,
                &self.root.vertex_patches[at], bytes.as_ref(), self.store.decode_resolver(),
            ).map_err(StoreError::MalformedRoot).map_err(BufferedReadError::from)?;
            if rows.len() != descriptor.rows {
                return Err(BufferedReadError::Buffer(BufferError::InvalidLoad).into());
            }
            for row in &rows {
                work.step(cx, observe)?;
                if row.vid == vid && row.created_at <= as_of
                    && result.as_ref().is_none_or(|old| old.created_at <= row.created_at)
                {
                    match result.as_mut() {
                        Some(old) => old.value = row.clone(),
                        None => {
                            let charge = self.reserve_scan_workspace(cx)?;
                            result = Some(BufferedValue::from_reserved(row.clone(), charge));
                        }
                    }
                }
            }
        }
        work.step(cx, observe)?;
        Ok(result.filter(|row| row.visible_at(as_of)))
    }
}
