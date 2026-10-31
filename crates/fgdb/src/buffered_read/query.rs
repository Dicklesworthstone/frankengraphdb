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
use fgdb_gql::spill_aggregate::{AsyncVertexSpillAggregateCursor, AsyncVertexSpillAggregatePlan};
use fgdb_gql::stream::{
    AsyncVertexCandidate, AsyncVertexScanCursor, AsyncVertexScanEvent, AsyncVertexScanPlan,
    AsyncVertexScanRecord, AsyncVertexScanSource, VertexScanError, VertexScanEvent,
    VertexScanOutput, VertexScanRow, VertexScanSourceError, VertexScanState,
};
use fgdb_gql::{GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryPolicy};
use fgdb_strata::store::{BufferedScanError, BufferedScanEvent, BufferedVertexScan};
use fgdb_strata::tiered::edge_scan::BufferedEdgeJoinScan;
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
    charge: MemoryCharge,
}

impl<Row> AsRef<Row> for BufferedQueryRow<Row> {
    fn as_ref(&self) -> &Row {
        &self.row
    }
}

impl<Row> BufferedQueryRow<Row> {
    pub(crate) fn into_parts(self) -> (Row, MemoryCharge) {
        (self.row, self.charge)
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
                    BufferedQueryRow { row, charge }
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

/// Owned selected roots. Empty and singleton inputs need no seed allocation;
/// a larger admitted batch drops its capacity before resolving its last root.
/// Declaration order releases the iterator before refunding its reservations.
pub(crate) enum BufferedVertexSeeds {
    Empty,
    One(VId),
    Many {
        remaining: std::vec::IntoIter<VId>,
        _charge: MemoryCharge,
        _extra: Option<MemoryCharge>,
    },
}

impl BufferedVertexSeeds {
    fn next(&mut self) -> Option<VId> {
        match self {
            Self::Empty => None,
            Self::One(vid) => {
                let vid = *vid;
                *self = Self::Empty;
                Some(vid)
            }
            Self::Many { remaining, .. } => {
                let vid = remaining.next();
                if remaining.len() == 0 {
                    *self = Self::Empty;
                }
                vid
            }
        }
    }
}

#[cfg(test)]
#[test]
fn seed_capacity_is_available_before_the_last_point_read_in_a_full_pool() {
    let ((), report) = asupersync::lab::run_async_under_lab(0xe6_73, |root| async move {
        let contexts = fgdb_types::PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let bytes = 3 * core::mem::size_of::<VId>();
        let pool = MemoryPool::new(bytes, 0).unwrap();
        let charge = pool.reserve(&cx, bytes).unwrap();
        let values: Box<[VId]> = Box::new([VId(0), VId(1_u128 << 100), VId(u128::MAX)]);
        let mut seeds = BufferedVertexSeeds::Many {
            remaining: values.into_vec().into_iter(),
            _charge: charge,
            _extra: None,
        };
        assert_eq!(pool.available(), 0);
        // A singleton body needs no seed capacity, even while another owner
        // holds every available byte. Full-width IDs survive the inline path.
        let mut one = BufferedVertexSeeds::One(VId(u128::MAX));
        assert_eq!(one.next(), Some(VId(u128::MAX)));
        assert_eq!(one.next(), None);
        assert_eq!(pool.used(), bytes);
        assert_eq!(seeds.next(), Some(VId(0)));
        assert_eq!(seeds.next(), Some(VId(1_u128 << 100)));
        assert!(
            pool.reserve(&cx, 1).is_err(),
            "unread seeds retain admission"
        );
        assert_eq!(seeds.next(), Some(VId(u128::MAX)));
        // next_candidate calls point I/O only AFTER this selection returns.
        // Its complete workspace can reuse all seed capacity immediately.
        let point_workspace = pool.reserve(&cx, bytes).unwrap();
        assert_eq!(seeds.next(), None);
        drop(point_workspace);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

enum BufferedVertexInput<'view, V: Vfs> {
    Scan(BufferedVertexScan<'view, V>),
    Seeds {
        scan: BufferedEdgeJoinScan<'view, V>,
        seeds: BufferedVertexSeeds,
    },
}

struct BufferedVertexQuerySource<'view, 'q, V: Vfs> {
    input: BufferedVertexInput<'view, V>,
    cx: &'q QueryCx,
    pool: MemoryPool,
}

impl<V: Vfs + Clone> AsyncVertexScanSource for BufferedVertexQuerySource<'_, '_, V> {
    type Error = BufferedReadError;
    type Record = BufferedQueryRecord;
    type OutputGuard = MemoryCharge;

    fn snapshot_seq(&self) -> CommitSeq {
        match &self.input {
            BufferedVertexInput::Scan(scan) => scan.snapshot_seq(),
            BufferedVertexInput::Seeds { scan, .. } => scan.snapshot_seq(),
        }
    }

    async fn next_candidate<C: Send>(
        &mut self,
        control: &mut (impl FnMut(AsyncVertexScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<AsyncVertexCandidate<Self::Record>>, VertexScanSourceError<Self::Error, C>>
    {
        let mut observe = |event| {
            control(match event {
                BufferedScanEvent::Work => AsyncVertexScanEvent::Work,
                BufferedScanEvent::Identity(vid) => AsyncVertexScanEvent::Candidate(vid),
            })
        };
        let candidate = match &mut self.input {
            BufferedVertexInput::Scan(scan) => {
                scan.next_candidate_with(self.cx, &mut observe).await
            }
            BufferedVertexInput::Seeds { scan, seeds } => {
                let Some(vid) = seeds.next() else {
                    return Ok(None);
                };
                scan.vertex_candidate_with(self.cx, vid, &mut observe).await
            }
        }
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
        self.open_vertex_input(cx, plan, as_of, policy)
    }

    /// Open an already admitted local physical input. Its terminal clauses
    /// may belong to an external operator; the native cursor still owns all
    /// source predicates, projection and cumulative evaluation accounting.
    pub(crate) fn open_vertex_input<'view, 'q, Row: VertexScanOutput>(
        &'view mut self,
        cx: &'q QueryCx,
        plan: AsyncVertexScanPlan<Row>,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<BufferedQueryCursor<'view, 'q, V, Row>, BufferedQueryError> {
        let (source, metadata) = self.vertex_input_source(cx, as_of)?;
        let checkpoint: Checkpoint<'q> = Box::new(move || cx.with_restriction(|| cx.checkpoint()));
        Ok(BufferedQueryCursor {
            inner: AsyncVertexScanCursor::new(source, plan, policy, checkpoint),
            cx,
            _metadata: metadata,
        })
    }

    /// The selected-product compiler has proved these sorted distinct roots
    /// contain every possible matching pair. The cursor owns their byte
    /// reservation through the last root selection. Only admission changes:
    /// native predicates, fields, source tail and counters still run in full.
    pub(crate) fn open_seeded_vertex_input<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        plan: AsyncVertexScanPlan<GraphValueRow>,
        seeds: BufferedVertexSeeds,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<BufferedQueryCursor<'view, 'q, V>, BufferedQueryError> {
        if as_of > self.frontier() {
            return Err(source_error(BufferedReadError::BeyondPublication {
                requested: as_of,
                publication: self.frontier(),
            }));
        }
        cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
        let pool = self.memory_pool().clone();
        let metadata = pool
            .reserve(cx, 1024)
            .map_err(BufferedReadError::Memory)
            .map_err(source_error)?;
        let scan = self
            .partition
            .edge_scan(cx, as_of)
            .map_err(source_error)?
            .into_join_scan();
        let source = BufferedVertexQuerySource {
            input: BufferedVertexInput::Seeds { scan, seeds },
            cx,
            pool,
        };
        let checkpoint: Checkpoint<'q> = Box::new(move || cx.with_restriction(|| cx.checkpoint()));
        Ok(BufferedQueryCursor {
            inner: AsyncVertexScanCursor::new(source, plan, policy, checkpoint),
            cx,
            _metadata: metadata,
        })
    }

    pub(crate) fn open_vertex_aggregate_input<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        plan: AsyncVertexSpillAggregatePlan,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        AsyncVertexSpillAggregateCursor<
            impl AsyncVertexScanSource<Error = BufferedReadError, OutputGuard = MemoryCharge>
            + use<'view, 'q, V>,
            impl FnMut() -> Result<(), Cancel> + Send + use<'view, 'q, V>,
        >,
        BufferedQueryError,
    > {
        let (source, metadata) = self.vertex_input_source(cx, as_of)?;
        let checkpoint: Checkpoint<'q> = Box::new(move || {
            let _retained = &metadata;
            cx.with_restriction(|| cx.checkpoint())
        });
        Ok(AsyncVertexSpillAggregateCursor::new(
            source, plan, policy, checkpoint,
        ))
    }

    fn vertex_input_source<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        as_of: CommitSeq,
    ) -> Result<(BufferedVertexQuerySource<'view, 'q, V>, MemoryCharge), BufferedQueryError> {
        if as_of > self.frontier() {
            return Err(source_error(BufferedReadError::BeyondPublication {
                requested: as_of,
                publication: self.frontier(),
            }));
        }
        cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
        let pool = self.memory_pool().clone();
        let metadata = pool
            .reserve(cx, 1024)
            .map_err(BufferedReadError::Memory)
            .map_err(source_error)?;
        let source = BufferedVertexQuerySource {
            input: BufferedVertexInput::Scan(
                self.partition
                    .vertex_scan(cx, as_of)
                    .map_err(source_error)?,
            ),
            cx,
            pool,
        };
        Ok((source, metadata))
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
