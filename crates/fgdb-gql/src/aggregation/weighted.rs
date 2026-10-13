//! Factor parallel topology occurrences before the SAME GLA binding visitor.
//!
//! For a positive pattern, a complete vertex assignment has one independent
//! edge choice per edge atom. Its bag weight is the product of the admitted
//! endpoint-triple multiplicities, even when an atom/variable repeats. COUNT
//! adds that weight; COUNT DISTINCT and MIN/MAX use positive support.
//! No edge-identity/trail semantics are claimed by this vertex-only profile.
//!
//! Vertex properties and predicates keep every binding and use the original
//! readers and selections. Only topology-only plans may eliminate hidden
//! variables. Scoped and edge-identity operators remain ineligible. A relation used
//! undirected must be exclusively undirected; opposite stored directions then
//! contribute to ONE unordered pair. Mixed directed/undirected uses fall back.
//! Sources are consumed once. Preprocessing, retained keys, multiplicity reads
//! and downstream execution share the original control/error path. This is a
//! bounded factorization law, not general FreeJoin, a COLT, or spill storage.

mod chain;
mod tree;

use super::*;
use crate::algebra::GlaDirection;
use core::num::NonZeroU64;

type TopologyKey = (VId, RelationId, VId);
type VisitError<E, C> = GqlQueryError<GraphAggregateError<E>, C>;

/// None proves a POSITIVE multiplicity exceeds u64::MAX. It is never a capped
/// numeric answer. Overflow matters only when a completed witness contributes
/// to COUNT: a doomed path or support-only summary must not fail prematurely.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Multiplicity(Option<NonZeroU64>);

impl Multiplicity {
    pub(super) const ONE: Self = Self(Some(NonZeroU64::MIN));

    pub(super) fn exact_count(self) -> Option<u64> {
        self.0.map(NonZeroU64::get)
    }

    fn increment(self) -> Self {
        Self(
            self.exact_count()
                .and_then(|n| n.checked_add(1))
                .and_then(NonZeroU64::new),
        )
    }

    fn product(self, other: Self) -> Self {
        Self(
            self.exact_count()
                .zip(other.exact_count())
                .and_then(|(a, b)| a.checked_mul(b))
                .and_then(NonZeroU64::new),
        )
    }

    fn sum(self, other: Self) -> Self {
        Self(
            self.exact_count()
                .zip(other.exact_count())
                .and_then(|(a, b)| a.checked_add(b))
                .and_then(NonZeroU64::new),
        )
    }
}

#[derive(Clone, Copy)]
struct EdgeAccess {
    source: usize,
    destination: usize,
    relation: RelationId,
    direction: GlaDirection,
}

fn eligible(aggregate: &PreparedGraphAggregate) -> bool {
    let operators = aggregate.input.plan().operators();
    // A mixed-type atom must retain each concrete relationship's multiplicity.
    // Until the weighted topology has a selector-aware normalization law, use
    // the ordinary governed GLA evaluator for those plans.
    if !aggregate.input.plan().single_typed_edges()
        || !matches!(operators.first(), Some(GlaOperator::ScanEdges { .. }))
        || aggregate.aggregates.iter().any(|a| {
            !matches!(
                a.function,
                GraphAggregateFunction::CountRows
                    | GraphAggregateFunction::Count
                    | GraphAggregateFunction::CountDistinct
                    | GraphAggregateFunction::Min
                    | GraphAggregateFunction::Max
            )
        })
    {
        return false;
    }
    operators.iter().all(|operator| match operator {
        GlaOperator::ScanEdges {
            relation,
            direction,
        }
        | GlaOperator::Expand {
            relation,
            direction,
            ..
        } => operators.iter().all(|other| match other {
            GlaOperator::ScanEdges {
                relation: other_relation,
                direction: other_direction,
            }
            | GlaOperator::Expand {
                relation: other_relation,
                direction: other_direction,
                ..
            } if relation == other_relation => {
                (*direction == GlaDirection::Undirected)
                    == (*other_direction == GlaDirection::Undirected)
            }
            _ => true,
        }),
        GlaOperator::VertexIdentity { .. }
        | GlaOperator::Select { .. }
        | GlaOperator::CompareProperties { .. }
        | GlaOperator::SelectBoolean { .. }
        | GlaOperator::OrderByValues
        | GlaOperator::Limit {
            offset: 0,
            count: None,
        } => true,
        GlaOperator::ProjectValues { columns } => columns.iter().all(|column| {
            matches!(
                column,
                ValueProjection::Vertex { .. } | ValueProjection::Property { .. }
            )
        }),
        GlaOperator::DifferentEdges { .. } => true,
        _ => false,
    }) && (!has_distinct_edges(operators) || distinct_edges_are_complete(operators))
}

fn has_distinct_edges(operators: &[GlaOperator]) -> bool {
    operators
        .iter()
        .any(|operator| matches!(operator, GlaOperator::DifferentEdges { .. }))
}

/// Whether the plan's DIFFERENT EDGES constraint pairs every two fixed atoms
/// of one relation (fgdb-ujaad). Two such atoms can bind one relationship only
/// when their bound (source, relation, destination) keys are equal, and atoms
/// of different relations never can. A clause-complete constraint therefore
/// has an exact per-binding weight: a falling factorial per equal-key group.
/// Any other constraint shape keeps the identified evaluator.
fn distinct_edges_are_complete(operators: &[GlaOperator]) -> bool {
    // Atom segment slots in compiler order: ScanEdges binds slot 1, each
    // Expand appends the next slot (algebra/pattern.rs constrain_edge).
    let mut atoms: Vec<(u32, Option<RelationId>)> = Vec::new();
    let mut next_slot = 2;
    for operator in operators {
        match operator {
            GlaOperator::ScanEdges { relation, .. } => atoms.push((1, relation.one())),
            GlaOperator::Expand { relation, .. } => {
                atoms.push((next_slot, relation.one()));
                next_slot += 1;
            }
            _ => {}
        }
    }
    let pair = |a: u32, b: u32| (a.min(b), a.max(b));
    let mut constrained = BTreeSet::new();
    for operator in operators {
        let GlaOperator::DifferentEdges { segments } = operator else {
            continue;
        };
        let Some((last, previous)) = segments.split_last() else {
            return false;
        };
        if !segments
            .iter()
            .all(|slot| atoms.iter().any(|(atom, _)| *atom == slot.ordinal()))
        {
            return false;
        }
        for slot in previous {
            constrained.insert(pair(slot.ordinal(), last.ordinal()));
        }
    }
    atoms.iter().enumerate().all(|(at, (left, relation))| {
        atoms[at + 1..]
            .iter()
            .all(|(right, other)| relation != other || constrained.contains(&pair(*left, *right)))
    })
}

/// The factorized evaluator answers this DIFFERENT EDGES aggregate exactly
/// (fgdb-ujaad). Computed and relational inputs keep their own evaluators.
pub(super) fn counts_distinct_edges(aggregate: &PreparedGraphAggregate) -> bool {
    aggregate.relational_input.is_none()
        && aggregate.computed_input.is_none()
        && has_distinct_edges(aggregate.input.plan().operators())
        && eligible(aggregate)
}

/// Whether this evaluator answers the identified plan exactly, so routing may
/// skip relationship identities: a clause-complete DIFFERENT EDGES count
/// (fgdb-ujaad), a support-only quantified reach (fgdb-20vwx) or a
/// support-only short chain (fgdb-h3gf6).
pub(super) fn answers_identified_exactly(aggregate: &PreparedGraphAggregate) -> bool {
    counts_distinct_edges(aggregate)
        || reach_shape(aggregate).is_some()
        || chain_shape(aggregate).is_some()
}

/// One directed, single-typed quantified atom whose aggregates read only the
/// SUPPORT of its (source, endpoint) bindings (fgdb-20vwx).
///
/// For ALL or TRAIL search with a lower bound of at most one, an endpoint is
/// reachable by a walk or trail of length within the bounds exactly when its
/// hop distance is within them. A shortest walk to another vertex is a simple
/// path, hence a trail. The shortest closed walk through the source is a
/// simple directed cycle, whose only edge into the source is its last. So
/// breadth-first reachability yields the exact support set: COUNT DISTINCT,
/// MIN and MAX, which ignore occurrence multiplicity, answer from it unchanged.
struct Reach<'a> {
    relation: RelationId,
    forward: bool,
    minimum: u32,
    maximum: u32,
    source_predicates: Vec<&'a [VertexPredicate]>,
    target_predicates: Vec<&'a [VertexPredicate]>,
    columns: &'a [ValueProjection],
    reads_source: bool,
}

