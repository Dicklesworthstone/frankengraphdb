//! Shared allocation-free admission for the existing native row encoder.
//! The byte shape and overlap bound serve buffered sort input and aggregate
//! frames alike; GraphValueRow remains the only canonical encoder.

use super::*;
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
