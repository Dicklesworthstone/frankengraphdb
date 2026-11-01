use super::*;
use crate::algebra::{GraphColumn, GraphPath, GraphPatternBuilder};
use crate::{GraphAggregate, GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp};
use fgdb_types::{CanonicalScalar, EId, VId};
use std::cell::Cell;
use std::sync::Arc;

fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}

#[cfg(test)]
fn no_source(
    _: &PreparedGraphPattern<GraphValueRow>,
    _: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>> {
    panic!("source-free Apply opened a graph")
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}

fn outer(values: Vec<GraphValue>) -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .unwind("x".into(), GraphSetValue::Value(list(values)))
        .unwrap()
}

fn arguments() -> PreparedGraphSet {
    PreparedGraphSet::arguments(vec!["imported".into()], vec![GraphSetColumnType::Any]).unwrap()
}

fn leaf() -> PreparedGraphSet {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder
        .prepare_values(&[GraphColumn::vertex("n", "n")], 0, None)
        .unwrap()
        .with_duplicates()
        .into()
}

fn source_row() -> GqlQueryExecution<GraphValueRow> {
    GqlQueryExecution {
        value: vec![GraphValueRow::from_owned_values(vec![GraphValue::Vertex(
            VId(u128::MAX),
        )])],
        rows: GqlExecutionStats {
            snapshot_records: 1,
            result_rows: 1,
        },
        evaluator: GlaExecutionStats::default(),
    }
}

fn counted(query: &PreparedGraphSet) -> SetResult<Option<u64>, &'static str, usize> {
    query
        .count_governed(wide(), no_source, || Ok(()))
        .map(|(size, rows, _)| {
            assert_eq!(rows.result_rows, 0);
            size
        })
}

fn materialized(query: &PreparedGraphSet) -> SetResult<u64, &'static str, usize> {
    query
        .execute_governed(wide(), no_source, || Ok(()))
        .map(|execution| execution.value.len() as u64)
}

