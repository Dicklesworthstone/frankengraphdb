//! Rejected programs retain observations; accepted programs publish only once.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, VertexRow, WriteBatch, WriteError, WriteTxnError};
use fgdb_chronicle::commit::CrashPoint;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphMutationError, GraphMutationPolicy,
    GraphMutationProgramDimension as Dimension, GraphMutationProgramError as Error,
    GraphSymbol, GraphSymbolKind, PreparedGraphMutationProgram, PreparedGraphMutationProgramTemplate,
    PreparedGraphMutationText,
};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
type Logical = Vec<(VId, Vec<(PropertyKeyId, CanonicalScalar)>)>;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x51; 32], DatabaseSecurityNamespaceId([0x52; 32]), [0x53; 32])
}
fn policy() -> GraphMutationPolicy {
    GraphMutationPolicy::new(GqlQueryPolicy::new(100_000, 10_000, 10_000_000, 10_000_000), 10_000)
}
fn input(text: &str) -> PreparedGraphMutationText {
    PreparedGraphMutationText::prepare(text, R, |kind, name| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }).unwrap()
}
fn program(statements: &[&str]) -> PreparedGraphMutationProgram {
    PreparedGraphMutationProgram::prepare(statements.iter().map(|text| {
        input(text).bind_parameters(&GqlParameters::new()).unwrap()
    }).collect()).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for id in 1..=3 {
        batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64 * 10))]);
    }
    for (id, source, target) in [(101, 1, 2), (102, 1, 2), (103, 2, 1), (104, 2, 3)] {
        batch.add_edge(EId(id), VId(source), VId(target), vec![]);
    }
    db.write(cx, batch).await.unwrap()
}
fn prefix() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(777), vec![], vec![]);
    batch
}
fn logical(rows: &[VertexRow]) -> Logical {
    rows.iter().map(|row| (row.vid, row.props.clone())).collect()
}
fn property(row: &VertexRow, key: PropertyKeyId) -> Option<&CanonicalScalar> {
    row.props.iter().find(|(property, _)| *property == key).map(|(_, value)| value)
}

