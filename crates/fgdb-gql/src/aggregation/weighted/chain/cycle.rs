//! Sparse sum/product elimination inside the positive topology-only profile.
//!
//! Equality identifies binding positions before degree is measured. Parallel
//! constraints multiply pointwise; alternative hidden assignments add. A loop
//! is a unary factor, not an independent endpoint. Eliminate only hidden nodes
//! with one or two distinct neighbors, in stable original-position order. This
//! covers series/parallel cyclic regions without a higher-arity factor or a
//! second matcher. Larger separators stay in the original GLA execution core.
//!
//! Root positions, output operands, inequalities and forest attachments remain
//! observable. All tables are private, metered and in memory, not spill storage.

use super::*;

type Pair = (usize, usize);
type Factors = BTreeMap<Pair, Factor>;

fn pair(left: usize, right: usize) -> Pair {
    (left.min(right), left.max(right))
}
fn representative(parents: &[usize], mut slot: usize) -> usize {
    while parents[slot] != slot { slot = parents[slot]; }
    slot
}

struct Shape {
    representatives: [usize; MAX_PATTERN_VERTICES],
    retained: [bool; MAX_PATTERN_VERTICES],
    eliminations: Vec<(usize, usize, Option<usize>)>,
    width: usize,
}

/// A structural, deterministic schedule. No cardinality-driven plan choice.
fn shape<E, C>(
    plan: &GlaPlan<GraphValueRow>,
    accesses: &[EdgeAccess],
    forest: &tree::Forest,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
) -> Result<Option<Shape>, VisitError<E, C>> {
    // Pure trees already have a cheaper completion/chain path. Do not change
    // that path's physical counters merely because this specialization exists.
    if !plan.operators().iter().any(|op| matches!(op,
        GlaOperator::VertexIdentity { equal: true, .. })) { return Ok(None); }
    let width = accesses.len() + 1;
    if !(2..=MAX_PATTERN_VERTICES).contains(&width) { return Err(unavailable()); }
    let mut parents: [usize; MAX_PATTERN_VERTICES] = std::array::from_fn(|at| at);
    let mut observed = [false; MAX_PATTERN_VERTICES];
    observed[0] = true;
    observed[1] = true;
    for operator in plan.operators() {
        control(GlaExecutionEvent::Work)?;
        match operator {
            GlaOperator::VertexIdentity { left, right, equal } => {
                let (left, right) = (left.ordinal() as usize, right.ordinal() as usize);
                if left >= width || right >= width { return Err(unavailable()); }
                if *equal {
                    let left = representative(&parents, left);
                    let right = representative(&parents, right);
                    parents[left.max(right)] = left.min(right);
                } else { observed[left] = true; observed[right] = true; }
            }
            GlaOperator::ProjectValues { columns } => {
                for column in columns {
                    control(GlaExecutionEvent::Work)?;
                    let ValueProjection::Vertex { slot } = column else { return Ok(None); };
                    let at = slot.ordinal() as usize;
                    if at >= width { return Err(unavailable()); }
                    observed[at] = true;
                }
            }
            GlaOperator::ScanEdges { .. } | GlaOperator::Expand { .. }
            | GlaOperator::OrderByValues | GlaOperator::Limit { offset: 0, count: None } => {}
            _ => return Ok(None),
        }
    }
    for slot in forest.anchors() {
        control(GlaExecutionEvent::Work)?;
        if slot >= width { return Err(unavailable()); }
        observed[slot] = true;
    }
    let mut retained = [false; MAX_PATTERN_VERTICES];
    let mut protected = [false; MAX_PATTERN_VERTICES];
    for at in 0..width {
        control(GlaExecutionEvent::Work)?;
        parents[at] = representative(&parents, at);
        retained[parents[at]] = true;
        protected[parents[at]] |= observed[at];
    }
    let mut edges = BTreeSet::new();
    for (at, edge) in accesses.iter().enumerate() {
        control(GlaExecutionEvent::Work)?;
        if edge.destination != at + 1 || edge.source >= edge.destination {
            return Err(unavailable());
        }
        let key = pair(parents[edge.source], parents[edge.destination]);
        if !edges.contains(&key) {
            control(GlaExecutionEvent::ScratchEntry)?;
            edges.insert(key);
        }
    }
    let mut eliminations = Vec::new();
    loop {
        let mut selected = None;
        for vertex in 0..width {
            if !retained[vertex] || protected[vertex] { continue; }
            control(GlaExecutionEvent::Work)?;
            let mut neighbors = [0; 2];
            let mut degree = 0;
            for &(left, right) in &edges {
                control(GlaExecutionEvent::Work)?;
                if left == right { continue; }
                let other = if left == vertex { right } else if right == vertex { left } else { continue; };
                if degree == 2 { degree = 3; break; }
                neighbors[degree] = other;
                degree += 1;
            }
            if degree == 1 || degree == 2 {
                selected = Some((vertex, neighbors[0], (degree == 2).then_some(neighbors[1])));
                break;
            }
        }
        let Some((vertex, left, right)) = selected else { break; };
        control(GlaExecutionEvent::ScratchEntry)?;
        eliminations.push((vertex, left, right));
        retained[vertex] = false;
        edges.remove(&pair(vertex, left));
        edges.remove(&(vertex, vertex));
        if let Some(right) = right { edges.remove(&pair(vertex, right)); }
        let next = pair(left, right.unwrap_or(left));
        if !edges.contains(&next) {
            control(GlaExecutionEvent::ScratchEntry)?;
            edges.insert(next);
        }
    }
    if eliminations.is_empty() { return Ok(None); }
    Ok(Some(Shape { representatives: parents, retained, eliminations, width }))
}

