//! Bounded graph expansion for Beacon's graph candidate lane.
//! Resident adjacency is a disposable query structure, never storage or an
//! authorization grant. Cursor sources can instead supply admitted neighbors
//! on demand. Both use the same multi-source BFS and canonical rank reduction.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque};

use fgdb_types::VId;

use crate::{BeaconError, WorkControl};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpansionDirection {
    Outgoing,
    Incoming,
    Undirected,
}

/// Independent resident limits. Zero means zero. Resident input edges count
/// admitted logical edges BEFORE parallel-edge collapse. An indexed adapter
/// may instead bound admitted incidence visits; that profile must say so.
/// Source scratch bounds the adapter's admission events, not process RSS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExpansionLimits {
    pub max_vertices: usize,
    pub max_input_edges: usize,
    pub max_visited_vertices: usize,
    pub max_seed_ids: usize,
    pub max_source_scratch: usize,
}

impl Default for ExpansionLimits {
    fn default() -> Self {
        Self {
            max_vertices: 100_000,
            max_input_edges: 1_000_000,
            max_visited_vertices: 100_000,
            max_seed_ids: 10_000,
            max_source_scratch: 1_000_000,
        }
    }
}

/// Host adapter instantiates R with its native relation key. Seeds are explicit
/// IDs, not fabricated vertices. Hidden, missing and out-of-selection seeds
/// all behave as absent. Vertex selection also constrains every transit node.
#[derive(Clone, Copy, Debug)]
pub struct ExpansionSpec<'a, R> {
    pub seeds: &'a [VId],
    pub relation: Option<R>,
    pub direction: ExpansionDirection,
    pub max_hops: u32,
    pub include_seeds: bool,
    pub limits: ExpansionLimits,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphHit {
    pub id: VId,
    pub hops: u32,
}

/// A pinned, already-authorized adjacency source. The source owns historical
/// winner selection, relation/direction filtering and BOTH endpoint checks.
/// Every scalable scan must charge or poll its host control BEFORE observation
/// or allocation. Invoke `visit` with the SAME work control; never refresh a
/// budget for each vertex. A source must not swallow visitor/control failures.
///
/// Only reached vertices below the hop bound are requested. Neighbor order and
/// parallel occurrences do not affect minimum distances or canonical ranking.
/// This interface confers no graph authority, durability, or memory/I/O bound.
pub trait ExpansionNeighbors {
    fn visit_neighbors(
        &mut self,
        vertex: VId,
        work: &mut dyn WorkControl,
        visit: &mut dyn FnMut(VId, &mut dyn WorkControl) -> Result<(), BeaconError>,
    ) -> Result<(), BeaconError>;
}

/// Query-private selected corpus and optional ordered adjacency. Mutations fail
/// closed: after an error or unwinding insertion, expansion refuses until a
/// NEW builder is created. A cancelled read does not mutate this structure.
pub struct ExpansionGraph {
    vertices: BTreeSet<VId>,
    arcs: BTreeSet<(VId, VId)>,
    input_edges: usize,
    limits: ExpansionLimits,
    failure: Option<BeaconError>,
}

fn tree_work(entries: usize) -> usize {
    1 + entries.checked_ilog2().unwrap_or(0) as usize
}

struct ResidentNeighbors<'a>(&'a BTreeSet<(VId, VId)>);

impl ExpansionNeighbors for ResidentNeighbors<'_> {
    fn visit_neighbors(
        &mut self,
        vertex: VId,
        work: &mut dyn WorkControl,
        visit: &mut dyn FnMut(VId, &mut dyn WorkControl) -> Result<(), BeaconError>,
    ) -> Result<(), BeaconError> {
        work.charge(tree_work(self.0.len()))?;
        for &(from, next) in self.0.range((vertex, VId(0))..) {
            if from != vertex {
                break;
            }
            visit(next, work)?;
        }
        Ok(())
    }
}

