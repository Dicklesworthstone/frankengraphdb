//! Imported rows, matched endpoints and graph creation share the native
//! transaction workspace. These are production parser/engine/VFS tests.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, IdentityPermutation, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertRequest};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
    PreparedGraphInsertQueryText, PreparedGraphInsertText, PreparedGraphWriteProgram,
    PreparedGraphWriteScript,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::sync::atomic::{AtomicUsize, Ordering};

const R: RelationId = RelationId(1);
const SOURCE: LabelId = LabelId(1);
const COPY: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xc4; 32],
        DatabaseSecurityNamespaceId([0xc5; 32]),
        [0xc6; 32],
    )
}
/// The vertex identity the engine issues for `counter` under [`keys`].
fn engine_vertex(counter: u64) -> VId {
    VId(u128::from(
        IdentityPermutation::vertices(&keys())
            .permute(counter)
            .unwrap(),
    ))
}
/// The edge identity the engine issues for `counter` under [`keys`].
fn engine_edge(counter: u64) -> EId {
    EId(u128::from(
        IdentityPermutation::edges(&keys())
            .permute(counter)
            .unwrap(),
    ))
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Source") => Some(GraphSymbol::Label(SOURCE)),
        (GraphSymbolKind::Label, "Copy") => Some(GraphSymbol::Label(COPY)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000, 1_000, 5_000_000, 2_000_000)
}
fn program(text: &str, parameters: &GqlParameters) -> PreparedGraphWriteProgram {
    PreparedGraphWriteScript::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(parameters)
        .unwrap()
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

#[test]
fn bulk_connect_script_observes_staged_sources_and_reopens_at_one_frontier() {
    let ((), report) = run_async_under_lab(0xb01c_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let before = db.frontier().unwrap();
        let parameters = GqlParameters::new()
            .with_list("keys", vec![int(2), int(1), int(2)])
            .unwrap();
        let plan = program(
            "UNWIND [1,2,2] AS value CREATE (:Source {p:value});
             UNWIND $keys AS wanted MATCH (a:Source {p:wanted})
             CREATE (a)-[:R {p:wanted+10}]->(:Copy {p:a.p+100});
             MATCH (c:Copy) SET c.q=c.p*2",
            &parameters,
        );
        let (stats, _) = db
            .execute_graph_write_program_autocommit_engine_governed(
                &txcx,
                &cx,
                &commit,
                &plan,
                GraphWriteProgramPolicy::new(policy(), 1_000, 8, 5),
            )
            .await
            .unwrap();
        assert_eq!(stats.completed_statements, 3);
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(db.delta_since(before).unwrap().count(), 1);
        assert_eq!(db.vertices().unwrap().len(), 8);
        assert_eq!(db.edges().unwrap().len(), 5);
        // The first statement takes vertex counters 1, 2 and 3 for its Source
        // rows p=1, p=2 and p=2. The second statement's rows follow $keys
        // [2,1,2], each key's matches in ascending identity order, and each row
        // takes its Copy vertex counter (4..=8) and edge counter (1..=5) in
        // row order.
        let sources = [engine_vertex(1), engine_vertex(2), engine_vertex(3)];
        let (low, high) = (sources[1].min(sources[2]), sources[1].max(sources[2]));
        for (edge, source, destination, key) in [
            (1, low, 4, 2),
            (2, high, 5, 2),
            (3, sources[0], 6, 1),
            (4, low, 7, 2),
            (5, high, 8, 2),
        ] {
            let destination = engine_vertex(destination);
            let stored = db.edge(engine_edge(edge)).unwrap().unwrap();
            assert_eq!((stored.entry.src, stored.entry.dst), (source, destination));
            assert_eq!(stored.props, vec![(P, CanonicalScalar::Int(key + 10))]);
            assert_eq!(
                db.vertex(destination).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(key + 100)),
                    (Q, CanonicalScalar::Int((key + 100) * 2)),
                ]
            );
        }
        let vertices = db.vertices().unwrap();
        let edges = db.edges().unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(reopened.frontier().unwrap().0, before.0 + 1);
        assert_eq!(reopened.vertices().unwrap(), vertices);
        assert_eq!(reopened.edges().unwrap(), edges);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_late_script_failure_preserves_only_the_outer_transaction_prefix() {
    let ((), report) = run_async_under_lab(0xb01c_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let before = db.frontier().unwrap();
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(10), vec![SOURCE], vec![(P, CanonicalScalar::Int(1))]);
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let plan = program(
            "UNWIND [1,1] AS wanted MATCH (a:Source {p:wanted})
             CREATE (a)-[:R]->(:Copy {p:wanted});
             MATCH (c:Copy) SET c.q=1/0",
            &GqlParameters::new(),
        );
        assert!(
            txn.execute_graph_write_program_engine_governed(
                &mut db,
                &contexts.query(),
                &plan,
                GraphWriteProgramPolicy::new(policy(), 1_000, 1_000, 1_000),
            )
            .is_err()
        );
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert!(txn.vertex(&db, VId(10)).unwrap().is_some());
        assert!(db.vertices().unwrap().is_empty());
        assert!(db.edges().unwrap().is_empty());
        assert_eq!(db.frontier().unwrap(), before);
        txn.finish(&mut db, &contexts.commit()).await.unwrap();
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert!(db.edges().unwrap().is_empty());
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn returning_imports_keep_matched_and_created_endpoint_functions_in_their_slots() {
    let ((), report) = run_async_under_lab(0xb01c_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![SOURCE], vec![(P, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![SOURCE], vec![(P, CanonicalScalar::Int(2))]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&contexts.commit(), seed).await.unwrap();
        let before = db.frontier().unwrap();
        let query = PreparedGraphInsertQueryText::prepare(
            "UNWIND [1,1] AS wanted MATCH (a:Source {p:wanted})-[r:R]->(b)
             CREATE (a)<-[e:R {p:wanted}]-(c:Copy {p:b.p+wanted})
             RETURN wanted,startNode(r) AS old_start,endNode(r) AS old_end,
                    startNode(e) AS new_start,endNode(e) AS new_end,c.p AS value
             ORDER BY new_start",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let (stats, rows, _) = db
            .execute_graph_insert_query_autocommit_engine_governed(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                &query,
                GraphInsertPolicy::new(policy(), 2, 2),
            )
            .await
            .unwrap();
        // Each UNWIND row takes one Copy vertex counter and one edge counter,
        // in row order. ORDER BY new_start then sorts by the Copy identity.
        let created = [
            (engine_edge(1), engine_vertex(1)),
            (engine_edge(2), engine_vertex(2)),
        ];
        let mut starts = created.map(|(_, vertex)| vertex);
        starts.sort();
        let expected: Vec<_> = starts
            .into_iter()
            .map(|created| {
                GraphValueRow::from_owned_values(vec![
                    int(1),
                    GraphValue::Vertex(VId(1)),
                    GraphValue::Vertex(VId(2)),
                    GraphValue::Vertex(created),
                    GraphValue::Vertex(VId(1)),
                    int(3),
                ])
            })
            .collect();
        assert_eq!(rows.value, expected);
        assert_eq!((stats.created_vertices, stats.created_edges), (2, 2));
        for (edge, created) in created {
            let edge = db.edge(edge).unwrap().unwrap();
            assert_eq!((edge.entry.src, edge.entry.dst), (created, VId(1)));
        }
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn invalid_import_values_and_creation_caps_refuse_before_requesting_an_identity() {
    let ((), report) = run_async_under_lab(0xb01c_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![SOURCE], vec![(P, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![SOURCE], vec![(P, CanonicalScalar::Int(2))]);
        db.write(&contexts.commit(), seed).await.unwrap();
        let before = db.frontier().unwrap();
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        for (text, cap) in [
            (
                "UNWIND [1,0] AS x MATCH (a:Source) CREATE (:Copy {p:10/x})",
                10,
            ),
            ("UNWIND [1,1] AS x MATCH (a:Source) CREATE (:Copy {p:x})", 3),
            (
                "UNWIND [1,[2]] AS x MATCH (a:Source) CREATE (:Copy {p:x})",
                10,
            ),
        ] {
            let insertion = PreparedGraphInsertText::prepare(text, R, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            let calls = AtomicUsize::new(0);
            let result = txn.execute_graph_insert_governed(
                &mut db,
                &contexts.query(),
                &insertion,
                GraphInsertPolicy::new(policy(), cap, 10),
                |request| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, ()>(match request {
                        GraphInsertRequest::Vertex { row, vertex } => {
                            ElementId::Vertex(VId(100 + row as u128 * 10 + vertex as u128))
                        }
                        GraphInsertRequest::Edge { row, edge } => {
                            ElementId::Edge(EId(100 + row as u128 * 10 + edge as u128))
                        }
                    })
                },
            );
            assert!(result.is_err(), "{text}");
            assert_eq!(calls.load(Ordering::SeqCst), 0, "{text}");
            assert_eq!(txn.staged_effect_digest().unwrap(), digest);
            assert_eq!(db.frontier().unwrap(), before);
        }
        txn.abort();
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn invalid_imported_script_scopes_refuse_before_any_statement_catalog_resolution() {
    for tail in [
        "UNWIND [1] AS x MATCH (x) CREATE (:Copy)",
        "UNWIND [1] AS x MATCH (a:Source) SET a.p=x",
        "UNWIND [1] AS x MATCH (a:Source) MERGE (a)-[:R]->(b)",
        "UNWIND [1] AS x MATCH (a:Source) WHERE a.p=x OR a.p=0 CREATE (:Copy)",
    ] {
        let text = format!("CREATE (:Source {{p:1}}); {tail}");
        let calls = AtomicUsize::new(0);
        let error = PreparedGraphWriteScript::prepare(&text, R, |kind, name| {
            calls.fetch_add(1, Ordering::SeqCst);
            symbols(kind, name)
        })
        .unwrap_err();
        assert_eq!(error.statement, Some(1), "{tail}");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{tail}");
    }
}
