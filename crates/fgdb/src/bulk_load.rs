//! Bounded online ingestion through ordinary prepared Chronicle commits.
//!
//! The source must be replayable. Preflight seals every chunk's complete source
//! transcript; replay verifies it before allocating identities or preparing a
//! write. Malformed preflight input leaves the graph unchanged. A changed replay
//! returns only the already committed prefix. Payload memory is one chunk plus
//! one admitted scalar encoding. Explicit logical source/key/chunk bounds cover
//! both passes, including resumed prefixes. These are not allocator-byte or
//! external-memory guarantees. Edges name preceding vertices; keys are unique
//! across both kinds. Sources must make each next()/clone() call finite and
//! enforce their own I/O and decoding limits; the loader checkpoints around next().
//!
//! Resume supplies the same source and policy plus a checkpoint. After an
//! uncertain commit, reopen first: choose `pending` only if its frontier equals
//! the recovered frontier, otherwise choose `committed`. No checkpoint is an
//! independent durability authority: its identities are checked against the
//! recovered graph. Source-prefix contents/order must remain unchanged. This
//! is caller-managed continuation, not a durable import registry.

use crate::{CrashPoint, Database, WriteBatch, WriteTxnError};
use asupersync::fs::Vfs;
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_strata::{edge_props::EdgePropertyPatchError, vertex::VertexPatchError};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, EId, QueryCx, VId};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

mod source;

#[derive(Clone, Debug)]
pub struct BulkVertex {
    pub key: String,
    pub labels: Vec<LabelId>,
    pub props: Vec<(PropertyKeyId, CanonicalScalar)>,
}
#[derive(Clone, Debug)]
pub struct BulkEdge {
    pub key: String,
    pub source: BulkEndpoint,
    pub destination: BulkEndpoint,
    pub relation: RelationId,
    pub props: Vec<(PropertyKeyId, CanonicalScalar)>,
}

/// The vertex at one end of a bulk-loaded edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BulkEndpoint {
    /// A vertex row of the same source, named by its caller key.
    Key(String),
    /// The one vertex committed AS OF THE LOAD'S BASIS (the frontier before
    /// its first chunk) that matches. Resolving at the basis makes preflight,
    /// every chunk and every resume agree, even when this load creates
    /// vertices that also match. No match or several is a typed preflight
    /// refusal. Boxed so a key endpoint, the common case, stays small.
    Match(Box<EndpointMatch>),
}
/// A vertex that carries `label`, when given, and whose property `key` equals
/// `value` on its visible row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointMatch {
    pub label: Option<LabelId>,
    pub key: PropertyKeyId,
    pub value: CanonicalScalar,
}
impl From<String> for BulkEndpoint {
    fn from(key: String) -> Self {
        Self::Key(key)
    }
}
impl From<&str> for BulkEndpoint {
    fn from(key: &str) -> Self {
        Self::Key(key.to_owned())
    }
}
impl BulkEndpoint {
    /// An endpoint naming the committed vertex that matches.
    #[must_use]
    pub fn matching(label: Option<LabelId>, key: PropertyKeyId, value: CanonicalScalar) -> Self {
        Self::Match(Box::new(EndpointMatch { label, key, value }))
    }

    /// The caller key, for an endpoint that names a row of this source.
    #[must_use]
    pub fn key(&self) -> Option<&str> {
        match self {
            Self::Key(key) => Some(key),
            Self::Match(_) => None,
        }
    }
}
#[derive(Clone, Debug)]
pub enum BulkRow {
    Vertex(BulkVertex),
    Edge(BulkEdge),
}
#[derive(Clone, Debug)]
pub struct BulkLoadPolicy {
    pub rows_per_chunk: usize,
    /// Coordinate for vertex-only chunks and shared vertex initialization.
    pub vertex_relation: RelationId,
    pub resume: Option<BulkLoadCheckpoint>,
    /// Whole-source row/key count, including a resumed prefix; not a chunk quota.
    pub max_source_rows: usize,
    /// UTF-8 bytes of each caller key or endpoint reference.
    pub max_key_bytes: usize,
    /// Sum of unique caller-key bytes over the complete source. Internal maps
    /// may retain multiple copies; this is not an allocator-byte measurement.
    pub max_total_key_bytes: usize,
    /// Per-chunk source transcript bytes: row tags, framed keys, labels,
    /// property keys and canonical scalar encodings. Excludes the fixed chunk
    /// header/trailer and caller-allocated spare capacities. Chunk boundaries
    /// remain rows_per_chunk; refusal never silently splits a transaction.
    pub max_chunk_bytes: usize,
}
impl BulkLoadPolicy {
    pub const MAX_ROWS_PER_CHUNK: usize = 65_536;

