//! Awaitable source intake for the existing ordered vertex operator. Only the
//! source driver is asynchronous: predicates, projection, window semantics and
//! counters enter the same kernel as VertexScanCursor. No graph is collected.

use super::*;

mod aggregate;
pub use aggregate::{AsyncVertexSpillAggregateCursor, AsyncVertexSpillAggregatePlan};

/// One owned, admitted vertex record. A host can keep its byte reservation in
/// this object while the ordinary GLA evaluator borrows its canonical fields.
/// This interface conveys no authority; the source owns visibility and scope.
pub trait AsyncVertexScanRecord {
    fn as_row(&self) -> VertexScanRow<'_>;
}

/// Every distinct historical identity is yielded once in increasing order.
/// An identity invisible at the pinned cut still debits SnapshotRecords.
pub struct AsyncVertexCandidate<Record> {
    pub vid: VId,
    pub record: Option<Record>,
}

/// What one [`AsyncVertexScanSource::next_candidate`] resolves to: the next
/// candidate, `None` at EOF, or the source's or a control's refusal.
pub type AsyncVertexCandidateResult<Record, Error, Control> =
    Result<Option<AsyncVertexCandidate<Record>>, VertexScanSourceError<Error, Control>>;

/// One source callback keeps work and candidate admission in the same meter
/// without a lock or a shared mutable borrow spanning asynchronous I/O.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsyncVertexScanEvent {
    Work,
    ScratchEntry,
    Candidate(VId),
}

/// A fixed-generation asynchronous source with host-owned memory admission.
///
/// Source controls precede each logical work/scratch operation, including I/O
/// and repeated decoding; refused controls propagate unchanged. The source
/// must never disguise a failed read as an absent vertex or end of input.
/// Returning a candidate may need several governed source operations, but may
/// retain only the source's declared bounded state and that one record.
/// Each next_candidate emits Candidate(vid) exactly once before resolving
/// that identity's history or allocating its record, then returns that same
/// identity. EOF invokes no admission. A refused admission is a Control error.
///
/// reserve_output admits storage for at most `columns` copied cells from this
/// record, including row/vector overhead and repeated property payloads. It
/// runs after predicates and the query window admit the row, before projection
/// copies any fields. Its guard survives with the returned result. A host that
/// does not provide byte accounting can explicitly use (), but then this seam
/// alone establishes no physical memory bound. Caller collection is independent.
pub trait AsyncVertexScanSource: Send {
    type Error: Send;
    type Record: AsyncVertexScanRecord + Send;
    type OutputGuard: Send;

    fn snapshot_seq(&self) -> CommitSeq;

    fn next_candidate<C: Send>(
        &mut self,
        control: &mut (impl FnMut(AsyncVertexScanEvent) -> Result<(), C> + Send),
    ) -> impl core::future::Future<Output = AsyncVertexCandidateResult<Self::Record, Self::Error, C>>
    + Send;

    fn reserve_output<C>(
        &self,
        record: &Self::Record,
        columns: usize,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Self::OutputGuard, VertexScanSourceError<Self::Error, C>>;

    /// Observe each admitted row-local evaluator event before its operation.
    /// ScratchEntry precedes owned operand, VM frame and projected-cell growth;
    /// byte-accounted hosts grow a temporary reservation retained by `record`.
    /// The record's reservation remains live through projection, while the
    /// independent output guard protects the delivered row after record drop.
    /// Work callbacks need no allocation. Refusals are terminal source errors.
    fn evaluation_event(
        &self,
        record: &Self::Record,
        event: VertexScanEvent,
    ) -> Result<(), Self::Error>;
}

/// The ordinary compiler's ordered, vertex-local physical profile. Probes are
/// refused before source construction: their nested readers need a separate
/// asynchronous access contract. Alternate ordering and blocking operators
/// retain the ordinary compiler's explicit refusal, without an eager fallback.
#[derive(Clone)]
pub struct AsyncVertexScanPlan<Row = VId> {
    inner: VertexScanPlan<Row>,
    columns: usize,
}

impl<Row: VertexScanOutput> AsyncVertexScanPlan<Row> {
    pub fn compile(plan: &GlaPlan<Row>) -> Result<Self, VertexScanBuildError> {
        let inner = VertexScanPlan::compile(plan)?;
        Self::check_source(plan)?;
        Ok(Self::from_inner(inner))
    }

    fn check_source(plan: &GlaPlan<Row>) -> Result<(), VertexScanBuildError> {
        if let Some(operator) = plan
            .operators()
            .iter()
            .position(|operator| matches!(operator, GlaOperator::Probe { .. }))
        {
            return Err(VertexScanBuildError { operator });
        }
        Ok(())
    }

