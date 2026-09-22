//! Shared resident-byte admission with affine, automatically refunded charges.
//!
//! A snapshot lease protects a GENERATION, not its RAM. Charges belong to actual
//! allocations and can outlive the pool handle or a cache entry. Normal callers
//! cannot spend the emergency reserve. These counters account requested vector
//! capacity and explicitly supplied metadata, not allocator headers, host RSS,
//! overcommit, or unrelated foundation allocations.

use core::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use fgdb_types::StorageReadCx;

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
            Self::ResourceExhausted { requested, available, limit } => write!(
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
}

/// Clones share one budget; cloning does not mint additional admission.
#[derive(Clone, Debug)]
pub struct MemoryPool {
    budget: Arc<Budget>,
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
        })
    }

    /// Spendable regular bytes; the emergency reserve is deliberately excluded.
    pub fn limit(&self) -> usize {
        self.budget.limit
    }

    pub fn emergency_reserve(&self) -> usize {
        self.budget.emergency
    }

    pub fn used(&self) -> usize {
        self.budget.used.load(Ordering::Acquire)
    }

    /// Advisory observation only. Admission itself uses a checked atomic CAS.
    pub fn available(&self) -> usize {
        self.limit() - self.used()
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

    pub(super) fn reserve_inner(&self, bytes: usize) -> Result<MemoryCharge, MemoryError> {
        self.budget.acquire(bytes)?;
        Ok(MemoryCharge {
            budget: Arc::clone(&self.budget),
            bytes,
        })
    }

    pub(super) fn allocate_inner(
        &self,
        bytes: usize,
        metadata: usize,
    ) -> Result<TrackedBytes, MemoryError> {
        let requested = bytes.checked_add(metadata).ok_or(MemoryError::SizeOverflow)?;
        let mut charge = self.reserve_inner(requested)?;
        let mut data = Vec::new();
        data.try_reserve_exact(bytes)
            .map_err(|_| MemoryError::AllocationFailed { requested: bytes })?;
        // reserve_exact is permitted to return a larger logical capacity. It
        // must be charged before the allocation can escape to any consumer.
        let actual = data.capacity().checked_add(metadata).ok_or(MemoryError::SizeOverflow)?;
        if actual > charge.bytes {
            charge.budget.acquire(actual - charge.bytes)?;
            charge.bytes = actual;
        }
        data.resize(bytes, 0);
        Ok(TrackedBytes { data, charge })
    }
}

/// An affine byte reservation. There is deliberately no Clone or public refund.
#[derive(Debug)]
pub struct MemoryCharge {
    budget: Arc<Budget>,
    bytes: usize,
}

impl MemoryCharge {
    pub const fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for MemoryCharge {
    fn drop(&mut self) {
        let previous = self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
        debug_assert!(previous >= self.bytes, "a resident charge was refunded twice");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emergency_bytes_are_never_normal_query_headroom() {
        let pool = MemoryPool::new(100, 20).unwrap();
        let charge = pool.reserve_inner(80).unwrap();
        assert_eq!(pool.available(), 0);
        assert!(matches!(pool.reserve_inner(1), Err(MemoryError::ResourceExhausted { .. })));
        drop(charge);
        assert_eq!(pool.available(), 80);
        assert_eq!(pool.emergency_reserve(), 20);
        assert!(matches!(MemoryPool::new(19, 20), Err(MemoryError::InvalidLimits)));
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
        assert!(matches!(pool.allocate_inner(usize::MAX, 1), Err(MemoryError::SizeOverflow)));
        assert_eq!(pool.used(), 0);
        // Vec rejects a byte capacity beyond isize::MAX before calling malloc.
        assert!(matches!(pool.allocate_inner(usize::MAX, 0), Err(MemoryError::AllocationFailed { .. })));
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
}
