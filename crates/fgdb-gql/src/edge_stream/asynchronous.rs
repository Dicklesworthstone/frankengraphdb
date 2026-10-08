//! Awaitable intake for the existing identified-edge operator. Source I/O may
//! suspend; the ordinary EdgeScanPlan still owns predicates, path capture and
//! projection. One admitted edge record is retained across its orientations.

use super::*;
use crate::algebra::EdgeRelation;

/// An owned, source-admitted edge and its endpoint images at one immutable cut.
/// The source must mask relation, endpoint and field visibility BEFORE exposing
/// the record. It may retain buffer reservations here while the evaluator lends
/// its fields. This interface is not an authorization grant or a graph cache.
/// Repeated calls must return the same topology and fields without I/O.
pub trait AsyncEdgeScanRecord {
    fn edge(&self) -> EdgeScanRow<'_>;
    fn vertex(&self, vid: VId) -> Option<VertexScanRow<'_>>;
}

/// One increasing historical identity. An invisible edge still consumes its
/// candidate charge, but has no record and produces no orientation or row.
pub struct AsyncEdgeCandidate<Record> {
    pub eid: EId,
    pub record: Option<Record>,
}

/// What one [`AsyncEdgeScanSource::next_candidate`] resolves to: the next
/// candidate, `None` at EOF, or the source's or a control's refusal.
pub type AsyncEdgeCandidateResult<Record, Error, Control> =
    Result<Option<AsyncEdgeCandidate<Record>>, EdgeScanSourceError<Error, Control>>;

/// Source work and history admission enter the SAME cumulative query meter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsyncEdgeScanEvent {
    Work,
    ScratchEntry,
    Candidate(EId),
}

/// Fixed-generation asynchronous edge access with host-owned memory admission.
///
/// Each successful non-EOF pull emits Candidate(eid) exactly once BEFORE
/// resolving that history or allocating its record, and returns that identity.
/// Candidates are strictly increasing even when invisible. EOF admits nothing.
/// Work/scratch controls precede source operations, including I/O and decoding;
/// refused controls propagate unchanged, never as absent data or EOF.
///
/// `relation` is the compiler's selector, not authority. A scoped source must
/// admit it before opening its index and mask every candidate's actual relation
/// and endpoints. In particular Any cannot read hidden relation descriptors.
/// A visible record contains both visible endpoint images at snapshot_seq();
/// a missing endpoint is structural failure, not a NULL extension.
///
/// reserve_output runs after filters, SKIP and output-quota admission, before
/// projection copies payloads. Admit up to `columns` copies of the largest
/// selected payload plus row/path framing, retaining the guard with the result.
/// evaluation_event admits each row-local allocation before the shared kernel
/// performs it; a byte-accounted host retains this temporary charge in record.
/// Hosts may explicitly use () guards, but then this API establishes no byte
/// bound. Source buffers, caller collection and durable retention remain separate.
pub trait AsyncEdgeScanSource: Send {
    type Error: Send;
    type Record: AsyncEdgeScanRecord + Send;
    type OutputGuard: Send;

    fn snapshot_seq(&self) -> CommitSeq;

    fn next_candidate<C: Send>(
        &mut self,
        relation: EdgeRelation,
        control: &mut (impl FnMut(AsyncEdgeScanEvent) -> Result<(), C> + Send),
    ) -> impl core::future::Future<Output = AsyncEdgeCandidateResult<Self::Record, Self::Error, C>> + Send;

    fn reserve_output<C>(
        &self,
        record: &Self::Record,
        columns: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Self::OutputGuard, EdgeScanSourceError<Self::Error, C>>;

    fn evaluation_event(
        &self,
        record: &Self::Record,
        event: GlaExecutionEvent,
    ) -> Result<(), Self::Error>;
}

/// The existing ordered single-edge profile, with awaitable source access.
/// No join/probe, variable-length traversal, hidden sort key or eager fallback
/// is admitted. Those readers need their own asynchronous access contracts.
/// Edge identity then oriented source identity still prove order/uniqueness;
/// both ALL and DISTINCT reuse that proof without retaining a result bag.
#[derive(Clone)]
pub struct AsyncEdgeScanPlan {
    inner: EdgeScanPlan,
    columns: usize,
}
impl AsyncEdgeScanPlan {
    pub fn compile(plan: &GlaPlan<GraphValueRow>) -> Result<Self, EdgeScanBuildError> {
        if let Some(operator) = plan.operators().iter().position(|operator| {
            matches!(
                operator,
                GlaOperator::Expand { .. } | GlaOperator::Probe { .. }
            )
        }) {
            return Err(EdgeScanBuildError { operator });
        }
        let inner = EdgeScanPlan::compile(plan)?;
        Ok(Self::from_inner(inner))
    }