fn reach_shape(aggregate: &PreparedGraphAggregate) -> Option<Reach<'_>> {
    if aggregate.relational_input.is_some()
        || aggregate.computed_input.is_some()
        || !aggregate.aggregates.iter().all(|a| {
            matches!(
                a.function,
                GraphAggregateFunction::CountDistinct
                    | GraphAggregateFunction::Min
                    | GraphAggregateFunction::Max
            )
        })
    {
        return None;
    }
    let mut operators = aggregate.input.plan().operators().iter().peekable();
    if !matches!(operators.next()?, GlaOperator::ScanVertices) {
        return None;
    }
    let mut source_predicates = Vec::new();
    while let Some(GlaOperator::Select { slot, predicates }) = operators.peek() {
        if slot.ordinal() != 0 {
            return None;
        }
        source_predicates.push(predicates.as_slice());
        operators.next();
    }
    let GlaOperator::VarLengthExpand {
        source,
        relation,
        direction,
        bounds,
        search,
    } = operators.next()?
    else {
        return None;
    };
    let forward = match direction {
        GlaDirection::Forward => true,
        GlaDirection::Reverse => false,
        GlaDirection::Undirected => return None,
    };
    if source.ordinal() != 0
        || bounds.minimum() > 1
        || !matches!(
            search,
            crate::algebra::GraphWalkSearch::All | crate::algebra::GraphWalkSearch::Trail
        )
    {
        return None;
    }
    let relation = relation.one()?;
    if let Some(GlaOperator::DifferentEdges { segments }) = operators.peek() {
        // Only the walk's own edge-uniqueness: it makes the walk a trail.
        if segments.len() != 1 || segments[0].ordinal() != 1 {
            return None;
        }
        operators.next();
    }
    let mut target_predicates = Vec::new();
    while let Some(GlaOperator::Select { slot, predicates }) = operators.peek() {
        if slot.ordinal() != 1 {
            return None;
        }
        target_predicates.push(predicates.as_slice());
        operators.next();
    }
    let GlaOperator::ProjectValues { columns } = operators.next()? else {
        return None;
    };
    let mut reads_source = false;
    for column in columns {
        let slot = match column {
            ValueProjection::Vertex { slot } | ValueProjection::Property { slot, .. } => *slot,
            _ => return None,
        };
        match slot.ordinal() {
            0 => reads_source = true,
            1 => {}
            _ => return None,
        }
    }
    for operator in operators {
        if !matches!(
            operator,
            GlaOperator::OrderByValues
                | GlaOperator::Limit {
                    offset: 0,
                    count: None,
                }
        ) {
            return None;
        }
    }
    Some(Reach {
        relation,
        forward,
        minimum: bounds.minimum(),
        maximum: bounds.maximum(),
        source_predicates,
        target_predicates,
        columns,
        reads_source,
    })
}

/// Vertices reachable from `sources` within the reach bounds (minimum <= 1).
fn reachable<E, C>(
    reach: &Reach<'_>,
    adjacency: &BTreeMap<VId, Vec<VId>>,
    sources: &[VId],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
) -> Result<BTreeSet<VId>, VisitError<E, C>> {
    let mut reached = BTreeSet::new();
    if reach.minimum > reach.maximum {
        return Ok(reached);
    }
    if reach.minimum == 0 {
        for &source in sources {
            control(GlaExecutionEvent::ScratchEntry)?;
            reached.insert(source);
        }
    }
    let mut expanded: BTreeSet<VId> = sources.iter().copied().collect();
    let mut frontier = sources.to_vec();
    let mut depth = 0;
    while !frontier.is_empty() && depth < reach.maximum {
        depth += 1;
        let mut next = Vec::new();
        for vertex in frontier {
            for &neighbor in adjacency.get(&vertex).map_or(&[][..], Vec::as_slice) {
                control(GlaExecutionEvent::Work)?;
                if reached.insert(neighbor) {
                    control(GlaExecutionEvent::ScratchEntry)?;
                }
                if expanded.insert(neighbor) {
                    next.push(neighbor);
                }
            }
        }
        frontier = next;
    }
    Ok(reached)
}

/// Feed the aggregate the deduplicated (source, endpoint) support bindings of
/// a quantified reach in canonical order. Without a column reading the source,
/// one multi-source search answers; otherwise each admitted source searches.
#[allow(clippy::too_many_arguments)]
fn visit_reach_bindings<'a, E, C, F, R, M, P>(
    reach: &Reach<'_>,
    vertices: impl IntoIterator<Item = VId>,
    edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
    mut test_vertex: F,
    mut property: R,
    mut control: M,
    mut visit: P,
) -> Result<(), VisitError<E, C>>
where
    F: FnMut(VId, &[VertexPredicate]) -> Result<bool, VisitError<E, C>>,
    R: FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, VisitError<E, C>>,
    M: FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
    P: FnMut(
        &[ValueProjection],
        &[Option<VId>],
        &mut R,
        &mut M,
        Multiplicity,
    ) -> Result<(), VisitError<E, C>>,
{
    let mut admitted = BTreeSet::new();
    for vertex in vertices {
        control(GlaExecutionEvent::Work)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        admitted.insert(vertex);
    }
    let mut sources = Vec::new();
    for &vertex in &admitted {
        let mut selected = true;
        for predicates in &reach.source_predicates {
            if !test_vertex(vertex, predicates)? {
                selected = false;
                break;
            }
        }
        if selected {
            sources.push(vertex);
        }
    }
    let mut adjacency: BTreeMap<VId, Vec<VId>> = BTreeMap::new();
    for (source, relation, destination) in edges {
        control(GlaExecutionEvent::Work)?;
        if relation != reach.relation {
            continue;
        }
        let (from, to) = if reach.forward {
            (source, destination)
        } else {
            (destination, source)
        };
        control(GlaExecutionEvent::ScratchEntry)?;
        adjacency.entry(from).or_default().push(to);
    }
    for neighbors in adjacency.values_mut() {
        neighbors.sort_unstable();
        neighbors.dedup();
    }
    let mut emit = |source: VId,
                    target: VId,
                    property: &mut R,
                    control: &mut M|
     -> Result<(), VisitError<E, C>> {
        for predicates in &reach.target_predicates {
            if !test_vertex(target, predicates)? {
                return Ok(());
            }
        }
        visit(
            reach.columns,
            &[Some(source), Some(target)],
            property,
            control,
            Multiplicity::ONE,
        )
    };
    if reach.reads_source {
        for &source in &sources {
            for target in reachable(reach, &adjacency, &[source], &mut control)? {
                emit(source, target, &mut property, &mut control)?;
            }
        }
    } else if let Some(&witness) = sources.first() {
        // No column reads the source: any admitted source witnesses the pair.
        for target in reachable(reach, &adjacency, &sources, &mut control)? {
            emit(witness, target, &mut property, &mut control)?;
        }
    }
    Ok(())
}

/// A directed chain of at most three hops, all in one orientation, whose
/// aggregates read only the SUPPORT of its (source, endpoint) bindings
/// (fgdb-h3gf6): a fixed chain of two or three single-typed atoms, or one
/// quantified atom with a lower bound of at least two (reach_shape owns lower
/// bounds of at most one) and an upper bound of at most three.
///
/// Walk counts per endpoint propagate level by level over relation
/// multiplicities, never enumerating paths. Under DIFFERENT EDGES (or TRAIL
/// search) an endpoint is in the support exactly when some walk to it is a
/// trail, i.e. when its trail count is positive. For two hops the only walks
/// reusing an edge stay on one self-loop; for three hops inclusion-exclusion
/// over the edge coincidences e1=e2 (a loop at the source), e2=e3 (a loop at
/// the endpoint) and e1=e3 (an edge out and back over another edge), whose
/// pairwise intersections all equal e1=e2=e3, gives
/// trails = walks - N12 - N23 - N13 + 2 N123. Coincidences need equal
/// relations. Multiple sources sum non-negative per-source trail counts.
struct Chain<'a> {
    relations: Vec<RelationId>,
    lengths: Vec<usize>,
    /// The endpoint's binding slot (the source is slot 0).
    last: usize,
    forward: bool,
    trail: bool,
    source_predicates: Vec<&'a [VertexPredicate]>,
    target_predicates: Vec<&'a [VertexPredicate]>,
    columns: &'a [ValueProjection],
    reads_source: bool,
}

