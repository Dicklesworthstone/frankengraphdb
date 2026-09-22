use super::*;
use crate::algebra::{GraphValue, IntegerComparison};
use crate::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GraphAggregate, GraphSymbol,
    GraphSymbolKind, PreparedGraphText,
};
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::CanonicalScalar;

const KINDS: [RowJoinKind; 6] = [
    RowJoinKind::Inner,
    RowJoinKind::Left,
    RowJoinKind::Right,
    RowJoinKind::Full,
    RowJoinKind::Semi,
    RowJoinKind::Anti,
];
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000)
}
fn leaf() -> PreparedGraphSet {
    PreparedGraphText::prepare(
        "MATCH (n) RETURN n.k AS k, n.p AS p",
        |kind, name: &str| match (kind, name) {
            (GraphSymbolKind::Property, "k") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            _ => None,
        },
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
    .with_duplicates()
    .into()
}
fn spec(keyed: bool, kind: RowJoinKind) -> RowJoinSpec {
    let types = [GraphSetColumnType::Scalar; 2];
    let spec = if keyed {
        RowJoinSpec::new(&types, &types, &[(0, 0)])
    } else {
        RowJoinSpec::cross(&types, &types)
    }
    .unwrap();
    spec.with_kind(kind)
        .with_predicate(&[GraphSetPredicateOp::Compare {
            left: GraphSetOperand::Column(1),
            comparison: IntegerComparison::Less,
            right: GraphSetOperand::Column(3),
        }])
        .unwrap()
}
fn row(values: &[Option<i64>]) -> GraphValueRow {
    GraphValueRow::from_owned_values(
        values
            .iter()
            .map(|value| {
                GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
            })
            .collect(),
    )
}
fn execution(value: Vec<GraphValueRow>) -> GqlQueryExecution<GraphValueRow> {
    GqlQueryExecution {
        rows: GqlExecutionStats {
            snapshot_records: value.len() as u64,
            result_rows: value.len() as u64,
        },
        value,
        evaluator: GlaExecutionStats::default(),
    }
}
fn run(
    query: &PreparedGraphSet,
    inputs: [Vec<GraphValueRow>; 2],
    policy: GqlQueryPolicy,
    checkpoint: impl FnMut() -> Result<(), usize>,
) -> SetResult<GqlQueryExecution<GraphValueRow>, usize, usize> {
    let mut inputs = inputs.into_iter();
    let result = query.execute_governed(
        policy,
        |_, _| Ok(execution(inputs.next().expect("each source executes once"))),
        checkpoint,
    );
    if result.is_ok() {
        assert!(inputs.next().is_none());
    }
    result
}
// Primitive occurrence oracle, independent of the production row-join,
// predicate evaluator, Z-set derivative and count arrangements.
fn oracle(
    left: &[[Option<i64>; 2]],
    right: &[[Option<i64>; 2]],
    keyed: bool,
    kind: RowJoinKind,
) -> Vec<GraphValueRow> {
    let matches = |a: &[Option<i64>; 2], b: &[Option<i64>; 2]| {
        (!keyed || a[0].zip(b[0]).is_some_and(|(a, b)| a == b))
            && a[1].zip(b[1]).is_some_and(|(a, b)| a < b)
    };
    let mut out = Vec::new();
    for a in left {
        let mut found = false;
        for b in right {
            if matches(a, b) {
                found = true;
                if !matches!(kind, RowJoinKind::Semi | RowJoinKind::Anti) {
                    out.push(row(&[a[0], a[1], b[0], b[1]]));
                }
            }
        }
        match kind {
            RowJoinKind::Semi if found => out.push(row(a)),
            RowJoinKind::Anti if !found => out.push(row(a)),
            RowJoinKind::Left | RowJoinKind::Full if !found => {
                out.push(row(&[a[0], a[1], None, None]))
            }
            _ => {}
        }
    }
    if matches!(kind, RowJoinKind::Right | RowJoinKind::Full) {
        for b in right {
            if !left.iter().any(|a| matches(a, b)) {
                out.push(row(&[None, None, b[0], b[1]]));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn all_six_relational_joins_match_independent_occurrence_oracles() {
    for counts in 0..81_usize {
        let mut counts = counts;
        let mut amounts = [0; 4];
        for amount in &mut amounts {
            *amount = counts % 3;
            counts /= 3;
        }
        let left: Vec<_> = std::iter::repeat_n([Some(1), Some(10)], amounts[0])
            .chain(std::iter::repeat_n([Some(1), Some(30)], amounts[1]))
            .collect();
        let right: Vec<_> = std::iter::repeat_n([Some(1), Some(20)], amounts[2])
            .chain(std::iter::repeat_n([Some(2), Some(40)], amounts[3]))
            .collect();
        for keyed in [true, false] {
            for kind in KINDS {
                let query = leaf().join(leaf(), spec(keyed, kind)).unwrap();
                let inputs = [
                    left.iter().map(|a| row(a)).collect(),
                    right.iter().map(|b| row(b)).collect(),
                ];
                let result = run(&query, inputs, policy(), || Ok(())).unwrap();
                assert_eq!(result.value, oracle(&left, &right, keyed, kind));
                assert_eq!(
                    result.rows.snapshot_records,
                    (left.len() + right.len()) as u64
                );
                assert_eq!(result.rows.result_rows, result.value.len() as u64);
                assert_eq!(query.operand_count(), 2);
                let expected = if matches!(kind, RowJoinKind::Semi | RowJoinKind::Anti) {
                    vec!["left.k", "left.p"]
                } else {
                    vec!["left.k", "left.p", "right.k", "right.p"]
                };
                assert_eq!(query.columns(), expected);
            }
        }
    }
}

#[test]
fn full_join_finishes_child_pages_before_on_and_supports_further_relational_stages() {
    let left = leaf()
        .with_order_by(&[GraphValueOrder::descending(1)])
        .unwrap()
        .with_page(1, Some(1));
    let right = leaf().with_page(0, Some(2));
    let query = left.join(right, spec(true, RowJoinKind::Full)).unwrap();
    let inputs = [
        vec![
            row(&[Some(1), Some(10)]),
            row(&[Some(1), Some(20)]),
            row(&[Some(1), Some(30)]),
        ],
        vec![
            row(&[Some(1), Some(15)]),
            row(&[Some(1), Some(25)]),
            row(&[Some(1), Some(35)]),
        ],
    ];
    let result = run(&query, inputs.clone(), policy(), || Ok(())).unwrap();
    assert_eq!(
        result.value,
        vec![
            row(&[None, None, Some(1), Some(15)]),
            row(&[Some(1), Some(20), Some(1), Some(25)])
        ]
    );
    let selected = query
        .clone()
        .filter(&[GraphSetPredicateOp::IsNull {
            operand: GraphSetOperand::Column(0),
            is_null: false,
        }])
        .unwrap()
        .project(
            vec![
                GraphSetProjection::new("right_value", GraphSetValue::Column(3)),
                GraphSetProjection::new("left_value", GraphSetValue::Column(1)),
            ],
            GraphSetQuantifier::Distinct,
        )
        .unwrap()
        .with_page(0, Some(1));
    assert_eq!(
        run(&selected, inputs.clone(), policy(), || Ok(()))
            .unwrap()
            .value,
        vec![row(&[Some(25), Some(20)])]
    );
    let summary = PreparedGraphSetAggregate::prepare(
        query,
        &[],
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::count("left_nonnull", 1),
        ],
        0,
        None,
    )
    .unwrap();
    let mut inputs = inputs.into_iter();
    let result = summary
        .execute_governed(
            policy(),
            |_, _| Ok::<_, GqlQueryError<usize, usize>>(execution(inputs.next().unwrap())),
            || Ok::<_, usize>(()),
        )
        .unwrap();
    assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(2));
    assert_eq!(result.value[0].get(1).unwrap().as_count(), Some(1));
    assert!(inputs.next().is_none());
}

#[test]
fn nulls_and_incompatible_values_remain_unknown_under_not_and_zero_width_full_keeps_both_sides() {
    let types = [GraphSetColumnType::Scalar; 2];
    let negative = RowJoinSpec::cross(&types, &types)
        .unwrap()
        .with_kind(RowJoinKind::Full)
        .with_predicate(&[
            GraphSetPredicateOp::Compare {
                left: GraphSetOperand::Column(1),
                comparison: IntegerComparison::Less,
                right: GraphSetOperand::Column(3),
            },
            GraphSetPredicateOp::Not,
        ])
        .unwrap();
    let text = GraphValueRow::from_owned_values(vec![
        GraphValue::Scalar(CanonicalScalar::Int(1)),
        GraphValue::Scalar(CanonicalScalar::ucs_basic_text("private").unwrap()),
    ]);
    let query = leaf().join(leaf(), negative).unwrap();
    let result = run(
        &query,
        [
            vec![row(&[Some(1), None]), text],
            vec![row(&[Some(1), Some(10)])],
        ],
        policy(),
        || Ok(()),
    )
    .unwrap();
    assert_eq!(result.value.len(), 3); // no UNKNOWN pair was accepted by NOT
    let spec = RowJoinSpec::cross(&[], &[])
        .unwrap()
        .with_kind(RowJoinKind::Full)
        .with_predicate(&[GraphSetPredicateOp::Truth(Some(false))])
        .unwrap();
    let query = PreparedGraphSet::singleton()
        .join(PreparedGraphSet::singleton(), spec)
        .unwrap();
    let result = query
        .execute_governed(
            policy(),
            |_, _| -> Result<_, GqlQueryError<usize, usize>> {
                panic!("source-free relation must not invent a graph leaf")
            },
            || Ok::<_, usize>(()),
        )
        .unwrap();
    assert_eq!(
        result.value,
        vec![GraphValueRow::unit(), GraphValueRow::unit()]
    );
    assert_eq!(result.rows.snapshot_records, 0);
}

#[test]
fn empty_left_and_zero_final_limit_do_not_hide_right_source_failure() {
    for kind in KINDS {
        let query = leaf()
            .join(leaf(), spec(true, kind))
            .unwrap()
            .with_page(0, Some(0));
        let mut calls = 0;
        let result = query.execute_governed(
            policy(),
            |_, _| {
                calls += 1;
                if calls == 1 {
                    Ok(execution(vec![]))
                } else {
                    Err(GqlQueryError::Source(73_usize))
                }
            },
            || Ok::<_, usize>(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(73)))
        ));
        assert_eq!(calls, 2);
    }
}

#[test]
fn every_cancellation_checkpoint_and_inclusive_budget_is_fail_closed_and_retryable() {
    let query = leaf().join(leaf(), spec(true, RowJoinKind::Full)).unwrap();
    let inputs = [
        vec![row(&[Some(1), Some(10)]), row(&[Some(1), Some(10)])],
        vec![row(&[Some(1), Some(20)])],
    ];
    let mut calls = 0;
    let expected = run(&query, inputs.clone(), policy(), || {
        calls += 1;
        Ok(())
    })
    .unwrap();
    for stop in 1..=calls {
        let mut seen = 0;
        let result = run(&query, inputs.clone(), policy(), || {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
    }
    let exact = GqlQueryPolicy::new(
        3,
        2,
        expected.evaluator.work_units,
        expected.evaluator.scratch_entries,
    );
    assert_eq!(
        run(&query, inputs.clone(), exact, || Ok(())).unwrap().value,
        expected.value
    );
    for (records, rows, work, scratch) in [
        (
            2,
            2,
            expected.evaluator.work_units,
            expected.evaluator.scratch_entries,
        ),
        (
            3,
            1,
            expected.evaluator.work_units,
            expected.evaluator.scratch_entries,
        ),
        (
            3,
            2,
            expected.evaluator.work_units - 1,
            expected.evaluator.scratch_entries,
        ),
        (
            3,
            2,
            expected.evaluator.work_units,
            expected.evaluator.scratch_entries - 1,
        ),
    ] {
        assert!(
            run(
                &query,
                inputs.clone(),
                GqlQueryPolicy::new(records, rows, work, scratch),
                || Ok(())
            )
            .is_err()
        );
    }
    let empty = query.with_page(0, Some(0));
    assert!(
        run(
            &empty,
            inputs,
            GqlQueryPolicy::new(3, 0, 1_000_000, 1_000_000),
            || Ok(())
        )
        .unwrap()
        .value
        .is_empty()
    );
}

#[test]
fn bound_join_identity_schema_and_window_accessors_cover_complete_semantics() {
    let left = leaf();
    let mut transcripts = std::collections::BTreeSet::new();
    for kind in KINDS {
        for keyed in [true, false] {
            let definition = spec(keyed, kind);
            let query = left.clone().join(leaf(), definition.clone()).unwrap();
            assert!(transcripts.insert(query.canonical_bytes()));
            let (a, b, observed) = query.incremental_join().unwrap();
            assert_eq!(a, &left);
            assert_eq!(b, &left);
            assert_eq!(observed, &definition);
            assert_eq!(query.incremental_result_order(), Some((&[][..], None)));
            assert!(query.incremental_cross_join().is_none());
            for (offset, count) in [(1, None), (0, Some(0)), (0, Some(2))] {
                let paged = query.clone().with_page(offset, count);
                assert!(paged.incremental_join().is_none());
                assert_ne!(paged.canonical_bytes(), query.canonical_bytes());
                if count.is_some() {
                    let (input, _) = paged.incremental_window().unwrap().unwrap();
                    assert_eq!(input, query);
                }
            }
        }
    }
    let types = [GraphSetColumnType::Scalar; 2];
    let mut changed = types;
    changed[1] = GraphSetColumnType::Any;
    for side in 0..2 {
        let (a, b) = if side == 0 {
            (&changed, &types)
        } else {
            (&types, &changed)
        };
        let definition = RowJoinSpec::cross(a, b)
            .unwrap()
            .with_kind(RowJoinKind::Semi)
            .with_predicate(&[GraphSetPredicateOp::Truth(Some(false))])
            .unwrap();
        assert_eq!(
            left.clone().join(leaf(), definition),
            Err(GraphSetBuildError::JoinInputSchema { side })
        );
    }
    for code in [
        vec![GraphSetPredicateOp::Truth(Some(true))],
        vec![GraphSetPredicateOp::Truth(Some(false))],
        vec![GraphSetPredicateOp::Truth(None)],
        vec![GraphSetPredicateOp::Truth(None), GraphSetPredicateOp::Not],
    ] {
        let definition = RowJoinSpec::cross(&types, &types)
            .unwrap()
            .with_predicate(&code)
            .unwrap();
        assert!(
            transcripts.insert(
                left.clone()
                    .join(leaf(), definition)
                    .unwrap()
                    .canonical_bytes()
            )
        );
    }
    let mut deep = leaf();
    for _ in 1..MAX_GRAPH_SET_DEPTH {
        deep = deep.nested().unwrap();
    }
    assert!(matches!(
        deep.join(leaf(), spec(true, RowJoinKind::Inner)),
        Err(GraphSetBuildError::TooDeep { .. })
    ));
}
