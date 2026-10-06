//! A per-database commit counter that wakes subscription streams.
//!
//! The signal carries no data and is never authority: it only tells a waiting
//! subscription that the database may have moved, after which the
//! subscription polls its own maintained query under the read lock. A missed
//! or spurious wake therefore costs at most one empty poll, never a gap.

use core::task::{Context, Waker};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

#[derive(Default)]
pub(crate) struct CommitSignal {
    epoch: AtomicU64,
    next: AtomicU64,
    wakers: Mutex<BTreeMap<u64, Waker>>,
}

impl CommitSignal {
    /// Record one published commit and wake every waiting subscription.
    pub(crate) fn committed(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        let wakers =
            core::mem::take(&mut *self.wakers.lock().unwrap_or_else(PoisonError::into_inner));
        for waker in wakers.into_values() {
            waker.wake();
        }
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    pub(crate) fn watcher(&self) -> CommitWatcher<'_> {
        CommitWatcher {
            signal: self,
            id: self.next.fetch_add(1, Ordering::Relaxed),
            seen: self.epoch(),
        }
    }
}

/// One subscription's registration; dropping it unregisters.
pub(crate) struct CommitWatcher<'a> {
    signal: &'a CommitSignal,
    id: u64,
    seen: u64,
}

impl CommitWatcher<'_> {
    /// Whether a commit happened since the last call that returned true. The
    /// waker is registered before the epoch is reread, so a commit between
    /// the two still wakes the task.
    pub(crate) fn poll_changed(&mut self, task: &Context<'_>) -> bool {
        if self.signal.epoch() != self.seen {
            self.seen = self.signal.epoch();
            return true;
        }
        self.signal
            .wakers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(self.id, task.waker().clone());
        if self.signal.epoch() != self.seen {
            self.seen = self.signal.epoch();
            return true;
        }
        false
    }
}

impl Drop for CommitWatcher<'_> {
    fn drop(&mut self) {
        self.signal
            .wakers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }
}
