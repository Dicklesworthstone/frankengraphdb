//! Bounded WALK expansion over an already admitted, sorted adjacency index.
//!
//! Every edge occurrence is significant. Repeated vertices and edges are legal;
//! this is neither TRAIL/SIMPLE enumeration nor DISTINCT reachability. The cursor
//! retains only one path frontier and crosses the ordinary GLA control seam
//! before work and frame growth. No database observation or authorization is
//! performed here; the caller owns the immutable, admitted graph generation.

use crate::GlaExecutionEvent;
use crate::algebra::{GraphPath, GraphWalkSearch};
use fgdb_types::{EId, VId};
use std::collections::BTreeMap;

/// Definition limit, not a guarantee of inexpensive enumeration. Runtime work
/// and scratch budgets still govern every visited path occurrence.
pub const MAX_GRAPH_WALK_HOPS: u32 = 1_024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphWalkBoundsError {
    Reversed,
    TooManyHops { limit: u32, observed: u32 },
}
impl core::fmt::Display for GraphWalkBoundsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Reversed => f.write_str("walk minimum exceeds its maximum"),
            Self::TooManyHops { limit, observed } => {
                write!(f, "walk hop limit exceeded: {observed} > {limit}")
            }
        }
    }
}
impl core::error::Error for GraphWalkBoundsError {}

/// Inclusive, finite hop interval. Private fields make unbounded or reversed
/// intervals unrepresentable in an executable walk. Zero hops retain the source
/// identity, including an isolated vertex whose adjacency entry is absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphWalkBounds {
    minimum: u32,
    maximum: u32,
}
impl GraphWalkBounds {
    pub const fn new(minimum: u32, maximum: u32) -> Result<Self, GraphWalkBoundsError> {
        if minimum > maximum {
            return Err(GraphWalkBoundsError::Reversed);
        }
        if maximum > MAX_GRAPH_WALK_HOPS {
            return Err(GraphWalkBoundsError::TooManyHops {
                limit: MAX_GRAPH_WALK_HOPS,
                observed: maximum,
            });
        }
        Ok(Self { minimum, maximum })
    }
    #[must_use]
    pub const fn minimum(self) -> u32 {
        self.minimum
    }
    #[must_use]
    pub const fn maximum(self) -> u32 {
        self.maximum
    }
}

struct Frame<'a> {
    vertex: VId,
    neighbors: &'a [VId],
    next: usize,
    emitted: bool,
}

/// Fallible, iterative physical expansion. `next_with_control` yields one
/// endpoint for each admitted walk, in deterministic depth-first index order.
/// Distinctness, predicates, correlations, ranking and pagination belong to the
/// enclosing GLA plan, not to this cursor. In particular, an endpoint predicate
/// rejecting a short walk cannot prevent a longer walk passing through it.
///
/// The adjacency is derived query scratch, never authoritative graph storage.
/// Callers must supply the requested relation/orientation from one admitted
/// snapshot. Duplicate neighbors represent parallel edge occurrences and must
/// not be deduplicated. An undirected self-loop belongs in its index only once.
pub struct GraphWalkCursor<'a> {
    adjacency: Option<&'a BTreeMap<VId, Vec<VId>>>,
    bounds: GraphWalkBounds,
    stack: Vec<Frame<'a>>,
}
impl<'a> GraphWalkCursor<'a> {
    pub fn new<E>(
        source: VId,
        bounds: GraphWalkBounds,
        adjacency: Option<&'a BTreeMap<VId, Vec<VId>>>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        control(GlaExecutionEvent::ScratchEntry)?;
        let neighbors = if bounds.maximum == 0 {
            &[][..]
        } else {
            adjacency
                .and_then(|index| index.get(&source))
                .map_or(&[][..], Vec::as_slice)
        };
        Ok(Self {
            adjacency,
            bounds,
            stack: vec![Frame {
                vertex: source,
                neighbors,
                next: 0,
                emitted: false,
            }],
        })
    }

