//! Bounded breadth-first shortest-walk expansion over admitted sorted adjacency.
//!
//! For each endpoint, emit every walk occurrence at its first admissible depth
//! and never emit a longer alternative. Parallel edges and equal-length routes
//! therefore retain multiplicity. A lower hop bound delays settlement: a vertex
//! reached too early may still be reached by a shortest walk satisfying the
//! requested interval. This cursor returns endpoints only; it is not yet a GQL
//! path value, TRAIL/SIMPLE selector, weighted shortest path, or path witness.
//!
//! ALL keeps a shared reachable-vertex layer rather than one frontier entry per
//! path occurrence. A backward viability pass and an iterative, reusable frame
//! array enumerate each layer lazily in the original adjacency-occurrence order.
//! Multiplicity is structural: no fixed-width path count can overflow or saturate.
//! Retained traversal state is O(hops * reachable vertices), not O(path count).
//! Output can still be exponential; each occurrence remains individually governed.
//! This is query scratch over an admitted index, not larger-than-memory storage.

use crate::{GlaExecutionEvent, GraphWalkBounds};
use fgdb_types::VId;
use std::collections::{BTreeMap, BTreeSet};

mod unique;

/// Iterative BFS frontier governed by the same logical work/scratch controls as
/// ordinary WALK. The adjacency is derived query scratch from one admitted
/// snapshot. Duplicate neighbors are distinct edge occurrences.
pub struct GraphShortestWalkCursor<'a> {
    adjacency: Option<&'a BTreeMap<VId, Vec<VId>>>,
    bounds: GraphWalkBounds,
    depth: u32,
    frontier: Vec<VId>,
    settled: BTreeSet<VId>,
    pending: Vec<VId>,
    pending_at: usize,
    done: bool,
    one_per_endpoint: bool,
    all: AllShortestLayers,
}

#[derive(Clone, Copy)]
struct WalkFrame {
    vertex: VId,
    next_neighbor: usize,
}

/// The layer graph shares every (depth, vertex) across all path occurrences.
/// `viable[k]` contains vertices with an eligible continuation of exactly k
/// hops to the current output layer. It removes dead suffixes before traversal;
/// otherwise even lazy DFS could enumerate exponentially many rejected prefixes.
#[derive(Default)]
struct AllShortestLayers {
    layers: Vec<BTreeSet<VId>>,
    viable: Vec<BTreeSet<VId>>,
    frames: Vec<WalkFrame>,
    frame_at: usize,
    walking: bool,
}

impl AllShortestLayers {
    fn prepare_output<E>(
        &mut self,
        adjacency: Option<&BTreeMap<VId, Vec<VId>>>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        self.viable = Vec::new();
        self.frames = Vec::new();
        self.frame_at = 0;
        self.walking = false;
        // Rebuild only Boolean reachability, never a bag of partial paths.
        // Across all result depths this may revisit O(hops^2) layers; the work
        // is governed and independent of tied-route multiplicity.
        for layer in self.layers.iter().rev() {
            control(GlaExecutionEvent::Work)?;
            let mut live = BTreeSet::new();
            for &vertex in layer {
                control(GlaExecutionEvent::Work)?;
                let mut reaches_output = self.viable.is_empty();
                if let Some(suffix) = self.viable.last()
                    && let Some(neighbors) = adjacency.and_then(|map| map.get(&vertex))
                {
                    for destination in neighbors {
                        control(GlaExecutionEvent::Work)?;
                        if suffix.contains(destination) {
                            reaches_output = true;
                            break;
                        }
                    }
                }
                if reaches_output {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    live.insert(vertex);
                }
            }
            control(GlaExecutionEvent::ScratchEntry)?;
            self.viable.push(live);
        }
        // The first layer has exactly the construction source. An empty root
        // viability set means there is no output, not an invented zero-hop row.
        if let Some(&source) = self.viable.last().and_then(BTreeSet::first) {
            for _ in 0..self.layers.len() {
                control(GlaExecutionEvent::ScratchEntry)?;
                self.frames.push(WalkFrame {
                    vertex: source,
                    next_neighbor: 0,
                });
            }
            self.walking = true;
        }
        Ok(())
    }

