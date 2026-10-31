//! Awaitable, occurrence-preserving bounded paths for a blocking consumer.
//!
//! The cursor keeps one route and its admitted records. It never coalesces
//! endpoints: two parallel edges or two routes are two input occurrences.
//! Complete rows enter the ordinary external sort before DISTINCT or paging.

use super::*;
use crate::GraphWalkBounds;
use crate::algebra::{EdgeRelation, GraphWalkSearch};
use crate::stream::{AsyncVertexCandidate, AsyncVertexScanEvent, AsyncVertexScanRecord};

/// One immutable vertex-root and indexed-incidence source. Root candidates
/// obey the same exactly-once admission and strict identity ordering as
/// AsyncVertexScanSource. Incidences obey AsyncEdgeJoinSource, independently of
/// the root position, at the same cut and with the same visibility policy.
/// This interface grants no authority and never authorizes an eager fallback.
pub trait AsyncGraphWalkSource: AsyncEdgeJoinSource {
    type RootRecord: AsyncVertexScanRecord + Send;

    fn next_root_candidate<C: Send>(
        &mut self,
        control: &mut (impl FnMut(AsyncVertexScanEvent) -> Result<(), C> + Send),
    ) -> impl core::future::Future<
        Output = Result<
            Option<AsyncVertexCandidate<Self::RootRecord>>,
            EdgeScanSourceError<Self::Error, C>,
        >,
    > + Send;

    /// Admit root-local evaluator temporaries, including a zero-hop capture,
    /// before allocation. Other path temporaries use the last active edge's
    /// existing evaluation_event seam and cannot outlive that record.
    fn root_evaluation_event(
        &self,
        root: &Self::RootRecord,
        event: GlaExecutionEvent,
    ) -> Result<(), Self::Error>;

