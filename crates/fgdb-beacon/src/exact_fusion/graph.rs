//! Third-lane RRF using an explicitly ranked graph population. Expansion is
//! separate from fusion so its path semantics and resource refusals cannot be
//! hidden inside an approximate vector search or a post-filtering predicate.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::num::NonZeroU32;

use super::{ExactHybridQuery, ExactRrfProfile, ExactRrfScore, Ranked, rank};
use crate::expansion::GraphHit;
use crate::read::{ReadOptions, Search};
use crate::{BeaconError, IndexConfig, IndexSnapshot, TextMatch, VectorSearch, WorkControl};
use fgdb_types::{CanonicalDecimal, VId};

/// Add a graph lane to a native text/vector request. `retrieval.k` is the FINAL
/// output depth, and may exceed the text/vector sum when graph candidates
/// contribute. [`Self::graph_only`] explicitly disables both index lanes; it
/// requires no text document, embedding, or index. A zero graph weight OR
/// depth disables graph preparation when another modality remains enabled.
/// Graph seeds and hop/edge semantics are pinned separately in ExpansionSpec.
#[derive(Clone, Copy, Debug)]
pub struct GraphHybridQuery<'a> {
    pub retrieval: ExactHybridQuery<'a>,
    pub graph_candidates: u32,
    pub graph_weight: u16,
}

impl<'a> GraphHybridQuery<'a> {
    /// A topology-only ranked request. The ordinary two-source profile's
    /// constructor still rejects two zero weights: only this graph owner can
    /// construct the neutral base contribution, alongside an active graph lane.
    /// The shared fusion engine retains exact scores, ordering and admission.
    pub fn graph_only(
        k: usize,
        graph_candidates: u32,
        graph_weight: u16,
        rank_constant: u32,
    ) -> Result<Self, BeaconError> {
        let rank_constant = NonZeroU32::new(rank_constant).ok_or(BeaconError::InvalidQuery(
            "RRF rank constant must be positive",
        ))?;
        let query = Self {
            retrieval: ExactHybridQuery {
                vector: &[],
                text: "",
                k,
                vector_candidates: 0,
                text_candidates: 0,
                vector_mode: VectorSearch::Exact,
                text_mode: TextMatch::Any,
                profile: ExactRrfProfile {
                    rank_constant,
                    vector_weight: 0,
                    text_weight: 0,
                },
            },
            graph_candidates,
            graph_weight,
        };
        query.candidate_depths()?;
        Ok(query)
    }

    /// No text/vector source participates, including corpus construction.
    #[must_use]
    pub fn is_graph_only(self) -> bool {
        self.retrieval.profile.vector_weight() == 0 && self.retrieval.profile.text_weight() == 0
    }

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
        // Public fields may have changed after graph_only constructed a valid
        // request. Even k=0 must not resurrect an entirely disabled request.
        if self.is_graph_only() && !self.graph_enabled() {
            return Err(BeaconError::InvalidQuery(
                "graph-only retrieval needs a positive graph weight and candidate depth",
            ));
        }
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

    /// Rank an already admitted graph population without creating an index.
    /// This is the SAME validation, dense ranking and exact RRF implementation
    /// used by the indexed path below. It authenticates no caller-supplied IDs;
    /// database hosts still own historical selection, authorization and transit.
    /// A request with an enabled text/vector lane refuses instead of dropping it.
    pub fn fuse_graph_only(
        self,
        graph: &[GraphHit],
        eligible: impl Fn(VId) -> bool,
        work: &mut dyn WorkControl,
    ) -> Result<Vec<GraphHybridHit>, BeaconError> {
        fuse(None, self, graph, eligible, work)
    }
}

impl<K, L> ReadOptions<K, L> {
    /// Validate the graph request and output ceiling without validating or
    /// enabling an unused index. Source/expansion limits remain host-enforced.
    pub fn validate_graph_only(&self, query: GraphHybridQuery<'_>) -> Result<(), BeaconError> {
        if !query.is_graph_only() {
            return Err(BeaconError::InvalidQuery(
                "index-free retrieval cannot discard an enabled text or vector lane",
            ));
        }
        query.candidate_depths()?;
        if query.retrieval.k > self.policy.max_result_rows {
            return Err(BeaconError::ResourceLimit {
                resource: "result rows",
                limit: self.policy.max_result_rows,
            });
        }
        Ok(())
    }

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
    /// candidates that can become winners after the third contribution arrives.
    pub fn hybrid_search_graph(
        &self,
        query: GraphHybridQuery<'_>,
        graph: &[GraphHit],
        work: &mut dyn WorkControl,
    ) -> Result<Vec<GraphHybridHit>, BeaconError> {
        self.hybrid_search_graph_filtered(query, graph, |_| true, work)
    }

