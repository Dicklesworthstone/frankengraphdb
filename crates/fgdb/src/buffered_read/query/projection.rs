//! Host ownership for computed rows over the buffered native sources.
//! Preparation, property demand, evaluation and pagination belong to GQL;
//! this host only retains the source and output reservations across each pull.

use super::*;
use fgdb_gql::PreparedGraphSet;
use fgdb_gql::edge_stream::{AsyncEdgeProjectionPlan, EdgeScanState};
use fgdb_gql::scan_stream::ScanError;
use fgdb_gql::stream::{AsyncVertexProjectionCursor, AsyncVertexProjectionPlan};
use std::sync::{Arc, Mutex};

/// Computed rows preserve native source, projection, resource and cancellation
/// failures. Projection errors carry the original source row and column.
pub type BufferedProjectionError = GqlQueryError<ScanError<BufferedReadError>, Cancel>;

/// Opaque accounting for both the native input copy and the VM's owned values.
/// The reservation grows before the VM allocates and survives its source record.
pub struct BufferedProjectionOutputGuard {
    _copy: MemoryCharge,
    _evaluation: Arc<Mutex<MemoryCharge>>,
}

impl BufferedProjectionOutputGuard {
    pub(super) fn new(copy: MemoryCharge, evaluation: Arc<Mutex<MemoryCharge>>) -> Self {
        Self {
            _copy: copy,
            _evaluation: evaluation,
        }
    }
}

pub(super) fn retain_evaluation(
    cx: &QueryCx,
    mut charge: MemoryCharge,
) -> Result<Arc<Mutex<MemoryCharge>>, BufferedReadError> {
    // Admit the shared owner before allocating it. The two reference counts
    // plus alignment slack are bounded. Existing image charges stay attached.
    charge.grow(
        cx,
        size_of::<Mutex<MemoryCharge>>() + 4 * size_of::<usize>(),
    )?;
    Ok(Arc::new(Mutex::new(charge)))
}

pub(super) fn reserve_evaluation(
    cx: &QueryCx,
    charge: &Arc<Mutex<MemoryCharge>>,
) -> Result<(), BufferedReadError> {
    let bytes = 8 * size_of::<GraphValue>().max(GRAPH_VALUE_PAYLOAD_UNIT_BYTES);
    charge
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .grow(cx, bytes)?;
    Ok(())
}

impl core::fmt::Debug for BufferedProjectionOutputGuard {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("BufferedProjectionOutputGuard([REDACTED])")
    }
}

/// One computed row whose reservation lasts through the application's last use.
pub type BufferedProjectionRow = BufferedQueryRow<GraphValueRow, BufferedProjectionOutputGuard>;

/// A fixed-cut computed vertex or single-edge stream with the same bounded property source
/// and cumulative native meters as ordinary buffered reads. Every matching
/// occurrence is evaluated before final pagination, including the suffix after
/// LIMIT and a LIMIT 0 query. Successful completion requires Exhausted.
///
/// Returned rows may outlive this cursor and the view. Their guards retain all
/// VM reservations. Native preparation and caller-owned parameter/catalog
/// objects remain outside the view's pool, as for BufferedQueryCursor.
pub struct BufferedProjectionCursor<'view, 'q, V: Vfs + Clone> {
    inner: NativeProjection<'view, 'q, V>,
    cx: &'q QueryCx,
}

enum NativeProjection<'view, 'q, V: Vfs + Clone> {
    Vertex {
        cursor: AsyncVertexProjectionCursor<ProjectionSource<'view, 'q, V>, Checkpoint<'q>>,
        _metadata: MemoryCharge,
    },
    Edge(super::edge::projection::Cursor<'view, 'q, V>),
}

impl<V: Vfs + Clone> core::fmt::Debug for BufferedProjectionCursor<'_, '_, V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("BufferedProjectionCursor([REDACTED])")
    }
}

impl<V: Vfs + Clone> BufferedProjectionCursor<'_, '_, V> {
    pub fn snapshot_seq(&self) -> CommitSeq {
        match &self.inner {
            NativeProjection::Vertex { cursor, .. } => cursor.snapshot_seq(),
            NativeProjection::Edge(cursor) => cursor.snapshot_seq(),
        }
    }

    pub fn state(&self) -> VertexScanState {
        match &self.inner {
            NativeProjection::Vertex { cursor, .. } => cursor.state(),
            NativeProjection::Edge(cursor) => match cursor.state() {
                EdgeScanState::Open => VertexScanState::Open,
                EdgeScanState::Exhausted => VertexScanState::Exhausted,
                EdgeScanState::Closed => VertexScanState::Closed,
                EdgeScanState::Failed => VertexScanState::Failed,
            },
        }
    }

    pub fn row_stats(&self) -> GqlExecutionStats {
        match &self.inner {
            NativeProjection::Vertex { cursor, .. } => cursor.row_stats(),
            NativeProjection::Edge(cursor) => cursor.row_stats(),
        }
    }

    pub fn evaluator_stats(&self) -> GlaExecutionStats {
        match &self.inner {
            NativeProjection::Vertex { cursor, .. } => cursor.evaluator_stats(),
            NativeProjection::Edge(cursor) => cursor.evaluator_stats(),
        }
    }

    /// Release the unread suffix without claiming successful completion.
    pub fn close(&mut self) {
        match &mut self.inner {
            NativeProjection::Vertex { cursor, .. } => cursor.close(),
            NativeProjection::Edge(cursor) => cursor.close(),
        }
    }

