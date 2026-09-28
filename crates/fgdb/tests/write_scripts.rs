//! Complete native scripts use the production overlay, commit and reopen paths.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphEdgeMergeOutcome, GraphSymbol, GraphSymbolKind,
    GraphWriteProgramPolicy, PreparedGraphWriteScript,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn script() -> PreparedGraphWriteScript {
    PreparedGraphWriteScript::prepare(
        "MERGE (n:Person {p:$left});\n\
         MERGE (n:Person {p:$right});\n\
         MATCH (a:Person),(b:Person) WHERE a.p=$left AND b.p=$right\n\
         MERGE (a)-[e:R]->(b) ON CREATE SET e.q=$created ON MATCH SET e.q=$matched;",
        R,
        symbols,
    )
    .unwrap()
}
fn arguments() -> GqlParameters {
    GqlParameters::new()
        .with_int64("left", 1)
        .unwrap()
        .with_int64("right", 2)
        .unwrap()
        .with_int64("created", 10)
        .unwrap()
        .with_int64("matched", 20)
        .unwrap()
}
fn policy(edges: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(50_000, 50_000, 5_000_000, 5_000_000),
        20,
        2,
        edges,
    )
}

#[test]
fn native_script_creates_then_updates_the_same_graph_and_survives_reopen() {
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
        let script = script();
        let program = script.bind_parameters(&arguments()).unwrap();
        let (receipt, completion) = db
            .execute_graph_write_program_returning_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &program,
                policy(1),
                |request| {
                    Ok::<_, ()>(match request.statement {
                        0 => ElementId::Vertex(VId(1)),
                        1 => ElementId::Vertex(VId(2)),
                        2 => ElementId::Edge(EId(10)),
                        _ => panic!("unexpected script statement"),
                    })
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
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
            vec![(Q, CanonicalScalar::Int(10))]
        );

        let (receipt, completion) = db
            .execute_graph_write_program_returning_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &script.bind_parameters(&arguments()).unwrap(),
                GraphWriteProgramPolicy {
                    max_created_vertices: 0,
                    ..policy(0)
                },
                |_| -> Result<ElementId, ()> { panic!("repeated ingestion must not allocate") },
            )
            .await
            .unwrap();
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
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
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(1))]
        );
        let edge = db.edge(EId(10)).unwrap().unwrap();
        assert_eq!((edge.entry.src, edge.entry.dst), (VId(1), VId(2)));
        assert_eq!(edge.props, vec![(Q, CanonicalScalar::Int(20))]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_script_failure_rolls_back_all_script_steps_but_preserves_outer_work() {
    let ((), report) = run_async_under_lab(0x5c71_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let mut allocations = 0;
        let program = script().bind_parameters(&arguments()).unwrap();
        let result = txn.execute_graph_write_program_returning_governed(
            &mut db,
            &query,
            &program,
            policy(0),
            |request| {
                allocations += 1;
                assert!(
                    request.statement < 2,
                    "edge quota must refuse before allocation"
                );
                Ok::<_, ()>(ElementId::Vertex(VId(request.statement as u128 + 1)))
            },
        );
        assert!(result.is_err());
        assert_eq!(
            allocations, 2,
            "allocated identities are not reclaimed by rollback"
        );
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        assert!(txn.vertex(&db, VId(1)).unwrap().is_none());
        assert!(txn.vertex(&db, VId(2)).unwrap().is_none());
        assert!(txn.vertex(&db, VId(99)).unwrap().is_some());
        txn.finish(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(99)).unwrap().is_some());
        assert!(db.vertex(VId(1)).unwrap().is_none());
        assert!(db.edge(EId(10)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn record(left: i64, right: i64, weight: i64) -> GqlParameters {
    GqlParameters::new()
        .with_int64("left", left)
        .unwrap()
        .with_int64("right", right)
        .unwrap()
        .with_int64("created", weight)
        .unwrap()
        .with_int64("matched", weight)
        .unwrap()
}

#[test]
fn parameter_batch_ingests_shared_vertices_and_edges_with_one_durable_completion() {
    let ((), report) = run_async_under_lab(0x5c71_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let batch = script()
            .bind_parameter_sets(&[record(1, 2, 10), record(2, 3, 20), record(1, 2, 30)])
            .unwrap();
        let allowance = GraphWriteProgramPolicy {
            max_created_vertices: 3,
            ..policy(2)
        };
        let mut allocations = 0;
        let (receipt, completion) = db
            .execute_graph_write_program_returning_autocommit_governed(
                &txcx,
                &query,
                &commit,
                batch.program(),
                allowance,
                |request| {
                    allocations += 1;
                    let location = batch.location(request.statement).unwrap();
                    Ok::<_, ()>(match (location.argument_set, location.statement) {
                        (0, 0) => ElementId::Vertex(VId(1)),
                        (0, 1) => ElementId::Vertex(VId(2)),
                        (1, 1) => ElementId::Vertex(VId(3)),
                        (0, 2) => ElementId::Edge(EId(10)),
                        (1, 2) => ElementId::Edge(EId(11)),
                        _ => panic!("an existing vertex or edge requested a second identity"),
                    })
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(
            db.frontier().unwrap().0,
            before.0 + 1,
            "the batch publishes exactly one commit"
        );
        assert_eq!(allocations, 5);
        assert_eq!(receipt.stats().completed_statements, 9);
        assert_eq!(
            (
                receipt.stats().created_vertices,
                receipt.stats().created_edges
            ),
            (3, 2)
        );
        assert_eq!(receipt.stats().mutation_effects, 3);
        let third = &receipt.steps()[batch.statement_range(2).unwrap()];
        assert_eq!(
            third[2].merged_edge(),
            Some(GraphEdgeMergeOutcome::Matched(EId(10)))
        );
        assert_eq!(
            db.edge(EId(10)).unwrap().unwrap().props,
            vec![(Q, CanonicalScalar::Int(30))]
        );
        assert_eq!(
            db.edge(EId(11)).unwrap().unwrap().props,
            vec![(Q, CanonicalScalar::Int(20))]
        );
        assert!(db.vertex(VId(3)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn batch_creation_quota_is_shared_across_records_and_late_refusal_rolls_back_every_record() {
    let ((), report) = run_async_under_lab(0x5c71_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let batch = script()
            .bind_parameter_sets(&[record(1, 2, 10), record(2, 3, 20)])
            .unwrap();
        let mut allocations = 0;
        let result = txn.execute_graph_write_program_returning_governed(
            &mut db,
            &query,
            batch.program(),
            GraphWriteProgramPolicy {
                max_created_vertices: 3,
                ..policy(1)
            },
            |request| {
                allocations += 1;
                Ok::<_, ()>(match request.statement {
                    0 => ElementId::Vertex(VId(1)),
                    1 => ElementId::Vertex(VId(2)),
                    2 => ElementId::Edge(EId(10)),
                    4 => ElementId::Vertex(VId(3)),
                    _ => panic!("second relationship must refuse before allocating"),
                })
            },
        );
        let fgdb_gql::GraphWriteProgramError::CreationBudget {
            statement,
            limit,
            observed,
            ..
        } = result.unwrap_err()
        else {
            panic!("expected cumulative creation refusal")
        };
        assert_eq!((statement, limit, observed), (5, 1, 2));
        let location = batch.location(statement).unwrap();
        assert_eq!((location.argument_set, location.statement), (1, 2));
        assert_eq!(allocations, 4);
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        for id in 1..=3 {
            assert!(txn.vertex(&db, VId(id)).unwrap().is_none());
        }
        assert!(txn.edge(&db, EId(10)).unwrap().is_none());
        txn.finish(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(99)).unwrap().is_some());
        assert!(db.vertex(VId(1)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