    fn from_inner(inner: EdgeScanPlan) -> Self {
        let GlaOperator::ProjectValues { columns } = inner.projection.as_ref() else {
            unreachable!("the ordinary edge compiler proves the projection")
        };
        let columns = columns.len();
        debug_assert!(
            inner.joined.is_none(),
            "async intake has no nested source access"
        );
        Self { inner, columns }
    }

    /// Compile all single-edge occurrences for a blocking ORDER BY/DISTINCT.
    /// Predicates, properties, captures and orientation use the SAME local GLA
    /// compiler and evaluator as ordinary async rows. Expansion and probes
    /// refuse before source construction; a joined plan never enters this lane.
    ///
    /// The returned tail retains every hidden key and final clause. Its host
    /// must sort, deduplicate when requested, window, then hide trailing cells.
    /// This input ignores final SKIP/LIMIT (including LIMIT zero), and its
    /// ResultRows allowance counts intermediate occurrences rather than output.
    pub fn compile_sort_input(
        plan: &GlaPlan<GraphValueRow>,
    ) -> Result<(Self, crate::scan_stream::ScanSortTail), EdgeScanBuildError> {
        let tail = crate::scan_stream::ScanSortTail::compile(plan)
            .map_err(|operator| EdgeScanBuildError { operator })?;
        let inner = EdgeScanPlan::compile_local(plan, LocalOutput::SortInput)?;
        Ok((Self::from_inner(inner), tail))
    }
}
impl core::fmt::Debug for AsyncEdgeScanPlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AsyncEdgeScanPlan([REDACTED])")
    }
}

/// A complete row and its host reservation. Fields drop in declaration order:
/// payload before guard. Taking the pair transfers both to the caller, who must
/// retain the guard while retaining its row. No source or record borrow escapes.
pub struct AsyncEdgeScanOutput<Guard> {
    row: GraphValueRow,
    guard: Guard,
}
impl<Guard> AsyncEdgeScanOutput<Guard> {
    pub fn row(&self) -> &GraphValueRow {
        &self.row
    }
    pub fn into_parts(self) -> (GraphValueRow, Guard) {
        (self.row, self.guard)
    }
}
impl<Guard> AsRef<GraphValueRow> for AsyncEdgeScanOutput<Guard> {
    fn as_ref(&self) -> &GraphValueRow {
        &self.row
    }
}
impl<Guard> core::ops::Deref for AsyncEdgeScanOutput<Guard> {
    type Target = GraphValueRow;
    fn deref(&self) -> &Self::Target {
        &self.row
    }
}
impl<Guard> core::fmt::Debug for AsyncEdgeScanOutput<Guard> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AsyncEdgeScanOutput([REDACTED])")
    }
}

