use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, VertexPredicate};
use fgdb_gql::stream::VertexScanState;
use fgdb_gql::{GraphAggregate, PreparedGraphAggregate};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};
use std::cell::Cell;
use std::sync::Arc;

const LABEL: LabelId = LabelId(3);
const KEY: PropertyKeyId = PropertyKeyId(7);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x81; 32],
        DatabaseSecurityNamespaceId([0x82; 32]),
        [0x83; 32],
    )
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn definition() -> PreparedGraphAggregate {
    let mut input = GraphPatternBuilder::new();
    input.vertex("n").unwrap();
    input.filter("n", VertexPredicate::HasLabel(LABEL)).unwrap();
    let input = input
        .prepare_values(&[GraphColumn::property("value", "n", KEY)], 0, None)
        .unwrap()
        .with_duplicates();
    PreparedGraphAggregate::prepare(
        input,
        &[],
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::count("nonnull", 0),
            GraphAggregate::sum_int("sum", 0),
        ],
        0,
        None,
    )
    .unwrap()
}
fn seed() -> WriteBatch {
    let mut seed = WriteBatch::new(RelationId(1));
    seed.create_vertex(VId(0), vec![LABEL], vec![(KEY, CanonicalScalar::Int(5))]);
    seed.create_vertex(VId(1), vec![LABEL], vec![(KEY, CanonicalScalar::Int(-2))]);
    seed.create_vertex(VId(2), vec![LABEL], vec![(KEY, CanonicalScalar::Null)]);
    seed.create_vertex(VId(u128::MAX), vec![LABEL], vec![]);
    seed
}

#[test]
fn historical_and_pinned_global_streams_match_the_existing_snapshot_aggregate() {
    let ((), report) = run_async_under_lab(0x51ca_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let retained = db.read_session().unwrap();
        let definition = definition();
        let plan = VertexAggregatePlan::compile(&definition).unwrap();
        let frozen = retained
            .execute_graph_aggregate_governed_at(&cx, &definition, CommitSeq(1), wide())
            .unwrap()
            .value;
        let mut pinned = retained
            .stream_global_aggregate_governed(&cx, &plan, wide())
            .unwrap();
        assert_eq!(pinned.row_stats().snapshot_records, 0);
        assert_eq!(pinned.row_stats().result_rows, 0);
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(0), KEY, Some(CanonicalScalar::Int(99)));
        edit.delete_vertex(VId(1));
        edit.set_vertex_label(VId(2), LABEL, false);
        db.write(&commit, edit).await.unwrap();
        for as_of in [CommitSeq(0), CommitSeq(1), CommitSeq(2)] {
            let expected = db
                .execute_graph_aggregate_governed_at(&cx, &definition, as_of, wide())
                .unwrap()
                .value;
            let mut cursor = db
                .stream_global_aggregate_governed_at(&cx, &plan, as_of, wide())
                .unwrap();
            assert_eq!(cursor.snapshot_seq(), as_of);
            assert_eq!(
                cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected
            );
            assert_eq!(cursor.row_stats().result_rows, 1);
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
        }
        assert!(
            retained
                .stream_global_aggregate_governed_at(&cx, &plan, CommitSeq(2), wide())
                .is_err()
        );
        assert!(
            db.stream_global_aggregate_governed_at(
                &cx,
                &plan,
                CommitSeq(3),
                GqlQueryPolicy::new(0, 0, 0, 0),
            )
            .is_err()
        );
        let expected = db
            .execute_graph_aggregate_governed_at(&cx, &definition, CommitSeq(2), wide())
            .unwrap()
            .value;
        let mut current = db
            .stream_global_aggregate_governed(&cx, &plan, wide())
            .unwrap();
        drop(plan);
        drop(definition);
        drop(retained);
        drop(db);
        assert_eq!(
            pinned.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
            frozen
        );
        assert_eq!(
            current.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
            expected
        );
        assert!(pinned.next().is_none());
        assert!(current.next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn all_terminal_paths_release_the_last_generation_without_waiting_for_cursor_drop() {
    let ((), report) = run_async_under_lab(0x51ca_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        for mode in 0..4 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            db.write(&commit, seed()).await.unwrap();
            let view = db.read_session().unwrap();
            let generation = Arc::downgrade(&view.snapshot);
            let plan = VertexAggregatePlan::compile(&definition()).unwrap();
            let policy = if mode == 2 {
                GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX)
            } else {
                wide()
            };
            let mut cursor = view
                .stream_global_aggregate_governed(&cx, &plan, policy)
                .unwrap();
            drop(view);
            drop(db);
            drop(plan);
            assert!(generation.upgrade().is_some());
            match mode {
                0 => {
                    cursor.next().unwrap().unwrap();
                    assert_eq!(cursor.state(), VertexScanState::Exhausted);
                }
                1 => {
                    cursor.close();
                    assert_eq!(cursor.state(), VertexScanState::Closed);
                }
                2 => {
                    assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Rows(_)))));
                    assert_eq!(cursor.state(), VertexScanState::Failed);
                }
                _ => {
                    drop(cursor);
                    assert!(generation.upgrade().is_none());
                    continue;
                }
            }
            assert!(generation.upgrade().is_none());
            let stats = (cursor.row_stats(), cursor.evaluator_stats());
            assert!(cursor.next().is_none());
            cursor.close();
            assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), stats);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_history_checkpoint_refuses_without_releasing_partial_aggregate_state() {
    let ((), report) = run_async_under_lab(0x51ca_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        for value in [4, 8] {
            let mut edit = WriteBatch::new(RelationId(1));
            edit.set_vertex_property(VId(0), KEY, Some(CanonicalScalar::Int(value)));
            db.write(&commit, edit).await.unwrap();
        }
        let view = db.read_session().unwrap();
        let definition = definition();
        let plan = VertexAggregatePlan::compile(&definition).unwrap();
        for as_of in [CommitSeq(0), CommitSeq(1), CommitSeq(3)] {
            let expected = view
                .execute_graph_aggregate_governed_at(&cx, &definition, as_of, wide())
                .unwrap()
                .value;
            let calls = Cell::new(0_usize);
            let mut baseline = VertexAggregateCursor::new(
                SnapshotVertexSource {
                    view: view.clone(),
                    cx: &cx,
                    as_of,
                    after: None,
                },
                plan.clone(),
                wide(),
                || {
                    calls.set(calls.get() + 1);
                    Ok::<_, usize>(())
                },
            );
            assert_eq!(
                baseline.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected
            );
            let total = calls.get();
            drop(baseline);
            for stop in 1..=total {
                let pins = Arc::strong_count(&view.snapshot);
                let calls = Cell::new(0);
                let mut cursor = VertexAggregateCursor::new(
                    SnapshotVertexSource {
                        view: view.clone(),
                        cx: &cx,
                        as_of,
                        after: None,
                    },
                    plan.clone(),
                    wide(),
                    || {
                        let at = calls.get() + 1;
                        calls.set(at);
                        if at == stop { Err(stop) } else { Ok(()) }
                    },
                );
                assert_eq!(Arc::strong_count(&view.snapshot), pins + 1);
                assert!(matches!(
                    cursor.next(),
                    Some(Err(GqlQueryError::Interrupted(at))) if at == stop
                ));
                assert_eq!(cursor.row_stats().result_rows, 0);
                assert_eq!(cursor.state(), VertexScanState::Failed);
                assert_eq!(Arc::strong_count(&view.snapshot), pins);
                assert!(cursor.next().is_none());
                assert_eq!(calls.get(), stop);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
