//! Exact, finite ANY CHEAPEST WALK with signed integer edge costs.
//!
//! This typed PathFind specialization wraps a compiled, captured WALK input.
//! It keeps the least-cost prefix for each (hop, vertex), not each vertex:
//! negative edges, cycles and a positive minimum hop bound make ordinary
//! vertex settlement unsound. Equal-cost prefixes use canonical path order.
//! Only the selected path is returned; no bag of candidate walks is built.
//!
//! Costs are canonical Int values accumulated exactly in i128. The existing
//! finite hop ceiling makes every admitted sum representable, with checked
//! addition retained as a fail-closed boundary. Missing/null/noninteger costs
//! on any edge of the selected relation refuse, even on unreachable edges.
//! The supplied source must already be authorized. This is neither a security
//! filter nor an unbounded/weighted GQL text grammar or a spill implementation.

mod ranked;
pub use ranked::GraphCheapestPathCursor;

use crate::algebra::{
    GlaDirection, GraphColumn, GraphPath, GraphPathFunction, GraphPatternBuilder,
    GraphValueRow, PatternBuildError, PreparedGraphPattern,
};
use crate::{
    GlaExecutionEvent, GlaExecutionStats, GqlBudgetDimension, GqlExecutionStats,
    GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphWalkBounds,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, EId, VId};
use std::collections::{BTreeMap, BTreeSet};

type WeightedIndex = BTreeMap<VId, BTreeMap<(EId, VId), i64>>;

/// A domain failure contains no source identities, property keys or values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphPathCostError {
    MissingWeight,
    NonIntegerWeight,
    DuplicateEdge,
    DanglingEndpoint,
    CostOverflow,
}

impl core::fmt::Display for GraphPathCostError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "bounded path cost error: {self:?}")
    }
}
impl core::error::Error for GraphPathCostError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphCheapestPathError<E> {
    Source(E),
    Cost(GraphPathCostError),
}
impl<E> GraphCheapestPathError<E> {
    pub fn map_source<F>(self, map: impl FnOnce(E) -> F) -> GraphCheapestPathError<F> {
        match self {
            Self::Source(error) => GraphCheapestPathError::Source(map(error)),
            Self::Cost(error) => GraphCheapestPathError::Cost(error),
        }
    }
}
impl<E: core::fmt::Display> core::fmt::Display for GraphCheapestPathError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Source(error) => error.fmt(f),
            Self::Cost(error) => error.fmt(f),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for GraphCheapestPathError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Cost(error) => Some(error),
        }
    }
}

/// One real edge-identified traversal and its exact signed cost.
#[derive(Clone, PartialEq, Eq)]
pub struct GraphCostPath {
    path: GraphPath,
    cost: i128,
}
impl GraphCostPath {
    /// Explicit data export; ordinary Debug output is redacted.
    #[must_use]
    pub fn path(&self) -> &GraphPath {
        &self.path
    }
    #[must_use]
    pub const fn cost(&self) -> i128 {
        self.cost
    }
}
impl core::fmt::Debug for GraphCostPath {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphCostPath([REDACTED])")
    }
}

/// Immutable typed PathFind definition over one finite captured WALK.
///
/// Select the minimum-cost path between the two anchors WITHIN the hop
/// interval. Equal costs select the lexicographically smallest GraphPath,
/// including across different lengths, not the fewest-hop path. Repeated
/// vertices and edges are legal. A negative cycle is finite under this bound.
/// Parallel edges remain distinct and an undirected self-loop appears once.
///
/// The ordinary compiler owns the input pattern. The physical specialization
/// below evaluates its cost selection by dynamic programming instead of
/// materializing that input's potentially exponential path bag.
#[derive(Clone)]
pub struct PreparedGraphCheapestPath {
    source: VId,
    target: VId,
    relation: RelationId,
    direction: GlaDirection,
    weight: PropertyKeyId,
    bounds: GraphWalkBounds,
    input: PreparedGraphPattern<GraphValueRow>,
}
impl PreparedGraphCheapestPath {
    pub fn new(
        source: VId,
        target: VId,
        relation: RelationId,
        direction: GlaDirection,
        weight: PropertyKeyId,
        bounds: GraphWalkBounds,
    ) -> Result<Self, PatternBuildError> {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("start")?;
        builder.vertex("end")?;
        builder.walk("start", relation, direction, "end", bounds)?;
        builder.capture_path("route")?;
        let input = builder
            .prepare_values(&[GraphColumn::path("route", "route", GraphPathFunction::Value)], 0, None)?
            .with_duplicates();
        Ok(Self { source, target, relation, direction, weight, bounds, input })
    }

    /// Compiled input, before endpoint anchoring and cost selection. This is
    /// also the source-admission contract, not a substitute graph or parser.
    #[must_use]
    pub fn input_pattern(&self) -> &PreparedGraphPattern<GraphValueRow> {
        &self.input
    }
    #[must_use]
    pub const fn source(&self) -> VId { self.source }
    #[must_use]
    pub const fn target(&self) -> VId { self.target }
    #[must_use]
    pub const fn relation(&self) -> RelationId { self.relation }
    #[must_use]
    pub const fn direction(&self) -> GlaDirection { self.direction }
    #[must_use]
    pub const fn weight_property(&self) -> PropertyKeyId { self.weight }
    #[must_use]
    pub const fn bounds(&self) -> GraphWalkBounds { self.bounds }

