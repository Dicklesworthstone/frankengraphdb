//! An owned cold read view of the same authenticated Tier-D checkpoint used by
//! resident opens. Graph payloads fault through Strata's charged extent cache;
//! no resident Snapshot or BlockWriter is constructed by this path.

use super::*;
use fgdb_delta_types::PropertyKeyId;
pub use fgdb_strata::store::{BufferedEdge, BufferedReadError, BufferedReadLimits, BufferedValue};
use fgdb_strata::store::{BufferedPartition, BufferedRootSelection};
pub use fgdb_strata::tiered::buffer::{BufferLimits, BufferStats};
use fgdb_strata::tiered::edge_scan::{BufferedEdgeDirection, BufferedEdgeIncidenceScan};
pub use fgdb_strata::tiered::memory::{MemoryError, MemoryPool};
use fgdb_types::{CanonicalPropertyValue, QueryCx};

mod query;
pub(crate) use query::BufferedVertexSeeds;
pub use query::{
    BufferedProjectionCursor, BufferedProjectionError, BufferedProjectionOutputGuard,
    BufferedProjectionRow, BufferedQueryCursor, BufferedQueryError, BufferedQueryRow,
};

#[path = "query_prism/buffered.rs"]
mod prism;
#[path = "query_prism/buffered_cooperative.rs"]
mod prism_cooperative;

/// Failure to acquire an authenticated, bounded cold read view.
#[derive(Debug)]
pub enum BufferedOpenError {
    Open(OpenError),
    Read(BufferedReadError),
    Memory(MemoryError),
    /// The committed chain has no current published checkpoint: none yet, or
    /// a root behind it (a live writer mid-publication, or a crash). Healing
    /// is the writer's or the ordinary writable open's, never an implicit
    /// unbounded fallback.
    RecoveryRequired,
}

impl core::fmt::Display for BufferedOpenError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Open(error) => error.fmt(f),
            Self::Read(error) => error.fmt(f),
            Self::Memory(error) => error.fmt(f),
            Self::RecoveryRequired => f.write_str(
                "buffered open needs a current authenticated checkpoint, and the published root is missing or behind the commit log (a live writer publishes it; otherwise recover through Database::open first)",
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
/// Chronicle's read-only scan remains outside that pool: it retains one bounded
/// frame and the current head per distinct graph/branch while verifying the
/// complete prefix, then releases them before graph admission. A cold open
/// verifies every named graph object and can refuse when its bounded
/// identity/history metadata does not fit.
/// Database::buffered_read_view reuses the live handle's complete admission,
/// reading only descriptors before queries fault the extents they touch.
/// The governed query methods execute native ordered vertex and single-edge
/// GQL profiles through this same cache. Other physical profiles refuse before
/// reading candidates. This is not an assertion that arbitrarily large
/// databases can already be opened.
///
/// As with EmbeddedReadView, callers must retain the database's immutable
/// object directory. This handle does not invent the still-unimplemented
/// cross-process retention/GC lease protocol. It retains no writer lease.
pub struct BufferedReadView<V: Vfs = UnixVfs> {
    partition: BufferedPartition<V>,
    manifest: ManifestVersion,
    projection_cache: crate::query::BufferedProjectionCache,
    sealed_cache: crate::query::BufferedSealedCache,
}

impl<V: Vfs> core::fmt::Debug for BufferedReadView<V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("BufferedReadView([REDACTED])")
    }
}

/// An authenticated checkpoint selected under the caller's buffered read limits.
/// The catalog and graph share one immutable generation. As for
/// [`BufferedReadView`], the caller retains the immutable object directory.
pub struct BufferedReadPublication<V: Vfs = UnixVfs> {
    namespace: DatabaseSecurityNamespaceId,
    published: PublishedRead<V>,
    source: BufferedReadSource<V>,
}

enum BufferedReadSource<V: Vfs> {
    Selected(Box<BufferedRootSelection<V>>),
    Deferred {
        pool: MemoryPool,
        limits: BufferedReadLimits,
    },
}

impl<V: Vfs + Clone> BufferedReadPublication<V> {
    pub(crate) fn security_namespace(&self) -> DatabaseSecurityNamespaceId {
        self.namespace
    }