    /// Apply one result predicate to ALL three candidate lanes before fusion
    /// and final top-k, rather than discarding winners after truncation. Text
    /// and vector searches select their candidate depths from eligible rows;
    /// graph ranks are dense among eligible entries of the supplied population.
    ///
    /// The graph slice remains an explicit bounded input: this method cannot
    /// refill it from vertices not supplied by the caller. Filter during graph
    /// expansion, before its candidate limit, when that limit must count only
    /// eligible endpoints. Rejected endpoints may still be traversal vertices.
    /// The predicate must be stable for this query and is NOT an ACL: the
    /// corpus and graph input must already share an authorized snapshot.
    /// Malformed graph input refuses even when the predicate rejects its IDs.
    pub fn hybrid_search_graph_filtered(
        &self,
        query: GraphHybridQuery<'_>,
        graph: &[GraphHit],
        eligible: impl Fn(VId) -> bool,
        work: &mut dyn WorkControl,
    ) -> Result<Vec<GraphHybridHit>, BeaconError> {
        fuse(Some(self), query, graph, eligible, work)
    }
}

// One ranking owner for indexed and graph-only retrieval. An absent index is
// legal only when both source weights are zero, never a fallback after a failed
// search. All input validation, resource charges and exact arithmetic are shared.
fn fuse(
    index: Option<&IndexSnapshot>,
    query: GraphHybridQuery<'_>,
    graph: &[GraphHit],
    eligible: impl Fn(VId) -> bool,
    work: &mut dyn WorkControl,
) -> Result<Vec<GraphHybridHit>, BeaconError> {
    work.charge(1)?;
    if index.is_none() && !query.is_graph_only() {
        return Err(BeaconError::InvalidQuery(
            "index-free retrieval cannot discard an enabled text or vector lane",
        ));
    }
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
        let mut seen = BTreeSet::new();
        for hit in graph {
            // Include rejected entries in validation and work accounting.
            // They must not hide duplicates, disorder, or budget failures.
            work.charge(2 * (1 + seen.len().checked_ilog2().unwrap_or(0) as usize))?;
            let key = (hit.hops, hit.id);
            if previous.is_some_and(|before| before >= key) || seen.contains(&hit.id) {
                return Err(BeaconError::InvalidQuery(
                    "graph ranks must be ordered and unique",
                ));
            }
            work.reserve_scratch(1)?;
            seen.insert(hit.id);
            previous = Some(key);
            if eligible(hit.id) {
                let next_rank = rank(graph_ranks.len())?;
                work.reserve_scratch(1)?;
                graph_ranks.insert(hit.id, (next_rank, hit.hops));
            }
        }
    }
    // Every enabled modality applies the same predicate before assigning
    // ranks. Search each source ONCE, without an earlier final top-k.
    let mut fused = match index {
        Some(index) => index.fusion_candidates(
            query.source_query(),
            vector_depth,
            text_depth,
            &eligible,
            work,
        )?,
        None => BTreeMap::new(),
    };
    for &id in graph_ranks.keys() {
        work.charge(1 + fused.len().checked_ilog2().unwrap_or(0) as usize)?;
        if let std::collections::btree_map::Entry::Vacant(entry) = fused.entry(id) {
            work.reserve_scratch(1)?;
            entry.insert(Default::default());
        }
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
            work.reserve_scratch(1)?;
            best.push(candidate);
        } else if best.peek().is_some_and(|worst| candidate < *worst) {
            best.pop();
            best.push(candidate);
        }
    }
    let mut rows = Vec::new();
    while !best.is_empty() {
        work.charge(1 + best.len().checked_ilog2().unwrap_or(0) as usize)?;
        work.reserve_scratch(1)?;
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

#[cfg(test)]
mod filtered_tests {
    use super::*;
    use crate::{
        BeaconIndex, DistanceMetric, ExactRrfProfile, HnswConfig, IndexDocument, TextMatch,
        VectorSearch, WorkBudget,
    };

    fn budget() -> WorkBudget {
        WorkBudget::new(1_000_000)
    }

    fn index() -> IndexSnapshot {
        BeaconIndex::build(
            IndexConfig {
                vector: Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean)),
                ..IndexConfig::default()
            },
            [
                IndexDocument {
                    id: VId(1),
                    vector: Some(vec![0.0]),
                    text: Some("red".into()),
                },
                IndexDocument {
                    id: VId(2),
                    vector: Some(vec![1.0]),
                    text: Some("red".into()),
                },
                IndexDocument {
                    id: VId(3),
                    vector: Some(vec![2.0]),
                    text: Some("blue".into()),
                },
                IndexDocument {
                    id: VId(4),
                    vector: None,
                    text: None,
                },
            ],
            &mut budget(),
        )
        .unwrap()
        .snapshot()
    }

    fn query() -> GraphHybridQuery<'static> {
        GraphHybridQuery {
            retrieval: ExactHybridQuery {
                vector: &[0.0],
                text: "red",
                k: 2,
                vector_candidates: 1,
                text_candidates: 1,
                vector_mode: VectorSearch::Exact,
                text_mode: TextMatch::Any,
                profile: ExactRrfProfile::default(),
            },
            graph_candidates: 3,
            graph_weight: 100,
        }
    }

    fn graph() -> [GraphHit; 3] {
        [
            GraphHit {
                id: VId(1),
                hops: 0,
            },
            GraphHit {
                id: VId(3),
                hops: 1,
            },
            GraphHit {
                id: VId(4),
                hops: 2,
            },
        ]
    }

    #[test]
    fn predicate_precedes_source_depths_and_final_top_k() {
        let rows = index()
            .hybrid_search_graph_filtered(
                query(),
                &graph(),
                |id| id == VId(2) || id == VId(3),
                &mut budget(),
            )
            .unwrap();
        assert_eq!(
            rows.iter().map(|hit| hit.id).collect::<Vec<_>>(),
            [VId(3), VId(2)]
        );
        assert_eq!(rows[0].graph_rank, NonZeroU32::new(1));
        assert_eq!(rows[0].graph_hops, Some(1));
        assert_eq!(rows[0].vector_rank, None);
        assert_eq!(rows[0].text_rank, None);
        assert_eq!(rows[1].graph_rank, None);
        assert_eq!(rows[1].vector_rank, NonZeroU32::new(1));
        assert_eq!(rows[1].text_rank, NonZeroU32::new(1));
        assert_eq!(rows[0].score.numerator(), 100);
        assert_eq!(rows[0].score.denominator(), 61);
        assert_eq!(rows[1].score.numerator(), 2);
        assert_eq!(rows[1].score.denominator(), 61);
    }

    #[test]
    fn rejected_graph_entries_still_validate_order_and_global_uniqueness() {
        let snapshot = index();
        for graph in [
            vec![
                GraphHit {
                    id: VId(1),
                    hops: 1,
                },
                GraphHit {
                    id: VId(1),
                    hops: 2,
                },
            ],
            vec![
                GraphHit {
                    id: VId(2),
                    hops: 1,
                },
                GraphHit {
                    id: VId(1),
                    hops: 1,
                },
            ],
        ] {
            assert!(matches!(
                snapshot.hybrid_search_graph_filtered(query(), &graph, |_| false, &mut budget()),
                Err(BeaconError::InvalidQuery(
                    "graph ranks must be ordered and unique"
                ))
            ));
        }
        let mut q = query();
        q.graph_candidates = 1;
        assert!(matches!(
            snapshot.hybrid_search_graph_filtered(q, &graph(), |_| false, &mut budget()),
            Err(BeaconError::InvalidQuery(
                "graph population exceeds candidate depth"
            ))
        ));
    }

    #[test]
    fn disabled_graph_is_not_observed_and_reject_all_returns_no_rows() {
        let snapshot = index();
        let mut q = query();
        q.graph_weight = 0;
        let malformed = [GraphHit {
            id: VId(99),
            hops: 2,
        }; 4];
        let rows = snapshot
            .hybrid_search_graph_filtered(
                q,
                &malformed,
                |id| {
                    assert_ne!(id, VId(99));
                    true
                },
                &mut budget(),
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, VId(1));
        assert!(
            snapshot
                .hybrid_search_graph_filtered(query(), &graph(), |_| false, &mut budget())
                .unwrap()
                .is_empty()
        );
    }

    #[derive(Default)]
    struct Count(usize);
    impl WorkControl for Count {
        fn charge(&mut self, units: usize) -> Result<(), BeaconError> {
            self.0 += units;
            Ok(())
        }
    }

    #[test]
    fn every_shorter_allowance_refuses_without_a_partial_result() {
        let snapshot = index();
        let eligible = |id| id != VId(1);
        let mut count = Count::default();
        let expected = snapshot
            .hybrid_search_graph_filtered(query(), &graph(), eligible, &mut count)
            .unwrap();
        for units in 0..count.0 {
            assert_eq!(
                snapshot
                    .hybrid_search_graph_filtered(
                        query(),
                        &graph(),
                        eligible,
                        &mut WorkBudget::new(units),
                    )
                    .unwrap_err(),
                BeaconError::WorkBudgetExceeded
            );
        }
        assert_eq!(
            snapshot
                .hybrid_search_graph_filtered(
                    query(),
                    &graph(),
                    eligible,
                    &mut WorkBudget::new(count.0),
                )
                .unwrap(),
            expected
        );
    }

    #[test]
    fn graph_only_ranks_without_an_index_and_keeps_full_width_identities() {
        let q = GraphHybridQuery::graph_only(3, 3, 100, 60).unwrap();
        let population = [
            GraphHit {
                id: VId(9),
                hops: 0,
            },
            GraphHit {
                id: VId(1),
                hops: 1,
            },
            GraphHit {
                id: VId(u128::MAX),
                hops: 1,
            },
        ];
        let rows = q
            .fuse_graph_only(&population, |_| true, &mut budget())
            .unwrap();
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            [VId(9), VId(1), VId(u128::MAX)]
        );
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.graph_rank, NonZeroU32::new(i as u32 + 1));
            assert_eq!(row.graph_hops, Some(population[i].hops));
            assert_eq!(row.vector_rank, None);
            assert_eq!(row.text_rank, None);
            assert_eq!(row.vector_distance, None);
            assert_eq!(row.text_score, None);
            assert_eq!(
                row.score.numerator() * (61 + i as u128),
                100 * row.score.denominator()
            );
        }
        assert_eq!(
            rows,
            index()
                .hybrid_search_graph(q, &population, &mut budget())
                .unwrap()
        );
        let filtered = q
            .fuse_graph_only(&population, |id| id != VId(9), &mut budget())
            .unwrap();
        assert_eq!(filtered[0].id, VId(1));
        assert_eq!(filtered[0].score.numerator(), 100);
        assert_eq!(filtered[0].score.denominator(), 61);
    }

    #[test]
    fn index_free_execution_never_discards_an_enabled_lane_or_weakens_profiles() {
        assert!(ExactRrfProfile::new(60, 0, 0).is_err());
        for (k, depth, weight, constant) in
            [(1, 1, 1, 0), (0, 1, 0, 60), (0, 0, 1, 60), (2, 1, 1, 60)]
        {
            assert!(GraphHybridQuery::graph_only(k, depth, weight, constant).is_err());
        }
        assert!(
            query()
                .fuse_graph_only(&[], |_| true, &mut budget())
                .is_err()
        );
        let q = GraphHybridQuery::graph_only(0, 1, 1, 60).unwrap();
        assert!(
            q.fuse_graph_only(&[], |_| true, &mut budget())
                .unwrap()
                .is_empty()
        );
        let mut disabled = q;
        disabled.graph_weight = 0;
        assert!(
            disabled
                .fuse_graph_only(&[], |_| true, &mut budget())
                .is_err()
        );
        let mut options = ReadOptions::<u64, u64>::text(1);
        options.index.text = None;
        options.index.vector = None;
        assert!(options.index.validate().is_err());
        options.validate_graph_only(q).unwrap();
        options.policy.max_result_rows = 0;
        assert!(matches!(
            options.validate_graph_only(GraphHybridQuery::graph_only(1, 1, 1, 60).unwrap()),
            Err(BeaconError::ResourceLimit {
                resource: "result rows",
                limit: 0
            })
        ));
        assert!(options.validate_graph_only(query()).is_err());
    }

    #[test]
    fn graph_only_refusals_survive_reject_all_and_every_work_cut() {
        let q = GraphHybridQuery::graph_only(3, 3, 1, 60).unwrap();
        let duplicate = [
            GraphHit {
                id: VId(1),
                hops: 0,
            },
            GraphHit {
                id: VId(1),
                hops: 1,
            },
        ];
        assert!(matches!(
            q.fuse_graph_only(&duplicate, |_| false, &mut budget()),
            Err(BeaconError::InvalidQuery(
                "graph ranks must be ordered and unique"
            ))
        ));
        let reversed = [
            GraphHit {
                id: VId(2),
                hops: 1,
            },
            GraphHit {
                id: VId(1),
                hops: 1,
            },
        ];
        assert!(
            q.fuse_graph_only(&reversed, |_| false, &mut budget())
                .is_err()
        );
        let too_many = [GraphHit {
            id: VId(0),
            hops: 0,
        }; 4];
        assert!(
            q.fuse_graph_only(&too_many, |_| false, &mut budget())
                .is_err()
        );
        for reject_all in [false, true] {
            let eligible = |id| !reject_all && id != VId(1);
            let mut count = Count::default();
            let expected = q.fuse_graph_only(&graph(), eligible, &mut count).unwrap();
            for units in 0..count.0 {
                assert_eq!(
                    q.fuse_graph_only(&graph(), eligible, &mut WorkBudget::new(units))
                        .unwrap_err(),
                    BeaconError::WorkBudgetExceeded
                );
            }
            assert_eq!(
                q.fuse_graph_only(&graph(), eligible, &mut WorkBudget::new(count.0))
                    .unwrap(),
                expected
            );
        }
    }
}
