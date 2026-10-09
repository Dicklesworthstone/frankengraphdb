//! Awaitable indexed intake for the existing fixed-hop join compiler. The
//! synchronous stage/capture/projector remains the semantic authority. Only
//! admitted records on the active traversal are retained, never neighbor bags.

use super::*;
use crate::algebra::EdgeRelation;
use crate::edge_stream::aggregate::{EdgeAggregateError, lift, value_event};
use crate::spill_aggregate::SpillAggregateDefinition;
use crate::stream::VertexScanEvent;
use crate::GraphAggregateError;

/// An asynchronous source with independent strict-successor incidence reads.
/// Root and nested reads share one immutable cut and masking policy. Nested
/// reads must not advance the root scan, scan unrelated graph edges, or collect
/// a neighbor bag. `after` belongs to a particular traversal prefix, not to a
/// global source position; repeated prefixes can examine an identity again.
///
/// Emit Candidate(eid) exactly once before resolving/allocating that history,
/// including invisible candidates, and return the same identity. EOF emits no
/// Candidate. Propagate every control refusal unchanged. A missing index is
/// Unavailable, never an empty neighborhood. Relation and endpoint visibility
/// must be admitted BEFORE opening an index; the selector conveys no authority.
///
/// The cursor holds at most `hops` records. reserve_traversal admits fixed
/// cursor vectors (record slots, choices, bindings and successor positions)
/// before allocation and retains its guard until that traversal is dropped.
/// Each record owns its source payload reservation. reserve_join_output admits
/// the full copied projection, including repeated fields/path cells from ANY
/// retained record, before the shared projector allocates. Its output guard
/// outlives every source record. Explicit () guards declare no byte bound.
pub trait AsyncEdgeJoinSource: AsyncEdgeScanSource {
    type TraversalGuard: Send;

    fn next_incident_candidate<C: Send>(
        &mut self,
        endpoint: VId,
        relation: EdgeRelation,
        direction: GlaDirection,
        after: Option<EId>,
        control: &mut (impl FnMut(AsyncEdgeScanEvent) -> Result<(), C> + Send),
    ) -> impl core::future::Future<
        Output = Result<
            Option<AsyncEdgeCandidate<Self::Record>>,
            EdgeExpansionSourceError<Self::Error, C>,
        >,
    > + Send;

    fn reserve_traversal<C>(
        &self,
        hops: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Self::TraversalGuard, EdgeScanSourceError<Self::Error, C>>;

    fn reserve_join_output<C>(
        &self,
        records: &[Self::Record],
        columns: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Self::OutputGuard, EdgeScanSourceError<Self::Error, C>>;
}

/// The ordinary fixed-hop compiler, restricted to awaitable positive joins.
/// Its identity prefix proves canonical order and DISTINCT without a seen set.
/// Probes, OPTIONAL and variable-length traversal remain typed refusals; this
/// compiler cannot unlock them by accidentally treating missing reads as EOF.
#[derive(Clone)]
pub struct AsyncEdgeJoinPlan {
    inner: EdgeScanPlan,
    columns: usize,
}
impl AsyncEdgeJoinPlan {
    pub fn compile(plan: &GlaPlan<GraphValueRow>) -> Result<Self, EdgeScanBuildError> {
        Self::check_access(plan)?;
        Ok(Self::from_inner(compile_output(plan, Output::OrderedRows)?))
    }

    // Only the sealed external-aggregate definition may relax the identity
    // prefix. Reuse the ordinary aggregate input proof: no child DISTINCT,
    // window, hidden keys, probes or unsupported expansion can be discarded.
    pub(crate) fn compile_aggregate(
        plan: &GlaPlan<GraphValueRow>,
    ) -> Result<Self, EdgeScanBuildError> {
        Self::check_access(plan)?;
        Ok(Self::from_inner(compile_output(plan, Output::AggregateInput)?))
    }

    /// ALL occurrences for a blocking consumer. The returned native tail must
    /// be applied exactly once AFTER sorting, including DISTINCT and hidden
    /// key removal. Final SKIP/LIMIT never suppresses input errors in this lane.
    /// ResultRows counts input occurrences, not the consumer's final page.
    pub fn compile_sort_input(
        plan: &GlaPlan<GraphValueRow>,
    ) -> Result<(Self, crate::scan_stream::ScanSortTail), EdgeScanBuildError> {
        Self::check_access(plan)?;
        let (inner, tail) = EdgeScanPlan::compile_sort_input(plan)?;
        Ok((Self::from_inner(inner), tail))
    }

    fn check_access(plan: &GlaPlan<GraphValueRow>) -> Result<(), EdgeScanBuildError> {
        if let Some(operator) = plan
            .operators()
            .iter()
            .position(|op| matches!(op, GlaOperator::Probe { .. }))
        {
            return Err(EdgeScanBuildError { operator });
        }
        Ok(())
    }

    fn from_inner(inner: EdgeScanPlan) -> Self {
        let GlaOperator::ProjectValues { columns } = inner.projection.as_ref() else {
            unreachable!("the fixed-hop compiler proves the projection")
        };
        let columns = columns.len();
        Self { inner, columns }
    }

    pub fn hop_count(&self) -> usize {
        self.inner
            .joined
            .as_ref()
            .expect("compiled join")
            .expansions
            .len()
            + 1
    }
}
impl core::fmt::Debug for AsyncEdgeJoinPlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AsyncEdgeJoinPlan([REDACTED])")
    }
}

