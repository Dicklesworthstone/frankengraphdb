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
mod topology;
mod tree;

use super::*;
use crate::algebra::GlaDirection;
use core::num::NonZeroU64;

type TopologyKey = (VId, RelationId, VId);
type VisitError<E, C> = GqlQueryError<GraphAggregateError<E>, C>;

/// Each topology key once, ascending, beside its multiplicity: the ordered
/// map's contents as one sorted array. Its readers only iterate it in key
/// order or look a key up, so the array answers exactly as the map did,
/// without a node allocation per key (a quarter of a 730k-edge triangle
/// count was building and walking the map).
///
/// A topology weighed from a positioned graph also keeps each key's endpoint
/// positions, in key order, for the visitors that index vertices by position.
#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct Topology {
    entries: Vec<(TopologyKey, Multiplicity)>,
    positions: Option<Vec<[u32; 2]>>,
}

impl Topology {
    /// From keys that are already ascending and distinct.
    fn from_sorted(entries: Vec<(TopologyKey, Multiplicity)>) -> Self {
        debug_assert!(entries.windows(2).all(|pair| pair[0].0 < pair[1].0));
        Self {
            entries,
            positions: None,
        }
    }
    /// From ascending distinct keys and each one's endpoint positions.
    fn from_sorted_positioned(
        entries: Vec<(TopologyKey, Multiplicity)>,
        positions: Vec<[u32; 2]>,
    ) -> Self {
        debug_assert_eq!(entries.len(), positions.len());
        Self {
            positions: Some(positions),
            ..Self::from_sorted(entries)
        }
    }
    fn get(&self, key: &TopologyKey) -> Option<&Multiplicity> {
        self.entries
            .binary_search_by(|(known, _)| known.cmp(key))
            .ok()
            .map(|at| &self.entries[at].1)
    }
    #[cfg(test)]
    fn contains_key(&self, key: &TopologyKey) -> bool {
        self.get(key).is_some()
    }
    fn keys(&self) -> impl Iterator<Item = &TopologyKey> + '_ {
        self.entries.iter().map(|(key, _)| key)
    }
    fn values(&self) -> impl Iterator<Item = &Multiplicity> + '_ {
        self.entries.iter().map(|(_, weight)| weight)
    }
    /// The keys with their endpoints by position, when weighed positioned.
    fn positioned_keys(&self) -> Option<impl Iterator<Item = (u32, RelationId, u32)> + '_> {
        let positions = self.positions.as_ref()?;
        Some(self.entries.iter().zip(positions).map(
            |(&((_, relation, _), _), &[source, destination])| (source, relation, destination),
        ))
    }
}

impl IntoIterator for Topology {
    type Item = (TopologyKey, Multiplicity);
    type IntoIter = std::vec::IntoIter<(TopologyKey, Multiplicity)>;
    /// Ascending by key, as the ordered map's owned iteration.
    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl<'t> IntoIterator for &'t Topology {
    type Item = (&'t TopologyKey, &'t Multiplicity);
    type IntoIter = core::iter::Map<
        core::slice::Iter<'t, (TopologyKey, Multiplicity)>,
        fn(&'t (TopologyKey, Multiplicity)) -> (&'t TopologyKey, &'t Multiplicity),
    >;
    /// Ascending by key, as the ordered map iterated.
    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter().map(|(key, weight)| (key, weight))
    }
}

impl From<BTreeMap<TopologyKey, Multiplicity>> for Topology {
    fn from(map: BTreeMap<TopologyKey, Multiplicity>) -> Self {
        Self::from_sorted(map.into_iter().collect())
    }
}

impl FromIterator<(TopologyKey, Multiplicity)> for Topology {
    /// As collecting into the ordered map: a repeated key keeps its last weight.
    fn from_iter<I: IntoIterator<Item = (TopologyKey, Multiplicity)>>(entries: I) -> Self {
        entries.into_iter().collect::<BTreeMap<_, _>>().into()
    }
}

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

/// A plain vertex scan: ScanVertices, then only Selects on that vertex, one
/// projection of it, and the terminal order/page of an aggregate child
/// (fgdb-61p1n). Each vertex binds once, so the GLA's identity-keyed predicate
/// cache never hits: every test is a miss that pays a cache insert. This shape
/// runs the same steps directly, charging exactly the GLA's events.
struct VertexScan<'p> {
    selects: Vec<&'p [VertexPredicate]>,
    columns: &'p [ValueProjection],
}

fn vertex_scan(aggregate: &PreparedGraphAggregate) -> Option<VertexScan<'_>> {
    let operators = aggregate.input.plan().operators();
    if !matches!(operators.first(), Some(GlaOperator::ScanVertices)) {
        return None;
    }
    let mut selects = Vec::new();
    let mut columns = None;
    for operator in &operators[1..] {
        match operator {
            GlaOperator::Select { slot, predicates }
                if columns.is_none() && slot.ordinal() == 0 =>
            {
                selects.push(predicates.as_slice());
            }
            GlaOperator::ProjectValues { columns: output }
                if columns.is_none()
                    && output.iter().all(|column| match column {
                        ValueProjection::Vertex { slot }
                        | ValueProjection::Property { slot, .. } => slot.ordinal() == 0,
                        _ => false,
                    }) =>
            {
                columns = Some(output.as_slice());
            }
            GlaOperator::OrderByValues if columns.is_some() => {}
            GlaOperator::Limit {
                offset: 0,
                count: None,
            } if columns.is_some() => {}
            _ => return None,
        }
    }
    Some(VertexScan {
        selects,
        columns: columns?,
    })
}

/// The GLA's vertex scan, step for step: one Work per vertex, then per
/// Select one Work, the predicate cache entry it would insert (each vertex
/// is new to it), the literal comparison work and the test; then one Work
/// and the projection's visit. A refused Select ends the vertex.
fn visit_vertex_scan<'a, E, C, F, R, M, P>(
    scan: &VertexScan<'_>,
    vertices: impl IntoIterator<Item = VId>,
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
    'vertices: for vertex in vertices {
        control(GlaExecutionEvent::Work)?;
        for predicates in &scan.selects {
            control(GlaExecutionEvent::Work)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            for predicate in *predicates {
                for _ in 0..predicate.comparison_work_units() {
                    control(GlaExecutionEvent::Work)?;
                }
            }
            if !test_vertex(vertex, predicates)? {
                continue 'vertices;
            }
        }
        control(GlaExecutionEvent::Work)?;
        visit(
            scan.columns,
            &[Some(vertex)],
            &mut property,
            &mut control,
            Multiplicity::ONE,
        )?;
    }
    Ok(())
}

/// An aggregate over one directed typed edge whose only other operators are
/// endpoint selections and a projection of its two endpoints (fgdb-61p1n).
/// Each admitted edge that passes its endpoints is one binding, so weighing
/// them into a topology first (a sort of every arrival, an index over the
/// keys, a weight search per binding) cannot pay for itself.
struct SingleScan<'p> {
    access: EdgeAccess,
    columns: &'p [ValueProjection],
    /// Every column reads slot 0 alone: bindings that share their slot-0
    /// vertex are the same input to every key and aggregate, so each such
    /// anchor is visited once, weighted by its passing edges, in ascending
    /// vertex order (the root order `RootGroups` requires). Otherwise only
    /// an ungrouped aggregate of vertex columns qualifies, edge by edge.
    anchored: bool,
    /// The aggregate is ungrouped COUNT(*) over vertex columns: every passing
    /// edge is the same input, whatever its endpoints, and no column read
    /// can refuse, so the scan only counts them.
    counts_only: bool,
}

fn single_scan(aggregate: &PreparedGraphAggregate) -> Option<SingleScan<'_>> {
    let operators = aggregate.input.plan().operators();
    let Some(GlaOperator::ScanEdges {
        relation,
        direction,
    }) = operators.first()
    else {
        return None;
    };
    if *direction == GlaDirection::Undirected {
        return None;
    }
    let mut columns = None;
    for operator in &operators[1..] {
        match operator {
            GlaOperator::Select { slot, .. } if columns.is_none() && slot.ordinal() < 2 => {}
            GlaOperator::ProjectValues { columns: output } if columns.is_none() => {
                columns = Some(output.as_slice());
            }
            GlaOperator::OrderByValues if columns.is_some() => {}
            GlaOperator::Limit {
                offset: 0,
                count: None,
            } if columns.is_some() => {}
            _ => return None,
        }
    }
    let columns = columns?;
    let anchored = columns.iter().all(|column| match column {
        ValueProjection::Vertex { slot } | ValueProjection::Property { slot, .. } => {
            slot.ordinal() == 0
        }
        _ => false,
    });
    let vertices = columns
        .iter()
        .all(|column| matches!(column, ValueProjection::Vertex { slot } if slot.ordinal() < 2));
    if !anchored && !(vertices && aggregate.keys.is_empty()) {
        return None;
    }
    let counts_only = aggregate.keys.is_empty()
        && aggregate
            .aggregates
            .iter()
            .all(|bound| bound.function == GraphAggregateFunction::CountRows)
        && columns
            .iter()
            .all(|column| matches!(column, ValueProjection::Vertex { .. }));
    Some(SingleScan {
        access: EdgeAccess {
            source: 0,
            destination: 1,
            relation: relation.one()?,
            direction: *direction,
        },
        columns,
        anchored,
        counts_only,
    })
}