    fn from_inner(inner: VertexScanPlan<Row>) -> Self {
        let columns = match inner.projection.as_ref() {
            GlaOperator::Project { .. } => 1,
            GlaOperator::ProjectValues { columns } => columns.len(),
            _ => unreachable!("the existing physical compiler proves the projection"),
        };
        Self { inner, columns }
    }
}

impl AsyncVertexScanPlan<crate::algebra::GraphValueRow> {
    /// Compile the private source of a blocking ORDER BY or DISTINCT.
    /// The ordinary compiler owns every predicate, projected cell and terminal
    /// clause. This relaxes only its leading-identity order proof; probes still
    /// refuse before a source is constructed. No hidden key is discarded here.
    ///
    /// Every matching occurrence is emitted in source identity order, including
    /// duplicates and the complete suffix when the requested LIMIT is zero.
    /// ResultRows counts intermediate occurrences. A host must budget them and
    /// apply the returned sort/distinct/window contract before exposing results.
    pub fn compile_sort_input(
        plan: &GlaPlan<crate::algebra::GraphValueRow>,
    ) -> Result<(Self, crate::scan_stream::ScanSortTail), VertexScanBuildError> {
        Self::check_source(plan)?;
        let (inner, tail) = VertexScanPlan::compile_sort_input(plan)?;
        Ok((Self::from_inner(inner), tail))
    }
}

impl<Row> core::fmt::Debug for AsyncVertexScanPlan<Row> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AsyncVertexScanPlan([REDACTED])")
    }
}

/// One complete result and its source-provided allocation reservation. The row
/// is dropped before its guard. Extracting the pair transfers both obligations
/// to the caller; retaining the row requires retaining its matching guard.
pub struct AsyncVertexScanOutput<Row, Guard> {
    row: Row,
    guard: Guard,
}

impl<Row, Guard> AsyncVertexScanOutput<Row, Guard> {
    pub fn row(&self) -> &Row {
        &self.row
    }

    pub fn into_parts(self) -> (Row, Guard) {
        (self.row, self.guard)
    }
}

impl<Row, Guard> AsRef<Row> for AsyncVertexScanOutput<Row, Guard> {
    fn as_ref(&self) -> &Row {
        &self.row
    }
}

impl<Row, Guard> core::ops::Deref for AsyncVertexScanOutput<Row, Guard> {
    type Target = Row;

    fn deref(&self) -> &Self::Target {
        &self.row
    }
}

impl<Row, Guard> core::fmt::Debug for AsyncVertexScanOutput<Row, Guard> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AsyncVertexScanOutput([REDACTED])")
    }
}

/// A fused asynchronous cursor using the SAME row-local GLA kernel as the
/// synchronous stream. Source, predicate, payload and result work share one
/// cumulative policy over all pulls. SnapshotRecords counts candidate identity
/// histories, including invisible identities, before the source resolves their
/// payloads; ResultRows counts only complete deliveries after SKIP/LIMIT.
///
/// Construction performs no source read. LIMIT exhaustion never fetches the
/// next identity. Error is delivered once and drops the source; earlier rows
/// stay delivered. Closing or dropping does not drain the unread suffix.
/// Dropping a pending next() future fuses the cursor and releases its source:
/// a half-consumed source prefix can never resume as successful query output.
pub struct AsyncVertexScanCursor<S: AsyncVertexScanSource, F, Row = VId> {
    source: Option<S>,
    plan: AsyncVertexScanPlan<Row>,
    meter: Meter<F>,
    snapshot_seq: CommitSeq,
    last: Option<VId>,
    skip: u64,
    state: VertexScanState,
}

impl<S: AsyncVertexScanSource, F, Row: VertexScanOutput> AsyncVertexScanCursor<S, F, Row> {
    pub fn new(
        source: S,
        plan: AsyncVertexScanPlan<Row>,
        policy: GqlQueryPolicy,
        checkpoint: F,
    ) -> Self {
        Self {
            snapshot_seq: source.snapshot_seq(),
            skip: plan.inner.offset,
            source: Some(source),
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
            state: VertexScanState::Open,
        }
    }

    pub fn snapshot_seq(&self) -> CommitSeq {
        self.snapshot_seq
    }

    pub fn state(&self) -> VertexScanState {
        self.state
    }

    pub fn row_stats(&self) -> GqlExecutionStats {
        self.meter.rows
    }

    pub fn evaluator_stats(&self) -> GlaExecutionStats {
        self.meter.evaluator
    }

    pub fn close(&mut self) {
        if self.state == VertexScanState::Open {
            self.state = VertexScanState::Closed;
        }
        self.source = None;
    }