fn support_only(aggregate: &PreparedGraphAggregate) -> bool {
    aggregate.relational_input.is_none()
        && aggregate.computed_input.is_none()
        && aggregate.aggregates.iter().all(|a| {
            matches!(
                a.function,
                GraphAggregateFunction::CountDistinct
                    | GraphAggregateFunction::Min
                    | GraphAggregateFunction::Max
            )
        })
}

fn oriented(direction: GlaDirection) -> Option<bool> {
    match direction {
        GlaDirection::Forward => Some(true),
        GlaDirection::Reverse => Some(false),
        GlaDirection::Undirected => None,
    }
}

fn chain_shape(aggregate: &PreparedGraphAggregate) -> Option<Chain<'_>> {
    if !support_only(aggregate) {
        return None;
    }
    let operators = aggregate.input.plan().operators();
    let mut relations = Vec::new();
    let mut forward = None;
    let mut lengths = Vec::new();
    let mut trail = false;
    let mut selects = Vec::new();
    let mut columns = None;
    let mut last = 0;
    let mut rest = operators.iter();
    match rest.next()? {
        GlaOperator::ScanEdges {
            relation,
            direction,
        } => {
            relations.push(relation.one()?);
            forward = Some(oriented(*direction)?);
            last = 1;
        }
        GlaOperator::ScanVertices => {}
        _ => return None,
    }
    let quantified = relations.is_empty();
    for operator in rest {
        match operator {
            GlaOperator::Expand {
                source,
                relation,
                direction,
            } if !quantified && columns.is_none() && source.ordinal() == last => {
                relations.push(relation.one()?);
                if forward != Some(oriented(*direction)?) {
                    return None;
                }
                last += 1;
            }
            GlaOperator::VarLengthExpand {
                source,
                relation,
                direction,
                bounds,
                search,
            } if quantified && last == 0 && source.ordinal() == 0 => {
                let (minimum, maximum) = (bounds.minimum(), bounds.maximum());
                if !(2..=3).contains(&minimum) || !(minimum..=3).contains(&maximum) {
                    return None;
                }
                match search {
                    crate::algebra::GraphWalkSearch::All => {}
                    crate::algebra::GraphWalkSearch::Trail => trail = true,
                    _ => return None,
                }
                let relation = relation.one()?;
                relations = vec![relation; maximum as usize];
                lengths = (minimum as usize..=maximum as usize).collect();
                forward = Some(oriented(*direction)?);
                last = 1;
            }
            GlaOperator::DifferentEdges { segments } => {
                // Fixed chains need the clause-complete constraint; a walk's
                // own uniqueness makes it a trail.
                if quantified && !(segments.len() == 1 && segments[0].ordinal() == 1) {
                    return None;
                }
                trail = true;
            }
            GlaOperator::Select { slot, predicates } => {
                selects.push((slot.ordinal(), predicates.as_slice()))
            }
            GlaOperator::ProjectValues { columns: projected } if columns.is_none() => {
                columns = Some(projected.as_slice())
            }
            GlaOperator::OrderByValues
            | GlaOperator::Limit {
                offset: 0,
                count: None,
            } if columns.is_some() => {}
            _ => return None,
        }
    }
    if relations.is_empty() {
        return None;
    }
    if !quantified {
        if !(2..=3).contains(&relations.len()) {
            return None;
        }
        if trail && !distinct_edges_are_complete(operators) {
            return None;
        }
        lengths = vec![relations.len()];
    }
    let columns = columns?;
    let mut source_predicates = Vec::new();
    let mut target_predicates = Vec::new();
    for (slot, predicates) in selects {
        match slot {
            0 => source_predicates.push(predicates),
            slot if slot == last => target_predicates.push(predicates),
            _ => return None,
        }
    }
    let mut reads_source = false;
    for column in columns {
        let slot = match column {
            ValueProjection::Vertex { slot } | ValueProjection::Property { slot, .. } => {
                slot.ordinal()
            }
            _ => return None,
        };
        match slot {
            0 => reads_source = true,
            slot if slot == last => {}
            _ => return None,
        }
    }
    Some(Chain {
        relations,
        lengths,
        last: usize::try_from(last).ok()?,
        forward: forward?,
        trail,
        source_predicates,
        target_predicates,
        columns,
        reads_source,
    })
}

/// Relation multiplicities oriented along the chain, and self-loop counts.
#[derive(Default)]
struct Hops {
    out: BTreeMap<VId, BTreeMap<VId, u128>>,
    loops: BTreeMap<VId, u128>,
}

impl Hops {
    fn multiplicity(&self, from: VId, to: VId) -> u128 {
        self.out
            .get(&from)
            .and_then(|out| out.get(&to))
            .copied()
            .unwrap_or(0)
    }
    fn loops(&self, vertex: VId) -> u128 {
        self.loops.get(&vertex).copied().unwrap_or(0)
    }
}

fn overflow<E, C>() -> VisitError<E, C> {
    GqlQueryError::Source(GraphAggregateError::ArithmeticOverflow { aggregate: 0 })
}

/// Endpoints with a positive trail (or walk) count from `sources`.
fn chain_support<E, C>(
    chain: &Chain<'_>,
    hops: &BTreeMap<RelationId, Hops>,
    sources: &[VId],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
) -> Result<BTreeSet<VId>, VisitError<E, C>> {
    let empty = Hops::default();
    let hop = |at: usize| hops.get(&chain.relations[at]).unwrap_or(&empty);
    let mut levels: Vec<BTreeMap<VId, u128>> = vec![sources.iter().map(|&a| (a, 1)).collect()];
    for at in 0..chain.relations.len() {
        let mut next = BTreeMap::<VId, u128>::new();
        for (&vertex, &walks) in &levels[at] {
            for (&far, &multiplicity) in hop(at).out.get(&vertex).into_iter().flatten() {
                control(GlaExecutionEvent::Work)?;
                let slot = next.entry(far).or_insert(0);
                *slot = walks
                    .checked_mul(multiplicity)
                    .and_then(|w| slot.checked_add(w))
                    .ok_or_else(overflow)?;
            }
        }
        for _ in &next {
            control(GlaExecutionEvent::ScratchEntry)?;
        }
        levels.push(next);
    }
    let same = |i: usize, j: usize| chain.relations[i] == chain.relations[j];
    let mut support = BTreeSet::new();
    for &length in &chain.lengths {
        for (&end, &walks) in &levels[length] {
            control(GlaExecutionEvent::Work)?;
            let mut trails = walks;
            if chain.trail && length == 2 && same(0, 1) && sources.binary_search(&end).is_ok() {
                trails = trails.saturating_sub(hop(0).loops(end));
            }
            if chain.trail && length == 3 {
                // Signed inclusion-exclusion; every term is a count of walks.
                let mut reused: u128 = 0;
                let mut add = |value: u128| {
                    reused
                        .checked_add(value)
                        .map(|sum| reused = sum)
                        .ok_or_else(overflow)
                };
                if same(0, 1) {
                    for &a in sources {
                        let loops = hop(0).loops(a);
                        if loops > 0 {
                            add(loops
                                .checked_mul(hop(2).multiplicity(a, end))
                                .ok_or_else(overflow)?)?;
                        }
                    }
                }
                if same(1, 2) {
                    let into = levels[1].get(&end).copied().unwrap_or(0);
                    add(hop(1).loops(end).checked_mul(into).ok_or_else(overflow)?)?;
                }
                if same(0, 2) {
                    for &a in sources {
                        let out = hop(0).multiplicity(a, end);
                        if out > 0 {
                            add(out
                                .checked_mul(hop(1).multiplicity(end, a))
                                .ok_or_else(overflow)?)?;
                        }
                    }
                }
                let triple = if same(0, 1) && same(1, 2) && sources.binary_search(&end).is_ok() {
                    hop(0).loops(end).checked_mul(2).ok_or_else(overflow)?
                } else {
                    0
                };
                trails = trails
                    .checked_add(triple)
                    .ok_or_else(overflow)?
                    .saturating_sub(reused);
            }
            if trails > 0 && support.insert(end) {
                control(GlaExecutionEvent::ScratchEntry)?;
            }
        }
    }
    Ok(support)
}