    /// Admit every copied output cell before native projection, including root
    /// properties, repeated fields and the complete path/edge/node-list frame.
    /// The guard must survive all source records. Empty records means a real
    /// zero-hop path, not an absent root. Explicit () guards claim no byte cap.
    fn reserve_walk_output<C>(
        &self,
        root: &Self::RootRecord,
        records: &[Self::Record],
        columns: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Self::OutputGuard, EdgeScanSourceError<Self::Error, C>>;
}

/// A single finite variable-length atom rooted at an admitted vertex scan.
/// WALK, TRAIL, ACYCLIC and SIMPLE retain their native occurrence semantics.
/// Zero hops include isolates; terminal endpoint filters never prune transit.
/// Captures expose real full-width identities, never reconstructed edge IDs.
///
/// This is a private input contract for a blocking consumer. There is no
/// canonical-order claim about DFS output. The returned ScanSortTail must be
/// applied once after complete intake, even for LIMIT zero. Shortest selectors,
/// unbounded intervals, further atoms, OPTIONAL/probes and per-edge predicates
/// refuse during preparation, before any source or budget is observed.
#[derive(Clone)]
pub struct AsyncGraphWalkPlan {
    relation: EdgeRelation,
    direction: GlaDirection,
    bounds: GraphWalkBounds,
    search: GraphWalkSearch,
    different_edges: bool,
    prefix: Arc<[GlaOperator]>,
    suffix: Arc<[GlaOperator]>,
    projection: Arc<GlaOperator>,
    columns: usize,
}

impl AsyncGraphWalkPlan {
    pub fn compile_sort_input(
        plan: &GlaPlan<GraphValueRow>,
    ) -> Result<(Self, crate::scan_stream::ScanSortTail), EdgeScanBuildError> {
        let tail = crate::scan_stream::ScanSortTail::compile(plan)
            .map_err(|operator| EdgeScanBuildError { operator })?;
        let ops = plan.operators();
        if !matches!(ops.first(), Some(GlaOperator::ScanVertices)) {
            return Err(EdgeScanBuildError { operator: 0 });
        }
        let mut at = 1;
        while let Some(op) = ops.get(at) {
            if matches!(op, GlaOperator::VarLengthExpand { .. }) {
                break;
            }
            if !predicate_valid(op, 1, false) {
                return Err(EdgeScanBuildError { operator: at });
            }
            at += 1;
        }
        let Some(GlaOperator::VarLengthExpand {
            source,
            relation,
            direction,
            bounds,
            search,
            edge_predicates,
        }) = ops.get(at)
        else {
            return Err(EdgeScanBuildError { operator: at });
        };
        if source.ordinal() != 0
            || !bounds.is_bounded()
            || !edge_predicates.is_empty()
            || !matches!(
                search,
                GraphWalkSearch::All
                    | GraphWalkSearch::Trail
                    | GraphWalkSearch::Acyclic
                    | GraphWalkSearch::Simple
            )
        {
            return Err(EdgeScanBuildError { operator: at });
        }
        let expansion = at;
        let single_edge = bounds.minimum() == 1 && bounds.maximum() == 1;
        let mut captures = 0;
        let mut different_edges = false;
        at += 1;
        loop {
            let bad = || EdgeScanBuildError { operator: at };
            let Some(op) = ops.get(at) else {
                return Err(bad());
            };
            match op {
                GlaOperator::CapturePath {
                    capture,
                    start,
                    segments,
                } if *capture as usize == captures
                    && captures < crate::algebra::MAX_PATTERN_IDENTITIES
                    && start.ordinal() == 0
                    && matches!(segments.as_slice(), [segment] if segment.ordinal() == 1) =>
                {
                    captures += 1;
                }
                GlaOperator::DifferentEdges { segments } if matches!(segments.as_slice(), [segment] if segment.ordinal() == 1) =>
                {
                    different_edges = true;
                }
                GlaOperator::SelectPathLength { capture, .. }
                | GlaOperator::SelectPathNull { capture, .. }
                    if (*capture as usize) < captures => {}
                GlaOperator::ProjectValues { columns } => {
                    for column in columns {
                        let valid = match column {
                            ValueProjection::Vertex { slot }
                            | ValueProjection::Property { slot, .. } => slot.ordinal() < 2,
                            ValueProjection::EdgeProperty { capture, .. } => {
                                single_edge && (*capture as usize) < captures
                            }
                            ValueProjection::Path { capture, function } => {
                                (*capture as usize) < captures
                                    && match function {
                                        GraphPathFunction::Value
                                        | GraphPathFunction::Length
                                        | GraphPathFunction::Nodes
                                        | GraphPathFunction::Edges => true,
                                        GraphPathFunction::Edge => single_edge,
                                        _ => false,
                                    }
                            }
                            _ => false,
                        };
                        if !valid {
                            return Err(bad());
                        }
                    }
                    break;
                }
                op if predicate_valid(op, 2, single_edge && captures > 0) => {
                    // Boolean edge operands use capture ordinals, not vertex
                    // slots. A multi-edge path is never treated as one edge.
                    if let GlaOperator::SelectBoolean { expression } = op {
                        let mut valid = true;
                        let _ = expression.remap_elements(
                            |slot| slot,
                            |capture| {
                                valid &= (capture as usize) < captures;
                                capture
                            },
                        );
                        if !valid {
                            return Err(bad());
                        }
                    }
                }
                _ => return Err(bad()),
            }
            at += 1;
        }
        let projection = at;
        at += 1;
        if matches!(ops.get(at), Some(GlaOperator::Distinct)) {
            at += 1;
        }
        if !matches!(
            ops.get(at),
            Some(GlaOperator::OrderByValues | GlaOperator::OrderByValueColumns { .. })
        ) || !matches!(ops.get(at + 1), Some(GlaOperator::Limit { .. }))
            || at + 2 != ops.len()
        {
            return Err(EdgeScanBuildError { operator: at });
        }
        Ok((
            Self {
                relation: *relation,
                direction: *direction,
                bounds: *bounds,
                search: *search,
                different_edges,
                prefix: Arc::from(&ops[1..expansion]),
                suffix: Arc::from(&ops[expansion + 1..projection]),
                projection: Arc::new(ops[projection].clone()),
                columns: tail.evaluation_width(),
            },
            tail,
        ))
    }

