//! Lazy ALL SHORTEST capture over the executor's sorted identified index.
//!
//! Reachability is shared by (depth, vertex), not by path occurrence. After a
//! layer is settled, backward viability removes prefixes with no continuation
//! to that layer. A depth-first walk of that layered graph then emits the real
//! edge/vertex identities in the same hop-first, lexicographic order as the
//! eager path cursor. Parallel edges and duplicate occurrences are not merged.
//! This is bounded query scratch, not a storage or spill implementation.

use super::{EId, GlaExecutionEvent, GraphPath, GraphWalkBounds, VId};
use std::collections::{BTreeMap, BTreeSet};

type Adjacency = BTreeMap<VId, Vec<(EId, VId)>>;

struct Frame {
    vertex: VId,
    next: usize,
}

pub(crate) struct CapturedShortestCursor<'a> {
    adjacency: Option<&'a Adjacency>,
    source: VId,
    bounds: GraphWalkBounds,
    depth: u32,
    layers: Vec<BTreeSet<VId>>,
    settled: BTreeSet<VId>,
    // Reverse order: viable[k] can reach this output layer in exactly k hops.
    viable: Vec<BTreeSet<VId>>,
    frames: Vec<Frame>,
    steps: Vec<(EId, VId)>,
    started: bool,
    done: bool,
}

impl<'a> CapturedShortestCursor<'a> {
    /// The enclosing identified-index builder sorts by (EId, VId) with its
    /// ordinary fallible controls. Borrow that index; do not copy graph rows.
    pub(super) fn new<E>(
        source: VId,
        bounds: GraphWalkBounds,
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
            depth: 0,
            layers: vec![BTreeSet::from([source])],
            settled: BTreeSet::new(),
            viable: Vec::new(),
            frames: Vec::new(),
            steps: Vec::new(),
            started: false,
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
                    if self.settled.contains(&destination) || next.contains(&destination) {
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
        for &vertex in self.layers.last().expect("an active cursor has a layer") {
            control(GlaExecutionEvent::Work)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            self.settled.insert(vertex);
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
        self.frames.pop();
        self.steps.pop();
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
            if self.viable[remaining].contains(&step.1) {
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

    fn collect<E>(
        source: VId,
        bounds: GraphWalkBounds,
        adjacency: Option<&Adjacency>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Vec<GraphPath>, E> {
        let mut cursor = CapturedShortestCursor::new(source, bounds, adjacency, control)?;
        let mut output = Vec::new();
        while let Some(path) = cursor.next_with_control(control)? {
            output.push(path);
        }
        Ok(output)
    }

    // No reachability sharing, settlement or viability: enumerate all finite
    // walks, independently select each endpoint's first admissible depth, sort.
    fn oracle(source: VId, bounds: GraphWalkBounds, adjacency: &Adjacency) -> Vec<GraphPath> {
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
            if path.len() >= bounds.minimum() as usize {
                first
                    .entry(endpoint)
                    .and_modify(|depth| *depth = (*depth).min(path.len()))
                    .or_insert(path.len());
                paths.push(path);
            }
        }
        paths.retain(|path| {
            let endpoint = path.steps().last().map_or(source, |step| step.1);
            first.get(&endpoint) == Some(&path.len())
        });
        paths.sort_by(|left, right| left.len().cmp(&right.len()).then_with(|| left.cmp(right)));
        paths
    }

    #[test]
    fn captured_shortest_matches_all_unpruned_three_vertex_graphs() {
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
            for source in 0..4_u128 {
                for maximum in 0..=3 {
                    for minimum in 0..=maximum {
                        let bounds = GraphWalkBounds::new(minimum, maximum).unwrap();
                        let actual = collect(VId(source), bounds, Some(&adjacency), &mut |_| {
                            Ok::<_, ()>(())
                        })
                        .unwrap();
                        assert_eq!(
                            actual,
                            oracle(VId(source), bounds, &adjacency),
                            "mask={mask}, source={source}, bounds={bounds:?}"
                        );
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
            CapturedShortestCursor::new(VId(7), bounds, Some(&adjacency), &mut control).unwrap();
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
        assert!(cursor.frames.capacity() <= crate::MAX_GRAPH_WALK_HOPS as usize + 1);
        assert!(cursor.steps.capacity() <= crate::MAX_GRAPH_WALK_HOPS as usize);
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
        let actual = collect(VId(1), bounds, Some(&adjacency), &mut |_| Ok::<_, ()>(())).unwrap();
        assert_eq!(actual, oracle(VId(1), bounds, &adjacency));
        assert_eq!(actual.len(), 3);
        assert_eq!(actual[0], actual[1]);
        assert_eq!(actual[0].edges().collect::<Vec<_>>(), vec![EId(2), EId(8)]);
        assert_eq!(actual[2].edges().collect::<Vec<_>>(), vec![EId(9), EId(1)]);
    }

    #[test]
    fn zero_hops_never_build_or_consult_a_later_layer() {
        let adjacency = Adjacency::from([(VId(5), vec![(EId(1), VId(6)); 10_000])]);
        let mut cursor = CapturedShortestCursor::new(
            VId(5),
            GraphWalkBounds::new(0, 10).unwrap(),
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
            collect(VId(5), GraphWalkBounds::new(0, 0).unwrap(), None, &mut |_| {
                Ok::<_, ()>(())
            })
            .unwrap(),
            vec![path]
        );
        assert!(
            collect(VId(5), GraphWalkBounds::new(1, 1).unwrap(), None, &mut |_| {
                Ok::<_, ()>(())
            })
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn every_refusal_is_terminal_and_releases_all_retained_search_state() {
        let adjacency = Adjacency::from([
            (VId(1), vec![(EId(1), VId(1)), (EId(2), VId(2))]),
            (VId(2), vec![(EId(3), VId(1)), (EId(4), VId(3))]),
        ]);
        for minimum in [0, 1, 3] {
            let bounds = GraphWalkBounds::new(minimum, 4).unwrap();
            let mut total = 0;
            collect(VId(1), bounds, Some(&adjacency), &mut |_| {
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
                let mut cursor = match CapturedShortestCursor::new(
                    VId(1),
                    bounds,
                    Some(&adjacency),
                    &mut control,
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
