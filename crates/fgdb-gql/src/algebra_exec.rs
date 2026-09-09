//! One scan-backed evaluator for scalar and correlated binding-row projections.
//! Requested relation/orientation pairs are indexed once. Controls precede
//! admitted-row work, operator visits, scratch growth and final row release.

mod policy;
mod projection;
pub use policy::{GqlQueryError, GqlQueryExecution, GqlQueryPolicy};
pub use projection::ProjectedRows;

use crate::algebra::{
    GlaDirection, GlaIdentityOutput, GlaOperator, GlaOutput, GlaPlan, VertexPredicate,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, VId};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlaExecutionEvent {
    Work,
    ScratchEntry,
    /// One final ordered/paginated occurrence before releasing it to output.
    /// Repeated values are removed only when terminal DISTINCT is present.
    ResultRow,
}

/// Logical-entry/work limits, not allocator-byte, I/O or wall-clock limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlaExecutionLimits {
    pub max_work_units: u64,
    pub max_scratch_entries: u64,
}

impl GlaExecutionLimits {
    #[must_use]
    pub const fn new(max_work_units: u64, max_scratch_entries: u64) -> Self {
        Self {
            max_work_units,
            max_scratch_entries,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlaLimitDimension {
    WorkUnits,
    ScratchEntries,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlaLimitExceeded {
    pub dimension: GlaLimitDimension,
    pub limit: u64,
    pub observed: u128,
}

impl core::fmt::Display for GlaLimitExceeded {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "GLA {:?} limit exceeded: observed {}, limit {}",
            self.dimension, self.observed, self.limit
        )
    }
}
impl core::error::Error for GlaLimitExceeded {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GlaExecutionStats {
    pub work_units: u64,
    pub scratch_entries: u64,
}

#[derive(Clone, PartialEq, Eq)]
pub struct GlaExecution<Row = VId> {
    pub value: Vec<Row>,
    pub stats: GlaExecutionStats,
}

impl<Row> core::fmt::Debug for GlaExecution<Row> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GlaExecution")
            .field("value", &"[REDACTED]")
            .field("row_count", &self.value.len())
            .field("stats", &self.stats)
            .finish()
    }
}

#[derive(Debug)]
pub enum GlaExecutionError<E> {
    Source(E),
    Limit(GlaLimitExceeded),
}

impl<E: core::fmt::Display> core::fmt::Display for GlaExecutionError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Source(error) => error.fmt(f),
            Self::Limit(error) => error.fmt(f),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for GlaExecutionError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Limit(error) => Some(error),
        }
    }
}

fn increment(
    value: u64,
    limit: u64,
    dimension: GlaLimitDimension,
) -> Result<u64, GlaLimitExceeded> {
    let observed = u128::from(value) + 1;
    if observed > u128::from(limit) {
        return Err(GlaLimitExceeded {
            dimension,
            limit,
            observed,
        });
    }
    Ok(observed as u64)
}

fn charge(
    stats: &mut GlaExecutionStats,
    limits: GlaExecutionLimits,
    event: GlaExecutionEvent,
) -> Result<(), GlaLimitExceeded> {
    let mut next = *stats;
    next.work_units = increment(
        next.work_units,
        limits.max_work_units,
        GlaLimitDimension::WorkUnits,
    )?;
    if event == GlaExecutionEvent::ScratchEntry {
        next.scratch_entries = increment(
            next.scratch_entries,
            limits.max_scratch_entries,
            GlaLimitDimension::ScratchEntries,
        )?;
    }
    *stats = next;
    Ok(())
}

type Adjacency = BTreeMap<VId, Vec<VId>>;
type Index = BTreeMap<(RelationId, GlaDirection), Adjacency>;

fn push_neighbor<E>(
    adjacency: &mut Adjacency,
    source: VId,
    destination: VId,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    control(GlaExecutionEvent::ScratchEntry)?;
    adjacency.entry(source).or_default().push(destination);
    Ok(())
}

/// In-place iterative heapsort; each comparison/swap crosses the work seam.
fn sort_neighbors<E>(
    values: &mut [VId],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    let len = values.len();
    for root in (0..len / 2).rev() {
        sift_neighbors(values, root, len, control)?;
    }
    for end in (1..len).rev() {
        control(GlaExecutionEvent::Work)?;
        values.swap(0, end);
        sift_neighbors(values, 0, end, control)?;
    }
    Ok(())
}

