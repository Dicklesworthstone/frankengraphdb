//! ALL output uses the existing governed sources, overlays and conflict guards.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, EdgeRecord, MemVfs, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GraphBindingRow, GraphColumn, GraphPatternBuilder, GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{GlaLimitDimension, GqlBudgetDimension, GqlQueryError, GqlQueryPolicy};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x31; 32], DatabaseSecurityNamespaceId([0x32; 32]), [0x33; 32])
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100, 100, 1_000_000, 1_000_000)
}

async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut first = WriteBatch::new(R);
    for id in 1..=4 {
        first.create_vertex(VId(id), vec![], vec![]);
    }
    first.add_edge(EId(10), VId(1), VId(2), vec![]);
    first.add_edge(EId(11), VId(1), VId(2), vec![]);
    first.add_edge(EId(12), VId(4), VId(2), vec![]);
    db.write(cx, first).await.unwrap();
    let mut second = WriteBatch::new(S);
    second.add_edge(EId(20), VId(2), VId(3), vec![]);
    second.add_edge(EId(21), VId(2), VId(3), vec![]);
    db.write(cx, second).await.unwrap()
}

async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    seed(&mut db, cx).await;
    db
}

fn builder() -> GraphPatternBuilder {
    let mut b = GraphPatternBuilder::new();
    for name in ["source", "middle", "destination"] {
        b.vertex(name).unwrap();
    }
    b.edge("source", R, GlaDirection::Forward, "middle").unwrap();
    b.edge("middle", S, GlaDirection::Forward, "destination").unwrap();
    b
}

fn pattern(offset: u64, count: Option<u64>) -> PreparedGraphPattern<GraphBindingRow> {
    builder().prepare_bindings(&["source", "destination"], offset, count).unwrap().with_duplicates()
}

fn plain(rows: Vec<GraphBindingRow>) -> Vec<Vec<VId>> {
    rows.into_iter().map(|row| row.values().to_vec()).collect()
}

// One occurrence for each qualifying pair of concrete edge records. This
// deliberately does not deduplicate, use GLA, or multiply scalar projections.
fn oracle(edges: &[EdgeRecord]) -> Vec<Vec<VId>> {
    let mut result = Vec::new();
    for first in edges.iter().filter(|edge| edge.entry.relation == R) {
        for second in edges.iter().filter(|edge| edge.entry.relation == S) {
            if first.entry.dst == second.entry.src {
                result.push(vec![first.entry.src, second.entry.dst]);
            }
        }
    }
    result.sort();
    result
}

