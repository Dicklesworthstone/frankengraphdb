//! Real snapshot-backed pull execution: no artifact or complete result is built.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, ReadError, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GraphPatternBuilder, IntegerComparison, PreparedGraphPattern, VertexPredicate};
use fgdb_gql::stream::{VertexScanError, VertexScanState};
use fgdb_gql::{GqlBudgetDimension, GqlExecutionStats, GqlQueryError, GqlQueryPolicy, PreparedGqlQuery, RelationBind};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const LABEL: LabelId = LabelId(3);
const KEY: PropertyKeyId = PropertyKeyId(7);
const R: RelationId = RelationId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x81; 32], DatabaseSecurityNamespaceId([0x82; 32]), [0x83; 32])
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX) }
fn pattern(offset: u64, count: Option<u64>, filter: bool) -> PreparedGraphPattern {
    let mut builder = GraphPatternBuilder::new(); builder.vertex("n").unwrap();
    if filter {
        builder.filter("n", VertexPredicate::HasLabel(LABEL)).unwrap();
        builder.filter("n", VertexPredicate::IntegerProperty { key: KEY, comparison: IntegerComparison::GreaterOrEqual, value: 0 }).unwrap();
    }
    builder.prepare("n", offset, count).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for (vid, value) in [(0, 0), (1, -1), (2, 2), (3, 3), (4, 4), (u128::MAX, 5)] {
        batch.create_vertex(VId(vid), if vid == 3 { vec![] } else { vec![LABEL] }, vec![(KEY, CanonicalScalar::Int(value))]);
    }
    db.write(cx, batch).await.unwrap()
}

