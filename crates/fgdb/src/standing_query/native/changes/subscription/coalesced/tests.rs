use super::*;
use crate::{DatabaseKeys, MemVfs, QueryResult, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};
use std::collections::BTreeMap;

type Bag = BTreeMap<Vec<QueryValue>, i128>;
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([71; 32], DatabaseSecurityNamespaceId([72; 32]), [73; 32])
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 20_000_000, 20_000_000)
}
fn delivery(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(0, rows, 20_000_000, 20_000_000)
}
fn ticks(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, value) in [(1, 1), (2, 2), (u128::MAX, 3)] {
        batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(value))]);
    }
    batch
}
fn edit(id: u128, value: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    batch.set_vertex_property(VId(id), P, Some(CanonicalScalar::Int(value)));
    batch
}
fn plain(rows: &ZSet<Vec<QueryValue>>) -> Bag {
    rows.iter()
        .map(|(row, weight)| (row.clone(), weight.to_i128().unwrap()))
        .collect()
}
fn snapshot(db: &Database<MemVfs>, cx: &QueryCx, text: &str) -> Bag {
    let QueryResult::Rows { rows, .. } = db
        .query(cx, text, &GqlParameters::new(), symbols, policy())
        .unwrap()
    else {
        panic!("native read must return rows");
    };
    let mut bag = Bag::new();
    for row in rows {
        *bag.entry(row).or_default() += 1;
    }
    bag
}
fn integrate(old: &mut Bag, delta: &ZSet<Vec<QueryValue>>) {
    for (row, weight) in delta.iter() {
        *old.entry(row.clone()).or_default() += weight.to_i128().unwrap();
    }
    old.retain(|_, weight| *weight != 0);
    assert!(old.values().all(|weight| *weight > 0));
}
fn open(
    db: &mut Database<MemVfs>,
    cx: &QueryCx,
    text: &str,
    retained: usize,
) -> NativeSubscription {
    let handle = db
        .register_standing_native(cx, text, &GqlParameters::new(), symbols, policy())
        .unwrap();
    let mut sub = db.open_standing_subscription(cx, &handle).unwrap();
    sub.enable_replay(db, cx, retained, 10_000, 1_000_000, policy())
        .unwrap();
    sub
}

