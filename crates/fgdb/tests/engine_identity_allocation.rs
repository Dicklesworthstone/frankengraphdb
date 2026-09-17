//! Allocation follows committed creation history, not just the live graph.
//! Never-committed reservations are not claimed to be durable leases.
use asupersync::lab::run_async_under_lab;
use fgdb::{CrashPoint, Database, DatabaseKeys, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertRequest, PreparedGraphInsert};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphEdgeMergeOutcome, GraphSymbol, GraphSymbolKind,
    GraphVertexMergeOutcome, GraphWriteProgramPolicy, PreparedGraphEdgeMergeText,
    PreparedGraphInsertText, PreparedGraphVertexMergeText, PreparedGraphWriteProgram,
    PreparedGraphWriteProgramTemplate,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId,
    EmbeddedTxnCompletion, PurposeContexts, VId,
};
use std::path::{Path, PathBuf};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const VERTEX: GraphInsertRequest = GraphInsertRequest::Vertex { row: 0, vertex: 0 };
const EDGE: GraphInsertRequest = GraphInsertRequest::Edge { row: 0, edge: 0 };

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xcd; 32], DatabaseSecurityNamespaceId([0x7c; 32]), [0x71; 32])
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-engine-identity-{}-{name}", std::process::id()))
}

fn under_lab<Fut>(seed: u64, test: impl FnOnce(PurposeContexts) -> Fut + Send + 'static)
where
    Fut: Future<Output = ()> + Send + 'static,
{
    let ((), report) = run_async_under_lab(seed, |root| async move {
        test(PurposeContexts::narrow_runtime_root(&root)).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn insertion(text: &str) -> PreparedGraphInsert {
    PreparedGraphInsertText::prepare(text, R, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}

fn query_policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

fn insert_policy() -> GraphInsertPolicy {
    GraphInsertPolicy::new(query_policy(), 100, 100)
}

fn program_policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(query_policy(), 100, 100, 100)
}

async fn deleted_maxima(cx: &CommitCx, path: &Path) -> Database {
    let mut db = Database::create(cx, path, keys()).await.unwrap();
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(7), vec![], vec![]);
    seed.create_vertex(VId(1000), vec![], vec![]);
    seed.add_edge(EId(9000), VId(7), VId(1000), vec![]);
    db.write(cx, seed).await.unwrap();
    let mut deletion = WriteBatch::new(R);
    deletion.delete_edge(EId(9000));
    deletion.delete_vertex(VId(1000));
    db.write(cx, deletion).await.unwrap();
    assert!(db.vertex(VId(1000)).unwrap().is_none());
    assert!(db.edge(EId(9000)).unwrap().is_none());
    db
}

#[test]
fn deleted_maxima_survive_fast_open_rebuild_and_compaction() {
    under_lab(0xcd70, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        for rebuilding in [false, true] {
            let path = scratch(if rebuilding { "delete-rebuild" } else { "delete-fast" });
            drop(deleted_maxima(&commit, &path).await);
            let mut db = if rebuilding {
                Database::open_rebuilding(&commit, &path, keys()).await.unwrap()
            } else {
                Database::open(&commit, &path, keys()).await.unwrap()
            };
            db.compact(&commit).await.unwrap();
            let vertex = db.allocate_identity(&query, VERTEX).unwrap();
            let edge = db.allocate_identity(&query, EDGE).unwrap();
            assert_eq!(vertex, ElementId::Vertex(VId(1001)));
            assert_eq!(edge, ElementId::Edge(EId(9001)));
            let mut created = WriteBatch::new(R);
            created.create_vertex(VId(1001), vec![PERSON], vec![(P, CanonicalScalar::Int(11))]);
            created.add_edge(EId(9001), VId(7), VId(1001), vec![]);
            db.write(&commit, created).await.unwrap();
            drop(db);

            let mut db = Database::open(&commit, &path, keys()).await.unwrap();
            assert_eq!(db.vertex(VId(1001)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(11))]);
            let edge = db.edge(EId(9001)).unwrap().unwrap();
            assert_eq!((edge.entry.src, edge.entry.dst), (VId(7), VId(1001)));
            assert!(db.vertex(VId(1000)).unwrap().is_none());
            assert!(db.edge(EId(9000)).unwrap().is_none());
            let (_, vertices, edges, completion) = db
                .execute_graph_insert_returning_autocommit_engine_governed(
                    &txcx, &query, &commit,
                    &insertion("CREATE (a:Person {p:12})-[:R]->(b:Person {p:13})"),
                    insert_policy(),
                ).await.unwrap();
            assert_eq!(vertices, vec![VId(1002), VId(1003)]);
            assert_eq!(edges, vec![EId(9002)]);
            assert!(matches!(completion, EmbeddedTxnCompletion::WriteCommitted { .. }));
            drop(db);
            let db = Database::open_rebuilding(&commit, &path, keys()).await.unwrap();
            assert_eq!(db.vertex(VId(1003)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(13))]);
            let edge = db.edge(EId(9002)).unwrap().unwrap();
            assert_eq!((edge.entry.src, edge.entry.dst), (VId(1002), VId(1003)));
        }
    });
}