/// Feed the aggregate the deduplicated (source, endpoint) support bindings of
/// a short chain, like visit_reach_bindings: one multi-source pass unless a
/// column reads the source, then one pass per admitted source.
#[allow(clippy::too_many_arguments)]
fn visit_chain_bindings<'a, E, C, F, R, M, P>(
    chain: &Chain<'_>,
    vertices: impl IntoIterator<Item = VId>,
    edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
    mut test_vertex: F,
    mut property: R,
    mut control: M,
    mut visit: P,
) -> Result<(), VisitError<E, C>>
where
    F: FnMut(VId, &[VertexPredicate]) -> Result<bool, VisitError<E, C>>,
    R: FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, VisitError<E, C>>,
    M: FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
    P: FnMut(
        &[ValueProjection],
        &[Option<VId>],
        &mut R,
        &mut M,
        Multiplicity,
    ) -> Result<(), VisitError<E, C>>,
{
    let mut admitted = BTreeSet::new();
    for vertex in vertices {
        control(GlaExecutionEvent::Work)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        admitted.insert(vertex);
    }
    let mut sources = Vec::new();
    for &vertex in &admitted {
        let mut selected = true;
        for predicates in &chain.source_predicates {
            if !test_vertex(vertex, predicates)? {
                selected = false;
                break;
            }
        }
        if selected {
            sources.push(vertex);
        }
    }
    let mut hops = BTreeMap::<RelationId, Hops>::new();
    for (source, relation, destination) in edges {
        control(GlaExecutionEvent::Work)?;
        if !chain.relations.contains(&relation) {
            continue;
        }
        let (from, to) = if chain.forward {
            (source, destination)
        } else {
            (destination, source)
        };
        control(GlaExecutionEvent::ScratchEntry)?;
        let hop = hops.entry(relation).or_default();
        let slot = hop.out.entry(from).or_default().entry(to).or_insert(0);
        *slot = slot.checked_add(1).ok_or_else(overflow)?;
        if from == to {
            let loops = hop.loops.entry(from).or_insert(0);
            *loops = loops.checked_add(1).ok_or_else(overflow)?;
        }
    }
    let mut emit = |source: VId,
                    target: VId,
                    property: &mut R,
                    control: &mut M|
     -> Result<(), VisitError<E, C>> {
        for predicates in &chain.target_predicates {
            if !test_vertex(target, predicates)? {
                return Ok(());
            }
        }
        // Only the source and endpoint slots are projected; transit slots stay unbound.
        let mut bindings = vec![None; chain.last + 1];
        bindings[0] = Some(source);
        bindings[chain.last] = Some(target);
        visit(
            chain.columns,
            &bindings,
            property,
            control,
            Multiplicity::ONE,
        )
    };
    if chain.reads_source {
        for &source in &sources {
            for target in chain_support(chain, &hops, &[source], &mut control)? {
                emit(source, target, &mut property, &mut control)?;
            }
        }
    } else if let Some(&witness) = sources.first() {
        for target in chain_support(chain, &hops, &sources, &mut control)? {
            emit(witness, target, &mut property, &mut control)?;
        }
    }
    Ok(())
}

pub(super) fn needs_original_bindings<R>(plan: &crate::algebra::GlaPlan<R>) -> bool {
    plan.operators().iter().any(|operator| match operator {
        GlaOperator::Select { .. }
        | GlaOperator::CompareProperties { .. }
        | GlaOperator::SelectBoolean { .. } => true,
        GlaOperator::ProjectValues { columns } => columns
            .iter()
            .any(|column| matches!(column, ValueProjection::Property { .. })),
        _ => false,
    })
}

/// The ordinary edge scan walks a BTreeMap of oriented source identities and
/// completes every descendant scope before advancing its root. Guarded cyclic
/// intersection keeps the original root as its first ascending variable and
/// retains all original slots on this property-bearing path. Input edge
/// order and parallel occurrences do not break that property. Arbitrary node
/// iterators and transformed weighted/factorized plans claim no such ordering.
/// Keep this capability beside physical-path selection so future rewrites
/// cannot accidentally enable group retirement under a different root order.
pub(super) fn root_groups_are_contiguous(aggregate: &PreparedGraphAggregate) -> bool {
    matches!(
        aggregate.input.plan().operators().first(),
        Some(GlaOperator::ScanEdges { .. })
    ) && (!eligible(aggregate)
        || needs_original_bindings(aggregate.input.plan())
        || has_distinct_edges(aggregate.input.plan().operators()))
}

fn normalized(
    source: VId,
    relation: RelationId,
    destination: VId,
    undirected: bool,
) -> TopologyKey {
    if undirected && source > destination {
        (destination, relation, source)
    } else {
        (source, relation, destination)
    }
}

/// One complete binding's exact bag weight, or `None` when it admits no
/// relationship choice. Without DIFFERENT EDGES it is the product of the atoms'
/// endpoint-triple multiplicities. With it, atoms whose bound keys are equal
/// must choose distinct parallel relationships: k of them over m occurrences
/// admit m·(m-1)···(m-k+1) choices, none when k > m (fgdb-ujaad).
fn binding_weight<E, C>(
    accesses: &[EdgeAccess],
    topology: &BTreeMap<TopologyKey, Multiplicity>,
    bindings: &[Option<VId>],
    distinct: bool,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
) -> Result<Option<Multiplicity>, VisitError<E, C>> {
    let unavailable = || GqlQueryError::Source(GraphAggregateError::MultiplicityUnavailable);
    let mut keys = [(VId(0), RelationId(0), VId(0)); crate::algebra::MAX_PATTERN_EDGES];
    let keys = keys.get_mut(..accesses.len()).ok_or_else(unavailable)?;
    let mut weight = Multiplicity::ONE;
    for (access, slot) in accesses.iter().zip(keys.iter_mut()) {
        control(GlaExecutionEvent::Work)?;
        let source = bindings
            .get(access.source)
            .copied()
            .flatten()
            .ok_or_else(unavailable)?;
        let destination = bindings
            .get(access.destination)
            .copied()
            .flatten()
            .ok_or_else(unavailable)?;
        *slot = match access.direction {
            GlaDirection::Forward => (source, access.relation, destination),
            GlaDirection::Reverse => (destination, access.relation, source),
            GlaDirection::Undirected => normalized(source, access.relation, destination, true),
        };
        if !distinct {
            weight = weight.product(topology.get(&*slot).copied().ok_or_else(unavailable)?);
        }
    }
    if !distinct {
        return Ok(Some(weight));
    }
    for at in 0..keys.len() {
        if keys[..at].contains(&keys[at]) {
            continue;
        }
        control(GlaExecutionEvent::Work)?;
        let group = keys[at..].iter().filter(|key| **key == keys[at]).count() as u64;
        let available = topology.get(&keys[at]).copied().ok_or_else(unavailable)?;
        let Some(count) = available.exact_count() else {
            // More than u64::MAX occurrences: the product stays above-count.
            weight = weight.product(available);
            continue;
        };
        if group > count {
            return Ok(None);
        }
        for taken in 0..group {
            weight = weight.product(Multiplicity(NonZeroU64::new(count - taken)));
        }
    }
    Ok(Some(weight))
}

