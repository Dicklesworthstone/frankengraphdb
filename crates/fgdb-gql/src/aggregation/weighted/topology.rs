//! The weighted topology a statement's atoms read: each distinct
//! `(source, relation, destination)` key with its bag multiplicity.
//!
//! Both builders emit ONE event sequence, in arrival order: Work for an edge,
//! that edge's endpoint tests, then one ScratchEntry at the first arrival of
//! each retained key. `online` keeps the ordered map current as edges arrive,
//! one tree descent per edge at an unrelated key. `sorted` learns each key's
//! first arrival and multiplicity from one sort, replays the same events, and
//! fills the map in ascending order. On a whole-graph statement the descents
//! were the evaluator's largest cost (29% of a 508k-edge count on fgdbd).

use super::*;

/// Endpoint predicate verdicts, cached per (side, relation, vertex).
struct Endpoints<'a> {
    filters: Option<&'a EndpointFilters>,
    tested: BTreeMap<(bool, RelationId, VId), bool>,
}

impl Endpoints<'_> {
    /// Whether both endpoints pass their relation's predicates. A vertex is
    /// tested once per side; its verdict costs one ScratchEntry.
    fn admit<E, C>(
        &mut self,
        (source, relation, destination): TopologyKey,
        test_vertex: &mut impl FnMut(VId, &[VertexPredicate]) -> Result<bool, VisitError<E, C>>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
    ) -> Result<bool, VisitError<E, C>> {
        let Some((from, to)) = self.filters.and_then(|filters| filters.get(&relation)) else {
            return Ok(true);
        };
        let mut passes = |at_source: bool, vertex: VId, predicates: &[VertexPredicate]| {
            if predicates.is_empty() {
                return Ok(true);
            }
            if let Some(&known) = self.tested.get(&(at_source, relation, vertex)) {
                return Ok(known);
            }
            let result = test_vertex(vertex, predicates)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            self.tested.insert((at_source, relation, vertex), result);
            Ok::<_, VisitError<E, C>>(result)
        };
        Ok(passes(true, source, from)? && passes(false, destination, to)?)
    }
}

/// Weigh `edges`. Endpoint filters exist only for directed atoms, and a
/// directed key is its edge's own triple, which `sorted` relies on.
pub(super) fn weigh<E, C>(
    edges: impl IntoIterator<Item = TopologyKey>,
    undirected: &BTreeSet<RelationId>,
    filters: Option<&EndpointFilters>,
    test_vertex: &mut impl FnMut(VId, &[VertexPredicate]) -> Result<bool, VisitError<E, C>>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
) -> Result<Topology, VisitError<E, C>> {
    if undirected.is_empty() {
        sorted(edges, filters, test_vertex, control)
    } else {
        online(edges, undirected, filters, test_vertex, control)
    }
}

