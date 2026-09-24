//! Third-lane RRF using an explicitly ranked graph population. Expansion is
//! separate from fusion so its path semantics and resource refusals cannot be
//! hidden inside an approximate vector search or a post-filtering predicate.

use std::collections::{BTreeMap, BinaryHeap};
use std::num::NonZeroU32;

use super::{ExactHybridQuery, ExactRrfScore, Ranked, rank};
use crate::expansion::GraphHit;
use crate::read::{ReadOptions, Search};
use crate::{BeaconError, IndexConfig, IndexSnapshot, WorkControl};
use fgdb_types::{CanonicalDecimal, VId};

/// Add a graph lane to a native text/vector request. `retrieval.k` is the FINAL
/// output depth, and may exceed the text/vector sum when graph candidates
/// contribute. At least one text/vector weight remains required by the base
/// profile. A zero graph weight OR depth disables graph preparation entirely.
/// Graph seeds and hop/edge semantics are pinned separately in ExpansionSpec.
#[derive(Clone, Copy, Debug)]
pub struct GraphHybridQuery<'a> {
    pub retrieval: ExactHybridQuery<'a>,
    pub graph_candidates: u32,
    pub graph_weight: u16,
}

impl<'a> GraphHybridQuery<'a> {
    #[must_use]
    pub fn graph_enabled(self) -> bool {
        self.graph_weight != 0 && self.graph_candidates != 0
    }

    /// Internal candidate preparation must not enforce the OLD two-lane output
    /// ceiling or truncate the base union before the graph contribution arrives.
    #[must_use]
    pub fn source_query(self) -> ExactHybridQuery<'a> {
        ExactHybridQuery {
            k: 0,
            ..self.retrieval
        }
    }

    pub fn candidate_depths(self) -> Result<(usize, usize, usize), BeaconError> {
        let (vector, text) = self.source_query().depths()?;
        let graph = if self.graph_weight == 0 {
            0
        } else {
            usize::try_from(self.graph_candidates)
                .map_err(|_| BeaconError::InvalidQuery("graph candidate depth exceeds usize"))?
        };
        let total = vector
            .checked_add(text)
            .and_then(|n| n.checked_add(graph))
            .ok_or(BeaconError::InvalidQuery("RRF candidate sum exceeds usize"))?;
        if self.retrieval.k > total {
            return Err(BeaconError::InvalidQuery(
                "RRF k exceeds active candidate depths",
            ));
        }
        Ok((vector, text, graph))
    }
}

impl<K, L> ReadOptions<K, L> {
    pub fn config_for_graph(
        &self,
        query: GraphHybridQuery<'_>,
    ) -> Result<IndexConfig, BeaconError> {
        query.candidate_depths()?;
        if query.retrieval.k > self.policy.max_result_rows {
            return Err(BeaconError::ResourceLimit {
                resource: "result rows",
                limit: self.policy.max_result_rows,
            });
        }
        self.config_for(Search::Hybrid(query.source_query()))
    }
}

/// Exact rank arithmetic does not make truncated source populations, ANN, or
/// floating BM25 exact. These are native hit cells, not an AnswerContract or
/// durable certificate. Missing modalities contribute ZERO, including graph.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphHybridHit {
    pub id: VId,
    pub score: ExactRrfScore,
    pub decimal_score: CanonicalDecimal,
    pub vector_rank: Option<NonZeroU32>,
    pub text_rank: Option<NonZeroU32>,
    pub graph_rank: Option<NonZeroU32>,
    pub vector_distance: Option<f64>,
    pub text_score: Option<f64>,
    pub graph_hops: Option<u32>,
}

impl IndexSnapshot {
    /// Fuse this snapshot's complete bounded text/vector candidate union with
    /// the supplied graph candidates. The graph slice MUST already come from
    /// the same selected snapshot/authority domain, as the database adapter
    /// ensures. This low-level arithmetic API does not authenticate sources.
    ///
    /// Graph input is strict (hop count, VId) order, with globally unique IDs;
    /// malformed lists refuse instead of double-counting or changing ranks.
    /// A disabled graph lane is not inspected. No earlier final top-k prunes
    /// candidates that can become winners after the third contribution.
    pub fn hybrid_search_graph(
        &self,
        query: GraphHybridQuery<'_>,
        graph: &[GraphHit],
        work: &mut dyn WorkControl,
    ) -> Result<Vec<GraphHybridHit>, BeaconError> {
        work.charge(1)?;
        let (vector_depth, text_depth, graph_depth) = query.candidate_depths()?;
        if query.retrieval.k == 0 {
            return Ok(Vec::new());
        }
        let mut graph_ranks = BTreeMap::new();
        if graph_depth != 0 {
            if graph.len() > graph_depth {
                return Err(BeaconError::InvalidQuery(
                    "graph population exceeds candidate depth",
                ));
            }
            let mut previous = None;
            for (offset, hit) in graph.iter().enumerate() {
                work.charge(1 + graph_ranks.len().checked_ilog2().unwrap_or(0) as usize)?;
                let key = (hit.hops, hit.id);
                if previous.is_some_and(|before| before >= key) || graph_ranks.contains_key(&hit.id)
                {
                    return Err(BeaconError::InvalidQuery(
                        "graph ranks must be ordered and unique",
                    ));
                }
                previous = Some(key);
                graph_ranks.insert(hit.id, (rank(offset)?, hit.hops));
            }
        }
        // Same native source selection/rank assignment as two-lane RRF. It
        // searches each enabled modality ONCE and performs no premature top-k.
        let mut fused = self.fusion_candidates(
            query.source_query(),
            vector_depth,
            text_depth,
            |_| true,
            work,
        )?;
        for &id in graph_ranks.keys() {
            work.charge(1 + fused.len().checked_ilog2().unwrap_or(0) as usize)?;
            fused.entry(id).or_default();
        }
        let mut best = BinaryHeap::<Ranked>::new();
        for (&id, evidence) in &fused {
            work.charge(2 * (1 + best.len().checked_ilog2().unwrap_or(0) as usize))?;
            let candidate = Ranked {
                id,
                score: ExactRrfScore::from_graph_ranks(
                    query.retrieval.profile,
                    evidence.vector_rank,
                    evidence.text_rank,
                    graph_ranks.get(&id).map(|(rank, _)| *rank),
                    query.graph_weight,
                ),
            };
            if best.len() < query.retrieval.k {
                best.push(candidate);
            } else if best.peek().is_some_and(|worst| candidate < *worst) {
                best.pop();
                best.push(candidate);
            }
        }
        let mut rows = Vec::new();
        while !best.is_empty() {
            work.charge(1 + best.len().checked_ilog2().unwrap_or(0) as usize)?;
            let hit = best
                .pop()
                .ok_or(BeaconError::Invariant("RRF heap disappeared"))?;
            let evidence = fused
                .remove(&hit.id)
                .ok_or(BeaconError::Invariant("RRF evidence disappeared"))?;
            let graph = graph_ranks.get(&hit.id);
            rows.push(GraphHybridHit {
                id: hit.id,
                score: hit.score,
                decimal_score: hit.score.decimal()?,
                vector_rank: evidence.vector_rank,
                text_rank: evidence.text_rank,
                graph_rank: graph.map(|(rank, _)| *rank),
                vector_distance: evidence.vector_distance,
                text_score: evidence.text_score,
                graph_hops: graph.map(|(_, hops)| *hops),
            });
        }
        for i in 0..rows.len() / 2 {
            work.charge(1)?;
            let other = rows.len() - i - 1;
            rows.swap(i, other);
        }
        work.charge(1)?;
        Ok(rows)
    }
}
