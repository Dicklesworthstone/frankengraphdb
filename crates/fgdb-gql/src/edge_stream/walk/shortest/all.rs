//! Cold ALL SHORTEST retains a depth-layered incidence DAG, not path prefixes.
//!
//! Each reached (depth, vertex) opens adjacency once. Parallel real EIds remain
//! separate DAG edges, so sharing never changes path multiplicity. A complete
//! layer settles its endpoints only after the lower bound. Backwards viability
//! then removes dead prefixes before a lazy, edge-identity-ordered DFS emits
//! that layer's paths. A repeated vertex before the lower bound is an ordinary
//! distinct layer node, not an accidental global visited-set rejection.
//!
//! The DAG owns source records directly; records need neither Clone nor Sync.
//! Every map entry and vector backing is admitted before allocation. Only one
//! route is copied for projection, under a temporary state reservation, and
//! the resulting row has the ordinary independent shortest-output guard.

use super::*;
use std::ops::Bound::{Excluded, Unbounded};

struct Hop<Record, Guard> {
    target: VId,
    record: Record,
    _guard: Guard,
}

struct Node<Record, Guard> {
    edges: BTreeMap<EId, Hop<Record, Guard>>,
    viable: bool,
    _guard: Guard,
}

/// Vector backing has its own lifetime: popping the last value does not free
/// its allocation. Reallocation retains both reservations until the old
/// allocation is dropped, and moving a guarded value never clones its record.
struct Retained<T, Guard> {
    values: Vec<T>,
    guard: Option<(Guard, Option<Guard>)>,
}

impl<T, Guard> Retained<T, Guard> {
    fn new() -> Self {
        Self {
            values: Vec::new(),
            guard: None,
        }
    }
}

pub(crate) struct AllBreadth<Root, Record, Guard> {
    root: Root,
    start: VId,
    depth: usize,
    layers: Retained<BTreeMap<VId, Node<Record, Guard>>, Guard>,
    settled: BTreeMap<VId, Guard>,
    positions: Retained<Position, Guard>,
    steps: Retained<(EId, VId), Guard>,
    ready: bool,
    // Root, records, map entries and vectors drop before this reservation.
    _guard: Guard,
}

/// Account for BTreeMap's spare node slots, including inline record and guard
/// sizes. Like ANY's prefix accounting this is a conservative backing-store
/// reservation, independent of the source record's own retained-payload guard.
fn entry_guard<S, T, F, C>(
    source: &S,
    meter: &mut Meter<F>,
) -> ScanResult<S::TraversalGuard, S::Error, C>
where
    S: AsyncGraphWalkSource,
    F: FnMut() -> Result<(), C>,
{
    let bytes = core::mem::size_of::<T>()
        .checked_mul(16)
        .and_then(|bytes| bytes.checked_add(512))
        .ok_or_else(overflow)?;
    flatten(source.reserve_shortest_state(bytes, &mut |event| meter.event(event)))
}

fn admit_node<S, F, C>(
    source: &S,
    meter: &mut Meter<F>,
) -> ScanResult<Node<S::Record, S::TraversalGuard>, S::Error, C>
where
    S: AsyncGraphWalkSource,
    F: FnMut() -> Result<(), C>,
{
    let guard = entry_guard::<S, (VId, Node<S::Record, S::TraversalGuard>), _, _>(source, meter)?;
    meter.event(GlaExecutionEvent::ScratchEntry)?;
    Ok(Node {
        edges: BTreeMap::new(),
        viable: false,
        _guard: guard,
    })
}

fn push_retained<S, T, F, C>(
    source: &S,
    retained: &mut Retained<T, S::TraversalGuard>,
    value: T,
    meter: &mut Meter<F>,
) -> ScanResult<(), S::Error, C>
where
    S: AsyncGraphWalkSource,
    F: FnMut() -> Result<(), C>,
{
    if retained.values.len() == retained.values.capacity() {
        let capacity = retained
            .values
            .capacity()
            .max(1)
            .checked_mul(2)
            .ok_or_else(overflow)?;
        let bytes = core::mem::size_of::<T>()
            .checked_mul(capacity)
            .ok_or_else(overflow)?;
        let guard =
            flatten(source.reserve_shortest_state(bytes, &mut |event| meter.event(event)))?;
        let mut extra_guard = None;
        let mut replacement = Vec::with_capacity(capacity);
        // Allocators may expose more backing than requested. Cover that
        // actual excess before moving any entries or retaining the vector;
        // subsequent pushes use only fully admitted capacity.
        let extra = replacement
            .capacity()
            .checked_sub(capacity)
            .and_then(|extra| extra.checked_mul(core::mem::size_of::<T>()))
            .ok_or_else(overflow)?;
        if extra != 0 {
            extra_guard = Some(flatten(source.reserve_shortest_state(
                extra,
                &mut |event| meter.event(event),
            ))?);
        }
        for previous in retained.values.drain(..) {
            meter.event(GlaExecutionEvent::Work)?;
            replacement.push(previous);
        }
        let previous = core::mem::replace(&mut retained.values, replacement);
        drop(previous);
        retained.guard = Some((guard, extra_guard));
    }
    meter.event(GlaExecutionEvent::ScratchEntry)?;
    retained.values.push(value);
    Ok(())
}