/// A completed projection and its host reservation, dropped in that order.
/// Extracting the pair transfers both obligations to the caller; retain the
/// guard for as long as the extracted row is retained.
pub struct AsyncEdgeJoinOutput<Guard> {
    row: GraphValueRow,
    guard: Guard,
}
impl<Guard> AsyncEdgeJoinOutput<Guard> {
    pub fn row(&self) -> &GraphValueRow {
        &self.row
    }
    pub fn into_parts(self) -> (GraphValueRow, Guard) {
        (self.row, self.guard)
    }
}
impl<Guard> AsRef<GraphValueRow> for AsyncEdgeJoinOutput<Guard> {
    fn as_ref(&self) -> &GraphValueRow {
        &self.row
    }
}
impl<Guard> core::ops::Deref for AsyncEdgeJoinOutput<Guard> {
    type Target = GraphValueRow;
    fn deref(&self) -> &Self::Target {
        &self.row
    }
}
impl<Guard> core::fmt::Debug for AsyncEdgeJoinOutput<Guard> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AsyncEdgeJoinOutput([REDACTED])")
    }
}

struct Frame<R, Guard> {
    records: Vec<R>,
    traversal: Traversal,
    pending: Option<(EId, R)>,
    reverse_root: bool,
    // Payloads and vectors drop before their reservation is refunded.
    _guard: Guard,
}
impl<R, Guard> Frame<R, Guard> {
    fn pop(&mut self) {
        let record = self.records.pop().expect("active record");
        let eid = self.traversal.choices.last().expect("active choice").eid;
        self.traversal.pop();
        if self.records.is_empty() && self.reverse_root {
            self.reverse_root = false;
            self.pending = Some((eid, record));
        }
    }
}

/// Depth-bounded asynchronous fixed-hop joins over owned source records.
/// Parallel edges, repeated vertices/edges and undirected self-loops retain the
/// ordinary compiled semantics. Each nested prefix has its own successor; a
/// root edge's second orientation reuses its record without another root read.
///
/// Work, scratch, candidate histories and output rows share one cumulative
/// policy. Closing, error or dropping a polled pending next() releases ALL
/// active records and the source, without reading an unused suffix. Earlier
/// successful rows remain delivered; exhaustion establishes full completion.
/// This is not a storage implementation, byte-bound promise, Warden grant or
/// durable resume token. Hosts must supply a real indexed source and guards.
pub struct AsyncEdgeJoinCursor<S: AsyncEdgeJoinSource, F> {
    frame: Option<Frame<S::Record, S::TraversalGuard>>,
    source: Option<S>,
    plan: AsyncEdgeJoinPlan,
    meter: Meter<F>,
    seq: CommitSeq,
    last: Option<EId>,
    skip: u64,
    state: EdgeScanState,
}
impl<S: AsyncEdgeJoinSource, F> AsyncEdgeJoinCursor<S, F> {
    pub fn new(source: S, plan: AsyncEdgeJoinPlan, policy: GqlQueryPolicy, checkpoint: F) -> Self {
        Self {
            seq: source.snapshot_seq(),
            skip: plan.inner.offset,
            frame: None,
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
        self.frame = None;
        self.source = None;
    }

    pub async fn next<C>(
        &mut self,
    ) -> Option<ScanResult<AsyncEdgeJoinOutput<S::OutputGuard>, S::Error, C>>
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        self.next_inner::<true, C>().await
    }

