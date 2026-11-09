//! Awaitable indexed intake for the existing fixed-hop join compiler. The
//! synchronous stage/capture/projector remains the semantic authority. Only
//! admitted records on the active traversal are retained, never neighbor bags.

use super::*;
use crate::GraphAggregateError;
use crate::algebra::EdgeRelation;
use crate::edge_stream::aggregate::{EdgeAggregateError, lift, value_event};
use crate::spill_aggregate::SpillAggregateDefinition;
use crate::stream::VertexScanEvent;

/// One nested incidence read: the next admitted candidate, or None at EOF.
pub type AsyncIncidentCandidateResult<Record, Error, Control> =
    Result<Option<AsyncEdgeCandidate<Record>>, EdgeExpansionSourceError<Error, Control>>;

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
/// The outer traversal holds at most `hops` records. A running correlated probe
/// separately reserves its definition-bounded binding slots and holds at most
/// one additional record per active expansion. Variable-length probes retain
/// identities and mode history, admitting every growing container through
/// evaluation_event on a live outer record before allocating. reserve_traversal admits fixed
/// cursor vectors (record slots, choices, bindings and successor positions)
/// before allocation and retains its guard until that traversal is dropped.
/// Each record owns its source payload reservation. reserve_join_output admits
/// the full copied projection, including repeated fields/path cells from ANY
/// retained record, before the shared projector allocates. Its output guard
/// outlives every source record. A collected projection additionally forwards
/// every native allocation event to join_output_event before allocation: its
/// variable witness payload cannot be bounded by the active record slice.
/// A terminal OPTIONAL likewise reserves its fixed output with an empty
/// record slice and forwards every projection allocation to join_output_event;
/// its independently owned local records need not be moved into a flat slice.
/// Explicit () guards declare no byte bound.
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
        Output = AsyncIncidentCandidateResult<Self::Record, Self::Error, C>,
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

    /// Admit native projection temporaries and copied collection cells against
    /// the independent output owner, before each allocation. ScratchEntry
    /// covers a value cell or GRAPH_VALUE_PAYLOAD_UNIT_BYTES payload quantum;
    /// the host must also cover vector growth/temporary copies as its ordinary
    /// evaluation_event contract does. The guard retains this charge through
    /// delivery, including after every source/traversal record is released.
    /// Work remains a cancellation observation, not a second logical debit.
    fn join_output_event(
        &self,
        guard: &mut Self::OutputGuard,
        event: GlaExecutionEvent,
    ) -> Result<(), Self::Error>;
}

/// The ordinary fixed-hop compiler with awaitable correlated semi/anti probes.
/// Its identity prefix proves canonical order and DISTINCT without a seen set.
/// Probe bindings copy outer values or expand indexed fixed/variable-length
/// edges through the same native endpoint-support semantics. Correlated fixed
/// collections exhaust the same witness driver, retaining native identity,
/// path and tuple elements under their outer record's allocation owner.
/// Prepare these scopes with GraphPatternBuilder::prepare_values_with_clauses
/// and GraphMatchClause::collect/collect_tuple; each collection is complete
/// before its containing row can be delivered.
/// Independent vertex scans, variable collection atoms, nested collection
/// probes, OPTIONAL and returned variable-length paths remain typed refusals
/// for ordinary ordered streams. External-sort input additionally admits one
/// terminal correlated fixed-hop OPTIONAL and retains its resumable local
/// witness frame, without materializing the nullable relation;
/// missing indexes and interrupted reads never establish existential absence.
#[derive(Clone)]
pub struct AsyncEdgeJoinPlan {
    inner: EdgeScanPlan,
    columns: usize,
    optional: Option<Arc<probe::Optional>>,
}
impl AsyncEdgeJoinPlan {
    /// The checked root selector. Nested expansions and probes retain their
    /// independent selectors and must not inherit this source restriction.
    pub fn root_relation(&self) -> EdgeRelation {
        self.inner.relation
    }

