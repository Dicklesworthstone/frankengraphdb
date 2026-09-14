//! One ordered program stages dependent mutations or none of its effects.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, VertexRow, WriteBatch, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphMutationError, GraphMutationPolicy,
    GraphMutationProgramDimension as Dimension, GraphMutationProgramError as Error,
    GraphSymbol, GraphSymbolKind, PreparedGraphMutationProgram, PreparedGraphMutationText,
};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const MARKED: LabelId = LabelId(1);
type Logical = Vec<(VId, Vec<LabelId>, Vec<(PropertyKeyId, CanonicalScalar)>)>;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x41; 32], DatabaseSecurityNamespaceId([0x42; 32]), [0x43; 32])
}
fn policy() -> GraphMutationPolicy {
    GraphMutationPolicy::new(GqlQueryPolicy::new(100_000, 10_000, 10_000_000, 10_000_000), 10_000)
}
fn program(statements: &[&str]) -> PreparedGraphMutationProgram { program_at(statements, R) }
fn program_at(statements: &[&str], relation: RelationId) -> PreparedGraphMutationProgram {
    PreparedGraphMutationProgram::prepare(statements.iter().map(|text| {
        PreparedGraphMutationText::prepare(text, relation, |kind, name| match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
            (GraphSymbolKind::Label, "Marked") => Some(GraphSymbol::Label(MARKED)),
            _ => None,
        }).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
    }).collect()).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, commit: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(10))]);
    batch.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(20))]);
    batch.create_vertex(VId(3), vec![], vec![]);
    for (id, source, target) in [(101, 1, 2), (102, 1, 2), (103, 2, 1)] {
        batch.add_edge(EId(id), VId(source), VId(target), vec![]);
    }
    db.write(commit, batch).await.unwrap()
}
fn logical(rows: &[VertexRow]) -> Logical {
    rows.iter().map(|row| (row.vid, row.labels.clone(), row.props.clone())).collect()
}
fn property(row: &VertexRow, key: PropertyKeyId) -> Option<&CanonicalScalar> {
    row.props.iter().find(|(property, _)| *property == key).map(|(_, value)| value)
}
fn prefix() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(777), vec![], vec![]);
    batch
}

