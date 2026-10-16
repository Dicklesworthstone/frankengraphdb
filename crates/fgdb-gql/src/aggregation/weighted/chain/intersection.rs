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

/// What a trie is keyed by: engine identities, or positions in a positioned
/// graph's vertex table. Positions order and equate exactly as the
/// identities they stand for (the table ascends strictly), so every
/// comparison below takes the same branch on either, and a search over
/// positions makes the same charges in the same order; positions index
/// their rows and seen-sets by array instead of by hash.
pub(in crate::aggregation::weighted) trait Key:
    Copy + Ord + core::hash::Hash
{
    /// Root -> its children's range: probed, never iterated.
    type Rows;
    /// Keys seen: probed, never iterated.
    type Seen;
    /// The index of ascending `roots`, root `r`'s children being
    /// `offsets[r]..offsets[r + 1]`, over keys below `bound` (positions;
    /// identities ignore it).
    fn index(bound: usize, roots: &[Self], offsets: &[usize]) -> Self::Rows;
    /// `key`'s children's range, if it is a root (a root has children).
    fn children(rows: &Self::Rows, key: Self) -> Option<(usize, usize)>;
    fn seen(bound: usize) -> Self::Seen;
    fn was_seen(seen: &Self::Seen, key: Self) -> bool;
    fn see(seen: &mut Self::Seen, key: Self);
}

impl Key for VId {
    type Rows = crate::id_hash::IdMap<VId, (usize, usize)>;
    type Seen = crate::id_hash::IdSet<VId>;
    fn index(_: usize, roots: &[Self], offsets: &[usize]) -> Self::Rows {
        roots
            .iter()
            .enumerate()
            .map(|(row, &root)| (root, (offsets[row], offsets[row + 1])))
            .collect()
    }
    fn children(rows: &Self::Rows, key: Self) -> Option<(usize, usize)> {
        rows.get(&key).copied()
    }
    fn seen(_: usize) -> Self::Seen {
        Self::Seen::default()
    }
    fn was_seen(seen: &Self::Seen, key: Self) -> bool {
        seen.contains(&key)
    }
    fn see(seen: &mut Self::Seen, key: Self) {
        seen.insert(key);
    }
}

