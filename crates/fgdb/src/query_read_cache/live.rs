//! Bounded weak-snapshot reuse with cancellable serialized metadata access.

use std::borrow::Borrow;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError, Weak};

pub(crate) const CACHE_ENTRIES: usize = 8;

pub(crate) struct Entry<Owner, Key, Value> {
    owner: Weak<Owner>,
    key: Key,
    value: Value,
    bytes: usize,
}

type Entries<Owner, Key, Value> = [Option<Entry<Owner, Key, Value>>; CACHE_ENTRIES];
type Guard<'a, Owner, Key, Value> = MutexGuard<'a, Entries<Owner, Key, Value>>;

/// Values derived from still-live snapshot allocations, owned and budgeted by
/// one database. Admission never displaces a live entry or skips contention.
pub(crate) struct LiveCache<Owner, Key, Value> {
    pub(crate) entries: Mutex<Entries<Owner, Key, Value>>,
    max_bytes: AtomicUsize,
}

impl<Owner, Key, Value> core::fmt::Debug for LiveCache<Owner, Key, Value> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LiveCache")
            .field("max_bytes", &self.max_bytes())
            .finish_non_exhaustive()
    }
}

impl<Owner, Key, Value> LiveCache<Owner, Key, Value> {
    pub(crate) const fn new(max_bytes: usize) -> Self {
        Self {
            entries: Mutex::new([const { None }; CACHE_ENTRIES]),
            max_bytes: AtomicUsize::new(max_bytes),
        }
    }

    pub(crate) fn max_bytes(&self) -> usize {
        self.max_bytes.load(Ordering::Relaxed)
    }

    pub(crate) fn try_lock(&self) -> Option<Guard<'_, Owner, Key, Value>> {
        match self.entries.try_lock() {
            Ok(entries) => Some(entries),
            Err(TryLockError::Poisoned(poisoned)) => {
                // Complete entries are installed without caller code under
                // the mutex; poisoning never exposes a partial graph.
                self.entries.clear_poison();
                Some(poisoned.into_inner())
            }
            Err(TryLockError::WouldBlock) => None,
        }
    }

    /// The synchronous host entrypoints cannot await the foundation's async
    /// mutex. Attempt once between live polls and yield the OS thread while
    /// contended. No uninterruptible lock wait or wall-clock admission policy:
    /// the caller's cancellation/deadline is the bound. A poll is uncharged.
    pub(crate) fn lock<E>(
        &self,
        poll: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Guard<'_, Owner, Key, Value>, E> {
        loop {
            if let Some(entries) = self.try_lock() {
                return Ok(entries);
            }
            poll()?;
            std::thread::yield_now();
        }
    }

    pub(crate) fn prune(entries: &mut Entries<Owner, Key, Value>) -> Entries<Owner, Key, Value> {
        std::array::from_fn(|at| {
            if entries[at]
                .as_ref()
                .is_some_and(|entry| entry.owner.strong_count() == 0)
            {
                entries[at].take()
            } else {
                None
            }
        })
    }

    pub(crate) fn usage(&self, entries: &Entries<Owner, Key, Value>) -> crate::ReadCacheUsage {
        crate::ReadCacheUsage {
            max_bytes: self.max_bytes(),
            retained_bytes: entries.iter().flatten().map(|entry| entry.bytes).sum(),
            entries: entries.iter().flatten().count(),
        }
    }

    /// Only an explicit host limit reduction may displace a live entry. Keep
    /// the lowest occupied slots that fit, releasing higher slots first.
    pub(crate) fn set_limit(
        &self,
        entries: &mut Entries<Owner, Key, Value>,
        max_bytes: usize,
    ) -> Entries<Owner, Key, Value> {
        let mut retired = Self::prune(entries);
        let mut bytes: usize = entries.iter().flatten().map(|entry| entry.bytes).sum();
        for at in (0..CACHE_ENTRIES).rev() {
            if bytes <= max_bytes {
                break;
            }
            if let Some(entry) = entries[at].take() {
                bytes -= entry.bytes;
                retired[at] = Some(entry);
            }
        }
        self.max_bytes.store(max_bytes, Ordering::Relaxed);
        retired
    }
}

impl<Owner, Key: PartialEq, Value: Clone> LiveCache<Owner, Key, Value> {
    pub(crate) fn lookup_controlled<E>(
        &self,
        owner: &Arc<Owner>,
        key: impl Borrow<Key>,
        poll: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Option<Value>, E> {
        self.lookup_controlled_by(owner, |candidate, _| candidate == key.borrow(), poll)
    }

    /// Select the first completed entry whose bounded metadata proves reuse.
    /// The predicate runs under the metadata lock: it must only compare keys
    /// and retained counters, never poll, allocate, visit source rows or build.
    /// Owner identity, pruning and deterministic slot order remain mandatory.
    pub(crate) fn lookup_controlled_by<E>(
        &self,
        owner: &Arc<Owner>,
        matches: impl Fn(&Key, &Value) -> bool,
        poll: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Option<Value>, E> {
        let mut entries = self.lock(poll)?;
        let retired = Self::prune(&mut entries);
        let owner = Arc::downgrade(owner);
        let found = entries.iter().flatten().find_map(|entry| {
            (entry.owner.ptr_eq(&owner) && matches(&entry.key, &entry.value))
                .then(|| entry.value.clone())
        });
        drop(entries);
        drop(retired);
        Ok(found)
    }

    #[cfg(test)]
    pub(crate) fn lookup(&self, owner: &Arc<Owner>, key: impl Borrow<Key>) -> Option<Value> {
        self.lookup_controlled(owner, key, &mut || Ok::<_, core::convert::Infallible>(()))
            .unwrap()
    }

    /// `bytes` is the retained size charged for `value`.
    pub(crate) fn remember<E>(
        &self,
        owner: &Arc<Owner>,
        key: Key,
        value: Value,
        bytes: usize,
        poll: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<(), E> {
        let mut entries = self.lock(poll)?;
        let retired = Self::prune(&mut entries);
        let owner = Arc::downgrade(owner);
        // Two concurrent misses may both complete. Keep the first complete
        // admission, never reserve a slot for an in-flight/failed build.
        let present = entries
            .iter()
            .flatten()
            .any(|entry| entry.owner.ptr_eq(&owner) && entry.key == key);
        let total = entries
            .iter()
            .flatten()
            .try_fold(bytes, |total, entry| total.checked_add(entry.bytes));
        if !present
            && total.is_some_and(|total| total <= self.max_bytes())
            && let Some(slot) = entries.iter_mut().find(|entry| entry.is_none())
        {
            *slot = Some(Entry {
                owner,
                key,
                value,
                bytes,
            });
        }
        drop(entries);
        drop(retired);
        Ok(())
    }
}