fn sift_neighbors<E>(
    values: &mut [VId],
    mut root: usize,
    end: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    while root < end / 2 {
        let mut child = 2 * root + 1;
        if child + 1 < end {
            control(GlaExecutionEvent::Work)?;
            if values[child + 1] > values[child] {
                child += 1;
            }
        }
        control(GlaExecutionEvent::Work)?;
        if values[root] >= values[child] {
            break;
        }
        control(GlaExecutionEvent::Work)?;
        values.swap(root, child);
        root = child;
    }
    Ok(())
}

fn build_index<E>(
    operators: &[GlaOperator],
    edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Index, E> {
    let mut index = Index::new();
    for operator in operators {
        if let GlaOperator::ScanEdges {
            relation,
            direction,
        }
        | GlaOperator::Expand {
            relation,
            direction,
            ..
        } = operator
        {
            index.entry((*relation, *direction)).or_default();
        }
    }
    for (source, relation, destination) in edges {
        control(GlaExecutionEvent::Work)?;
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            let Some(adjacency) = index.get_mut(&(relation, direction)) else {
                continue;
            };
            match direction {
                GlaDirection::Forward => push_neighbor(adjacency, source, destination, control)?,
                GlaDirection::Reverse => push_neighbor(adjacency, destination, source, control)?,
                GlaDirection::Undirected => {
                    push_neighbor(adjacency, source, destination, control)?;
                    if source != destination {
                        push_neighbor(adjacency, destination, source, control)?;
                    }
                }
            }
        }
    }
    for adjacency in index.values_mut() {
        for neighbors in adjacency.values_mut() {
            control(GlaExecutionEvent::Work)?;
            sort_neighbors(neighbors, control)?;
        }
    }
    Ok(index)
}

struct Execution<F, C, P, Row> {
    test_vertex: F,
    control: C,
    project: P,
    predicate_cache: BTreeMap<(usize, VId), bool>,
    projected: ProjectedRows<Row>,
}

impl<F, C, P, Row: GlaOutput> Execution<F, C, P, Row> {
    fn visit<E>(
        &mut self,
        operators: &[GlaOperator],
        ordinal: usize,
        bindings: &mut Vec<VId>,
        index: &Index,
    ) -> Result<(), E>
    where
        F: FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        C: FnMut(GlaExecutionEvent) -> Result<(), E>,
        P: FnMut(&GlaOperator, &[VId], &mut ProjectedRows<Row>, &mut C) -> Result<(), E>,
    {
        let Some(operator) = operators.get(ordinal) else {
            return Ok(());
        };
        (self.control)(GlaExecutionEvent::Work)?;
        match operator {
            GlaOperator::Select { slot, predicates } => {
                let Some(vid) = bindings.get(slot.ordinal() as usize).copied() else {
                    return Ok(());
                };
                let key = (ordinal, vid);
                let keep = if let Some(keep) = self.predicate_cache.get(&key) {
                    *keep
                } else {
                    (self.control)(GlaExecutionEvent::ScratchEntry)?;
                    let keep = (self.test_vertex)(vid, predicates)?;
                    self.predicate_cache.insert(key, keep);
                    keep
                };
                if keep {
                    self.visit(operators, ordinal + 1, bindings, index)?;
                }
            }
            GlaOperator::VertexIdentity { left, right, equal } => {
                if let (Some(left), Some(right)) = (
                    bindings.get(left.ordinal() as usize),
                    bindings.get(right.ordinal() as usize),
                ) && (left == right) == *equal
                {
                    self.visit(operators, ordinal + 1, bindings, index)?;
                }
            }
            GlaOperator::Expand {
                source,
                relation,
                direction,
            } => {
                let Some(source) = bindings.get(source.ordinal() as usize).copied() else {
                    return Ok(());
                };
                if let Some(neighbors) = index
                    .get(&(*relation, *direction))
                    .and_then(|adjacency| adjacency.get(&source))
                {
                    for destination in neighbors {
                        bindings.push(*destination);
                        let result = self.visit(operators, ordinal + 1, bindings, index);
                        let _ = bindings.pop();
                        result?;
                    }
                }
            }
            GlaOperator::Project { .. }
            | GlaOperator::ProjectBindings { .. }
            | GlaOperator::ProjectValues { .. } => {
                (self.project)(operator, bindings, &mut self.projected, &mut self.control)?;
            }
            GlaOperator::Empty
            | GlaOperator::ScanVertices
            | GlaOperator::ScanEdges { .. }
            | GlaOperator::Distinct
            | GlaOperator::OrderByVertexId
            | GlaOperator::OrderByBindings
            | GlaOperator::OrderByValues
            | GlaOperator::Limit { .. } => {}
        }
        Ok(())
    }
}

