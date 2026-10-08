//! An admitted immutable partition whose payloads fault through ExtentBuffer.
//!
//! Opening proves the ordinary root history laws, retaining only descriptors
//! afterwards. Vertex admission retains charged lifecycle metadata and birth
//! locators; restatements refault their authenticated birth patch for exact
//! payload comparison. Resident admission therefore depends on version count,
//! not the sum of vertex property bytes. A metadata-heavy history may still
//! refuse; this is not an unbounded external history validator. No durable
//! format changes or permission to reclaim objects follow from this reader.

mod admission;
mod scan_access;

use super::{BlockStore, RootReadEvent, RootWalk, StoreError};
use crate::edge_props::{BlockProps, EdgePropertyRow};
use crate::root::PartitionRoot;
use crate::tiered::buffer::{
    Admission, BufferError, BufferHandle, BufferLimits, BufferStats, ExtentBuffer, ExtentKey,
    PreparedExtent, extent_checksum,
};
use crate::tiered::memory::{MemoryCharge, MemoryError, MemoryPool};
use crate::vertex::{VertexPatchRows, VertexRow};
use crate::{AdjacencyEntry, PartitionRootVersion};
use asupersync::fs::Vfs;
use asupersync::io::AsyncReadExt;
use fgdb_delta_types::RelationId;
use fgdb_types::{CommitCx, CommitSeq, EId, QueryCx, StorageReadCx, VId};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

// One stored payload is at most 16 KiB, with at most 256 rows. This allowance
// covers two encoded objects, their decoded vectors/scalars, temporary scalar
// validation encodings, and a property's per-entry digest transcript. It is a
// conservative database charge, not allocator/RSS telemetry. Admission and each
// read reserve it BEFORE any payload allocation or I/O.
const OBJECT_WORKSPACE_BYTES: usize = super::MAX_STORED_OBJECT_BYTES as usize * 64 + 65_536;
const HISTORY_ENTRY_BYTES: usize = 512;

/// Storage contexts with a cancellation checkpoint; no new effect is granted.
pub trait BufferedReadCx: StorageReadCx {
    fn buffered_checkpoint(&self) -> Result<(), Box<asupersync::error::Error>>;
}

impl BufferedReadCx for CommitCx {
    fn buffered_checkpoint(&self) -> Result<(), Box<asupersync::error::Error>> {
        self.checkpoint()
    }
}

impl BufferedReadCx for QueryCx {
    fn buffered_checkpoint(&self) -> Result<(), Box<asupersync::error::Error>> {
        self.checkpoint()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufferedReadLimits {
    /// Combined root frame and root-segment encoded byte ceiling. Also bounds
    /// the manifest used by the native single-partition selector.
    pub max_root_bytes: usize,
    /// Encoded payload bytes inspected during initial admission, including
    /// birth-patch refaults needed to validate vertex restatements. As with
    /// RootReadLimits, at most one format-bounded object has been read when this
    /// source limit refuses; the resident reservation always precedes the read.
    pub max_source_bytes: usize,
    pub max_blocks: usize,
    pub max_vertex_patches: usize,
    /// Object visits plus decoded row visits, separately per open/read call.
    /// Scans also count head initialization/consumption and share this one
    /// cumulative ceiling across every pull, including edge endpoint reads.
    pub max_work: usize,
    pub buffer: BufferLimits,
}

impl BufferedReadLimits {
    /// Reserve before decoding a manifest/root. This covers simultaneous
    /// encoded frames, V4 segment vectors, flattened references, descriptors,
    /// and bounded cache-policy bookkeeping. Payload and history use separate
    /// reservations. The allowance intentionally remains attached to the view.
    pub fn metadata_bytes(self) -> Result<usize, BufferedReadError> {
        self.max_root_bytes
            .checked_mul(16)
            .and_then(|bytes| {
                self.buffer
                    .max_frames
                    .checked_add(self.buffer.max_ghost_entries)
                    .and_then(|entries| entries.checked_mul(512))
                    .and_then(|policy| bytes.checked_add(policy))
            })
            .and_then(|bytes| bytes.checked_add(4096))
            .ok_or(BufferedReadError::SizeOverflow)
    }
}

#[derive(Debug)]
pub enum BufferedReadError {
    Store(Box<StoreError>),
    Buffer(BufferError),
    Memory(MemoryError),
    Interrupted(Box<asupersync::error::Error>),
    Limit {
        resource: &'static str,
        requested: usize,
        limit: usize,
    },
    BeyondPublication {
        requested: CommitSeq,
        publication: CommitSeq,
    },
    /// The ordinary validator found incompatible rows for this identity. The
    /// buffered path returns no property-bearing conflict rows, whose admission
    /// reservations end when the failed open unwinds.
    VertexHistoryConflict {
        vid: VId,
    },
    /// A visible edge has no visible source or target at the same cut.
    DanglingEndpoint,
    SizeOverflow,
}

impl From<StoreError> for BufferedReadError {
    fn from(error: StoreError) -> Self {
        Self::Store(Box::new(error))
    }
}
impl From<BufferError> for BufferedReadError {
    fn from(error: BufferError) -> Self {
        Self::Buffer(error)
    }
}
impl From<MemoryError> for BufferedReadError {
    fn from(error: MemoryError) -> Self {
        Self::Memory(error)
    }
}
impl core::fmt::Display for BufferedReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Store(error) => error.fmt(f),
            Self::Buffer(error) => error.fmt(f),
            Self::Memory(error) => error.fmt(f),
            Self::Interrupted(_) => f.write_str("buffered read interrupted"),
            Self::Limit {
                resource,
                requested,
                limit,
            } => write!(
                f,
                "ResourceExhausted: {resource} needs {requested}, limit {limit}"
            ),
            Self::BeyondPublication {
                requested,
                publication,
            } => write!(
                f,
                "buffered read cut {} exceeds publication {}",
                requested.0, publication.0
            ),
            Self::VertexHistoryConflict { vid } => write!(
                f,
                "buffered root has incompatible vertex history for {vid:?}"
            ),
            Self::SizeOverflow => f.write_str("buffered read accounting overflow"),
            Self::DanglingEndpoint => f.write_str("buffered edge has a missing visible endpoint"),
        }
    }
}
impl std::error::Error for BufferedReadError {}

