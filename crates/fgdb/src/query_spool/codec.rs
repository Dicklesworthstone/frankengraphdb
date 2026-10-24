//! Shared allocation-free admission for the existing native row encoder.
//! The byte shape and overlap bound serve buffered sort input and aggregate
//! frames alike; the native value and row encoders remain authoritative.

use super::*;
use core::mem::{size_of, size_of_val};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::spill_set::SpillSetRows;
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

// Count only owned heap storage here: the enclosing row/list allocation
// already accounts for each inline GraphValue. The bounded walk allocates no
// inspection workspace and uses physical work, not native evaluator events.
fn value_heap(
    value: &GraphValue,
    depth: usize,
    remaining: &mut usize,
    work: &mut sort::Work<'_>,
) -> Result<usize> {
    work.charge(1)?;
    if depth > GraphValue::MAX_LIST_DEPTH || *remaining == 0 {
        return invalid();
    }
    *remaining -= 1;
    match value {
        GraphValue::Scalar(value) => value
            .owned_heap_bytes()
            .ok_or_else(|| SpillError::SizeOverflow.into()),
        GraphValue::Vertex(_) | GraphValue::Edge(_) => Ok(0),
        GraphValue::Path(path) => Ok(size_of_val(path.steps())),
        GraphValue::Vertices(values) => Ok(size_of_val(values.as_ref())),
        GraphValue::Edges(values) => Ok(size_of_val(values.as_ref())),
        GraphValue::List(values) => {
            if values.len() > *remaining {
                return invalid();
            }
            let mut bytes = size_of_val(values.as_ref());
            for value in values {
                bytes = add(bytes, value_heap(value, depth + 1, remaining, work)?)?;
            }
            Ok(bytes)
        }
        GraphValue::Map { keys, values } => {
            if keys.len() != values.len() || values.len() > *remaining {
                return invalid();
            }
            let mut bytes = add(size_of_val(keys.as_ref()), size_of_val(values.as_ref()))?;
            for (key, value) in keys.iter().zip(values) {
                work.charge(1)?;
                bytes = add(bytes, key.len())?;
                bytes = add(bytes, value_heap(value, depth + 1, remaining, work)?)?;
            }
            Ok(bytes)
        }
    }
}

fn values_memory(
    values: &[GraphValue],
    capacity: usize,
    work: &mut sort::Work<'_>,
) -> Result<usize> {
    if values.len() > capacity {
        return invalid();
    }
    let mut bytes = mul(capacity, size_of::<GraphValue>())?;
    for value in values {
        let mut remaining = GraphValue::MAX_LIST_NODES;
        bytes = add(bytes, value_heap(value, 0, &mut remaining, work)?)?;
    }
    Ok(bytes)
}

/// Owned row storage after decoding has freed its temporary vectors.
pub(super) fn row_memory(row: &GraphValueRow, work: &mut sort::Work<'_>) -> Result<usize> {
    work.charge(1)?;
    add(
        size_of::<GraphValueRow>(),
        values_memory(row.values(), row.len(), work)?,
    )
}

/// The initial cursor footprint, measured only after start() has released its
/// temporary evaluation workspace and before the first element is moved out.
/// Keep this bound for the entire expansion: advancing IntoIter neither frees
/// its backing allocation nor relinquishes admission for a yielded payload.
pub(super) fn cursor_memory(cursor: &SpillSetRows, work: &mut sort::Work<'_>) -> Result<usize> {
    work.charge(1)?;
    let mut bytes = size_of::<SpillSetRows>();
    cursor.visit_retained_values(&mut |values, capacity| {
        bytes = add(bytes, values_memory(values, capacity, work)?)?;
        Ok::<_, NativeSpoolError>(())
    })?;
    Ok(bytes)
}

