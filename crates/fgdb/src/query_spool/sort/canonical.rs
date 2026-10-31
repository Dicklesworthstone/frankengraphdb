//! Borrowed comparison of frames produced by GraphValueRow::canonical_bytes.
//! Only the private native-spool path calls this after page authentication.
//! Scalar bytes keep fgdb-types' memcomparable ordering; we neither decode
//! scalars nor admit arbitrary external scalar claims. This is NOT a public
//! deserializer/authentication boundary. Structural lengths are still checked.

use super::*;
use fgdb_gql::algebra::GraphValue;

pub(super) const ROW: &[u8] = b"fgdb:graph-row:v1\0";
const VALUE: &[u8] = b"fgdb:graph-value:v1\0";

fn invalid() -> NativeSpoolError {
    SpillError::InvalidRun.into()
}

fn take<'a>(bytes: &mut &'a [u8], len: usize) -> Result<&'a [u8]> {
    let value = bytes.get(..len).ok_or_else(invalid)?;
    *bytes = &bytes[len..];
    Ok(value)
}
fn count(bytes: &mut &[u8]) -> Result<usize> {
    let mut word = [0; 8];
    word.copy_from_slice(take(bytes, 8)?);
    usize::try_from(u64::from_be_bytes(word)).map_err(|_| invalid())
}
fn frame<'a>(bytes: &mut &'a [u8]) -> Result<&'a [u8]> {
    let len = count(bytes)?;
    take(bytes, len)
}
fn row(bytes: &[u8], columns: usize) -> Result<&[u8]> {
    let mut bytes = bytes.strip_prefix(ROW).ok_or_else(invalid)?;
    if count(&mut bytes)? != columns || columns > MAX_PATTERN_VERTICES {
        return Err(invalid());
    }
    Ok(bytes)
}
fn root<'a>(bytes: &mut &'a [u8]) -> Result<Cell<'a>> {
    let mut value = frame(bytes)?.strip_prefix(VALUE).ok_or_else(invalid)?;
    let cell = Cell::next(&mut value)?;
    if !value.is_empty() {
        return Err(invalid());
    }
    Ok(cell)
}

#[derive(Clone, Copy)]
struct Cell<'a> {
    tag: u8,
    body: &'a [u8],
}
impl<'a> Cell<'a> {
    fn next(bytes: &mut &'a [u8]) -> Result<Self> {
        let (tag, body) = frame(bytes)?.split_first().ok_or_else(invalid)?;
        if *tag > 7 {
            return Err(invalid());
        }
        Ok(Self { tag: *tag, body })
    }
    fn scalar(self) -> Result<&'a [u8]> {
        let mut body = self.body;
        let scalar = frame(&mut body)?;
        if !body.is_empty()
            || scalar.is_empty()
            // Scalar profile V2 adds temporal ranks after the property-value
            // List/Map tags (8/9). Keep those holes and unknown tags refused;
            // valid temporal payloads remain borrowed memcomparable bytes.
            || !matches!(scalar[0], 0x00..=0x07 | 0x0a..=0x0e)
            || (scalar[0] == 0 && scalar.len() != 1)
        {
            return Err(invalid());
        }
        Ok(scalar)
    }
    fn is_null(self) -> Result<bool> {
        Ok(self.tag == 0 && self.scalar()? == [0])
    }
    fn validate(self, depth: usize, nodes: &mut usize, work: &mut Work<'_>) -> Result<()> {
        work.charge(1)?;
        if depth > GraphValue::MAX_LIST_DEPTH || *nodes == 0 {
            return Err(invalid());
        }
        *nodes -= 1;
        let mut body = self.body;
        match self.tag {
            0 => {
                self.scalar()?;
            }
            1 | 5 => {
                if body.len() != 16 {
                    return Err(invalid());
                }
            }
            2 => {
                take(&mut body, 16)?;
                let steps = count(&mut body)?;
                if steps.checked_mul(32) != Some(body.len()) {
                    return Err(invalid());
                }
            }
            3 | 4 => {
                let len = count(&mut body)?;
                if len.checked_mul(16) != Some(body.len()) {
                    return Err(invalid());
                }
            }
            6 => {
                let len = count(&mut body)?;
                if len > *nodes {
                    return Err(invalid());
                }
                for _ in 0..len {
                    Self::next(&mut body)?.validate(depth + 1, nodes, work)?;
                }
                if !body.is_empty() {
                    return Err(invalid());
                }
            }
            7 => {
                let len = count(&mut body)?;
                if len > *nodes {
                    return Err(invalid());
                }
                let mut previous = None;
                for _ in 0..len {
                    let key = frame(&mut body)?;
                    validate_key(key, work)?;
                    if let Some(before) = previous
                        && lex(before, key, work)? != Ordering::Less
                    {
                        return Err(invalid());
                    }
                    previous = Some(key);
                    Self::next(&mut body)?.validate(depth + 1, nodes, work)?;
                }
                if !body.is_empty() {
                    return Err(invalid());
                }
            }
            _ => return Err(invalid()),
        }
        Ok(())
    }
}