/// An asynchronous physical cursor, never paging over an eager result bag.
/// One source, one edge/endpoints record and at most one completed row are live
/// inside a pull. Undirected non-self edges retain that record for the second
/// orientation; self-loops occur once. SnapshotRecords charges each candidate
/// once, not each endpoint/orientation. Other meters accumulate across all pulls.
///
/// Construction and an unpolled next() read nothing. LIMIT 0 reads no candidate;
/// the final requested row never prefetches the suffix. Errors fuse and drop the
/// source, while previously delivered rows remain delivered. A dropped POLLED
/// next() future also fuses the cursor and releases both source and pending
/// record: partially consumed I/O can never be resumed as successful output.
/// This is an operator/source seam, not a database lease or restart token, and
/// does not by itself make existing decoded-snapshot sources out-of-core.
pub struct AsyncEdgeScanCursor<S: AsyncEdgeScanSource, F> {
    source: Option<S>,
    pending: Option<(EId, S::Record)>,
    plan: AsyncEdgeScanPlan,
    meter: Meter<F>,
    seq: CommitSeq,
    last: Option<EId>,
    skip: u64,
    state: EdgeScanState,
}
impl<S: AsyncEdgeScanSource, F> AsyncEdgeScanCursor<S, F> {
    pub fn new(source: S, plan: AsyncEdgeScanPlan, policy: GqlQueryPolicy, checkpoint: F) -> Self {
        Self {
            seq: source.snapshot_seq(),
            skip: plan.inner.offset,
            source: Some(source),
            pending: None,
            plan,
            meter: Meter {
                checkpoint,
                policy,
                rows: GqlExecutionStats {
                    snapshot_records: 0,
                    result_rows: 0,
                },
                evaluator: GlaExecutionStats::default(),
            },
            last: None,
            state: EdgeScanState::Open,
        }
    }
    pub fn snapshot_seq(&self) -> CommitSeq {
        self.seq
    }
    pub fn state(&self) -> EdgeScanState {
        self.state
    }
    pub fn row_stats(&self) -> GqlExecutionStats {
        self.meter.rows
    }
    pub fn evaluator_stats(&self) -> GlaExecutionStats {
        self.meter.evaluator
    }
    pub fn close(&mut self) {
        if self.state == EdgeScanState::Open {
            self.state = EdgeScanState::Closed;
        }
        self.pending = None;
        self.source = None;
    }

