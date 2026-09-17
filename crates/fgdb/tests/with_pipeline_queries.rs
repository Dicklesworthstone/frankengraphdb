//! Relational pipelines use the real Chronicle/Strata and transaction read path.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, GqlError, MemVfs, ReadError, WriteBatch, WriteError, WriteTxnError,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSetExecutionError, GraphSymbol,
    GraphSymbolKind, PreparedGraphSet, PreparedGraphSetText,
};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x41; 32],
        DatabaseSecurityNamespaceId([0x42; 32]),
        [0x43; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 10_000, 20_000_000, 20_000_000)
}
fn query(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn ranked() -> PreparedGraphSet {
    query(
        "MATCH (n) WITH n AS owner,n.p+1 AS score ORDER BY score DESC LIMIT 2 WHERE score>2 RETURN owner,score*10 AS rank ORDER BY owner",
    )
}
fn pairs(rows: &[GraphValueRow]) -> Vec<(VId, i64)> {
    rows.iter()
        .map(|row| {
            let Some(CanonicalScalar::Int(score)) = row.get(1).unwrap().as_scalar() else {
                panic!("integer rank")
            };
            (row.get(0).unwrap().as_vertex().unwrap(), *score)
        })
        .collect()
}
async fn seed(db: &mut Database<MemVfs>, cx: &fgdb_types::CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for (id, value) in [(1, 1), (2, 2), (3, 3), (4, 0)] {
        batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(value))]);
    }
    db.write(cx, batch).await.unwrap()
}
fn change(id: u128, value: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.set_vertex_property(VId(id), P, Some(CanonicalScalar::Int(value)));
    batch
}

