//! Exact two-boundary path factors for hidden, degree-two binding positions.
//!
//! Each contracted route is a sum of products over its hidden assignments.
//! Its two endpoints remain correlated: it is never a product of marginals.
//! The ordinary typed pattern compiler and GLA visitor execute the reduced
//! pattern, and the ordinary aggregate accumulator consumes its exact weights.
//!
//! Only weighted.rs's positive topology-only profile enters here. Original
//! root bindings, projected bindings, identity operands and forest attachment
//! points are protected. A hidden position with multiple children is retained.
//! No property read, predicate or nullable scope is removed or reordered.
//!
//! Derived relation numbers are private route ordinals in an entirely separate
//! input table. ALL retained edges, including single-edge routes, are remapped;
//! no ordinal is mixed with a catalog RelationId or stored in the database.
//! Tables are metered, in-memory preprocessing, not spill or a byte-memory cap.

use super::*;
use crate::algebra::{GraphColumn, GraphPatternBuilder, GlaPlan, MAX_PATTERN_IDENTITIES, MAX_PATTERN_VERTICES};

type Factor = BTreeMap<(VId, VId), Multiplicity>;

struct Contraction {
    pattern: PreparedGraphPattern<GraphValueRow>,
    slots: [Option<usize>; MAX_PATTERN_VERTICES],
    width: usize,
    routes: Vec<Vec<usize>>,
}

fn unavailable<E, C>() -> VisitError<E, C> {
    GqlQueryError::Source(GraphAggregateError::MultiplicityUnavailable)
}

/// Use original binding positions, not VIds: distinct variables may have equal
/// values and still make independent edge choices. All route interiors are
/// disjoint and unobserved; one-child removal cannot separate the kept graph.
fn prepare<E, C>(
    plan: &GlaPlan<GraphValueRow>,
    accesses: &[EdgeAccess],
    forest: &tree::Forest,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
) -> Result<Option<Contraction>, VisitError<E, C>> {
    let width = accesses.len() + 1;
    if !(2..=MAX_PATTERN_VERTICES).contains(&width) {
        return Err(unavailable());
    }
    let mut parents = [0_usize; MAX_PATTERN_VERTICES];
    let mut children = [0_usize; MAX_PATTERN_VERTICES];
    let mut kept = [false; MAX_PATTERN_VERTICES];
    kept[0] = true;
    kept[1] = true;
    for (at, edge) in accesses.iter().enumerate() {
        control(GlaExecutionEvent::Work)?;
        if edge.destination != at + 1 || edge.source >= edge.destination {
            return Err(unavailable());
        }
        parents[edge.destination] = edge.source;
        children[edge.source] += 1;
    }
    let mut projection = None;
    let mut identities = 0_usize;
    for operator in plan.operators() {
        control(GlaExecutionEvent::Work)?;
        match operator {
            GlaOperator::VertexIdentity { left, right, .. } => {
                for slot in [left, right] {
                    let at = slot.ordinal() as usize;
                    if at >= width { return Err(unavailable()); }
                    kept[at] = true;
                }
                identities += 1;
            }
            GlaOperator::ProjectValues { columns } => {
                for column in columns {
                    control(GlaExecutionEvent::Work)?;
                    let ValueProjection::Vertex { slot } = column else { return Ok(None); };
                    let at = slot.ordinal() as usize;
                    if at >= width { return Err(unavailable()); }
                    kept[at] = true;
                }
                projection = Some(columns.as_slice());
            }
            GlaOperator::ScanEdges { .. } | GlaOperator::Expand { .. }
            | GlaOperator::OrderByValues | GlaOperator::Limit { offset: 0, count: None } => {}
            _ => return Ok(None),
        }
    }
    for slot in forest.anchors() {
        control(GlaExecutionEvent::Work)?;
        if slot >= width { return Err(unavailable()); }
        kept[slot] = true;
    }
    for (keep, children) in kept[..width].iter_mut().zip(&children[..width]) {
        control(GlaExecutionEvent::Work)?;
        *keep |= *children != 1;
    }
    // Lowering can contain implicit repeated-variable equalities in addition
    // to the user's maximum number of explicit constraints. Rebuilding through
    // the public typed compiler must not turn that valid plan into a refusal.
    if identities > MAX_PATTERN_IDENTITIES || kept[..width].iter().all(|keep| *keep) {
        return Ok(None);
    }
    let columns = projection.ok_or_else(unavailable)?;
    let mut slots = [None; MAX_PATTERN_VERTICES];
    let mut names: [String; MAX_PATTERN_VERTICES] = std::array::from_fn(|_| String::new());
    let mut builder = GraphPatternBuilder::new();
    let mut retained = 0_usize;
    for old in 0..width {
        if !kept[old] { continue; }
        // One generated bounded name and the compiler-owned variable entry.
        control(GlaExecutionEvent::ScratchEntry)?;
        names[old] = format!("s{old}");
        control(GlaExecutionEvent::ScratchEntry)?;
        builder.vertex(&names[old]).map_err(|_| unavailable())?;
        slots[old] = Some(retained);
        retained += 1;
    }
    let mut routes = Vec::new();
    for destination in 1..width {
        if !kept[destination] { continue; }
        control(GlaExecutionEvent::ScratchEntry)?;
        let mut route = Vec::new();
        let mut source = destination;
        loop {
            control(GlaExecutionEvent::ScratchEntry)?;
            route.push(source - 1);
            source = parents[source];
            if kept[source] { break; }
        }
        for at in 0..route.len() / 2 {
            control(GlaExecutionEvent::Work)?;
            let other = route.len() - 1 - at;
            route.swap(at, other);
        }
        let relation = RelationId(routes.len() as u64);
        control(GlaExecutionEvent::ScratchEntry)?;
        builder.edge(&names[source], relation, GlaDirection::Forward, &names[destination])
            .map_err(|_| unavailable())?;
        routes.push(route);
    }
    for operator in plan.operators() {
        if let GlaOperator::VertexIdentity { left, right, equal } = operator {
            control(GlaExecutionEvent::ScratchEntry)?;
            builder.identity(&names[left.ordinal() as usize], &names[right.ordinal() as usize], *equal)
                .map_err(|_| unavailable())?;
        }
    }
    let mut aliases = Vec::new();
    for at in 0..columns.len() {
        control(GlaExecutionEvent::ScratchEntry)?;
        aliases.push(format!("c{at}"));
    }
    let mut projected = Vec::new();
    for (at, column) in columns.iter().enumerate() {
        let ValueProjection::Vertex { slot } = column else { return Err(unavailable()); };
        control(GlaExecutionEvent::ScratchEntry)?;
        projected.push(GraphColumn::vertex(&aliases[at], &names[slot.ordinal() as usize]));
    }
    // Reserve the bounded typed compiler's temporary slot/flag tables, emitted
    // operators and owned output schema before preparation. This conservative
    // logical-entry reservation is deliberately not allocator-byte accounting.
    for _ in 0..(3 * retained + 3 * routes.len() + 2 * identities + 4 * columns.len() + 4) {
        control(GlaExecutionEvent::ScratchEntry)?;
    }
    let pattern = builder.prepare_values(&projected, 0, None)
        .map_err(|_| unavailable())?.with_duplicates();
    Ok(Some(Contraction { pattern, slots, width, routes }))
}

