//! Bounded immutable extent residency with a deterministic S3-FIFO-style policy.
//!
//! Only BufferHandle pins RAM. An external snapshot protects object generations
//! from GC, but is deliberately absent from the eviction predicate. Cold loads
//! fill pre-admitted storage and verify a caller-authenticated extent checksum
//! before publication. A scan can bypass admission without bypassing accounting.
//!
//! This is the safe reference-counted residency path, not an unsafe pointer
//! swizzler or an EBR/OLC implementation. Misses require exclusive access to the
//! manager; already-pinned reads do not. Policy metadata has explicit cardinality
//! limits. Payload capacity and frame metadata share MemoryPool admission; this
//! module does not claim that BTreeMap/allocator overhead is byte-exact RSS.

use core::fmt;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::sync::Arc;

use fgdb_types::context::QueryCx;
use fgdb_types::ids::ObjectId;
use fgdb_types::StorageReadCx;

use super::memory::{MemoryError, MemoryPool, TrackedBytes};

const MAX_FREQUENCY: u8 = 3;
const FRAME_METADATA_CHARGE: usize = core::mem::size_of::<Frame>() + 2 * core::mem::size_of::<usize>();
const CHECKSUM_DOMAIN: &[u8] = b"fgdb.strata.extent-checksum.v1";

/// Integrity only. The owner must authenticate the descriptor containing this
/// checksum before using its contents as graph data.
pub fn extent_checksum(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = fgdb_crypto::Hasher::new();
    hasher.update(CHECKSUM_DOMAIN);
    hasher.update(bytes);
    hasher.finalize().0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct ExtentKey {
    object: ObjectId,
    offset: u64,
    len: usize,
    checksum: [u8; 32],
}

impl ExtentKey {
    pub fn new(
        object: ObjectId,
        offset: u64,
        len: usize,
        checksum: [u8; 32],
    ) -> Result<Self, BufferError> {
        let length = u64::try_from(len).map_err(|_| BufferError::InvalidExtent)?;
        if len == 0 || offset.checked_add(length).is_none() {
            return Err(BufferError::InvalidExtent);
        }
        Ok(Self { object, offset, len, checksum })
    }

    pub const fn object(self) -> ObjectId {
        self.object
    }

    pub const fn offset(self) -> u64 {
        self.offset
    }

    pub const fn len(self) -> usize {
        self.len
    }

    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    pub const fn checksum(self) -> [u8; 32] {
        self.checksum
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Admission {
    Normal,
    /// A miss is returned as a charged handle but does not enter either queue
    /// or the ghost history. A hit does not increase its frequency counter.
    ScanBypass,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BufferLimits {
    pub max_frames: usize,
    pub max_ghost_entries: usize,
    pub max_extent_bytes: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BufferStats {
    pub hits: u64,
    pub misses: u64,
    pub bypasses: u64,
    pub evictions: u64,
    pub promotions: u64,
}

#[derive(Debug)]
pub enum BufferError {
    InvalidLimits,
    InvalidExtent,
    ExtentTooLarge { bytes: usize, limit: usize },
    CacheSlotsExhausted { limit: usize },
    Memory(MemoryError),
    Load(io::Error),
    Cancelled(Box<asupersync::error::Error>),
    ChecksumMismatch,
}

impl fmt::Display for BufferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CacheSlotsExhausted { limit } => {
                write!(f, "ResourceExhausted: all {limit} cache frame slots are pinned")
            }
            Self::Memory(error) => error.fmt(f),
            Self::Load(error) => write!(f, "extent read failed: {error}"),
            Self::Cancelled(_) => write!(f, "extent admission cancelled"),
            other => write!(f, "extent buffer: {other:?}"),
        }
    }
}

impl std::error::Error for BufferError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Memory(error) => Some(error),
            Self::Load(error) => Some(error),
            _ => None,
        }
    }
}

impl From<MemoryError> for BufferError {
    fn from(error: MemoryError) -> Self {
        Self::Memory(error)
    }
}

#[derive(Debug)]
struct Frame {
    key: ExtentKey,
    data: TrackedBytes,
}

