use fgdb_beacon::{
    BeaconError, DistanceMetric, Hnsw, HnswConfig, VectorSearch, WorkBudget, WorkControl,
};
use fgdb_types::VId;

fn budget() -> WorkBudget {
    WorkBudget::new(100_000_000)
}

fn line() -> Hnsw {
    Hnsw::build(
        HnswConfig::new(2, DistanceMetric::SquaredEuclidean),
        (0..100).map(|id| (VId(id), vec![id as f32, 0.0])),
        &mut budget(),
    ).unwrap()
}

#[test]
fn approximate_line_agrees_with_exact() {
    let graph = line();
    for x in [0.0, 11.1, 42.4, 98.9] {
        let exact = graph.search(&[x, 0.0], 10, VectorSearch::Exact, |_| true, &mut budget()).unwrap();
        let approximate = graph.search(&[x, 0.0], 10, VectorSearch::Approximate { ef_search: 100 }, |_| true, &mut budget()).unwrap();
        assert_eq!(approximate, exact);
    }
}

#[test]
fn filtered_vertices_do_not_consume_result_capacity() {
    let graph = line();
    let answer = graph.search(&[0.0, 0.0], 1, VectorSearch::Approximate { ef_search: 1 }, |id| id == VId(99), &mut budget()).unwrap();
    assert_eq!(answer.len(), 1);
    assert_eq!(answer[0].id, VId(99));
    assert_eq!(answer[0].distance, 9801.0);
}

#[test]
fn no_eligible_vertex_is_an_empty_answer() {
    let graph = line();
    for mode in [VectorSearch::Exact, VectorSearch::Approximate { ef_search: 10 }] {
        assert!(graph.search(&[0.0, 0.0], 10, mode, |_| false, &mut budget()).unwrap().is_empty());
    }
}

#[test]
fn equal_distance_ties_use_vertex_id() {
    let graph = Hnsw::build(HnswConfig::new(2, DistanceMetric::SquaredEuclidean), [
        (VId(7), vec![0.0, 1.0]), (VId(3), vec![1.0, 0.0]), (VId(9), vec![-1.0, 0.0]),
    ], &mut budget()).unwrap();
    for mode in [VectorSearch::Exact, VectorSearch::Approximate { ef_search: 3 }] {
        let answer = graph.search(&[0.0, 0.0], 2, mode, |_| true, &mut budget()).unwrap();
        assert_eq!(answer.iter().map(|hit| hit.id).collect::<Vec<_>>(), [VId(3), VId(7)]);
    }
}

#[test]
fn cosine_and_dot_product_have_explicit_distance_semantics() {
    let rows = [(VId(1), vec![1.0, 0.0]), (VId(2), vec![2.0, 0.0]), (VId(3), vec![0.0, 1.0])];
    let cosine = Hnsw::build(HnswConfig::new(2, DistanceMetric::Cosine), rows.clone(), &mut budget()).unwrap();
    let hits = cosine.search(&[1.0, 0.0], 3, VectorSearch::Exact, |_| true, &mut budget()).unwrap();
    assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [VId(1), VId(2), VId(3)]);
    assert_eq!(hits[0].distance, 0.0);
    assert_eq!(hits[2].distance, 1.0);
    let dot = Hnsw::build(HnswConfig::new(2, DistanceMetric::NegativeDotProduct), rows, &mut budget()).unwrap();
    let hits = dot.search(&[1.0, 0.0], 1, VectorSearch::Exact, |_| true, &mut budget()).unwrap();
    assert_eq!(hits[0].id, VId(2));
    assert_eq!(hits[0].distance, -2.0);
}

#[test]
fn invalid_vectors_are_rejected_even_on_zero_limit_queries() {
    let graph = line();
    assert_eq!(graph.search(&[0.0], 0, VectorSearch::Exact, |_| true, &mut budget()).unwrap_err(), BeaconError::Dimension { expected: 2, actual: 1 });
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert!(matches!(graph.search(&[value, 0.0], 0, VectorSearch::Exact, |_| true, &mut budget()), Err(BeaconError::NonFinite { .. })));
        assert!(matches!(Hnsw::build(HnswConfig::new(2, DistanceMetric::Cosine), [(VId(1), vec![value, 0.0])], &mut budget()), Err(BeaconError::NonFinite { .. })));
    }
    assert!(matches!(Hnsw::build(HnswConfig::new(2, DistanceMetric::Cosine), [(VId(1), vec![0.0, 0.0])], &mut budget()), Err(BeaconError::ZeroVector)));
}

