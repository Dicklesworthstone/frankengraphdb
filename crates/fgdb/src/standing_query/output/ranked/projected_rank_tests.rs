//! Computed rank cells are retained, never evaluated by the ordered index.
use super::*;
use crate::standing_query::StandingQueryStats;
use fgdb_gql::{GqlQueryPolicy, GraphAggregateValue as Value, GraphExactAverage};
use fgdb_types::{CanonicalScalar, VId};

fn rank(key: u128, score: Value, descending: bool, nulls: GraphNullPlacement) -> Rank {
    let output = GraphAggregateRow::from_group_values(vec![], vec![score, Value::Count(99)]);
    Rank {
        order: Arc::from([GraphAggregateOrder {
            column: GraphAggregateColumn::Aggregate(0),
            descending,
            nulls,
        }]),
        // The raw count deliberately has the opposite order to some scores.
        group: Arc::new(GraphAggregateRow::from_group_values(
            vec![GraphValue::Vertex(VId(key))],
            vec![Value::Count(
                u64::MAX - key.min(u128::from(u64::MAX)) as u64,
            )],
        )),
        projected: Some(Arc::new(output)),
    }
}

#[test]
fn projected_rank_preserves_extreme_fractions_nulls_and_original_key_ties() {
    let a = Value::Average(GraphExactAverage::new(i128::MAX, u64::MAX).unwrap());
    let b = Value::Average(GraphExactAverage::new(i128::MAX - 1, u64::MAX - 1).unwrap());
    // (M-1)/(D-1) > M/D when M>D. Cross-multiplication would overflow i128.
    for descending in [false, true] {
        for nulls in [GraphNullPlacement::First, GraphNullPlacement::Last] {
            let left = rank(9, a.clone(), descending, nulls);
            let right = rank(1, b.clone(), descending, nulls);
            let expected = if descending {
                Ordering::Greater
            } else {
                Ordering::Less
            };
            assert_eq!(left.cmp(&right), expected);
            assert_eq!(right.cmp(&left), expected.reverse());
            let null = rank(
                0,
                Value::Value(GraphValue::Scalar(CanonicalScalar::Null)),
                descending,
                nulls,
            );
            assert_eq!(
                null.cmp(&left),
                if nulls == GraphNullPlacement::First {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            );
            let left = rank(1, Value::Count(2), descending, nulls);
            let right = rank(
                u128::MAX,
                Value::Average(GraphExactAverage::new(4, 2).unwrap()),
                descending,
                nulls,
            );
            assert_eq!(
                left.cmp(&right),
                Ordering::Less,
                "complete 128-bit keys break numeric ties"
            );
        }
    }
}

#[test]
fn equal_rank_replacement_refreshes_complete_and_projected_rows_without_copying_payloads() {
    let old = rank(1, Value::Integer(7), true, GraphNullPlacement::Last);
    let mut next = old.clone();
    next.group = Arc::new(GraphAggregateRow::from_group_values(
        old.group.keys().to_vec(),
        vec![Value::Count(17)],
    ));
    next.projected = Some(Arc::new(GraphAggregateRow::from_group_values(
        vec![],
        vec![Value::Integer(7), Value::Count(100)],
    )));
    assert_eq!(
        old.cmp(&next),
        Ordering::Equal,
        "unreferenced cells do not reorder a group"
    );
    let mut base = Ranked::new();
    base.insert(old.clone(), Arc::clone(old.projected.as_ref().unwrap()));
    let mut patch = RankPatch::new();
    patch.insert(
        next.clone(),
        Some(Arc::clone(next.projected.as_ref().unwrap())),
    );
    publish_ranked(&mut base, patch);
    let (stored, output) = base.first_key_value().unwrap();
    assert_eq!(stored.group.values(), &[Value::Count(17)]);
    assert_eq!(output.values()[1], Value::Count(100));
    assert!(Arc::ptr_eq(stored.projected.as_ref().unwrap(), output));
    assert!(Arc::ptr_eq(output, next.projected.as_ref().unwrap()));
}

#[test]
fn projected_index_merges_deletions_and_score_changes_before_refilling_each_window() {
    for descending in [false, true] {
        for removed in 0..5 {
            let make = |key: usize, score: i128| {
                rank(
                    key as u128,
                    Value::Integer(score),
                    descending,
                    GraphNullPlacement::Last,
                )
            };
            let scores = [7, -2, 7, 99, 0];
            let mut base = Ranked::new();
            for (key, score) in scores.iter().copied().enumerate() {
                let candidate = make(key, score);
                base.insert(
                    candidate.clone(),
                    Arc::clone(candidate.projected.as_ref().unwrap()),
                );
            }
            let mut patch = RankPatch::new();
            patch.insert(make(removed, scores[removed]), None);
            let replacement = make(removed, -20);
            patch.insert(
                replacement.clone(),
                Some(Arc::clone(replacement.projected.as_ref().unwrap())),
            );
            let mut expected: Vec<_> = scores
                .iter()
                .copied()
                .enumerate()
                .map(|(key, score)| (key, if key == removed { -20 } else { score }))
                .collect();
            expected.sort_by(|(ka, a), (kb, b)| {
                (if descending { b.cmp(a) } else { a.cmp(b) }).then_with(|| ka.cmp(kb))
            });
            for offset in [0, 1, 3, 8, u64::MAX] {
                for count in [None, Some(0), Some(1), Some(3), Some(u64::MAX)] {
                    let mut checkpoint = || Ok(());
                    let mut meter = Meter {
                        policy: GqlQueryPolicy::new(100, 100, 100_000, 100_000),
                        stats: StandingQueryStats::default(),
                        checkpoint: &mut checkpoint,
                    };
                    let rows = select_page(&base, &patch, offset, count, &mut meter).unwrap();
                    let values: Vec<_> = expected
                        .iter()
                        .skip(usize::try_from(offset).unwrap_or(usize::MAX))
                        .take(
                            count
                                .and_then(|n| usize::try_from(n).ok())
                                .unwrap_or(usize::MAX),
                        )
                        .map(|(_, score)| Value::Integer(*score))
                        .collect();
                    assert_eq!(
                        rows.iter()
                            .map(|row| row.values()[0].clone())
                            .collect::<Vec<_>>(),
                        values
                    );
                    assert_eq!(
                        base.len(),
                        5,
                        "selection must not publish its tentative patch"
                    );
                }
            }
        }
    }
}

#[test]
fn distinct_class_refill_uses_complete_key_after_equal_projected_numeric_values() {
    let a = rank(1, Value::Count(2), true, GraphNullPlacement::Last);
    let b = rank(
        3,
        Value::Average(GraphExactAverage::new(4, 2).unwrap()),
        true,
        GraphNullPlacement::Last,
    );
    let c = rank(2, Value::Integer(2), true, GraphNullPlacement::Last);
    let mut base = Ranked::new();
    for candidate in [&a, &b] {
        base.insert(
            candidate.clone(),
            Arc::clone(candidate.projected.as_ref().unwrap()),
        );
    }
    let mut patch = RankPatch::new();
    patch.insert(a, None);
    patch.insert(c.clone(), Some(Arc::clone(c.projected.as_ref().unwrap())));
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: GqlQueryPolicy::new(100, 100, 100_000, 100_000),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    let (representative, value) = first_after(Some(&base), &patch, &mut meter)
        .unwrap()
        .unwrap();
    assert_eq!(representative.group.keys(), &[GraphValue::Vertex(VId(2))]);
    assert_eq!(
        value.values()[0],
        Value::Integer(2),
        "retain the winning representation"
    );
}

use crate::standing_query::output::State as Output;
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, GraphValueOrder};
use fgdb_gql::{
    GraphAggregate, GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp as Op,
    GraphSetProjection, GraphSetValue,
};