impl ExpansionGraph {
    #[must_use]
    pub fn new(limits: ExpansionLimits) -> Self {
        Self {
            vertices: BTreeSet::new(),
            arcs: BTreeSet::new(),
            input_edges: 0,
            limits,
            failure: None,
        }
    }

    fn mutate(
        &mut self,
        change: impl FnOnce(&mut Self) -> Result<(), BeaconError>,
    ) -> Result<(), BeaconError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        self.failure = Some(BeaconError::Invariant("unfinished expansion mutation"));
        let result = change(self);
        self.failure = result.as_ref().err().cloned();
        result
    }

    pub fn insert_vertex(
        &mut self,
        id: VId,
        work: &mut dyn WorkControl,
    ) -> Result<(), BeaconError> {
        self.mutate(|graph| {
            work.charge(tree_work(graph.vertices.len()))?;
            if graph.vertices.contains(&id) {
                return Ok(());
            }
            if graph.vertices.len() >= graph.limits.max_vertices {
                return Err(BeaconError::ResourceLimit {
                    resource: "expansion vertices",
                    limit: graph.limits.max_vertices,
                });
            }
            graph.vertices.insert(id);
            Ok(())
        })
    }

    /// A directory probe, not a capability verifier. Adapters charge their
    /// source checkpoint before probing this bounded selected-vertex set.
    #[must_use]
    pub fn contains(&self, id: VId) -> bool {
        self.vertices.contains(&id)
    }

    pub fn insert_edge(
        &mut self,
        source: VId,
        target: VId,
        direction: ExpansionDirection,
        work: &mut dyn WorkControl,
    ) -> Result<(), BeaconError> {
        self.mutate(|graph| {
            work.charge(2 * tree_work(graph.vertices.len()))?;
            if !graph.contains(source) || !graph.contains(target) {
                return Err(BeaconError::InvalidQuery(
                    "expansion endpoint outside selected corpus",
                ));
            }
            if graph.input_edges >= graph.limits.max_input_edges {
                return Err(BeaconError::ResourceLimit {
                    resource: "expansion input edges",
                    limit: graph.limits.max_input_edges,
                });
            }
            graph.input_edges += 1;
            for (from, to, enabled) in [
                (source, target, direction != ExpansionDirection::Incoming),
                (target, source, direction != ExpansionDirection::Outgoing),
            ] {
                if enabled {
                    work.charge(tree_work(graph.arcs.len()))?;
                    graph.arcs.insert((from, to));
                }
            }
            Ok(())
        })
    }

    /// Multi-source unit-distance BFS over the admitted resident arcs. Uses the
    /// same distance/visited/ranking body as on-demand sources below.
    pub fn expand(
        &self,
        seeds: &[VId],
        max_hops: u32,
        include_seeds: bool,
        candidates: u32,
        work: &mut dyn WorkControl,
    ) -> Result<Vec<GraphHit>, BeaconError> {
        self.expand_from_source(
            seeds,
            max_hops,
            include_seeds,
            candidates,
            &mut ResidentNeighbors(&self.arcs),
            work,
        )
    }

    /// Explore a source without constructing resident adjacency. This object's
    /// selected vertex corpus still governs seeds and every transit vertex;
    /// its stored arcs are NOT consulted. An out-of-corpus neighbor is a source
    /// error, not a post-filtered candidate. Sources must filter hidden vertices
    /// before invoking the visitor, so even error/work traces cannot reveal them.
    ///
    /// Duplicate seeds/paths, parallel edges and cycles contribute a vertex
    /// once, at MINIMUM hop distance. Rank by (hops, full-width VId). Candidate
    /// truncation follows COMPLETE bounded exploration: k=1 cannot mask a late
    /// source, work or visited refusal. Zero candidates make no adjacency calls.
    /// Zero hops visits selected seeds only; neither that nor an absent seed
    /// causes adjacency demand. No successful partial population escapes.
    ///
    /// Retains O(selected vertices + visited vertices + candidates), not edges.
    /// Actual source residency is separate. This is not a spill implementation.
    pub fn expand_from_source(
        &self,
        seeds: &[VId],
        max_hops: u32,
        include_seeds: bool,
        candidates: u32,
        source: &mut impl ExpansionNeighbors,
        work: &mut dyn WorkControl,
    ) -> Result<Vec<GraphHit>, BeaconError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        expand_controlled(
            seeds, max_hops, include_seeds, candidates, self.limits,
            &mut |id, entries, work| {
                // Preserve the resident profile's existing combined charge.
                work.charge(tree_work(self.vertices.len()) + tree_work(entries))?;
                Ok(self.contains(id))
            },
            source, work,
        )
    }
}