    /// Application identity, not a durable format or an old BoundPlan certificate.
    /// The domain pins WALK, ANY, Int64 costs, exact accumulation and path-lex ties.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:path-find:any-cheapest-int64-walk:path-lex:v1\0".to_vec();
        bytes.extend_from_slice(&self.source.0.to_be_bytes());
        bytes.extend_from_slice(&self.target.0.to_be_bytes());
        bytes.extend_from_slice(&self.relation.0.to_be_bytes());
        bytes.push(match self.direction {
            GlaDirection::Forward => 0,
            GlaDirection::Reverse => 1,
            GlaDirection::Undirected => 2,
        });
        bytes.extend_from_slice(&self.weight.0.to_be_bytes());
        bytes.extend_from_slice(&self.bounds.minimum().to_be_bytes());
        bytes.extend_from_slice(&self.bounds.maximum().to_be_bytes());
        bytes
    }

    /// Evaluate an admitted immutable source. Controls precede logical work,
    /// retained entries and result release. On any refusal no partial result
    /// escapes; a fresh execution can retry against the unchanged source.
    pub fn execute_with_control<'a, E>(
        &self,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
        mut property: impl FnMut(EId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        mut control: impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<GraphCostPath>, GraphCheapestPathError<E>> {
        self.evaluate(
            vertices, edges,
            &mut |eid, key| property(eid, key).map_err(GraphCheapestPathError::Source),
            &mut |event| control(event).map_err(GraphCheapestPathError::Source),
            GraphCheapestPathError::Cost,
        )
    }

    /// Shared query-policy vocabulary and meter; source admission must be
    /// charged by the host first and pass only its remaining work allowance.
    pub fn execute_governed_with_edge_properties<'a, E, C>(
        &self,
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
        let selected = self.evaluate(
            vertices, edges,
            &mut |eid, key| property(eid, key)
                .map_err(|error| GqlQueryError::Source(GraphCheapestPathError::Source(error))),
            &mut |event| {
                checkpoint().map_err(GqlQueryError::Interrupted)?;
                if event == GlaExecutionEvent::ResultRow {
                    policy.rows.check(GqlBudgetDimension::ResultRows, 1).map_err(GqlQueryError::Rows)?;
                }
                evaluator.charge_event(policy.evaluator, event).map_err(GqlQueryError::Evaluator)?;
                Ok(())
            },
            |error| GqlQueryError::Source(GraphCheapestPathError::Cost(error)),
        )?;
        let value: Vec<_> = selected.into_iter().collect();
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        Ok(GqlQueryExecution {
            rows: GqlExecutionStats { snapshot_records, result_rows: value.len() as u64 },
            value,
            evaluator,
        })
    }

    // Single-answer and ranked searches share identical source admission,
    // cost-domain refusals, orientation rules and logical event ordering.
    fn admit<'a, E>(
        &self,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
        property: &mut impl FnMut(EId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        failure: impl Fn(GraphPathCostError) -> E,
    ) -> Result<(BTreeSet<VId>, WeightedIndex), E> {
        let mut live = BTreeSet::new();
        for vertex in vertices {
            control(GlaExecutionEvent::Work)?;
            if !live.contains(&vertex) {
                control(GlaExecutionEvent::ScratchEntry)?;
                live.insert(vertex);
            }
        }
        let mut identities = BTreeSet::new();
        let mut adjacency = BTreeMap::<VId, BTreeMap<(EId, VId), i64>>::new();
        for (edge, left, relation, right) in edges {
            control(GlaExecutionEvent::Work)?;
            if relation != self.relation { continue; }
            if identities.contains(&edge) { return Err(failure(GraphPathCostError::DuplicateEdge)); }
            if !live.contains(&left) || !live.contains(&right) {
                return Err(failure(GraphPathCostError::DanglingEndpoint));
            }
            let weight = match property(edge, self.weight)? {
                Some(CanonicalScalar::Int(weight)) => *weight,
                None => return Err(failure(GraphPathCostError::MissingWeight)),
                Some(_) => return Err(failure(GraphPathCostError::NonIntegerWeight)),
            };
            control(GlaExecutionEvent::ScratchEntry)?;
            identities.insert(edge);
            let orientations = match self.direction {
                GlaDirection::Forward => [(left, right), (left, right)],
                GlaDirection::Reverse => [(right, left), (right, left)],
                GlaDirection::Undirected => [(left, right), (right, left)],
            };
            let count = if self.direction == GlaDirection::Undirected && left != right { 2 } else { 1 };
            for &(from, to) in orientations.iter().take(count) {
                control(GlaExecutionEvent::Work)?;
                if !adjacency.contains_key(&from) { control(GlaExecutionEvent::ScratchEntry)?; }
                control(GlaExecutionEvent::ScratchEntry)?;
                adjacency.entry(from).or_default().insert((edge, to), weight);
            }
        }
        Ok((live, adjacency))
    }

    fn evaluate<'a, E>(
        &self,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
        property: &mut impl FnMut(EId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        failure: impl Fn(GraphPathCostError) -> E,
    ) -> Result<Option<GraphCostPath>, E> {
        let (live, adjacency) = self.admit(vertices, edges, property, control, &failure)?;
        // Do not hide malformed weights behind missing anchors or LIMIT-like
        // output policies. The complete selected relation was admitted above.
        if !live.contains(&self.source) || !live.contains(&self.target) { return Ok(None); }
        control(GlaExecutionEvent::ScratchEntry)?;
        let mut arena = vec![State { vertex: self.source, cost: 0, parent: None, rank: 0 }];
        control(GlaExecutionEvent::ScratchEntry)?;
        let mut current = vec![0_usize];
        let mut best = (self.bounds.minimum() == 0 && self.source == self.target).then_some(0);
        for depth in 1..=self.bounds.maximum() {
            control(GlaExecutionEvent::Work)?;
            let mut next = BTreeMap::<VId, Candidate>::new();
            for &parent in &current {
                control(GlaExecutionEvent::Work)?;
                let state = &arena[parent];
                if let Some(neighbors) = adjacency.get(&state.vertex) {
                    for (&(edge, vertex), &weight) in neighbors {
                        control(GlaExecutionEvent::Work)?;
                        let cost = state.cost.checked_add(i128::from(weight))
                            .ok_or_else(|| failure(GraphPathCostError::CostOverflow))?;
                        let candidate = Candidate { cost, parent, edge, parent_rank: state.rank };
                        if next.get(&vertex).is_none_or(|prior| candidate.key() < prior.key()) {
                            if !next.contains_key(&vertex) { control(GlaExecutionEvent::ScratchEntry)?; }
                            next.insert(vertex, candidate);
                        }
                    }
                }
            }
            if next.is_empty() { break; }
            // A rank is the canonical path order WITHIN this layer, not its
            // cost order. Appending equal-length prefixes preserves this order.
            let mut ranked = BTreeMap::new();
            for (vertex, candidate) in next {
                control(GlaExecutionEvent::Work)?;
                control(GlaExecutionEvent::ScratchEntry)?;
                ranked.insert((candidate.parent_rank, candidate.edge, vertex), candidate);
            }
            current.clear();
            for (rank, ((_, edge, vertex), candidate)) in ranked.into_iter().enumerate() {
                control(GlaExecutionEvent::Work)?;
                control(GlaExecutionEvent::ScratchEntry)?;
                let at = arena.len();
                arena.push(State { vertex, cost: candidate.cost, parent: Some((candidate.parent, edge)), rank });
                control(GlaExecutionEvent::ScratchEntry)?;
                current.push(at);
                if depth >= self.bounds.minimum() && vertex == self.target {
                    let improves = match best {
                        None => true,
                        Some(prior) => candidate.cost < arena[prior].cost
                            || (candidate.cost == arena[prior].cost && path_less(&arena, at, prior, control)?),
                    };
                    if improves { best = Some(at); }
                }
            }
        }
        let Some(best) = best else { return Ok(None); };
        let steps = path_steps(&arena, best, control)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        control(GlaExecutionEvent::ResultRow)?;
        Ok(Some(GraphCostPath {
            path: GraphPath::new(self.source, steps.into_boxed_slice()),
            cost: arena[best].cost,
        }))
    }
}
impl core::fmt::Debug for PreparedGraphCheapestPath {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphCheapestPath")
            .field("bounds", &self.bounds)
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

struct State {
    vertex: VId,
    cost: i128,
    parent: Option<(usize, EId)>,
    rank: usize,
}
struct Candidate {
    cost: i128,
    parent: usize,
    edge: EId,
    parent_rank: usize,
}
impl Candidate {
    fn key(&self) -> (i128, usize, EId) { (self.cost, self.parent_rank, self.edge) }
}
fn path_steps<E>(
    arena: &[State], mut at: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Vec<(EId, VId)>, E> {
    let mut steps = Vec::new();
    while let Some((parent, edge)) = arena[at].parent {
        control(GlaExecutionEvent::Work)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        steps.push((edge, arena[at].vertex));
        at = parent;
    }
    for left in 0..steps.len() / 2 {
        control(GlaExecutionEvent::Work)?;
        let right = steps.len() - left - 1;
        steps.swap(left, right);
    }
    Ok(steps)
}
fn path_less<E>(
    arena: &[State], left: usize, right: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<bool, E> {
    let left = path_steps(arena, left, control)?;
    let right = path_steps(arena, right, control)?;
    for (left, right) in left.iter().zip(&right) {
        control(GlaExecutionEvent::Work)?;
        match left.cmp(right) {
            core::cmp::Ordering::Less => return Ok(true),
            core::cmp::Ordering::Greater => return Ok(false),
            core::cmp::Ordering::Equal => {}
        }
    }
    control(GlaExecutionEvent::Work)?;
    Ok(left.len() < right.len())
}

#[cfg(test)]
mod tests;
