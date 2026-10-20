//! One scan-backed evaluator for scalar and correlated binding-row projections.
//! Requested relation/orientation pairs are indexed once. Controls precede
//! admitted-row work, operator visits, scratch growth and final row release.

mod comparison;
mod different_edges;
mod join;
mod policy;
mod projection;
mod trail;
pub(crate) use comparison::{
    charge_payload, charge_stored_payload, compare_element_properties, compare_properties,
    compare_stored,
};
pub(crate) use policy::Charges;
pub use policy::{GqlQueryError, GqlQueryExecution, GqlQueryPolicy};
pub use projection::{GovernedCheckpoint, ProjectedRows};

use crate::algebra::{
    EdgeRelation, GlaDirection, GlaIdentityOutput, GlaOperator, GlaOutput, GlaPlan, GraphPath,
    GraphWalkSearch, VertexPredicate,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, EId, VId};
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

#[inline]
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

#[inline]
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
type Index = BTreeMap<(EdgeRelation, GlaDirection), Adjacency>;
use crate::identified::{IdentifiedAdjacency, IdentifiedNeighbors, IdentifiedStage};
type IdentifiedIndex = BTreeMap<(EdgeRelation, GlaDirection), IdentifiedAdjacency>;

// Search kernels borrow the SAME admitted index and feed the same binding
// continuation. Search is a prepared logical choice, never an adaptive fallback
// or a post-filter over all walks. Ordinary WALK's event sequence is unchanged.
enum WalkExpansion<'a> {
    All(crate::GraphWalkCursor<'a>),
    AllShortest(crate::GraphShortestWalkCursor<'a>),
}
impl<'a> WalkExpansion<'a> {
    fn new<E>(
        search: GraphWalkSearch,
        source: VId,
        bounds: crate::GraphWalkBounds,
        adjacency: Option<&'a Adjacency>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        match search {
            GraphWalkSearch::All => {
                crate::GraphWalkCursor::new(source, bounds, adjacency, control).map(Self::All)
            }
            GraphWalkSearch::AllShortest => {
                crate::GraphShortestWalkCursor::new(source, bounds, adjacency, control)
                    .map(Self::AllShortest)
            }
            GraphWalkSearch::AnyShortest => {
                crate::GraphShortestWalkCursor::new_any(source, bounds, adjacency, control)
                    .map(Self::AllShortest)
            }
            GraphWalkSearch::Acyclic => {
                crate::GraphWalkCursor::new_acyclic(source, bounds, adjacency, control)
                    .map(Self::All)
            }
            GraphWalkSearch::Simple => {
                crate::GraphWalkCursor::new_simple(source, bounds, adjacency, control)
                    .map(Self::All)
            }
            GraphWalkSearch::Trail => {
                unreachable!("TRAIL requires the identified-edge execution lane")
            }
        }
    }

    fn next_with_control<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<VId>, E> {
        match self {
            Self::All(cursor) => cursor.next_with_control(control),
            Self::AllShortest(cursor) => cursor.next_with_control(control),
        }
    }
}

fn push_neighbor<E>(
    adjacency: &mut Adjacency,
    source: VId,
    destination: VId,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    control(GlaExecutionEvent::ScratchEntry)?;
    // Scans deliver edges grouped by source, so the newest key usually takes
    // the next neighbor without a search from the root. Same events.
    if let Some(mut last) = adjacency.last_entry()
        && *last.key() == source
    {
        last.get_mut().push(destination);
        return Ok(());
    }
    adjacency.entry(source).or_default().push(destination);
    Ok(())
}

