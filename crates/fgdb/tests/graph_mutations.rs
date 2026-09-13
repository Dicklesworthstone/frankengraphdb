//! Query-selected writes use the real Chronicle/Strata transaction path.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, VertexRow, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphMutationError,
    GraphMutationPolicy, PreparedGraphMutation, PreparedGraphMutationText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId,
    EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const OLD: PropertyKeyId = PropertyKeyId(3);
const UPDATED: LabelId = LabelId(1);
type LogicalRow = (VId, Vec<LabelId>, Vec<(PropertyKeyId, CanonicalScalar)>);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x91; 32], DatabaseSecurityNamespaceId([0x92; 32]), [0x93; 32])
}
fn policy() -> GraphMutationPolicy {
    GraphMutationPolicy::new(GqlQueryPolicy::new(10_000, 10_000, 5_000_000, 2_000_000), 1_000)
}
fn mutation(text: &str) -> PreparedGraphMutation { mutation_at(text, R) }
fn mutation_at(text: &str, relation: RelationId) -> PreparedGraphMutation {
    use fgdb_gql::{GraphSymbol, GraphSymbolKind};
    PreparedGraphMutationText::prepare(text, relation, |kind, name| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Property, "old") => Some(GraphSymbol::Property(OLD)),
        (GraphSymbolKind::Label, "Updated") => Some(GraphSymbol::Label(UPDATED)),
        _ => None,
    }).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut data = WriteBatch::new(R);
    for id in 1..=3_u128 {
        data.create_vertex(VId(id), vec![], vec![
            (P, CanonicalScalar::Int(id as i64 * 10)), (OLD, CanonicalScalar::Bool(true)),
        ]);
    }
    for (id, source, target) in [(101, 1, 2), (102, 1, 2), (103, 2, 1)] {
        data.add_edge(EId(id), VId(source), VId(target), vec![]);
    }
    db.write(cx, data).await.unwrap()
}
fn logical(rows: &[VertexRow]) -> Vec<LogicalRow> {
    rows.iter().map(|row| (row.vid, row.labels.clone(), row.props.clone())).collect()
}
fn value(row: &VertexRow, key: PropertyKeyId) -> Option<&CanonicalScalar> {
    row.props.iter().find(|(property, _)| *property == key).map(|(_, value)| value)
}
fn staged_prefix() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(777), vec![], vec![(P, CanonicalScalar::Int(99))]);
    batch
}