    pub async fn next<C>(
        &mut self,
    ) -> Option<ScanResult<AsyncEdgeScanOutput<S::OutputGuard>, S::Error, C>>
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        if self.state != EdgeScanState::Open {
            return None;
        }
        // Move every source-owned reservation into this future before awaiting.
        // Cancellation/unwind cannot leave an apparently open retained cursor.
        self.state = EdgeScanState::Failed;
        let mut source = self.source.take().expect("open cursor owns its source");
        let mut pending = self.pending.take();
        match self.advance(&mut source, &mut pending).await {
            Ok(Some(row)) => {
                if self.plan.inner.count == Some(self.meter.rows.result_rows) {
                    self.state = EdgeScanState::Exhausted;
                } else {
                    self.state = EdgeScanState::Open;
                    self.source = Some(source);
                    self.pending = pending;
                }
                Some(Ok(row))
            }
            Ok(None) => {
                self.state = EdgeScanState::Exhausted;
                None
            }
            Err(error) => Some(Err(error)),
        }
    }

    async fn advance<C>(
        &mut self,
        source: &mut S,
        pending: &mut Option<(EId, S::Record)>,
    ) -> ScanResult<Option<AsyncEdgeScanOutput<S::OutputGuard>>, S::Error, C>
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        self.meter.event(GlaExecutionEvent::Work)?;
        if self.plan.inner.count == Some(0) {
            return Ok(None);
        }
        loop {
            let (eid, record, second) = if let Some((eid, record)) = pending.take() {
                (eid, record, true)
            } else {
                let meter = &mut self.meter;
                let last = &mut self.last;
                let mut admitted = None;
                let candidate = flatten(
                    source
                        .next_candidate(self.plan.inner.relation, &mut |event| match event {
                            AsyncEdgeScanEvent::Work => meter.event(GlaExecutionEvent::Work),
                            AsyncEdgeScanEvent::ScratchEntry => {
                                meter.event(GlaExecutionEvent::ScratchEntry)
                            }
                            AsyncEdgeScanEvent::Candidate(eid) => {
                                meter.event(GlaExecutionEvent::Work)?;
                                if admitted.is_some() {
                                    return Err(GqlQueryError::Source(
                                        EdgeScanError::InvalidCandidateAdmission,
                                    ));
                                }
                                if last.is_some_and(|previous| eid <= previous) {
                                    return Err(GqlQueryError::Source(
                                        EdgeScanError::NonIncreasingIdentity,
                                    ));
                                }
                                meter.rows.snapshot_records = meter.increment(
                                    GqlBudgetDimension::SnapshotRecords,
                                    meter.rows.snapshot_records,
                                )?;
                                *last = Some(eid);
                                admitted = Some(eid);
                                Ok(())
                            }
                        })
                        .await,
                )?;
                // Cancellation after a suspension still wins, even at EOF or
                // when the last candidate is invisible and needs no projection.
                meter.event(GlaExecutionEvent::Work)?;
                if candidate.as_ref().map(|candidate| candidate.eid) != admitted {
                    return Err(GqlQueryError::Source(
                        EdgeScanError::InvalidCandidateAdmission,
                    ));
                }
                let Some(candidate) = candidate else {
                    return Ok(None);
                };
                let Some(record) = candidate.record else {
                    continue;
                };
                (candidate.eid, record, false)
            };
            row_event(&mut self.meter, source, &record, GlaExecutionEvent::Work)?;
            let edge = record.edge();
            if !self.plan.inner.relation.matches(edge.relation) {
                continue;
            }
            let reverse = self.plan.inner.direction == GlaDirection::Undirected
                && edge.source != edge.target
                && !second;
            let (from, to) = match self.plan.inner.direction {
                GlaDirection::Forward => (edge.source, edge.target),
                GlaDirection::Reverse => (edge.target, edge.source),
                GlaDirection::Undirected if second => {
                    (edge.source.max(edge.target), edge.source.min(edge.target))
                }
                GlaDirection::Undirected => {
                    (edge.source.min(edge.target), edge.source.max(edge.target))
                }
            };
            let output = self.project_record(source, eid, &record, from, to)?;
            // Even a rejected or skipped first orientation leaves the second
            // eligible. Moving the record preserves its reservation, no clone.
            if reverse {
                *pending = Some((eid, record));
            }
            if let Some(output) = output {
                return Ok(Some(output));
            }
        }
    }

    fn project_record<C>(
        &mut self,
        source: &S,
        eid: EId,
        record: &S::Record,
        from: VId,
        to: VId,
    ) -> ScanResult<Option<AsyncEdgeScanOutput<S::OutputGuard>>, S::Error, C>
    where
        F: FnMut() -> Result<(), C>,
    {
        let meter = &mut self.meter;
        row_event(meter, source, record, GlaExecutionEvent::Work)?;
        let left = record
            .vertex(from)
            .ok_or(GqlQueryError::Source(EdgeScanError::DanglingEndpoint))?;
        let right = if from == to {
            left
        } else {
            row_event(meter, source, record, GlaExecutionEvent::Work)?;
            record
                .vertex(to)
                .ok_or(GqlQueryError::Source(EdgeScanError::DanglingEndpoint))?
        };
        let image = Binding {
            eid,
            ids: [from, to],
            vertices: [left, right],
            edge: record.edge().properties,
        };
        let Some(paths) = self
            .plan
            .inner
            .test(&image, &mut |event| row_event(meter, source, record, event))?
        else {
            return Ok(None);
        };
        if self.skip != 0 {
            self.skip -= 1;
            return Ok(None);
        }
        let next = meter.increment(GqlBudgetDimension::ResultRows, meter.rows.result_rows)?;
        let guard = flatten(
            source.reserve_output(record, self.plan.columns, &mut |event| meter.event(event)),
        )?;
        let row = self.plan.inner.project(&image, &paths, &mut |event| {
            row_event(meter, source, record, event)
        })?;
        row_event(meter, source, record, GlaExecutionEvent::ResultRow)?;
        meter.rows.result_rows = next;
        Ok(Some(AsyncEdgeScanOutput { row, guard }))
    }
}

fn row_event<S: AsyncEdgeScanSource, F, C>(
    meter: &mut Meter<F>,
    source: &S,
    record: &S::Record,
    event: GlaExecutionEvent,
) -> ScanResult<(), S::Error, C>
where
    F: FnMut() -> Result<(), C>,
{
    meter.event(event)?;
    source
        .evaluation_event(record, event)
        .map_err(|error| GqlQueryError::Source(EdgeScanError::Source(error)))
}

impl<S: AsyncEdgeScanSource, F> core::fmt::Debug for AsyncEdgeScanCursor<S, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AsyncEdgeScanCursor")
            .field("snapshot_seq", &self.seq)
            .field("state", &self.state)
            .field("rows", &self.meter.rows)
            .field("evaluator", &self.meter.evaluator)
            .field("source_and_plan", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests;
