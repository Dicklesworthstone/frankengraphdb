use super::*;
use crate::recovery::tests::{served, vertex};
use asupersync::lab::run_async_under_lab;
use fgdb_gql::GqlParameters;
use fgdb_protocol::body::ExecuteMode;
use fgdb_warden::Restriction;

fn request(statement: &str) -> Execute {
    Execute {
        mode: ExecuteMode::Subscribe,
        statement: statement.into(),
        parameters: vec![],
    }
}

fn success<T>(result: Result<T, Refusal>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("unexpected {:?}: {}", error.code, error.message),
    }
}

fn refused<T>(result: Result<T, Refusal>, expected: ErrorCode) {
    match result {
        Ok(_) => panic!("expected {expected:?} refusal"),
        Err(error) => assert_eq!(error.code, expected, "{}", error.message),
    }
}

#[test]
fn native_phase_reservations_share_work_and_keep_rows_at_final_delivery() {
    let policy = GqlQueryPolicy::new(100_000, 100_000, 100_000, 100_000);
    let limits = QueryLimits {
        max_nodes: 4096,
        max_work: 4096,
        max_rows: 1,
    };
    for nodes in [1, 2, 7, 20] {
        for width in [0, 1, 2, 65] {
            let reservation = success(reserve(policy, limits, 4096, 8192, nodes, width));
            let maintained = reservation.setup.maintenance.evaluator;
            let delivery = reservation.setup.delivery.evaluator;
            let total = (nodes + 1) * (maintained.max_work_units + maintained.max_scratch_entries)
                + delivery.max_work_units
                + delivery.max_scratch_entries
                + reservation.wire_work;
            assert!(
                total <= 4096,
                "every circuit node, replay, baseline, and wire copy share work"
            );
            assert!(reservation.nodes <= limits.max_nodes);
            assert_eq!(
                reservation.setup.maintenance.rows.max_result_rows(),
                Some(100_000),
                "signed final rows must not truncate private maintained state"
            );
            assert_eq!(reservation.setup.delivery.rows.max_result_rows(), Some(1));
        }
    }
    refused(reserve(policy, limits, 4096, 8192, 0, 1), ErrorCode::Budget);
    refused(reserve(policy, limits, 1, 8192, 1, 1), ErrorCode::Budget);
}

#[test]
fn zero_signed_grants_refuse_before_installation_and_source_free_needs_no_nodes() {
    let ((), report) = run_async_under_lab(0x79a0_0301, |root| async move {
        let (mut server, token, _) = served(&root, "subscription-zero-grants").await;
        Arc::get_mut(server.databases.get_mut("test").unwrap())
            .unwrap()
            .max_subscriptions = 1;
        let db = &server.databases["test"];
        let zero_work = token.attenuate(Restriction::MaxWork(0)).unwrap();
        refused(
            subscribe(
                &root,
                db,
                &zero_work,
                &request("SUBSCRIBE TO RETURN 7 AS value"),
            )
            .await,
            ErrorCode::Budget,
        );
        let zero_nodes = token.attenuate(Restriction::MaxNodes(0)).unwrap();
        refused(
            subscribe(
                &root,
                db,
                &zero_nodes,
                &request("SUBSCRIBE TO MATCH (n) RETURN n"),
            )
            .await,
            ErrorCode::Budget,
        );
        let zero_rows = zero_nodes.attenuate(Restriction::MaxRows(0)).unwrap();
        refused(
            subscribe(
                &root,
                db,
                &zero_rows,
                &request("SUBSCRIBE TO RETURN 7 AS value"),
            )
            .await,
            ErrorCode::Budget,
        );
        assert_eq!(db.subscriptions.load(Ordering::Acquire), 0);
        let mut subscription = success(
            subscribe(
                &root,
                db,
                &zero_nodes,
                &request("SUBSCRIBE TO RETURN 7 AS value"),
            )
            .await,
        );
        assert_eq!(db.subscriptions.load(Ordering::Acquire), 1);
        let delivered = success(poll(&root, db, &zero_nodes, &mut subscription).await).unwrap();
        assert_eq!(delivered.entries, vec![(1, vec![WireValue::Int(7)])]);
        subscription
            .consumer
            .acknowledge(delivered.batch.receipt())
            .unwrap();
        assert!(success(poll(&root, db, &zero_nodes, &mut subscription).await).is_none());
    });
    assert!(report.lab_test_passed());
}

