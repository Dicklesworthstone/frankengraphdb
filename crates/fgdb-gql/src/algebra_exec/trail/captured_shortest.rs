//! Lazy layered path capture over the executor's sorted identified index.
//!
//! Reachability is shared by (depth, vertex), not by path occurrence. Once an
//! output layer is selected, backward viability removes dead prefixes. A
//! depth-first walk of that layered graph then emits the real
//! edge/vertex identities in the same hop-first, lexicographic order as the
//! eager path cursor. Only ALL SHORTEST settles endpoints across depths.
//! ACYCLIC/SIMPLE use WALK reachability as an overapproximation, then check
//! the actual path's membership before descent. Their histories never coalesce.
//! Parallel edges and duplicate occurrences are not merged.
//! This is bounded query scratch, not a storage or spill implementation.

use super::{EId, GlaExecutionEvent, GraphPath, GraphWalkBounds, VId};
use std::collections::{BTreeMap, BTreeSet};

type Adjacency = BTreeMap<VId, Vec<(EId, VId)>>;

struct Frame {
    vertex: VId,
    next: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CaptureMode {
    All,
    AllShortest,
    Acyclic,
    Simple,
}

pub(crate) struct CapturedPathCursor<'a> {
    adjacency: Option<&'a Adjacency>,
    source: VId,
    bounds: GraphWalkBounds,
    mode: CaptureMode,
    depth: u32,
    layers: Vec<BTreeSet<VId>>,
    settled: BTreeSet<VId>,
    // Reverse order: viable[k] can reach this output layer in exactly k hops.
    viable: Vec<BTreeSet<VId>>,
    frames: Vec<Frame>,
    steps: Vec<(EId, VId)>,
    started: bool,
    emitted_layer: bool,
    done: bool,
}