/// A returned value retains its own resident charge after the read ends.
/// Dropping the view does not refund a still-live answer's allocation.
#[derive(Debug)]
pub struct BufferedValue<T> {
    value: T,
    _charge: MemoryCharge,
}
impl<T> BufferedValue<T> {
    pub(crate) fn from_reserved(value: T, charge: MemoryCharge) -> Self {
        Self {
            value,
            _charge: charge,
        }
    }

    /// The conservative reservation retained by this value. This is database
    /// accounting, not allocator/RSS telemetry or permission to detach a clone.
    pub const fn charged_bytes(&self) -> usize {
        self._charge.bytes()
    }
}
impl<T> AsRef<T> for BufferedValue<T> {
    fn as_ref(&self) -> &T {
        &self.value
    }
}
impl<T> core::ops::Deref for BufferedValue<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferedEdge {
    pub entry: AdjacencyEntry,
    pub props: EdgePropertyRow,
}

#[derive(Clone, Copy)]
struct BlockDescriptor {
    block: ExtentKey,
    properties: Option<ExtentKey>,
    // Adjacency order is not EId order. This is the minimum identity/version
    // in the authenticated block, not necessarily its first physical row.
    first_edge: Option<(EId, CommitSeq)>,
    rows: usize,
}

#[derive(Clone, Copy)]
struct PatchDescriptor {
    extent: ExtentKey,
    first: Option<(VId, CommitSeq)>,
    last: Option<VId>,
    rows: usize,
}

/// One canonical identity encountered by a buffered scan. Invisible identities
/// still produce a candidate, so a query can account for its complete source
/// work without inferring that every visited identity was a visible record.
#[derive(Debug)]
pub struct BufferedVertexCandidate {
    pub vid: VId,
    pub row: Option<BufferedValue<VertexRow>>,
}

/// One governed source boundary. Identity admission precedes that identity's
/// history reads; a single mutable callback can share a Send query meter with
/// the evaluator without shared interior borrows across an await.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferedScanEvent {
    Work,
    Identity(VId),
}

/// Keeps caller control failures distinct from storage/admission failures.
#[derive(Debug)]
pub enum BufferedScanError<C> {
    Read(BufferedReadError),
    Control(C),
}

impl<C> From<BufferedReadError> for BufferedScanError<C> {
    fn from(error: BufferedReadError) -> Self {
        Self::Read(error)
    }
}

impl<C: core::fmt::Display> core::fmt::Display for BufferedScanError<C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Read(error) => error.fmt(f),
            Self::Control(error) => error.fmt(f),
        }
    }
}

impl<C: std::error::Error + 'static> std::error::Error for BufferedScanError<C> {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct VertexScanHead {
    vid: VId,
    created_at: CommitSeq,
    patch: usize,
    row: usize,
}

struct DecodedVertexPatch {
    patch: usize,
    rows: VertexPatchRows,
}

