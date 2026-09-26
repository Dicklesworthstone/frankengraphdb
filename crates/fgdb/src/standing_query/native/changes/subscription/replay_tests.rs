use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LimbLimit, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

const LIMBS: LimbLimit = LimbLimit::new(4);
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32])
}
fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, value) in [(1, 3), (2, 3), (3, 7)] {
        batch.create_vertex(
            VId(id),
            vec![],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
        );
    }
    batch
}
fn property(id: u128, key: u64, value: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    batch.set_vertex_property(
        VId(id),
        PropertyKeyId(key),
        Some(CanonicalScalar::Int(value)),
    );
    batch
}
fn copied(rows: &ZSet<Vec<QueryValue>>) -> ZSet<Vec<QueryValue>> {
    rows.checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap()
}

#[test]
fn independent_consumers_share_payloads_and_replay_every_delayed_tick() {
    let ((), report) = run_async_under_lab(0x006d_de71, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let start = db.write(&commit, seed()).await.unwrap();
        let params = GqlParameters::new();
        let mut first = db
            .subscribe_native(
                &cx,
                "SUBSCRIBE TO MATCH (n) RETURN n.p AS p",
                &params,
                resolve,
                policy(),
            )
            .unwrap();
        assert_eq!(
            first
                .enable_replay(&mut db, &cx, 16, 1000, 100000, policy())
                .unwrap(),
            start
        );
        let count = db.standing_queries.len();
        let mut second = first.fork(&db, &cx).unwrap();
        assert_eq!(db.standing_queries.len(), count);
        assert_eq!(
            first.replay_handle().unwrap().index,
            second.replay_handle().unwrap().index
        );
        let baseline = first.poll(&db, &cx, policy()).unwrap().unwrap();
        let other_baseline = second.poll(&db, &cx, policy()).unwrap().unwrap();
        let mut bags = [copied(baseline.rows()), copied(other_baseline.rows())];
        first.acknowledge(baseline.receipt()).unwrap();
        second.acknowledge(other_baseline.receipt()).unwrap();

        let mut update = WriteBatch::new(RelationId(1));
        update.delete_vertex(VId(1));
        let at = db.write(&commit, update).await.unwrap();
        let mut cuts = vec![at];
        let mut expected = vec![
            db.standing_native_bag(&cx, first.handle(), policy())
                .unwrap()
                .1,
        ];
        let pending = first.poll(&db, &cx, policy()).unwrap().unwrap();
        for change in [property(3, 1, 2), property(2, 99, 4)] {
            cuts.push(db.write(&commit, change).await.unwrap());
            expected.push(
                db.standing_native_bag(&cx, first.handle(), policy())
                    .unwrap()
                    .1,
            );
        }
        // Pending delivery is pinned even though the source has advanced twice.
        assert!(Arc::ptr_eq(
            &pending,
            &first
                .poll(&db, &cx, GqlQueryPolicy::new(0, 0, 0, 0))
                .unwrap()
                .unwrap()
        ));
        let mut after = start;
        for (tick, at) in cuts.iter().enumerate() {
            // No payload copies: this tiny allowance admits a retained handle.
            let delivery = GqlQueryPolicy::new(0, 100, 1, 1);
            let a = first.poll(&db, &cx, delivery).unwrap().unwrap();
            let b = second.poll(&db, &cx, delivery).unwrap().unwrap();
            assert_eq!((a.from(), a.frontier()), (Some(after), *at));
            assert_eq!((b.from(), b.frontier()), (Some(after), *at));
            assert!(!Arc::ptr_eq(&a, &b)); // different subscription receipts
            assert!(Arc::ptr_eq(&a.rows, &b.rows)); // same immutable payload
            let retained = db
                .standing_replay_next(&cx, first.replay_handle().unwrap(), after, delivery)
                .unwrap()
                .unwrap();
            assert!(Arc::ptr_eq(&a.rows, &retained.shared_rows()));
            assert!(matches!(
                first.acknowledge(b.receipt()),
                Err(SubscriptionError::InvalidReceipt)
            ));
            for (bag, frame) in bags.iter_mut().zip([&a, &b]) {
                bag.integrate(frame.rows(), LIMBS, &mut |_| Ok::<_, ()>(()))
                    .unwrap();
                assert_eq!(&*bag, &expected[tick]);
            }
            if tick == 2 {
                assert!(a.rows().is_empty());
            }
            first.acknowledge(a.receipt()).unwrap();
            assert_eq!(second.acknowledged_frontier(), Some(after));
            second.acknowledge(b.receipt()).unwrap();
            after = *at;
        }
        assert!(first.poll(&db, &cx, policy()).unwrap().is_none());
        first.close();
        let at = db.write(&commit, property(2, 99, 5)).await.unwrap();
        let frame = second.poll(&db, &cx, policy()).unwrap().unwrap();
        assert_eq!(frame.frontier(), at);
        assert!(frame.rows().is_empty());
        second.acknowledge(frame.receipt()).unwrap();
        assert!(matches!(
            first.poll(&db, &cx, policy()),
            Err(SubscriptionError::Closed)
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn evicted_pending_frame_replays_but_a_missing_successor_requires_rebaseline() {
    let ((), report) = run_async_under_lab(0x006d_de72, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let start = db.write(&commit, seed()).await.unwrap();
        let params = GqlParameters::new();
        let mut sub = db
            .subscribe_native(
                &cx,
                "SUBSCRIBE TO MATCH (n) RETURN n.p AS p",
                &params,
                resolve,
                policy(),
            )
            .unwrap();
        sub.enable_replay(&mut db, &cx, 2, 1000, 100000, policy())
            .unwrap();
        let baseline = sub.poll(&db, &cx, policy()).unwrap().unwrap();
        sub.acknowledge(baseline.receipt()).unwrap();
        let first = db.write(&commit, property(2, 1, 8)).await.unwrap();
        let pending = sub.poll(&db, &cx, policy()).unwrap().unwrap();
        let mut current = first;
        for value in 9..13 {
            current = db.write(&commit, property(2, 1, value)).await.unwrap();
        }
        assert!(Arc::ptr_eq(
            &pending,
            &sub.poll(&db, &cx, policy()).unwrap().unwrap()
        ));
        assert_eq!(sub.acknowledged_frontier(), Some(start));
        sub.acknowledge(pending.receipt()).unwrap();
        assert!(
            matches!(sub.poll(&db, &cx, policy()), Err(SubscriptionError::Query(
            StandingQueryError::ReplayGap { after, frontier, .. }
        )) if after == first && frontier == current)
        );
        assert_eq!(sub.acknowledged_frontier(), Some(first));
        sub.restart_from_current().unwrap();
        let replacement = sub.poll(&db, &cx, policy()).unwrap().unwrap();
        assert!(replacement.is_snapshot());
        assert_eq!(replacement.frontier(), current);
        assert_eq!(
            replacement.rows(),
            &db.standing_native_bag(&cx, sub.handle(), policy())
                .unwrap()
                .1
        );
        assert!(matches!(
            sub.acknowledge(pending.receipt()),
            Err(SubscriptionError::InvalidReceipt)
        ));
        sub.acknowledge(replacement.receipt()).unwrap();
        let at = db.write(&commit, property(2, 99, 17)).await.unwrap();
        let delta = sub.poll(&db, &cx, policy()).unwrap().unwrap();
        assert_eq!((delta.from(), delta.frontier()), (Some(current), at));
        assert!(delta.rows().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn attachment_refusals_do_not_leak_sinks_or_fabricate_old_history() {
    let ((), report) = run_async_under_lab(0x006d_de73, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let start = db.write(&commit, seed()).await.unwrap();
        let params = GqlParameters::new();
        let mut sub = db
            .subscribe_native(
                &cx,
                "SUBSCRIBE TO MATCH (n) RETURN n.p AS p",
                &params,
                resolve,
                policy(),
            )
            .unwrap();
        let count = db.standing_queries.len();
        assert!(matches!(
            sub.enable_replay(&mut db, &cx, 0, 100, 1000, policy()),
            Err(SubscriptionError::Query(
                StandingQueryError::InvalidReplayLimits
            ))
        ));
        assert!(sub.replay_handle().is_none());
        assert_eq!(db.standing_queries.len(), count);
        let baseline = sub.poll(&db, &cx, policy()).unwrap().unwrap();
        assert!(matches!(
            sub.enable_replay(&mut db, &cx, 8, 100, 1000, policy()),
            Err(SubscriptionError::Unacknowledged)
        ));
        assert_eq!(db.standing_queries.len(), count);
        assert!(Arc::ptr_eq(
            &baseline,
            &sub.poll(&db, &cx, policy()).unwrap().unwrap()
        ));
        sub.acknowledge(baseline.receipt()).unwrap();
        let at = db.write(&commit, property(2, 1, 8)).await.unwrap();
        assert!(
            matches!(sub.enable_replay(&mut db, &cx, 8, 100, 1000, policy()),
            Err(SubscriptionError::Query(StandingQueryError::DeltaUnavailable { from, frontier }))
            if from == start && frontier == at)
        );
        assert_eq!(db.standing_queries.len(), count);
        // Catch up on the existing one-tick lane, THEN start retention at that cut.
        let delta = sub.poll(&db, &cx, policy()).unwrap().unwrap();
        sub.acknowledge(delta.receipt()).unwrap();
        assert_eq!(
            sub.enable_replay(&mut db, &cx, 8, 100, 1000, policy())
                .unwrap(),
            at
        );
        assert_eq!(db.standing_queries.len(), count + 1);
        assert!(matches!(
            sub.enable_replay(&mut db, &cx, 8, 100, 1000, policy()),
            Err(SubscriptionError::ReplayAlreadyEnabled)
        ));
        assert_eq!(db.standing_queries.len(), count + 1);
        let mut foreign = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(
            sub.fork(&foreign, &cx),
            Err(SubscriptionError::Query(StandingQueryError::ForeignHandle))
        ));
        assert!(matches!(
            sub.poll(&foreign, &cx, policy()),
            Err(SubscriptionError::Query(StandingQueryError::ForeignHandle))
        ));
        let mut other = db.open_standing_subscription(&cx, sub.handle()).unwrap();
        assert!(matches!(
            other.enable_replay(&mut foreign, &cx, 8, 100, 1000, policy()),
            Err(SubscriptionError::Query(StandingQueryError::ForeignHandle))
        ));
        assert!(other.replay_handle().is_none());
        assert_eq!(foreign.standing_queries.len(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn delivery_refusal_and_shared_sink_rebuild_preserve_acknowledgements() {
    let ((), report) = run_async_under_lab(0x006d_de74, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let start = db.write(&commit, seed()).await.unwrap();
        let params = GqlParameters::new();
        let mut sub = db
            .subscribe_native(
                &cx,
                "SUBSCRIBE TO MATCH (n) RETURN COUNT(*) AS c,SUM(n.p) AS s",
                &params,
                resolve,
                policy(),
            )
            .unwrap();
        sub.enable_replay(&mut db, &cx, 8, 100, 10000, policy())
            .unwrap();
        let baseline = sub.poll(&db, &cx, policy()).unwrap().unwrap();
        sub.acknowledge(baseline.receipt()).unwrap();
        let at = db.write(&commit, property(2, 1, 8)).await.unwrap();
        for delivery in [
            GqlQueryPolicy::new(0, 0, 1000, 1000),
            GqlQueryPolicy::new(0, 1000, 0, 1000),
            GqlQueryPolicy::new(0, 1000, 1000, 0),
        ] {
            assert!(matches!(
                sub.poll(&db, &cx, delivery),
                Err(SubscriptionError::Query(StandingQueryError::Delivery(_)))
            ));
            assert_eq!(sub.acknowledged_frontier(), Some(start));
            assert!(sub.pending.is_none());
        }
        let pending = sub.poll(&db, &cx, policy()).unwrap().unwrap();
        assert_eq!(pending.frontier(), at);
        db.rebuild_standing_query(&cx, sub.replay_handle().unwrap(), policy())
            .unwrap();
        assert!(Arc::ptr_eq(
            &pending,
            &sub.poll(&db, &cx, policy()).unwrap().unwrap()
        ));
        sub.acknowledge(pending.receipt()).unwrap();
        assert!(sub.poll(&db, &cx, policy()).unwrap().is_none());
        let next = db.write(&commit, property(2, 1, 9)).await.unwrap();
        // A rebuild discards undelivered history, never treating it as empty.
        db.rebuild_standing_query(&cx, sub.replay_handle().unwrap(), policy())
            .unwrap();
        assert!(matches!(
            sub.poll(&db, &cx, policy()),
            Err(SubscriptionError::Query(
                StandingQueryError::ReplayGap { .. }
            ))
        ));
        assert_eq!(sub.acknowledged_frontier(), Some(at));
        sub.restart_from_current().unwrap();
        let baseline = sub.poll(&db, &cx, policy()).unwrap().unwrap();
        assert_eq!(baseline.frontier(), next);
        assert!(baseline.is_snapshot());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn replay_failure_does_not_rollback_writes_or_erase_an_already_delivered_frame() {
    let ((), report) = run_async_under_lab(0x006d_de75, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let start = db.write(&commit, seed()).await.unwrap();
        let params = GqlParameters::new();
        let mut sub = db
            .subscribe_native(
                &cx,
                "SUBSCRIBE TO MATCH (n) RETURN n.p AS p",
                &params,
                resolve,
                policy(),
            )
            .unwrap();
        sub.enable_replay(&mut db, &cx, 8, 0, 1000, policy())
            .unwrap();
        let baseline = sub.poll(&db, &cx, policy()).unwrap().unwrap();
        // Retention admits empty ticks, but the next changed tuple cannot fit.
        let at = db.write(&commit, property(2, 1, 8)).await.unwrap();
        assert_eq!(db.frontier().unwrap(), at);
        assert!(db.standing_native_bag(&cx, sub.handle(), policy()).is_ok());
        assert!(Arc::ptr_eq(
            &baseline,
            &sub.poll(&db, &cx, policy()).unwrap().unwrap()
        ));
        sub.acknowledge(baseline.receipt()).unwrap();
        assert_eq!(sub.acknowledged_frontier(), Some(start));
        assert!(matches!(
            sub.poll(&db, &cx, policy()),
            Err(SubscriptionError::Query(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::ResultBudget,
                ..
            }))
        ));
        assert!(sub.pending.is_none());
        db.rebuild_standing_query(&cx, sub.replay_handle().unwrap(), policy())
            .unwrap();
        sub.restart_from_current().unwrap();
        let replacement = sub.poll(&db, &cx, policy()).unwrap().unwrap();
        sub.acknowledge(replacement.receipt()).unwrap();
        let next = db.write(&commit, property(2, 99, 11)).await.unwrap();
        let empty = sub.poll(&db, &cx, policy()).unwrap().unwrap();
        assert_eq!((empty.from(), empty.frontier()), (Some(at), next));
        assert!(empty.rows().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
