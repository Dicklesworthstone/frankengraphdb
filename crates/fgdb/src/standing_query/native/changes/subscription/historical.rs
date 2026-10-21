//! Historical bootstrap reuses the ordinary one-pending-frame/receipt lifecycle.

use super::*;

impl<V: Vfs + Clone> Database<V> {
    /// Start a new consumer with a complete baseline at a retained replay cut.
    /// The replay handle determines the source; callers cannot pair unrelated
    /// query state and history. Reconstruction, source binding and baseline
    /// publication share this immutable database borrow. No new maintained
    /// circuit or replay sink is registered, and no graph query is executed.
    ///
    /// Unlike open_standing_subscription, this performs baseline reconstruction
    /// NOW and returns that frame together with the consumer. Apply it as a
    /// replacement bag, then acknowledge its receipt. Until ACK, either poll
    /// mode redelivers the same immutable allocation, never a newer baseline.
    /// After ACK, ordinary poll delivers the first retained successor; the
    /// coalesced mode can deliver a net retained prefix. Empty baselines and
    /// empty ticks still require their own acknowledgement.
    ///
    /// `policy` governs the entire reconstruction as standing_replay_snapshot
    /// documents. Each later new frame uses its poll policy. Retries share the
    /// previously admitted frame, as for ordinary subscription delivery. Failed
    /// opening returns no consumer, receipt or partial baseline and changes no
    /// registry, replay retention, existing consumer or maintenance allowance.
    ///
    /// This adds no retention lease: writes may evict required successors while
    /// the baseline is pending. Its ACK still acknowledges that exact baseline,
    /// but subsequent catch-up then reports ReplayGap, never silently skips to
    /// current state. Explicit restart_from_current retains its existing reset
    /// semantics. Source health and cancellation remain live on redelivery.
    ///
    /// The consumer owns no private registration; closing it cannot unregister
    /// shared source/history nodes. Fork starts a fresh CURRENT baseline, not a
    /// clone of this historical delivery position. Receipts remain in-process,
    /// not serializable resume authority or crash-safe exactly-once effects.
    pub fn open_standing_replay_subscription_at(
        &self,
        cx: &QueryCx,
        replay: &StandingQueryHandle,
        at: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<(NativeSubscription, Arc<SubscriptionBatch>), SubscriptionError> {
        let StandingQuery::Replay(state) = self.admitted_standing_query(cx, replay)? else {
            return Err(StandingQueryError::Unsupported.into());
        };
        let mut consumer = self.open_standing_subscription(cx, &state.source)?;
        let rows = self.standing_replay_snapshot(cx, replay, at, policy)?;
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        consumer.replay = Some(replay.clone());
        // Fresh owner, serial zero, no ACK and no pending frame. Reuse the
        // existing publisher rather than minting a caller-selected ACK token.
        let baseline = consumer.publish(at, rows)?;
        Ok((consumer, baseline))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DatabaseKeys, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind};
    use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};
    use std::num::NonZeroUsize;

