//! Rowless all-target primitives share the native matcher, action evaluator,
//! transaction overlay and rollback guard with returning vertex MERGE.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    GraphVertexMergeOutcome, GraphVertexMergePolicy, GraphVertexUpsertError,
    GraphVertexUpsertPolicy, PreparedGraphVertexMergeText, PreparedGraphVertexUpsert,
    PreparedGraphVertexUpsertText,
};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const IDS: [VId; 3] = [VId(1), VId((1_u128 << 96) + 2), VId((1_u128 << 112) + 3)];

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x47; 32],
        DatabaseSecurityNamespaceId([0x48; 32]),
        [0x49; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn policy(actions: u64) -> GraphVertexUpsertPolicy {
    GraphVertexUpsertPolicy::new(
        GraphVertexMergePolicy::new(GqlQueryPolicy::new(10_000, 10, 5_000_000, 2_000_000))
            .with_creation_limit(1),
        actions,
    )
}

fn upsert(text: &str) -> PreparedGraphVertexUpsert {
    PreparedGraphVertexUpsertText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn no_allocation(_: GraphInsertRequest) -> Result<ElementId, ()> {
    panic!("matching existing vertices cannot allocate")
}

async fn database(commit: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(commit, keys()).await.unwrap();
    let mut batch = WriteBatch::new(R);
    for (vertex, p) in IDS.into_iter().zip([1, 2, 3]) {
        batch.create_vertex(
            vertex,
            vec![],
            vec![(P, CanonicalScalar::Int(p)), (Q, CanonicalScalar::Int(7))],
        );
    }
    db.write(commit, batch).await.unwrap();
    db
}

#[test]
fn plain_merge_returns_every_full_identity_without_creating() {
    let ((), report) = run_async_under_lab(0x7e90_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = database(&commit).await;
        let merge = PreparedGraphVertexMergeText::prepare("MERGE (n {q:7})", R, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let (stats, outcomes) = txn
            .execute_graph_vertex_merges_governed(
                &mut db,
                &cx,
                &merge,
                policy(0).merge,
                no_allocation,
            )
            .unwrap();
        assert_eq!(outcomes, IDS.map(GraphVertexMergeOutcome::Matched).to_vec());
        assert_eq!(stats.created_vertices, 0);
        assert_eq!(stats.match_selection.result_rows, 3);
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn rowless_actions_freeze_targets_and_admit_the_whole_action_count() {
    let ((), report) = run_async_under_lab(0x7e90_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = database(&commit).await;
        let input = upsert("MERGE (n {q:7}) ON MATCH SET n.p=n.p+10,n.q=8 SET n.p=n.p*2");
        let mut txn = db.begin(&txcx).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let error = txn
            .execute_graph_vertex_upserts_governed(&mut db, &cx, &input, policy(8), no_allocation)
            .unwrap_err();
        assert!(matches!(
            error,
            GqlQueryError::Source(GraphVertexUpsertError::ActionLimit {
                limit: 8,
                observed: 9,
            })
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        let (stats, outcomes) = txn
            .execute_graph_vertex_upserts_governed(&mut db, &cx, &input, policy(9), no_allocation)
            .unwrap();
        assert_eq!(outcomes, IDS.map(GraphVertexMergeOutcome::Matched).to_vec());
        assert_eq!(stats.action_effects, 9);
        assert_eq!(stats.merge.match_selection.result_rows, 3);
        for (vertex, p) in IDS.into_iter().zip([22, 24, 26]) {
            assert_eq!(
                txn.vertex(&db, vertex).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(p).into()),
                    (Q, CanonicalScalar::Int(8).into())
                ]
            );
        }
        txn.finish(&mut db, &commit).await.unwrap();
        for (vertex, p) in IDS.into_iter().zip([22, 24, 26]) {
            assert_eq!(
                db.vertex(vertex).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(p).into()),
                    (Q, CanonicalScalar::Int(8).into())
                ]
            );
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_late_target_failure_restores_all_targets_and_keeps_the_prior_prefix() {
    let ((), report) = run_async_under_lab(0x7e90_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = database(&commit).await;
        let before = db.frontier().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![], vec![(Q, CanonicalScalar::Int(99))]);
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let input = upsert("MERGE (n {q:7}) ON MATCH SET n.p=10/(n.p-2)");
        let error = txn.execute_graph_vertex_upserts_governed(
            &mut db,
            &cx,
            &input,
            policy(3),
            no_allocation,
        );
        assert!(error.is_err());
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        for (vertex, p) in IDS.into_iter().zip([1, 2, 3]) {
            assert_eq!(
                txn.vertex(&db, vertex).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(p).into()),
                    (Q, CanonicalScalar::Int(7).into())
                ]
            );
        }
        assert!(txn.vertex(&db, VId(99)).unwrap().is_some());
        assert_eq!(db.frontier().unwrap(), before);
        txn.finish(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(99)).unwrap().is_some());
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_matches_create_once_and_a_later_match_never_allocates_again() {
    let ((), report) = run_async_under_lab(0x7e90_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let input =
            upsert("MERGE (n {q:7}) ON CREATE SET n.p=4 ON MATCH SET n.p=n.p+1 SET n.p=n.p*2");
        let created = VId((1_u128 << 120) + 5);
        let mut txn = db.begin(&txcx).unwrap();
        let mut calls = 0;
        let (first, outcomes) = txn
            .execute_graph_vertex_upserts_governed(
                &mut db,
                &cx,
                &input,
                policy(2),
                |request| -> Result<ElementId, ()> {
                    calls += 1;
                    assert_eq!(request, GraphInsertRequest::Vertex { row: 0, vertex: 0 });
                    Ok(ElementId::Vertex(created))
                },
            )
            .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(outcomes, vec![GraphVertexMergeOutcome::Created(created)]);
        assert_eq!(first.merge.match_selection.result_rows, 0);
        assert_eq!(first.merge.created_vertices, 1);
        assert_eq!(first.action_effects, 2);
        let (second, outcomes) = txn
            .execute_graph_vertex_upserts_governed(&mut db, &cx, &input, policy(2), no_allocation)
            .unwrap();
        assert_eq!(outcomes, vec![GraphVertexMergeOutcome::Matched(created)]);
        assert_eq!(second.merge.created_vertices, 0);
        assert_eq!(second.merge.match_selection.result_rows, 1);
        assert_eq!(
            txn.vertex(&db, created).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(18).into()),
                (Q, CanonicalScalar::Int(7).into())
            ]
        );
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
