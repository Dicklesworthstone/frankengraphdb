//! One endpoint occurrence without enumerating tied shortest routes.
//!
//! Dominance is depth-local before the lower bound: two WALK prefixes ending
//! at the same vertex and depth have identical possible continuations. Only
//! after the lower bound may a first visit settle that vertex across depths.
//! This is valid for endpoint-only ANY shortest WALK, not ALL, captured paths,
//! edge/vertex-simple modes, costs, or predicates on the path interior.

use super::*;

impl<'a> GraphShortestWalkCursor<'a> {
    /// Select one minimum-hop occurrence for each endpoint within `bounds`.
    /// Unlike `new`, this coalesces equal-depth prefixes before expansion, so
    /// tied routes and parallel edges cannot multiply the frontier. The result
    /// contains endpoints only; it does not choose or expose a captured path.
    /// Adjacency must belong to one admitted snapshot, as for `new`.
    pub fn new_any<E>(
        source: VId,
        bounds: GraphWalkBounds,
        adjacency: Option<&'a BTreeMap<VId, Vec<VId>>>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        let mut cursor = Self::new(source, bounds, adjacency, control)?;
        cursor.one_per_endpoint = true;
        Ok(cursor)
    }

    pub(super) fn fill_unique_layer<E>(
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
        let admissible = self.depth >= self.bounds.minimum();
        let mut next = BTreeSet::new();
        // Construction and every next frontier contain each vertex at most
        // once. Sorted sets make layer order independent of adjacency order.
        for &vertex in &self.frontier {
            control(GlaExecutionEvent::Work)?;
            if admissible {
                if self.settled.contains(&vertex) {
                    continue;
                }
                // Admit both retained entries before either collection grows.
                control(GlaExecutionEvent::ScratchEntry)?;
                control(GlaExecutionEvent::ScratchEntry)?;
                self.settled.insert(vertex);
                self.pending.push(vertex);
            }
            if self.depth < self.bounds.maximum()
                && let Some(neighbors) = self.adjacency.and_then(|map| map.get(&vertex))
            {
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
        // Conversion owns separate vector storage. Charge every entry before
        // its insertion instead of hiding this allocation in collect(). A
        // refusal unwinds this local set/vector and the shared cursor boundary
        // clears every previously retained frontier, output and settlement.
        let mut frontier = Vec::new();
        for vertex in next {
            control(GlaExecutionEvent::ScratchEntry)?;
            frontier.push(vertex);
        }
        self.frontier = frontier;
        self.depth += 1;
        if self.depth > self.bounds.maximum() && self.pending.is_empty() {
            self.done = true;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect<E>(source: VId, bounds: GraphWalkBounds,
        adjacency: &BTreeMap<VId, Vec<VId>>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>) -> Result<Vec<VId>, E> {
        let mut cursor = GraphShortestWalkCursor::new_any(source, bounds, Some(adjacency), control)?;
        let mut rows = Vec::new();
        while let Some(vertex) = cursor.next_with_control(control)? { rows.push(vertex); }
        Ok(rows)
    }

    // Enumerate every raw edge occurrence, without coalescing or settlement.
    // The first admissible depth is computed independently for each endpoint.
    fn oracle(source: VId, bounds: GraphWalkBounds,
        adjacency: &BTreeMap<VId, Vec<VId>>) -> Vec<(u32, VId)> {
        let mut stack = vec![(source, 0)];
        let mut first = BTreeMap::<VId, u32>::new();
        while let Some((vertex, depth)) = stack.pop() {
            if depth >= bounds.minimum() {
                first.entry(vertex).and_modify(|old| *old = (*old).min(depth)).or_insert(depth);
            }
            if depth < bounds.maximum() {
                for &next in adjacency.get(&vertex).into_iter().flatten() {
                    stack.push((next, depth + 1));
                }
            }
        }
        let mut answer = first.into_iter().map(|(vertex, depth)| (depth, vertex)).collect::<Vec<_>>();
        answer.sort_unstable();
        answer
    }

    #[test]
    fn any_shortest_matches_unpruned_walks_at_every_admissible_depth() {
        for mask in 0..512_u32 {
            let mut adjacency = BTreeMap::<VId, Vec<VId>>::new();
            for source in 0..3_u128 {
                for destination in 0..3_u128 {
                    if mask & (1 << (source * 3 + destination)) != 0 {
                        let row = adjacency.entry(VId(source)).or_default();
                        row.push(VId(destination));
                        if source == destination { row.push(VId(destination)); }
                    }
                }
            }
            for source in 0..4_u128 {
                for maximum in 0..=4 {
                    for minimum in 0..=maximum {
                        let bounds = GraphWalkBounds::new(minimum, maximum).unwrap();
                        let expected = oracle(VId(source), bounds, &adjacency);
                        let mut cursor = GraphShortestWalkCursor::new_any(
                            VId(source), bounds, Some(&adjacency), &mut |_| Ok::<_, ()>(()),
                        ).unwrap();
                        let mut actual = Vec::new();
                        while let Some(vertex) = cursor.next_with_control(&mut |_| Ok::<_, ()>(())).unwrap() {
                            actual.push((cursor.depth - 1, vertex));
                        }
                        assert_eq!(actual, expected, "mask={mask}, source={source}, {bounds:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn lower_bound_coalescing_handles_exponential_parallel_cycles_at_maximum_depth() {
        let adjacency = BTreeMap::from([(VId(7), vec![VId(7); 8])]);
        let bounds = GraphWalkBounds::new(crate::MAX_GRAPH_WALK_HOPS, crate::MAX_GRAPH_WALK_HOPS).unwrap();
        let mut events = 0;
        let mut scratch = 0;
        let rows = collect(VId(7), bounds, &adjacency, &mut |event| {
            events += 1;
            scratch += usize::from(event == GlaExecutionEvent::ScratchEntry);
            if events > 20_000 { Err("route explosion") } else { Ok(()) }
        }).unwrap();
        assert_eq!(rows, vec![VId(7)]);
        assert!(scratch <= 2 * crate::MAX_GRAPH_WALK_HOPS as usize + 3);
        // An ordinary ALL cursor must still retain ties, not silently switch
        // semantics. The same finite event ceiling cannot enumerate 8^1024.
        let mut events = 0;
        let mut control = |_| {
            events += 1;
            if events > 20_000 { Err("route explosion") } else { Ok(()) }
        };
        let mut all = GraphShortestWalkCursor::new(VId(7), bounds, Some(&adjacency), &mut control).unwrap();
        assert_eq!(all.next_with_control(&mut control), Err("route explosion"));
    }

    #[test]
    fn layered_diamonds_expand_vertices_not_path_multiplicity() {
        let mut adjacency = BTreeMap::new();
        for level in 0..40_u128 {
            for source in [2 * level, 2 * level + 1] {
                adjacency.insert(VId(source), vec![VId(2 * level + 2), VId(2 * level + 3)]);
            }
        }
        let mut events = 0;
        let rows = collect(VId(0), GraphWalkBounds::new(1, 40).unwrap(), &adjacency, &mut |_| {
            events += 1;
            if events > 2_000 { Err("expanded routes instead of vertices") } else { Ok(()) }
        }).unwrap();
        assert_eq!(rows, (2..=81).map(VId).collect::<Vec<_>>());
    }

    #[test]
    fn any_and_all_have_distinct_tie_semantics_and_share_delivery_control() {
        let adjacency = BTreeMap::from([(VId(1), vec![VId(3), VId(2), VId(2), VId(3)])]);
        let bounds = GraphWalkBounds::new(1, 1).unwrap();
        assert_eq!(collect(VId(1), bounds, &adjacency, &mut |_| Ok::<_, ()>(())).unwrap(), vec![VId(2), VId(3)]);
        let mut cursor = GraphShortestWalkCursor::new_any(VId(1), bounds, Some(&adjacency), &mut |_| Ok::<_, ()>(())).unwrap();
        assert_eq!(cursor.next_with_control(&mut |_| Ok::<_, ()>(())), Ok(Some(VId(2))));
        let mut calls = 0;
        assert_eq!(cursor.next_with_control(&mut |event| {
            calls += 1;
            assert_eq!(event, GlaExecutionEvent::Work);
            Err("delivery cancelled")
        }), Err("delivery cancelled"));
        assert_eq!(calls, 1);
        assert_eq!(cursor.next_with_control(&mut |_| Err::<(), _>("resumed")), Ok(None));
    }

    #[test]
    fn every_unique_search_refusal_releases_state_and_is_terminal() {
        let adjacency = BTreeMap::from([
            (VId(1), vec![VId(1), VId(2), VId(2)]),
            (VId(2), vec![VId(1), VId(3)]), (VId(3), vec![VId(4)]),
        ]);
        for minimum in [0, 1, 3] {
            let bounds = GraphWalkBounds::new(minimum, 4).unwrap();
            let mut total = 0;
            collect(VId(1), bounds, &adjacency, &mut |_| { total += 1; Ok::<_, usize>(()) }).unwrap();
            for stop in 2..=total {
                let mut seen = 0;
                let mut control = |_| { seen += 1; if seen == stop { Err(stop) } else { Ok(()) } };
                let mut cursor = GraphShortestWalkCursor::new_any(VId(1), bounds, Some(&adjacency), &mut control).unwrap();
                loop {
                    match cursor.next_with_control(&mut control) {
                        Ok(Some(_)) => {}
                        Ok(None) => panic!("missed refusal {stop}"),
                        Err(error) => { assert_eq!(error, stop); break; }
                    }
                }
                assert_eq!(seen, stop);
                assert!(cursor.done);
                assert!(cursor.settled.is_empty());
                assert!(cursor.frontier.is_empty());
                assert!(cursor.pending.is_empty());
                assert_eq!(cursor.frontier.capacity(), 0);
                assert_eq!(cursor.pending.capacity(), 0);
                assert_eq!(cursor.pending_at, 0);
                for _ in 0..3 {
                    assert_eq!(cursor.next_with_control(&mut |_| Err::<(), _>(usize::MAX)), Ok(None));
                }
            }
        }
    }
}
