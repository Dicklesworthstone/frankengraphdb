//! Ordered multiway intersection for cyclic, topology-only aggregate cores.
//!
//! At each canonical variable position, intersect every incident factor's
//! current trie level before extending the binding. A closing edge therefore
//! constrains the candidate domain, rather than filtering a materialized wedge.
//! Equality aliases are one variable; distinct variables may share a VId. Each
//! complete support binding reaches the existing multiplicity/forest callback.
//! This does not merge multiplicities or invent another aggregate evaluator.
//!
//! The tries are private, metered in-memory arrangements of the already admitted
//! topology. They do not claim a persisted Strata trie order, spill, allocator-
//! byte accounting, adaptive variable order, or the full FreeJoin contract.

use super::*;

#[derive(Clone, Copy)]
struct Atom {
    left: usize,
    right: usize,
    relation: RelationId,
    direction: GlaDirection,
}

struct Shape<'a> {
    representatives: [usize; MAX_PATTERN_VERTICES],
    order: [usize; MAX_PATTERN_VERTICES],
    variables: usize,
    width: usize,
    atoms: [Option<Atom>; crate::algebra::MAX_PATTERN_EDGES],
    edges: usize,
    columns: &'a [ValueProjection],
    operators: &'a [GlaOperator],
}

fn root(parents: &[usize], mut at: usize) -> usize {
    while parents[at] != at { at = parents[at]; }
    at
}

impl<'a> Shape<'a> {
    /// Pure bounded definition inspection. No source or catalog is consulted.
    /// Keep the cheaper existing tree/chain path and all effectful predicates
    /// unchanged. Only a genuine cycle after equality/parallel-edge reduction
    /// selects this path; loops or duplicated constraints alone do not do so.
    fn compile(plan: &'a GlaPlan<GraphValueRow>) -> Option<Self> {
        let operators = plan.operators();
        let GlaOperator::ScanEdges { relation, direction } = operators.first()? else { return None; };
        let mut atoms = [None; crate::algebra::MAX_PATTERN_EDGES];
        atoms[0] = Some(Atom { left: 0, right: 1, relation: *relation, direction: *direction });
        let mut edges = 1;
        let mut width = 2;
        let mut parents: [usize; MAX_PATTERN_VERTICES] = std::array::from_fn(|at| at);
        let mut columns = None;
        let mut projected = false;
        let mut ordered = false;
        let mut finished = false;
        for op in &operators[1..] {
            if finished { return None; }
            match op {
                GlaOperator::Expand { source, relation, direction }
                    if !projected && edges < atoms.len() && width < MAX_PATTERN_VERTICES
                        && (source.ordinal() as usize) < width => {
                    atoms[edges] = Some(Atom { left: source.ordinal() as usize, right: width,
                        relation: *relation, direction: *direction });
                    edges += 1;
                    width += 1;
                }
                GlaOperator::VertexIdentity { left, right, equal } if !projected => {
                    let (left, right) = (left.ordinal() as usize, right.ordinal() as usize);
                    if left >= width || right >= width { return None; }
                    if *equal {
                        let (left, right) = (root(&parents, left), root(&parents, right));
                        parents[left.max(right)] = left.min(right);
                    }
                }
                GlaOperator::ProjectValues { columns: output } if !projected => {
                    if output.iter().any(|column| !matches!(column,
                        ValueProjection::Vertex { slot } if (slot.ordinal() as usize) < width)) { return None; }
                    columns = Some(output.as_slice());
                    projected = true;
                }
                GlaOperator::OrderByValues if projected && !ordered => ordered = true,
                GlaOperator::Limit { offset: 0, count: None } if ordered => finished = true,
                _ => return None,
            }
        }
        if !finished { return None; }
        for at in 0..width { parents[at] = root(&parents, at); }
        let mut components: [usize; MAX_PATTERN_VERTICES] = std::array::from_fn(|at| at);
        let mut pairs = [None; crate::algebra::MAX_PATTERN_EDGES];
        let mut count = 0;
        let mut cyclic = false;
        for atom in atoms[..edges].iter_mut().flatten() {
            atom.left = parents[atom.left];
            atom.right = parents[atom.right];
            let pair = (atom.left.min(atom.right), atom.left.max(atom.right));
            if pair.0 == pair.1 || pairs[..count].contains(&Some(pair)) { continue; }
            pairs[count] = Some(pair);
            count += 1;
            let (left, right) = (root(&components, pair.0), root(&components, pair.1));
            if left == right { cyclic = true; }
            components[left.max(right)] = left.min(right);
        }
        if !cyclic { return None; }
        let mut order = [0; MAX_PATTERN_VERTICES];
        let mut variables = 0;
        for (at, representative) in parents[..width].iter().enumerate() {
            if at == *representative { order[variables] = at; variables += 1; }
        }
        Some(Self { representatives: parents, order, variables, width, atoms, edges,
            columns: columns?, operators })
    }
}

