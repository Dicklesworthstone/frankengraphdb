use super::*;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::collections::BTreeMap;

fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn row(n: i64) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![GraphValue::Scalar(CanonicalScalar::Int(n))])
}
fn source(rows: Vec<GraphValueRow>) -> GraphDiffInput<'static> {
    let n = rows.len() as u64;
    GraphDiffInput::values(GqlQueryExecution {
        value: rows,
        rows: GqlExecutionStats {
            snapshot_records: n,
            result_rows: n,
        },
        evaluator: GlaExecutionStats {
            work_units: n,
            scratch_entries: n,
        },
    })
}
fn run(
    before: &[GraphValueRow],
    after: &[GraphValueRow],
    policy: GqlQueryPolicy,
) -> Result<GraphResultDiff, Error<&'static str, usize>> {
    GraphResultDiff::execute(
        CommitSeq(1),
        CommitSeq(2),
        vec!["v".into()],
        policy,
        |at, _| {
            Ok(source(
                match at {
                    DiffEndpoint::Before => before,
                    DiffEndpoint::After => after,
                }
                .to_vec(),
            ))
        },
        || Ok(()),
    )
}
fn observed(diff: &GraphResultDiff) -> BTreeMap<i64, i128> {
    diff.changes()
        .iter()
        .map(|(row, weight)| {
            let GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value))) =
                row[0]
            else {
                panic!("integer fixture");
            };
            (value, weight.to_i128().unwrap())
        })
        .collect()
}

#[test]
fn all_6561_bag_transitions_match_signed_counts_not_presence_or_net_total() {
    for mut code in 0..6561 {
        let mut counts = [0usize; 8];
        for count in &mut counts {
            *count = code % 3;
            code /= 3;
        }
        let make = |offset: usize| {
            (0usize..4)
                .flat_map(|value| std::iter::repeat_n(row(value as i64), counts[offset + value]))
                .collect::<Vec<_>>()
        };
        let (before, after) = (make(0), make(4));
        let expected: BTreeMap<_, _> = (0..4)
            .filter_map(|i| {
                let weight = counts[i + 4] as i128 - counts[i] as i128;
                (weight != 0).then_some((i as i64, weight))
            })
            .collect();
        let actual = run(&before, &after, wide()).unwrap();
        assert_eq!(observed(&actual), expected);
        assert_eq!(actual.row_stats().result_rows, expected.len() as u64);
        assert_eq!(
            actual.row_stats().snapshot_records,
            (before.len() + after.len()) as u64
        );
        let inverse = run(&after, &before, wide()).unwrap();
        assert_eq!(
            observed(&inverse),
            expected.iter().map(|(&r, &w)| (r, -w)).collect()
        );
        assert!(run(&before, &before, wide()).unwrap().changes().is_empty());
    }
    let swapped = run(&[row(1)], &[row(2)], wide()).unwrap();
    assert_eq!(swapped.changes().len(), 2); // signed total zero is NOT no change.
}

