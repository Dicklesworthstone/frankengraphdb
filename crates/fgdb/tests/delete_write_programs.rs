//! Mixed programs preserve the distinction between DELETE and DETACH DELETE.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphDeleteError, GraphMutationProgramDimension,
    GraphMutationProgramError, GraphSymbol, GraphSymbolKind, GraphWriteProgramError,
    GraphWriteProgramPolicy, GraphWriteStatement, PreparedGraphDeleteText, PreparedGraphInsertText,
    PreparedGraphMutationText, PreparedGraphWriteProgram,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x81; 32],
        DatabaseSecurityNamespaceId([0x82; 32]),
        [0x83; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn delete(text: &str) -> GraphWriteStatement {
    PreparedGraphDeleteText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .into()
}
fn insert(text: &str) -> GraphWriteStatement {
    PreparedGraphInsertText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .into()
}
fn mutation(text: &str) -> GraphWriteStatement {
    PreparedGraphMutationText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .into()
}
fn policy(effects: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(50_000, 50_000, 5_000_000, 5_000_000),
        effects,
        10,
        10,
    )
}

#[test]
fn delete_sees_earlier_creation_and_later_creation_sees_the_deletion() {
    let ((), report) = run_async_under_lab(0xd31e_2001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let program = PreparedGraphWriteProgram::prepare(vec![
            insert("CREATE (n {p:1})"),
            delete("MATCH (n) WHERE n.p=1 DELETE n"),
            insert("CREATE (n {p:2})"),
            delete("MATCH (n) WHERE n.p=1 DELETE n"),
        ])
        .unwrap();
        let before = db.frontier().unwrap();
        let (receipt, _) = db
            .execute_graph_write_program_returning_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &program,
                policy(1),
                |request| {
                    assert!(request.statement == 0 || request.statement == 2);
                    Ok::<_, ()>(ElementId::Vertex(VId(request.statement as u128 + 1)))
                },
            )
            .await
            .unwrap();
        assert_eq!(receipt.steps()[1].deleted_vertices(), Some(&[VId(1)][..]));
        assert_eq!(receipt.steps()[3].deleted_vertices(), Some(&[][..]));
        assert_eq!(
            (
                receipt.stats().created_vertices,
                receipt.stats().mutation_effects
            ),
            (2, 1)
        );
        assert_eq!(receipt.stats().target_vertex_visits, 1);
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert!(db.vertex(VId(1)).unwrap().is_none());
        assert_eq!(
            db.vertex(VId(3)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(2))]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn attached_target_refuses_all_steps_preserves_outer_prefix_and_retains_witnesses() {
    let ((), report) = run_async_under_lab(0xd31e_2002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=3_u128 {
            seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64))]);
        }
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let program = PreparedGraphWriteProgram::prepare(vec![
            mutation("MATCH (n) WHERE n.p=3 SET n.p=30"),
            delete("MATCH (n) WHERE n.p=1 DELETE n"),
            insert("CREATE (n)"),
        ])
        .unwrap();
        let result = txn.execute_graph_write_program_returning_governed(
            &mut db,
            &query,
            &program,
            policy(10),
            |_| -> Result<ElementId, ()> { panic!("no suffix allocation after refusal") },
        );
        assert!(matches!(
            result,
            Err(GraphWriteProgramError::Delete {
                statement: 1,
                source: GqlQueryError::Source(GraphDeleteError::IncidentRelationships)
            })
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        assert_eq!(
            txn.vertex(&db, VId(3)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(3))]
        );
        assert!(txn.edge(&db, EId(10)).unwrap().is_some());
        assert!(txn.vertex(&db, VId(99)).unwrap().is_some());
        let mut winner = WriteBatch::new(R);
        winner.delete_edge(EId(10));
        db.write(&commit, winner).await.unwrap();
        assert!(
            txn.finish(&mut db, &commit).await.is_err(),
            "failed deletion's topology observations remain witnesses"
        );
        assert!(db.vertex(VId(1)).unwrap().is_some());
        assert!(db.vertex(VId(99)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prior_detach_removes_incidence_and_plain_delete_uses_the_updated_overlay() {
    let ((), report) = run_async_under_lab(0xd31e_2003, |root| async move {
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
        let program = PreparedGraphWriteProgram::prepare(vec![
            mutation("MATCH (n) WHERE n.p=1 DETACH DELETE n"),
            delete("MATCH (n) WHERE n.p=2 DELETE n"),
        ])
        .unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let stats = txn
            .execute_graph_write_program_governed(
                &mut db,
                &query,
                &program,
                policy(2),
                |_| -> Result<ElementId, ()> { panic!("deletion cannot allocate") },
            )
            .unwrap();
        assert_eq!((stats.mutation_effects, stats.target_vertex_visits), (2, 2));
        txn.finish(&mut db, &commit).await.unwrap();
        assert!(db.vertices().unwrap().is_empty());
        assert!(db.edges().unwrap().is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn updates_and_deletes_share_the_effect_quota_and_late_refusal_rolls_back_updates() {
    let ((), report) = run_async_under_lab(0xd31e_2004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(1))]);
        db.write(&commit, seed).await.unwrap();
        let before = db.frontier().unwrap();
        let program = PreparedGraphWriteProgram::prepare(vec![
            mutation("MATCH (n) SET n.p=2"),
            delete("MATCH (n) DELETE n"),
        ])
        .unwrap();
        let result = db
            .execute_graph_write_program_returning_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &program,
                policy(1),
                |_| -> Result<ElementId, ()> { panic!("update/delete cannot allocate") },
            )
            .await;
        assert!(matches!(
            result,
            Err(GraphWriteProgramError::Program(
                GraphMutationProgramError::Budget {
                    statement: 1,
                    dimension: GraphMutationProgramDimension::Effects,
                    limit: 1,
                    observed: 2,
                }
            ))
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(1))]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
