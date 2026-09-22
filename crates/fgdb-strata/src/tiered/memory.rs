//! Shared resident-byte admission with affine, automatically refunded charges.
//!
//! A snapshot lease protects a GENERATION, not its RAM. Charges belong to actual
//! allocations and can outlive the pool handle or a cache entry. Normal callers
//! cannot spend the emergency reserve. These counters account requested vector
//! capacity and explicitly supplied metadata, not allocator headers, host RSS,
//! overcommit, or unrelated foundation allocations.
//!
//! Child pools add query/operator ceilings without minting new admission. A
//! reservation charges every ancestor and rolls back every tentative charge on
//! refusal. Individual counters are atomic; observations across multiple pools
//! are not an atomic snapshot. A racing admission may observe a tentative charge
//! that is subsequently refunded, but no successful allocation can escape its
//! ancestor ceilings. Reservation and refund neither allocate nor take a lock.

use core::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use fgdb_types::{QueryCx, StorageReadCx};

pub mod spill;
pub use spill::{SpillError, SpillFile, SpillLimits, SpillRun, SpillStats};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryError {
    InvalidLimits,
    SizeOverflow,
    ResourceExhausted {
        requested: usize,
        available: usize,
        limit: usize,
    },
    AllocationFailed {
        requested: usize,
    },
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => write!(f, "emergency reserve exceeds the resident limit"),
            Self::SizeOverflow => write!(f, "resident allocation size overflow"),
            Self::ResourceExhausted {
                requested,
                available,
                limit,
            } => write!(
                f,
                "ResourceExhausted: requested {requested} resident bytes, {available} available under {limit}"
            ),
            Self::AllocationFailed { requested } => {
                write!(f, "allocator refused {requested} resident bytes")
            }
        }
    }
}

impl std::error::Error for MemoryError {}

#[derive(Debug)]
struct Budget {
    limit: usize,
    emergency: usize,
    used: AtomicUsize,
}

impl Budget {
    fn acquire(&self, bytes: usize) -> Result<(), MemoryError> {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let available = self.limit - used;
            if bytes > available {
                return Err(MemoryError::ResourceExhausted {
                    requested: bytes,
                    available,
                    limit: self.limit,
                });
            }
            match self.used.compare_exchange_weak(
                used,
                used + bytes,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(observed) => used = observed,
            }
        }
    }

    fn refund(&self, bytes: usize) {
        let previous = self.used.fetch_sub(bytes, Ordering::AcqRel);
        debug_assert!(previous >= bytes, "a resident charge was refunded twice");
    }
}

/// Clones share one budget; cloning does not mint additional admission.
///
/// Ancestors are stored in a flat, root-first path. Creating children allocates
/// this path once; reserving, refunding, and dropping deep hierarchies do not
/// recurse or allocate. There is no registry retaining dead child pools.
#[derive(Clone, Debug)]
pub struct MemoryPool {
    budget: Arc<Budget>,
    ancestors: Arc<[Arc<Budget>]>,
}

impl MemoryPool {
    pub fn new(resident_limit: usize, emergency_reserve: usize) -> Result<Self, MemoryError> {
        let limit = resident_limit
            .checked_sub(emergency_reserve)
            .ok_or(MemoryError::InvalidLimits)?;
        Ok(Self {
            budget: Arc::new(Budget {
                limit,
                emergency: emergency_reserve,
                used: AtomicUsize::new(0),
            }),
            ancestors: Arc::from([]),
        })
    }

    /// Create a separately capped child of this pool (for example an operator
    /// under a query, or a query under a shard). Every child allocation also
    /// consumes this pool and all its ancestors. The child's emergency reserve
    /// is excluded from its own ceiling; it does not reserve bytes in advance.
    pub fn child(
        &self,
        resident_limit: usize,
        emergency_reserve: usize,
    ) -> Result<Self, MemoryError> {
        let mut child = Self::new(resident_limit, emergency_reserve)?;
        let count = self
            .ancestors
            .len()
            .checked_add(1)
            .ok_or(MemoryError::SizeOverflow)?;
        let requested = count
            .checked_mul(size_of::<Arc<Budget>>())
            .ok_or(MemoryError::SizeOverflow)?;
        let mut ancestors = Vec::new();
        ancestors
            .try_reserve_exact(count)
            .map_err(|_| MemoryError::AllocationFailed { requested })?;
        ancestors.extend(self.ancestors.iter().cloned());
        ancestors.push(Arc::clone(&self.budget));
        child.ancestors = ancestors.into();
        Ok(child)
    }

