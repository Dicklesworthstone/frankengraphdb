//! Bounded immutable extent residency with a deterministic S3-FIFO-style policy.
//!
//! Only BufferHandle pins RAM. An external snapshot protects object generations
//! from GC, but is deliberately absent from the eviction predicate. Cold loads
//! fill pre-admitted storage and verify a caller-authenticated extent checksum
//! before publication. A scan can bypass admission without bypassing accounting.
//!
//! This is the safe reference-counted residency path, not an unsafe pointer
//! swizzler or an EBR/OLC implementation. Misses require exclusive access to the
//! manager only for prepare/complete; owned PendingExtent values carry admitted
//! storage across asynchronous reads without a manager borrow or cache pin.
//! Already-pinned reads do not borrow the manager. Policy metadata has explicit cardinality
//! limits. Payload capacity and frame metadata share MemoryPool admission; this
//! module does not claim that BTreeMap/allocator overhead is byte-exact RSS.

use core::fmt;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::sync::Arc;

use fgdb_types::StorageReadCx;
use fgdb_types::context::QueryCx;
use fgdb_types::ids::ObjectId;

use super::memory::{MemoryCharge, MemoryError, MemoryPool, TrackedBytes};

const MAX_FREQUENCY: u8 = 3;
const FRAME_METADATA_CHARGE: usize =
    core::mem::size_of::<Frame>() + 2 * core::mem::size_of::<usize>();
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
        Ok(Self {
            object,
            offset,
            len,
            checksum,
        })
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
    /// Foreign, retired, or substituted load reservation.
    InvalidLoad,
    ExtentTooLarge {
        bytes: usize,
        limit: usize,
    },
    CacheSlotsExhausted {
        limit: usize,
    },
    Memory(MemoryError),
    Load(io::Error),
    Cancelled(Box<asupersync::error::Error>),
    ChecksumMismatch,
}

impl fmt::Display for BufferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CacheSlotsExhausted { limit } => {
                write!(
                    f,
                    "ResourceExhausted: all {limit} cache frame slots are pinned"
                )
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

/// The payload is allocated and charged, but has not passed read integrity
/// checks and is not a resident frame. Dropping this affine token refunds it.
/// It borrows neither the manager nor an existing cache frame.
#[derive(Debug)]
pub struct PendingExtent {
    key: ExtentKey,
    admission: Admission,
    data: TrackedBytes,
    generation: Arc<()>,
}

impl PendingExtent {
    pub const fn key(&self) -> ExtentKey {
        self.key
    }
    pub fn len(&self) -> usize {
        self.data.len()
    }
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
    pub fn charged_bytes(&self) -> usize {
        self.data.charged_bytes()
    }
}

impl AsMut<[u8]> for PendingExtent {
    fn as_mut(&mut self) -> &mut [u8] {
        self.data.as_mut()
    }
}

#[derive(Debug)]
pub enum PreparedExtent {
    Resident(BufferHandle),
    Load(PendingExtent),
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
    load_generation: Arc<()>,
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
            load_generation: Arc::new(()),
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

    /// Reserve operator scratch from the shared pool, reclaiming unpinned
    /// cache frames before asking the operator to spill or refuse work.
    ///
    /// Snapshot/anchor lifetime is not an eviction veto; BufferHandle pins are.
    /// The emergency reserve remains inaccessible. Oversized requests fail
    /// before eviction, and pool races cannot oversubscribe the budget or make
    /// reclamation spin without progress.
    ///
    /// Keep the returned charge alive until the actual scratch is freed.
    /// Prefer `allocate_scratch` for byte buffers whose charge should remain
    /// structurally attached to their allocation. ResourceExhausted is the
    /// operator's signal to use its spill path; this method performs no spill
    /// I/O itself.
    pub fn reserve_scratch(
        &mut self,
        cx: &QueryCx,
        bytes: usize,
    ) -> Result<MemoryCharge, BufferError> {
        cx.with_restriction(|| {
            self.admit_scratch_inner(
                bytes,
                |pool| pool.reserve_inner(bytes),
                || cx.checkpoint().map_err(BufferError::Cancelled),
            )
        })
    }

