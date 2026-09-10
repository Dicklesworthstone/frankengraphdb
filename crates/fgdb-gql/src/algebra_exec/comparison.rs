//! Borrowed, binding-dependent property selection in the shared GLA visitor.

use crate::algebra::{GlaOperator, GRAPH_VALUE_PAYLOAD_UNIT_BYTES};
use crate::GlaExecutionEvent;
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{CanonicalScalar, VId};

/// Both source reads are explicit and fallible; no source failure is a missing
/// value. A null binding has no property source and rejects without a read.
/// This predicate depends on the complete pair and deliberately does NOT use
/// the ordinary Select cache keyed only by operator and one vertex identity.
/// The source must supply the same immutable generation as projection reads.
pub(super) fn compare_properties<'a, E>(
    operator: &GlaOperator,
    bindings: &[Option<VId>],
    property: &mut impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<bool, E> {
    let GlaOperator::CompareProperties { left, left_key, right, right_key, comparison } = operator else {
        unreachable!("the compiler dispatches only a binding property comparison")
    };
    let (Some(Some(left)), Some(Some(right))) = (
        bindings.get(left.ordinal() as usize), bindings.get(right.ordinal() as usize),
    ) else { return Ok(false); };
    control(GlaExecutionEvent::Work)?;
    let left = property(*left, *left_key)?;
    control(GlaExecutionEvent::Work)?;
    let right = property(*right, *right_key)?;
    for value in [left, right].into_iter().flatten() {
        charge_payload(value, control)?;
    }
    control(GlaExecutionEvent::Work)?;
    Ok(comparison.accepts_scalar_pair(left, right))
}

/// Reserve each borrowed variable-size payload before comparing it. The two
/// text fields are accounted separately, avoiding a potentially wrapping sum.
/// This is logical work accounting, not preemption within Ord or a byte cap.
fn charge_payload<E>(
    value: &CanonicalScalar,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    let sizes = match value {
        CanonicalScalar::Text(value) => [value.len(), value.canonical_sort_key().map_or(0, <[u8]>::len)],
        CanonicalScalar::Bytes(value) => [value.as_slice().len(), 0],
        CanonicalScalar::Timestamp(value) => [value.zone().map_or(0, |zone| zone.identifier().len()), 0],
        _ => [0, 0],
    };
    for bytes in sizes {
        for _ in 0..bytes.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
            control(GlaExecutionEvent::Work)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algebra::{BindingSlot, IntegerComparison};

    fn comparison() -> GlaOperator {
        GlaOperator::CompareProperties {
            left: BindingSlot(0), left_key: PropertyKeyId(1),
            right: BindingSlot(1), right_key: PropertyKeyId(2),
            comparison: IntegerComparison::Less,
        }
    }

    #[test]
    fn missing_properties_do_not_hide_source_errors_but_null_bindings_do_not_read() {
        let mut reads = 0;
        let result = compare_properties(&comparison(), &[Some(VId(1)), Some(VId(2))],
            &mut |vid, _| { reads += 1; if vid == VId(1) { Ok(None) } else { Err("right source failed") } },
            &mut |_| Ok(()));
        assert_eq!(result, Err("right source failed"));
        assert_eq!(reads, 2);
        assert!(!compare_properties(&comparison(), &[Some(VId(1)), None],
            &mut |_, _| Err::<Option<&CanonicalScalar>, _>("must not read null bindings"),
            &mut |_| Ok(())).unwrap());
    }

    #[test]
    fn payload_work_and_both_source_reads_can_be_interrupted() {
        let first = CanonicalScalar::ucs_basic_text(&"a".repeat(4096)).unwrap();
        let second = CanonicalScalar::ucs_basic_text(&"b".repeat(8192)).unwrap();
        let values = [&first, &second];
        let mut events = 0;
        let complete = compare_properties(&comparison(), &[Some(VId(0)), Some(VId(1))],
            &mut |vid, _| Ok::<_, usize>(Some(values[vid.0 as usize])),
            &mut |event| { assert_eq!(event, GlaExecutionEvent::Work); events += 1; Ok(()) }).unwrap();
        assert!(complete);
        assert_eq!(events, 3 + 4096_usize.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
            + 8192_usize.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES));
        for stop in 1..=events {
            let mut at = 0;
            let result = compare_properties(&comparison(), &[Some(VId(0)), Some(VId(1))],
                &mut |vid, _| Ok::<_, usize>(Some(values[vid.0 as usize])),
                &mut |_| { at += 1; if at == stop { Err(stop) } else { Ok(()) } });
            assert_eq!(result, Err(stop));
            assert_eq!(at, stop);
        }
    }
}