/// Binary trie in canonical variable order, not stored edge orientation.
/// Unary loops contain only diagonal values. Parallel edge support is unique
/// here; its exact weight remains in the parent's unchanged topology table.
struct Trie {
    low: usize,
    high: usize,
    roots: Vec<VId>,
    children: BTreeMap<VId, Vec<VId>>,
}
impl Trie {
    fn build<E>(atom: Atom, topology: &BTreeMap<TopologyKey, Multiplicity>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>) -> Result<Self, E> {
        let mut rows: BTreeMap<VId, BTreeSet<VId>> = BTreeMap::new();
        for &(left, relation, right) in topology.keys() {
            control(GlaExecutionEvent::Work)?;
            if relation != atom.relation { continue; }
            let orientations = match atom.direction {
                GlaDirection::Forward => [(left, right), (left, right)],
                GlaDirection::Reverse => [(right, left), (right, left)],
                GlaDirection::Undirected => [(left, right), (right, left)],
            };
            let count = if atom.direction == GlaDirection::Undirected && left != right { 2 } else { 1 };
            for &(from, to) in orientations.iter().take(count) {
                control(GlaExecutionEvent::Work)?;
                if atom.left == atom.right && from != to { continue; }
                let (low, high) = if atom.left <= atom.right { (from, to) } else { (to, from) };
                if !rows.contains_key(&low) { control(GlaExecutionEvent::ScratchEntry)?; }
                if !rows.get(&low).is_some_and(|row| row.contains(&high)) {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    rows.entry(low).or_default().insert(high);
                }
            }
        }
        let mut roots = Vec::new();
        let mut children = BTreeMap::new();
        for (low, values) in rows {
            control(GlaExecutionEvent::Work)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            roots.push(low);
            control(GlaExecutionEvent::ScratchEntry)?;
            let mut child = Vec::new();
            for value in values {
                control(GlaExecutionEvent::Work)?;
                control(GlaExecutionEvent::ScratchEntry)?;
                child.push(value);
            }
            children.insert(low, child);
        }
        Ok(Self { low: atom.left.min(atom.right), high: atom.left.max(atom.right), roots, children })
    }
    fn domain(&self, variable: usize, assigned: &[Option<VId>]) -> Option<&[VId]> {
        if variable == self.low { Some(&self.roots) }
        else if variable == self.high {
            Some(assigned[self.low].and_then(|low| self.children.get(&low)).map_or(&[][..], Vec::as_slice))
        } else { None }
    }
}

/// Fallible lower/strict-upper bound: all comparisons are charged and no
/// identity is incremented, so the full u128 domain includes both endpoints.
fn seek<E>(values: &[VId], bound: Option<VId>, exclusive: bool,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>) -> Result<Option<VId>, E> {
    control(GlaExecutionEvent::Work)?;
    let Some(bound) = bound else { return Ok(values.first().copied()); };
    let (mut low, mut high) = (0, values.len());
    while low < high {
        control(GlaExecutionEvent::Work)?;
        let middle = low + (high - low) / 2;
        if values[middle] < bound || (exclusive && values[middle] == bound) { low = middle + 1; }
        else { high = middle; }
    }
    Ok(values.get(low).copied())
}

