//! Every failure boundary must leave the retained rank indexes and page intact.

use super::*;
use super::super::State as Output;
use crate::standing_query::StandingQueryStats;
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::{GqlQueryPolicy, GraphAggregate, GraphAggregateValue,
    GraphIntegerExpression, GraphIntegerOp as Op, GraphSetProjection, GraphSetValue};
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, IntegerComparison};
use fgdb_types::CanonicalScalar;

fn definition(distinct: bool) -> PreparedGraphAggregate {
    let mut source = GraphPatternBuilder::new();
    source.vertex("n").unwrap();
    let input = source.prepare_values(&[
        GraphColumn::property("key", "n", PropertyKeyId(1)),
        GraphColumn::property("score", "n", PropertyKeyId(2)),
    ], 0, None).unwrap().with_duplicates();
    let value = GraphIntegerExpression::prepare_scalar(&[
        Op::Column(0), Op::Literal(Some(1)), Op::Compare(IntegerComparison::Equal),
        Op::Column(1), Op::Literal(Some(2)), Op::Case,
    ]).unwrap();
    PreparedGraphAggregate::prepare(input, &[0], &[
        GraphAggregate::count_rows("count"), GraphAggregate::sum_int("score_sum", 1),
    ], 0, Some(2)).unwrap()
        .with_result_clauses(&[], &[GraphAggregateOrder::descending(GraphAggregateColumn::Aggregate(1))]).unwrap()
        .with_output_projection(vec![GraphSetProjection::new("value", GraphSetValue::Integer(value))]).unwrap()
        .with_distinct_output(distinct)
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000, 2, 10_000_000, 10_000_000) }

fn delta(rows: &[(i64, u64, i128, i128)]) -> ZSet<GraphAggregateRow> {
    let source = definition(false).incremental_source_definition().unwrap();
    ZSet::from_updates(rows.iter().map(|&(key, count, sum, sign)| (
        source.materialize_incremental_row(vec![GraphValue::Scalar(CanonicalScalar::Int(key))],
            vec![GraphAggregateValue::Count(count), GraphAggregateValue::Integer(sum)]).unwrap(),
        ZWeight::from_i128(sign),
    )), LimbLimit::new(4), &mut |_| Ok::<_, ()>(())).unwrap()
}
fn apply(output: &mut Output, delta: &ZSet<GraphAggregateRow>, policy: GqlQueryPolicy,
    checkpoint: &mut dyn FnMut() -> Result<(), StandingQueryFailure>)
    -> Result<StandingQueryStats, StandingQueryFailure>
{
    let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint };
    output.prepare(delta, &mut meter)?.commit();
    Ok(meter.stats)
}
fn seeded(distinct: bool) -> Output {
    let mut output = Output::new(definition(distinct));
    apply(&mut output, &delta(&[(1, 2, 4, 1), (2, 1, 9, 1), (3, 1, 8, 1)]),
        policy(), &mut || Ok(())).unwrap();
    output
}
fn entries(rows: &Ranked) -> Vec<(GraphAggregateRow, GraphAggregateRow)> {
    rows.iter().map(|(rank, value)| (rank.group.as_ref().clone(), value.as_ref().clone())).collect()
}
fn same(actual: &Output, expected: &Output) {
    assert_eq!(actual.rows, expected.rows);
    assert_eq!(actual.row_count, expected.row_count);
    let a = actual.ranked.as_ref().unwrap();
    let b = expected.ranked.as_ref().unwrap();
    assert_eq!(a.order, b.order);
    // Rank equality deliberately ignores unreferenced summaries. Compare the
    // COMPLETE rows here to detect stale keys retained by equal-key insertion.
    let groups = |state: &State| state.groups.iter()
        .map(|(key, rank)| (key.clone(), rank.group.clone())).collect::<Vec<_>>();
    let classes = |state: &State| state.classes.iter()
        .map(|(key, members)| (key.clone(), entries(members))).collect::<Vec<_>>();
    assert_eq!(groups(a), groups(b));
    assert_eq!(classes(a), classes(b));
    assert_eq!(entries(&a.candidates), entries(&b.candidates));
    assert_eq!(a.page, b.page);
}

