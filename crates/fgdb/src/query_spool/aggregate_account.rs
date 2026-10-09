//! Allocation-free structural admission for the existing canonical row codec.
//! Payload bytes cost bytes; only actual nodes cost GraphValue-sized slots.
use super::*;

/// Predecode reservation: typed slots and actual variable payloads, plus bounded
/// scalar decoder overlap (notably zoned timestamp's temporary identifier).
/// A second factor admits vector capacity/compaction overlap. Decoder semantic
/// checks, scalar artifacts and canonical map ordering remain authoritative.
pub(super) fn decoded(bytes: &[u8], cx: &QueryCx) -> Result<usize> {
    super::super::codec::decoded(bytes, cx).map_err(Into::into)
}

pub(super) use super::super::codec::encoded_shape;

/// The existing encoder can temporarily own row, cell and scalar byte vectors
/// together. Admit their geometric growth and iterative traversal stack before
/// calling it; the host checks the exact returned length against this preflight.
pub(super) fn encode(
    pool: &MemoryPool,
    work: &mut Work<'_>,
    row: &GraphValueRow,
    limit: usize,
) -> Result<(Vec<u8>, MemoryCharge)> {
    super::super::codec::encode(pool, work, row, limit).map_err(Into::into)
}