// Validate UTF-8 without cloning keys or scanning an unbounded string between
// checkpoints. A code point split by a chunk is re-read with the next chunk;
// at most three bytes are revisited, and an incomplete final code point refuses.
fn validate_key(key: &[u8], work: &mut Work<'_>) -> Result<()> {
    let mut remaining = key;
    while !remaining.is_empty() {
        let len = remaining.len().min(1024);
        work.charge(len)?;
        match core::str::from_utf8(&remaining[..len]) {
            Ok(_) => remaining = &remaining[len..],
            Err(error)
                if error.error_len().is_none()
                    && len < remaining.len()
                    && error.valid_up_to() != 0 =>
            {
                remaining = &remaining[error.valid_up_to()..];
            }
            Err(_) => return Err(invalid()),
        }
    }
    Ok(())
}

pub(super) fn validate(bytes: &[u8], columns: usize, work: &mut Work<'_>) -> Result<()> {
    work.charge(1)?;
    let mut bytes = row(bytes, columns)?;
    for _ in 0..columns {
        let mut nodes = GraphValue::MAX_LIST_NODES;
        root(&mut bytes)?.validate(0, &mut nodes, work)?;
    }
    if !bytes.is_empty() {
        return Err(invalid());
    }
    Ok(())
}

// Return the original framed cells, borrowing the authenticated input row.
// Validate the COMPLETE evaluation row first, including every hidden cell and
// its nested frames. A discarded tail must never turn malformed input into a
// successful visible row. Only the domain/count header changes on output.
pub(super) fn visible_prefix<'a>(
    bytes: &'a [u8],
    columns: usize,
    visible: usize,
    work: &mut Work<'_>,
) -> Result<&'a [u8]> {
    work.charge(1)?;
    if visible == 0 || visible > columns {
        return Err(invalid());
    }
    validate(bytes, columns, work)?;
    let frames = row(bytes, columns)?;
    let mut remaining = frames;
    for _ in 0..visible {
        work.charge(1)?;
        frame(&mut remaining)?;
    }
    let prefix = frames.len() - remaining.len();
    Ok(&frames[..prefix])
}

