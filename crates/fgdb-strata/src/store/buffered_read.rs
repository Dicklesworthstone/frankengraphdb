//! An admitted immutable partition whose payloads fault through ExtentBuffer.
//!
//! Publication proves root history laws before sealing V6 descriptors.
//! A live opaque receipt for the exact admitted root permits metadata-only
//! source construction. Every cold open, including V6, still walks payloads
//! and proves history: a keyed root or raw slot publication is not admission.
//! Vertex admission retains charged lifecycle metadata and birth
//! locators; restatements refault their authenticated birth patch for exact
//! payload comparison. Resident admission therefore depends on version count,
//! not the sum of vertex property bytes. A metadata-heavy history may still
//! refuse; this is not an unbounded external history validator. No durable
//! permission to reclaim objects follows from this reader.

mod admission;
mod block_directory;
mod patch_directory;
mod scan_access;

use super::{BlockStore, RootReadEvent, RootWalk, StoreError};
use crate::edge_props::{BlockProps, EdgePropertyRow};
use crate::root::PartitionRoot;
use crate::root::descriptor::{BlockDescriptor, BlockSummary, PatchDescriptor};
use crate::tiered::buffer::{
    Admission, BufferError, BufferHandle, BufferLimits, BufferStats, ExtentBuffer, ExtentKey,
    PreparedExtent, extent_checksum,
};
use crate::tiered::memory::{MemoryCharge, MemoryError, MemoryPool};
use crate::vertex::{VertexPatchRows, VertexRow};
use crate::{AdjacencyEntry, PartitionRootVersion};
use asupersync::fs::Vfs;
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
    /// Object/decoded-row visits and interval-directory construction/inspection,
    /// separately per open/read call.
    /// Scans also count head initialization/consumption and share this one
    /// cumulative ceiling across every pull, including edge endpoint reads.
    pub max_work: usize,
    pub buffer: BufferLimits,
}

impl BufferedReadLimits {
    /// Reserve before decoding a manifest/root. This opening allowance covers
    /// simultaneous encoded frames, V4 segment vectors, flattened references,
    /// descriptors, and bounded cache-policy bookkeeping. Payload and history
    /// use separate reservations. After root decoding, unused decode slack is
    /// refunded while both staged descriptor sets remain charged. After
    /// admission, only retained metadata capacities and the cache-policy
    /// allowance remain charged to the view.
    pub fn metadata_bytes(self) -> Result<usize, BufferedReadError> {
        let policy = self.metadata_policy_bytes()?;
        self.max_root_bytes
            .checked_mul(16)
            .and_then(|bytes| bytes.checked_add(policy))
            .ok_or(BufferedReadError::SizeOverflow)
    }

    fn metadata_policy_bytes(self) -> Result<usize, BufferedReadError> {
        self.buffer
            .max_frames
            .checked_add(self.buffer.max_ghost_entries)
            .and_then(|entries| entries.checked_mul(512))
            .and_then(|bytes| bytes.checked_add(4096))
            .ok_or(BufferedReadError::SizeOverflow)
    }

