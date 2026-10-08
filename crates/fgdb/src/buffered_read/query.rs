//! Native GLA pulls over the authenticated buffered vertex merge. The source
//! supplies rows and byte admission; the existing GQL operator owns predicates,
//! projections, ordering, pagination, and cumulative logical budgets.

mod edge;

use super::{BufferedReadError, BufferedReadView, BufferedValue, MemoryPool};
use crate::VertexRow;
use asupersync::fs::Vfs;
use fgdb_gql::algebra::{
    GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphValue, GraphValueRow, PreparedGraphPattern,
};
use fgdb_gql::stream::{
    AsyncVertexCandidate, AsyncVertexScanCursor, AsyncVertexScanEvent, AsyncVertexScanPlan,
    AsyncVertexScanRecord, AsyncVertexScanSource, VertexScanError, VertexScanEvent,
    VertexScanOutput, VertexScanRow, VertexScanSourceError, VertexScanState,
};
use fgdb_gql::{GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryPolicy};
use fgdb_strata::store::{BufferedScanError, BufferedScanEvent, BufferedVertexScan};
use fgdb_strata::tiered::memory::MemoryCharge;
use fgdb_types::{CommitSeq, QueryCx, VId};
use std::cell::RefCell;

type Cancel = Box<asupersync::error::Error>;
type Checkpoint<'q> = Box<dyn FnMut() -> Result<(), Cancel> + Send + 'q>;

/// A native query refusal preserves storage, row-budget, evaluator-budget,
/// arithmetic, and cancellation failures as separate variants.
pub type BufferedQueryError = GqlQueryError<VertexScanError<BufferedReadError>, Cancel>;

/// A projected result owns its resident reservation independently of its
/// cursor and source view. Borrowing the row does not release its charge.
/// Explicit copies made by the application belong to the application's budget.
pub struct BufferedQueryRow<Row = GraphValueRow> {
    row: Row,
    // Declaration order releases the data before refunding its reservation.
    _charge: MemoryCharge,
}

impl<Row> AsRef<Row> for BufferedQueryRow<Row> {
    fn as_ref(&self) -> &Row {
        &self.row
    }
}

impl<Row> core::ops::Deref for BufferedQueryRow<Row> {
    type Target = Row;
    fn deref(&self) -> &Row {
        &self.row
    }
}

impl<Row> core::fmt::Debug for BufferedQueryRow<Row> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("BufferedQueryRow([REDACTED])")
    }
}

/// Pulls a fixed historical cut without building a resident snapshot or result
/// table. `next().await` supplies demand; close, error, and a dropped in-flight
/// pull stop the cursor. Earlier rows remain delivered if a later pull fails,
/// so successful query completion requires reaching Exhausted.
///
/// The cursor borrows the buffered view and QueryCx. Returned rows may outlive
/// both. Source decoding, merge heads, evaluator temporaries, and output copies
/// use the view's MemoryPool. Native preparation and caller-owned catalog or
/// parameter objects remain outside that pool, as does Chronicle recovery.
pub struct BufferedQueryCursor<'view, 'q, V: Vfs + Clone, Row: VertexScanOutput = GraphValueRow> {
    inner: AsyncVertexScanCursor<BufferedVertexQuerySource<'view, 'q, V>, Checkpoint<'q>, Row>,
    cx: &'q QueryCx,
    _metadata: MemoryCharge,
}

impl<V: Vfs + Clone, Row: VertexScanOutput> core::fmt::Debug
    for BufferedQueryCursor<'_, '_, V, Row>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("BufferedQueryCursor([REDACTED])")
    }
}

impl<V: Vfs + Clone, Row: VertexScanOutput> BufferedQueryCursor<'_, '_, V, Row> {
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

    pub fn close(&mut self) {
        self.inner.close();
    }

    pub async fn next(&mut self) -> Option<Result<BufferedQueryRow<Row>, BufferedQueryError>> {
        self.cx
            .with_restriction_async(self.inner.next())
            .await
            .map(|result| {
                result.map(|output| {
                    let (row, charge) = output.into_parts();
                    BufferedQueryRow {
                        row,
                        _charge: charge,
                    }
                })
            })
    }
}

struct BufferedQueryRecord {
    row: BufferedValue<VertexRow>,
    // Temporary evaluator allocations live no longer than this candidate.
    scratch: RefCell<MemoryCharge>,
}

impl AsyncVertexScanRecord for BufferedQueryRecord {
    fn as_row(&self) -> VertexScanRow<'_> {
        VertexScanRow {
            labels: &self.row.labels,
            properties: &self.row.props,
        }
    }
}

struct BufferedVertexQuerySource<'view, 'q, V: Vfs> {
    scan: BufferedVertexScan<'view, V>,
    cx: &'q QueryCx,
    pool: MemoryPool,
}

impl<V: Vfs + Clone> AsyncVertexScanSource for BufferedVertexQuerySource<'_, '_, V> {
    type Error = BufferedReadError;
    type Record = BufferedQueryRecord;
    type OutputGuard = MemoryCharge;

    fn snapshot_seq(&self) -> CommitSeq {
        self.scan.snapshot_seq()
    }