impl<Row: GlaIdentityOutput> GlaPlan<Row> {
    /// Unlimited execution preserves the source's original error type.
    pub fn execute<E>(
        &self,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
        test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
    ) -> Result<Vec<Row>, E> {
        self.execute_with_control(vertices, edges, test_vertex, |_| Ok(()))
    }

    pub fn execute_with_limits<E>(
        &self,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
        mut test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        limits: GlaExecutionLimits,
    ) -> Result<GlaExecution<Row>, GlaExecutionError<E>> {
        let mut stats = GlaExecutionStats::default();
        let value = self.execute_with_control(
            vertices,
            edges,
            |vid, predicates| test_vertex(vid, predicates).map_err(GlaExecutionError::Source),
            |event| charge(&mut stats, limits, event).map_err(GlaExecutionError::Limit),
        )?;
        Ok(GlaExecution { value, stats })
    }

    /// Predicate-only sources are admitted only for identity-only row types.
    pub fn execute_with_control<E>(
        &self,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
        test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        control: impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Vec<Row>, E> {
        self.execute_projected(
            vertices,
            edges,
            test_vertex,
            control,
            |operator, bindings, projected, control| {
                Row::collect(operator, bindings, projected, control)
            },
        )
    }
}

impl<Row: GlaOutput> GlaPlan<Row> {
    /// Project borrowed canonical properties from the same immutable source as
    /// predicate reads. A property failure propagates unchanged, never as null.
    /// None means an absent property, not an unreadable source. The resolver's
    /// returned references must remain stable throughout this execution.
    pub fn execute_with_properties_control<'a, E>(
        &self,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
        test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        mut property: impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        control: impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Vec<Row>, E> {
        self.execute_projected(
            vertices,
            edges,
            test_vertex,
            control,
            |operator, bindings, projected, control| {
                Row::collect_properties(operator, bindings, projected, &mut property, control)
            },
        )
    }