// Bounded chunks admit comparison work before inspecting variable payloads.
fn lex(left: &[u8], right: &[u8], work: &mut Work<'_>) -> Result<Ordering> {
    let common = left.len().min(right.len());
    for (a, b) in left[..common]
        .chunks(1024)
        .zip(right[..common].chunks(1024))
    {
        work.charge(a.len())?;
        let cmp = a.cmp(b);
        if cmp != Ordering::Equal {
            return Ok(cmp);
        }
    }
    work.charge(1)?;
    Ok(left.len().cmp(&right.len()))
}
fn compare_cells(a: Cell<'_>, b: Cell<'_>, depth: usize, work: &mut Work<'_>) -> Result<Ordering> {
    work.charge(1)?;
    if depth > GraphValue::MAX_LIST_DEPTH {
        return Err(invalid());
    }
    if a.tag != b.tag {
        return Ok(a.tag.cmp(&b.tag));
    }
    let (mut left, mut right) = (a.body, b.body);
    match a.tag {
        0 => lex(a.scalar()?, b.scalar()?, work),
        1 | 5 => lex(left, right, work),
        2 => {
            let result = lex(take(&mut left, 16)?, take(&mut right, 16)?, work)?;
            if result != Ordering::Equal {
                return Ok(result);
            }
            count(&mut left)?;
            count(&mut right)?;
            lex(left, right, work)
        }
        3 | 4 => {
            count(&mut left)?;
            count(&mut right)?;
            lex(left, right, work)
        }
        6 => {
            let a_len = count(&mut left)?;
            let b_len = count(&mut right)?;
            for _ in 0..a_len.min(b_len) {
                let cmp = compare_cells(
                    Cell::next(&mut left)?,
                    Cell::next(&mut right)?,
                    depth + 1,
                    work,
                )?;
                if cmp != Ordering::Equal {
                    return Ok(cmp);
                }
            }
            Ok(a_len.cmp(&b_len))
        }
        7 => {
            let a_len = count(&mut left)?;
            let b_len = count(&mut right)?;
            let (a_entries, b_entries) = (left, right);
            // GraphValue::Map derives its order from the complete keys slice,
            // then the complete values slice. Its wire cells are interleaved;
            // comparing key/value pairs would change the native result order.
            for _ in 0..a_len.min(b_len) {
                work.charge(1)?;
                let cmp = lex(frame(&mut left)?, frame(&mut right)?, work)?;
                if cmp != Ordering::Equal {
                    return Ok(cmp);
                }
                Cell::next(&mut left)?;
                Cell::next(&mut right)?;
            }
            if a_len != b_len {
                return Ok(a_len.cmp(&b_len));
            }
            // Equal key vectors permit a second borrowed pass over the values.
            // Framed child skipping is O(1); no decoded map or index is built.
            (left, right) = (a_entries, b_entries);
            for _ in 0..a_len {
                work.charge(1)?;
                frame(&mut left)?;
                frame(&mut right)?;
                let cmp = compare_cells(
                    Cell::next(&mut left)?,
                    Cell::next(&mut right)?,
                    depth + 1,
                    work,
                )?;
                if cmp != Ordering::Equal {
                    return Ok(cmp);
                }
            }
            Ok(Ordering::Equal)
        }
        _ => Err(invalid()),
    }
}
fn column<'a>(
    bytes: &'a [u8],
    width: usize,
    column: usize,
    work: &mut Work<'_>,
) -> Result<Cell<'a>> {
    let mut bytes = row(bytes, width)?;
    for at in 0..width {
        work.charge(1)?;
        let cell = root(&mut bytes)?;
        if at == column {
            return Ok(cell);
        }
    }
    Err(invalid())
}

// The external selected-product host appends an order-preserving signed
// representation of an unsigned occurrence ordinal. Read only that exact
// native scalar shape; the caller validates the complete tagged frame before
// exposing any visible payload. This adds no alternate scalar wire format.
pub(super) fn trailing_ordinal(bytes: &[u8], width: usize, work: &mut Work<'_>) -> Result<u64> {
    let at = width.checked_sub(1).ok_or_else(invalid)?;
    let cell = column(bytes, width, at, work)?;
    if cell.tag != 0 {
        return Err(invalid());
    }
    let scalar = cell.scalar()?;
    if scalar.first() != Some(&0x02) {
        return Err(invalid());
    }
    work.charge(scalar.len())?;
    match fgdb_types::CanonicalScalar::decode(scalar).map_err(|_| invalid())? {
        fgdb_types::CanonicalScalar::Int(value) => {
            Ok(u64::from_be_bytes(value.to_be_bytes()) ^ (1_u64 << 63))
        }
        _ => Err(invalid()),
    }
}

