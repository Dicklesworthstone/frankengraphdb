//! Bounded breadth-first shortest-walk expansion over admitted sorted adjacency.
//!
//! For each endpoint, emit every walk occurrence at its first admissible depth
//! and never emit a longer alternative. Parallel edges and equal-length routes
//! therefore retain multiplicity. A lower hop bound delays settlement: a vertex
//! reached too early may still be reached by a shortest walk satisfying the
//! requested interval. This cursor returns endpoints only; it is not yet a GQL
//! path value, TRAIL/SIMPLE selector, weighted shortest path, or path witness.

use crate::{GlaExecutionEvent, GraphWalkBounds};
use fgdb_types::VId;
use std::collections::{BTreeMap, BTreeSet};

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
        })
    }

    /// Return one endpoint occurrence. A complete layer is processed before its
    /// first result is released so equal-depth alternatives cannot be mistaken
    /// for longer paths. Source/work/scratch refusal returns no fabricated row.
    pub fn next_with_control<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<VId>, E> {
        loop {
            if let Some(&value) = self.pending.get(self.pending_at) {
                self.pending_at += 1;
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
        control(GlaExecutionEvent::Work)?;
        if self.depth > self.bounds.maximum() || self.frontier.is_empty() {
            self.done = true;
            self.pending.clear();
            self.pending_at = 0;
            return Ok(());
        }

        self.pending.clear();
        self.pending_at = 0;
        let mut newly_settled = BTreeSet::new();
        if self.depth >= self.bounds.minimum() {
            for &vertex in &self.frontier {
                control(GlaExecutionEvent::Work)?;
                if self.settled.contains(&vertex) {
                    continue;
                }
                control(GlaExecutionEvent::ScratchEntry)?;
                self.pending.push(vertex);
                if newly_settled.insert(vertex) {
                    // The temporary layer set and the retained settled set each
                    // own one logical entry. Charge both before either can grow.
                    control(GlaExecutionEvent::ScratchEntry)?;
                    control(GlaExecutionEvent::ScratchEntry)?;
                    let inserted = self.settled.insert(vertex);
                    debug_assert!(inserted);
                }
            }
        }

        let mut next = Vec::new();
        if self.depth < self.bounds.maximum() {
            for &vertex in &self.frontier {
                control(GlaExecutionEvent::Work)?;
                // A vertex settled on an EARLIER admissible layer cannot lie on
                // a shortest continuation. Same-layer shortest occurrences all
                // expand so multiplicity reaches downstream endpoints exactly.
                if self.settled.contains(&vertex) && !newly_settled.contains(&vertex) {
                    continue;
                }
                if let Some(neighbors) = self.adjacency.and_then(|map| map.get(&vertex)) {
                    for &destination in neighbors {
                        control(GlaExecutionEvent::Work)?;
                        control(GlaExecutionEvent::ScratchEntry)?;
                        next.push(destination);
                    }
                }
            }
        }
        self.frontier = next;
        self.depth += 1;
        if self.depth > self.bounds.maximum() && self.pending.is_empty() {
            self.done = true;
        }
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

    fn collect<E>(source: VId, bounds: GraphWalkBounds,
        adjacency: Option<&BTreeMap<VId, Vec<VId>>>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>) -> Result<Vec<VId>, E> {
        let mut cursor = GraphShortestWalkCursor::new(source, bounds, adjacency, control)?;
        let mut result = Vec::new();
        while let Some(vertex) = cursor.next_with_control(control)? { result.push(vertex); }
        Ok(result)
    }

    /// Enumerate every bounded walk independently, then choose each endpoint's
    /// minimum admissible depth. This oracle has no settled set or BFS pruning.
    fn oracle(source: VId, bounds: GraphWalkBounds,
        adjacency: &BTreeMap<VId, Vec<VId>>) -> Vec<VId> {
        let mut stack = vec![(source, 0_u32)];
        let mut walks = Vec::new();
        while let Some((vertex, depth)) = stack.pop() {
            if depth >= bounds.minimum() { walks.push((depth, vertex)); }
            if depth < bounds.maximum() {
                for &next in adjacency.get(&vertex).into_iter().flatten() {
                    stack.push((next, depth + 1));
                }
            }
        }
        let mut minimum = BTreeMap::<VId, u32>::new();
        for &(depth, vertex) in &walks {
            minimum.entry(vertex).and_modify(|old| *old = (*old).min(depth)).or_insert(depth);
        }
        let mut result = walks.into_iter()
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
                        adjacency.entry(VId(source)).or_default().push(VId(destination));
                    }
                }
            }
            if let Some(neighbors) = adjacency.values_mut().find(|rows| !rows.is_empty()) {
                neighbors.push(neighbors[0]);
            }
            for neighbors in adjacency.values_mut() { neighbors.sort_unstable(); }
            for source in 0..4_u128 {
                for maximum in 0..=3 {
                    for minimum in 0..=maximum {
                        let bounds = GraphWalkBounds::new(minimum, maximum).unwrap();
                        let mut actual = collect(VId(source), bounds, Some(&adjacency), &mut |_| Ok::<_, ()>(())).unwrap();
                        actual.sort_unstable();
                        assert_eq!(actual, oracle(VId(source), bounds, &adjacency),
                            "mask={mask}, source={source}, bounds={bounds:?}");
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
        let rows = collect(VId(1), GraphWalkBounds::new(1, 4).unwrap(), Some(&adjacency),
            &mut |_| Ok::<_, ()>(())).unwrap();
        let counts = rows.into_iter().fold(BTreeMap::<VId, usize>::new(), |mut out, value| {
            *out.entry(value).or_default() += 1; out
        });
        assert_eq!(counts.get(&VId(2)), Some(&2));
        assert_eq!(counts.get(&VId(3)), Some(&1));
        assert_eq!(counts.get(&VId(4)), Some(&3));
    }

    #[test]
    fn lower_bound_delays_settlement_instead_of_erasing_valid_longer_paths() {
        let adjacency = BTreeMap::from([
            (VId(1), vec![VId(2)]),
            (VId(2), vec![VId(1)]),
        ]);
        assert_eq!(
            collect(VId(1), GraphWalkBounds::new(2, 3).unwrap(), Some(&adjacency),
                &mut |_| Ok::<_, ()>(())).unwrap(),
            vec![VId(1), VId(2)]
        );
        assert_eq!(
            collect(VId(1), GraphWalkBounds::new(0, 3).unwrap(), Some(&adjacency),
                &mut |_| Ok::<_, ()>(())).unwrap(),
            vec![VId(1), VId(2)]
        );
    }

    #[test]
    fn maximum_depth_is_iterative_and_cycle_growth_is_pruned_after_settlement() {
        let adjacency = BTreeMap::from([(VId(7), vec![VId(7)])]);
        let bounds = GraphWalkBounds::new(crate::MAX_GRAPH_WALK_HOPS, crate::MAX_GRAPH_WALK_HOPS).unwrap();
        assert_eq!(collect(VId(7), bounds, Some(&adjacency), &mut |_| Ok::<_, ()>(())).unwrap(), vec![VId(7)]);
        assert_eq!(collect(VId(7), GraphWalkBounds::new(0, crate::MAX_GRAPH_WALK_HOPS).unwrap(),
            Some(&adjacency), &mut |_| Ok::<_, ()>(())).unwrap(), vec![VId(7)]);
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
            calls += 1; Ok::<_, usize>(())
        }).unwrap();
        assert!(!expected.is_empty());
        for stop in 1..=calls {
            let mut seen = 0;
            let result = collect(VId(1), bounds, Some(&adjacency), &mut |_| {
                seen += 1; if seen == stop { Err(stop) } else { Ok(()) }
            });
            assert_eq!(result, Err(stop));
            assert_eq!(seen, stop);
        }
        assert_eq!(collect(VId(1), bounds, Some(&adjacency), &mut |_| Ok::<_, ()>(())).unwrap(), expected);
    }
}
