//! Dynamic value-map lookup shared by row and exact aggregate expressions.
//! No graph/catalog read, key coercion, container clone or new transcript tag.

use super::{GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GlaExecutionEvent, GraphSetValue};
use crate::{GraphIntegerError, GraphIntegerErrorKind, GraphIntegerEvaluationError};
use core::cmp::Ordering;

impl GraphSetValue {
    /// Keys have already been admitted as canonical, unique UTF-8 byte order.
    /// Return only an index into the original values: the evaluator owns the
    /// selected value's copy and reservation. No unselected payload is visited.
    /// Each binary-search probe and each bounded key-byte comparison uses the
    /// caller's original control, including long equal-prefix keys.
    pub(crate) fn map_subscript_index<E>(
        keys: &[Box<str>],
        values_len: usize,
        key: &str,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<usize>, GraphIntegerEvaluationError<E>> {
        control(GlaExecutionEvent::Work).map_err(GraphIntegerEvaluationError::Control)?;
        if keys.len() != values_len {
            return Err(GraphIntegerEvaluationError::Value(GraphIntegerError {
                instruction: 0,
                kind: GraphIntegerErrorKind::IncompatibleOperands,
            }));
        }
        let (mut lo, mut hi) = (0, keys.len());
        while lo < hi {
            control(GlaExecutionEvent::Work).map_err(GraphIntegerEvaluationError::Control)?;
            let mid = lo + (hi - lo) / 2;
            let order = compare_key(keys[mid].as_bytes(), key.as_bytes(), control)
                .map_err(GraphIntegerEvaluationError::Control)?;
            match order {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => return Ok(Some(mid)),
            }
        }
        Ok(None)
    }
}

// Map key identity is UTF-8 bytes, not scalar collation or numeric comparison.
// Chunking keeps comparisons interruptible without changing lexicographic order.
fn compare_key<E>(
    left: &[u8],
    right: &[u8],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Ordering, E> {
    for (a, b) in left
        .chunks(GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
        .zip(right.chunks(GRAPH_VALUE_PAYLOAD_UNIT_BYTES))
    {
        control(GlaExecutionEvent::Work)?;
        let order = a.cmp(b);
        if order != Ordering::Equal {
            return Ok(order);
        }
    }
    Ok(left.len().cmp(&right.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GraphSetColumnType;
    use crate::algebra::{GraphValue, GraphValueRow};
    use crate::set_ops::projection::{ProjectionFailure, evaluate_value, value_type};
    use fgdb_types::{CanonicalScalar, EId, VId};

    fn text(value: &str) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap())
    }
    fn int(value: i64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Int(value))
    }
    fn null() -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Null)
    }
    fn map(entries: Vec<(&str, GraphValue)>) -> GraphValue {
        GraphValue::map(
            entries
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
        )
        .unwrap()
    }
    fn expression() -> GraphSetValue {
        GraphSetValue::Index {
            list: Box::new(GraphSetValue::Column(0)),
            index: Box::new(GraphSetValue::Column(1)),
        }
    }
    fn evaluate(source: GraphValue, key: GraphValue) -> Result<GraphValue, GraphIntegerErrorKind> {
        evaluate_value(
            &expression(),
            &GraphValueRow::from_owned_values(vec![source, key]),
            0,
            &mut |_| Ok::<_, core::convert::Infallible>(()),
        )
        .map_err(|error| match error {
            ProjectionFailure::Arithmetic { error, .. } => error.kind,
            ProjectionFailure::Control(never) => match never {},
        })
    }

    #[test]
    fn dynamic_keys_select_exact_values_and_do_not_coerce_or_read_identities() {
        let source = map(vec![
            ("a", int(10)),
            ("b", GraphValue::Vertex(VId(u128::MAX))),
            ("c", GraphValue::Edge(EId(u128::MAX - 1))),
            ("nested", map(vec![("x", int(30))])),
            (
                "list",
                GraphValue::List(vec![null(), int(7)].into_boxed_slice()),
            ),
        ]);
        let (keys, values) = source.as_map().unwrap();
        for (key, value) in keys.iter().zip(values) {
            assert_eq!(evaluate(source.clone(), text(key)), Ok(value.clone()));
        }
        assert_eq!(evaluate(source.clone(), text("absent")), Ok(null()));
        assert_eq!(evaluate(source.clone(), null()), Ok(null()));
        for key in [
            int(0),
            GraphValue::Scalar(CanonicalScalar::Bool(true)),
            map(vec![]),
        ] {
            assert_eq!(
                evaluate(source.clone(), key),
                Err(GraphIntegerErrorKind::IncompatibleOperands)
            );
        }
        assert_eq!(evaluate(null(), int(0)), Ok(null()));
        assert_eq!(
            evaluate(GraphValue::Vertex(VId(1)), text("p")),
            Err(GraphIntegerErrorKind::IncompatibleOperands)
        );
        assert!(
            value_type(
                &expression(),
                &[GraphSetColumnType::Vertex, GraphSetColumnType::Scalar],
                0
            )
            .is_err()
        );
        assert!(
            value_type(
                &expression(),
                &[GraphSetColumnType::Any, GraphSetColumnType::Scalar],
                0
            )
            .is_ok()
        );
    }

