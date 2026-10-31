//! Vertex-rooted intake for the existing indexed asynchronous semi/anti kernel.
//! This explicit capability leaves the ordinary row-local source unchanged.

use super::*;
use crate::edge_stream::{AsyncGraphWalkSource, EdgeScanError, EdgeScanSourceError};

/// A vertex scan with correlated EXISTS/NOT EXISTS bindings. Fixed edges and
/// finite variable-length atoms reuse the native probe compiler and endpoint
/// support kernel. Independent scans, nested/optional scopes, unbounded walks
/// and unsupported captured paths refuse before a source is constructed.
#[derive(Clone)]
pub struct AsyncVertexProbePlan<Row = VId> {
    inner: VertexScanPlan<Row>,
    columns: usize,
}

impl<Row: VertexScanOutput> AsyncVertexProbePlan<Row> {
    pub fn compile(plan: &GlaPlan<Row>) -> Result<Self, VertexScanBuildError> {
        Self::check_access(plan)?;
        Ok(Self::from_inner(VertexScanPlan::compile(plan)?))
    }

    fn check_access(plan: &GlaPlan<Row>) -> Result<(), VertexScanBuildError> {
        if !plan
            .operators()
            .iter()
            .any(|op| matches!(op, GlaOperator::Probe { .. }))
        {
            return Err(VertexScanBuildError { operator: 0 });
        }
        if let Some(operator) = plan
            .operators()
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(at, op)| match op {
                GlaOperator::ScanVertices => Some(at),
                GlaOperator::VarLengthExpand { bounds, .. } if !bounds.is_bounded() => Some(at),
                _ => None,
            })
        {
            return Err(VertexScanBuildError { operator });
        }
        Ok(())
    }

    fn from_inner(inner: VertexScanPlan<Row>) -> Self {
        let columns = match inner.projection.as_ref() {
            GlaOperator::Project { .. } => 1,
            GlaOperator::ProjectValues { columns } => columns.len(),
            _ => unreachable!("native vertex projection"),
        };
        Self { inner, columns }
    }
}

impl AsyncVertexProbePlan<crate::algebra::GraphValueRow> {
    /// Complete private occurrences for the ordinary external tail. A final
    /// empty page does not suppress a reached predicate or probe failure.
    pub fn compile_sort_input(
        plan: &GlaPlan<crate::algebra::GraphValueRow>,
    ) -> Result<(Self, crate::scan_stream::ScanSortTail), VertexScanBuildError> {
        Self::check_access(plan)?;
        let (inner, tail) = VertexScanPlan::compile_sort_input(plan)?;
        Ok((Self::from_inner(inner), tail))
    }
}

impl<Row> core::fmt::Debug for AsyncVertexProbePlan<Row> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AsyncVertexProbePlan([REDACTED])")
    }
}

/// One real root record and the existing probe's governed active bindings.
/// Root and incidence reads share one source, snapshot, policy and memory
/// owner. No graph snapshot or neighbor/result bag is built by this cursor.
/// A failed or dropped polled pull drops the source and makes it terminal.
pub struct AsyncVertexProbeCursor<S: AsyncGraphWalkSource, F, Row = VId> {
    source: Option<S>,
    plan: AsyncVertexProbePlan<Row>,
    meter: Meter<F>,
    snapshot_seq: CommitSeq,
    last: Option<VId>,
    skip: u64,
    state: VertexScanState,
}