/// Whether `values` is already ascending, one charged comparison per
/// adjacent pair up to the first descent. Edges usually arrive in canonical
/// (source, destination) order, which leaves forward and reverse neighbor
/// lists sorted: a heapsort's worst case (every sift moves) at about
/// 3 d log d charged events for a list of d, against d - 1 for this check.
fn ascending<T: Ord, E>(
    values: &[T],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<bool, E> {
    for pair in values.windows(2) {
        control(GlaExecutionEvent::Work)?;
        if pair[0] > pair[1] {
            return Ok(false);
        }
    }
    Ok(true)
}

/// In-place iterative heapsort; each comparison/swap crosses the work seam.
/// An already ascending list is only checked (`ascending`).
fn sort_neighbors<E>(
    values: &mut [VId],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    if ascending(values, control)? {
        return Ok(());
    }
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

/// The distinct type-alternation selectors a plan registered in its index.
fn alternation_selectors(selectors: impl Iterator<Item = EdgeRelation>) -> Vec<EdgeRelation> {
    let mut alternations: Vec<_> = selectors
        .filter(|selector| matches!(selector, EdgeRelation::Set(_)))
        .collect();
    alternations.sort_unstable();
    alternations.dedup();
    alternations
}

/// Every index selector one edge of `relation` belongs to: its own type,
/// the untyped selector, and each registered alternation naming the type.
fn edge_selectors(
    relation: RelationId,
    alternations: &[EdgeRelation],
) -> impl Iterator<Item = EdgeRelation> + '_ {
    [EdgeRelation::One(relation), EdgeRelation::Any]
        .into_iter()
        .chain(
            alternations
                .iter()
                .copied()
                .filter(move |selector| selector.matches(relation)),
        )
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
        }
        | GlaOperator::VarLengthExpand {
            relation,
            direction,
            ..
        } = operator
        {
            index.entry((*relation, *direction)).or_default();
        }
    }
    join::register_indexes(operators, &mut index, control)?;
    let alternations = alternation_selectors(index.keys().map(|(relation, _)| *relation));
    for (source, relation, destination) in edges {
        control(GlaExecutionEvent::Work)?;
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            for selector in edge_selectors(relation, &alternations) {
                let Some(adjacency) = index.get_mut(&(selector, direction)) else {
                    continue;
                };
                match direction {
                    GlaDirection::Forward => {
                        push_neighbor(adjacency, source, destination, control)?
                    }
                    GlaDirection::Reverse => {
                        push_neighbor(adjacency, destination, source, control)?
                    }
                    GlaDirection::Undirected => {
                        push_neighbor(adjacency, source, destination, control)?;
                        if source != destination {
                            push_neighbor(adjacency, destination, source, control)?;
                        }
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

fn build_identified_index<E>(
    operators: &[GlaOperator],
    edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<IdentifiedIndex, E> {
    let mut index = IdentifiedIndex::new();
    for operator in operators {
        if matches!(
            operator,
            GlaOperator::ScanEdges { .. }
                | GlaOperator::Expand { .. }
                | GlaOperator::VarLengthExpand { .. }
        ) {
            let (relation, direction) = match operator {
                GlaOperator::ScanEdges {
                    relation,
                    direction,
                }
                | GlaOperator::Expand {
                    relation,
                    direction,
                    ..
                }
                | GlaOperator::VarLengthExpand {
                    relation,
                    direction,
                    ..
                } => (*relation, *direction),
                _ => unreachable!(),
            };
            if let std::collections::btree_map::Entry::Vacant(e) =
                index.entry((relation, direction))
            {
                control(GlaExecutionEvent::ScratchEntry)?;
                e.insert(IdentifiedAdjacency::default());
            }
        }
    }
    let alternations = alternation_selectors(index.keys().map(|(relation, _)| *relation));
    // Identified edges need not arrive grouped by endpoint, so each push is
    // one hash probe into a stage, which becomes the flat adjacency once at
    // the end. Neighbors keep their arrival order within a key, so every
    // charged event, and each list's sort, is as before.
    let keys: Vec<_> = index.keys().copied().collect();
    let mut staged: Vec<IdentifiedStage> =
        keys.iter().map(|_| IdentifiedStage::default()).collect();
    // The stages one relation's edges join, in the order the per-edge walk
    // reaches them (direction, then selector), resolved once per run of that
    // relation: three tree searches per edge were 9% of an epinions SHORTEST.
    let stages_of = |relation| {
        let mut stages = Vec::new();
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            for selector in edge_selectors(relation, &alternations) {
                if let Ok(at) = keys.binary_search(&(selector, direction)) {
                    stages.push((at, direction));
                }
            }
        }
        stages
    };
    let mut resolved: Option<(RelationId, Vec<(usize, GlaDirection)>)> = None;
    for (edge, source, relation, destination) in edges {
        control(GlaExecutionEvent::Work)?;
        if resolved
            .as_ref()
            .is_none_or(|(known, _)| *known != relation)
        {
            resolved = Some((relation, stages_of(relation)));
        }
        let (_, stages) = resolved.as_ref().expect("resolved above");
        for &(at, direction) in stages {
            let adjacency = &mut staged[at];
            let (from, to) = if direction == GlaDirection::Reverse {
                (destination, source)
            } else {
                (source, destination)
            };
            control(GlaExecutionEvent::ScratchEntry)?;
            adjacency.push(from, (edge, to), control)?;
            if direction == GlaDirection::Undirected && source != destination {
                control(GlaExecutionEvent::ScratchEntry)?;
                adjacency.push(destination, (edge, source), control)?;
            }
        }
    }
    for (key, adjacency) in keys.into_iter().zip(staged) {
        index.insert(key, adjacency.finish());
    }
    for adjacency in index.values_mut() {
        for neighbors in adjacency.lists_mut() {
            sort_identified_neighbors(neighbors, control)?;
        }
    }
    Ok(index)
}

fn sort_identified_neighbors<E>(
    values: &mut [(EId, VId)],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    fn sift<E>(
        values: &mut [(EId, VId)],
        mut root: usize,
        end: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        while root < end / 2 {
            let mut child = 2 * root + 1;
            if child + 1 < end {
                control(GlaExecutionEvent::Work)?;
                if values[child] < values[child + 1] {
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
    if ascending(values, control)? {
        return Ok(());
    }
    let end = values.len();
    for root in (0..end / 2).rev() {
        sift(values, root, end, control)?;
    }
    for end in (1..end).rev() {
        control(GlaExecutionEvent::Work)?;
        values.swap(0, end);
        sift(values, 0, end, control)?;
    }
    Ok(())
}

// Interpreter bookkeeping is bounded by the prepared operator count, not by
// row multiplicity. It does not introduce logical Work/ScratchEntry events.
enum Enumeration<'i> {
    Scan { at: usize },
    Candidates(join::Candidates<'i>),
    Walk(WalkExpansion<'i>),
    // Boxed: the different-edges cursors (7d752d17) made this the one large
    // arm, and every Continuation::Enumerate carried its full size.
    Identified(Box<trail::IdentifiedExpansion<'i>>),
}

impl Enumeration<'_> {
    fn next<E>(
        &mut self,
        vertex_domain: &[VId],
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<(VId, Option<GraphPath>)>, E> {
        match self {
            Self::Scan { at } => {
                let Some(&vid) = vertex_domain.get(*at) else {
                    return Ok(None);
                };
                control(GlaExecutionEvent::ScratchEntry)?;
                *at += 1;
                Ok(Some((vid, None)))
            }
            Self::Candidates(cursor) => {
                cursor.next(control).map(|next| next.map(|vid| (vid, None)))
            }
            Self::Walk(cursor) => cursor
                .next_with_control(control)
                .map(|next| next.map(|vid| (vid, None))),
            Self::Identified(cursor) => {
                let next = cursor.next_with_control(control)?;
                if next.is_some() {
                    control(GlaExecutionEvent::ScratchEntry)?;
                }
                Ok(next)
            }
        }
    }
}

enum Continuation<'i> {
    Enumerate {
        ordinal: usize,
        width: usize,
        cursor: Enumeration<'i>,
        yielded: bool,
        previous_segment: Option<GraphPath>,
    },
    Bind {
        width: usize,
    },
    CapturePath {
        capture: usize,
        previous: Option<GraphPath>,
    },
    Probe {
        group: usize,
        end: usize,
        anti: bool,
        width: usize,
        previous: Option<usize>,
    },
    Optional {
        group: usize,
        end: usize,
        slots: u32,
        width: usize,
    },
}

struct Execution<'i, F, C, P, Row> {
    test_vertex: F,
    control: C,
    project: P,
    // Probed per binding and never iterated, so a hash map: one probe where
    // a tree took a search for get and another for insert.
    predicate_cache: crate::id_hash::IdMap<(usize, VId), bool>,
    projected: ProjectedRows<Row>,
    active_probe: Option<usize>,
    probe_matches: [bool; crate::algebra::MAX_PATTERN_IDENTITIES],
    optional_matches: [bool; crate::algebra::MAX_PATTERN_IDENTITIES],
    // One admitted identity stream, shared by all independent scan frames.
    // Never materialize a Cartesian intermediate or reopen a source per row.
    vertex_domain: Vec<VId>,
    identified_index: Option<&'i IdentifiedIndex>,
    segments: [Option<GraphPath>; crate::algebra::MAX_PATTERN_BINDINGS],
    paths: [Option<GraphPath>; crate::algebra::MAX_PATTERN_IDENTITIES],
    continuations: Vec<Continuation<'i>>,
    terminal_closure: Option<join::TerminalClosure>,
}

// Aggregate visitors may observe every occurrence. Only sealed terminal row
// collectors can use a DISTINCT/page bound to factor repeated closing edges.
enum Projection<P> {
    Terminal(P),
    Visitor(P),
}

impl<'i, F, C, P, Row: GlaOutput> Execution<'i, F, C, P, Row> {
    // Restoration is infallible and emits no events. Both normal backtracking
    // and refusal drain the same scope frames in reverse installation order.
    fn restore(&mut self, frame: &mut Continuation<'i>, bindings: &mut Vec<Option<VId>>) {
        match frame {
            Continuation::Enumerate {
                width,
                cursor,
                yielded,
                previous_segment,
                ..
            } => {
                if *yielded {
                    if matches!(cursor, Enumeration::Identified(_)) {
                        self.segments[*width] = previous_segment.take();
                    }
                    bindings.truncate(*width);
                    *yielded = false;
                }
            }
            Continuation::Bind { width } | Continuation::Optional { width, .. } => {
                bindings.truncate(*width);
            }
            Continuation::CapturePath { capture, previous } => {
                self.paths[*capture] = previous.take();
            }
            Continuation::Probe {
                width, previous, ..
            } => {
                bindings.truncate(*width);
                self.active_probe = *previous;
            }
        }
    }

    fn visit<E>(
        &mut self,
        operators: &[GlaOperator],
        ordinal: usize,
        bindings: &mut Vec<Option<VId>>,
        index: &'i Index,
    ) -> Result<(), E>
    where
        F: FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        C: FnMut(GlaExecutionEvent) -> Result<(), E>,
        P: FnMut(
            &GlaOperator,
            &[Option<VId>],
            &[Option<GraphPath>],
            &mut ProjectedRows<Row>,
            &mut C,
        ) -> Result<bool, E>,
    {
        debug_assert!(self.continuations.is_empty());
        let result = self.run(operators, ordinal, bindings, index);
        // Never resume an enumerator, synthesize absence, or call control after
        // a refusal. Even partially constructed captures/null rows are restored.
        while let Some(mut frame) = self.continuations.pop() {
            self.restore(&mut frame, bindings);
        }
        result
    }

    fn run<E>(
        &mut self,
        operators: &[GlaOperator],
        ordinal: usize,
        bindings: &mut Vec<Option<VId>>,
        index: &'i Index,
    ) -> Result<(), E>
    where
        F: FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        C: FnMut(GlaExecutionEvent) -> Result<(), E>,
        P: FnMut(
            &GlaOperator,
            &[Option<VId>],
            &[Option<GraphPath>],
            &mut ProjectedRows<Row>,
            &mut C,
        ) -> Result<bool, E>,
    {
        let mut next = Some(ordinal);
        loop {
            // Every installed frame belongs to a different live operator.
            // Resumption replaces that frame; it never accumulates row history.
            debug_assert!(self.continuations.len() <= operators.len());
            let Some(ordinal) = next.take() else {
                let Some(mut frame) = self.continuations.pop() else {
                    return Ok(());
                };
                let yielded = matches!(&frame, Continuation::Enumerate { yielded: true, .. });
                self.restore(&mut frame, bindings);
                match frame {
                    Continuation::Enumerate {
                        ordinal,
                        width,
                        mut cursor,
                        ..
                    } => {
                        // Check only after returning from a candidate, just as
                        // the cursor loops did; the first next is unconditional.
                        if yielded
                            && self
                                .active_probe
                                .is_some_and(|group| self.probe_matches[group])
                        {
                            continue;
                        }
                        if let Some((destination, segment)) =
                            cursor.next(&self.vertex_domain, &mut self.control)?
                        {
                            bindings.push(Some(destination));
                            let previous_segment = if matches!(&cursor, Enumeration::Identified(_))
                            {
                                core::mem::replace(&mut self.segments[width], segment)
                            } else {
                                None
                            };
                            self.continuations.push(Continuation::Enumerate {
                                ordinal,
                                width,
                                cursor,
                                yielded: true,
                                previous_segment,
                            });
                            next = Some(ordinal + 1);
                        }
                    }
                    Continuation::Probe {
                        group, end, anti, ..
                    } => {
                        if self.probe_matches[group] != anti {
                            next = Some(end + 1);
                        }
                    }
                    Continuation::Optional {
                        group,
                        end,
                        slots,
                        width,
                    } => {
                        // OptionalEnd records a witness BEFORE later clauses.
                        // Only successful exhaustion of this scope proves absence.
                        if !self.optional_matches[group] {
                            self.continuations.push(Continuation::Bind { width });
                            for _ in 0..slots {
                                (self.control)(GlaExecutionEvent::ScratchEntry)?;
                                bindings.push(None);
                            }
                            next = Some(end + 1);
                        }
                    }
                    Continuation::Bind { .. } | Continuation::CapturePath { .. } => {}
                }
                continue;
            };
            let Some(operator) = operators.get(ordinal) else {
                continue;
            };
            if let Some(closure) = self
                .terminal_closure
                .filter(|closure| closure.start == ordinal && closure.width == bindings.len())
            {
                self.continuations.push(Continuation::Bind {
                    width: bindings.len(),
                });
                let copies = closure.bind(operators, bindings, index, &mut self.control)?;
                for _ in 0..copies {
                    (self.control)(GlaExecutionEvent::Work)?;
                    let _ = (self.project)(
                        &operators[closure.projection],
                        bindings,
                        &self.paths,
                        &mut self.projected,
                        &mut self.control,
                    )?;
                }
                next = None;
                continue;
            }
            (self.control)(GlaExecutionEvent::Work)?;
            next = Some(ordinal + 1);
            match operator {
                GlaOperator::VertexIdentity { left, right, equal } => {
                    // NULL = NULL and NULL <> x are not matching predicates.
                    let keep = matches!((
                        bindings.get(left.ordinal() as usize),
                        bindings.get(right.ordinal() as usize),
                    ), (Some(Some(left)), Some(Some(right))) if (left == right) == *equal);
                    if !keep {
                        next = None;
                    }
                }
                GlaOperator::OptionalEnd { group } => {
                    self.optional_matches[*group as usize] = true;
                }
                GlaOperator::SelectPathLength {
                    capture,
                    comparison,
                    value,
                } => {
                    if !self.paths[*capture as usize]
                        .as_ref()
                        .is_some_and(|path| comparison.accepts(path.len() as i64, *value))
                    {
                        next = None;
                    }
                }
                GlaOperator::SelectPathNull {
                    capture, is_null, ..
                } => {
                    if self.paths[*capture as usize].is_none() != *is_null {
                        next = None;
                    }
                }
                GlaOperator::CompareProperties { .. } | GlaOperator::SelectBoolean { .. } => {
                    // The value-aware action selects without producing a row.
                    if !(self.project)(
                        operator,
                        bindings,
                        &self.paths,
                        &mut self.projected,
                        &mut self.control,
                    )? {
                        next = None;
                    }
                }
                GlaOperator::Select { slot, predicates } => {
                    let Some(value) = bindings.get(slot.ordinal() as usize).copied() else {
                        next = None;
                        continue;
                    };
                    let Some(vid) = value else {
                        // Nullable captures use missing-property semantics, with
                        // no source read or identity-keyed predicate cache entry.
                        for predicate in predicates {
                            for _ in 0..predicate.comparison_work_units() {
                                (self.control)(GlaExecutionEvent::Work)?;
                            }
                        }
                        if !predicates
                            .iter()
                            .all(|predicate| predicate.matches(&[], &[]))
                        {
                            next = None;
                        }
                        continue;
                    };
                    let keep = match self.predicate_cache.entry((ordinal, vid)) {
                        std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            (self.control)(GlaExecutionEvent::ScratchEntry)?;
                            // Cache hits neither reread the source nor repeat literal
                            // comparison work. Reserve that work before source entry.
                            for predicate in predicates {
                                for _ in 0..predicate.comparison_work_units() {
                                    (self.control)(GlaExecutionEvent::Work)?;
                                }
                            }
                            *entry.insert((self.test_vertex)(vid, predicates)?)
                        }
                    };
                    if !keep {
                        next = None;
                    }
                }
                GlaOperator::Project { .. }
                | GlaOperator::ProjectBindings { .. }
                | GlaOperator::ProjectValues { .. } => {
                    let _ = (self.project)(
                        operator,
                        bindings,
                        &self.paths,
                        &mut self.projected,
                        &mut self.control,
                    )?;
                    next = None;
                }
                GlaOperator::Empty
                | GlaOperator::ScanEdges { .. }
                | GlaOperator::Distinct
                | GlaOperator::OrderByVertexId
                | GlaOperator::OrderByBindings
                | GlaOperator::OrderByValues
                | GlaOperator::OrderByValueColumns { .. }
                | GlaOperator::Limit { .. } => next = None,
                GlaOperator::ScanVertices => {
                    self.continuations.push(Continuation::Enumerate {
                        ordinal,
                        width: bindings.len(),
                        cursor: Enumeration::Scan { at: 0 },
                        yielded: false,
                        previous_segment: None,
                    });
                    next = None;
                }
                GlaOperator::Expand {
                    source,
                    relation,
                    direction,
                }
                | GlaOperator::VarLengthExpand {
                    source,
                    relation,
                    direction,
                    ..
                } => {
                    let Some(source) = bindings.get(source.ordinal() as usize).copied().flatten()
                    else {
                        next = None;
                        continue;
                    };
                    let (bounds, search) = match operator {
                        GlaOperator::VarLengthExpand { bounds, search, .. } => (*bounds, *search),
                        _ => (
                            crate::GraphWalkBounds::new(1, 1).expect("one hop"),
                            GraphWalkSearch::All,
                        ),
                    };
                    let cursor = if let Some(identified) = self.identified_index {
                        let capture = operators.iter().any(|op| {
                            matches!(
                                op,
                                GlaOperator::CapturePath { .. }
                                    | GlaOperator::DifferentEdges { .. }
                            )
                        });
                        let adjacency = identified
                            .get(&(*relation, *direction))
                            .map(|adjacency| adjacency as &dyn IdentifiedNeighbors);
                        let cursor = if let Some(segments) =
                            different_edges::for_expansion(operators, ordinal, bindings.len())
                        {
                            let Some(forbidden) = different_edges::forbidden(
                                segments,
                                &self.segments,
                                &mut self.control,
                            )?
                            else {
                                next = None;
                                continue;
                            };
                            trail::IdentifiedExpansion::new_different_edges(
                                source,
                                bounds,
                                search,
                                adjacency,
                                forbidden,
                                &mut self.control,
                            )?
                        } else {
                            trail::IdentifiedExpansion::new(
                                source,
                                bounds,
                                search,
                                adjacency,
                                capture,
                                &mut self.control,
                            )?
                        };
                        Enumeration::Identified(Box::new(cursor))
                    } else if matches!(operator, GlaOperator::VarLengthExpand { .. }) {
                        Enumeration::Walk(WalkExpansion::new(
                            search,
                            source,
                            bounds,
                            index.get(&(*relation, *direction)),
                            &mut self.control,
                        )?)
                    } else {
                        let Some(neighbors) = index
                            .get(&(*relation, *direction))
                            .and_then(|adjacency| adjacency.get(&source))
                        else {
                            next = None;
                            continue;
                        };
                        Enumeration::Candidates(join::candidates(
                            operators,
                            ordinal,
                            bindings,
                            neighbors,
                            index,
                            &mut self.control,
                        )?)
                    };
                    self.continuations.push(Continuation::Enumerate {
                        ordinal,
                        width: bindings.len(),
                        cursor,
                        yielded: false,
                        previous_segment: None,
                    });
                    next = None;
                }
                GlaOperator::Probe { group, end, anti } => {
                    let group = *group as usize;
                    self.continuations.push(Continuation::Probe {
                        group,
                        end: *end as usize,
                        anti: *anti,
                        width: bindings.len(),
                        previous: self.active_probe,
                    });
                    self.active_probe = Some(group);
                    self.probe_matches[group] = false;
                }
                GlaOperator::ProbeEnd { group } => {
                    debug_assert_eq!(self.active_probe, Some(*group as usize));
                    self.probe_matches[*group as usize] = true;
                    next = None;
                }
                GlaOperator::Optional { group, end, slots } => {
                    let group = *group as usize;
                    self.optional_matches[group] = false;
                    self.continuations.push(Continuation::Optional {
                        group,
                        end: *end as usize,
                        slots: *slots,
                        width: bindings.len(),
                    });
                }
                GlaOperator::BindVertex { source } | GlaOperator::BindOuterVertex { source } => {
                    let value = bindings[source.ordinal() as usize];
                    if value.is_none() && matches!(operator, GlaOperator::BindVertex { .. }) {
                        next = None;
                        continue;
                    }
                    (self.control)(GlaExecutionEvent::ScratchEntry)?;
                    self.continuations.push(Continuation::Bind {
                        width: bindings.len(),
                    });
                    bindings.push(value);
                }
                GlaOperator::DifferentEdges { segments } => {
                    if !different_edges::accepts(segments, &self.segments, &mut self.control)? {
                        next = None;
                    }
                }
                GlaOperator::CapturePath {
                    capture,
                    start,
                    segments,
                } => {
                    let at = *capture as usize;
                    let previous = self.paths[at].take();
                    self.continuations.push(Continuation::CapturePath {
                        capture: at,
                        previous,
                    });
                    if let Some(start) = bindings[start.ordinal() as usize] {
                        let mut steps = Vec::new();
                        let mut complete = true;
                        let mut endpoint = start;
                        (self.control)(GlaExecutionEvent::ScratchEntry)?;
                        for slot in segments {
                            (self.control)(GlaExecutionEvent::Work)?;
                            let Some(segment) = &self.segments[slot.ordinal() as usize] else {
                                complete = false;
                                break;
                            };
                            // A segment is recorded from the vertex its edge was
                            // expanded from. A closing edge back to a bound vertex,
                            // or a walk the plan expanded from its far end, ends
                            // where the path stands instead: read it backwards,
                            // so the path keeps its declared order (fgdb-tk2u6).
                            let raw = segment.steps();
                            let end = raw.last().map_or(segment.start(), |step| step.1);
                            let forward = segment.start() == endpoint;
                            if !forward && end != endpoint {
                                complete = false;
                                break;
                            }
                            for index in 0..raw.len() {
                                (self.control)(GlaExecutionEvent::Work)?;
                                (self.control)(GlaExecutionEvent::ScratchEntry)?;
                                (self.control)(GlaExecutionEvent::ScratchEntry)?;
                                let step = if forward {
                                    raw[index]
                                } else {
                                    let at = raw.len() - 1 - index;
                                    let previous = if at == 0 {
                                        segment.start()
                                    } else {
                                        raw[at - 1].1
                                    };
                                    (raw[at].0, previous)
                                };
                                steps.push(step);
                                endpoint = step.1;
                            }
                        }
                        if complete {
                            self.paths[at] = Some(GraphPath::new(start, steps.into_boxed_slice()));
                        }
                    }
                }
            }
        }
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
            Projection::Terminal(
                |operator: &GlaOperator,
                 bindings: &[Option<VId>],
                 _paths: &[Option<GraphPath>],
                 projected: &mut ProjectedRows<Row>,
                 control: &mut _|
                 -> Result<bool, E> {
                    Row::collect(operator, bindings, projected, control)?;
                    Ok(false)
                },
            ),
        )
    }
}

impl<Row: GlaOutput> GlaPlan<Row> {
    /// Project borrowed canonical properties from the same immutable source as
    /// predicate reads. A property failure propagates unchanged, never as null.
    /// Execute captured values against admitted, real edge identities.
    pub fn execute_with_identified_properties_control<'a, E: From<crate::GraphIntegerError>>(
        &self,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
        test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        mut property: impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalPropertyValue>, E>,
        mut control: impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Vec<Row>, E> {
        if self.projects_property_maps() {
            return Err(crate::GraphIntegerError {
                instruction: 0,
                kind: crate::GraphIntegerErrorKind::PropertyMapSourceRequired,
            }
            .into());
        }
        if !self.requires_identified_edges() {
            return self.execute_with_properties_control(
                vertices,
                edges.into_iter().map(|(_, s, r, d)| (s, r, d)),
                test_vertex,
                property,
                control,
            );
        }
        let identified = build_identified_index(self.operators(), edges, &mut control)?;
        self.execute_projected_index(
            vertices,
            Index::new(),
            Some(identified),
            test_vertex,
            control,
            Projection::Terminal(
                |operator: &GlaOperator,
                 bindings: &[Option<VId>],
                 paths: &[Option<GraphPath>],
                 projected: &mut ProjectedRows<Row>,
                 control: &mut _|
                 -> Result<bool, E> {
                    if matches!(
                        operator,
                        GlaOperator::CompareProperties { .. } | GlaOperator::SelectBoolean { .. }
                    ) {
                        return compare_properties(operator, bindings, &mut property, control);
                    }
                    Row::collect_properties_with_paths(
                        operator,
                        bindings,
                        paths,
                        projected,
                        &mut property,
                        control,
                    )?;
                    Ok(false)
                },
            ),
        )
    }

    /// Execute scalar properties in their disjoint vertex and edge identity domains.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_with_element_properties_control<'a, E: From<crate::GraphIntegerError>>(
        &self,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
        test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        property: impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalPropertyValue>, E>,
        edge_property: impl FnMut(EId, PropertyKeyId) -> Result<Option<&'a CanonicalPropertyValue>, E>,
        vertex_labels: impl FnMut(VId) -> Result<Option<&'a [crate::algebra::GraphValue]>, E>,
        edge_type: impl FnMut(EId) -> Result<Option<&'a CanonicalScalar>, E>,
        control: impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Vec<Row>, E> {
        if self.projects_property_maps() {
            return Err(crate::GraphIntegerError {
                instruction: 0,
                kind: crate::GraphIntegerErrorKind::PropertyMapSourceRequired,
            }
            .into());
        }
        self.execute_with_element_maps_control(
            vertices,
            edges,
            test_vertex,
            property,
            edge_property,
            vertex_labels,
            edge_type,
            |_| Ok(None),
            |_| Ok(None),
            control,
        )
    }

    /// Enumerating map sources must name every visible property of the same
    /// admitted element. A bound element with no properties supplies an empty
    /// map; only an absent graph binding produces NULL without a source read.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_with_element_maps_control<'a, E: From<crate::GraphIntegerError>>(
        &self,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
        test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        mut property: impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalPropertyValue>, E>,
        mut edge_property: impl FnMut(
            EId,
            PropertyKeyId,
        ) -> Result<Option<&'a CanonicalPropertyValue>, E>,
        mut vertex_labels: impl FnMut(VId) -> Result<Option<&'a [crate::algebra::GraphValue]>, E>,
        mut edge_type: impl FnMut(EId) -> Result<Option<&'a CanonicalScalar>, E>,
        mut vertex_properties: impl FnMut(VId) -> Result<Option<&'a crate::algebra::GraphValue>, E>,
        mut edge_properties: impl FnMut(EId) -> Result<Option<&'a crate::algebra::GraphValue>, E>,
        mut control: impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Vec<Row>, E> {
        if !self.requires_identified_edges()
            && !self.projects_labels()
            && !self.projects_types()
            && !self.projects_property_maps()
        {
            return self.execute_with_properties_control(
                vertices,
                edges.into_iter().map(|(_, s, r, d)| (s, r, d)),
                test_vertex,
                property,
                control,
            );
        }
        let identified = build_identified_index(self.operators(), edges, &mut control)?;
        self.execute_projected_index(
            vertices,
            Index::new(),
            Some(identified),
            test_vertex,
            control,
            Projection::Terminal(
                |operator: &GlaOperator,
                 bindings: &[Option<VId>],
                 paths: &[Option<GraphPath>],
                 projected: &mut ProjectedRows<Row>,
                 control: &mut _|
                 -> Result<bool, E> {
                    if matches!(
                        operator,
                        GlaOperator::CompareProperties { .. } | GlaOperator::SelectBoolean { .. }
                    ) {
                        return compare_element_properties(
                            operator,
                            bindings,
                            paths,
                            &mut property,
                            &mut edge_property,
                            control,
                        );
                    }
                    Row::collect_element_maps(
                        operator,
                        bindings,
                        paths,
                        projected,
                        &mut property,
                        &mut edge_property,
                        &mut vertex_labels,
                        &mut edge_type,
                        &mut vertex_properties,
                        &mut edge_properties,
                        control,
                    )?;
                    Ok(false)
                },
            ),
        )
    }

    /// None means an absent property, not an unreadable source. The resolver's
    /// returned references must remain stable throughout this execution.
    pub fn execute_with_properties_control<'a, E: From<crate::GraphIntegerError>>(
        &self,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
        test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        mut property: impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalPropertyValue>, E>,
        control: impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Vec<Row>, E> {
        if self.projects_property_maps() {
            return Err(crate::GraphIntegerError {
                instruction: 0,
                kind: crate::GraphIntegerErrorKind::PropertyMapSourceRequired,
            }
            .into());
        }
        self.execute_projected(
            vertices,
            edges,
            test_vertex,
            control,
            Projection::Terminal(
                |operator: &GlaOperator,
                 bindings: &[Option<VId>],
                 paths: &[Option<GraphPath>],
                 projected: &mut ProjectedRows<Row>,
                 control: &mut _|
                 -> Result<bool, E> {
                    if matches!(
                        operator,
                        GlaOperator::CompareProperties { .. } | GlaOperator::SelectBoolean { .. }
                    ) {
                        return compare_properties(operator, bindings, &mut property, control);
                    }
                    Row::collect_properties_with_paths(
                        operator,
                        bindings,
                        paths,
                        projected,
                        &mut property,
                        control,
                    )?;
                    Ok(false)
                },
            ),
        )
    }

    /// One evaluator body. The private action handles value-dependent selection
    /// and the sealed terminal projection. Sources, traversal, scope boundaries,
    /// ordering and release remain shared, including streaming aggregation.
    fn execute_projected<E, F, C, P>(
        &self,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
        test_vertex: F,
        mut control: C,
        project: Projection<P>,
    ) -> Result<Vec<Row>, E>
    where
        F: FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        C: FnMut(GlaExecutionEvent) -> Result<(), E>,
        P: FnMut(
            &GlaOperator,
            &[Option<VId>],
            &[Option<GraphPath>],
            &mut ProjectedRows<Row>,
            &mut C,
        ) -> Result<bool, E>,
    {
        if self.requires_identified_edges() {
            return Ok(Vec::new());
        }
        let operators = self.operators();
        let index = if self.reads_edges() {
            build_index(operators, edges, &mut control)?
        } else {
            Index::new()
        };
        self.execute_projected_index(vertices, index, None, test_vertex, control, project)
    }

    fn execute_projected_index<E, F, C, P>(
        &self,
        vertices: impl IntoIterator<Item = VId>,
        index: Index,
        identified_index: Option<IdentifiedIndex>,
        test_vertex: F,
        mut control: C,
        project: Projection<P>,
    ) -> Result<Vec<Row>, E>
    where
        F: FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        C: FnMut(GlaExecutionEvent) -> Result<(), E>,
        P: FnMut(
            &GlaOperator,
            &[Option<VId>],
            &[Option<GraphPath>],
            &mut ProjectedRows<Row>,
            &mut C,
        ) -> Result<bool, E>,
    {
        let operators = self.operators();
        let repeated_vertex_scan = operators
            .iter()
            .skip(1)
            .any(|operator| matches!(operator, GlaOperator::ScanVertices));
        let mut vertices = vertices.into_iter();
        let mut vertex_domain = Vec::new();
        if repeated_vertex_scan {
            for vid in vertices.by_ref() {
                control(GlaExecutionEvent::ScratchEntry)?;
                vertex_domain.push(vid);
            }
        }
        // Only the compiler-owned terminal DISTINCT selects duplicate removal.
        let distinct = matches!(operators.iter().rev().nth(2), Some(GlaOperator::Distinct));
        let (project, terminal) = match project {
            Projection::Terminal(project) => (project, true),
            Projection::Visitor(project) => (project, false),
        };
        let mut execution = Execution {
            test_vertex,
            control,
            project,
            predicate_cache: crate::id_hash::IdMap::default(),
            projected: ProjectedRows::<Row>::for_plan(distinct, operators),
            active_probe: None,
            probe_matches: [false; crate::algebra::MAX_PATTERN_IDENTITIES],
            optional_matches: [false; crate::algebra::MAX_PATTERN_IDENTITIES],
            vertex_domain,
            identified_index: identified_index.as_ref(),
            segments: core::array::from_fn(|_| None),
            paths: core::array::from_fn(|_| None),
            continuations: Vec::with_capacity(operators.len()),
            terminal_closure: if terminal && identified_index.is_none() {
                join::TerminalClosure::compile(operators)
            } else {
                None
            },
        };
        let mut bindings = Vec::new();
        match operators.first() {
            Some(GlaOperator::ScanVertices) if repeated_vertex_scan => {
                execution.visit(operators, 0, &mut bindings, &index)?;
            }
            Some(GlaOperator::ScanVertices) => {
                for vid in vertices {
                    (execution.control)(GlaExecutionEvent::Work)?;
                    bindings.clear();
                    bindings.push(Some(vid));
                    execution.visit(operators, 1, &mut bindings, &index)?;
                }
            }
            Some(GlaOperator::ScanEdges {
                relation,
                direction,
            }) => {
                if let Some(adjacency) = identified_index
                    .as_ref()
                    .and_then(|index| index.get(&(*relation, *direction)))
                {
                    for (source, neighbors) in adjacency.iter() {
                        for &(edge, destination) in neighbors {
                            (execution.control)(GlaExecutionEvent::Work)?;
                            for _ in 0..3 {
                                (execution.control)(GlaExecutionEvent::ScratchEntry)?;
                            }
                            execution.segments[1] = Some(GraphPath::new(
                                source,
                                vec![(edge, destination)].into_boxed_slice(),
                            ));
                            bindings.clear();
                            bindings.extend([Some(source), Some(destination)]);
                            execution.visit(operators, 1, &mut bindings, &index)?;
                            execution.segments[1] = None;
                        }
                    }
                }
                if let Some(adjacency) = index.get(&(*relation, *direction)) {
                    for (source, destinations) in adjacency {
                        let destinations = join::bound_neighbors(
                            operators.get(1),
                            1,
                            &[Some(*source)],
                            destinations,
                            &mut execution.control,
                        )?;
                        for destination in destinations {
                            (execution.control)(GlaExecutionEvent::Work)?;
                            bindings.clear();
                            bindings.extend([Some(*source), Some(*destination)]);
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
            value.push(row.retain_visible(self.visible_columns));
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
                    predicate.matches(&[], &[(PropertyKeyId(4), CanonicalScalar::Int(7).into())])
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
        let mut edges = Vec::new();
        plan.limit = Some(1);
        let logical = GlaPlan::lower(&plan);
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
    fn an_ascending_neighbor_list_is_checked_not_sorted() {
        for len in 0..=40_u128 {
            // Repeated neighbors (parallel edges) are still ascending.
            let values: Vec<_> = (0..len).map(|at| VId(at / 2)).collect();
            let mut checked = values.clone();
            let mut events = 0;
            sort_neighbors(&mut checked, &mut |event| {
                assert_eq!(event, GlaExecutionEvent::Work);
                events += 1;
                Ok::<_, ()>(())
            })
            .unwrap();
            assert_eq!(checked, values);
            assert_eq!(events, len.saturating_sub(1));

            let values: Vec<_> = (0..len).map(|at| (EId(at), VId(len - at))).collect();
            let mut checked = values.clone();
            let mut events = 0;
            sort_identified_neighbors(&mut checked, &mut |_| {
                events += 1;
                Ok::<_, ()>(())
            })
            .unwrap();
            assert_eq!(checked, values);
            assert_eq!(events, len.saturating_sub(1));
        }
        // One descent anywhere still sorts the whole list.
        let mut values = vec![VId(1), VId(2), VId(3), VId(0)];
        sort_neighbors(&mut values, &mut |_| Ok::<_, ()>(())).unwrap();
        assert_eq!(values, [VId(0), VId(1), VId(2), VId(3)]);
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

    type OrderedIdentifiedIndex =
        BTreeMap<(EdgeRelation, GlaDirection), BTreeMap<VId, Vec<(EId, VId)>>>;

    /// The identified index the flat one replaced (fgdb-61p1n): a staging map
    /// of vectors per key, collected into ordered maps, then each list sorted
    /// in key order. Its lists, their order and its events are the oracle.
    fn ordered_identified_index<E>(
        operators: &[GlaOperator],
        edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<OrderedIdentifiedIndex, E> {
        type Stage = crate::id_hash::IdMap<VId, Vec<(EId, VId)>>;
        fn push<E>(
            stage: &mut Stage,
            from: VId,
            neighbor: (EId, VId),
            control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        ) -> Result<(), E> {
            match stage.entry(from) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    entry.get_mut().push(neighbor)
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    entry.insert(vec![neighbor]);
                }
            }
            Ok(())
        }
        let mut index = OrderedIdentifiedIndex::new();
        for operator in operators {
            let GlaOperator::ScanEdges {
                relation,
                direction,
            } = operator
            else {
                continue;
            };
            if let std::collections::btree_map::Entry::Vacant(e) =
                index.entry((*relation, *direction))
            {
                control(GlaExecutionEvent::ScratchEntry)?;
                e.insert(BTreeMap::new());
            }
        }
        let alternations = alternation_selectors(index.keys().map(|(relation, _)| *relation));
        let keys: Vec<_> = index.keys().copied().collect();
        let mut staged: Vec<Stage> = keys.iter().map(|_| Stage::default()).collect();
        for (edge, source, relation, destination) in edges {
            control(GlaExecutionEvent::Work)?;
            for direction in [
                GlaDirection::Forward,
                GlaDirection::Reverse,
                GlaDirection::Undirected,
            ] {
                for selector in edge_selectors(relation, &alternations) {
                    let Ok(at) = keys.binary_search(&(selector, direction)) else {
                        continue;
                    };
                    let (from, to) = if direction == GlaDirection::Reverse {
                        (destination, source)
                    } else {
                        (source, destination)
                    };
                    control(GlaExecutionEvent::ScratchEntry)?;
                    push(&mut staged[at], from, (edge, to), control)?;
                    if direction == GlaDirection::Undirected && source != destination {
                        control(GlaExecutionEvent::ScratchEntry)?;
                        push(&mut staged[at], destination, (edge, source), control)?;
                    }
                }
            }
        }
        for (key, stage) in keys.into_iter().zip(staged) {
            let mut entries: Vec<_> = stage.into_iter().collect();
            entries.sort_unstable_by_key(|(from, _)| *from);
            index.insert(key, entries.into_iter().collect());
        }
        for adjacency in index.values_mut() {
            for neighbors in adjacency.values_mut() {
                sort_identified_neighbors(neighbors, control)?;
            }
        }
        Ok(index)
    }

    /// The flat identified index holds exactly the ordered one's lists, in
    /// its order, and charges exactly its events (fgdb-61p1n): random edges
    /// with self-loops, parallel edges and three relations, into random key
    /// sets of one type, an alternation and the untyped selector, in every
    /// orientation, edges arriving in no endpoint order.
    #[test]
    fn flat_identified_index_equals_the_ordered_one_event_for_event() {
        let alternation = EdgeRelation::alternation(&[RelationId(1), RelationId(2)]).unwrap();
        let selectors = [
            EdgeRelation::One(RelationId(1)),
            EdgeRelation::One(RelationId(2)),
            alternation,
            EdgeRelation::Any,
        ];
        let directions = [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ];
        let mut state = 0x1de7_1f1e_d000_0001_u64;
        let mut random = |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };
        let (mut checked, mut listed) = (0, 0);
        for _ in 0..300 {
            let operators: Vec<GlaOperator> = (0..1 + random(5))
                .map(|_| GlaOperator::ScanEdges {
                    relation: selectors[random(4) as usize],
                    direction: directions[random(3) as usize],
                })
                .collect();
            let vertices = 1 + random(25);
            let edges: Vec<(EId, VId, RelationId, VId)> = (0..random(120))
                .map(|eid| {
                    let vertex = |at: u64| VId(u128::from(at) * 0x9_0000_0007 % 1_000_003);
                    (
                        EId((u128::from(random(1 << 30)) << 8) | u128::from(eid)),
                        vertex(random(vertices)),
                        RelationId(1 + random(3)),
                        vertex(random(vertices)),
                    )
                })
                .collect();
            let (mut flat_log, mut ordered_log) = (Vec::new(), Vec::new());
            let flat = build_identified_index(&operators, edges.iter().copied(), &mut |event| {
                flat_log.push(event);
                Ok::<_, ()>(())
            })
            .unwrap();
            let ordered =
                ordered_identified_index(&operators, edges.iter().copied(), &mut |event| {
                    ordered_log.push(event);
                    Ok::<_, ()>(())
                })
                .unwrap();
            assert_eq!(flat_log, ordered_log);
            assert_eq!(
                flat.keys().collect::<Vec<_>>(),
                ordered.keys().collect::<Vec<_>>()
            );
            for (key, adjacency) in &flat {
                let lists: Vec<(VId, Vec<(EId, VId)>)> = adjacency
                    .iter()
                    .map(|(endpoint, list)| (endpoint, list.to_vec()))
                    .collect();
                let expected: Vec<_> = ordered[key].clone().into_iter().collect();
                assert_eq!(lists, expected, "{key:?}");
                for probe in 0..vertices + 2 {
                    let vertex = VId(u128::from(probe) * 0x9_0000_0007 % 1_000_003);
                    assert_eq!(
                        adjacency.neighbors(&vertex),
                        ordered[key].neighbors(&vertex)
                    );
                }
                listed += lists.len();
            }
            checked += 1;
        }
        assert_eq!(checked, 300);
        assert!(listed > 3_000, "{listed}");
    }
}