    async fn next_candidate<C: Send>(
        &mut self,
        control: &mut (impl FnMut(AsyncVertexScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<AsyncVertexCandidate<Self::Record>>, VertexScanSourceError<Self::Error, C>>
    {
        let candidate = self
            .scan
            .next_candidate_with(self.cx, &mut |event| {
                control(match event {
                    BufferedScanEvent::Work => AsyncVertexScanEvent::Work,
                    BufferedScanEvent::Identity(vid) => AsyncVertexScanEvent::Candidate(vid),
                })
            })
            .await
            .map_err(|error| match error {
                BufferedScanError::Read(error) => VertexScanSourceError::Source(error),
                BufferedScanError::Control(error) => VertexScanSourceError::Control(error),
            })?;
        candidate
            .map(|candidate| {
                let record = candidate
                    .row
                    .map(|row| {
                        let scratch = self
                            .pool
                            .reserve(self.cx, 0)
                            .map_err(BufferedReadError::Memory)
                            .map_err(VertexScanSourceError::Source)?;
                        Ok::<_, VertexScanSourceError<Self::Error, C>>(BufferedQueryRecord {
                            row,
                            scratch: RefCell::new(scratch),
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
            // Native controls count payload quanta and fixed value cells.
            // Cover both, including transient vector growth, before allocating.
            // The charge is refunded when this candidate finishes evaluation.
            let bytes = 8 * core::mem::size_of::<GraphValue>().max(GRAPH_VALUE_PAYLOAD_UNIT_BYTES);
            record.scratch.borrow_mut().grow(self.cx, bytes)?;
        }
        Ok(())
    }

    fn reserve_output<C>(
        &self,
        record: &Self::Record,
        columns: usize,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Self::OutputGuard, VertexScanSourceError<Self::Error, C>> {
        // The sealed profile can copy only identity cells and properties from
        // this one row. A conservative row-local bound permits sources larger
        // than RAM; it never reserves max_source_bytes for an individual row.
        let mut bytes = 1024usize;
        for (_, value) in &record.row.props {
            control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
            let payload = value
                .canonical_encoded_len()
                .map_err(|_| VertexScanSourceError::Source(BufferedReadError::SizeOverflow))?;
            bytes = payload
                .checked_mul(4)
                .and_then(|payload| payload.checked_add(256))
                .and_then(|payload| bytes.checked_add(payload))
                .ok_or(VertexScanSourceError::Source(
                    BufferedReadError::SizeOverflow,
                ))?;
        }
        let bytes = bytes
            .checked_mul(columns.max(1))
            .ok_or(VertexScanSourceError::Source(
                BufferedReadError::SizeOverflow,
            ))?;
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        self.pool
            .reserve(self.cx, bytes)
            .map_err(BufferedReadError::Memory)
            .map_err(VertexScanSourceError::Source)
    }
}

fn source_error(error: BufferedReadError) -> BufferedQueryError {
    GqlQueryError::Source(VertexScanError::Source(error))
}

impl<V: Vfs + Clone> BufferedReadView<V> {
    fn open_query<'view, 'q, Row: VertexScanOutput>(
        &'view mut self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<Row>,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<BufferedQueryCursor<'view, 'q, V, Row>, BufferedQueryError> {
        if as_of > self.frontier() {
            return Err(source_error(BufferedReadError::BeyondPublication {
                requested: as_of,
                publication: self.frontier(),
            }));
        }
        cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
        let plan = AsyncVertexScanPlan::compile(pattern.plan())
            .map_err(|error| GqlQueryError::Source(VertexScanError::Plan(error)))?;
        let pool = self.memory_pool().clone();
        let metadata = pool
            .reserve(cx, 1024)
            .map_err(BufferedReadError::Memory)
            .map_err(source_error)?;
        let source = BufferedVertexQuerySource {
            scan: self
                .partition
                .vertex_scan(cx, as_of)
                .map_err(source_error)?,
            cx,
            pool,
        };
        let checkpoint: Checkpoint<'q> = Box::new(move || cx.checkpoint());
        Ok(BufferedQueryCursor {
            inner: AsyncVertexScanCursor::new(source, plan, policy, checkpoint),
            cx,
            _metadata: metadata,
        })
    }

    /// Execute an already bound native GQL plan through the extent cache.
    /// The projection begins with the scanned vertex identity, followed by
    /// identities or canonical properties of that same vertex. Labels, local
    /// WHERE expressions, DISTINCT/ALL, and SKIP/LIMIT reuse native semantics.
    /// Joins, edge expansion, probes, aggregates, property-only projections,
    /// and alternate orderings refuse before any source candidate is read.
    /// This owner-level API does not grant or apply Warden session authority.
    pub fn stream_graph_values_governed<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<BufferedQueryCursor<'view, 'q, V>, BufferedQueryError> {
        let as_of = self.frontier();
        self.open_query(cx, pattern, as_of, policy)
    }

    /// The same query over a fixed historical cut of the admitted generation.
    pub fn stream_graph_values_governed_at<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<BufferedQueryCursor<'view, 'q, V>, BufferedQueryError> {
        self.open_query(cx, pattern, as_of, policy)
    }

    /// Identity-only counterpart of the same native GLA operator.
    pub fn stream_graph_vertices_governed<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<VId>,
        policy: GqlQueryPolicy,
    ) -> Result<BufferedQueryCursor<'view, 'q, V, VId>, BufferedQueryError> {
        let as_of = self.frontier();
        self.open_query(cx, pattern, as_of, policy)
    }

    pub fn stream_graph_vertices_governed_at<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<VId>,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<BufferedQueryCursor<'view, 'q, V, VId>, BufferedQueryError> {
        self.open_query(cx, pattern, as_of, policy)
    }
}
