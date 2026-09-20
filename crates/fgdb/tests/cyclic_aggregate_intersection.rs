//! Multiway cyclic support joins through the real admitted snapshot/overlay APIs.
//! The comparison plan forces ordinary bag visitation with a hidden COLLECT.
//! Its extra output is removed only after aggregation, not by input rewriting.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::RelationId;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, GraphSymbol,
    GraphSymbolKind, PreparedGraphAggregate, PreparedGraphAggregateText,
};
use fgdb_types::{CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const HI: u128 = 1_u128 << 100;
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 20_000_000, 5_000_000)
}
fn query(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, |kind, name| match kind {
        GraphSymbolKind::Relation => Some(GraphSymbol::Relation(RelationId(match name {
            "R" => 1,
            "S" => 2,
            "T" => 3,
            "U" => 4,
            _ => 5,
        }))),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
fn cases() -> Vec<(PreparedGraphAggregate, PreparedGraphAggregate)> {
    [
        "(a)-[:R]->(b)-[:S]->(c)-[:T]->(a)",
        "(a)<-[:R]-(b)<-[:S]-(c)<-[:T]-(a)",
        "(a)-[:R]-(b)-[:S]-(c)-[:T]-(a)",
        "(a)-[:R]->(b)-[:S]->(c)-[:T]->(a) WHERE a<>c",
        "(a)-[:R]->(b)-[:S]->(c)-[:T]->(a),(a)-[:R]->(b)",
        "(a)-[:R]->(b)-[:S]->(c)-[:T]->(a),(c)-[:U]->(leaf)",
        "(a)-[:R]->(b)-[:S]->(c)-[:U]->(hidden)-[:T]->(a)",
    ].into_iter().map(|body| {
        let prefix = format!("MATCH {body} RETURN a,b,c,COUNT(*) AS n,COUNT(c) AS m,COUNT(DISTINCT c) AS d,MIN(c) AS lo,MAX(c) AS hi");
        (query(&format!("{prefix} GROUP BY a,b,c")),
            query(&format!("{prefix},COLLECT(c) AS hidden_bag GROUP BY a,b,c"))
                .with_aggregate_output_prefix(5).unwrap())
    }).collect()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut vertices = WriteBatch::new(RelationId(1));
    for id in [0, 1, 2, 3, HI, u128::MAX] {
        vertices.create_vertex(VId(id), vec![], vec![]);
    }
    let mut batches = vec![vertices];
    for (relation, records) in [
        (
            1,
            vec![(10, 0, 1), (11, 0, 1), (12, 2, 1), (13, HI, 1), (14, 3, 3)],
        ),
        (
            2,
            vec![
                (20, 1, 2),
                (21, 1, 2),
                (22, 1, 3),
                (23, 1, u128::MAX),
                (24, 3, 3),
            ],
        ),
        (
            3,
            vec![
                (30, 2, 0),
                (31, 3, 0),
                (32, 3, 2),
                (33, 2, HI),
                (34, u128::MAX, HI),
                (35, 3, 3),
            ],
        ),
        (
            4,
            vec![
                (40, 2, 3),
                (41, 2, 3),
                (42, 3, u128::MAX),
                (43, 1, u128::MAX),
            ],
        ),
    ] {
        let mut batch = WriteBatch::new(RelationId(relation));
        for (id, from, to) in records {
            batch.add_edge(EId(id), VId(from), VId(to), vec![]);
        }
        batches.push(batch);
    }
    db.write_atomic(cx, batches).await.unwrap()
}

#[test]
fn cyclic_counts_preserve_historical_pinned_and_canonical_staged_multiplicities() {
    let ((), report) = run_async_under_lab(0x1a73_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let definitions = cases();
        let mut old = Vec::new();
        for (fast, ordinary) in &definitions {
            let expected = db
                .execute_graph_aggregate_governed(&cx, ordinary, wide())
                .unwrap()
                .value;
            assert_eq!(
                db.execute_graph_aggregate_governed(&cx, fast, wide())
                    .unwrap()
                    .value,
                expected
            );
            assert_eq!(
                pinned
                    .execute_graph_aggregate_governed(&cx, fast, wide())
                    .unwrap()
                    .value,
                expected
            );
            old.push(expected);
        }
        assert!(
            old[0]
                .iter()
                .any(|row| row.get(0).unwrap().as_count().unwrap() > 1)
        );
        let mut txn = db.begin(&txcx).unwrap();
        let mut edits = WriteBatch::new(RelationId(1));
        edits.delete_edge(EId(10));
        edits.ensure_edge_by_triple(EId(888), VId(0), VId(1), vec![]);
        edits.add_edge(EId(90), VId(2), VId(1), vec![]);
        txn.write(&mut db, edits).unwrap();
        let mut staged = Vec::new();
        for (fast, ordinary) in &definitions {
            let expected = txn
                .execute_graph_aggregate_governed(&db, &cx, ordinary, wide())
                .unwrap()
                .value;
            assert_eq!(
                txn.execute_graph_aggregate_governed(&db, &cx, fast, wide())
                    .unwrap()
                    .value,
                expected
            );
            staged.push(expected);
        }
        assert_ne!(staged[0], old[0]);
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        for (((fast, ordinary), expected_old), expected_new) in
            definitions.iter().zip(&old).zip(&staged)
        {
            assert_eq!(
                &db.execute_graph_aggregate_governed(&cx, fast, wide())
                    .unwrap()
                    .value,
                expected_new
            );
            assert_eq!(
                &db.execute_graph_aggregate_governed(&cx, ordinary, wide())
                    .unwrap()
                    .value,
                expected_new
            );
            assert_eq!(
                &db.execute_graph_aggregate_governed_at(&cx, fast, basis, wide())
                    .unwrap()
                    .value,
                expected_old
            );
            assert_eq!(
                &pinned
                    .execute_graph_aggregate_governed(&cx, fast, wide())
                    .unwrap()
                    .value,
                expected_old
            );
        }
        let mut cascade = WriteBatch::new(RelationId(1));
        cascade.delete_vertex(VId(1));
        db.write(&commit, cascade).await.unwrap();
        for ((fast, ordinary), expected_old) in definitions.iter().zip(&old) {
            assert_eq!(
                db.execute_graph_aggregate_governed(&cx, fast, wide())
                    .unwrap()
                    .value,
                db.execute_graph_aggregate_governed(&cx, ordinary, wide())
                    .unwrap()
                    .value
            );
            assert_eq!(
                &pinned
                    .execute_graph_aggregate_governed_at(&cx, fast, basis, wide())
                    .unwrap()
                    .value,
                expected_old
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cumulative_limits_future_cuts_and_transaction_conflicts_survive_physical_intersection() {
    let ((), report) = run_async_under_lab(0x1a73_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        for outcome in 0..3 {
            for change in 0..3 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let basis = seed(&mut db, &commit).await;
                let (fast, _) = cases().remove(0);
                let baseline = db
                    .execute_graph_aggregate_governed(&cx, &fast, wide())
                    .unwrap();
                let exact = GqlQueryPolicy::new(
                    baseline.rows.snapshot_records,
                    baseline.rows.result_rows,
                    baseline.evaluator.work_units,
                    baseline.evaluator.scratch_entries,
                );
                assert_eq!(
                    db.execute_graph_aggregate_governed(&cx, &fast, exact)
                        .unwrap(),
                    baseline
                );
                for policy in [
                    GqlQueryPolicy::new(
                        baseline.rows.snapshot_records - 1,
                        100_000,
                        u64::MAX,
                        u64::MAX,
                    ),
                    GqlQueryPolicy::new(100_000, baseline.rows.result_rows - 1, u64::MAX, u64::MAX),
                    GqlQueryPolicy::new(
                        100_000,
                        100_000,
                        baseline.evaluator.work_units - 1,
                        u64::MAX,
                    ),
                    GqlQueryPolicy::new(
                        100_000,
                        100_000,
                        u64::MAX,
                        baseline.evaluator.scratch_entries - 1,
                    ),
                ] {
                    assert!(
                        db.execute_graph_aggregate_governed(&cx, &fast, policy)
                            .is_err()
                    );
                }
                assert!(matches!(
                    db.execute_graph_aggregate_governed_at(
                        &cx,
                        &fast,
                        CommitSeq(basis.0 + 1),
                        GqlQueryPolicy::new(0, 0, 0, 0)
                    ),
                    Err(GqlQueryError::Source(GraphAggregateError::Source(_)))
                ));
                let mut txn = db.begin(&txcx).unwrap();
                let mut edit = WriteBatch::new(RelationId(1));
                edit.create_vertex(VId(777), vec![], vec![]);
                txn.write(&mut db, edit).unwrap();
                let q = if outcome == 1 {
                    query(
                        "MATCH (a)-[:R]->(b)-[:S]->(c)-[:T]->(a) WHERE a<>a RETURN a,b,c,COUNT(*) AS n GROUP BY a,b,c",
                    )
                } else {
                    fast
                };
                let policy = if outcome == 2 {
                    GqlQueryPolicy::new(100_000, 0, 20_000_000, 5_000_000)
                } else {
                    wide()
                };
                let result = txn.execute_graph_aggregate_governed(&db, &cx, &q, policy);
                match outcome {
                    0 => assert!(!result.unwrap().value.is_empty()),
                    1 => assert!(result.unwrap().value.is_empty()),
                    _ => assert!(matches!(result, Err(GqlQueryError::Rows(_)))),
                }
                let mut winner = WriteBatch::new(RelationId(1));
                match change {
                    0 => {
                        winner.add_edge(EId(91), VId(0), VId(1), vec![]);
                    }
                    1 => {
                        winner.delete_edge(EId(11));
                    }
                    _ => {
                        winner.delete_vertex(VId(2));
                    }
                }
                db.write(&commit, winner).await.unwrap();
                let frontier = db.frontier().unwrap();
                // No second graph read can repair missing query dependencies.
                assert!(matches!(
                    txn.commit(&mut db, &commit).await,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01",
                        ..
                    }))
                ));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertex(VId(777)).unwrap().is_none());
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn database_cyclic_join_handles_sparse_closure_without_quadratic_intermediates() {
    let ((), report) = run_async_under_lab(0x1a73_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let n = 256_u128;
        let mut vertices = WriteBatch::new(RelationId(1));
        for id in 0..=2 * n {
            vertices.create_vertex(VId(id), vec![], vec![]);
        }
        let mut r = WriteBatch::new(RelationId(1));
        let mut s = WriteBatch::new(RelationId(2));
        let mut t = WriteBatch::new(RelationId(3));
        for id in 0..n {
            r.add_edge(EId(id * 3), VId(id), VId(n), vec![]);
            s.add_edge(EId(id * 3 + 1), VId(n), VId(n + 1 + id), vec![]);
            t.add_edge(EId(id * 3 + 2), VId(n + 1 + id), VId(id), vec![]);
        }
        db.write_atomic(&commit, vec![vertices, r, s, t])
            .await
            .unwrap();
        let q = query(
            "MATCH (a)-[:R]->(b)-[:S]->(c)-[:T]->(a) RETURN a,b,c,COUNT(*) AS n GROUP BY a,b,c",
        );
        // The budget includes real snapshot admission and result accumulation,
        // not just the intersection kernel's private candidate count.
        let result = db
            .execute_graph_aggregate_governed(
                &cx,
                &q,
                GqlQueryPolicy::new(10_000, n as u64, 1_000_000, 500_000),
            )
            .unwrap();
        assert_eq!(result.value.len(), n as usize);
        assert!(
            result
                .value
                .iter()
                .all(|row| row.get(0).unwrap().as_count() == Some(1))
        );
        assert!(result.evaluator.work_units < n as u64 * n as u64);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
