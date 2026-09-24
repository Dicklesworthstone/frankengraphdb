//! Bounded, resident graph expansion for Beacon's graph candidate lane.
//! This is a disposable query structure, never storage or an authorization
//! grant. The database adapter admits historical vertices and BOTH endpoints
//! before insertion. No edge weights, properties or multiplicities are scored.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque};

use fgdb_types::VId;

use crate::{BeaconError, WorkControl};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpansionDirection {
    Outgoing,
    Incoming,
    Undirected,
}

/// Independent resident limits. Zero means zero. Input edges count admitted
/// logical edges BEFORE parallel-edge collapse. At most two arcs per input
/// edge are retained. Source scratch bounds the historical edge visitor's
/// admission events in the database adapter, not process RSS.
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

/// Query-private ordered adjacency, bounded during construction. Mutations
/// fail closed: after an error or unwinding insertion, expansion refuses until
/// a NEW builder is created. A cancelled read does not mutate this structure.
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

    pub fn insert_vertex(&mut self, id: VId, work: &mut dyn WorkControl) -> Result<(), BeaconError> {
        self.mutate(|graph| {
            work.charge(tree_work(graph.vertices.len()))?;
            if graph.vertices.contains(&id) {
                return Ok(());
            }
            if graph.vertices.len() >= graph.limits.max_vertices {
                return Err(BeaconError::ResourceLimit {
                    resource: "expansion vertices", limit: graph.limits.max_vertices,
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
                return Err(BeaconError::InvalidQuery("expansion endpoint outside selected corpus"));
            }
            if graph.input_edges >= graph.limits.max_input_edges {
                return Err(BeaconError::ResourceLimit {
                    resource: "expansion input edges", limit: graph.limits.max_input_edges,
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

    /// Complete multi-source unit-distance BFS up to the explicit hop bound.
    /// Duplicate seeds/paths, parallel edges and cycles contribute a vertex
    /// only once, at MINIMUM hop distance. Rank by (hops, full-width VId).
    /// Candidate truncation happens AFTER complete bounded exploration; a
    /// work/visited limit is an error, never a successful partial population.
    /// max_hops=0 still permits selected seeds when include_seeds is true.
    pub fn expand(
        &self,
        seeds: &[VId],
        max_hops: u32,
        include_seeds: bool,
        candidates: u32,
        work: &mut dyn WorkControl,
    ) -> Result<Vec<GraphHit>, BeaconError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        work.charge(1)?;
        if seeds.len() > self.limits.max_seed_ids {
            return Err(BeaconError::ResourceLimit {
                resource: "expansion seed IDs", limit: self.limits.max_seed_ids,
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
            work.charge(tree_work(self.vertices.len()) + tree_work(distance.len()))?;
            if self.contains(id) && !distance.contains_key(&id) {
                self.admit_visit(distance.len())?;
                distance.insert(id, 0_u32);
                frontier.push_back((id, 0_u32));
            }
        }
        while let Some((id, hops)) = frontier.pop_front() {
            work.charge(tree_work(self.arcs.len()))?;
            if hops == max_hops {
                continue;
            }
            for &(from, next) in self.arcs.range((id, VId(0))..) {
                work.charge(tree_work(distance.len()))?;
                if from != id {
                    break;
                }
                if distance.contains_key(&next) {
                    continue;
                }
                self.admit_visit(distance.len())?;
                // hops < max_hops <= u32::MAX.
                let hops = hops + 1;
                distance.insert(next, hops);
                frontier.push_back((next, hops));
            }
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
            let (hops, id) = best.pop().ok_or(BeaconError::Invariant("expansion heap disappeared"))?;
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

    fn admit_visit(&self, count: usize) -> Result<(), BeaconError> {
        if count >= self.limits.max_visited_vertices {
            Err(BeaconError::ResourceLimit {
                resource: "expansion visited vertices", limit: self.limits.max_visited_vertices,
            })
        } else {
            Ok(())
        }
    }
}