impl<S: AsyncGraphWalkSource, F, Row: VertexScanOutput> AsyncVertexProbeCursor<S, F, Row> {
    pub fn new(
        source: S,
        plan: AsyncVertexProbePlan<Row>,
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
        if self.state != VertexScanState::Open {
            return None;
        }
        self.state = VertexScanState::Failed;
        let mut source = self.source.take().expect("open cursor owns its source");
        match self.advance(&mut source).await {
            Ok(Some(value)) => {
                if self.plan.inner.count == Some(self.meter.rows.result_rows) {
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

    async fn advance<C>(
        &mut self,
        source: &mut S,
    ) -> ScanResult<Option<AsyncVertexScanOutput<Row, S::OutputGuard>>, S::Error, C>
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        let meter = &mut self.meter;
        if self.plan.inner.empty || self.plan.inner.count == Some(0) {
            (meter.checkpoint)().map_err(GqlQueryError::Interrupted)?;
            return Ok(None);
        }
        meter.event(VertexScanEvent::Work)?;
        loop {
            let mut admitted = None;
            let candidate = flatten_source(
                source
                    .next_root_candidate(&mut |event| match event {
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
            let Some(mut record) = candidate.record else {
                continue;
            };
            let mut accepted = true;
            for test in self.plan.inner.tests.iter() {
                row_event(
                    meter,
                    &mut |event| source.root_evaluation_event(&record, gla_event(event)),
                    VertexScanEvent::Work,
                )?;
                let passed = match test {
                    Test::Probe(probe) => {
                        let mut loan = ProbeMeter::new(meter);
                        probe
                            .accepts_vertex_async(
                                candidate.vid,
                                &mut record,
                                source,
                                &mut loan.inner,
                                self.snapshot_seq,
                            )
                            .await
                            .map_err(|error| error.map_source(probe_error))?
                    }
                    native => {
                        accepts_native_test(native, candidate.vid, record.as_row(), &mut |event| {
                            row_event(
                                meter,
                                &mut |event| {
                                    source.root_evaluation_event(&record, gla_event(event))
                                },
                                event,
                            )
                        })?
                    }
                };
                if !passed {
                    accepted = false;
                    break;
                }
            }
            if !accepted {
                continue;
            }
            if let Some((row, guard)) = self.plan.inner.project_accepted::<true, _, _, _, _>(
                candidate.vid,
                record.as_row(),
                &mut self.skip,
                meter,
                (
                    |meter| {
                        flatten_source(source.reserve_walk_output(
                            &record,
                            &[],
                            self.plan.columns,
                            &mut |event| meter.event(vertex_event(event)),
                        ))
                    },
                    |event| source.root_evaluation_event(&record, gla_event(event)),
                ),
            )? {
                return Ok(Some(AsyncVertexScanOutput { row, guard }));
            }
        }
    }
}

fn gla_event(event: VertexScanEvent) -> GlaExecutionEvent {
    match event {
        VertexScanEvent::Work => GlaExecutionEvent::Work,
        VertexScanEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
    }
}

fn vertex_event(event: GlaExecutionEvent) -> VertexScanEvent {
    match event {
        GlaExecutionEvent::ScratchEntry => VertexScanEvent::ScratchEntry,
        GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => VertexScanEvent::Work,
    }
}

fn probe_error<E>(error: EdgeScanError<E>) -> VertexScanError<E> {
    match error {
        EdgeScanError::Source(source) => VertexScanError::Source(source),
        error => VertexScanError::Probe(error),
    }
}

fn flatten_source<T, E, C>(
    result: Result<T, EdgeScanSourceError<E, GqlQueryError<VertexScanError<E>, C>>>,
) -> ScanResult<T, E, C> {
    result.map_err(|error| match error {
        EdgeScanSourceError::Source(error) => GqlQueryError::Source(VertexScanError::Source(error)),
        EdgeScanSourceError::Control(error) => error,
    })
}

// This is a loan of the existing allowance and counters, not another budget.
// The native edge probe kernel takes its familiar meter; Drop copies every
// charged event back on success, error, unwind and a cancelled pending await.
struct ProbeMeter<'a, F> {
    inner: crate::edge_stream::Meter<&'a mut F>,
    rows: &'a mut GqlExecutionStats,
    evaluator: &'a mut GlaExecutionStats,
}

impl<'a, F> ProbeMeter<'a, F> {
    fn new(meter: &'a mut Meter<F>) -> Self {
        Self {
            inner: crate::edge_stream::Meter {
                checkpoint: &mut meter.checkpoint,
                policy: meter.policy,
                rows: meter.rows,
                evaluator: meter.evaluator,
            },
            rows: &mut meter.rows,
            evaluator: &mut meter.evaluator,
        }
    }
}

impl<F> Drop for ProbeMeter<'_, F> {
    fn drop(&mut self) {
        *self.rows = self.inner.rows;
        *self.evaluator = self.inner.evaluator;
    }
}

impl<S: AsyncGraphWalkSource, F, Row> core::fmt::Debug for AsyncVertexProbeCursor<S, F, Row> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AsyncVertexProbeCursor")
            .field("snapshot", &self.snapshot_seq)
            .field("state", &self.state)
            .field("rows", &self.meter.rows)
            .field("evaluator", &self.meter.evaluator)
            .finish_non_exhaustive()
    }
}
