use std::num::NonZeroU32;
use fgdb_beacon::expansion::{ExpansionDirection as D, ExpansionGraph, ExpansionLimits, GraphHit};
use fgdb_beacon::{
    BeaconError, BeaconIndex, DistanceMetric, ExactHybridQuery, ExactRrfProfile,
    ExactRrfScore, GraphHybridQuery, HnswConfig, IndexConfig, IndexDocument,
    TextMatch, VectorSearch, WorkBudget, WorkControl,
};
use fgdb_types::VId;

fn budget() -> WorkBudget { WorkBudget::new(10_000_000) }
fn rank(n: u32) -> Option<NonZeroU32> { NonZeroU32::new(n) }
fn graph(vertices: &[VId], edges: &[(VId, VId)], direction: D, limits: ExpansionLimits) -> ExpansionGraph {
    let mut graph = ExpansionGraph::new(limits);
    for &id in vertices { graph.insert_vertex(id, &mut budget()).unwrap(); }
    for &(a, b) in edges { graph.insert_edge(a, b, direction, &mut budget()).unwrap(); }
    graph
}

#[test]
#[allow(clippy::needless_range_loop)]
fn exhaustive_three_vertex_topologies_match_independent_all_pairs_distances() {
    let vertices = [VId(1), VId(2), VId(u128::MAX)];
    for mask in 0_u16..512 {
        let mut edges = Vec::new();
        let mut shortest = [[4_u32; 3]; 3];
        for i in 0..3 {
            shortest[i][i] = 0;
            for j in 0..3 {
                if mask & (1 << (i * 3 + j)) != 0 {
                    edges.push((vertices[i], vertices[j]));
                    shortest[i][j] = shortest[i][j].min(1);
                }
            }
        }
        for via in 0..3 {
            for i in 0..3 {
                for j in 0..3 {
                    shortest[i][j] = shortest[i][j].min(shortest[i][via] + shortest[via][j]);
                }
            }
        }
        let graph = graph(&vertices, &edges, D::Outgoing, ExpansionLimits::default());
        for seed_mask in 0_u8..8 {
            let mut seeds: Vec<_> = (0..3).filter(|i| seed_mask & (1 << i) != 0)
                .map(|i| vertices[i]).collect();
            seeds.reverse();
            seeds.extend(seeds.clone()); // Duplicate/permuted seeds must not get extra ranks.
            seeds.push(VId(99)); // Missing seed is absent, not an implicit graph vertex.
            for max_hops in 0..=3 {
                for include_seeds in [false, true] {
                    let mut expected = Vec::new();
                    for i in 0..3 {
                        let hops = (0..3).filter(|s| seed_mask & (1 << s) != 0)
                            .map(|s| shortest[s][i]).min().unwrap_or(4);
                        if hops <= max_hops && (include_seeds || hops != 0) {
                            expected.push(GraphHit { id: vertices[i], hops });
                        }
                    }
                    expected.sort_by_key(|hit| (hit.hops, hit.id));
                    for k in [1_u32, 3] {
                        let actual = graph.expand(&seeds, max_hops, include_seeds, k, &mut budget()).unwrap();
                        assert_eq!(actual, expected[..expected.len().min(k as usize)], "mask={mask}, seeds={seed_mask}");
                    }
                }
            }
        }
    }
}

#[test]
fn orientation_parallel_edges_cycles_and_seed_inclusion_have_explicit_laws() {
    let vertices = [VId(1), VId(2), VId(3), VId(4)];
    let edges = [(VId(1), VId(2)), (VId(1), VId(2)), (VId(2), VId(2)), (VId(2), VId(3))];
    for (direction, expected) in [
        (D::Outgoing, vec![GraphHit { id: VId(3), hops: 1 }]),
        (D::Incoming, vec![GraphHit { id: VId(1), hops: 1 }]),
        (D::Undirected, vec![GraphHit { id: VId(1), hops: 1 }, GraphHit { id: VId(3), hops: 1 }]),
    ] {
        let graph = graph(&vertices, &edges, direction, ExpansionLimits::default());
        assert_eq!(graph.expand(&[VId(2)], u32::MAX, false, 4, &mut budget()).unwrap(), expected);
        assert!(graph.expand(&[VId(2)], 0, false, 4, &mut budget()).unwrap().is_empty());
        assert_eq!(graph.expand(&[VId(2)], 0, true, 4, &mut budget()).unwrap(), [GraphHit { id: VId(2), hops: 0 }]);
        assert!(graph.expand(&[VId(99)], 10, true, 4, &mut budget()).unwrap().is_empty());
    }
}

