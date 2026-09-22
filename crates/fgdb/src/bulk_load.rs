//! Bounded online ingestion through ordinary prepared Chronicle commits.
//!
//! The source must be replayable. Preflight seals every chunk's complete source
//! transcript; replay verifies it before allocating identities or preparing a
//! write. Malformed preflight input leaves the graph unchanged. A changed replay
//! returns only the already committed prefix. Payload memory is one chunk plus
//! one scalar encoding; caller-key maps and chunk seals grow with
//! the load. Edges name preceding vertices. Keys are unique across both kinds.
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
use std::collections::{BTreeMap, BTreeSet};

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
    pub source: String,
    pub destination: String,
    pub relation: RelationId,
    pub props: Vec<(PropertyKeyId, CanonicalScalar)>,
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
}
impl BulkLoadPolicy {
    #[must_use]
    pub const fn new(rows_per_chunk: usize, vertex_relation: RelationId) -> Self {
        Self {
            rows_per_chunk,
            vertex_relation,
            resume: None,
        }
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
    SourceChanged { row: usize },
    SourceEncoding { row: usize, source: fgdb_types::ScalarEncodeError },
    CounterOverflow,
    DuplicateCallerKey {
        key: String,
    },
    DanglingEndpointKey {
        edge: String,
        endpoint: String,
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
    /// The chunk is durable, but the caller could not persist/announce it.
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
impl core::error::Error for BulkLoadError {}

impl<V: Vfs + Clone> Database<V> {
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
    pub async fn bulk_load_with_checkpoint<I, F>(
        &mut self,
        cx: &QueryCx,
        commit_cx: &CommitCx,
        source: I,
        policy: BulkLoadPolicy,
        crash: Option<(usize, CrashPoint)>,
        mut acknowledged: F,
    ) -> Result<BulkLoadCheckpoint, BulkLoadError>
    where
        I: IntoIterator<Item = BulkRow>,
        I::IntoIter: Clone,
        F: FnMut(&BulkLoadCheckpoint) -> Result<(), std::io::Error>,
    {
        let mut committed = policy.resume.clone().unwrap_or_default();
        let mut source = source.into_iter();
        let preflight = (|| {
            self.ensure_writable()
                .map_err(|e| BulkLoadErrorKind::Write(e.into()))?;
            if policy.rows_per_chunk == 0 {
                return Err(BulkLoadErrorKind::InvalidPolicy);
            }
            let frontier = self
                .frontier()
                .map_err(|e| BulkLoadErrorKind::Write(e.into()))?;
            if policy.resume.is_some() && committed.frontier != frontier {
                return Err(BulkLoadErrorKind::InvalidResume);
            }
            if policy.resume.is_none() {
                committed.frontier = frontier;
            }
            let mut keys = BTreeSet::new();
            let mut vertices = BTreeSet::new();
            let mut prefix_vertices = BTreeSet::new();
            let mut prefix_edges = BTreeSet::new();
            let mut count = 0;
            let mut seals = Vec::new();
            let mut transcript = source::ChunkHasher::new(0, &policy);
            for (index, row) in source.clone().enumerate() {
                cx.checkpoint()
                    .map_err(|e| BulkLoadErrorKind::Write(WriteTxnError::Interrupted(e)))?;
                count = index.checked_add(1).ok_or(BulkLoadErrorKind::CounterOverflow)?;
                source::admit_row(&row)?;
                transcript.push(cx, &row)?;
                if count.is_multiple_of(policy.rows_per_chunk) {
                    seals.push(transcript.finish());
                    transcript = source::ChunkHasher::new(count, &policy);
                }
                let key = match &row {
                    BulkRow::Vertex(v) => &v.key,
                    BulkRow::Edge(e) => &e.key,
                };
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
                            if !vertices.contains(endpoint) {
                                return Err(BulkLoadErrorKind::DanglingEndpointKey {
                                    edge: e.key.clone(),
                                    endpoint: endpoint.clone(),
                                });
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
                            if Some(&row.entry.src) != committed.vertices.get(&e.source)
                                || Some(&row.entry.dst) != committed.vertices.get(&e.destination)
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
                seals.push(transcript.finish());
            }
            Ok(seals)
        })();
        let seals = match preflight {
            Ok(seals) => seals,
            Err(kind) => return Err(BulkLoadError { kind, committed, pending: None }),
        };
        if seals.is_empty() {
            if let Err(kind) = source::expect_end(cx, &mut source, 0) {
                return Err(BulkLoadError { kind, committed, pending: None });
            }
        }
        for (index, seal) in seals.iter().enumerate() {
            let skip = seal.end() <= committed.next_row;
            let chunk = match source::read_verified(
                cx, &mut source, seal, &policy, !skip, index + 1 == seals.len(),
            ) {
                Ok(chunk) => chunk,
                Err(kind) => return Err(BulkLoadError { kind, committed, pending: None }),
            };
            if skip { continue; }
            let Some(next_chunks) = committed.committed_chunks.checked_add(1) else {
                return Err(BulkLoadError {
                    kind: BulkLoadErrorKind::CounterOverflow, committed, pending: None,
                });
            };
            let mut new_vertices = BTreeMap::new();
            let mut new_edges = BTreeMap::new();
            let prepared = (|| {
                let mut vertex_batch = WriteBatch::new(policy.vertex_relation);
                let mut edge_batches: BTreeMap<RelationId, WriteBatch> = BTreeMap::new();
                for row in chunk {
                    source::checkpoint(cx)?;
                    match row {
                        BulkRow::Vertex(v) => {
                            let ElementId::Vertex(id) = self.allocate_identity(
                                cx,
                                GraphInsertRequest::Vertex { row: 0, vertex: 0 },
                            ).map_err(BulkLoadErrorKind::Write)?
                            else {
                                unreachable!("typed allocator")
                            };
                            vertex_batch.create_vertex(id, v.labels, v.props);
                            new_vertices.insert(v.key, id);
                        }
                        BulkRow::Edge(e) => {
                            let src = *new_vertices
                                .get(&e.source)
                                .or_else(|| committed.vertices.get(&e.source))
                                .ok_or_else(|| BulkLoadErrorKind::DanglingEndpointKey {
                                    edge: e.key.clone(), endpoint: e.source.clone(),
                                })?;
                            let dst = *new_vertices
                                .get(&e.destination)
                                .or_else(|| committed.vertices.get(&e.destination))
                                .ok_or_else(|| BulkLoadErrorKind::DanglingEndpointKey {
                                    edge: e.key.clone(), endpoint: e.destination.clone(),
                                })?;
                            let ElementId::Edge(id) = self.allocate_identity(
                                cx,
                                GraphInsertRequest::Edge { row: 0, edge: 0 },
                            ).map_err(BulkLoadErrorKind::Write)?
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
                self.prepare_atomic_writes(batches).map_err(BulkLoadErrorKind::Write)
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
