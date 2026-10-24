//! Regression laws for admission and consolidation at the endpoint boundary.
//! Source fixtures deliberately report zero evaluator work: these tests isolate
//! adaptation/consolidation from the independently metered native source engine.
use super::*;
use fgdb_types::CanonicalScalar;
use std::collections::BTreeMap;

fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}

fn execution<T>(value: Vec<T>) -> GqlQueryExecution<T> {
    let result_rows = value.len() as u64;
    GqlQueryExecution {
        value,
        rows: GqlExecutionStats {
            snapshot_records: 0,
            result_rows,
        },
        evaluator: GlaExecutionStats::default(),
    }
}

fn row(value: i64) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![GraphValue::Scalar(CanonicalScalar::Int(value))])
}

fn run(
    before: &[GraphValueRow],
    after: &[GraphValueRow],
    policy: GqlQueryPolicy,
    checkpoint: impl FnMut() -> Result<(), usize>,
) -> Result<GraphResultDiff, Error<&'static str, usize>> {
    GraphResultDiff::execute(
        CommitSeq(4),
        CommitSeq(9),
        vec!["v".to_owned()],
        policy,
        |endpoint, _| {
            Ok(GraphDiffInput::values(execution(
                match endpoint {
                    DiffEndpoint::Before => before,
                    DiffEndpoint::After => after,
                }
                .to_vec(),
            )))
        },
        checkpoint,
    )
}

fn observed(result: &GraphResultDiff) -> BTreeMap<i64, i128> {
    result
        .changes()
        .iter()
        .map(|(row, weight)| {
            let GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value))) =
                row[0]
            else {
                panic!("integer fixture required");
            };
            (value, weight.to_i128().unwrap())
        })
        .collect()
}

#[test]
fn before_consolidation_is_charged_before_the_second_source_is_admitted() {
    // Zero-column rows isolate exact entry accounting without payload copies.
    // Each occurrence spends 3 work: row storage, kernel visit, then either
    // new-key storage or exact addition. Initial schema and endpoint work is 4.
    for n in [1_usize, 4, 64] {
        let policy = GqlQueryPolicy::new(0, 1, 10_000, 10_000);
        let mut second = None;
        let result: Result<_, Error<&str, usize>> = GraphResultDiff::execute(
            CommitSeq(1),
            CommitSeq(2),
            Vec::new(),
            policy,
            |endpoint, remaining| {
                let rows = if endpoint == DiffEndpoint::Before {
                    vec![GraphValueRow::from_owned_values(vec![]); n]
                } else {
                    second = Some(remaining);
                    vec![]
                };
                Ok(GraphDiffInput::values(execution(rows)))
            },
            || Ok(()),
        );
        let result = result.unwrap();
        let remaining = second.unwrap();
        assert_eq!(remaining.rows.max_result_rows(), None);
        assert_eq!(remaining.rows.max_snapshot_records(), Some(0));
        assert_eq!(
            remaining.evaluator.max_work_units,
            10_000 - (4 + 3 * n as u64)
        );
        assert_eq!(
            remaining.evaluator.max_scratch_entries,
            10_000 - (n as u64 + 2)
        );
        assert_eq!(result.changes().len(), 1);
        assert_eq!(
            result.changes().iter().next().unwrap().1.to_i128(),
            Some(-(n as i128))
        );
    }
}

#[test]
fn insufficient_before_consolidation_budget_never_invokes_after() {
    let mut visited = Vec::new();
    let result: Result<_, Error<&str, usize>> = GraphResultDiff::execute(
        CommitSeq(1),
        CommitSeq(2),
        Vec::new(),
        GqlQueryPolicy::new(0, 1, 12, u64::MAX),
        |endpoint, _| {
            visited.push(endpoint);
            Ok(GraphDiffInput::values(execution(
                vec![GraphValueRow::from_owned_values(vec![]); 4],
            )))
        },
        || Ok(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Evaluator(GlaLimitExceeded {
            dimension: GlaLimitDimension::WorkUnits,
            limit: 12,
            observed: 13,
        }))
    ));
    // With deferred consolidation, the second source had already run under a
    // budget that did not include the first source's pending exact additions.
    assert_eq!(visited, [DiffEndpoint::Before]);
}

#[test]
fn duplicate_occurrences_need_no_second_staging_bag_or_its_scratch_reservation() {
    let n = 16_384_usize;
    let work = 6 + 3 * n as u64;
    let scratch = n as u64 + 2;
    let execute = |policy| {
        GraphResultDiff::execute(
            CommitSeq(1),
            CommitSeq(2),
            Vec::new(),
            policy,
            |endpoint, _| {
                Ok::<_, GqlQueryError<&str, usize>>(GraphDiffInput::values(execution(
                    if endpoint == DiffEndpoint::Before {
                        vec![GraphValueRow::from_owned_values(vec![]); n]
                    } else {
                        vec![]
                    },
                )))
            },
            || Ok(()),
        )
    };
    let result = execute(GqlQueryPolicy::new(0, 1, work, scratch)).unwrap();
    assert_eq!(result.evaluator_stats().work_units, work);
    assert_eq!(result.evaluator_stats().scratch_entries, scratch);
    assert_eq!(result.changes().len(), 1);
    assert_eq!(
        result.changes().iter().next().unwrap().1.to_i128(),
        Some(-(n as i128))
    );
    for policy in [
        GqlQueryPolicy::new(0, 1, work - 1, scratch),
        GqlQueryPolicy::new(0, 1, work, scratch - 1),
    ] {
        assert!(execute(policy).is_err());
    }
    // This is a logical accounting regression, not a measured RSS assertion.
    // The source's original Vec and per-row adaptation are still admitted.
}