    fn next<E>(
        &mut self,
        adjacency: Option<&BTreeMap<VId, Vec<VId>>>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<VId>, E> {
        while self.walking {
            // Includes every delivery and backtrack. Cancellation between pulls
            // cannot be bypassed by a previously admitted layer or frame array.
            control(GlaExecutionEvent::Work)?;
            let frame = self.frames[self.frame_at];
            if self.frame_at + 1 == self.frames.len() {
                if self.frame_at == 0 {
                    self.walking = false;
                } else {
                    self.frame_at -= 1;
                }
                return Ok(Some(frame.vertex));
            }
            let destination = adjacency
                .and_then(|map| map.get(&frame.vertex))
                .and_then(|neighbors| neighbors.get(frame.next_neighbor));
            let Some(&destination) = destination else {
                if self.frame_at == 0 {
                    self.walking = false;
                } else {
                    self.frame_at -= 1;
                }
                continue;
            };
            self.frames[self.frame_at].next_neighbor += 1;
            let remaining = self.viable.len() - self.frame_at - 2;
            if self.viable[remaining].contains(&destination) {
                self.frame_at += 1;
                // Every slot was admitted before the first output. Reuse it
                // instead of allocating a stack entry for each path occurrence.
                self.frames[self.frame_at] = WalkFrame {
                    vertex: destination,
                    next_neighbor: 0,
                };
            }
        }
        Ok(None)
    }
}

impl<'a> GraphShortestWalkCursor<'a> {
    pub fn new<E>(
        source: VId,
        bounds: GraphWalkBounds,
        adjacency: Option<&'a BTreeMap<VId, Vec<VId>>>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        control(GlaExecutionEvent::ScratchEntry)?;
        Ok(Self {
            adjacency,
            bounds,
            depth: 0,
            frontier: vec![source],
            settled: BTreeSet::new(),
            pending: Vec::new(),
            pending_at: 0,
            done: false,
            one_per_endpoint: false,
            all: AllShortestLayers::default(),
        })
    }

    /// Return one endpoint occurrence. A complete reachability layer is admitted
    /// before its first result, but ALL does not materialize its occurrence bag.
    /// Source/work/scratch refusal returns no fabricated row. Every endpoint
    /// delivery and every lazy traversal step charges a work event.
    /// A refusal is terminal: discard retained traversal state and return `None`
    /// on later calls without invoking the controller again.
    pub fn next_with_control<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<VId>, E> {
        let result = self.next_controlled(control);
        if result.is_err() {
            // A layer may already have buffered rows or installed settlement
            // markers. Neither can safely survive a failed layer transition.
            self.done = true;
            self.frontier = Vec::new();
            self.pending = Vec::new();
            self.pending_at = 0;
            self.settled.clear();
            self.all = AllShortestLayers::default();
        }
        result
    }

    fn next_controlled<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<VId>, E> {
        loop {
            if let Some(&value) = self.pending.get(self.pending_at) {
                // Layer admission does not authorize later delivery: the caller
                // may have cancelled or exhausted its budget between pulls.
                control(GlaExecutionEvent::Work)?;
                self.pending_at += 1;
                return Ok(Some(value));
            }
            if !self.one_per_endpoint
                && let Some(value) = self.all.next(self.adjacency, control)?
            {
                return Ok(Some(value));
            }
            if self.done {
                return Ok(None);
            }
            self.fill_layer(control)?;
        }
    }

    fn fill_layer<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        if self.one_per_endpoint {
            return self.fill_unique_layer(control);
        }
        control(GlaExecutionEvent::Work)?;
        if self.depth > self.bounds.maximum() || self.frontier.is_empty() {
            self.done = true;
            self.frontier = Vec::new();
            self.settled.clear();
            self.all = AllShortestLayers::default();
            return Ok(());
        }