/// A canonical k-way merge over an authenticated immutable patch generation.
///
/// The cursor retains one small head per patch and at most one decoded patch;
/// it never constructs the full vertex directory or retains graph properties.
/// A changed winning patch may refault, but every patch has at most
/// `MAX_PATCH_ROWS` rows. For P patches and N stored row versions, the counted
/// source events are bounded by P + N * (MAX_PATCH_ROWS + 2); heap maintenance
/// takes O(N log P) comparisons. Scan misses bypass resident cache admission.
/// Output rows retain their own reservations. A failed or dropped in-flight
/// pull makes the cursor terminal, so no consumed history prefix can resume.
pub struct BufferedVertexScan<'source, V: Vfs> {
    partition: &'source mut BufferedPartition<V>,
    as_of: CommitSeq,
    heads: BinaryHeap<Reverse<VertexScanHead>>,
    head_charge: Option<MemoryCharge>,
    active: Option<BufferedValue<DecodedVertexPatch>>,
    initialized: bool,
    done: bool,
    work: usize,
}

/// An owned authenticated source generation. Only cached/active extent handles
/// pin bytes; this root descriptor never pins every graph payload in RAM.
pub struct BufferedPartition<V: Vfs> {
    store: BlockStore<V>,
    root_id: PartitionRootVersion,
    root: PartitionRoot,
    blocks: Vec<BlockDescriptor>,
    patches: Vec<PatchDescriptor>,
    buffer: ExtentBuffer,
    limits: BufferedReadLimits,
    _metadata: MemoryCharge,
}

fn limit(
    resource: &'static str,
    requested: usize,
    maximum: usize,
) -> Result<(), BufferedReadError> {
    if requested > maximum {
        Err(BufferedReadError::Limit {
            resource,
            requested,
            limit: maximum,
        })
    } else {
        Ok(())
    }
}

fn advance(work: &mut usize, amount: usize, maximum: usize) -> Result<(), BufferedReadError> {
    let next = work
        .checked_add(amount)
        .ok_or(BufferedReadError::SizeOverflow)?;
    limit("buffered source work", next, maximum)?;
    *work = next;
    Ok(())
}

fn descriptor(id: fgdb_types::ids::ObjectId, bytes: &[u8]) -> Result<ExtentKey, BufferedReadError> {
    Ok(ExtentKey::new(id, 0, bytes.len(), extent_checksum(bytes))?)
}

impl<V: Vfs + Clone> BlockStore<V> {
    /// Bounded counterpart of resolve_manifest for the native one-partition
    /// database. The caller owns the pre-admitted metadata reservation.
    pub async fn resolve_manifest_bounded(
        &self,
        cx: &impl StorageReadCx,
        id: crate::manifest::ManifestVersion,
        limits: &BufferedReadLimits,
    ) -> Result<Vec<(crate::manifest::ManifestRecord, PartitionRoot)>, BufferedReadError> {
        let bytes = self
            .read_object_bytes(
                cx,
                id.0,
                (limits.max_root_bytes as u64).min(super::MANIFEST_HEADER_AND_RECORDS_CEILING),
            )
            .await?;
        let records =
            crate::manifest::read_manifest(self.k_oid.expose(), self.namespace, &bytes, id)
                .map_err(StoreError::MalformedManifest)?;
        limit("native manifest roots", records.len(), 1)?;
        let mut resolved = Vec::with_capacity(records.len());
        for record in records {
            let (root, _) = self
                .get_root_and_segments_limited(
                    cx,
                    record.root,
                    limits.max_root_bytes,
                    limits.max_blocks,
                    limits.max_vertex_patches,
                )
                .await?;
            resolved.push((record, root));
        }
        Ok(resolved)
    }