    #[must_use]
    pub const fn new(rows_per_chunk: usize, vertex_relation: RelationId) -> Self {
        Self {
            rows_per_chunk,
            vertex_relation,
            resume: None,
            max_source_rows: 1_000_000,
            max_key_bytes: 1024,
            max_total_key_bytes: 64 * 1024 * 1024,
            max_chunk_bytes: 8 * 1024 * 1024,
        }
    }

    /// Inclusive, independent bounds; zero is a valid fail-closed limit.
    /// The whole-source limits include previously committed rows on resume.
    #[must_use]
    pub const fn with_source_limits(
        mut self,
        max_source_rows: usize,
        max_key_bytes: usize,
        max_total_key_bytes: usize,
        max_chunk_bytes: usize,
    ) -> Self {
        self.max_source_rows = max_source_rows;
        self.max_key_bytes = max_key_bytes;
        self.max_total_key_bytes = max_total_key_bytes;
        self.max_chunk_bytes = max_chunk_bytes;
        self
    }
}
// ChunkFit's candidates 2^0 ..= 2^16 are exactly the powers of two up to the cap.
const _: () = assert!(BulkLoadPolicy::MAX_ROWS_PER_CHUNK.is_power_of_two());

/// Fits `rows_per_chunk` to a source: fed every row in source order, it
/// reports the largest power of two, at most
/// [`BulkLoadPolicy::MAX_ROWS_PER_CHUNK`], under which every chunk carries at
/// most `budget` bytes of source transcript, the same measure preflight
/// refuses past `max_chunk_bytes`. Chunks start at row 0, as in preflight and
/// on resume, so the answer is a function of the source alone and a resumed
/// load recomputes it exactly.
#[derive(Clone, Debug)]
pub struct ChunkFit {
    budget: usize,
    rows: usize,
    /// Running transcript bytes of the current chunk for 2^k rows per chunk.
    open: [usize; Self::SIZES],
    fits: [bool; Self::SIZES],
}
impl ChunkFit {
    const SIZES: usize = BulkLoadPolicy::MAX_ROWS_PER_CHUNK.ilog2() as usize + 1;

    #[must_use]
    pub const fn new(budget: usize) -> Self {
        Self {
            budget,
            rows: 0,
            open: [0; Self::SIZES],
            fits: [true; Self::SIZES],
        }
    }

    pub fn push(&mut self, cx: &QueryCx, row: &BulkRow) -> Result<(), BulkLoadErrorKind> {
        let bytes = source::transcript_len(cx, self.rows, row)?;
        for (k, (open, fits)) in self.open.iter_mut().zip(&mut self.fits).enumerate() {
            if self.rows.is_multiple_of(1 << k) {
                *open = 0;
            }
            *open = open.saturating_add(bytes);
            *fits &= *open <= self.budget;
        }
        self.rows = self
            .rows
            .checked_add(1)
            .ok_or(BulkLoadErrorKind::CounterOverflow)?;
        Ok(())
    }

    /// The fitted rows per chunk, or `None` when one row alone exceeds the
    /// budget.
    #[must_use]
    pub fn rows_per_chunk(&self) -> Option<usize> {
        self.fits.iter().rposition(|&fits| fits).map(|k| 1 << k)
    }
}