    async fn next_inner<const EMIT: bool, C>(
        &mut self,
    ) -> Option<ScanResult<AsyncEdgeJoinOutput<S::OutputGuard>, S::Error, C>>
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        if self.state != EdgeScanState::Open {
            return None;
        }
        // A pending future owns the source and every retained record. Its drop
        // cannot leave a consumed prefix in a resumable persistent cursor.
        self.state = EdgeScanState::Failed;
        let mut source = self.source.take().expect("open cursor owns source");
        let mut frame = self.frame.take();
        match self.advance::<EMIT, C>(&mut source, &mut frame).await {
            Ok(Some(row)) => {
                if self.plan.inner.count == Some(self.meter.rows.result_rows) {
                    self.state = EdgeScanState::Exhausted;
                } else {
                    self.state = EdgeScanState::Open;
                    self.source = Some(source);
                    self.frame = frame;
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

    async fn advance<const EMIT: bool, C>(
        &mut self,
        source: &mut S,
        frame: &mut Option<Frame<S::Record, S::TraversalGuard>>,
    ) -> ScanResult<Option<AsyncEdgeJoinOutput<S::OutputGuard>>, S::Error, C>
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        let meter = &mut self.meter;
        meter.event(GlaExecutionEvent::Work)?;
        if self.plan.inner.count == Some(0) {
            return Ok(None);
        }
        let plan = self.plan.inner.joined.as_ref().expect("compiled join");
        let hops = plan.expansions.len() + 1;
        if frame.is_none() {
            let guard = flatten(source.reserve_traversal(hops, &mut |event| meter.event(event)))?;
            let traversal = Traversal::new(hops, &mut |event| meter.event(event))?;
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            *frame = Some(Frame {
                records: Vec::with_capacity(hops),
                traversal,
                pending: None,
                reverse_root: false,
                _guard: guard,
            });
        }
        let frame = frame.as_mut().expect("admitted traversal");
        if frame.traversal.resume {
            frame.pop();
            frame.traversal.resume = false;
        }
        loop {
            meter.event(GlaExecutionEvent::Work)?;
            let depth = frame.records.len();
            let (eid, record, from, to) = if depth == 0 {
                let (eid, record, second) = if let Some((eid, record)) = frame.pending.take() {
                    (eid, record, true)
                } else {
                    let mut admitted = None;
                    let candidate = flatten(
                        source
                            .next_candidate(self.plan.inner.relation, &mut |event| {
                                admit(meter, &mut self.last, &mut admitted, event)
                            })
                            .await,
                    )?;
                    check_admission(&candidate, admitted)?;
                    let Some(candidate) = candidate else {
                        return Ok(None);
                    };
                    let Some(record) = candidate.record else {
                        continue;
                    };
                    (candidate.eid, record, false)
                };
                meter.event(GlaExecutionEvent::Work)?;
                let edge = record.edge();
                if !self.plan.inner.relation.matches(edge.relation) {
                    continue;
                }
                let (from, to) = match self.plan.inner.direction {
                    GlaDirection::Forward => (edge.source, edge.target),
                    GlaDirection::Reverse => (edge.target, edge.source),
                    GlaDirection::Undirected => {
                        let low = edge.source.min(edge.target);
                        let high = edge.source.max(edge.target);
                        frame.reverse_root = !second && low != high;
                        if second { (high, low) } else { (low, high) }
                    }
                };
                (eid, record, from, to)
            } else {
                let expansion = plan.expansions[depth - 1];
                let from = frame.traversal.bindings[expansion.source].expect("bound endpoint");
                let after = &mut frame.traversal.after[depth];
                let mut admitted = None;
                let next = source
                    .next_incident_candidate(
                        from,
                        expansion.relation,
                        expansion.direction,
                        *after,
                        &mut |event| admit(meter, after, &mut admitted, event),
                    )
                    .await;
                let candidate = match next {
                    Ok(candidate) => candidate,
                    Err(EdgeExpansionSourceError::Read(error)) => flatten(Err(error))?,
                    Err(EdgeExpansionSourceError::Unavailable) => {
                        return Err(GqlQueryError::Source(EdgeScanError::ExpansionUnavailable));
                    }
                };
                check_admission(&candidate, admitted)?;
                let Some(candidate) = candidate else {
                    frame.pop();
                    continue;
                };
                let Some(record) = candidate.record else {
                    continue;
                };
                meter.event(GlaExecutionEvent::Work)?;
                let edge = record.edge();
                if !expansion.relation.matches(edge.relation) {
                    continue;
                }
                // An index may include retired/nonincident historical members.
                // Rechecking cannot invent an orientation or resurrect an edge.
                let to = match expansion.direction {
                    GlaDirection::Forward if edge.source == from => edge.target,
                    GlaDirection::Reverse if edge.target == from => edge.source,
                    GlaDirection::Undirected if edge.source == from => edge.target,
                    GlaDirection::Undirected if edge.target == from => edge.source,
                    _ => continue,
                };
                (candidate.eid, record, from, to)
            };
            for vid in [from, to] {
                meter.event(GlaExecutionEvent::Work)?;
                if record.vertex(vid).is_none() {
                    return Err(GqlQueryError::Source(EdgeScanError::DanglingEndpoint));
                }
            }
            if depth == 0 {
                frame.traversal.bindings.push(Some(from));
            }
            frame.traversal.choices.push(Choice { eid, target: to });
            frame.traversal.bindings.push(Some(to));
            frame.records.push(record);
            let local = FrameSource::<_, S::Error> {
                records: &frame.records,
                choices: &frame.traversal.choices,
                seq: self.seq,
                error: core::marker::PhantomData,
            };
            let mut control = |event| {
                meter.event(event)?;
                source
                    .evaluation_event(&frame.records[depth], event)
                    .map_err(|error| GqlQueryError::Source(EdgeScanError::Source(error)))
            };
            let paths = test_stage(
                &plan.stages[depth],
                &frame.traversal,
                &local,
                &mut control,
                &mut || Err(GqlQueryError::Source(EdgeScanError::ExpansionUnavailable)),
            )?;
            let Some(paths) = paths else {
                frame.pop();
                continue;
            };
            if depth + 1 < hops {
                frame.traversal.after[depth + 1] = None;
                continue;
            }
            if self.skip != 0 {
                self.skip -= 1;
                // Path allocations are charged to the record popped below.
                drop(paths);
                frame.pop();
                continue;
            }
            let count = if EMIT {
                Some(meter.increment(GqlBudgetDimension::ResultRows, meter.rows.result_rows)?)
            } else {
                None
            };
            let guard = flatten(source.reserve_join_output(
                &frame.records,
                self.plan.columns,
                &mut |event| meter.event(event),
            ))?;
            let row = project(
                &self.plan.inner.projection,
                &frame.traversal.bindings,
                &paths,
                &local,
                &mut |event| {
                    meter.event(event)?;
                    source
                        .evaluation_event(&frame.records[depth], event)
                        .map_err(|error| GqlQueryError::Source(EdgeScanError::Source(error)))
                },
            )?;
            if let Some(count) = count {
                meter.event(GlaExecutionEvent::ResultRow)?;
                meter.rows.result_rows = count;
            } else {
                // A complete private occurrence is work, not a delivered
                // aggregate group. The host separately bounds input rows.
                meter.event(GlaExecutionEvent::Work)?;
            }
            frame.traversal.resume = true;
            return Ok(Some(AsyncEdgeJoinOutput { row, guard }));
        }
    }

    fn fail(&mut self) {
        self.state = EdgeScanState::Failed;
        self.close();
    }

    // The public aggregate adapter seals this definition together with the
    // physical plan. Keep the source AND every ancestor record owned by this
    // call until computed input and domain validation have both succeeded.
    // This shares traversal, projection and native input evaluation; there is
    // no second join driver or reducer, and no input-sized table is retained.
    pub(crate) async fn next_aggregate_input<C>(
        &mut self,
        definition: &SpillAggregateDefinition,
        output_event: &mut (
                 impl FnMut(&mut S::OutputGuard, VertexScanEvent) -> Result<(), S::Error> + Send
             ),
    ) -> Result<Option<AsyncEdgeJoinOutput<S::OutputGuard>>, EdgeAggregateError<S::Error, C>>
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        let Some(output) = self.next_inner::<false, C>().await else {
            return Ok(None);
        };
        let (row, mut guard) = output.map_err(lift)?.into_parts();
        self.state = EdgeScanState::Failed;
        let source = self.source.take();
        let frame = self.frame.take();
        let result = (|| {
            let meter = &mut self.meter;
            let mut control = |event| {
                meter.event(value_event(event)).map_err(lift)?;
                output_event(&mut guard, event).map_err(|error| {
                    GqlQueryError::Source(GraphAggregateError::Source(EdgeScanError::Source(error)))
                })
            };
            let row = definition.aggregate.evaluate_streamed_input(row, &mut |event| {
                control(match event {
                    GlaExecutionEvent::ScratchEntry => VertexScanEvent::ScratchEntry,
                    GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => VertexScanEvent::Work,
                })
            })?;
            definition.validate_input(&row, &mut control)?;
            Ok(AsyncEdgeJoinOutput { row, guard })
        })();
        match result {
            Ok(row) => {
                self.source = source;
                self.frame = frame;
                self.state = EdgeScanState::Open;
                Ok(Some(row))
            }
            Err(error) => {
                self.fail();
                Err(error)
            }
        }
    }

    pub(crate) fn charge_aggregate<E, C>(
        &mut self,
        event: VertexScanEvent,
    ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        let result = self.meter.control(value_event(event))
            .map_err(|error| error.map_source(|never| match never {}));
        if result.is_err() {
            self.fail();
        }
        result
    }

    pub(crate) fn finish_aggregate_result<E, C>(
        &mut self,
    ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        if self.state != EdgeScanState::Exhausted {
            self.fail();
            return Err(GqlQueryError::Source(GraphAggregateError::InvalidReductionInput));
        }
        let result = (|| {
            let count = self.meter.rows.result_rows.checked_add(1)
                .ok_or(GqlQueryError::Source(GraphAggregateError::ResultCountOverflow))?;
            self.meter.policy.rows.check(GqlBudgetDimension::ResultRows, count)
                .map_err(GqlQueryError::Rows)?;
            self.meter.control(GlaExecutionEvent::ResultRow)
                .map_err(|error| error.map_source(|never| match never {}))?;
            self.meter.rows.result_rows = count;
            Ok(())
        })();
        if result.is_err() {
            self.fail();
        }
        result
    }
}
impl<S: AsyncEdgeJoinSource, F> core::fmt::Debug for AsyncEdgeJoinCursor<S, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AsyncEdgeJoinCursor")
            .field("snapshot", &self.seq)
            .field("state", &self.state)
            .field("rows", &self.meter.rows)
            .field("evaluator", &self.meter.evaluator)
            .field("source_and_plan", &"[REDACTED]")
            .finish()
    }
}