    /// Authenticate all named objects and history before issuing a cold source.
    /// No successfully validated prefix escapes on cancellation or quota error.
    pub async fn open_buffered_root(
        &self,
        cx: &impl BufferedReadCx,
        id: PartitionRootVersion,
        pool: MemoryPool,
        limits: BufferedReadLimits,
    ) -> Result<BufferedPartition<V>, BufferedReadError> {
        cx.buffered_checkpoint()
            .map_err(BufferedReadError::Interrupted)?;
        let buffer = ExtentBuffer::new(pool.clone(), limits.buffer)?;
        let metadata = pool.reserve(cx, limits.metadata_bytes()?)?;
        let (root, segments) = self
            .get_root_and_segments_limited(
                cx,
                id,
                limits.max_root_bytes,
                limits.max_blocks,
                limits.max_vertex_patches,
            )
            .await?;
        drop(segments);
        let mut blocks = Vec::with_capacity(root.blocks.len());
        let mut patches = Vec::with_capacity(root.vertex_patches.len());
        // Declaration order keeps the charges alive until the validators drop.
        let mut history_charges = Vec::new();
        let mut walk = RootWalk::default();
        let mut vertex_history = admission::VertexAdmission::new(cx, self, &root, pool.clone())?;
        let mut work = 0;
        let mut source_bytes = 0usize;
        let mut observe = |event| {
            cx.buffered_checkpoint()
                .map_err(BufferedReadError::Interrupted)?;
            match event {
                RootReadEvent::ObjectStart => {
                    limit(
                        "buffered source bytes",
                        source_bytes.saturating_add(1),
                        limits.max_source_bytes,
                    )?;
                    advance(&mut work, 1, limits.max_work)
                }
                RootReadEvent::SourceBytes(bytes) => {
                    source_bytes = source_bytes
                        .checked_add(bytes)
                        .ok_or(BufferedReadError::SizeOverflow)?;
                    limit(
                        "buffered source bytes",
                        source_bytes,
                        limits.max_source_bytes,
                    )
                }
                RootReadEvent::Incidences(rows) | RootReadEvent::VertexVersions(rows) => {
                    advance(&mut work, rows, limits.max_work)
                }
            }
        };
        for (at, reference) in root.blocks.iter().enumerate() {
            let _workspace = pool.reserve(cx, OBJECT_WORKSPACE_BYTES)?;
            observe(RootReadEvent::ObjectStart)?;
            // Keep decoding in this future: the ordinary open's optional
            // blocking-pool decode could outlive a dropped admission future
            // and thus outlive this affine workspace charge.
            let bytes = self
                .get_bytes(cx, crate::DeltaBlockVersion(reference.block_id))
                .await?;
            let decoded = crate::decode_block_with_properties(&bytes);
            let read = self
                .admit_root_block(
                    cx,
                    at,
                    root.partition,
                    reference,
                    Ok((bytes, decoded)),
                    &mut observe,
                )
                .await?;
            let (entries, _, predecessor) = &read.resolved;
            history_charges.push(
                pool.reserve(
                    cx,
                    entries
                        .len()
                        .checked_add(1)
                        .and_then(|rows| rows.checked_mul(HISTORY_ENTRY_BYTES))
                        .ok_or(BufferedReadError::SizeOverflow)?,
                )?,
            );
            if let Some(first) = entries.first() {
                let family = (first.src, first.relation);
                let expected = walk.chain_heads.get(&family).copied();
                if predecessor.map(|link| link.0) != expected {
                    return Err(StoreError::MalformedRoot(
                        crate::root::RootError::BlockChainMismatch {
                            at,
                            declared: predecessor.map(|link| link.0),
                            expected,
                        },
                    )
                    .into());
                }
                walk.chain_heads.insert(family, reference.block_id);
            }
            walk.history
                .observe_block(at, entries)
                .map_err(StoreError::MalformedRoot)?;
            blocks.push(BlockDescriptor {
                block: descriptor(reference.block_id, &read.bytes)?,
                properties: read
                    .property_patch
                    .as_ref()
                    .map(|(id, bytes)| descriptor(*id, bytes))
                    .transpose()?,
                first_edge: entries.iter().map(|row| (row.eid, row.created_at)).min(),
                rows: entries.len(),
            });
        }
        for (at, reference) in root.vertex_patches.iter().enumerate() {
            let _workspace = pool.reserve(cx, OBJECT_WORKSPACE_BYTES)?;
            let (rows, bytes) = self
                .resolve_root_patch_observed(cx, at, reference, &mut observe)
                .await?;
            vertex_history
                .observe_patch(cx, at, &rows, &mut observe)
                .await?;
            patches.push(PatchDescriptor {
                extent: descriptor(reference.patch_id, &bytes)?,
                first: rows.first().map(|row| (row.vid, row.created_at)),
                last: rows.last().map(|row| row.vid),
                rows: rows.len(),
            });
        }
        drop(vertex_history);
        drop(walk);
        drop(history_charges);
        cx.buffered_checkpoint()
            .map_err(BufferedReadError::Interrupted)?;
        Ok(BufferedPartition {
            store: self.clone(),
            root_id: id,
            root,
            blocks,
            patches,
            buffer,
            limits,
            _metadata: metadata,
        })
    }
}

impl<V: Vfs> BufferedPartition<V> {
    pub const fn root_id(&self) -> PartitionRootVersion {
        self.root_id
    }
    pub const fn root(&self) -> &PartitionRoot {
        &self.root
    }
    pub fn pool(&self) -> &MemoryPool {
        self.buffer.memory_pool()
    }
    pub const fn stats(&self) -> BufferStats {
        self.buffer.stats()
    }