#[test]
fn every_rank_checkpoint_budget_and_dropped_guard_preserves_all_indexes_then_retries() {
    let change = delta(&[(1, 2, 4, -1), (2, 1, 9, -1), (1, 3, 12, 1), (4, 1, 11, 1)]);
    for distinct in [false, true] {
        let before = seeded(distinct);
        let mut complete = seeded(distinct);
        let mut calls = 0;
        let stats = apply(&mut complete, &change, policy(), &mut || { calls += 1; Ok(()) }).unwrap();
        assert_eq!(complete.row_count, 2);
        assert!(calls > 0);
        for stop in 1..=calls {
            let mut candidate = seeded(distinct);
            let mut seen = 0;
            assert_eq!(apply(&mut candidate, &change, policy(), &mut || {
                seen += 1;
                if seen == stop { Err(StandingQueryFailure::Interrupted) } else { Ok(()) }
            }), Err(StandingQueryFailure::Interrupted));
            assert_eq!(seen, stop);
            same(&candidate, &before);
            let mut checkpoint = || Ok(());
            let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            drop(candidate.prepare(&change, &mut meter).unwrap());
            same(&candidate, &before);
            apply(&mut candidate, &change, policy(), &mut || Ok(())).unwrap();
            same(&candidate, &complete);
        }
        for reason in [StandingQueryFailure::WorkBudget, StandingQueryFailure::ScratchBudget,
            StandingQueryFailure::ResultBudget]
        {
            let mut candidate = seeded(distinct);
            let mut bounded = policy();
            match reason {
                StandingQueryFailure::WorkBudget => bounded.evaluator.max_work_units = stats.work_units - 1,
                StandingQueryFailure::ScratchBudget => bounded.evaluator.max_scratch_entries = stats.scratch_entries - 1,
                _ => bounded = GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000),
            }
            assert_eq!(apply(&mut candidate, &change, bounded, &mut || Ok(())), Err(reason));
            same(&candidate, &before);
            apply(&mut candidate, &change, policy(), &mut || Ok(())).unwrap();
            same(&candidate, &complete);
        }
    }
}

#[test]
fn unchanged_rank_refreshes_full_rows_and_bad_before_images_never_publish() {
    for distinct in [false, true] {
        let mut output = seeded(distinct);
        // Group 2's output is constant and its sum/rank is unchanged.
        apply(&mut output, &delta(&[(2, 1, 9, -1), (2, 7, 9, 1)]), policy(), &mut || Ok(())).unwrap();
        let current = output.ranked.as_ref().unwrap().groups.values()
            .find(|rank| rank.group.keys() == [GraphValue::Scalar(CanonicalScalar::Int(2))]).unwrap();
        assert_eq!(current.group.get(0).unwrap().as_count(), Some(7));
        for malformed in [delta(&[(2, 1, 9, -1)]), delta(&[(2, 9, 10, 1)]),
            delta(&[(4, 1, 5, 1), (4, 2, 6, 1)])]
        {
            let mut original = seeded(distinct);
            apply(&mut original, &delta(&[(2, 1, 9, -1), (2, 7, 9, 1)]), policy(), &mut || Ok(())).unwrap();
            assert_eq!(apply(&mut output, &malformed, policy(), &mut || Ok(())), Err(StandingQueryFailure::InvalidDelta));
            same(&output, &original);
        }
        apply(&mut output, &delta(&[(2, 7, 9, -1)]), policy(), &mut || Ok(())).unwrap();
        let mut expected = seeded(distinct);
        apply(&mut expected, &delta(&[(2, 1, 9, -1)]), policy(), &mut || Ok(())).unwrap();
        same(&output, &expected);
    }
}

