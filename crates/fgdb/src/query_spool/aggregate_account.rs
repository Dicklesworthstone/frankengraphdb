//! Allocation-free structural admission for the existing canonical row codec.
//! Payload bytes cost bytes; only actual nodes cost GraphValue-sized slots.
use super::*;

const ROW: &[u8] = b"fgdb:graph-row:v1\0";
const VALUE: &[u8] = b"fgdb:graph-value:v1\0";

fn add(a: usize, b: usize) -> Result<usize> {
    a.checked_add(b)
        .ok_or_else(|| SpillError::SizeOverflow.into())
}
fn mul(a: usize, b: usize) -> Result<usize> {
    a.checked_mul(b)
        .ok_or_else(|| SpillError::SizeOverflow.into())
}

struct Frame<'a>(&'a [u8]);
impl<'a> Frame<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let (head, rest) = self.0.split_at_checked(len).ok_or(SpillError::InvalidRun)?;
        self.0 = rest;
        Ok(head)
    }
    fn count(&mut self) -> Result<usize> {
        let bytes: [u8; 8] = self.take(8)?.try_into().expect("checked length");
        usize::try_from(u64::from_be_bytes(bytes)).map_err(|_| SpillError::InvalidRun.into())
    }
    fn frame(&mut self) -> Result<Frame<'a>> {
        let len = self.count()?;
        Ok(Frame(self.take(len)?))
    }
    fn domain(&mut self, expected: &[u8]) -> Result<()> {
        if self.take(expected.len())? != expected {
            return invalid();
        }
        Ok(())
    }
    fn finish(self) -> Result<()> {
        if !self.0.is_empty() {
            return invalid();
        }
        Ok(())
    }
}

fn cell_memory(
    frame: &mut Frame<'_>,
    depth: usize,
    nodes: &mut usize,
    cx: &QueryCx,
) -> Result<usize> {
    cx.with_restriction(|| cx.checkpoint())
        .map_err(SpillError::Interrupted)?;
    if depth > GraphValue::MAX_LIST_DEPTH {
        return invalid();
    }
    let mut body = frame.frame()?;
    let tag = body.take(1)?[0];
    let mut bytes = size_of::<GraphValue>();
    match tag {
        0 => {
            let scalar = body.frame()?;
            bytes = add(bytes, scalar.0.len())?;
        }
        1 | 5 => {
            body.take(16)?;
        }
        2 => {
            body.take(16)?;
            let count = body.count()?;
            let len = mul(count, 32)?;
            body.take(len)?;
            bytes = add(bytes, len)?;
        }
        3 | 4 => {
            let count = body.count()?;
            let len = mul(count, 16)?;
            body.take(len)?;
            bytes = add(bytes, len)?;
        }
        6 | 7 => {
            let count = body.count()?;
            let minimum = if tag == 6 { 9 } else { 17 };
            if count > *nodes
                || count > body.0.len() / minimum
                || (count != 0 && depth == GraphValue::MAX_LIST_DEPTH)
            {
                return invalid();
            }
            *nodes -= count;
            if tag == 7 {
                bytes = add(bytes, mul(count, size_of::<Box<str>>())?)?;
            }
            for _ in 0..count {
                if tag == 7 {
                    let len = body.count()?;
                    body.take(len)?;
                    bytes = add(bytes, len)?;
                }
                bytes = add(bytes, cell_memory(&mut body, depth + 1, nodes, cx)?)?;
            }
        }
        _ => return invalid(),
    }
    body.finish()?;
    Ok(bytes)
}

/// Predecode reservation: typed slots and actual variable payloads, plus bounded
/// scalar decoder overlap (notably zoned timestamp's temporary identifier).
/// A second factor admits vector capacity/compaction overlap. Decoder semantic
/// checks, scalar artifacts and canonical map ordering remain authoritative.
pub(super) fn decoded(bytes: &[u8], cx: &QueryCx) -> Result<usize> {
    let mut row = Frame(bytes);
    row.domain(ROW)?;
    let count = row.count()?;
    if count > fgdb_gql::algebra::MAX_PATTERN_VERTICES
        || count > row.0.len() / (8 + VALUE.len() + 9)
    {
        return invalid();
    }
    let mut resident = size_of::<GraphValueRow>();
    for _ in 0..count {
        let mut frame = row.frame()?;
        frame.domain(VALUE)?;
        let mut nodes = GraphValue::MAX_LIST_NODES - 1;
        resident = add(resident, cell_memory(&mut frame, 0, &mut nodes, cx)?)?;
        frame.finish()?;
    }
    row.finish()?;
    add(mul(resident, 2)?, 256)
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