    /// Frontier of the exact catalog/graph publication selected at open.
    pub fn frontier(&self) -> CommitSeq {
        self.published.published_at
    }

    /// Read the selected generation's symbol history without opening graph payloads.
    pub async fn symbol_catalog(&self, cx: &CommitCx) -> Result<SymbolCatalogReadView, OpenError> {
        SymbolCatalogReadView::from_published(cx, &self.published).await
    }

    /// Admit graph storage for this same checkpoint after query preparation.
    pub async fn into_read_view(
        self,
        cx: &CommitCx,
    ) -> Result<BufferedReadView<V>, BufferedOpenError> {
        self.into_read_view_with_history_counts(cx, 0).await
    }

    /// Admit this same publication with a bounded EId counting prepass for
    /// overlapping V6 block ranges. Zero uses ordinary cold admission. A
    /// positive allowance of at least 32 bytes is drawn from the source pool.
    /// Every extra block visit spends the same source-byte and work budgets.
    pub async fn into_read_view_with_history_counts(
        self,
        cx: &CommitCx,
        bytes: usize,
    ) -> Result<BufferedReadView<V>, BufferedOpenError> {
        let published = self.published;
        let admission: crate::SendFuture<'_, _> = match self.source {
            BufferedReadSource::Selected(selection) => {
                Box::pin(selection.into_partition_with_history_counts(cx, bytes))
            }
            BufferedReadSource::Deferred { pool, limits } => {
                Box::pin(published.store.open_buffered_root_with_history_counts(
                    cx,
                    published.root_id,
                    pool,
                    limits,
                    bytes,
                ))
            }
        };
        Ok(BufferedReadView {
            partition: admission.await?,
            manifest: published.manifest,
            projection_cache: crate::query::BufferedProjectionCache::new(MemoryPool::new(
                crate::query::PROJECTION_CACHE_BYTES,
                0,
            )?),
            sealed_cache: crate::query::BufferedSealedCache::new(MemoryPool::new(
                crate::query::SEALED_CACHE_BYTES,
                0,
            )?),
        })
    }
}

impl<V: Vfs + Clone> BufferedReadView<V> {
    pub(crate) fn partition_mut(&mut self) -> &mut BufferedPartition<V> {
        &mut self.partition
    }

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

    /// Enable durable relation runs for native typed-edge MATCH streams, their
    /// aggregate inputs, typed join children, vertex/edge-rooted probes and
    /// typed standalone incidence streams/counts.
    /// This explicit source profile reserves decoder capacity before loading
    /// a selected run and an EId locator before building its directory. If the
    /// run predates relation writes, its authenticated post-seal blocks remain
    /// separately charged and their original rows join that directory. A join
    /// driver retains at most one independent incident relation beside its
    /// root source, reusing it across outer roots. All source reservations stay
    /// in this view's MemoryPool until their cursor releases them; returned
    /// records retain independent charges.
    ///
    /// A floor-zero run plus its complete post-seal history must cover the
    /// pinned frontier, even for a historical query, so invisible historical
    /// candidates keep their normal identity admission. `max_incidences` also
    /// caps the combined base population and raw suffix statements before
    /// suffix payload loading. Other root shapes keep ordinary buffered scans;
    /// their typed incidents may still select independently eligible runs.
    /// Source selection is lazy; direct streaming LIMIT 0 and close before
    /// first demand do not load a run. External spooling retains complete-input
    /// semantics. Selected-run or suffix failures never retry another source.
    /// Encoded extents reserve their frame and erasure-decoder scratch through
    /// this pool, including when the selected object lives inside a pack.
    pub fn set_native_run_limits(
        &mut self,
        cx: &QueryCx,
        limits: Option<fgdb_strata::tiered::sealed::SealedLimits>,
    ) -> Result<(), BufferedReadError> {
        self.partition.set_native_run_limits(cx, limits)
    }

    /// Retention for completed decoded analytics projections. It uses the
    /// resident projection cache's default allowance and at most eight entries,
    /// independently of source extents and compressed projection retention.
    /// Charges cover retained vertex, name, adjacency and weight capacities,
    /// not build peaks, allocator metadata, caller-held clones or process RSS.
    pub fn projection_read_cache_pool(&self) -> &MemoryPool {
        self.projection_cache.pool()
    }

