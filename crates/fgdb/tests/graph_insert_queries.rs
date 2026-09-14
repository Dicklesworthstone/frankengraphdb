//! Query-generated vertices and edges use the real transaction/write path.
//! Independent expected records pin topology, properties and match multiplicity.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::{GraphInsertError, GraphInsertPolicy, GraphInsertRequest, PreparedGraphInsert};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    PreparedGraphInsertText, PreparedGraphText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const SOURCE: LabelId = LabelId(1);
const COPY: LabelId = LabelId(9);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const TEXT: &str = "MATCH (a:Source)-[:R]->(b) WHERE a.p >= $floor \
    CREATE (copy:Copy {p:a.p+$step,q:b.p}), \
    (a)-[:R {p:CASE WHEN b.p IS NULL THEN 0 ELSE b.p END}]->(copy), (copy)-[:R]->(b)";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x31; 32], DatabaseSecurityNamespaceId([0x32; 32]), [0x33; 32])
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Label, "Source") => Some(GraphSymbol::Label(SOURCE)),
        (GraphSymbolKind::Label, "Copy") => Some(GraphSymbol::Label(COPY)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn policy() -> GraphInsertPolicy {
    GraphInsertPolicy::new(GqlQueryPolicy::new(10_000, 10_000, 5_000_000, 2_000_000), 10_000, 10_000)
}
fn allocator(request: GraphInsertRequest) -> Result<ElementId, &'static str> {
    Ok(match request {
        GraphInsertRequest::Vertex { row, vertex } => ElementId::Vertex(VId(1_000 + row as u128 * 4 + vertex as u128)),
        GraphInsertRequest::Edge { row, edge } => ElementId::Edge(EId(10_000 + row as u128 * 4 + edge as u128)),
    })
}
fn query(text: &str) -> PreparedGraphInsert {
    PreparedGraphInsertText::prepare(text, R, symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn standard() -> PreparedGraphInsert {
    PreparedGraphInsertText::prepare(TEXT, R, symbols).unwrap().bind_parameters(
        &GqlParameters::new().with_int64("floor", 0).unwrap().with_int64("step", 1).unwrap(),
    ).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for (id, value) in [(1, 10), (2, 20)] {
        batch.create_vertex(VId(id), vec![SOURCE], vec![(P, CanonicalScalar::Int(value))]);
    }
    batch.create_vertex(VId(3), vec![], vec![(P, CanonicalScalar::Null)]);
    batch.create_vertex(VId(98), vec![], vec![]);
    batch.create_vertex(VId(99), vec![], vec![(P, CanonicalScalar::Int(99))]);
    for (id, source, destination) in [(11, 1, 2), (12, 1, 2), (13, 2, 3)] {
        batch.add_edge(EId(id), VId(source), VId(destination), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    let mut retire = WriteBatch::new(R); retire.delete_vertex(VId(98));
    db.write(cx, retire).await.unwrap()
}
fn verify_created(db: &Database<MemVfs>) {
    for (row, source, destination, p, q) in [
        (0_u128, 1, 2, 12, Some(20)), (1, 1, 2, 12, Some(20)), (2, 2, 3, 21, None),
    ] {
        let id = VId(1_000 + row * 4);
        let vertex = db.vertex(id).unwrap().unwrap();
        assert_eq!(vertex.labels, vec![COPY]);
        assert_eq!(vertex.props, vec![(P, CanonicalScalar::Int(p)), (Q, q.map_or(CanonicalScalar::Null, CanonicalScalar::Int))]);
        let edge = db.edge(EId(10_000 + row * 4)).unwrap().unwrap();
        assert_eq!((edge.entry.src, edge.entry.dst, edge.entry.relation), (VId(source), id, R));
        let edge = db.edge(EId(10_001 + row * 4)).unwrap().unwrap();
        assert_eq!((edge.entry.src, edge.entry.dst, edge.entry.relation), (id, VId(destination), R));
    }
}

#[test]
fn creation_consumes_staged_values_publishes_once_and_survives_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0xc8ea_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query(); let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let initial = db.vertices().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prior = WriteBatch::new(R);
        prior.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(11)));
        txn.write(&mut db, prior).unwrap();
        let calls = Cell::new(0);
        let template = PreparedGraphInsertText::prepare(TEXT, R, |kind, name| {
            calls.set(calls.get() + 1); symbols(kind, name)
        }).unwrap();
        assert_eq!(calls.get(), 5);
        let arguments = GqlParameters::new().with_int64("floor", 0).unwrap().with_int64("step", 1).unwrap();
        let insertion = template.bind_parameters(&arguments).unwrap();
        let frozen = insertion.canonical_bytes();
        let stats = txn.execute_graph_insert_governed(&mut db, &cx, &insertion, policy(), allocator).unwrap();
        assert_eq!((stats.selection.result_rows, stats.created_vertices, stats.created_edges), (3, 3, 6));
        assert_eq!(txn.vertices(&db).unwrap().len(), initial.len() + 3);
        assert_eq!(txn.edges(&db).unwrap().len(), 9);
        assert_eq!(db.vertices().unwrap(), initial, "staging does not mutate the durable snapshot");
        assert_eq!(pinned.vertices().unwrap(), initial);
        let committed = txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(committed.0, basis.0 + 1, "one ordinary commit publishes the whole insertion");
        verify_created(&db);
        assert_eq!(calls.get(), 5);
        assert_eq!(template.bind_parameters(&arguments).unwrap().canonical_bytes(), frozen);
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        verify_created(&db);
        let copies = PreparedGraphText::prepare("MATCH (n:Copy) RETURN n", symbols).unwrap()
            .bind_parameters(&GqlParameters::new()).unwrap();
        assert!(db.execute_graph_pattern_governed_at(&cx, &copies, basis, policy().query).unwrap().value.is_empty());
        assert_eq!(db.execute_graph_pattern_governed(&cx, &copies, policy().query).unwrap().value.len(), 3);
        assert_eq!(pinned.vertices().unwrap(), initial);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_allocator_and_storage_identity_failures_preserve_the_exact_prior_workspace() {
    let ((), report) = run_async_under_lab(0xc8ea_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query(); let txcx = contexts.txn();
        for mode in 0..5 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit).await;
            let initial = db.vertices().unwrap(); let initial_edges = db.edges().unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            let mut prior = WriteBatch::new(R); prior.create_vertex(VId(777), vec![], vec![]);
            txn.write(&mut db, prior).unwrap();
            let before = txn.staged_effect_digest().unwrap();
            let requested = Cell::new(0);
            let result = txn.execute_graph_insert_governed(&mut db, &cx, &standard(), policy(), |request| {
                requested.set(requested.get() + 1);
                match (mode, request) {
                    (0, GraphInsertRequest::Edge { row: 2, edge: 1 }) => Err("late allocation refusal"),
                    (1, GraphInsertRequest::Vertex { row: 2, .. }) => Ok(ElementId::Vertex(VId(1))),
                    (2, GraphInsertRequest::Vertex { row: 2, .. }) => Ok(ElementId::Vertex(VId(98))),
                    (3, GraphInsertRequest::Edge { row: 2, edge: 1 }) => Ok(ElementId::Edge(EId(11))),
                    (4, GraphInsertRequest::Vertex { row: 2, .. }) => Ok(ElementId::Vertex(VId(1_000))),
                    _ => allocator(request),
                }
            });
            assert!(result.is_err(), "mode {mode}");
            assert!(requested.get() >= 7, "exercise failure after real partial allocation");
            assert_eq!(txn.staged_effect_digest().unwrap(), before);
            assert_eq!(txn.vertices(&db).unwrap().len(), initial.len() + 1);
            assert_eq!(txn.edges(&db).unwrap(), initial_edges);
            assert_eq!(db.vertices().unwrap(), initial);
            txn.commit(&mut db, &commit).await.unwrap();
            assert!(db.vertex(VId(777)).unwrap().is_some());
            for id in [1_000, 1_004, 1_008, 98] { assert!(db.vertex(VId(id)).unwrap().is_none()); }
            assert_eq!(db.edges().unwrap(), initial_edges);
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn data_and_count_refusals_precede_allocation_and_do_not_discard_earlier_staging() {
    let ((), report) = run_async_under_lab(0xc8ea_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query(); let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut prior = WriteBatch::new(R);
        prior.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(i64::MAX)));
        txn.write(&mut db, prior).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let requested = Cell::new(0);
        for (insertion, allowance) in [
            (standard(), policy()),
            (query("MATCH (a:Source) OPTIONAL MATCH (a)-[:S]->(b) CREATE (copy),(b)-[:R]->(copy)"), policy()),
            (query("MATCH (a:Source)-[:R]->(b) CREATE (copy)"), GraphInsertPolicy::new(policy().query, 2, 100)),
            (query("MATCH (a:Source)-[:R]->(b) CREATE (a)-[:R]->(b)"), GraphInsertPolicy::new(policy().query, 100, 2)),
        ] {
            assert!(txn.execute_graph_insert_governed(&mut db, &cx, &insertion, allowance, |request| {
                requested.set(requested.get() + 1); allocator(request)
            }).is_err());
            assert_eq!(requested.get(), 0);
            assert_eq!(txn.staged_effect_digest().unwrap(), before);
        }
        txn.abort();
        assert_eq!(db.vertex(VId(2)).unwrap().unwrap().props, vec![(P, CanonicalScalar::Int(20))]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn creation_retains_read_and_absence_dependencies_after_success_refusal_or_empty_selection() {
    let ((), report) = run_async_under_lab(0xc8ea_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query(); let txcx = contexts.txn();
        for mode in 0..5 {
            for conflict in 0..3 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let mut txn = db.begin(&txcx).unwrap();
                let mut prior = WriteBatch::new(R); prior.create_vertex(VId(777), vec![], vec![]);
                txn.write(&mut db, prior).unwrap();
                let text = match mode {
                    2 => "MATCH (a:Source)-[:R]->(b) WHERE a.p >= 10 CREATE (copy:Copy {p:a.p/0})",
                    4 => "MATCH (a:Source)-[:R]->(b) WHERE a.p >= 1000 CREATE (copy:Copy {p:a.p})",
                    _ => "MATCH (a:Source)-[:R]->(b) WHERE a.p >= 10 CREATE (copy:Copy {p:a.p})",
                };
                let allowance = GraphInsertPolicy::new(policy().query, if mode == 1 { 0 } else { 100 }, 100);
                let result = txn.execute_graph_insert_governed(&mut db, &cx, &query(text), allowance,
                    |request| if mode == 3 { Err("identity unavailable") } else { allocator(request) });
                assert_eq!(result.is_ok(), mode == 0 || mode == 4);
                // No transaction query between insertion and commit: a later
                // scan must not repair a missing creation-source observation.
                let mut winner = WriteBatch::new(R);
                match conflict {
                    0 => { winner.create_vertex(VId(888), vec![], vec![]); }
                    1 => { winner.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(1001))); }
                    _ => { winner.add_edge(EId(14), VId(1), VId(3), vec![]); }
                }
                db.write(&commit, winner).await.unwrap();
                let frontier = db.frontier().unwrap();
                let result = txn.commit(&mut db, &commit).await;
                if conflict == 0 {
                    result.unwrap();
                    assert!(db.vertex(VId(777)).unwrap().is_some());
                } else {
                    assert!(matches!(result, Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01", ..
                    }))));
                    assert_eq!(db.frontier().unwrap(), frontier);
                    assert!(db.vertex(VId(777)).unwrap().is_none());
                    assert!(db.vertex(VId(1_000)).unwrap().is_none());
                }
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn owner_basis_relation_and_exact_query_budgets_guard_the_public_creation_entrypoint() {
    let ((), report) = run_async_under_lab(0xc8ea_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query(); let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap(); seed(&mut db, &commit).await;
        let mut foreign = Database::open_memory(&commit, keys()).await.unwrap(); seed(&mut foreign, &commit).await;
        let insertion = standard();
        let mut measured_txn = db.begin(&txcx).unwrap();
        let measured = measured_txn.execute_graph_insert_governed(&mut db, &cx, &insertion, policy(), allocator).unwrap();
        measured_txn.abort();
        let exact = GraphInsertPolicy::new(GqlQueryPolicy::new(measured.selection.snapshot_records,
            measured.selection.result_rows, measured.evaluator.work_units, measured.evaluator.scratch_entries), 3, 6);
        let mut txn = db.begin(&txcx).unwrap();
        assert_eq!(txn.execute_graph_insert_governed(&mut db, &cx, &insertion, exact, allocator).unwrap(), measured);
        txn.abort();
        for query_budget in [
            GqlQueryPolicy::new(measured.selection.snapshot_records - 1, 100, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(100, measured.selection.result_rows - 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(100, 100, measured.evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(100, 100, u64::MAX, measured.evaluator.scratch_entries - 1),
        ] {
            let mut txn = db.begin(&txcx).unwrap();
            assert!(txn.execute_graph_insert_governed(&mut db, &cx, &insertion,
                GraphInsertPolicy::new(query_budget, 3, 6), allocator).is_err());
            assert!(txn.vertex(&db, VId(1_000)).unwrap().is_none()); txn.abort();
        }
        let mut txn = db.begin(&txcx).unwrap();
        let calls = Cell::new(0);
        let result = txn.execute_graph_insert_governed(&mut foreign, &cx, &insertion, policy(), |request| {
            calls.set(calls.get() + 1); allocator(request)
        });
        assert!(matches!(result, Err(GqlQueryError::Source(GraphInsertError::Source(WriteTxnError::WrongDatabase)))));
        let mut prior = WriteBatch::new(R); prior.create_vertex(VId(777), vec![], vec![]); txn.write(&mut db, prior).unwrap();
        let other_relation = PreparedGraphInsertText::prepare("MATCH (n) CREATE (copy)", S, symbols).unwrap()
            .bind_parameters(&GqlParameters::new()).unwrap();
        assert!(matches!(txn.execute_graph_insert_governed(&mut db, &cx, &other_relation, policy(), |request| {
            calls.set(calls.get() + 1); allocator(request)
        }), Err(GqlQueryError::Source(GraphInsertError::Source(WriteTxnError::RelationMismatch { .. })))));
        let mut advance = WriteBatch::new(R); advance.create_vertex(VId(888), vec![], vec![]); db.write(&commit, advance).await.unwrap();
        assert!(matches!(txn.execute_graph_insert_governed(&mut db, &cx, &insertion, policy(), |request| {
            calls.set(calls.get() + 1); allocator(request)
        }), Err(GqlQueryError::Source(GraphInsertError::Source(WriteTxnError::SnapshotAdvanced { .. })))));
        assert_eq!(calls.get(), 0); txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