    /// Direction of the first edge binding slot zero, independent of every
    /// nested expansion's own direction and relation selector.
    pub fn root_direction(&self) -> GlaDirection {
        self.inner.direction
    }

    pub fn compile(plan: &GlaPlan<GraphValueRow>) -> Result<Self, EdgeScanBuildError> {
        Self::check_access(plan)?;
        Ok(Self::from_inner(compile_output(plan, Output::OrderedRows)?))
    }

    // Only the sealed external-aggregate definition may relax the identity
    // prefix. Reuse the ordinary aggregate input proof: no child DISTINCT,
    // window, hidden keys or unsupported expansion can be discarded. Supported
    // probes run on the complete input binding before aggregate projection.
    pub(crate) fn compile_aggregate(
        plan: &GlaPlan<GraphValueRow>,
    ) -> Result<Self, EdgeScanBuildError> {
        Self::check_access(plan)?;
        Ok(Self::from_inner(compile_output(
            plan,
            Output::AggregateInput,
        )?))
    }

    /// ALL occurrences for a blocking consumer. The returned native tail must
    /// be applied exactly once AFTER sorting, including DISTINCT and hidden
    /// key removal. Final SKIP/LIMIT never suppresses input errors in this lane.
    /// ResultRows counts input occurrences, not the consumer's final page.
    /// One terminal correlated OPTIONAL may copy outer bindings and expand
    /// fixed edges through native predicates and path captures. Each complete
    /// witness contributes once; successful exhaustion contributes one NULL
    /// extension only when no witness existed. Independent roots, multiple or
    /// nested optional scopes, variable paths and post-scope predicates still
    /// refuse before source construction. The complete external tail remains
    /// responsible for ordering, DISTINCT, hidden keys and final pagination.
    pub fn compile_sort_input(
        plan: &GlaPlan<GraphValueRow>,
    ) -> Result<(Self, crate::scan_stream::ScanSortTail), EdgeScanBuildError> {
        Self::check_access(plan)?;
        if let Some(start) = plan
            .operators()
            .iter()
            .position(|op| matches!(op, GlaOperator::Optional { .. }))
        {
            return Self::compile_optional_sort_input(plan, start);
        }
        let (inner, tail) = EdgeScanPlan::compile_sort_input(plan)?;
        Ok((Self::from_inner(inner), tail))
    }

    fn compile_optional_sort_input(
        plan: &GlaPlan<GraphValueRow>,
        start: usize,
    ) -> Result<(Self, crate::scan_stream::ScanSortTail), EdgeScanBuildError> {
        let ops = plan.operators();
        let tail = crate::scan_stream::ScanSortTail::compile(plan)
            .map_err(|operator| EdgeScanBuildError { operator })?;
        let prefix = &ops[..start];
        let width = 2 + prefix
            .iter()
            .filter(|op| matches!(op, GlaOperator::Expand { .. }))
            .count();
        let captures: Vec<_> = prefix
            .iter()
            .filter_map(|op| match op {
                GlaOperator::CapturePath { segments, .. } => Some(segments.len() == 1),
                _ => None,
            })
            .collect();
        // The positive compiler below validates the complete prefix. Private
        // probe/collection frames cannot stand for exported outer slots.
        if prefix
            .iter()
            .any(|op| matches!(op, GlaOperator::Probe { .. } | GlaOperator::Collect { .. }))
        {
            return Err(EdgeScanBuildError { operator: start });
        }
        let optional = probe::Optional::compile(ops, start, width, &captures)?;
        let columns = optional.columns();
        let mut positive = prefix.to_vec();
        positive.push(GlaOperator::ProjectValues {
            columns: (0..width)
                .map(|slot| ValueProjection::Vertex {
                    slot: crate::algebra::BindingSlot(slot as u32),
                })
                .collect(),
        });
        positive.push(GlaOperator::OrderByValues);
        positive.push(GlaOperator::Limit {
            offset: 0,
            count: None,
        });
        let positive = GlaPlan::from_operators(positive);
        let inner = compile_output(&positive, Output::SortInput)?;
        Ok((
            Self {
                inner,
                columns,
                optional: Some(Arc::new(optional)),
            },
            tail,
        ))
    }