    /// One evaluator body. Only its sealed terminal projection depends on row
    /// shape; sources, traversal, ordering and the release tail are shared.
    fn execute_projected<E, F, C, P>(
        &self,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
        test_vertex: F,
        mut control: C,
        project: P,
    ) -> Result<Vec<Row>, E>
    where
        F: FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        C: FnMut(GlaExecutionEvent) -> Result<(), E>,
        P: FnMut(&GlaOperator, &[VId], &mut ProjectedRows<Row>, &mut C) -> Result<(), E>,
    {
        let operators = self.operators();
        let index = if self.scans_edges() {
            build_index(operators, edges, &mut control)?
        } else {
            Index::new()
        };
        // Only the compiler-owned terminal DISTINCT selects duplicate removal.
        let distinct = matches!(operators.iter().rev().nth(2), Some(GlaOperator::Distinct));
        let mut execution = Execution {
            test_vertex,
            control,
            project,
            predicate_cache: BTreeMap::new(),
            projected: ProjectedRows::<Row>::new(distinct),
        };
        let mut bindings = Vec::new();
        match operators.first() {
            Some(GlaOperator::ScanVertices) => {
                for vid in vertices {
                    (execution.control)(GlaExecutionEvent::Work)?;
                    bindings.clear();
                    bindings.push(vid);
                    execution.visit(operators, 1, &mut bindings, &index)?;
                }
            }
            Some(GlaOperator::ScanEdges {
                relation,
                direction,
            }) => {
                if let Some(adjacency) = index.get(&(*relation, *direction)) {
                    for (source, destinations) in adjacency {
                        for destination in destinations {
                            (execution.control)(GlaExecutionEvent::Work)?;
                            bindings.clear();
                            bindings.extend([*source, *destination]);
                            execution.visit(operators, 1, &mut bindings, &index)?;
                        }
                    }
                }
            }
            _ => {}
        }
        let (offset, count) = match operators.last() {
            Some(GlaOperator::Limit { offset, count }) => (
                usize::try_from(*offset).unwrap_or(usize::MAX),
                count
                    .and_then(|count| usize::try_from(count).ok())
                    .unwrap_or(usize::MAX),
            ),
            _ => (0, usize::MAX),
        };
        let mut value = Vec::new();
        for row in execution.projected.into_rows().skip(offset).take(count) {
            (execution.control)(GlaExecutionEvent::ResultRow)?;
            value.push(row);
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BoundPlan, EdgeDirection, RelationBind, ReturnProjection};
    use fgdb_delta_types::{LabelId, PropertyKeyId};
    use fgdb_types::CanonicalScalar;
    use std::cell::Cell;

    fn bound(two_hops: bool) -> BoundPlan {
        let bind = RelationBind::new()
            .with_relation("R", RelationId(1))
            .with_relation("S", RelationId(2));
        bind.bind(if two_hops {
            "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c"
        } else {
            "MATCH (a)-[:R]->(b) RETURN b"
        })
        .unwrap()
    }

    fn orient(
        source: VId,
        destination: VId,
        direction: EdgeDirection,
        two: bool,
    ) -> Vec<(VId, VId)> {
        match direction {
            EdgeDirection::Incoming if two => vec![(destination, source)],
            EdgeDirection::Undirected if source != destination => {
                vec![(source, destination), (destination, source)]
            }
            _ => vec![(source, destination)],
        }
    }

    fn reference(plan: &BoundPlan, edges: &[(VId, RelationId, VId)]) -> Vec<VId> {
        let mut bag = Vec::new();
        for &(source, relation, destination) in edges {
            if Some(relation) != plan.relation {
                continue;
            }
            for (a, b) in orient(
                source,
                destination,
                plan.direction,
                plan.hop2_relation.is_some(),
            ) {
                if plan.neq.is_some() && a == b || plan.eq.is_some() && a != b {
                    continue;
                }
                if let Some(hop2) = plan.hop2_relation {
                    for &(source2, relation2, destination2) in edges {
                        if relation2 != hop2 {
                            continue;
                        }
                        for (via, c) in orient(source2, destination2, plan.direction, true) {
                            if b == via {
                                bag.push(match plan.projection {
                                    ReturnProjection::Source => a,
                                    ReturnProjection::Destination => b,
                                    ReturnProjection::Hop2Destination => c,
                                });
                            }
                        }
                    }
                } else {
                    bag.push(if plan.projection == ReturnProjection::Source {
                        a
                    } else {
                        b
                    });
                }
            }
        }
        bag.sort_unstable();
        bag.dedup();
        bag.into_iter()
            .skip(plan.skip.unwrap_or(0) as usize)
            .take(plan.limit.unwrap_or(u64::MAX) as usize)
            .collect()
    }

    #[test]
    fn exhaustive_small_multigraph_matches_independent_path_enumeration() {
        let universe = [
            (VId(1), RelationId(1), VId(1)),
            (VId(1), RelationId(1), VId(2)),
            (VId(1), RelationId(1), VId(2)),
            (VId(2), RelationId(1), VId(3)),
            (VId(2), RelationId(2), VId(1)),
            (VId(2), RelationId(2), VId(3)),
            (VId(3), RelationId(2), VId(3)),
        ];
        for mask in 0..(1_u32 << universe.len()) {
            let edges: Vec<_> = universe
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1_u32 << *i) != 0)
                .map(|(_, edge)| *edge)
                .collect();
            for two_hops in [false, true] {
                for direction in [
                    EdgeDirection::Outgoing,
                    EdgeDirection::Incoming,
                    EdgeDirection::Undirected,
                ] {
                    for projection in [
                        ReturnProjection::Source,
                        ReturnProjection::Destination,
                        ReturnProjection::Hop2Destination,
                    ] {
                        let mut plan = bound(two_hops);
                        plan.direction = direction;
                        plan.projection = projection;
                        for identity in 0..3 {
                            plan.eq = (identity == 1).then(|| ("a".into(), "b".into()));
                            plan.neq = (identity == 2).then(|| ("a".into(), "b".into()));
                            let actual = GlaPlan::lower(&plan)
                                .execute([], edges.iter().copied(), |_, _| Ok::<_, ()>(true))
                                .unwrap();
                            assert_eq!(
                                actual,
                                reference(&plan, &edges),
                                "mask={mask}, plan={plan:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn input_is_consumed_once_and_predicates_are_memoized() {
        let mut plan = bound(false);
        plan.dst_prop = Some((PropertyKeyId(4), 7));
        let admitted = Cell::new(0);
        let reads = Cell::new(0);
        let edges = (0..100).map(|_| {
            admitted.set(admitted.get() + 1);
            (VId(1), RelationId(1), VId(2))
        });
        let rows = GlaPlan::lower(&plan)
            .execute([], edges, |vid, predicates| {
                reads.set(reads.get() + 1);
                assert_eq!(vid, VId(2));
                Ok::<_, ()>(predicates.iter().all(|predicate| {
                    predicate.matches(&[], &[(PropertyKeyId(4), CanonicalScalar::Int(7))])
                }))
            })
            .unwrap();
        assert_eq!(rows, vec![VId(2)]);
        assert_eq!(admitted.get(), 100);
        assert_eq!(reads.get(), 1);
    }

    #[test]
    fn a_late_source_failure_never_returns_partial_results() {
        let mut plan = bound(false);
        plan.dst_label = Some(LabelId(1));
        let result = GlaPlan::lower(&plan).execute(
            [],
            [
                (VId(1), RelationId(1), VId(2)),
                (VId(1), RelationId(1), VId(3)),
            ],
            |vid, _| {
                if vid == VId(3) {
                    Err("unreadable vertex")
                } else {
                    Ok(true)
                }
            },
        );
        assert_eq!(result, Err("unreadable vertex"));
    }

    #[test]
    fn distinct_and_order_precede_skip_and_limit() {
        let mut plan = bound(false);
        plan.skip = Some(1);
        plan.limit = Some(1);
        let edges = [
            (VId(1), RelationId(1), VId(3)),
            (VId(1), RelationId(1), VId(2)),
            (VId(1), RelationId(1), VId(2)),
        ];
        assert_eq!(
            GlaPlan::lower(&plan)
                .execute([], edges, |_, _| Ok::<_, ()>(true))
                .unwrap(),
            vec![VId(3)]
        );
        plan.skip = Some(u64::MAX);
        assert!(
            GlaPlan::lower(&plan)
                .execute([], edges, |_, _| Ok::<_, ()>(true))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn exact_limits_succeed_and_one_below_each_dimension_refuses() {
        let logical = GlaPlan::lower(&bound(true));
        let edges = [
            (VId(1), RelationId(1), VId(2)),
            (VId(2), RelationId(2), VId(3)),
        ];
        let run = |limits| logical.execute_with_limits([], edges, |_, _| Ok::<_, ()>(true), limits);
        let measured = run(GlaExecutionLimits::new(u64::MAX, u64::MAX)).unwrap();
        assert_eq!(measured.value, vec![VId(3)]);
        let exact =
            GlaExecutionLimits::new(measured.stats.work_units, measured.stats.scratch_entries);
        assert_eq!(run(exact).unwrap(), measured);
        assert!(matches!(
            run(GlaExecutionLimits::new(
                exact.max_work_units - 1,
                exact.max_scratch_entries
            )),
            Err(GlaExecutionError::Limit(GlaLimitExceeded {
                dimension: GlaLimitDimension::WorkUnits,
                ..
            }))
        ));
        assert!(matches!(
            run(GlaExecutionLimits::new(
                exact.max_work_units,
                exact.max_scratch_entries - 1
            )),
            Err(GlaExecutionError::Limit(GlaLimitExceeded {
                dimension: GlaLimitDimension::ScratchEntries,
                ..
            }))
        ));
    }

    #[test]
    fn work_limit_interrupts_path_fanout_even_when_limit_is_one() {
        let mut plan = bound(true);
        plan.limit = Some(1);
        let logical = GlaPlan::lower(&plan);
        let mut edges = Vec::new();
        for i in 2..22 {
            edges.push((VId(1), RelationId(1), VId(i)));
            for j in 100..120 {
                edges.push((VId(i), RelationId(2), VId(j)));
            }
        }
        let wide = logical
            .execute_with_limits(
                [],
                edges.iter().copied(),
                |_, _| Ok::<_, ()>(true),
                GlaExecutionLimits::new(u64::MAX, u64::MAX),
            )
            .unwrap();
        assert_eq!(wide.value, vec![VId(100)]);
        assert!(matches!(
            logical.execute_with_limits(
                [],
                edges,
                |_, _| Ok::<_, ()>(true),
                GlaExecutionLimits::new(wide.stats.work_units - 1, u64::MAX)
            ),
            Err(GlaExecutionError::Limit(_))
        ));
    }

    #[test]
    fn zero_scratch_refuses_before_predicate_reads_and_growth() {
        let mut plan = bound(false);
        plan.dst_label = Some(LabelId(1));
        let reads = Cell::new(0);
        let result = GlaPlan::lower(&plan).execute_with_limits(
            [],
            [(VId(1), RelationId(1), VId(2))],
            |_, _| {
                reads.set(reads.get() + 1);
                Ok::<_, ()>(true)
            },
            GlaExecutionLimits::new(100, 0),
        );
        assert!(matches!(
            result,
            Err(GlaExecutionError::Limit(GlaLimitExceeded {
                dimension: GlaLimitDimension::ScratchEntries,
                observed: 1,
                ..
            }))
        ));
        assert_eq!(reads.get(), 0);
    }

    #[test]
    fn cancellation_propagates_unchanged_during_expansion() {
        let logical = GlaPlan::lower(&bound(true));
        let calls = Cell::new(0);
        let result = logical.execute_with_control(
            [],
            [
                (VId(1), RelationId(1), VId(2)),
                (VId(2), RelationId(2), VId(3)),
            ],
            |_, _| Ok(true),
            |_| {
                calls.set(calls.get() + 1);
                if calls.get() == 8 {
                    Err("cancelled")
                } else {
                    Ok(())
                }
            },
        );
        assert_eq!(result, Err("cancelled"));
        assert_eq!(calls.get(), 8);
    }

    #[test]
    fn accounting_never_wraps_at_u64_max() {
        let mut stats = GlaExecutionStats {
            work_units: u64::MAX,
            scratch_entries: 0,
        };
        let before = stats;
        let error = charge(
            &mut stats,
            GlaExecutionLimits::new(u64::MAX, u64::MAX),
            GlaExecutionEvent::Work,
        )
        .unwrap_err();
        assert_eq!(error.observed, u128::from(u64::MAX) + 1);
        assert_eq!(stats, before);
    }

    #[test]
    fn controlled_ordering_matches_std_for_every_small_array() {
        let choices = [VId(0), VId(1_u128 << 100), VId(u128::MAX)];
        for len in 0..=7_u32 {
            for mut encoding in 0..3_usize.pow(len) {
                let mut values = Vec::new();
                for _ in 0..len {
                    values.push(choices[encoding % 3]);
                    encoding /= 3;
                }
                let mut expected = values.clone();
                expected.sort_unstable();
                let mut events = 0_u64;
                sort_neighbors(&mut values, &mut |event| {
                    assert_eq!(event, GlaExecutionEvent::Work);
                    events += 1;
                    Ok::<_, ()>(())
                })
                .unwrap();
                assert_eq!(values, expected);
                let depth = u64::from(usize::BITS - values.len().leading_zeros());
                assert!(events <= 8 * u64::from(len) * (depth + 1));
            }
        }
    }

    #[test]
    fn controlled_ordering_can_stop_at_every_event_without_losing_occurrences() {
        let original = [
            VId(9),
            VId(1),
            VId(9),
            VId(3),
            VId(2),
            VId(u128::MAX),
            VId(0),
        ];
        let mut expected = original;
        expected.sort_unstable();
        let mut total = 0;
        let mut completed = original;
        sort_neighbors(&mut completed, &mut |_| {
            total += 1;
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(completed, expected);
        for stop in 1..=total {
            let mut values = original;
            let mut events = 0;
            let result = sort_neighbors(&mut values, &mut |_| {
                events += 1;
                if events == stop { Err(stop) } else { Ok(()) }
            });
            assert_eq!(result, Err(stop));
            assert_eq!(events, stop);
            values.sort_unstable();
            assert_eq!(
                values, expected,
                "even a refused private index remains a permutation"
            );
        }
    }

    #[test]
    fn index_ordering_refuses_before_any_predicate_read() {
        let original: Vec<_> = (2..34).rev().map(VId).collect();
        let mut ordered = original.clone();
        let mut sort_events = 0;
        sort_neighbors(&mut ordered, &mut |_| {
            sort_events += 1;
            Ok::<_, ()>(())
        })
        .unwrap();
        let stop = 2 * original.len() + 1 + sort_events;
        let mut plan = bound(false);
        plan.dst_label = Some(LabelId(1));
        let mut events = 0;
        let mut reads = 0;
        let result = GlaPlan::lower(&plan).execute_with_control(
            [],
            original.into_iter().map(|vid| (VId(1), RelationId(1), vid)),
            |_, _| {
                reads += 1;
                Ok(true)
            },
            |_| {
                events += 1;
                if events == stop {
                    Err("stopped in adjacency ordering")
                } else {
                    Ok(())
                }
            },
        );
        assert_eq!(result, Err("stopped in adjacency ordering"));
        assert_eq!(events, stop);
        assert_eq!(reads, 0);
    }
}