        let admissible = self.depth >= self.bounds.minimum();
        let mut active = BTreeSet::new();
        // Before the minimum, a vertex may recur at later depths. At and after
        // the minimum, its first eligible layer settles it. Sharing that layer
        // does NOT remove tied paths: the lazy walk uses real adjacency entries.
        for &vertex in &self.frontier {
            control(GlaExecutionEvent::Work)?;
            if admissible && self.settled.contains(&vertex) {
                continue;
            }
            control(GlaExecutionEvent::ScratchEntry)?;
            active.insert(vertex);
            if admissible {
                control(GlaExecutionEvent::ScratchEntry)?;
                self.settled.insert(vertex);
            }
        }

        let mut next = BTreeSet::new();
        if self.depth < self.bounds.maximum() {
            for &vertex in &active {
                control(GlaExecutionEvent::Work)?;
                if let Some(neighbors) = self.adjacency.and_then(|map| map.get(&vertex)) {
                    for &destination in neighbors {
                        control(GlaExecutionEvent::Work)?;
                        if self.settled.contains(&destination) || next.contains(&destination) {
                            continue;
                        }
                        control(GlaExecutionEvent::ScratchEntry)?;
                        next.insert(destination);
                    }
                }
            }
        }
        let mut frontier = Vec::new();
        for vertex in next {
            control(GlaExecutionEvent::ScratchEntry)?;
            frontier.push(vertex);
        }
        self.frontier = frontier;
        control(GlaExecutionEvent::ScratchEntry)?;
        self.all.layers.push(active);
        if admissible {
            self.all.prepare_output(self.adjacency, control)?;
        }
        self.depth += 1;
        Ok(())
    }
}