    /// Open a lazy canonical vertex cursor at one fixed historical cut. Heap
    /// admission precedes allocation; source work starts on its first pull.
    /// The patch heads come from initial authenticated admission, so opening a
    /// cursor does not fault every payload merely to discover its first key.
    pub fn vertex_scan(
        &mut self,
        cx: &QueryCx,
        as_of: CommitSeq,
    ) -> Result<BufferedVertexScan<'_, V>, BufferedReadError> {
        self.begin(cx, as_of)?;
        let bytes = self
            .patches
            .len()
            .checked_mul(HISTORY_ENTRY_BYTES)
            .and_then(|bytes| bytes.checked_add(1024))
            .ok_or(BufferedReadError::SizeOverflow)?;
        let head_charge = self.buffer.reserve_scratch(cx, bytes)?;
        let mut heads = BinaryHeap::new();
        heads
            .try_reserve_exact(self.patches.len())
            .map_err(|_| MemoryError::AllocationFailed { requested: bytes })?;
        let allocated = heads
            .capacity()
            .checked_mul(size_of::<Reverse<VertexScanHead>>())
            .ok_or(BufferedReadError::SizeOverflow)?;
        limit("buffered scan heap bytes", allocated, bytes)?;
        Ok(BufferedVertexScan {
            partition: self,
            as_of,
            heads,
            head_charge: Some(head_charge),
            active: None,
            initialized: false,
            done: false,
            work: 0,
        })
    }

    fn begin(&self, cx: &QueryCx, as_of: CommitSeq) -> Result<(), BufferedReadError> {
        cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
        if as_of.0 > self.root.published_at.0 {
            return Err(BufferedReadError::BeyondPublication {
                requested: as_of,
                publication: self.root.published_at,
            });
        }
        Ok(())
    }

    async fn pin(
        &mut self,
        cx: &QueryCx,
        key: ExtentKey,
        admission: Admission,
    ) -> Result<BufferHandle, BufferedReadError> {
        match self.buffer.prepare(cx, key, admission)? {
            PreparedExtent::Resident(handle) => Ok(handle),
            PreparedExtent::Load(mut pending) => {
                let mut file = cx
                    .with_restriction_async(
                        self.store.vfs.open_read(&self.store.path(key.object())),
                    )
                    .await
                    .map_err(StoreError::Io)?;
                cx.with_restriction_async(async {
                    file.read_exact(pending.as_mut()).await?;
                    let mut extra = [0u8; 1];
                    if file.read(&mut extra).await? != 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "admitted object length changed",
                        ));
                    }
                    Ok(())
                })
                .await
                .map_err(StoreError::Io)?;
                Ok(self.buffer.complete(cx, pending)?)
            }
        }
    }

    async fn block(
        &mut self,
        cx: &QueryCx,
        at: usize,
        work: &mut usize,
    ) -> Result<(Vec<AdjacencyEntry>, Option<BlockProps>), BufferedReadError> {
        let maximum = self.limits.max_work;
        self.block_controlled::<core::convert::Infallible>(cx, at, Admission::Normal, &mut || {
            advance(work, 1, maximum).map_err(BufferedScanError::Read)
        })
        .await
        .map_err(|error| match error {
            BufferedScanError::Read(error) => error,
            BufferedScanError::Control(never) => match never {},
        })
    }

    pub async fn vertex_at(
        &mut self,
        cx: &QueryCx,
        vid: VId,
        as_of: CommitSeq,
    ) -> Result<Option<BufferedValue<VertexRow>>, BufferedReadError> {
        self.begin(cx, as_of)?;
        let result_charge = self.buffer.reserve_scratch(cx, OBJECT_WORKSPACE_BYTES)?;
        let mut result: Option<VertexRow> = None;
        let mut work = 0;
        for at in 0..self.patches.len() {
            cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
            advance(&mut work, 1, self.limits.max_work)?;
            if self.root.vertex_patches[at].first_seq.0 > as_of.0 {
                continue;
            }
            let _workspace = self.buffer.reserve_scratch(cx, OBJECT_WORKSPACE_BYTES)?;
            let key = self.patches[at].extent;
            let bytes = self.pin(cx, key, Admission::Normal).await?;
            let rows = crate::root::resolve_patch_ref(
                self.store.k_oid.expose(),
                self.store.namespace,
                at,
                &self.root.vertex_patches[at],
                bytes.as_ref(),
                self.store.decode_resolver(),
            )
            .map_err(StoreError::MalformedRoot)?;
            for row in &rows {
                advance(&mut work, 1, self.limits.max_work)?;
                if row.vid == vid
                    && row.created_at.0 <= as_of.0
                    && result
                        .as_ref()
                        .is_none_or(|old| old.created_at.0 <= row.created_at.0)
                {
                    result = Some(row.clone());
                }
            }
        }
        cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
        Ok(result
            .filter(|row| row.visible_at(as_of))
            .map(|value| BufferedValue {
                value,
                _charge: result_charge,
            }))
    }

    pub async fn edge_at(
        &mut self,
        cx: &QueryCx,
        eid: EId,
        as_of: CommitSeq,
    ) -> Result<Option<BufferedValue<BufferedEdge>>, BufferedReadError> {
        self.begin(cx, as_of)?;
        let result_charge = self.buffer.reserve_scratch(cx, OBJECT_WORKSPACE_BYTES)?;
        let mut result: Option<BufferedEdge> = None;
        let mut work = 0;
        for at in 0..self.blocks.len() {
            cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
            advance(&mut work, 1, self.limits.max_work)?;
            if self.root.blocks[at].first_seq.0 > as_of.0 {
                continue;
            }
            let _workspace = self.buffer.reserve_scratch(cx, OBJECT_WORKSPACE_BYTES)?;
            let (entries, props) = self.block(cx, at, &mut work).await?;
            for (index, entry) in entries.iter().enumerate() {
                advance(&mut work, 1, self.limits.max_work)?;
                if entry.eid == eid
                    && entry.created_at.0 <= as_of.0
                    && result
                        .as_ref()
                        .is_none_or(|old| old.entry.created_at.0 <= entry.created_at.0)
                {
                    result = Some(BufferedEdge {
                        entry: *entry,
                        props: props
                            .as_ref()
                            .map_or_else(Vec::new, |props| props.props_of(index)),
                    });
                }
            }
        }
        cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
        Ok(result
            .filter(|row| row.entry.visible_at(as_of))
            .map(|value| BufferedValue {
                value,
                _charge: result_charge,
            }))
    }

    /// Bounded winning adjacency, in the canonical (src, relation, dst, eid)
    /// order. The cap bounds candidate identities, including later-retired
    /// ones: exceeding it refuses the whole read, never returns a prefix.
    pub async fn adjacency_at(
        &mut self,
        cx: &QueryCx,
        vertex: VId,
        relation: Option<RelationId>,
        incoming: bool,
        as_of: CommitSeq,
        max_entries: usize,
    ) -> Result<BufferedValue<Vec<AdjacencyEntry>>, BufferedReadError> {
        self.begin(cx, as_of)?;
        let bytes = max_entries
            .checked_mul(HISTORY_ENTRY_BYTES)
            .and_then(|n| n.checked_add(1024))
            .ok_or(BufferedReadError::SizeOverflow)?;
        let result_charge = self.buffer.reserve_scratch(cx, bytes)?;
        let mut candidates = BTreeMap::<EId, AdjacencyEntry>::new();
        let mut work = 0;
        for at in 0..self.blocks.len() {
            cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
            advance(&mut work, 1, self.limits.max_work)?;
            if self.root.blocks[at].first_seq.0 > as_of.0 {
                continue;
            }
            let _workspace = self.buffer.reserve_scratch(cx, OBJECT_WORKSPACE_BYTES)?;
            let (entries, _) = self.block(cx, at, &mut work).await?;
            for entry in entries {
                advance(&mut work, 1, self.limits.max_work)?;
                let endpoint = if incoming { entry.dst } else { entry.src };
                if endpoint != vertex
                    || relation.is_some_and(|wanted| wanted != entry.relation)
                    || entry.created_at.0 > as_of.0
                {
                    continue;
                }
                if !candidates.contains_key(&entry.eid) {
                    limit(
                        "buffered adjacency identities",
                        candidates.len().saturating_add(1),
                        max_entries,
                    )?;
                }
                if candidates
                    .get(&entry.eid)
                    .is_none_or(|old| old.created_at.0 <= entry.created_at.0)
                {
                    candidates.insert(entry.eid, entry);
                }
            }
        }
        let mut value: Vec<_> = candidates
            .into_values()
            .filter(|row| row.visible_at(as_of))
            .collect();
        value.sort_unstable_by_key(|row| (row.src, row.relation, row.dst, row.eid));
        cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
        Ok(BufferedValue {
            value,
            _charge: result_charge,
        })
    }
}

