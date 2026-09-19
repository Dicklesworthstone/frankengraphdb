//! Lazy cost-ranked path partitions over an acyclic (depth, vertex) state space.
//!
//! A suffix table chooses the cheapest completion at each depth. One heap entry
//! represents ALL completions of a fixed prefix, but stores only their minimum.
//! After returning that minimum, partition its remaining completions by the
//! first different action (including STOP). The partitions are disjoint, so no
//! global seen-path set or bag of already enumerated walks is necessary.

use super::*;
use std::cmp::Ordering;

type Step = (EId, VId);

/// An owned, pull-driven enumeration of every finite WALK between two anchors,
/// ordered by exact signed cost, then canonical edge/vertex path order.
///
/// Construct through [`PreparedGraphCheapestPath::cursor_with_control`]. The
/// source is admitted once, including every selected-relation cost. This cursor
/// owns the checked topology and weights; later source changes cannot change its
/// answers. Parallel edges remain distinct. Repeated vertices/edges, bounded
/// negative cycles and positive minimum hop bounds retain WALK semantics.
///
/// Only requested answers are expanded into alternatives. A finite prefix does
/// not enumerate the remaining result bag. Preparation retains polynomially
/// many suffix states; the pending partition heap grows with requested answers
/// and branching. This is not a spill operator, allocator-byte memory bound,
/// durable cursor, authorization grant, or session/lease protocol.
///
/// Every refused pull is terminal: it returns no partial path and releases all
/// retained search state. Earlier successful pulls remain valid. A new cursor
/// can replay from the same immutable source. `close` is idempotent.
pub struct GraphCheapestPathCursor {
    search: Option<Search>,
}

#[derive(Clone, Copy)]
struct Suffix {
    cost: i128,
    // STOP sorts before any step at equal cost: an empty suffix is a prefix.
    step: Option<Step>,
}

struct Partition {
    cost: i128,
    steps: Vec<Step>,
    fixed: usize,
    // This partition selected STOP rather than all continuations of a prefix.
    terminal: bool,
}

struct Search {
    source: VId,
    target: VId,
    bounds: GraphWalkBounds,
    forward: WeightedIndex,
    // Indexed by maximum - depth, so the immediately following depth is
    // available while building the table backwards, with no cloned maps.
    suffixes: Vec<BTreeMap<VId, Suffix>>,
    heap: Vec<Partition>,
    pending: Option<Partition>,
}

