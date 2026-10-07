//! An owned cold read view of the same authenticated Tier-D checkpoint used by
//! resident opens. Graph payloads fault through Strata's charged extent cache;
//! no resident Snapshot or BlockWriter is constructed by this path.

use super::*;
use fgdb_strata::store::BufferedPartition;
pub use fgdb_strata::store::{BufferedEdge, BufferedReadError, BufferedReadLimits, BufferedValue};
pub use fgdb_strata::tiered::buffer::{BufferLimits, BufferStats};
pub use fgdb_strata::tiered::memory::{MemoryError, MemoryPool};
use fgdb_types::QueryCx;

/// Failure to acquire an authenticated, bounded cold read view.
#[derive(Debug)]
pub enum BufferedOpenError {
    Open(OpenError),
    Read(BufferedReadError),
    Memory(MemoryError),
    /// The recovered chain has no current published checkpoint. Healing uses
    /// the ordinary writable open and is never an implicit unbounded fallback.
    RecoveryRequired,
}

impl core::fmt::Display for BufferedOpenError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Open(error) => error.fmt(f),
            Self::Read(error) => error.fmt(f),
            Self::Memory(error) => error.fmt(f),
            Self::RecoveryRequired => f.write_str(
                "buffered open needs a current authenticated checkpoint; recover through Database::open first",
            ),
        }
    }
}

impl core::error::Error for BufferedOpenError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Open(error) => Some(error),
            Self::Read(error) => Some(error),
            Self::Memory(error) => Some(error),
            Self::RecoveryRequired => None,
        }
    }
}

impl From<OpenError> for BufferedOpenError {
    fn from(error: OpenError) -> Self {
        Self::Open(error)
    }
}

impl From<BufferedReadError> for BufferedOpenError {
    fn from(error: BufferedReadError) -> Self {
        Self::Read(error)
    }
}

impl From<MemoryError> for BufferedOpenError {
    fn from(error: MemoryError) -> Self {
        Self::Memory(error)
    }
}

/// A fixed authenticated generation with bounded decoded graph residency.
///
/// Each read takes QueryCx and faults immutable objects through the extent
/// cache. Returned values retain their memory reservations independently of
/// this view. Holding the view does not pin every payload in RAM, and later
/// commits or compaction cannot change its selected root.
///
/// The metadata and graph read allowances belong to the supplied MemoryPool.
/// Chronicle recovery and its marker-chain metadata remain outside that pool,
/// as in the ordinary opener. Initial admission verifies every named graph
/// object, and can refuse when its bounded identity/history metadata does not
/// fit. This is an async point/adjacency surface, not general GQL execution or
/// an assertion that arbitrarily large databases can already be opened.
///
/// As with EmbeddedReadView, callers must retain the database's immutable
/// object directory. This handle does not invent the still-unimplemented
/// cross-process retention/GC lease protocol. It retains no writer lease.
pub struct BufferedReadView<V: Vfs = UnixVfs> {
    partition: BufferedPartition<V>,
    manifest: ManifestVersion,
}

impl<V: Vfs> core::fmt::Debug for BufferedReadView<V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("BufferedReadView([REDACTED])")
    }
}

impl<V: Vfs + Clone> BufferedReadView<V> {
    pub fn frontier(&self) -> CommitSeq {
        self.partition.root().published_at
    }

    pub fn manifest(&self) -> ManifestVersion {
        self.manifest
    }

    pub fn partition_root(&self) -> PartitionRootVersion {
        self.partition.root_id()
    }

    pub fn buffer_stats(&self) -> BufferStats {
        self.partition.stats()
    }

    pub fn memory_pool(&self) -> &MemoryPool {
        self.partition.pool()
    }

    pub async fn vertex(
        &mut self,
        cx: &QueryCx,
        vid: VId,
    ) -> Result<Option<BufferedValue<VertexRow>>, BufferedReadError> {
        self.vertex_at(cx, vid, self.frontier()).await
    }

    pub async fn vertex_at(
        &mut self,
        cx: &QueryCx,
        vid: VId,
        as_of: CommitSeq,
    ) -> Result<Option<BufferedValue<VertexRow>>, BufferedReadError> {
        self.partition.vertex_at(cx, vid, as_of).await
    }

    pub async fn edge(
        &mut self,
        cx: &QueryCx,
        eid: EId,
    ) -> Result<Option<BufferedValue<BufferedEdge>>, BufferedReadError> {
        self.edge_at(cx, eid, self.frontier()).await
    }