impl<S: AsyncGraphWalkSource, F> AsyncGraphWalkCursor<S, F> {
    pub(in crate::edge_stream::walk) async fn advance_all_shortest<C>(
        &mut self,
        source: &mut S,
        frame: &mut Option<AllBreadth<S::RootRecord, S::Record, S::TraversalGuard>>,
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
                    core::mem::size_of::<AllBreadth<S::RootRecord, S::Record, S::TraversalGuard>>()
                        .checked_add(1024)
                        .ok_or_else(overflow)?;
                let guard =
                    flatten(source.reserve_shortest_state(bytes, &mut |event| meter.event(event)))?;
                let node = admit_node(source, meter)?;
                let mut initial = BTreeMap::new();
                initial.insert(start, node);
                let mut layers = Retained::new();
                push_retained(source, &mut layers, initial, meter)?;
                *frame = Some(AllBreadth {
                    root,
                    start,
                    depth: 0,
                    layers,
                    settled: BTreeMap::new(),
                    positions: Retained::new(),
                    steps: Retained::new(),
                    ready: false,
                    _guard: guard,
                });
            }
            let active = frame.as_mut().expect("admitted all-shortest root");
            if !active.ready {
                prepare_layer(source, active, &self.plan, meter)?;
                active.ready = true;
            }
            if next_route(source, active, meter)? {
                if let Some(row) = emit_route(source, active, &self.plan, self.seq, meter)? {
                    return Ok(Some(row));
                }
                continue;
            }
            if active.depth == self.plan.maximum_hops()
                || !extend_layer(source, active, &self.plan, meter).await?
            {
                *frame = None;
            }
        }
    }
}

/// Settling precedes every suffix predicate. A shortest endpoint rejected by
/// RETURN/WHERE is still settled, and every other tied path to it is retained.
fn prepare_layer<S, F, C>(
    source: &S,
    frame: &mut AllBreadth<S::RootRecord, S::Record, S::TraversalGuard>,
    plan: &AsyncGraphWalkPlan,
    meter: &mut Meter<F>,
) -> ScanResult<(), S::Error, C>
where
    S: AsyncGraphWalkSource,
    F: FnMut() -> Result<(), C>,
{
    if frame.depth < plan.bounds.minimum() as usize {
        return Ok(());
    }
    for &endpoint in frame.layers.values[frame.depth].keys() {
        meter.event(GlaExecutionEvent::Work)?;
        if let std::collections::btree_map::Entry::Vacant(entry) = frame.settled.entry(endpoint) {
            let guard = entry_guard::<S, (VId, S::TraversalGuard), _, _>(source, meter)?;
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            entry.insert(guard);
        }
    }
    // Every newest-layer vertex is a selected endpoint. Mark only prefixes
    // that can reach that layer; enumerating dead prefixes would recreate an
    // exponential path search even though the graph itself is shared.
    for node in frame.layers.values[frame.depth].values_mut() {
        meter.event(GlaExecutionEvent::Work)?;
        node.viable = true;
    }
    for depth in (0..frame.depth).rev() {
        let (before, after) = frame.layers.values.split_at_mut(depth + 1);
        let next = &after[0];
        for node in before[depth].values_mut() {
            meter.event(GlaExecutionEvent::Work)?;
            let mut viable = false;
            for hop in node.edges.values() {
                meter.event(GlaExecutionEvent::Work)?;
                if next.get(&hop.target).is_some_and(|node| node.viable) {
                    viable = true;
                    break;
                }
            }
            node.viable = viable;
        }
    }
    frame.positions.values.clear();
    frame.steps.values.clear();
    if frame.layers.values[0]
        .get(&frame.start)
        .expect("initial layer contains the root")
        .viable
    {
        push_retained(
            source,
            &mut frame.positions,
            Position {
                vertex: frame.start,
                after: None,
                offered: false,
            },
            meter,
        )?;
    }
    Ok(())
}