#[test]
fn empty_index_preserves_query_validation() {
    let graph = Hnsw::build(HnswConfig::new(2, DistanceMetric::Cosine), [], &mut budget()).unwrap();
    assert!(graph.is_empty());
    assert!(graph.search(&[1.0, 0.0], 10, VectorSearch::Exact, |_| true, &mut budget()).unwrap().is_empty());
    assert!(matches!(graph.search(&[0.0, 0.0], 0, VectorSearch::Exact, |_| true, &mut budget()), Err(BeaconError::ZeroVector)));
    assert!(matches!(graph.search(&[1.0, 0.0], 0, VectorSearch::Approximate { ef_search: 0 }, |_| true, &mut budget()), Err(BeaconError::InvalidQuery(_))));
}

#[test]
fn duplicate_ids_and_invalid_configuration_fail() {
    assert!(matches!(Hnsw::build(HnswConfig::new(1, DistanceMetric::SquaredEuclidean), [(VId(1), vec![1.0]), (VId(1), vec![2.0])], &mut budget()), Err(BeaconError::DuplicateVertex(VId(1)))));
    for dimensions in [0, 65_537] {
        assert!(HnswConfig::new(dimensions, DistanceMetric::Cosine).validate().is_err());
    }
    let mut config = HnswConfig::new(1, DistanceMetric::SquaredEuclidean);
    config.max_vectors = 1;
    assert!(matches!(Hnsw::build(config, [(VId(1), vec![1.0]), (VId(2), vec![2.0])], &mut budget()), Err(BeaconError::ResourceLimit { .. })));
}

#[test]
fn f32_extremes_do_not_overflow_distance_accumulation() {
    for metric in [DistanceMetric::SquaredEuclidean, DistanceMetric::Cosine, DistanceMetric::NegativeDotProduct] {
        let graph = Hnsw::build(HnswConfig::new(2, metric), [(VId(1), vec![f32::MAX, f32::MAX])], &mut budget()).unwrap();
        let answer = graph.search(&[-f32::MAX, f32::MAX], 1, VectorSearch::Exact, |_| true, &mut budget()).unwrap();
        assert!(answer[0].distance.is_finite());
    }
}

#[test]
fn work_exhaustion_and_cancellation_never_return_partial_answers() {
    let graph = line();
    assert_eq!(graph.search(&[42.0, 0.0], 20, VectorSearch::Exact, |_| true, &mut WorkBudget::new(10)).unwrap_err(), BeaconError::WorkBudgetExceeded);
    struct Cancel;
    impl WorkControl for Cancel {
        fn charge(&mut self, _: usize) -> Result<(), BeaconError> { Err(BeaconError::Cancelled) }
    }
    assert_eq!(graph.search(&[42.0, 0.0], 20, VectorSearch::Exact, |_| true, &mut Cancel).unwrap_err(), BeaconError::Cancelled);
    assert_eq!(graph.len(), 100);
}

#[test]
fn debug_does_not_dump_vectors() {
    let graph = Hnsw::build(HnswConfig::new(1, DistanceMetric::SquaredEuclidean), [(VId(7), vec![123456.25])], &mut budget()).unwrap();
    assert!(!format!("{graph:?}").contains("123456.25"));
}

#[test]
fn full_width_vertex_identities_are_distinct_and_totally_ordered() {
    let low = VId(7);
    let high = VId((1u128 << 96) | 7);
    let graph = Hnsw::build(
        HnswConfig::new(1, DistanceMetric::SquaredEuclidean),
        [(high, vec![1.0]), (low, vec![1.0])], &mut budget(),
    ).unwrap();
    for mode in [VectorSearch::Exact, VectorSearch::Approximate { ef_search: 2 }] {
        let hits = graph.search(&[0.0], 2, mode, |_| true, &mut budget()).unwrap();
        assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [low, high]);
    }
}