#[test]
fn whole_ranked_maintenance_aborts_source_result_and_page_at_every_checkpoint() {
    use asupersync::lab::run_async_under_lab;
    use crate::{Database, DatabaseKeys, WriteBatch};
    use fgdb_delta_types::RelationId;
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts, VId};
    let ((), report) = run_async_under_lab(0x6a97, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let keys = || DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32]);
        let seed = || {
            let mut batch = WriteBatch::new(RelationId(1));
            for (id, key, score) in [(1,1,2), (11,1,2), (2,2,9), (3,3,8)] {
                batch.create_vertex(VId(id), vec![], vec![
                    (PropertyKeyId(1), CanonicalScalar::Int(key)),
                    (PropertyKeyId(2), CanonicalScalar::Int(score)),
                ]);
            }
            batch
        };
        let mut basis = Database::open_memory(&commit, keys()).await.unwrap();
        basis.write(&commit, seed()).await.unwrap();
        let mut driver = Database::open_memory(&commit, keys()).await.unwrap();
        driver.write(&commit, seed()).await.unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_vertex(VId(2));
        change.set_vertex_property(VId(1), PropertyKeyId(2), Some(CanonicalScalar::Int(20)));
        change.create_vertex(VId(12), vec![], vec![
            (PropertyKeyId(1), CanonicalScalar::Int(1)), (PropertyKeyId(2), CanonicalScalar::Int(0)),
        ]);
        let at = driver.write(&commit, change).await.unwrap();
        let delta = driver.delta_index().unwrap().get(at).unwrap().clone();
        let make = || {
            let definition = definition(true);
            let raw = definition.incremental_source_definition().unwrap();
            let mut output = Output::new(definition);
            let source = basis.prepare_standing_query_with_output(&query, raw, policy(), Some(&mut output)).unwrap();
            (source, output)
        };
        let (before, initial) = make();
        let (mut complete, mut final_output) = make();
        let mut calls = 0;
        let stats = {
            let mut checkpoint = || { calls += 1; Ok(()) };
            let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            complete.maintain_with_output(&delta, &mut meter, Some(&mut final_output)).unwrap();
            meter.stats
        };
        assert_eq!(initial.row_count, 1);
        assert_eq!(final_output.row_count, 2);
        assert_eq!(final_output.ordered_rows().unwrap()[0].get(0).unwrap().as_count(), Some(3));
        for stop in 1..=calls {
            let (mut source, mut output) = make();
            let mut seen = 0;
            {
                let mut checkpoint = || {
                    seen += 1; if seen == stop { Err(StandingQueryFailure::Interrupted) } else { Ok(()) }
                };
                let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
                assert_eq!(source.maintain_with_output(&delta, &mut meter, Some(&mut output)),
                    Err(StandingQueryFailure::Interrupted));
            }
            assert_eq!(seen, stop);
            assert_eq!(source.rows, before.rows);
            assert_eq!(source.frontier, before.frontier);
            same(&output, &initial);
            let mut checkpoint = || Ok(());
            let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            source.maintain_with_output(&delta, &mut meter, Some(&mut output)).unwrap();
            assert_eq!(source.rows, complete.rows);
            same(&output, &final_output);
        }
        for reason in [StandingQueryFailure::WorkBudget, StandingQueryFailure::ScratchBudget,
            StandingQueryFailure::ResultBudget]
        {
            let (mut source, mut output) = make();
            let mut bounded = policy();
            match reason {
                StandingQueryFailure::WorkBudget => bounded.evaluator.max_work_units = stats.work_units - 1,
                StandingQueryFailure::ScratchBudget => bounded.evaluator.max_scratch_entries = stats.scratch_entries - 1,
                _ => bounded = GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000),
            }
            let mut checkpoint = || Ok(());
            let mut meter = Meter { policy: bounded, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            assert_eq!(source.maintain_with_output(&delta, &mut meter, Some(&mut output)), Err(reason));
            assert_eq!(source.rows, before.rows);
            same(&output, &initial);
            let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            source.maintain_with_output(&delta, &mut meter, Some(&mut output)).unwrap();
            assert_eq!(source.rows, complete.rows);
            same(&output, &final_output);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