#[test]
fn same_basis_staged_engine_inserts_are_disjoint_and_loser_cannot_publish() {
    under_lab(0xcd71, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let path = scratch("same-basis");
        let mut db = deleted_maxima(&commit, &path).await;
        let basis = db.frontier().unwrap();
        let mut first = db.begin(&txcx).unwrap();
        let mut second = db.begin(&txcx).unwrap();
        // Both insertions depend on the same observed vertex population.
        assert_eq!(first.vertices(&db).unwrap().into_iter().map(|row| row.vid).collect::<Vec<_>>(), vec![VId(7)]);
        assert_eq!(second.vertices(&db).unwrap().into_iter().map(|row| row.vid).collect::<Vec<_>>(), vec![VId(7)]);
        let create = insertion("INSERT (a:Person {p:1})-[:R]->(b:Person {p:2})");
        let (_, first_vertices, first_edges) = first.execute_graph_insert_returning_engine_governed(
            &mut db, &query, &create, insert_policy(),
        ).unwrap();
        let (_, second_vertices, second_edges) = second.execute_graph_insert_returning_engine_governed(
            &mut db, &query, &create, insert_policy(),
        ).unwrap();
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(first_vertices, vec![VId(1001), VId(1002)]);
        assert_eq!(first_edges, vec![EId(9001)]);
        assert_eq!(second_vertices, vec![VId(1003), VId(1004)]);
        assert_eq!(second_edges, vec![EId(9002)]);
        assert!(first_vertices.iter().all(|id| !second_vertices.contains(id)));
        assert!(first_edges.iter().all(|id| !second_edges.contains(id)));
        assert!(first.vertex(&db, first_vertices[0]).unwrap().is_some());
        assert!(second.vertex(&db, second_vertices[0]).unwrap().is_some());
        assert!(db.vertex(first_vertices[0]).unwrap().is_none());
        let committed = first.commit(&mut db, &commit).await.unwrap();
        assert!(matches!(second.commit(&mut db, &commit).await,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))));
        assert_eq!(db.frontier().unwrap(), committed);
        for id in &second_vertices {
            assert!(db.vertex(*id).unwrap().is_none());
        }
        assert!(db.edge(second_edges[0]).unwrap().is_none());
        // On the opened handle even the losing reservations remain disjoint.
        assert_eq!(db.allocate_identity(&query, VERTEX).unwrap(), ElementId::Vertex(VId(1005)));
        assert_eq!(db.allocate_identity(&query, EDGE).unwrap(), ElementId::Edge(EId(9003)));
        drop(db);

        let mut db = Database::open(&commit, &path, keys()).await.unwrap();
        let (_, vertices, edges, _) = db.execute_graph_insert_returning_autocommit_engine_governed(
            &txcx, &query, &commit, &create, insert_policy(),
        ).await.unwrap();
        // No promise is made about the loser's never-committed reservations.
        assert!(vertices.iter().all(|id| id.0 > first_vertices[1].0));
        assert!(edges.iter().all(|id| id.0 > first_edges[0].0));
        drop(db);
        let db = Database::open_rebuilding(&commit, &path, keys()).await.unwrap();
        for id in first_vertices.iter().chain(vertices.iter()) {
            assert!(db.vertex(*id).unwrap().is_some());
        }
        for id in first_edges.iter().chain(edges.iter()) {
            assert!(db.edge(*id).unwrap().is_some());
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn d1_d2_recovery_uses_only_committed_creation_maxima() {
    under_lab(0xcd72, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        for (name, point, tear, committed) in [
            ("before-d1", Some(CrashPoint::AfterCapsuleBeforeD1), false, false),
            ("after-d1", Some(CrashPoint::AfterD1), false, false),
            ("marker-survived", Some(CrashPoint::AfterMarkerBeforeD2), false, true),
            ("marker-torn", Some(CrashPoint::AfterMarkerBeforeD2), true, false),
            ("marker-synced", Some(CrashPoint::AfterMarkerFileSyncBeforeDirectorySync), false, true),
            ("d2-complete", None, false, true),
        ] {
            let path = scratch(name);
            let mut db = deleted_maxima(&commit, &path).await;
            let basis = db.frontier().unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            let create = insertion("CREATE (a:Person {p:21})-[:R]->(b:Person {p:22})");
            let (_, vertices, edges) = txn.execute_graph_insert_returning_engine_governed(
                &mut db, &query, &create, insert_policy(),
            ).unwrap();
            assert_eq!(vertices, vec![VId(1001), VId(1002)]);
            assert_eq!(edges, vec![EId(9001)]);
            let result = txn.commit_with_crash(&mut db, &commit, point).await;
            assert_eq!(result.is_ok(), point.is_none(), "injection reached: {name}");
            if matches!(point, Some(CrashPoint::AfterMarkerBeforeD2
                | CrashPoint::AfterMarkerFileSyncBeforeDirectorySync)) {
                assert!(matches!(result,
                    Err(WriteTxnError::Write(WriteError::CommitOutcomeUnknown { .. }))));
                assert!(db.frontier().is_err());
            }
            drop(db);
            if tear {
                // Existing fault helper models a torn marker, not a new power-loss oracle.
                fgdb_chronicle::CommitCoordinator::<asupersync::fs::UnixVfs>::tear_log_tail_for_test(
                    &path, 1,
                ).unwrap();
            }
            let next_vertex = if committed { 1003 } else { 1001 };
            let next_edge = if committed { 9002 } else { 9001 };
            for rebuilding in [false, true] {
                let mut recovered = if rebuilding {
                    Database::open_rebuilding(&commit, &path, keys()).await.unwrap()
                } else {
                    Database::open(&commit, &path, keys()).await.unwrap()
                };
                assert_eq!(recovered.frontier().unwrap(), CommitSeq(basis.0 + u64::from(committed)), "{name}");
                for id in &vertices {
                    assert_eq!(recovered.vertex(*id).unwrap().is_some(), committed, "{name}");
                }
                assert_eq!(recovered.edge(edges[0]).unwrap().is_some(), committed, "{name}");
                assert_eq!(recovered.allocate_identity(&query, VERTEX).unwrap(),
                    ElementId::Vertex(VId(next_vertex)), "{name}");
                assert_eq!(recovered.allocate_identity(&query, EDGE).unwrap(),
                    ElementId::Edge(EId(next_edge)), "{name}");
            }
            let mut recovered = Database::open(&commit, &path, keys()).await.unwrap();
            let (_, vertices, edges, completion) = recovered
                .execute_graph_insert_returning_autocommit_engine_governed(
                    &txcx, &query, &commit, &create, insert_policy(),
                ).await.unwrap();
            assert_eq!(vertices, vec![VId(next_vertex), VId(next_vertex + 1)], "{name}");
            assert_eq!(edges, vec![EId(next_edge)], "{name}");
            assert!(matches!(completion, EmbeddedTxnCompletion::WriteCommitted { .. }));
            drop(recovered);
            let recovered = Database::open_rebuilding(&commit, &path, keys()).await.unwrap();
            let edge = recovered.edge(edges[0]).unwrap().unwrap();
            assert_eq!((edge.entry.src, edge.entry.dst), (vertices[0], vertices[1]), "{name}");
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
}

fn merge_program() -> PreparedGraphWriteProgram {
    let left = PreparedGraphVertexMergeText::prepare("MERGE (n:Person {p:$left})", R, symbols).unwrap();
    let right = PreparedGraphVertexMergeText::prepare("MERGE (n:Person {p:$right})", R, symbols).unwrap();
    let edge = PreparedGraphEdgeMergeText::prepare(
        "MATCH (a:Person),(b:Person) WHERE a.p=$left AND b.p=$right MERGE (a)-[:R]->(b)",
        R, symbols,
    ).unwrap();
    PreparedGraphWriteProgramTemplate::prepare(vec![
        left.into(), right.into(), edge.clone().into(), edge.into(),
    ]).unwrap().bind_parameters(
        &GqlParameters::new().with_int64("left", 31).unwrap().with_int64("right", 32).unwrap(),
    ).unwrap()
}

#[test]
fn engine_insert_create_and_merge_programs_need_no_caller_allocator() {
    under_lab(0xcd73, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let path = scratch("programs");
        let mut db = deleted_maxima(&commit, &path).await;
        let mut txn = db.begin(&txcx).unwrap();
        let stats = txn.execute_graph_insert_engine_governed(
            &mut db, &query, &insertion("INSERT (n:Person {p:30})"), insert_policy(),
        ).unwrap();
        assert_eq!(stats.created_vertices, 1);
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(db.vertex(VId(1001)).unwrap().unwrap().props, vec![(P, CanonicalScalar::Int(30))]);
        let (stats, completion) = db.execute_graph_insert_autocommit_engine_governed(
            &txcx, &query, &commit, &insertion("CREATE (n:Person {p:33})"), insert_policy(),
        ).await.unwrap();
        assert_eq!(stats.created_vertices, 1);
        assert!(matches!(completion, EmbeddedTxnCompletion::WriteCommitted { .. }));

        let program = merge_program();
        let mut txn = db.begin(&txcx).unwrap();
        let receipt = txn.execute_graph_write_program_returning_engine_governed(
            &mut db, &query, &program, program_policy(),
        ).unwrap();
        assert_eq!(receipt.steps()[0].merged_vertex(), Some(GraphVertexMergeOutcome::Created(VId(1003))));
        assert_eq!(receipt.steps()[1].merged_vertex(), Some(GraphVertexMergeOutcome::Created(VId(1004))));
        assert_eq!(receipt.steps()[2].merged_edge(), Some(GraphEdgeMergeOutcome::Created(EId(9001))));
        assert_eq!(receipt.steps()[3].merged_edge(), Some(GraphEdgeMergeOutcome::Matched(EId(9001))));
        txn.commit(&mut db, &commit).await.unwrap();
        drop(db);
        let mut db = Database::open(&commit, &path, keys()).await.unwrap();
        let edge = db.edge(EId(9001)).unwrap().unwrap();
        assert_eq!((edge.entry.src, edge.entry.dst, edge.entry.relation), (VId(1003), VId(1004), R));
        let frontier = db.frontier().unwrap();
        let (receipt, completion) = db.execute_graph_write_program_returning_autocommit_engine_governed(
            &txcx, &query, &commit, &program, program_policy(),
        ).await.unwrap();
        assert_eq!(receipt.steps()[0].merged_vertex(), Some(GraphVertexMergeOutcome::Matched(VId(1003))));
        assert_eq!(receipt.steps()[1].merged_vertex(), Some(GraphVertexMergeOutcome::Matched(VId(1004))));
        for step in &receipt.steps()[2..] {
            assert_eq!(step.merged_edge(), Some(GraphEdgeMergeOutcome::Matched(EId(9001))));
        }
        assert!(matches!(completion, EmbeddedTxnCompletion::ReadClosed { .. }));
        let mut txn = db.begin(&txcx).unwrap();
        let stats = txn.execute_graph_write_program_engine_governed(
            &mut db, &query, &program, program_policy(),
        ).unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (0, 0));
        assert!(matches!(txn.finish(&mut db, &commit).await.unwrap(), EmbeddedTxnCompletion::ReadClosed { .. }));
        let (stats, completion) = db.execute_graph_write_program_autocommit_engine_governed(
            &txcx, &query, &commit, &program, program_policy(),
        ).await.unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (0, 0));
        assert!(matches!(completion, EmbeddedTxnCompletion::ReadClosed { .. }));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(db.allocate_identity(&query, VERTEX).unwrap(), ElementId::Vertex(VId(1005)));
        assert_eq!(db.allocate_identity(&query, EDGE).unwrap(), ElementId::Edge(EId(9002)));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn deleted_u128_maxima_exhaust_each_identity_kind_without_wrapping() {
    under_lab(0xcd74, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let path = scratch("exhausted");
        let mut db = Database::create(&commit, &path, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(u128::MAX), vec![], vec![]);
        seed.add_edge(EId(u128::MAX - 1), VId(1), VId(u128::MAX), vec![]);
        db.write(&commit, seed).await.unwrap();
        assert!(matches!(db.allocate_identity(&query, VERTEX), Err(WriteTxnError::IdentityExhausted)));
        let mut txn = db.begin(&txcx).unwrap();
        assert_eq!(txn.allocate_identity(&mut db, &query, EDGE).unwrap(), ElementId::Edge(EId(u128::MAX)));
        let mut last = WriteBatch::new(R);
        last.add_edge(EId(u128::MAX), VId(1), VId(u128::MAX), vec![]);
        txn.write(&mut db, last).unwrap();
        txn.commit(&mut db, &commit).await.unwrap();
        let mut deletion = WriteBatch::new(R);
        deletion.delete_vertex(VId(u128::MAX));
        db.write(&commit, deletion).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        for rebuilding in [false, true] {
            let mut db = if rebuilding {
                Database::open_rebuilding(&commit, &path, keys()).await.unwrap()
            } else {
                Database::open(&commit, &path, keys()).await.unwrap()
            };
            assert!(db.vertex(VId(u128::MAX)).unwrap().is_none());
            assert!(db.edge(EId(u128::MAX)).unwrap().is_none());
            assert!(matches!(db.allocate_identity(&query, VERTEX), Err(WriteTxnError::IdentityExhausted)));
            assert!(matches!(db.allocate_identity(&query, EDGE), Err(WriteTxnError::IdentityExhausted)));
            assert!(db.vertex(VId(0)).unwrap().is_none());
            assert!(db.edge(EId(0)).unwrap().is_none());
        }
    });
}