/// Reconcile completed-operation admission with the storage that survives it.
/// Splitting transfers ownership without reserving the same bytes twice; only
/// the now-unused guard is dropped. Unexpected allocator capacity is admitted
/// before any result can escape, as with the sorter's charged vectors.
pub(super) fn retain_reservation(
    charge: &mut MemoryCharge,
    bytes: usize,
    cx: &QueryCx,
) -> Result<()> {
    if bytes > charge.bytes() {
        charge
            .grow(cx, bytes - charge.bytes())
            .map_err(SpillError::Memory)?;
    } else {
        drop(
            charge
                .split_off(charge.bytes() - bytes)
                .ok_or(SpillError::SizeOverflow)?,
        );
    }
    Ok(())
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

fn encoded_value_shape(value: &GraphValue, cx: &QueryCx) -> Result<(usize, usize)> {
    let mut remaining = GraphValue::MAX_LIST_NODES;
    let (body, nodes) = body_shape(value, 0, &mut remaining, cx)?;
    Ok((add(8 + VALUE.len(), body)?, nodes))
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

/// Encode one borrowed grouping key without cloning it into a temporary row
/// or charging unrelated input columns. The same structural walker and codec
/// overlap bound apply as for a whole row. Only one key workspace is live at
/// a time; its returned capacity stays admitted until the caller drops it.
pub(super) fn encode_value(
    pool: &MemoryPool,
    work: &mut sort::Work<'_>,
    value: &GraphValue,
) -> Result<(Vec<u8>, MemoryCharge)> {
    let (len, nodes) = encoded_value_shape(value, work.cx)?;
    work.charge(add(len, nodes)?)?;
    let budget = add(mul(len, 8)?, mul(nodes, 128)?)?;
    let mut charge = pool.reserve(work.cx, budget).map_err(SpillError::Memory)?;
    let bytes = value.canonical_bytes().map_err(NativeSpoolError::Encode)?;
    if bytes.len() != len {
        return invalid();
    }
    retain_reservation(&mut charge, bytes.capacity(), work.cx)?;
    Ok((bytes, charge))
}

/// Admit overlapping row/cell/scalar vectors and iterative traversal before
/// calling the native encoder. After its temporary vectors have been freed,
/// retain the returned byte vector's actual capacity through every awaited
/// write. Both byte refusal and work refusal precede encoding; the exact
/// returned length must agree with the native shape.
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
    let mut charge = pool.reserve(work.cx, budget).map_err(SpillError::Memory)?;
    let bytes = row.canonical_bytes().map_err(NativeSpoolError::Encode)?;
    if bytes.len() != len {
        return invalid();
    }
    retain_reservation(&mut charge, bytes.capacity(), work.cx)?;
    Ok((bytes, charge))
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_strata::tiered::memory::MemoryError;
    use fgdb_types::{CanonicalBytes, CanonicalScalar, PurposeContexts, VId};

    #[test]
    fn individual_key_encodings_keep_native_bytes_and_admitted_capacity() {
        let ((), report) = run_async_under_lab(0xc0de_a003, |root| async move {
            let cx = PurposeContexts::narrow_runtime_root(&root).query();
            let values = [
                GraphValue::Scalar(CanonicalScalar::Null),
                GraphValue::Scalar(CanonicalScalar::Int(i64::MIN)),
                GraphValue::Scalar(CanonicalScalar::ucs_basic_text("λ\0雪").unwrap()),
                GraphValue::Scalar(CanonicalScalar::bytes(vec![0, 255, 42]).unwrap()),
                GraphValue::Vertex(VId(u128::MAX)),
                GraphValue::Edge(fgdb_types::EId(u128::MAX)),
                GraphValue::Vertices(vec![VId(1), VId(u128::MAX)].into_boxed_slice()),
                GraphValue::Edges(vec![fgdb_types::EId(7)].into_boxed_slice()),
                GraphValue::List(Vec::new().into_boxed_slice()),
                GraphValue::map(vec![(
                    "payload".into(),
                    GraphValue::List(
                        vec![
                            GraphValue::Scalar(CanonicalScalar::Int(7)),
                            GraphValue::Scalar(
                                CanonicalScalar::ucs_basic_text(&"x".repeat(8192)).unwrap(),
                            ),
                        ]
                        .into_boxed_slice(),
                    ),
                )])
                .unwrap(),
            ];
            for value in values {
                let expected = value.canonical_bytes().unwrap();
                let (len, nodes) = encoded_value_shape(&value, &cx).unwrap();
                assert_eq!(len, expected.len());
                let workspace = 8 * len + 128 * nodes;
                let mut work = sort::Work {
                    cx: &cx,
                    used: 0,
                    limit: u64::MAX,
                };
                let small = MemoryPool::new(workspace - 1, 0).unwrap();
                assert!(matches!(
                    encode_value(&small, &mut work, &value),
                    Err(NativeSpoolError::Spill(SpillError::Memory(
                        MemoryError::ResourceExhausted { requested, .. }
                    ))) if requested == workspace
                ));
                assert_eq!(small.used(), 0);
                let pool = MemoryPool::new(workspace, 0).unwrap();
                let encoded = encode_value(&pool, &mut work, &value).unwrap();
                assert_eq!(encoded.0, expected);
                assert_eq!(encoded.1.bytes(), encoded.0.capacity());
                assert_eq!(pool.used(), encoded.0.capacity());
                drop(encoded);
                assert_eq!(pool.used(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn individual_key_encoding_refuses_work_and_depth_before_retention() {
        let ((), report) = run_async_under_lab(0xc0de_a004, |root| async move {
            let cx = PurposeContexts::narrow_runtime_root(&root).query();
            let value = GraphValue::Scalar(CanonicalScalar::Int(7));
            let (len, nodes) = encoded_value_shape(&value, &cx).unwrap();
            let pool = MemoryPool::new(65_536, 0).unwrap();
            let mut work = sort::Work {
                cx: &cx,
                used: 0,
                limit: (len + nodes - 1) as u64,
            };
            assert!(matches!(
                encode_value(&pool, &mut work, &value),
                Err(NativeSpoolError::SortWorkLimit { attempted, .. })
                    if attempted == (len + nodes) as u64
            ));
            assert_eq!(pool.used(), 0);
            work.limit = u64::MAX;
            let mut too_deep = value;
            for _ in 0..=GraphValue::MAX_LIST_DEPTH {
                too_deep = GraphValue::List(vec![too_deep].into_boxed_slice());
            }
            assert!(matches!(
                encode_value(&pool, &mut work, &too_deep),
                Err(NativeSpoolError::Spill(SpillError::InvalidRun))
            ));
            assert_eq!(pool.used(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn encoder_releases_completed_workspace_but_keeps_output_capacity_admitted() {
        let ((), report) = run_async_under_lab(0xc0de_a001, |root| async move {
            let cx = PurposeContexts::narrow_runtime_root(&root).query();
            let row = GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
                CanonicalScalar::ucs_basic_text(&"x".repeat(8400)).unwrap(),
            )]);
            let expected = row.canonical_bytes().unwrap();
            let (len, nodes) = encoded_shape(&row, &cx).unwrap();
            let workspace = 8 * len + 128 * nodes;
            let mut work = sort::Work {
                cx: &cx,
                used: 0,
                limit: u64::MAX,
            };

            // Retaining fewer bytes must not weaken pre-encoding admission.
            let small = MemoryPool::new(workspace - 1, 0).unwrap();
            let error = encode(&small, &mut work, &row, len).unwrap_err();
            assert!(matches!(
                error,
                NativeSpoolError::Spill(SpillError::Memory(
                    MemoryError::ResourceExhausted { requested, .. }
                )) if requested == workspace
            ));
            assert_eq!(small.used(), 0);

            let pool = MemoryPool::new(workspace, 0).unwrap();
            let encoded = encode(&pool, &mut work, &row, len).unwrap();
            assert_eq!(encoded.0, expected);
            assert_eq!(encoded.1.bytes(), encoded.0.capacity());
            assert_eq!(pool.used(), encoded.0.capacity());
            assert!(pool.available() > len);
            // Another owner can use freed workspace while output is alive,
            // but cannot borrow even one byte of the output's reservation.
            let reused = pool.reserve(&cx, pool.available()).unwrap();
            assert!(matches!(
                pool.reserve(&cx, 1),
                Err(MemoryError::ResourceExhausted { .. })
            ));
            let retained = encoded.0.capacity();
            drop(encoded);
            assert_eq!(pool.available(), retained);
            drop(reused);
            assert_eq!(pool.used(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn retained_values_include_nested_capacity_and_charge_bounded_inspection_work() {
        let ((), report) = run_async_under_lab(0xc0de_a002, |root| async move {
            let cx = PurposeContexts::narrow_runtime_root(&root).query();
            let mut payload = Vec::with_capacity(8192);
            payload.extend_from_slice(b"abc");
            let capacity = payload.capacity();
            let value = GraphValue::map(vec![
                (
                    "data".into(),
                    GraphValue::List(
                        vec![
                            GraphValue::Scalar(CanonicalScalar::Bytes(
                                CanonicalBytes::new(payload).unwrap(),
                            )),
                            GraphValue::Scalar(CanonicalScalar::ucs_basic_text("text").unwrap()),
                        ]
                        .into_boxed_slice(),
                    ),
                ),
                (
                    "nodes".into(),
                    GraphValue::Vertices(vec![VId(1), VId(2)].into_boxed_slice()),
                ),
            ])
            .unwrap();
            let values = [value];
            let mut work = sort::Work {
                cx: &cx,
                used: 0,
                limit: 7,
            };
            let expected = 5 * size_of::<GraphValue>()
                + 2 * size_of::<Box<str>>()
                + "data".len()
                + "nodes".len()
                + capacity
                + "text".len()
                + 2 * size_of::<VId>();
            assert_eq!(values_memory(&values, 1, &mut work).unwrap(), expected);
            assert_eq!(work.used, 7);
            work.used = 0;
            work.limit = 6;
            assert!(matches!(
                values_memory(&values, 1, &mut work),
                Err(NativeSpoolError::SortWorkLimit {
                    attempted: 7,
                    limit: 6
                })
            ));
            work.used = 0;
            work.limit = u64::MAX;
            assert!(matches!(
                values_memory(&[], usize::MAX, &mut work),
                Err(NativeSpoolError::Spill(SpillError::SizeOverflow))
            ));
            let mut too_deep = GraphValue::Scalar(CanonicalScalar::Null);
            for _ in 0..=GraphValue::MAX_LIST_DEPTH {
                too_deep = GraphValue::List(vec![too_deep].into_boxed_slice());
            }
            assert!(matches!(
                values_memory(&[too_deep], 1, &mut work),
                Err(NativeSpoolError::Spill(SpillError::InvalidRun))
            ));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