    pub fn relation(&self) -> EdgeRelation {
        self.relation
    }
    pub fn maximum_hops(&self) -> usize {
        self.bounds.maximum() as usize
    }
}

fn predicate_valid(op: &GlaOperator, width: u32, edge_capture: bool) -> bool {
    match op {
        GlaOperator::Select { slot, .. } => slot.ordinal() < width,
        GlaOperator::VertexIdentity { left, right, .. }
        | GlaOperator::CompareProperties { left, right, .. } => {
            left.ordinal() < width && right.ordinal() < width
        }
        GlaOperator::SelectBoolean { expression } => {
            let mut vertices_valid = true;
            let mut edges_valid = true;
            let _ = expression.remap_elements(
                |slot| {
                    vertices_valid &= slot.ordinal() < width;
                    slot
                },
                |capture| {
                    edges_valid &= edge_capture;
                    capture
                },
            );
            vertices_valid && edges_valid
        }
        _ => false,
    }
}

impl core::fmt::Debug for AsyncGraphWalkPlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AsyncGraphWalkPlan([REDACTED])")
    }
}

pub struct AsyncGraphWalkOutput<Guard> {
    row: GraphValueRow,
    guard: Guard,
}
impl<Guard> AsyncGraphWalkOutput<Guard> {
    pub fn row(&self) -> &GraphValueRow {
        &self.row
    }
    pub fn into_parts(self) -> (GraphValueRow, Guard) {
        (self.row, self.guard)
    }
}
impl<Guard> AsRef<GraphValueRow> for AsyncGraphWalkOutput<Guard> {
    fn as_ref(&self) -> &GraphValueRow {
        &self.row
    }
}
impl<Guard> core::ops::Deref for AsyncGraphWalkOutput<Guard> {
    type Target = GraphValueRow;
    fn deref(&self) -> &Self::Target {
        &self.row
    }
}
impl<Guard> core::fmt::Debug for AsyncGraphWalkOutput<Guard> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AsyncGraphWalkOutput([REDACTED])")
    }
}

struct Position {
    vertex: VId,
    after: Option<EId>,
    offered: bool,
}

struct Frame<Root, Record, Guard> {
    root: Root,
    start: VId,
    records: Vec<Record>,
    steps: Vec<(EId, VId)>,
    positions: Vec<Position>,
    // Root, records and all vectors drop before their backing reservation.
    _guard: Guard,
}
impl<Root, Record, Guard> Frame<Root, Record, Guard> {
    fn pop(&mut self) {
        self.positions.pop();
        self.records.pop();
        self.steps.pop();
    }
}

/// Awaitable bounded path occurrences, never a complete ordered query result.
/// Source, evaluator, candidates and emitted input occurrences share one
/// cumulative policy. The retained route has one root and at most maximum_hops
/// edge records; one candidate may coexist while it is checked or rejected.
/// Source payloads and output guards have independent host-owned admissions.
///
/// A polled pending pull owns the entire source and route. Cancellation, error,
/// close or drop releases them without draining; subsequent pulls are inert.
/// Earlier occurrences do not prove success: the blocking owner requires EOF.
pub struct AsyncGraphWalkCursor<S: AsyncGraphWalkSource, F> {
    source: Option<S>,
    frame: Option<Frame<S::RootRecord, S::Record, S::TraversalGuard>>,
    plan: AsyncGraphWalkPlan,
    meter: Meter<F>,
    seq: CommitSeq,
    last_root: Option<VId>,
    state: EdgeScanState,
}