    pub fn next_with_control<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<VId>, E> {
        while !self.stack.is_empty() {
            control(GlaExecutionEvent::Work)?;
            let depth = (self.stack.len() - 1) as u32;
            let frame = self.stack.last_mut().expect("the frontier is nonempty");
            if !frame.emitted {
                frame.emitted = true;
                if depth >= self.bounds.minimum {
                    return Ok(Some(frame.vertex));
                }
            }
            if depth == self.bounds.maximum || frame.next == frame.neighbors.len() {
                let _ = self.stack.pop();
                continue;
            }
            let destination = frame.neighbors[frame.next];
            // A refused frame is not partially admitted. The cursor may be
            // dropped immediately on cancellation without recursive cleanup.
            control(GlaExecutionEvent::ScratchEntry)?;
            frame.next += 1;
            let neighbors = if depth + 1 == self.bounds.maximum {
                &[][..]
            } else {
                self.adjacency
                    .and_then(|index| index.get(&destination))
                    .map_or(&[][..], Vec::as_slice)
            };
            self.stack.push(Frame {
                vertex: destination,
                neighbors,
                next: 0,
                emitted: false,
            });
        }
        Ok(None)
    }
}
impl core::fmt::Debug for GraphWalkCursor<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphWalkCursor")
            .field("bounds", &self.bounds)
            .field("frontier_depth", &self.stack.len())
            .field("graph", &"[REDACTED]")
            .finish()
    }
}

/// Identity-preserving traversal over one admitted graph generation. Layers are
/// ordered by hop count, then by alternating edge/vertex identities, regardless
/// of adjacency input order. Duplicate input occurrences remain distinct.
/// Unlike the endpoint cursors, every returned path contains real edge IDs.
pub(crate) struct GraphPathCursor<'a> {
    adjacency: Option<&'a BTreeMap<VId, Vec<(EId, VId)>>>,
    bounds: GraphWalkBounds,
    search: GraphWalkSearch,
    depth: u32,
    frontier: Vec<GraphPath>,
    pending: std::vec::IntoIter<GraphPath>,
    settled: BTreeMap<VId, u32>,
    done: bool,
}