fn admit<F: FnMut() -> Result<(), C>, E, C>(
    meter: &mut Meter<F>,
    after: &mut Option<EId>,
    admitted: &mut Option<EId>,
    event: AsyncEdgeScanEvent,
) -> ScanResult<(), E, C> {
    match event {
        AsyncEdgeScanEvent::Work => meter.event(GlaExecutionEvent::Work),
        AsyncEdgeScanEvent::ScratchEntry => meter.event(GlaExecutionEvent::ScratchEntry),
        AsyncEdgeScanEvent::Candidate(eid) => {
            meter.event(GlaExecutionEvent::Work)?;
            if admitted.is_some() {
                return Err(GqlQueryError::Source(
                    EdgeScanError::InvalidCandidateAdmission,
                ));
            }
            if after.is_some_and(|prior| eid <= prior) {
                return Err(GqlQueryError::Source(EdgeScanError::NonIncreasingIdentity));
            }
            let count = meter.increment(
                GqlBudgetDimension::SnapshotRecords,
                meter.rows.snapshot_records,
            )?;
            meter.rows.snapshot_records = count;
            *after = Some(eid);
            *admitted = Some(eid);
            Ok(())
        }
    }
}
fn check_admission<R, E, C>(
    candidate: &Option<AsyncEdgeCandidate<R>>,
    admitted: Option<EId>,
) -> ScanResult<(), E, C> {
    if candidate.as_ref().map(|value| value.eid) != admitted {
        return Err(GqlQueryError::Source(
            EdgeScanError::InvalidCandidateAdmission,
        ));
    }
    Ok(())
}

