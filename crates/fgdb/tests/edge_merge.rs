//! Directed relationship MERGE over the canonical transaction overlay.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GqlScalarParameter, GraphEdgeMergeError,
    GraphEdgeMergeOutcome, GraphEdgeMergePolicy, GraphEdgeMergeRequest, GraphSymbol,
    GraphSymbolKind, PreparedGraphEdgeMerge, PreparedGraphText,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion, PurposeContexts, VId,
};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const WEIGHT: PropertyKeyId = PropertyKeyId(2);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb1; 32],
        DatabaseSecurityNamespaceId([0xb2; 32]),
        [0xb3; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn definition(source: i64, destination: Option<i64>) -> PreparedGraphEdgeMerge {
    let where_clause = destination.map_or_else(
        || format!("a.p = {source}"),
        |destination| format!("a.p = {source} AND b.p = {destination}"),
    );
    let text = format!("MATCH (a),(b) WHERE {where_clause} RETURN ALL a,b");
    let selection = PreparedGraphText::prepare(&text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    PreparedGraphEdgeMerge::prepare(
        selection,
        R,
        0,
        1,
        vec![(
            WEIGHT,
            GqlScalarParameter::new(CanonicalScalar::Int(7)).unwrap(),
        )],
    )
    .unwrap()
}
fn policy() -> GraphEdgeMergePolicy {
    GraphEdgeMergePolicy::new(GqlQueryPolicy::new(50_000, 50_000, 5_000_000, 5_000_000))
}
async fn seed(db: &mut Database<MemVfs>, cx: &fgdb_types::CommitCx) {
    let mut batch = WriteBatch::new(R);
    for id in 1..=4_u128 {
        batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64))]);
    }
    // The MERGE map {WEIGHT: 7} is a match predicate (fgdb-at06q): edge 10
    // matches it, and its sibling 13 on the same pair is filtered out.
    batch.add_edge(EId(10), VId(1), VId(2), weight(7));
    batch.add_edge(EId(13), VId(1), VId(2), weight(3));
    db.write(cx, batch).await.unwrap();
}
fn weight(value: i64) -> Vec<(PropertyKeyId, CanonicalScalar)> {
    vec![(WEIGHT, CanonicalScalar::Int(value))]
}