    pub async fn edge_at(
        &mut self,
        cx: &QueryCx,
        eid: EId,
        as_of: CommitSeq,
    ) -> Result<Option<BufferedValue<BufferedEdge>>, BufferedReadError> {
        self.partition.edge_at(cx, eid, as_of).await
    }

    /// Read bounded outgoing incidence. Parallel edges remain distinct and
    /// rows use canonical (source, relation, destination, EId) order. None selects all
    /// relation types; this owner-level API is not a capability-masked session.
    pub async fn adjacency(
        &mut self,
        cx: &QueryCx,
        vertex: VId,
        relation: Option<RelationId>,
        max_entries: usize,
    ) -> Result<BufferedValue<Vec<AdjacencyEntry>>, BufferedReadError> {
        self.adjacency_at(cx, vertex, relation, false, self.frontier(), max_entries)
            .await
    }

    /// Select incoming or outgoing incidence at a fixed historical cut. The
    /// entry limit is an explicit refusal bound, never silent truncation.
    pub async fn adjacency_at(
        &mut self,
        cx: &QueryCx,
        vertex: VId,
        relation: Option<RelationId>,
        incoming: bool,
        as_of: CommitSeq,
        max_entries: usize,
    ) -> Result<BufferedValue<Vec<AdjacencyEntry>>, BufferedReadError> {
        self.partition
            .adjacency_at(cx, vertex, relation, incoming, as_of, max_entries)
            .await
    }
}

impl Database<UnixVfs> {
    /// Open a current checkpoint with charged extent-backed graph reads.
    ///
    /// This authenticates the same slot, manifest coordinate and Chronicle
    /// chain binding as open_read_view. It admits graph objects under explicit
    /// bounds and releases the writer lease before returning. A missing or
    /// lagging checkpoint yields RecoveryRequired; it does not allocate a full
    /// resident database as a fallback. The caller owns the pool and chooses
    /// both metadata and graph read limits.
    pub async fn open_buffered_read_view(
        cx: &CommitCx,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
        pool: MemoryPool,
        limits: BufferedReadLimits,
    ) -> Result<BufferedReadView<UnixVfs>, BufferedOpenError> {
        Self::open_buffered_read_view_with_vfs(cx, UnixVfs::new(), path, keys, pool, limits).await
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Buffered open through the same injected filesystem as Chronicle and
    /// Strata. The native lab/fault seam, with identical production admission.
    pub async fn open_buffered_read_view_with_vfs(
        cx: &CommitCx,
        vfs: V,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
        pool: MemoryPool,
        limits: BufferedReadLimits,
    ) -> Result<BufferedReadView<V>, BufferedOpenError> {
        let path = path.as_ref();
        require_database_dir(cx, &vfs, path).await?;
        let coordinator =
            CommitCoordinator::open_with_vfs(cx, vfs.clone(), path, keys.capsule_keys())
                .await
                .map_err(OpenError::Commit)?;
        let store = open_block_store(cx, &vfs, path, &keys).await?;
        let probe = RootStore::with_vfs(vfs, path);
        let slot = match probe.current(cx).await {
            Ok(slot) => slot,
            Err(SlotStoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(BufferedOpenError::RecoveryRequired);
            }
            Err(error) => return Err(OpenError::Slot(error).into()),
        };
        validate_plain_slot(&slot, &keys, path)?;
        let manifest = ManifestVersion(ObjectId(slot.root_manifest_oid));
        // Keep this reservation until all decoded selection metadata has
        // dropped. The store checks frame counts before flattening V4 roots.
        let selection_charge = pool.reserve(cx, limits.metadata_bytes()?)?;
        let resolved = store
            .resolve_manifest_bounded(cx, manifest, &limits)
            .await?;
        let checkpoint = checkpoint_from_manifest(coordinator.chain(), manifest, &resolved, path)?;
        let chain_frontier = coordinator
            .chain()
            .entries()
            .last()
            .map_or(CommitSeq::ORIGIN, |entry| {
                CommitSeq(entry.marker.commit_seq)
            });
        if checkpoint.published_at != chain_frontier {
            return Err(BufferedOpenError::RecoveryRequired);
        }
        drop(resolved);
        drop(selection_charge);
        let partition = store
            .open_buffered_root(cx, checkpoint.root_id, pool, limits)
            .await?;
        // The owned reader contains the authenticated immutable root and store,
        // not the coordinator. Returning drops its exclusive writer lease.
        Ok(BufferedReadView {
            partition,
            manifest,
        })
    }
}