/// Conjoining two constraints is intersection/product, never union/sum. Even
/// an empty table must be retained: it records an impossible required factor.
fn conjoin<E>(
    factors: &mut Factors,
    key: Pair,
    value: Factor,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    let value = if let Some(previous) = factors.remove(&key) {
        let mut product = Factor::new();
        for (endpoints, weight) in previous {
            control(GlaExecutionEvent::Work)?;
            if let Some(other) = value.get(&endpoints) {
                control(GlaExecutionEvent::ScratchEntry)?;
                product.insert(endpoints, weight.product(*other));
            }
        }
        product
    } else { value };
    control(GlaExecutionEvent::ScratchEntry)?;
    factors.insert(key, value);
    Ok(())
}

fn take_oriented<E, C>(
    factors: &mut Factors,
    source: usize,
    destination: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
) -> Result<Factor, VisitError<E, C>> {
    let factor = factors.remove(&pair(source, destination)).ok_or_else(unavailable)?;
    if source <= destination { return Ok(factor); }
    let mut reverse = Factor::new();
    for ((left, right), weight) in factor {
        control(GlaExecutionEvent::ScratchEntry)?;
        reverse.insert((right, left), weight);
    }
    Ok(reverse)
}

fn eliminate<E, C>(
    factors: &mut Factors,
    vertex: usize,
    left: usize,
    right: Option<usize>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
) -> Result<(), VisitError<E, C>> {
    let unary = factors.remove(&(vertex, vertex));
    let incoming = take_oriented(factors, left, vertex, control)?;
    let outgoing = right.map(|right| take_oriented(factors, vertex, right, control)).transpose()?;
    let mut result = Factor::new();
    for ((source, middle), mut weight) in incoming {
        control(GlaExecutionEvent::Work)?;
        if let Some(unary) = &unary {
            let Some(other) = unary.get(&(middle, middle)) else { continue; };
            weight = weight.product(*other);
        }
        if let Some(outgoing) = &outgoing {
            for (&(_, destination), &other) in outgoing.range((middle, VId(0))..=(middle, VId(u128::MAX))) {
                control(GlaExecutionEvent::Work)?;
                let (source, destination) = if left <= right.expect("outgoing has an endpoint") {
                    (source, destination)
                } else { (destination, source) };
                add(&mut result, source, destination, weight.product(other), control)?;
            }
        } else { add(&mut result, source, source, weight, control)?; }
    }
    conjoin(factors, pair(left, right.unwrap_or(left)), result, control)
}

pub(super) struct Reduced {
    pattern: PreparedGraphPattern<GraphValueRow>,
    topology: BTreeMap<TopologyKey, Multiplicity>,
    accesses: Vec<EdgeAccess>,
    slots: [Option<usize>; MAX_PATTERN_VERTICES],
    columns: Vec<ValueProjection>,
    width: usize,
}

