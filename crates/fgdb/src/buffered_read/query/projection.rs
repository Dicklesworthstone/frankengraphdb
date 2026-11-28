//! Host ownership for computed vertex rows over the buffered native source.
//! Preparation, property demand, evaluation and pagination belong to GQL;
//! this host only retains the source and output reservations across each pull.

use super::*;
use fgdb_gql::PreparedGraphSet;
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

impl core::fmt::Debug for BufferedProjectionOutputGuard {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("BufferedProjectionOutputGuard([REDACTED])")
    }
}

/// One computed row whose reservation lasts through the application's last use.
pub type BufferedProjectionRow = BufferedQueryRow<GraphValueRow, BufferedProjectionOutputGuard>;

/// A fixed-cut computed vertex stream with the same bounded property source
/// and cumulative native meters as ordinary buffered reads. Every matching
/// occurrence is evaluated before final pagination, including the suffix after
/// LIMIT and a LIMIT 0 query. Successful completion requires Exhausted.
///
/// Returned rows may outlive this cursor and the view. Their guards retain all
/// VM reservations. Native preparation and caller-owned parameter/catalog
/// objects remain outside the view's pool, as for BufferedQueryCursor.
pub struct BufferedProjectionCursor<'view, 'q, V: Vfs + Clone> {
    inner: AsyncVertexProjectionCursor<ProjectionSource<'view, 'q, V>, Checkpoint<'q>>,
    cx: &'q QueryCx,
    _metadata: MemoryCharge,
}

impl<V: Vfs + Clone> core::fmt::Debug for BufferedProjectionCursor<'_, '_, V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("BufferedProjectionCursor([REDACTED])")
    }
}

impl<V: Vfs + Clone> BufferedProjectionCursor<'_, '_, V> {
    pub fn snapshot_seq(&self) -> CommitSeq {
        self.inner.snapshot_seq()
    }

    pub fn state(&self) -> VertexScanState {
        self.inner.state()
    }

    pub fn row_stats(&self) -> GqlExecutionStats {
        self.inner.row_stats()
    }

    pub fn evaluator_stats(&self) -> GlaExecutionStats {
        self.inner.evaluator_stats()
    }

    /// Release the unread suffix without claiming successful completion.
    pub fn close(&mut self) {
        self.inner.close();
    }

    pub async fn next(&mut self) -> Option<Result<BufferedProjectionRow, BufferedProjectionError>> {
        self.cx
            .with_restriction_async(self.inner.next())
            .erase_send()
            .await
            .map(|result| result.map(|output| BufferedQueryRow::from_parts(output.into_parts())))
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
                        let mut scratch = record.scratch.into_inner();
                        // Admit the shared owner before allocating it. The two
                        // reference counts plus alignment slack are bounded.
                        let owner_bytes = size_of::<Mutex<MemoryCharge>>() + 4 * size_of::<usize>();
                        scratch
                            .grow(self.inner.cx, owner_bytes)
                            .map_err(BufferedReadError::Memory)
                            .map_err(VertexScanSourceError::Source)?;
                        Ok::<_, VertexScanSourceError<Self::Error, C>>(ProjectionRecord {
                            fields: record.fields,
                            scratch: Arc::new(Mutex::new(scratch)),
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
            let bytes = 8 * size_of::<GraphValue>().max(GRAPH_VALUE_PAYLOAD_UNIT_BYTES);
            record
                .scratch
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .grow(self.inner.cx, bytes)?;
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
        Ok(BufferedProjectionOutputGuard {
            _copy: copy,
            _evaluation: Arc::clone(&record.scratch),
        })
    }
}

impl<V: Vfs + Clone> BufferedReadView<V> {
    /// Stream a fully bound row-local vertex projection through the extent
    /// cache. The compiled source must already have increasing unique vertex
    /// identity order; the final projection may compute scalar/list/map values,
    /// hide its identity ranking key, and apply a final SKIP/LIMIT.
    ///
    /// Every property receiver must be the current root, with a bounded key
    /// demand proved before opening storage. DISTINCT, alternate ordering,
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
        let plan = AsyncVertexProjectionPlan::compile(query).map_err(|error| {
            GqlQueryError::Source(ScanError::Vertex(VertexScanError::Plan(error)))
        })?;
        let (source, metadata) = self
            .vertex_input_source(cx, at, plan.property_demand())
            .map_err(|error| error.map_source(ScanError::Vertex))?;
        let checkpoint: Checkpoint<'q> = Box::new(move || cx.with_restriction(|| cx.checkpoint()));
        Ok(BufferedProjectionCursor {
            inner: AsyncVertexProjectionCursor::new(
                ProjectionSource { inner: source },
                plan,
                policy,
                checkpoint,
            ),
            cx,
            _metadata: metadata,
        })
    }
}
