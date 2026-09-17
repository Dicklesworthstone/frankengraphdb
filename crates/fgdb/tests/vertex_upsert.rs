//! Branch-specific unique vertex MERGE actions over the real transaction overlay.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::{GraphInsertVertex, PreparedGraphInsert};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GqlScalarParameter, GraphMutationValue,
    GraphSymbol, GraphSymbolKind, GraphVertexMergeOutcome, GraphVertexMergePolicy,
    GraphVertexUpsertAction, GraphVertexUpsertBranch, GraphVertexUpsertError,
    GraphVertexUpsertPolicy, PreparedGraphText, PreparedGraphVertexMerge,
    PreparedGraphVertexUpsert,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const MARKED: LabelId = LabelId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x31; 32],
        DatabaseSecurityNamespaceId([0x32; 32]),
        [0x33; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn scalar(value: i64) -> GqlScalarParameter {
    GqlScalarParameter::new(CanonicalScalar::Int(value)).unwrap()
}
fn upsert(value: i64) -> PreparedGraphVertexUpsert {
    let text = format!("MATCH (n) WHERE n.p={value} RETURN ALL n");
    let selection = PreparedGraphText::prepare(&text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let create = PreparedGraphInsert::prepare_standalone(
        R,
        vec![GraphInsertVertex {
            labels: vec![],
            properties: vec![(P, GraphMutationValue::Literal(scalar(value)))],
        }],
        vec![],
    )
    .unwrap();
    let merge = PreparedGraphVertexMerge::prepare(selection, R, 0, create).unwrap();
    PreparedGraphVertexUpsert::prepare(
        merge,
        vec![
            GraphVertexUpsertAction::SetProperty {
                key: Q,
                value: scalar(100),
            },
            GraphVertexUpsertAction::SetLabel {
                label: MARKED,
                present: true,
            },
        ],
        vec![
            GraphVertexUpsertAction::SetProperty {
                key: Q,
                value: scalar(200),
            },
            GraphVertexUpsertAction::SetLabel {
                label: MARKED,
                present: true,
            },
        ],
    )
    .unwrap()
}
fn policy(max_actions: u64) -> GraphVertexUpsertPolicy {
    GraphVertexUpsertPolicy::new(
        GraphVertexMergePolicy::new(GqlQueryPolicy::new(10_000, 10_000, 2_000_000, 2_000_000)),
        max_actions,
    )
}

#[test]
fn on_match_updates_existing_vertex_without_allocating() {
    let ((), report) = run_async_under_lab(0x6e29_3001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(7))]);
        db.write(&commit, seed).await.unwrap();

        let mut txn = db.begin(&txcx).unwrap();
        let (stats, outcome) = txn
            .execute_graph_vertex_upsert_governed(
                &mut db,
                &query,
                &upsert(7),
                policy(2),
                |_| -> Result<ElementId, ()> { panic!("ON MATCH must not allocate") },
            )
            .unwrap();
        assert_eq!(outcome, GraphVertexMergeOutcome::Matched(VId(1)));
        assert_eq!(stats.branch, GraphVertexUpsertBranch::Match);
        assert_eq!(stats.action_effects, 2);
        let staged = txn.vertex(&db, VId(1)).unwrap().unwrap();
        assert_eq!(staged.labels, vec![MARKED]);
        assert_eq!(
            staged.props,
            vec![(P, CanonicalScalar::Int(7)), (Q, CanonicalScalar::Int(100))]
        );
        txn.commit(&mut db, &commit).await.unwrap();
        let row = db.vertex(VId(1)).unwrap().unwrap();
        assert_eq!(row.labels, vec![MARKED]);
        assert_eq!(
            row.props,
            vec![(P, CanonicalScalar::Int(7)), (Q, CanonicalScalar::Int(100))]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn on_create_decorates_new_vertex_atomically_and_survives_reopen() {
    let ((), report) = run_async_under_lab(0x6e29_3002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let (stats, outcome) = txn
            .execute_graph_vertex_upsert_governed(&mut db, &query, &upsert(9), policy(2), |_| {
                Ok::<_, ()>(ElementId::Vertex(VId(9)))
            })
            .unwrap();
        assert_eq!(outcome, GraphVertexMergeOutcome::Created(VId(9)));
        assert_eq!(stats.branch, GraphVertexUpsertBranch::Create);
        assert_eq!(stats.action_effects, 2);
        let staged = txn.vertex(&db, VId(9)).unwrap().unwrap();
        assert_eq!(staged.labels, vec![MARKED]);
        assert_eq!(
            staged.props,
            vec![(P, CanonicalScalar::Int(9)), (Q, CanonicalScalar::Int(200))]
        );
        assert!(db.vertex(VId(9)).unwrap().is_none());
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        let row = db.vertex(VId(9)).unwrap().unwrap();
        assert_eq!(row.labels, vec![MARKED]);
        assert_eq!(
            row.props,
            vec![(P, CanonicalScalar::Int(9)), (Q, CanonicalScalar::Int(200))]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn create_branch_action_limit_refusal_rolls_back_created_vertex_but_not_allocator() {
    let ((), report) = run_async_under_lab(0x6e29_3003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let result = txn.execute_graph_vertex_upsert_governed(
            &mut db,
            &query,
            &upsert(11),
            policy(1),
            |_| Ok::<_, ()>(ElementId::Vertex(VId(11))),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphVertexUpsertError::ActionLimit {
                limit: 1,
                observed: 2
            }))
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        assert!(txn.vertex(&db, VId(11)).unwrap().is_none());
        assert!(db.vertex(VId(11)).unwrap().is_none());
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ambiguity_refuses_before_branch_actions_and_preserves_workspace() {
    let ((), report) = run_async_under_lab(0x6e29_3004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(7))]);
        seed.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(7))]);
        db.write(&commit, seed).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        assert!(
            txn.execute_graph_vertex_upsert_governed(
                &mut db,
                &query,
                &upsert(7),
                policy(2),
                |_| Ok::<_, ()>(ElementId::Vertex(VId(99))),
            )
            .is_err()
        );
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        assert!(
            txn.vertex(&db, VId(1))
                .unwrap()
                .unwrap()
                .props
                .iter()
                .all(|(key, _)| *key != Q)
        );
        assert!(
            txn.vertex(&db, VId(2))
                .unwrap()
                .unwrap()
                .props
                .iter()
                .all(|(key, _)| *key != Q)
        );
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