fn maintained_definition(
    distinct: bool,
    offset: u64,
    count: Option<u64>,
) -> PreparedGraphAggregate {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let input = builder
        .prepare_values(
            &[
                GraphColumn::property("key", "n", PropertyKeyId(1)),
                GraphColumn::property("amount", "n", PropertyKeyId(2)),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    let score = GraphIntegerExpression::prepare(&[
        Op::Column(2),
        Op::Literal(Some(-1)),
        Op::Binary(GraphIntegerBinary::Multiply),
    ])
    .unwrap();
    PreparedGraphAggregate::prepare(
        input,
        &[0],
        &[
            GraphAggregate::count_rows("count"),
            GraphAggregate::sum_int("sum", 1),
        ],
        offset,
        count,
    )
    .unwrap()
    .with_output_projection(vec![GraphSetProjection::new(
        "score",
        GraphSetValue::Integer(score),
    )])
    .unwrap()
    .with_projected_order_by(&[GraphValueOrder::descending(0)])
    .unwrap()
    .with_distinct_output(distinct)
}

fn maintenance_policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 2, 10_000_000, 10_000_000)
}

fn changes(values: &[(i64, u64, i128, i128)]) -> ZSet<GraphAggregateRow> {
    ZSet::from_updates(
        values.iter().map(|&(key, count, sum, sign)| {
            (
                GraphAggregateRow::from_group_values(
                    vec![GraphValue::Scalar(CanonicalScalar::Int(key))],
                    vec![Value::Count(count), Value::Integer(sum)],
                ),
                ZWeight::from_i128(sign),
            )
        }),
        LimbLimit::new(4),
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}

fn apply_output(
    output: &mut Output,
    delta: &ZSet<GraphAggregateRow>,
    policy: GqlQueryPolicy,
    checkpoint: &mut dyn FnMut() -> Result<(), StandingQueryFailure>,
) -> Result<StandingQueryStats, StandingQueryFailure> {
    let mut meter = Meter {
        policy,
        stats: StandingQueryStats::default(),
        checkpoint,
    };
    output.prepare(delta, &mut meter)?.commit();
    Ok(meter.stats)
}

fn initial_output(distinct: bool) -> Output {
    let definition = maintained_definition(distinct, 1, Some(2));
    let source = definition.incremental_source_definition().unwrap();
    assert!(!source.has_incremental_output_transform());
    assert!(source.projected_ordering().is_empty());
    assert!(source.output_projection().is_none());
    assert_eq!(source.incremental_result_window(), (0, None));
    let mut output = Output::new(definition);
    apply_output(
        &mut output,
        &changes(&[
            (1, 2, 4, 1),
            (2, 1, 9, 1),
            (3, 1, 8, 1),
            (4, 1, 4, 1),
            (5, 1, 11, 1),
        ]),
        maintenance_policy(),
        &mut || Ok(()),
    )
    .unwrap();
    output
}

fn scores(output: &Output) -> Vec<i128> {
    output
        .ordered_rows()
        .unwrap()
        .iter()
        .map(|row| row.values()[0].as_integer().unwrap())
        .collect()
}

fn same_output(actual: &Output, expected: &Output) {
    assert_eq!(actual.rows, expected.rows);
    assert_eq!(actual.row_count, expected.row_count);
    let a = actual.ranked.as_ref().unwrap();
    let b = expected.ranked.as_ref().unwrap();
    assert_eq!(a.order, b.order);
    assert_eq!(a.projected_order, b.projected_order);
    let groups = |state: &State| {
        state
            .groups
            .iter()
            .map(|(key, rank)| (key.clone(), rank.group.clone(), rank.projected.clone()))
            .collect::<Vec<_>>()
    };
    let entries = |rows: &Ranked| {
        rows.iter()
            .map(|(rank, value)| (rank.group.clone(), rank.projected.clone(), value.clone()))
            .collect::<Vec<_>>()
    };
    let classes = |state: &State| {
        state
            .classes
            .iter()
            .map(|(key, members)| (key.clone(), entries(members)))
            .collect::<Vec<_>>()
    };
    assert_eq!(groups(a), groups(b));
    assert_eq!(classes(a), classes(b));
    assert_eq!(entries(&a.candidates), entries(&b.candidates));
    assert_eq!(a.page, b.page);
}

#[test]
fn computed_output_maintenance_aborts_every_checkpoint_budget_and_dropped_guard_then_retries() {
    let delta = changes(&[(1, 2, 4, -1), (2, 1, 9, -1), (1, 3, 12, 1), (6, 1, 2, 1)]);
    for distinct in [false, true] {
        let before = initial_output(distinct);
        assert_eq!(
            scores(&before),
            if distinct { vec![-8, -9] } else { vec![-4, -8] }
        );
        let mut complete = initial_output(distinct);
        let mut calls = 0;
        let stats = apply_output(&mut complete, &delta, maintenance_policy(), &mut || {
            calls += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(scores(&complete), vec![-4, -8]);
        assert!(calls > 0);
        for stop in 1..=calls {
            let mut candidate = initial_output(distinct);
            let mut seen = 0;
            assert_eq!(
                apply_output(&mut candidate, &delta, maintenance_policy(), &mut || {
                    seen += 1;
                    if seen == stop {
                        Err(StandingQueryFailure::Interrupted)
                    } else {
                        Ok(())
                    }
                }),
                Err(StandingQueryFailure::Interrupted)
            );
            assert_eq!(seen, stop);
            same_output(&candidate, &before);
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: maintenance_policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            drop(candidate.prepare(&delta, &mut meter).unwrap());
            same_output(&candidate, &before);
            apply_output(&mut candidate, &delta, maintenance_policy(), &mut || Ok(())).unwrap();
            same_output(&candidate, &complete);
        }
        for reason in [
            StandingQueryFailure::WorkBudget,
            StandingQueryFailure::ScratchBudget,
            StandingQueryFailure::ResultBudget,
        ] {
            let mut candidate = initial_output(distinct);
            let mut bounded = maintenance_policy();
            match reason {
                StandingQueryFailure::WorkBudget => {
                    bounded.evaluator.max_work_units = stats.work_units - 1
                }
                StandingQueryFailure::ScratchBudget => {
                    bounded.evaluator.max_scratch_entries = stats.scratch_entries - 1
                }
                _ => bounded = GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000),
            }
            assert_eq!(
                apply_output(&mut candidate, &delta, bounded, &mut || Ok(())),
                Err(reason)
            );
            same_output(&candidate, &before);
            apply_output(&mut candidate, &delta, maintenance_policy(), &mut || Ok(())).unwrap();
            same_output(&candidate, &complete);
        }
    }
}

#[test]
fn computed_output_class_moves_and_equal_rank_updates_refresh_before_images() {
    for distinct in [false, true] {
        let mut output = initial_output(distinct);
        // Change an unsorted summary without changing the computed score.
        apply_output(
            &mut output,
            &changes(&[(4, 1, 4, -1), (4, 7, 4, 1)]),
            maintenance_policy(),
            &mut || Ok(()),
        )
        .unwrap();
        let current = output
            .ranked
            .as_ref()
            .unwrap()
            .groups
            .values()
            .find(|rank| rank.group.keys() == [GraphValue::Scalar(CanonicalScalar::Int(4))])
            .unwrap();
        assert_eq!(current.group.values()[0], Value::Count(7));
        // A stale before-image must refuse without modifying the arrangements.
        let mut expected = initial_output(distinct);
        apply_output(
            &mut expected,
            &changes(&[(4, 1, 4, -1), (4, 7, 4, 1)]),
            maintenance_policy(),
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(
            apply_output(
                &mut output,
                &changes(&[(4, 1, 4, -1)]),
                maintenance_policy(),
                &mut || Ok(())
            ),
            Err(StandingQueryFailure::InvalidDelta)
        );
        same_output(&output, &expected);
        // Move two groups between classes in one tick, then remove a winner.
        apply_output(
            &mut output,
            &changes(&[(1, 2, 4, -1), (3, 1, 8, -1), (1, 2, 8, 1), (3, 1, 4, 1)]),
            maintenance_policy(),
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(
            scores(&output),
            if distinct { vec![-8, -9] } else { vec![-4, -8] }
        );
        apply_output(
            &mut output,
            &changes(&[(3, 1, 4, -1), (4, 7, 4, -1)]),
            maintenance_policy(),
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(scores(&output), vec![-9, -11]);
        let before_page = output.ranked.as_ref().unwrap().page.clone();
        apply_output(&mut output, &ZSet::new(), maintenance_policy(), &mut || {
            Ok(())
        })
        .unwrap();
        assert_eq!(output.ranked.as_ref().unwrap().page, before_page);
    }
}

#[test]
fn computed_output_errors_do_not_hide_behind_zero_limits_or_off_page_groups() {
    for count in [Some(0), Some(1)] {
        let expression = GraphIntegerExpression::prepare(&[
            Op::Literal(Some(1)),
            Op::Column(2),
            Op::Binary(GraphIntegerBinary::Divide),
        ])
        .unwrap();
        let definition = maintained_definition(false, 0, count)
            .with_output_projection(vec![GraphSetProjection::new(
                "score",
                GraphSetValue::Integer(expression),
            )])
            .unwrap();
        let mut actual = Output::new(definition.clone());
        let mut before = Output::new(definition);
        let seed = changes(&[(1, 1, 1, 1)]);
        for output in [&mut actual, &mut before] {
            apply_output(output, &seed, maintenance_policy(), &mut || Ok(())).unwrap();
        }
        let failed = apply_output(
            &mut actual,
            &changes(&[(2, 1, 0, 1)]),
            maintenance_policy(),
            &mut || Ok(()),
        );
        assert!(matches!(
            failed,
            Err(StandingQueryFailure::OutputExpression { column: 0, .. })
        ));
        same_output(&actual, &before);
        apply_output(
            &mut actual,
            &changes(&[(2, 1, 2, 1)]),
            maintenance_policy(),
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(actual.row_count, u128::from(count.unwrap()));
    }
}
