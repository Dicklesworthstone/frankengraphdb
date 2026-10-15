//! Ordered multiway intersection for cyclic aggregate support bindings.
//!
//! At each canonical variable position, intersect every incident factor's
//! current trie level before extending the binding. A closing edge therefore
//! constrains the candidate domain, rather than filtering a materialized wedge.
//! Equality aliases are one variable; distinct variables may share a VId. Each
//! complete support binding reaches the existing multiplicity/forest callback.
//! This does not merge multiplicities or invent another aggregate evaluator.
//!
//! Property projection and a trailing property/Boolean selection phase are
//! supported without assuming pure or infallible readers. ALL topology and
//! identity constraints must precede the first read. The surviving complete
//! bindings retain native lexicographic order; each read occurs in its original
//! phase and order. A filter before a later expansion/identity stays on GLA:
//! moving it could suppress its errors on an ultimately rejected prefix.
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
    selections: &'a [GlaOperator],
}

fn root(parents: &[usize], mut at: usize) -> usize {
    while parents[at] != at {
        at = parents[at];
    }
    at
}

impl<'a> Shape<'a> {
    /// Pure bounded definition inspection. No source or catalog is consulted.
    /// Keep the cheaper existing tree/chain path. No topology constraint may
    /// move across a property read. Only a genuine cycle after equality and
    /// parallel-edge reduction selects this path; loops alone do not do so.
    fn compile(plan: &'a GlaPlan<GraphValueRow>) -> Option<Self> {
        let operators = plan.operators();
        let GlaOperator::ScanEdges {
            relation,
            direction,
        } = operators.first()?
        else {
            return None;
        };
        let mut atoms = [None; crate::algebra::MAX_PATTERN_EDGES];
        atoms[0] = Some(Atom {
            left: 0,
            right: 1,
            relation: relation.one()?,
            direction: *direction,
        });
        let mut edges = 1;
        let mut width = 2;
        let mut parents: [usize; MAX_PATTERN_VERTICES] = std::array::from_fn(|at| at);
        let mut columns = None;
        let mut projected = false;
        let mut ordered = false;
        let mut finished = false;
        let mut first_selection = None;
        let mut projection_at = None;
        for (at, op) in operators.iter().enumerate().skip(1) {
            if finished {
                return None;
            }
            match op {
                GlaOperator::Expand {
                    source,
                    relation,
                    direction,
                } if !projected
                    && first_selection.is_none()
                    && edges < atoms.len()
                    && width < MAX_PATTERN_VERTICES
                    && (source.ordinal() as usize) < width =>
                {
                    atoms[edges] = Some(Atom {
                        left: source.ordinal() as usize,
                        right: width,
                        relation: relation.one()?,
                        direction: *direction,
                    });
                    edges += 1;
                    width += 1;
                }
                GlaOperator::VertexIdentity { left, right, equal }
                    if !projected && first_selection.is_none() =>
                {
                    let (left, right) = (left.ordinal() as usize, right.ordinal() as usize);
                    if left >= width || right >= width {
                        return None;
                    }
                    if *equal {
                        let (left, right) = (root(&parents, left), root(&parents, right));
                        parents[left.max(right)] = left.min(right);
                    }
                }
                GlaOperator::CompareProperties { left, right, .. } if !projected => {
                    if (left.ordinal() as usize) >= width || (right.ordinal() as usize) >= width {
                        return None;
                    }
                    first_selection.get_or_insert(at);
                }
                GlaOperator::SelectBoolean { expression } if !projected => {
                    let mut valid = true;
                    let mut captured = false;
                    let _ = expression.remap_elements(
                        |slot| {
                            valid &= (slot.ordinal() as usize) < width;
                            slot
                        },
                        |path| {
                            captured = true;
                            path
                        },
                    );
                    if !valid || captured {
                        return None;
                    }
                    first_selection.get_or_insert(at);
                }
                GlaOperator::ProjectValues { columns: output } if !projected => {
                    if output.iter().any(|column| {
                        !matches!(column,
                        ValueProjection::Vertex { slot } | ValueProjection::Property { slot, .. }
                            if (slot.ordinal() as usize) < width)
                    }) {
                        return None;
                    }
                    columns = Some(output.as_slice());
                    projection_at = Some(at);
                    projected = true;
                }
                GlaOperator::OrderByValues if projected && !ordered => ordered = true,
                GlaOperator::Limit {
                    offset: 0,
                    count: None,
                } if ordered => finished = true,
                _ => return None,
            }
        }
        if !finished {
            return None;
        }
        for at in 0..width {
            parents[at] = root(&parents, at);
        }
        let mut components: [usize; MAX_PATTERN_VERTICES] = std::array::from_fn(|at| at);
        let mut pairs = [None; crate::algebra::MAX_PATTERN_EDGES];
        let mut count = 0;
        let mut cyclic = false;
        for atom in atoms[..edges].iter_mut().flatten() {
            atom.left = parents[atom.left];
            atom.right = parents[atom.right];
            let pair = (atom.left.min(atom.right), atom.left.max(atom.right));
            if pair.0 == pair.1 || pairs[..count].contains(&Some(pair)) {
                continue;
            }
            pairs[count] = Some(pair);
            count += 1;
            let (left, right) = (root(&components, pair.0), root(&components, pair.1));
            if left == right {
                cyclic = true;
            }
            components[left.max(right)] = left.min(right);
        }
        if !cyclic {
            return None;
        }
        let mut order = [0; MAX_PATTERN_VERTICES];
        let mut variables = 0;
        for (at, representative) in parents[..width].iter().enumerate() {
            if at == *representative {
                order[variables] = at;
                variables += 1;
            }
        }
        let projection_at = projection_at?;
        Some(Self {
            representatives: parents,
            order,
            variables,
            width,
            atoms,
            edges,
            columns: columns?,
            operators,
            selections: &operators[first_selection.unwrap_or(projection_at)..projection_at],
        })
    }
}