// Equality of selected typed cells, without the sorter's whole-row tie-break.
// Caller supplies authenticated frames already validated by the completed sort.
pub(super) fn equal_columns(
    a: &[u8],
    b: &[u8],
    selected: &[GraphValueOrder],
    columns: usize,
    work: &mut Work<'_>,
) -> Result<bool> {
    for key in selected {
        if compare_cells(
            column(a, columns, key.column, work)?,
            column(b, columns, key.column, work)?,
            0,
            work,
        )? != Ordering::Equal
        {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn compare(
    a: &[u8],
    b: &[u8],
    order: &[GraphValueOrder],
    columns: usize,
    work: &mut Work<'_>,
) -> Result<Ordering> {
    for key in order {
        let a = column(a, columns, key.column, work)?;
        let b = column(b, columns, key.column, work)?;
        let cmp = match (a.is_null()?, b.is_null()?) {
            (true, false) => {
                if key.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, true) => {
                if key.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            _ => {
                let value = compare_cells(a, b, 0, work)?;
                if key.descending {
                    value.reverse()
                } else {
                    value
                }
            }
        };
        if cmp != Ordering::Equal {
            return Ok(cmp);
        }
    }
    // GLA's tie-break is the entire TYPED row, not the framed byte transcript.
    let (mut left, mut right) = (row(a, columns)?, row(b, columns)?);
    for _ in 0..columns {
        let cmp = compare_cells(root(&mut left)?, root(&mut right)?, 0, work)?;
        if cmp != Ordering::Equal {
            return Ok(cmp);
        }
    }
    Ok(Ordering::Equal)
}

#[cfg(test)]
mod temporal_tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_gql::algebra::GraphValueRow;
    use fgdb_types::context::SimulationCheckpointProbe;
    use fgdb_types::{
        CanonicalDate, CanonicalDuration, CanonicalLocalDateTime, CanonicalLocalTime,
        CanonicalScalar, CanonicalTime, PurposeContexts,
    };

    fn values() -> Vec<GraphValue> {
        let midnight = CanonicalLocalTime::MIDNIGHT;
        let date = CanonicalDate::from_epoch_day(-1).unwrap();
        let scalars = [
            CanonicalScalar::Null,
            CanonicalScalar::Int(7),
            CanonicalScalar::Date(date),
            CanonicalScalar::LocalTime(midnight),
            CanonicalScalar::Time(CanonicalTime::new(midnight, 3600).unwrap()),
            CanonicalScalar::LocalDateTime(CanonicalLocalDateTime::new(date, midnight)),
            CanonicalScalar::Duration(CanonicalDuration::new(-1, 2, -3).unwrap()),
        ];
        let mut values = Vec::new();
        for scalar in scalars {
            let value = GraphValue::Scalar(scalar);
            values.push(value.clone());
            values.push(GraphValue::List(vec![value.clone()].into_boxed_slice()));
            values.push(GraphValue::map(vec![("when".into(), value)]).unwrap());
        }
        values
    }

    fn encoded(value: &GraphValue) -> Vec<u8> {
        GraphValueRow::from_owned_values(vec![value.clone()])
            .canonical_bytes()
            .unwrap()
    }

    #[test]
    fn temporal_frames_preserve_typed_order_equality_and_visible_prefixes() {
        let ((), report) = run_async_under_lab(0x50a7_0010, |root| async move {
            let cx = PurposeContexts::narrow_runtime_root(&root).query();
            let values = values();
            let frames: Vec<_> = values.iter().map(encoded).collect();
            let mut work = Work {
                cx: &cx,
                used: 0,
                limit: u64::MAX,
            };
            for (value, bytes) in values.iter().zip(&frames) {
                validate(bytes, 1, &mut work).unwrap();
                let hidden = GraphValueRow::from_owned_values(vec![value.clone(), value.clone()])
                    .canonical_bytes()
                    .unwrap();
                let prefix = visible_prefix(&hidden, 2, 1, &mut work).unwrap();
                let mut visible = ROW.to_vec();
                visible.extend_from_slice(&1_u64.to_be_bytes());
                visible.extend_from_slice(prefix);
                assert_eq!(
                    &visible, bytes,
                    "visible cells keep their exact encoded bytes"
                );
            }
            for (a, left) in values.iter().enumerate() {
                for (b, right) in values.iter().enumerate() {
                    assert_eq!(
                        equal_columns(
                            &frames[a],
                            &frames[b],
                            &[GraphValueOrder::ascending(0)],
                            1,
                            &mut work,
                        )
                        .unwrap(),
                        left == right
                    );
                    for descending in [false, true] {
                        for nulls_first in [false, true] {
                            let expected = match (left.is_null(), right.is_null()) {
                                (true, false) if nulls_first => Ordering::Less,
                                (true, false) => Ordering::Greater,
                                (false, true) if nulls_first => Ordering::Greater,
                                (false, true) => Ordering::Less,
                                _ if descending => right.cmp(left),
                                _ => left.cmp(right),
                            };
                            let order = [GraphValueOrder {
                                column: 0,
                                descending,
                                nulls_first,
                            }];
                            assert_eq!(
                                compare(&frames[a], &frames[b], &order, 1, &mut work).unwrap(),
                                expected
                            );
                        }
                    }
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn temporal_admission_keeps_reserved_and_unknown_scalar_tags_closed() {
        let ((), report) = run_async_under_lab(0x50a7_0011, |root| async move {
            let cx = PurposeContexts::narrow_runtime_root(&root).query();
            let scalar = CanonicalScalar::Date(CanonicalDate::from_epoch_day(0).unwrap());
            let payload = scalar.encode().unwrap();
            let value = GraphValue::Scalar(scalar);
            for hidden in [
                value.clone(),
                GraphValue::List(vec![value].into_boxed_slice()),
            ] {
                let valid = GraphValueRow::from_owned_values(vec![
                    GraphValue::Scalar(CanonicalScalar::Int(1)),
                    hidden,
                ])
                .canonical_bytes()
                .unwrap();
                assert!(valid.ends_with(&payload));
                let tag_at = valid.len() - payload.len();
                for tag in (8_u8..=9).chain(15..=u8::MAX) {
                    let mut malformed = valid.clone();
                    malformed[tag_at] = tag;
                    assert!(GraphValueRow::decode_canonical(&malformed).is_err());
                    let mut work = Work {
                        cx: &cx,
                        used: 0,
                        limit: u64::MAX,
                    };
                    assert!(matches!(
                        validate(&malformed, 2, &mut work),
                        Err(NativeSpoolError::Spill(SpillError::InvalidRun))
                    ));
                    assert!(matches!(
                        visible_prefix(&malformed, 2, 1, &mut work),
                        Err(NativeSpoolError::Spill(SpillError::InvalidRun))
                    ));
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn temporal_comparison_keeps_exact_work_and_cancellation_boundaries() {
        let ((), report) = run_async_under_lab(0x50a7_0012, |root| async move {
            let cx = PurposeContexts::narrow_runtime_root(&root).query();
            let bytes = encoded(&GraphValue::List(values().into_boxed_slice()));
            let order = [GraphValueOrder::ascending(0)];
            let exercise = |work: &mut Work<'_>| {
                validate(&bytes, 1, work)?;
                compare(&bytes, &bytes, &order, 1, work)
            };
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let observed = cx.with_checkpoint_probe(Arc::clone(&probe));
            let mut work = Work {
                cx: &observed,
                used: 0,
                limit: u64::MAX,
            };
            assert_eq!(exercise(&mut work).unwrap(), Ordering::Equal);
            let required = work.used;
            assert!(required > 0);
            for limit in [required - 1, required] {
                let mut work = Work {
                    cx: &cx,
                    used: 0,
                    limit,
                };
                let result = exercise(&mut work);
                if limit == required {
                    assert_eq!(result.unwrap(), Ordering::Equal);
                    assert_eq!(work.used, required);
                } else {
                    assert!(matches!(
                        result,
                        Err(NativeSpoolError::SortWorkLimit { .. })
                    ));
                }
            }
            for stop in 1..=probe.calls() {
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let interrupted = cx.with_checkpoint_probe(Arc::clone(&probe));
                let mut work = Work {
                    cx: &interrupted,
                    used: 0,
                    limit: u64::MAX,
                };
                assert!(matches!(
                    exercise(&mut work),
                    Err(NativeSpoolError::Spill(SpillError::Interrupted(_)))
                ));
                assert_eq!(probe.calls(), stop);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}

#[path = "language_order.rs"]
mod language_order;