/// Edges in arrival order, each charged one Work as the topology charges
/// it. Endpoints are tested in `topology::Endpoints::admit`'s order: source
/// then destination, each vertex once per side (once in all when both sides
/// share their predicates), its verdict one ScratchEntry. Unanchored, a passing edge is visited with multiplicity
/// one. Anchored, it joins its slot-0 vertex's count (a new anchor costs one
/// ScratchEntry); each anchor is then visited once with that multiplicity,
/// after re-reading its property columns once per further edge, as the
/// weighted visitor re-reads them per further occurrence. Without columns
/// (COUNT(*) alone) no anchor is kept: one visit carries every passing edge.
/// For the counts, distinct counts and extrema `eligible` admits, each equals
/// the topology's weighted sum over the same edges.
fn visit_single_scan<'a, E, C, F, R, M, P>(
    scan: &SingleScan<'_>,
    plan: &crate::algebra::GlaPlan<GraphValueRow>,
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
    let filters = endpoint_filters(plan.operators(), core::slice::from_ref(&scan.access));
    let (from, to) = filters
        .as_ref()
        .and_then(|filters| filters.get(&scan.access.relation))
        .map_or((&[][..], &[][..]), |(from, to)| {
            (from.as_slice(), to.as_slice())
        });
    // Both sides under one predicate list (`(:N)-[:E]->(:N)`) give a vertex
    // one verdict wherever it is reached: a second test could only repeat
    // the first, so each vertex is read and charged once, not once per side.
    let shared = from == to;
    // Probed, never iterated: no order of theirs reaches a result.
    let mut tested = [
        crate::id_hash::IdMap::<VId, bool>::default(),
        crate::id_hash::IdMap::<VId, bool>::default(),
    ];
    // Each side's newest verdict. Edges arrive grouped by an endpoint, so
    // most probes repeat the one before; a known vertex charges nothing.
    let mut newest: [Option<(VId, bool)>; 2] = [None; 2];
    // Anchor -> (passing edges, the first partner): probed, then sorted.
    let mut anchors = crate::id_hash::IdMap::<VId, (u64, VId)>::default();
    // The anchor of the newest passing edge and how many passing edges
    // followed its run's first, not yet added to its count.
    let mut run: Option<(VId, u64)> = None;
    // No column reads a binding, or an ungrouped COUNT(*) reads only vertex
    // columns it never uses: every passing edge is the same input, so the
    // scan only counts them, keeping the first binding.
    let constant = scan.columns.is_empty() || scan.counts_only;
    let mut total: Option<(u64, VId, VId)> = None;
    for (source, relation, destination) in edges {
        control(GlaExecutionEvent::Work)?;
        if relation != scan.access.relation {
            continue;
        }
        let mut passes = |side: usize, vertex: VId, predicates: &[VertexPredicate]| {
            if predicates.is_empty() {
                return Ok(true);
            }
            if let Some((known, verdict)) = newest[side]
                && known == vertex
            {
                return Ok(verdict);
            }
            let verdict = match tested[if shared { 0 } else { side }].entry(vertex) {
                std::collections::hash_map::Entry::Occupied(known) => *known.get(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let result = test_vertex(vertex, predicates)?;
                    control(GlaExecutionEvent::ScratchEntry)?;
                    *entry.insert(result)
                }
            };
            newest[side] = Some((vertex, verdict));
            Ok::<_, VisitError<E, C>>(verdict)
        };
        if !passes(0, source, from)? || !passes(1, destination, to)? {
            continue;
        }
        let (anchor, partner) = match scan.access.direction {
            GlaDirection::Forward => (source, destination),
            _ => (destination, source),
        };
        if constant {
            match &mut total {
                Some((count, ..)) => *count += 1,
                None => total = Some((1, anchor, partner)),
            }
            continue;
        }
        if scan.anchored {
            match &mut run {
                Some((last, more)) if *last == anchor => *more += 1,
                _ => {
                    if let Some((last, more)) = run.take() {
                        anchors
                            .get_mut(&last)
                            .expect("a run's anchor was entered")
                            .0 += more;
                    }
                    match anchors.entry(anchor) {
                        std::collections::hash_map::Entry::Occupied(mut entry) => {
                            entry.get_mut().0 += 1;
                        }
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            control(GlaExecutionEvent::ScratchEntry)?;
                            entry.insert((1, partner));
                        }
                    }
                    run = Some((anchor, 0));
                }
            }
            continue;
        }
        visit(
            scan.columns,
            &[Some(anchor), Some(partner)],
            &mut property,
            &mut control,
            Multiplicity::ONE,
        )?;
    }
    if let Some((count, anchor, partner)) = total {
        return visit(
            scan.columns,
            &[Some(anchor), Some(partner)],
            &mut property,
            &mut control,
            Multiplicity(NonZeroU64::new(count)),
        );
    }
    if let Some((last, more)) = run {
        anchors
            .get_mut(&last)
            .expect("a run's anchor was entered")
            .0 += more;
    }
    let mut anchors: Vec<_> = anchors.into_iter().collect();
    anchors.sort_unstable_by_key(|(anchor, _)| *anchor);
    for (anchor, (count, partner)) in anchors {
        for _ in 1..count {
            for column in scan.columns {
                if let ValueProjection::Property { key, .. } = column {
                    control(GlaExecutionEvent::Work)?;
                    property(anchor, *key)?;
                }
            }
        }
        visit(
            scan.columns,
            &[Some(anchor), Some(partner)],
            &mut property,
            &mut control,
            Multiplicity(NonZeroU64::new(count)),
        )?;
    }
    Ok(())
}

/// A positioned graph and its position-keyed vertex test, for a visitor
/// that keeps per-vertex state by position. The host's readers are pure:
/// the same answer for the same input, with no effect.
pub(super) struct Positioned<'r, 'g, T, A> {
    pub(super) graph: &'r PositionedGraph<'g, T>,
    pub(super) test_at: A,
}

/// `visit_single_scan` over a positioned graph, event for event: the same
/// arrivals, tests at the same first arrivals, the same entries, visits and
/// multiplicities. A position stands for its identity in every comparison
/// (the vertex table ascends strictly), so verdicts and anchors live in
/// arrays by position instead of identity-keyed maps, and anchors are
/// visited in ascending position, which is ascending identity. The readers
/// are pure, so of an anchor's repeated property re-reads only the first
/// is made; each still charges its Work.
fn visit_positioned_single_scan<'a, E, C, T, A, R, M, P>(
    scan: &SingleScan<'_>,
    plan: &crate::algebra::GlaPlan<GraphValueRow>,
    positioned: Positioned<'_, '_, T, A>,
    mut property: R,
    mut control: M,
    mut visit: P,
) -> Result<(), VisitError<E, C>>
where
    A: FnMut(u32, &[VertexPredicate]) -> Result<bool, VisitError<E, C>>,
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
    let Positioned { graph, mut test_at } = positioned;
    let filters = endpoint_filters(plan.operators(), core::slice::from_ref(&scan.access));
    let (from, to) = filters
        .as_ref()
        .and_then(|filters| filters.get(&scan.access.relation))
        .map_or((&[][..], &[][..]), |(from, to)| {
            (from.as_slice(), to.as_slice())
        });
    let shared = from == to;
    // A relation the graph does not hold matches no edge; each still arrives.
    let relation = graph.relation(scan.access.relation);
    // Each side's verdicts by position: 0 untested, 1 fails, 2 passes. One
    // table when both sides share their predicates, none for a side without.
    let table = |predicates: &[VertexPredicate]| {
        if predicates.is_empty() {
            Vec::new()
        } else {
            vec![0_u8; graph.len()]
        }
    };
    let mut tested = [table(from), if shared { Vec::new() } else { table(to) }];
    let mut newest: [Option<(u32, bool)>; 2] = [None; 2];
    let constant = scan.columns.is_empty() || scan.counts_only;
    let anchored = scan.anchored && !constant;
    // Anchor -> passing edges and the first partner, by position.
    let mut counts = if anchored {
        vec![0_u64; graph.len()]
    } else {
        Vec::new()
    };
    let mut partners = if anchored {
        vec![0_u32; graph.len()]
    } else {
        Vec::new()
    };
    let mut run: Option<(u32, u64)> = None;
    let mut total: Option<(u64, u32, u32)> = None;
    for edge in graph.edges() {
        control(GlaExecutionEvent::Work)?;
        if Some(edge.relation) != relation {
            continue;
        }
        let mut passes = |side: usize, at: u32, predicates: &[VertexPredicate]| {
            if predicates.is_empty() {
                return Ok(true);
            }
            if let Some((known, verdict)) = newest[side]
                && known == at
            {
                return Ok(verdict);
            }
            let verdicts = &mut tested[if shared { 0 } else { side }];
            let verdict = match verdicts[at as usize] {
                0 => {
                    let result = test_at(at, predicates)?;
                    control(GlaExecutionEvent::ScratchEntry)?;
                    verdicts[at as usize] = 1 + u8::from(result);
                    result
                }
                known => known == 2,
            };
            newest[side] = Some((at, verdict));
            Ok::<_, VisitError<E, C>>(verdict)
        };
        if !passes(0, edge.source, from)? || !passes(1, edge.destination, to)? {
            continue;
        }
        let (anchor, partner) = match scan.access.direction {
            GlaDirection::Forward => (edge.source, edge.destination),
            _ => (edge.destination, edge.source),
        };
        if constant {
            match &mut total {
                Some((count, ..)) => *count += 1,
                None => total = Some((1, anchor, partner)),
            }
            continue;
        }
        if scan.anchored {
            match &mut run {
                Some((last, more)) if *last == anchor => *more += 1,
                _ => {
                    if let Some((last, more)) = run.take() {
                        counts[last as usize] += more;
                    }
                    if counts[anchor as usize] == 0 {
                        control(GlaExecutionEvent::ScratchEntry)?;
                        counts[anchor as usize] = 1;
                        partners[anchor as usize] = partner;
                    } else {
                        counts[anchor as usize] += 1;
                    }
                    run = Some((anchor, 0));
                }
            }
            continue;
        }
        visit(
            scan.columns,
            &[Some(graph.vertex(anchor)), Some(graph.vertex(partner))],
            &mut property,
            &mut control,
            Multiplicity::ONE,
        )?;
    }
    if let Some((count, anchor, partner)) = total {
        return visit(
            scan.columns,
            &[Some(graph.vertex(anchor)), Some(graph.vertex(partner))],
            &mut property,
            &mut control,
            Multiplicity(NonZeroU64::new(count)),
        );
    }
    if let Some((last, more)) = run {
        counts[last as usize] += more;
    }
    for (at, &count) in counts.iter().enumerate() {
        if count == 0 {
            continue;
        }
        let anchor = graph.vertex(at as u32);
        for further in 1..count {
            for column in scan.columns {
                if let ValueProjection::Property { key, .. } = column {
                    control(GlaExecutionEvent::Work)?;
                    if further == 1 {
                        property(anchor, *key)?;
                    }
                }
            }
        }
        visit(
            scan.columns,
            &[Some(anchor), Some(graph.vertex(partners[at]))],
            &mut property,
            &mut control,
            Multiplicity(NonZeroU64::new(count)),
        )?;
    }
    Ok(())
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

