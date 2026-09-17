//! Native source -> typed arguments -> ordinary atomic staging -> Chronicle.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::{GraphInsertLimitDimension, GraphInsertRequest};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphEdgeMergeOutcome, GraphSymbol, GraphSymbolKind,
    GraphWriteProgramError, GraphWriteProgramPolicy, GraphWriteScriptExecutionError,
    PreparedGraphWriteScript,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion, PurposeContexts, VId,
};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn policy(effects: u64, vertices: u64, edges: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(20_000, 20_000, 2_000_000, 2_000_000),
        effects,
        vertices,
        edges,
    )
}
fn script(source: &str) -> PreparedGraphWriteScript {
    PreparedGraphWriteScript::prepare(source, R, symbols).unwrap()
}
fn arguments(fresh: i64, seen: i64) -> GqlParameters {
    GqlParameters::new()
        .with_int64("left", 1)
        .unwrap()
        .with_int64("right", 2)
        .unwrap()
        .with_int64("fresh", fresh)
        .unwrap()
        .with_int64("seen", seen)
        .unwrap()
}

#[test]
fn parameterized_script_commits_once_rebinds_without_allocation_and_survives_reopen() {
    let ((), report) = run_async_under_lab(0x5c71_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let definition = script(
            "MERGE (n:Person {p:$left});
             MERGE (n:Person {p:$right});
             MATCH (a:Person),(b:Person) WHERE a.p=$left AND b.p=$right
             MERGE (a)-[e:R]->(b) ON CREATE SET e.q=$fresh ON MATCH SET e.q=$seen;",
        );
        let before = db.frontier().unwrap();
        // The autocommit future is awaited under the lab runtime, so a counter the
        // allocator closure holds across that await must be Sync.
        let allocated = AtomicUsize::new(0);
        let (receipt, completion) = db
            .execute_graph_write_script_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &definition,
                &arguments(200, 100),
                policy(1, 2, 1),
                |request| {
                    allocated.fetch_add(1, Ordering::Relaxed);
                    Ok::<_, ()>(match (request.statement, request.request) {
                        (0, GraphInsertRequest::Vertex { row: 0, vertex: 0 }) => {
                            ElementId::Vertex(VId(1))
                        }
                        (1, GraphInsertRequest::Vertex { row: 0, vertex: 0 }) => {
                            ElementId::Vertex(VId(2))
                        }
                        (2, GraphInsertRequest::Edge { row: 0, edge: 0 }) => {
                            ElementId::Edge(EId(10))
                        }
                        _ => panic!("unexpected statement-scoped identity request"),
                    })
                },
            )
            .await
            .unwrap();
        assert!(
            matches!(completion, EmbeddedTxnCompletion::WriteCommitted { commit_seq }
            if commit_seq.0 == before.0 + 1)
        );
        assert_eq!(allocated.load(Ordering::Relaxed), 3);
        assert_eq!(receipt.stats().completed_statements, 3);
        assert_eq!(
            (
                receipt.stats().created_vertices,
                receipt.stats().created_edges
            ),
            (2, 1)
        );
        assert_eq!(
            receipt.steps()[2].merged_edge(),
            Some(GraphEdgeMergeOutcome::Created(EId(10)))
        );
        assert_eq!(
            db.edge(EId(10)).unwrap().unwrap().props,
            vec![(Q, CanonicalScalar::Int(200))]
        );

        let (receipt, completion) = db
            .execute_graph_write_script_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &definition,
                &arguments(999, 101),
                policy(1, 0, 0),
                |_| -> Result<ElementId, ()> { panic!("existing matches may not allocate") },
            )
            .await
            .unwrap();
        assert!(
            matches!(completion, EmbeddedTxnCompletion::WriteCommitted { commit_seq }
            if commit_seq.0 == before.0 + 2)
        );
        assert_eq!(
            (
                receipt.stats().created_vertices,
                receipt.stats().created_edges
            ),
            (0, 0)
        );
        assert_eq!(
            receipt.steps()[2].merged_edge(),
            Some(GraphEdgeMergeOutcome::Matched(EId(10)))
        );
        assert_eq!(receipt.stats().mutation_effects, 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(db.edges().unwrap().len(), 1);
        assert_eq!(
            db.edge(EId(10)).unwrap().unwrap().props,
            vec![(Q, CanonicalScalar::Int(101))]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_binding_refusal_cannot_run_the_earlier_creation_or_allocate_an_identity() {
    let ((), report) = run_async_under_lab(0x5c71_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let source = "CREATE (n {p:1});\n\u{2003}MATCH (n) SET n.q=$missing;";
        let definition = script(source);
        let before = db.frontier().unwrap();
        let result = db
            .execute_graph_write_script_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &definition,
                &GqlParameters::new(),
                policy(1, 1, 0),
                |_| -> Result<ElementId, ()> { panic!("a bind refusal must precede allocation") },
            )
            .await;
        assert!(
            matches!(result, Err(GraphWriteScriptExecutionError::Binding(error))
            if error.statement == Some(1) && error.offset == source.find('$').unwrap())
        );
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);

        let mut transaction = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![], vec![]);
        transaction.write(&mut db, prefix).unwrap();
        let effect = transaction.staged_effect_digest().unwrap();
        assert!(matches!(
            transaction.execute_graph_write_script_governed(
                &mut db,
                &query,
                &definition,
                &GqlParameters::new(),
                policy(1, 1, 0),
                |_| -> Result<ElementId, ()> { panic!("a bind refusal must not stage") },
            ),
            Err(GraphWriteScriptExecutionError::Binding(_))
        ));
        assert_eq!(transaction.staged_effect_digest().unwrap(), effect);
        transaction.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_creation_quota_rolls_back_the_script_not_the_outer_transaction_prefix() {
    let ((), report) = run_async_under_lab(0x5c71_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut transaction = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![], vec![(P, CanonicalScalar::Int(99))]);
        transaction.write(&mut db, prefix).unwrap();
        let before = transaction.staged_effect_digest().unwrap();
        let allocations = Cell::new(0);
        let result = transaction.execute_graph_write_script_governed(
            &mut db,
            &query,
            &script("MERGE (n:Person {p:1}); MERGE (n:Person {p:2});"),
            &GqlParameters::new(),
            policy(0, 1, 0),
            |_| {
                allocations.set(allocations.get() + 1);
                Ok::<_, ()>(ElementId::Vertex(VId(10)))
            },
        );
        assert!(matches!(
            result,
            Err(GraphWriteScriptExecutionError::Program(
                GraphWriteProgramError::CreationBudget {
                    statement: 1,
                    dimension: GraphInsertLimitDimension::Vertices,
                    limit: 1,
                    observed: 2,
                },
            ))
        ));
        assert_eq!(
            allocations.get(),
            1,
            "second creation must refuse before allocation"
        );
        assert_eq!(transaction.staged_effect_digest().unwrap(), before);
        assert!(transaction.vertex(&db, VId(10)).unwrap().is_none());
        assert!(transaction.vertex(&db, VId(99)).unwrap().is_some());
        assert!(db.vertices().unwrap().is_empty());
        transaction.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert!(db.vertex(VId(99)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn matched_read_only_script_publishes_no_marker_and_uses_no_creation_quota() {
    let ((), report) = run_async_under_lab(0x5c71_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![PERSON], vec![(P, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![PERSON], vec![(P, CanonicalScalar::Int(2))]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.unwrap();
        let before = db.frontier().unwrap();
        let definition = script(
            "MERGE (n:Person {p:1}); MERGE (n:Person {p:2});
            MATCH (a),(b) WHERE a.p=1 AND b.p=2 MERGE (a)-[:R]->(b);",
        );
        let (receipt, completion) = db
            .execute_graph_write_script_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &definition,
                &GqlParameters::new(),
                policy(0, 0, 0),
                |_| -> Result<ElementId, ()> { panic!("read-only script cannot allocate") },
            )
            .await
            .unwrap();
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(receipt.stats().proposed_effects(), 0);
        assert_eq!(
            receipt.steps()[2].merged_edge(),
            Some(GraphEdgeMergeOutcome::Matched(EId(10)))
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn no_input_relationship_does_not_skip_a_later_statement_or_consume_branch_quota() {
    let ((), report) = run_async_under_lab(0x5c71_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let definition = script(
            "MATCH (a),(b) MERGE (a)-[e:R]->(b) ON CREATE SET e.q=1;
            MERGE (n:Person {p:3});",
        );
        let (receipt, _) = db
            .execute_graph_write_script_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &definition,
                &GqlParameters::new(),
                policy(0, 1, 0),
                |request| {
                    assert_eq!(request.statement, 1);
                    assert!(matches!(
                        request.request,
                        GraphInsertRequest::Vertex { row: 0, vertex: 0 }
                    ));
                    Ok::<_, ()>(ElementId::Vertex(VId(3)))
                },
            )
            .await
            .unwrap();
        assert_eq!(
            receipt.steps()[0].merged_edge(),
            Some(GraphEdgeMergeOutcome::NoInput)
        );
        assert_eq!(receipt.stats().completed_statements, 2);
        assert_eq!(receipt.stats().created_vertices, 1);
        assert_eq!(receipt.stats().created_edges, 0);
        assert_eq!(receipt.stats().mutation_effects, 0);
        assert!(db.vertex(VId(3)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn allocator_failure_withholds_all_receipts_and_all_staged_creations() {
    let ((), report) = run_async_under_lab(0x5c71_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let calls = AtomicUsize::new(0);
        let result = db
            .execute_graph_write_script_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &script("MERGE (n:Person {p:1}); MERGE (n:Person {p:2});"),
                &GqlParameters::new(),
                policy(0, 2, 0),
                |request| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    if request.statement == 0 {
                        Ok(ElementId::Vertex(VId(10)))
                    } else {
                        Err("identity source refused")
                    }
                },
            )
            .await;
        assert!(matches!(
            result,
            Err(GraphWriteScriptExecutionError::Program(
                GraphWriteProgramError::VertexMerge { statement: 1, .. },
            ))
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
