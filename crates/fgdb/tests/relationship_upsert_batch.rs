//! Native bulk relationship upsert laws: real staging, not a second graph model.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphEdgeMergeError, GraphEdgeMergePolicy,
    GraphEdgeUpsertError, GraphEdgeUpsertPolicy, GraphEdgeUpsertQueryError, GraphSymbol,
    GraphSymbolKind, PreparedGraphEdgeUpsertQuery, PreparedGraphEdgeUpsertQueryText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
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
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn prepare(text: &str) -> PreparedGraphEdgeUpsertQuery {
    PreparedGraphEdgeUpsertQueryText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn policy(created: u64, actions: u64, rows: u64) -> GraphEdgeUpsertPolicy {
    GraphEdgeUpsertPolicy::new(
        GraphEdgeMergePolicy::new(GqlQueryPolicy::new(100_000, rows, 10_000_000, 2_000_000))
            .with_creation_limit(created),
        actions,
    )
}
fn vertices(count: u128) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for id in 1..=count {
        batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64))]);
    }
    batch
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn cells(rows: &[GraphValueRow]) -> Vec<Vec<GraphValue>> {
    rows.iter().map(|row| row.values().to_vec()).collect()
}

#[test]
fn multiple_pairs_keep_parallel_matches_and_one_atomic_creation() {
    let ((), report) = run_async_under_lab(0xe6e1_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = vertices(3);
        seed.add_edge(EId(10), VId(1), VId(2), vec![(Q, CanonicalScalar::Int(2))]);
        seed.add_edge(EId(11), VId(1), VId(2), vec![(Q, CanonicalScalar::Int(5))]);
        db.write(&commit, seed).await.unwrap();
        let before = db.frontier().unwrap();
        let query = prepare(
            "MATCH (a),(b) WHERE a.p=1 AND b.p>1 MERGE (a)-[r:R]->(b) \
             ON MATCH SET r.q=r.q+10 ON CREATE SET r.q=1 SET r.q=r.q+1 \
             RETURN r,r.q AS q ORDER BY r",
        );
        let mut transaction = db.begin(&txcx).unwrap();
        let (stats, rows) = transaction
            .execute_graph_edge_upserts_query_governed(
                &mut db,
                &cx,
                &query,
                policy(1, 6, 3),
                |request| -> Result<ElementId, ()> {
                    assert!(matches!(
                        request,
                        GraphInsertRequest::Edge { row: 1, edge: 0 }
                    ));
                    Ok(ElementId::Edge(EId(100)))
                },
            )
            .unwrap();
        assert_eq!(stats.match_selection.result_rows, 2);
        assert_eq!(stats.created_edges, 1);
        assert_eq!(stats.matched_occurrences, 2);
        assert_eq!(stats.action_effects, 6);
        assert_eq!(
            cells(&rows.value),
            vec![
                vec![GraphValue::Edge(EId(10)), int(13)],
                vec![GraphValue::Edge(EId(11)), int(16)],
                vec![GraphValue::Edge(EId(100)), int(2)],
            ]
        );
        assert!(rows.evaluator.work_units > stats.evaluator.work_units);
        assert_eq!(
            rows.rows.snapshot_records,
            stats.match_selection.snapshot_records + stats.overlay_edges
        );
        assert_eq!(db.edges().unwrap().len(), 2);
        assert_eq!(db.frontier().unwrap(), before);
        transaction.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(db.edges().unwrap().len(), 3);
        assert_eq!(
            db.edge(EId(100)).unwrap().unwrap().props,
            vec![(Q, CanonicalScalar::Int(2).into())]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn duplicate_input_occurrences_observe_staging_and_aggregate_the_whole_bag() {
    let ((), report) = run_async_under_lab(0xe6e1_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, vertices(3)).await.unwrap();
        let query = prepare(
            "MATCH (a),(b),(w) WHERE a.p=1 AND b.p=2 AND w.p>1 \
             MERGE (a)-[r:R]->(b) ON CREATE SET r.q=7 ON MATCH SET r.q=r.q+1 \
             RETURN count(r) AS occurrences,collect(r.q) AS values",
        );
        let calls = AtomicU64::new(0);
        let mut transaction = db.begin(&txcx).unwrap();
        let (stats, rows) = transaction
            .execute_graph_edge_upserts_query_governed(
                &mut db,
                &cx,
                &query,
                policy(1, 2, 1),
                |_| -> Result<ElementId, ()> {
                    assert_eq!(calls.fetch_add(1, Ordering::Relaxed), 0);
                    Ok(ElementId::Edge(EId(100)))
                },
            )
            .unwrap();
        assert_eq!(stats.match_selection.result_rows, 2);
        assert_eq!(
            (
                stats.created_edges,
                stats.matched_occurrences,
                stats.action_effects
            ),
            (1, 1, 2)
        );
        assert_eq!(
            cells(&rows.value),
            vec![vec![
                int(2),
                GraphValue::List(vec![int(8), int(8)].into_boxed_slice()),
            ]]
        );
        assert_eq!(rows.rows.result_rows, 1);
        assert_eq!(transaction.edges(&db).unwrap().len(), 1);
        transaction.abort();
        assert!(db.edges().unwrap().is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn changed_merge_key_is_rechecked_in_the_next_occurrence() {
    let ((), report) = run_async_under_lab(0xe6e1_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = vertices(3);
        seed.add_edge(EId(10), VId(1), VId(2), vec![(P, CanonicalScalar::Int(7))]);
        db.write(&commit, seed).await.unwrap();
        let query = prepare(
            "MATCH (a),(b),(w) WHERE a.p=1 AND b.p=2 AND w.p>1 \
             MERGE (a)-[r:R {p:7}]->(b) ON MATCH SET r.p=8 ON CREATE SET r.p=42 \
             RETURN r,r.p AS p ORDER BY r",
        );
        let mut transaction = db.begin(&txcx).unwrap();
        let (stats, rows) = transaction
            .execute_graph_edge_upserts_query_governed(
                &mut db,
                &cx,
                &query,
                policy(1, 2, 2),
                |request| -> Result<ElementId, ()> {
                    assert!(matches!(
                        request,
                        GraphInsertRequest::Edge { row: 1, edge: 0 }
                    ));
                    Ok(ElementId::Edge(EId(100)))
                },
            )
            .unwrap();
        assert_eq!((stats.created_edges, stats.matched_occurrences), (1, 1));
        assert_eq!(
            cells(&rows.value),
            vec![
                vec![GraphValue::Edge(EId(10)), int(8)],
                vec![GraphValue::Edge(EId(100)), int(42)],
            ]
        );
        transaction.abort();
        assert_eq!(
            db.edge(EId(10)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(7).into())]
        );
        assert!(db.edge(EId(100)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn opposite_undirected_occurrences_share_the_earlier_created_relationship() {
    let ((), report) = run_async_under_lab(0xe6e1_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, vertices(2)).await.unwrap();
        let query = prepare(
            "MATCH (a),(b) WHERE a.p<>b.p MERGE (a)-[r:R]-(b) \
             ON CREATE SET r.q=7 ON MATCH SET r.q=r.q+1 RETURN r,r.q AS q",
        );
        let calls = AtomicU64::new(0);
        let mut transaction = db.begin(&txcx).unwrap();
        let (stats, rows) = transaction
            .execute_graph_edge_upserts_query_governed(
                &mut db,
                &cx,
                &query,
                policy(1, 2, 2),
                |_| -> Result<ElementId, ()> {
                    assert_eq!(calls.fetch_add(1, Ordering::Relaxed), 0);
                    Ok(ElementId::Edge(EId(100)))
                },
            )
            .unwrap();
        assert_eq!((stats.created_edges, stats.matched_occurrences), (1, 1));
        assert_eq!(
            cells(&rows.value),
            vec![
                vec![GraphValue::Edge(EId(100)), int(8)],
                vec![GraphValue::Edge(EId(100)), int(8)],
            ]
        );
        transaction.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.edges().unwrap().len(), 1);
        let edge = db.edge(EId(100)).unwrap().unwrap();
        assert_eq!((edge.entry.src, edge.entry.dst), (VId(1), VId(2)));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_return_failure_even_with_zero_output_budget_restores_the_entire_prefix() {
    let ((), report) = run_async_under_lab(0xe6e1_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let mut transaction = db.begin(&txcx).unwrap();
        transaction.write(&mut db, vertices(3)).unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        let next = AtomicU64::new(100);
        let mut allocate = |_| -> Result<ElementId, ()> {
            Ok(ElementId::Edge(EId(u128::from(
                next.fetch_add(1, Ordering::Relaxed),
            ))))
        };
        let failing = prepare(
            "MATCH (a),(b) WHERE a.p=1 AND b.p>1 MERGE (a)-[r:R]->(b) \
             ON CREATE SET r.q=0 RETURN 10/r.q AS bad LIMIT 0",
        );
        let result = transaction.execute_graph_edge_upserts_query_governed(
            &mut db,
            &cx,
            &failing,
            policy(2, 2, 0),
            &mut allocate,
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphEdgeUpsertQueryError::Returning(
                _
            )))
        ));
        assert_eq!(next.load(Ordering::Relaxed), 102);
        assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
        assert!(transaction.edges(&db).unwrap().is_empty());
        assert_eq!(transaction.vertices(&db).unwrap().len(), 3);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(db.frontier().unwrap(), before);
        let successful = prepare(
            "MATCH (a),(b) WHERE a.p=1 AND b.p>1 MERGE (a)-[r:R]->(b) \
             ON CREATE SET r.q=1 RETURN r",
        );
        let (_, rows) = transaction
            .execute_graph_edge_upserts_query_governed(
                &mut db,
                &cx,
                &successful,
                policy(2, 2, 2),
                &mut allocate,
            )
            .unwrap();
        assert_eq!(rows.value.len(), 2);
        transaction.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert!(db.edge(EId(100)).unwrap().is_none());
        assert!(db.edge(EId(101)).unwrap().is_none());
        assert!(db.edge(EId(102)).unwrap().is_some());
        assert!(db.edge(EId(103)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn creation_and_action_allowances_are_statement_wide_not_per_occurrence() {
    let ((), report) = run_async_under_lab(0xe6e1_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let query = prepare(
            "MATCH (a),(b) WHERE a.p=1 AND b.p>1 MERGE (a)-[r:R]->(b) \
             ON CREATE SET r.q=1 RETURN count(r) AS count",
        );
        for creation_limit in [true, false] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut transaction = db.begin(&txcx).unwrap();
            transaction.write(&mut db, vertices(3)).unwrap();
            let digest = transaction.staged_effect_digest().unwrap();
            let next = AtomicU64::new(100);
            let allowance = if creation_limit {
                policy(1, 10, 1)
            } else {
                policy(10, 1, 1)
            };
            let result = transaction.execute_graph_edge_upserts_query_governed(
                &mut db,
                &cx,
                &query,
                allowance,
                |_| -> Result<ElementId, ()> {
                    Ok(ElementId::Edge(EId(u128::from(
                        next.fetch_add(1, Ordering::Relaxed),
                    ))))
                },
            );
            if creation_limit {
                assert!(matches!(
                    result,
                    Err(GqlQueryError::Source(GraphEdgeUpsertQueryError::Upsert(
                        GraphEdgeUpsertError::Merge(GraphEdgeMergeError::CreationLimit {
                            limit: 1,
                            observed: 2
                        })
                    )))
                ));
                assert_eq!(next.load(Ordering::Relaxed), 101);
            } else {
                assert!(matches!(
                    result,
                    Err(GqlQueryError::Source(GraphEdgeUpsertQueryError::Upsert(
                        GraphEdgeUpsertError::ActionLimit {
                            limit: 1,
                            observed: 2
                        }
                    )))
                ));
                assert_eq!(next.load(Ordering::Relaxed), 102);
            }
            assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
            assert!(transaction.edges(&db).unwrap().is_empty());
            transaction.abort();
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_input_keeps_global_aggregate_without_actions_or_allocation() {
    let ((), report) = run_async_under_lab(0xe6e1_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, vertices(2)).await.unwrap();
        let query = prepare(
            "MATCH (a),(b) WHERE a.p=99 AND b.p=2 MERGE (a)-[r:R {q:0}]->(b) \
             ON CREATE SET r.q=10/r.q RETURN count(r) AS count",
        );
        let mut transaction = db.begin(&txcx).unwrap();
        let (stats, rows) = transaction
            .execute_graph_edge_upserts_query_governed(
                &mut db,
                &cx,
                &query,
                policy(0, 0, 1),
                |_| -> Result<ElementId, ()> { panic!("no input cannot allocate") },
            )
            .unwrap();
        assert_eq!(
            (
                stats.created_edges,
                stats.matched_occurrences,
                stats.action_effects
            ),
            (0, 0, 0)
        );
        assert_eq!(cells(&rows.value), vec![vec![int(0)]]);
        transaction.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn bulk_autocommit_survives_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0xe6e1_0008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        db.write(&commit, vertices(3)).await.unwrap();
        let before = db.frontier().unwrap();
        let query = prepare(
            "MATCH (a),(b) WHERE a.p=1 AND b.p>1 MERGE (a)-[r:R]->(b) \
             ON CREATE SET r.q=7 RETURN r,r.q AS q ORDER BY r",
        );
        let (stats, rows, _) = db
            .execute_graph_edge_upserts_query_autocommit_governed(
                &txcx,
                &cx,
                &commit,
                &query,
                policy(2, 2, 2),
                |request| -> Result<ElementId, ()> {
                    let GraphInsertRequest::Edge { row, edge: 0 } = request else {
                        panic!("one edge request per missing pair")
                    };
                    Ok(ElementId::Edge(EId(100 + row as u128)))
                },
            )
            .await
            .unwrap();
        assert_eq!(stats.created_edges, 2);
        assert_eq!(
            cells(&rows.value),
            vec![
                vec![GraphValue::Edge(EId(100)), int(7)],
                vec![GraphValue::Edge(EId(101)), int(7)],
            ]
        );
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
        db.compact(&commit).await.unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(reopened.frontier().unwrap().0, before.0 + 1);
        assert_eq!(reopened.edges().unwrap().len(), 2);
        for edge in [EId(100), EId(101)] {
            assert_eq!(
                reopened.edge(edge).unwrap().unwrap().props,
                vec![(Q, CanonicalScalar::Int(7).into())]
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