    /// Local spendable bytes; emergency bytes are deliberately excluded.
    /// Ancestor ceilings may further constrain admission; see `available`.
    pub fn limit(&self) -> usize {
        self.budget.limit
    }

    /// Largest reservation this hierarchy can ever admit, even when empty.
    pub fn effective_limit(&self) -> usize {
        self.ancestors
            .iter()
            .fold(self.limit(), |limit, ancestor| limit.min(ancestor.limit))
    }

    pub fn emergency_reserve(&self) -> usize {
        self.budget.emergency
    }

    /// Local usage, including all outstanding descendant reservations.
    pub fn used(&self) -> usize {
        self.budget.used.load(Ordering::Acquire)
    }

    /// Advisory minimum headroom across this pool and all its ancestors.
    /// Admission itself uses checked CAS operations, not this observation.
    pub fn available(&self) -> usize {
        self.ancestors
            .iter()
            .fold(self.limit() - self.used(), |available, ancestor| {
                available.min(ancestor.limit - ancestor.used.load(Ordering::Acquire))
            })
    }

    /// Reserve before an operator allocates its own region-owned scratch.
    /// Retain the returned affine charge until that scratch is actually freed.
    pub fn reserve(
        &self,
        cx: &impl StorageReadCx,
        bytes: usize,
    ) -> Result<MemoryCharge, MemoryError> {
        cx.with_restriction(|| self.reserve_inner(bytes))
    }

    /// Allocate a byte buffer whose accounting cannot be detached from its data.
    pub fn allocate_zeroed(
        &self,
        cx: &impl StorageReadCx,
        bytes: usize,
    ) -> Result<TrackedBytes, MemoryError> {
        cx.with_restriction(|| self.allocate_inner(bytes, 0))
    }