fn online<E, C>(
    edges: impl IntoIterator<Item = TopologyKey>,
    undirected: &BTreeSet<RelationId>,
    filters: Option<&EndpointFilters>,
    test_vertex: &mut impl FnMut(VId, &[VertexPredicate]) -> Result<bool, VisitError<E, C>>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
) -> Result<Topology, VisitError<E, C>> {
    let mut endpoints = Endpoints {
        filters,
        tested: BTreeMap::new(),
    };
    let mut topology = BTreeMap::<TopologyKey, Multiplicity>::new();
    for (source, relation, destination) in edges {
        control(GlaExecutionEvent::Work)?;
        if !endpoints.admit((source, relation, destination), test_vertex, control)? {
            continue;
        }
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
    Ok(topology.into())
}

/// Every key is its edge's own triple, so the edges of one key share one
/// endpoint verdict: a key is retained exactly when its first arrival passes.
/// The sort copy (and, under filters, the arrival copy) is transient scratch
/// of the same order as the map it builds, released before the map is read.
fn sorted<E, C>(
    edges: impl IntoIterator<Item = TopologyKey>,
    filters: Option<&EndpointFilters>,
    test_vertex: &mut impl FnMut(VId, &[VertexPredicate]) -> Result<bool, VisitError<E, C>>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
) -> Result<Topology, VisitError<E, C>> {
    // The arrival ordinal breaks ties: each key's run starts at its first
    // arrival, and the run's length is its multiplicity.
    let mut runs: Vec<(VId, RelationId, VId, usize)> = edges
        .into_iter()
        .enumerate()
        .map(|(at, (source, relation, destination))| (source, relation, destination, at))
        .collect();
    // Endpoint tests replay in arrival order, which the sort discards.
    let arrivals: Vec<TopologyKey> = if filters.is_some() {
        runs.iter()
            .map(|&(source, relation, destination, _)| (source, relation, destination))
            .collect()
    } else {
        Vec::new()
    };
    runs.sort_unstable();
    let mut first = vec![false; runs.len()];
    let mut previous = None;
    for &(source, relation, destination, at) in &runs {
        let key = (source, relation, destination);
        if previous != Some(key) {
            first[at] = true;
            previous = Some(key);
        }
    }
    let mut verdicts = filters.map(|filters| Verdicts::new(filters, &runs, &arrivals));
    for at in 0..runs.len() {
        control(GlaExecutionEvent::Work)?;
        if let Some(verdicts) = &mut verdicts
            && !verdicts.admit(at, arrivals[at], test_vertex, control)?
        {
            // A first arrival that fails rejects its whole key.
            first[at] = false;
            continue;
        }
        if first[at] {
            // One key/weight entry, as in `online`.
            control(GlaExecutionEvent::ScratchEntry)?;
        }
    }
    let mut topology = Vec::new();
    let mut start = 0;
    while start < runs.len() {
        let (source, relation, destination, arrival) = runs[start];
        let mut end = start + 1;
        while end < runs.len()
            && (runs[end].0, runs[end].1, runs[end].2) == (source, relation, destination)
        {
            end += 1;
        }
        if first[arrival] {
            topology.push((
                (source, relation, destination),
                Multiplicity(NonZeroU64::new((end - start) as u64)),
            ));
        }
        start = end;
    }
    // Runs are sorted and each key's run is one entry, so keys ascend.
    Ok(Topology::from_sorted(topology))
}

/// `sorted` over a positioned graph's edges, event for event: the same
/// arrival order, the same endpoint tests at the same first arrivals and the
/// same entry per retained key. Positions order exactly as identities, and
/// relation positions as relations, so the 16-byte position runs sort into
/// the order of `sorted`'s 48-byte identity runs; endpoint verdicts are kept
/// per (side, relation, position) in arrays. Every kept key also records its
/// endpoint positions. `None` when arrival ordinals do not fit 32 bits.
pub(super) fn weigh_positioned<T, E, C>(
    graph: &PositionedGraph<'_, T>,
    filters: Option<&EndpointFilters>,
    test_at: &mut impl FnMut(u32, &[VertexPredicate]) -> Result<bool, VisitError<E, C>>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
) -> Option<Result<Topology, VisitError<E, C>>> {
    let edges = graph.edges();
    u32::try_from(edges.len()).ok()?;
    let mut runs: Vec<(u32, u32, u32, u32)> = edges
        .iter()
        .enumerate()
        .map(|(at, edge)| (edge.source, edge.relation, edge.destination, at as u32))
        .collect();
    runs.sort_unstable();
    let mut first = vec![false; runs.len()];
    let mut previous = None;
    for &(source, relation, destination, at) in &runs {
        let key = (source, relation, destination);
        if previous != Some(key) {
            first[at as usize] = true;
            previous = Some(key);
        }
    }
    // Each relation's predicates on each side, by relation position, and
    // their verdicts by vertex position: 0 untested, 1 fails, 2 passes.
    let relations = graph.relations();
    let mut sides: Vec<[(&[VertexPredicate], Vec<u8>); 2]> = relations
        .iter()
        .map(|relation| {
            let (from, to) = filters
                .and_then(|filters| filters.get(relation))
                .map_or((&[][..], &[][..]), |(from, to)| {
                    (from.as_slice(), to.as_slice())
                });
            let table = |predicates: &[VertexPredicate]| {
                if predicates.is_empty() {
                    Vec::new()
                } else {
                    vec![0_u8; graph.len()]
                }
            };
            [(from, table(from)), (to, table(to))]
        })
        .collect();
    let filtered: Vec<bool> = relations
        .iter()
        .map(|relation| filters.is_some_and(|filters| filters.contains_key(relation)))
        .collect();
    let result = (|| {
        for (at, edge) in edges.iter().enumerate() {
            control(GlaExecutionEvent::Work)?;
            if filtered[edge.relation as usize] {
                let [source, destination] = &mut sides[edge.relation as usize];
                // Both sides under one predicate list test a vertex alike.
                // The readers are pure, so a side's first test of a vertex
                // the other side has already tested reuses that answer; the
                // side still charges its own entry, as `sorted` does.
                let shared = source.0 == destination.0;
                let mut passes =
                    |own: &mut (&[VertexPredicate], Vec<u8>), other: &[u8], vertex: u32| {
                        let (predicates, verdicts) = own;
                        if predicates.is_empty() {
                            return Ok(true);
                        }
                        match verdicts[vertex as usize] {
                            0 => {
                                let known = if shared { other[vertex as usize] } else { 0 };
                                let result = match known {
                                    0 => test_at(vertex, predicates)?,
                                    known => known == 2,
                                };
                                control(GlaExecutionEvent::ScratchEntry)?;
                                verdicts[vertex as usize] = 1 + u8::from(result);
                                Ok::<_, VisitError<E, C>>(result)
                            }
                            known => Ok(known == 2),
                        }
                    };
                if !(passes(source, &destination.1, edge.source)?
                    && passes(destination, &source.1, edge.destination)?)
                {
                    // A first arrival that fails rejects its whole key.
                    first[at] = false;
                    continue;
                }
            }
            if first[at] {
                // One key/weight entry, as in `online`.
                control(GlaExecutionEvent::ScratchEntry)?;
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        return Some(Err(error));
    }
    drop(sides);
    let mut topology = Vec::new();
    let mut positions = Vec::new();
    let mut start = 0;
    while start < runs.len() {
        let (source, relation, destination, arrival) = runs[start];
        let mut end = start + 1;
        while end < runs.len()
            && (runs[end].0, runs[end].1, runs[end].2) == (source, relation, destination)
        {
            end += 1;
        }
        if first[arrival as usize] {
            topology.push((
                (
                    graph.vertex(source),
                    relations[relation as usize],
                    graph.vertex(destination),
                ),
                Multiplicity(NonZeroU64::new((end - start) as u64)),
            ));
            positions.push([source, destination]);
        }
        start = end;
    }
    Some(Ok(Topology::from_sorted_positioned(topology, positions)))
}

/// Endpoint verdicts for `sorted`, one per (vertex, relation) group and side:
/// exactly what `Endpoints` caches per (side, relation, vertex), without a
/// tree search per edge (22% of a 730k-edge grouped count with a source
/// label). The key sort already makes each source group contiguous; one more
/// sort numbers the destination groups. A group is tested at its first
/// arrival that reaches it, so tests and their entries keep their order.
struct Verdicts<'a> {
    filters: &'a EndpointFilters,
    /// The group of each arrival's source, then of its destination.
    source: Vec<usize>,
    destination: Vec<usize>,
    sources: Vec<Option<bool>>,
    destinations: Vec<Option<bool>>,
}

impl<'a> Verdicts<'a> {
    fn new(
        filters: &'a EndpointFilters,
        runs: &[(VId, RelationId, VId, usize)],
        arrivals: &[TopologyKey],
    ) -> Self {
        let tests = |side: fn(&(Vec<VertexPredicate>, Vec<VertexPredicate>)) -> bool| {
            filters.values().any(side)
        };
        let (mut source, mut sources) = (Vec::new(), Vec::new());
        if tests(|(from, _)| !from.is_empty()) {
            source = vec![0; runs.len()];
            let mut previous = None;
            for &(vertex, relation, _, at) in runs {
                if previous != Some((vertex, relation)) {
                    previous = Some((vertex, relation));
                    sources.push(None);
                }
                source[at] = sources.len() - 1;
            }
        }
        let (mut destination, mut destinations) = (Vec::new(), Vec::new());
        if tests(|(_, to)| !to.is_empty()) {
            let mut order: Vec<(VId, RelationId, usize)> = arrivals
                .iter()
                .enumerate()
                .map(|(at, &(_, relation, vertex))| (vertex, relation, at))
                .collect();
            order.sort_unstable();
            destination = vec![0; arrivals.len()];
            let mut previous = None;
            for (vertex, relation, at) in order {
                if previous != Some((vertex, relation)) {
                    previous = Some((vertex, relation));
                    destinations.push(None);
                }
                destination[at] = destinations.len() - 1;
            }
        }
        Self {
            filters,
            source,
            destination,
            sources,
            destinations,
        }
    }

    /// `Endpoints::admit` for arrival `at`: the same tests, in the same order.
    fn admit<E, C>(
        &mut self,
        at: usize,
        (source, relation, destination): TopologyKey,
        test_vertex: &mut impl FnMut(VId, &[VertexPredicate]) -> Result<bool, VisitError<E, C>>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
    ) -> Result<bool, VisitError<E, C>> {
        let Some((from, to)) = self.filters.get(&relation) else {
            return Ok(true);
        };
        let mut passes =
            |verdict: &mut Option<bool>, vertex: VId, predicates: &[VertexPredicate]| {
                if let Some(known) = *verdict {
                    return Ok(known);
                }
                let result = test_vertex(vertex, predicates)?;
                control(GlaExecutionEvent::ScratchEntry)?;
                *verdict = Some(result);
                Ok::<_, VisitError<E, C>>(result)
            };
        if !from.is_empty() && !passes(&mut self.sources[self.source[at]], source, from)? {
            return Ok(false);
        }
        Ok(to.is_empty()
            || passes(
                &mut self.destinations[self.destination[at]],
                destination,
                to,
            )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;
    use fgdb_delta_types::LabelId;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Step {
        Event(GlaExecutionEvent),
        Test(VId),
    }
    type Refusal = VisitError<core::convert::Infallible, &'static str>;
    type Outcome = (Option<Vec<(TopologyKey, Option<u64>)>>, Vec<Step>);

    /// Run one builder to completion, or stop it at its `stop`th step. The
    /// trace is every event and endpoint test, in order.
    fn run(
        sorts: bool,
        edges: &[TopologyKey],
        filters: Option<&EndpointFilters>,
        stop: usize,
    ) -> Outcome {
        let trace = RefCell::new(Vec::new());
        let step = |step: Step| -> Result<(), Refusal> {
            trace.borrow_mut().push(step);
            if trace.borrow().len() == stop {
                return Err(GqlQueryError::Interrupted("stop"));
            }
            Ok(())
        };
        let mut test_vertex = |vertex: VId, _: &[VertexPredicate]| {
            step(Step::Test(vertex))?;
            Ok(!vertex.0.is_multiple_of(3))
        };
        let mut control = |event| step(Step::Event(event));
        let edges = edges.iter().copied();
        let built = if sorts {
            sorted(edges, filters, &mut test_vertex, &mut control)
        } else {
            online(
                edges,
                &BTreeSet::new(),
                filters,
                &mut test_vertex,
                &mut control,
            )
        };
        let built = match built {
            Ok(topology) => Some(
                topology
                    .into_iter()
                    .map(|(key, weight)| (key, weight.exact_count()))
                    .collect(),
            ),
            Err(GqlQueryError::Interrupted("stop")) => None,
            Err(_) => panic!("only the injected stop refuses"),
        };
        (built, trace.into_inner())
    }

    /// A directed multigraph over a few vertices and two relations, so keys
    /// repeat, arrive out of key order and fail endpoint tests.
    fn multigraph(edges: usize, seed: u64) -> Vec<TopologyKey> {
        let mut x = seed | 1;
        let mut next = |bound: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % bound
        };
        (0..edges)
            .map(|_| {
                (
                    VId(u128::from(next(9))),
                    RelationId(1 + next(2)),
                    VId(u128::from(next(9))),
                )
            })
            .collect()
    }

    fn filters() -> EndpointFilters {
        let label = vec![VertexPredicate::HasLabel(LabelId(1))];
        // Relation 1 tests both endpoints, relation 2 only its destination.
        BTreeMap::from([
            (RelationId(1), (label.clone(), label.clone())),
            (RelationId(2), (Vec::new(), label)),
        ])
    }

    #[test]
    fn the_sorted_build_is_the_online_build_event_for_event() {
        let filters = filters();
        for (edges, seed) in [(0, 1), (1, 2), (2, 3), (40, 4), (300, 5), (300, 6)] {
            let edges = multigraph(edges, seed);
            for filters in [None, Some(&filters)] {
                let expected = run(false, &edges, filters, 0);
                assert_eq!(run(true, &edges, filters, 0), expected);
                let (topology, trace) = expected;
                // Every arrival is weighed unless its key fails an endpoint.
                let weighed: u64 = topology
                    .unwrap()
                    .iter()
                    .map(|(_, weight)| weight.unwrap())
                    .sum();
                if filters.is_none() {
                    assert_eq!(weighed, edges.len() as u64);
                } else if edges.len() >= 40 {
                    assert!(0 < weighed && weighed < edges.len() as u64);
                }
                // Stopped at any step, both refuse there having done the same.
                for stop in 1..=trace.len() {
                    let stopped = run(false, &edges, filters, stop);
                    assert_eq!(stopped, (None, trace[..stop].to_vec()));
                    assert_eq!(run(true, &edges, filters, stop), stopped, "stop={stop}");
                }
            }
        }
    }

    #[test]
    fn a_key_weighs_its_arrivals_and_costs_one_entry_at_the_first() {
        let key = |source: u128, destination: u128| (VId(source), RelationId(1), VId(destination));
        let edges = [key(5, 1), key(2, 1), key(5, 1), key(2, 4), key(5, 1)];
        let (topology, trace) = run(true, &edges, None, 0);
        assert_eq!(
            topology.unwrap(),
            [
                (key(2, 1), Some(1)),
                (key(2, 4), Some(1)),
                (key(5, 1), Some(3))
            ]
        );
        use GlaExecutionEvent::{ScratchEntry, Work};
        assert_eq!(
            trace,
            [
                Work,
                ScratchEntry,
                Work,
                ScratchEntry,
                Work,
                Work,
                ScratchEntry,
                Work
            ]
            .map(Step::Event)
        );
    }
}