#[test]
fn inner_and_optional_count_each_duplicate_null_and_locally_paged_body() {
    let input = outer(vec![
        list(vec![]),
        list(vec![int(2), int(2)]),
        GraphValue::Scalar(CanonicalScalar::Null),
        list(vec![int(3)]),
        list(vec![int(2), int(2)]),
    ]);
    let body = arguments()
        .unwind("member".into(), GraphSetValue::Column(0))
        .unwrap()
        .project(
            vec![GraphSetProjection::new("member", GraphSetValue::Column(1))],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .with_order_by(&[GraphValueOrder::descending(0)])
        .unwrap();
    for optional in [false, true] {
        for body_skip in [0, 1, u64::MAX] {
            for body_limit in [None, Some(0), Some(1), Some(3)] {
                // Independent occurrence counts: neither implementation
                // supplies this oracle, including the two repeated lists.
                let expected: u64 = [0_u64, 2, 0, 1, 2]
                    .into_iter()
                    .map(|n| {
                        let n = n.saturating_sub(body_skip);
                        let n = body_limit.map_or(n, |limit| n.min(limit));
                        if optional { n.max(1) } else { n }
                    })
                    .sum();
                let body = body.clone().with_page(body_skip, body_limit);
                let applied = if optional {
                    input.clone().optional_apply(body, vec![0])
                } else {
                    input.clone().apply(body, vec![0])
                }
                .unwrap();
                assert!(applied.has_factorized_cardinality());
                for skip in [0, 1, u64::MAX] {
                    for limit in [None, Some(0), Some(4)] {
                        let query = applied.clone().with_page(skip, limit);
                        let transcript = query.canonical_bytes();
                        let remaining = expected.saturating_sub(skip);
                        let selected = limit.map_or(remaining, |n| n.min(remaining));
                        assert_eq!(counted(&query).unwrap(), Some(selected));
                        assert_eq!(materialized(&query).unwrap(), selected);
                        assert_eq!(query.canonical_bytes(), transcript);
                    }
                }
            }
        }
    }
}

#[test]
fn full_width_vertex_edge_path_and_null_imports_keep_their_domains() {
    let high_vertex = GraphValue::Vertex(VId(u128::MAX));
    let high_edge = GraphValue::Edge(EId(u128::MAX));
    let path = GraphValue::Path(GraphPath::new(
        VId(u128::MAX),
        vec![(EId(u128::MAX - 1), VId(7))].into_boxed_slice(),
    ));
    let values = vec![
        high_vertex.clone(),
        GraphValue::Vertex(VId(u64::MAX.into())),
        high_edge.clone(),
        GraphValue::Edge(EId(u64::MAX.into())),
        path.clone(),
        GraphValue::Path(GraphPath::new(
            VId(u64::MAX.into()),
            vec![(EId(u128::MAX - 1), VId(7))].into_boxed_slice(),
        )),
        GraphValue::Scalar(CanonicalScalar::Null),
        high_vertex.clone(),
        high_edge.clone(),
        path.clone(),
    ];
    for target in [high_vertex, high_edge, path] {
        let body = arguments()
            .unwind(
                "hit".into(),
                GraphSetValue::Case {
                    arms: vec![(
                        GraphSetValue::Equal {
                            left: Box::new(GraphSetValue::Column(0)),
                            right: Box::new(GraphSetValue::Value(target)),
                        },
                        GraphSetValue::Value(list(vec![int(1)])),
                    )],
                    otherwise: Some(Box::new(GraphSetValue::Value(list(vec![])))),
                },
            )
            .unwrap();
        let query = outer(values.clone()).apply(body, vec![0]).unwrap();
        assert_eq!(counted(&query).unwrap(), Some(2));
        assert_eq!(materialized(&query).unwrap(), 2);
    }
    // NULL is still an imported one-row argument relation, not an absent frame.
    let query = outer(values).apply(arguments(), vec![0]).unwrap();
    assert_eq!(counted(&query).unwrap(), Some(10));
    assert_eq!(materialized(&query).unwrap(), 10);
}

#[test]
fn nested_import_order_and_empty_keyless_aggregate_are_observable_rows() {
    let input = outer(vec![int(2), int(2), int(0)]);
    let nested = arguments()
        .unwind(
            "member".into(),
            GraphSetValue::Range {
                start: Box::new(GraphSetValue::Value(int(1))),
                end: Box::new(GraphSetValue::Column(0)),
                step: None,
            },
        )
        .unwrap();
    let body = arguments().apply(nested, vec![0]).unwrap();
    let query = input.clone().apply(body, vec![0]).unwrap();
    assert_eq!(counted(&query).unwrap(), Some(4));
    assert_eq!(materialized(&query).unwrap(), 4);

    let empty_summary = arguments()
        .unwind("absent".into(), GraphSetValue::Value(list(vec![])))
        .unwrap()
        .group_by(&[], &[GraphAggregate::count_rows("zero")])
        .unwrap();
    for optional in [false, true] {
        let query = if optional {
            input.clone().optional_apply(empty_summary.clone(), vec![0])
        } else {
            input.clone().apply(empty_summary.clone(), vec![0])
        }
        .unwrap();
        assert_eq!(counted(&query).unwrap(), Some(3));
        let rows = query
            .execute_governed(wide(), no_source, || Ok(()))
            .unwrap();
        assert_eq!(rows.value.len(), 3);
        assert!(rows.value.iter().all(|row| row.values()[1] == int(0)));
    }
}

#[test]
fn body_sources_keep_logical_ordinals_even_with_zero_or_repeated_outer_rows() {
    for copies in [0, 1, 3] {
        let body = arguments().cross_join(leaf()).unwrap();
        let query = outer(vec![int(1); copies])
            .apply(body, vec![0])
            .unwrap()
            .cross_join(leaf())
            .unwrap()
            .with_page(0, Some(0));
        let mut calls = 0;
        let result = query.count_governed(
            wide(),
            |_: &PreparedGraphPattern<GraphValueRow>, _: GqlQueryPolicy| {
                let at = calls;
                calls += 1;
                let mut execution = source_row();
                if at == copies {
                    // The final leaf is logical operand 1 regardless of the
                    // number of times operand 0 ran under an imported frame.
                    execution.value[0] = GraphValueRow::unit();
                }
                Ok::<_, GqlQueryError<&'static str, usize>>(execution)
            },
            || Ok(()),
        );
        assert_eq!(calls, copies + 1);
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphSetExecutionError::InputSchema {
                operand: 1
            }))
        ));
    }
}

#[test]
fn nested_frames_restore_after_a_late_body_failure_or_cancellation() {
    for interrupt in [false, true] {
        let calls = Cell::new(0);
        let parent = Arc::new(GraphValueRow::from_owned_values(vec![int(99)]));
        let mut meter = Meter {
            policy: wide(),
            checkpoint: || {
                if interrupt && calls.get() == 2 {
                    Err(2)
                } else {
                    Ok(())
                }
            },
            arguments: vec![Arc::clone(&parent)],
            rows: GqlExecutionStats {
                snapshot_records: 0,
                result_rows: 0,
            },
            evaluator: GlaExecutionStats::default(),
        };
        let nested = arguments().cross_join(leaf()).unwrap();
        let body = arguments().apply(nested, vec![0]).unwrap();
        let query = outer(vec![int(1), int(1), int(2)])
            .apply(body, vec![0])
            .unwrap()
            .with_page(0, Some(0));
        let mut source = |_: &PreparedGraphPattern<GraphValueRow>, _: GqlQueryPolicy| {
            calls.set(calls.get() + 1);
            if calls.get() == 2 && !interrupt {
                Err(GqlQueryError::Source("late body failure"))
            } else {
                Ok(source_row())
            }
        };
        let result = count(&query, &mut source, &mut meter, &mut 0);
        if interrupt {
            assert!(matches!(result, Err(GqlQueryError::Interrupted(2))));
        } else {
            assert!(matches!(
                result,
                Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                    "late body failure"
                )))
            ));
        }
        assert_eq!(calls.get(), 2);
        assert_eq!(meter.arguments.len(), 1);
        assert!(Arc::ptr_eq(&meter.arguments[0], &parent));
        assert_eq!(meter.rows.result_rows, 0);
    }
}