    /// Try resident admission, then spill one caller-selected victim and retry
    /// once. Arithmetic/allocator errors and intrinsically impossible requests
    /// never cause scratch I/O. Operators choose the victim and may invoke this
    /// again with another batch; there is no hidden eviction loop or victim list.
    pub async fn allocate_spilling<F>(
        &self,
        cx: &QueryCx,
        bytes: usize,
        victim: &mut SpillableBytes,
        scratch: &mut SpillFile<F>,
    ) -> Result<TrackedBytes, SpillError>
    where
        F: asupersync::io::AsyncRead
            + asupersync::io::AsyncWrite
            + asupersync::io::AsyncSeek
            + Unpin,
    {
        cx.checkpoint().map_err(SpillError::Interrupted)?;
        if bytes > self.effective_limit() {
            return Err(MemoryError::ResourceExhausted {
                requested: bytes,
                available: self.available(),
                limit: self.effective_limit(),
            }
            .into());
        }
        match self.allocate_zeroed(cx, bytes) {
            Ok(allocation) => Ok(allocation),
            Err(error @ MemoryError::ResourceExhausted { .. }) => {
                if !victim.spill(cx, scratch).await? {
                    return Err(error.into());
                }
                cx.checkpoint().map_err(SpillError::Interrupted)?;
                self.allocate_zeroed(cx, bytes).map_err(SpillError::Memory)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn acquire(&self, bytes: usize) -> Result<(), MemoryError> {
        for (index, ancestor) in self.ancestors.iter().enumerate() {
            if let Err(error) = ancestor.acquire(bytes) {
                for acquired in self.ancestors[..index].iter().rev() {
                    acquired.refund(bytes);
                }
                return Err(error);
            }
        }
        if let Err(error) = self.budget.acquire(bytes) {
            for ancestor in self.ancestors.iter().rev() {
                ancestor.refund(bytes);
            }
            return Err(error);
        }
        Ok(())
    }

    fn refund(&self, bytes: usize) {
        self.budget.refund(bytes);
        for ancestor in self.ancestors.iter().rev() {
            ancestor.refund(bytes);
        }
    }

    pub(super) fn reserve_inner(&self, bytes: usize) -> Result<MemoryCharge, MemoryError> {
        self.acquire(bytes)?;
        Ok(MemoryCharge {
            pool: self.clone(),
            bytes,
        })
    }

    pub(super) fn allocate_inner(
        &self,
        bytes: usize,
        metadata: usize,
    ) -> Result<TrackedBytes, MemoryError> {
        let requested = bytes
            .checked_add(metadata)
            .ok_or(MemoryError::SizeOverflow)?;
        let mut charge = self.reserve_inner(requested)?;
        let mut data = Vec::new();
        data.try_reserve_exact(bytes)
            .map_err(|_| MemoryError::AllocationFailed { requested: bytes })?;
        // reserve_exact is permitted to return a larger logical capacity. It
        // must be charged before the allocation can escape to any consumer.
        let actual = data
            .capacity()
            .checked_add(metadata)
            .ok_or(MemoryError::SizeOverflow)?;
        if actual > charge.bytes {
            charge.pool.acquire(actual - charge.bytes)?;
            charge.bytes = actual;
        }
        data.resize(bytes, 0);
        Ok(TrackedBytes { data, charge })
    }
}

/// An affine byte reservation. There is deliberately no Clone or public refund.
#[derive(Debug)]
pub struct MemoryCharge {
    pool: MemoryPool,
    bytes: usize,
}

impl MemoryCharge {
    pub const fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for MemoryCharge {
    fn drop(&mut self) {
        self.pool.refund(self.bytes);
    }
}

/// Data is dropped BEFORE its charge (declaration-order field destruction).
/// Capacity-changing and into-Vec escape hatches are intentionally absent.
pub struct TrackedBytes {
    data: Vec<u8>,
    charge: MemoryCharge,
}

impl TrackedBytes {
    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub const fn charged_bytes(&self) -> usize {
        self.charge.bytes
    }
}

impl AsRef<[u8]> for TrackedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

impl AsMut<[u8]> for TrackedBytes {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
}

impl fmt::Debug for TrackedBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrackedBytes")
            .field("len", &self.len())
            .field("charged_bytes", &self.charged_bytes())
            .finish_non_exhaustive()
    }
}

/// An operator-owned batch that moves between accounted RAM and query scratch.
/// No transition takes the old state out before awaiting: failed or dropped
/// spill futures keep the source bytes, and failed/dropped restores keep the run.
#[derive(Debug)]
pub struct SpillableBytes {
    state: SpillableState,
}

#[derive(Debug)]
enum SpillableState {
    Resident(TrackedBytes),
    Spilled(SpillRun),
}

impl SpillableBytes {
    pub fn new(bytes: TrackedBytes) -> Self {
        Self {
            state: SpillableState::Resident(bytes),
        }
    }

    pub fn len(&self) -> usize {
        match &self.state {
            SpillableState::Resident(bytes) => bytes.len(),
            SpillableState::Spilled(run) => run.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn is_spilled(&self) -> bool {
        matches!(&self.state, SpillableState::Spilled(_))
    }

    pub fn resident(&self) -> Option<&[u8]> {
        match &self.state {
            SpillableState::Resident(bytes) => Some(bytes.as_ref()),
            SpillableState::Spilled(_) => None,
        }
    }

    pub fn resident_mut(&mut self) -> Option<&mut [u8]> {
        match &mut self.state {
            SpillableState::Resident(bytes) => Some(bytes.as_mut()),
            SpillableState::Spilled(_) => None,
        }
    }

    pub fn charged_bytes(&self) -> usize {
        match &self.state {
            SpillableState::Resident(bytes) => bytes.charged_bytes(),
            SpillableState::Spilled(_) => 0,
        }
    }

    /// Returns false when already spilled. Source memory is freed only after
    /// scratch publishes a complete run; there is no second payload allocation.
    pub async fn spill<F>(
        &mut self,
        cx: &QueryCx,
        scratch: &mut SpillFile<F>,
    ) -> Result<bool, SpillError>
    where
        F: asupersync::io::AsyncRead
            + asupersync::io::AsyncWrite
            + asupersync::io::AsyncSeek
            + Unpin,
    {
        cx.checkpoint().map_err(SpillError::Interrupted)?;
        let SpillableState::Resident(bytes) = &self.state else {
            return Ok(false);
        };
        let run = scratch.append(cx, bytes.as_ref()).await?;
        self.state = SpillableState::Spilled(run);
        Ok(true)
    }

    /// Returns false when already resident. The old run remains available on
    /// admission failure, corrupt/truncated I/O, or cancellation, permitting a
    /// retry once another batch has released its resident charge.
    pub async fn restore<F>(
        &mut self,
        cx: &QueryCx,
        scratch: &mut SpillFile<F>,
    ) -> Result<bool, SpillError>
    where
        F: asupersync::io::AsyncRead
            + asupersync::io::AsyncWrite
            + asupersync::io::AsyncSeek
            + Unpin,
    {
        cx.checkpoint().map_err(SpillError::Interrupted)?;
        let SpillableState::Spilled(run) = &self.state else {
            return Ok(false);
        };
        let bytes = scratch.restore(cx, run).await?;
        self.state = SpillableState::Resident(bytes);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emergency_bytes_are_never_normal_query_headroom() {
        let pool = MemoryPool::new(100, 20).unwrap();
        let charge = pool.reserve_inner(80).unwrap();
        assert_eq!(pool.available(), 0);
        assert!(matches!(
            pool.reserve_inner(1),
            Err(MemoryError::ResourceExhausted { .. })
        ));
        drop(charge);
        assert_eq!(pool.available(), 80);
        assert_eq!(pool.emergency_reserve(), 20);
        assert!(matches!(
            MemoryPool::new(19, 20),
            Err(MemoryError::InvalidLimits)
        ));
    }

    #[test]
    fn clones_share_admission_and_last_buffer_drop_refunds() {
        let pool = MemoryPool::new(1024, 128).unwrap();
        let other = pool.clone();
        let mut bytes = other.allocate_inner(64, 16).unwrap();
        assert_eq!(pool.used(), bytes.charged_bytes());
        assert!(bytes.charged_bytes() >= 80);
        bytes.as_mut().fill(42);
        assert_eq!(bytes.as_ref(), &[42; 64]);
        drop(other);
        assert!(pool.used() >= 80);
        drop(bytes);
        assert_eq!(pool.used(), 0);
    }

    #[test]
    fn refused_allocations_and_arithmetic_leave_no_reservation() {
        let pool = MemoryPool::new(usize::MAX, 0).unwrap();
        assert!(matches!(
            pool.allocate_inner(usize::MAX, 1),
            Err(MemoryError::SizeOverflow)
        ));
        assert_eq!(pool.used(), 0);
        // Vec rejects a byte capacity beyond isize::MAX before calling malloc.
        assert!(matches!(
            pool.allocate_inner(usize::MAX, 0),
            Err(MemoryError::AllocationFailed { .. })
        ));
        assert_eq!(pool.used(), 0);
    }

    #[test]
    fn independent_pools_do_not_refund_each_other() {
        let left = MemoryPool::new(10, 0).unwrap();
        let right = MemoryPool::new(10, 0).unwrap();
        let charge = left.reserve_inner(10).unwrap();
        assert_eq!(right.available(), 10);
        drop(charge);
        assert_eq!(left.available(), 10);
        assert_eq!(right.available(), 10);
    }

    #[test]
    fn racing_reservations_never_exceed_the_shared_ceiling() {
        let pool = MemoryPool::new(17, 1).unwrap();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let pool = pool.clone();
                scope.spawn(move || {
                    for _ in 0..1000 {
                        if let Ok(charge) = pool.reserve_inner(3) {
                            assert!(pool.used() <= 16);
                            std::thread::yield_now();
                            drop(charge);
                        }
                    }
                });
            }
        });
        assert_eq!(pool.used(), 0);
    }

    #[test]
    fn sibling_operators_share_the_query_and_shard_ceiling() {
        let shard = MemoryPool::new(100, 10).unwrap();
        let query = shard.child(80, 10).unwrap();
        let left = query.child(60, 0).unwrap();
        let right = query.child(60, 0).unwrap();
        let a = left.reserve_inner(50).unwrap();
        assert_eq!(
            (shard.used(), query.used(), left.used(), right.used()),
            (50, 50, 50, 0)
        );
        assert_eq!(right.available(), 20);
        assert_eq!(right.effective_limit(), 60);
        assert!(matches!(
            right.reserve_inner(21),
            Err(MemoryError::ResourceExhausted { limit: 70, .. })
        ));
        assert_eq!((shard.used(), query.used(), right.used()), (50, 50, 0));
        let b = right.reserve_inner(20).unwrap();
        assert_eq!(query.available(), 0);
        drop(a);
        drop(b);
        assert_eq!(
            (shard.used(), query.used(), left.used(), right.used()),
            (0, 0, 0, 0)
        );
    }

    #[test]
    fn local_refusal_rolls_back_every_ancestor() {
        let root = MemoryPool::new(100, 0).unwrap();
        let parent = root.child(80, 0).unwrap();
        let leaf = parent.child(5, 0).unwrap();
        assert!(leaf.reserve_inner(6).is_err());
        assert_eq!((root.used(), parent.used(), leaf.used()), (0, 0, 0));
        let charge = leaf.reserve_inner(5).unwrap();
        let before = (root.used(), parent.used(), leaf.used());
        assert!(leaf.reserve_inner(1).is_err());
        assert_eq!((root.used(), parent.used(), leaf.used()), before);
        drop(charge);
    }

    #[test]
    fn parent_refusal_does_not_consume_sibling_allowance() {
        let root = MemoryPool::new(10, 0).unwrap();
        let a = root.child(100, 0).unwrap();
        let b = root.child(100, 0).unwrap();
        let charge = a.reserve_inner(10).unwrap();
        assert!(b.reserve_inner(1).is_err());
        assert_eq!(b.used(), 0);
        assert_eq!(b.available(), 0);
        assert_eq!(b.effective_limit(), 10);
        drop(charge);
        assert_eq!(b.available(), 10);
    }

    #[test]
    fn live_allocation_keeps_ancestors_alive_without_retaining_dead_children() {
        let root = MemoryPool::new(100, 0).unwrap();
        let child = root.child(80, 0).unwrap();
        let child_budget = Arc::downgrade(&child.budget);
        let parent_budget = Arc::downgrade(&root.budget);
        let bytes = child.allocate_inner(32, 8).unwrap();
        drop(child);
        assert!(child_budget.upgrade().is_some());
        assert!(root.used() >= 40);
        drop(root);
        assert!(parent_budget.upgrade().is_some());
        drop(bytes);
        assert!(child_budget.upgrade().is_none());
        assert!(parent_budget.upgrade().is_none());
    }

    #[test]
    fn failed_child_allocation_refunds_the_whole_path() {
        let root = MemoryPool::new(usize::MAX, 0).unwrap();
        let child = root.child(usize::MAX, 0).unwrap();
        assert!(matches!(
            child.allocate_inner(usize::MAX, 0),
            Err(MemoryError::AllocationFailed { .. })
        ));
        assert_eq!((root.used(), child.used()), (0, 0));
        assert!(matches!(root.child(1, 2), Err(MemoryError::InvalidLimits)));
        assert_eq!(root.used(), 0);
    }

    #[test]
    fn racing_children_cannot_multiply_parent_admission() {
        let root = MemoryPool::new(32, 2).unwrap();
        let query = root.child(24, 3).unwrap();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let child = query.child(12, 0).unwrap();
                let root = root.clone();
                let query = query.clone();
                scope.spawn(move || {
                    for _ in 0..1000 {
                        if let Ok(charge) = child.reserve_inner(3) {
                            assert!(child.used() <= 12);
                            assert!(query.used() <= 21);
                            assert!(root.used() <= 30);
                            std::thread::yield_now();
                            drop(charge);
                        }
                    }
                    assert_eq!(child.used(), 0);
                });
            }
        });
        assert_eq!((root.used(), query.used()), (0, 0));
    }
}