    pub fn projection_read_cache_usage(&self) -> ReadCacheUsage {
        self.projection_cache.usage()
    }

    /// Replace decoded-projection retention and release this cache's owners.
    /// A zero-limit pool disables reuse without disabling analytics. A child
    /// pool also charges its host's ancestor ceilings. Previously returned
    /// immutable graphs remain valid under their caller-owned allowances.
    pub fn set_projection_read_cache_pool(
        &mut self,
        cx: &QueryCx,
        pool: MemoryPool,
    ) -> Result<ReadCacheUsage, Box<asupersync::error::Error>> {
        self.projection_cache.replace_pool(cx, pool)
    }

    /// Retention for completed compressed analytics projections. This pool is
    /// separate from the source extent pool and defaults to 1 GiB with at most
    /// eight entries. It counts completed image/index/property capacities, not
    /// allocator metadata, active builders, caller-held clones or process RSS.
    pub fn sealed_read_cache_pool(&self) -> &MemoryPool {
        self.sealed_cache.pool()
    }

    pub fn sealed_read_cache_usage(&self) -> ReadCacheUsage {
        self.sealed_cache.usage()
    }

    /// Replace this view's compressed-retention allowance and release its
    /// existing cache ownership. A zero-limit pool explicitly disables reuse;
    /// ordinary preparations still work. A host may supply a child of its own
    /// global budget, so ancestor ceilings constrain every retained admission.
    /// Previously returned graphs stay valid; their clones are caller-owned.
    pub fn set_sealed_read_cache_pool(
        &mut self,
        cx: &QueryCx,
        pool: MemoryPool,
    ) -> Result<ReadCacheUsage, Box<asupersync::error::Error>> {
        self.sealed_cache.replace_pool(cx, pool)
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

    /// Read one current vertex property while retaining its resident charge.
    /// The complete immutable patch remains authenticated; unrelated property
    /// payloads are not decoded into the returned value.
    pub async fn vertex_property(
        &mut self,
        cx: &QueryCx,
        vid: VId,
        key: PropertyKeyId,
    ) -> Result<Option<BufferedValue<CanonicalPropertyValue>>, BufferedReadError> {
        self.vertex_property_at(cx, vid, key, self.frontier()).await
    }

    /// Read one property at an exact historical cut of this view. A missing
    /// vertex or key returns None; a stored NULL remains a present value.
    /// The returned value keeps its reservation after this view is dropped.
    pub async fn vertex_property_at(
        &mut self,
        cx: &QueryCx,
        vid: VId,
        key: PropertyKeyId,
        as_of: CommitSeq,
    ) -> Result<Option<BufferedValue<CanonicalPropertyValue>>, BufferedReadError> {
        self.partition.vertex_property_at(cx, vid, key, as_of).await
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

    /// Read one current edge property while retaining its resident charge.
    /// The authenticated edge winner selects its sidecar before the requested
    /// value is decoded; unrelated property values are not materialized.
    pub async fn edge_property(
        &mut self,
        cx: &QueryCx,
        eid: EId,
        key: PropertyKeyId,
    ) -> Result<Option<BufferedValue<CanonicalPropertyValue>>, BufferedReadError> {
        self.edge_property_at(cx, eid, key, self.frontier()).await
    }

    /// Read one property at an exact historical cut of this view. A missing
    /// edge or key returns None; a stored NULL remains a present value.
    /// The returned value keeps its reservation after this view is dropped.
    pub async fn edge_property_at(
        &mut self,
        cx: &QueryCx,
        eid: EId,
        key: PropertyKeyId,
        as_of: CommitSeq,
    ) -> Result<Option<BufferedValue<CanonicalPropertyValue>>, BufferedReadError> {
        self.partition.edge_property_at(cx, eid, key, as_of).await
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

    /// Pull visible incident edges and their same-cut endpoints without building
    /// a neighborhood vector. Incoming/outgoing/undirected requests preserve
    /// stored edge orientation; an undirected self-loop is returned once and
    /// parallel EIds remain distinct. `None` selects all relations, not a grant.
    ///
    /// Unlike adjacency(), this stream is ordered by EId, NOT by destination.
    /// Each output owns its existing MemoryPool reservations independently of
    /// the cursor and view. Callers retaining outputs retain their charges too.
    /// The first pull seeks authenticated block descriptors and seeds persistent
    /// merge heads, without building per-query routing from graph payloads.
    /// Initial descriptor metadata and individual records can still exceed
    /// admission. No resident Snapshot, eager fallback,
    /// complete result vector or cross-process retention lease is introduced.
    /// When native run limits are enabled, typed requests may instead retain
    /// the same durable run-plus-suffix source used by native query joins.
    /// Source admission remains lazy and charged; selected-source failures
    /// refuse without retrying the ordinary Tier-D representation.
    pub fn stream_adjacency(
        &mut self,
        cx: &QueryCx,
        vertex: VId,
        relation: Option<RelationId>,
        direction: BufferedEdgeDirection,
    ) -> Result<BufferedEdgeIncidenceScan<'_, V>, BufferedReadError> {
        let as_of = self.frontier();
        self.stream_adjacency_at(cx, vertex, relation, direction, as_of, None)
    }

    /// The same owner-level stream at an exact historical cut. `after` excludes
    /// that EId and every smaller identity; it is not a durable continuation
    /// token. Future cuts refuse before opening a scan. No identity increment
    /// or narrowing is used, including at u128::MAX. The caller supplies QueryCx
    /// on every pull; close, error or a dropped polled pull permanently fuses
    /// the stream. Earlier rows are delivered, not a promise of complete output.
    pub fn stream_adjacency_at(
        &mut self,
        cx: &QueryCx,
        vertex: VId,
        relation: Option<RelationId>,
        direction: BufferedEdgeDirection,
        as_of: CommitSeq,
        after: Option<EId>,
    ) -> Result<BufferedEdgeIncidenceScan<'_, V>, BufferedReadError> {
        self.partition
            .edge_scan(cx, as_of)
            .map(|scan| scan.into_incidence_scan(vertex, relation, direction, after))
    }

    /// Count visible incident edge identities without collecting their rows.
    /// A self-loop counts ONCE even in the undirected case; this is not the
    /// graph-theoretic degree convention that counts a loop twice. Parallel
    /// edges each count once. No partial count escapes a source/work/memory or
    /// cancellation failure. All history and endpoint reads use one cumulative
    /// scan allowance, and this does not bypass owner-level visibility rules.
    /// Edge-property sidecars are not faulted and endpoint properties are not
    /// decoded for a count. Initial open still validates all property payloads;
    /// current endpoint faults authenticate bytes and resolve same-cut history.
    pub async fn incident_edge_count(
        &mut self,
        cx: &QueryCx,
        vertex: VId,
        relation: Option<RelationId>,
        direction: BufferedEdgeDirection,
    ) -> Result<u64, BufferedReadError> {
        let as_of = self.frontier();
        self.incident_edge_count_at(cx, vertex, relation, direction, as_of)
            .await
    }

    /// Exact-cut counterpart of incident_edge_count(). Missing or retired
    /// vertices contribute no visible incidence; malformed dangling histories
    /// still refuse through the canonical endpoint resolver.
    pub async fn incident_edge_count_at(
        &mut self,
        cx: &QueryCx,
        vertex: VId,
        relation: Option<RelationId>,
        direction: BufferedEdgeDirection,
        as_of: CommitSeq,
    ) -> Result<u64, BufferedReadError> {
        self.stream_adjacency_at(cx, vertex, relation, direction, as_of, None)?
            .count_visible(cx)
            .await
    }
}

impl Database<UnixVfs> {
    /// Open a current checkpoint with charged extent-backed graph reads.
    ///
    /// This authenticates the same slot, manifest coordinate and Chronicle
    /// chain binding as open_read_view. It admits graph objects under explicit
    /// bounds, takes no writer lease and writes nothing, so it reads beside a
    /// live writer. A missing or lagging checkpoint yields RecoveryRequired;
    /// it does not allocate a full resident database as a fallback. The
    /// caller owns the pool and chooses both metadata and graph read limits.
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
    /// Capture this healthy writer's exact publication for deferred cold reads.
    ///
    /// This copies only the immutable store, root, manifest, frontier and catalog
    /// identities. It performs no storage I/O, reopens no independently selected
    /// root, and retains neither the resident snapshot nor the writer lease.
    /// Later writes, compaction and writer drop cannot change the selected cut.
    ///
    /// The returned publication can be consumed by
    /// [`BufferedReadPublication::into_authorized_read_session`], which verifies
    /// the signed capability before catalog or graph admission. Graph admission
    /// remains the ordinary bounded cold validation: this handle does not copy
    /// the writer's private receipts or manufacture an admission bypass. Use
    /// [`Self::buffered_read_view`] when a trusted host instead wants immediate
    /// receipt-backed admission of a privileged read view.
    ///
    /// The caller must retain the immutable object directory while the
    /// publication or its readers are live. This is an in-process selection,
    /// not the still-unimplemented cross-process retention lease.
    pub fn buffered_read_publication(
        &self,
        cx: &QueryCx,
        pool: MemoryPool,
        limits: BufferedReadLimits,
    ) -> Result<BufferedReadPublication<V>, BufferedOpenError> {
        self.ensure_readable()
            .map_err(|_| BufferedOpenError::RecoveryRequired)?;
        cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
        Ok(BufferedReadPublication {
            namespace: self.keys.namespace,
            published: PublishedRead {
                store: self.store.clone().with_buffered_reads(pool.clone()),
                root_id: self.snapshot.root,
                manifest: self.snapshot.manifest,
                published_at: self.snapshot.frontier,
                schema: self.snapshot.schema.reference,
            },
            source: BufferedReadSource::Deferred { pool, limits },
        })
    }