fn intersect<E>(tries: &[Trie], variable: usize, assigned: &[Option<VId>], after: Option<VId>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>) -> Result<Option<VId>, E> {
    let mut domains = [None; crate::algebra::MAX_PATTERN_EDGES];
    let mut count = 0;
    for trie in tries {
        control(GlaExecutionEvent::Work)?;
        if let Some(domain) = trie.domain(variable, assigned) {
            if domain.is_empty() { return Ok(None); }
            domains[count] = Some(domain);
            count += 1;
        }
    }
    let Some(first) = domains[0] else { return Ok(None); };
    let Some(mut candidate) = seek(first, after, true, control)? else { return Ok(None); };
    loop {
        let mut moved = false;
        for domain in domains[..count].iter().flatten() {
            let Some(next) = seek(domain, Some(candidate), false, control)? else { return Ok(None); };
            if next > candidate { candidate = next; moved = true; }
        }
        if !moved { return Ok(Some(candidate)); }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn visit_bindings<'a, E, C, F, R, M, P>(
    plan: &GlaPlan<GraphValueRow>, vertices: impl IntoIterator<Item = VId>,
    topology: &BTreeMap<TopologyKey, Multiplicity>, test_vertex: F,
    mut property: R, mut control: M, mut visit: P,
) -> Result<(), VisitError<E, C>>
where
    F: FnMut(VId, &[VertexPredicate]) -> Result<bool, VisitError<E, C>>,
    R: FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, VisitError<E, C>>,
    M: FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
    P: FnMut(&[ValueProjection], &[Option<VId>], &mut R, &mut M) -> Result<(), VisitError<E, C>>,
{
    let Some(shape) = Shape::compile(plan) else {
        return plan.visit_value_bindings(vertices, topology.keys().copied(), test_vertex, property, control, visit);
    };
    // Definition-bounded frames live on the stack. Heap arrangements reserve
    // every retained entry before allocation. Counters are cumulative, not a
    // claim about peak bytes or BTreeMap's internal comparison count.
    let mut tries = Vec::new();
    for atom in shape.atoms[..shape.edges].iter().flatten() {
        control(GlaExecutionEvent::Work)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        tries.push(Trie::build(*atom, topology, &mut control)?);
    }
    let mut assigned = [None; MAX_PATTERN_VERTICES];
    let mut after = [None; MAX_PATTERN_VERTICES];
    let mut bindings = [None; MAX_PATTERN_VERTICES];
    let mut depth = 0;
    loop {
        control(GlaExecutionEvent::Work)?;
        if depth == shape.variables {
            for at in 0..shape.width {
                control(GlaExecutionEvent::Work)?;
                bindings[at] = assigned[shape.representatives[at]];
            }
            visit(shape.columns, &bindings[..shape.width], &mut property, &mut control)?;
            depth -= 1;
            assigned[shape.order[depth]] = None;
            continue;
        }
        let variable = shape.order[depth];
        let Some(value) = intersect(&tries, variable, &assigned, after[depth], &mut control)? else {
            assigned[variable] = None;
            after[depth] = None;
            if depth == 0 { return Ok(()); }
            depth -= 1;
            assigned[shape.order[depth]] = None;
            continue;
        };
        after[depth] = Some(value);
        assigned[variable] = Some(value);
        let mut accepted = true;
        for operator in shape.operators {
            if let GlaOperator::VertexIdentity { left, right, equal: false } = operator {
                control(GlaExecutionEvent::Work)?;
                let left = assigned[shape.representatives[left.ordinal() as usize]];
                let right = assigned[shape.representatives[right.ordinal() as usize]];
                if left.is_some() && left == right { accepted = false; break; }
            }
        }
        if !accepted { assigned[variable] = None; continue; }
        depth += 1;
        if depth < shape.variables { after[depth] = None; }
    }
}

#[cfg(test)]
mod tests;