    fn policy() -> GqlQueryPolicy {
        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
    }
    fn delivery(rows: u64) -> GqlQueryPolicy {
        GqlQueryPolicy::new(0, rows, 10_000_000, 10_000_000)
    }
    fn keys() -> DatabaseKeys {
        DatabaseKeys::new([0x51; 32], DatabaseSecurityNamespaceId([0x52; 32]), [0x53; 32])
    }
    fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        }
    }
    fn seed() -> WriteBatch {
        let mut batch = WriteBatch::new(RelationId(1));
        for (id, p) in [(1, 3), (2, 3), (3, 7)] {
            batch.create_vertex(VId(id), vec![], vec![(PropertyKeyId(1), CanonicalScalar::Int(p))]);
        }
        batch
    }
    fn register<V: Vfs + Clone>(
        db: &mut Database<V>,
        cx: &QueryCx,
        ticks: usize,
    ) -> (StandingQueryHandle, StandingQueryHandle) {
        let source = db.register_standing_native(
            cx, "MATCH (n) RETURN n.p AS p", &GqlParameters::new(), resolve, policy(),
        ).unwrap();
        let replay = db.register_standing_replay(cx, &source, ticks, 1000, 100_000, policy()).unwrap();
        (source, replay)
    }

    #[test]
    fn historical_baseline_ack_then_atomic_or_coalesced_catchup_preserves_exact_bags() {
        let ((), report) = run_async_under_lab(0x71a0_5201, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let first = db.write(&commit, seed()).await.unwrap();
            let (source, replay) = register(&mut db, &cx, 8);
            let (_, old) = db.standing_native_bag(&cx, &source, delivery(2)).unwrap();
            let mut edit = WriteBatch::new(RelationId(1));
            edit.delete_vertex(VId(1));
            let second = db.write(&commit, edit).await.unwrap();
            let mut edit = WriteBatch::new(RelationId(1));
            edit.set_vertex_property(VId(2), PropertyKeyId(99), Some(CanonicalScalar::Int(1)));
            let third = db.write(&commit, edit).await.unwrap();
            let window = db.standing_replay_window(&cx, &replay).unwrap();
            let (mut atomic, baseline) = db.open_standing_replay_subscription_at(&cx, &replay, first, delivery(2)).unwrap();
            let (mut net, net_baseline) = db.open_standing_replay_subscription_at(&cx, &replay, first, delivery(2)).unwrap();
            assert_eq!(baseline.rows(), &old);
            assert_eq!(baseline.frontier(), first);
            assert!(baseline.is_snapshot());
            assert_eq!(atomic.acknowledged_frontier(), None);
            assert_eq!(db.standing_replay_window(&cx, &replay).unwrap(), window);
            assert!(matches!(atomic.acknowledge(net_baseline.receipt()), Err(SubscriptionError::InvalidReceipt)));
            assert!(Arc::ptr_eq(&baseline, &atomic.poll(&db, &cx, delivery(0)).unwrap().unwrap()));
            assert!(Arc::ptr_eq(&baseline, &atomic.poll_coalesced(&db, &cx, NonZeroUsize::new(8).unwrap(), delivery(0)).unwrap().unwrap()));
            // Neither constructor nor its returned consumer borrows the writer.
            let mut edit = WriteBatch::new(RelationId(1));
            edit.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(9)));
            let fourth = db.write(&commit, edit).await.unwrap();
            let mut result = old.checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
            assert_eq!(atomic.acknowledge(baseline.receipt()).unwrap(), first);
            assert_eq!(atomic.acknowledge(baseline.receipt()).unwrap(), first);
            let mut from = first;
            for at in [second, third, fourth] {
                let frame = atomic.poll(&db, &cx, delivery(100)).unwrap().unwrap();
                assert_eq!(frame.from(), Some(from));
                assert_eq!(frame.frontier(), at);
                assert!(!frame.is_snapshot());
                if at == third { assert!(frame.rows().is_empty()); }
                assert!(Arc::ptr_eq(&frame, &atomic.poll(&db, &cx, delivery(0)).unwrap().unwrap()));
                result.integrate(frame.rows(), LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
                assert_eq!(atomic.acknowledge(frame.receipt()).unwrap(), at);
                from = at;
            }
            assert!(atomic.poll(&db, &cx, delivery(0)).unwrap().is_none());
            let (_, current) = db.standing_native_bag(&cx, &source, delivery(100)).unwrap();
            assert_eq!(result, current);
            net.acknowledge(net_baseline.receipt()).unwrap();
            let delta = net.poll_coalesced(&db, &cx, NonZeroUsize::new(8).unwrap(), delivery(100)).unwrap().unwrap();
            assert_eq!(delta.from(), Some(first));
            assert_eq!(delta.frontier(), fourth);
            let mut result = old;
            result.integrate(delta.rows(), LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
            assert_eq!(result, current);
            net.acknowledge(delta.receipt()).unwrap();
            assert!(net.poll(&db, &cx, delivery(0)).unwrap().is_none());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn eviction_after_open_keeps_pending_baseline_but_cannot_forge_catchup() {
        let ((), report) = run_async_under_lab(0x71a0_5202, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let first = db.write(&commit, seed()).await.unwrap();
            let (source, replay) = register(&mut db, &cx, 1);
            let (mut sub, baseline) = db.open_standing_replay_subscription_at(&cx, &replay, first, delivery(2)).unwrap();
            let mut latest = first;
            for value in [8, 9] {
                let mut edit = WriteBatch::new(RelationId(1));
                edit.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(value)));
                latest = db.write(&commit, edit).await.unwrap();
            }
            assert!(db.standing_replay_window(&cx, &replay).unwrap().retained_after > first);
            assert!(Arc::ptr_eq(&baseline, &sub.poll(&db, &cx, delivery(0)).unwrap().unwrap()));
            sub.acknowledge(baseline.receipt()).unwrap();
            assert!(matches!(sub.poll(&db, &cx, delivery(100)), Err(SubscriptionError::Query(StandingQueryError::ReplayGap { .. }))));
            assert_eq!(sub.acknowledged_frontier(), Some(first));
            assert!(sub.pending.is_none());
            sub.restart_from_current().unwrap();
            assert!(matches!(sub.acknowledge(baseline.receipt()), Err(SubscriptionError::InvalidReceipt)));
            let replacement = sub.poll(&db, &cx, delivery(100)).unwrap().unwrap();
            assert!(replacement.is_snapshot());
            assert_eq!(replacement.frontier(), latest);
            assert_eq!(replacement.rows(), &db.standing_native_bag(&cx, &source, delivery(100)).unwrap().1);
            let mut fork = sub.fork(&db, &cx).unwrap();
            let fork_baseline = fork.poll(&db, &cx, delivery(100)).unwrap().unwrap();
            assert_eq!(fork_baseline.frontier(), latest);
            assert!(matches!(fork.acknowledge(replacement.receipt()), Err(SubscriptionError::InvalidReceipt)));
            sub.close();
            assert!(matches!(sub.acknowledge(replacement.receipt()), Err(SubscriptionError::Closed)));
            fork.acknowledge(fork_baseline.receipt()).unwrap();
            assert!(fork.poll(&db, &cx, delivery(0)).unwrap().is_none());
            assert!(db.standing_native_bag(&cx, &source, delivery(100)).is_ok());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn empty_origin_baselines_and_opening_refusals_preserve_existing_consumers() {
        let ((), report) = run_async_under_lab(0x71a0_5203, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let (source, replay) = register(&mut db, &cx, 8);
            let first = db.write(&commit, seed()).await.unwrap();
            let (mut sub, empty) = db.open_standing_replay_subscription_at(&cx, &replay, CommitSeq::ORIGIN, delivery(0)).unwrap();
            assert!(empty.rows().is_empty());
            assert!(empty.is_snapshot());
            assert_eq!(empty.frontier(), CommitSeq::ORIGIN);
            let window = db.standing_replay_window(&cx, &replay).unwrap();
            assert!(matches!(db.open_standing_replay_subscription_at(&cx, &source, first, delivery(100)), Err(SubscriptionError::Query(StandingQueryError::Unsupported))));
            assert!(matches!(db.open_standing_replay_subscription_at(&cx, &replay, CommitSeq(u64::MAX), delivery(100)), Err(SubscriptionError::Query(StandingQueryError::ReplayGap { .. }))));
            assert!(db.open_standing_replay_subscription_at(&cx, &replay, first, GqlQueryPolicy::new(0, 0, 0, 0)).is_err());
            let foreign = Database::open_memory(&commit, keys()).await.unwrap();
            assert!(matches!(foreign.open_standing_replay_subscription_at(&cx, &replay, first, delivery(100)), Err(SubscriptionError::Query(StandingQueryError::ForeignHandle))));
            assert_eq!(db.standing_replay_window(&cx, &replay).unwrap(), window);
            assert!(Arc::ptr_eq(&empty, &sub.poll(&db, &cx, delivery(0)).unwrap().unwrap()));
            assert_eq!(sub.acknowledged_frontier(), None);
            sub.acknowledge(empty.receipt()).unwrap();
            let first_delta = sub.poll(&db, &cx, delivery(2)).unwrap().unwrap();
            assert_eq!(first_delta.from(), Some(CommitSeq::ORIGIN));
            assert_eq!(first_delta.frontier(), first);
            assert!(!first_delta.is_snapshot());
            assert_eq!(first_delta.rows(), &db.standing_native_bag(&cx, &source, delivery(2)).unwrap().1);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