#[test]
fn streams_pin_history_without_borrowing_the_database_view_or_prepared_definition() {
    let ((), report) = run_async_under_lab(0x51ca_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let prepared = pattern(0, None, true);
        let expected = db.execute_graph_pattern_governed(&cx, &prepared, wide()).unwrap().value;
        assert_eq!(expected, vec![VId(0), VId(2), VId(4), VId(u128::MAX)]);
        let mut live_cursor = db.stream_graph_vertices_governed(&cx, &prepared, wide()).unwrap();
        let view = db.read_session().unwrap();
        let pinned_cursor = view.stream_graph_vertices_governed(&cx, &prepared, wide()).unwrap();
        drop(view); drop(prepared); // The public return types must not capture these borrows.
        assert_eq!(live_cursor.snapshot_seq(), basis);
        assert_eq!(live_cursor.row_stats(), GqlExecutionStats { snapshot_records: 0, result_rows: 0 });
        assert_eq!(live_cursor.next().unwrap().unwrap(), VId(0));

        let mut changes = WriteBatch::new(R);
        changes.set_vertex_property(VId(2), KEY, Some(CanonicalScalar::Int(-9)));
        changes.set_vertex_property(VId(1), KEY, Some(CanonicalScalar::Int(10)));
        changes.set_vertex_label(VId(3), LABEL, true);
        changes.delete_vertex(VId(4));
        changes.create_vertex(VId(5), vec![LABEL], vec![(KEY, CanonicalScalar::Int(5))]);
        db.write(&commit, changes).await.unwrap(); // Must compile with the cursors still alive.
        let prepared = pattern(0, None, true);
        let current = db.execute_graph_pattern_governed(&cx, &prepared, wide()).unwrap().value;
        assert_eq!(current, vec![VId(0), VId(1), VId(3), VId(5), VId(u128::MAX)]);
        let historical = db.stream_graph_vertices_governed_at(&cx, &prepared, basis, wide()).unwrap();
        let current_cursor = db.stream_graph_vertices_governed(&cx, &prepared, wide()).unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        assert_eq!(live_cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(), expected[1..]);
        assert_eq!(live_cursor.state(), VertexScanState::Exhausted);
        assert_eq!(pinned_cursor.collect::<Result<Vec<_>, _>>().unwrap(), expected);
        assert_eq!(historical.collect::<Result<Vec<_>, _>>().unwrap(), expected);
        assert_eq!(current_cursor.collect::<Result<Vec<_>, _>>().unwrap(), current);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(reopened.stream_graph_vertices_governed(&cx, &prepared, wide()).unwrap()
            .collect::<Result<Vec<_>, _>>().unwrap(), current);
        assert_eq!(reopened.stream_graph_vertices_governed_at(&cx, &prepared, basis, wide()).unwrap()
            .collect::<Result<Vec<_>, _>>().unwrap(), expected);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn all_cuts_match_snapshot_execution_through_updates_restatements_and_compaction() {
    let ((), report) = run_async_under_lab(0x51ca_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        for seq in 2..=5 {
            let mut changes = WriteBatch::new(R);
            changes.set_vertex_property(VId(1), KEY, Some(CanonicalScalar::Int(seq - 3)));
            changes.set_vertex_label(VId(3), LABEL, seq % 2 == 0);
            changes.create_vertex(VId(10 + seq as u128), vec![LABEL], vec![(KEY, CanonicalScalar::Int(seq))]);
            if seq == 4 { changes.delete_vertex(VId(2)); }
            db.write(&commit, changes).await.unwrap();
        }
        for compact in [false, true] {
            if compact { db.compact(&commit).await.unwrap(); }
            let view = db.read_session().unwrap();
            for sequence in 0..=5 {
                for skip in [0, 1, 3, u64::MAX] {
                    for limit in [None, Some(0), Some(1), Some(3)] {
                        for all in [false, true] {
                            let prepared = pattern(skip, limit, true);
                            let prepared = if all { prepared.with_duplicates() } else { prepared };
                            let expected = db.execute_graph_pattern_governed_at(&cx, &prepared, CommitSeq(sequence), wide()).unwrap().value;
                            let mut actual = view.stream_graph_vertices_governed_at(&cx, &prepared, CommitSeq(sequence), wide()).unwrap();
                            assert_eq!(actual.by_ref().collect::<Result<Vec<_>, _>>().unwrap(), expected);
                            assert_eq!(actual.snapshot_seq(), CommitSeq(sequence));
                            assert_eq!(actual.row_stats().result_rows, expected.len() as u64);
                            assert!(actual.next().is_none());
                        }
                    }
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn first_rows_do_not_admit_the_remaining_table_and_limits_survive_consumer_pauses() {
    let ((), report) = run_async_under_lab(0x51ca_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        for start in (0..2048).step_by(128) {
            let mut batch = WriteBatch::new(R);
            for id in start..start+128 { batch.create_vertex(VId(id), vec![], vec![]); }
            db.write(&commit, batch).await.unwrap();
        }
        let prepared = pattern(0, Some(2), false);
        let mut cursor = db.stream_graph_vertices_governed(&cx, &prepared,
            GqlQueryPolicy::new(2, 2, 512, 0)).unwrap();
        assert_eq!(cursor.row_stats().snapshot_records, 0);
        assert_eq!(cursor.evaluator_stats().work_units, 0);
        assert_eq!(cursor.next().unwrap().unwrap(), VId(0));
        assert_eq!(cursor.row_stats().snapshot_records, 1);
        assert_eq!(cursor.next().unwrap().unwrap(), VId(1));
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        assert_eq!(cursor.row_stats().snapshot_records, 2);
        assert!(cursor.evaluator_stats().work_units < 512);
        assert_eq!(cursor.evaluator_stats().scratch_entries, 0);
        assert!(cursor.next().is_none());

        let unbounded = pattern(0, None, false);
        for records in [true, false] {
            let policy = if records { GqlQueryPolicy::new(2, u64::MAX, u64::MAX, u64::MAX) }
                else { GqlQueryPolicy::new(u64::MAX, 2, u64::MAX, u64::MAX) };
            let mut cursor = db.stream_graph_vertices_governed(&cx, &unbounded, policy).unwrap();
            assert_eq!(cursor.next().unwrap().unwrap(), VId(0));
            assert_eq!(cursor.next().unwrap().unwrap(), VId(1));
            assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Rows(error)))
                if error.dimension == (if records { GqlBudgetDimension::SnapshotRecords } else { GqlBudgetDimension::ResultRows })
                    && error.limit == 2 && error.observed == 3));
            assert_eq!(cursor.row_stats().result_rows, 2);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
        }
        let mut cursor = db.stream_graph_vertices_governed(&cx, &unbounded, wide()).unwrap();
        cursor.next().unwrap().unwrap();
        let stats = (cursor.row_stats(), cursor.evaluator_stats());
        cursor.close();
        assert_eq!(cursor.state(), VertexScanState::Closed);
        assert!(cursor.next().is_none());
        assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), stats);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn owned_preparation_future_fences_unsupported_shapes_and_zero_limit_use_the_same_stream() {
    let ((), report) = run_async_under_lab(0x51ca_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let bind = RelationBind::new().with_label("Thing", LABEL).with_property("score", KEY);
        let prepared = PreparedGqlQuery::prepare(
            "MATCH (n:Thing) WHERE n.score >= 0 RETURN n SKIP 1 LIMIT 2", &bind).unwrap();
        let view = db.read_session().unwrap();
        let a = db.stream_prepared_query_governed(&cx, &prepared, wide()).unwrap();
        let b = db.stream_prepared_query_governed_at(&cx, &prepared, basis, wide()).unwrap();
        let c = view.stream_prepared_query_governed(&cx, &prepared, wide()).unwrap();
        let d = view.stream_prepared_query_governed_at(&cx, &prepared, basis, wide()).unwrap();
        drop(prepared); drop(bind); drop(view);
        for result in [a.collect::<Result<Vec<_>, _>>().unwrap(), b.collect::<Result<Vec<_>, _>>().unwrap(),
            c.collect::<Result<Vec<_>, _>>().unwrap(), d.collect::<Result<Vec<_>, _>>().unwrap()] {
            assert_eq!(result, vec![VId(2), VId(4)]);
        }
        let zero = pattern(u64::MAX, Some(0), false);
        let mut cursor = db.stream_graph_vertices_governed(&cx, &zero, GqlQueryPolicy::new(0, 0, 1, 0)).unwrap();
        assert!(cursor.next().is_none());
        assert_eq!(cursor.row_stats(), GqlExecutionStats { snapshot_records: 0, result_rows: 0 });
        let future = CommitSeq(basis.0 + 1);
        assert!(matches!(db.stream_graph_vertices_governed_at(&cx, &zero, future, GqlQueryPolicy::new(0, 0, 0, 0)),
            Err(GqlQueryError::Source(VertexScanError::Source(ReadError::BeyondFrontier { .. })))));
        let mut edge = GraphPatternBuilder::new(); edge.vertex("a").unwrap(); edge.vertex("b").unwrap();
        edge.edge("a", R, GlaDirection::Forward, "b").unwrap();
        let edge = edge.prepare("a", 0, None).unwrap();
        assert!(matches!(db.stream_graph_vertices_governed(&cx, &edge, wide()),
            Err(GqlQueryError::Source(VertexScanError::Plan(_)))));
        assert!(matches!(db.stream_graph_vertices_governed_at(&cx, &edge, future, wide()),
            Err(GqlQueryError::Source(VertexScanError::Source(ReadError::BeyondFrontier { .. })))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_work_limits_and_consumer_chunk_sizes_preserve_one_execution_budget() {
    let ((), report) = run_async_under_lab(0x51ca_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap(); seed(&mut db, &commit).await;
        let prepared = pattern(1, None, true);
        let mut baseline = db.stream_graph_vertices_governed(&cx, &prepared, wide()).unwrap();
        let expected = baseline.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        let rows = baseline.row_stats(); let evaluator = baseline.evaluator_stats();
        let exact = GqlQueryPolicy::new(rows.snapshot_records, rows.result_rows, evaluator.work_units, evaluator.scratch_entries);
        for width in 1..=5 {
            let mut cursor = db.stream_graph_vertices_governed(&cx, &prepared, exact).unwrap();
            let mut actual = Vec::new();
            while cursor.state() == VertexScanState::Open {
                actual.extend(cursor.by_ref().take(width).collect::<Result<Vec<_>, _>>().unwrap());
            }
            assert_eq!(actual, expected);
            assert_eq!(cursor.row_stats(), rows); assert_eq!(cursor.evaluator_stats(), evaluator);
        }
        let too_small = GqlQueryPolicy::new(rows.snapshot_records, rows.result_rows, evaluator.work_units - 1, u64::MAX);
        let mut refused = db.stream_graph_vertices_governed(&cx, &prepared, too_small).unwrap();
        assert!(matches!(refused.by_ref().collect::<Result<Vec<_>, _>>(), Err(GqlQueryError::Evaluator(_))));
        assert_eq!(refused.state(), VertexScanState::Failed);
        assert!(refused.next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn value_pattern(offset: u64, count: Option<u64>) -> PreparedGraphPattern<fgdb_gql::algebra::GraphValueRow> {
    use fgdb_gql::algebra::GraphColumn;
    let mut builder = GraphPatternBuilder::new(); builder.vertex("n").unwrap();
    builder.prepare_values(&[
        GraphColumn::vertex("id", "n"),
        GraphColumn::property("score", "n", KEY),
        GraphColumn::property("name", "n", PropertyKeyId(8)),
        GraphColumn::vertex("again", "n"),
    ], offset, count).unwrap()
}

#[test]
fn correlated_property_rows_stream_from_pinned_live_and_historical_generations() {
    let ((), report) = run_async_under_lab(0x51ca_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut names = WriteBatch::new(R);
        names.set_vertex_property(VId(0), PropertyKeyId(8), Some(CanonicalScalar::ucs_basic_text(&"long name".repeat(40)).unwrap()));
        names.set_vertex_property(VId(1), KEY, None);
        names.set_vertex_property(VId(2), KEY, Some(CanonicalScalar::Null));
        let basis = db.write(&commit, names).await.unwrap();
        let pattern = value_pattern(0, None);
        let expected = db.execute_graph_pattern_governed(&cx, &pattern, wide()).unwrap().value;
        let mut stream = db.stream_graph_values_governed(&cx, &pattern, wide()).unwrap();
        let view = db.read_session().unwrap();
        let pinned = view.stream_graph_values_governed(&cx, &pattern, wide()).unwrap();
        let pinned_at = view.stream_graph_values_governed_at(&cx, &pattern, basis, wide()).unwrap();
        drop(view); drop(pattern);
        let first = stream.next().unwrap().unwrap();
        assert_eq!(first, expected[0]);
        assert_eq!(first.get(0), first.get(3));
        let mut edit = WriteBatch::new(R);
        edit.set_vertex_property(VId(0), PropertyKeyId(8), None);
        edit.set_vertex_property(VId(1), KEY, Some(CanonicalScalar::Int(999)));
        edit.delete_vertex(VId(2));
        edit.create_vertex(VId(5), vec![], vec![(KEY, CanonicalScalar::Int(-8))]);
        db.write(&commit, edit).await.unwrap();
        let pattern = value_pattern(0, None).with_duplicates();
        let current = db.execute_graph_pattern_governed(&cx, &pattern, wide()).unwrap().value;
        let historical = db.stream_graph_values_governed_at(&cx, &pattern, basis, wide()).unwrap();
        let live = db.stream_graph_values_governed(&cx, &pattern, wide()).unwrap();
        db.compact(&commit).await.unwrap(); drop(db);
        assert_eq!(stream.collect::<Result<Vec<_>, _>>().unwrap(), expected[1..]);
        assert_eq!(pinned.collect::<Result<Vec<_>, _>>().unwrap(), expected);
        assert_eq!(pinned_at.collect::<Result<Vec<_>, _>>().unwrap(), expected);
        assert_eq!(historical.collect::<Result<Vec<_>, _>>().unwrap(), expected);
        assert_eq!(live.collect::<Result<Vec<_>, _>>().unwrap(), current);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(reopened.stream_graph_values_governed_at(&cx, &pattern, basis, wide()).unwrap()
            .collect::<Result<Vec<_>, _>>().unwrap(), expected);
        assert_eq!(reopened.stream_graph_values_governed(&cx, &pattern, wide()).unwrap()
            .collect::<Result<Vec<_>, _>>().unwrap(), current);
        assert_eq!(first, expected[0], "delivered values own their scalar payload after source release");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn value_streams_keep_exact_shared_budgets_and_refuse_unstreamable_order_before_scanning() {
    let ((), report) = run_async_under_lab(0x51ca_0007, |root| async move {
        use fgdb_gql::algebra::GraphColumn;
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap(); seed(&mut db, &commit).await;
        let pattern = value_pattern(1, Some(3));
        let mut baseline = db.stream_graph_values_governed(&cx, &pattern, wide()).unwrap();
        let expected = baseline.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        let rows = baseline.row_stats(); let stats = baseline.evaluator_stats();
        assert_eq!(rows, GqlExecutionStats { snapshot_records: 4, result_rows: 3 });
        let exact = GqlQueryPolicy::new(4, 3, stats.work_units, stats.scratch_entries);
        let mut cursor = db.stream_graph_values_governed(&cx, &pattern, exact).unwrap();
        let mut actual = Vec::new();
        actual.extend(cursor.by_ref().take(1).collect::<Result<Vec<_>, _>>().unwrap());
        actual.extend(cursor.by_ref().take(1).collect::<Result<Vec<_>, _>>().unwrap());
        actual.extend(cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap());
        assert_eq!(actual, expected); assert_eq!(cursor.evaluator_stats(), stats);
        for policy in [
            GqlQueryPolicy::new(3, 3, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(4, 2, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(4, 3, stats.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(4, 3, u64::MAX, stats.scratch_entries - 1),
        ] {
            let mut failed = db.stream_graph_values_governed(&cx, &pattern, policy).unwrap();
            assert!(failed.by_ref().collect::<Result<Vec<_>, _>>().is_err());
            assert_eq!(failed.state(), VertexScanState::Failed); assert!(failed.next().is_none());
        }
        let mut builder = GraphPatternBuilder::new(); builder.vertex("n").unwrap();
        let property_only = builder.prepare_values(&[GraphColumn::property("score", "n", KEY)], 0, None).unwrap();
        assert!(matches!(db.stream_graph_values_governed(&cx, &property_only, GqlQueryPolicy::new(0, 0, 0, 0)),
            Err(GqlQueryError::Source(VertexScanError::Plan(_)))));
        let empty = value_pattern(u64::MAX, Some(0));
        let mut empty = db.stream_graph_values_governed(&cx, &empty, GqlQueryPolicy::new(0, 0, 1, 0)).unwrap();
        assert!(empty.next().is_none()); assert_eq!(empty.evaluator_stats().scratch_entries, 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
