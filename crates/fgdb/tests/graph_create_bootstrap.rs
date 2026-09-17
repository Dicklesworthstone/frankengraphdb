//! Native CREATE initializes the actual Chronicle/Strata database, not a test
//! graph model. Unit-input writes must not invent whole-graph read dependencies.

use asupersync::lab::run_async_under_lab;
use fgdb::{CrashPoint, Database, DatabaseKeys, MemVfs, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::{
    GraphInsertError, GraphInsertPolicy, GraphInsertRequest, PreparedGraphInsert,
};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    PreparedGraphInsertText, PreparedGraphText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, EmbeddedTxnState,
    PurposeContexts, VId,
};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const NODE: LabelId = LabelId(1);
const COPY: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const BOOTSTRAP: &str = "CREATE (root:Node {p:$seed})-[:R {p:$seed}]->(child:Node {p:$seed+1}), \
    (child)-[:R]->(root),(root)-[:R]->(root),(:Node {p:NULL})";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x41; 32],
        DatabaseSecurityNamespaceId([0x42; 32]),
        [0x43; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Node") => Some(GraphSymbol::Label(NODE)),
        (GraphSymbolKind::Label, "Copy") => Some(GraphSymbol::Label(COPY)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn policy() -> GraphInsertPolicy {
    GraphInsertPolicy::new(
        GqlQueryPolicy::new(0, 1, 1_000_000, 1_000_000),
        1_000,
        1_000,
    )
}
fn read_policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000, 1_000, 1_000_000, 1_000_000)
}
fn query(text: &str) -> PreparedGraphInsert {
    PreparedGraphInsertText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn allocator(base: u128, request: GraphInsertRequest) -> Result<ElementId, &'static str> {
    Ok(match request {
        GraphInsertRequest::Vertex { row, vertex } => {
            ElementId::Vertex(VId(base + row as u128 * 16 + vertex as u128))
        }
        GraphInsertRequest::Edge { row, edge } => {
            ElementId::Edge(EId(base + row as u128 * 16 + edge as u128))
        }
    })
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![NODE], vec![(P, CanonicalScalar::Int(5))]);
    batch.create_vertex(VId(98), vec![], vec![]);
    db.write(cx, batch).await.unwrap();
    let mut retire = WriteBatch::new(R);
    retire.delete_vertex(VId(98));
    db.write(cx, retire).await.unwrap();
}
fn verify_bootstrap(db: &Database<MemVfs>, base: u128, seed: i64) {
    for (offset, value) in [(0, Some(seed)), (1, Some(seed + 1)), (2, None)] {
        let vertex = db.vertex(VId(base + offset)).unwrap().unwrap();
        assert_eq!(vertex.labels, vec![NODE]);
        assert_eq!(
            vertex.props,
            vec![(P, value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))]
        );
    }
    for (offset, source, destination) in [(0, base, base + 1), (1, base + 1, base), (2, base, base)]
    {
        let edge = db.edge(EId(base + offset)).unwrap().unwrap();
        assert_eq!(
            (edge.entry.src, edge.entry.dst, edge.entry.relation),
            (VId(source), VId(destination), R)
        );
        let properties = if offset == 0 {
            vec![(P, CanonicalScalar::Int(seed))]
        } else {
            vec![]
        };
        assert_eq!(edge.props, properties);
    }
}