fn add<E>(
    factor: &mut Factor,
    source: VId,
    destination: VId,
    weight: Multiplicity,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    if let Some(existing) = factor.get_mut(&(source, destination)) {
        *existing = existing.sum(weight);
    } else {
        control(GlaExecutionEvent::ScratchEntry)?;
        factor.insert((source, destination), weight);
    }
    Ok(())
}

fn oriented<E>(
    topology: &BTreeMap<TopologyKey, Multiplicity>,
    access: EdgeAccess,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Factor, E> {
    let mut result = Factor::new();
    for (&(source, relation, destination), &weight) in topology {
        control(GlaExecutionEvent::Work)?;
        if relation != access.relation { continue; }
        match access.direction {
            GlaDirection::Forward => add(&mut result, source, destination, weight, control)?,
            GlaDirection::Reverse => add(&mut result, destination, source, weight, control)?,
            GlaDirection::Undirected => {
                add(&mut result, source, destination, weight, control)?;
                if source != destination { add(&mut result, destination, source, weight, control)?; }
            }
        }
    }
    Ok(result)
}

/// A binary factor retains the joint endpoint distribution. Only equal middle
/// VIds join. Summing products preserves correlations, parallel choices, zero
/// annihilation and the explicit positive-overflow state.
fn compose<E>(
    left: &Factor,
    right: &Factor,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Factor, E> {
    let mut result = Factor::new();
    for (&(source, middle), &weight) in left {
        control(GlaExecutionEvent::Work)?;
        for (&(_, destination), &other) in right.range((middle, VId(0))..=(middle, VId(u128::MAX))) {
            control(GlaExecutionEvent::Work)?;
            add(&mut result, source, destination, weight.product(other), control)?;
        }
    }
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn visit_bindings<'a, E, C, F, R, M, P>(
    plan: &GlaPlan<GraphValueRow>,
    vertices: impl IntoIterator<Item = VId>,
    topology: &BTreeMap<TopologyKey, Multiplicity>,
    accesses: &[EdgeAccess],
    forest: &tree::Forest,
    test_vertex: F,
    property: R,
    mut control: M,
    mut visit: P,
) -> Result<(), VisitError<E, C>>
where
    F: FnMut(VId, &[VertexPredicate]) -> Result<bool, VisitError<E, C>>,
    R: FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, VisitError<E, C>>,
    M: FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
    P: FnMut(&[ValueProjection], &[Option<VId>], &mut R, &mut M, Multiplicity)
        -> Result<(), VisitError<E, C>>,
{
    let reduced = prepare(plan, accesses, forest, &mut control)?;
    let mut derived = BTreeMap::new();
    let mut remapped_accesses = Vec::new();
    if let Some(reduced) = &reduced {
        for (ordinal, route) in reduced.routes.iter().enumerate() {
            control(GlaExecutionEvent::Work)?;
            let first = accesses[*route.first().ok_or_else(unavailable)?];
            let last = accesses[*route.last().ok_or_else(unavailable)?];
            let mut factor = oriented(topology, first, &mut control)?;
            for &at in &route[1..] {
                control(GlaExecutionEvent::Work)?;
                if factor.is_empty() { break; }
                let next = oriented(topology, accesses[at], &mut control)?;
                factor = compose(&factor, &next, &mut control)?;
            }
            let relation = RelationId(ordinal as u64);
            for ((source, destination), weight) in factor {
                control(GlaExecutionEvent::ScratchEntry)?;
                derived.insert((source, relation, destination), weight);
            }
            control(GlaExecutionEvent::ScratchEntry)?;
            remapped_accesses.push(EdgeAccess {
                source: reduced.slots[first.source].ok_or_else(unavailable)?,
                destination: reduced.slots[last.destination].ok_or_else(unavailable)?,
                relation,
                direction: GlaDirection::Forward,
            });
        }
    }
    let (execution, topology, accesses) = if let Some(reduced) = &reduced {
        (reduced.pattern.plan(), &derived, remapped_accesses.as_slice())
    } else { (plan, topology, accesses) };
    execution.visit_value_bindings(vertices, topology.keys().copied(), test_vertex, property, control,
        |columns, bindings, property, control| {
            let mut original = [None; MAX_PATTERN_VERTICES];
            let forest_bindings = if let Some(reduced) = &reduced {
                for (old, slot) in reduced.slots[..reduced.width].iter().enumerate() {
                    if let Some(slot) = slot {
                        control(GlaExecutionEvent::Work)?;
                        original[old] = bindings.get(*slot).copied().flatten();
                    }
                }
                &original[..reduced.width]
            } else { bindings };
            let Some(mut weight) = forest.completion(forest_bindings, control)? else { return Ok(()); };
            for access in accesses {
                control(GlaExecutionEvent::Work)?;
                let source = bindings.get(access.source).copied().flatten().ok_or_else(unavailable)?;
                let destination = bindings.get(access.destination).copied().flatten().ok_or_else(unavailable)?;
                let key = match access.direction {
                    GlaDirection::Forward => (source, access.relation, destination),
                    GlaDirection::Reverse => (destination, access.relation, source),
                    GlaDirection::Undirected => normalized(source, access.relation, destination, true),
                };
                weight = weight.product(topology.get(&key).copied().ok_or_else(unavailable)?);
            }
            visit(columns, bindings, property, control, weight)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(n: u64) -> Multiplicity { Multiplicity(NonZeroU64::new(n)) }

    #[test]
    fn binary_composition_preserves_endpoint_correlations_and_sums_shared_middles() {
        let left = Factor::from([((VId(0), VId(10)), count(2)),
            ((VId(0), VId(11)), count(3)), ((VId(u128::MAX), VId(12)), count(7))]);
        let right = Factor::from([((VId(10), VId(20)), count(5)),
            ((VId(11), VId(20)), count(11)), ((VId(12), VId(21)), count(13))]);
        let actual = compose(&left, &right, &mut |_| Ok::<_, ()>(())).unwrap();
        assert_eq!(actual.len(), 2);
        assert_eq!(actual[&(VId(0), VId(20))].exact_count(), Some(43));
        assert_eq!(actual[&(VId(u128::MAX), VId(21))].exact_count(), Some(91));
        assert!(!actual.contains_key(&(VId(0), VId(21))));
    }

    #[test]
    fn zero_annihilates_overflow_and_every_composition_checkpoint_propagates() {
        let left = Factor::from([((VId(0), VId(u128::MAX)), Multiplicity(None))]);
        let absent = Factor::from([((VId(1), VId(2)), count(1))]);
        assert!(compose(&left, &absent, &mut |_| Ok::<_, ()>(())).unwrap().is_empty());
        let right = Factor::from([((VId(u128::MAX), VId(0)), count(1))]);
        let mut events = 0;
        let completed = compose(&left, &right, &mut |_| { events += 1; Ok::<_, usize>(()) }).unwrap();
        assert_eq!(completed[&(VId(0), VId(0))].exact_count(), None);
        for stop in 1..=events {
            let mut at = 0;
            let result = compose(&left, &right, &mut |_| {
                at += 1; if at == stop { Err(stop) } else { Ok(()) }
            });
            assert!(matches!(result, Err(value) if value == stop));
            assert_eq!(at, stop);
        }
    }
}