impl PreparedGraphCheapestPath {
    /// Admit one source and prepare a lazy rank-ordered enumeration. The same
    /// selected-relation validation is used by the single-answer evaluator.
    pub fn cursor_with_control<'a, E>(
        &self,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
        mut property: impl FnMut(EId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        mut control: impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<GraphCheapestPathCursor, GraphCheapestPathError<E>> {
        GraphCheapestPathCursor::create(
            self,
            vertices,
            edges,
            &mut |edge, key| property(edge, key).map_err(GraphCheapestPathError::Source),
            &mut |event| control(event).map_err(GraphCheapestPathError::Source),
            &GraphCheapestPathError::Cost,
        )
    }

    /// Return at most `count` cost-ranked walks. Count zero still admits the
    /// source and refuses invalid weights; it is not a validation bypass.
    /// A later refusal drops the complete batch rather than returning a prefix.
    pub fn execute_k_with_control<'a, E>(
        &self,
        count: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
        mut property: impl FnMut(EId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        mut control: impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Vec<GraphCostPath>, GraphCheapestPathError<E>> {
        collect(
            self, count, vertices, edges,
            &mut |edge, key| property(edge, key).map_err(GraphCheapestPathError::Source),
            &mut |event| control(event).map_err(GraphCheapestPathError::Source),
            &GraphCheapestPathError::Cost,
        )
    }

    /// Source admission, suffix construction, heap ordering, path copies and
    /// final output consume one existing GLA allowance. Result rows count the
    /// returned K-prefix, not all possible completions or distinct endpoints.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_k_governed_with_edge_properties<'a, E, C>(
        &self,
        count: u64,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
        mut property: impl FnMut(EId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        policy: GqlQueryPolicy,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphCostPath>, GqlQueryError<GraphCheapestPathError<E>, C>> {
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        policy.rows.check(GqlBudgetDimension::SnapshotRecords, snapshot_records)
            .map_err(GqlQueryError::Rows)?;
        let mut evaluator = GlaExecutionStats::default();
        let mut result_rows = 0_u64;
        let value = collect(
            self, count, vertices, edges,
            &mut |edge, key| property(edge, key)
                .map_err(|error| GqlQueryError::Source(GraphCheapestPathError::Source(error))),
            &mut |event| {
                checkpoint().map_err(GqlQueryError::Interrupted)?;
                if event == GlaExecutionEvent::ResultRow {
                    // At most `count <= u64::MAX` successful pulls occur.
                    let observed = result_rows.checked_add(1).expect("bounded K-prefix row count");
                    policy.rows.check(GqlBudgetDimension::ResultRows, observed)
                        .map_err(GqlQueryError::Rows)?;
                    result_rows = observed;
                }
                evaluator.charge_event(policy.evaluator, event).map_err(GqlQueryError::Evaluator)
            },
            &|error| GqlQueryError::Source(GraphCheapestPathError::Cost(error)),
        )?;
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        debug_assert_eq!(value.len() as u64, result_rows);
        Ok(GqlQueryExecution {
            value,
            rows: GqlExecutionStats { snapshot_records, result_rows },
            evaluator,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn collect<'a, E>(
    query: &PreparedGraphCheapestPath,
    count: u64,
    vertices: impl IntoIterator<Item = VId>,
    edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
    property: &mut impl FnMut(EId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    failure: &impl Fn(GraphPathCostError) -> E,
) -> Result<Vec<GraphCostPath>, E> {
    let mut cursor = GraphCheapestPathCursor::create(query, vertices, edges, property, control, failure)?;
    let mut rows = Vec::new();
    for _ in 0..count {
        let Some(row) = cursor.pull(control, failure)? else { break; };
        // The pull reserves one returned row as well as every owned path step.
        rows.push(row);
    }
    Ok(rows)
}

impl GraphCheapestPathCursor {
    fn create<'a, E>(
        query: &PreparedGraphCheapestPath,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
        property: &mut impl FnMut(EId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        failure: &impl Fn(GraphPathCostError) -> E,
    ) -> Result<Self, E> {
        let (live, forward) = query.admit(vertices, edges, property, control, failure)?;
        if !live.contains(&query.source) || !live.contains(&query.target) {
            return Ok(Self { search: None });
        }
        let mut reverse = WeightedIndex::new();
        for (&from, edges) in &forward {
            control(GlaExecutionEvent::Work)?;
            for (&(edge, to), &weight) in edges {
                control(GlaExecutionEvent::Work)?;
                if !reverse.contains_key(&to) { control(GlaExecutionEvent::ScratchEntry)?; }
                control(GlaExecutionEvent::ScratchEntry)?;
                reverse.entry(to).or_default().insert((edge, from), weight);
            }
        }
        let mut suffixes: Vec<BTreeMap<VId, Suffix>> = Vec::new();
        for depth in (0..=query.bounds.maximum()).rev() {
            control(GlaExecutionEvent::ScratchEntry)?;
            let mut layer = BTreeMap::new();
            if depth >= query.bounds.minimum() {
                control(GlaExecutionEvent::ScratchEntry)?;
                layer.insert(query.target, Suffix { cost: 0, step: None });
            }
            if let Some(next) = suffixes.last() {
                // Only predecessors of viable suffixes are visited. A sparse
                // exact-length chain does not rescan every edge per hop.
                for (&to, suffix) in next {
                    control(GlaExecutionEvent::Work)?;
                    for (&(edge, from), &weight) in reverse.get(&to).into_iter().flatten() {
                        control(GlaExecutionEvent::Work)?;
                        let cost = suffix.cost.checked_add(i128::from(weight))
                            .ok_or_else(|| failure(GraphPathCostError::CostOverflow))?;
                        let candidate = Suffix { cost, step: Some((edge, to)) };
                        if layer.get(&from).is_none_or(|prior: &Suffix|
                            (candidate.cost, candidate.step) < (prior.cost, prior.step)) {
                            if !layer.contains_key(&from) { control(GlaExecutionEvent::ScratchEntry)?; }
                            layer.insert(from, candidate);
                        }
                    }
                }
            }
            suffixes.push(layer);
        }
        let Some(best) = suffixes.last().and_then(|layer| layer.get(&query.source)).copied() else {
            return Ok(Self { search: None });
        };
        let mut search = Search {
            source: query.source, target: query.target, bounds: query.bounds,
            forward, suffixes, heap: Vec::new(), pending: None,
        };
        let steps = search.complete(Vec::new(), query.source, 0, control)?;
        heap_push(&mut search.heap, Partition { cost: best.cost, steps, fixed: 0, terminal: false }, control)?;
        Ok(Self { search: Some(search) })
    }

    pub fn next_with_control<E>(
        &mut self,
        mut control: impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<GraphCostPath>, GraphCheapestPathError<E>> {
        self.pull(
            &mut |event| control(event).map_err(GraphCheapestPathError::Source),
            &GraphCheapestPathError::Cost,
        )
    }

    fn pull<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        failure: &impl Fn(GraphPathCostError) -> E,
    ) -> Result<Option<GraphCostPath>, E> {
        let Some(search) = &mut self.search else { return Ok(None); };
        let result = search.pull(control, failure);
        if result.is_err() || matches!(&result, Ok(None)) { self.close(); }
        result
    }

    pub fn close(&mut self) { self.search = None; }

    /// True after explicit close, refusal, or a pull that observed exhaustion.
    #[must_use]
    pub fn is_exhausted(&self) -> bool { self.search.is_none() }
}

impl core::fmt::Debug for GraphCheapestPathCursor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphCheapestPathCursor")
            .field("exhausted", &self.is_exhausted())
            .field("search", &"[REDACTED]")
            .finish()
    }
}

impl Search {
    fn suffix(&self, depth: usize, vertex: VId) -> Option<Suffix> {
        self.suffixes[self.bounds.maximum() as usize - depth].get(&vertex).copied()
    }

    fn complete<E>(
        &self, mut steps: Vec<Step>, mut vertex: VId, mut depth: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Vec<Step>, E> {
        loop {
            control(GlaExecutionEvent::Work)?;
            let suffix = self.suffix(depth, vertex).expect("admitted feasible suffix");
            let Some(step) = suffix.step else { return Ok(steps); };
            control(GlaExecutionEvent::ScratchEntry)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            steps.push(step);
            vertex = step.1;
            depth += 1;
        }
    }

    fn pull<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        failure: &impl Fn(GraphPathCostError) -> E,
    ) -> Result<Option<GraphCostPath>, E> {
        // Defer all deviations from a returned answer until another pull.
        if let Some(previous) = self.pending.take() { self.expand(previous, control, failure)?; }
        let Some(entry) = heap_pop(&mut self.heap, control)? else { return Ok(None); };
        let steps = copy_steps(&entry.steps, control)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        control(GlaExecutionEvent::ResultRow)?;
        let result = GraphCostPath { path: GraphPath::new(self.source, steps.into_boxed_slice()), cost: entry.cost };
        self.pending = Some(entry);
        Ok(Some(result))
    }

    fn expand<E>(
        &mut self, entry: Partition,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        failure: &impl Fn(GraphPathCostError) -> E,
    ) -> Result<(), E> {
        if entry.terminal { return Ok(()); }
        let mut vertex = self.source;
        let mut prefix_cost = 0_i128;
        for depth in 0..=entry.steps.len() {
            control(GlaExecutionEvent::Work)?;
            let selected = entry.steps.get(depth).copied();
            if depth >= entry.fixed {
                // STOP is a real alternative when the minimum continued past
                // this target. Its partition must not grow descendants again.
                if selected.is_some() && vertex == self.target && depth >= self.bounds.minimum() as usize {
                    let steps = copy_steps(&entry.steps[..depth], control)?;
                    heap_push(&mut self.heap, Partition { cost: prefix_cost, steps, fixed: depth, terminal: true }, control)?;
                }
                // Also visit the final STOP position: otherwise longer paths
                // sharing an emitted path as a prefix would disappear.
                if depth < self.bounds.maximum() as usize {
                    for (&step, &weight) in self.forward.get(&vertex).into_iter().flatten() {
                        control(GlaExecutionEvent::Work)?;
                        if Some(step) == selected { continue; }
                        let Some(suffix) = self.suffix(depth + 1, step.1) else { continue; };
                        let cost = prefix_cost.checked_add(i128::from(weight))
                            .and_then(|value| value.checked_add(suffix.cost))
                            .ok_or_else(|| failure(GraphPathCostError::CostOverflow))?;
                        let mut steps = copy_steps(&entry.steps[..depth], control)?;
                        control(GlaExecutionEvent::ScratchEntry)?;
                        control(GlaExecutionEvent::ScratchEntry)?;
                        steps.push(step);
                        let steps = self.complete(steps, step.1, depth + 1, control)?;
                        heap_push(&mut self.heap, Partition { cost, steps, fixed: depth + 1, terminal: false }, control)?;
                    }
                }
            }
            if let Some(step) = selected {
                prefix_cost = prefix_cost.checked_add(i128::from(self.forward[&vertex][&step]))
                    .ok_or_else(|| failure(GraphPathCostError::CostOverflow))?;
                vertex = step.1;
            }
        }
        Ok(())
    }
}

fn copy_steps<E>(steps: &[Step], control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>) -> Result<Vec<Step>, E> {
    let mut copy = Vec::new();
    for &step in steps {
        control(GlaExecutionEvent::Work)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        copy.push(step);
    }
    Ok(copy)
}

// A fallible heap keeps long equal-cost path comparisons inside query control.
// std::BinaryHeap's Ord cannot propagate a mid-comparison interruption.
fn less<E>(left: &Partition, right: &Partition, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>) -> Result<bool, E> {
    control(GlaExecutionEvent::Work)?;
    match left.cost.cmp(&right.cost) {
        Ordering::Less => return Ok(true),
        Ordering::Greater => return Ok(false),
        Ordering::Equal => {}
    }
    for (left, right) in left.steps.iter().zip(&right.steps) {
        control(GlaExecutionEvent::Work)?;
        match left.cmp(right) {
            Ordering::Less => return Ok(true),
            Ordering::Greater => return Ok(false),
            Ordering::Equal => {}
        }
    }
    control(GlaExecutionEvent::Work)?;
    Ok(left.steps.len() < right.steps.len())
}

fn heap_push<E>(heap: &mut Vec<Partition>, entry: Partition, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>) -> Result<(), E> {
    control(GlaExecutionEvent::ScratchEntry)?;
    heap.push(entry);
    let mut at = heap.len() - 1;
    while at > 0 {
        let parent = (at - 1) / 2;
        if !less(&heap[at], &heap[parent], control)? { break; }
        control(GlaExecutionEvent::Work)?;
        heap.swap(at, parent);
        at = parent;
    }
    Ok(())
}

fn heap_pop<E>(heap: &mut Vec<Partition>, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>) -> Result<Option<Partition>, E> {
    if heap.is_empty() { return Ok(None); }
    control(GlaExecutionEvent::Work)?;
    let last = heap.pop().expect("nonempty heap");
    if heap.is_empty() { return Ok(Some(last)); }
    let best = std::mem::replace(&mut heap[0], last);
    let mut root = 0;
    while root < heap.len() / 2 {
        let mut child = 2 * root + 1;
        if child + 1 < heap.len() && less(&heap[child + 1], &heap[child], control)? { child += 1; }
        if !less(&heap[child], &heap[root], control)? { break; }
        control(GlaExecutionEvent::Work)?;
        heap.swap(root, child);
        root = child;
    }
    Ok(Some(best))
}

#[cfg(test)]
mod tests;