    #[test]
    fn integer_list_subscripts_keep_null_negative_and_invalid_key_laws() {
        let source = GraphValue::List(vec![int(2), int(4), int(8)].into_boxed_slice());
        for (index, expected) in [
            (0, int(2)),
            (2, int(8)),
            (-1, int(8)),
            (-3, int(2)),
            (3, null()),
            (-4, null()),
            (i64::MIN, null()),
            (i64::MAX, null()),
        ] {
            assert_eq!(evaluate(source.clone(), int(index)), Ok(expected));
        }
        assert_eq!(
            evaluate(source.clone(), text("0")),
            Err(GraphIntegerErrorKind::NonInteger)
        );
        assert_eq!(evaluate(source, null()), Ok(null()));
        assert_eq!(
            evaluate(map(vec![]), int(0)),
            Err(GraphIntegerErrorKind::IncompatibleOperands)
        );
        assert_eq!(evaluate(map(vec![]), text("0")), Ok(null()));
    }

    #[test]
    fn binary_search_matches_linear_lookup_for_utf8_and_long_prefix_keys() {
        let names = ["", "A", "a", "a\0", "aa", "e\u{301}", "é", "🦀"];
        for mask in 0..1usize << names.len() {
            let mut keys: Vec<Box<str>> = names
                .iter()
                .enumerate()
                .filter(|(at, _)| mask & (1 << at) != 0)
                .map(|(_, name)| Box::<str>::from(*name))
                .collect();
            keys.sort();
            for name in names.iter().copied().chain(["missing", "É", "a\0b"]) {
                let expected = keys.iter().position(|key| key.as_ref() == name);
                let actual =
                    GraphSetValue::map_subscript_index(&keys, keys.len(), name, &mut |_| {
                        Ok::<_, ()>(())
                    })
                    .unwrap();
                assert_eq!(actual, expected);
            }
        }
        let prefix = "x".repeat(GRAPH_VALUE_PAYLOAD_UNIT_BYTES * 5 + 1);
        let keys = [
            Box::<str>::from(format!("{prefix}a")),
            Box::<str>::from(format!("{prefix}b")),
        ];
        for key in [&prefix, &format!("{prefix}a"), &format!("{prefix}c")] {
            let mut events = 0;
            let result = GraphSetValue::map_subscript_index(&keys, keys.len(), key, &mut |_| {
                events += 1;
                Ok::<_, ()>(())
            })
            .unwrap();
            assert_eq!(
                result,
                keys.iter().position(|probe| probe.as_ref() == key.as_str())
            );
            assert!(events > 5, "equal-prefix byte work must be metered");
        }
    }

    #[test]
    fn unselected_payloads_are_not_copied_and_selected_output_is_reserved() {
        let mut usage = Vec::new();
        for hidden in [int(0), text(&"large".repeat(1000))] {
            let row = GraphValueRow::from_owned_values(vec![
                map(vec![("hidden", hidden), ("selected", int(7))]),
                text("selected"),
            ]);
            let mut events = Vec::new();
            let result = evaluate_value(&expression(), &row, 0, &mut |event| {
                events.push(event);
                Ok::<_, ()>(())
            });
            assert!(matches!(result, Ok(value) if value == int(7)));
            usage.push(events);
        }
        assert_eq!(usage[0], usage[1]);
        assert!(usage[0].contains(&GlaExecutionEvent::ScratchEntry));
    }

    #[test]
    fn every_lookup_and_output_control_can_refuse_without_changing_the_input() {
        let key = "a".repeat(GRAPH_VALUE_PAYLOAD_UNIT_BYTES * 3);
        let row =
            GraphValueRow::from_owned_values(vec![map(vec![(&key, text("payload"))]), text(&key)]);
        let before = row.clone();
        let mut events = 0;
        let result = evaluate_value(&expression(), &row, 0, &mut |_| {
            events += 1;
            Ok::<_, usize>(())
        });
        assert!(matches!(result, Ok(value) if value == text("payload")));
        for stop in 0..events {
            let mut count = 0;
            let error = evaluate_value(&expression(), &row, 0, &mut |_| {
                let at = count;
                count += 1;
                if at == stop { Err(stop) } else { Ok(()) }
            });
            assert!(matches!(error, Err(ProjectionFailure::Control(at)) if at == stop));
            assert_eq!(row, before);
        }
        for limit in [events, events - 1] {
            let mut used = 0;
            let result = evaluate_value(&expression(), &row, 0, &mut |_| {
                used += 1;
                if used > limit { Err(()) } else { Ok(()) }
            });
            assert_eq!(result.is_ok(), limit == events);
        }
    }

    #[test]
    fn malformed_key_value_alignment_is_a_typed_refusal() {
        let keys = [Box::<str>::from("a")];
        assert!(matches!(
            GraphSetValue::map_subscript_index(&keys, 0, "a", &mut |_| Ok::<_, ()>(())),
            Err(GraphIntegerEvaluationError::Value(GraphIntegerError {
                kind: GraphIntegerErrorKind::IncompatibleOperands,
                ..
            }))
        ));
    }
}
