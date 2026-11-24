//! Host-owned physical admission for capability-authorized FGP cold reads.

use crate::ServerError;
use crate::execute::Refusal;
use fgdb::{BufferLimits, BufferedReadLimits, MemoryPool};
use fgdb_protocol::body::ErrorCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Select bounded cold-storage sources for FGP `ReadStream` and prepared
/// `SourceStream`, plus `ReadOrdered` and prepared `OrderedStream` when query
/// scratch is configured. Both routes share this pool and concurrency limit;
/// ordered reads also retain their separate scratch admission.
///
/// This profile requires durable catalog names. Exact operator redeclarations
/// are accepted; process-local names absent from the selected cut refuse.
/// The writer and server startup still use the resident database. This policy
/// changes query source residency, not the whole server's memory contract.
/// Difference reads retain their resident two-cut execution owner.
#[derive(Clone, Debug)]
pub struct BufferedReadConfig {
    /// Shared metadata, extent-cache and native row reservations across reads.
    /// Wire frames and foundation I/O buffers have their existing separate bounds.
    pub memory_bytes: usize,
    /// Active cold reads, including ordering and reads suspended on client credit.
    pub max_concurrent: usize,
    /// Physical root, history, extent and cumulative scan work ceilings.
    pub limits: BufferedReadLimits,
}

impl BufferedReadConfig {
    #[must_use]
    pub fn new(memory_bytes: usize) -> Self {
        let root = (memory_bytes / 64).min(1024 * 1024);
        Self {
            memory_bytes,
            max_concurrent: 4,
            limits: BufferedReadLimits {
                max_root_bytes: root,
                max_source_bytes: 1024 * 1024 * 1024,
                max_blocks: root,
                max_vertex_patches: root,
                max_work: 10_000_000,
                buffer: BufferLimits {
                    max_frames: 32,
                    max_ghost_entries: 64,
                    max_extent_bytes: 16 * 1024,
                },
            },
        }
    }
}

pub(crate) struct BufferedReads {
    config: BufferedReadConfig,
    pool: MemoryPool,
    active: Arc<AtomicUsize>,
}

/// This slot outlives the native source, including while the client has no
/// credit. Dropping any opening/pull/delivery future releases the same slot.
pub(crate) struct BufferedLease(Arc<AtomicUsize>);

impl Drop for BufferedLease {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl BufferedReads {
    pub(crate) fn new(config: BufferedReadConfig) -> Result<Self, ServerError> {
        if config.memory_bytes == 0
            || config.max_concurrent == 0
            || config.limits.max_root_bytes == 0
            || config.limits.max_source_bytes == 0
            || config.limits.max_work == 0
            || config.limits.buffer.max_frames == 0
            || config.limits.buffer.max_extent_bytes == 0
        {
            return Err(ServerError::InvalidLimits);
        }
        config
            .limits
            .metadata_bytes()
            .map_err(|_| ServerError::InvalidLimits)?;
        let pool =
            MemoryPool::new(config.memory_bytes, 0).map_err(|_| ServerError::InvalidLimits)?;
        Ok(Self {
            config,
            pool,
            active: Arc::new(AtomicUsize::new(0)),
        })
    }

    #[cfg(test)]
    pub(crate) fn pool(&self) -> &MemoryPool {
        &self.pool
    }

    /// Authentication precedes this admission; requests cannot replace any
    /// physical limit or select a separate pool to evade retained charges.
    pub(crate) fn admit(&self) -> Result<(MemoryPool, BufferedReadLimits, BufferedLease), Refusal> {
        self.active
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < self.config.max_concurrent).then(|| count + 1)
            })
            .map_err(|_| {
                Refusal::new(ErrorCode::Budget, "buffered read concurrency limit reached")
            })?;
        Ok((
            self.pool.clone(),
            self.config.limits,
            BufferedLease(Arc::clone(&self.active)),
        ))
    }
}