/// Every position's children start, so a position's children are
/// `starts[at]..starts[at + 1]` (empty unless it is a root) with no row
/// index between: one dependent load fewer per lookup. One seen bit per
/// position. Children never exceed the edges a positioned graph holds,
/// whose count fits 32 bits.
impl Key for u32 {
    type Rows = Vec<u32>;
    type Seen = Vec<u64>;
    fn index(bound: usize, roots: &[Self], offsets: &[usize]) -> Self::Rows {
        let mut starts = Vec::with_capacity(bound + 1);
        let mut next = 0;
        for at in 0..=bound {
            while next < roots.len() && (roots[next] as usize) < at {
                next += 1;
            }
            starts.push(offsets[next] as u32);
        }
        starts
    }
    fn children(rows: &Self::Rows, key: Self) -> Option<(usize, usize)> {
        let (start, end) = (rows[key as usize], rows[key as usize + 1]);
        (start < end).then_some((start as usize, end as usize))
    }
    fn seen(bound: usize) -> Self::Seen {
        vec![0; bound.div_ceil(64)]
    }
    fn was_seen(seen: &Self::Seen, key: Self) -> bool {
        seen[key as usize / 64] >> (key % 64) & 1 == 1
    }
    fn see(seen: &mut Self::Seen, key: Self) {
        seen[key as usize / 64] |= 1 << (key % 64);
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
struct Trie<K: Key> {
    low: usize,
    high: usize,
    roots: Vec<K>,
    /// Probed, never iterated: root -> its children's range in `children`.
    rows: K::Rows,
    children: Vec<K>,
}
impl<K: Key> Trie<K> {
    /// From the topology's keys in key order; positions are below `bound`.
    fn build<E>(
        atom: Atom,
        keys: impl Iterator<Item = (K, RelationId, K)>,
        bound: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        use crate::id_hash::IdSet;
        // A key is unique per (left, relation, right), so one orientation per
        // key never repeats a pair; only the undirected doubling can.
        let mut repeated =
            (atom.direction == GlaDirection::Undirected).then(IdSet::<(K, K)>::default);
        let mut lows = K::seen(bound);
        let mut pairs = Vec::new();
        for (left, relation, right) in keys {
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
                let new_low = !K::was_seen(&lows, low);
                if new_low {
                    control(GlaExecutionEvent::ScratchEntry)?;
                }
                let new_pair = new_low
                    || repeated
                        .as_ref()
                        .is_none_or(|seen| !seen.contains(&(low, high)));
                if new_pair {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    K::see(&mut lows, low);
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
        let mut offsets = vec![0];
        let mut children = Vec::with_capacity(pairs.len());
        let mut at = 0;
        while at < pairs.len() {
            let low = pairs[at].0;
            control(GlaExecutionEvent::Work)?;
            control(GlaExecutionEvent::ScratchEntry)?;
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
        // The index takes no event: the build's charges are the tree
        // build's, in its order.
        let rows = K::index(bound, &roots, &offsets);
        Ok(Self {
            low: atom.left.min(atom.right),
            high: atom.left.max(atom.right),
            roots,
            rows,
            children,
        })
    }
    fn domain(&self, variable: usize, assigned: &[Option<K>]) -> Option<&[K]> {
        if variable == self.low {
            Some(&self.roots)
        } else if variable == self.high {
            Some(
                assigned[self.low]
                    .and_then(|low| K::children(&self.rows, low))
                    .map_or(&[][..], |(start, end)| &self.children[start..end]),
            )
        } else {
            None
        }
    }
}

/// One variable's candidates from one trie: all of its roots, or the
/// children of its low variable's assignment.
enum Domain<'t, K: Key> {
    Roots(&'t Trie<K>),
    Children(&'t [K]),
}
impl<K: Key> Clone for Domain<'_, K> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<K: Key> Copy for Domain<'_, K> {}
impl<'t, K: Key> Domain<'t, K> {
    fn values(self) -> &'t [K] {
        match self {
            Self::Roots(trie) => &trie.roots,
            Self::Children(values) => values,
        }
    }

    /// Whether `value` is a candidate: one probe of a trie's row index, or a
    /// binary search of a child list, each comparison charged.
    fn contains<E>(
        self,
        value: K,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<bool, E> {
        match self {
            Self::Roots(trie) => {
                control(GlaExecutionEvent::Work)?;
                Ok(K::children(&trie.rows, value).is_some())
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
fn lower_bound<K: Ord, E>(
    values: &[K],
    bound: Option<K>,
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
fn intersect<'t, K: Key, E>(
    tries: &'t [Trie<K>],
    variable: usize,
    assigned: &[Option<K>],
    after: Option<K>,
    resume: &mut Resume,
    domains: &mut [Option<Domain<'t, K>>; crate::algebra::MAX_PATTERN_EDGES],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<K>, E> {
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

/// The vertex count of a positioned graph and its identity at a position,
/// for a topology weighed with positions.
pub(in crate::aggregation::weighted) type VertexAt<'v> = (usize, &'v dyn Fn(u32) -> VId);

/// `visit_positioned_bindings` over identities alone.
pub(in crate::aggregation::weighted) fn visit_bindings<'a, E, C, F, R, M, P>(
    plan: &GlaPlan<GraphValueRow>,
    vertices: impl IntoIterator<Item = VId>,
    topology: &Topology,
    test_vertex: F,
    property: R,
    control: M,
    visit: P,
) -> Result<(), VisitError<E, C>>
where
    F: FnMut(VId, &[VertexPredicate]) -> Result<bool, VisitError<E, C>>,
    R: FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, VisitError<E, C>>,
    M: FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
    P: FnMut(&[ValueProjection], &[Option<VId>], &mut R, &mut M) -> Result<(), VisitError<E, C>>,
{
    visit_positioned_bindings(
        plan,
        vertices,
        topology,
        None,
        test_vertex,
        property,
        control,
        visit,
    )
}

/// Every complete support binding of a cyclic plan, by ordered multiway
/// intersection. A topology weighed with positions, given its graph's
/// `positioned` vertices, is searched by position.
#[allow(clippy::too_many_arguments)]
pub(in crate::aggregation::weighted) fn visit_positioned_bindings<'a, E, C, F, R, M, P>(
    plan: &GlaPlan<GraphValueRow>,
    vertices: impl IntoIterator<Item = VId>,
    topology: &Topology,
    positioned: Option<VertexAt<'_>>,
    test_vertex: F,
    property: R,
    mut control: M,
    visit: P,
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
    if let (Some((bound, vertex)), Some(_)) = (positioned, topology.positioned_keys()) {
        let mut tries = Vec::new();
        for atom in shape.atoms[..shape.edges].iter().flatten() {
            control(GlaExecutionEvent::Work)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            let keys = topology
                .positioned_keys()
                .expect("a positioned topology keeps its positions");
            tries.push(Trie::build(*atom, keys, bound, &mut control)?);
        }
        return search(&shape, &tries, vertex, property, control, visit);
    }
    let mut tries = Vec::new();
    for atom in shape.atoms[..shape.edges].iter().flatten() {
        control(GlaExecutionEvent::Work)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        tries.push(Trie::build(
            *atom,
            topology.keys().copied(),
            0,
            &mut control,
        )?);
    }
    search(&shape, &tries, |vid| vid, property, control, visit)
}

/// The ordered search over built tries; each complete binding reaches the
/// selections and `visit` by identity.
fn search<'a, K: Key, E, C, R, M, P>(
    shape: &Shape<'_>,
    tries: &[Trie<K>],
    vertex: impl Fn(K) -> VId,
    mut property: R,
    mut control: M,
    mut visit: P,
) -> Result<(), VisitError<E, C>>
where
    R: FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, VisitError<E, C>>,
    M: FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
    P: FnMut(&[ValueProjection], &[Option<VId>], &mut R, &mut M) -> Result<(), VisitError<E, C>>,
{
    let mut assigned: [Option<K>; MAX_PATTERN_VERTICES] = [None; MAX_PATTERN_VERTICES];
    let mut after: [Option<K>; MAX_PATTERN_VERTICES] = [None; MAX_PATTERN_VERTICES];
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
                bindings[at] = assigned[shape.representatives[at]].map(&vertex);
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
            tries,
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