pub(super) fn contract<E, C>(
    plan: &GlaPlan<GraphValueRow>,
    topology: &BTreeMap<TopologyKey, Multiplicity>,
    accesses: &[EdgeAccess],
    forest: &tree::Forest,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
) -> Result<Option<Reduced>, VisitError<E, C>> {
    let Some(shape) = shape(plan, accesses, forest, control)? else { return Ok(None); };
    let reps = &shape.representatives;
    let mut factors = Factors::new();
    for &edge in accesses {
        control(GlaExecutionEvent::Work)?;
        let (source, destination) = (reps[edge.source], reps[edge.destination]);
        let factor = oriented(topology, edge, control)?;
        let mut normalized = Factor::new();
        for ((left, right), weight) in factor {
            control(GlaExecutionEvent::Work)?;
            if source == destination && left != right { continue; }
            let endpoints = if source <= destination { (left, right) } else { (right, left) };
            control(GlaExecutionEvent::ScratchEntry)?;
            normalized.insert(endpoints, weight);
        }
        conjoin(&mut factors, pair(source, destination), normalized, control)?;
    }
    for &(vertex, left, right) in &shape.eliminations {
        control(GlaExecutionEvent::Work)?;
        eliminate(&mut factors, vertex, left, right, control)?;
    }
    let mut builder = GraphPatternBuilder::new();
    let mut names: [String; MAX_PATTERN_VERTICES] = std::array::from_fn(|_| String::new());
    let mut variables = Vec::new();
    for (at, retained) in shape.retained[..shape.width].iter().enumerate() {
        if !*retained { continue; }
        control(GlaExecutionEvent::ScratchEntry)?;
        names[at] = format!("v{at}");
        control(GlaExecutionEvent::ScratchEntry)?;
        builder.vertex(&names[at]).map_err(|_| unavailable())?;
        control(GlaExecutionEvent::ScratchEntry)?;
        variables.push(at);
    }
    // Retain the original root factor first. All derived relation identities
    // live in a new private table, disjoint from catalog relation identities.
    let root = pair(reps[0], reps[1]);
    let mut pairs = Vec::new();
    control(GlaExecutionEvent::ScratchEntry)?;
    pairs.push(root);
    for &key in factors.keys() {
        if key != root { control(GlaExecutionEvent::ScratchEntry)?; pairs.push(key); }
    }
    let mut derived = BTreeMap::new();
    for (ordinal, &(source, destination)) in pairs.iter().enumerate() {
        let factor = factors.remove(&(source, destination)).ok_or_else(unavailable)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        builder.edge(&names[source], RelationId(ordinal as u64), GlaDirection::Forward, &names[destination])
            .map_err(|_| unavailable())?;
        for ((left, right), weight) in factor {
            control(GlaExecutionEvent::ScratchEntry)?;
            derived.insert((left, RelationId(ordinal as u64), right), weight);
        }
    }
    let mut identities = 0;
    for operator in plan.operators() {
        if let GlaOperator::VertexIdentity { left, right, equal: false } = operator {
            control(GlaExecutionEvent::ScratchEntry)?;
            builder.identity(&names[reps[left.ordinal() as usize]], &names[reps[right.ordinal() as usize]], false)
                .map_err(|_| unavailable())?;
            identities += 1;
        }
    }
    let mut output = Vec::new();
    for &variable in &variables {
        control(GlaExecutionEvent::ScratchEntry)?;
        output.push(GraphColumn::vertex(&names[variable], &names[variable]));
    }
    for _ in 0..(7 * variables.len() + 3 * pairs.len() + 2 * identities + 4) {
        control(GlaExecutionEvent::ScratchEntry)?;
    }
    let pattern = builder.prepare_values(&output, 0, None).map_err(|_| unavailable())?.with_duplicates();
    // Ask the SAME compiler for its actual slots. Declaration order is not
    // binding order after cyclic constraints and self-loops are rebuilt.
    let mapped = pattern.plan().operators().iter().find_map(|op| match op {
        GlaOperator::ProjectValues { columns } => Some(columns), _ => None,
    }).ok_or_else(unavailable)?;
    let mut canonical_slots = [None; MAX_PATTERN_VERTICES];
    for (&variable, column) in variables.iter().zip(mapped) {
        control(GlaExecutionEvent::Work)?;
        let ValueProjection::Vertex { slot } = column else { return Err(unavailable()); };
        canonical_slots[variable] = Some(slot.ordinal() as usize);
    }
    let mut slots = [None; MAX_PATTERN_VERTICES];
    for old in 0..shape.width { slots[old] = canonical_slots[reps[old]]; }
    let mut columns = Vec::new();
    for operator in plan.operators() {
        if let GlaOperator::ProjectValues { columns: original } = operator {
            for column in original {
                let ValueProjection::Vertex { slot } = column else { return Err(unavailable()); };
                control(GlaExecutionEvent::ScratchEntry)?;
                let mapped = canonical_slots[reps[slot.ordinal() as usize]].ok_or_else(unavailable)?;
                // Reuse a compiler-owned slot value; no private constructor is
                // made public merely to enable this physical specialization.
                let native = mapped_slot(mapped, pattern.plan()).ok_or_else(unavailable)?;
                columns.push(ValueProjection::Vertex { slot: native });
            }
        }
    }
    let mut mapped_accesses = Vec::new();
    for (ordinal, (source, destination)) in pairs.into_iter().enumerate() {
        control(GlaExecutionEvent::ScratchEntry)?;
        mapped_accesses.push(EdgeAccess {
            source: canonical_slots[source].ok_or_else(unavailable)?,
            destination: canonical_slots[destination].ok_or_else(unavailable)?,
            relation: RelationId(ordinal as u64), direction: GlaDirection::Forward,
        });
    }
    Ok(Some(Reduced { pattern, topology: derived, accesses: mapped_accesses, slots, columns, width: shape.width }))
}