/// Whether a group key or an aggregate argument reads the source slot. A
/// projected column that nothing reads (a COUNT(*) row carrier) only rides
/// along each binding, so one multi-source pass stays exact for it.
fn source_is_read(aggregate: &PreparedGraphAggregate, columns: &[ValueProjection]) -> bool {
    aggregate
        .keys
        .iter()
        .copied()
        .chain(aggregate.aggregates.iter().filter_map(|a| a.column))
        .any(|index| {
            matches!(
                columns.get(index),
                Some(ValueProjection::Vertex { slot } | ValueProjection::Property { slot, .. })
                    if slot.ordinal() == 0
            )
        })
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
    for column in columns {
        let slot = match column {
            ValueProjection::Vertex { slot } | ValueProjection::Property { slot, .. } => *slot,
            _ => return None,
        };
        if slot.ordinal() > 1 {
            return None;
        }
    }
    let reads_source = source_is_read(aggregate, columns);
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

/// A reach's arcs over dense vertex indices numbered in identity order, so an
/// ascending index walk is an ascending identity walk. The walk kept ordered
/// sets of identities and the arcs in an ordered map, about three tree
/// operations per arc: half of a pokec three-hop reach over the ten million
/// edges of its closure (fgdb-61p1n).
struct ReachGraph {
    /// Index to identity, ascending.
    vertices: Vec<VId>,
    /// Identity to index; probed only, never iterated into a result.
    index: crate::id_hash::IdMap<VId, usize>,
    /// The distinct successors of vertex `i`, ascending, are
    /// `targets[starts[i]..starts[i + 1]]`.
    starts: Vec<usize>,
    targets: Vec<usize>,
}

impl ReachGraph {
    /// From arcs over indices that `index` numbered in arrival order.
    fn new(
        mut index: crate::id_hash::IdMap<VId, usize>,
        arrivals: Vec<VId>,
        arcs: Vec<(usize, usize)>,
    ) -> Self {
        let mut ordered: Vec<(VId, usize)> = arrivals.into_iter().zip(0..).collect();
        ordered.sort_unstable();
        let mut rank = vec![0; ordered.len()];
        for (at, &(_, arrival)) in ordered.iter().enumerate() {
            rank[arrival] = at;
        }
        for at in index.values_mut() {
            *at = rank[*at];
        }
        let vertices: Vec<VId> = ordered.into_iter().map(|(vertex, _)| vertex).collect();
        let mut starts = vec![0; vertices.len() + 1];
        for &(from, _) in &arcs {
            starts[rank[from] + 1] += 1;
        }
        for at in 1..starts.len() {
            starts[at] += starts[at - 1];
        }
        let mut fill = starts.clone();
        let mut targets = vec![0; arcs.len()];
        for (from, to) in arcs {
            let from = rank[from];
            targets[fill[from]] = rank[to];
            fill[from] += 1;
        }
        // Sort each vertex's run and keep one of each parallel arc, moving the
        // kept arcs down in place: every write lands at or below the read.
        let mut kept = 0;
        let mut begin = 0;
        for vertex in 0..vertices.len() {
            let end = starts[vertex + 1];
            targets[begin..end].sort_unstable();
            starts[vertex] = kept;
            let mut last = None;
            for read in begin..end {
                let target = targets[read];
                if last != Some(target) {
                    targets[kept] = target;
                    kept += 1;
                    last = Some(target);
                }
            }
            begin = end;
        }
        starts[vertices.len()] = kept;
        targets.truncate(kept);
        Self {
            vertices,
            index,
            starts,
            targets,
        }
    }

    fn successors(&self, vertex: usize) -> &[usize] {
        &self.targets[self.starts[vertex]..self.starts[vertex + 1]]
    }
}

const REACHED: u8 = 1;
const EXPANDED: u8 = 2;

/// One search's marks over a ReachGraph. A search clears only the vertices
/// the previous one touched, so a search per source costs what it reaches.
struct ReachWalk {
    marks: Vec<u8>,
    touched: Vec<usize>,
}

impl ReachWalk {
    fn new(vertices: usize) -> Self {
        Self {
            marks: vec![0; vertices],
            touched: Vec::new(),
        }
    }

    /// Sets `mark` on `vertex`; whether it was unset.
    fn mark(&mut self, vertex: usize, mark: u8) -> bool {
        let marks = self.marks[vertex];
        if marks & mark != 0 {
            return false;
        }
        if marks == 0 {
            self.touched.push(vertex);
        }
        self.marks[vertex] = marks | mark;
        true
    }

    fn clear(&mut self) {
        for vertex in self.touched.drain(..) {
            self.marks[vertex] = 0;
        }
    }
}

/// Vertices reachable from `sources` within the reach bounds (minimum <= 1),
/// ascending. Breadth-first over ascending successors: each crossed arc is
/// one unit of work and each newly reached vertex one scratch entry.
fn reachable<E, C>(
    reach: &Reach<'_>,
    graph: &ReachGraph,
    walk: &mut ReachWalk,
    sources: &[VId],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
) -> Result<Vec<VId>, VisitError<E, C>> {
    walk.clear();
    if reach.minimum > reach.maximum {
        return Ok(Vec::new());
    }
    // A source on no arc reaches nothing, and nothing reaches it.
    let mut isolated = Vec::new();
    if reach.minimum == 0 {
        for &source in sources {
            control(GlaExecutionEvent::ScratchEntry)?;
            match graph.index.get(&source) {
                Some(&at) => {
                    walk.mark(at, REACHED);
                }
                None => isolated.push(source),
            }
        }
    }
    let mut frontier = Vec::new();
    for source in sources {
        if let Some(&at) = graph.index.get(source) {
            walk.mark(at, EXPANDED);
            frontier.push(at);
        }
    }
    let mut depth = 0;
    while !frontier.is_empty() && depth < reach.maximum {
        depth += 1;
        let mut next = Vec::new();
        for vertex in frontier {
            for &neighbor in graph.successors(vertex) {
                control(GlaExecutionEvent::Work)?;
                if walk.mark(neighbor, REACHED) {
                    control(GlaExecutionEvent::ScratchEntry)?;
                }
                if walk.mark(neighbor, EXPANDED) {
                    next.push(neighbor);
                }
            }
        }
        frontier = next;
    }
    let mut found: Vec<usize> = walk
        .touched
        .iter()
        .copied()
        .filter(|&at| walk.marks[at] & REACHED != 0)
        .collect();
    found.sort_unstable();
    let mut reached: Vec<VId> = found.into_iter().map(|at| graph.vertices[at]).collect();
    if !isolated.is_empty() {
        reached.extend(isolated);
        reached.sort_unstable();
        reached.dedup();
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
    let mut admitted = Vec::new();
    for vertex in vertices {
        control(GlaExecutionEvent::Work)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        admitted.push(vertex);
    }
    admitted.sort_unstable();
    admitted.dedup();
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
    let mut index = crate::id_hash::IdMap::<VId, usize>::default();
    let mut arrivals = Vec::new();
    let mut dense = |vertex: VId| {
        *index.entry(vertex).or_insert_with(|| {
            arrivals.push(vertex);
            arrivals.len() - 1
        })
    };
    let mut arcs = Vec::new();
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
        arcs.push((dense(from), dense(to)));
    }
    let graph = ReachGraph::new(index, arrivals, arcs);
    let mut walk = ReachWalk::new(graph.vertices.len());
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
            for target in reachable(reach, &graph, &mut walk, &[source], &mut control)? {
                emit(source, target, &mut property, &mut control)?;
            }
        }
    } else if let Some(&witness) = sources.first() {
        // No column reads the source: any admitted source witnesses the pair.
        for target in reachable(reach, &graph, &mut walk, &sources, &mut control)? {
            emit(witness, target, &mut property, &mut control)?;
        }
    }
    Ok(())
}

/// A directed chain of at most three hops, all in one orientation, whose
/// aggregates read only the source and endpoint of each binding (fgdb-h3gf6):
/// a fixed chain of two or three single-typed atoms, or one quantified atom
/// with a lower bound of at least two (reach_shape owns lower bounds of at
/// most one) and an upper bound of at most three. COUNT(*) and COUNT read the
/// number of bindings per (source, endpoint), and then no property; COUNT
/// DISTINCT, MIN and MAX read only their support.
///
/// Walk counts per endpoint propagate level by level over relation
/// multiplicities, never enumerating paths. Under DIFFERENT EDGES (or TRAIL
/// search) a binding is a trail, and the trail count per endpoint is exact;
/// an endpoint is in the support when it is positive. For two hops the only walks
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
    /// A quantified atom scans vertices for its sources; a fixed chain's
    /// sources are the tails of its first atom's edges.
    vertex_rooted: bool,
    forward: bool,
    trail: bool,
    source_predicates: Vec<&'a [VertexPredicate]>,
    target_predicates: Vec<&'a [VertexPredicate]>,
    columns: &'a [ValueProjection],
    reads_source: bool,
}