#[derive(Clone, Debug, Default)]
pub struct BulkLoadCheckpoint {
    pub vertices: BTreeMap<String, VId>,
    pub edges: BTreeMap<String, EId>,
    pub next_row: usize,
    pub frontier: CommitSeq,
    pub committed_chunks: usize,
}
#[derive(Debug)]
pub enum BulkLoadErrorKind {
    InvalidPolicy,
    InvalidResume,
    /// Replay differs from preflight; this chunk has not entered publication.
    /// The row is the chunk start for content drift, or the first missing/extra row.
    SourceChanged {
        row: usize,
    },
    SourceEncoding {
        row: usize,
        source: fgdb_types::ScalarEncodeError,
    },
    CounterOverflow,
    /// A source reader/decoder failed. No partially read chunk was submitted.
    Source {
        row: usize,
        source: Box<dyn core::error::Error + Send + Sync>,
    },
    /// Exact observed logical admission count, never estimated resident bytes.
    /// Dimensions are source_rows, key_bytes, total_key_bytes or chunk_bytes.
    SourceLimit {
        row: usize,
        dimension: &'static str,
        limit: usize,
        observed: usize,
    },
    SourceAllocation {
        row: usize,
    },
    DuplicateCallerKey {
        key: String,
    },
    DanglingEndpointKey {
        edge: String,
        endpoint: String,
    },
    /// A `BulkEndpoint::Match` named no vertex committed as of the basis.
    /// `row` is the zero-based source row of the edge.
    UnresolvedEndpoint {
        row: usize,
        edge: String,
        endpoint: BulkEndpoint,
    },
    /// A `BulkEndpoint::Match` named more than one vertex as of the basis.
    AmbiguousEndpoint {
        row: usize,
        edge: String,
        endpoint: BulkEndpoint,
    },
    InvalidVertex {
        key: String,
        source: VertexPatchError,
    },
    InvalidEdge {
        key: String,
        source: EdgePropertyPatchError,
    },
    Write(WriteTxnError),
    /// A validated baseline or durable chunk could not be persisted/announced.
    /// No pending write is implied; inspect `BulkLoadError::committed`.
    Checkpoint(std::io::Error),
}
#[derive(Debug)]
pub struct BulkLoadError {
    pub kind: BulkLoadErrorKind,
    /// Last positively acknowledged chunk. Never labels an unknown commit as absent.
    pub committed: BulkLoadCheckpoint,
    /// Candidate checkpoint for a commit that was attempted but not acknowledged.
    pub pending: Option<BulkLoadCheckpoint>,
}
impl core::fmt::Display for BulkLoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "bulk load stopped at row {}: {:?}",
            self.committed.next_row, self.kind
        )
    }
}
impl core::error::Error for BulkLoadError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match &self.kind {
            BulkLoadErrorKind::Source { source, .. } => Some(source.as_ref()),
            BulkLoadErrorKind::SourceEncoding { source, .. } => Some(source),
            BulkLoadErrorKind::Write(source) => Some(source),
            BulkLoadErrorKind::Checkpoint(source) => Some(source),
            _ => None,
        }
    }
}