#[test]
fn arbitrary_occurrence_orders_match_independent_counts_and_cancel_under_zero_rows() {
    // Three keys, 0..=2 occurrences per key on each endpoint: 729 bag pairs.
    // Reverse/rotate the visits; the oracle is scalar arithmetic, not ZSet.
    for mut code in 0..729_u32 {
        let mut counts = [0_usize; 6];
        for count in &mut counts {
            *count = (code % 3) as usize;
            code /= 3;
        }
        let make = |offset: usize| {
            (0..3)
                .flat_map(|key| std::iter::repeat_n(row(key as i64), counts[offset + key]))
                .collect::<Vec<_>>()
        };
        let mut before = make(0);
        let mut after = make(3);
        let expected: BTreeMap<_, _> = (0..3)
            .filter_map(|key| {
                let weight = counts[key + 3] as i128 - counts[key] as i128;
                (weight != 0).then_some((key as i64, weight))
            })
            .collect();
        for order in 0..3 {
            if order == 1 {
                before.reverse();
                after.reverse();
            } else if order == 2 {
                if before.len() > 1 {
                    before.rotate_left(1);
                }
                if after.len() > 1 {
                    after.rotate_right(1);
                }
            }
            let result = run(&before, &after, wide(), || Ok(())).unwrap();
            assert_eq!(observed(&result), expected);
            assert_eq!(result.row_stats().result_rows, expected.len() as u64);
        }
        let identical = run(
            &before,
            &before,
            GqlQueryPolicy::new(0, 0, u64::MAX, u64::MAX),
            || Ok(()),
        )
        .unwrap();
        assert!(identical.changes().is_empty());
    }
}

#[test]
fn aggregate_output_slot_consolidation_preserves_repeats_and_ignores_hidden_cells() {
    let slots = [
        GraphAggregateTextSlot::Aggregate(0),
        GraphAggregateTextSlot::Aggregate(0),
    ];
    // Changes only to a non-returned cell must cancel, even under a zero-row
    // allowance. Repeated slots are part of each whole row, not separate keys.
    let result: Result<_, Error<&str, usize>> = GraphResultDiff::execute(
        CommitSeq(1),
        CommitSeq(2),
        vec!["a".into(), "again".into()],
        GqlQueryPolicy::new(0, 0, u64::MAX, u64::MAX),
        |endpoint, _| {
            let hidden = if endpoint == DiffEndpoint::Before {
                7
            } else {
                19
            };
            let rows = (0..64)
                .map(|_| {
                    GraphAggregateRow::from_global_values(vec![
                        GraphAggregateValue::Integer(i128::MAX),
                        GraphAggregateValue::Count(hidden),
                    ])
                })
                .collect();
            Ok(GraphDiffInput::aggregates(execution(rows), &slots, 0, 2))
        },
        || Ok(()),
    );
    assert!(result.unwrap().changes().is_empty());
}

#[test]
fn every_interruption_or_unwind_discards_the_private_partial_relation() {
    let before = vec![row(1), row(1), row(2), row(7)];
    let after = vec![row(7), row(2), row(2), row(9)];
    let mut calls = 0;
    let expected = run(&before, &after, wide(), || {
        calls += 1;
        Ok(())
    })
    .unwrap();
    for stop in 1..=calls {
        let mut seen = 0;
        let result = run(&before, &after, wide(), || {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert_eq!(result.unwrap_err(), GqlQueryError::Interrupted(stop));
        assert_eq!(seen, stop);
        let mut seen = 0;
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = run(&before, &after, wide(), || {
                seen += 1;
                assert_ne!(seen, stop, "injected checkpoint unwind");
                Ok(())
            });
        }));
        assert!(unwound.is_err());
        assert_eq!(seen, stop);
        assert_eq!(run(&before, &after, wide(), || Ok(())).unwrap(), expected);
    }
}

#[test]
fn late_source_or_row_shape_failures_do_not_turn_partial_consolidation_into_success() {
    for source_error in [false, true] {
        let mut visited = Vec::new();
        let result: Result<_, Error<&str, usize>> = GraphResultDiff::execute(
            CommitSeq(1),
            CommitSeq(2),
            vec!["v".into()],
            wide(),
            |endpoint, _| {
                visited.push(endpoint);
                if endpoint == DiffEndpoint::After {
                    if source_error {
                        return Err(GqlQueryError::Source("late source failure"));
                    }
                    return Ok(GraphDiffInput::values(execution(vec![
                        row(1), // already cancels part of the first endpoint
                        GraphValueRow::from_owned_values(vec![]),
                    ])));
                }
                Ok(GraphDiffInput::values(execution(vec![row(1); 64])))
            },
            || Ok(()),
        );
        if source_error {
            assert!(matches!(
                result,
                Err(GqlQueryError::Source(GraphDiffError::Endpoint {
                    endpoint: DiffEndpoint::After,
                    source: "late source failure",
                }))
            ));
        } else {
            assert!(matches!(
                result,
                Err(GqlQueryError::Source(GraphDiffError::RowWidth {
                    endpoint: DiffEndpoint::After,
                    expected: 1,
                    observed: 0,
                }))
            ));
        }
        assert_eq!(visited, [DiffEndpoint::Before, DiffEndpoint::After]);
    }
}
