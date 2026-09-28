//! Borrowed, binding-dependent property selection in the shared GLA visitor.

use crate::GlaExecutionEvent;
use crate::algebra::{GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GlaOperator};
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{CanonicalScalar, VId};

/// Both source reads are explicit and fallible; no source failure is a missing
/// value. A null binding has no property source and rejects without a read.
/// Mixed vertex/captured-edge scalar comparisons execute through the same
/// three-valued engine once bound; each read names its disjoint identity
/// domain and an unreadable source always propagates.
pub(crate) fn compare_element_properties<'a, E: From<crate::GraphIntegerError>>(
    operator: &GlaOperator,
    bindings: &[Option<VId>],
    paths: &[Option<crate::algebra::GraphPath>],
    property: &mut impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
    edge_property: &mut impl FnMut(
        fgdb_types::EId,
        PropertyKeyId,
    ) -> Result<Option<&'a CanonicalScalar>, E>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<bool, E> {
    if let GlaOperator::SelectBoolean { expression } = operator {
        return expression.evaluate_elements(bindings, paths, property, edge_property, control);
    }
    // CompareProperties carries VERTEX binding slots. Capture ordinals occupy
    // a separate namespace and may have exactly the same numeric values. Only
    // SelectBoolean's typed EdgeProperty operands authorize an edge read.
    // In particular a populated capture cannot turn a NULL vertex into a row.
    compare_properties(operator, bindings, property, control)
}
pub(crate) fn compare_properties<'a, E: From<crate::GraphIntegerError>>(
    operator: &GlaOperator,
    bindings: &[Option<VId>],
    property: &mut impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<bool, E> {
    if let GlaOperator::SelectBoolean { expression } = operator {
        return expression.evaluate(bindings, property, control);
    }
    let GlaOperator::CompareProperties {
        left,
        left_key,
        right,
        right_key,
        comparison,
    } = operator
    else {
        unreachable!("the compiler dispatches only a binding property comparison")
    };
    let (Some(Some(left)), Some(Some(right))) = (
        bindings.get(left.ordinal() as usize),
        bindings.get(right.ordinal() as usize),
    ) else {
        return Ok(false);
    };
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
pub(crate) fn charge_payload<E>(
    value: &CanonicalScalar,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    let sizes = match value {
        CanonicalScalar::Text(value) => [
            value.len(),
            value.canonical_sort_key().map_or(0, <[u8]>::len),
        ],
        CanonicalScalar::Bytes(value) => [value.as_slice().len(), 0],
        CanonicalScalar::Timestamp(value) => {
            [value.zone().map_or(0, |zone| zone.identifier().len()), 0]
        }
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

    /// A caller's own source or control failure, or a data exception the
    /// evaluator raised. The evaluator never folds the second into the first.
    #[derive(Debug, PartialEq)]
    enum Failure<S> {
        Source(S),
        Data(crate::GraphIntegerError),
    }
    impl<S> From<crate::GraphIntegerError> for Failure<S> {
        fn from(error: crate::GraphIntegerError) -> Self {
            Self::Data(error)
        }
    }
    use crate::algebra::{BindingSlot, IntegerComparison};

    fn comparison() -> GlaOperator {
        GlaOperator::CompareProperties {
            left: BindingSlot(0),
            left_key: PropertyKeyId(1),
            right: BindingSlot(1),
            right_key: PropertyKeyId(2),
            comparison: IntegerComparison::Less,
        }
    }

    #[test]
    fn missing_properties_do_not_hide_source_errors_but_null_bindings_do_not_read() {
        let mut reads = 0;
        let result = compare_properties(
            &comparison(),
            &[Some(VId(1)), Some(VId(2))],
            &mut |vid, _| {
                reads += 1;
                if vid == VId(1) {
                    Ok(None)
                } else {
                    Err(Failure::Source("right source failed"))
                }
            },
            &mut |_| Ok(()),
        );
        assert_eq!(result, Err(Failure::Source("right source failed")));
        assert_eq!(reads, 2);
        assert!(
            !compare_properties(
                &comparison(),
                &[Some(VId(1)), None],
                &mut |_, _| Err::<Option<&CanonicalScalar>, _>(Failure::Source(
                    "must not read null bindings"
                )),
                &mut |_| Ok(())
            )
            .unwrap()
        );
    }

    #[test]
    fn payload_work_and_both_source_reads_can_be_interrupted() {
        let first = CanonicalScalar::ucs_basic_text(&"a".repeat(4096)).unwrap();
        let second = CanonicalScalar::ucs_basic_text(&"b".repeat(8192)).unwrap();
        let values = [&first, &second];
        let mut events = 0;
        let complete = compare_properties(
            &comparison(),
            &[Some(VId(0)), Some(VId(1))],
            &mut |vid, _| Ok::<_, Failure<usize>>(Some(values[vid.0 as usize])),
            &mut |event| {
                assert_eq!(event, GlaExecutionEvent::Work);
                events += 1;
                Ok(())
            },
        )
        .unwrap();
        assert!(complete);
        assert_eq!(
            events,
            3 + 4096_usize.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
                + 8192_usize.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
        );
        for stop in 1..=events {
            let mut at = 0;
            let result = compare_properties(
                &comparison(),
                &[Some(VId(0)), Some(VId(1))],
                &mut |vid, _| Ok::<_, Failure<usize>>(Some(values[vid.0 as usize])),
                &mut |_| {
                    at += 1;
                    if at == stop {
                        Err(Failure::Source(stop))
                    } else {
                        Ok(())
                    }
                },
            );
            assert_eq!(result, Err(Failure::Source(stop)));
            assert_eq!(at, stop);
        }
    }

    #[test]
    fn null_element_bindings_reject_without_reading_either_operand() {
        for bindings in [[Some(VId(1)), None], [None, Some(VId(2))], [None, None]] {
            let result = compare_element_properties(
                &comparison(),
                &bindings,
                &[],
                &mut |_, _| {
                    Err::<Option<&CanonicalScalar>, _>(Failure::Source("unexpected vertex read"))
                },
                &mut |_, _| {
                    Err::<Option<&CanonicalScalar>, _>(Failure::Source("unexpected edge read"))
                },
                &mut |_| Err(Failure::Source("unexpected work for null bindings")),
            );
            assert_eq!(result, Ok(false));
        }
    }

    #[test]
    fn missing_element_properties_still_propagate_the_other_source_error() {
        let mut reads = Vec::new();
        let result = compare_element_properties(
            &comparison(),
            &[Some(VId(1)), Some(VId(2))],
            &[],
            &mut |vid, _| {
                reads.push(vid);
                if vid == VId(1) {
                    Ok(None)
                } else {
                    Err(Failure::Source("right source failed"))
                }
            },
            &mut |_, _| Err(Failure::Source("unexpected edge read")),
            &mut |_| Ok(()),
        );
        assert_eq!(result, Err(Failure::Source("right source failed")));
        assert_eq!(reads, vec![VId(1), VId(2)]);
    }

    #[test]
    fn element_comparison_can_be_cancelled_before_each_source_read() {
        let value = CanonicalScalar::ucs_basic_text("").unwrap();
        for stop in 1..=3 {
            let mut events = 0;
            let mut reads = 0;
            let result = compare_element_properties(
                &comparison(),
                &[Some(VId(1)), Some(VId(2))],
                &[],
                &mut |_, _| {
                    reads += 1;
                    Ok(Some(&value))
                },
                &mut |_, _| panic!("vertex bindings must not read the edge source"),
                &mut |event| {
                    assert_eq!(event, GlaExecutionEvent::Work);
                    events += 1;
                    if events == stop {
                        Err(Failure::Source(stop))
                    } else {
                        Ok(())
                    }
                },
            );
            assert_eq!(result, Err(Failure::Source(stop)));
            assert_eq!(events, stop);
            assert_eq!(reads, stop - 1);
        }
    }

    fn overlapping_captures() -> [Option<crate::algebra::GraphPath>; 2] {
        use crate::algebra::GraphPath;
        use fgdb_types::EId;
        [
            Some(GraphPath::new(
                VId(77),
                vec![(EId(1), VId(78))].into_boxed_slice(),
            )),
            Some(GraphPath::new(
                VId(88),
                vec![(EId(u128::MAX), VId(89))].into_boxed_slice(),
            )),
        ]
    }

    #[test]
    fn captured_edges_never_replace_vertex_property_operands() {
        let values = [CanonicalScalar::Int(1), CanonicalScalar::Int(2)];
        for (comparison, expected) in [
            (IntegerComparison::Equal, false),
            (IntegerComparison::NotEqual, true),
            (IntegerComparison::Less, true),
            (IntegerComparison::LessOrEqual, true),
            (IntegerComparison::Greater, false),
            (IntegerComparison::GreaterOrEqual, false),
        ] {
            let operator = GlaOperator::CompareProperties {
                left: BindingSlot(0),
                left_key: PropertyKeyId(1),
                right: BindingSlot(1),
                right_key: PropertyKeyId(2),
                comparison,
            };
            let mut reads = Vec::new();
            let result = compare_element_properties(
                &operator,
                &[Some(VId(0)), Some(VId(1))],
                &overlapping_captures(),
                &mut |vid, _| {
                    reads.push(vid);
                    Ok::<_, Failure<&str>>(Some(&values[vid.0 as usize]))
                },
                &mut |_, _| Err(Failure::Source("vertex comparison accessed a captured edge")),
                &mut |_| Ok(()),
            );
            assert_eq!(result, Ok(expected));
            assert_eq!(reads, vec![VId(0), VId(1)]);
        }
    }

    #[test]
    fn captures_cannot_resurrect_null_vertices_or_hide_vertex_read_failures() {
        let paths = overlapping_captures();
        for ids in [[None, Some(VId(2))], [Some(VId(1)), None], [None, None]] {
            assert_eq!(
                compare_element_properties(
                    &comparison(),
                    &ids,
                    &paths,
                    &mut |_, _| Err::<Option<&CanonicalScalar>, _>(Failure::Source("vertex read")),
                    &mut |_, _| Err(Failure::Source("edge read")),
                    &mut |_| Err(Failure::Source("work after a NULL binding")),
                ),
                Ok(false)
            );
        }
        let mut reads = Vec::new();
        assert_eq!(
            compare_element_properties(
                &comparison(),
                &[Some(VId(1)), Some(VId(2))],
                &paths,
                &mut |vid, _| {
                    reads.push(vid);
                    if vid == VId(1) {
                        Ok(None)
                    } else {
                        Err(Failure::Source("second vertex refused"))
                    }
                },
                &mut |_, _| Err(Failure::Source("unexpected captured-edge read")),
                &mut |_| Ok(()),
            ),
            Err(Failure::Source("second vertex refused"))
        );
        assert_eq!(reads, vec![VId(1), VId(2)]);
    }

    #[test]
    fn explicitly_typed_edge_operands_still_read_their_own_domain() {
        use crate::algebra::{
            GlaDirection, GraphBooleanExpression, GraphBooleanOp, GraphBooleanOperand,
            GraphColumn, GraphPath, GraphPatternBuilder,
        };
        use fgdb_types::EId;
        let expression = GraphBooleanExpression::prepare(&[GraphBooleanOp::Compare {
            left: GraphBooleanOperand::EdgeProperty {
                variable: "r",
                key: PropertyKeyId(1),
            },
            comparison: IntegerComparison::Greater,
            right: GraphBooleanOperand::Property {
                variable: "a",
                key: PropertyKeyId(1),
            },
        }])
        .unwrap();
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("a").unwrap().vertex("b").unwrap();
        builder
            .edge("a", fgdb_delta_types::RelationId(1), GlaDirection::Forward, "b")
            .unwrap();
        builder
            .capture_edge("r", 0)
            .unwrap()
            .filter_boolean(&expression)
            .unwrap();
        let plan = builder
            .prepare_values(&[GraphColumn::vertex("a", "a")], 0, None)
            .unwrap();
        let predicate = plan
            .plan()
            .operators()
            .iter()
            .find(|op| matches!(op, GlaOperator::SelectBoolean { .. }))
            .unwrap();
        let vertex_value = CanonicalScalar::Int(5);
        let edge_value = CanonicalScalar::Int(10);
        let mut vertices = Vec::new();
        let mut edges = Vec::new();
        let result = compare_element_properties(
            predicate,
            &[Some(VId(1)), Some(VId(2))],
            &[Some(GraphPath::new(
                VId(1),
                vec![(EId(1), VId(2))].into_boxed_slice(),
            ))],
            &mut |id, _| {
                vertices.push(id);
                Ok::<_, Failure<&str>>(Some(&vertex_value))
            },
            &mut |id, _| {
                edges.push(id);
                Ok(Some(&edge_value))
            },
            &mut |_| Ok(()),
        );
        assert_eq!(result, Ok(true));
        assert_eq!(vertices, vec![VId(1)]);
        assert_eq!(edges, vec![EId(1)]);
    }
}
