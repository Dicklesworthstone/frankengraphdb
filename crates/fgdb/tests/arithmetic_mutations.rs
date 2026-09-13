//! Arithmetic mutations over canonical staged effects and durable snapshots.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, VertexRow, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphIntegerErrorKind,
    GraphMutationError, GraphMutationPolicy, GraphSymbol, GraphSymbolKind,
    PreparedGraphMutation, PreparedGraphMutationText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId,
    EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
type Logical = Vec<(VId, Vec<(PropertyKeyId, CanonicalScalar)>)>;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x71; 32], DatabaseSecurityNamespaceId([0x72; 32]), [0x73; 32])
}
fn policy() -> GraphMutationPolicy {
    GraphMutationPolicy::new(GqlQueryPolicy::new(10_000, 10_000, 5_000_000, 2_000_000), 1_000)
}
fn mutation(text: &str) -> PreparedGraphMutation {
    PreparedGraphMutationText::prepare(text, R, |kind, name| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(10))]);
    batch.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(20))]);
    batch.create_vertex(VId(3), vec![], vec![(P, CanonicalScalar::Null)]);
    batch.create_vertex(VId(4), vec![], vec![]);
    for (id, source, target) in [(101, 1, 2), (102, 1, 2), (103, 2, 1)] {
        batch.add_edge(EId(id), VId(source), VId(target), vec![]);
    }
    db.write(cx, batch).await.unwrap()
}
fn logical(rows: &[VertexRow]) -> Logical {
    rows.iter().map(|row| (row.vid, row.props.clone())).collect()
}
fn value(row: &VertexRow, key: PropertyKeyId) -> Option<&CanonicalScalar> {
    row.props.iter().find(|(property, _)| *property == key).map(|(_, value)| value)
}
fn prefix() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(777), vec![], vec![(P, CanonicalScalar::Int(99))]);
    batch
}

