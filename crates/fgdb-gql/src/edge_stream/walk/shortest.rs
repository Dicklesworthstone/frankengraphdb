//! Cold ANY SHORTEST uses breadth-first canonical prefixes, not a path bag.
//!
//! Before the lower bound, dominance is local to (depth, vertex). At and after
//! it, a complete layer settles its endpoints before any expansion. Equal-depth
//! candidates compare real (EId, VId) steps before copying, so parallel edges
//! and tied routes cannot multiply the next frontier. This is the same WALK
//! dominance and canonical route rule as the resident GraphPathCursor.
//!
//! A prefix owns only its path identities and final incoming record. Root and
//! endpoint fields suffice for this admitted single-atom projection; copied
//! paths have an independent output reservation. The two breadth layers and
//! settled identities are bounded query state, with explicit byte admission.

use super::*;
use std::collections::BTreeMap;

pub(super) struct Prefix<Record, Guard> {
    path: GraphPath,
    record: Option<Record>,
    // Path, record and container slots precede their backing reservation.
    _guard: Guard,
}
impl<Record, Guard> Prefix<Record, Guard> {
    fn endpoint(&self) -> VId {
        self.path
            .steps()
            .last()
            .map_or(self.path.start(), |step| step.1)
    }
}

pub(super) struct Breadth<Root, Record, Guard> {
    root: Root,
    start: VId,
    depth: usize,
    frontier: Vec<Prefix<Record, Guard>>,
    frontier_guard: Option<Guard>,
    pending: std::vec::IntoIter<Prefix<Record, Guard>>,
    pending_guard: Option<Guard>,
    settled: BTreeMap<VId, Guard>,
    _guard: Guard,
}

fn overflow<E, C>() -> GqlQueryError<EdgeScanError<E>, C> {
    GqlQueryError::Source(EdgeScanError::CounterExhausted)
}

fn prefix_bytes<Record, Guard>(hops: usize) -> Option<usize> {
    // The entry may move through a BTreeMap. Cover a node's spare value slots
    // and the exact path buffer, including large inline record/guard types.
    // Vector allocations have independent guards: consuming entries does not
    // release a Vec/IntoIter's capacity. Source payloads are also independent.
    core::mem::size_of::<Prefix<Record, Guard>>()
        .checked_mul(16)?
        .checked_add(1024)?
        .checked_add(hops.checked_mul(core::mem::size_of::<(EId, VId)>().checked_mul(4)?)?)
}

fn reserve_vector<S: AsyncGraphWalkSource, F: FnMut() -> Result<(), C>, C>(
    source: &S,
    capacity: usize,
    meter: &mut Meter<F>,
) -> ScanResult<Option<S::TraversalGuard>, S::Error, C> {
    if capacity == 0 {
        return Ok(None);
    }
    let bytes = core::mem::size_of::<Prefix<S::Record, S::TraversalGuard>>()
        .checked_mul(capacity)
        .ok_or_else(overflow)?;
    flatten(source.reserve_shortest_state(bytes, &mut |event| meter.event(event))).map(Some)
}

fn compare_paths<E>(
    left: &GraphPath,
    right: &GraphPath,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<core::cmp::Ordering, E> {
    control(GlaExecutionEvent::Work)?;
    let order = left.start().cmp(&right.start());
    if !order.is_eq() {
        return Ok(order);
    }
    for (left, right) in left.steps().iter().zip(right.steps()) {
        control(GlaExecutionEvent::Work)?;
        let order = left.cmp(right);
        if !order.is_eq() {
            return Ok(order);
        }
    }
    Ok(left.len().cmp(&right.len()))
}

impl<S: AsyncGraphWalkSource, F> AsyncGraphWalkCursor<S, F> {
    pub(super) async fn advance_shortest<C>(
        &mut self,
        source: &mut S,
        frame: &mut Option<Breadth<S::RootRecord, S::Record, S::TraversalGuard>>,
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
                let bytes =
                    core::mem::size_of::<Breadth<S::RootRecord, S::Record, S::TraversalGuard>>()
                        .checked_add(2048)
                        .ok_or_else(overflow)?;
                let guard =
                    flatten(source.reserve_shortest_state(bytes, &mut |event| meter.event(event)))?;
                let bytes = prefix_bytes::<S::Record, S::TraversalGuard>(0).ok_or_else(overflow)?;
                let prefix_guard =
                    flatten(source.reserve_shortest_state(bytes, &mut |event| meter.event(event)))?;
                let frontier_guard = reserve_vector(source, 1, meter)?;
                meter.event(GlaExecutionEvent::ScratchEntry)?;
                *frame = Some(Breadth {
                    root,
                    start,
                    depth: 0,
                    frontier: vec![Prefix {
                        path: GraphPath::new(start, Box::new([])),
                        record: None,
                        _guard: prefix_guard,
                    }],
                    frontier_guard,
                    pending: Vec::new().into_iter(),
                    pending_guard: None,
                    settled: BTreeMap::new(),
                    _guard: guard,
                });
            }
            let active = frame.as_mut().expect("admitted shortest root");
            if let Some(prefix) = active.pending.next() {
                if let Some(output) = output(source, active, prefix, &self.plan, self.seq, meter)? {
                    return Ok(Some(output));
                }
                continue;
            }
            // Even an exhausted IntoIter retains its original allocation.
            // Drop that allocation before releasing its separate byte guard.
            active.pending = Vec::new().into_iter();
            active.pending_guard = None;
            if active.frontier.is_empty() || active.depth > self.plan.maximum_hops() {
                *frame = None;
                continue;
            }
            fill_layer(source, active, &self.plan, meter).await?;
        }
    }
}