#[test]
fn all_five_read_surfaces_keep_correlated_occurrences_and_ordinary_distinct_is_unchanged() {
    let ((), report) = run_async_under_lab(0xbab1_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = seeded(&commit).await;
        let at = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let all = pattern(0, None);
        let expected = oracle(&db.edges().unwrap());
        assert_eq!(expected.len(), 6);
        let txn = db.begin(&txn_cx).unwrap();
        for value in [
            db.execute_graph_pattern_governed(&query_cx, &all, policy()).unwrap().value,
            db.execute_graph_pattern_governed_at(&query_cx, &all, at, policy()).unwrap().value,
            pinned.execute_graph_pattern_governed(&query_cx, &all, policy()).unwrap().value,
            pinned.execute_graph_pattern_governed_at(&query_cx, &all, at, policy()).unwrap().value,
            txn.execute_graph_pattern_governed(&db, &query_cx, &all, policy()).unwrap().value,
        ] {
            assert_eq!(plain(value), expected);
        }
        txn.abort();
        let distinct = builder().prepare_bindings(&["source", "destination"], 0, None).unwrap();
        assert!(!distinct.preserves_duplicates());
        let mut unique = expected;
        unique.dedup();
        assert_eq!(plain(db.execute_graph_pattern_governed(&query_cx, &distinct, policy()).unwrap().value), unique);
        let paged = pattern(3, Some(2));
        assert_eq!(plain(db.execute_graph_pattern_governed(&query_cx, &paged, policy()).unwrap().value),
            vec![vec![VId(1), VId(3)], vec![VId(4), VId(3)]]);
        assert!(matches!(db.execute_graph_pattern_governed(&query_cx, &all,
            GqlQueryPolicy::new(5, 2, 1_000_000, 1_000_000)),
            Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::ResultRows
                && error.limit == 2 && error.observed == 3));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn canonical_staging_changes_multiplicity_without_inventing_ensure_aliases_or_rebasing_history() {
    let ((), report) = run_async_under_lab(0xbab1_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = seeded(&commit).await;
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let all = pattern(0, None);
        let before = oracle(&db.edges().unwrap());
        let mut txn = db.begin(&txn_cx).unwrap();
        let mut stage = WriteBatch::new(R);
        stage.delete_edge(EId(10));
        stage.ensure_edge_by_triple(EId(999), VId(1), VId(2), vec![]);
        stage.add_edge(EId(13), VId(4), VId(2), vec![]);
        txn.write(&mut db, stage).unwrap();
        assert!(txn.edge(&db, EId(999)).unwrap().is_none());
        let expected = oracle(&txn.edges(&db).unwrap());
        assert_eq!(expected.len(), 6);
        assert_ne!(expected, before);
        let run = txn.execute_graph_pattern_governed(&db, &query_cx, &all, policy()).unwrap();
        assert_eq!(plain(run.value.clone()), expected);
        assert_eq!(run.rows.snapshot_records, 5);
        let exact = GqlQueryPolicy::new(run.rows.snapshot_records, run.rows.result_rows,
            run.evaluator.work_units, run.evaluator.scratch_entries);
        assert_eq!(txn.execute_graph_pattern_governed(&db, &query_cx, &all, exact).unwrap(), run);
        assert_eq!(plain(db.execute_graph_pattern_governed(&query_cx, &all, policy()).unwrap().value), before);
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(plain(db.execute_graph_pattern_governed(&query_cx, &all, policy()).unwrap().value), expected);
        assert_eq!(plain(db.execute_graph_pattern_governed_at(&query_cx, &all, basis, policy()).unwrap().value), before);
        assert_eq!(plain(pinned.execute_graph_pattern_governed(&query_cx, &all, policy()).unwrap().value), before);
        assert!(db.edge(EId(999)).unwrap().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn refused_bag_output_keeps_unprojected_dependencies_without_globally_fencing_writes() {
    let ((), report) = run_async_under_lab(0xbab1_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn_cx = contexts.txn();
        for conflict in [false, true] {
            let mut db = seeded(&commit).await;
            let mut txn = db.begin(&txn_cx).unwrap();
            let mut stage = WriteBatch::new(R);
            stage.create_vertex(VId(99), vec![], vec![]);
            txn.write(&mut db, stage).unwrap();
            assert!(matches!(txn.execute_graph_pattern_governed(&db, &query_cx, &pattern(0, None),
                GqlQueryPolicy::new(100, 0, 1_000_000, 1_000_000)), Err(GqlQueryError::Rows(_))));
            let mut winner = WriteBatch::new(R);
            if conflict {
                winner.set_vertex_property(VId(2), PropertyKeyId(1), Some(CanonicalScalar::Int(42)));
            } else {
                winner.create_vertex(VId(77), vec![], vec![]);
            }
            db.write(&commit, winner).await.unwrap();
            let frontier = db.frontier().unwrap();
            let result = txn.commit(&mut db, &commit).await;
            if conflict {
                assert!(matches!(result, Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01", ..
                }))));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertex(VId(99)).unwrap().is_none());
            } else {
                result.expect("unrelated vertex insertion does not conflict with an edge-pattern read");
                assert!(db.vertex(VId(99)).unwrap().is_some());
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn historical_bags_survive_compaction_and_reopening() {
    let ((), report) = run_async_under_lab(0xbab1_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let all = pattern(0, None);
        let old = oracle(&db.edges().unwrap());
        let mut changed = WriteBatch::new(R);
        changed.delete_edge(EId(10));
        db.write(&commit, changed).await.unwrap();
        let current = oracle(&db.edges().unwrap());
        assert_eq!(old.len(), 6);
        assert_eq!(current.len(), 4);
        db.compact(&commit).await.unwrap();
        assert_eq!(plain(db.execute_graph_pattern_governed(&query_cx, &all, policy()).unwrap().value), current);
        assert_eq!(plain(db.execute_graph_pattern_governed_at(&query_cx, &all, basis, policy()).unwrap().value), old);
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(plain(reopened.execute_graph_pattern_governed(&query_cx, &all, policy()).unwrap().value), current);
        assert_eq!(plain(reopened.execute_graph_pattern_governed_at(&query_cx, &all, basis, policy()).unwrap().value), old);
        assert_eq!(plain(pinned.execute_graph_pattern_governed(&query_cx, &all, policy()).unwrap().value), old);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn property_pattern() -> PreparedGraphPattern<GraphValueRow> {
    builder().prepare_values(&[
        GraphColumn::vertex("owner", "source"),
        GraphColumn::property("name", "source", PropertyKeyId(1)),
        GraphColumn::property("payload", "destination", PropertyKeyId(2)),
    ], 0, None).unwrap().with_duplicates()
}

fn property_oracle(vertices: &[fgdb::VertexRow], edges: &[EdgeRecord]) -> Vec<Vec<GraphValue>> {
    let value = |vid, key| {
        let row = vertices.iter().find(|row| row.vid == vid).expect("live endpoint");
        GraphValue::Scalar(row.props.iter().find(|(found, _)| *found == key)
            .map(|(_, scalar)| scalar.clone()).unwrap_or(CanonicalScalar::Null))
    };
    let mut rows: Vec<_> = oracle(edges).into_iter().map(|pair| vec![
        GraphValue::Vertex(pair[0]), value(pair[0], PropertyKeyId(1)), value(pair[1], PropertyKeyId(2)),
    ]).collect();
    rows.sort();
    rows
}

fn value_rows(rows: &[GraphValueRow]) -> Vec<Vec<GraphValue>> {
    rows.iter().map(|row| row.values().to_vec()).collect()
}

#[test]
fn property_bags_keep_correlation_nulls_and_owned_payloads_across_staging_and_history() {
    let ((), report) = run_async_under_lab(0xbab1_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = seeded(&commit).await;
        let mut values = WriteBatch::new(R);
        values.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::ucs_basic_text("secret alpha").unwrap()));
        values.set_vertex_property(VId(3), PropertyKeyId(2), Some(CanonicalScalar::bytes(vec![5; 129]).unwrap()));
        let basis = db.write(&commit, values).await.unwrap();
        let pinned = db.read_session().unwrap();
        let pattern = property_pattern();
        let before = property_oracle(&db.vertices().unwrap(), &db.edges().unwrap());
        assert_eq!(before.len(), 6);
        let mut txn = db.begin(&txn_cx).unwrap();
        for run in [
            db.execute_graph_pattern_governed(&query_cx, &pattern, policy()).unwrap(),
            db.execute_graph_pattern_governed_at(&query_cx, &pattern, basis, policy()).unwrap(),
            pinned.execute_graph_pattern_governed(&query_cx, &pattern, policy()).unwrap(),
            pinned.execute_graph_pattern_governed_at(&query_cx, &pattern, basis, policy()).unwrap(),
            txn.execute_graph_pattern_governed(&db, &query_cx, &pattern, policy()).unwrap(),
        ] {
            assert_eq!(value_rows(&run.value), before);
            assert_eq!(run.rows.result_rows, 6);
            assert!(!format!("{run:?}").contains("secret alpha"));
        }
        let mut stage = WriteBatch::new(R);
        stage.delete_edge(EId(10));
        stage.set_vertex_property(VId(3), PropertyKeyId(2), None);
        stage.set_vertex_property(VId(4), PropertyKeyId(1), Some(CanonicalScalar::ucs_basic_text("beta").unwrap()));
        txn.write(&mut db, stage).unwrap();
        let expected = property_oracle(&txn.vertices(&db).unwrap(), &txn.edges(&db).unwrap());
        assert_eq!(expected.len(), 4);
        assert_ne!(expected, before);
        let staged = txn.execute_graph_pattern_governed(&db, &query_cx, &pattern, policy()).unwrap();
        assert_eq!(value_rows(&staged.value), expected);
        assert!(staged.value.iter().all(|row| row.get(2).unwrap().is_null()));
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(value_rows(&db.execute_graph_pattern_governed(&query_cx, &pattern, policy()).unwrap().value), expected);
        assert_eq!(value_rows(&db.execute_graph_pattern_governed_at(&query_cx, &pattern, basis, policy()).unwrap().value), before);
        assert_eq!(value_rows(&pinned.execute_graph_pattern_governed(&query_cx, &pattern, policy()).unwrap().value), before);
        // Rows own their payloads; no source lifetime escapes the call.
        drop(pinned);
        drop(db);
        assert_eq!(value_rows(&staged.value), expected);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn property_bag_limits_and_cancellation_preserve_the_original_error_classes() {
    let ((), report) = run_async_under_lab(0xbab1_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = seeded(&commit).await;
        let foreign = seeded(&commit).await;
        let mut values = WriteBatch::new(R);
        values.set_vertex_property(VId(3), PropertyKeyId(2), Some(CanonicalScalar::bytes(vec![8; 129]).unwrap()));
        db.write(&commit, values).await.unwrap();
        let pattern = property_pattern();
        let full = db.execute_graph_pattern_governed(&query_cx, &pattern, policy()).unwrap();
        let exact = GqlQueryPolicy::new(full.rows.snapshot_records, full.rows.result_rows,
            full.evaluator.work_units, full.evaluator.scratch_entries);
        assert_eq!(db.execute_graph_pattern_governed(&query_cx, &pattern, exact).unwrap(), full);
        for (work, scratch, dimension) in [
            (full.evaluator.work_units - 1, full.evaluator.scratch_entries, GlaLimitDimension::WorkUnits),
            (full.evaluator.work_units, full.evaluator.scratch_entries - 1, GlaLimitDimension::ScratchEntries),
        ] {
            assert!(matches!(db.execute_graph_pattern_governed(&query_cx, &pattern,
                GqlQueryPolicy::new(5, 6, work, scratch)), Err(GqlQueryError::Evaluator(error))
                if error.dimension == dimension && error.observed == u128::from(error.limit) + 1));
        }
        assert!(matches!(db.execute_graph_pattern_governed(&query_cx, &pattern,
            GqlQueryPolicy::new(5, 2, u64::MAX, u64::MAX)), Err(GqlQueryError::Rows(error))
            if error.dimension == GqlBudgetDimension::ResultRows && error.observed == 3));
        let txn = db.begin(&txn_cx).unwrap();
        root.cancel_with(asupersync::CancelKind::User, Some("bag cancellation"));
        assert!(matches!(db.execute_graph_pattern_governed(&query_cx, &pattern, policy()),
            Err(GqlQueryError::Interrupted(_))));
        assert!(matches!(txn.execute_graph_pattern_governed(&db, &query_cx, &pattern, policy()),
            Err(GqlQueryError::Interrupted(_))));
        assert!(matches!(txn.execute_graph_pattern_governed(&foreign, &query_cx, &pattern, policy()),
            Err(GqlQueryError::Source(WriteTxnError::WrongDatabase))));
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