#[test]
fn ranked_pipeline_pins_history_and_survives_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0x71f0_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let old = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let query = ranked();
        let original = vec![(VId(2), 30), (VId(3), 40)];
        assert_eq!(
            pairs(
                &db.execute_graph_set_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            original
        );
        let latest = db.write(&commit, change(1, 9)).await.unwrap();
        let current = vec![(VId(1), 100), (VId(3), 40)];
        assert_eq!(
            pairs(
                &db.execute_graph_set_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            current
        );
        assert_eq!(
            pairs(
                &pinned
                    .execute_graph_set_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            original
        );
        assert_eq!(
            pairs(
                &db.execute_graph_set_governed_at(&cx, &query, old, policy())
                    .unwrap()
                    .value
            ),
            original
        );
        assert!(matches!(
            pinned.execute_graph_set_governed_at(
                &cx,
                &query,
                latest,
                GqlQueryPolicy::new(0, 0, 0, 0)
            ),
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                GqlError::Read(ReadError::BeyondFrontier { .. })
            )))
        ));
        db.compact(&commit).await.unwrap();
        drop(pinned);
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            pairs(
                &db.execute_graph_set_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            current
        );
        assert_eq!(
            pairs(
                &db.execute_graph_set_governed_at(&cx, &query, old, policy())
                    .unwrap()
                    .value
            ),
            original
        );
        assert_eq!(db.frontier().unwrap(), latest);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn pipeline_reads_canonical_staged_values_without_publishing_or_changing_effects() {
    let ((), report) = run_async_under_lab(0x71f0_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let frontier = seed(&mut db, &commit).await;
        let query = ranked();
        let mut txn = db.begin(&txcx).unwrap();
        txn.write(&mut db, change(1, 9)).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let first = txn
            .execute_graph_set_governed(&db, &cx, &query, policy())
            .unwrap();
        assert_eq!(pairs(&first.value), vec![(VId(1), 100), (VId(3), 40)]);
        assert_eq!(
            txn.execute_graph_set_governed(&db, &cx, &query, policy())
                .unwrap(),
            first
        );
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(
            pairs(
                &db.execute_graph_set_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            vec![(VId(2), 30), (VId(3), 40)]
        );
        txn.abort();
        assert_eq!(db.frontier().unwrap(), frontier);
        let mut reader = db.begin(&txcx).unwrap();
        reader
            .execute_graph_set_governed(&db, &cx, &query, policy())
            .unwrap();
        reader.finish(&mut db, &commit).await.unwrap();
        assert_eq!(
            db.frontier().unwrap(),
            frontier,
            "read-only completion must not publish a marker"
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtered_empty_and_limited_rows_keep_transaction_conflict_witnesses() {
    let ((), report) = run_async_under_lab(0x71f0_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        for text in [
            "MATCH (n) WITH n,n.p AS p WHERE p>100 RETURN n",
            "MATCH (n) WITH n,n.p AS p ORDER BY p DESC LIMIT 1 RETURN n",
            "MATCH (n) WITH n,n.p AS p RETURN n LIMIT 0",
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit).await;
            let mut txn = db.begin(&txcx).unwrap();
            txn.execute_graph_set_governed(&db, &cx, &query(text), policy())
                .unwrap();
            db.write(&commit, change(4, 200)).await.unwrap();
            assert!(matches!(
                txn.finish(&mut db, &commit).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
            ));
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn relational_adapters_share_all_caps_and_validate_lifecycle_before_zero_budget() {
    let ((), report) = run_async_under_lab(0x71f0_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let frontier = seed(&mut db, &commit).await;
        let query = ranked();
        let measured = db
            .execute_graph_set_governed(&cx, &query, policy())
            .unwrap();
        let caps = [
            measured.rows.snapshot_records,
            measured.rows.result_rows,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        ];
        assert_eq!(
            db.execute_graph_set_governed(
                &cx,
                &query,
                GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3])
            )
            .unwrap(),
            measured
        );
        for dimension in 0..4 {
            let mut limit = caps;
            limit[dimension] -= 1;
            assert!(
                db.execute_graph_set_governed(
                    &cx,
                    &query,
                    GqlQueryPolicy::new(limit[0], limit[1], limit[2], limit[3])
                )
                .is_err()
            );
            assert_eq!(db.frontier().unwrap(), frontier);
        }
        let zero = GqlQueryPolicy::new(0, 0, 0, 0);
        assert!(matches!(
            db.execute_graph_set_governed_at(&cx, &query, CommitSeq(frontier.0 + 1), zero),
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                GqlError::Read(ReadError::BeyondFrontier { .. })
            )))
        ));
        let other = Database::open_memory(&commit, keys()).await.unwrap();
        let txn = db.begin(&txcx).unwrap();
        assert!(matches!(
            txn.execute_graph_set_governed(&other, &cx, &query, zero),
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                WriteTxnError::WrongDatabase
            )))
        ));
        txn.abort();
        let mut finished = db.begin(&txcx).unwrap();
        finished.finish(&mut db, &commit).await.unwrap();
        assert!(matches!(
            finished.execute_graph_set_governed(&db, &cx, &query, zero),
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                WriteTxnError::Finished
            )))
        ));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_arithmetic_failure_preserves_outer_effects_and_observed_reads() {
    let ((), report) = run_async_under_lab(0x71f0_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let frontier = seed(&mut db, &commit).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let query = query("MATCH (n) WITH n,n.p AS value RETURN n,10/value AS q LIMIT 0");
        assert!(matches!(
            txn.execute_graph_set_governed(&db, &cx, &query, policy()),
            Err(GqlQueryError::Source(
                GraphSetExecutionError::Projection { .. }
            ))
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(db.vertex(VId(99)).unwrap().is_none());
        db.write(&commit, change(4, 1)).await.unwrap();
        assert!(matches!(
            txn.finish(&mut db, &commit).await,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
        ));
        assert!(db.vertex(VId(99)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn parallel_edges_keep_bag_multiplicity_through_with_and_union() {
    let ((), report) = run_async_under_lab(0x71f0_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut edges = WriteBatch::new(R);
        for (id, dst) in [(10, 2), (11, 2), (12, 3)] {
            edges.add_edge(EId(id), VId(1), VId(dst), vec![]);
        }
        let frontier = db.write(&commit, edges).await.unwrap();
        for (text, expected) in [
            (
                "MATCH (a)-[:R]->(b) WITH b.p AS score RETURN score",
                vec![2, 2, 3],
            ),
            (
                "MATCH (a)-[:R]->(b) WITH DISTINCT b.p AS score RETURN score",
                vec![2, 3],
            ),
            (
                "MATCH (a)-[:R]->(b) WITH b.p AS score WHERE score=2 RETURN score UNION ALL MATCH (n) WITH n.p AS score WHERE score=3 RETURN score",
                vec![2, 2, 3],
            ),
        ] {
            let rows = db
                .execute_graph_set_governed(&cx, &query(text), policy())
                .unwrap()
                .value;
            let actual = rows
                .iter()
                .map(|row| match row.get(0).unwrap().as_scalar() {
                    Some(CanonicalScalar::Int(value)) => *value,
                    _ => panic!("integer score"),
                })
                .collect::<Vec<_>>();
            assert_eq!(actual, expected);
        }
        assert_eq!(db.frontier().unwrap(), frontier);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