/// A stable, immutable extent view. Clones pin the SAME charged allocation.
#[derive(Clone, Debug)]
pub struct BufferHandle {
    frame: Arc<Frame>,
}

impl BufferHandle {
    pub fn key(&self) -> ExtentKey {
        self.frame.key
    }

    pub fn len(&self) -> usize {
        self.frame.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frame.data.is_empty()
    }
}

impl AsRef<[u8]> for BufferHandle {
    fn as_ref(&self) -> &[u8] {
        self.frame.data.as_ref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Queue {
    Small,
    Main,
}

struct Resident {
    frame: Arc<Frame>,
    queue: Queue,
    frequency: u8,
}

/// One fixed, deterministic replacement policy. The small queue targets one
/// tenth of spendable resident bytes. A twice-reused small entry promotes to
/// main; main entries consume bounded frequency credit before eviction. Ghost
/// hits enter main directly. There is no clock, entropy, or online retuning.
pub struct ExtentBuffer {
    pool: MemoryPool,
    limits: BufferLimits,
    frames: BTreeMap<ExtentKey, Resident>,
    small: VecDeque<ExtentKey>,
    main: VecDeque<ExtentKey>,
    small_bytes: usize,
    ghost: BTreeSet<ExtentKey>,
    ghost_order: VecDeque<ExtentKey>,
    stats: BufferStats,
}

impl ExtentBuffer {
    pub fn new(pool: MemoryPool, limits: BufferLimits) -> Result<Self, BufferError> {
        if limits.max_frames == 0 || limits.max_extent_bytes == 0 {
            return Err(BufferError::InvalidLimits);
        }
        Ok(Self {
            pool,
            limits,
            frames: BTreeMap::new(),
            small: VecDeque::new(),
            main: VecDeque::new(),
            small_bytes: 0,
            ghost: BTreeSet::new(),
            ghost_order: VecDeque::new(),
            stats: BufferStats::default(),
        })
    }

    pub fn memory_pool(&self) -> &MemoryPool {
        &self.pool
    }

    pub fn resident_frames(&self) -> usize {
        self.frames.len()
    }

    pub fn ghost_entries(&self) -> usize {
        self.ghost.len()
    }

    pub const fn stats(&self) -> BufferStats {
        self.stats
    }

    /// Pin an extent, invoking `load` at most once and only after admission.
    /// The loader must fill the supplied exact-size buffer through its own
    /// storage authority; it must not retain the mutable slice. Its I/O runs
    /// under QueryCx's restriction. Snapshot authorization remains the owner’s
    /// responsibility, not a consequence of obtaining a cache handle.
    pub fn pin(
        &mut self,
        cx: &QueryCx,
        key: ExtentKey,
        admission: Admission,
        load: impl FnOnce(&QueryCx, ExtentKey, &mut [u8]) -> io::Result<()>,
    ) -> Result<BufferHandle, BufferError> {
        self.pin_inner(
            key,
            admission,
            |buffer| cx.with_restriction(|| load(cx, key, buffer)),
            || cx.checkpoint().map_err(BufferError::Cancelled),
        )
    }

    fn pin_inner(
        &mut self,
        key: ExtentKey,
        admission: Admission,
        load: impl FnOnce(&mut [u8]) -> io::Result<()>,
        mut checkpoint: impl FnMut() -> Result<(), BufferError>,
    ) -> Result<BufferHandle, BufferError> {
        checkpoint()?;
        if key.len > self.limits.max_extent_bytes {
            return Err(BufferError::ExtentTooLarge {
                bytes: key.len,
                limit: self.limits.max_extent_bytes,
            });
        }
        if let Some(resident) = self.frames.get_mut(&key) {
            if admission == Admission::Normal {
                resident.frequency = resident.frequency.saturating_add(1).min(MAX_FREQUENCY);
            }
            self.stats.hits = self.stats.hits.saturating_add(1);
            return Ok(BufferHandle { frame: Arc::clone(&resident.frame) });
        }
        self.stats.misses = self.stats.misses.saturating_add(1);
        let required = key.len.checked_add(FRAME_METADATA_CHARGE).ok_or(MemoryError::SizeOverflow)?;
        if required > self.pool.limit() {
            return Err(MemoryError::ResourceExhausted {
                requested: required,
                available: self.pool.available(),
                limit: self.pool.limit(),
            }.into());
        }
        let admitted = admission == Admission::Normal;
        while self.pool.available() < required
            || (admitted && self.frames.len() >= self.limits.max_frames)
        {
            checkpoint()?;
            if !self.evict_one() {
                if admitted && self.frames.len() >= self.limits.max_frames {
                    return Err(BufferError::CacheSlotsExhausted { limit: self.limits.max_frames });
                }
                return Err(MemoryError::ResourceExhausted {
                    requested: required,
                    available: self.pool.available(),
                    limit: self.pool.limit(),
                }.into());
            }
        }
        // A racing user of the shared pool can still win this reservation.
        // That is an ordinary typed refusal, never oversubscription.
        let mut data = self.pool.allocate_inner(key.len, FRAME_METADATA_CHARGE)?;
        checkpoint()?;
        load(data.as_mut()).map_err(BufferError::Load)?;
        checkpoint()?;
        if extent_checksum(data.as_ref()) != key.checksum {
            return Err(BufferError::ChecksumMismatch);
        }
        checkpoint()?;
        let frame = Arc::new(Frame { key, data });
        if !admitted {
            self.stats.bypasses = self.stats.bypasses.saturating_add(1);
            return Ok(BufferHandle { frame });
        }
        let queue = if self.ghost.remove(&key) {
            self.ghost_order.retain(|candidate| *candidate != key);
            Queue::Main
        } else {
            self.small_bytes += frame.data.charged_bytes();
            Queue::Small
        };
        self.push(queue, key);
        self.frames.insert(key, Resident {
            frame: Arc::clone(&frame),
            queue,
            frequency: 0,
        });
        Ok(BufferHandle { frame })
    }

    fn push(&mut self, queue: Queue, key: ExtentKey) {
        match queue {
            Queue::Small => self.small.push_back(key),
            Queue::Main => self.main.push_back(key),
        }
    }

    fn record_ghost(&mut self, key: ExtentKey) {
        if self.limits.max_ghost_entries == 0 {
            return;
        }
        if self.ghost.insert(key) {
            self.ghost_order.push_back(key);
        }
        while self.ghost_order.len() > self.limits.max_ghost_entries {
            if let Some(oldest) = self.ghost_order.pop_front() {
                self.ghost.remove(&oldest);
            }
        }
    }

    fn evict_one(&mut self) -> bool {
        let preferred = if self.small_bytes > self.pool.limit() / 10 || self.main.is_empty() {
            Queue::Small
        } else {
            Queue::Main
        };
        let other = if preferred == Queue::Small { Queue::Main } else { Queue::Small };
        // The final main pass also visits entries promoted by a fallback small
        // pass. Pinned entries in one queue must not starve an evictable other.
        for queue in [preferred, other, Queue::Main] {
            if self.evict_from(queue) {
                return true;
            }
        }
        false
    }

    fn evict_from(&mut self, queue: Queue) -> bool {
        let count = match queue {
            Queue::Small => self.small.len(),
            Queue::Main => self.main.len(),
        };
        // At most MAX_FREQUENCY decrements plus one eviction per entry. Pins
        // cannot cause an unbounded spin; a failed bounded sweep refuses work.
        let attempts = count.saturating_mul(usize::from(MAX_FREQUENCY) + 2);
        for _ in 0..attempts {
            let key = match queue {
                Queue::Small => self.small.pop_front(),
                Queue::Main => self.main.pop_front(),
            };
            let Some(key) = key else { break; };
            let Some(entry) = self.frames.get_mut(&key) else { continue; };
            if entry.queue != queue {
                continue;
            }
            if Arc::strong_count(&entry.frame) != 1 {
                self.push(queue, key);
                continue;
            }
            if queue == Queue::Small && entry.frequency > 1 {
                self.small_bytes -= entry.frame.data.charged_bytes();
                entry.queue = Queue::Main;
                entry.frequency = 0;
                self.main.push_back(key);
                self.stats.promotions = self.stats.promotions.saturating_add(1);
                continue;
            }
            if queue == Queue::Main && entry.frequency > 0 {
                entry.frequency -= 1;
                self.main.push_back(key);
                continue;
            }
            let removed = self.frames.remove(&key).expect("queue entry was just resolved");
            if queue == Queue::Small {
                self.small_bytes -= removed.frame.data.charged_bytes();
                self.record_ghost(key);
            }
            drop(removed);
            self.stats.evictions = self.stats.evictions.saturating_add(1);
            return true;
        }
        false
    }

    /// Remove resident/ghost metadata after the owner retires an object
    /// generation. No filesystem object is deleted here. An outstanding handle
    /// remains readable AND charged until its last clone is dropped.
    pub fn forget_object(&mut self, cx: &impl StorageReadCx, object: ObjectId) {
        cx.with_restriction(|| self.forget_inner(object));
    }

    fn forget_inner(&mut self, object: ObjectId) {
        let mut small_bytes = 0;
        self.frames.retain(|key, entry| {
            if key.object == object {
                false
            } else {
                if entry.queue == Queue::Small {
                    small_bytes += entry.frame.data.charged_bytes();
                }
                true
            }
        });
        self.small_bytes = small_bytes;
        self.small.retain(|key| key.object != object);
        self.main.retain(|key| key.object != object);
        self.ghost.retain(|key| key.object != object);
        self.ghost_order.retain(|key| key.object != object);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn key(id: u8, bytes: &[u8]) -> ExtentKey {
        ExtentKey::new(ObjectId([id; 32]), 0, bytes.len(), extent_checksum(bytes)).unwrap()
    }

    fn cache(slots: usize) -> ExtentBuffer {
        let pool = MemoryPool::new((FRAME_METADATA_CHARGE + 64) * slots, 0).unwrap();
        ExtentBuffer::new(pool, BufferLimits {
            max_frames: slots,
            max_ghost_entries: 2,
            max_extent_bytes: 64,
        }).unwrap()
    }

    fn pin(cache: &mut ExtentBuffer, id: u8, bytes: &[u8], mode: Admission) -> BufferHandle {
        cache.pin_inner(key(id, bytes), mode, |target| {
            target.copy_from_slice(bytes);
            Ok(())
        }, || Ok(())).unwrap()
    }

    #[test]
    fn pinned_frames_survive_eviction_and_unpinned_frames_fault_back() {
        let mut cache = cache(2);
        let pinned = pin(&mut cache, 1, &[1; 64], Admission::Normal);
        drop(pin(&mut cache, 2, &[2; 64], Admission::Normal));
        drop(pin(&mut cache, 3, &[3; 64], Admission::Normal));
        assert_eq!(pinned.as_ref(), &[1; 64]);
        assert!(cache.frames.contains_key(&key(1, &[1; 64])));
        assert!(!cache.frames.contains_key(&key(2, &[2; 64])));
        drop(pinned);
        assert_eq!(pin(&mut cache, 2, &[2; 64], Admission::Normal).as_ref(), &[2; 64]);
    }

    #[test]
    fn all_pinned_refuses_before_invoking_io() {
        let mut cache = cache(1);
        let held = pin(&mut cache, 1, &[1; 64], Admission::Normal);
        let used = cache.pool.used();
        let called = Cell::new(false);
        let refused = cache.pin_inner(key(2, &[2; 64]), Admission::Normal, |_| {
            called.set(true);
            Ok(())
        }, || Ok(()));
        assert!(matches!(refused, Err(BufferError::CacheSlotsExhausted { .. })));
        assert!(!called.get());
        assert_eq!(cache.pool.used(), used);
        drop(held);
    }

    #[test]
    fn scan_bypass_is_charged_but_does_not_pollute_queues_or_ghosts() {
        let mut cache = cache(2);
        drop(pin(&mut cache, 1, &[1; 64], Admission::Normal));
        let baseline = cache.pool.used();
        for id in 2..100 {
            let scan = pin(&mut cache, id, &[id; 64], Admission::ScanBypass);
            assert!(cache.pool.used() > baseline);
            assert_eq!(cache.resident_frames(), 1);
            assert_eq!(cache.ghost_entries(), 0);
            drop(scan);
            assert_eq!(cache.pool.used(), baseline);
        }
        assert!(cache.frames.contains_key(&key(1, &[1; 64])));
    }

    #[test]
    fn twice_reused_small_entry_promotes_instead_of_being_evicted() {
        let mut cache = cache(2);
        for _ in 0..3 {
            drop(pin(&mut cache, 1, &[1; 64], Admission::Normal));
        }
        drop(pin(&mut cache, 2, &[2; 64], Admission::Normal));
        drop(pin(&mut cache, 3, &[3; 64], Admission::Normal));
        assert_eq!(cache.frames[&key(1, &[1; 64])].queue, Queue::Main);
        assert!(!cache.frames.contains_key(&key(2, &[2; 64])));
        assert_eq!(cache.stats.promotions, 1);
    }

    #[test]
    fn ghost_history_is_bounded_and_object_retirement_cleans_it() {
        let mut cache = cache(1);
        for id in 1..10 {
            drop(pin(&mut cache, id, &[id; 64], Admission::Normal));
            assert!(cache.ghost_entries() <= 2);
        }
        assert!(cache.ghost.contains(&key(8, &[8; 64])));
        cache.forget_inner(ObjectId([8; 32]));
        assert!(!cache.ghost.contains(&key(8, &[8; 64])));
        assert!(cache.ghost_order.iter().all(|key| key.object != ObjectId([8; 32])));
    }

    #[test]
    fn retiring_cache_membership_does_not_refund_an_active_handle() {
        let mut cache = cache(1);
        let held = pin(&mut cache, 1, &[1; 64], Admission::Normal);
        let clone = held.clone();
        let pool = cache.pool.clone();
        cache.forget_inner(ObjectId([1; 32]));
        assert_eq!(cache.resident_frames(), 0);
        assert!(pool.used() > 0);
        drop(cache);
        drop(held);
        assert_eq!(clone.as_ref(), &[1; 64]);
        assert!(pool.used() > 0);
        drop(clone);
        assert_eq!(pool.used(), 0);
    }

    #[test]
    fn checksum_and_source_failures_publish_nothing_and_refund_every_byte() {
        let mut cache = cache(1);
        let descriptor = key(1, &[1; 64]);
        let bad = cache.pin_inner(descriptor, Admission::Normal, |target| {
            target.fill(2);
            Ok(())
        }, || Ok(()));
        assert!(matches!(bad, Err(BufferError::ChecksumMismatch)));
        assert_eq!(cache.pool.used(), 0);
        let failed = cache.pin_inner(descriptor, Admission::Normal,
            |_| Err(io::Error::other("injected read failure")), || Ok(()));
        assert!(matches!(failed, Err(BufferError::Load(_))));
        assert_eq!(cache.resident_frames(), 0);
        assert_eq!(cache.pool.used(), 0);
    }

    #[test]
    fn cancellation_after_read_does_not_publish_a_frame() {
        let mut cache = cache(1);
        let calls = Cell::new(0);
        let result = cache.pin_inner(key(1, &[1; 64]), Admission::Normal, |target| {
            target.fill(1);
            Ok(())
        }, || {
            calls.set(calls.get() + 1);
            if calls.get() == 3 {
                Err(BufferError::Load(io::Error::other("injected checkpoint")))
            } else {
                Ok(())
            }
        });
        assert!(result.is_err());
        assert_eq!(cache.resident_frames(), 0);
        assert_eq!(cache.pool.used(), 0);
    }

    #[test]
    fn malformed_and_oversized_extents_never_reach_the_loader() {
        assert!(ExtentKey::new(ObjectId([1; 32]), u64::MAX, 1, [0; 32]).is_err());
        assert!(ExtentKey::new(ObjectId([1; 32]), 0, 0, [0; 32]).is_err());
        let mut cache = cache(1);
        let called = Cell::new(false);
        let result = cache.pin_inner(key(1, &[1; 65]), Admission::Normal, |_| {
            called.set(true);
            Ok(())
        }, || Ok(()));
        assert!(matches!(result, Err(BufferError::ExtentTooLarge { .. })));
        assert!(!called.get());
        assert_eq!(cache.pool.used(), 0);
    }
}
