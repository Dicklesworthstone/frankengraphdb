//! Shared allocation-free admission for the existing native row encoder.
//! The byte shape and overlap bound serve buffered sort input and aggregate
//! frames alike; GraphValueRow remains the only canonical encoder.

use super::*;
use core::mem::size_of;
use fgdb_gql::algebra::GraphValue;
use fgdb_strata::tiered::memory::MemoryPool;

type Result<T> = core::result::Result<T, NativeSpoolError>;
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
fn invalid<T>() -> Result<T> {
    Err(SpillError::InvalidRun.into())
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

/// Predecode reservation for native aggregate and unary stage rows. Typed
/// slots and actual variable payloads include bounded scalar-decoder overlap;
/// the second factor admits vector capacity and compaction overlap. This is
/// allocation-free structural admission, not a replacement scalar decoder.
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

fn body_shape(
    value: &GraphValue,
    depth: usize,
    remaining: &mut usize,
    cx: &QueryCx,
) -> Result<(usize, usize)> {
    cx.with_restriction(|| cx.checkpoint())
        .map_err(SpillError::Interrupted)?;
    if depth > GraphValue::MAX_LIST_DEPTH || *remaining == 0 {
        return invalid();
    }
    *remaining -= 1;
    let mut nodes = 1;
    let body = match value {
        GraphValue::Scalar(value) => add(
            9,
            value
                .canonical_encoded_len()
                .map_err(NativeSpoolError::Encode)?,
        )?,
        GraphValue::Vertex(_) | GraphValue::Edge(_) => 17,
        GraphValue::Path(path) => add(25, mul(path.steps().len(), 32)?)?,
        GraphValue::Vertices(values) => add(9, mul(values.len(), 16)?)?,
        GraphValue::Edges(values) => add(9, mul(values.len(), 16)?)?,
        GraphValue::List(values) => {
            let mut bytes = 9;
            if values.len() > *remaining {
                return invalid();
            }
            for value in values {
                let (body, children) = body_shape(value, depth + 1, remaining, cx)?;
                bytes = add(bytes, add(8, body)?)?;
                nodes = add(nodes, children)?;
            }
            bytes
        }
        GraphValue::Map { keys, values } => {
            if keys.len() != values.len() || values.len() > *remaining {
                return invalid();
            }
            let mut bytes = 9;
            for (key, value) in keys.iter().zip(values) {
                let (body, children) = body_shape(value, depth + 1, remaining, cx)?;
                bytes = add(bytes, add(add(16, key.len())?, body)?)?;
                nodes = add(nodes, children)?;
            }
            bytes
        }
    };
    Ok((body, nodes))
}

pub(super) fn encoded_shape(row: &GraphValueRow, cx: &QueryCx) -> Result<(usize, usize)> {
    if row.len() > fgdb_gql::algebra::MAX_PATTERN_VERTICES {
        return invalid();
    }
    let mut bytes = ROW.len() + 8;
    let mut nodes = 0;
    for value in row.values() {
        let mut remaining = GraphValue::MAX_LIST_NODES;
        let (body, count) = body_shape(value, 0, &mut remaining, cx)?;
        bytes = add(bytes, add(16 + VALUE.len(), body)?)?;
        nodes = add(nodes, count)?;
    }
    Ok((bytes, nodes))
}

/// Admit overlapping row/cell/scalar vectors and iterative traversal before
/// calling the native encoder. The returned reservation accompanies the bytes
/// through every awaited write. Both byte refusal and work refusal precede
/// allocation; the exact returned length must agree with the native shape.
pub(super) fn encode(
    pool: &MemoryPool,
    work: &mut sort::Work<'_>,
    row: &GraphValueRow,
    limit: usize,
) -> Result<(Vec<u8>, MemoryCharge)> {
    let (len, nodes) = encoded_shape(row, work.cx)?;
    if len > limit {
        return Err(NativeSpoolError::RowTooLarge { bytes: len, limit });
    }
    work.charge(add(len, nodes)?)?;
    let budget = add(mul(len, 8)?, mul(nodes, 128)?)?;
    let charge = pool.reserve(work.cx, budget).map_err(SpillError::Memory)?;
    let bytes = row.canonical_bytes().map_err(NativeSpoolError::Encode)?;
    if bytes.len() != len {
        return invalid();
    }
    Ok((bytes, charge))
}