#[test]
fn construction_refusals_are_terminal_and_small_top_k_cannot_hide_incomplete_expansion() {
    let limits = ExpansionLimits { max_vertices: 2, ..ExpansionLimits::default() };
    let mut g = graph(&[VId(1), VId(2)], &[], D::Outgoing, limits);
    let error = g.insert_vertex(VId(3), &mut budget()).unwrap_err();
    assert_eq!(g.expand(&[VId(1)], 0, true, 1, &mut budget()).unwrap_err(), error);
    assert_eq!(g.insert_vertex(VId(1), &mut budget()).unwrap_err(), error);
    let limits = ExpansionLimits { max_input_edges: 1, ..ExpansionLimits::default() };
    let mut g = graph(&[VId(1), VId(2)], &[(VId(1), VId(2))], D::Outgoing, limits);
    assert!(g.insert_edge(VId(1), VId(2), D::Outgoing, &mut budget()).is_err());
    assert!(g.expand(&[VId(1)], 2, true, 1, &mut budget()).is_err());
    let mut g = graph(&[VId(1)], &[], D::Outgoing, ExpansionLimits::default());
    assert!(g.insert_edge(VId(1), VId(99), D::Outgoing, &mut budget()).is_err());
    assert!(g.expand(&[VId(1)], 0, true, 1, &mut budget()).is_err());
    let limits = ExpansionLimits { max_visited_vertices: 2, ..ExpansionLimits::default() };
    let g = graph(&[VId(1), VId(2), VId(3)], &[(VId(1), VId(2)), (VId(2), VId(3))], D::Outgoing, limits);
    assert!(matches!(g.expand(&[VId(1)], 2, true, 1, &mut budget()),
        Err(BeaconError::ResourceLimit { resource: "expansion visited vertices", limit: 2 })));
    assert_eq!(g.expand(&[VId(1)], 1, false, 1, &mut budget()).unwrap(), [GraphHit { id: VId(2), hops: 1 }]);
    let limits = ExpansionLimits { max_seed_ids: 1, ..ExpansionLimits::default() };
    let g = graph(&[VId(1)], &[], D::Outgoing, limits);
    assert!(g.expand(&[VId(1), VId(1)], 0, true, 1, &mut budget()).is_err());
}

#[derive(Default)]
struct Count(usize);
impl WorkControl for Count {
    fn charge(&mut self, units: usize) -> Result<(), BeaconError> { self.0 += units; Ok(()) }
}

#[test]
fn every_shorter_expansion_allowance_fails_without_a_successful_prefix() {
    let g = graph(&[VId(1), VId(2), VId(3)], &[(VId(1), VId(2)), (VId(2), VId(3))], D::Outgoing, ExpansionLimits::default());
    let mut count = Count::default();
    let expected = g.expand(&[VId(1)], 5, true, 3, &mut count).unwrap();
    for units in 0..count.0 {
        assert_eq!(g.expand(&[VId(1)], 5, true, 3, &mut WorkBudget::new(units)).unwrap_err(), BeaconError::WorkBudgetExceeded);
    }
    assert_eq!(g.expand(&[VId(1)], 5, true, 3, &mut WorkBudget::new(count.0)).unwrap(), expected);
    // A failed second arc cannot leave a usable one-directional "undirected" graph.
    let mut g = graph(&[VId(1), VId(2)], &[], D::Outgoing, ExpansionLimits::default());
    assert!(g.insert_edge(VId(1), VId(2), D::Undirected, &mut WorkBudget::new(5)).is_err());
    assert!(g.expand(&[VId(1)], 1, true, 2, &mut budget()).is_err());
}

#[test]
fn three_lane_rationals_are_reduced_and_overflow_paths_match_exact_vectors() {
    let p = ExactRrfProfile::default();
    let score = ExactRrfScore::from_graph_ranks(p, rank(1), rank(1), rank(1), 1);
    assert_eq!((score.numerator(), score.denominator()), (3, 61));
    assert_eq!(ExactRrfScore::from_graph_ranks(p, rank(2), rank(7), rank(9), 0),
        ExactRrfScore::from_ranks(p, rank(2), rank(7)));
    let p = ExactRrfProfile::new(u32::MAX, u16::MAX, u16::MAX).unwrap();
    let a = ExactRrfScore::from_graph_ranks(p, rank(u32::MAX - 4), rank(u32::MAX - 14), rank(u32::MAX - 16), u16::MAX);
    let b = ExactRrfScore::from_graph_ranks(p, rank(u32::MAX - 6), rank(u32::MAX - 12), rank(u32::MAX - 18), u16::MAX);
    // Generated by independent arbitrary-integer rational arithmetic, not f64.
    assert_eq!((a.numerator(), a.denominator()), (3_626_722_107_352_839_133_790_085, 158_456_324_290_658_913_295_267_790_416));
    assert_eq!((b.numerator(), b.denominator()), (67_161_520_496_109_217_986_785, 2_934_376_375_069_730_097_716_624_184));
    assert!(a.numerator().checked_mul(b.denominator()).is_none());
    assert!(a.numerator().checked_mul(10_u128.pow(18)).is_none());
    assert!(a < b);
    assert_eq!(a.cmp(&a), std::cmp::Ordering::Equal);
    assert_eq!(a.decimal().unwrap().coefficient(), 22_887_834_383_311);
    assert_eq!(b.decimal().unwrap().coefficient(), 22_887_834_385_087);
    let permuted = ExactRrfScore::from_graph_ranks(p, rank(u32::MAX - 14), rank(u32::MAX - 16), rank(u32::MAX - 4), u16::MAX);
    assert_eq!(a, permuted);
}

