//! The real group and output operators prepare together, including failures
//! at the registry's final checkpoint. No alternate arithmetic path is used.
use super::*;
use fgdb_delta_types::{PropertyKeyId, ZWeight};
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, GraphValue};
use fgdb_gql::{GraphAggregate, GraphAggregateColumn, GraphAggregateOrder};
use fgdb_types::CanonicalScalar;

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn definition(mode: usize) -> PreparedGraphSetAggregate {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let input = builder
        .prepare_values(
            &[
                GraphColumn::property("key", "n", PropertyKeyId(1)),
                GraphColumn::property("value", "n", PropertyKeyId(2)),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates()
        .into();
    let specs = [
        GraphAggregate::count_rows("count"),
        GraphAggregate::sum_int("sum", 1),
    ];
    let mut result = PreparedGraphSetAggregate::prepare(
        input,
        &[0],
        &specs,
        u64::from(mode == 2),
        (mode == 2).then_some(1),
    )
    .unwrap();
    if mode == 1 {
        result = result
            .with_key_output_columns(&[])
            .unwrap()
            .with_aggregate_output_prefix(1)
            .unwrap()
            .with_distinct_output(true);
    }
    if mode == 2 {
        result = result
            .with_result_clauses(
                &[],
                &[GraphAggregateOrder::descending(
                    GraphAggregateColumn::Aggregate(1),
                )],
            )
            .unwrap();
    }
    result
}
fn empty(definition: PreparedGraphSetAggregate) -> State {
    let operator = IncrementalGroupAggregate::new(
        definition.complete_groups().unwrap(),
        definition.input().column_types(),
    )
    .unwrap();
    State {
        input: 0,
        operator,
        output: output::State::new(definition),
        last_delta: None,
        policy: policy(),
        frontier: CommitSeq(0),
        stats: StandingQueryStats::default(),
        failure: None,
    }
}
fn row(key: i64, value: Option<i64>) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![
        GraphValue::Scalar(CanonicalScalar::Int(key)),
        GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int)),
    ])
}
fn z(values: impl IntoIterator<Item = (GraphValueRow, i128)>) -> ZSet<GraphValueRow> {
    ZSet::from_updates(
        values
            .into_iter()
            .map(|(row, n)| (row, ZWeight::from_i128(n))),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}
fn apply(state: &mut State, rows: &ZSet<GraphValueRow>) -> StandingQueryStats {
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: state.policy,
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    state.apply(rows, &mut meter).unwrap();
    meter.stats
}
fn copy<T: Ord + Clone>(rows: &ZSet<T>) -> ZSet<T> {
    rows.checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap()
}
fn same(a: &State, b: &State) {
    assert_eq!(a.operator, b.operator);
    assert_eq!(a.rows(), b.rows());
    assert_eq!(a.ordered_rows(), b.ordered_rows());
    assert_eq!(a.last_delta, b.last_delta);
    assert_eq!(a.frontier, b.frontier);
    assert_eq!(a.failure, b.failure);
}

#[test]
fn every_small_bag_transition_integrates_the_final_group_projection_and_rank_delta() {
    let bag = |n: usize| {
        z([(1, Some(2)), (1, None), (2, Some(7))]
            .into_iter()
            .enumerate()
            .map(|(i, (key, value))| (row(key, value), ((n / 3usize.pow(i as u32)) % 3) as i128)))
    };
    for mode in 0..3 {
        for before in 0..27 {
            for after in 0..27 {
                let old = bag(before);
                let new = bag(after);
                let mut candidate = empty(definition(mode));
                apply(&mut candidate, &old);
                let mut expected = empty(definition(mode));
                apply(&mut expected, &new);
                let changes = new.minus(&old, LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
                let mut integrated = copy(candidate.rows());
                apply(&mut candidate, &changes);
                integrated
                    .integrate(candidate.last_delta.as_ref().unwrap(), LIMBS, &mut |_| {
                        Ok::<_, ()>(())
                    })
                    .unwrap();
                assert_eq!(
                    &integrated,
                    expected.rows(),
                    "mode {mode}: {before}->{after}"
                );
                assert_eq!(candidate.operator, expected.operator);
                assert_eq!(candidate.ordered_rows(), expected.ordered_rows());
                let forward = copy(candidate.last_delta.as_ref().unwrap());
                apply(
                    &mut candidate,
                    &changes.negated(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap(),
                );
                assert_eq!(
                    candidate.last_delta.as_ref().unwrap(),
                    &forward.negated(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap()
                );
            }
        }
    }
}

#[test]
fn all_group_output_and_final_checkpoint_refusals_leave_the_whole_state_retryable() {
    let initial = z([(row(1, Some(2)), 2), (row(2, Some(7)), 1)]);
    let change = z([
        (row(1, Some(2)), -1),
        (row(2, Some(7)), -1),
        (row(3, Some(4)), 2),
    ]);
    for mode in 0..3 {
        let make = || {
            let mut s = empty(definition(mode));
            apply(&mut s, &initial);
            s
        };
        let before = make();
        let mut success = make();
        let mut calls = 0;
        let stats = {
            let mut checkpoint = || {
                calls += 1;
                Ok(())
            };
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            success.apply(&change, &mut meter).unwrap();
            meter.stats
        };
        for stop in 1..=calls {
            let mut s = make();
            let mut seen = 0;
            {
                let mut checkpoint = || {
                    seen += 1;
                    if seen == stop {
                        Err(StandingQueryFailure::Interrupted)
                    } else {
                        Ok(())
                    }
                };
                let mut meter = Meter {
                    policy: policy(),
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                assert_eq!(
                    s.apply(&change, &mut meter),
                    Err(StandingQueryFailure::Interrupted)
                );
            }
            assert_eq!(seen, stop);
            same(&s, &before);
            apply(&mut s, &change);
            same(&s, &success);
        }
        for (work, scratch) in [
            (stats.work_units - 1, stats.scratch_entries),
            (stats.work_units, stats.scratch_entries - 1),
        ] {
            let mut s = make();
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: GqlQueryPolicy::new(100_000, 100_000, work, scratch),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            assert!(s.apply(&change, &mut meter).is_err());
            same(&s, &before);
        }
        let mut s = make();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            let groups = s
                .operator
                .prepare(&change, LIMBS, None, &mut |event| meter.charge(event))
                .unwrap();
            let _output = s.output.prepare(groups.delta(), &mut meter).unwrap();
            panic!("downstream refusal after both operators prepared");
        }));
        assert!(panic.is_err());
        same(&s, &before);
        s.policy = GqlQueryPolicy::new(100_000, 100_000, stats.work_units, stats.scratch_entries);
        apply(&mut s, &change);
        same(&s, &success);
    }
}

#[test]
fn hidden_output_does_not_mask_native_count_overflow_or_noninteger_data() {
    let definition = definition(0).with_result_clauses(&[], &[]).unwrap();
    // Build the same complete definition with an explicit zero output window.
    let definition = PreparedGraphSetAggregate::prepare(
        definition.input().clone(),
        &[0],
        &[
            GraphAggregate::count_rows("count"),
            GraphAggregate::sum_int("sum", 1),
        ],
        0,
        Some(0),
    )
    .unwrap();
    let mut s = empty(definition.clone());
    let mut before = empty(definition);
    let initial = z([(row(1, Some(1)), i128::from(u64::MAX))]);
    apply(&mut s, &initial);
    apply(&mut before, &initial);
    assert!(s.rows().is_empty());
    for (change, expected) in [
        (z([(row(1, Some(1)), 1)]), StandingQueryFailure::Arithmetic),
        (
            z([(
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Scalar(CanonicalScalar::Int(2)),
                    GraphValue::Scalar(CanonicalScalar::Bool(true)),
                ]),
                1,
            )]),
            StandingQueryFailure::NonIntegerAggregate { column: 1 },
        ),
        (
            z([(row(9, Some(1)), -1)]),
            StandingQueryFailure::InvalidDelta,
        ),
    ] {
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: policy(),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        assert_eq!(s.apply(&change, &mut meter), Err(expected));
        same(&s, &before);
    }
}

#[test]
fn unranked_group_updates_do_not_visit_unaffected_groups_or_expand_occurrences() {
    let mut observed = Vec::new();
    for n in [2, 2048] {
        let mut s = empty(definition(0));
        apply(&mut s, &z((0..n).map(|key| (row(key, Some(3)), 2))));
        let stats = apply(&mut s, &z([(row(0, Some(3)), -1), (row(0, Some(5)), 1)]));
        observed.push((stats.work_units, stats.scratch_entries));
    }
    assert_eq!(observed[0], observed[1]);
}
