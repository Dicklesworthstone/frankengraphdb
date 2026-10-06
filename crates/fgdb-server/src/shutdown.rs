//! A one-way, wakeable drain signal shared by the listener and connections.
//!
//! Triggering it stops admission and asks every idle connection to drain: a
//! connection observes it only at a receive point, so an admitted statement
//! always finishes under its own rules (a write commits or refuses) before
//! the connection says GOODBYE. Nothing here releases or acknowledges output.

use core::task::{Context, Waker};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

#[derive(Clone, Default)]
pub struct Shutdown {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    triggered: AtomicBool,
    next: AtomicU64,
    wakers: Mutex<BTreeMap<u64, Waker>>,
}

impl core::fmt::Debug for Shutdown {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Shutdown")
            .field("triggered", &self.is_triggered())
            .finish()
    }
}

impl Shutdown {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Idempotent. Wakes every registered waiter exactly once per trigger.
    pub fn trigger(&self) {
        self.inner.triggered.store(true, Ordering::Release);
        let wakers = core::mem::take(
            &mut *self
                .inner
                .wakers
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for waker in wakers.into_values() {
            waker.wake();
        }
    }

    #[must_use]
    pub fn is_triggered(&self) -> bool {
        self.inner.triggered.load(Ordering::Acquire)
    }

    pub(crate) fn waiter(&self) -> Waiter {
        Waiter {
            shutdown: self.clone(),
            id: self.inner.next.fetch_add(1, Ordering::Relaxed),
        }
    }
}

/// One registration slot; dropping it unregisters, so finished connections
/// leave nothing behind.
pub(crate) struct Waiter {
    shutdown: Shutdown,
    id: u64,
}

impl Waiter {
    /// Register the task's waker, then report whether the signal fired. The
    /// check follows registration, so a trigger between the two still wakes.
    pub(crate) fn poll_triggered(&self, task: &Context<'_>) -> bool {
        if self.shutdown.is_triggered() {
            return true;
        }
        self.shutdown
            .inner
            .wakers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(self.id, task.waker().clone());
        self.shutdown.is_triggered()
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        self.shutdown
            .inner
            .wakers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }
}