#[test]
fn empty_database_bootstrap_and_dependent_match_publish_once_and_reopen_with_history() {
    let ((), report) = run_async_under_lab(0xc8eb_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let calls = Cell::new(0);
        let template = PreparedGraphInsertText::prepare(BOOTSTRAP, R, |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        })
        .unwrap();
        assert_eq!(calls.get(), 3);
        let first = template
            .bind_parameters(&GqlParameters::new().with_int64("seed", 10).unwrap())
            .unwrap();
        let second = template
            .bind_parameters(&GqlParameters::new().with_int64("seed", 20).unwrap())
            .unwrap();
        let frozen = first.canonical_bytes();
        let mut txn = db.begin(&txcx).unwrap();
        let before = txn
            .execute_graph_insert_governed(&mut db, &cx, &first, policy(), |request| {
                allocator(100, request)
            })
            .unwrap();
        let after = txn
            .execute_graph_insert_governed(&mut db, &cx, &second, policy(), |request| {
                allocator(200, request)
            })
            .unwrap();
        assert_eq!(
            before, after,
            "unit cost/cardinality must not grow with staged graph population"
        );
        assert_eq!(
            (
                before.selection.snapshot_records,
                before.selection.result_rows
            ),
            (0, 1)
        );
        assert_eq!((before.created_vertices, before.created_edges), (3, 3));
        assert_eq!(txn.vertices(&db).unwrap().len(), 6);
        assert_eq!(txn.edges(&db).unwrap().len(), 6);
        assert!(db.vertices().unwrap().is_empty());
        let attach = query("MATCH (n:Node) WHERE n.p=10 CREATE (n)-[:R]->(copy:Copy {p:n.p+100})");
        let attached = txn
            .execute_graph_insert_governed(
                &mut db,
                &cx,
                &attach,
                GraphInsertPolicy::new(read_policy(), 10, 10),
                |request| allocator(1_000, request),
            )
            .unwrap();
        assert_eq!(
            (
                attached.selection.result_rows,
                attached.created_vertices,
                attached.created_edges
            ),
            (1, 1, 1)
        );
        let mut expected_vertices = txn.vertices(&db).unwrap();
        let mut expected_edges = txn.edges(&db).unwrap();
        let committed = txn
            .finish(&mut db, &commit)
            .await
            .unwrap()
            .commit_seq()
            .unwrap();
        assert_eq!(committed, CommitSeq(basis.0 + 1));
        // All rows were created in this workspace. Staged rows carry the basis
        // placeholder; publication supplies the actual sequence, not a new
        // birth ordinal, property value, endpoint or retirement state.
        for row in &mut expected_vertices {
            assert_eq!(row.created_at, basis);
            row.created_at = committed;
        }
        for row in &mut expected_edges {
            assert_eq!(row.entry.created_at, basis);
            row.entry.created_at = committed;
        }
        assert_eq!(db.vertices().unwrap(), expected_vertices);
        assert_eq!(db.edges().unwrap(), expected_edges);
        verify_bootstrap(&db, 100, 10);
        verify_bootstrap(&db, 200, 20);
        let copy = db.vertex(VId(1_000)).unwrap().unwrap();
        assert_eq!(copy.labels, vec![COPY]);
        assert_eq!(copy.props, vec![(P, CanonicalScalar::Int(110))]);
        assert_eq!(db.edge(EId(1_000)).unwrap().unwrap().entry.src, VId(100));
        assert_eq!(calls.get(), 3);
        assert_eq!(first.canonical_bytes(), frozen);
        assert!(pinned.vertices().unwrap().is_empty());
        db.compact(&commit).await.unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(reopened.vertices().unwrap(), expected_vertices);
        assert_eq!(reopened.edges().unwrap(), expected_edges);
        let all = PreparedGraphText::prepare("MATCH (n) RETURN n", symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        assert!(
            reopened
                .execute_graph_pattern_governed_at(&cx, &all, basis, read_policy())
                .unwrap()
                .value
                .is_empty()
        );
        assert_eq!(
            reopened
                .execute_graph_pattern_governed(&cx, &all, read_policy())
                .unwrap()
                .value
                .len(),
            7
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unit_insertion_does_not_invent_phantoms_or_erase_earlier_absence_observations() {
    let ((), report) = run_async_under_lab(0xc8eb_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        for observe in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit).await;
            let mut txn = db.begin(&txcx).unwrap();
            if observe {
                assert!(txn.vertex(&db, VId(900)).unwrap().is_none());
            }
            let insertion = query("CREATE (a:Node {p:1})-[:R]->(b:Node {p:2})");
            let stats = txn
                .execute_graph_insert_governed(&mut db, &cx, &insertion, policy(), |request| {
                    allocator(100, request)
                })
                .unwrap();
            assert_eq!(stats.selection.snapshot_records, 0);
            // No transaction read after insertion. A scan here would invalidate
            // the no-spurious-phantom discriminator and could repair lost reads.
            let mut winner = WriteBatch::new(R);
            winner.create_vertex(VId(900), vec![NODE], vec![]);
            db.write(&commit, winner).await.unwrap();
            let frontier = db.frontier().unwrap();
            let completed = txn.finish(&mut db, &commit).await;
            if observe {
                assert!(matches!(
                    completed,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01",
                        ..
                    }))
                ));
                assert_eq!(txn.state(), EmbeddedTxnState::Aborted);
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertex(VId(100)).unwrap().is_none());
            } else {
                assert_eq!(
                    completed.unwrap().commit_seq(),
                    Some(CommitSeq(frontier.0 + 1))
                );
                assert!(db.vertex(VId(100)).unwrap().is_some());
                assert!(db.vertex(VId(101)).unwrap().is_some());
                assert!(db.edge(EId(100)).unwrap().is_some());
            }
            assert!(db.vertex(VId(900)).unwrap().is_some());
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn standalone_failures_preserve_prior_staging_without_recycling_allocated_ids() {
    let ((), report) = run_async_under_lab(0xc8eb_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        for mode in 0..7 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit).await;
            let mut txn = db.begin(&txcx).unwrap();
            let mut prior = WriteBatch::new(R);
            prior.create_vertex(VId(777), vec![], vec![]);
            txn.write(&mut db, prior).unwrap();
            let before = txn.staged_effect_digest().unwrap();
            let insertion = query(if mode == 0 {
                "CREATE (a {p:1})-[:R]->(b {p:1/0})"
            } else {
                "CREATE (a {p:1})-[:R]->(b {p:2})"
            });
            let allowance = if mode == 1 {
                GraphInsertPolicy::new(policy().query, 1, 10)
            } else {
                policy()
            };
            let calls = Cell::new(0);
            let result =
                txn.execute_graph_insert_governed(&mut db, &cx, &insertion, allowance, |request| {
                    calls.set(calls.get() + 1);
                    match (mode, request) {
                        (2, GraphInsertRequest::Edge { .. }) => Err("identity service refused"),
                        (3, GraphInsertRequest::Vertex { .. }) => Ok(ElementId::Edge(EId(100))),
                        (4, GraphInsertRequest::Vertex { .. }) => Ok(ElementId::Vertex(VId(100))),
                        (5, GraphInsertRequest::Vertex { vertex: 0, .. }) => {
                            Ok(ElementId::Vertex(VId(1)))
                        }
                        (6, GraphInsertRequest::Vertex { vertex: 0, .. }) => {
                            Ok(ElementId::Vertex(VId(98)))
                        }
                        _ => allocator(100, request),
                    }
                });
            assert!(result.is_err(), "mode {mode}");
            assert_eq!(calls.get(), [0, 0, 3, 1, 2, 3, 3][mode]);
            assert_eq!(txn.staged_effect_digest().unwrap(), before);
            assert_eq!(txn.vertices(&db).unwrap().len(), 2);
            assert!(txn.edges(&db).unwrap().is_empty());
            txn.finish(&mut db, &commit).await.unwrap();
            assert!(db.vertex(VId(777)).unwrap().is_some());
            for id in [98, 100, 101] {
                assert!(db.vertex(VId(id)).unwrap().is_none());
            }
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn nullable_match_endpoints_are_not_reinterpreted_as_inline_new_nodes() {
    let ((), report) = run_async_under_lab(0xc8eb_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut txn = db.begin(&txcx).unwrap();
        let insertion = query(
            "MATCH (n:Node) OPTIONAL MATCH (n)-[:R]->(missing) \
            CREATE (missing)-[:R]->(copy:Copy {p:1})",
        );
        assert_eq!(insertion.vertices_per_row(), 1);
        let calls = Cell::new(0);
        let result = txn.execute_graph_insert_governed(
            &mut db,
            &cx,
            &insertion,
            GraphInsertPolicy::new(read_policy(), 100, 100),
            |request| {
                calls.set(calls.get() + 1);
                allocator(100, request)
            },
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphInsertError::NullEndpoint { .. }))
        ));
        assert_eq!(calls.get(), 0);
        assert_eq!(
            txn.finish(&mut db, &commit).await.unwrap().commit_seq(),
            None
        );
        let mut txn = db.begin(&txcx).unwrap();
        let standalone = query("CREATE (missing)-[:R]->(copy:Copy {p:1})");
        assert_eq!(standalone.vertices_per_row(), 2);
        txn.execute_graph_insert_governed(&mut db, &cx, &standalone, policy(), |request| {
            allocator(100, request)
        })
        .unwrap();
        txn.finish(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(100)).unwrap().is_some());
        assert_eq!(db.edge(EId(100)).unwrap().unwrap().entry.dst, VId(101));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn standalone_creation_still_checks_owner_and_basis_before_allocating() {
    let ((), report) = run_async_under_lab(0xc8eb_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut foreign = Database::open_memory(&commit, keys()).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let insertion = query("CREATE (:Node {p:1})");
        let calls = Cell::new(0);
        let denied = GraphInsertPolicy::new(GqlQueryPolicy::new(0, 0, 0, 0), 0, 0);
        let result =
            txn.execute_graph_insert_governed(&mut foreign, &cx, &insertion, denied, |request| {
                calls.set(calls.get() + 1);
                allocator(100, request)
            });
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphInsertError::Source(
                WriteTxnError::WrongDatabase
            )))
        ));
        assert_eq!(calls.get(), 0);
        let mut winner = WriteBatch::new(R);
        winner.create_vertex(VId(1), vec![], vec![]);
        db.write(&commit, winner).await.unwrap();
        let result =
            txn.execute_graph_insert_governed(&mut db, &cx, &insertion, denied, |request| {
                calls.set(calls.get() + 1);
                allocator(100, request)
            });
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphInsertError::Source(
                WriteTxnError::SnapshotAdvanced { .. }
            )))
        ));
        assert_eq!(calls.get(), 0);
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn standalone_bootstrap_recovery_never_exposes_a_partial_created_structure() {
    let ((), report) = run_async_under_lab(0xc8eb_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        for phase in 0..4 {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            let insertion = PreparedGraphInsertText::prepare(BOOTSTRAP, R, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new().with_int64("seed", 10).unwrap())
                .unwrap();
            txn.execute_graph_insert_governed(&mut db, &cx, &insertion, policy(), |request| {
                allocator(100, request)
            })
            .unwrap();
            let crash = match phase {
                0 => Some(CrashPoint::BeforeCapsule),
                1 => Some(CrashPoint::AfterD1),
                2 => Some(CrashPoint::AfterMarkerBeforeD2),
                _ => None,
            };
            let outcome = txn.finish_with_crash(&mut db, &commit, crash).await;
            assert_eq!(outcome.is_ok(), phase == 3);
            assert_eq!(txcx.outstanding_obligations(), 0);
            drop(db);
            let db = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            let counts = (db.vertices().unwrap().len(), db.edges().unwrap().len());
            if phase < 2 {
                assert_eq!(counts, (0, 0));
            } else if phase == 3 {
                assert_eq!(counts, (3, 3));
            } else {
                assert!(counts == (0, 0) || counts == (3, 3));
            }
            if counts == (3, 3) {
                verify_bootstrap(&db, 100, 10);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
