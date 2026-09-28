//! Native atomic replacement and explicit non-cascading cleanup through scripts.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphDeleteError, GraphSymbol, GraphSymbolKind,
    GraphWriteProgramError, GraphWriteProgramPolicy, GraphWriteScriptExecutionError,
    PreparedGraphWriteScript,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x91; 32],
        DatabaseSecurityNamespaceId([0x92; 32]),
        [0x93; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn script(text: &str) -> PreparedGraphWriteScript {
    PreparedGraphWriteScript::prepare(text, R, symbols).unwrap()
}
fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(50_000, 50_000, 5_000_000, 5_000_000),
        3,
        3,
        0,
    )
}

#[test]
fn repeated_delete_create_records_replace_in_one_commit_and_survive_reopen() {
    let ((), report) = run_async_under_lab(0xd31e_3001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(7))]);
        db.write(&commit, seed).await.unwrap();
        let values = GqlParameters::new().with_int64("key", 7).unwrap();
        let batch = script("MATCH (n) WHERE n.p=$key DELETE n;CREATE (n {p:$key})")
            .bind_parameter_sets(&[values.clone(), values.clone(), values])
            .unwrap();
        let before = db.frontier().unwrap();
        let (receipt, _) = db
            .execute_graph_write_program_returning_autocommit_governed(
                &txcx,
                &query,
                &commit,
                batch.program(),
                policy(),
                |request| {
                    let location = batch.location(request.statement).unwrap();
                    assert_eq!(location.statement, 1);
                    Ok::<_, ()>(ElementId::Vertex(VId(10 + location.argument_set as u128)))
                },
            )
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(
            (
                receipt.stats().mutation_effects,
                receipt.stats().created_vertices
            ),
            (3, 3)
        );
        assert_eq!(receipt.steps()[0].deleted_vertices(), Some(&[VId(1)][..]));
        assert_eq!(receipt.steps()[2].deleted_vertices(), Some(&[VId(10)][..]));
        assert_eq!(receipt.steps()[4].deleted_vertices(), Some(&[VId(11)][..]));
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert!(db.vertex(VId(12)).unwrap().is_some());
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(
            db.vertex(VId(12)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(7))]
        );
        assert!(db.vertex_at(VId(1), before).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_plain_delete_refuses_attached_vertex_but_explicit_detach_succeeds() {
    let ((), report) = run_async_under_lab(0xd31e_3002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(2))]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.unwrap();
        let before = db.frontier().unwrap();
        let result = db
            .execute_graph_write_script_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &script("CREATE (x {p:9});MATCH (n) WHERE n.p=1 DELETE n"),
                &GqlParameters::new(),
                policy(),
                |_| Ok::<_, ()>(ElementId::Vertex(VId(9))),
            )
            .await;
        assert!(matches!(
            result,
            Err(GraphWriteScriptExecutionError::Program(
                GraphWriteProgramError::Delete {
                    statement: 1,
                    source: GqlQueryError::Source(GraphDeleteError::IncidentRelationships)
                }
            ))
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertex(VId(9)).unwrap().is_none());
        assert!(db.vertex(VId(1)).unwrap().is_some());
        assert!(db.edge(EId(10)).unwrap().is_some());
        let (receipt, _) = db
            .execute_graph_write_script_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &script("CREATE (x {p:9});MATCH (n) WHERE n.p=1 DETACH DELETE n"),
                &GqlParameters::new(),
                policy(),
                |_| Ok::<_, ()>(ElementId::Vertex(VId(19))),
            )
            .await
            .unwrap();
        assert_eq!(receipt.stats().mutation_effects, 1);
        assert!(db.vertex(VId(1)).unwrap().is_none());
        assert!(db.vertex(VId(2)).unwrap().is_some());
        assert!(db.vertex(VId(19)).unwrap().is_some());
        assert!(db.edge(EId(10)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