    fn check_access(plan: &GlaPlan<GraphValueRow>) -> Result<(), EdgeScanBuildError> {
        let ops = plan.operators();
        for (operator, op) in ops.iter().enumerate() {
            match op {
                GlaOperator::ScanVertices => return Err(EdgeScanBuildError { operator }),
                GlaOperator::Collect { end, .. } => {
                    let end = *end as usize;
                    let body = ops
                        .get(operator + 1..end)
                        .ok_or(EdgeScanBuildError { operator })?;
                    // One correlated fixed-hop profile, sealed before source
                    // construction even at LIMIT zero. The shared compiler
                    // still validates scope widths, captures and projections.
                    if let Some(offset) = body.iter().position(|op| {
                        matches!(
                            op,
                            GlaOperator::ScanVertices
                                | GlaOperator::VarLengthExpand { .. }
                                | GlaOperator::Probe { .. }
                                | GlaOperator::Collect { .. }
                        )
                    }) {
                        return Err(EdgeScanBuildError {
                            operator: operator + 1 + offset,
                        });
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn from_inner(inner: EdgeScanPlan) -> Self {
        let GlaOperator::ProjectValues { columns } = inner.projection.as_ref() else {
            unreachable!("the fixed-hop compiler proves the projection")
        };
        let columns = columns.len();
        Self {
            inner,
            columns,
            optional: None,
        }
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
    pub(super) row: GraphValueRow,
    pub(super) guard: Guard,
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
    optional: Option<probe::OptionalState<R, Guard>>,
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
                optional: None,
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
            if let Some(optional) = &self.plan.optional
                && let Some(state) = &mut frame.optional
            {
                if let Some(row) = optional
                    .next_async::<S, F, C, EMIT>(
                        state,
                        &frame.traversal,
                        &mut frame.records,
                        source,
                        meter,
                        self.seq,
                        self.plan.columns,
                    )
                    .await?
                {
                    return Ok(Some(row));
                }
                frame.optional = None;
                frame.pop();
                continue;
            }
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
            frame.traversal.choices.push(Choice {
                eid,
                from,
                target: to,
            });
            frame.traversal.bindings.push(Some(to));
            frame.records.push(record);
            let values = test_async_stage(
                &plan.stages[depth],
                &frame.traversal,
                &mut frame.records,
                source,
                meter,
                self.seq,
            )
            .await?;
            let Some(values) = values else {
                frame.pop();
                continue;
            };
            if depth + 1 < hops {
                frame.traversal.after[depth + 1] = None;
                continue;
            }
            if let Some(optional) = &self.plan.optional {
                frame.optional = Some(optional.start_async(
                    &frame.traversal.bindings,
                    values.paths,
                    source,
                    meter,
                )?);
                continue;
            }
            if self.skip != 0 {
                self.skip -= 1;
                // Captures and collection payloads are charged to the record
                // popped below, never to an already released child candidate.
                drop(values);
                frame.pop();
                continue;
            }
            let count = if EMIT {
                Some(meter.increment(GqlBudgetDimension::ResultRows, meter.rows.result_rows)?)
            } else {
                None
            };
            let mut guard = flatten(source.reserve_join_output(
                &frame.records,
                self.plan.columns,
                &mut |event| meter.event(event),
            ))?;
            let local = FrameSource::<_, S::Error> {
                records: &frame.records,
                choices: &frame.traversal.choices,
                seq: self.seq,
                error: core::marker::PhantomData,
            };
            let row = project_collected(
                &self.plan.inner.projection,
                &frame.traversal.bindings,
                &values.paths,
                &values.collected,
                &local,
                &mut |event| {
                    meter.event(event)?;
                    let observed = if values.collected.is_empty() {
                        source.evaluation_event(&frame.records[depth], event)
                    } else {
                        source.join_output_event(&mut guard, event)
                    };
                    observed.map_err(|error| GqlQueryError::Source(EdgeScanError::Source(error)))
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
            let row = self.plan.inner.retain_visible(row);
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
            let row = definition
                .aggregate
                .evaluate_streamed_input(row, &mut |event| {
                    control(match event {
                        GlaExecutionEvent::ScratchEntry => VertexScanEvent::ScratchEntry,
                        GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => {
                            VertexScanEvent::Work
                        }
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
        let result = self
            .meter
            .control(value_event(event))
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
            return Err(GqlQueryError::Source(
                GraphAggregateError::InvalidReductionInput,
            ));
        }
        let result = (|| {
            let count = self
                .meter
                .rows
                .result_rows
                .checked_add(1)
                .ok_or(GqlQueryError::Source(
                    GraphAggregateError::ResultCountOverflow,
                ))?;
            self.meter
                .policy
                .rows
                .check(GqlBudgetDimension::ResultRows, count)
                .map_err(GqlQueryError::Rows)?;
            self.meter
                .control(GlaExecutionEvent::ResultRow)
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

// Await only the probe's source access. Every native instruction still runs
// through the same kernel, in its original order, with prior captures intact.
// Mutable record loans (not shared loans) cross await: cold records may own
// RefCell-backed reservations and need Send, never an artificial Sync bound.
async fn test_async_stage<S: AsyncEdgeJoinSource, F, C>(
    ops: &[Instruction],
    traversal: &Traversal,
    records: &mut [S::Record],
    source: &mut S,
    meter: &mut Meter<F>,
    seq: CommitSeq,
) -> ScanResult<Option<StageValues>, S::Error, C>
where
    F: FnMut() -> Result<(), C> + Send,
    C: Send,
{
    let mut paths = Vec::new();
    let mut collected = Vec::new();
    for instruction in ops {
        meter.event(GlaExecutionEvent::Work)?;
        source
            .evaluation_event(
                records.last().expect("active record"),
                GlaExecutionEvent::Work,
            )
            .map_err(|error| GqlQueryError::Source(EdgeScanError::Source(error)))?;
        match instruction {
            Instruction::Probe(probe) => {
                if !probe
                    .accepts_async(traversal, records, source, meter, seq)
                    .await?
                {
                    return Ok(None);
                }
            }
            Instruction::Collect(collection) => {
                while collected.len() <= collection.group {
                    meter.event(GlaExecutionEvent::ScratchEntry)?;
                    source
                        .evaluation_event(
                            records.last().expect("active collection record"),
                            GlaExecutionEvent::ScratchEntry,
                        )
                        .map_err(|error| GqlQueryError::Source(EdgeScanError::Source(error)))?;
                    collected.push(None);
                }
                collected[collection.group] = Some(
                    collection
                        .evaluate_async(traversal, records, source, meter, seq)
                        .await?,
                );
            }
            Instruction::Native(op) => {
                let local = FrameSource::<_, S::Error> {
                    records,
                    choices: &traversal.choices,
                    seq,
                    error: core::marker::PhantomData,
                };
                if !test_native_stage(op, traversal, &mut paths, &local, &mut |event| {
                    meter.event(event)?;
                    source
                        .evaluation_event(records.last().expect("active record"), event)
                        .map_err(|error| GqlQueryError::Source(EdgeScanError::Source(error)))
                })? {
                    return Ok(None);
                }
            }
        }
    }
    Ok(Some(StageValues { paths, collected }))
}

pub(super) fn admit<F: FnMut() -> Result<(), C>, E, C>(
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

pub(super) fn check_admission<R, E, C>(
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
// unobserved graph lookup occurs here. Probe-local predicate loans include this
// same outer frame; expansion always uses the separate awaitable source.
pub(super) struct FrameSource<'a, R, E> {
    pub(super) records: &'a [R],
    pub(super) choices: &'a [Choice],
    pub(super) seq: CommitSeq,
    pub(super) error: core::marker::PhantomData<E>,
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
