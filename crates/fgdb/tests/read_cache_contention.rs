//! Compile the exact production generic cache module as a small integration
//! leaf, avoiding the unrelated monolithic fgdb unit-test harness.

pub use fgdb::ReadCacheUsage;
#[path = "../src/query_read_cache/live.rs"]
mod live;

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb_types::PurposeContexts;
use fgdb_types::context::SimulationCheckpointProbe;
use live::LiveCache;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

#[test]
fn live_cancellation_interrupts_contention_without_losing_hits_or_admissions() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let cx = PurposeContexts::narrow_runtime_root(&root).query();
    let cache = LiveCache::new(32);
    let owner = Arc::new(());
    let first = Arc::new([1u8; 8]);
    cache
        .remember(&owner, 1, Arc::clone(&first), 8, &mut || cx.checkpoint())
        .unwrap();

    let mut held = Some(cache.try_lock().unwrap());
    let cancelled = Arc::new(SimulationCheckpointProbe::new(Some(1)));
    let interrupted = cx.with_checkpoint_probe(Arc::clone(&cancelled));
    assert!(
        cache
            .lookup_controlled(&owner, 1, &mut || interrupted.checkpoint())
            .is_err()
    );
    assert_eq!(cancelled.calls(), 1);
    let polled = Arc::new(SimulationCheckpointProbe::new(None));
    let controlled = cx.with_checkpoint_probe(Arc::clone(&polled));
    let retained = cache
        .lookup_controlled(&owner, 1, &mut || {
            drop(held.take());
            assert!(
                cache.try_lock().is_some(),
                "live control ran under the cache lock"
            );
            controlled.checkpoint()
        })
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&retained, &first));
    assert_eq!(polled.calls(), 1);

    let second = Arc::new([2u8; 8]);
    let mut held = Some(cache.try_lock().unwrap());
    cache
        .remember(&owner, 2, Arc::clone(&second), 8, &mut || {
            drop(held.take());
            assert!(cache.try_lock().is_some());
            controlled.checkpoint()
        })
        .unwrap();
    assert_eq!(polled.calls(), 2);
    assert!(Arc::ptr_eq(&cache.lookup(&owner, 2).unwrap(), &second));

    let held = cache.try_lock().unwrap();
    let cancelled = Arc::new(SimulationCheckpointProbe::new(Some(1)));
    let interrupted = cx.with_checkpoint_probe(Arc::clone(&cancelled));
    assert!(
        cache
            .remember(&owner, 3, Arc::new([3u8; 8]), 8, &mut || interrupted
                .checkpoint())
            .is_err()
    );
    assert_eq!(cancelled.calls(), 1);
    drop(held);
    assert!(cache.lookup(&owner, 3).is_none());
    let guard = cache.try_lock().unwrap();
    assert_eq!(cache.usage(&guard).entries, 2);
    assert_eq!(cache.usage(&guard).retained_bytes, 16);
}

struct ReleasedAfterUnlock {
    cache: Weak<LiveCache<(), u8, Arc<Self>>>,
    released: Arc<AtomicUsize>,
}

impl Drop for ReleasedAfterUnlock {
    fn drop(&mut self) {
        let cache = self
            .cache
            .upgrade()
            .expect("cache remains live during release");
        assert!(
            cache.try_lock().is_some(),
            "retained value dropped under a cache lock"
        );
        self.released.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn dead_owners_and_limit_reductions_release_payloads_after_unlocking() {
    let cache = Arc::new(LiveCache::new(2));
    let released = Arc::new(AtomicUsize::new(0));
    let first = Arc::new(());
    cache
        .remember(
            &first,
            1,
            Arc::new(ReleasedAfterUnlock {
                cache: Arc::downgrade(&cache),
                released: Arc::clone(&released),
            }),
            1,
            &mut || Ok::<_, ()>(()),
        )
        .unwrap();
    drop(first);
    let replacement = Arc::new(());
    assert!(cache.lookup(&replacement, 1).is_none());
    assert_eq!(released.load(Ordering::Relaxed), 1);
    cache
        .remember(
            &replacement,
            2,
            Arc::new(ReleasedAfterUnlock {
                cache: Arc::downgrade(&cache),
                released: Arc::clone(&released),
            }),
            1,
            &mut || Ok::<_, ()>(()),
        )
        .unwrap();
    let mut guard = cache.try_lock().unwrap();
    let retired = cache.set_limit(&mut guard, 0);
    assert_eq!(cache.usage(&guard).retained_bytes, 0);
    assert_eq!(released.load(Ordering::Relaxed), 1);
    drop(guard);
    drop(retired);
    assert_eq!(released.load(Ordering::Relaxed), 2);
}

#[test]
fn explicit_budget_reductions_use_fixed_slot_order_after_reusing_holes() {
    let cache = LiveCache::new(12);
    let first = Arc::new(());
    let second = Arc::new(());
    let third = Arc::new(());
    for (owner, key) in [(&first, 1), (&second, 2), (&third, 3)] {
        cache
            .remember(owner, key, Arc::new([key; 4]), 4, &mut || Ok::<_, ()>(()))
            .unwrap();
    }
    drop(first);
    let replacement = Arc::new(());
    cache
        .remember(
            &replacement,
            4,
            Arc::new([4; 4]),
            4,
            &mut || Ok::<_, ()>(()),
        )
        .unwrap();
    let mut guard = cache.try_lock().unwrap();
    let retired = cache.set_limit(&mut guard, 8);
    assert_eq!(cache.usage(&guard).retained_bytes, 8);
    assert_eq!(cache.usage(&guard).entries, 2);
    drop(guard);
    drop(retired);
    assert!(cache.lookup(&replacement, 4).is_some());
    assert!(cache.lookup(&second, 2).is_some());
    assert!(cache.lookup(&third, 3).is_none());
}

#[test]
fn poisoning_does_not_silently_disable_completed_reuse() {
    let cache = LiveCache::new(8);
    let owner = Arc::new(());
    let value = Arc::new([1u8; 8]);
    cache
        .remember(&owner, 1, Arc::clone(&value), 8, &mut || Ok::<_, ()>(()))
        .unwrap();
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = cache.try_lock().unwrap();
        panic!("metadata holder interrupted");
    }));
    assert!(poisoned.is_err());
    assert!(Arc::ptr_eq(&cache.lookup(&owner, 1).unwrap(), &value));
    assert!(!cache.entries.is_poisoned());
}