impl<'a> CapturedPathCursor<'a> {
    /// The enclosing identified-index builder sorts by (EId, VId) with its
    /// ordinary fallible controls. Borrow that index; do not copy graph rows.
    pub(super) fn new<E>(
        source: VId,
        bounds: GraphWalkBounds,
        mode: CaptureMode,
        adjacency: Option<&'a Adjacency>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        control(GlaExecutionEvent::Work)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        Ok(Self {
            adjacency,
            source,
            bounds,
            mode,
            depth: 0,
            layers: vec![BTreeSet::from([source])],
            settled: BTreeSet::new(),
            viable: Vec::new(),
            frames: Vec::new(),
            steps: Vec::new(),
            started: false,
            emitted_layer: false,
            done: false,
        })
    }

    pub(super) fn next_with_control<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<GraphPath>, E> {
        let result = self.next_controlled(control);
        if result.is_err() {
            self.finish();
        }
        result
    }

    fn finish(&mut self) {
        self.done = true;
        self.adjacency = None;
        self.layers = Vec::new();
        self.settled.clear();
        self.viable = Vec::new();
        self.frames = Vec::new();
        self.steps = Vec::new();
    }

    fn next_controlled<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<GraphPath>, E> {
        loop {
            if self.done {
                return Ok(None);
            }
            if let Some(path) = self.next_path(control)? {
                return Ok(Some(path));
            }
            if self.started {
                // A repetition-restricted path at a later depth would have
                // a valid prefix at this depth. Once an admissible layer is
                // empty, WALK's reachability overapproximation cannot justify
                // searching more layers of impossible histories.
                if self.depth >= self.bounds.minimum()
                    && !self.emitted_layer
                    && matches!(self.mode, CaptureMode::Acyclic | CaptureMode::Simple)
                {
                    self.finish();
                    return Ok(None);
                }
                // Do not build a later layer before the caller has consumed
                // this one. LIMIT/cancellation can stop without that work.
                if !self.advance_layer(control)? {
                    self.finish();
                    return Ok(None);
                }
            } else {
                self.started = true;
            }
            if self.depth >= self.bounds.minimum() {
                self.emitted_layer = false;
                self.prepare_output(control)?;
            }
        }
    }

    fn advance_layer<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<bool, E> {
        control(GlaExecutionEvent::Work)?;
        if self.depth == self.bounds.maximum() {
            return Ok(false);
        }
        let mut next = BTreeSet::new();
        for &vertex in self.layers.last().expect("an active cursor has a layer") {
            control(GlaExecutionEvent::Work)?;
            if let Some(neighbors) = self.adjacency.and_then(|map| map.get(&vertex)) {
                for &(_, destination) in neighbors {
                    control(GlaExecutionEvent::Work)?;
                    if (self.mode == CaptureMode::AllShortest
                        && self.settled.contains(&destination))
                        || next.contains(&destination)
                    {
                        continue;
                    }
                    control(GlaExecutionEvent::ScratchEntry)?;
                    next.insert(destination);
                }
            }
        }
        if next.is_empty() {
            return Ok(false);
        }
        control(GlaExecutionEvent::ScratchEntry)?;
        self.layers.push(next);
        self.depth += 1;
        Ok(true)
    }

    fn prepare_output<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        // Only admissible depths settle an endpoint. A visit below the lower
        // bound must not suppress a later qualifying walk through a cycle.
        if self.mode == CaptureMode::AllShortest {
            for &vertex in self.layers.last().expect("an active cursor has a layer") {
                control(GlaExecutionEvent::Work)?;
                control(GlaExecutionEvent::ScratchEntry)?;
                self.settled.insert(vertex);
            }
        }
        self.viable = Vec::new();
        self.frames = Vec::new();
        self.steps = Vec::new();
        for layer in self.layers.iter().rev() {
            control(GlaExecutionEvent::Work)?;
            let mut live = BTreeSet::new();
            for &vertex in layer {
                control(GlaExecutionEvent::Work)?;
                let mut reaches = self.viable.is_empty();
                if let Some(suffix) = self.viable.last()
                    && let Some(neighbors) = self.adjacency.and_then(|map| map.get(&vertex))
                {
                    for &(_, destination) in neighbors {
                        control(GlaExecutionEvent::Work)?;
                        if suffix.contains(&destination) {
                            reaches = true;
                            break;
                        }
                    }
                }
                if reaches {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    live.insert(vertex);
                }
            }
            control(GlaExecutionEvent::ScratchEntry)?;
            self.viable.push(live);
        }
        if self
            .viable
            .last()
            .is_none_or(|root| !root.contains(&self.source))
        {
            return Ok(());
        }
        // Reserve a single reusable traversal stack and step buffer. Tied
        // occurrences reuse these entries instead of retaining a path bag.
        let depth = self.depth as usize;
        for _ in 0..depth + 1 {
            control(GlaExecutionEvent::ScratchEntry)?;
        }
        for _ in 0..depth {
            control(GlaExecutionEvent::ScratchEntry)?;
        }
        self.frames = Vec::with_capacity(depth + 1);
        self.steps = Vec::with_capacity(depth);
        self.frames.push(Frame {
            vertex: self.source,
            next: 0,
        });
        Ok(())
    }

    fn pop_frame(&mut self) {
        let _ = self.frames.pop();
        let _ = self.steps.pop();
    }

    fn accepts_step<E>(
        &self,
        destination: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<bool, E> {
        if !matches!(self.mode, CaptureMode::Acyclic | CaptureMode::Simple) {
            return Ok(true);
        }
        control(GlaExecutionEvent::Work)?;
        if destination == self.source {
            // A SIMPLE closing return may only be this layer's last step;
            // it never becomes a transit prefix, even below the lower bound.
            return Ok(self.mode == CaptureMode::Simple
                && self.steps.len() + 1 == self.depth as usize);
        }
        for &(_, vertex) in &self.steps {
            control(GlaExecutionEvent::Work)?;
            if vertex == destination {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn next_path<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<GraphPath>, E> {
        while !self.frames.is_empty() {
            // Every delivery, candidate edge and backtrack is interruptible.
            control(GlaExecutionEvent::Work)?;
            let at = self.frames.len() - 1;
            if at == self.depth as usize {
                for _ in &self.steps {
                    control(GlaExecutionEvent::Work)?;
                    control(GlaExecutionEvent::ScratchEntry)?;
                }
                let path = GraphPath::new(self.source, self.steps.clone().into_boxed_slice());
                self.pop_frame();
                self.emitted_layer = true;
                return Ok(Some(path));
            }
            let frame = &mut self.frames[at];
            let step = self
                .adjacency
                .and_then(|map| map.get(&frame.vertex))
                .and_then(|neighbors| neighbors.get(frame.next))
                .copied();
            let Some(step) = step else {
                self.pop_frame();
                continue;
            };
            frame.next += 1;
            let remaining = self.depth as usize - at - 1;
            if self.viable[remaining].contains(&step.1) && self.accepts_step(step.1, control)? {
                self.frames.push(Frame {
                    vertex: step.1,
                    next: 0,
                });
                self.steps.push(step);
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{GraphWalkSearch, IdentifiedExpansion};

    const MODES: [CaptureMode; 4] = [
        CaptureMode::All,
        CaptureMode::AllShortest,
        CaptureMode::Acyclic,
        CaptureMode::Simple,
    ];

    fn collect<E>(
        source: VId,
        bounds: GraphWalkBounds,
        mode: CaptureMode,
        adjacency: Option<&Adjacency>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Vec<GraphPath>, E> {
        let mut cursor = CapturedPathCursor::new(source, bounds, mode, adjacency, control)?;
        let mut output = Vec::new();
        while let Some(path) = cursor.next_with_control(control)? {
            output.push(path);
        }
        Ok(output)
    }

    // Enumerate every bounded WALK, without sharing, settlement or pruning.
    // Then independently filter complete paths by mode and select minima.
    fn oracle(
        source: VId,
        bounds: GraphWalkBounds,
        mode: CaptureMode,
        adjacency: &Adjacency,
    ) -> Vec<GraphPath> {
        let mut todo = vec![GraphPath::new(source, Box::new([]))];
        let mut paths = Vec::new();
        let mut first = BTreeMap::<VId, usize>::new();
        while let Some(path) = todo.pop() {
            let endpoint = path.steps().last().map_or(source, |step| step.1);
            if path.len() < bounds.maximum() as usize {
                for &step in adjacency.get(&endpoint).into_iter().flatten() {
                    let mut steps = path.steps().to_vec();
                    steps.push(step);
                    todo.push(GraphPath::new(source, steps.into_boxed_slice()));
                }
            }
            if path.len() < bounds.minimum() as usize {
                continue;
            }
            if matches!(mode, CaptureMode::Acyclic | CaptureMode::Simple) {
                let mut vertices = path.vertices().collect::<Vec<_>>();
                if mode == CaptureMode::Simple && !path.is_empty() && endpoint == source {
                    let _ = vertices.pop();
                }
                let unique = vertices.iter().copied().collect::<BTreeSet<_>>();
                if unique.len() != vertices.len() {
                    continue;
                }
            }
            first
                .entry(endpoint)
                .and_modify(|depth| *depth = (*depth).min(path.len()))
                .or_insert(path.len());
            paths.push(path);
        }
        if mode == CaptureMode::AllShortest {
            paths.retain(|path| {
                let endpoint = path.steps().last().map_or(source, |step| step.1);
                first.get(&endpoint) == Some(&path.len())
            });
        }
        paths.sort_by(|left, right| left.len().cmp(&right.len()).then_with(|| left.cmp(right)));
        paths
    }

    #[test]
    fn all_capture_modes_match_every_unpruned_three_vertex_graph() {
        for mask in 0..512_u32 {
            let mut adjacency = Adjacency::new();
            for source in 0..3_u128 {
                for destination in 0..3_u128 {
                    let bit = source * 3 + destination;
                    if mask & (1 << bit) != 0 {
                        let neighbors = adjacency.entry(VId(source)).or_default();
                        neighbors.push((EId(2 * bit), VId(destination)));
                        if source == destination {
                            neighbors.push((EId(2 * bit + 1), VId(destination)));
                        }
                    }
                }
            }
            for mode in MODES {
                for source in 0..4_u128 {
                    for maximum in 0..=3 {
                        for minimum in 0..=maximum {
                            let bounds = GraphWalkBounds::new(minimum, maximum).unwrap();
                            let actual = collect(
                                VId(source),
                                bounds,
                                mode,
                                Some(&adjacency),
                                &mut |_| Ok::<_, ()>(()),
                            )
                            .unwrap();
                            assert_eq!(
                                actual,
                                oracle(VId(source), bounds, mode, &adjacency),
                                "mask={mask}, source={source}, bounds={bounds:?}, mode={mode:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn maximum_depth_parallel_cycles_deliver_without_materializing_ties() {
        let adjacency = Adjacency::from([(
            VId(7),
            (0..8).map(|edge| (EId(edge), VId(7))).collect(),
        )]);
        let bounds =
            GraphWalkBounds::new(crate::MAX_GRAPH_WALK_HOPS, crate::MAX_GRAPH_WALK_HOPS).unwrap();
        for mode in [CaptureMode::All, CaptureMode::AllShortest] {
            let mut events = 0;
            let mut control = |_| {
                events += 1;
                if events > 50_000 {
                    Err("materialized tied routes")
                } else {
                    Ok(())
                }
            };
            let mut cursor =
                CapturedPathCursor::new(VId(7), bounds, mode, Some(&adjacency), &mut control)
                    .unwrap();
            let first = cursor.next_with_control(&mut control).unwrap().unwrap();
            let second = cursor.next_with_control(&mut control).unwrap().unwrap();
            assert_eq!(first.len(), crate::MAX_GRAPH_WALK_HOPS as usize);
            assert!(first.edges().all(|edge| edge == EId(0)));
            assert_eq!(second.steps().last(), Some(&(EId(1), VId(7))));
            assert_eq!(
                &first.steps()[..first.len() - 1],
                &second.steps()[..second.len() - 1]
            );
            assert!(first < second);
            // Logical retained entries, not an allocator-specific byte bound:
            // Vec::with_capacity is permitted to reserve more than requested.
            assert!(cursor.frames.len() <= crate::MAX_GRAPH_WALK_HOPS as usize + 1);
            assert!(cursor.steps.len() <= crate::MAX_GRAPH_WALK_HOPS as usize);
            assert_eq!(cursor.layers.len(), crate::MAX_GRAPH_WALK_HOPS as usize + 1);
        }
    }

    #[test]
    fn production_dispatch_streams_the_first_of_a_trillion_captured_paths() {
        let mut adjacency = Adjacency::new();
        for depth in 0..40_u128 {
            adjacency.insert(
                VId(depth),
                vec![
                    (EId(2 * depth), VId(depth + 1)),
                    (EId(2 * depth + 1), VId(depth + 1)),
                ],
            );
        }
        for search in [
            GraphWalkSearch::All,
            GraphWalkSearch::AllShortest,
            GraphWalkSearch::Acyclic,
            GraphWalkSearch::Simple,
        ] {
            let mut events = 0;
            let mut control = |_| {
                events += 1;
                if events > 50_000 {
                    Err("eager captured-path layer")
                } else {
                    Ok(())
                }
            };
            let mut cursor = IdentifiedExpansion::new(
                VId(0),
                GraphWalkBounds::new(40, 40).unwrap(),
                search,
                Some(&adjacency),
                true,
                &mut control,
            )
            .unwrap();
            let (endpoint, first) = cursor.next_with_control(&mut control).unwrap().unwrap();
            let (second_endpoint, second) = cursor.next_with_control(&mut control).unwrap().unwrap();
            let first = first.expect("capture survives production dispatch");
            let second = second.expect("capture survives production dispatch");
            assert_eq!(endpoint, VId(40));
            assert_eq!(second_endpoint, endpoint);
            assert_eq!(first.len(), 40);
            assert!(first.edges().all(|edge| edge.0 % 2 == 0));
            assert_eq!(&first.steps()[..39], &second.steps()[..39]);
            assert_eq!(first.steps()[39], (EId(78), VId(40)));
            assert_eq!(second.steps()[39], (EId(79), VId(40)));
        }
    }

    #[test]
    fn duplicate_occurrences_and_edge_identity_order_survive_layer_sharing() {
        let adjacency = Adjacency::from([
            (
                VId(1),
                vec![(EId(2), VId(3)), (EId(2), VId(3)), (EId(9), VId(2))],
            ),
            (VId(2), vec![(EId(1), VId(4))]),
            (VId(3), vec![(EId(8), VId(4))]),
        ]);
        let bounds = GraphWalkBounds::new(2, 3).unwrap();
        for mode in MODES {
            let actual = collect(VId(1), bounds, mode, Some(&adjacency), &mut |_| {
                Ok::<_, ()>(())
            })
            .unwrap();
            assert_eq!(actual, oracle(VId(1), bounds, mode, &adjacency));
            assert_eq!(actual.len(), 3);
            assert_eq!(actual[0], actual[1]);
            assert_eq!(actual[0].edges().collect::<Vec<_>>(), vec![EId(2), EId(8)]);
            assert_eq!(actual[2].edges().collect::<Vec<_>>(), vec![EId(9), EId(1)]);
        }
    }

    #[test]
    fn zero_hops_never_build_or_consult_a_later_layer() {
        let adjacency = Adjacency::from([(VId(5), vec![(EId(1), VId(6)); 10_000])]);
        for mode in MODES {
            let mut cursor = CapturedPathCursor::new(
                VId(5),
                GraphWalkBounds::new(0, 10).unwrap(),
                mode,
                Some(&adjacency),
                &mut |_| Ok::<_, ()>(()),
            )
            .unwrap();
            let mut work = 0;
            let path = cursor
                .next_with_control(&mut |_| {
                    work += 1;
                    if work > 20 {
                        Err("read future adjacency before output")
                    } else {
                        Ok(())
                    }
                })
                .unwrap()
                .unwrap();
            assert!(path.is_empty());
            assert_eq!(path.start(), VId(5));
            assert_eq!(cursor.layers.len(), 1);
            assert_eq!(
                collect(VId(5), GraphWalkBounds::new(0, 0).unwrap(), mode, None, &mut |_| {
                    Ok::<_, ()>(())
                })
                .unwrap(),
                vec![path]
            );
            assert!(
                collect(VId(5), GraphWalkBounds::new(1, 1).unwrap(), mode, None, &mut |_| {
                    Ok::<_, ()>(())
                })
                .unwrap()
                .is_empty()
            );
        }
    }

    #[test]
    fn impossible_restricted_prefixes_stop_without_searching_the_remaining_hop_bound() {
        let adjacency = Adjacency::from([(
            VId(1),
            vec![(EId(1), VId(1)), (EId(2), VId(2))],
        )]);
        for mode in [CaptureMode::Acyclic, CaptureMode::Simple] {
            let mut work = 0;
            let paths = collect(
                VId(1),
                GraphWalkBounds::new(0, crate::MAX_GRAPH_WALK_HOPS).unwrap(),
                mode,
                Some(&adjacency),
                &mut |_| {
                    work += 1;
                    if work > 200 { Err(()) } else { Ok(()) }
                },
            )
            .unwrap();
            assert_eq!(paths.len(), if mode == CaptureMode::Simple { 3 } else { 2 });
            assert!(paths.iter().all(|path| path.len() <= 1));
        }
    }

    #[test]
    fn every_refusal_is_terminal_and_releases_all_retained_search_state() {
        let adjacency = Adjacency::from([
            (VId(1), vec![(EId(1), VId(1)), (EId(2), VId(2))]),
            (VId(2), vec![(EId(3), VId(1)), (EId(4), VId(3))]),
        ]);
        for mode in MODES {
            for minimum in [0, 1, 3] {
                let bounds = GraphWalkBounds::new(minimum, 4).unwrap();
                let mut total = 0;
                collect(VId(1), bounds, mode, Some(&adjacency), &mut |_| {
                    total += 1;
                    Ok::<_, usize>(())
                })
                .unwrap();
                for stop in 1..=total {
                    let mut seen = 0;
                    let mut control = |_| {
                        seen += 1;
                        if seen == stop { Err(stop) } else { Ok(()) }
                    };
                    let mut cursor = match CapturedPathCursor::new(
                        VId(1), bounds, mode, Some(&adjacency), &mut control,
                    ) {
                        Err(error) => {
                            assert_eq!(error, stop);
                            continue;
                        }
                        Ok(cursor) => cursor,
                    };
                    loop {
                        match cursor.next_with_control(&mut control) {
                            Ok(Some(_)) => {}
                            Ok(None) => panic!("missed refusal {stop}"),
                            Err(error) => {
                                assert_eq!(error, stop);
                                break;
                            }
                        }
                    }
                    assert_eq!(seen, stop);
                    assert!(cursor.done);
                    assert!(cursor.adjacency.is_none());
                    assert!(cursor.settled.is_empty());
                    assert_eq!(cursor.layers.capacity(), 0);
                    assert_eq!(cursor.viable.capacity(), 0);
                    assert_eq!(cursor.frames.capacity(), 0);
                    assert_eq!(cursor.steps.capacity(), 0);
                    assert_eq!(
                        cursor.next_with_control(&mut |_| Err::<(), _>(usize::MAX)),
                        Ok(None)
                    );
                }
            }
        }
    }
}