/// Counts (weighted by the binding multiplicity) and support summaries.
fn counts_or_support(aggregate: &PreparedGraphAggregate) -> bool {
    aggregate.relational_input.is_none()
        && aggregate.computed_input.is_none()
        && aggregate.aggregates.iter().all(|a| {
            matches!(
                a.function,
                GraphAggregateFunction::CountRows
                    | GraphAggregateFunction::Count
                    | GraphAggregateFunction::CountDistinct
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
    if !counts_or_support(aggregate) {
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
    for column in columns {
        let slot = match column {
            ValueProjection::Vertex { slot } | ValueProjection::Property { slot, .. } => {
                slot.ordinal()
            }
            _ => return None,
        };
        if slot != 0 && slot != last {
            return None;
        }
    }
    // A count folds a pair's occurrences into one weighted binding, but
    // property reads keep the uncompressed fallible-read contract (one read
    // per occurrence), so a counting chain reads no property.
    let counts = aggregate.aggregates.iter().any(|a| {
        matches!(
            a.function,
            GraphAggregateFunction::CountRows | GraphAggregateFunction::Count
        )
    });
    if counts
        && columns
            .iter()
            .any(|column| matches!(column, ValueProjection::Property { .. }))
    {
        return None;
    }
    let reads_source = source_is_read(aggregate, columns);
    Some(Chain {
        relations,
        lengths,
        last: usize::try_from(last).ok()?,
        vertex_rooted: quantified,
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

/// Each endpoint's positive trail (or walk) count from `sources`, summed over
/// the chain's lengths.
fn chain_counts<E, C>(
    chain: &Chain<'_>,
    hops: &BTreeMap<RelationId, Hops>,
    sources: &[VId],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
) -> Result<BTreeMap<VId, u128>, VisitError<E, C>> {
    let empty = Hops::default();
    let hop = |at: usize| hops.get(&chain.relations[at]).unwrap_or(&empty);
    // Each level accumulates in an id-hash map, pair by pair in the order the
    // ordered levels produce them (so an overflow stops at the same pair), and
    // is then frozen as a vector sorted by vertex: later passes read it in
    // the same order a BTreeMap would, and look vertices up by binary search.
    let mut first: Vec<(VId, u128)> = sources.iter().map(|&a| (a, 1)).collect();
    first.sort_unstable_by_key(|&(vertex, _)| vertex);
    first.dedup_by_key(|&mut (vertex, _)| vertex);
    let mut levels: Vec<Vec<(VId, u128)>> = vec![first];
    for at in 0..chain.relations.len() {
        let mut next = crate::id_hash::IdMap::<VId, u128>::default();
        for &(vertex, walks) in &levels[at] {
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
        let mut next: Vec<(VId, u128)> = next.into_iter().collect();
        next.sort_unstable_by_key(|&(vertex, _)| vertex);
        levels.push(next);
    }
    let walks_into = |level: &[(VId, u128)], vertex: VId| {
        level
            .binary_search_by_key(&vertex, |&(vertex, _)| vertex)
            .map_or(0, |at| level[at].1)
    };
    let same = |i: usize, j: usize| chain.relations[i] == chain.relations[j];
    // Three-hop edge coincidences summed over the sources once, per endpoint:
    // N12 (the same loop twice at a source, then a third edge) and N13 (an
    // edge from a source, any edge back, the first edge again). N23 reads the
    // one-hop level, N123 the endpoint's own loops.
    let mut n12 = BTreeMap::<VId, u128>::new();
    let mut n13 = BTreeMap::<VId, u128>::new();
    let accumulate = |into: &mut BTreeMap<VId, u128>, end: VId, value: u128| {
        let slot = into.entry(end).or_insert(0);
        slot.checked_add(value)
            .map(|sum| *slot = sum)
            .ok_or_else(overflow)
    };
    if chain.trail && chain.lengths.contains(&3) {
        for &a in sources {
            let loops = hop(0).loops(a);
            if same(0, 1) && loops > 0 {
                for (&end, &third) in hop(2).out.get(&a).into_iter().flatten() {
                    control(GlaExecutionEvent::Work)?;
                    accumulate(
                        &mut n12,
                        end,
                        loops.checked_mul(third).ok_or_else(overflow)?,
                    )?;
                }
            }
            if same(0, 2) {
                for (&end, &first) in hop(0).out.get(&a).into_iter().flatten() {
                    control(GlaExecutionEvent::Work)?;
                    let back = hop(1).multiplicity(end, a);
                    if back > 0 {
                        accumulate(&mut n13, end, first.checked_mul(back).ok_or_else(overflow)?)?;
                    }
                }
            }
        }
    }
    let mut counts = BTreeMap::<VId, u128>::new();
    for &length in &chain.lengths {
        for &(end, walks) in &levels[length] {
            control(GlaExecutionEvent::Work)?;
            let source = sources.binary_search(&end).is_ok();
            let mut trails = walks;
            if chain.trail && length == 2 && same(0, 1) && source {
                trails = trails.saturating_sub(hop(0).loops(end));
            }
            if chain.trail && length == 3 {
                // trails = walks - N12 - N23 - N13 + 2 N123, all walk counts.
                let n23 = if same(1, 2) {
                    let into = walks_into(&levels[1], end);
                    hop(1).loops(end).checked_mul(into).ok_or_else(overflow)?
                } else {
                    0
                };
                let reused = [n12.get(&end), n13.get(&end)]
                    .into_iter()
                    .flatten()
                    .try_fold(n23, |sum, value| sum.checked_add(*value))
                    .ok_or_else(overflow)?;
                let triple = if same(0, 1) && same(1, 2) && source {
                    hop(0).loops(end).checked_mul(2).ok_or_else(overflow)?
                } else {
                    0
                };
                trails = trails
                    .checked_add(triple)
                    .ok_or_else(overflow)?
                    .saturating_sub(reused);
            }
            if trails > 0 {
                if !counts.contains_key(&end) {
                    control(GlaExecutionEvent::ScratchEntry)?;
                }
                let total = counts.entry(end).or_insert(0);
                *total = total.checked_add(trails).ok_or_else(overflow)?;
            }
        }
    }
    Ok(counts)
}

/// Feed the aggregate one binding per (source, endpoint) pair of a short chain,
/// weighted by that pair's binding count, like visit_reach_bindings: one
/// multi-source pass unless a column reads the source, then one pass per source.
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
    if chain.vertex_rooted {
        for vertex in vertices {
            control(GlaExecutionEvent::Work)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            admitted.insert(vertex);
        }
    }
    // Each relation's oriented edges, charged as they arrive, then counted
    // once from their sorted order into the same multiplicity maps (built in
    // bulk), not with two tree searches per edge.
    let mut arrivals: Vec<(RelationId, Vec<(VId, VId)>)> = Vec::new();
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
        let at = match arrivals.iter().position(|(known, _)| *known == relation) {
            Some(at) => at,
            None => {
                arrivals.push((relation, Vec::new()));
                arrivals.len() - 1
            }
        };
        arrivals[at].1.push((from, to));
    }
    let mut hops = BTreeMap::<RelationId, Hops>::new();
    for (relation, mut pairs) in arrivals {
        pairs.sort_unstable();
        let mut out = Vec::new();
        let mut loops = Vec::new();
        let mut at = 0;
        while at < pairs.len() {
            let from = pairs[at].0;
            let mut targets = Vec::new();
            while at < pairs.len() && pairs[at].0 == from {
                let to = pairs[at].1;
                let start = at;
                while at < pairs.len() && pairs[at] == (from, to) {
                    at += 1;
                }
                let count = (at - start) as u128;
                if from == to {
                    loops.push((from, count));
                }
                targets.push((to, count));
            }
            out.push((from, targets.into_iter().collect()));
        }
        hops.insert(
            relation,
            Hops {
                out: out.into_iter().collect(),
                loops: loops.into_iter().collect(),
            },
        );
    }
    // A fixed chain binds its source from its first atom's edges: an edge
    // scan admits no vertex table unless rows are read, so the admitted
    // vertices are not its source domain. A quantified atom scans vertices.
    let candidates: Vec<VId> = if chain.vertex_rooted {
        admitted.into_iter().collect()
    } else {
        hops.get(&chain.relations[0])
            .map(|hop| hop.out.keys().copied().collect())
            .unwrap_or_default()
    };
    let mut sources = Vec::new();
    for vertex in candidates {
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
    let mut emit = |source: VId,
                    target: VId,
                    bindings_count: u128,
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
        // One emitted binding carries all of this pair's bindings: None
        // records a positive count beyond u64 (a COUNT then overflows).
        let weight = Multiplicity(u64::try_from(bindings_count).ok().and_then(NonZeroU64::new));
        visit(chain.columns, &bindings, property, control, weight)
    };
    if chain.reads_source {
        for &source in &sources {
            for (target, count) in chain_counts(chain, &hops, &[source], &mut control)? {
                emit(source, target, count, &mut property, &mut control)?;
            }
        }
    } else if let Some(&witness) = sources.first() {
        for (target, count) in chain_counts(chain, &hops, &sources, &mut control)? {
            emit(witness, target, count, &mut property, &mut control)?;
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
/// `simple` says every topology key weighs one (no parallel edges, decided
/// once per evaluation). Then a binding's weight is one, or, under DIFFERENT
/// EDGES, nothing when it takes one key twice, so no key is searched: the
/// same Work events, without a topology search per atom per binding (a fifth
/// of a directed triangle count over epinions).
fn binding_weight<E, C>(
    accesses: &[EdgeAccess],
    topology: &Topology,
    simple: bool,
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
        if !distinct && !simple {
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
        if simple {
            // One occurrence: a key taken twice has none left for the second.
            if group > 1 {
                return Ok(None);
            }
            continue;
        }
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
/// Per relation, the vertex predicates of the source and destination slot of
/// every directed atom of that relation, when all of its atoms agree. A slot's
/// Select predicates then hold for every binding exactly when each edge of
/// its atoms satisfies them at the matching end, so they can filter the
/// relation's edges once, before the weighted topology exists, instead of
/// being re-tested on every binding. None when there is nothing to push or
/// atoms of one relation disagree (or are undirected): keep the Selects.
type EndpointFilters = BTreeMap<RelationId, (Vec<VertexPredicate>, Vec<VertexPredicate>)>;

fn endpoint_filters(operators: &[GlaOperator], accesses: &[EdgeAccess]) -> Option<EndpointFilters> {
    // Slots an equality identity joins (a cycle's closing slot and its start)
    // bind one vertex, so they share every predicate of their class.
    let width = accesses
        .iter()
        .map(|access| access.source.max(access.destination) + 1)
        .max()
        .unwrap_or(0);
    let mut class: Vec<usize> = (0..width).collect();
    fn root(class: &mut [usize], mut slot: usize) -> usize {
        while class[slot] != slot {
            class[slot] = class[class[slot]];
            slot = class[slot];
        }
        slot
    }
    for operator in operators {
        if let GlaOperator::VertexIdentity {
            left,
            right,
            equal: true,
        } = operator
        {
            let (left, right) = (
                usize::try_from(left.ordinal()).ok()?,
                usize::try_from(right.ordinal()).ok()?,
            );
            if left.max(right) >= width {
                return None;
            }
            let (left, right) = (root(&mut class, left), root(&mut class, right));
            class[left.max(right)] = left.min(right);
        }
    }
    let mut slots = BTreeMap::<usize, Vec<VertexPredicate>>::new();
    for operator in operators {
        if let GlaOperator::Select { slot, predicates } = operator {
            let slot = usize::try_from(slot.ordinal()).ok()?;
            if slot >= width {
                return None;
            }
            let predicates_of = slots.entry(root(&mut class, slot)).or_default();
            for predicate in predicates {
                if !predicates_of.contains(predicate) {
                    predicates_of.push(predicate.clone());
                }
            }
        }
    }
    if slots.is_empty() {
        return None;
    }
    let mut filters = EndpointFilters::new();
    for access in accesses {
        let (from, to) = match access.direction {
            GlaDirection::Forward => (access.source, access.destination),
            GlaDirection::Reverse => (access.destination, access.source),
            GlaDirection::Undirected => return None,
        };
        let (from, to) = (root(&mut class, from), root(&mut class, to));
        let pair = (
            slots.get(&from).cloned().unwrap_or_default(),
            slots.get(&to).cloned().unwrap_or_default(),
        );
        match filters.get(&access.relation) {
            Some(existing) if *existing != pair => return None,
            Some(_) => {}
            None => {
                filters.insert(access.relation, pair);
            }
        }
    }
    // Every predicated class must hold an endpoint of some atom.
    let mut covered = BTreeSet::new();
    for access in accesses {
        covered.insert(root(&mut class, access.source));
        covered.insert(root(&mut class, access.destination));
    }
    slots
        .keys()
        .all(|slot| covered.contains(slot))
        .then_some(filters)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn visit_bindings<'a, 'g, E, C, T, A, F, R, M, P>(
    aggregate: &PreparedGraphAggregate,
    vertices: impl IntoIterator<Item = VId>,
    edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
    positioned: Option<Positioned<'_, 'g, T, A>>,
    mut test_vertex: F,
    property: R,
    mut control: M,
    mut visit: P,
) -> Result<(), VisitError<E, C>>
where
    A: FnMut(u32, &[VertexPredicate]) -> Result<bool, VisitError<E, C>>,
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
    if let Some(scan) = vertex_scan(aggregate) {
        return visit_vertex_scan(&scan, vertices, test_vertex, property, control, visit);
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
    if let Some(scan) = single_scan(aggregate) {
        if let Some(positioned) = positioned {
            return visit_positioned_single_scan(&scan, plan, positioned, property, control, visit);
        }
        return visit_single_scan(&scan, plan, edges, test_vertex, property, control, visit);
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

    let filters = endpoint_filters(plan.operators(), &accesses);
    // A positioned graph weighs by position unless an undirected relation
    // needs the online normalization.
    let mut positioned = positioned.filter(|_| undirected.is_empty());
    let weighed = match &mut positioned {
        Some(Positioned { graph, test_at }) => {
            topology::weigh_positioned(graph, filters.as_ref(), test_at, &mut control)
        }
        None => None,
    };
    let topology = match weighed {
        Some(weighed) => weighed?,
        None => topology::weigh(
            edges,
            &undirected,
            filters.as_ref(),
            &mut test_vertex,
            &mut control,
        )?,
    };
    // A topology weighed by position is searched by position.
    let graph = positioned.as_ref().map(|positioned| positioned.graph);
    let vertex = |at: u32| graph.map_or(VId(0), |graph| graph.vertex(at));
    let positioned = graph.map(|graph| (graph.len(), &vertex as &dyn Fn(u32) -> VId));

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
    let prefiltered = filters.is_some();
    let stripped;
    let plan = if distinct || prefiltered {
        stripped = crate::algebra::GlaPlan::from_operators(
            plan.operators()
                .iter()
                .filter(|operator| match operator {
                    GlaOperator::DifferentEdges { .. } => !distinct,
                    // Their predicates already filtered every atom's edges.
                    GlaOperator::Select { .. } => !prefiltered,
                    _ => true,
                })
                .cloned()
                .collect(),
        );
        &stripped
    } else {
        plan
    };
    let needs_original_bindings = needs_original_bindings(plan);
    if needs_original_bindings || distinct {
        let simple = topology.values().all(|weight| *weight == Multiplicity::ONE);
        // Keep all slots and the complete multiplicity/read callback. Only a
        // topology prefix followed by late reads may use intersection; its
        // guarded fallback is the original visitor, with no extra events.
        return chain::visit_original_bindings(
            plan,
            vertices,
            &topology,
            positioned,
            test_vertex,
            property,
            control,
            |columns, bindings, property, control| {
                let Some(weight) =
                    binding_weight(&accesses, &topology, simple, bindings, distinct, control)?
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
        positioned,
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

    /// Every `text` over a sparse-identity multigraph of `width` vertices
    /// and `arcs` edges in two relations (parallel edges, loops, arrivals in
    /// no endpoint order) answers positioned with the identity evaluator's
    /// value and statistics, makes no vertex test the identity run does not
    /// (its pure readers may skip a repeat), stops at every `every`th host
    /// checkpoint (and the first and last 40) with the same count, and
    /// refuses at every sampled work and scratch limit alike.
    fn positioned_matches_identity(
        texts: &[&str],
        width: u32,
        arcs: usize,
        every: usize,
        single: bool,
    ) {
        use core::cell::Cell;
        let mut vertices: Vec<(VId, ())> = (0..u128::from(width))
            .map(|at| (VId(at * 7_919 + (at % 5) * (1 << 70)), ()))
            .collect();
        vertices.sort_unstable_by_key(|&(vid, _)| vid);
        let mut x = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = |bound: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % bound
        };
        let edges: Vec<PositionedEdge> = (0..arcs)
            .map(|_| PositionedEdge {
                source: next(u64::from(width)) as u32,
                destination: next(u64::from(width)) as u32,
                relation: next(2) as u32,
            })
            .collect();
        let relations = [RelationId(1), RelationId(2)];
        let graph = PositionedGraph::new(&vertices, &relations, &edges).unwrap();
        let triples: Vec<_> = graph.triples().collect();
        let values: Vec<CanonicalScalar> = (0..width)
            .map(|at| CanonicalScalar::Int(i64::from(at % 7)))
            .collect();
        let passes = |vid: VId| vid.0 % 3 != 1;
        let value = |vid: VId| {
            vertices
                .binary_search_by_key(&vid, |&(known, _)| known)
                .ok()
                .map(|at| &values[at])
        };
        let wide = GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX);
        for text in texts {
            let q = query(text);
            assert_eq!(single_scan(&q).is_some(), single, "{text}");
            let run = |positioned: bool, policy, stop: Option<usize>| {
                let (mut at, tests) = (0, Cell::new(0));
                let checkpoint = || {
                    at += 1;
                    if Some(at) == stop { Err(at) } else { Ok(()) }
                };
                let test = |vid: VId| {
                    tests.set(tests.get() + 1);
                    Ok::<_, ()>(passes(vid))
                };
                let result = if positioned {
                    q.execute_governed_positioned(
                        edges.len() as u64,
                        &graph,
                        |vid, _| test(vid),
                        |position, _| test(graph.vertex(position)),
                        |vid, _| Ok(value(vid)),
                        policy,
                        checkpoint,
                    )
                } else {
                    q.execute_governed(
                        edges.len() as u64,
                        graph.vertices(),
                        triples.iter().copied(),
                        |vid, _| test(vid),
                        |vid, _| Ok(value(vid)),
                        policy,
                        checkpoint,
                    )
                };
                (format!("{result:?}"), at, tests.get())
            };
            // The same outcome and host stops; pure readers let the
            // positioned run skip a test that only repeats one it made.
            let same = |positioned: (String, usize, usize), identity: (String, usize, usize)| {
                (positioned.0 == identity.0 && positioned.1 == identity.1)
                    .then_some(())
                    .filter(|()| positioned.2 <= identity.2)
                    .ok_or((positioned, identity))
            };
            let expected = run(false, wide, None);
            assert!(expected.0.starts_with("Ok("), "{text}: {expected:?}");
            same(run(true, wide, None), expected.clone())
                .unwrap_or_else(|pair| panic!("{text}: {pair:?}"));
            let total = expected.1;
            for stop in
                (1..=total).filter(|stop| stop % every == 0 || *stop <= 40 || total - stop < 40)
            {
                same(run(true, wide, Some(stop)), run(false, wide, Some(stop)))
                    .unwrap_or_else(|pair| panic!("{text} stop {stop}: {pair:?}"));
            }
            let completed = q
                .execute_governed(
                    edges.len() as u64,
                    graph.vertices(),
                    triples.iter().copied(),
                    |vid, _| Ok::<_, ()>(passes(vid)),
                    |vid, _| Ok(value(vid)),
                    wide,
                    || Ok::<_, usize>(()),
                )
                .unwrap();
            let step = every as u64;
            let sampled =
                move |total: u64| (0..=total).filter(move |at| at % step == 0 || total - at <= 3);
            for limit in sampled(completed.evaluator.work_units) {
                let policy = GqlQueryPolicy::new(u64::MAX, u64::MAX, limit, u64::MAX);
                same(run(true, policy, None), run(false, policy, None))
                    .unwrap_or_else(|pair| panic!("{text} work {limit}: {pair:?}"));
            }
            for limit in sampled(completed.evaluator.scratch_entries) {
                let policy = GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, limit);
                same(run(true, policy, None), run(false, policy, None))
                    .unwrap_or_else(|pair| panic!("{text} scratch {limit}: {pair:?}"));
            }
        }
    }

    /// **A POSITIONED SINGLE SCAN IS THE IDENTITY SCAN, EVENT FOR EVENT.**
    /// Shared, one-sided and absent endpoint predicates; forward and
    /// reverse; counted, anchored with and without a property column, and
    /// unanchored: every host checkpoint stop is compared.
    #[test]
    fn positioned_single_scans_answer_and_refuse_as_identity_scans() {
        positioned_matches_identity(
            &[
                "MATCH (a:L)-[:R]->(b:L) RETURN COUNT(*) AS n",
                "MATCH (a:L)-[:R]->(b) RETURN COUNT(*) AS n",
                "MATCH (a)-[:R]->(b:L) RETURN COUNT(*) AS n",
                "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS n",
                "MATCH (a:L)-[:R]->(b) RETURN a.n AS id, COUNT(*) AS d ORDER BY d DESC, id ASC LIMIT 3",
                "MATCH (a:L)<-[:R]-(b) RETURN a, COUNT(*) AS c GROUP BY a",
                "MATCH (a)-[:R]->(b:L) RETURN a, COUNT(*) AS c GROUP BY a",
                "MATCH (a:L)-[:R]->(b:L) RETURN COUNT(b) AS n, MIN(b) AS lo, MAX(a) AS hi",
            ],
            40,
            300,
            1,
            true,
        );
    }

    /// **A POSITIONED TOPOLOGY IS THE IDENTITY TOPOLOGY, EVENT FOR EVENT.**
    /// Triangles and four-cycles (distinct and repeatable edges, labelled,
    /// grouped, with a trailing property selection), a chain, a wedge with an
    /// identity test and an undirected cycle (weighed by identity) answer and
    /// refuse alike, whether their tries are keyed by position or identity.
    #[test]
    fn positioned_topologies_answer_and_refuse_as_identity_topologies() {
        positioned_matches_identity(
            &[
                "MATCH (a:L)-[:R]->(b:L)-[:R]->(c:L)-[:R]->(a) RETURN COUNT(*) AS n",
                "MATCH REPEATABLE ELEMENTS (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) RETURN COUNT(*) AS n",
                "MATCH REPEATABLE ELEMENTS (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) RETURN a, COUNT(*) AS n GROUP BY a",
                "MATCH REPEATABLE ELEMENTS (a:L)-[:R]->(b)-[:R]->(c)-[:R]->(d)-[:R]->(a) RETURN COUNT(*) AS n",
                "MATCH REPEATABLE ELEMENTS (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) WHERE a.n = c.n RETURN COUNT(*) AS n",
                "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN COUNT(*) AS n",
                "MATCH (a)-[:R]->(b)<-[:R]-(c) WHERE a<>c RETURN COUNT(*) AS n",
                "MATCH REPEATABLE ELEMENTS (a)-[:R]-(b)-[:R]-(c)-[:R]-(a) RETURN COUNT(*) AS n",
            ],
            24,
            120,
            9,
            false,
        );
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
        let parallel = distinct_edge_graph();
        // The same graph without parallel edges, where every weight is one
        // and binding_weight searches no key.
        let mut seen = BTreeSet::new();
        let simple: Vec<_> = parallel
            .iter()
            .copied()
            .filter(|&(_, source, relation, destination)| {
                seen.insert((source, relation, destination))
            })
            .collect();
        assert!(simple.len() < parallel.len());
        for edges in [&parallel, &simple] {
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
    }

    #[test]
    fn simple_topologies_weigh_repeatable_bindings_one_without_a_search() {
        let mut seen = BTreeSet::new();
        let simple: Vec<_> = distinct_edge_graph()
            .into_iter()
            .filter(|&(_, source, relation, destination)| {
                seen.insert((source, relation, destination))
            })
            .collect();
        let labelled: BTreeSet<VId> = [0, 1, 2, 3].map(VId).into();
        let properties = BTreeMap::from([
            (VId(0), CanonicalScalar::Int(7)),
            (VId(2), CanonicalScalar::Int(7)),
            (VId(3), CanonicalScalar::Int(7)),
        ]);
        let all: Vec<VId> = (0..5).map(VId).collect();
        for pattern in [
            "(a)-[:R]->(b)-[:R]->(c)-[:R]->(a) WHERE a.n = 7",
            "(a)-[:R]->(b)-[:R]->(c) WHERE a.n = 7",
            "(a:L)-[:R]->(b)<-[:R]-(c {n: 7})",
        ] {
            let text =
                format!("MATCH REPEATABLE ELEMENTS {pattern} RETURN COUNT(*) AS c,MIN(b) AS lo");
            let factored = query(&text);
            let enumerated = query(&format!("{text},COLLECT(b) AS bag"))
                .with_aggregate_output_prefix(2)
                .unwrap();
            assert!(eligible(&factored), "{text}");
            assert!(!eligible(&enumerated), "{text}");
            assert_eq!(
                run_counted(&factored, &all, &simple, &labelled, &properties).value,
                run_counted(&enumerated, &all, &simple, &labelled, &properties).value,
                "{text}"
            );
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

    /// One concrete runner, hence one monomorphization of the generic
    /// evaluator, shared by the support and count laws below: the lib test
    /// binary sits at the Apple linker's compact-unwind limit (fgdb-jon4t), and
    /// every distinct closure set instantiates the whole evaluator again.
    fn run_counted(
        aggregate: &PreparedGraphAggregate,
        admitted: &[VId],
        edges: &[(EId, VId, RelationId, VId)],
        labelled: &BTreeSet<VId>,
        properties: &BTreeMap<VId, CanonicalScalar>,
    ) -> crate::GqlQueryExecution<GraphAggregateRow> {
        aggregate
            .execute_governed_with_element_maps(
                edges.len() as u64,
                admitted.iter().copied(),
                edges.iter().copied(),
                |vid, predicates| {
                    Ok::<_, ()>(predicates.iter().all(|predicate| {
                        predicate.matches_borrowed(
                            labelled
                                .contains(&vid)
                                .then_some(fgdb_delta_types::LabelId(1)),
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
                GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX),
                || Ok::<_, ()>(()),
            )
            .unwrap()
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
                        let none = BTreeSet::new();
                        let all: Vec<VId> = (0..7).map(VId).collect();
                        let run = |aggregate: &PreparedGraphAggregate| {
                            run_counted(aggregate, &all, &edges, &none, &properties)
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

    #[derive(Debug, PartialEq)]
    enum ReachStep {
        Charge(GlaExecutionEvent),
        Test(VId),
        Bind(VId, VId),
    }

    /// The ordered walk the dense one replaced (fgdb-61p1n): an ordered map
    /// of sorted successor lists and ordered sets of reached and expanded
    /// identities. Its steps, in order, are the dense walk's oracle.
    fn ordered_reach(
        reach: &Reach<'_>,
        admitted: &[VId],
        edges: &[(VId, RelationId, VId)],
        selects: &dyn Fn(VId, &[VertexPredicate]) -> bool,
    ) -> Vec<ReachStep> {
        use GlaExecutionEvent::{ScratchEntry, Work};
        use ReachStep::{Bind, Charge, Test};
        let mut log = Vec::new();
        let mut distinct = BTreeSet::new();
        for &vertex in admitted {
            log.extend([Charge(Work), Charge(ScratchEntry)]);
            distinct.insert(vertex);
        }
        let mut sources = Vec::new();
        for &vertex in &distinct {
            let mut selected = true;
            for predicates in &reach.source_predicates {
                log.push(Test(vertex));
                if !selects(vertex, predicates) {
                    selected = false;
                    break;
                }
            }
            if selected {
                sources.push(vertex);
            }
        }
        let mut adjacency: BTreeMap<VId, Vec<VId>> = BTreeMap::new();
        for &(source, relation, destination) in edges {
            log.push(Charge(Work));
            if relation != reach.relation {
                continue;
            }
            let (from, to) = if reach.forward {
                (source, destination)
            } else {
                (destination, source)
            };
            log.push(Charge(ScratchEntry));
            adjacency.entry(from).or_default().push(to);
        }
        for successors in adjacency.values_mut() {
            successors.sort_unstable();
            successors.dedup();
        }
        let walk = |log: &mut Vec<ReachStep>, sources: &[VId]| {
            let mut reached = BTreeSet::new();
            if reach.minimum > reach.maximum {
                return reached;
            }
            if reach.minimum == 0 {
                for &source in sources {
                    log.push(Charge(ScratchEntry));
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
                    for &successor in adjacency.get(&vertex).map_or(&[][..], Vec::as_slice) {
                        log.push(Charge(Work));
                        if reached.insert(successor) {
                            log.push(Charge(ScratchEntry));
                        }
                        if expanded.insert(successor) {
                            next.push(successor);
                        }
                    }
                }
                frontier = next;
            }
            reached
        };
        let emit = |log: &mut Vec<ReachStep>, source: VId, target: VId| {
            for predicates in &reach.target_predicates {
                log.push(Test(target));
                if !selects(target, predicates) {
                    return;
                }
            }
            log.push(Bind(source, target));
        };
        if reach.reads_source {
            for &source in &sources {
                for target in walk(&mut log, &[source]) {
                    emit(&mut log, source, target);
                }
            }
        } else if let Some(&witness) = sources.first() {
            for target in walk(&mut log, &sources) {
                emit(&mut log, witness, target);
            }
        }
        log
    }

    /// The dense reach walk binds, tests and charges exactly what the ordered
    /// walk did, in the same order, over random multigraphs whose identities
    /// arrive out of order, with parallel arcs, self-loops, another relation,
    /// endpoints outside the admitted vertices, isolated and repeated admitted
    /// vertices, and both orientations, bounds from zero to unbounded, and
    /// grouped and ungrouped sources.
    #[test]
    fn dense_reach_steps_equal_the_ordered_walk_event_for_event() {
        let texts = [
            "MATCH (a)-[:R]->{1,3}(b) RETURN COUNT(DISTINCT b) AS n",
            "MATCH (a)<-[:R]-{0,2}(b) RETURN MIN(b.n) AS lo,MAX(b.n) AS hi",
            "MATCH REPEATABLE ELEMENTS (a)-[:R]->{0,4}(b) RETURN a,COUNT(DISTINCT b) AS n GROUP BY a",
            "MATCH (a)-[:R]->{1,}(b) RETURN COUNT(DISTINCT b) AS n",
            "MATCH (a)<-[:R]-{1,2}(b) WHERE a.n=1 RETURN a,MAX(b.n) AS hi GROUP BY a",
            "MATCH (a)-[:R]->{0,3}(b) WHERE b.n IS NOT NULL RETURN COUNT(DISTINCT b) AS n",
        ];
        let aggregates: Vec<_> = texts.iter().map(|text| query(text)).collect();
        let reaches: Vec<_> = texts
            .iter()
            .zip(&aggregates)
            .map(|(text, aggregate)| reach_shape(aggregate).unwrap_or_else(|| panic!("{text}")))
            .collect();
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut random = |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };
        let (mut checked, mut binds, mut tests) = (0, 0, 0);
        for _ in 0..300 {
            let count = 1 + random(14);
            // Distinct identities whose order is unrelated to their arrival.
            let ids: Vec<VId> = (0..count)
                .map(|at| VId((u128::from(random(1 << 20)) << 8) | u128::from(at)))
                .collect();
            let mut properties = BTreeMap::new();
            for &id in &ids {
                if let Some(value) = [Some(0), Some(1), Some(2), None][random(4) as usize] {
                    properties.insert(id, CanonicalScalar::Int(value));
                } else if random(2) == 0 {
                    properties.insert(id, CanonicalScalar::Null);
                }
            }
            let pick = |random: &mut dyn FnMut(u64) -> u64| ids[random(count) as usize];
            let edges: Vec<(VId, RelationId, VId)> = (0..random(3 * count))
                .map(|_| {
                    let relation = RelationId(if random(4) == 0 { 2 } else { 1 });
                    (pick(&mut random), relation, pick(&mut random))
                })
                .collect();
            let admitted: Vec<VId> = (0..random(count + 3)).map(|_| pick(&mut random)).collect();
            let selects = |vertex: VId, predicates: &[VertexPredicate]| {
                predicates.iter().all(|predicate| {
                    predicate.matches_borrowed(
                        None,
                        properties
                            .get(&vertex)
                            .map(|value| (PropertyKeyId(1), value)),
                    )
                })
            };
            for (text, reach) in texts.iter().zip(&reaches) {
                let log = std::cell::RefCell::new(Vec::new());
                let dense = visit_reach_bindings::<(), (), _, _, _, _>(
                    reach,
                    admitted.iter().copied(),
                    edges.iter().copied(),
                    |vertex, predicates| {
                        log.borrow_mut().push(ReachStep::Test(vertex));
                        Ok(selects(vertex, predicates))
                    },
                    |_, _| Ok(None),
                    |event| {
                        log.borrow_mut().push(ReachStep::Charge(event));
                        Ok(())
                    },
                    |_, bindings, _, _, _| {
                        let (source, target) = (bindings[0].unwrap(), bindings[1].unwrap());
                        log.borrow_mut().push(ReachStep::Bind(source, target));
                        Ok(())
                    },
                );
                assert!(dense.is_ok(), "{text}");
                let dense = log.into_inner();
                let ordered = ordered_reach(reach, &admitted, &edges, &selects);
                assert_eq!(dense, ordered, "{text} over {edges:?} from {admitted:?}");
                binds += dense
                    .iter()
                    .filter(|step| matches!(step, ReachStep::Bind(..)))
                    .count();
                tests += dense
                    .iter()
                    .filter(|step| matches!(step, ReachStep::Test(..)))
                    .count();
                checked += 1;
            }
        }
        assert_eq!(checked, 300 * texts.len());
        assert!(
            binds > 1_000 && tests > 1_000,
            "{binds} binds, {tests} tests"
        );
    }

    #[test]
    fn chain_counts_and_support_equal_trail_and_walk_enumeration() {
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
                        (
                            format!("COUNT(*) AS c,COUNT({end}) AS m,COUNT(DISTINCT {end}) AS n"),
                            false,
                        ),
                        (
                            format!("a,COUNT(*) AS c,COUNT({end}) AS m,COUNT(DISTINCT {end}) AS n"),
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
                        // An edge scan may admit no vertex table at all (no rows
                        // read), so every shape also runs without one.
                        let none = BTreeSet::new();
                        let run = |aggregate: &PreparedGraphAggregate, admitted: &[VId]| {
                            run_counted(aggregate, admitted, &edges, &none, &properties)
                        };
                        let all: Vec<VId> = (0..7).map(VId).collect();
                        for admitted in [&all[..], &[]] {
                            let actual = run(&factored, admitted);
                            let expected = run(&enumerated, admitted);
                            assert_eq!(
                                actual.value,
                                expected.value,
                                "{prefix}{group} with {} admitted vertices",
                                admitted.len()
                            );
                            assert_eq!(actual.rows, expected.rows, "{prefix}{group}");
                        }
                        let actual = run(&factored, &all);
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
        assert_eq!(checked, 264);
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
            // A sum reads values, not just bindings; a count reading a
            // property keeps one read per occurrence.
            "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN SUM(c.n) AS n",
            "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN COUNT(c.n) AS n",
            "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN COUNT(*) AS n,MIN(c.n) AS lo",
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

    /// The direct vertex scan is the GLA's vertex scan event for event: the
    /// same Work/ScratchEntry sequence, the same vertex tests in the same
    /// order and the same visited bindings, for unfiltered, labelled,
    /// property-filtered and grouped shapes; and it stops at every event.
    #[test]
    fn vertex_scans_are_the_gla_scan_event_for_event() {
        #[derive(Debug, PartialEq, Eq)]
        enum Step {
            Event(GlaExecutionEvent),
            Test(VId),
            Visit(Vec<Option<VId>>),
        }
        let labelled = |vid: VId| !vid.0.is_multiple_of(3);
        let value = CanonicalScalar::Int(7);
        let other = CanonicalScalar::Int(1);
        let property_of = |vid: VId| {
            if vid.0.is_multiple_of(2) {
                &value
            } else {
                &other
            }
        };
        let vertices: Vec<VId> = (0..12).map(VId).collect();
        for text in [
            "MATCH (a) RETURN COUNT(*) AS n",
            "MATCH (a:L) RETURN COUNT(*) AS n",
            "MATCH (a:L {n: 7}) RETURN a,COUNT(*) AS n GROUP BY a",
            "MATCH (a:L) WHERE a.n = 7 RETURN a.n,COUNT(*) AS n GROUP BY a.n",
        ] {
            let aggregate = query(text);
            let scan = vertex_scan(&aggregate).unwrap_or_else(|| panic!("{text}"));
            let trace = |direct: bool, stop: Option<usize>| {
                let steps = core::cell::RefCell::new(Vec::new());
                let mut events = 0;
                let record = |step: Step| steps.borrow_mut().push(step);
                let test = |vid: VId, predicates: &[VertexPredicate]| {
                    record(Step::Test(vid));
                    Ok::<_, VisitError<(), usize>>(predicates.iter().all(|predicate| {
                        predicate.matches_borrowed(
                            labelled(vid).then_some(fgdb_delta_types::LabelId(1)),
                            Some((PropertyKeyId(1), property_of(vid))),
                        )
                    }))
                };
                let property = |vid: VId, _: PropertyKeyId| Ok(Some(property_of(vid)));
                let control = |event: GlaExecutionEvent| {
                    events += 1;
                    record(Step::Event(event));
                    if Some(events) == stop {
                        Err(GqlQueryError::Interrupted(events))
                    } else {
                        Ok(())
                    }
                };
                let result = if direct {
                    visit_vertex_scan(
                        &scan,
                        vertices.iter().copied(),
                        test,
                        property,
                        control,
                        |_, bindings, _, _, _| {
                            record(Step::Visit(bindings.to_vec()));
                            Ok(())
                        },
                    )
                } else {
                    aggregate.input.plan().visit_value_bindings(
                        vertices.iter().copied(),
                        [],
                        test,
                        property,
                        control,
                        |_, bindings, _, _| {
                            record(Step::Visit(bindings.to_vec()));
                            Ok(())
                        },
                    )
                };
                (result.map_err(|_| ()), steps.into_inner())
            };
            let (direct, direct_steps) = trace(true, None);
            let (gla, gla_steps) = trace(false, None);
            assert!(direct.is_ok() && gla.is_ok(), "{text}");
            assert_eq!(direct_steps, gla_steps, "{text}");
            let events = direct_steps
                .iter()
                .filter(|step| matches!(step, Step::Event(_)))
                .count();
            for stop in 1..=events {
                let (result, steps) = trace(true, Some(stop));
                assert!(result.is_err(), "{text}: {stop}");
                let seen = steps
                    .iter()
                    .filter(|step| matches!(step, Step::Event(_)))
                    .count();
                assert_eq!(seen, stop, "{text}");
            }
        }
        for text in [
            "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS n",
            "MATCH (a), (b) RETURN COUNT(*) AS n",
            "MATCH (a) WHERE a.n = a.n RETURN COUNT(*) AS n",
        ] {
            assert!(vertex_scan(&query(text)).is_none(), "{text}");
        }
    }

    #[test]
    fn single_edge_scans_visit_arrivals_with_enumeration_answers() {
        let mut edges = distinct_edge_graph();
        edges.push((EId(400), VId(4), RelationId(1), VId(2)));
        edges.push((EId(401), VId(5), RelationId(1), VId(0)));
        // Into the unlabelled 4 and 5, and into 1, which lacks n = 7.
        edges.push((EId(404), VId(0), RelationId(1), VId(4)));
        edges.push((EId(405), VId(2), RelationId(1), VId(5)));
        edges.push((EId(406), VId(3), RelationId(1), VId(1)));
        // Another relation: scanned by none of the patterns below.
        edges.push((EId(402), VId(1), RelationId(2), VId(5)));
        edges.push((EId(403), VId(0), RelationId(2), VId(1)));
        let labelled: BTreeSet<VId> = [0, 1, 2, 3].map(VId).into();
        let properties = BTreeMap::from([
            (VId(0), CanonicalScalar::Int(7)),
            (VId(2), CanonicalScalar::Int(7)),
            (VId(3), CanonicalScalar::Int(7)),
            (VId(4), CanonicalScalar::Int(7)),
        ]);
        let all: Vec<VId> = (0..6).map(VId).collect();
        let run = |aggregate: &PreparedGraphAggregate| {
            run_counted(aggregate, &all, &edges, &labelled, &properties)
        };
        for pattern in [
            "(a:L)-[:R]->(b:L)",
            "(a)-[:R]->(b)",
            "(a:L)<-[:R]-(b)",
            "(a {n: 7})-[:R]->(b:L)",
            "(a)<-[:R]-(b {n: 7})",
            "(a)-[:R]->(b {n: 7})",
            "(a:L)<-[:R]-(b:L)",
        ] {
            for (returns, group, anchored) in [
                ("COUNT(*) AS c", "", true),
                (
                    "COUNT(*) AS c,COUNT(DISTINCT b) AS d,COUNT(a) AS e,MIN(b) AS lo,MAX(a) AS hi",
                    "",
                    false,
                ),
                ("a,COUNT(*) AS c", " GROUP BY a", true),
                ("a.n,COUNT(*) AS c,MAX(a) AS hi", " GROUP BY a.n", true),
                (
                    "COUNT(a.n) AS k,COUNT(DISTINCT a.n) AS d,MIN(a.n) AS lo,COUNT(*) AS c",
                    "",
                    true,
                ),
            ] {
                let text = format!("MATCH {pattern} RETURN {returns}");
                let scanned = query(&format!("{text}{group}"));
                // The prefix counts aggregate columns; a grouped query's
                // first column is its key.
                let aggregates = returns.split(',').count() - usize::from(!group.is_empty());
                let enumerated = query(&format!("{text},COLLECT(b) AS bag{group}"))
                    .with_aggregate_output_prefix(aggregates)
                    .unwrap();
                let scan = single_scan(&scanned);
                assert_eq!(
                    scan.map(|scan| scan.anchored),
                    Some(anchored),
                    "{text}{group}"
                );
                assert!(!eligible(&enumerated), "{text}{group}");
                assert_eq!(run(&scanned).value, run(&enumerated).value, "{text}{group}");
            }
        }
        for text in [
            "MATCH (a)-[:R]->(b) RETURN b,COUNT(*) AS c GROUP BY b",
            "MATCH (a)-[:R]->(b) RETURN a,COUNT(b) AS c GROUP BY a",
            "MATCH (a)-[:R]-(b) RETURN COUNT(*) AS c",
            "MATCH (a)-[:R]->(b) RETURN MIN(b.n) AS lo",
            "MATCH (a)-[:R]->(a) RETURN COUNT(*) AS c",
            "MATCH (a)-[:R]->(b) WHERE a.n = b.n RETURN COUNT(*) AS c",
            "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN COUNT(*) AS c",
        ] {
            assert!(single_scan(&query(text)).is_none(), "{text}");
        }
    }

    #[test]
    fn anchored_scans_read_every_property_occurrence_and_keep_each_failure() {
        let text = "MATCH (a:L)-[:R]->(b) RETURN a.n,COUNT(*) AS c";
        let scanned = query(&format!("{text} GROUP BY a.n"));
        let enumerated = query(&format!("{text},COLLECT(b) AS bag GROUP BY a.n"))
            .with_aggregate_output_prefix(1)
            .unwrap();
        assert!(single_scan(&scanned).is_some_and(|scan| scan.anchored));
        let edges: Vec<_> = distinct_edge_graph()
            .into_iter()
            .map(|(_, source, relation, destination)| (source, relation, destination))
            .collect();
        let value = CanonicalScalar::Int(7);
        let policy = GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX);
        let execute = |aggregate: &PreparedGraphAggregate, fail_at: Option<usize>| {
            let mut reads = 0;
            let result = aggregate.execute_governed(
                edges.len() as u64,
                (0..5).map(VId),
                edges.iter().copied(),
                |vid, _| Ok::<_, usize>(vid != VId(4)),
                |_, _| {
                    reads += 1;
                    if Some(reads) == fail_at {
                        Err(reads)
                    } else {
                        Ok(Some(&value))
                    }
                },
                policy,
                || Ok::<_, ()>(()),
            );
            (result.map(|result| result.value), reads)
        };
        let (scanned_rows, scanned_reads) = execute(&scanned, None);
        let (enumerated_rows, enumerated_reads) = execute(&enumerated, None);
        assert_eq!(scanned_rows.unwrap(), enumerated_rows.unwrap());
        // One read per passing edge, as the ordinary evaluator makes.
        assert_eq!(scanned_reads, enumerated_reads);
        for fail_at in 1..=scanned_reads {
            let (result, reads) = execute(&scanned, Some(fail_at));
            assert!(
                matches!(
                    result,
                    Err(GqlQueryError::Source(GraphAggregateError::Source(n))) if n == fail_at
                ),
                "{fail_at}"
            );
            assert_eq!(reads, fail_at);
        }
    }

    #[test]
    fn one_predicate_list_on_both_sides_tests_each_vertex_once() {
        let edges: Vec<_> = distinct_edge_graph()
            .into_iter()
            .map(|(_, source, relation, destination)| (source, relation, destination))
            .collect();
        let policy = GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX);
        for (text, shared) in [
            ("MATCH (a:L)-[:R]->(b:L) RETURN COUNT(*) AS c", true),
            ("MATCH (a:L)<-[:R]-(b:L) RETURN COUNT(*) AS c", true),
            ("MATCH (a:L)-[:R]->(b {n: 7}) RETURN COUNT(*) AS c", false),
        ] {
            let aggregate = query(text);
            assert!(single_scan(&aggregate).is_some(), "{text}");
            let mut tested = Vec::new();
            let value = CanonicalScalar::Int(7);
            aggregate
                .execute_governed(
                    edges.len() as u64,
                    (0..5).map(VId),
                    edges.iter().copied(),
                    |vid, predicates| {
                        tested.push(vid);
                        Ok::<_, ()>(predicates.iter().all(|predicate| {
                            predicate.matches_borrowed(
                                (vid != VId(4)).then_some(fgdb_delta_types::LabelId(1)),
                                (vid != VId(1)).then_some((PropertyKeyId(1), &value)),
                            )
                        }))
                    },
                    |_, _| Ok(None),
                    policy,
                    || Ok::<_, ()>(()),
                )
                .unwrap();
            let distinct: BTreeSet<_> = tested.iter().copied().collect();
            assert_eq!(distinct.len() == tested.len(), shared, "{text}: {tested:?}");
        }
    }

    #[test]
    fn single_edge_scans_stop_at_every_checkpoint() {
        // Unanchored, counted without anchors, and anchored in runs.
        for (text, group, aggregates) in [
            (
                "MATCH (a:L)-[:R]->(b:L) RETURN COUNT(*) AS c,MAX(b) AS hi",
                "",
                2,
            ),
            ("MATCH (a:L)-[:R]->(b:L) RETURN COUNT(*) AS c", "", 1),
            (
                "MATCH (a:L)<-[:R]-(b) RETURN a,COUNT(*) AS c",
                " GROUP BY a",
                1,
            ),
        ] {
            single_edge_scan_stops_at_every_checkpoint(text, group, aggregates);
        }
    }

    fn single_edge_scan_stops_at_every_checkpoint(text: &str, group: &str, aggregates: usize) {
        let aggregate = query(&format!("{text}{group}"));
        let enumerated = query(&format!("{text},COLLECT(b) AS bag{group}"))
            .with_aggregate_output_prefix(aggregates)
            .unwrap();
        assert!(single_scan(&aggregate).is_some());
        let edges: Vec<_> = distinct_edge_graph()
            .into_iter()
            .map(|(_, source, relation, destination)| (source, relation, destination))
            .collect();
        let policy = GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX);
        let execute = |aggregate: &PreparedGraphAggregate, stop: Option<usize>| {
            let mut at = 0;
            let result = aggregate.execute_governed(
                edges.len() as u64,
                (0..5).map(VId),
                edges.iter().copied(),
                |vid, _| Ok::<_, ()>(vid != VId(4)),
                |_, _| Ok(None),
                policy,
                || {
                    at += 1;
                    if Some(at) == stop { Err(at) } else { Ok(()) }
                },
            );
            (result, at)
        };
        let (completed, events) = execute(&aggregate, None);
        let (expected, _) = execute(&enumerated, None);
        assert_eq!(completed.unwrap().value, expected.unwrap().value);
        for stop in 1..=events {
            let (result, at) = execute(&aggregate, Some(stop));
            assert!(matches!(result, Err(GqlQueryError::Interrupted(n)) if n == stop));
            assert_eq!(at, stop);
        }
    }

    #[test]
    fn endpoint_predicates_filter_edges_once_with_enumeration_answers() {
        let mut edges = distinct_edge_graph();
        edges.push((EId(400), VId(4), RelationId(1), VId(2)));
        edges.push((EId(401), VId(5), RelationId(1), VId(0)));
        edges.push((EId(402), VId(1), RelationId(1), VId(5)));
        // Vertices 4 and 5 lack the label; 1 and 5 lack n = 7.
        let labelled: BTreeSet<VId> = [0, 1, 2, 3].map(VId).into();
        let properties = BTreeMap::from([
            (VId(0), CanonicalScalar::Int(7)),
            (VId(2), CanonicalScalar::Int(7)),
            (VId(3), CanonicalScalar::Int(7)),
            (VId(4), CanonicalScalar::Int(7)),
        ]);
        let accesses = |aggregate: &PreparedGraphAggregate| {
            let mut accesses = Vec::new();
            let mut next = 2;
            for operator in aggregate.input.plan().operators() {
                match operator {
                    GlaOperator::ScanEdges {
                        relation,
                        direction,
                    } => accesses.push(EdgeAccess {
                        source: 0,
                        destination: 1,
                        relation: relation.one().unwrap(),
                        direction: *direction,
                    }),
                    GlaOperator::Expand {
                        source,
                        relation,
                        direction,
                    } => {
                        accesses.push(EdgeAccess {
                            source: source.ordinal() as usize,
                            destination: next,
                            relation: relation.one().unwrap(),
                            direction: *direction,
                        });
                        next += 1;
                    }
                    _ => {}
                }
            }
            accesses
        };
        let mut filtered = 0;
        for mode in ["", "REPEATABLE ELEMENTS "] {
            for (pattern, uniform) in [
                ("(a:L)-[:R]->(b:L)-[:R]->(c:L)-[:R]->(a)", true),
                ("(a:L)-[:R]->(b:L)-[:R]->(c:L)", true),
                ("(a:L)<-[:R]-(b:L)<-[:R]-(c:L)", true),
                ("(a {n: 7})-[:R]->(b {n: 7})-[:R]->(a)", true),
                ("(a:L)-[:R]->(b:L)", true),
                ("(a:L)-[:R]->(b)-[:R]->(c:L)", false),
                ("(a:L)-[:R]-(b:L)-[:R]-(c:L)", false),
            ] {
                for (returns, group) in [
                    ("COUNT(*) AS c,COUNT(DISTINCT b) AS d,MIN(b.n) AS lo", ""),
                    (
                        "a,COUNT(*) AS c,COUNT(DISTINCT b) AS d,MIN(b.n) AS lo",
                        " GROUP BY a",
                    ),
                ] {
                    let text = format!("MATCH {mode}{pattern} RETURN {returns}");
                    let factored = query(&format!("{text}{group}"));
                    let enumerated = query(&format!("{text},COLLECT(b.n) AS bag{group}"))
                        .with_aggregate_output_prefix(3)
                        .unwrap();
                    assert!(eligible(&factored), "{text}{group}");
                    assert!(!eligible(&enumerated), "{text}{group}");
                    let applies =
                        endpoint_filters(factored.input.plan().operators(), &accesses(&factored))
                            .is_some();
                    assert_eq!(applies, uniform, "{text}{group}");
                    filtered += usize::from(applies);
                    let all: Vec<VId> = (0..6).map(VId).collect();
                    let run = |aggregate: &PreparedGraphAggregate| {
                        run_counted(aggregate, &all, &edges, &labelled, &properties)
                    };
                    assert_eq!(
                        run(&factored).value,
                        run(&enumerated).value,
                        "{text}{group}"
                    );
                }
            }
        }
        assert_eq!(filtered, 20);
    }
}