async fn fill_layer<S, F, C>(
    source: &mut S,
    frame: &mut Breadth<S::RootRecord, S::Record, S::TraversalGuard>,
    plan: &AsyncGraphWalkPlan,
    meter: &mut Meter<F>,
) -> ScanResult<(), S::Error, C>
where
    S: AsyncGraphWalkSource,
    F: FnMut() -> Result<(), C> + Send,
    C: Send,
{
    let emit = frame.depth >= plan.bounds.minimum() as usize;
    if emit {
        // Settle the WHOLE layer before expansion, including endpoints that a
        // later predicate rejects. Selection precedes those output predicates.
        for prefix in &frame.frontier {
            meter.event(GlaExecutionEvent::Work)?;
            let endpoint = prefix.endpoint();
            if !frame.settled.contains_key(&endpoint) {
                let bytes = core::mem::size_of::<(VId, S::TraversalGuard)>()
                    .checked_mul(16)
                    .and_then(|bytes| bytes.checked_add(512))
                    .ok_or_else(overflow)?;
                let guard =
                    flatten(source.reserve_shortest_state(bytes, &mut |event| meter.event(event)))?;
                frame.settled.insert(endpoint, guard);
            }
        }
    }
    let mut next = BTreeMap::<VId, Prefix<S::Record, S::TraversalGuard>>::new();
    let pending_capacity = if emit { frame.frontier.len() } else { 0 };
    let pending_guard = reserve_vector(source, pending_capacity, meter)?;
    let mut pending = Vec::with_capacity(pending_capacity);
    // The old vector remains allocated while its entries are consumed. Keep
    // its guard across awaits even when almost every prefix has been dropped.
    let previous_guard = frame.frontier_guard.take();
    let mut previous = core::mem::take(&mut frame.frontier).into_iter();
    while let Some(prefix) = previous.next() {
        meter.event(GlaExecutionEvent::Work)?;
        let endpoint = prefix.endpoint();
        if frame.depth < plan.maximum_hops() {
            let mut after = None;
            loop {
                let mut admitted = None;
                let candidate = match source
                    .next_incident_candidate(
                        endpoint,
                        plan.relation,
                        plan.direction,
                        after,
                        &mut |event| admit_edge(meter, &mut after, &mut admitted, event),
                    )
                    .await
                {
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
                let Some(candidate) = candidate else {
                    break;
                };
                let Some(record) = candidate.record else {
                    continue;
                };
                let Some(target) = accepted_target(plan, source, &record, endpoint, meter)? else {
                    continue;
                };
                meter.event(GlaExecutionEvent::Work)?;
                if frame.settled.contains_key(&target) {
                    continue;
                }
                let step = (candidate.eid, target);
                if let Some(previous) = next.get(&target) {
                    // A dominated candidate never allocates a path or enters
                    // the next frontier. Compare full real edge identities.
                    let mut better = false;
                    for (left, &right) in prefix
                        .path
                        .steps()
                        .iter()
                        .copied()
                        .chain(core::iter::once(step))
                        .zip(previous.path.steps())
                    {
                        meter.event(GlaExecutionEvent::Work)?;
                        match left.cmp(&right) {
                            core::cmp::Ordering::Less => {
                                better = true;
                                break;
                            }
                            core::cmp::Ordering::Greater => break,
                            core::cmp::Ordering::Equal => {}
                        }
                    }
                    if !better {
                        continue;
                    }
                }
                let hops = prefix.path.len().checked_add(1).ok_or_else(overflow)?;
                let bytes =
                    prefix_bytes::<S::Record, S::TraversalGuard>(hops).ok_or_else(overflow)?;
                let guard =
                    flatten(source.reserve_shortest_state(bytes, &mut |event| meter.event(event)))?;
                for _ in 0..hops {
                    meter.event(GlaExecutionEvent::Work)?;
                    meter.event(GlaExecutionEvent::ScratchEntry)?;
                }
                let mut steps = Vec::with_capacity(hops);
                steps.extend_from_slice(prefix.path.steps());
                steps.push(step);
                next.insert(
                    target,
                    Prefix {
                        path: GraphPath::new(frame.start, steps.into_boxed_slice()),
                        record: Some(record),
                        _guard: guard,
                    },
                );
            }
        }
        if emit {
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            pending.push(prefix);
        }
    }
    drop(previous);
    drop(previous_guard);
    // The vector has its own guard before allocation. Prefix reservations
    // continue to cover map nodes until the map iterator releases them.
    // The ordinary fallible sort keeps comparisons/copies governed; no custom
    // infallible Ord callback can hide cancellation or path-comparison work.
    let frontier_guard = reserve_vector(source, next.len(), meter)?;
    let mut frontier = Vec::with_capacity(next.len());
    for prefix in next.into_values() {
        meter.event(GlaExecutionEvent::Work)?;
        meter.event(GlaExecutionEvent::ScratchEntry)?;
        frontier.push(prefix);
    }
    let mut sort = crate::set_ops::InPlaceSort::new(frontier.len());
    loop {
        let mut remaining = usize::MAX;
        if sort
            .advance(
                &mut frontier,
                &mut remaining,
                &mut |event| meter.event(event),
                &mut |left, right, control| compare_paths(&left.path, &right.path, control),
            )?
            .is_ready()
        {
            break;
        }
    }
    frame.frontier = frontier;
    frame.frontier_guard = frontier_guard;
    frame.pending = pending.into_iter();
    frame.pending_guard = pending_guard;
    frame.depth = frame.depth.checked_add(1).ok_or_else(overflow)?;
    Ok(())
}

fn output<S: AsyncGraphWalkSource, F: FnMut() -> Result<(), C>, C>(
    source: &S,
    frame: &Breadth<S::RootRecord, S::Record, S::TraversalGuard>,
    prefix: Prefix<S::Record, S::TraversalGuard>,
    plan: &AsyncGraphWalkPlan,
    seq: CommitSeq,
    meter: &mut Meter<F>,
) -> ScanResult<Option<AsyncGraphWalkOutput<S::OutputGuard>>, S::Error, C> {
    let endpoint = prefix.endpoint();
    let records = prefix.record.as_slice();
    // This retained record corresponds to the final step. Captures below use
    // the complete path, while scalar edge access is admitted only at one hop.
    let last = prefix
        .path
        .steps()
        .last()
        .map_or(&[][..], core::slice::from_ref);
    let local = FrameSource::<_, _, S::Error> {
        root: &frame.root,
        start: frame.start,
        records,
        steps: last,
        seq,
        error: core::marker::PhantomData,
    };
    let ids = [Some(frame.start), Some(endpoint)];
    let mut paths = Vec::new();
    let observe = |meter: &mut Meter<F>, event| {
        meter.event(event)?;
        let result = match prefix.record.as_ref() {
            Some(record) => source.evaluation_event(record, event),
            None => source.root_evaluation_event(&frame.root, event),
        };
        result.map_err(|error| GqlQueryError::Source(EdgeScanError::Source(error)))
    };
    for op in plan.suffix.iter() {
        let mut control = |event| observe(meter, event);
        control(GlaExecutionEvent::Work)?;
        match op {
            GlaOperator::CapturePath { .. } => {
                control(GlaExecutionEvent::ScratchEntry)?;
                for _ in prefix.path.steps() {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    control(GlaExecutionEvent::ScratchEntry)?;
                }
                paths.push(Some(prefix.path.clone()));
            }
            // Compilation proves this redundant for minimum-zero shortest.
            GlaOperator::DifferentEdges { .. } => {}
            GlaOperator::SelectPathLength {
                capture,
                comparison,
                value,
            } => {
                if !paths[*capture as usize]
                    .as_ref()
                    .is_some_and(|path| comparison.accepts(path.len() as i64, *value))
                {
                    return Ok(None);
                }
            }
            GlaOperator::SelectPathNull {
                capture, is_null, ..
            } => {
                if paths[*capture as usize].is_none() != *is_null {
                    return Ok(None);
                }
            }
            op => {
                if !join::accepts(op, &ids, &paths, &local, &mut control)? {
                    return Ok(None);
                }
            }
        }
    }
    let count = meter.increment(GqlBudgetDimension::ResultRows, meter.rows.result_rows)?;
    let guard = flatten(source.reserve_shortest_output(
        &frame.root,
        records,
        prefix.path.len(),
        plan.columns,
        &mut |event| meter.event(event),
    ))?;
    let row = join::project(&plan.projection, &ids, &paths, &local, &mut |event| {
        observe(meter, event)
    })?;
    meter.event(GlaExecutionEvent::ResultRow)?;
    meter.rows.result_rows = count;
    Ok(Some(AsyncGraphWalkOutput { row, guard }))
}