/// Return one complete route while retaining only its depth-sized DFS stack.
/// A leaf is offered once, then backtracked on the next pull. Edge maps use
/// real EId order; the source contract supplies one target per incidence.
fn next_route<S, F, C>(
    source: &S,
    frame: &mut AllBreadth<S::RootRecord, S::Record, S::TraversalGuard>,
    meter: &mut Meter<F>,
) -> ScanResult<bool, S::Error, C>
where
    S: AsyncGraphWalkSource,
    F: FnMut() -> Result<(), C>,
{
    loop {
        meter.event(GlaExecutionEvent::Work)?;
        let Some(depth) = frame.positions.values.len().checked_sub(1) else {
            return Ok(false);
        };
        let position = frame.positions.values.last_mut().expect("active route");
        if depth == frame.depth {
            if !position.offered {
                position.offered = true;
                return Ok(true);
            }
            frame.positions.values.pop();
            frame.steps.values.pop();
            continue;
        }
        let node = frame.layers.values[depth]
            .get(&position.vertex)
            .expect("route vertices belong to their layer");
        let next = match position.after {
            Some(after) => node.edges.range((Excluded(after), Unbounded)).next(),
            None => node.edges.iter().next(),
        }
        .map(|(&eid, hop)| (eid, hop.target));
        let Some((eid, target)) = next else {
            frame.positions.values.pop();
            frame.steps.values.pop();
            continue;
        };
        position.after = Some(eid);
        if !frame.layers.values[depth + 1]
            .get(&target)
            .expect("every admitted arc has a target node")
            .viable
        {
            continue;
        }
        push_retained(source, &mut frame.steps, (eid, target), meter)?;
        push_retained(
            source,
            &mut frame.positions,
            Position {
                vertex: target,
                after: None,
                offered: false,
            },
            meter,
        )?;
    }
}

/// Build one shared next layer, opening each reached vertex exactly once.
/// Mutable node iteration is intentional: records may be Send but not Sync,
/// and no shared reference into record-containing state crosses an await.
async fn extend_layer<S, F, C>(
    source: &mut S,
    frame: &mut AllBreadth<S::RootRecord, S::Record, S::TraversalGuard>,
    plan: &AsyncGraphWalkPlan,
    meter: &mut Meter<F>,
) -> ScanResult<bool, S::Error, C>
where
    S: AsyncGraphWalkSource,
    F: FnMut() -> Result<(), C> + Send,
    C: Send,
{
    let mut next = BTreeMap::<VId, Node<S::Record, S::TraversalGuard>>::new();
    for (&endpoint, node) in frame.layers.values[frame.depth].iter_mut() {
        meter.event(GlaExecutionEvent::Work)?;
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
            if let std::collections::btree_map::Entry::Vacant(entry) = next.entry(target) {
                entry.insert(admit_node(source, meter)?);
            }
            let guard = entry_guard::<S, (EId, Hop<S::Record, S::TraversalGuard>), _, _>(
                source, meter,
            )?;
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            node.edges.insert(
                candidate.eid,
                Hop {
                    target,
                    record,
                    _guard: guard,
                },
            );
        }
    }
    if next.is_empty() {
        return Ok(false);
    }
    let depth = frame.depth.checked_add(1).ok_or_else(overflow)?;
    push_retained(source, &mut frame.layers, next, meter)?;
    frame.depth = depth;
    frame.ready = false;
    Ok(true)
}

fn emit_route<S, F, C>(
    source: &S,
    frame: &AllBreadth<S::RootRecord, S::Record, S::TraversalGuard>,
    plan: &AsyncGraphWalkPlan,
    seq: CommitSeq,
    meter: &mut Meter<F>,
) -> ScanResult<Option<AsyncGraphWalkOutput<S::OutputGuard>>, S::Error, C>
where
    S: AsyncGraphWalkSource,
    F: FnMut() -> Result<(), C>,
{
    let bytes = frame
        .steps
        .values
        .len()
        .checked_mul(core::mem::size_of::<(EId, VId)>())
        .and_then(|bytes| bytes.checked_mul(2))
        .and_then(|bytes| bytes.checked_add(core::mem::size_of::<GraphPath>() + 128))
        .ok_or_else(overflow)?;
    let _guard =
        flatten(source.reserve_shortest_state(bytes, &mut |event| meter.event(event)))?;
    for _ in &frame.steps.values {
        meter.event(GlaExecutionEvent::Work)?;
        meter.event(GlaExecutionEvent::ScratchEntry)?;
    }
    let mut steps = Vec::with_capacity(frame.steps.values.len());
    steps.extend_from_slice(&frame.steps.values);
    let path = GraphPath::new(frame.start, steps.into_boxed_slice());
    let record = frame.depth.checked_sub(1).map(|depth| {
        let previous = frame.positions.values[depth].vertex;
        let eid = frame.steps.values[depth].0;
        &frame.layers.values[depth]
            .get(&previous)
            .expect("route predecessor belongs to its layer")
            .edges
            .get(&eid)
            .expect("route step belongs to its predecessor")
            .record
    });
    let result = output_path(source, &frame.root, &path, record, plan, seq, meter);
    // Evaluation temporaries have dropped inside output_path. The row owns
    // its independent output guard, and this local path still has _guard.
    // Reused DAG records must not retain one temporary reservation per tie.
    source.release_shortest_temporaries(
        &frame.root,
        record.map_or(&[][..], core::slice::from_ref),
    );
    result
}