/// Binary trie in canonical variable order, not stored edge orientation.
/// Unary loops contain only diagonal values. Parallel edge support is unique
/// here; its exact weight remains in the parent's unchanged topology table.
///
/// Rows are stored flat: the roots ascending, each root's children ascending
/// in one shared vector, and a probed root -> row index. A tree of trees cost
/// two random descents per topology key to build and one per domain lookup
/// (38% of a 730K-edge triangle count). The build charges exactly what that
/// tree build charged, in the same order (pinned by a law against it).
struct Trie {
    low: usize,
    high: usize,
    roots: Vec<VId>,
    /// Probed, never iterated: root -> its row.
    rows: crate::id_hash::IdMap<VId, usize>,
    /// Row `r`'s children are `children[offsets[r]..offsets[r + 1]]`.
    offsets: Vec<usize>,
    children: Vec<VId>,
}
impl Trie {
    fn build<E>(
        atom: Atom,
        topology: &Topology,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        use crate::id_hash::IdSet;
        // A key is unique per (left, relation, right), so one orientation per
        // key never repeats a pair; only the undirected doubling can.
        let mut repeated =
            (atom.direction == GlaDirection::Undirected).then(IdSet::<(VId, VId)>::default);
        let mut lows = IdSet::<VId>::default();
        let mut pairs = Vec::new();
        for &(left, relation, right) in topology.keys() {
            control(GlaExecutionEvent::Work)?;
            if relation != atom.relation {
                continue;
            }
            let orientations = match atom.direction {
                GlaDirection::Forward => [(left, right), (left, right)],
                GlaDirection::Reverse => [(right, left), (right, left)],
                GlaDirection::Undirected => [(left, right), (right, left)],
            };
            let count = if atom.direction == GlaDirection::Undirected && left != right {
                2
            } else {
                1
            };
            for &(from, to) in orientations.iter().take(count) {
                control(GlaExecutionEvent::Work)?;
                if atom.left == atom.right && from != to {
                    continue;
                }
                let (low, high) = if atom.left <= atom.right {
                    (from, to)
                } else {
                    (to, from)
                };
                let new_low = !lows.contains(&low);
                if new_low {
                    control(GlaExecutionEvent::ScratchEntry)?;
                }
                let new_pair = new_low
                    || repeated
                        .as_ref()
                        .is_none_or(|seen| !seen.contains(&(low, high)));
                if new_pair {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    lows.insert(low);
                    if let Some(seen) = &mut repeated {
                        seen.insert((low, high));
                    }
                    pairs.push((low, high));
                }
            }
        }
        drop(lows);
        drop(repeated);
        pairs.sort_unstable();
        let mut roots = Vec::new();
        let mut rows = crate::id_hash::IdMap::default();
        let mut offsets = vec![0];
        let mut children = Vec::with_capacity(pairs.len());
        let mut at = 0;
        while at < pairs.len() {
            let low = pairs[at].0;
            control(GlaExecutionEvent::Work)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            rows.insert(low, roots.len());
            roots.push(low);
            control(GlaExecutionEvent::ScratchEntry)?;
            while let Some(&(_, value)) = pairs.get(at).filter(|(row, _)| *row == low) {
                control(GlaExecutionEvent::Work)?;
                control(GlaExecutionEvent::ScratchEntry)?;
                children.push(value);
                at += 1;
            }
            offsets.push(children.len());
        }
        Ok(Self {
            low: atom.left.min(atom.right),
            high: atom.left.max(atom.right),
            roots,
            rows,
            offsets,
            children,
        })
    }
    fn domain(&self, variable: usize, assigned: &[Option<VId>]) -> Option<&[VId]> {
        if variable == self.low {
            Some(&self.roots)
        } else if variable == self.high {
            Some(
                assigned[self.low]
                    .and_then(|low| self.rows.get(&low))
                    .map_or(&[][..], |&row| {
                        &self.children[self.offsets[row]..self.offsets[row + 1]]
                    }),
            )
        } else {
            None
        }
    }
}