fn index() -> BeaconIndex {
    BeaconIndex::build(IndexConfig {
        vector: Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean)),
        ..IndexConfig::default()
    }, [
        IndexDocument { id: VId(1), vector: Some(vec![0.0]), text: Some("red".into()) },
        IndexDocument { id: VId(2), vector: Some(vec![5.0]), text: Some("red red".into()) },
        IndexDocument { id: VId(3), vector: Some(vec![10.0]), text: Some("blue".into()) },
        IndexDocument { id: VId(4), vector: None, text: None },
    ], &mut budget()).unwrap()
}
fn query() -> GraphHybridQuery<'static> {
    GraphHybridQuery {
        retrieval: ExactHybridQuery {
            vector: &[0.0], text: "red", k: 1, vector_candidates: 3, text_candidates: 0,
            vector_mode: VectorSearch::Exact, text_mode: TextMatch::Any,
            profile: ExactRrfProfile::new(60, 1, 0).unwrap(),
        },
        graph_candidates: 1, graph_weight: 100,
    }
}

#[test]
fn third_lane_can_promote_a_base_loser_and_add_graph_only_candidates() {
    let index = index().snapshot();
    let q = query();
    let base = index.hybrid_search_exact_fusion(q.retrieval, |_| true, &mut budget()).unwrap();
    assert_eq!(base[0].id, VId(1));
    let hits = index.hybrid_search_graph(q, &[GraphHit { id: VId(3), hops: 1 }], &mut budget()).unwrap();
    assert_eq!(hits[0].id, VId(3));
    assert_eq!(hits[0].vector_rank, rank(3));
    assert_eq!(hits[0].graph_rank, rank(1));
    let mut q = q;
    q.retrieval.k = 4; // Greater than the two-lane sum, legal for the three-lane union.
    let hits = index.hybrid_search_graph(q, &[GraphHit { id: VId(4), hops: 2 }], &mut budget()).unwrap();
    assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [VId(4), VId(1), VId(2), VId(3)]);
    assert_eq!(hits[0].vector_rank, None);
    assert_eq!(hits[0].text_rank, None);
    assert_eq!(hits[0].graph_hops, Some(2));
    q.retrieval.k = 5;
    assert!(q.candidate_depths().is_err());
}

#[test]
fn disabled_graph_matches_two_lane_results_and_malformed_graph_ranks_refuse() {
    let index = index().snapshot();
    let mut q = query();
    q.retrieval.k = 3;
    q.graph_weight = 0;
    let malformed = [GraphHit { id: VId(3), hops: 2 }, GraphHit { id: VId(3), hops: 1 }];
    let actual = index.hybrid_search_graph(q, &malformed, &mut budget()).unwrap();
    let expected = index.hybrid_search_exact_fusion(q.retrieval, |_| true, &mut budget()).unwrap();
    for (a, b) in actual.iter().zip(&expected) {
        assert_eq!((a.id, a.score, a.decimal_score, a.vector_rank, a.text_rank), (b.id, b.score, b.decimal_score, b.vector_rank, b.text_rank));
        assert_eq!(a.graph_rank, None);
    }
    assert_eq!(actual.len(), expected.len());
    q.graph_weight = 1;
    q.graph_candidates = 2;
    assert!(index.hybrid_search_graph(q, &malformed, &mut budget()).is_err());
    let duplicates = [GraphHit { id: VId(3), hops: 1 }, GraphHit { id: VId(3), hops: 2 }];
    assert!(index.hybrid_search_graph(q, &duplicates, &mut budget()).is_err());
    q.graph_candidates = 1;
    assert!(index.hybrid_search_graph(q, &duplicates, &mut budget()).is_err());
}

#[test]
fn fusion_budget_is_shared_across_sources_and_final_output() {
    let index = index().snapshot();
    let graph = [GraphHit { id: VId(3), hops: 1 }];
    let mut q = query();
    q.retrieval.profile = ExactRrfProfile::default();
    q.retrieval.text_candidates = 3;
    q.retrieval.k = 3;
    let mut count = Count::default();
    let expected = index.hybrid_search_graph(q, &graph, &mut count).unwrap();
    for units in 0..count.0 {
        assert_eq!(index.hybrid_search_graph(q, &graph, &mut WorkBudget::new(units)).unwrap_err(), BeaconError::WorkBudgetExceeded);
    }
    assert_eq!(index.hybrid_search_graph(q, &graph, &mut WorkBudget::new(count.0)).unwrap(), expected);
}
