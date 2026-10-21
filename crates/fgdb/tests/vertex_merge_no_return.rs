//! Native no-RETURN cardinality, cumulative admission and rollback contracts.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{
    BoundNativeGraphWrite, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol,
    GraphSymbolKind, GraphVertexMergeError, GraphVertexMergeOutcome, GraphVertexMergePolicy,
    GraphVertexUpsertError, GraphVertexUpsertPolicy, GraphWriteStatement,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const IDS: [VId; 2] = [VId(1), VId((1_u128 << 100) | 1)];

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xa1; 32], DatabaseSecurityNamespaceId([0xa2; 32]), [0xa3; 32])
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "N") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn statement(text: &str) -> GraphWriteStatement {
    BoundNativeGraphWrite::bind_with_resolver(text, &GqlParameters::new(), R, symbols)
        .unwrap()
        .program()
        .statements()[0]
        .clone()
}

fn policy(rows: u64) -> GraphVertexMergePolicy {
    GraphVertexMergePolicy::new(GqlQueryPolicy::new(1_000, rows, 5_000_000, 2_000_000))
        .with_creation_limit(0)
}

fn no_allocate(_: GraphInsertRequest) -> Result<ElementId, ()> {
    panic!("existing targets never allocate")
}

fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for (id, q) in IDS.into_iter().zip([3, 7]) {
        batch.create_vertex(
            id,
            vec![LabelId(1)],
            vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(q))],
        );
    }
    batch
}

#[test]
fn native_plain_merge_returns_every_target_but_keeps_selected_row_limits() {
    let ((), report) = run_async_under_lab(0xc9e3_0011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let before = db.frontier().unwrap();
        let GraphWriteStatement::VertexMerge(merge) = statement("MERGE (n:N {p:1})") else {
            panic!("plain MERGE must retain its native program family")
        };
        let mut txn = db.begin(&txcx).unwrap();
        let unique = txn.execute_graph_vertex_merge_governed(
            &mut db, &cx, &merge, policy(2), no_allocate,
        );
        assert!(matches!(
            unique,
            Err(GqlQueryError::Source(GraphVertexMergeError::AmbiguousMatches { observed: 2 }))
        ));
        let (stats, outcomes) = txn
            .execute_graph_vertex_merges_governed(&mut db, &cx, &merge, policy(2), no_allocate)
            .unwrap();
        assert_eq!(stats.match_selection.result_rows, 2);
        assert_eq!(stats.created_vertices, 0);
        assert_eq!(outcomes, IDS.map(GraphVertexMergeOutcome::Matched));
        let limited = txn.execute_graph_vertex_merges_governed(
            &mut db, &cx, &merge, policy(1), no_allocate,
        );
        assert!(matches!(limited, Err(GqlQueryError::Rows(_))));
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_upsert_freezes_targets_and_charges_all_actions_before_acceptance() {
    let ((), report) = run_async_under_lab(0xc9e3_0012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let before = db.frontier().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(90), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let GraphWriteStatement::VertexUpsert(upsert) = statement(
            "MERGE (n:N {p:1}) ON MATCH SET n.p=2,n.q=n.q+1 SET n.q=n.q*2",
        ) else {
            panic!("ON/SET must bind a native upsert")
        };
        let limited = txn.execute_graph_vertex_upserts_governed(
            &mut db, &cx, &upsert, GraphVertexUpsertPolicy::new(policy(2), 5), no_allocate,
        );
        assert!(matches!(
            limited,
            Err(GqlQueryError::Source(GraphVertexUpsertError::ActionLimit {
                limit: 5, observed: 6,
            }))
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        let (stats, outcomes) = txn.execute_graph_vertex_upserts_governed(
            &mut db, &cx, &upsert, GraphVertexUpsertPolicy::new(policy(2), 6), no_allocate,
        ).unwrap();
        assert_eq!(stats.action_effects, 6);
        assert_eq!(outcomes, IDS.map(GraphVertexMergeOutcome::Matched));
        let accepted = txn.staged_effect_digest().unwrap();
        // The final target, not the first, divides by zero. Its failure must
        // undo the earlier target's accepted ON/SET clauses as well.
        let GraphWriteStatement::VertexUpsert(failing) = statement(
            "MERGE (n:N {p:2}) ON MATCH SET n.q=n.q-16 SET n.q=10/n.q",
        ) else {
            panic!("expected upsert")
        };
        let error = txn.execute_graph_vertex_upserts_governed(
            &mut db, &cx, &failing, GraphVertexUpsertPolicy::new(policy(2), 4), no_allocate,
        );
        assert!(matches!(
            error,
            Err(GqlQueryError::Source(GraphVertexUpsertError::Expression { .. }))
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), accepted);
        assert_eq!(db.frontier().unwrap(), before);
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        for (id, q) in IDS.into_iter().zip([8, 16]) {
            assert_eq!(
                db.vertex(id).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(2).into()),
                    (Q, CanonicalScalar::Int(q).into())
                ],
            );
        }
        assert!(db.vertex(VId(90)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_empty_merge_creates_once_with_original_allocator_coordinates() {
    let ((), report) = run_async_under_lab(0xc9e3_0013, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let GraphWriteStatement::VertexMerge(merge) = statement("MERGE (n:N {p:1})") else {
            panic!("expected merge")
        };
        let mut txn = db.begin(&txcx).unwrap();
        let (stats, outcomes) = txn.execute_graph_vertex_merges_governed(
            &mut db,
            &cx,
            &merge,
            policy(1).with_creation_limit(1),
            |request| -> Result<ElementId, ()> {
                assert!(matches!(request, GraphInsertRequest::Vertex { row: 0, vertex: 0 }));
                Ok(ElementId::Vertex(IDS[1]))
            },
        ).unwrap();
        assert_eq!(stats.created_vertices, 1);
        assert_eq!(outcomes, [GraphVertexMergeOutcome::Created(IDS[1])]);
        let (stats, outcomes) = txn.execute_graph_vertex_merges_governed(
            &mut db, &cx, &merge, policy(1), no_allocate,
        ).unwrap();
        assert_eq!(stats.created_vertices, 0);
        assert_eq!(outcomes, [GraphVertexMergeOutcome::Matched(IDS[1])]);
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