impl<V: Vfs> BufferedVertexScan<'_, V> {
    pub const fn snapshot_seq(&self) -> CommitSeq {
        self.as_of
    }

    /// Cumulative admitted source events, never reset by a successful pull.
    pub const fn work_used(&self) -> usize {
        self.work
    }

    fn finish(&mut self) {
        self.done = true;
        self.active = None;
        // Release the actual capacity before refunding its affine charge.
        self.heads = BinaryHeap::new();
        self.head_charge = None;
    }

    fn observe<C: Send>(
        &mut self,
        cx: &QueryCx,
        observe: &mut (impl FnMut(BufferedScanEvent) -> Result<(), C> + Send),
    ) -> Result<(), BufferedScanError<C>> {
        cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
        let next = self
            .work
            .checked_add(1)
            .ok_or(BufferedReadError::SizeOverflow)?;
        limit("buffered source work", next, self.partition.limits.max_work)?;
        observe(BufferedScanEvent::Work).map_err(BufferedScanError::Control)?;
        self.work = next;
        Ok(())
    }

    async fn load_patch<C: Send>(
        &mut self,
        cx: &QueryCx,
        at: usize,
        observe: &mut (impl FnMut(BufferedScanEvent) -> Result<(), C> + Send),
    ) -> Result<(), BufferedScanError<C>> {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.patch == at)
        {
            return Ok(());
        }
        let descriptor = self.partition.patches[at];
        self.observe(cx, observe)?;
        // The authenticated descriptor already knows the complete row count.
        // Admit all decode visits before performing I/O or allocating rows.
        for _ in 0..descriptor.rows {
            self.observe(cx, observe)?;
        }
        self.active = None;
        let charge = self
            .partition
            .buffer
            .reserve_scratch(cx, OBJECT_WORKSPACE_BYTES)
            .map_err(BufferedReadError::Buffer)?;
        let bytes = self
            .partition
            .pin(cx, descriptor.extent, Admission::ScanBypass)
            .await?;
        let rows = crate::root::resolve_patch_ref(
            self.partition.store.k_oid.expose(),
            self.partition.store.namespace,
            at,
            &self.partition.root.vertex_patches[at],
            bytes.as_ref(),
            self.partition.store.decode_resolver(),
        )
        .map_err(StoreError::MalformedRoot)
        .map_err(BufferedReadError::from)?;
        if rows.len() != descriptor.rows {
            return Err(BufferedReadError::Buffer(BufferError::InvalidLoad).into());
        }
        drop(bytes);
        cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
        self.active = Some(BufferedValue {
            value: DecodedVertexPatch { patch: at, rows },
            _charge: charge,
        });
        Ok(())
    }

    async fn next_candidate_inner<C: Send>(
        &mut self,
        cx: &QueryCx,
        observe: &mut (impl FnMut(BufferedScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedVertexCandidate>, BufferedScanError<C>> {
        if !self.initialized {
            for at in 0..self.partition.patches.len() {
                self.observe(cx, observe)?;
                if let Some((vid, created_at)) = self.partition.patches[at].first {
                    self.heads.push(Reverse(VertexScanHead {
                        vid,
                        created_at,
                        patch: at,
                        row: 0,
                    }));
                }
            }
            self.initialized = true;
        }
        cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
        let Some(Reverse(first)) = self.heads.peek().copied() else {
            return Ok(None);
        };
        // Candidate-cardinality admission precedes all of its history loads,
        // including loads that would prove this identity invisible at the cut.
        observe(BufferedScanEvent::Identity(first.vid)).map_err(BufferedScanError::Control)?;
        let mut winning_row: Option<BufferedValue<VertexRow>> = None;
        while let Some(Reverse(head)) = self.heads.peek().copied() {
            if head.vid != first.vid {
                break;
            }
            self.observe(cx, observe)?;
            self.load_patch(cx, head.patch, observe).await?;
            let active = self
                .active
                .as_ref()
                .ok_or_else(|| BufferedReadError::Buffer(BufferError::InvalidLoad))?;
            let row = active
                .rows
                .get(head.row)
                .ok_or_else(|| BufferedReadError::Buffer(BufferError::InvalidLoad))?;
            if row.vid != head.vid || row.created_at != head.created_at {
                return Err(BufferedReadError::Buffer(BufferError::InvalidLoad).into());
            }
            if row.created_at.0 <= self.as_of.0 {
                // Heap order visits creation versions and then publication
                // positions in ascending order. The final eligible statement
                // therefore incorporates every later retirement restatement.
                match winning_row.as_mut() {
                    Some(winner) => winner.value = row.clone(),
                    None => {
                        let charge = self
                            .partition
                            .buffer
                            .reserve_scratch(cx, OBJECT_WORKSPACE_BYTES)
                            .map_err(BufferedReadError::Buffer)?;
                        winning_row = Some(BufferedValue {
                            value: row.clone(),
                            _charge: charge,
                        });
                    }
                }
            }
            let next = active.rows.get(head.row + 1).map(|row| {
                Reverse(VertexScanHead {
                    vid: row.vid,
                    created_at: row.created_at,
                    patch: head.patch,
                    row: head.row + 1,
                })
            });
            self.heads.pop();
            if let Some(next) = next {
                self.heads.push(next);
            }
        }
        cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
        let row = winning_row.filter(|row| row.visible_at(self.as_of));
        Ok(Some(BufferedVertexCandidate {
            vid: first.vid,
            row,
        }))
    }

    /// Yield the next identity, including an invisible identity with no row.
    /// The callback runs before each source work event and can share one query
    /// meter with the evaluator. Neither callback failure nor a failed source
    /// read returns a partial winning row. Identity runs exactly once before
    /// any history read or output allocation for the next identity.
    pub async fn next_candidate_with<C: Send>(
        &mut self,
        cx: &QueryCx,
        observe: &mut (impl FnMut(BufferedScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedVertexCandidate>, BufferedScanError<C>> {
        if self.done {
            self.finish();
            return Ok(None);
        }
        // Leave terminal set while the future can suspend. Dropping that
        // future must never restart after partially consuming one identity.
        self.done = true;
        let result = self.next_candidate_inner(cx, observe).await;
        if matches!(result, Ok(Some(_))) && !self.heads.is_empty() {
            self.done = false;
        } else {
            self.finish();
        }
        result
    }

    /// Yield visible rows in ascending stable VId order.
    pub async fn next(
        &mut self,
        cx: &QueryCx,
    ) -> Result<Option<BufferedValue<VertexRow>>, BufferedReadError> {
        let mut observe = |_| Ok::<(), core::convert::Infallible>(());
        loop {
            match self.next_candidate_with(cx, &mut observe).await {
                Ok(Some(candidate)) => {
                    if let Some(row) = candidate.row {
                        return Ok(Some(row));
                    }
                }
                Ok(None) => return Ok(None),
                Err(BufferedScanError::Read(error)) => return Err(error),
                Err(BufferedScanError::Control(never)) => match never {},
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_types::{
        BranchId, CanonicalScalar, DatabaseSecurityNamespaceId, GraphId, PurposeContexts,
    };

    #[test]
    fn a_read_charges_the_hosted_property_visit_before_its_cache_pin() {
        let dir = std::env::temp_dir().join(format!("fgdb-cold-work-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (_, report) = run_async_under_lab(0xb0_10, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let query = contexts.query();
            let store = BlockStore::open(
                &commit,
                &dir,
                [0x17; 32],
                DatabaseSecurityNamespaceId([0x18; 32]),
            )
            .await
            .unwrap();
            let rows = vec![vec![(PropertyKeyId(1), CanonicalScalar::Int(7))]];
            let patch = store
                .put_edge_property_patch(
                    &commit,
                    &crate::edge_props::encode_property_patch(&rows).unwrap(),
                )
                .await
                .unwrap();
            let entry = AdjacencyEntry {
                src: VId(1),
                relation: RelationId(1),
                dst: VId(2),
                eid: EId(1),
                created_at: CommitSeq(1),
                retired_at: None,
            };
            let block = store
                .put(
                    &commit,
                    &crate::encode_block_with_properties(0, None, &[entry], patch.0, &[1], &rows)
                        .unwrap(),
                )
                .await
                .unwrap();
            let id = store
                .put_root(
                    &commit,
                    &PartitionRoot {
                        graph: GraphId(1),
                        branch: BranchId(1),
                        partition: 0,
                        published_at: CommitSeq(1),
                        blocks: vec![crate::root::BlockRef {
                            block_id: block.0,
                            first_seq: CommitSeq(1),
                            last_seq: CommitSeq(1),
                        }],
                        vertex_patches: vec![],
                    },
                )
                .await
                .unwrap();
            let pool = MemoryPool::new(4 * 1024 * 1024, 0).unwrap();
            let limits = BufferedReadLimits {
                max_root_bytes: 4096,
                max_source_bytes: 4096,
                max_blocks: 1,
                max_vertex_patches: 0,
                max_work: 3,
                buffer: BufferLimits {
                    max_frames: 1,
                    max_ghost_entries: 1,
                    max_extent_bytes: 4096,
                },
            };
            let mut view = store
                .open_buffered_root(&query, id, pool.clone(), limits)
                .await
                .unwrap();
            // Admission proves the whole source first. Lower only the private
            // read allowance to exercise each boundary independently of open.
            view.limits.max_work = 1;
            for adjacency in [false, true] {
                let refusal = if adjacency {
                    view.adjacency_at(&query, VId(1), None, false, CommitSeq(1), 1)
                        .await
                        .map(|_| ())
                } else {
                    view.edge_at(&query, EId(1), CommitSeq(1)).await.map(|_| ())
                };
                assert!(matches!(
                    refusal,
                    Err(BufferedReadError::Limit {
                        resource: "buffered source work",
                        requested: 2,
                        limit: 1,
                    })
                ));
                assert_eq!(view.stats().misses, 1, "property payload was never pinned");
            }
            view.limits.max_work = 3;
            let found = view
                .edge_at(&query, EId(1), CommitSeq(1))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(found.entry, entry);
            assert_eq!(found.props, rows[0]);
            drop(found);
            let found = view
                .adjacency_at(&query, VId(1), None, false, CommitSeq(1), 1)
                .await
                .unwrap();
            assert_eq!(found.as_ref(), &vec![entry]);
            drop(found);
            drop(view);
            assert_eq!(pool.used(), 0);
        });
        assert!(report.invariant_violations.is_empty(), "{report:?}");
    }
}