/// Membership in one immutable, already-admitted graph selection. A host may
/// resolve just this vertex through its pinned historical index rather than
/// constructing a complete selected-ID directory. Charge/poll the SAME control
/// before observation, traversal or allocation. Values must not change during
/// an expansion, and membership and adjacency must name the SAME logical cut.
/// Neither this trait nor the expansion kernel authenticates that contract.
pub trait ExpansionMembership {
    fn contains_vertex(
        &self,
        vertex: VId,
        work: &mut dyn WorkControl,
    ) -> Result<bool, BeaconError>;
}

impl ExpansionMembership for ExpansionGraph {
    fn contains_vertex(
        &self,
        vertex: VId,
        work: &mut dyn WorkControl,
    ) -> Result<bool, BeaconError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        work.charge(tree_work(self.vertices.len()))?;
        Ok(self.contains(vertex))
    }
}

/// The same complete BFS/ranking kernel with demand-driven vertex membership.
/// No full vertex directory or adjacency is constructed. Absent seeds are
/// skipped; an out-of-selection neighbor is a source error, never a filtered
/// successful prefix. Neighbor sources must mask BOTH endpoints before calling
/// their visitor. A swallowed visitor/membership failure remains terminal.
///
/// This profile bounds retained visits by min(max_vertices, max_visited_vertices)
/// and checks max_seed_ids before demand. It does NOT count the full selected
/// population. The host owns incidence and source-scratch limits; all work
/// shares the supplied control. Zero candidates demand no membership/adjacency;
/// one candidate still requires complete bounded exploration. State is resident
/// O(visited vertices + candidates), excluding the host's source representation.
#[allow(clippy::too_many_arguments)]
pub fn expand_from_membership(
    seeds: &[VId],
    max_hops: u32,
    include_seeds: bool,
    candidates: u32,
    limits: ExpansionLimits,
    membership: &impl ExpansionMembership,
    source: &mut impl ExpansionNeighbors,
    work: &mut dyn WorkControl,
) -> Result<Vec<GraphHit>, BeaconError> {
    expand_controlled(
        seeds, max_hops, include_seeds, candidates, limits,
        &mut |id, entries, work| {
            work.charge(tree_work(entries))?;
            membership.contains_vertex(id, work)
        },
        source, work,
    )
}