/// The vertices a load has written, by caller key. The checkpoint keeps an
/// ordered map; the chunk loop resolves endpoints through hashed ones, since a
/// string-keyed tree search per endpoint was ~5% of a web-google load
/// (fgdb-79kyi). Only probed, so no order reaches a result.
trait VertexKeys {
    fn find(&self, key: &str) -> Option<VId>;
}
impl VertexKeys for BTreeMap<String, VId> {
    fn find(&self, key: &str) -> Option<VId> {
        self.get(key).copied()
    }
}
impl VertexKeys for HashMap<String, VId> {
    fn find(&self, key: &str) -> Option<VId> {
        self.get(key).copied()
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Resolve an edge endpoint: a caller key against this source's vertices
    /// (`fresh` for the current chunk, then the committed prefix), a match
    /// against the committed graph as of `basis` through the property index.
    /// A match must name exactly one visible vertex carrying the label (when
    /// given) and the exact property value on its visible row.
    #[allow(clippy::too_many_arguments)]
    fn resolve_endpoint(
        &self,
        cx: &QueryCx,
        basis: CommitSeq,
        row: usize,
        edge: &str,
        endpoint: &BulkEndpoint,
        fresh: &impl VertexKeys,
        committed: &impl VertexKeys,
    ) -> Result<VId, BulkLoadErrorKind> {
        let (label, key, value) = match endpoint {
            BulkEndpoint::Key(key) => {
                return fresh
                    .find(key)
                    .or_else(|| committed.find(key))
                    .ok_or_else(|| BulkLoadErrorKind::DanglingEndpointKey {
                        edge: edge.to_owned(),
                        endpoint: key.clone(),
                    });
            }
            BulkEndpoint::Match(matched) => (&matched.label, &matched.key, &matched.value),
        };
        let unresolved = || BulkLoadErrorKind::UnresolvedEndpoint {
            row,
            edge: edge.to_owned(),
            endpoint: endpoint.clone(),
        };
        // A stored NULL is never an equality candidate (as for MATCH).
        if matches!(value, CanonicalScalar::Null) {
            return Err(unresolved());
        }
        let mut control = |_| source::checkpoint(cx);
        let mut found = None;
        for vid in self.snapshot.property_index().lookup(*key, value) {
            let Some(vertex) =
                crate::gql_exec::source::find_vertex(&self.snapshot, *vid, basis, &mut control)?
            else {
                continue;
            };
            // Historical membership is only a candidate superset: recheck the
            // visible row's label and exact value.
            if label.is_some_and(|label| !vertex.labels.contains(&label))
                || vertex
                    .props
                    .binary_search_by_key(key, |(property, _)| *property)
                    .map_or(true, |at| vertex.props[at].1 != *value)
            {
                continue;
            }
            if found.replace(vertex.vid).is_some() {
                return Err(BulkLoadErrorKind::AmbiguousEndpoint {
                    row,
                    edge: edge.to_owned(),
                    endpoint: endpoint.clone(),
                });
            }
        }
        found.ok_or_else(unresolved)
    }

    // BulkLoadError is a resume report (kind plus committed and pending
    // checkpoints) returned once per load, not per row: its size is immaterial.
    #[allow(clippy::result_large_err)]
    pub async fn bulk_load<I>(
        &mut self,
        cx: &QueryCx,
        commit_cx: &CommitCx,
        source: I,
        policy: BulkLoadPolicy,
    ) -> Result<BulkLoadCheckpoint, BulkLoadError>
    where
        I: IntoIterator<Item = BulkRow>,
        I::IntoIter: Clone,
    {
        self.bulk_load_with_crash(cx, commit_cx, source, policy, None)
            .await
    }

    /// Production path with the existing commit crash seam at an absolute,
    /// zero-based chunk index. Faults do not introduce another writer.
    #[doc(hidden)]
    #[allow(clippy::result_large_err)] // once-per-load resume report, as above
    pub async fn bulk_load_with_crash<I>(
        &mut self,
        cx: &QueryCx,
        commit_cx: &CommitCx,
        source: I,
        policy: BulkLoadPolicy,
        crash: Option<(usize, CrashPoint)>,
    ) -> Result<BulkLoadCheckpoint, BulkLoadError>
    where
        I: IntoIterator<Item = BulkRow>,
        I::IntoIter: Clone,
    {
        self.bulk_load_with_checkpoint(cx, commit_cx, source, policy, crash, |_| Ok(()))
            .await
    }

    /// Invoke `acknowledged` after each successful durable commit and before
    /// starting another chunk. A hook error stops ingestion with the durable
    /// chunk in `committed` and no `pending` candidate. The hook must not label
    /// an unacknowledged write as committed.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::result_large_err)] // once-per-load resume report, as above
    pub async fn bulk_load_with_checkpoint<I, F>(
        &mut self,
        cx: &QueryCx,
        commit_cx: &CommitCx,
        source: I,
        policy: BulkLoadPolicy,
        crash: Option<(usize, CrashPoint)>,
        acknowledged: F,
    ) -> Result<BulkLoadCheckpoint, BulkLoadError>
    where
        I: IntoIterator<Item = BulkRow>,
        I::IntoIter: Clone,
        F: FnMut(&BulkLoadCheckpoint) -> Result<(), std::io::Error>,
    {
        self.try_bulk_load_with_checkpoint(
            cx,
            commit_cx,
            source.into_iter().map(Ok::<_, core::convert::Infallible>),
            policy,
            crash,
            acknowledged,
        )
        .await
    }

    /// Replayable, fallible ingestion without converting a decoder error to EOF
    /// or buffering an entire decoded input. All preflight source failures leave
    /// this invocation's graph unchanged. Replay failures preserve the completed
    /// prefix and never mint a pending candidate for an unread or unchecked chunk.
    #[allow(clippy::result_large_err)] // once-per-load resume report, as above
    pub async fn try_bulk_load<I, E>(
        &mut self,
        cx: &QueryCx,
        commit_cx: &CommitCx,
        source: I,
        policy: BulkLoadPolicy,
    ) -> Result<BulkLoadCheckpoint, BulkLoadError>
    where
        I: IntoIterator<Item = Result<BulkRow, E>>,
        I::IntoIter: Clone,
        E: core::error::Error + Send + Sync + 'static,
    {
        self.try_bulk_load_with_checkpoint(cx, commit_cx, source, policy, None, |_| Ok(()))
            .await
    }

    /// Fallible counterpart of bulk_load_with_checkpoint, using the very same
    /// preparation/publication loop, limits and acknowledgement boundary.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::result_large_err)] // once-per-load resume report, as above
    pub async fn try_bulk_load_with_checkpoint<I, E, F>(
        &mut self,
        cx: &QueryCx,
        commit_cx: &CommitCx,
        source: I,
        policy: BulkLoadPolicy,
        crash: Option<(usize, CrashPoint)>,
        acknowledged: F,
    ) -> Result<BulkLoadCheckpoint, BulkLoadError>
    where
        I: IntoIterator<Item = Result<BulkRow, E>>,
        I::IntoIter: Clone,
        E: core::error::Error + Send + Sync + 'static,
        F: FnMut(&BulkLoadCheckpoint) -> Result<(), std::io::Error>,
    {
        self.try_bulk_load_with_admission(cx, commit_cx, source, policy, crash, |_| {
            Ok(acknowledged)
        })
        .await
    }

    /// Validate the complete source and any resumed prefix before invoking
    /// `admitted` exactly once. The callback receives the checked baseline and
    /// returns the ordinary per-durable-chunk checkpoint callback. This lets a
    /// host publish its import origin or reconstructed recovery progress only
    /// AFTER endpoint resolution, chunk admission and graph-prefix validation,
    /// but BEFORE any new identity allocation or write in this invocation.
    ///
    /// Existing checkpoint-only entrypoints retain their original callback
    /// sequence: one call per new durable chunk, never one for admission. An
    /// empty load or completed resume still invokes `admitted`, but performs
    /// no new commit. Admission callback failure returns Checkpoint with the
    /// checked baseline and no pending write; its own external side effects
    /// are not rolled back. Preflight failure invokes neither callback.
    ///
    /// The returned callback may own the same output state used by admission;
    /// no interior mutability or second source-validation pass is required.
    /// Each later source chunk is still replay-verified before it can commit.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::result_large_err)] // once-per-load resume report
    pub async fn try_bulk_load_with_admission<I, E, F, A>(
        &mut self,
        cx: &QueryCx,
        commit_cx: &CommitCx,
        source: I,
        mut policy: BulkLoadPolicy,
        crash: Option<(usize, CrashPoint)>,
        admitted: A,
    ) -> Result<BulkLoadCheckpoint, BulkLoadError>
    where
        I: IntoIterator<Item = Result<BulkRow, E>>,
        I::IntoIter: Clone,
        E: core::error::Error + Send + Sync + 'static,
        F: FnMut(&BulkLoadCheckpoint) -> Result<(), std::io::Error>,
        A: FnOnce(&BulkLoadCheckpoint) -> Result<F, std::io::Error>,
    {
        let resuming = policy.resume.is_some();
        // Move, do not duplicate, caller-owned checkpoint maps before admission.
        let mut committed = policy.resume.take().unwrap_or_default();
        let mut source = source.into_iter();
        // The frontier before this load's first chunk: every chunk commits
        // exactly one sequence, and a resume requires no interleaved writer.
        let mut basis = CommitSeq(0);
        let no_keys = BTreeMap::<String, VId>::new();
        let preflight = (|| {
            source::checkpoint(cx)?;
            self.ensure_writable()
                .map_err(|e| BulkLoadErrorKind::Write(e.into()))?;
            if policy.rows_per_chunk == 0
                || policy.rows_per_chunk > BulkLoadPolicy::MAX_ROWS_PER_CHUNK
            {
                return Err(BulkLoadErrorKind::InvalidPolicy);
            }
            let frontier = self
                .frontier()
                .map_err(|e| BulkLoadErrorKind::Write(e.into()))?;
            if resuming && committed.frontier != frontier {
                return Err(BulkLoadErrorKind::InvalidResume);
            }
            if !resuming {
                committed.frontier = frontier;
            }
            basis = u64::try_from(committed.committed_chunks)
                .ok()
                .and_then(|chunks| committed.frontier.0.checked_sub(chunks))
                .map(CommitSeq)
                .ok_or(BulkLoadErrorKind::InvalidResume)?;
            source::admit_checkpoint(cx, &committed, &policy)?;
            // Probed, never iterated: hashed sets spare an ordered set's string
            // comparisons over every source key (fgdb-79kyi).
            let mut keys = HashSet::new();
            let mut vertices = HashSet::new();
            let mut prefix_vertices = BTreeSet::new();
            let mut prefix_edges = BTreeSet::new();
            let mut count = 0usize;
            let mut seals = Vec::new();
            let mut transcript = source::ChunkHasher::new(0, &policy);
            let mut total_key_bytes = 0usize;
            let mut audit = source.clone();
            loop {
                let index = count;
                let Some(row) = source::next_row(cx, &mut audit, index)? else {
                    break;
                };
                count = index
                    .checked_add(1)
                    .ok_or(BulkLoadErrorKind::CounterOverflow)?;
                source::limit(index, "source_rows", policy.max_source_rows, count)?;
                source::admit_keys(index, &row, &policy)?;
                source::admit_row(&row)?;
                transcript.push(cx, &row)?;
                if count.is_multiple_of(policy.rows_per_chunk) {
                    seals
                        .try_reserve(1)
                        .map_err(|_| BulkLoadErrorKind::SourceAllocation { row: index })?;
                    seals.push(transcript.finish());
                    transcript = source::ChunkHasher::new(count, &policy);
                }
                let key = match &row {
                    BulkRow::Vertex(v) => &v.key,
                    BulkRow::Edge(e) => &e.key,
                };
                total_key_bytes = total_key_bytes
                    .checked_add(key.len())
                    .ok_or(BulkLoadErrorKind::CounterOverflow)?;
                source::limit(
                    index,
                    "total_key_bytes",
                    policy.max_total_key_bytes,
                    total_key_bytes,
                )?;
                if !keys.insert(key.clone()) {
                    return Err(BulkLoadErrorKind::DuplicateCallerKey { key: key.clone() });
                }
                match row {
                    BulkRow::Vertex(v) => {
                        if index < committed.next_row {
                            let id = committed
                                .vertices
                                .get(&v.key)
                                .ok_or(BulkLoadErrorKind::InvalidResume)?;
                            let row = self
                                .vertex(*id)
                                .map_err(|e| BulkLoadErrorKind::Write(e.into()))?
                                .ok_or(BulkLoadErrorKind::InvalidResume)?;
                            if row.labels != v.labels
                                || row.props != v.props
                                || !prefix_vertices.insert(*id)
                            {
                                return Err(BulkLoadErrorKind::InvalidResume);
                            }
                        }
                        vertices.insert(v.key);
                    }
                    BulkRow::Edge(e) => {
                        for endpoint in [&e.source, &e.destination] {
                            match endpoint {
                                BulkEndpoint::Key(key) => {
                                    if !vertices.contains(key) {
                                        return Err(BulkLoadErrorKind::DanglingEndpointKey {
                                            edge: e.key.clone(),
                                            endpoint: key.clone(),
                                        });
                                    }
                                }
                                BulkEndpoint::Match(_) => {
                                    self.resolve_endpoint(
                                        cx, basis, index, &e.key, endpoint, &no_keys, &no_keys,
                                    )?;
                                }
                            }
                        }
                        if index < committed.next_row {
                            let id = committed
                                .edges
                                .get(&e.key)
                                .ok_or(BulkLoadErrorKind::InvalidResume)?;
                            let row = self
                                .edge(*id)
                                .map_err(|e| BulkLoadErrorKind::Write(e.into()))?
                                .ok_or(BulkLoadErrorKind::InvalidResume)?;
                            let prefix = |endpoint| {
                                self.resolve_endpoint(
                                    cx,
                                    basis,
                                    index,
                                    &e.key,
                                    endpoint,
                                    &no_keys,
                                    &committed.vertices,
                                )
                                .map_err(|_| BulkLoadErrorKind::InvalidResume)
                            };
                            if row.entry.src != prefix(&e.source)?
                                || row.entry.dst != prefix(&e.destination)?
                                || row.entry.relation != e.relation
                                || row.props != e.props
                                || !prefix_edges.insert(*id)
                            {
                                return Err(BulkLoadErrorKind::InvalidResume);
                            }
                        }
                    }
                }
            }
            if committed.next_row > count
                || prefix_vertices.len() != committed.vertices.len()
                || prefix_edges.len() != committed.edges.len()
                || (committed.next_row < count
                    && !committed.next_row.is_multiple_of(policy.rows_per_chunk))
                || committed.committed_chunks != committed.next_row.div_ceil(policy.rows_per_chunk)
            {
                return Err(BulkLoadErrorKind::InvalidResume);
            }
            if !count.is_multiple_of(policy.rows_per_chunk) {
                seals
                    .try_reserve(1)
                    .map_err(|_| BulkLoadErrorKind::SourceAllocation { row: count })?;
                seals.push(transcript.finish());
            }
            Ok(seals)
        })();
        let seals = match preflight {
            Ok(seals) => seals,
            Err(kind) => {
                return Err(BulkLoadError {
                    kind,
                    committed,
                    pending: None,
                });
            }
        };
        if seals.is_empty()
            && let Err(kind) = source::expect_end(cx, &mut source, 0)
        {
            return Err(BulkLoadError {
                kind,
                committed,
                pending: None,
            });
        }
        // Nothing externally acknowledges a caller-supplied/reconstructed
        // prefix until the SAME engine preflight has validated all of it.
        // In particular BulkEndpoint::Match must resolve at the load basis,
        // and a previously committed edge must have those exact endpoints.
        let mut acknowledged = match source::checkpoint(cx)
            .and_then(|()| admitted(&committed).map_err(BulkLoadErrorKind::Checkpoint))
        {
            Ok(acknowledged) => acknowledged,
            Err(kind) => {
                return Err(BulkLoadError {
                    kind,
                    committed,
                    pending: None,
                });
            }
        };
        // `committed.vertices`, hashed for endpoint resolution, and extended
        // with each chunk exactly when the checkpoint is.
        let mut vertex_keys: HashMap<String, VId> = committed
            .vertices
            .iter()
            .map(|(key, &id)| (key.clone(), id))
            .collect();
        for (index, seal) in seals.iter().enumerate() {
            let skip = seal.end() <= committed.next_row;
            let chunk = match source::read_verified(
                cx,
                &mut source,
                seal,
                &policy,
                !skip,
                index + 1 == seals.len(),
            ) {
                Ok(chunk) => chunk,
                Err(kind) => {
                    return Err(BulkLoadError {
                        kind,
                        committed,
                        pending: None,
                    });
                }
            };
            if skip {
                continue;
            }
            let Some(next_chunks) = committed.committed_chunks.checked_add(1) else {
                return Err(BulkLoadError {
                    kind: BulkLoadErrorKind::CounterOverflow,
                    committed,
                    pending: None,
                });
            };
            let mut new_vertices = HashMap::new();
            let mut new_edges = BTreeMap::new();
            let prepared = (|| {
                let mut vertex_batch = WriteBatch::new(policy.vertex_relation);
                let mut edge_batches: BTreeMap<RelationId, WriteBatch> = BTreeMap::new();
                for (offset, row) in chunk.into_iter().enumerate() {
                    source::checkpoint(cx)?;
                    let ordinal = seal.start + offset;
                    match row {
                        BulkRow::Vertex(v) => {
                            let ElementId::Vertex(id) = self
                                .allocate_identity(
                                    cx,
                                    GraphInsertRequest::Vertex { row: 0, vertex: 0 },
                                )
                                .map_err(BulkLoadErrorKind::Write)?
                            else {
                                unreachable!("typed allocator")
                            };
                            vertex_batch.create_vertex(id, v.labels, v.props);
                            new_vertices.insert(v.key, id);
                        }
                        BulkRow::Edge(e) => {
                            let src = self.resolve_endpoint(
                                cx,
                                basis,
                                ordinal,
                                &e.key,
                                &e.source,
                                &new_vertices,
                                &vertex_keys,
                            )?;
                            let dst = self.resolve_endpoint(
                                cx,
                                basis,
                                ordinal,
                                &e.key,
                                &e.destination,
                                &new_vertices,
                                &vertex_keys,
                            )?;
                            let ElementId::Edge(id) = self
                                .allocate_identity(cx, GraphInsertRequest::Edge { row: 0, edge: 0 })
                                .map_err(BulkLoadErrorKind::Write)?
                            else {
                                unreachable!("typed allocator")
                            };
                            edge_batches
                                .entry(e.relation)
                                .or_insert_with(|| WriteBatch::new(e.relation))
                                .add_edge(id, src, dst, e.props);
                            new_edges.insert(e.key, id);
                        }
                    }
                }
                let mut batches = Vec::new();
                if !vertex_batch.is_empty() {
                    batches.push(vertex_batch);
                }
                batches.extend(edge_batches.into_values());
                self.prepare_atomic_writes(batches)
                    .map_err(BulkLoadErrorKind::Write)
            })();
            let prepared = match prepared {
                Ok(prepared) => prepared,
                Err(kind) => {
                    return Err(BulkLoadError {
                        kind,
                        committed,
                        pending: None,
                    });
                }
            };
            let fault = crash
                .filter(|(index, _)| *index == committed.committed_chunks)
                .map(|(_, point)| point);
            match self
                .commit_prepared_with_crash(commit_cx, prepared, fault)
                .await
            {
                Ok(frontier) => {
                    vertex_keys.extend(new_vertices.iter().map(|(key, &id)| (key.clone(), id)));
                    committed.vertices.extend(new_vertices);
                    committed.edges.extend(new_edges);
                    committed.frontier = frontier;
                    committed.next_row = seal.end();
                    committed.committed_chunks = next_chunks;
                    if let Err(error) = acknowledged(&committed) {
                        return Err(BulkLoadError {
                            kind: BulkLoadErrorKind::Checkpoint(error),
                            committed,
                            pending: None,
                        });
                    }
                }
                Err(source) => {
                    let mut pending = committed.clone();
                    pending.vertices.extend(new_vertices);
                    pending.edges.extend(new_edges);
                    pending.next_row = seal.end();
                    pending.committed_chunks = next_chunks;
                    // A prepared commit advances exactly one sequence. At
                    // exhaustion no successor exists, hence no candidate.
                    let pending = committed.frontier.0.checked_add(1).map(|next| {
                        pending.frontier = CommitSeq(next);
                        pending
                    });
                    return Err(BulkLoadError {
                        kind: BulkLoadErrorKind::Write(source.into()),
                        committed,
                        pending,
                    });
                }
            }
        }
        Ok(committed)
    }
}