#[test]
fn one_existing_relationship_matches_without_allocation_or_write() {
    let ((), report) = run_async_under_lab(0xed9e_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let before = db.frontier().unwrap();
        let allocations = Cell::new(0);
        let mut txn = db.begin(&txcx).unwrap();
        let (stats, outcome) = txn
            .execute_graph_edge_merge_governed(
                &mut db,
                &query,
                &definition(1, Some(2)),
                policy(),
                |_| {
                    allocations.set(allocations.get() + 1);
                    Ok::<_, ()>(ElementId::Edge(EId(99)))
                },
            )
            .unwrap();
        assert_eq!(outcome, GraphEdgeMergeOutcome::Matched(EId(10)));
        assert_eq!(stats.created_edges, 0);
        // The pair probe inspects both parallel edges (10 and 13); only edge
        // 10 satisfies the map.
        assert_eq!(stats.overlay_edges, 2);
        assert_eq!(allocations.get(), 0);
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn parallel_relationships_and_multiple_endpoint_pairs_refuse_before_allocation() {
    let ((), report) = run_async_under_lab(0xed9e_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut parallel = WriteBatch::new(R);
        parallel.add_edge(EId(11), VId(1), VId(2), weight(7));
        db.write(&commit, parallel).await.unwrap();

        let allocations = Cell::new(0);
        let mut txn = db.begin(&txcx).unwrap();
        let result = txn.execute_graph_edge_merge_governed(
            &mut db,
            &query,
            &definition(1, Some(2)),
            policy(),
            |_| {
                allocations.set(allocations.get() + 1);
                Ok::<_, ()>(ElementId::Edge(EId(99)))
            },
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(
                GraphEdgeMergeError::AmbiguousRelationships { observed: 2 }
            ))
        ));
        assert_eq!(allocations.get(), 0);
        txn.abort();

        let mut txn = db.begin(&txcx).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let result = txn.execute_graph_edge_merge_governed(
            &mut db,
            &query,
            &definition(1, None),
            policy(),
            |_| {
                allocations.set(allocations.get() + 1);
                Ok::<_, ()>(ElementId::Edge(EId(100)))
            },
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(
                GraphEdgeMergeError::AmbiguousEndpointPairs { .. }
            ))
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        assert_eq!(allocations.get(), 0);
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn zero_relationship_creates_one_edge_with_frozen_properties_and_survives_reopen() {
    let ((), report) = run_async_under_lab(0xed9e_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut db, &commit).await;
        let before = db.frontier().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let (stats, outcome) = txn
            .execute_graph_edge_merge_governed(
                &mut db,
                &query,
                &definition(2, Some(3)),
                policy(),
                |request| {
                    assert_eq!(request, GraphEdgeMergeRequest);
                    Ok::<_, ()>(ElementId::Edge(EId(20)))
                },
            )
            .unwrap();
        assert_eq!(stats.created_edges, 1);
        assert_eq!(outcome, GraphEdgeMergeOutcome::Created(EId(20)));
        let staged = txn.edge(&db, EId(20)).unwrap().unwrap();
        assert_eq!((staged.entry.src, staged.entry.dst), (VId(2), VId(3)));
        assert_eq!(staged.props, vec![(WEIGHT, CanonicalScalar::Int(7).into())]);
        assert!(db.edge(EId(20)).unwrap().is_none());
        txn.commit(&mut db, &commit).await.unwrap();
        assert!(db.frontier().unwrap().0 > before.0);
        assert_eq!(
            db.edge(EId(20)).unwrap().unwrap().props,
            vec![(WEIGHT, CanonicalScalar::Int(7).into())]
        );
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            db.edge(EId(20)).unwrap().unwrap().props,
            vec![(WEIGHT, CanonicalScalar::Int(7).into())]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn staged_edge_creation_and_deletion_are_visible_to_merge() {
    let ((), report) = run_async_under_lab(0xed9e_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;

        let mut txn = db.begin(&txcx).unwrap();
        let mut create = WriteBatch::new(R);
        create.add_edge(EId(30), VId(3), VId(4), weight(7));
        txn.write(&mut db, create).unwrap();
        let (stats, matched) = txn
            .execute_graph_edge_merge_governed(
                &mut db,
                &query,
                &definition(3, Some(4)),
                policy(),
                |_| -> Result<ElementId, ()> { panic!("staged edge must match") },
            )
            .unwrap();
        assert_eq!(matched, GraphEdgeMergeOutcome::Matched(EId(30)));
        assert_eq!(stats.created_edges, 0);

        // With edge 10 deleted, only the filtered sibling 13 remains: a miss.
        let mut remove = WriteBatch::new(R);
        remove.delete_edge(EId(10));
        txn.write(&mut db, remove).unwrap();
        let (_, created) = txn
            .execute_graph_edge_merge_governed(
                &mut db,
                &query,
                &definition(1, Some(2)),
                policy(),
                |_| Ok::<_, ()>(ElementId::Edge(EId(12))),
            )
            .unwrap();
        assert_eq!(created, GraphEdgeMergeOutcome::Created(EId(12)));
        assert!(txn.edge(&db, EId(10)).unwrap().is_none());
        assert!(txn.edge(&db, EId(12)).unwrap().is_some());
        txn.commit(&mut db, &commit).await.unwrap();
        assert!(db.edge(EId(10)).unwrap().is_none());
        assert!(db.edge(EId(12)).unwrap().is_some());
        assert!(db.edge(EId(30)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn wrong_identity_collision_and_concurrent_matching_insert_never_publish_duplicate() {
    let ((), report) = run_async_under_lab(0xed9e_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let definition = definition(2, Some(4));

        let mut wrong = db.begin(&txcx).unwrap();
        let before = wrong.staged_effect_digest().unwrap();
        assert!(matches!(
            wrong.execute_graph_edge_merge_governed(
                &mut db,
                &query,
                &definition,
                policy(),
                |_| Ok::<_, ()>(ElementId::Vertex(VId(99))),
            ),
            Err(GqlQueryError::Source(GraphEdgeMergeError::IdentityKind))
        ));
        assert_eq!(wrong.staged_effect_digest().unwrap(), before);
        wrong.abort();

        let mut collision = db.begin(&txcx).unwrap();
        assert!(matches!(
            collision.execute_graph_edge_merge_governed(
                &mut db,
                &query,
                &definition,
                policy(),
                |_| Ok::<_, ()>(ElementId::Edge(EId(10))),
            ),
            Err(GqlQueryError::Source(GraphEdgeMergeError::Source(_)))
        ));
        collision.abort();

        let mut first = db.begin(&txcx).unwrap();
        let (_, outcome) = first
            .execute_graph_edge_merge_governed(&mut db, &query, &definition, policy(), |_| {
                Ok::<_, ()>(ElementId::Edge(EId(100)))
            })
            .unwrap();
        assert_eq!(outcome, GraphEdgeMergeOutcome::Created(EId(100)));

        let mut competitor = db.begin(&txcx).unwrap();
        let mut edge = WriteBatch::new(R);
        edge.add_edge(EId(200), VId(2), VId(4), weight(7));
        competitor.write(&mut db, edge).unwrap();
        competitor.commit(&mut db, &commit).await.unwrap();

        assert!(
            first.commit(&mut db, &commit).await.is_err(),
            "the pair's incidence gap must reject a concurrent matching relationship insert"
        );
        assert!(db.edge(EId(100)).unwrap().is_none());
        assert!(db.edge(EId(200)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn indexed_pair_probe_preserves_parallel_properties_orientation_and_staged_rollback() {
    let ((), report) = run_async_under_lab(0xed9e_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=600_u128 {
            seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64))]);
            if id > 1 {
                seed.add_edge(EId(id), VId(id - 1), VId(id), weight(7));
            }
        }
        seed.add_edge(EId(1000), VId(100), VId(102), weight(7));
        seed.add_edge(EId(1001), VId(100), VId(102), weight(3));
        seed.add_edge(EId(1002), VId(100), VId(100), weight(7));
        db.write(&commit, seed).await.unwrap();

        // Even with complete vertex admission this leaves room for only the
        // pair's two records, not the other 600 relationships. Independent
        // indexed-root admission can further reduce the vertex side.
        let bounded = GraphEdgeMergePolicy::new(GqlQueryPolicy::new(602, 10, 5_000_000, 5_000_000));
        let mut txn = db.begin(&txcx).unwrap();
        txn.savepoint(&db, "before-pair-edits").unwrap();
        for merge in [
            definition(100, Some(102)),
            definition(102, Some(100)).with_undirected(),
        ] {
            let (stats, outcome) = txn
                .execute_graph_edge_merge_governed(
                    &mut db,
                    &query,
                    &merge,
                    bounded,
                    |_| -> Result<ElementId, ()> {
                        panic!("the matching parallel edge must not allocate")
                    },
                )
                .unwrap();
            assert_eq!(stats.overlay_edges, 2);
            assert_eq!(outcome, GraphEdgeMergeOutcome::Matched(EId(1000)));
        }
        let (stats, outcome) = txn
            .execute_graph_edge_merge_governed(
                &mut db,
                &query,
                &definition(100, Some(100)).with_undirected(),
                bounded,
                |_| -> Result<ElementId, ()> { panic!("the self-loop already exists") },
            )
            .unwrap();
        assert_eq!(stats.overlay_edges, 1, "a self-loop has one EId");
        assert_eq!(outcome, GraphEdgeMergeOutcome::Matched(EId(1002)));

        let mut edits = WriteBatch::new(R);
        edits.set_edge_property(EId(1000), WEIGHT, Some(CanonicalScalar::Int(3)));
        edits.set_edge_property(EId(1001), WEIGHT, Some(CanonicalScalar::Int(7)));
        txn.write(&mut db, edits).unwrap();
        let (_, outcome) = txn
            .execute_graph_edge_merge_governed(
                &mut db,
                &query,
                &definition(100, Some(102)),
                bounded,
                |_| -> Result<ElementId, ()> { panic!("the staged property now matches") },
            )
            .unwrap();
        assert_eq!(outcome, GraphEdgeMergeOutcome::Matched(EId(1001)));

        let mut replacement = WriteBatch::new(R);
        replacement.delete_edge(EId(1001));
        replacement.add_edge(EId(u128::MAX), VId(100), VId(102), weight(7));
        txn.write(&mut db, replacement).unwrap();
        let (stats, outcome) = txn
            .execute_graph_edge_merge_governed(
                &mut db,
                &query,
                &definition(100, Some(102)),
                bounded,
                |_| -> Result<ElementId, ()> { panic!("the staged replacement already exists") },
            )
            .unwrap();
        assert_eq!(stats.overlay_edges, 2);
        assert_eq!(outcome, GraphEdgeMergeOutcome::Matched(EId(u128::MAX)));
        txn.rollback_to_savepoint(&db, "before-pair-edits").unwrap();
        let (_, outcome) = txn
            .execute_graph_edge_merge_governed(
                &mut db,
                &query,
                &definition(100, Some(102)),
                bounded,
                |_| -> Result<ElementId, ()> { panic!("rollback restores the basis match") },
            )
            .unwrap();
        assert_eq!(outcome, GraphEdgeMergeOutcome::Matched(EId(1000)));
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert!(db.edge(EId(u128::MAX)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