    /// Allocate charged, zeroed query workspace, reclaiming cache residency
    /// through the same deterministic policy as `reserve_scratch`.
    pub fn allocate_scratch(
        &mut self,
        cx: &QueryCx,
        bytes: usize,
    ) -> Result<TrackedBytes, BufferError> {
        cx.with_restriction(|| {
            self.admit_scratch_inner(
                bytes,
                |pool| pool.allocate_inner(bytes, 0),
                || cx.checkpoint().map_err(BufferError::Cancelled),
            )
        })
    }

    fn admit_scratch_inner<T>(
        &mut self,
        bytes: usize,
        mut admit: impl FnMut(&MemoryPool) -> Result<T, MemoryError>,
        mut checkpoint: impl FnMut() -> Result<(), BufferError>,
    ) -> Result<T, BufferError> {
        checkpoint()?;
        if bytes > self.pool.limit() {
            return Err(MemoryError::ResourceExhausted {
                requested: bytes,
                available: self.pool.available(),
                limit: self.pool.limit(),
            }
            .into());
        }
        loop {
            checkpoint()?;
            match admit(&self.pool) {
                Ok(value) => {
                    checkpoint()?;
                    return Ok(value);
                }
                Err(error @ MemoryError::ResourceExhausted { .. }) => {
                    // Each retry evicts one resident frame owned by this
                    // exclusively borrowed manager. Pinned frames survive;
                    // once no victim remains, return pressure to the operator.
                    if !self.evict_one() {
                        return Err(error.into());
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
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

    /// Acquire an already resident frame or an owned, pre-admitted load
    /// reservation. A caller may release its manager lock after this method,
    /// perform asynchronous I/O into the token, then reacquire it for complete.
    pub fn prepare(
        &mut self,
        cx: &QueryCx,
        key: ExtentKey,
        admission: Admission,
    ) -> Result<PreparedExtent, BufferError> {
        cx.with_restriction(|| {
            self.prepare_inner(key, admission, &mut || {
                cx.checkpoint().map_err(BufferError::Cancelled)
            })
        })
    }

    /// Verify and publish a prepared load. Foreign tokens and tokens predating
    /// object retirement cannot publish. Parallel misses deduplicate here, and
    /// a last-slot race is a typed refusal rather than unbounded cache growth.
    pub fn complete(
        &mut self,
        cx: &QueryCx,
        pending: PendingExtent,
    ) -> Result<BufferHandle, BufferError> {
        cx.with_restriction(|| {
            self.complete_inner(pending, &mut || {
                cx.checkpoint().map_err(BufferError::Cancelled)
            })
        })
    }

    /// Asynchronous convenience path using the same prepare/complete protocol.
    /// The loader OWNS the token and returns it after filling its exact-size
    /// storage. Both loader construction and every future poll run restricted.
    /// Use prepare/complete directly to avoid holding the exclusive manager
    /// borrow while awaiting. No cache pin is created before verification.
    pub async fn pin_async<F, Fut>(
        &mut self,
        cx: &QueryCx,
        key: ExtentKey,
        admission: Admission,
        load: F,
    ) -> Result<BufferHandle, BufferError>
    where
        F: FnOnce(PendingExtent) -> Fut,
        Fut: std::future::Future<Output = io::Result<PendingExtent>>,
    {
        match self.prepare(cx, key, admission)? {
            PreparedExtent::Resident(handle) => Ok(handle),
            PreparedExtent::Load(pending) => {
                let future = cx.with_restriction(|| load(pending));
                let pending = cx
                    .with_restriction_async(future)
                    .await
                    .map_err(BufferError::Load)?;
                if pending.key != key || pending.admission != admission {
                    return Err(BufferError::InvalidLoad);
                }
                self.complete(cx, pending)
            }
        }
    }

    fn pin_inner(
        &mut self,
        key: ExtentKey,
        admission: Admission,
        load: impl FnOnce(&mut [u8]) -> io::Result<()>,
        mut checkpoint: impl FnMut() -> Result<(), BufferError>,
    ) -> Result<BufferHandle, BufferError> {
        match self.prepare_inner(key, admission, &mut checkpoint)? {
            PreparedExtent::Resident(handle) => Ok(handle),
            PreparedExtent::Load(mut pending) => {
                load(pending.as_mut()).map_err(BufferError::Load)?;
                self.complete_inner(pending, &mut checkpoint)
            }
        }
    }

    fn prepare_inner(
        &mut self,
        key: ExtentKey,
        admission: Admission,
        checkpoint: &mut impl FnMut() -> Result<(), BufferError>,
    ) -> Result<PreparedExtent, BufferError> {
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
            return Ok(PreparedExtent::Resident(BufferHandle {
                frame: Arc::clone(&resident.frame),
            }));
        }
        self.stats.misses = self.stats.misses.saturating_add(1);
        let required = key
            .len
            .checked_add(FRAME_METADATA_CHARGE)
            .ok_or(MemoryError::SizeOverflow)?;
        if required > self.pool.effective_limit() {
            return Err(MemoryError::ResourceExhausted {
                requested: required,
                available: self.pool.available(),
                limit: self.pool.effective_limit(),
            }
            .into());
        }
        let admitted = admission == Admission::Normal;
        while self.pool.available() < required
            || (admitted && self.frames.len() >= self.limits.max_frames)
        {
            checkpoint()?;
            if !self.evict_one() {
                if admitted && self.frames.len() >= self.limits.max_frames {
                    return Err(BufferError::CacheSlotsExhausted {
                        limit: self.limits.max_frames,
                    });
                }
                return Err(MemoryError::ResourceExhausted {
                    requested: required,
                    available: self.pool.available(),
                    limit: self.pool.effective_limit(),
                }
                .into());
            }
        }
        // A racing user of the shared pool can still win this reservation.
        // That is an ordinary typed refusal, never oversubscription.
        let data = self.pool.allocate_inner(key.len, FRAME_METADATA_CHARGE)?;
        checkpoint()?;
        Ok(PreparedExtent::Load(PendingExtent {
            key,
            admission,
            data,
            generation: Arc::clone(&self.load_generation),
        }))
    }

    fn complete_inner(
        &mut self,
        pending: PendingExtent,
        checkpoint: &mut impl FnMut() -> Result<(), BufferError>,
    ) -> Result<BufferHandle, BufferError> {
        checkpoint()?;
        if !Arc::ptr_eq(&pending.generation, &self.load_generation) {
            return Err(BufferError::InvalidLoad);
        }
        let PendingExtent {
            key,
            admission,
            data,
            ..
        } = pending;
        if extent_checksum(data.as_ref()) != key.checksum {
            return Err(BufferError::ChecksumMismatch);
        }
        checkpoint()?;
        if let Some(resident) = self.frames.get_mut(&key) {
            if admission == Admission::Normal {
                resident.frequency = resident.frequency.saturating_add(1).min(MAX_FREQUENCY);
            }
            self.stats.hits = self.stats.hits.saturating_add(1);
            // Another prepared miss won publication. Drop this duplicate's
            // charged allocation and reuse the already authenticated frame.
            return Ok(BufferHandle {
                frame: Arc::clone(&resident.frame),
            });
        }
        let admitted = admission == Admission::Normal;
        // Slots are not held across I/O. Another completion may have occupied
        // the last one, so recheck the bound without reserving memory twice.
        while admitted && self.frames.len() >= self.limits.max_frames {
            checkpoint()?;
            if !self.evict_one() {
                return Err(BufferError::CacheSlotsExhausted {
                    limit: self.limits.max_frames,
                });
            }
        }
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
        self.frames.insert(
            key,
            Resident {
                frame: Arc::clone(&frame),
                queue,
                frequency: 0,
            },
        );
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
        let preferred =
            if self.small_bytes > self.pool.effective_limit() / 10 || self.main.is_empty() {
                Queue::Small
            } else {
                Queue::Main
            };
        let other = if preferred == Queue::Small {
            Queue::Main
        } else {
            Queue::Small
        };
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
            let Some(key) = key else {
                break;
            };
            let Some(entry) = self.frames.get_mut(&key) else {
                continue;
            };
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
            let removed = self
                .frames
                .remove(&key)
                .expect("queue entry was just resolved");
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
        // A fresh identity invalidates all earlier pending loads, including
        // unrelated objects conservatively. No unbounded retired-ID registry
        // or wrapping epoch is needed, and old handles remain independently live.
        self.load_generation = Arc::new(());
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
        ExtentBuffer::new(
            pool,
            BufferLimits {
                max_frames: slots,
                max_ghost_entries: 2,
                max_extent_bytes: 64,
            },
        )
        .unwrap()
    }

    fn pin(cache: &mut ExtentBuffer, id: u8, bytes: &[u8], mode: Admission) -> BufferHandle {
        cache
            .pin_inner(
                key(id, bytes),
                mode,
                |target| {
                    target.copy_from_slice(bytes);
                    Ok(())
                },
                || Ok(()),
            )
            .unwrap()
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
        assert_eq!(
            pin(&mut cache, 2, &[2; 64], Admission::Normal).as_ref(),
            &[2; 64]
        );
    }

    #[test]
    fn all_pinned_refuses_before_invoking_io() {
        let mut cache = cache(1);
        let held = pin(&mut cache, 1, &[1; 64], Admission::Normal);
        let used = cache.pool.used();
        let called = Cell::new(false);
        let refused = cache.pin_inner(
            key(2, &[2; 64]),
            Admission::Normal,
            |_| {
                called.set(true);
                Ok(())
            },
            || Ok(()),
        );
        assert!(matches!(
            refused,
            Err(BufferError::CacheSlotsExhausted { .. })
        ));
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
        assert!(
            cache
                .ghost_order
                .iter()
                .all(|key| key.object != ObjectId([8; 32]))
        );
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
        let bad = cache.pin_inner(
            descriptor,
            Admission::Normal,
            |target| {
                target.fill(2);
                Ok(())
            },
            || Ok(()),
        );
        assert!(matches!(bad, Err(BufferError::ChecksumMismatch)));
        assert_eq!(cache.pool.used(), 0);
        let failed = cache.pin_inner(
            descriptor,
            Admission::Normal,
            |_| Err(io::Error::other("injected read failure")),
            || Ok(()),
        );
        assert!(matches!(failed, Err(BufferError::Load(_))));
        assert_eq!(cache.resident_frames(), 0);
        assert_eq!(cache.pool.used(), 0);
    }

    #[test]
    fn cancellation_after_read_does_not_publish_a_frame() {
        let mut cache = cache(1);
        let calls = Cell::new(0);
        let result = cache.pin_inner(
            key(1, &[1; 64]),
            Admission::Normal,
            |target| {
                target.fill(1);
                Ok(())
            },
            || {
                calls.set(calls.get() + 1);
                if calls.get() == 3 {
                    Err(BufferError::Load(io::Error::other("injected checkpoint")))
                } else {
                    Ok(())
                }
            },
        );
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
        let result = cache.pin_inner(
            key(1, &[1; 65]),
            Admission::Normal,
            |_| {
                called.set(true);
                Ok(())
            },
            || Ok(()),
        );
        assert!(matches!(result, Err(BufferError::ExtentTooLarge { .. })));
        assert!(!called.get());
        assert_eq!(cache.pool.used(), 0);
    }

    fn pending(cache: &mut ExtentBuffer, id: u8) -> PendingExtent {
        match cache
            .prepare_inner(key(id, &[id; 64]), Admission::Normal, &mut || Ok(()))
            .unwrap()
        {
            PreparedExtent::Load(mut pending) => {
                pending.as_mut().fill(id);
                pending
            }
            PreparedExtent::Resident(_) => panic!("expected a cold extent"),
        }
    }

    #[test]
    fn pending_load_drop_refunds_without_publishing() {
        let mut cache = cache(1);
        let load = pending(&mut cache, 1);
        assert_eq!(cache.resident_frames(), 0);
        assert_eq!(cache.pool.used(), load.charged_bytes());
        drop(load);
        assert_eq!(cache.pool.used(), 0);
    }

    #[test]
    fn parallel_misses_deduplicate_their_resident_allocation() {
        let mut cache = cache(2);
        let a = pending(&mut cache, 1);
        let b = pending(&mut cache, 1);
        let charge = a.charged_bytes();
        assert_eq!(cache.pool.used(), charge * 2);
        let a = cache.complete_inner(a, &mut || Ok(())).unwrap();
        let b = cache.complete_inner(b, &mut || Ok(())).unwrap();
        assert!(Arc::ptr_eq(&a.frame, &b.frame));
        assert_eq!(cache.resident_frames(), 1);
        assert_eq!(cache.pool.used(), charge);
    }

    #[test]
    fn retired_and_foreign_pending_loads_cannot_publish() {
        let mut a = cache(1);
        let mut b = cache(1);
        let load = pending(&mut a, 1);
        assert!(matches!(
            b.complete_inner(load, &mut || Ok(())),
            Err(BufferError::InvalidLoad)
        ));
        assert_eq!(a.pool.used(), 0);
        let load = pending(&mut a, 1);
        a.forget_inner(ObjectId([1; 32]));
        assert!(matches!(
            a.complete_inner(load, &mut || Ok(())),
            Err(BufferError::InvalidLoad)
        ));
        assert_eq!(
            (a.resident_frames(), b.resident_frames(), a.pool.used()),
            (0, 0, 0)
        );
    }

    #[test]
    fn completions_recheck_slot_limits_after_other_io_wins() {
        let pool = MemoryPool::new((FRAME_METADATA_CHARGE + 64) * 2, 0).unwrap();
        let mut cache = ExtentBuffer::new(
            pool.clone(),
            BufferLimits {
                max_frames: 1,
                max_ghost_entries: 1,
                max_extent_bytes: 64,
            },
        )
        .unwrap();
        let first = pending(&mut cache, 1);
        let second = pending(&mut cache, 2);
        let held = cache.complete_inner(first, &mut || Ok(())).unwrap();
        assert!(matches!(
            cache.complete_inner(second, &mut || Ok(())),
            Err(BufferError::CacheSlotsExhausted { limit: 1 })
        ));
        assert_eq!(cache.resident_frames(), 1);
        assert_eq!(pool.used(), held.frame.data.charged_bytes());
    }

    #[test]
    fn impossible_ancestor_extent_does_not_flush_useful_cache_entries() {
        let root = MemoryPool::new(FRAME_METADATA_CHARGE + 64, 0).unwrap();
        let pool = root.child(10000, 0).unwrap();
        let mut cache = ExtentBuffer::new(
            pool,
            BufferLimits {
                max_frames: 4,
                max_ghost_entries: 4,
                max_extent_bytes: 128,
            },
        )
        .unwrap();
        drop(pin(&mut cache, 1, &[1; 64], Admission::Normal));
        let used = root.used();
        assert!(matches!(
            cache.prepare_inner(key(2, &[2; 128]), Admission::Normal, &mut || Ok(())),
            Err(BufferError::Memory(MemoryError::ResourceExhausted { .. }))
        ));
        assert_eq!(cache.resident_frames(), 1);
        assert_eq!(cache.stats().evictions, 0);
        assert_eq!(root.used(), used);
    }

    fn under_lab<Fut>(test: impl FnOnce(QueryCx) -> Fut + Send + 'static)
    where
        Fut: std::future::Future<Output = ()> + Send,
    {
        let (_, report) = asupersync::lab::run_async_under_lab(20260923, |root| async move {
            test(fgdb_types::PurposeContexts::narrow_runtime_root(&root).query()).await;
        });
        assert!(report.invariant_violations.is_empty(), "{report:?}");
    }

    #[test]
    fn async_loader_is_not_called_for_hits_and_failed_reads_refund() {
        under_lab(|cx| async move {
            let mut cache = cache(2);
            let first = cache
                .pin_async(
                    &cx,
                    key(1, &[1; 64]),
                    Admission::Normal,
                    |mut pending| async move {
                        pending.as_mut().fill(1);
                        Ok(pending)
                    },
                )
                .await
                .unwrap();
            let second = cache
                .pin_async(&cx, key(1, &[1; 64]), Admission::Normal, |_| async {
                    panic!("a cache hit must not invoke the loader")
                })
                .await
                .unwrap();
            assert!(Arc::ptr_eq(&first.frame, &second.frame));
            let before = cache.pool.used();
            let failed = cache
                .pin_async(
                    &cx,
                    key(2, &[2; 64]),
                    Admission::Normal,
                    |pending| async move {
                        drop(pending);
                        Err(io::Error::other("read failed"))
                    },
                )
                .await;
            assert!(matches!(failed, Err(BufferError::Load(_))));
            assert_eq!(cache.pool.used(), before);
            let corrupt = cache
                .pin_async(
                    &cx,
                    key(2, &[2; 64]),
                    Admission::Normal,
                    |mut pending| async move {
                        pending.as_mut().fill(3);
                        Ok(pending)
                    },
                )
                .await;
            assert!(matches!(corrupt, Err(BufferError::ChecksumMismatch)));
            assert_eq!(cache.pool.used(), before);
        });
    }

    #[test]
    fn dropped_async_load_refunds_the_owned_staging_allocation() {
        under_lab(|cx| async move {
            use std::future::Future;
            use std::task::{Context, Waker};
            let mut cache = cache(1);
            {
                let future = cache.pin_async(
                    &cx,
                    key(1, &[1; 64]),
                    Admission::Normal,
                    |pending| async move {
                        std::future::pending::<()>().await;
                        Ok(pending)
                    },
                );
                let mut future = std::pin::pin!(future);
                assert!(
                    future
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
            }
            assert_eq!((cache.pool.used(), cache.resident_frames()), (0, 0));
        });
    }

    #[test]
    fn async_loader_cannot_substitute_another_valid_extent() {
        under_lab(|cx| async move {
            let mut cache = cache(2);
            let other = pending(&mut cache, 2);
            let result = cache
                .pin_async(
                    &cx,
                    key(1, &[1; 64]),
                    Admission::Normal,
                    |requested| async move {
                        drop(requested);
                        Ok(other)
                    },
                )
                .await;
            assert!(matches!(result, Err(BufferError::InvalidLoad)));
            assert_eq!((cache.pool.used(), cache.resident_frames()), (0, 0));
        });
    }
    fn scratch(cache: &mut ExtentBuffer, bytes: usize) -> Result<MemoryCharge, BufferError> {
        cache.admit_scratch_inner(bytes, |pool| pool.reserve_inner(bytes), || Ok(()))
    }

    #[test]
    fn query_scratch_reclaims_cache_without_revoking_pinned_inputs() {
        let mut cache = cache(3);
        let held = pin(&mut cache, 1, &[1; 64], Admission::Normal);
        drop(pin(&mut cache, 2, &[2; 64], Admission::Normal));
        drop(pin(&mut cache, 3, &[3; 64], Admission::Normal));
        let bytes = cache.pool.limit() - held.frame.data.charged_bytes();
        let charge = scratch(&mut cache, bytes).unwrap();
        assert_eq!(charge.bytes(), bytes);
        assert_eq!(cache.resident_frames(), 1);
        assert_eq!(cache.stats.evictions, 2);
        assert_eq!(held.as_ref(), &[1; 64]);
        assert_eq!(cache.pool.available(), 0);
        drop(charge);
        assert_eq!(cache.pool.used(), held.frame.data.charged_bytes());
    }

    #[test]
    fn oversized_scratch_does_not_destroy_useful_residency() {
        let mut cache = cache(2);
        drop(pin(&mut cache, 1, &[1; 64], Admission::Normal));
        drop(pin(&mut cache, 2, &[2; 64], Admission::Normal));
        let used = cache.pool.used();
        let before = cache.stats();
        let called = Cell::new(false);
        let bytes = cache.pool.limit() + 1;
        let result = cache.admit_scratch_inner(
            bytes,
            |pool| {
                called.set(true);
                pool.reserve_inner(bytes)
            },
            || Ok(()),
        );
        assert!(matches!(
            result,
            Err(BufferError::Memory(MemoryError::ResourceExhausted { .. }))
        ));
        assert!(!called.get());
        assert_eq!(cache.pool.used(), used);
        assert_eq!(cache.stats(), before);
        assert_eq!(cache.resident_frames(), 2);
    }

    #[test]
    fn pinned_memory_forces_typed_scratch_refusal() {
        let mut cache = cache(1);
        let held = pin(&mut cache, 1, &[1; 64], Admission::Normal);
        let used = cache.pool.used();
        let result = scratch(&mut cache, 1);
        assert!(matches!(
            result,
            Err(BufferError::Memory(MemoryError::ResourceExhausted { .. }))
        ));
        assert_eq!(cache.pool.used(), used);
        assert_eq!(cache.stats.evictions, 0);
        assert_eq!(held.as_ref(), &[1; 64]);
    }

    #[test]
    fn losing_every_pool_race_has_a_finite_reclamation_bound() {
        let mut cache = cache(4);
        for id in 1..=4 {
            drop(pin(&mut cache, id, &[id; 64], Admission::Normal));
        }
        let attempts = Cell::new(0);
        let result: Result<MemoryCharge, BufferError> = cache.admit_scratch_inner(
            1,
            |pool| {
                attempts.set(attempts.get() + 1);
                Err(MemoryError::ResourceExhausted {
                    requested: 1,
                    available: 0,
                    limit: pool.limit(),
                })
            },
            || Ok(()),
        );
        assert!(matches!(
            result,
            Err(BufferError::Memory(MemoryError::ResourceExhausted { .. }))
        ));
        assert_eq!(attempts.get(), 5);
        assert_eq!(cache.stats.evictions, 4);
        assert_eq!(cache.resident_frames(), 0);
        assert_eq!(cache.pool.used(), 0);
    }

    #[test]
    fn query_scratch_never_spends_the_emergency_reserve() {
        let regular = FRAME_METADATA_CHARGE + 64;
        let pool = MemoryPool::new(regular + 4096, 4096).unwrap();
        let mut cache = ExtentBuffer::new(
            pool,
            BufferLimits {
                max_frames: 1,
                max_ghost_entries: 1,
                max_extent_bytes: 64,
            },
        )
        .unwrap();
        drop(pin(&mut cache, 1, &[1; 64], Admission::Normal));
        let charge = scratch(&mut cache, regular).unwrap();
        assert_eq!(cache.pool.used(), regular);
        assert_eq!(cache.pool.emergency_reserve(), 4096);
        assert!(matches!(
            scratch(&mut cache, 1),
            Err(BufferError::Memory(MemoryError::ResourceExhausted { .. }))
        ));
        drop(charge);
        assert_eq!(cache.pool.available(), regular);
    }
}