#[test]
fn frozen_arithmetic_and_walk_counters_publish_once_and_preserve_pinned_history() {
    let ((), report) = run_async_under_lab(0xa817_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let query = contexts.query(); let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let before = logical(&db.vertices().unwrap());
        let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut staged = WriteBatch::new(R);
        staged.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(12)));
        txn.write(&mut db, staged).unwrap();
        let reciprocal = mutation("MATCH (a)-[:R]->(b) SET a.p=b.p+1,b.p=a.p+1");
        let frozen = reciprocal.canonical_bytes();
        let result = txn.execute_graph_mutation_governed(&mut db, &query, &reciprocal, policy()).unwrap();
        assert_eq!((result.selection.result_rows, result.effects), (3, 2));
        let rows = txn.vertices(&db).unwrap();
        assert_eq!(value(rows.iter().find(|row| row.vid == VId(1)).unwrap(), P), Some(&CanonicalScalar::Int(21)));
        assert_eq!(value(rows.iter().find(|row| row.vid == VId(2)).unwrap(), P), Some(&CanonicalScalar::Int(13)));
        txn.execute_graph_mutation_governed(&mut db, &query,
            &mutation("MATCH (n) SET n.q=COALESCE(n.q,0)+1,n.p=COALESCE(n.p,0)+2"), policy()).unwrap();
        let counter = mutation("MATCH WALK (a)-[:R*0..2]->(b) SET b.q=COALESCE(b.q,0)+1");
        let result = txn.execute_graph_mutation_governed(&mut db, &query, &counter, policy()).unwrap();
        assert_eq!((result.selection.result_rows, result.effects), (11, 4));
        let rows = txn.vertices(&db).unwrap();
        for (id, expected) in [(1, 23), (2, 15), (3, 2), (4, 2)] {
            let row = rows.iter().find(|row| row.vid == VId(id)).unwrap();
            assert_eq!(value(row, P), Some(&CanonicalScalar::Int(expected)));
            assert_eq!(value(row, Q), Some(&CanonicalScalar::Int(2)));
        }
        let expected = logical(&rows);
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(logical(&db.vertices().unwrap()), before);
        let sequence = txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(sequence.0, basis.0 + 1);
        db.compact(&commit).await.unwrap(); drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(logical(&reopened.vertices().unwrap()), expected);
        assert_eq!(logical(&reopened.vertices_at(basis).unwrap()), before);
        assert_eq!(logical(&pinned.vertices().unwrap()), before);
        assert_eq!(reciprocal.canonical_bytes(), frozen);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_arithmetic_errors_and_effect_limits_leave_every_prior_staged_effect_intact() {
    let ((), report) = run_async_under_lab(0xa817_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let query = contexts.query(); let txcx = contexts.txn();
        for (expression, bad, expected_error) in [
            ("n.p+1", CanonicalScalar::Int(i64::MAX), Some(GraphIntegerErrorKind::Overflow)),
            ("10/n.p", CanonicalScalar::Int(0), Some(GraphIntegerErrorKind::DivisionByZero)),
            ("n.p*2", CanonicalScalar::Bool(true), Some(GraphIntegerErrorKind::NonInteger)),
            ("-n.p", CanonicalScalar::Int(i64::MIN), Some(GraphIntegerErrorKind::Overflow)),
            ("ABS(n.p)", CanonicalScalar::Int(i64::MIN), Some(GraphIntegerErrorKind::Overflow)),
            ("COALESCE(n.p,0)+1", CanonicalScalar::Int(2), None),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit).await;
            let mut setup = WriteBatch::new(R);
            setup.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(1)));
            setup.set_vertex_property(VId(2), P, Some(bad));
            db.write(&commit, setup).await.unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            txn.write(&mut db, prefix()).unwrap();
            let before = logical(&txn.vertices(&db).unwrap());
            let mut budget = policy();
            if expected_error.is_none() { budget.max_effects = 1; }
            let plan = mutation(&format!("MATCH (n) SET n.q={expression}"));
            let result = txn.execute_graph_mutation_governed(&mut db, &query, &plan, budget);
            match expected_error {
                Some(kind) => assert!(matches!(result,
                    Err(GqlQueryError::Source(GraphMutationError::Arithmetic { row: 1, error, .. })) if error.kind == kind)),
                None => assert!(matches!(result, Err(GqlQueryError::Source(GraphMutationError::EffectLimit { .. })))),
            }
            assert_eq!(logical(&txn.vertices(&db).unwrap()), before, "{expression}");
            txn.commit(&mut db, &commit).await.unwrap();
            assert!(db.vertex(VId(777)).unwrap().is_some());
            for row in db.vertices().unwrap() { assert!(value(&row, Q).is_none(), "{expression}"); }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn arithmetic_refusals_and_empty_selections_retain_rejected_and_absent_read_dependencies() {
    let ((), report) = run_async_under_lab(0xa817_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let query = contexts.query(); let txcx = contexts.txn();
        for mode in 0..4 {
            for insertion in [false, true] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let mut extra = WriteBatch::new(R);
                extra.add_edge(EId(104), VId(2), VId(3), vec![]);
                db.write(&commit, extra).await.unwrap();
                let mut txn = db.begin(&txcx).unwrap();
                txn.write(&mut db, prefix()).unwrap();
                let text = match mode {
                    1 => "MATCH (a)-[:R]->(b) WHERE b.p<25 SET a.q=1/(b.p-10)",
                    3 => "MATCH (a)-[:R]->(b) WHERE b.p<0 SET a.q=COALESCE(a.p,0)+1",
                    _ => "MATCH (a)-[:R]->(b) WHERE b.p<25 SET a.q=COALESCE(a.p,0)+1",
                };
                let mut budget = policy();
                if mode == 2 { budget.max_effects = 0; }
                let result = txn.execute_graph_mutation_governed(&mut db, &query, &mutation(text), budget);
                match mode {
                    0 => assert_eq!(result.unwrap().effects, 2),
                    1 => assert!(matches!(result, Err(GqlQueryError::Source(GraphMutationError::Arithmetic { error, .. }))
                        if error.kind == GraphIntegerErrorKind::DivisionByZero)),
                    2 => assert!(matches!(result, Err(GqlQueryError::Source(GraphMutationError::EffectLimit { .. })))),
                    _ => assert_eq!(result.unwrap().effects, 0),
                }
                let mut winner = WriteBatch::new(R);
                if insertion {
                    winner.create_vertex(VId(5), vec![], vec![(P, CanonicalScalar::Int(-1))]);
                    winner.add_edge(EId(105), VId(1), VId(5), vec![]);
                } else {
                    winner.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(-1)));
                }
                db.write(&commit, winner).await.unwrap();
                let frontier = db.frontier().unwrap();
                // No intervening transaction read can repair a missing witness.
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
fn all_five_exact_limits_cover_expression_execution_before_workspace_publication() {
    let ((), report) = run_async_under_lab(0xa817_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let query = contexts.query(); let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let plan = mutation("MATCH (n) SET n.q=COALESCE(n.p,0)+ABS(-2)*3");
        let before = logical(&db.vertices().unwrap());
        let mut txn = db.begin(&txcx).unwrap();
        let measured = txn.execute_graph_mutation_governed(&mut db, &query, &plan, policy()).unwrap();
        txn.abort();
        let exact = GraphMutationPolicy::new(GqlQueryPolicy::new(
            measured.selection.snapshot_records, measured.selection.result_rows,
            measured.evaluator.work_units, measured.evaluator.scratch_entries), measured.effects);
        let mut txn = db.begin(&txcx).unwrap();
        assert_eq!(txn.execute_graph_mutation_governed(&mut db, &query, &plan, exact).unwrap(), measured);
        txn.abort();
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
        assert_eq!(logical(&db.vertices().unwrap()), before);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