    /// Capture this healthy handle's exact published generation as an owned
    /// buffered source. Its opaque publication receipts already prove every
    /// payload, chain link and history law, so constructing another source
    /// reads only authenticated metadata. Queries fault and authenticate
    /// individual extents through the supplied pool. The resulting view owns
    /// no writer lease and remains fixed across later writes or writer drop.
    pub async fn buffered_read_view(
        &self,
        cx: &QueryCx,
        pool: MemoryPool,
        limits: BufferedReadLimits,
    ) -> Result<BufferedReadView<V>, BufferedOpenError> {
        self.ensure_readable()
            .map_err(|_| BufferedOpenError::RecoveryRequired)?;
        let admission: crate::SendFuture<'_, _> =
            Box::pin(self.store.open_buffered_root_with_receipts(
                cx,
                self.snapshot.root,
                &self.receipts,
                pool,
                limits,
            ));
        let partition = admission.await?;
        Ok(BufferedReadView {
            partition,
            manifest: self.snapshot.manifest,
            projection_cache: crate::query::BufferedProjectionCache::new(MemoryPool::new(
                crate::query::PROJECTION_CACHE_BYTES,
                0,
            )?),
            sealed_cache: crate::query::BufferedSealedCache::new(MemoryPool::new(
                crate::query::SEALED_CACHE_BYTES,
                0,
            )?),
        })
    }

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
        Self::open_buffered_publication_with_vfs(cx, vfs, path, keys, pool, limits)
            .await?
            .into_read_view(cx)
            .await
    }