    fn retained_metadata_bytes(
        self,
        root: &PartitionRoot,
        block_capacity: usize,
        patch_capacity: usize,
    ) -> Result<usize, BufferedReadError> {
        // All four element types contain only fixed-width fields. Account the
        // allocations' capacities, including any spare decoder capacity.
        let mut bytes = self.metadata_policy_bytes()?;
        for (capacity, width) in [
            (root.blocks.capacity(), size_of::<crate::root::BlockRef>()),
            (
                root.vertex_patches.capacity(),
                size_of::<crate::root::PatchRef>(),
            ),
            (block_capacity, size_of::<BlockDescriptor>()),
            (patch_capacity, size_of::<PatchDescriptor>()),
        ] {
            bytes = capacity
                .checked_mul(width)
                .and_then(|allocated| bytes.checked_add(allocated))
                .ok_or(BufferedReadError::SizeOverflow)?;
        }
        Ok(bytes)
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

/// One canonical identity encountered by a buffered scan. Invisible identities
/// still produce a candidate, so a query can account for its complete source
/// work without inferring that every visited identity was a visible record.
#[derive(Debug)]
pub struct BufferedVertexCandidate<T = BufferedValue<VertexRow>> {
    pub vid: VId,
    pub row: Option<T>,
}

/// Identity, lifetime and labels from an authenticated historical vertex.
/// Property payloads were not requested. This owner deliberately exposes no
/// complete VertexRow, so a skipped property cannot be mistaken for absence.
/// The patch's complete bytes were validated during initial root admission.
#[derive(Debug)]
pub struct BufferedVertexTopology(BufferedValue<VertexRow>);

impl BufferedVertexTopology {
    pub fn vid(&self) -> VId {
        self.0.vid
    }

    pub fn birth_ordinal(&self) -> u64 {
        self.0.birth_ordinal
    }

    pub fn created_at(&self) -> CommitSeq {
        self.0.created_at
    }

    pub fn retired_at(&self) -> Option<CommitSeq> {
        self.0.retired_at
    }

    pub fn labels(&self) -> &[fgdb_delta_types::LabelId] {
        &self.0.labels
    }

    /// The actual reservation held independently of the scan and its view.
    pub fn charged_bytes(&self) -> usize {
        self.0.charged_bytes()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VertexPropertyAccess {
    Read,
    Skip,
}

impl VertexPropertyAccess {
    fn copy_row(self, row: &VertexRow) -> VertexRow {
        match self {
            Self::Read => row.clone(),
            Self::Skip => VertexRow {
                vid: row.vid,
                birth_ordinal: row.birth_ordinal,
                created_at: row.created_at,
                retired_at: row.retired_at,
                labels: row.labels.clone(),
                props: Vec::new(),
            },
        }
    }
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
    properties: VertexPropertyAccess,
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
    patch_directory: patch_directory::PatchDirectory,
    block_directory: block_directory::BlockDirectory,
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

    /// An arbitrary keyed root ID is not published-source authority. This
    /// entry point always proves complete payload/history admission, including
    /// V6 roots. Actual prior admission can be reused with
    /// open_buffered_root_with_receipts.
    pub async fn open_buffered_root(
        &self,
        cx: &impl BufferedReadCx,
        id: PartitionRootVersion,
        pool: MemoryPool,
        limits: BufferedReadLimits,
    ) -> Result<BufferedPartition<V>, BufferedReadError> {
        self.open_buffered_root_inner(cx, id, pool, limits, None)
            .await
    }

    /// Reuse a complete admission already earned by this live publish lineage.
    /// The opaque receipts bind the exact root, identity key, namespace,
    /// coordinate, frontier and complete membership. An absent or stale match
    /// takes the ordinary full payload/history admission path; possessing a
    /// keyed root or publishing a raw root slot never mints this evidence.
    ///
    /// Immutable payloads are still authenticated on each cache miss. V6
    /// descriptors must agree with the previously admitted bytes. This owner
    /// API grants no Warden permission and does not create writer receipts.
    pub async fn open_buffered_root_with_receipts(
        &self,
        cx: &impl BufferedReadCx,
        id: PartitionRootVersion,
        receipts: &super::PublishReceipts,
        pool: MemoryPool,
        limits: BufferedReadLimits,
    ) -> Result<BufferedPartition<V>, BufferedReadError> {
        self.open_buffered_root_inner(cx, id, pool, limits, Some(receipts))
            .await
    }

    async fn open_buffered_root_inner(
        &self,
        cx: &impl BufferedReadCx,
        id: PartitionRootVersion,
        pool: MemoryPool,
        limits: BufferedReadLimits,
        receipts: Option<&super::PublishReceipts>,
    ) -> Result<BufferedPartition<V>, BufferedReadError> {
        cx.buffered_checkpoint()
            .map_err(BufferedReadError::Interrupted)?;
        let buffer = ExtentBuffer::new(pool.clone(), limits.buffer)?;
        let mut metadata = pool.reserve(cx, limits.metadata_bytes()?)?;
        let (root, mut segments) = self
            .get_root_and_segments_limited(
                cx,
                id,
                limits.max_root_bytes,
                limits.max_blocks,
                limits.max_vertex_patches,
            )
            .await?;
        let published = segments.descriptors.take();
        drop(segments);
        let identity = self.root_identity_domain();
        let trusted = receipts
            .and_then(|receipts| receipts.root_memo.as_ref())
            .filter(|memo| {
                memo.partition == root.partition
                    && memo.blocks == root.blocks
                    && memo.patches == root.vertex_patches
                    && memo.binding.as_ref().is_some_and(|binding| {
                        binding.identity == identity
                            && binding.root == id
                            && binding.graph == root.graph
                            && binding.branch == root.branch
                            && binding.published_at == root.published_at
                    })
            })
            .map(|memo| &memo.descriptors);
        if trusted
            .zip(published.as_ref())
            .is_some_and(|(admitted, stored)| admitted != stored)
        {
            return Err(
                StoreError::MalformedRoot(crate::root::RootError::InvalidDescriptor).into(),
            );
        }
        let certified = trusted.is_some();
        let untrusted = published;
        let (mut blocks, mut patches) = trusted.map_or_else(
            || {
                (
                    Vec::with_capacity(root.blocks.len()),
                    Vec::with_capacity(root.vertex_patches.len()),
                )
            },
            |descriptors| (descriptors.blocks.clone(), descriptors.patches.clone()),
        );
        // Root frames and segment-decode workspaces have dropped. Keep the
        // actual reference capacities and BOTH descriptor sets charged, but
        // release vanished decode slack before admitting payloads. In
        // particular, an incoming vertex patch and its refaulted birth patch
        // each still reserve their complete independent object workspace.
        let (staged_blocks, staged_patches) = untrusted.as_ref().map_or((0, 0), |descriptors| {
            (
                descriptors.blocks.capacity(),
                descriptors.patches.capacity(),
            )
        });
        let admission_metadata = limits.retained_metadata_bytes(
            &root,
            blocks
                .capacity()
                .checked_add(staged_blocks)
                .ok_or(BufferedReadError::SizeOverflow)?,
            patches
                .capacity()
                .checked_add(staged_patches)
                .ok_or(BufferedReadError::SizeOverflow)?,
        )?;
        let excess = metadata.bytes().checked_sub(admission_metadata).ok_or(
            MemoryError::ResourceExhausted {
                requested: admission_metadata,
                available: metadata.bytes(),
                limit: metadata.bytes(),
            },
        )?;
        drop(
            metadata
                .split_off(excess)
                .ok_or(BufferedReadError::SizeOverflow)?,
        );
        let mut work = 0;
        if !certified {
            // Declaration order keeps the charges alive until the validators drop.
            let mut history_charges = Vec::new();
            let mut walk = RootWalk::default();
            let mut vertex_history =
                admission::VertexAdmission::new(cx, self, &root, pool.clone())?;
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
                // and thus outlive this affine workspace charge. For the same
                // reason no hosted patch is prefetched: admission reads it.
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
                        Ok(((bytes, decoded), None)),
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
                    summary: BlockSummary::from_entries(entries),
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
            if untrusted
                .as_ref()
                .is_some_and(|claimed| claimed.blocks != blocks || claimed.patches != patches)
            {
                return Err(
                    StoreError::MalformedRoot(crate::root::RootError::InvalidDescriptor).into(),
                );
            }
        } else {
            for _ in 0..blocks.len().saturating_add(patches.len()) {
                cx.buffered_checkpoint()
                    .map_err(BufferedReadError::Interrupted)?;
                advance(&mut work, 1, limits.max_work)?;
            }
        }
        // Published descriptors are temporary admission evidence; their copies
        // must die before releasing their allowance. The root-segment vectors,
        // unused decode slack and cold history validators have already dropped.
        drop(untrusted);
        let retained =
            limits.retained_metadata_bytes(&root, blocks.capacity(), patches.capacity())?;
        let excess =
            metadata
                .bytes()
                .checked_sub(retained)
                .ok_or(MemoryError::ResourceExhausted {
                    requested: retained,
                    available: metadata.bytes(),
                    limit: metadata.bytes(),
                })?;
        // Keep the original, early-declared owner so later failures drop the
        // decoded vectors before their charge. Directories charge themselves.
        drop(
            metadata
                .split_off(excess)
                .ok_or(BufferedReadError::SizeOverflow)?,
        );
        let patch_directory =
            patch_directory::PatchDirectory::new(&patches, cx, &pool, &mut || {
                cx.buffered_checkpoint()
                    .map_err(BufferedReadError::Interrupted)?;
                advance(&mut work, 1, limits.max_work)
            })?;
        // Publication (V6), or this open's complete legacy walk, authenticated
        // descriptor topology before these fences may exclude payloads.
        let block_directory =
            block_directory::BlockDirectory::new(&blocks, cx, &pool, &mut || {
                cx.buffered_checkpoint()
                    .map_err(BufferedReadError::Interrupted)?;
                advance(&mut work, 1, limits.max_work)
            })?;
        cx.buffered_checkpoint()
            .map_err(BufferedReadError::Interrupted)?;
        Ok(BufferedPartition {
            store: self.clone(),
            root_id: id,
            root,
            blocks,
            patches,
            patch_directory,
            block_directory,
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
                self.store
                    .read_object_exact(cx, key.object(), pending.as_mut())
                    .await?;
                Ok(self.buffer.complete(cx, pending)?)
            }
        }
    }

    // Only checksum-verified bytes pinned from this admitted generation reach
    // this selector. Initial admission already proved full property codecs,
    // object identity, format profile and all cross-patch history laws.
    fn decode_scan_patch(
        &self,
        at: usize,
        bytes: &[u8],
        properties: VertexPropertyAccess,
        selected_vertex: Option<VId>,
    ) -> Result<VertexPatchRows, BufferedReadError> {
        let descriptor = self.patches[at];
        let reference = &self.root.vertex_patches[at];
        let rows = match (properties, selected_vertex) {
            (VertexPropertyAccess::Read, Some(vid)) => {
                let rows = crate::vertex::read_admitted_patch_for_vertex(
                    self.store.k_oid.expose(),
                    self.store.namespace,
                    bytes,
                    crate::vertex::VertexPatchVersion(reference.patch_id),
                    self.store.decode_resolver(),
                    vid,
                )
                .map_err(|error| {
                    StoreError::MalformedRoot(crate::root::RootError::Patch { at, error })
                })?;
                let actual = crate::vertex::span_of_rows(&rows);
                let declared = (reference.first_seq, reference.last_seq);
                if actual != Some(declared) {
                    return Err(StoreError::MalformedRoot(
                        crate::root::RootError::PatchRangeMismatch {
                            at,
                            declared,
                            actual: actual.unwrap_or((CommitSeq(0), CommitSeq(0))),
                        },
                    )
                    .into());
                }
                rows
            }
            (VertexPropertyAccess::Read, None) => crate::root::resolve_patch_ref(
                self.store.k_oid.expose(),
                self.store.namespace,
                at,
                reference,
                bytes,
                self.store.decode_resolver(),
            )
            .map_err(StoreError::MalformedRoot)?,
            (VertexPropertyAccess::Skip, _) => crate::vertex::decode_admitted_patch_topology(bytes)
                .map_err(StoreError::MalformedPatch)?,
        };
        if rows.len() != descriptor.rows
            || rows.first().map(|row| (row.vid, row.created_at)) != descriptor.first
            || rows.last().map(|row| row.vid) != descriptor.last
            || (properties == VertexPropertyAccess::Skip
                && crate::vertex::span_of_rows(&rows)
                    != Some((reference.first_seq, reference.last_seq)))
        {
            return Err(BufferError::InvalidLoad.into());
        }
        Ok(rows)
    }

    /// Resolve one vertex through an admitted interval directory. Every patch
    /// that can restate its history remains a candidate; publication ordinal
    /// breaks equal-version ties independently of directory traversal order.
    /// Unrelated patch payloads do not enter the cache or decode workspace.
    pub async fn vertex_at(
        &mut self,
        cx: &QueryCx,
        vid: VId,
        as_of: CommitSeq,
    ) -> Result<Option<BufferedValue<VertexRow>>, BufferedReadError> {
        self.begin(cx, as_of)?;
        let result_charge = self.buffer.reserve_scratch(cx, OBJECT_WORKSPACE_BYTES)?;
        let mut result: Option<(VertexRow, usize)> = None;
        let mut work = 0;
        let mut candidates = self.patch_directory.candidates(vid);
        while let Some(at) = self.patch_directory.next(&mut candidates, &mut || {
            cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
            advance(&mut work, 1, self.limits.max_work)
        })? {
            let descriptor = self.patches[at];
            if self.root.vertex_patches[at].first_seq.0 > as_of.0 || !descriptor.may_contain(vid) {
                continue;
            }
            advance(&mut work, descriptor.rows, self.limits.max_work)?;
            let _workspace = self.buffer.reserve_scratch(cx, OBJECT_WORKSPACE_BYTES)?;
            let bytes = self.pin(cx, descriptor.extent, Admission::Normal).await?;
            let rows =
                self.decode_scan_patch(at, bytes.as_ref(), VertexPropertyAccess::Read, Some(vid))?;
            if rows.len() != descriptor.rows {
                return Err(BufferError::InvalidLoad.into());
            }
            for row in &rows {
                if row.vid == vid
                    && row.created_at.0 <= as_of.0
                    && result.as_ref().is_none_or(|(old, old_at)| {
                        (old.created_at, *old_at) <= (row.created_at, at)
                    })
                {
                    result = Some((row.clone(), at));
                }
            }
        }
        cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
        result
            .filter(|(row, _)| row.visible_at(as_of))
            .map(|(value, _)| {
                BufferedValue::from_reserved(value, result_charge).retain_scan_vertex(&mut || {
                    cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
                    advance(&mut work, 1, self.limits.max_work)?;
                    Ok::<(), BufferedScanError<core::convert::Infallible>>(())
                })
            })
            .transpose()
            .map_err(|error| match error {
                BufferedScanError::Read(error) => error,
                BufferedScanError::Control(never) => match never {},
            })
    }

    /// Seek admitted EId intervals across all possibly matching histories, then
    /// fault only the winning edge's property patch. EId bounds are independent
    /// of adjacency order; an absent or propertyless winner reads no sidecar.
    pub async fn edge_at(
        &mut self,
        cx: &QueryCx,
        eid: EId,
        as_of: CommitSeq,
    ) -> Result<Option<BufferedValue<BufferedEdge>>, BufferedReadError> {
        self.begin(cx, as_of)?;
        let result_charge = self.buffer.reserve_scratch(cx, OBJECT_WORKSPACE_BYTES)?;
        let mut result: Option<(AdjacencyEntry, usize, u8)> = None;
        let mut work = 0;
        let mut candidates = self.block_directory.edge_candidates(eid);
        while let Some(at) = self.block_directory.next_edge(&mut candidates, &mut || {
            cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
            advance(&mut work, 1, self.limits.max_work)
        })? {
            let descriptor = self.blocks[at];
            if self.root.blocks[at].first_seq.0 > as_of.0
                || descriptor
                    .summary
                    .is_none_or(|summary| !summary.may_contain_edge(eid))
            {
                continue;
            }
            advance(&mut work, descriptor.rows, self.limits.max_work)?;
            let _workspace = self.buffer.reserve_scratch(cx, OBJECT_WORKSPACE_BYTES)?;
            let (entries, locators) = self.block_keys(cx, at, Admission::Normal).await?;
            for (index, entry) in entries.iter().enumerate() {
                if entry.eid == eid
                    && entry.created_at.0 <= as_of.0
                    && result.as_ref().is_none_or(|(old, old_at, _)| {
                        (old.created_at, *old_at) <= (entry.created_at, at)
                    })
                {
                    let locator = locators.as_ref().map_or(0, |locators| locators[index]);
                    result = Some((*entry, at, locator));
                }
            }
        }
        cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
        let Some((entry, block, locator)) = result.filter(|(entry, _, _)| entry.visible_at(as_of))
        else {
            return Ok(None);
        };
        let props = if locator == 0 {
            Vec::new()
        } else {
            advance(&mut work, 1, self.limits.max_work)?;
            let _workspace = self.buffer.reserve_scratch(cx, OBJECT_WORKSPACE_BYTES)?;
            self.block_property_row(cx, block, locator, Admission::Normal)
                .await?
        };
        cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
        BufferedValue::from_reserved(BufferedEdge { entry, props }, result_charge)
            .retain_scan_edge(&mut || {
                cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
                advance(&mut work, 1, self.limits.max_work)?;
                Ok::<(), BufferedScanError<core::convert::Infallible>>(())
            })
            .map(Some)
            .map_err(|error| match error {
                BufferedScanError::Read(error) => error,
                BufferedScanError::Control(never) => match never {},
            })
    }

    /// Bounded winning adjacency, in the canonical (src, relation, dst, eid)
    /// order. The cap bounds candidate identities, including later-retired
    /// ones: exceeding it refuses the whole read, never returns a prefix.
    /// The admitted source/relation/destination directory seeks possible
    /// blocks. Only topology is read; unrelated blocks and property sidecars
    /// are not needed to resolve this answer. Descriptor visits remain metered.
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
        let mut candidates = BTreeMap::<EId, (AdjacencyEntry, usize)>::new();
        let mut work = 0;
        let direction = if incoming {
            crate::tiered::edge_scan::BufferedEdgeDirection::Incoming
        } else {
            crate::tiered::edge_scan::BufferedEdgeDirection::Outgoing
        };
        let mut blocks = self.block_directory.candidates(vertex, relation, direction);
        while let Some(at) = self.block_directory.next(&mut blocks, &mut || {
            cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
            advance(&mut work, 1, self.limits.max_work)
        })? {
            let descriptor = self.blocks[at];
            if self.root.blocks[at].first_seq.0 > as_of.0
                || descriptor.summary.is_none_or(|summary| {
                    !summary.may_contain_adjacency(vertex, relation, incoming)
                })
            {
                continue;
            }
            advance(&mut work, descriptor.rows, self.limits.max_work)?;
            let _workspace = self.buffer.reserve_scratch(cx, OBJECT_WORKSPACE_BYTES)?;
            let (entries, _) = self.block_keys(cx, at, Admission::Normal).await?;
            for entry in entries {
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
                    .is_none_or(|(old, old_at)| (old.created_at, *old_at) <= (entry.created_at, at))
                {
                    candidates.insert(entry.eid, (entry, at));
                }
            }
        }
        let mut value: Vec<_> = candidates
            .into_values()
            .map(|(row, _)| row)
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
        properties: VertexPropertyAccess,
        observe: &mut (impl FnMut(BufferedScanEvent) -> Result<(), C> + Send),
    ) -> Result<(), BufferedScanError<C>> {
        if self.active.as_ref().is_some_and(|active| {
            active.patch == at
                && (properties == VertexPropertyAccess::Skip
                    || active.properties == VertexPropertyAccess::Read)
        }) {
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
        let rows = self
            .partition
            .decode_scan_patch(at, bytes.as_ref(), properties, None)?;
        drop(bytes);
        cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
        self.active = Some(BufferedValue {
            value: DecodedVertexPatch {
                patch: at,
                properties,
                rows,
            },
            _charge: charge,
        });
        Ok(())
    }

    async fn next_candidate_inner<C: Send>(
        &mut self,
        cx: &QueryCx,
        properties: VertexPropertyAccess,
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
            self.load_patch(cx, head.patch, properties, observe).await?;
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
                    Some(winner) => winner.value = properties.copy_row(row),
                    None => {
                        let charge = self
                            .partition
                            .buffer
                            .reserve_scratch(cx, OBJECT_WORKSPACE_BYTES)
                            .map_err(BufferedReadError::Buffer)?;
                        winning_row = Some(BufferedValue {
                            value: properties.copy_row(row),
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
        let row = winning_row
            .filter(|row| row.visible_at(self.as_of))
            .map(|row| row.retain_scan_vertex(&mut || self.observe(cx, observe)))
            .transpose()?;
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
        self.next_candidate_with_access(cx, VertexPropertyAccess::Read, observe)
            .await
    }

    /// Read identities, lifetimes and labels without decoding vertex property
    /// values. Initial root admission still validates every property and each
    /// refault checks the exact admitted extent checksum. FGSV stores properties
    /// inline, so the same patch bytes are read; scalar/collection allocation
    /// and resolver work are skipped. Complete and topology pulls may alternate:
    /// a complete pull refaults a topology-only active patch when necessary.
    pub async fn next_topology_candidate_with<C: Send>(
        &mut self,
        cx: &QueryCx,
        observe: &mut (impl FnMut(BufferedScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedVertexCandidate<BufferedVertexTopology>>, BufferedScanError<C>> {
        self.next_candidate_with_access(cx, VertexPropertyAccess::Skip, observe)
            .await
            .map(|candidate| {
                candidate.map(|candidate| BufferedVertexCandidate {
                    vid: candidate.vid,
                    row: candidate.row.map(BufferedVertexTopology),
                })
            })
    }

    async fn next_candidate_with_access<C: Send>(
        &mut self,
        cx: &QueryCx,
        properties: VertexPropertyAccess,
        observe: &mut (impl FnMut(BufferedScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedVertexCandidate>, BufferedScanError<C>> {
        if self.done {
            self.finish();
            return Ok(None);
        }
        // Leave terminal set while the future can suspend. Dropping that
        // future must never restart after partially consuming one identity.
        self.done = true;
        let result = self.next_candidate_inner(cx, properties, observe).await;
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
        BranchId, CanonicalPropertyValue, CanonicalScalar, DatabaseSecurityNamespaceId, GraphId,
        PurposeContexts,
    };

    #[test]
    fn retained_metadata_accounts_capacity_and_preserves_opening_peak() {
        let limits = BufferedReadLimits {
            max_root_bytes: 64 * 1024,
            max_source_bytes: 4 * 1024 * 1024,
            max_blocks: 32,
            max_vertex_patches: 32,
            max_work: 10_000,
            buffer: BufferLimits {
                max_frames: 1,
                max_ghost_entries: 2,
                max_extent_bytes: 16 * 1024,
            },
        };
        let root = PartitionRoot {
            graph: GraphId(1),
            branch: BranchId(1),
            partition: 0,
            published_at: CommitSeq(0),
            blocks: Vec::with_capacity(3),
            vertex_patches: Vec::with_capacity(7),
        };
        let policy = 3 * 512 + 4096;
        assert_eq!(limits.metadata_bytes().unwrap(), 16 * 64 * 1024 + policy);
        let expected = policy
            + root.blocks.capacity() * size_of::<crate::root::BlockRef>()
            + root.vertex_patches.capacity() * size_of::<crate::root::PatchRef>()
            + 11 * size_of::<BlockDescriptor>()
            + 13 * size_of::<PatchDescriptor>();
        assert_eq!(
            limits.retained_metadata_bytes(&root, 11, 13).unwrap(),
            expected
        );
        assert!(root.blocks.is_empty() && root.vertex_patches.is_empty());
        assert!(expected > policy, "spare capacity remains charged");
        let larger_ceiling = BufferedReadLimits {
            max_root_bytes: 128 * 1024,
            ..limits
        };
        assert_eq!(
            larger_ceiling.metadata_bytes().unwrap(),
            16 * 128 * 1024 + policy
        );
        assert_eq!(
            larger_ceiling
                .retained_metadata_bytes(&root, 11, 13)
                .unwrap(),
            expected,
            "a ceiling is not a retained allocation"
        );
        assert!(matches!(
            limits.retained_metadata_bytes(&root, usize::MAX, 0),
            Err(BufferedReadError::SizeOverflow)
        ));
    }

    #[test]
    fn reads_admit_complete_decode_and_property_work_before_their_cache_pins() {
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
            let rows = vec![vec![(
                PropertyKeyId(1),
                CanonicalPropertyValue::Scalar(CanonicalScalar::Int(7)),
            )]];
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
                // Three existing payload-admission events plus one descriptor
                // insertion and two interval augmentations. Read limits below
                // are still independently fixed at each original boundary.
                max_work: 6,
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
            assert_eq!(
                view._metadata.bytes(),
                limits
                    .retained_metadata_bytes(
                        &view.root,
                        view.blocks.capacity(),
                        view.patches.capacity(),
                    )
                    .unwrap()
            );
            assert!(view._metadata.bytes() < limits.metadata_bytes().unwrap());
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
                assert_eq!(
                    view.stats().misses,
                    0,
                    "decode work refuses before topology I/O"
                );
            }
            view.limits.max_work = 2;
            assert!(matches!(
                view.edge_at(&query, EId(1), CommitSeq(1)).await,
                Err(BufferedReadError::Limit {
                    resource: "buffered source work",
                    requested: 3,
                    limit: 2,
                })
            ));
            assert_eq!(view.stats().misses, 1, "property payload was never pinned");
            // Exact-source routing admits a binary-seek comparison and the
            // matching descriptor before its one topology row.
            view.limits.max_work = 3;
            let found = view
                .adjacency_at(&query, VId(1), None, false, CommitSeq(1), 1)
                .await
                .unwrap();
            assert_eq!(found.as_ref(), &vec![entry]);
            assert_eq!(
                view.stats().misses,
                1,
                "adjacency needs no property payload"
            );
            drop(found);
            // The point read's three payload events are followed by four
            // affine-retention events: owner, property vector, scalar heap,
            // and final reservation split. Every incomplete prefix refuses.
            const POINT_WORK: usize = 3 + 4;
            for allowance in 3..POINT_WORK {
                view.limits.max_work = allowance;
                assert!(matches!(
                    view.edge_at(&query, EId(1), CommitSeq(1)).await,
                    Err(BufferedReadError::Limit {
                        resource: "buffered source work",
                        requested,
                        limit,
                    }) if requested == allowance + 1 && limit == allowance
                ));
            }
            view.limits.max_work = POINT_WORK;
            let found = view
                .edge_at(&query, EId(1), CommitSeq(1))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(found.entry, entry);
            assert_eq!(found.props, rows[0]);
            drop(found);
            view.limits.max_work = 3;
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

    #[test]
    fn indexed_point_and_adjacency_reads_fit_a_sublinear_metadata_allowance() {
        const COUNT: usize = 256;
        let dir =
            std::env::temp_dir().join(format!("fgdb-cold-indexed-work-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (_, report) = run_async_under_lab(0xb0_14, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let query = contexts.query();
            let store = BlockStore::open(
                &commit,
                &dir,
                [0x19; 32],
                DatabaseSecurityNamespaceId([0x20; 32]),
            )
            .await
            .unwrap();
            let entries: Vec<_> = (0..COUNT)
                .map(|at| AdjacencyEntry {
                    src: VId(at as u128 * 4),
                    relation: RelationId(1),
                    dst: VId(at as u128 * 4 + 1),
                    eid: EId(at as u128 * 4),
                    created_at: CommitSeq(if at + 1 == COUNT { 2 } else { 1 }),
                    retired_at: None,
                })
                .collect();
            let mut blocks = Vec::new();
            for entry in &entries {
                let block = store
                    .put(&commit, &crate::encode_block(0, None, &[*entry]).unwrap())
                    .await
                    .unwrap();
                blocks.push(crate::root::BlockRef {
                    block_id: block.0,
                    first_seq: entry.created_at,
                    last_seq: entry.created_at,
                });
            }
            let id = store
                .put_root(
                    &commit,
                    &PartitionRoot {
                        graph: GraphId(1),
                        branch: BranchId(1),
                        partition: 0,
                        published_at: CommitSeq(2),
                        blocks,
                        vertex_patches: vec![],
                    },
                )
                .await
                .unwrap();
            let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
            let limits = BufferedReadLimits {
                max_root_bytes: 64 * 1024,
                max_source_bytes: COUNT * 4096,
                max_blocks: COUNT,
                max_vertex_patches: 0,
                // Full object/row admission, four controlled heap sorts,
                // descriptor insertion and two interval augmentations.
                max_work: COUNT * (12 * COUNT.ilog2() as usize + 16),
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
            // At most two tree nodes per level plus one decoded row are
            // needed here; full linear descriptor scans exceed this budget.
            view.limits.max_work = 64;
            let visits = |stats: BufferStats| stats.hits + stats.misses;
            for entry in [entries[0], entries[COUNT / 2], entries[COUNT - 2]] {
                let before = visits(view.stats());
                let found = view
                    .edge_at(&query, entry.eid, CommitSeq(1))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(found.entry, entry);
                assert!(found.props.is_empty());
                assert_eq!(visits(view.stats()) - before, 1);
                drop(found);
                for relation in [None, Some(RelationId(1))] {
                    for incoming in [false, true] {
                        let endpoint = if incoming { entry.dst } else { entry.src };
                        let before = visits(view.stats());
                        let found = view
                            .adjacency_at(&query, endpoint, relation, incoming, CommitSeq(1), 1)
                            .await
                            .unwrap();
                        assert_eq!(found.as_ref(), &vec![entry]);
                        assert_eq!(visits(view.stats()) - before, 1);
                    }
                }
            }
            let before = visits(view.stats());
            for eid in [EId(1), EId(u128::MAX), entries[COUNT - 1].eid] {
                assert!(
                    view.edge_at(&query, eid, CommitSeq(1))
                        .await
                        .unwrap()
                        .is_none()
                );
            }
            for (endpoint, relation, incoming) in [
                (VId(1), None, false),
                (VId(0), None, true),
                (VId(0), Some(RelationId(2)), false),
                (VId(1), Some(RelationId(2)), true),
            ] {
                let found = view
                    .adjacency_at(&query, endpoint, relation, incoming, CommitSeq(1), 1)
                    .await
                    .unwrap();
                assert!(found.is_empty());
            }
            assert_eq!(
                visits(view.stats()),
                before,
                "absent and future candidates do no I/O"
            );
            let future = view
                .edge_at(&query, entries[COUNT - 1].eid, CommitSeq(2))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(future.entry, entries[COUNT - 1]);
            assert_eq!(visits(view.stats()) - before, 1);
            drop(future);
            drop(view);
            assert_eq!(pool.used(), 0);
        });
        assert!(report.invariant_violations.is_empty(), "{report:?}");
    }
}