/// One variable's candidates from one trie: all of its roots, or the
/// children of its low variable's assignment.
#[derive(Clone, Copy)]
enum Domain<'t> {
    Roots(&'t Trie),
    Children(&'t [VId]),
}
impl<'t> Domain<'t> {
    fn values(self) -> &'t [VId] {
        match self {
            Self::Roots(trie) => &trie.roots,
            Self::Children(values) => values,
        }
    }

    /// Whether `value` is a candidate: one probe of a trie's row index, or a
    /// binary search of a child list, each comparison charged.
    fn contains<E>(
        self,
        value: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<bool, E> {
        match self {
            Self::Roots(trie) => {
                control(GlaExecutionEvent::Work)?;
                Ok(trie.rows.contains_key(&value))
            }
            Self::Children(values) => {
                let at = lower_bound(values, Some(value), false, control)?;
                Ok(values.get(at) == Some(&value))
            }
        }
    }
}

/// The first position at or after (strictly after, when `exclusive`)
/// `bound`; every comparison is charged and no identity is incremented, so
/// the full u128 domain includes both endpoints.
fn lower_bound<E>(
    values: &[VId],
    bound: Option<VId>,
    exclusive: bool,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<usize, E> {
    control(GlaExecutionEvent::Work)?;
    let Some(bound) = bound else {
        return Ok(0);
    };
    let (mut low, mut high) = (0, values.len());
    while low < high {
        control(GlaExecutionEvent::Work)?;
        let middle = low + (high - low) / 2;
        if values[middle] < bound || (exclusive && values[middle] == bound) {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    Ok(low)
}

/// Where a variable's previous answer sits in the domain that produced it:
/// that domain's (address, length) and the answer's index.
type Resume = Option<(usize, usize, usize)>;

/// The smallest value after `after` in every incident trie's domain. The
/// shortest domain drives and each other one is probed per candidate: a
/// root list (hundreds of thousands of vertices) is a hash probe rather than
/// a binary search repeated for every candidate, which was most of a
/// 730K-edge triangle count's comparisons. The next call for the same
/// variable resumes after its previous answer instead of searching again.
fn intersect<'t, E>(
    tries: &'t [Trie],
    variable: usize,
    assigned: &[Option<VId>],
    after: Option<VId>,
    resume: &mut Resume,
    domains: &mut [Option<Domain<'t>>; crate::algebra::MAX_PATTERN_EDGES],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<VId>, E> {
    let mut count = 0;
    for trie in tries {
        control(GlaExecutionEvent::Work)?;
        if let Some(values) = trie.domain(variable, assigned) {
            if values.is_empty() {
                return Ok(None);
            }
            domains[count] = Some(if variable == trie.low {
                Domain::Roots(trie)
            } else {
                Domain::Children(values)
            });
            count += 1;
        }
    }
    let Some(driver) = (0..count).min_by_key(|&at| domains[at].map_or(0, |d| d.values().len()))
    else {
        return Ok(None);
    };
    let values = domains[driver]
        .expect("counted domains are present")
        .values();
    let key = (values.as_ptr() as usize, values.len());
    let mut at = match (*resume, after) {
        (Some((address, len, index)), Some(after))
            if (address, len) == key && values.get(index) == Some(&after) =>
        {
            index + 1
        }
        _ => lower_bound(values, after, true, control)?,
    };
    'candidates: while let Some(&candidate) = values.get(at) {
        control(GlaExecutionEvent::Work)?;
        for (other, domain) in domains[..count].iter().enumerate() {
            if other != driver
                && !domain
                    .expect("counted domains are present")
                    .contains(candidate, control)?
            {
                at += 1;
                continue 'candidates;
            }
        }
        *resume = Some((key.0, key.1, at));
        return Ok(Some(candidate));
    }
    *resume = None;
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
pub(in crate::aggregation::weighted) fn visit_bindings<'a, E, C, F, R, M, P>(
    plan: &GlaPlan<GraphValueRow>,
    vertices: impl IntoIterator<Item = VId>,
    topology: &Topology,
    test_vertex: F,
    mut property: R,
    mut control: M,
    mut visit: P,
) -> Result<(), VisitError<E, C>>
where
    F: FnMut(VId, &[VertexPredicate]) -> Result<bool, VisitError<E, C>>,
    R: FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, VisitError<E, C>>,
    M: FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
    P: FnMut(&[ValueProjection], &[Option<VId>], &mut R, &mut M) -> Result<(), VisitError<E, C>>,
{
    let Some(shape) = Shape::compile(plan) else {
        return plan.visit_value_bindings(
            vertices,
            topology.keys().copied(),
            test_vertex,
            property,
            control,
            visit,
        );
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
    let mut resume: [Resume; MAX_PATTERN_VERTICES] = [None; MAX_PATTERN_VERTICES];
    let mut bindings = [None; MAX_PATTERN_VERTICES];
    // Every search's domains, filled afresh by each `intersect` up to its
    // own count: one pattern-wide array for the execution, not one per search.
    let mut domains = [None; crate::algebra::MAX_PATTERN_EDGES];
    let mut depth = 0;
    loop {
        control(GlaExecutionEvent::Work)?;
        if depth == shape.variables {
            for at in 0..shape.width {
                control(GlaExecutionEvent::Work)?;
                bindings[at] = assigned[shape.representatives[at]];
            }
            // The guard proves every ordinary topology/identity test has
            // already succeeded before these reads would occur. Do not cache,
            // pre-read, reorder, or swallow a fallible property lookup.
            let mut accepted = true;
            for selection in shape.selections {
                control(GlaExecutionEvent::Work)?;
                if !crate::algebra_exec::compare_properties(
                    selection,
                    &bindings[..shape.width],
                    &mut property,
                    &mut control,
                )? {
                    accepted = false;
                    break;
                }
            }
            if accepted {
                visit(
                    shape.columns,
                    &bindings[..shape.width],
                    &mut property,
                    &mut control,
                )?;
            }
            depth -= 1;
            assigned[shape.order[depth]] = None;
            continue;
        }
        let variable = shape.order[depth];
        let Some(value) = intersect(
            &tries,
            variable,
            &assigned,
            after[depth],
            &mut resume[depth],
            &mut domains,
            &mut control,
        )?
        else {
            assigned[variable] = None;
            after[depth] = None;
            if depth == 0 {
                return Ok(());
            }
            depth -= 1;
            assigned[shape.order[depth]] = None;
            continue;
        };
        after[depth] = Some(value);
        assigned[variable] = Some(value);
        let mut accepted = true;
        for operator in shape.operators {
            if let GlaOperator::VertexIdentity {
                left,
                right,
                equal: false,
            } = operator
            {
                control(GlaExecutionEvent::Work)?;
                let left = assigned[shape.representatives[left.ordinal() as usize]];
                let right = assigned[shape.representatives[right.ordinal() as usize]];
                if left.is_some() && left == right {
                    accepted = false;
                    break;
                }
            }
        }
        if !accepted {
            assigned[variable] = None;
            continue;
        }
        depth += 1;
        if depth < shape.variables {
            after[depth] = None;
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod properties;
