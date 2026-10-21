//! Transfer already-admitted relation history with its existing input charges.
//! No root reopen, vertex payload scan, or uncharged source clone is needed.

use super::*;
use core::convert::Infallible;

/// Field order keeps the input allocations alive under their reservations.
/// Compaction/output workspace is separate from this buffered-source owner.
pub(crate) struct SealedSource {
    pub(crate) blocks: Vec<Vec<AdjacencyEntry>>,
    pub(crate) properties: Vec<Option<BlockProps>>,
    _charges: Vec<MemoryCharge>,
}

fn source_error(error: BufferedScanError<Infallible>) -> BufferedReadError {
    match error {
        BufferedScanError::Read(error) => error,
        BufferedScanError::Control(never) => match never {},
    }
}

impl<V: Vfs> BufferedPartition<V> {
    /// Preserve publication order: equal-version restatements must not acquire
    /// a different precedence merely because another relation was omitted.
    pub(crate) async fn sealed_relation_source(
        &mut self,
        cx: &QueryCx,
        floor: CommitSeq,
        relation: RelationId,
        max_incidences: usize,
    ) -> Result<SealedSource, BufferedReadError> {
        self.begin(cx, floor)?;
        let maximum = self.limits.max_work;
        let mut work = 0;
        let mut count = 0usize;
        let mut incidences = 0usize;
        for descriptor in &self.blocks {
            cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
            advance(&mut work, 1, maximum)?;
            if descriptor.summary.is_some_and(|summary| summary.relation == relation) {
                count = count.checked_add(1).ok_or(BufferedReadError::SizeOverflow)?;
                incidences = incidences
                    .checked_add(descriptor.rows)
                    .ok_or(BufferedReadError::SizeOverflow)?;
                limit("sealed source incidences", incidences, max_incidences)?;
            }
        }
        // Count and reserve the three owner vectors before any allocation or
        // payload fault. Decoded images keep their own per-block reservations.
        let bytes = count
            .checked_mul(
                size_of::<Vec<AdjacencyEntry>>()
                    + size_of::<Option<BlockProps>>()
                    + size_of::<MemoryCharge>(),
            )
            .and_then(|bytes| bytes.checked_add(size_of::<MemoryCharge>()))
            .ok_or(BufferedReadError::SizeOverflow)?;
        let charge = self.reserve_scan_bytes(cx, bytes)?;
        let mut source = SealedSource {
            blocks: Vec::new(),
            properties: Vec::new(),
            _charges: Vec::new(),
        };
        source.blocks.try_reserve_exact(count)
            .map_err(|_| MemoryError::AllocationFailed { requested: bytes })?;
        source.properties.try_reserve_exact(count)
            .map_err(|_| MemoryError::AllocationFailed { requested: bytes })?;
        let charge_count = count.checked_add(1).ok_or(BufferedReadError::SizeOverflow)?;
        source._charges.try_reserve_exact(charge_count)
            .map_err(|_| MemoryError::AllocationFailed { requested: bytes })?;
        let allocated = source.blocks.capacity()
            .checked_mul(size_of::<Vec<AdjacencyEntry>>())
            .and_then(|bytes| {
                source.properties.capacity()
                    .checked_mul(size_of::<Option<BlockProps>>())
                    .and_then(|extra| bytes.checked_add(extra))
            })
            .and_then(|bytes| {
                source._charges.capacity()
                    .checked_mul(size_of::<MemoryCharge>())
                    .and_then(|extra| bytes.checked_add(extra))
            })
            .ok_or(BufferedReadError::SizeOverflow)?;
        limit("sealed source owner bytes", allocated, bytes)?;
        source._charges.push(charge);

        for at in 0..self.blocks.len() {
            cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
            advance(&mut work, 1, maximum)?;
            let descriptor = self.blocks[at];
            if !descriptor.summary.is_some_and(|summary| summary.relation == relation) {
                continue;
            }
            advance(&mut work, descriptor.rows, maximum)?;
            let charge = self.reserve_scan_workspace(cx)?;
            let mut observe = || -> Result<(), BufferedScanError<Infallible>> {
                cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
                advance(&mut work, 1, maximum)?;
                Ok(())
            };
            let image = self
                .block_controlled(cx, at, Admission::ScanBypass, &mut observe)
                .await
                .map_err(source_error)?;
            let BufferedValue { value: (entries, properties), _charge } = retained::block(
                BufferedValue::from_reserved(image, charge),
                &mut observe,
            ).map_err(source_error)?;
            source.blocks.push(entries);
            source.properties.push(properties);
            source._charges.push(_charge);
        }
        cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
        Ok(source)
    }
}