impl<S: AsyncGraphWalkSource, F> AsyncGraphWalkCursor<S, F> {
    pub fn new(source: S, plan: AsyncGraphWalkPlan, policy: GqlQueryPolicy, checkpoint: F) -> Self {
        Self {
            seq: source.snapshot_seq(),
            source: Some(source),
            frame: None,
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
            last_root: None,
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
    ) -> Option<ScanResult<AsyncGraphWalkOutput<S::OutputGuard>, S::Error, C>>
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        if self.state != EdgeScanState::Open {
            return None;
        }
        self.state = EdgeScanState::Failed;
        let mut source = self.source.take().expect("open cursor owns source");
        let mut frame = self.frame.take();
        match self.advance(&mut source, &mut frame).await {
            Ok(Some(output)) => {
                self.source = Some(source);
                self.frame = frame;
                self.state = EdgeScanState::Open;
                Some(Ok(output))
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
        frame: &mut Option<Frame<S::RootRecord, S::Record, S::TraversalGuard>>,
    ) -> ScanResult<Option<AsyncGraphWalkOutput<S::OutputGuard>>, S::Error, C>
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        let meter = &mut self.meter;
        loop {
            meter.event(GlaExecutionEvent::Work)?;
            if frame.is_none() {
                let mut admitted = None;
                let candidate = flatten(
                    source
                        .next_root_candidate(&mut |event| {
                            admit_root(meter, &mut self.last_root, &mut admitted, event)
                        })
                        .await,
                )?;
                if candidate.as_ref().map(|row| row.vid) != admitted {
                    return Err(GqlQueryError::Source(
                        EdgeScanError::InvalidCandidateAdmission,
                    ));
                }
                let Some(candidate) = candidate else {
                    return Ok(None);
                };
                let Some(root) = candidate.record else {
                    continue;
                };
                let start = candidate.vid;
                let local = FrameSource::<_, S::Record, S::Error> {
                    root: &root,
                    start,
                    records: &[],
                    steps: &[],
                    seq: self.seq,
                    error: core::marker::PhantomData,
                };
                let mut keep = true;
                for op in self.plan.prefix.iter() {
                    let mut control = |event| {
                        meter.event(event)?;
                        source
                            .root_evaluation_event(&root, event)
                            .map_err(|error| GqlQueryError::Source(EdgeScanError::Source(error)))
                    };
                    control(GlaExecutionEvent::Work)?;
                    if !join::accepts(op, &[Some(start)], &[], &local, &mut control)? {
                        keep = false;
                        break;
                    }
                }
                if !keep || self.plan.bounds.minimum() > self.plan.bounds.maximum() {
                    continue;
                }
                let hops = self.plan.maximum_hops();
                let guard =
                    flatten(source.reserve_traversal(hops + 1, &mut |event| meter.event(event)))?;
                // Pre-admit all vector backing. Later pushes cannot grow past
                // these capacities and are still logically charged before use.
                for _ in 0..hops * 4 + 1 {
                    meter.event(GlaExecutionEvent::ScratchEntry)?;
                }
                let mut positions = Vec::with_capacity(hops + 1);
                positions.push(Position {
                    vertex: start,
                    after: None,
                    offered: false,
                });
                *frame = Some(Frame {
                    root,
                    start,
                    records: Vec::with_capacity(hops),
                    steps: Vec::with_capacity(hops),
                    positions,
                    _guard: guard,
                });
            }
            let active = frame.as_mut().expect("admitted root");
            let depth = active.steps.len();
            let position = active.positions.last_mut().expect("active route");
            let endpoint = position.vertex;
            if !position.offered {
                position.offered = true;
                if depth >= self.plan.bounds.minimum() as usize {
                    let local = FrameSource::<_, _, S::Error> {
                        root: &active.root,
                        start: active.start,
                        records: &active.records,
                        steps: &active.steps,
                        seq: self.seq,
                        error: core::marker::PhantomData,
                    };
                    let ids = [Some(active.start), Some(endpoint)];
                    let mut paths = Vec::new();
                    let mut keep = true;
                    {
                        let mut control = |event| observe(source, active, meter, event);
                        for op in self.plan.suffix.iter() {
                            control(GlaExecutionEvent::Work)?;
                            match op {
                                GlaOperator::CapturePath { .. } => {
                                    control(GlaExecutionEvent::ScratchEntry)?;
                                    for _ in &active.steps {
                                        control(GlaExecutionEvent::ScratchEntry)?;
                                        control(GlaExecutionEvent::ScratchEntry)?;
                                    }
                                    let mut steps = Vec::with_capacity(active.steps.len());
                                    steps.extend_from_slice(&active.steps);
                                    paths.push(Some(GraphPath::new(
                                        active.start,
                                        steps.into_boxed_slice(),
                                    )));
                                }
                                // The single atom's EId law participates in
                                // traversal, just as native identified expansion.
                                GlaOperator::DifferentEdges { .. } => {}
                                GlaOperator::SelectPathLength {
                                    capture,
                                    comparison,
                                    value,
                                } => {
                                    if !paths[*capture as usize].as_ref().is_some_and(|path| {
                                        comparison.accepts(path.len() as i64, *value)
                                    }) {
                                        keep = false;
                                        break;
                                    }
                                }
                                GlaOperator::SelectPathNull {
                                    capture, is_null, ..
                                } => {
                                    if paths[*capture as usize].is_none() != *is_null {
                                        keep = false;
                                        break;
                                    }
                                }
                                op => {
                                    if !join::accepts(op, &ids, &paths, &local, &mut control)? {
                                        keep = false;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    if keep {
                        let count = meter
                            .increment(GqlBudgetDimension::ResultRows, meter.rows.result_rows)?;
                        let guard = flatten(source.reserve_walk_output(
                            &active.root,
                            &active.records,
                            self.plan.columns,
                            &mut |event| meter.event(event),
                        ))?;
                        let row = join::project(
                            &self.plan.projection,
                            &ids,
                            &paths,
                            &local,
                            &mut |event| observe(source, active, meter, event),
                        )?;
                        meter.event(GlaExecutionEvent::ResultRow)?;
                        meter.rows.result_rows = count;
                        return Ok(Some(AsyncGraphWalkOutput { row, guard }));
                    }
                    // A rejected endpoint may still lead to an accepted
                    // longer path. Do not pop or prune this route here.
                }
            }
            if depth == self.plan.maximum_hops()
                || (self.plan.search == GraphWalkSearch::Simple
                    && depth > 0
                    && endpoint == active.start)
            {
                active.pop();
                if active.positions.is_empty() {
                    *frame = None;
                }
                continue;
            }
            let mut after = active.positions.last().expect("active route").after;
            let mut admitted = None;
            let next = source
                .next_incident_candidate(
                    endpoint,
                    self.plan.relation,
                    self.plan.direction,
                    after,
                    &mut |event| admit_edge(meter, &mut after, &mut admitted, event),
                )
                .await;
            let candidate = match next {
                Ok(candidate) => candidate,
                Err(EdgeExpansionSourceError::Read(error)) => flatten(Err(error))?,
                Err(EdgeExpansionSourceError::Unavailable) => {
                    return Err(GqlQueryError::Source(EdgeScanError::ExpansionUnavailable));
                }
            };
            if candidate.as_ref().map(|row| row.eid) != admitted {
                return Err(GqlQueryError::Source(
                    EdgeScanError::InvalidCandidateAdmission,
                ));
            }
            active.positions.last_mut().expect("active route").after = after;
            let Some(candidate) = candidate else {
                active.pop();
                if active.positions.is_empty() {
                    *frame = None;
                }
                continue;
            };
            let Some(record) = candidate.record else {
                continue;
            };
            meter.event(GlaExecutionEvent::Work)?;
            let edge = record.edge();
            if !self.plan.relation.matches(edge.relation) {
                continue;
            }
            let target = match self.plan.direction {
                GlaDirection::Forward if edge.source == endpoint => edge.target,
                GlaDirection::Reverse if edge.target == endpoint => edge.source,
                GlaDirection::Undirected if edge.source == endpoint => edge.target,
                GlaDirection::Undirected if edge.target == endpoint => edge.source,
                _ => continue,
            };
            for vertex in [endpoint, target] {
                meter.event(GlaExecutionEvent::Work)?;
                if record.vertex(vertex).is_none() {
                    return Err(GqlQueryError::Source(EdgeScanError::DanglingEndpoint));
                }
            }
            let mut keep = true;
            if self.plan.different_edges || self.plan.search == GraphWalkSearch::Trail {
                for &(eid, _) in &active.steps {
                    meter.event(GlaExecutionEvent::Work)?;
                    if eid == candidate.eid {
                        keep = false;
                        break;
                    }
                }
            }
            if keep
                && matches!(
                    self.plan.search,
                    GraphWalkSearch::Acyclic | GraphWalkSearch::Simple
                )
            {
                for prior in &active.positions {
                    meter.event(GlaExecutionEvent::Work)?;
                    if prior.vertex == target {
                        keep =
                            self.plan.search == GraphWalkSearch::Simple && target == active.start;
                        break;
                    }
                }
            }
            if !keep {
                continue;
            }
            for _ in 0..3 {
                meter.event(GlaExecutionEvent::ScratchEntry)?;
            }
            active.steps.push((candidate.eid, target));
            active.records.push(record);
            active.positions.push(Position {
                vertex: target,
                after: None,
                offered: false,
            });
        }
    }
}

fn observe<S: AsyncGraphWalkSource, F: FnMut() -> Result<(), C>, C>(
    source: &S,
    frame: &Frame<S::RootRecord, S::Record, S::TraversalGuard>,
    meter: &mut Meter<F>,
    event: GlaExecutionEvent,
) -> ScanResult<(), S::Error, C> {
    meter.event(event)?;
    let result = match frame.records.last() {
        Some(record) => source.evaluation_event(record, event),
        None => source.root_evaluation_event(&frame.root, event),
    };
    result.map_err(|error| GqlQueryError::Source(EdgeScanError::Source(error)))
}

fn admit_root<F: FnMut() -> Result<(), C>, E, C>(
    meter: &mut Meter<F>,
    after: &mut Option<VId>,
    admitted: &mut Option<VId>,
    event: AsyncVertexScanEvent,
) -> ScanResult<(), E, C> {
    match event {
        AsyncVertexScanEvent::Work => meter.event(GlaExecutionEvent::Work),
        AsyncVertexScanEvent::ScratchEntry => meter.event(GlaExecutionEvent::ScratchEntry),
        AsyncVertexScanEvent::Candidate(vid) => {
            meter.event(GlaExecutionEvent::Work)?;
            if admitted.is_some() {
                return Err(GqlQueryError::Source(
                    EdgeScanError::InvalidCandidateAdmission,
                ));
            }
            if after.is_some_and(|prior| vid <= prior) {
                return Err(GqlQueryError::Source(EdgeScanError::NonIncreasingIdentity));
            }
            meter.rows.snapshot_records = meter.increment(
                GqlBudgetDimension::SnapshotRecords,
                meter.rows.snapshot_records,
            )?;
            *after = Some(vid);
            *admitted = Some(vid);
            Ok(())
        }
    }
}

fn admit_edge<F: FnMut() -> Result<(), C>, E, C>(
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
            meter.rows.snapshot_records = meter.increment(
                GqlBudgetDimension::SnapshotRecords,
                meter.rows.snapshot_records,
            )?;
            *after = Some(eid);
            *admitted = Some(eid);
            Ok(())
        }
    }
}

struct FrameSource<'a, Root, Record, Error> {
    root: &'a Root,
    start: VId,
    records: &'a [Record],
    steps: &'a [(EId, VId)],
    seq: CommitSeq,
    error: core::marker::PhantomData<Error>,
}
impl<Root: AsyncVertexScanRecord, Record: AsyncEdgeScanRecord, Error> EdgeScanSource
    for FrameSource<'_, Root, Record, Error>
{
    type Error = Error;
    fn snapshot_seq(&self) -> CommitSeq {
        self.seq
    }
    fn next_edge<C>(
        &mut self,
        _: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<Error, C>> {
        unreachable!("active path records are never a graph scan")
    }
    fn edge<'a, C>(
        &'a self,
        eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<Error, C>> {
        for (step, record) in self.steps.iter().zip(self.records) {
            control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
            if step.0 == eid {
                return Ok(Some(record.edge()));
            }
        }
        Ok(None)
    }
    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, EdgeScanSourceError<Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        if vid == self.start {
            return Ok(Some(self.root.as_row()));
        }
        for record in self.records.iter().rev() {
            control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
            let edge = record.edge();
            if vid == edge.source || vid == edge.target {
                return Ok(record.vertex(vid));
            }
        }
        Ok(None)
    }
}

impl<S: AsyncGraphWalkSource, F> core::fmt::Debug for AsyncGraphWalkCursor<S, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AsyncGraphWalkCursor")
            .field("snapshot", &self.seq)
            .field("state", &self.state)
            .field("rows", &self.meter.rows)
            .field("evaluator", &self.meter.evaluator)
            .field("source_and_plan", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests;