#[test]
fn dependent_steps_see_prior_effects_then_publish_one_commit_with_reopen_and_history() {
    let ((), report) = run_async_under_lab(0xb10c_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let query = contexts.query(); let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let before = logical(&db.vertices().unwrap());
        let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut earlier = WriteBatch::new(R);
        earlier.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(12)));
        txn.write(&mut db, earlier).unwrap();
        let program = program(&[
            "MATCH (n) WHERE n.p>=10 SET n.p=n.p+1,n:Marked",
            "MATCH (n:Marked) WHERE n.p>=20 SET n.q=n.p*2",
            "MATCH (n:Marked) REMOVE n:Marked",
        ]);
        let frozen = program.canonical_bytes();
        let stats = txn.execute_graph_mutation_program_governed(&mut db, &query, &program, policy()).unwrap();
        assert_eq!(stats.completed_statements, 3);
        assert_eq!((stats.selection.result_rows, stats.target_vertex_visits, stats.effects), (5, 5, 7));
        let staged = txn.vertices(&db).unwrap();
        assert_eq!(property(&staged[0], P), Some(&CanonicalScalar::Int(13)));
        assert_eq!(property(&staged[1], P), Some(&CanonicalScalar::Int(21)));
        assert_eq!(property(&staged[1], Q), Some(&CanonicalScalar::Int(42)));
        assert!(property(&staged[0], Q).is_none());
        assert!(staged.iter().all(|row| row.labels.is_empty()));
        let expected = logical(&staged);
        assert_eq!(logical(&db.vertices().unwrap()), before);
        assert_eq!(db.frontier().unwrap(), basis);
        let sequence = txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(sequence.0, basis.0 + 1);
        assert_eq!(logical(&db.vertices().unwrap()), expected);
        db.compact(&commit).await.unwrap(); drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(logical(&reopened.vertices().unwrap()), expected);
        assert_eq!(logical(&reopened.vertices_at(basis).unwrap()), before);
        assert_eq!(logical(&pinned.vertices().unwrap()), before);
        assert_eq!(program.canonical_bytes(), frozen);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn later_arithmetic_conflict_and_quota_errors_restore_all_earlier_program_steps() {
    let ((), report) = run_async_under_lab(0xb10c_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let query = contexts.query(); let txcx = contexts.txn();
        for mode in 0..4 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let _ = seed(&mut db, &commit).await;
            let mut extra = WriteBatch::new(R);
            extra.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(30)));
            extra.add_edge(EId(104), VId(1), VId(3), vec![]);
            db.write(&commit, extra).await.unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            txn.write(&mut db, prefix()).unwrap();
            let later = match mode {
                0 => "MATCH (n:Marked) SET n.p=1/(n.p-20)",
                1 => "MATCH (a)-[:R]->(b) SET a.q=b.p",
                _ => "MATCH (n:Marked) SET n.p=5",
            };
            let program = program(&["MATCH (n) WHERE n.p>=10 SET n.q=9,n:Marked", later]);
            let mut budget = policy();
            if mode == 2 { budget.max_effects = 6; }
            if mode == 3 { budget.query = GqlQueryPolicy::new(100_000, 3, 10_000_000, 10_000_000); }
            let result = txn.execute_graph_mutation_program_governed(&mut db, &query, &program, budget);
            match mode {
                0 => assert!(matches!(result, Err(Error::Statement { statement: 1,
                    source: GqlQueryError::Source(GraphMutationError::Arithmetic { .. }) }))),
                1 => assert!(matches!(result, Err(Error::Statement { statement: 1,
                    source: GqlQueryError::Source(GraphMutationError::ConflictingAssignment { .. }) }))),
                2 => assert!(matches!(result, Err(Error::Budget { statement: 1, dimension: Dimension::Effects, .. }))),
                _ => assert!(matches!(result, Err(Error::Budget { statement: 1, dimension: Dimension::SelectedRows, .. }))),
            }
            // Preserve and commit the pre-program write, with no repair query.
            txn.commit(&mut db, &commit).await.unwrap();
            assert!(db.vertex(VId(777)).unwrap().is_some());
            for row in db.vertices().unwrap() {
                assert!(property(&row, Q).is_none());
                assert!(row.labels.is_empty());
                if row.vid != VId(777) {
                    assert_eq!(property(&row, P), Some(&CanonicalScalar::Int(row.vid.0 as i64 * 10)));
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_cumulative_limits_and_the_final_acceptance_boundary_precede_workspace_visibility() {
    let ((), report) = run_async_under_lab(0xb10c_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let query = contexts.query(); let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let original = logical(&db.vertices().unwrap());
        let program = program(&[
            "MATCH (n) WHERE n.p>=10 SET n.q=COALESCE(n.q,0)+1",
            "MATCH (n) WHERE n.q=1 SET n.q=n.q+1",
        ]);
        let mut txn = db.begin(&txcx).unwrap();
        let stats = txn.execute_graph_mutation_program_governed(&mut db, &query, &program, policy()).unwrap();
        txn.abort();
        let exact = GraphMutationPolicy::new(GqlQueryPolicy::new(
            stats.selection.snapshot_records, stats.selection.result_rows,
            stats.evaluator.work_units, stats.evaluator.scratch_entries), stats.effects);
        let mut txn = db.begin(&txcx).unwrap();
        assert_eq!(txn.execute_graph_mutation_program_governed(&mut db, &query, &program, exact).unwrap(), stats);
        txn.abort();
        for (dimension, budget) in [
            (Dimension::SnapshotRecords, GraphMutationPolicy::new(GqlQueryPolicy::new(stats.selection.snapshot_records-1, u64::MAX, u64::MAX, u64::MAX), u64::MAX)),
            (Dimension::SelectedRows, GraphMutationPolicy::new(GqlQueryPolicy::new(u64::MAX, stats.selection.result_rows-1, u64::MAX, u64::MAX), u64::MAX)),
            (Dimension::WorkUnits, GraphMutationPolicy::new(GqlQueryPolicy::new(u64::MAX, u64::MAX, stats.evaluator.work_units-1, u64::MAX), u64::MAX)),
            (Dimension::ScratchEntries, GraphMutationPolicy::new(GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, stats.evaluator.scratch_entries-1), u64::MAX)),
            (Dimension::Effects, GraphMutationPolicy::new(GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX), stats.effects-1)),
        ] {
            let mut txn = db.begin(&txcx).unwrap();
            let result = txn.execute_graph_mutation_program_governed(&mut db, &query, &program, budget);
            match result {
                Err(Error::Budget { statement, dimension: found, .. }) => {
                    assert_eq!(found, dimension);
                    if dimension == Dimension::WorkUnits { assert_eq!(statement, 2, "both stages completed before final refusal"); }
                }
                other => panic!("expected a cumulative refusal: {other:?}"),
            }
            assert_eq!(logical(&txn.vertices(&db).unwrap()), original);
            assert!(matches!(txn.commit(&mut db, &commit).await, Err(WriteTxnError::NoPreparedWrite)));
            assert_eq!(logical(&db.vertices().unwrap()), original);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_steps_continue_and_owner_lifecycle_basis_and_relation_checks_win_over_zero_budget() {
    let ((), report) = run_async_under_lab(0xb10c_1004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let query = contexts.query(); let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let plan = program(&["MATCH (n) WHERE n.p<0 SET n.q=9", "MATCH (n) WHERE n.p>=10 SET n.q=1"]);
        let mut txn = db.begin(&txcx).unwrap();
        let stats = txn.execute_graph_mutation_program_governed(&mut db, &query, &plan, policy()).unwrap();
        assert_eq!((stats.completed_statements, stats.effects), (2, 2));
        txn.commit(&mut db, &commit).await.unwrap();
        let zero = GraphMutationPolicy::new(GqlQueryPolicy::new(0, 0, 0, 0), 0);
        assert!(matches!(txn.execute_graph_mutation_program_governed(&mut db, &query, &plan, zero),
            Err(Error::Preflight(WriteTxnError::Finished))));
        let mut foreign = Database::open_memory(&commit, keys()).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        assert!(matches!(txn.execute_graph_mutation_program_governed(&mut foreign, &query, &plan, zero),
            Err(Error::Preflight(WriteTxnError::WrongDatabase))));
        txn.write(&mut db, prefix()).unwrap();
        let different = program_at(&["MATCH (n) SET n.q=1"], RelationId(2));
        assert!(matches!(txn.execute_graph_mutation_program_governed(&mut db, &query, &different, zero),
            Err(Error::Preflight(WriteTxnError::RelationMismatch { .. }))));
        db.write(&commit, prefix()).await.unwrap();
        assert!(matches!(txn.execute_graph_mutation_program_governed(&mut db, &query, &plan, zero),
            Err(Error::Preflight(WriteTxnError::SnapshotAdvanced { .. }))));
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