#[test]
fn outer_completion_and_body_expression_errors_precede_empty_output_pages() {
    let bad = GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Literal(Some(1)),
            GraphIntegerOp::Literal(Some(0)),
            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
        ])
        .unwrap(),
    );
    let body = arguments()
        .project(
            vec![GraphSetProjection::new("bad", bad)],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .with_page(0, Some(0));
    for fail_outer in [false, true] {
        let input = if fail_outer {
            outer(vec![list(vec![int(1)]), int(7)])
                .unwind("invalid".into(), GraphSetValue::Column(0))
                .unwrap()
        } else {
            outer(vec![int(1)])
        };
        let query = input
            .apply(body.clone(), vec![0])
            .unwrap()
            .with_page(0, Some(0));
        let expected = materialized(&query).unwrap_err();
        if fail_outer {
            assert!(matches!(
                &expected,
                GqlQueryError::Source(GraphSetExecutionError::Projection { row: 1, .. })
            ));
        }
        assert_eq!(counted(&query).unwrap_err(), expected);
    }
}

#[test]
fn collected_lists_keep_depth_validation_before_parent_limit_zero() {
    let mut value = int(1);
    for _ in 0..GraphValue::MAX_LIST_DEPTH {
        value = list(vec![value]);
    }
    let input = outer(vec![list(vec![int(1)])]);
    let body = arguments()
        .project(
            vec![GraphSetProjection::new(
                "mapped",
                GraphSetValue::Value(value),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let query = input
        .collect_apply(body, vec![0], 0)
        .unwrap()
        .with_page(0, Some(0));
    assert!(!query.has_factorized_cardinality());
    let expected = materialized(&query).unwrap_err();
    assert!(matches!(
        &expected,
        GqlQueryError::Source(GraphSetExecutionError::Projection {
            error: crate::GraphIntegerError {
                kind: crate::GraphIntegerErrorKind::Overflow,
                ..
            },
            ..
        })
    ));
    assert_eq!(counted(&query).unwrap_err(), expected);
}

#[test]
fn exact_work_scratch_and_every_checkpoint_refuse_without_a_count_prefix() {
    let body = arguments()
        .unwind("member".into(), GraphSetValue::Column(0))
        .unwrap();
    let query = outer(vec![list(vec![int(1), int(1)]), list(vec![])])
        .optional_apply(body, vec![0])
        .unwrap();
    let mut calls = 0;
    let (expected, rows, used) = query
        .count_governed(wide(), no_source, || {
            calls += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(expected, Some(3));
    assert_eq!(rows.result_rows, 0);
    let exact = GqlQueryPolicy::new(0, 0, used.work_units, used.scratch_entries);
    assert_eq!(
        query.count_governed(exact, no_source, || Ok(())).unwrap().0,
        expected
    );
    for stop in 1..=calls {
        let parent = Arc::new(GraphValueRow::unit());
        let mut seen = 0;
        let mut meter = Meter {
            policy: exact,
            checkpoint: || {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            },
            arguments: vec![Arc::clone(&parent)],
            rows: GqlExecutionStats {
                snapshot_records: 0,
                result_rows: 0,
            },
            evaluator: GlaExecutionStats::default(),
        };
        // count_governed has one final checkpoint after the interpreter.
        let result = count(&query, &mut no_source, &mut meter, &mut 0).and_then(|size| {
            meter.event(GlaExecutionEvent::Work)?;
            Ok(size)
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(meter.arguments.len(), 1);
        assert!(Arc::ptr_eq(&meter.arguments[0], &parent));
        assert_eq!(meter.rows.result_rows, 0);
        drop(meter);
        assert_eq!(seen, stop);
    }
    for policy in [
        GqlQueryPolicy::new(0, 0, used.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(0, 0, u64::MAX, used.scratch_entries - 1),
    ] {
        assert!(matches!(
            query.count_governed(policy, no_source, || Ok(())),
            Err(GqlQueryError::Evaluator(_))
        ));
    }
}