    pub async fn next<C>(
        &mut self,
    ) -> Option<ScanResult<AsyncVertexScanOutput<Row, S::OutputGuard>, S::Error, C>>
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        self.next_inner::<true, C>().await
    }

    // Blocking reducers consume private occurrences through this same source
    // and projection loop, without spending the final result-row allowance.
    async fn next_inner<const EMIT: bool, C>(
        &mut self,
    ) -> Option<ScanResult<AsyncVertexScanOutput<Row, S::OutputGuard>, S::Error, C>>
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        if self.state != VertexScanState::Open {
            return None;
        }
        // The source is owned by this future through every await. Cancellation
        // or unwind drops it and leaves the persistent cursor terminal.
        self.state = VertexScanState::Failed;
        let mut source = self.source.take().expect("open cursor owns its source");
        match self.advance::<EMIT, C>(&mut source).await {
            Ok(Some(value)) => {
                if EMIT && self.plan.inner.count == Some(self.meter.rows.result_rows) {
                    self.state = VertexScanState::Exhausted;
                } else {
                    self.state = VertexScanState::Open;
                    self.source = Some(source);
                }
                Some(Ok(value))
            }
            Ok(None) => {
                self.state = VertexScanState::Exhausted;
                None
            }
            Err(error) => Some(Err(error)),
        }
    }

    async fn advance<const EMIT: bool, C>(
        &mut self,
        source: &mut S,
    ) -> ScanResult<Option<AsyncVertexScanOutput<Row, S::OutputGuard>>, S::Error, C>
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        let meter = &mut self.meter;
        meter.event(VertexScanEvent::Work)?;
        if self.plan.inner.empty || self.plan.inner.count == Some(0) {
            return Ok(None);
        }
        loop {
            let mut admitted = None;
            let candidate = flatten(
                source
                    .next_candidate(&mut |event| match event {
                        AsyncVertexScanEvent::Work => meter.event(VertexScanEvent::Work),
                        AsyncVertexScanEvent::ScratchEntry => {
                            meter.event(VertexScanEvent::ScratchEntry)
                        }
                        AsyncVertexScanEvent::Candidate(vid) => {
                            meter.event(VertexScanEvent::Work)?;
                            if admitted.is_some() {
                                return Err(GqlQueryError::Source(
                                    VertexScanError::InvalidCandidateAdmission,
                                ));
                            }
                            if self.last.is_some_and(|last| vid <= last) {
                                return Err(GqlQueryError::Source(
                                    VertexScanError::NonIncreasingIdentity,
                                ));
                            }
                            meter.record()?;
                            self.last = Some(vid);
                            admitted = Some(vid);
                            Ok(())
                        }
                    })
                    .await,
            )?;
            if candidate.as_ref().map(|candidate| candidate.vid) != admitted {
                return Err(GqlQueryError::Source(
                    VertexScanError::InvalidCandidateAdmission,
                ));
            }
            let Some(candidate) = candidate else {
                return Ok(None);
            };
            let Some(record) = candidate.record else {
                continue;
            };
            let row = record.as_row();
            let local = RecordSource::<S::Error> {
                vid: candidate.vid,
                row,
                snapshot_seq: self.snapshot_seq,
                error: core::marker::PhantomData,
            };
            if let Some((row, guard)) = self.plan.inner.project_record::<EMIT, _, _, _, _>(
                candidate.vid,
                row,
                &local,
                &mut self.skip,
                meter,
                (
                    |meter| {
                        flatten(
                            source.reserve_output(&record, self.plan.columns, &mut |event| {
                                meter.event(event)
                            }),
                        )
                    },
                    |event| source.evaluation_event(&record, event),
                ),
            )? {
                return Ok(Some(AsyncVertexScanOutput { row, guard }));
            }
        }
    }
}

impl<S: AsyncVertexScanSource, F, Row> core::fmt::Debug for AsyncVertexScanCursor<S, F, Row> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AsyncVertexScanCursor")
            .field("snapshot_seq", &self.snapshot_seq)
            .field("state", &self.state)
            .field("rows", &self.meter.rows)
            .field("evaluator", &self.meter.evaluator)
            .field("definition_and_source", &"[REDACTED]")
            .finish()
    }
}

// Only the already-borrowed candidate enters the shared row kernel. This is
// neither a graph nor an alternate source engine. The asynchronous compiler
// excludes probes, and the source defaults still refuse nested lookup if that
// proof ever regresses; no unsupported read is reported as an empty domain.
struct RecordSource<'a, E> {
    vid: VId,
    row: VertexScanRow<'a>,
    snapshot_seq: CommitSeq,
    error: core::marker::PhantomData<E>,
}

impl<E> VertexScanSource for RecordSource<'_, E> {
    type Error = E;

    fn snapshot_seq(&self) -> CommitSeq {
        self.snapshot_seq
    }

    fn next_vertex<C>(
        &mut self,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<E, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        Ok(None)
    }

    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<E, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        Ok((vid == self.vid).then_some(self.row))
    }
}

#[cfg(test)]
mod tests;