/// Ordinary/weighted inputs converge on visit_value_bindings, not a second
/// matcher. The callback receives one exact weight or an explicit above-count
/// state. Its accumulator decides whether that state matters to its function.
#[allow(clippy::too_many_arguments)]
pub(super) fn visit_bindings<'a, E, C, F, R, M, P>(
    aggregate: &PreparedGraphAggregate,
    vertices: impl IntoIterator<Item = VId>,
    edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
    test_vertex: F,
    property: R,
    mut control: M,
    mut visit: P,
) -> Result<(), VisitError<E, C>>
where
    F: FnMut(VId, &[VertexPredicate]) -> Result<bool, VisitError<E, C>>,
    R: FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, VisitError<E, C>>,
    M: FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
    P: FnMut(
        &[ValueProjection],
        &[Option<VId>],
        &mut R,
        &mut M,
        Multiplicity,
    ) -> Result<(), VisitError<E, C>>,
{
    let plan = aggregate.input.plan();
    if let Some(reach) = reach_shape(aggregate) {
        return visit_reach_bindings(
            &reach,
            vertices,
            edges,
            test_vertex,
            property,
            control,
            visit,
        );
    }
    if let Some(chain) = chain_shape(aggregate) {
        return visit_chain_bindings(
            &chain,
            vertices,
            edges,
            test_vertex,
            property,
            control,
            visit,
        );
    }
    if !eligible(aggregate) {
        return plan.visit_value_bindings(
            vertices,
            edges,
            test_vertex,
            property,
            control,
            |columns, bindings, property, control| {
                visit(columns, bindings, property, control, Multiplicity::ONE)
            },
        );
    }

    let mut accesses = Vec::new();
    let mut undirected = BTreeSet::new();
    let mut next_slot = 2;
    for operator in plan.operators() {
        control(GlaExecutionEvent::Work)?;
        let access = match operator {
            GlaOperator::ScanEdges {
                relation,
                direction,
            } => EdgeAccess {
                source: 0,
                destination: 1,
                relation: relation
                    .one()
                    .expect("weighted eligibility requires typed atoms"),
                direction: *direction,
            },
            GlaOperator::Expand {
                source,
                relation,
                direction,
            } => {
                let access = EdgeAccess {
                    source: source.ordinal() as usize,
                    destination: next_slot,
                    relation: relation
                        .one()
                        .expect("weighted eligibility requires typed atoms"),
                    direction: *direction,
                };
                next_slot += 1;
                access
            }
            _ => continue,
        };
        control(GlaExecutionEvent::ScratchEntry)?;
        accesses.push(access);
        if access.direction == GlaDirection::Undirected && !undirected.contains(&access.relation) {
            control(GlaExecutionEvent::ScratchEntry)?;
            undirected.insert(access.relation);
        }
    }

    let mut topology: BTreeMap<TopologyKey, Multiplicity> = BTreeMap::new();
    for (source, relation, destination) in edges {
        control(GlaExecutionEvent::Work)?;
        let key = normalized(
            source,
            relation,
            destination,
            undirected.contains(&relation),
        );
        if let Some(count) = topology.get_mut(&key) {
            *count = count.increment();
        } else {
            // One key/weight entry, before retaining the record. The original
            // snapshot-record allowance was already checked against the full
            // source, not this smaller derived relation.
            control(GlaExecutionEvent::ScratchEntry)?;
            topology.insert(key, Multiplicity::ONE);
        }
    }

    // Properties and positive predicates depend on vertex bindings, not edge
    // occurrences. Keep the original plan and ALL slots whenever either is
    // observed: topology-only contractions must not remove a predicate/read.
    // Selections still run in the ordinary visitor, BEFORE a complete witness
    // contributes its multiplicity. Rejected paths cannot cause COUNT overflow.
    // DIFFERENT EDGES needs every binding complete to see which atoms share a
    // bound key, so it bypasses the contractions below, which sum over hidden
    // assignments. The constraint operators leave the plan; their exact
    // effect is the per-binding falling-factorial weight (fgdb-ujaad).
    let distinct = has_distinct_edges(plan.operators());
    let stripped;
    let plan = if distinct {
        stripped = crate::algebra::GlaPlan::from_operators(
            plan.operators()
                .iter()
                .filter(|operator| !matches!(operator, GlaOperator::DifferentEdges { .. }))
                .cloned()
                .collect(),
        );
        &stripped
    } else {
        plan
    };
    let needs_original_bindings = needs_original_bindings(plan);
    if needs_original_bindings || distinct {
        // Keep all slots and the complete multiplicity/read callback. Only a
        // topology prefix followed by late reads may use intersection; its
        // guarded fallback is the original visitor, with no extra events.
        return chain::visit_original_bindings(
            plan,
            vertices,
            &topology,
            test_vertex,
            property,
            control,
            |columns, bindings, property, control| {
                let Some(weight) =
                    binding_weight(&accesses, &topology, bindings, distinct, control)?
                else {
                    return Ok(());
                };
                if let Some(count) = weight.exact_count() {
                    for _ in 1..count {
                        for operator in plan.operators() {
                            if matches!(
                                operator,
                                crate::algebra::GlaOperator::CompareProperties { .. }
                                    | crate::algebra::GlaOperator::SelectBoolean { .. }
                            ) {
                                crate::algebra_exec::compare_properties(
                                    operator, bindings, property, control,
                                )?;
                            }
                        }
                        for column in columns {
                            if let crate::algebra::ValueProjection::Property { slot, key } = column
                                && let Some(Some(vid)) = bindings.get(slot.ordinal() as usize)
                            {
                                control(GlaExecutionEvent::Work)?;
                                property(*vid, *key)?;
                            }
                        }
                    }
                }
                visit(columns, bindings, property, control, weight)
            },
        );
    }

    // Remove only a compiler-proved unprojected forest. Messages sum distinct
    // child assignments and multiply independent branches, without generating
    // their Cartesian product. Cyclic/projected core bindings still use GLA.
    // Both partitions preserve original edge order. All slots are remapped:
    // removed ones occupy a separate tail, so early branches cannot alias the
    // compact IDs of retained later variables in the completion-message maps.
    let reduced = plan.aggregate_core(&mut control)?;
    let mut branches = Vec::new();
    if let Some(core) = &reduced {
        let mut retained = 0;
        for at in 0..accesses.len() {
            control(GlaExecutionEvent::Work)?;
            let mut access = accesses[at];
            let unavailable =
                || GqlQueryError::Source(GraphAggregateError::MultiplicityUnavailable);
            access.source = core
                .slot_map
                .get(access.source)
                .ok_or_else(unavailable)?
                .ordinal() as usize;
            access.destination = core
                .slot_map
                .get(access.destination)
                .ok_or_else(unavailable)?
                .ordinal() as usize;
            if access.destination >= core.retained_width {
                control(GlaExecutionEvent::ScratchEntry)?;
                branches.push(access);
            } else {
                debug_assert!(access.source < core.retained_width);
                accesses[retained] = access;
                retained += 1;
            }
        }
        accesses.truncate(retained);
    }
    let forest = tree::Forest::build(&branches, &topology, &mut control)?;
    let execution_plan = reduced.as_ref().map_or(plan, |core| &core.plan);
    chain::visit_bindings(
        execution_plan,
        vertices,
        &topology,
        &accesses,
        &forest,
        test_vertex,
        property,
        control,
        visit,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText};

    fn query(text: &str) -> PreparedGraphAggregate {
        PreparedGraphAggregateText::prepare(text, |kind, _| match kind {
            GraphSymbolKind::Relation => Some(GraphSymbol::Relation(RelationId(1))),
            GraphSymbolKind::Property => Some(GraphSymbol::Property(PropertyKeyId(1))),
            GraphSymbolKind::Label => Some(GraphSymbol::Label(fgdb_delta_types::LabelId(1))),
        })
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
    }

    #[test]
    fn positive_count_weights_never_wrap_or_become_zero() {
        let max = Multiplicity(NonZeroU64::new(u64::MAX));
        assert_eq!(max.exact_count(), Some(u64::MAX));
        assert_eq!(max.increment().exact_count(), None);
        assert_eq!(max.product(Multiplicity::ONE).exact_count(), Some(u64::MAX));
        let two = Multiplicity::ONE.increment();
        assert_eq!(max.product(two).exact_count(), None);
        let mut weight = Multiplicity::ONE;
        for _ in 0..63 {
            weight = weight.product(two);
        }
        assert_eq!(weight.exact_count(), Some(1_u64 << 63));
        weight = weight.product(two);
        assert_eq!(weight.exact_count(), None);
        assert_eq!(weight.product(Multiplicity::ONE).exact_count(), None);
    }

    #[test]
    fn only_the_registered_vertex_support_and_count_profile_is_factored() {
        for text in [
            "MATCH (a)-[:R]->(b) RETURN a,COUNT(*) AS n,COUNT(b) AS m,COUNT(DISTINCT b) AS d,MIN(b) AS lo,MAX(b) AS hi GROUP BY a",
            // Two atoms of one type count homomorphisms only when repeated
            // edges are admitted (fgdb-vexfr).
            "MATCH REPEATABLE ELEMENTS (a)-[:R]-(b)-[:R]-(a) RETURN COUNT(*) AS n",
            "MATCH REPEATABLE ELEMENTS (a)-[:R]->(b)<-[:R]-(c) WHERE a<>c RETURN COUNT(*) AS n",
            "MATCH (a)-[:R]->(b) RETURN COUNT(b.n) AS n",
            "MATCH (a)-[:R]->(b) RETURN a.n,MIN(b.n) AS lo,MAX(b.n) AS hi GROUP BY a.n",
            "MATCH (a)-[:R]->(b) WHERE a.n=1 RETURN COUNT(*) AS n",
            "MATCH (a:L)-[:R]->(b) RETURN COUNT(*) AS n",
            "MATCH (a)-[:R]->(b) WHERE a.n=b.n RETURN COUNT(*) AS n",
            // Under the DIFFERENT EDGES default the same two shapes must not
            // share an edge. Their clause-complete constraint has an exact
            // per-binding falling-factorial weight (fgdb-ujaad); the laws
            // below hold it to the identified evaluator.
            "MATCH (a)-[:R]-(b)-[:R]-(a) RETURN COUNT(*) AS n",
            "MATCH (a)-[:R]->(b)<-[:R]-(c) WHERE a<>c RETURN COUNT(*) AS n",
        ] {
            assert!(eligible(&query(text)), "{text}");
        }
        for text in [
            "MATCH (a) RETURN COUNT(*) AS n",
            "MATCH (a)-[:R]->(b) RETURN SUM(b) AS n",
            "MATCH (a)-[:R]->(b)-[:R]-(c) RETURN COUNT(*) AS n",
            "MATCH (a)-[:R]->(b) OPTIONAL MATCH (b)-[:R]->(c) RETURN COUNT(*) AS n",
            "MATCH (a)-[:R]->(b) WHERE EXISTS { MATCH (b)-[:R]->(c) } RETURN COUNT(*) AS n",
        ] {
            assert!(!eligible(&query(text)), "{text}");
        }
    }

    /// A multigraph whose parallel edges and self-loops make DIFFERENT EDGES
    /// observable: 1, 2 and 3 parallel self-loops, parallel non-loop edges in
    /// both directions, 2-cycles, triangles and a sink.
    fn distinct_edge_graph() -> Vec<(EId, VId, RelationId, VId)> {
        let r = RelationId(1);
        [
            (0, 0),
            (0, 1),
            (0, 1),
            (1, 0),
            (1, 2),
            (2, 0),
            (2, 0),
            (1, 1),
            (2, 2),
            (2, 2),
            (3, 3),
            (3, 3),
            (3, 3),
            (3, 0),
            (0, 3),
            (2, 3),
            (4, 1),
        ]
        .into_iter()
        .enumerate()
        .map(|(eid, (src, dst))| (EId(eid as u128), VId(src), r, VId(dst)))
        .collect()
    }

    #[test]
    fn distinct_edge_counts_equal_the_identified_evaluator() {
        let edges = distinct_edge_graph();
        let properties = BTreeMap::from([
            (VId(0), CanonicalScalar::Int(7)),
            (VId(1), CanonicalScalar::Null),
            (VId(2), CanonicalScalar::Int(7)),
            (VId(3), CanonicalScalar::Int(-2)),
        ]);
        let policy = GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX);
        for pattern in [
            "(a)-[:R]->(b)-[:R]->(c)-[:R]->(a)",
            "(a)-[:R]->(b)-[:R]->(a)",
            "(a)-[:R]->(b)-[:R]->(c)",
            "(a)-[:R]->(b)<-[:R]-(c)",
            "(a)-[:R]->(b)-[:R]->(c)-[:R]->(b)",
            "(a)-[:R]-(b)-[:R]-(a)",
            "(a)-[:R]->(b)-[:R]->(c) WHERE a.n=7",
            "(a)-[:R]->(b)<-[:R]-(c) WHERE a<>c",
        ] {
            let prefix = format!(
                "MATCH {pattern} RETURN a.n AS root,COUNT(*) AS n,COUNT(b.n) AS c,COUNT(DISTINCT b.n) AS d,MIN(b.n) AS lo,MAX(b.n) AS hi"
            );
            let factored = query(&format!("{prefix} GROUP BY a.n"));
            // A hidden COLLECT keeps the same five aggregates on the identified
            // evaluator, which enforces DIFFERENT EDGES by relationship identity.
            let identified = query(&format!("{prefix},COLLECT(b.n) AS bag GROUP BY a.n"))
                .with_aggregate_output_prefix(5)
                .unwrap();
            assert!(counts_distinct_edges(&factored), "{pattern}");
            assert!(!counts_distinct_edges(&identified), "{pattern}");
            let run = |aggregate: &PreparedGraphAggregate| {
                aggregate
                    .execute_governed_with_element_maps(
                        edges.len() as u64,
                        (0..5).map(VId),
                        edges.iter().copied(),
                        |vid, predicates| {
                            Ok::<_, ()>(predicates.iter().all(|predicate| {
                                predicate.matches_borrowed(
                                    None,
                                    properties.get(&vid).map(|value| (PropertyKeyId(1), value)),
                                )
                            }))
                        },
                        |vid, _| Ok(properties.get(&vid)),
                        |_, _| Ok(None),
                        |_| Ok(None),
                        |_| Ok(None),
                        |_| Ok(None),
                        |_| Ok(None),
                        policy,
                        || Ok::<_, ()>(()),
                    )
                    .unwrap()
            };
            let actual = run(&factored);
            let expected = run(&identified);
            assert_eq!(actual.value, expected.value, "{pattern}");
            assert_eq!(actual.rows, expected.rows, "{pattern}");
        }
    }

    #[test]
    fn distinct_edge_triangles_count_falling_factorials_of_parallel_loops() {
        // Independent oracle: enumerate ordered triples of pairwise-distinct
        // relationships forming a directed 3-cycle.
        let edges = distinct_edge_graph();
        let mut expected = 0_u64;
        for (i, &(_, a, _, b)) in edges.iter().enumerate() {
            for (j, &(_, b2, _, c)) in edges.iter().enumerate() {
                for (k, &(_, c2, _, a2)) in edges.iter().enumerate() {
                    if b2 == b && c2 == c && a2 == a && i != j && j != k && i != k {
                        expected += 1;
                    }
                }
            }
        }
        // The three loops at VId(3) alone contribute 3*2*1 ordered choices,
        // where homomorphic counting would give 27.
        assert!(expected >= 6);
        let aggregate = query("MATCH (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) RETURN COUNT(*) AS n");
        assert!(counts_distinct_edges(&aggregate));
        let result = aggregate
            .execute_governed(
                edges.len() as u64,
                (0..5).map(VId),
                edges
                    .iter()
                    .map(|&(_, src, relation, dst)| (src, relation, dst)),
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok::<_, ()>(None),
                GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(result.value.len(), 1);
        assert_eq!(
            result.value[0].get(0),
            Some(&GraphAggregateValue::Count(expected))
        );
    }

    #[test]
    fn property_groups_equal_ordinary_bags_for_nulls_parallel_edges_and_directions() {
        let properties = BTreeMap::from([
            (VId(0), CanonicalScalar::Int(7)),
            (VId(1), CanonicalScalar::Null),
            (VId(2), CanonicalScalar::Int(7)),
            (VId(3), CanonicalScalar::Int(-2)),
        ]);
        let edges = [
            (VId(0), RelationId(1), VId(1)),
            (VId(0), RelationId(1), VId(1)),
            (VId(0), RelationId(1), VId(2)),
            (VId(2), RelationId(1), VId(3)),
            (VId(2), RelationId(1), VId(3)),
            (VId(3), RelationId(1), VId(3)),
            (VId(3), RelationId(1), VId(0)),
            (VId(4), RelationId(1), VId(1)),
        ];
        for pattern in [
            "(a)-[:R]->(b)",
            "(a)<-[:R]-(b)",
            "(a)-[:R]-(b)",
            "(a)-[:R]->(c)-[:R]->(b)",
            "(a)-[:R]-(b)-[:R]-(a)",
            "(a:L)-[:R]->(b)",
            "(a)-[:R]->(b) WHERE a.n=7",
            "(a)-[:R]->(b) WHERE a.n=b.n",
            "(a)-[:R]->(b) WHERE b.n IS NULL",
            "(a)-[:R]->(b) WHERE NOT (b.n=7)",
            "(a)-[:R]->(b) WHERE a.n=7 OR b.n IS NULL",
            "(a)-[:R]->(c)-[:R]->(b) WHERE c.n=-2",
        ] {
            // Both sides count homomorphisms: the two-atom patterns admit a
            // repeated edge, as the factored engine does (fgdb-vexfr).
            let prefix = format!(
                "MATCH REPEATABLE ELEMENTS {pattern} RETURN a.n AS root,COUNT(*) AS n,COUNT(b.n) AS c,COUNT(DISTINCT b.n) AS d,MIN(b.n) AS lo,MAX(b.n) AS hi"
            );
            let factored = query(&format!("{prefix} GROUP BY a.n"));
            // A hidden, order-sensitive COLLECT forces ordinary bag visitation.
            // All five visible aggregates and the input source stay identical.
            let ordinary = query(&format!("{prefix},COLLECT(b.n) AS bag GROUP BY a.n"))
                .with_aggregate_output_prefix(5)
                .unwrap();
            assert!(eligible(&factored));
            assert!(!eligible(&ordinary));
            let run = |aggregate: &PreparedGraphAggregate| {
                aggregate
                    .execute_governed(
                        edges.len() as u64,
                        (0..5).map(VId),
                        edges,
                        |vid, predicates| {
                            Ok::<_, ()>(predicates.iter().all(|predicate| {
                                predicate.matches_borrowed(
                                    (vid == VId(0)).then_some(fgdb_delta_types::LabelId(1)),
                                    properties.get(&vid).map(|value| (PropertyKeyId(1), value)),
                                )
                            }))
                        },
                        |vid, _| Ok::<_, ()>(properties.get(&vid)),
                        GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX),
                        || Ok::<_, ()>(()),
                    )
                    .unwrap()
            };
            let actual = run(&factored);
            let expected = run(&ordinary);
            assert_eq!(actual.value, expected.value, "{pattern}");
            assert_eq!(actual.rows, expected.rows, "{pattern}");
        }
    }

    #[test]
    fn property_factorization_preserves_source_errors_and_every_checkpoint() {
        let aggregate = query("MATCH (a)-[:R]->(b) RETURN COUNT(b.n) AS n");
        let edges = [(VId(0), RelationId(1), VId(1)); 8];
        let policy = GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX);
        let error = aggregate.execute_governed(
            edges.len() as u64,
            [VId(0), VId(1)],
            edges,
            |_, _| Ok::<_, &str>(true),
            |_, _| Err::<Option<&CanonicalScalar>, _>("property source failed"),
            policy,
            || Ok::<_, ()>(()),
        );
        assert!(matches!(
            error,
            Err(GqlQueryError::Source(GraphAggregateError::Source(
                "property source failed"
            )))
        ));
        let value = CanonicalScalar::Int(1);
        let mut events = 0;
        aggregate
            .execute_governed(
                edges.len() as u64,
                [VId(0), VId(1)],
                edges,
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(Some(&value)),
                policy,
                || {
                    events += 1;
                    Ok::<_, usize>(())
                },
            )
            .unwrap();
        for stop in 1..=events {
            let mut at = 0;
            let result = aggregate.execute_governed(
                edges.len() as u64,
                [VId(0), VId(1)],
                edges,
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(Some(&value)),
                policy,
                || {
                    at += 1;
                    if at == stop { Err(stop) } else { Ok(()) }
                },
            );
            assert!(matches!(result, Err(GqlQueryError::Interrupted(n)) if n == stop));
            assert_eq!(at, stop);
        }
    }

    #[test]
    fn filtering_and_null_support_precede_overflowed_multiplicities() {
        // Eight atoms over the same 256 parallel self-loops: homomorphisms,
        // so repeated edges are admitted (fgdb-vexfr).
        let pattern = format!("REPEATABLE ELEMENTS (a){}", "-[:R]->(a)".repeat(8));
        // 256 choices on each of eight atoms is 2^64 complete occurrences.
        // Only one vertex assignment is needed; expanding this bag is infeasible.
        let edges = [(VId(0), RelationId(1), VId(0)); 256];
        let policy = GqlQueryPolicy::new(256, 1, 100_000, 10_000);
        let run = |aggregate: &PreparedGraphAggregate,
                   source: &[(VId, RelationId, VId)],
                   value: &CanonicalScalar| {
            aggregate.execute_governed(
                source.len() as u64,
                [VId(0)],
                source.iter().copied(),
                |_, predicates| {
                    Ok::<_, ()>(predicates.iter().all(|predicate| {
                        predicate.matches_borrowed([], [(PropertyKeyId(1), value)])
                    }))
                },
                |_, _| Ok(Some(value)),
                policy,
                || Ok::<_, ()>(()),
            )
        };
        let rejected = query(&format!("MATCH {pattern} WHERE a.n=0 RETURN COUNT(*) AS n"));
        assert!(eligible(&rejected));
        let value = CanonicalScalar::Int(1);
        assert_eq!(
            run(&rejected, &edges, &value).unwrap().value,
            run(&rejected, &[], &value).unwrap().value
        );
        let counted = query(&format!("MATCH {pattern} RETURN COUNT(a.n) AS n"));
        assert!(matches!(
            run(&counted, &edges, &value),
            Err(GqlQueryError::Source(
                GraphAggregateError::ArithmeticOverflow { aggregate: 0 }
            ))
        ));
        let null = CanonicalScalar::Null;
        assert_eq!(
            run(&counted, &edges, &null).unwrap().value,
            run(&counted, &[], &null).unwrap().value
        );
        let support = query(&format!(
            "MATCH {pattern} RETURN COUNT(DISTINCT a.n) AS n,MIN(a.n) AS lo,MAX(a.n) AS hi"
        ));
        assert_eq!(
            run(&support, &edges, &value).unwrap().value,
            run(&support, &edges[..1], &value).unwrap().value
        );
    }

    #[test]
    fn support_only_reach_answers_from_breadth_first_bindings() {
        for text in [
            "MATCH (a)-[:R]->{1,3}(b) RETURN COUNT(DISTINCT b) AS n",
            "MATCH (a)<-[:R]-{1,2}(b) RETURN MIN(b.n) AS lo,MAX(b.n) AS hi",
            "MATCH REPEATABLE ELEMENTS (a)-[:R]->{0,4}(b) RETURN a,COUNT(DISTINCT b) AS n GROUP BY a",
            "MATCH (a)-[:R]->{1,}(b) RETURN COUNT(DISTINCT b) AS n",
        ] {
            assert!(reach_shape(&query(text)).is_some(), "{text}");
        }
        for text in [
            // Occurrence-sensitive aggregates count walks or trails.
            "MATCH (a)-[:R]->{1,3}(b) RETURN COUNT(*) AS n",
            "MATCH (a)-[:R]->{1,3}(b) RETURN COUNT(b) AS n",
            // A lower bound above one can need a longer route than the shortest.
            "MATCH (a)-[:R]->{2,3}(b) RETURN COUNT(DISTINCT b) AS n",
            // An undirected closed trail cannot reuse its single edge.
            "MATCH (a)-[:R]-{1,3}(b) RETURN COUNT(DISTINCT b) AS n",
        ] {
            assert!(reach_shape(&query(text)).is_none(), "{text}");
        }
    }

    #[test]
    fn support_only_reach_equals_trail_and_walk_enumeration() {
        let mut edges = distinct_edge_graph();
        // A longer cycle 1->4->... and a tail so four hops matter.
        for (eid, (src, dst)) in [(1, 4), (3, 4), (4, 5)].into_iter().enumerate() {
            edges.push((EId(100 + eid as u128), VId(src), RelationId(1), VId(dst)));
        }
        let properties = BTreeMap::from([
            (VId(0), CanonicalScalar::Int(7)),
            (VId(1), CanonicalScalar::Null),
            (VId(2), CanonicalScalar::Int(7)),
            (VId(3), CanonicalScalar::Int(-2)),
            (VId(5), CanonicalScalar::Int(9)),
        ]);
        let policy = GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX);
        let mut checked = 0;
        for mode in ["", "REPEATABLE ELEMENTS "] {
            for (pattern, bounds) in [
                ("(a)-[:R]->", "{1,1}"),
                ("(a)-[:R]->", "{1,2}"),
                ("(a)-[:R]->", "{1,4}"),
                ("(a)-[:R]->", "{0,3}"),
                ("(a)<-[:R]-", "{1,3}"),
                ("(a)<-[:R]-", "{0,2}"),
            ] {
                for tail in ["(b)", "(b) WHERE a.n=7", "(b) WHERE b.n IS NOT NULL"] {
                    for (returns, grouped) in [
                        (
                            "COUNT(DISTINCT b) AS n,MIN(b.n) AS lo,MAX(b.n) AS hi",
                            false,
                        ),
                        (
                            "a,COUNT(DISTINCT b) AS n,MIN(b.n) AS lo,MAX(b.n) AS hi",
                            true,
                        ),
                    ] {
                        let prefix =
                            format!("MATCH {mode}{pattern}{bounds}{tail} RETURN {returns}");
                        let group = if grouped { " GROUP BY a" } else { "" };
                        let factored = query(&format!("{prefix}{group}"));
                        // A hidden COLLECT keeps the same three aggregates on the
                        // enumerating evaluators (trails, or walks when repeatable).
                        let enumerated = query(&format!("{prefix},COLLECT(b.n) AS bag{group}"))
                            .with_aggregate_output_prefix(3)
                            .unwrap();
                        assert!(reach_shape(&factored).is_some(), "{prefix}");
                        assert!(reach_shape(&enumerated).is_none(), "{prefix}");
                        let run = |aggregate: &PreparedGraphAggregate| {
                            aggregate
                                .execute_governed_with_element_maps(
                                    edges.len() as u64,
                                    (0..7).map(VId),
                                    edges.iter().copied(),
                                    |vid, predicates| {
                                        Ok::<_, ()>(predicates.iter().all(|predicate| {
                                            predicate.matches_borrowed(
                                                None,
                                                properties
                                                    .get(&vid)
                                                    .map(|value| (PropertyKeyId(1), value)),
                                            )
                                        }))
                                    },
                                    |vid, _| Ok(properties.get(&vid)),
                                    |_, _| Ok(None),
                                    |_| Ok(None),
                                    |_| Ok(None),
                                    |_| Ok(None),
                                    |_| Ok(None),
                                    policy,
                                    || Ok::<_, ()>(()),
                                )
                                .unwrap()
                        };
                        let actual = run(&factored);
                        let expected = run(&enumerated);
                        assert_eq!(actual.value, expected.value, "{prefix}{group}");
                        assert_eq!(actual.rows, expected.rows, "{prefix}{group}");
                        checked += 1;
                    }
                }
            }
        }
        assert_eq!(checked, 72);
    }

    #[test]
    fn support_only_chains_equal_trail_and_walk_enumeration() {
        let two = |text: &str| {
            PreparedGraphAggregateText::prepare(text, |kind, name| match (kind, name) {
                (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
                (GraphSymbolKind::Relation, _) => Some(GraphSymbol::Relation(RelationId(1))),
                (GraphSymbolKind::Property, _) => Some(GraphSymbol::Property(PropertyKeyId(1))),
                (GraphSymbolKind::Label, _) => {
                    Some(GraphSymbol::Label(fgdb_delta_types::LabelId(1)))
                }
            })
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap()
        };
        let mut edges = distinct_edge_graph();
        // S: loops (single and parallel), a reciprocal pair and a tail.
        for (eid, (src, dst)) in [
            (0, 0),
            (0, 1),
            (1, 0),
            (2, 2),
            (1, 3),
            (3, 1),
            (4, 4),
            (4, 4),
            (3, 5),
        ]
        .into_iter()
        .enumerate()
        {
            edges.push((EId(200 + eid as u128), VId(src), RelationId(2), VId(dst)));
        }
        edges.push((EId(300), VId(4), RelationId(1), VId(5)));
        let properties = BTreeMap::from([
            (VId(0), CanonicalScalar::Int(7)),
            (VId(1), CanonicalScalar::Null),
            (VId(2), CanonicalScalar::Int(7)),
            (VId(3), CanonicalScalar::Int(-2)),
            (VId(5), CanonicalScalar::Int(9)),
        ]);
        let policy = GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX);
        let mut checked = 0;
        // Trail and walk answers per (shape, filter, grouping): the trail
        // corrections must change some answers, or this law is vacuous.
        let mut answers = BTreeMap::<String, Vec<GraphAggregateRow>>::new();
        let mut differing = 0;
        for mode in ["", "REPEATABLE ELEMENTS "] {
            for (pattern, end) in [
                ("(a)-[:R]->(b)-[:R]->(e)", "e"),
                ("(a)-[:R]->(b)-[:R]->(c)-[:R]->(e)", "e"),
                ("(a)<-[:R]-(b)<-[:R]-(e)", "e"),
                ("(a)<-[:R]-(b)<-[:R]-(c)<-[:R]-(e)", "e"),
                ("(a)-[:R]->(b)-[:S]->(c)-[:R]->(e)", "e"),
                ("(a)-[:S]->(b)-[:S]->(c)-[:S]->(e)", "e"),
                ("(a)-[:S]->(b)-[:R]->(c)-[:R]->(e)", "e"),
                ("(a)-[:R]->{2,2}(e)", "e"),
                ("(a)-[:R]->{3,3}(e)", "e"),
                ("(a)-[:S]->{2,3}(e)", "e"),
                ("(a)<-[:R]-{2,3}(e)", "e"),
            ] {
                for tail in ["", " WHERE a.n=7", " WHERE e.n IS NOT NULL"] {
                    for (returns, grouped) in [
                        (
                            format!(
                                "COUNT(DISTINCT {end}) AS n,MIN({end}.n) AS lo,MAX({end}.n) AS hi"
                            ),
                            false,
                        ),
                        (
                            format!(
                                "a,COUNT(DISTINCT {end}) AS n,MIN({end}.n) AS lo,MAX({end}.n) AS hi"
                            ),
                            true,
                        ),
                    ] {
                        let prefix = format!("MATCH {mode}{pattern}{tail} RETURN {returns}");
                        let group = if grouped { " GROUP BY a" } else { "" };
                        let factored = two(&format!("{prefix}{group}"));
                        // A hidden COLLECT keeps the same aggregates on the
                        // enumerating evaluators (trails, or walks when repeatable).
                        let enumerated = two(&format!("{prefix},COLLECT({end}.n) AS bag{group}"))
                            .with_aggregate_output_prefix(3)
                            .unwrap();
                        assert!(chain_shape(&factored).is_some(), "{prefix}{group}");
                        assert!(chain_shape(&enumerated).is_none(), "{prefix}{group}");
                        let run = |aggregate: &PreparedGraphAggregate| {
                            aggregate
                                .execute_governed_with_element_maps(
                                    edges.len() as u64,
                                    (0..7).map(VId),
                                    edges.iter().copied(),
                                    |vid, predicates| {
                                        Ok::<_, ()>(predicates.iter().all(|predicate| {
                                            predicate.matches_borrowed(
                                                None,
                                                properties
                                                    .get(&vid)
                                                    .map(|value| (PropertyKeyId(1), value)),
                                            )
                                        }))
                                    },
                                    |vid, _| Ok(properties.get(&vid)),
                                    |_, _| Ok(None),
                                    |_| Ok(None),
                                    |_| Ok(None),
                                    |_| Ok(None),
                                    |_| Ok(None),
                                    policy,
                                    || Ok::<_, ()>(()),
                                )
                                .unwrap()
                        };
                        let actual = run(&factored);
                        let expected = run(&enumerated);
                        assert_eq!(actual.value, expected.value, "{prefix}{group}");
                        assert_eq!(actual.rows, expected.rows, "{prefix}{group}");
                        let key = format!("{pattern}{tail}{group}");
                        match answers.get(&key) {
                            Some(trails) => differing += usize::from(*trails != actual.value),
                            None => {
                                answers.insert(key, actual.value);
                            }
                        }
                        checked += 1;
                    }
                }
            }
        }
        assert_eq!(checked, 132);
        assert!(
            differing >= 10,
            "only {differing} shapes separate trails from walks"
        );
    }

    #[test]
    fn chain_support_leaves_reach_occurrences_and_branches_to_other_evaluators() {
        for text in [
            // reach_shape owns lower bounds of at most one.
            "MATCH (a)-[:R]->{1,3}(b) RETURN COUNT(DISTINCT b) AS n",
            // Occurrence-sensitive aggregates count walks or trails.
            "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN COUNT(*) AS n",
            // A transit predicate, a mixed orientation, an undirected hop,
            // a cycle closure and four hops are not this shape.
            "MATCH (a)-[:R]->(b)-[:R]->(c) WHERE b.n=7 RETURN COUNT(DISTINCT c) AS n",
            "MATCH (a)-[:R]->(b)<-[:R]-(c) RETURN COUNT(DISTINCT c) AS n",
            "MATCH (a)-[:R]-(b)-[:R]-(c) RETURN COUNT(DISTINCT c) AS n",
            "MATCH (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) RETURN COUNT(DISTINCT b) AS n",
            "MATCH (a)-[:R]->(b)-[:R]->(c)-[:R]->(d)-[:R]->(e) RETURN COUNT(DISTINCT e) AS n",
            "MATCH (a)-[:R]->{2,4}(b) RETURN COUNT(DISTINCT b) AS n",
        ] {
            assert!(chain_shape(&query(text)).is_none(), "{text}");
        }
    }
}