fn mapped_slot(ordinal: usize, plan: &GlaPlan<GraphValueRow>) -> Option<crate::algebra::BindingSlot> {
    plan.operators().iter().find_map(|op| match op {
        GlaOperator::ProjectValues { columns } => columns.iter().find_map(|column| match column {
            ValueProjection::Vertex { slot } if slot.ordinal() as usize == ordinal => Some(*slot), _ => None,
        }),
        _ => None,
    })
}

impl Reduced {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn visit_bindings<'a, E, C, F, R, M, P>(
        &self, vertices: impl IntoIterator<Item = VId>, forest: &tree::Forest,
        test_vertex: F, property: R, control: M, mut visit: P,
    ) -> Result<(), VisitError<E, C>>
    where
        F: FnMut(VId, &[VertexPredicate]) -> Result<bool, VisitError<E, C>>,
        R: FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, VisitError<E, C>>,
        M: FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
        P: FnMut(&[ValueProjection], &[Option<VId>], &mut R, &mut M, Multiplicity) -> Result<(), VisitError<E, C>>,
    {
        self.pattern.plan().visit_value_bindings(vertices, self.topology.keys().copied(), test_vertex, property, control,
            |_, bindings, property, control| {
                let mut original = [None; MAX_PATTERN_VERTICES];
                for (old, slot) in self.slots[..self.width].iter().enumerate() {
                    if let Some(slot) = slot {
                        control(GlaExecutionEvent::Work)?;
                        original[old] = bindings.get(*slot).copied().flatten();
                    }
                }
                let Some(mut weight) = forest.completion(&original[..self.width], control)? else { return Ok(()); };
                for edge in &self.accesses {
                    control(GlaExecutionEvent::Work)?;
                    let source = bindings.get(edge.source).copied().flatten().ok_or_else(unavailable)?;
                    let destination = bindings.get(edge.destination).copied().flatten().ok_or_else(unavailable)?;
                    weight = weight.product(self.topology.get(&(source, edge.relation, destination))
                        .copied().ok_or_else(unavailable)?);
                }
                visit(&self.columns, bindings, property, control, weight)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weight(n: u64) -> Multiplicity { Multiplicity(NonZeroU64::new(n)) }

    #[test]
    fn alternative_paths_add_but_parallel_constraints_multiply() {
        let incoming = Factor::from([
            ((VId(10), VId(20)), weight(2)), ((VId(10), VId(21)), weight(3)),
        ]);
        let outgoing = Factor::from([
            ((VId(20), VId(30)), weight(5)), ((VId(21), VId(30)), weight(7)),
        ]);
        let mut factors = Factors::from([
            ((0, 1), incoming), ((1, 2), outgoing),
            ((0, 2), Factor::from([((VId(10), VId(30)), weight(11))])),
            ((1, 1), Factor::from([((VId(20), VId(20)), weight(13))])),
        ]);
        eliminate::<(), ()>(&mut factors, 1, 0, Some(2), &mut |_| Ok(())).unwrap();
        assert_eq!(factors.len(), 1);
        assert_eq!(factors[&(0, 2)][&(VId(10), VId(30))].exact_count(), Some(2 * 5 * 13 * 11));
    }

    #[test]
    fn impossible_unary_factor_annihilates_overflow_without_becoming_identity() {
        let make = || Factors::from([
            ((0, 1), Factor::from([((VId(0), VId(1)), Multiplicity(None))])),
            ((1, 1), Factor::new()),
        ]);
        let mut factors = make();
        let mut total = 0;
        eliminate::<(), usize>(&mut factors, 1, 0, None, &mut |_| {
            total += 1; Ok(())
        }).unwrap();
        assert!(factors.contains_key(&(0, 0)));
        assert!(factors[&(0, 0)].is_empty());
        for stop in 1..=total {
            let mut factors = make();
            let mut at = 0;
            let result = eliminate::<(), usize>(&mut factors, 1, 0, None, &mut |_| {
                at += 1;
                if at == stop { Err(GqlQueryError::Interrupted(stop)) } else { Ok(()) }
            });
            assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
            assert_eq!(at, stop);
        }
    }
}