#[allow(clippy::too_many_arguments)]
fn expand_controlled(
    seeds: &[VId],
    max_hops: u32,
    include_seeds: bool,
    candidates: u32,
    limits: ExpansionLimits,
    contains: &mut impl FnMut(VId, usize, &mut dyn WorkControl) -> Result<bool, BeaconError>,
    source: &mut impl ExpansionNeighbors,
    work: &mut dyn WorkControl,
) -> Result<Vec<GraphHit>, BeaconError> {
    work.charge(1)?;
    if seeds.len() > limits.max_seed_ids {
        return Err(BeaconError::ResourceLimit {
            resource: "expansion seed IDs",
            limit: limits.max_seed_ids,
        });
    }
    let k = usize::try_from(candidates)
        .map_err(|_| BeaconError::InvalidQuery("graph candidate depth exceeds usize"))?;
    if k == 0 {
        return Ok(Vec::new());
    }
    let mut distance = BTreeMap::new();
    let mut frontier = VecDeque::new();
    for &id in seeds {
        if contains(id, distance.len(), work)? && !distance.contains_key(&id) {
            admit_visit(distance.len(), limits)?;
            distance.insert(id, 0_u32);
            frontier.push_back((id, 0_u32));
        }
    }
    while let Some((id, hops)) = frontier.pop_front() {
        work.charge(1)?;
        if hops == max_hops {
            continue;
        }
        // Keep the first refusal even if a defective source swallows a
        // visitor error and continues. It cannot publish a partial search.
        let mut failure: Option<BeaconError> = None;
        let result = source.visit_neighbors(id, work, &mut |next, work| {
            if let Some(error) = &failure {
                return Err(error.clone());
            }
            let result: Result<(), BeaconError> = (|| {
                if !contains(next, distance.len(), work)? {
                    return Err(BeaconError::InvalidQuery(
                        "expansion endpoint outside selected corpus",
                    ));
                }
                if distance.contains_key(&next) {
                    return Ok(());
                }
                admit_visit(distance.len(), limits)?;
                // hops < max_hops <= u32::MAX.
                let next_hops = hops + 1;
                distance.insert(next, next_hops);
                frontier.push_back((next, next_hops));
                Ok(())
            })();
            if let Err(error) = &result {
                failure = Some(error.clone());
            }
            result
        });
        if let Some(error) = failure {
            return Err(error);
        }
        result?;
    }
    let mut best = BinaryHeap::new();
    for (id, hops) in distance {
        work.charge(2 * tree_work(best.len()))?;
        if hops == 0 && !include_seeds {
            continue;
        }
        let candidate = (hops, id);
        if best.len() < k {
            best.push(candidate);
        } else if best.peek().is_some_and(|worst| candidate < *worst) {
            best.pop();
            best.push(candidate);
        }
    }
    let mut rows = Vec::new();
    while !best.is_empty() {
        work.charge(tree_work(best.len()))?;
        let (hops, id) = best
            .pop()
            .ok_or(BeaconError::Invariant("expansion heap disappeared"))?;
        rows.push(GraphHit { id, hops });
    }
    for i in 0..rows.len() / 2 {
        work.charge(1)?;
        let other = rows.len() - i - 1;
        rows.swap(i, other);
    }
    work.charge(1)?;
    Ok(rows)
}