#[test]
fn one_allowance_covers_both_endpoints_and_final_changes_only() {
    let before = vec![row(1); 100];
    let mut after = before.clone();
    after.push(row(1));
    let full = run(&before, &after, wide()).unwrap();
    assert_eq!(observed(&full), BTreeMap::from([(1, 1)]));
    let stats = full.evaluator_stats();
    let exact = GqlQueryPolicy::new(201, 1, stats.work_units, stats.scratch_entries);
    assert_eq!(run(&before, &after, exact).unwrap(), full);
    for policy in [
        GqlQueryPolicy::new(200, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(201, 0, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(201, 1, stats.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(201, 1, u64::MAX, stats.scratch_entries - 1),
    ] {
        assert!(run(&before, &after, policy).is_err());
    }
    let mut seen = Vec::new();
    GraphResultDiff::execute(
        CommitSeq(2),
        CommitSeq(1),
        vec!["v".into()],
        exact,
        |at, policy| {
            seen.push((at, policy));
            Ok::<_, GqlQueryError<&str, usize>>(source(if at == DiffEndpoint::Before {
                before.clone()
            } else {
                after.clone()
            }))
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].1.rows.max_result_rows(), None);
    assert_eq!(seen[1].1.rows.max_snapshot_records(), Some(101));
    assert!(seen[1].1.evaluator.max_work_units < seen[0].1.evaluator.max_work_units);
    assert!(
        run(
            &before,
            &before,
            GqlQueryPolicy::new(200, 0, u64::MAX, u64::MAX)
        )
        .unwrap()
        .changes()
        .is_empty()
    );
}

#[test]
fn every_checkpoint_and_endpoint_failure_aborts_the_complete_difference() {
    let before = vec![row(1), row(1)];
    let after = vec![row(2), row(1)];
    let mut calls = 0;
    let full = GraphResultDiff::execute(
        CommitSeq(1),
        CommitSeq(2),
        vec!["v".into()],
        wide(),
        |at, _| {
            Ok::<_, GqlQueryError<&str, usize>>(source(if at == DiffEndpoint::Before {
                before.clone()
            } else {
                after.clone()
            }))
        },
        || {
            calls += 1;
            Ok(())
        },
    )
    .unwrap();
    for stop in 1..=calls {
        let mut at = 0;
        let result = GraphResultDiff::execute(
            CommitSeq(1),
            CommitSeq(2),
            vec!["v".into()],
            wide(),
            |side, _| {
                Ok::<_, GqlQueryError<&str, usize>>(source(if side == DiffEndpoint::Before {
                    before.clone()
                } else {
                    after.clone()
                }))
            },
            || {
                at += 1;
                if at == stop { Err(stop) } else { Ok(()) }
            },
        );
        assert_eq!(result.unwrap_err(), GqlQueryError::Interrupted(stop));
        assert_eq!(at, stop);
        assert_eq!(run(&before, &after, wide()).unwrap(), full);
    }
    for failed in [DiffEndpoint::Before, DiffEndpoint::After] {
        let result = GraphResultDiff::execute(
            CommitSeq(1),
            CommitSeq(1),
            vec!["v".into()],
            wide(),
            |side, _| {
                if side == failed {
                    Err(GqlQueryError::<&str, usize>::Source("unavailable"))
                } else {
                    Ok(source(vec![]))
                }
            },
            || Ok(()),
        );
        assert!(
            matches!(result, Err(GqlQueryError::Source(GraphDiffError::Endpoint { endpoint, source: "unavailable" })) if endpoint == failed)
        );
    }
}

#[test]
fn full_native_value_domains_and_nullable_identity_are_not_coerced() {
    let values = vec![
        GraphValue::Scalar(CanonicalScalar::Null),
        GraphValue::Vertex(VId(0)),
        GraphValue::Vertex(VId(u128::MAX)),
        GraphValue::Edge(EId(u128::MAX)),
        GraphValue::Scalar(CanonicalScalar::Int(7)),
        GraphValue::Scalar(CanonicalScalar::ucs_basic_text("7").unwrap()),
        GraphValue::List(
            vec![GraphValue::Edge(EId(0)), GraphValue::Vertex(VId(u128::MAX))].into_boxed_slice(),
        ),
    ];
    let rows: Vec<_> = values
        .iter()
        .map(|v| GraphValueRow::from_owned_values(vec![v.clone()]))
        .collect();
    let diff = run(&[], &rows, wide()).unwrap();
    assert_eq!(diff.changes().len(), values.len());
    for value in values {
        assert_eq!(
            diff.changes()
                .weight(&vec![GraphAggregateValue::Value(value)].into_boxed_slice()),
            Some(&ZWeight::ONE)
        );
    }
    let invalid = GraphValueRow::from_owned_values(vec![]);
    assert!(matches!(
        run(&[], &[invalid], wide()),
        Err(GqlQueryError::Source(GraphDiffError::RowWidth {
            endpoint: DiffEndpoint::After,
            ..
        }))
    ));
    assert!(!format!("{diff:?}").contains("Vertex"));
}

#[test]
fn aggregate_layout_is_validated_even_when_empty() {
    let slots = [GraphAggregateTextSlot::Aggregate(1)];
    let empty = || GqlQueryExecution {
        value: vec![],
        rows: GqlExecutionStats {
            snapshot_records: 0,
            result_rows: 0,
        },
        evaluator: GlaExecutionStats::default(),
    };
    let result = GraphResultDiff::execute(
        CommitSeq(0),
        CommitSeq(0),
        vec!["n".into()],
        wide(),
        |_, _| {
            Ok::<_, GqlQueryError<&str, usize>>(GraphDiffInput::aggregates(empty(), &slots, 0, 1))
        },
        || Ok(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphDiffError::AggregateLayout {
            endpoint: DiffEndpoint::Before
        }))
    ));
}

#[test]
fn aggregate_counts_wide_integers_fractions_and_scalars_keep_disjoint_domains() {
    let slots = [GraphAggregateTextSlot::Aggregate(0)];
    let numeric = [
        GraphAggregateValue::Count(7),
        GraphAggregateValue::Integer(7),
        GraphAggregateValue::Average(crate::GraphExactAverage::new(7, 1).unwrap()),
        GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(7))),
    ];
    let diff = GraphResultDiff::execute(
        CommitSeq(0),
        CommitSeq(1),
        vec!["v".into()],
        wide(),
        |side, _| {
            let rows = if side == DiffEndpoint::Before {
                vec![]
            } else {
                numeric
                    .iter()
                    .cloned()
                    .map(|v| GraphAggregateRow::from_global_values(vec![v]))
                    .collect()
            };
            let n = rows.len() as u64;
            Ok::<_, GqlQueryError<&str, usize>>(GraphDiffInput::aggregates(
                GqlQueryExecution {
                    value: rows,
                    rows: GqlExecutionStats {
                        snapshot_records: 0,
                        result_rows: n,
                    },
                    evaluator: GlaExecutionStats::default(),
                },
                &slots,
                0,
                1,
            ))
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(diff.changes().len(), 4);
    for cell in numeric {
        assert_eq!(
            diff.changes().weight(&vec![cell].into_boxed_slice()),
            Some(&ZWeight::ONE)
        );
    }
}