impl core::fmt::Debug for GraphShortestWalkCursor<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphShortestWalkCursor")
            .field("bounds", &self.bounds)
            .field("depth", &self.depth)
            .field("frontier", &self.frontier.len())
            .field("settled", &self.settled.len())
            .field("graph", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect<E>(
        source: VId,
        bounds: GraphWalkBounds,
        adjacency: Option<&BTreeMap<VId, Vec<VId>>>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Vec<VId>, E> {
        let mut cursor = GraphShortestWalkCursor::new(source, bounds, adjacency, control)?;
        let mut result = Vec::new();
        while let Some(vertex) = cursor.next_with_control(control)? {
            result.push(vertex);
        }
        Ok(result)
    }

    /// Enumerate every bounded walk independently, then choose each endpoint's
    /// minimum admissible depth. This oracle has no settled set or BFS pruning.
    fn oracle(
        source: VId,
        bounds: GraphWalkBounds,
        adjacency: &BTreeMap<VId, Vec<VId>>,
    ) -> Vec<VId> {
        let mut stack = vec![(source, 0_u32)];
        let mut walks = Vec::new();
        while let Some((vertex, depth)) = stack.pop() {
            if depth >= bounds.minimum() {
                walks.push((depth, vertex));
            }
            if depth < bounds.maximum() {
                for &next in adjacency.get(&vertex).into_iter().flatten() {
                    stack.push((next, depth + 1));
                }
            }
        }
        let mut minimum = BTreeMap::<VId, u32>::new();
        for &(depth, vertex) in &walks {
            minimum
                .entry(vertex)
                .and_modify(|old| *old = (*old).min(depth))
                .or_insert(depth);
        }
        let mut result = walks
            .into_iter()
            .filter_map(|(depth, vertex)| (minimum.get(&vertex) == Some(&depth)).then_some(vertex))
            .collect::<Vec<_>>();
        result.sort_unstable();
        result
    }

    #[test]
    fn exhaustive_small_multigraph_matches_full_walk_enumeration() {
        for mask in 0..512_u32 {
            let mut adjacency = BTreeMap::<VId, Vec<VId>>::new();
            for source in 0..3_u128 {
                for destination in 0..3_u128 {
                    if mask & (1 << (3 * source + destination)) != 0 {
                        adjacency
                            .entry(VId(source))
                            .or_default()
                            .push(VId(destination));
                    }
                }
            }
            if let Some(neighbors) = adjacency.values_mut().find(|rows| !rows.is_empty()) {
                neighbors.push(neighbors[0]);
            }
            for neighbors in adjacency.values_mut() {
                neighbors.sort_unstable();
            }
            for source in 0..4_u128 {
                for maximum in 0..=3 {
                    for minimum in 0..=maximum {
                        let bounds = GraphWalkBounds::new(minimum, maximum).unwrap();
                        let mut actual =
                            collect(VId(source), bounds, Some(&adjacency), &mut |_| {
                                Ok::<_, ()>(())
                            })
                            .unwrap();
                        actual.sort_unstable();
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
    fn equal_shortest_routes_and_parallel_edges_keep_multiplicity() {
        let adjacency = BTreeMap::from([
            (VId(1), vec![VId(2), VId(2), VId(3)]),
            (VId(2), vec![VId(4)]),
            (VId(3), vec![VId(4)]),
            (VId(4), vec![VId(4)]),
        ]);
        let rows = collect(
            VId(1),
            GraphWalkBounds::new(1, 4).unwrap(),
            Some(&adjacency),
            &mut |_| Ok::<_, ()>(()),
        )
        .unwrap();
        let counts = rows
            .into_iter()
            .fold(BTreeMap::<VId, usize>::new(), |mut out, value| {
                *out.entry(value).or_default() += 1;
                out
            });
        assert_eq!(counts.get(&VId(2)), Some(&2));
        assert_eq!(counts.get(&VId(3)), Some(&1));
        assert_eq!(counts.get(&VId(4)), Some(&3));
    }

    #[test]
    fn lower_bound_delays_settlement_instead_of_erasing_valid_longer_paths() {
        let adjacency = BTreeMap::from([(VId(1), vec![VId(2)]), (VId(2), vec![VId(1)])]);
        assert_eq!(
            collect(
                VId(1),
                GraphWalkBounds::new(2, 3).unwrap(),
                Some(&adjacency),
                &mut |_| Ok::<_, ()>(())
            )
            .unwrap(),
            vec![VId(1), VId(2)]
        );
        assert_eq!(
            collect(
                VId(1),
                GraphWalkBounds::new(0, 3).unwrap(),
                Some(&adjacency),
                &mut |_| Ok::<_, ()>(())
            )
            .unwrap(),
            vec![VId(1), VId(2)]
        );
    }

    #[test]
    fn maximum_depth_is_iterative_and_cycle_growth_is_pruned_after_settlement() {
        let adjacency = BTreeMap::from([(VId(7), vec![VId(7)])]);
        let bounds =
            GraphWalkBounds::new(crate::MAX_GRAPH_WALK_HOPS, crate::MAX_GRAPH_WALK_HOPS).unwrap();
        assert_eq!(
            collect(VId(7), bounds, Some(&adjacency), &mut |_| Ok::<_, ()>(())).unwrap(),
            vec![VId(7)]
        );
        assert_eq!(
            collect(
                VId(7),
                GraphWalkBounds::new(0, crate::MAX_GRAPH_WALK_HOPS).unwrap(),
                Some(&adjacency),
                &mut |_| Ok::<_, ()>(())
            )
            .unwrap(),
            vec![VId(7)]
        );
    }

    #[test]
    fn every_work_and_growth_boundary_is_interruptible() {
        let adjacency = BTreeMap::from([
            (VId(1), vec![VId(2), VId(2), VId(3)]),
            (VId(2), vec![VId(3), VId(4)]),
            (VId(3), vec![VId(4)]),
        ]);
        let bounds = GraphWalkBounds::new(1, 3).unwrap();
        let mut calls = 0;
        let expected = collect(VId(1), bounds, Some(&adjacency), &mut |_| {
            calls += 1;
            Ok::<_, usize>(())
        })
        .unwrap();
        assert!(!expected.is_empty());
        for stop in 1..=calls {
            let mut seen = 0;
            let result = collect(VId(1), bounds, Some(&adjacency), &mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            });
            assert_eq!(result, Err(stop));
            assert_eq!(seen, stop);
        }
        assert_eq!(
            collect(VId(1), bounds, Some(&adjacency), &mut |_| Ok::<_, ()>(())).unwrap(),
            expected
        );
    }

    #[test]
    fn every_refusal_discards_partial_state_and_fuses_the_cursor() {
        let adjacency = BTreeMap::from([
            (VId(1), vec![VId(2), VId(2), VId(3)]),
            (VId(2), vec![VId(3), VId(4)]),
            (VId(3), vec![VId(4)]),
        ]);
        for minimum in [0, 1, 3] {
            let bounds = GraphWalkBounds::new(minimum, 3).unwrap();
            let mut calls = 0;
            collect(VId(1), bounds, Some(&adjacency), &mut |_| {
                calls += 1;
                Ok::<_, usize>(())
            })
            .unwrap();

            // The first boundary is construction; no cursor exists to resume
            // after a construction refusal. Exercise every subsequent boundary.
            for stop in 2..=calls {
                let mut seen = 0;
                let mut control = |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                };
                let mut cursor =
                    GraphShortestWalkCursor::new(VId(1), bounds, Some(&adjacency), &mut control)
                        .unwrap();
                loop {
                    match cursor.next_with_control(&mut control) {
                        Ok(Some(_)) => {}
                        Ok(None) => panic!("missed refusal at boundary {stop}"),
                        Err(error) => {
                            assert_eq!(error, stop);
                            break;
                        }
                    }
                }
                assert_eq!(seen, stop);
                assert!(cursor.done);
                assert!(cursor.frontier.is_empty());
                assert!(cursor.pending.is_empty());
                assert!(cursor.settled.is_empty());
                assert_eq!(cursor.pending_at, 0);
                assert_eq!(cursor.frontier.capacity(), 0);
                assert_eq!(cursor.pending.capacity(), 0);
                assert!(cursor.all.layers.is_empty());
                assert!(cursor.all.viable.is_empty());
                assert!(cursor.all.frames.is_empty());
                assert_eq!(cursor.all.layers.capacity(), 0);
                assert_eq!(cursor.all.viable.capacity(), 0);
                assert_eq!(cursor.all.frames.capacity(), 0);
                assert!(!cursor.all.walking);
                assert_eq!(cursor.all.frame_at, 0);

                for _ in 0..3 {
                    // Any callback after refusal would return this sentinel.
                    assert_eq!(
                        cursor.next_with_control(&mut |_| Err::<(), _>(usize::MAX)),
                        Ok(None)
                    );
                }
            }
        }
    }

    #[test]
    fn cancellation_between_factorized_results_returns_no_more_rows() {
        let adjacency = BTreeMap::from([(VId(1), vec![VId(2), VId(3)])]);
        let bounds = GraphWalkBounds::new(1, 1).unwrap();
        let mut cursor = GraphShortestWalkCursor::new(
            VId(1),
            bounds,
            Some(&adjacency),
            &mut |_| Ok::<_, ()>(()),
        )
        .unwrap();
        assert_eq!(
            cursor.next_with_control(&mut |_| Ok::<_, ()>(())),
            Ok(Some(VId(2)))
        );
        assert!(cursor.pending.is_empty());
        assert!(cursor.all.walking);
        assert_eq!(cursor.all.frames.len(), 2);

        let mut checks = 0;
        assert_eq!(
            cursor.next_with_control(&mut |event| {
                checks += 1;
                assert!(matches!(event, GlaExecutionEvent::Work));
                Err("cancelled")
            }),
            Err("cancelled")
        );
        assert_eq!(checks, 1);
        assert_eq!(
            cursor.next_with_control(&mut |_| Err::<(), _>("must not resume")),
            Ok(None)
        );
        assert!(cursor.pending.is_empty());
        assert!(cursor.frontier.is_empty());
        assert!(cursor.settled.is_empty());
        assert!(cursor.all.frames.is_empty());
        assert!(cursor.all.layers.is_empty());
        assert!(cursor.all.viable.is_empty());
    }

    #[test]
    fn each_factorized_parallel_occurrence_charges_delivery_work() {
        let adjacency = BTreeMap::from([(VId(1), vec![VId(2), VId(3), VId(3)])]);
        let bounds = GraphWalkBounds::new(1, 1).unwrap();
        let mut cursor = GraphShortestWalkCursor::new(
            VId(1),
            bounds,
            Some(&adjacency),
            &mut |_| Ok::<_, ()>(()),
        )
        .unwrap();
        assert_eq!(
            cursor.next_with_control(&mut |_| Ok::<_, ()>(())),
            Ok(Some(VId(2)))
        );

        for remaining in (0..2).rev() {
            let mut work = 0;
            assert_eq!(
                cursor.next_with_control(&mut |event| {
                    assert!(matches!(event, GlaExecutionEvent::Work));
                    work += 1;
                    Ok::<_, ()>(())
                }),
                Ok(Some(VId(3)))
            );
            // One neighbor step and one delivery; neither allocates another
            // frame or materializes the remaining parallel occurrences.
            assert_eq!(work, 2);
            assert!(cursor.pending.is_empty());
            assert_eq!(cursor.all.frames[0].next_neighbor, 3 - remaining);
        }
        assert_eq!(cursor.next_with_control(&mut |_| Ok::<_, ()>(())), Ok(None));
    }

    /// Independent unpruned BFS: every raw walk enters the queue, including
    /// walks through an endpoint already returned at a shorter admissible depth.
    /// This oracle checks order as well as the multiset, without layer sharing.
    fn ordered_oracle(
        source: VId,
        bounds: GraphWalkBounds,
        adjacency: &BTreeMap<VId, Vec<VId>>,
    ) -> Vec<(u32, VId)> {
        let mut queue = std::collections::VecDeque::from([(source, 0_u32)]);
        let mut first = BTreeMap::new();
        let mut output = Vec::new();
        while let Some((vertex, depth)) = queue.pop_front() {
            if depth >= bounds.minimum() && *first.entry(vertex).or_insert(depth) == depth {
                output.push((depth, vertex));
            }
            if depth < bounds.maximum() {
                for &neighbor in adjacency.get(&vertex).into_iter().flatten() {
                    queue.push_back((neighbor, depth + 1));
                }
            }
        }
        output
    }

    #[test]
    fn shared_layers_preserve_exact_bfs_occurrence_order_on_every_small_multigraph() {
        for mask in 0..512_u32 {
            let mut adjacency = BTreeMap::<VId, Vec<VId>>::new();
            for source in 0..3_u128 {
                for destination in 0..3_u128 {
                    if mask & (1 << (3 * source + destination)) != 0 {
                        adjacency.entry(VId(source)).or_default().push(VId(destination));
                    }
                }
            }
            // Noncontiguous duplicates and unsorted neighbor lists prove that
            // neither set ordering nor run-length assumptions choose row order.
            for neighbors in adjacency.values_mut() {
                neighbors.push(neighbors[0]);
                neighbors.reverse();
            }
            for source in 0..4_u128 {
                for maximum in 0..=4 {
                    for minimum in 0..=maximum {
                        let bounds = GraphWalkBounds::new(minimum, maximum).unwrap();
                        let mut cursor = GraphShortestWalkCursor::new(
                            VId(source),
                            bounds,
                            Some(&adjacency),
                            &mut |_| Ok::<_, ()>(()),
                        )
                        .unwrap();
                        let mut actual = Vec::new();
                        while let Some(vertex) =
                            cursor.next_with_control(&mut |_| Ok::<_, ()>(())).unwrap()
                        {
                            actual.push((cursor.depth - 1, vertex));
                        }
                        assert_eq!(
                            actual,
                            ordered_oracle(VId(source), bounds, &adjacency),
                            "mask={mask}, source={source}, {bounds:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn exponential_maximum_depth_ties_stream_without_count_overflow_or_bag_storage() {
        use std::cell::Cell;
        let adjacency = BTreeMap::from([(VId(7), vec![VId(7); 8])]);
        let hops = crate::MAX_GRAPH_WALK_HOPS;
        let bounds = GraphWalkBounds::new(hops, hops).unwrap();
        let work = Cell::new(0_u64);
        let scratch = Cell::new(0_u64);
        let mut control = |event| {
            if event == GlaExecutionEvent::ScratchEntry {
                scratch.set(scratch.get() + 1);
            } else {
                assert_eq!(event, GlaExecutionEvent::Work);
                work.set(work.get() + 1);
            }
            if work.get() + scratch.get() > 100_000 {
                Err("enumerated tied prefixes before first output")
            } else {
                Ok(())
            }
        };
        let mut cursor =
            GraphShortestWalkCursor::new(VId(7), bounds, Some(&adjacency), &mut control).unwrap();
        assert_eq!(cursor.next_with_control(&mut control), Ok(Some(VId(7))));
        let admitted = scratch.get();
        // The complete bag has 8^1024 occurrences, more than any machine-word
        // count. Only consume a prefix; ALL must neither overflow nor become ANY.
        for _ in 0..31 {
            assert_eq!(cursor.next_with_control(&mut control), Ok(Some(VId(7))));
            assert_eq!(scratch.get(), admitted, "delivery allocated another path");
        }
        assert!(work.get() < 40_000);
        assert!(scratch.get() <= 8 * (u64::from(hops) + 1));
        assert!(cursor.pending.is_empty());
        assert!(cursor.frontier.is_empty());
        assert_eq!(cursor.all.layers.len(), hops as usize + 1);
        assert_eq!(cursor.all.viable.len(), hops as usize + 1);
        assert_eq!(cursor.all.frames.len(), hops as usize + 1);
        assert!(cursor.all.layers.iter().all(|layer| layer.len() == 1));
        assert!(cursor.all.viable.iter().all(|layer| layer.len() == 1));
        assert_eq!(
            cursor.next_with_control(&mut |_| Err("cancelled after prefix")),
            Err("cancelled after prefix")
        );
        assert_eq!(cursor.all.layers.capacity(), 0);
        assert_eq!(cursor.all.viable.capacity(), 0);
        assert_eq!(cursor.all.frames.capacity(), 0);
        assert_eq!(cursor.next_with_control(&mut |_| Err::<(), _>("resumed")), Ok(None));
    }

    #[test]
    fn backward_viability_skips_exponential_dead_prefixes() {
        let mut adjacency = BTreeMap::from([(VId(0), vec![VId(1), VId(100)])]);
        for source in 1..20_u128 {
            adjacency.insert(VId(source), vec![VId(source + 1); 8]);
        }
        for source in 100..140_u128 {
            adjacency.insert(VId(source), vec![VId(source + 1)]);
        }
        let mut events = 0;
        let rows = collect(
            VId(0),
            GraphWalkBounds::new(41, 41).unwrap(),
            Some(&adjacency),
            &mut |_| {
                events += 1;
                if events > 5_000 {
                    Err("enumerated dead paths")
                } else {
                    Ok(())
                }
            },
        )
        .unwrap();
        assert_eq!(rows, vec![VId(140)]);
    }

    #[test]
    fn exact_work_and_scratch_limits_succeed_and_one_below_refuses() {
        let adjacency = BTreeMap::from([
            (VId(1), vec![VId(3), VId(2), VId(2)]),
            (VId(2), vec![VId(1), VId(3)]),
            (VId(3), vec![VId(2), VId(4)]),
        ]);
        let bounds = GraphWalkBounds::new(2, 4).unwrap();
        let mut measured = [0_u64; 2];
        let expected = collect(VId(1), bounds, Some(&adjacency), &mut |event| {
            measured[usize::from(event == GlaExecutionEvent::ScratchEntry)] += 1;
            Ok::<_, ()>(())
        })
        .unwrap();
        for dimension in 0..2 {
            for short in [false, true] {
                let mut limits = measured;
                limits[dimension] -= u64::from(short);
                let mut seen = [0_u64; 2];
                let result = collect(VId(1), bounds, Some(&adjacency), &mut |event| {
                    let at = usize::from(event == GlaExecutionEvent::ScratchEntry);
                    seen[at] += 1;
                    if seen[at] > limits[at] { Err(at) } else { Ok(()) }
                });
                if short {
                    assert_eq!(result, Err(dimension));
                } else {
                    assert_eq!(result, Ok(expected.clone()));
                    assert_eq!(seen, measured);
                }
            }
        }
    }
}