fn admit_visit(count: usize, limits: ExpansionLimits) -> Result<(), BeaconError> {
    let limit = limits.max_vertices.min(limits.max_visited_vertices);
    if count >= limit {
        Err(BeaconError::ResourceLimit {
            resource: "expansion visited vertices",
            limit,
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod source_tests {
    use super::*;
    use crate::WorkBudget;

    struct Source {
        arcs: Vec<(VId, VId)>,
        calls: Vec<VId>,
        fail_at: Option<VId>,
        swallow: bool,
    }
    impl ExpansionNeighbors for Source {
        fn visit_neighbors(
            &mut self,
            vertex: VId,
            work: &mut dyn WorkControl,
            visit: &mut dyn FnMut(VId, &mut dyn WorkControl) -> Result<(), BeaconError>,
        ) -> Result<(), BeaconError> {
            self.calls.push(vertex);
            if self.fail_at == Some(vertex) {
                return Err(BeaconError::Invariant("injected source refusal"));
            }
            for &(from, to) in &self.arcs {
                if from == vertex {
                    work.charge(1)?;
                    let result = visit(to, work);
                    if !self.swallow {
                        result?;
                    }
                }
            }
            Ok(())
        }
    }
    fn source(arcs: Vec<(VId, VId)>) -> Source {
        Source {
            arcs,
            calls: Vec::new(),
            fail_at: None,
            swallow: false,
        }
    }
    fn corpus(ids: &[VId], limits: ExpansionLimits) -> ExpansionGraph {
        let mut graph = ExpansionGraph::new(limits);
        let mut work = WorkBudget::new(usize::MAX);
        for &id in ids {
            graph.insert_vertex(id, &mut work).unwrap();
        }
        graph
    }

    #[test]
    fn demand_stops_at_hop_boundary_and_never_visits_disconnected_vertices() {
        let high = VId(u128::MAX);
        let graph = corpus(&[VId(0), VId(1), VId(2), high], ExpansionLimits::default());
        let mut input = source(vec![
            (VId(0), VId(2)),
            (VId(0), VId(1)),
            (VId(0), VId(1)),
            (VId(1), high),
            (high, high),
        ]);
        let rows = graph
            .expand_from_source(
                &[VId(0), VId(0), VId(99)],
                1,
                false,
                1,
                &mut input,
                &mut WorkBudget::new(10_000),
            )
            .unwrap();
        assert_eq!(
            rows,
            vec![GraphHit {
                id: VId(1),
                hops: 1
            }]
        );
        assert_eq!(input.calls, vec![VId(0)]);
        assert!(graph.arcs.is_empty());
    }

    #[test]
    fn zero_hops_zero_candidates_and_missing_seeds_do_not_touch_adjacency() {
        let graph = corpus(&[VId(1)], ExpansionLimits::default());
        for (seeds, hops, k, expected) in [
            (
                vec![VId(1)],
                0,
                1,
                vec![GraphHit {
                    id: VId(1),
                    hops: 0,
                }],
            ),
            (vec![VId(1)], u32::MAX, 0, vec![]),
            (vec![VId(99)], u32::MAX, 1, vec![]),
        ] {
            let mut input = source(vec![]);
            input.fail_at = Some(VId(1));
            assert_eq!(
                graph
                    .expand_from_source(
                        &seeds,
                        hops,
                        true,
                        k,
                        &mut input,
                        &mut WorkBudget::new(1000)
                    )
                    .unwrap(),
                expected
            );
            assert!(input.calls.is_empty());
        }
    }

    #[test]
    fn candidate_limit_cannot_hide_late_failure_or_visited_overflow() {
        let graph = corpus(&[VId(1), VId(2), VId(3)], ExpansionLimits::default());
        let mut input = source(vec![(VId(1), VId(2)), (VId(2), VId(3))]);
        input.fail_at = Some(VId(2));
        assert!(matches!(
            graph.expand_from_source(
                &[VId(1)],
                3,
                false,
                1,
                &mut input,
                &mut WorkBudget::new(1000)
            ),
            Err(BeaconError::Invariant("injected source refusal"))
        ));
        let graph = corpus(
            &[VId(1), VId(2), VId(3)],
            ExpansionLimits {
                max_visited_vertices: 2,
                ..ExpansionLimits::default()
            },
        );
        input.fail_at = None;
        assert!(matches!(
            graph.expand_from_source(
                &[VId(1)],
                3,
                false,
                1,
                &mut input,
                &mut WorkBudget::new(1000)
            ),
            Err(BeaconError::ResourceLimit {
                resource: "expansion visited vertices",
                limit: 2
            })
        ));
    }

    #[test]
    fn swallowed_visitor_failures_and_out_of_corpus_neighbors_fail_closed() {
        let graph = corpus(&[VId(1)], ExpansionLimits::default());
        let mut input = source(vec![(VId(1), VId(99))]);
        input.swallow = true;
        assert!(matches!(
            graph.expand_from_source(
                &[VId(1)],
                1,
                true,
                1,
                &mut input,
                &mut WorkBudget::new(1000)
            ),
            Err(BeaconError::InvalidQuery(_))
        ));
        let graph = corpus(
            &[VId(1), VId(2)],
            ExpansionLimits {
                max_visited_vertices: 1,
                ..ExpansionLimits::default()
            },
        );
        let mut input = source(vec![(VId(1), VId(2))]);
        input.swallow = true;
        assert!(matches!(
            graph.expand_from_source(
                &[VId(1)],
                1,
                true,
                1,
                &mut input,
                &mut WorkBudget::new(1000)
            ),
            Err(BeaconError::ResourceLimit { .. })
        ));
    }

    #[test]
    fn exhaustive_three_vertex_graphs_match_independent_min_plus_distances() {
        let ids = [VId(0), VId(1_u128 << 100), VId(u128::MAX)];
        for bits in 0_u16..512 {
            let mut arcs = Vec::new();
            let mut distance = [[99_u32; 3]; 3];
            for from in 0..3 {
                distance[from][from] = 0;
                for to in 0..3 {
                    if bits & (1 << (from * 3 + to)) != 0 {
                        arcs.push((ids[from], ids[to]));
                        distance[from][to] = distance[from][to].min(1);
                    }
                }
            }
            // Independent min-plus closure, not the production frontier walk.
            for via in 0..3 {
                for from in 0..3 {
                    for to in 0..3 {
                        distance[from][to] =
                            distance[from][to].min(distance[from][via] + distance[via][to]);
                    }
                }
            }
            let mut graph = corpus(&ids, ExpansionLimits::default());
            let mut work = WorkBudget::new(usize::MAX);
            for &(from, to) in &arcs {
                graph
                    .insert_edge(from, to, ExpansionDirection::Outgoing, &mut work)
                    .unwrap();
            }
            for mask in 0_u8..8 {
                let seeds: Vec<_> = (0..3)
                    .filter(|i| mask & (1 << i) != 0)
                    .map(|i| ids[i])
                    .collect();
                for hops in 0..=3 {
                    for include in [false, true] {
                        for k in 0..=3 {
                            let mut expected: Vec<_> = (0..3)
                                .filter_map(|to| {
                                    let d = (0..3)
                                        .filter(|i| mask & (1 << i) != 0)
                                        .map(|from| distance[from][to])
                                        .min()
                                        .unwrap_or(99);
                                    (d <= hops && (include || d != 0)).then_some((d, ids[to]))
                                })
                                .collect();
                            expected.sort();
                            expected.truncate(k as usize);
                            let expected: Vec<_> = expected
                                .into_iter()
                                .map(|(hops, id)| GraphHit { id, hops })
                                .collect();
                            let mut input = source(arcs.iter().rev().copied().collect());
                            assert_eq!(
                                graph
                                    .expand_from_source(
                                        &seeds, hops, include, k, &mut input, &mut work
                                    )
                                    .unwrap(),
                                expected
                            );
                            assert_eq!(
                                graph.expand(&seeds, hops, include, k, &mut work).unwrap(),
                                expected
                            );
                            let mut input = source(arcs.clone());
                            assert_eq!(
                                expand_from_membership(
                                    &seeds, hops, include, k, ExpansionLimits::default(),
                                    &graph, &mut input, &mut work,
                                ).unwrap(),
                                expected
                            );
                        }
                    }
                }
            }
        }
    }

    struct Membership {
        calls: std::cell::Cell<usize>,
        absent: Option<VId>,
        refused: Option<VId>,
    }
    impl ExpansionMembership for Membership {
        fn contains_vertex(&self, id: VId, work: &mut dyn WorkControl) -> Result<bool, BeaconError> {
            work.charge(1)?;
            self.calls.set(self.calls.get() + 1);
            if self.refused == Some(id) {
                return Err(BeaconError::Invariant("injected membership refusal"));
            }
            Ok(self.absent != Some(id))
        }
    }
    fn membership() -> Membership {
        Membership { calls: std::cell::Cell::new(0), absent: None, refused: None }
    }

    #[test]
    fn unenumerated_full_width_membership_demands_only_reached_vertices() {
        // Membership covers the entire identity space without constructing it.
        let domain = membership();
        let high = VId(u128::MAX);
        let mut input = source(vec![(VId(0), high), (VId(7), VId(8)), (high, VId(9))]);
        let mut limits = ExpansionLimits::default();
        limits.max_vertices = 2;
        limits.max_visited_vertices = 2;
        let rows = expand_from_membership(&[VId(0), VId(0)], 1, true, 2, limits,
            &domain, &mut input, &mut WorkBudget::new(1000)).unwrap();
        assert_eq!(rows, vec![GraphHit { id: VId(0), hops: 0 }, GraphHit { id: high, hops: 1 }]);
        assert_eq!(domain.calls.get(), 3);
        assert_eq!(input.calls, vec![VId(0)]);
    }

    #[test]
    fn membership_refusal_remains_exact_even_if_neighbor_source_swallows_it() {
        let mut domain = membership();
        domain.refused = Some(VId(2));
        let mut input = source(vec![(VId(1), VId(2)), (VId(1), VId(3))]);
        input.swallow = true;
        let result = expand_from_membership(&[VId(1)], 1, true, 1,
            ExpansionLimits::default(), &domain, &mut input, &mut WorkBudget::new(1000));
        assert!(matches!(result, Err(BeaconError::Invariant("injected membership refusal"))));
        assert_eq!(domain.calls.get(), 2, "no membership demand after retained refusal");
    }

    #[test]
    fn demand_membership_preserves_zero_absence_and_both_visit_ceilings() {
        let mut domain = membership();
        domain.absent = Some(VId(99));
        domain.refused = Some(VId(2));
        let mut input = source(vec![]);
        assert!(expand_from_membership(&[VId(2)], u32::MAX, true, 0,
            ExpansionLimits::default(), &domain, &mut input, &mut WorkBudget::new(1000))
            .unwrap().is_empty());
        assert_eq!(domain.calls.get(), 0);
        assert!(expand_from_membership(&[VId(99)], u32::MAX, true, 1,
            ExpansionLimits::default(), &domain, &mut input, &mut WorkBudget::new(1000))
            .unwrap().is_empty());
        assert!(input.calls.is_empty());
        for vertices in [false, true] {
            let mut limits = ExpansionLimits::default();
            if vertices { limits.max_vertices = 0; }
            else { limits.max_visited_vertices = 0; }
            assert!(matches!(expand_from_membership(&[VId(1)], 0, true, 1,
                limits, &domain, &mut input, &mut WorkBudget::new(1000)),
                Err(BeaconError::ResourceLimit { resource: "expansion visited vertices", limit: 0 })));
        }
        let before = domain.calls.get();
        assert!(matches!(expand_from_membership(&[VId(1)], 1, true, 1,
            ExpansionLimits { max_seed_ids: 0, ..ExpansionLimits::default() },
            &domain, &mut input, &mut WorkBudget::new(1000)),
            Err(BeaconError::ResourceLimit { resource: "expansion seed IDs", limit: 0 })));
        assert_eq!(domain.calls.get(), before);
    }

    #[test]
    fn every_membership_and_neighbor_work_cut_discards_the_private_result() {
        struct Cut { calls: usize, stop: usize }
        impl WorkControl for Cut {
            fn charge(&mut self, _: usize) -> Result<(), BeaconError> {
                self.calls += 1;
                if self.calls == self.stop { Err(BeaconError::Cancelled) } else { Ok(()) }
            }
        }
        let run = |stop| {
            let mut work = Cut { calls: 0, stop };
            let domain = membership();
            let mut input = source(vec![(VId(0), VId(1)), (VId(1), VId(2)), (VId(2), VId(0))]);
            let result = expand_from_membership(&[VId(0)], 3, true, 1,
                ExpansionLimits::default(), &domain, &mut input, &mut work);
            (result, work.calls)
        };
        let (rows, count) = run(usize::MAX);
        assert_eq!(rows.unwrap(), vec![GraphHit { id: VId(0), hops: 0 }]);
        assert!(count > 10);
        for stop in 1..=count {
            let (result, seen) = run(stop);
            assert!(matches!(result, Err(BeaconError::Cancelled)), "cut {stop}");
            assert_eq!(seen, stop);
        }
    }
}
