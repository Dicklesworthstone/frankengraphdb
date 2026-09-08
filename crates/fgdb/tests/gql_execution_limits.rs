use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, GqlError, ReadError, RelationBind, WriteBatch, WriteError,
    WriteTxnError,
};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_gql::{GlaExecutionError, GlaExecutionLimits, GlaLimitDimension, GlaLimitExceeded};
use fgdb_types::{CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}

#[test]
fn limited_queries_agree_across_live_pinned_historical_and_staged_surfaces() {
    let ((), report) = run_async_under_lab(0x61a1_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&cx, keys()).await.expect("database");
        let mut seed = WriteBatch::new(RelationId(1));
        for id in 1..=4 {
            seed.create_vertex(VId(id), vec![LabelId(1)], vec![]);
        }
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(2), VId(3), vec![]);
        seed.add_edge(EId(12), VId(2), VId(4), vec![]);
        db.write(&cx, seed).await.expect("seed");
        let basis = db.frontier().expect("basis");
        let pinned = db.read_session().expect("pinned view");
        let bind = RelationBind::new()
            .with_relation("R", RelationId(1))
            .with_label("L", LabelId(1));
        let limits = GlaExecutionLimits::new(10_000, 10_000);
        let mut txn = db.begin(&txn_cx).expect("begin");
        for statement in [
            "MATCH (a:L) RETURN a",
            "MATCH (a)-[:R]->(b) RETURN b",
            "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN c",
            "MATCH (a)-[:R]-(b)-[:R]-(c) RETURN b SKIP 1 LIMIT 2",
            "MATCH (a)<-[:R]-(b)<-[:R]-(c) RETURN c",
        ] {
            let query = db.prepare_gql_query(statement, &bind).expect("prepare");
            let reference = db
                .execute_prepared_query(&query)
                .expect("durable reference");
            let live = db
                .execute_prepared_query_limited(&query, limits)
                .expect("limited live");
            assert_eq!(live.value, reference);
            assert_eq!(
                pinned
                    .execute_prepared_query_limited(&query, limits)
                    .expect("limited pinned"),
                live
            );
            assert_eq!(
                db.execute_prepared_query_limited_at(&query, basis, limits)
                    .expect("limited history"),
                live
            );
            assert_eq!(
                txn.execute_prepared_query_limited(&db, &query, limits)
                    .expect("limited overlay")
                    .value,
                reference
            );
        }
        let query = db
            .prepare_gql_query("MATCH (a)-[:R]->(b)-[:R]->(c) RETURN c", &bind)
            .expect("prepare");
        let old = pinned
            .execute_prepared_query_limited(&query, limits)
            .expect("old view")
            .value;
        let mut stage = WriteBatch::new(RelationId(1));
        stage.create_vertex(VId(5), vec![LabelId(1)], vec![]);
        stage.add_edge(EId(13), VId(3), VId(5), vec![]);
        txn.write(&mut db, stage).expect("stage");
        let overlay = txn
            .execute_prepared_query_limited(&db, &query, limits)
            .expect("staged query")
            .value;
        assert_eq!(
            overlay,
            txn.execute_prepared_query(&db, &query)
                .expect("ordinary overlay")
        );
        assert_ne!(overlay, old);
        let published = txn.commit(&mut db, &cx).await.expect("commit");
        assert_eq!(
            db.execute_prepared_query_limited(&query, limits)
                .expect("published query")
                .value,
            overlay
        );
        assert_eq!(
            db.execute_prepared_query_limited_at(&query, basis, limits)
                .expect("retained old query")
                .value,
            old
        );
        assert_eq!(
            pinned
                .execute_prepared_query_limited(&query, limits)
                .expect("still pinned")
                .value,
            old
        );
        assert!(matches!(
            pinned.execute_prepared_query_limited_at(
                &query,
                published,
                GlaExecutionLimits::new(0, 0)
            ),
            Err(GlaExecutionError::Source(GqlError::Read(
                ReadError::BeyondFrontier { .. }
            )))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn execution_refusal_preserves_phantom_validation_and_owner_checks() {
    let ((), report) = run_async_under_lab(0x61a1_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&cx, keys()).await.expect("owner");
        let foreign = Database::open_memory(&cx, keys()).await.expect("foreign");
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&cx, seed).await.expect("seed");
        let mut txn = db.begin(&txn_cx).expect("begin");
        let query = db
            .prepare_gql_query(
                "MATCH (a)-[:R]->(b) RETURN b",
                &RelationBind::new().with_relation("R", RelationId(1)),
            )
            .expect("prepare");
        assert!(matches!(
            txn.execute_prepared_query_limited(&foreign, &query, GlaExecutionLimits::new(0, 0)),
            Err(GlaExecutionError::Source(WriteTxnError::WrongDatabase))
        ));
        assert!(matches!(
            txn.execute_prepared_query_limited(&db, &query, GlaExecutionLimits::new(1, 100)),
            Err(GlaExecutionError::Limit(GlaLimitExceeded {
                dimension: GlaLimitDimension::WorkUnits,
                ..
            }))
        ));
        let mut stage = WriteBatch::new(RelationId(1));
        stage.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, stage)
            .expect("continue after refused query");
        let mut winner = WriteBatch::new(RelationId(1));
        winner.create_vertex(VId(3), vec![], vec![]);
        winner.create_vertex(VId(4), vec![], vec![]);
        winner.add_edge(EId(11), VId(3), VId(4), vec![]);
        db.write(&cx, winner).await.expect("disconnected phantom");
        let frontier = db.frontier().expect("frontier");
        assert!(matches!(
            txn.commit(&mut db, &cx).await,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                law: "FG-LAW-FCW-READ-01",
                ..
            }))
        ));
        assert_eq!(db.frontier().expect("unchanged"), frontier);
        assert!(db.vertex(VId(99)).expect("no partial output").is_none());
        assert!(matches!(
            db.execute_prepared_query_limited_at(
                &query,
                CommitSeq(frontier.0 + 1),
                GlaExecutionLimits::new(0, 0)
            ),
            Err(GlaExecutionError::Source(GqlError::Read(
                ReadError::BeyondFrontier { .. }
            )))
        ))
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
