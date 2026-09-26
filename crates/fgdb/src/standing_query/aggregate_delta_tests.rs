//! Retained final aggregate derivatives, including output transforms and
//! refusal/retry boundaries. No result is reconstructed from graph mutations.

use super::*;
use crate::{DatabaseKeys, StandingQueryError, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{RelationId, ZWeight};
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, IntegerComparison};
use fgdb_gql::{
    GraphAggregate, GraphAggregateColumn, GraphAggregateFilter, GraphAggregateOrder,
    GraphAggregateTest,
};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32])
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn definition(kind: usize) -> PreparedGraphAggregate {
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
    let query = PreparedGraphAggregate::prepare(
        input,
        &[0],
        &[
            GraphAggregate::count_rows("copies"),
            GraphAggregate::sum_int("total", 1),
            GraphAggregate::average_int("average", 1),
        ],
        0,
        (kind == 3).then_some(2),
    )
    .unwrap();
    match kind {
        1 => query.with_key_output_columns(&[]).unwrap(),
        2 => query
            .with_key_output_columns(&[])
            .unwrap()
            .with_distinct_output(true),
        3 => query
            .with_result_clauses(
                &[],
                &[GraphAggregateOrder::descending(
                    GraphAggregateColumn::Aggregate(1),
                )],
            )
            .unwrap(),
        4 => query
            .with_result_clauses(
                &[GraphAggregateFilter {
                    column: GraphAggregateColumn::Aggregate(0),
                    test: GraphAggregateTest::Integer {
                        comparison: IntegerComparison::Greater,
                        value: 1,
                    },
                }],
                &[],
            )
            .unwrap(),
        _ => query,
    }
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, group, value) in [(1, 1, 3), (2, 1, 5), (3, 2, 3), (4, 2, 5), (5, 3, 9)] {
        batch.create_vertex(
            VId(id),
            vec![],
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(group)),
                (PropertyKeyId(2), CanonicalScalar::Int(value)),
            ],
        );
    }
    batch
}
fn change() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    batch.delete_vertex(VId(1));
    batch.set_vertex_property(VId(3), PropertyKeyId(2), Some(CanonicalScalar::Int(20)));
    batch.set_vertex_property(VId(5), PropertyKeyId(1), Some(CanonicalScalar::Int(2)));
    batch
}
fn clone_bag<T: Ord + Clone>(rows: &ZSet<T>) -> ZSet<T> {
    rows.checked_clone(LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
        .unwrap()
}

#[test]
fn final_deltas_integrate_plain_having_hidden_distinct_and_ranked_outputs() {
    let ((), report) = run_async_under_lab(0x006d_de01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let mut handles = Vec::new();
        let mut bags = Vec::new();
        for kind in 0..5 {
            let handle = db
                .register_standing_query(&cx, definition(kind), policy())
                .unwrap();
            assert!(db.standing_query_delta(&cx, &handle).unwrap().is_none());
            bags.push(clone_bag(db.standing_query(&cx, &handle).unwrap().rows()));
            handles.push(handle);
        }
        for tick in 0..3 {
            let batch = if tick == 0 {
                change()
            } else {
                let mut batch = WriteBatch::new(RelationId(1));
                batch.set_vertex_property(
                    VId(2),
                    PropertyKeyId(99),
                    Some(CanonicalScalar::Int(tick)),
                );
                batch
            };
            let at = db.write(&commit, batch).await.unwrap();
            for (index, handle) in handles.iter().enumerate() {
                let view = db.standing_query_delta(&cx, handle).unwrap().unwrap();
                assert_eq!(view.frontier(), at);
                assert!(view.ordered_rows().is_none());
                if tick != 0 {
                    assert!(view.rows().is_empty());
                }
                bags[index]
                    .integrate(view.rows(), LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
                    .unwrap();
                assert_eq!(&bags[index], db.standing_query(&cx, handle).unwrap().rows());
                assert_eq!(
                    view.rows(),
                    db.standing_query_delta(&cx, handle)
                        .unwrap()
                        .unwrap()
                        .rows()
                );
            }
        }
        for handle in &handles {
            let before = clone_bag(
                db.standing_query_delta(&cx, handle)
                    .unwrap()
                    .unwrap()
                    .rows(),
            );
            assert!(
                db.rebuild_standing_query(&cx, handle, GqlQueryPolicy::new(100_000, 100_000, 0, 0))
                    .is_err()
            );
            assert_eq!(
                &before,
                db.standing_query_delta(&cx, handle)
                    .unwrap()
                    .unwrap()
                    .rows()
            );
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            assert!(db.standing_query_delta(&cx, handle).unwrap().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn projected_output_refusal_never_replaces_the_accepted_source_derivative() {
    let ((), report) = run_async_under_lab(0x006d_de02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut basis = Database::open_memory(&commit, keys()).await.unwrap();
        basis.write(&commit, seed()).await.unwrap();
        let mut driver = Database::open_memory(&commit, keys()).await.unwrap();
        driver.write(&commit, seed()).await.unwrap();
        let at = driver.write(&commit, change()).await.unwrap();
        let delta = driver.delta_index().unwrap().get(at).unwrap().clone();
        for kind in [1, 2, 3] {
            let make = || {
                let definition = definition(kind);
                let producer = definition.incremental_source_definition().unwrap();
                let mut output = super::super::output::State::new(definition);
                let source = basis
                    .prepare_standing_query_with_output(&cx, producer, policy(), Some(&mut output))
                    .unwrap();
                (source, output)
            };
            let (before, before_output) = make();
            let (mut success, mut result) = make();
            let mut calls = 0;
            {
                let mut checkpoint = || {
                    calls += 1;
                    Ok(())
                };
                let mut meter = Meter {
                    policy: policy(),
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                success
                    .maintain_with_output(&delta, &mut meter, Some(&mut result))
                    .unwrap();
            }
            let mut integrated = clone_bag(&before_output.rows);
            integrated
                .integrate(
                    success.last_delta.as_ref().unwrap(),
                    LimbLimit::new(4),
                    &mut |_| Ok::<_, ()>(()),
                )
                .unwrap();
            assert_eq!(integrated, result.rows);
            for stop in 1..=calls {
                let (mut candidate, mut output) = make();
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
                        candidate.maintain_with_output(&delta, &mut meter, Some(&mut output)),
                        Err(StandingQueryFailure::Interrupted)
                    );
                }
                assert_eq!(seen, stop);
                assert_eq!(candidate.last_delta, before.last_delta);
                assert_eq!(candidate.rows, before.rows);
                assert_eq!(output.rows, before_output.rows);
                let mut checkpoint = || Ok(());
                let mut meter = Meter {
                    policy: policy(),
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                candidate
                    .maintain_with_output(&delta, &mut meter, Some(&mut output))
                    .unwrap();
                assert_eq!(candidate.last_delta, success.last_delta);
                assert_eq!(output.rows, result.rows);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn aggregate_baseline_empty_ticks_and_failed_views_are_distinct() {
    let ((), report) = run_async_under_lab(0x006d_de03, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let handle = db
            .register_standing_query(&cx, definition(0), policy())
            .unwrap();
        assert!(db.standing_query_delta(&cx, &handle).unwrap().is_none());
        db.write(&commit, seed()).await.unwrap();
        assert!(
            db.standing_query_delta(&cx, &handle)
                .unwrap()
                .unwrap()
                .rows()
                .iter()
                .all(|(_, w)| w == &ZWeight::ONE)
        );
        let mut invalid = WriteBatch::new(RelationId(1));
        invalid.set_vertex_property(VId(1), PropertyKeyId(2), Some(CanonicalScalar::Bool(true)));
        db.write(&commit, invalid).await.unwrap();
        assert!(matches!(
            db.standing_query_delta(&cx, &handle),
            Err(StandingQueryError::Unavailable { .. })
        ));
        let foreign = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(
            foreign.standing_query_delta(&cx, &handle),
            Err(StandingQueryError::ForeignHandle)
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