#[test]
fn simultaneous_updates_commit_as_one_batch_and_pinned_history_survives_reopen() {
    let ((), report) = run_async_under_lab(0x6a7e_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let query = contexts.query(); let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let before = logical(&db.vertices().unwrap());
        let pinned = db.read_session().unwrap();
        let plan = mutation("MATCH (a)-[:R]->(b) SET a.p=b.p,a:Updated REMOVE a.old");
        let mut txn = db.begin(&txcx).unwrap();
        let stats = txn.execute_graph_mutation_governed(&mut db, &query, &plan, policy()).unwrap();
        assert_eq!((stats.selection.result_rows, stats.target_vertices, stats.effects), (3, 2, 6));
        assert_eq!(db.frontier().unwrap(), basis, "staging must not publish a marker");
        assert_eq!(logical(&db.vertices().unwrap()), before);
        let staged = txn.vertices(&db).unwrap();
        for (vid, expected) in [(1, 20), (2, 10), (3, 30)] {
            let row = staged.iter().find(|row| row.vid == VId(vid)).unwrap();
            assert_eq!(value(row, P), Some(&CanonicalScalar::Int(expected)));
            if vid != 3 {
                assert!(row.labels.contains(&UPDATED));
                assert!(value(row, OLD).is_none());
            }
        }
        let expected = logical(&staged);
        let committed = txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(committed.0, basis.0 + 1);
        assert_eq!(logical(&db.vertices().unwrap()), expected);
        db.compact(&commit).await.unwrap(); drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(logical(&reopened.vertices().unwrap()), expected);
        assert_eq!(logical(&reopened.vertices_at(basis).unwrap()), before);
        assert_eq!(logical(&pinned.vertices().unwrap()), before);
        assert_eq!(reopened.edges().unwrap().len(), 3);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_assignments_and_quotas_preserve_prior_staged_work_and_abort_discards_success() {
    let ((), report) = run_async_under_lab(0x6a7e_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let query = contexts.query(); let txcx = contexts.txn();
        for mode in 0..3 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit).await;
            let mut extra = WriteBatch::new(R);
            extra.add_edge(EId(104), VId(1), VId(3), vec![]);
            db.write(&commit, extra).await.unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            txn.write(&mut db, staged_prefix()).unwrap();
            let before = logical(&txn.vertices(&db).unwrap());
            let plan = mutation(if mode == 0 {
                "MATCH (a)-[:R]->(b) SET a.p=b.p"
            } else { "MATCH (a) SET a.q=7" });
            let mut budget = policy();
            if mode == 1 { budget.max_effects = 0; }
            if mode == 2 { budget.query = GqlQueryPolicy::new(10_000, 0, 5_000_000, 2_000_000); }
            let result = txn.execute_graph_mutation_governed(&mut db, &query, &plan, budget);
            match mode {
                0 => assert!(matches!(result, Err(GqlQueryError::Source(GraphMutationError::ConflictingAssignment { .. })))),
                1 => assert!(matches!(result, Err(GqlQueryError::Source(GraphMutationError::EffectLimit { .. })))),
                _ => assert!(matches!(result, Err(GqlQueryError::Rows(_)))),
            }
            assert_eq!(logical(&txn.vertices(&db).unwrap()), before);
            txn.commit(&mut db, &commit).await.unwrap();
            assert!(db.vertex(VId(777)).unwrap().is_some());
            assert_eq!(value(&db.vertex(VId(1)).unwrap().unwrap(), P), Some(&CanonicalScalar::Int(10)));
            assert!(value(&db.vertex(VId(1)).unwrap().unwrap(), Q).is_none());
            let frozen = logical(&db.vertices().unwrap());
            let mut txn = db.begin(&txcx).unwrap();
            txn.execute_graph_mutation_governed(&mut db, &query,
                &mutation("MATCH (a) SET a.q=5"), policy()).unwrap();
            txn.abort();
            assert_eq!(logical(&db.vertices().unwrap()), frozen);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn explicit_detach_delete_deduplicates_targets_and_retires_incident_edges_durably() {
    let ((), report) = run_async_under_lab(0x6a7e_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let query = contexts.query(); let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let plan = mutation("MATCH (a)-[:R]->(b) DETACH DELETE b");
        let mut txn = db.begin(&txcx).unwrap();
        let limited = GraphMutationPolicy::new(policy().query, 1);
        assert!(matches!(txn.execute_graph_mutation_governed(&mut db, &query, &plan, limited),
            Err(GqlQueryError::Source(GraphMutationError::EffectLimit { .. }))));
        assert_eq!(txn.vertices(&db).unwrap().len(), 3);
        assert_eq!(txn.edges(&db).unwrap().len(), 3);
        let stats = txn.execute_graph_mutation_governed(&mut db, &query, &plan, policy()).unwrap();
        assert_eq!((stats.selection.result_rows, stats.target_vertices, stats.effects), (3, 2, 2));
        assert!(txn.edges(&db).unwrap().is_empty());
        assert_eq!(txn.vertices(&db).unwrap()[0].vid, VId(3));
        assert_eq!(db.vertices().unwrap().len(), 3);
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap(); drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(reopened.vertices().unwrap().len(), 1);
        assert_eq!(reopened.vertices().unwrap()[0].vid, VId(3));
        assert!(reopened.edges().unwrap().is_empty());
        assert_eq!(reopened.vertices_at(basis).unwrap().len(), 3);
        assert_eq!(reopened.edges_at(basis).unwrap().len(), 3);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn mutation_reads_keep_rejected_candidates_and_phantoms_after_success_refusal_or_no_matches() {
    let ((), report) = run_async_under_lab(0x6a7e_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let query = contexts.query(); let txcx = contexts.txn();
        for mode in 0..4 {
            for changed_domain in 0..2 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let mut extra = WriteBatch::new(R);
                extra.add_edge(EId(104), VId(1), VId(3), vec![]);
                db.write(&commit, extra).await.unwrap();
                let mut txn = db.begin(&txcx).unwrap();
                txn.write(&mut db, staged_prefix()).unwrap();
                let plan = mutation(if mode == 2 {
                    "MATCH (a)-[:R]->(b) WHERE b.p<0 SET a.q=7"
                } else { "MATCH (a)-[:R]->(b) WHERE b.p<25 SET a.q=7" });
                let mut budget = policy();
                if mode == 1 { budget.query = GqlQueryPolicy::new(10_000, 0, 5_000_000, 2_000_000); }
                if mode == 3 { budget.max_effects = 0; }
                let result = txn.execute_graph_mutation_governed(&mut db, &query, &plan, budget);
                match mode {
                    0 => assert_eq!(result.unwrap().effects, 2),
                    2 => assert_eq!(result.unwrap().effects, 0),
                    _ => assert!(result.is_err()),
                }
                let mut winner = WriteBatch::new(R);
                if changed_domain == 0 {
                    // A previously rejected endpoint now changes the target set.
                    winner.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(-1)));
                } else {
                    winner.create_vertex(VId(4), vec![], vec![(P, CanonicalScalar::Int(-1))]);
                    winner.add_edge(EId(105), VId(3), VId(4), vec![]);
                }
                db.write(&commit, winner).await.unwrap();
                let frontier = db.frontier().unwrap();
                // Commit immediately: no later read can repair a missed witness.
                assert!(matches!(txn.commit(&mut db, &commit).await,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law: "FG-LAW-FCW-READ-01", .. }))));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertex(VId(777)).unwrap().is_none());
                assert!(value(&db.vertex(VId(1)).unwrap().unwrap(), Q).is_none());
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn all_exact_limits_and_owner_basis_relation_precedence_apply_before_workspace_change() {
    let ((), report) = run_async_under_lab(0x6a7e_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let query = contexts.query(); let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let plan = mutation("MATCH (a) SET a.q=9");
        let mut measured_txn = db.begin(&txcx).unwrap();
        let measured = measured_txn.execute_graph_mutation_governed(&mut db, &query, &plan, policy()).unwrap();
        measured_txn.abort();
        let exact = GraphMutationPolicy::new(GqlQueryPolicy::new(
            measured.selection.snapshot_records, measured.selection.result_rows,
            measured.evaluator.work_units, measured.evaluator.scratch_entries), measured.effects);
        let mut txn = db.begin(&txcx).unwrap();
        assert_eq!(txn.execute_graph_mutation_governed(&mut db, &query, &plan, exact).unwrap(), measured);
        txn.abort();
        let before = logical(&db.vertices().unwrap());
        for budget in [
            GraphMutationPolicy::new(GqlQueryPolicy::new(measured.selection.snapshot_records - 1, 100, u64::MAX, u64::MAX), 100),
            GraphMutationPolicy::new(GqlQueryPolicy::new(100, measured.selection.result_rows - 1, u64::MAX, u64::MAX), 100),
            GraphMutationPolicy::new(GqlQueryPolicy::new(100, 100, measured.evaluator.work_units - 1, u64::MAX), 100),
            GraphMutationPolicy::new(GqlQueryPolicy::new(100, 100, u64::MAX, measured.evaluator.scratch_entries - 1), 100),
            GraphMutationPolicy::new(GqlQueryPolicy::new(100, 100, u64::MAX, u64::MAX), measured.effects - 1),
        ] {
            let mut txn = db.begin(&txcx).unwrap();
            assert!(txn.execute_graph_mutation_governed(&mut db, &query, &plan, budget).is_err());
            assert_eq!(logical(&txn.vertices(&db).unwrap()), before);
            txn.abort();
        }
        let zero = GraphMutationPolicy::new(GqlQueryPolicy::new(0, 0, 0, 0), 0);
        let mut foreign = Database::open_memory(&commit, keys()).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        assert!(matches!(txn.execute_graph_mutation_governed(&mut foreign, &query, &plan, zero),
            Err(GqlQueryError::Source(GraphMutationError::Source(WriteTxnError::WrongDatabase)))));
        txn.write(&mut db, staged_prefix()).unwrap();
        let other_relation = mutation_at("MATCH (a) SET a.q=1", RelationId(2));
        assert!(matches!(txn.execute_graph_mutation_governed(&mut db, &query, &other_relation, zero),
            Err(GqlQueryError::Source(GraphMutationError::Source(WriteTxnError::RelationMismatch { .. })))));
        txn.abort();
        let mut txn = db.begin(&txcx).unwrap();
        db.write(&commit, staged_prefix()).await.unwrap();
        assert!(matches!(txn.execute_graph_mutation_governed(&mut db, &query,
            &mutation("MATCH (a) WHERE a.p<0 SET a.q=1"), zero),
            Err(GqlQueryError::Source(GraphMutationError::Source(WriteTxnError::SnapshotAdvanced { .. })))));
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