impl<'a> GraphPathCursor<'a> {
    pub(crate) fn new<E>(
        source: VId,
        bounds: GraphWalkBounds,
        search: GraphWalkSearch,
        adjacency: Option<&'a BTreeMap<VId, Vec<(EId, VId)>>>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        control(GlaExecutionEvent::Work)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        Ok(Self {
            adjacency,
            bounds,
            search,
            depth: 0,
            frontier: vec![GraphPath::new(source, Box::new([]))],
            pending: Vec::new().into_iter(),
            settled: BTreeMap::new(),
            done: false,
        })
    }

    /// Every delivery is governed, including buffered results. Refusal drops all
    /// retained paths and settlement markers and permanently fuses the cursor.
    pub(crate) fn next_with_control<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<GraphPath>, E> {
        let result = self.next_controlled(control);
        if result.is_err() {
            self.done = true;
            self.frontier = Vec::new();
            self.pending = Vec::new().into_iter();
            self.settled.clear();
        }
        result
    }

    fn next_controlled<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<GraphPath>, E> {
        loop {
            if self.pending.len() != 0 {
                control(GlaExecutionEvent::Work)?;
                return Ok(self.pending.next());
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
        if self.frontier.is_empty() || self.depth > self.bounds.maximum() {
            self.done = true;
            self.settled.clear();
            return Ok(());
        }

        let shortest = self.search != GraphWalkSearch::All;
        let unique = self.search == GraphWalkSearch::AnyShortest;
        let mut next = Vec::new();
        let mut pending = Vec::new();
        let mut canonical = BTreeMap::<VId, GraphPath>::new();
        for path in core::mem::take(&mut self.frontier) {
            control(GlaExecutionEvent::Work)?;
            let endpoint = path.steps().last().map_or(path.start(), |step| step.1);
            // Settlement starts at the lower bound, not at the first visit.
            // Recording the depth keeps every equal-depth witness eligible.
            if shortest && self.settled.get(&endpoint).is_some_and(|&at| at < self.depth) {
                continue;
            }
            let emit = self.depth >= self.bounds.minimum();
            if shortest && emit && !self.settled.contains_key(&endpoint) {
                control(GlaExecutionEvent::ScratchEntry)?;
                self.settled.insert(endpoint, self.depth);
            }

            if self.depth < self.bounds.maximum()
                && let Some(neighbors) = self.adjacency.and_then(|map| map.get(&endpoint))
            {
                for &step in neighbors {
                    control(GlaExecutionEvent::Work)?;
                    if unique {
                        // Compare before copying: parallel edges and tied
                        // routes cannot multiply the next layer's prefixes.
                        let previous = canonical.get(&step.1);
                        if let Some(previous) = previous {
                            let candidate = path.steps().iter().copied().chain(core::iter::once(step));
                            let mut better = false;
                            for (left, &right) in candidate.zip(previous.steps()) {
                                control(GlaExecutionEvent::Work)?;
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
                        } else {
                            control(GlaExecutionEvent::ScratchEntry)?;
                        }
                        let child = Self::extend_path(&path, step, control)?;
                        canonical.insert(step.1, child);
                    } else {
                        control(GlaExecutionEvent::ScratchEntry)?;
                        let child = Self::extend_path(&path, step, control)?;
                        next.push(child);
                    }
                }
            }
            if emit {
                control(GlaExecutionEvent::ScratchEntry)?;
                pending.push(path);
            }
        }
        // Endpoint coalescing chooses identities, not endpoint ordering. Move
        // those winners into the same canonical path order used by ALL modes.
        for (_, path) in canonical {
            control(GlaExecutionEvent::Work)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            next.push(path);
        }
        Self::sort_paths(&mut next, control)?;
        self.pending = pending.into_iter();
        self.frontier = next;
        self.depth += 1;
        self.done = self.frontier.is_empty();
        if self.done {
            self.settled.clear();
        }
        Ok(())
    }

    fn extend_path<E>(
        path: &GraphPath,
        step: (EId, VId),
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<GraphPath, E> {
        // Admit every copied identity pair and the new step before allocating
        // the exact-sized buffer. Moving paths between maps never copies it.
        let length = path.steps().len() + 1;
        for _ in 0..length {
            control(GlaExecutionEvent::Work)?;
            control(GlaExecutionEvent::ScratchEntry)?;
        }
        let mut steps = Vec::with_capacity(length);
        steps.extend_from_slice(path.steps());
        steps.push(step);
        Ok(GraphPath::new(path.start(), steps.into_boxed_slice()))
    }

    /// Fallible in-place heapsort keeps comparison work interruptible without
    /// allocating a second path buffer or hiding fallible control in `Ord`.
    fn sort_paths<E>(
        paths: &mut [GraphPath],
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        for root in (0..paths.len() / 2).rev() {
            Self::sift_paths(paths, root, control)?;
        }
        for end in (1..paths.len()).rev() {
            control(GlaExecutionEvent::Work)?;
            paths.swap(0, end);
            Self::sift_paths(&mut paths[..end], 0, control)?;
        }
        Ok(())
    }

    fn sift_paths<E>(
        paths: &mut [GraphPath],
        mut root: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        while root < paths.len() / 2 {
            control(GlaExecutionEvent::Work)?;
            let mut child = root * 2 + 1;
            if child + 1 < paths.len()
                && Self::compare_paths(&paths[child], &paths[child + 1], control)?.is_lt()
            {
                child += 1;
            }
            if !Self::compare_paths(&paths[root], &paths[child], control)?.is_lt() {
                break;
            }
            paths.swap(root, child);
            root = child;
        }
        Ok(())
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
}

impl core::fmt::Debug for GraphPathCursor<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphPathCursor")
            .field("bounds", &self.bounds)
            .field("search", &self.search)
            .field("depth", &self.depth)
            .field("frontier", &self.frontier.len())
            .field("pending", &self.pending.len())
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
        let mut cursor = GraphWalkCursor::new(source, bounds, adjacency, control)?;
        let mut rows = Vec::new();
        while let Some(vertex) = cursor.next_with_control(control)? {
            rows.push(vertex);
        }
        Ok(rows)
    }

    #[test]
    fn finite_bounds_and_isolated_zero_hops_are_explicit() {
        assert_eq!(
            GraphWalkBounds::new(2, 1),
            Err(GraphWalkBoundsError::Reversed)
        );
        assert_eq!(
            GraphWalkBounds::new(0, MAX_GRAPH_WALK_HOPS + 1),
            Err(GraphWalkBoundsError::TooManyHops {
                limit: MAX_GRAPH_WALK_HOPS,
                observed: MAX_GRAPH_WALK_HOPS + 1
            })
        );
        for source in [VId(0), VId(u128::MAX)] {
            for (minimum, expected) in [(0, vec![source]), (1, vec![])] {
                let result = collect(
                    source,
                    GraphWalkBounds::new(minimum, 3).unwrap(),
                    None,
                    &mut |_| Ok::<_, ()>(()),
                )
                .unwrap();
                assert_eq!(result, expected);
            }
        }
    }

    #[test]
    fn iterative_walks_match_independent_layer_multiplication() {
        // The oracle propagates complete multiplicities one matrix layer at a
        // time; it shares no cursor frames, visited flags or traversal order.
        for mask in 0..512_u32 {
            let mut adjacency = BTreeMap::<VId, Vec<VId>>::new();
            for source in 0..3_u128 {
                for target in 0..3_u128 {
                    if mask & (1 << (3 * source + target)) != 0 {
                        adjacency.entry(VId(source)).or_default().push(VId(target));
                    }
                }
            }
            if let Some(neighbors) = adjacency.values_mut().find(|rows| !rows.is_empty()) {
                neighbors.push(neighbors[0]);
                neighbors.sort();
            }
            for source in 0..4_u128 {
                for maximum in 0..=3 {
                    for minimum in 0..=maximum {
                        let mut layer = BTreeMap::from([(VId(source), 1_usize)]);
                        let mut expected = BTreeMap::<VId, usize>::new();
                        for depth in 0..=maximum {
                            if depth >= minimum {
                                for (&vertex, &count) in &layer {
                                    *expected.entry(vertex).or_default() += count;
                                }
                            }
                            let mut next = BTreeMap::<VId, usize>::new();
                            for (vertex, count) in layer {
                                for &target in adjacency.get(&vertex).into_iter().flatten() {
                                    *next.entry(target).or_default() += count;
                                }
                            }
                            layer = next;
                        }
                        let bounds = GraphWalkBounds::new(minimum, maximum).unwrap();
                        let rows = collect(VId(source), bounds, Some(&adjacency), &mut |_| {
                            Ok::<_, ()>(())
                        })
                        .unwrap();
                        let mut actual = BTreeMap::<VId, usize>::new();
                        for vertex in rows {
                            *actual.entry(vertex).or_default() += 1;
                        }
                        assert_eq!(actual, expected, "mask={mask}, source={source}, {bounds:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn maximum_depth_does_not_require_recursive_calls() {
        let adjacency = BTreeMap::from([(VId(7), vec![VId(7)])]);
        let bounds = GraphWalkBounds::new(MAX_GRAPH_WALK_HOPS, MAX_GRAPH_WALK_HOPS).unwrap();
        let rows = collect(VId(7), bounds, Some(&adjacency), &mut |_| Ok::<_, ()>(())).unwrap();
        assert_eq!(rows, vec![VId(7)]);
    }

    #[test]
    fn every_frontier_work_and_growth_boundary_is_interruptible() {
        let adjacency = BTreeMap::from([
            (VId(1), vec![VId(1), VId(2), VId(2)]),
            (VId(2), vec![VId(1)]),
        ]);
        let bounds = GraphWalkBounds::new(0, 3).unwrap();
        let mut events = Vec::new();
        let expected = collect(VId(1), bounds, Some(&adjacency), &mut |event| {
            events.push(event);
            Ok::<_, usize>(())
        })
        .unwrap();
        assert!(expected.len() > 10);
        assert!(events.contains(&GlaExecutionEvent::ScratchEntry));
        assert!(events.contains(&GlaExecutionEvent::Work));
        for stop in 1..=events.len() {
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
}