    pub async fn next(&mut self) -> Option<Result<BufferedProjectionRow, BufferedProjectionError>> {
        match &mut self.inner {
            NativeProjection::Vertex { cursor, .. } => self
                .cx
                .with_restriction_async(cursor.next())
                .erase_send()
                .await
                .map(|result| {
                    result.map(|output| BufferedQueryRow::from_parts(output.into_parts()))
                }),
            NativeProjection::Edge(cursor) => {
                self.cx
                    .with_restriction_async(cursor.next())
                    .erase_send()
                    .await
            }
        }
    }
}

struct ProjectionRecord {
    fields: BufferedVertexFields,
    scratch: Arc<Mutex<MemoryCharge>>,
}

impl AsyncVertexScanRecord for ProjectionRecord {
    fn as_row(&self) -> VertexScanRow<'_> {
        self.fields.as_row()
    }
}

/// Reuses the ordinary cold decoder and copy admission. The only difference is
/// the VM reservation's lifetime: both the current record and its returned row
/// own it, so dropping either one alone cannot refund live computed payloads.
struct ProjectionSource<'view, 'q, V: Vfs> {
    inner: BufferedVertexQuerySource<'view, 'q, V>,
}

impl<V: Vfs + Clone> AsyncVertexScanSource for ProjectionSource<'_, '_, V> {
    type Error = BufferedReadError;
    type Record = ProjectionRecord;
    type OutputGuard = BufferedProjectionOutputGuard;

    fn snapshot_seq(&self) -> CommitSeq {
        self.inner.snapshot_seq()
    }

    async fn next_candidate<C: Send>(
        &mut self,
        control: &mut (impl FnMut(AsyncVertexScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<AsyncVertexCandidate<Self::Record>>, VertexScanSourceError<Self::Error, C>>
    {
        self.inner
            .next_candidate(control)
            .await?
            .map(|candidate| {
                let record = candidate
                    .record
                    .map(|record| {
                        let scratch = retain_evaluation(self.inner.cx, record.scratch.into_inner())
                            .map_err(VertexScanSourceError::Source)?;
                        Ok::<_, VertexScanSourceError<Self::Error, C>>(ProjectionRecord {
                            fields: record.fields,
                            scratch,
                        })
                    })
                    .transpose()?;
                Ok(AsyncVertexCandidate {
                    vid: candidate.vid,
                    record,
                })
            })
            .transpose()
    }

    fn evaluation_event(
        &self,
        record: &Self::Record,
        event: VertexScanEvent,
    ) -> Result<(), Self::Error> {
        if event == VertexScanEvent::ScratchEntry {
            // Same conservative native payload/cell quantum as the ordinary
            // buffered source; the final output keeps this growing charge.
            reserve_evaluation(self.inner.cx, &record.scratch)?;
        }
        Ok(())
    }

    fn reserve_output<C>(
        &self,
        record: &Self::Record,
        columns: usize,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Self::OutputGuard, VertexScanSourceError<Self::Error, C>> {
        let copy = self
            .inner
            .reserve_row_output(record.as_row(), columns, control)?;
        Ok(BufferedProjectionOutputGuard::new(
            copy,
            Arc::clone(&record.scratch),
        ))
    }
}

impl<V: Vfs + Clone> BufferedReadView<V> {
    /// Stream a fully bound row-local vertex or single-edge projection through
    /// the extent cache. The compiled source must already have increasing
    /// unique identity order (including the source VId for undirected edges);
    /// the final projection may compute scalar/list/map values,
    /// hide its identity ranking key, and apply a final SKIP/LIMIT.
    ///
    /// Every property receiver must be the current vertex, edge or an endpoint,
    /// with bounded key demands proved before opening storage. Both undirected
    /// orientations share one admitted record; self-loops produce one occurrence.
    /// DISTINCT, alternate ordering,
    /// property-first source order, graph reads and multi-stage relational
    /// queries refuse before any candidate is read. This owner-level API does
    /// not grant or apply Warden session authority.
    pub fn stream_graph_projection_governed<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        query: &PreparedGraphSet,
        policy: GqlQueryPolicy,
    ) -> Result<BufferedProjectionCursor<'view, 'q, V>, BufferedProjectionError> {
        let at = self.frontier();
        self.stream_graph_projection_governed_at(cx, query, at, policy)
    }

    /// Execute the same proved projection at an exact committed historical cut.
    /// A cut beyond this view's publication refuses; no current-head fallback
    /// or resident graph materialization is used.
    pub fn stream_graph_projection_governed_at<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        query: &PreparedGraphSet,
        at: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<BufferedProjectionCursor<'view, 'q, V>, BufferedProjectionError> {
        cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
        let plan = match AsyncVertexProjectionPlan::compile(query) {
            Ok(plan) => plan,
            Err(error) => {
                // Both are complete physical-definition proofs. Selection
                // ends before opening one source; no runtime fallback exists.
                let plan = AsyncEdgeProjectionPlan::compile(query).map_err(|_| {
                    GqlQueryError::Source(ScanError::Vertex(VertexScanError::Plan(error)))
                })?;
                let cursor = self.open_edge_projection(cx, plan, at, policy)?;
                return Ok(BufferedProjectionCursor {
                    inner: NativeProjection::Edge(cursor),
                    cx,
                });
            }
        };
        let (source, metadata) = self
            .vertex_input_source(cx, at, plan.property_demand())
            .map_err(|error| error.map_source(ScanError::Vertex))?;
        let checkpoint: Checkpoint<'q> = Box::new(move || cx.with_restriction(|| cx.checkpoint()));
        Ok(BufferedProjectionCursor {
            inner: NativeProjection::Vertex {
                cursor: AsyncVertexProjectionCursor::new(
                    ProjectionSource { inner: source },
                    plan,
                    policy,
                    checkpoint,
                ),
                _metadata: metadata,
            },
            cx,
        })
    }
}
