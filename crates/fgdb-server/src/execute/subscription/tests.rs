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
fn subscription_map_fields_bind_the_same_key_for_baseline_and_later_deltas() {
    let ((), report) = run_async_under_lab(0x79a0_0317, |root| async move {
        let (mut server, token, _) = served(&root, "subscription-map-fields").await;
        Arc::get_mut(server.databases.get_mut("test").unwrap())
            .unwrap()
            .symbols
            .bind(fgdb_gql::GraphSymbolKind::Property, "p", 1)
            .unwrap();
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut seed = vertex(1);
        seed.set_vertex_property(
            fgdb_types::VId(1),
            fgdb_delta_types::PropertyKeyId(1),
            Some(fgdb_types::CanonicalScalar::Int(7)),
        );
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), seed)
            .await
            .unwrap();
        let mut statement =
            request("SUBSCRIBE TO MATCH (n {p:$payload.filter.key}) RETURN n.p AS p");
        statement.parameters = vec![(
            "payload".into(),
            WireValue::Map(vec![(
                "filter".into(),
                WireValue::Map(vec![("key".into(), WireValue::Int(7))]),
            )]),
        )];
        let mut selected = success(subscribe(&root, db, &token, &statement).await);
        let baseline = success(poll(&root, db, &token, &mut selected).await).unwrap();
        assert_eq!(baseline.entries, vec![(1, vec![WireValue::Int(7)])]);
        selected
            .consumer
            .acknowledge(baseline.batch.receipt())
            .unwrap();
        statement.parameters = vec![(
            "payload".into(),
            WireValue::Map(vec![("filter".into(), WireValue::Map(vec![]))]),
        )];
        let mut missing = success(subscribe(&root, db, &token, &statement).await);
        let empty = success(poll(&root, db, &token, &mut missing).await).unwrap();
        assert!(empty.entries.is_empty());
        missing.consumer.acknowledge(empty.batch.receipt()).unwrap();
        let mut changed = fgdb::WriteBatch::new(fgdb_delta_types::RelationId(1));
        changed.set_vertex_property(
            fgdb_types::VId(1),
            fgdb_delta_types::PropertyKeyId(1),
            Some(fgdb_types::CanonicalScalar::Int(8)),
        );
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), changed)
            .await
            .unwrap();
        let delta = success(poll(&root, db, &token, &mut selected).await).unwrap();
        assert!(!delta.batch.is_snapshot());
        assert_eq!(delta.entries, vec![(-1, vec![WireValue::Int(7)])]);
        selected
            .consumer
            .acknowledge(delta.batch.receipt())
            .unwrap();
        let unseen = success(poll(&root, db, &token, &mut missing).await).unwrap();
        assert!(unseen.entries.is_empty());
        let denied = token.attenuate(Restriction::MaxWork(0)).unwrap();
        refused(
            subscribe(&root, db, &denied, &statement).await,
            ErrorCode::Budget,
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
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
fn coalesced_server_pulls_bound_net_ranges_and_preserve_pending_receipts() {
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_types::{CanonicalScalar, VId};
    use std::future::Future as _;
    use std::num::NonZeroUsize;

    let ((), report) = run_async_under_lab(0x79a0_0320, |root| async move {
        let (mut server, token, _) = served(&root, "subscription-coalesced-ranges").await;
        let configured = Arc::get_mut(server.databases.get_mut("test").unwrap()).unwrap();
        assert_eq!(configured.subscription_max_coalesced_ticks.get(), 1);
        configured.subscription_max_coalesced_ticks = NonZeroUsize::new(3).unwrap();
        configured
            .symbols
            .bind(fgdb_gql::GraphSymbolKind::Property, "p", 1)
            .unwrap();
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let edit = |value| {
            let mut batch = fgdb::WriteBatch::new(RelationId(1));
            batch.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(value)));
            batch
        };
        let mut seed = vertex(1);
        seed.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(0)));
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), seed)
            .await
            .unwrap();
        let mut subscription = success(
            subscribe(
                &root,
                db,
                &token,
                &request("SUBSCRIBE TO MATCH (n) RETURN n.p AS p"),
            )
            .await,
        );
        let baseline = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert_eq!(baseline.entries, [(1, vec![WireValue::Int(0)])]);
        let mut cuts = Vec::new();
        for value in [1, 2, 0, 9] {
            cuts.push(
                db.db
                    .write(&root)
                    .await
                    .unwrap()
                    .write(&contexts.commit(), edit(value))
                    .await
                    .unwrap(),
            );
        }
        let repeated = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert!(Arc::ptr_eq(&baseline.batch, &repeated.batch));
        assert_eq!(subscription.consumer.acknowledged_frontier(), None);
        subscription
            .consumer
            .acknowledge(baseline.batch.receipt())
            .unwrap();

        // The first three commits cancel exactly. The final NET row ceiling
        // may therefore be zero despite two changed rows in each atomic tick.
        let no_rows = token.attenuate(Restriction::MaxRows(0)).unwrap();
        let empty = success(poll(&root, db, &no_rows, &mut subscription).await).unwrap();
        assert!(!empty.batch.is_snapshot());
        assert_eq!(empty.batch.from(), Some(baseline.batch.frontier()));
        assert_eq!(empty.batch.frontier(), cuts[2]);
        assert!(empty.entries.is_empty());
        assert_eq!(
            subscription.consumer.acknowledged_frontier(),
            Some(baseline.batch.frontier())
        );
        let latest = db
            .db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), edit(10))
            .await
            .unwrap();
        let repeated = success(poll(&root, db, &no_rows, &mut subscription).await).unwrap();
        assert!(Arc::ptr_eq(&empty.batch, &repeated.batch));
        subscription
            .consumer
            .acknowledge(empty.batch.receipt())
            .unwrap();

        // The next range is 0 -> 10, not 0 -> 9 or 9 -> 10. Refusing its
        // complete support leaves the acknowledged cut and both ticks intact.
        let one_row = token.attenuate(Restriction::MaxRows(1)).unwrap();
        refused(
            poll(&root, db, &one_row, &mut subscription).await,
            ErrorCode::Budget,
        );
        assert_eq!(subscription.consumer.acknowledged_frontier(), Some(cuts[2]));
        let changed = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert_eq!(changed.batch.from(), Some(cuts[2]));
        assert_eq!(changed.batch.frontier(), latest);
        assert_eq!(
            changed.entries,
            [(-1, vec![WireValue::Int(0)]), (1, vec![WireValue::Int(10)])]
        );
        let tiny = token.attenuate(Restriction::MaxWork(1)).unwrap();
        refused(
            poll(&root, db, &tiny, &mut subscription).await,
            ErrorCode::Budget,
        );
        let hidden = token
            .attenuate(Restriction::Properties(fgdb_warden::Scope::only([])))
            .unwrap();
        refused(
            poll(&root, db, &hidden, &mut subscription).await,
            ErrorCode::PermissionDenied,
        );
        // Dropping a pull while it waits behind a writer cannot acknowledge
        // or split the already selected range. The next pull shares its Arc.
        let held = db.db.write(&root).await.unwrap();
        let mut interrupted = Box::pin(poll(&root, db, &token, &mut subscription));
        assert!(
            interrupted
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        drop(interrupted);
        drop(held);
        assert_eq!(subscription.consumer.acknowledged_frontier(), Some(cuts[2]));
        let repeated = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert!(Arc::ptr_eq(&changed.batch, &repeated.batch));
        assert_eq!(changed.entries, repeated.entries);
        assert_eq!(
            subscription
                .consumer
                .acknowledge(changed.batch.receipt())
                .unwrap(),
            latest
        );
        assert_eq!(
            subscription
                .consumer
                .acknowledge(changed.batch.receipt())
                .unwrap(),
            latest
        );
        assert!(success(poll(&root, db, &token, &mut subscription).await).is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
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
        for max_ticks in [1, 3] {
            let (mut server, token, _) = served(
                &root,
                &format!("subscription-replay-gap-budget-{max_ticks}"),
            )
            .await;
            Arc::get_mut(server.databases.get_mut("test").unwrap())
                .unwrap()
                .subscription_max_coalesced_ticks = std::num::NonZeroUsize::new(max_ticks).unwrap();
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
                .subscribe_scoped(
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
            db.subscriptions.fetch_add(1, Ordering::AcqRel);
            let consumer = generation.own_subscription(consumer);
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
        }
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

#[test]
fn component_calls_subscribe_over_native_replay_with_full_width_vertex_outputs() {
    use fgdb_delta_types::RelationId;
    use fgdb_types::{EId, VId};

    let ((), report) = run_async_under_lab(0x79a0_03c1, |root| async move {
        let (mut server, token, _) = served(&root, "subscription-components").await;
        let served = Arc::get_mut(server.databases.get_mut("test").unwrap()).unwrap();
        served
            .symbols
            .bind(fgdb_gql::GraphSymbolKind::Relation, "R", 1)
            .unwrap();
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let wide = 1_u128 << 100;
        let mut seed = vertex(1);
        for id in [2, 3, wide] {
            seed.create_vertex(VId(id), vec![], vec![]);
        }
        for (eid, from, to) in [(1, 1, 2), (2, 2, 3), (3, 1, 2), (4, wide, wide)] {
            seed.add_edge(EId(eid), VId(from), VId(to), vec![]);
        }
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), seed)
            .await
            .unwrap();
        let statement = request(
            "SUBSCRIBE TO CALL fnx.strongly_connected_components(\
             {relations:['R'],parallel:'collapse'}) \
             YIELD component AS root,vertex AS member \
             RETURN member,root SKIP 1 LIMIT 3",
        );
        let mut subscription = success(subscribe(&root, db, &token, &statement).await);
        assert_eq!(subscription.columns, ["member", "root"]);
        let initial = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert!(initial.batch.is_snapshot());
        assert_eq!(
            initial.entries,
            [2, 3, wide]
                .map(|id| (1, vec![WireValue::Vertex(id), WireValue::Vertex(id)]))
                .to_vec()
        );

        // Keep the baseline pending while two committed SCC changes accumulate.
        // The ordinary CALL's canonical page still includes the wide isolate.
        let mut cycle = fgdb::WriteBatch::new(RelationId(1));
        cycle.add_edge(EId(5), VId(3), VId(1), vec![]);
        let merged = db
            .db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), cycle)
            .await
            .unwrap();
        let mut split = fgdb::WriteBatch::new(RelationId(1));
        split.delete_edge(EId(2));
        let split = db
            .db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), split)
            .await
            .unwrap();
        let repeat = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert!(Arc::ptr_eq(&initial.batch, &repeat.batch));
        assert_eq!(initial.entries, repeat.entries);
        subscription
            .consumer
            .acknowledge(initial.batch.receipt())
            .unwrap();
        let row = |member, component| vec![WireValue::Vertex(member), WireValue::Vertex(component)];
        let first = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert_eq!(
            (first.batch.from(), first.batch.frontier()),
            (Some(initial.batch.frontier()), merged)
        );
        assert_eq!(
            first.entries,
            vec![
                (1, row(2, 1)),
                (-1, row(2, 2)),
                (1, row(3, 1)),
                (-1, row(3, 3)),
            ]
        );
        subscription
            .consumer
            .acknowledge(first.batch.receipt())
            .unwrap();
        let second = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert_eq!(
            (second.batch.from(), second.batch.frontier()),
            (Some(first.batch.frontier()), split)
        );
        assert_eq!(
            second.entries,
            vec![
                (-1, row(2, 1)),
                (1, row(2, 2)),
                (-1, row(3, 1)),
                (1, row(3, 3)),
            ]
        );
        subscription
            .consumer
            .acknowledge(second.batch.receipt())
            .unwrap();
        assert!(success(poll(&root, db, &token, &mut subscription).await).is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn component_subscription_scope_and_projection_refusals_precede_registration() {
    use fgdb_delta_types::{LabelId, RelationId};
    use fgdb_types::{EId, VId};
    use fgdb_warden::Scope;

    let ((), report) = run_async_under_lab(0x79a0_03c2, |root| async move {
        let (mut server, token, _) = served(&root, "subscription-component-scope").await;
        let served = Arc::get_mut(server.databases.get_mut("test").unwrap()).unwrap();
        served
            .symbols
            .bind(fgdb_gql::GraphSymbolKind::Relation, "R", 1)
            .unwrap();
        served
            .symbols
            .bind(fgdb_gql::GraphSymbolKind::Label, "L", 1)
            .unwrap();
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let statement = request(
            "SUBSCRIBE TO CALL fnx.connected_components(\
             {relations:['R'],parallel:'collapse'}) YIELD node,component",
        );
        let scoped = [
            token
                .attenuate(Restriction::Labels(Scope::only([LabelId(1)])))
                .unwrap(),
            token
                .attenuate(Restriction::Relations(Scope::only([RelationId(1)])))
                .unwrap(),
            token
                .attenuate(Restriction::Properties(Scope::only([])))
                .unwrap(),
        ];
        for hidden in [false, true] {
            if hidden {
                let mut seed = vertex(1);
                seed.create_vertex(VId(2), vec![], vec![]);
                seed.add_edge(EId(1), VId(1), VId(2), vec![]);
                db.db
                    .write(&root)
                    .await
                    .unwrap()
                    .write(&contexts.commit(), seed)
                    .await
                    .unwrap();
            }
            for capability in &scoped {
                refused(
                    subscribe(&root, db, capability, &statement).await,
                    ErrorCode::PermissionDenied,
                );
                assert_eq!(db.subscriptions.load(Ordering::Acquire), 0);
            }
        }
        for text in [
            "SUBSCRIBE TO CALL fnx.connected_components(\
             {relations:['R']}) YIELD node,component",
            "SUBSCRIBE TO CALL fnx.connected_components(\
             {relations:['R'],parallel:'collapse',labels:['L']}) YIELD node,component",
            "SUBSCRIBE TO CALL fnx.connected_components(\
             {relations:['R'],parallel:'collapse',direction:'directed'}) YIELD node,component",
            "SUBSCRIBE TO CALL fnx.connected_components(\
             {relations:['R'],parallel:'collapse',self_loops:'reject'}) YIELD node,component",
            "SUBSCRIBE TO CALL fnx.degree_centrality(\
             {relations:['R'],parallel:'collapse'}) YIELD node,score LIMIT 0",
        ] {
            refused(
                subscribe(&root, db, &token, &request(text)).await,
                ErrorCode::Statement,
            );
            assert_eq!(db.subscriptions.load(Ordering::Acquire), 0);
        }
        let tiny = token.attenuate(Restriction::MaxRows(1)).unwrap();
        refused(
            subscribe(&root, db, &tiny, &statement).await,
            ErrorCode::Budget,
        );
        assert_eq!(db.subscriptions.load(Ordering::Acquire), 0);
        let mut subscription = success(subscribe(&root, db, &token, &statement).await);
        assert_eq!(db.subscriptions.load(Ordering::Acquire), 1);
        // Without RETURN, the CALL exposes its canonical named schema.
        assert_eq!(subscription.columns, ["component", "node"]);
        // A later narrowed capability cannot read or consume a pending result.
        refused(
            poll(&root, db, &scoped[0], &mut subscription).await,
            ErrorCode::PermissionDenied,
        );
        let delivered = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert_eq!(
            delivered.entries,
            vec![
                (1, vec![WireValue::Vertex(1), WireValue::Vertex(1)]),
                (1, vec![WireValue::Vertex(1), WireValue::Vertex(2)]),
            ]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn clustering_subscription_replays_degree_only_scores_empty_ticks_and_canonical_pages() {
    use fgdb_delta_types::RelationId;
    use fgdb_types::{EId, VId};

    let ((), report) = run_async_under_lab(0x79a0_03d1, |root| async move {
        let (mut server, token, _) = served(&root, "subscription-clustering").await;
        Arc::get_mut(server.databases.get_mut("test").unwrap())
            .unwrap()
            .symbols
            .bind(fgdb_gql::GraphSymbolKind::Relation, "R", 1)
            .unwrap();
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let wide = 1_u128 << 100;
        let mut seed = vertex(1);
        for id in [2, 3, wide] {
            seed.create_vertex(VId(id), vec![], vec![]);
        }
        for (eid, a, b) in [(1, 1, 2), (2, 2, 3), (3, 3, 1), (4, wide, wide)] {
            seed.add_edge(EId(eid), VId(a), VId(b), vec![]);
        }
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), seed)
            .await
            .unwrap();
        let statement = request(
            "SUBSCRIBE TO CALL fnx.clustering_coefficient(\
             {relations:['R'],parallel:'collapse',self_loops:'keep'}) \
             YIELD score AS local,vertex AS member RETURN local,member SKIP 1 LIMIT 3",
        );
        let mut subscription = success(subscribe(&root, db, &token, &statement).await);
        assert_eq!(subscription.columns, ["local", "member"]);
        let row = |vertex, score| vec![WireValue::Float(score), WireValue::Vertex(vertex)];
        let initial = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert!(initial.batch.is_snapshot());
        // The CALL is canonically sorted in score,member order before RETURN.
        // Its zero-score wide isolate is skipped; all three triangle vertices remain.
        assert_eq!(
            initial.entries,
            vec![(1, row(1, 1.0)), (1, row(2, 1.0)), (1, row(3, 1.0))]
        );

        let mut expected = Vec::new();
        for tick in 0..4 {
            let mut change = fgdb::WriteBatch::new(RelationId(1));
            match tick {
                0 => {
                    change.add_edge(EId(5), VId(1), VId(wide), vec![]);
                }
                1 => {
                    change.add_edge(EId(6), VId(wide), VId(1), vec![]);
                }
                2 => {
                    change.delete_edge(EId(5));
                }
                _ => {
                    change.delete_vertex(VId(wide));
                }
            }
            let at = db
                .db
                .write(&root)
                .await
                .unwrap()
                .write(&contexts.commit(), change)
                .await
                .unwrap();
            expected.push(at);
        }
        let repeat = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert!(Arc::ptr_eq(&repeat.batch, &initial.batch));
        assert_eq!(repeat.entries, initial.entries);
        subscription
            .consumer
            .acknowledge(initial.batch.receipt())
            .unwrap();
        let mut after = initial.batch.frontier();
        for (tick, at) in expected.into_iter().enumerate() {
            let frame = success(poll(&root, db, &token, &mut subscription).await).unwrap();
            assert_eq!(
                (frame.batch.from(), frame.batch.frontier()),
                (Some(after), at)
            );
            match tick {
                0 => {
                    assert_eq!(
                        frame.entries,
                        vec![(1, row(1, 1.0 / 3.0)), (-1, row(1, 1.0))]
                    );
                    let WireValue::Float(score) = frame.entries[0].1[0] else {
                        panic!("wire score");
                    };
                    assert_eq!(score.to_bits(), (1.0_f64 / 3.0).to_bits());
                }
                1 | 2 => assert!(
                    frame.entries.is_empty(),
                    "remaining physical support preserves the degree"
                ),
                _ => {
                    // Removing the wide isolate's lifetime also removes the
                    // first canonical row; vertex 1 now falls outside the page.
                    assert_eq!(frame.entries, vec![(-1, row(1, 1.0 / 3.0))]);
                }
            }
            subscription
                .consumer
                .acknowledge(frame.batch.receipt())
                .unwrap();
            after = at;
        }
        assert!(success(poll(&root, db, &token, &mut subscription).await).is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn clustering_subscription_refuses_hidden_topology_before_registration_or_delivery() {
    use fgdb_delta_types::{LabelId, RelationId};
    use fgdb_types::{EId, VId};
    use fgdb_warden::Scope;

    let ((), report) = run_async_under_lab(0x79a0_03d2, |root| async move {
        let (mut server, token, _) = served(&root, "subscription-clustering-scope").await;
        let served = Arc::get_mut(server.databases.get_mut("test").unwrap()).unwrap();
        served
            .symbols
            .bind(fgdb_gql::GraphSymbolKind::Relation, "R", 1)
            .unwrap();
        served
            .symbols
            .bind(fgdb_gql::GraphSymbolKind::Label, "L", 1)
            .unwrap();
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let statement = request(
            "SUBSCRIBE TO CALL fnx.clustering_coefficient(\
             {relations:['R'],parallel:'collapse',self_loops:'drop'}) YIELD vertex,score",
        );
        let scoped = [
            token
                .attenuate(Restriction::Labels(Scope::only([LabelId(1)])))
                .unwrap(),
            token
                .attenuate(Restriction::Relations(Scope::only([RelationId(1)])))
                .unwrap(),
            token
                .attenuate(Restriction::Properties(Scope::only([])))
                .unwrap(),
        ];
        for hidden in [false, true] {
            if hidden {
                let mut seed = vertex(1);
                for id in [2, 3] {
                    seed.create_vertex(VId(id), vec![], vec![]);
                }
                for (id, a, b) in [(1, 1, 2), (2, 2, 3), (3, 3, 1)] {
                    seed.add_edge(EId(id), VId(a), VId(b), vec![]);
                }
                db.db
                    .write(&root)
                    .await
                    .unwrap()
                    .write(&contexts.commit(), seed)
                    .await
                    .unwrap();
            }
            for token in &scoped {
                refused(
                    subscribe(&root, db, token, &statement).await,
                    ErrorCode::PermissionDenied,
                );
                assert_eq!(db.subscriptions.load(Ordering::Acquire), 0);
            }
        }
        for text in [
            "SUBSCRIBE TO CALL fnx.clustering_coefficient({relations:['R'],self_loops:'drop'}) YIELD vertex,score",
            "SUBSCRIBE TO CALL fnx.clustering_coefficient({relations:['R'],parallel:'collapse',self_loops:'reject'}) YIELD vertex,score",
            "SUBSCRIBE TO CALL fnx.clustering_coefficient({relations:['R'],parallel:'collapse',labels:['L'],self_loops:'drop'}) YIELD vertex,score",
            "SUBSCRIBE TO CALL fnx.clustering_coefficient({relations:['R'],parallel:'collapse',direction:'directed',self_loops:'drop'}) YIELD vertex,score LIMIT 0",
            "SUBSCRIBE TO CALL fnx.directed_clustering_coefficient({relations:['R'],parallel:'collapse',self_loops:'drop'}) YIELD vertex,score LIMIT 0",
        ] {
            refused(
                subscribe(&root, db, &token, &request(text)).await,
                ErrorCode::Statement,
            );
            assert_eq!(db.subscriptions.load(Ordering::Acquire), 0);
        }
        let mut subscription = success(subscribe(&root, db, &token, &statement).await);
        assert_eq!(subscription.columns, ["score", "vertex"]);
        for token in &scoped {
            refused(
                poll(&root, db, token, &mut subscription).await,
                ErrorCode::PermissionDenied,
            );
        }
        let initial = success(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert!(initial.batch.is_snapshot());
        assert_eq!(
            initial.entries,
            [1, 2, 3]
                .map(|vertex| (1, vec![WireValue::Float(1.0), WireValue::Vertex(vertex)]))
                .to_vec()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