#[test]
fn slow_consumers_integrate_bounded_net_ranges_to_each_exact_native_answer() {
    let ((), report) = run_async_under_lab(0x7263_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.write(&commit, seed()).await.unwrap();
        let texts = [
            "MATCH (n) RETURN n.p AS p",
            "MATCH (n) RETURN n AS id,n.p AS p",
            "MATCH (n) RETURN COUNT(*) AS c,MIN(n.p) AS m",
            "MATCH (n) RETURN DISTINCT COUNT(*) AS c GROUP BY n.p ORDER BY c DESC LIMIT 2",
            "MATCH (n) WITH n.p AS p RETURN AVG(p) AS a,SUM(p) AS s",
            "MATCH (n) RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p",
        ];
        let mut consumers = Vec::new();
        let mut snapshots = vec![
            texts
                .iter()
                .map(|text| snapshot(&db, &cx, text))
                .collect::<Vec<_>>(),
        ];
        for (index, text) in texts.iter().enumerate() {
            let mut sub = open(&mut db, &cx, text, 8);
            let initial = sub
                .poll_coalesced(&db, &cx, ticks(2), delivery(100))
                .unwrap()
                .unwrap();
            assert!(initial.is_snapshot());
            assert_eq!(initial.frontier(), basis);
            assert_eq!(plain(initial.rows()), snapshots[0][index]);
            sub.acknowledge(initial.receipt()).unwrap();
            consumers.push((sub, plain(initial.rows())));
        }
        for step in 0..6 {
            let batch = match step {
                0 => edit(1, 4),
                1 => edit(1, 1),
                2 => {
                    let mut batch = WriteBatch::new(RelationId(1));
                    batch.create_vertex(VId(7), vec![], vec![(P, CanonicalScalar::Int(2))]);
                    batch
                }
                3 => {
                    let mut batch = WriteBatch::new(RelationId(1));
                    batch.delete_vertex(VId(7));
                    batch
                }
                4 => edit(2, 9),
                _ => {
                    let mut batch = WriteBatch::new(RelationId(1));
                    batch.set_vertex_property(
                        VId(1),
                        PropertyKeyId(99),
                        Some(CanonicalScalar::Int(1)),
                    );
                    batch
                }
            };
            db.write(&commit, batch).await.unwrap();
            snapshots.push(texts.iter().map(|text| snapshot(&db, &cx, text)).collect());
        }
        let current = db.frontier().unwrap();
        for (index, (sub, accepted)) in consumers.iter_mut().enumerate() {
            let mut after = basis;
            let mut frames = 0;
            while let Some(frame) = sub
                .poll_coalesced(
                    &db,
                    &cx,
                    ticks(2),
                    delivery(if frames < 2 { 0 } else { 100 }),
                )
                .unwrap()
            {
                assert_eq!(frame.from(), Some(after));
                assert_eq!(frame.frontier().0 - after.0, 2);
                assert!(!frame.is_snapshot());
                if frames < 2 {
                    assert!(frame.rows().is_empty());
                }
                // A mode change, a smaller bound and a smaller delivery policy
                // cannot replace or split an already accepted range/receipt.
                let retry = sub
                    .poll_coalesced(&db, &cx, ticks(1), delivery(0))
                    .unwrap()
                    .unwrap();
                assert!(Arc::ptr_eq(&frame, &retry));
                assert!(Arc::ptr_eq(
                    &frame,
                    &sub.poll(&db, &cx, delivery(0)).unwrap().unwrap()
                ));
                integrate(accepted, frame.rows());
                let offset = usize::try_from(frame.frontier().0 - basis.0).unwrap();
                assert_eq!(*accepted, snapshots[offset][index], "{}", texts[index]);
                assert_eq!(sub.acknowledged_frontier(), Some(after));
                after = sub.acknowledge(frame.receipt()).unwrap();
                assert_eq!(sub.acknowledge(frame.receipt()).unwrap(), after);
                frames += 1;
            }
            assert_eq!(frames, 3);
            assert_eq!(after, current);
            assert_eq!(*accepted, snapshot(&db, &cx, texts[index]));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn quota_refusal_and_eviction_cannot_rewrite_a_pending_range_or_its_receipt() {
    let ((), report) = run_async_under_lab(0x7263_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let mut sub = open(&mut db, &cx, "MATCH (n) RETURN n.p AS p", 2);
        let initial = sub.poll(&db, &cx, delivery(100)).unwrap().unwrap();
        let basis = sub.acknowledge(initial.receipt()).unwrap();
        db.write(&commit, edit(1, 8)).await.unwrap();
        let end = db.write(&commit, edit(2, 9)).await.unwrap();
        let serial = sub.serial;
        for budget in [
            delivery(0),
            GqlQueryPolicy::new(0, 100, 0, 1000),
            GqlQueryPolicy::new(0, 100, 1000, 0),
        ] {
            assert!(matches!(
                sub.poll_coalesced(&db, &cx, ticks(2), budget),
                Err(SubscriptionError::Query(StandingQueryError::Delivery(_)))
            ));
            assert_eq!(sub.serial, serial);
            assert_eq!(sub.acknowledged_frontier(), Some(basis));
            assert!(sub.pending.is_none());
        }
        let pending = sub
            .poll_coalesced(&db, &cx, ticks(2), delivery(4))
            .unwrap()
            .unwrap();
        assert_eq!(pending.from(), Some(basis));
        assert_eq!(pending.frontier(), end);
        assert_eq!(pending.rows().len(), 4);
        let mut fork = sub.fork(&db, &cx).unwrap();
        let foreign = fork
            .poll_coalesced(&db, &cx, ticks(2), delivery(100))
            .unwrap()
            .unwrap();
        assert!(matches!(
            sub.acknowledge(foreign.receipt()),
            Err(SubscriptionError::InvalidReceipt)
        ));
        assert!(Arc::ptr_eq(sub.pending.as_ref().unwrap(), &pending));
        for value in 10..14 {
            db.write(&commit, edit(1, value)).await.unwrap();
        }
        let replay = sub.replay_handle().unwrap();
        assert!(
            db.standing_replay_window(&cx, replay)
                .unwrap()
                .retained_after
                > end
        );
        assert!(Arc::ptr_eq(
            &pending,
            &sub.poll_coalesced(&db, &cx, ticks(1), delivery(0))
                .unwrap()
                .unwrap()
        ));
        assert_eq!(sub.acknowledge(initial.receipt()).unwrap(), basis);
        assert!(Arc::ptr_eq(sub.pending.as_ref().unwrap(), &pending));
        sub.acknowledge(pending.receipt()).unwrap();
        assert!(matches!(
            sub.poll_coalesced(&db, &cx, ticks(2), delivery(100)),
            Err(SubscriptionError::Query(
                StandingQueryError::ReplayGap { .. }
            ))
        ));
        assert_eq!(sub.acknowledged_frontier(), Some(end));
        assert!(sub.pending.is_none());
        sub.restart_from_current().unwrap();
        assert!(matches!(
            sub.acknowledge(pending.receipt()),
            Err(SubscriptionError::InvalidReceipt)
        ));
        let reset = sub
            .poll_coalesced(&db, &cx, ticks(2), delivery(100))
            .unwrap()
            .unwrap();
        assert!(reset.is_snapshot());
        assert_eq!(
            plain(reset.rows()),
            snapshot(&db, &cx, "MATCH (n) RETURN n.p AS p")
        );
        sub.acknowledge(reset.receipt()).unwrap();
        assert!(
            sub.poll_coalesced(&db, &cx, ticks(2), delivery(0))
                .unwrap()
                .is_none()
        );
        sub.close();
        sub.close();
        assert!(matches!(
            sub.poll_coalesced(&db, &cx, ticks(2), delivery(100)),
            Err(SubscriptionError::Closed)
        ));
        assert!(fork.acknowledge(foreign.receipt()).is_ok());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn no_replay_rebuild_and_foreign_owner_boundaries_cannot_synthesize_history() {
    let ((), report) = run_async_under_lab(0x7263_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let handle = db
            .register_standing_native(
                &cx,
                "MATCH (n) RETURN n.p AS p",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap();
        let mut sub = db.open_standing_subscription(&cx, &handle).unwrap();
        let initial = sub
            .poll_coalesced(&db, &cx, ticks(8), delivery(100))
            .unwrap()
            .unwrap();
        let basis = sub.acknowledge(initial.receipt()).unwrap();
        let one = db.write(&commit, edit(1, 5)).await.unwrap();
        let atomic = sub
            .poll_coalesced(&db, &cx, ticks(8), delivery(100))
            .unwrap()
            .unwrap();
        assert_eq!(atomic.from(), Some(basis));
        assert_eq!(atomic.frontier(), one);
        sub.acknowledge(atomic.receipt()).unwrap();
        db.write(&commit, edit(1, 6)).await.unwrap();
        db.write(&commit, edit(1, 7)).await.unwrap();
        assert!(matches!(
            sub.poll_coalesced(&db, &cx, ticks(8), delivery(100)),
            Err(SubscriptionError::Query(
                StandingQueryError::DeltaUnavailable { .. }
            ))
        ));
        sub.restart_from_current().unwrap();
        sub.enable_replay(&mut db, &cx, 8, 100, 10_000, policy())
            .unwrap();
        let baseline = sub
            .poll_coalesced(&db, &cx, ticks(8), delivery(100))
            .unwrap()
            .unwrap();
        let before = sub.acknowledge(baseline.receipt()).unwrap();
        let replay = sub.replay_handle().unwrap().clone();
        let other = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(
            other.standing_replay_coalesced(&cx, &replay, before, ticks(8), delivery(100)),
            Err(StandingQueryError::ForeignHandle)
        ));
        assert!(matches!(
            db.standing_replay_coalesced(&cx, &handle, before, ticks(8), delivery(100)),
            Err(StandingQueryError::Unsupported)
        ));
        db.write(&commit, edit(1, 8)).await.unwrap();
        db.rebuild_standing_query(&cx, &replay, policy()).unwrap();
        assert!(matches!(
            sub.poll_coalesced(&db, &cx, ticks(8), delivery(100)),
            Err(SubscriptionError::Query(
                StandingQueryError::ReplayGap { .. }
            ))
        ));
        assert_eq!(sub.acknowledged_frontier(), Some(before));
        sub.restart_from_current().unwrap();
        let reset = sub
            .poll_coalesced(&db, &cx, ticks(8), delivery(100))
            .unwrap()
            .unwrap();
        sub.acknowledge(reset.receipt()).unwrap();
        db.write(&commit, edit(1, 9)).await.unwrap();
        sub.serial = u64::MAX;
        assert!(matches!(
            sub.poll_coalesced(&db, &cx, ticks(8), delivery(100)),
            Err(SubscriptionError::ReceiptExhausted)
        ));
        assert!(sub.pending.is_none());
        assert_eq!(sub.acknowledged_frontier(), Some(reset.frontier()));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_failed_maintained_source_never_redelivers_a_cached_coalesced_answer_as_current() {
    let ((), report) = run_async_under_lab(0x7263_1004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut first = WriteBatch::new(RelationId(1));
        first.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(1))]);
        db.write(&commit, first).await.unwrap();
        let handle = db
            .register_standing_native(
                &cx,
                "MATCH (n) RETURN n.p AS p",
                &GqlParameters::new(),
                symbols,
                GqlQueryPolicy::new(1000, 1, 10_000_000, 10_000_000),
            )
            .unwrap();
        let mut sub = db.open_standing_subscription(&cx, &handle).unwrap();
        sub.enable_replay(&mut db, &cx, 8, 100, 10_000, policy())
            .unwrap();
        let initial = sub
            .poll_coalesced(&db, &cx, ticks(8), delivery(100))
            .unwrap()
            .unwrap();
        sub.acknowledge(initial.receipt()).unwrap();
        db.write(&commit, edit(1, 2)).await.unwrap();
        db.write(&commit, edit(1, 3)).await.unwrap();
        let pending = sub
            .poll_coalesced(&db, &cx, ticks(8), delivery(100))
            .unwrap()
            .unwrap();
        let mut grow = WriteBatch::new(RelationId(1));
        grow.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(9))]);
        let durable = db.write(&commit, grow).await.unwrap();
        assert_eq!(db.frontier().unwrap(), durable);
        assert!(matches!(
            sub.poll_coalesced(&db, &cx, ticks(8), delivery(100)),
            Err(SubscriptionError::Query(
                StandingQueryError::Unavailable { .. }
            ))
        ));
        assert!(Arc::ptr_eq(sub.pending.as_ref().unwrap(), &pending));
        let replay = sub.replay_handle().unwrap().clone();
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        db.rebuild_standing_query(&cx, &replay, policy()).unwrap();
        sub.restart_from_current().unwrap();
        let baseline = sub
            .poll_coalesced(&db, &cx, ticks(8), delivery(100))
            .unwrap()
            .unwrap();
        assert!(baseline.is_snapshot());
        assert_eq!(baseline.frontier(), durable);
        assert_eq!(
            plain(baseline.rows()),
            snapshot(&db, &cx, "MATCH (n) RETURN n.p AS p")
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