// A loan of the CURRENT binding records into the existing synchronous stage
// kernel, not a graph or another source engine. No I/O, cloned field table or
// unobserved graph lookup occurs here. Probes are excluded at compilation and
// every nested-source method retains EdgeScanSource's Unavailable default.
struct FrameSource<'a, R, E> {
    records: &'a [R],
    choices: &'a [Choice],
    seq: CommitSeq,
    error: core::marker::PhantomData<E>,
}
impl<R: AsyncEdgeScanRecord, E> EdgeScanSource for FrameSource<'_, R, E> {
    type Error = E;
    fn snapshot_seq(&self) -> CommitSeq {
        self.seq
    }
    fn next_edge<C>(
        &mut self,
        _control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<E, C>> {
        unreachable!("a borrowed binding is never driven as a graph scan")
    }
    fn edge<'a, C>(
        &'a self,
        eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<E, C>> {
        for (choice, record) in self.choices.iter().zip(self.records) {
            control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
            if choice.eid == eid {
                return Ok(Some(record.edge()));
            }
        }
        Ok(None)
    }
    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, EdgeScanSourceError<E, C>> {
        for record in self.records {
            control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
            let edge = record.edge();
            if vid == edge.source || vid == edge.target {
                return Ok(record.vertex(vid));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests;