#[test]
fn signed_rows_count_complete_support_and_cover_cached_pending_redelivery() {
    let ((), report) = run_async_under_lab(0x79a0_0302, |root| async move {
        let (server, token, _) = served(&root, "subscription-pending-row-budget").await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut seed = vertex(1);
        seed.create_vertex(fgdb_types::VId(2), vec![], vec![]);
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), seed)
            .await
            .unwrap();
        let one_row = token.attenuate(Restriction::MaxRows(1)).unwrap();
        let mut subscription = success(
            subscribe(
                &root,
                db,
                &one_row,
                &request("SUBSCRIBE TO MATCH (n) RETURN 7 AS value"),
            )
            .await,
        );
        let zero_rows = one_row.attenuate(Restriction::MaxRows(0)).unwrap();
        refused(
            poll(&root, db, &zero_rows, &mut subscription).await,
            ErrorCode::Budget,
        );
        let first = success(poll(&root, db, &one_row, &mut subscription).await).unwrap();
        assert_eq!(first.entries, vec![(2, vec![WireValue::Int(7)])]);
        // The server's initial cache is now empty, but the native consumer
        // still owns this unacknowledged Arc and ignores a smaller native policy.
        refused(
            poll(&root, db, &zero_rows, &mut subscription).await,
            ErrorCode::Budget,
        );
        assert_eq!(subscription.consumer.acknowledged_frontier(), None);
        let replay = success(poll(&root, db, &one_row, &mut subscription).await).unwrap();
        assert!(Arc::ptr_eq(&first.batch, &replay.batch));
        assert_eq!(first.entries, replay.entries);
        subscription
            .consumer
            .acknowledge(first.batch.receipt())
            .unwrap();
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), vertex(3))
            .await
            .unwrap();
        let tiny_work = one_row.attenuate(Restriction::MaxWork(1)).unwrap();
        refused(
            poll(&root, db, &tiny_work, &mut subscription).await,
            ErrorCode::Budget,
        );
        let next = success(poll(&root, db, &one_row, &mut subscription).await).unwrap();
        assert!(!next.batch.is_snapshot());
        assert_eq!(next.entries, vec![(1, vec![WireValue::Int(7)])]);
    });
    assert!(report.lab_test_passed());
}

#[test]
fn maintenance_keeps_the_original_signed_policy_across_later_writes() {
    let ((), report) = run_async_under_lab(0x79a0_0303, |root| async move {
        let (server, token, _) = served(&root, "subscription-maintenance-budget").await;
        let db = &server.databases["test"];
        let bounded = token
            .attenuate(Restriction::MaxWork(4096))
            .unwrap()
            .attenuate(Restriction::MaxNodes(64))
            .unwrap();
        let mut subscription = success(
            subscribe(
                &root,
                db,
                &bounded,
                &request("SUBSCRIBE TO MATCH (n) RETURN n"),
            )
            .await,
        );
        let initial = success(poll(&root, db, &bounded, &mut subscription).await).unwrap();
        assert!(initial.entries.is_empty());
        subscription
            .consumer
            .acknowledge(initial.batch.receipt())
            .unwrap();
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut batch = vertex(1);
        for id in 2..=200 {
            batch.create_vertex(fgdb_types::VId(id), vec![], vec![]);
        }
        // The durable write still commits. Only the derived view is fenced
        // when its retained, originally admitted maintenance budget runs out.
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), batch)
            .await
            .unwrap();
        refused(
            poll(&root, db, &token, &mut subscription).await,
            ErrorCode::Budget,
        );
    });
    assert!(report.lab_test_passed());
}

#[test]
fn replay_retention_gap_uses_one_bounded_replacement_baseline() {
    let ((), report) = run_async_under_lab(0x79a0_0304, |root| async move {
        let (server, token, _) = served(&root, "subscription-replay-gap-budget").await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let query = contexts.query();
        let params = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare_subscription(
            &query,
            "SUBSCRIBE TO MATCH (n) RETURN n",
            &params,
            db.symbols.clone(),
        )
        .unwrap();
        let mut guard = db.db.write(&root).await.unwrap();
        let generation = guard.generation();
        let (mut consumer, baseline) = prepared
            .subscribe_replaying(
                &mut guard,
                &query,
                &params,
                NativeSubscriptionSetup {
                    maintenance: db.query_policy,
                    delivery: db.query_policy,
                    retention: [1, 1000, 100_000],
                },
                |_, batch| Ok::<_, SubscriptionError>(Arc::clone(batch)),
            )
            .unwrap();
        consumer.acknowledge(baseline.receipt()).unwrap();
        for id in 1..=3 {
            guard.write(&contexts.commit(), vertex(id)).await.unwrap();
        }
        drop(guard);
        let admitted = db
            .authority
            .verify_at(&token, TRUNK, unix_millis())
            .unwrap()
            .predicates()
            .limits();
        let mut subscription = Subscription {
            consumer,
            columns: vec!["n".into()],
            generation,
            admitted,
            initial: None,
            registration_work: 0,
            registration_nodes: 0,
        };
        let tiny = token.attenuate(Restriction::MaxWork(6)).unwrap();
        refused(
            poll(&root, db, &tiny, &mut subscription).await,
            ErrorCode::Budget,
        );
        assert_eq!(subscription.consumer.acknowledged_frontier(), None);
        let replacement = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert!(replacement.batch.is_snapshot());
        assert_eq!(replacement.entries.len(), 3);
        subscription
            .consumer
            .acknowledge(replacement.batch.receipt())
            .unwrap();
        assert!(success(poll(&root, db, &token, &mut subscription).await).is_none());
    });
    assert!(report.lab_test_passed());
}

#[test]
fn source_free_grouping_keeps_private_input_rows_with_zero_signed_nodes() {
    let ((), report) = run_async_under_lab(0x79a0_0305, |root| async move {
        let (server, token, _) = served(&root, "subscription-source-free-group").await;
        let db = &server.databases["test"];
        let bounded = token
            .attenuate(Restriction::MaxNodes(0))
            .unwrap()
            .attenuate(Restriction::MaxRows(1))
            .unwrap();
        let mut subscription = success(
            subscribe(
                &root,
                db,
                &bounded,
                &request("SUBSCRIBE TO UNWIND [1,2] AS x RETURN SUM(x) AS total"),
            )
            .await,
        );
        let initial = success(poll(&root, db, &bounded, &mut subscription).await).unwrap();
        assert_eq!(initial.entries, vec![(1, vec![WireValue::WideInt(3)])]);
    });
    assert!(report.lab_test_passed());
}