#[test]
fn rejected_candidates_and_phantoms_survive_success_failure_empty_and_final_refusal() {
    let ((), report) = run_async_under_lab(0xb10c_2001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let query = contexts.query(); let txcx = contexts.txn();
        for mode in 0..5 {
            for insertion in [false, true] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let first = if mode == 3 {
                    "MATCH (a)-[:R]->(b) WHERE b.p<0 SET a.q=COALESCE(a.q,0)+1"
                } else { "MATCH (a)-[:R]->(b) WHERE b.p<25 SET a.q=COALESCE(a.q,0)+1" };
                let second = if mode == 1 {
                    "MATCH (n) WHERE n.q=1 SET n.q=1/(n.p-10)"
                } else { "MATCH (n) WHERE n.q=1 SET n.q=n.q+1" };
                let program = program(&[first, second]);
                let mut budget = policy();
                if mode == 2 { budget.max_effects = 2; }
                if mode == 4 {
                    let mut measuring = db.begin(&txcx).unwrap();
                    measuring.write(&mut db, prefix()).unwrap();
                    let measured = measuring.execute_graph_mutation_program_governed(&mut db, &query, &program, policy()).unwrap();
                    measuring.abort();
                    budget.query.evaluator.max_work_units = measured.evaluator.work_units - 1;
                }
                let mut txn = db.begin(&txcx).unwrap();
                txn.write(&mut db, prefix()).unwrap();
                let result = txn.execute_graph_mutation_program_governed(&mut db, &query, &program, budget);
                match mode {
                    0 => assert_eq!(result.unwrap().effects, 4),
                    1 => assert!(matches!(result, Err(Error::Statement { statement: 1,
                        source: GqlQueryError::Source(GraphMutationError::Arithmetic { .. }) }))),
                    2 => assert!(matches!(result, Err(Error::Budget { statement: 1, dimension: Dimension::Effects, .. }))),
                    3 => assert_eq!(result.unwrap().effects, 0),
                    _ => assert!(matches!(result, Err(Error::Budget { statement: 2, dimension: Dimension::WorkUnits, .. }))),
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
                // No transaction query occurs after the program: missing read
                // evidence cannot be repaired before this commit validates it.
                let outcome = txn.commit(&mut db, &commit).await;
                assert!(matches!(outcome, Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01", ..
                }))));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertex(VId(777)).unwrap().is_none());
                for row in db.vertices().unwrap() { assert!(property(&row, Q).is_none()); }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn deleting_then_failing_restores_incident_edges_and_a_successful_retry_is_durable() {
    let ((), report) = run_async_under_lab(0xb10c_2002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let query = contexts.query(); let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let before = logical(&db.vertices().unwrap());
        let pinned = db.read_session().unwrap();
        let failing = program(&[
            "MATCH (n) WHERE n.p=20 DETACH DELETE n",
            "MATCH (n) WHERE n.p>=10 SET n.q=1/(n.p-30)",
        ]);
        let mut txn = db.begin(&txcx).unwrap();
        txn.write(&mut db, prefix()).unwrap();
        let failed = txn.execute_graph_mutation_program_governed(&mut db, &query, &failing, policy());
        assert!(matches!(failed, Err(Error::Statement { statement: 1,
            source: GqlQueryError::Source(GraphMutationError::Arithmetic { .. }) })));
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(db.edges().unwrap().len(), 4, "all discarded cascade effects were restored");
        assert!(db.vertex(VId(2)).unwrap().is_some());
        assert!(db.vertex(VId(777)).unwrap().is_some());
        for row in db.vertices().unwrap() { assert!(property(&row, Q).is_none()); }
        let accepted = program(&[
            "MATCH (n) WHERE n.p=20 DETACH DELETE n",
            "MATCH (n) WHERE n.p>=10 SET n.q=n.p+1",
        ]);
        let mut txn = db.begin(&txcx).unwrap();
        let stats = txn.execute_graph_mutation_program_governed(&mut db, &query, &accepted, policy()).unwrap();
        assert_eq!(stats.effects, 3, "one explicit delete and two updates, not cascade-cost accounting");
        assert!(txn.edges(&db).unwrap().is_empty());
        assert_eq!(db.edges().unwrap().len(), 4, "staging is not durable visibility");
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap(); drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert!(reopened.edges().unwrap().is_empty());
        assert!(reopened.vertex(VId(2)).unwrap().is_none());
        assert_eq!(property(&reopened.vertex(VId(1)).unwrap().unwrap(), Q), Some(&CanonicalScalar::Int(11)));
        assert_eq!(property(&reopened.vertex(VId(3)).unwrap().unwrap(), Q), Some(&CanonicalScalar::Int(31)));
        assert_eq!(logical(&reopened.vertices_at(basis).unwrap()), before);
        assert_eq!(reopened.edges_at(basis).unwrap().len(), 4);
        assert_eq!(logical(&pinned.vertices().unwrap()), before);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn parameterized_programs_recover_as_a_whole_before_or_after_the_commit_marker() {
    let ((), report) = run_async_under_lab(0xb10c_2003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let query = contexts.query(); let txcx = contexts.txn();
        let template = PreparedGraphMutationProgramTemplate::prepare(vec![
            input("MATCH (n) WHERE n.p>=10 SET n.q=COALESCE(n.q,0)+$step"),
            input("MATCH (n) WHERE n.q>=$step SET n.p=n.p+$step"),
        ]).unwrap();
        assert_eq!(template.parameter_schema()[0].occurrences, 3);
        let program = template.bind_parameters(&GqlParameters::new().with_int64("step", 2).unwrap()).unwrap();
        let frozen = program.canonical_bytes();
        for crash in [Some(CrashPoint::BeforeCapsule), Some(CrashPoint::AfterD1), None] {
            let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
            let basis = seed(&mut db, &commit).await;
            let before = logical(&db.vertices().unwrap());
            let mut txn = db.begin(&txcx).unwrap();
            let stats = txn.execute_graph_mutation_program_governed(&mut db, &query, &program, policy()).unwrap();
            assert_eq!((stats.completed_statements, stats.effects), (2, 6));
            assert_eq!(logical(&db.vertices().unwrap()), before);
            assert_eq!(db.frontier().unwrap(), basis);
            let result = txn.commit_with_crash(&mut db, &commit, crash).await;
            if crash.is_some() { assert!(result.is_err()); } else { assert_eq!(result.unwrap().0, basis.0 + 1); }
            drop(db);
            let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
            if crash.is_some() {
                // Both injected points precede a marker: an orphan capsule is
                // not evidence that any program step committed.
                assert_eq!(reopened.frontier().unwrap(), basis);
                assert_eq!(logical(&reopened.vertices().unwrap()), before);
            } else {
                assert_eq!(reopened.frontier().unwrap().0, basis.0 + 1);
                for row in reopened.vertices().unwrap() {
                    assert_eq!(property(&row, P), Some(&CanonicalScalar::Int(row.vid.0 as i64 * 10 + 2)));
                    assert_eq!(property(&row, Q), Some(&CanonicalScalar::Int(2)));
                }
            }
            assert_eq!(reopened.edges().unwrap().len(), 4);
            assert_eq!(program.canonical_bytes(), frozen);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