    /// Read symbol bindings at a current publication under the buffered reader's
    /// root-byte, reference-count, and metadata-memory limits. Graph payloads
    /// remain unopened. The returned catalog owns its schema independently.
    pub async fn open_buffered_symbol_catalog_with_vfs(
        cx: &CommitCx,
        vfs: V,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
        pool: &MemoryPool,
        limits: BufferedReadLimits,
    ) -> Result<SymbolCatalogReadView, BufferedOpenError> {
        let BufferedReadPublication {
            published, source, ..
        } = Self::open_buffered_publication_with_vfs(cx, vfs, path, keys, pool.clone(), limits)
            .await?;
        // A caller requesting only names does not need to retain the graph's
        // decoded root while it reads the independently owned catalog.
        drop(source);
        Ok(SymbolCatalogReadView::from_published(cx, &published).await?)
    }

    /// Authenticate the current checkpoint once for catalog binding and subsequent graph reads.
    pub async fn open_buffered_publication_with_vfs(
        cx: &CommitCx,
        vfs: V,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
        pool: MemoryPool,
        limits: BufferedReadLimits,
    ) -> Result<BufferedReadPublication<V>, BufferedOpenError> {
        Self::select_buffered_publication(cx, vfs, path.as_ref(), keys, pool, limits, None)
            .await
            .map(|(publication, _)| publication)
    }

    /// Select one bounded current publication and prove it contains a specific
    /// historical branch marker. Returns `None` if the complete verified chain
    /// has no such coordinate update. The returned publication has its own
    /// current frontier; use `marker.commit_seq` for both catalog and reads.
    /// A lagging checkpoint or a damaged captured suffix still refuses.
    pub async fn open_buffered_publication_containing_revision(
        cx: &CommitCx,
        vfs: V,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
        pool: MemoryPool,
        limits: BufferedReadLimits,
        marker: fgdb_types::MarkerRef,
    ) -> Result<Option<BufferedReadPublication<V>>, BufferedOpenError> {
        let (publication, matched) = Self::select_buffered_publication(
            cx,
            vfs,
            path.as_ref(),
            keys,
            pool,
            limits,
            Some(marker),
        )
        .await?;
        Ok(matched.then_some(publication))
    }

    async fn select_buffered_publication(
        cx: &CommitCx,
        vfs: V,
        path: &Path,
        keys: DatabaseKeys,
        pool: MemoryPool,
        limits: BufferedReadLimits,
        marker: Option<fgdb_types::MarkerRef>,
    ) -> Result<(BufferedReadPublication<V>, bool), BufferedOpenError> {
        require_database_dir(cx, &vfs, path).await?;
        // Like the resident read view's fast path, this takes no writer lease
        // and writes nothing (fgdb-xxxnb): the committed chain is scanned
        // without repairing a torn tail, and the store is opened only to read.
        // A lagging root refuses below, so nothing here ever needs to heal.
        let probe = RootStore::with_vfs(vfs.clone(), path);
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
        // dropped. Refuse an impossible budget before reading pack tables.
        // The store checks frame counts before flattening V4 roots.
        let selection_charge = pool.reserve(cx, limits.metadata_bytes()?)?;
        let Some(store) = open_block_store_for_read(cx, &vfs, path, &keys).await? else {
            return Err(BufferedOpenError::RecoveryRequired);
        };
        let store = store.with_buffered_reads(pool.clone());
        let resolved = store
            .resolve_manifest_bounded(cx, manifest, limits, selection_charge)
            .await?;
        // Scanned after the slot was read: a writer appends a marker before
        // it publishes the root that names it, so this chain reaches the
        // slot's frontier.
        let requested = resolved
            .as_ref()
            .map_or(CommitSeq::ORIGIN, |(_, selection)| {
                selection.root().published_at
            });
        let (scan, matched) = match marker {
            Some(at) => {
                CommitCoordinator::scan_committed_branch_prefix(
                    cx,
                    &vfs,
                    path,
                    requested,
                    crate::GRAPH,
                    crate::BRANCH,
                    at,
                )
                .await
            }
            None => CommitCoordinator::scan_committed_prefix(cx, &vfs, path, requested)
                .await
                .map(|scan| (scan, false)),
        }
        .map_err(OpenError::Commit)?;
        let (record, selection) = resolved.ok_or_else(|| OpenError::SlotDisagreesWithStream {
            path: path.to_path_buf(),
            slot_manifest: manifest.0,
        })?;
        let checkpoint = checkpoint_from_manifest_record(
            |at| scan.checkpoint_commitment(at),
            manifest,
            &record,
            selection.root(),
            path,
        )?;
        if checkpoint.published_at != scan.frontier() {
            return Err(BufferedOpenError::RecoveryRequired);
        }
        let source = if keys.strata_ecs.is_some() && checkpoint.schema.is_some() {
            // Protected catalog decoding uses this same pool. Preserve its
            // independent admission window before reopening the pinned graph.
            drop(selection);
            BufferedReadSource::Deferred { pool, limits }
        } else {
            BufferedReadSource::Selected(Box::new(selection))
        };
        Ok((
            BufferedReadPublication {
                namespace: keys.namespace,
                published: PublishedRead {
                    store,
                    root_id: checkpoint.root_id,
                    manifest,
                    published_at: checkpoint.published_at,
                    schema: checkpoint.schema,
                },
                source,
            },
            matched,
        ))
    }
}
