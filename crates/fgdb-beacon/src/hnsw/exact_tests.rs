//! Unbounded scalar distances plus a full sort are the independent selection
//! oracle. Tests retain original work/cancel traces, not merely equal top-k IDs.
use super::*;
use crate::WorkBudget;

fn bits(rows: &[Neighbor]) -> Vec<(VId, u64)> {
    rows.iter()
        .map(|row| (row.id, row.distance.to_bits()))
        .collect()
}

fn full_scan(
    graph: &Hnsw,
    query: &[f32],
    k: usize,
    eligible: impl Fn(VId) -> bool,
    work: &mut dyn WorkControl,
) -> Result<Vec<Neighbor>, BeaconError> {
    let norm = graph.config.validate_vector(query, work)?;
    let k = k.min(graph.nodes.len());
    if k == 0 {
        return Ok(Vec::new());
    }
    let mut rows = Vec::new();
    for slot in 0..graph.nodes.len() {
        work.charge(1)?;
        if eligible(graph.nodes[slot].id) {
            rows.push(graph.candidate(query, norm, slot, work)?);
        }
    }
    // No bounded heap, cutoff or optimized candidate call in this oracle.
    rows.sort_unstable();
    rows.truncate(k);
    Ok(rows
        .into_iter()
        .map(|row| Neighbor {
            id: row.id,
            distance: row.cost,
        })
        .collect())
}

fn graph(dimensions: usize, metric: DistanceMetric, links: bool) -> Hnsw {
    let mut config = HnswConfig::new(dimensions, metric);
    config.links = links;
    Hnsw::build(
        config,
        (0..48_u128).rev().map(|id| {
            // Full-width, noncontiguous IDs and repeated vector distances.
            let vid = VId((id << 96) | (47 - id));
            let mut vector: Vec<_> = (0..dimensions)
                .map(|axis| ((id % 17 * 31 + axis as u128 * 13) % 23) as f32 - 11.0)
                .collect();
            vector[0] = (id % 7 + 1) as f32;
            (vid, vector)
        }),
        &mut WorkBudget::new(usize::MAX),
    )
    .unwrap()
}

#[derive(Default)]
struct Trace {
    charges: Vec<usize>,
    stop: Option<(usize, BeaconError)>,
}

impl WorkControl for Trace {
    fn charge(&mut self, units: usize) -> Result<(), BeaconError> {
        let at = self.charges.len();
        self.charges.push(units);
        if let Some((cutoff, error)) = &self.stop
            && at == *cutoff
        {
            return Err(error.clone());
        }
        Ok(())
    }
}

#[test]
fn exact_top_k_matches_full_sort_for_metrics_filters_and_link_profiles() {
    for dimensions in [1, 16, 17, 129] {
        for metric in [
            DistanceMetric::SquaredEuclidean,
            DistanceMetric::Cosine,
            DistanceMetric::NegativeDotProduct,
        ] {
            for links in [false, true] {
                let graph = graph(dimensions, metric, links);
                let query = vec![1.25; dimensions];
                for k in [0, 1, 7, 48, 100] {
                    for selection in 0..4 {
                        let select = |id: VId| match selection {
                            0 => true,
                            1 => id.0.is_multiple_of(3),
                            2 => id == graph.nodes[47].id,
                            _ => false,
                        };
                        let optimized_seen = std::cell::RefCell::new(Vec::new());
                        let original_seen = std::cell::RefCell::new(Vec::new());
                        let mut optimized = Trace::default();
                        let actual = graph
                            .search(
                                &query,
                                k,
                                VectorSearch::Exact,
                                |id| {
                                    optimized_seen.borrow_mut().push(id);
                                    select(id)
                                },
                                &mut optimized,
                            )
                            .unwrap();
                        let mut original = Trace::default();
                        let expected = full_scan(
                            &graph,
                            &query,
                            k,
                            |id| {
                                original_seen.borrow_mut().push(id);
                                select(id)
                            },
                            &mut original,
                        )
                        .unwrap();
                        assert_eq!(
                            bits(&actual),
                            bits(&expected),
                            "dimensions={dimensions} metric={metric:?} links={links} k={k} filter={selection}"
                        );
                        assert_eq!(optimized.charges, original.charges);
                        assert_eq!(optimized_seen.into_inner(), original_seen.into_inner());
                    }
                }
            }
        }
    }
}

#[test]
fn losing_l2_suffixes_are_not_evaluated_but_are_still_admitted() {
    let dimensions = 1024;
    let graph = Hnsw::build(
        HnswConfig::new(dimensions, DistanceMetric::SquaredEuclidean).exact_only(),
        (0..64_u128).map(|id| (VId(id), vec![id as f32; dimensions])),
        &mut WorkBudget::new(usize::MAX),
    )
    .unwrap();
    let query = vec![0.0; dimensions];
    DISTANCE_COORDINATES.with(|used| used.set(0));
    let mut optimized = Trace::default();
    let actual = graph
        .search(&query, 1, VectorSearch::Exact, |_| true, &mut optimized)
        .unwrap();
    let pruned_coordinates = DISTANCE_COORDINATES.with(std::cell::Cell::get);
    assert_eq!(pruned_coordinates, dimensions + 63 * 16);
    DISTANCE_COORDINATES.with(|used| used.set(0));
    let mut original = Trace::default();
    let expected = full_scan(&graph, &query, 1, |_| true, &mut original).unwrap();
    assert_eq!(
        DISTANCE_COORDINATES.with(std::cell::Cell::get),
        64 * dimensions
    );
    assert_eq!(bits(&actual), bits(&expected));
    assert_eq!(optimized.charges, original.charges);
}

#[test]
fn every_original_cancellation_and_work_boundary_still_refuses() {
    let graph = graph(129, DistanceMetric::SquaredEuclidean, false);
    let query = vec![1.0; 129];
    let mut trace = Trace::default();
    let expected = full_scan(&graph, &query, 3, |_| true, &mut trace).unwrap();
    for reason in [BeaconError::Cancelled, BeaconError::WorkBudgetExceeded] {
        for cutoff in 0..=trace.charges.len() {
            let mut optimized = Trace {
                stop: Some((cutoff, reason.clone())),
                ..Trace::default()
            };
            let mut original = Trace {
                stop: Some((cutoff, reason.clone())),
                ..Trace::default()
            };
            let actual = graph.search(&query, 3, VectorSearch::Exact, |_| true, &mut optimized);
            let reference = full_scan(&graph, &query, 3, |_| true, &mut original);
            assert_eq!(optimized.charges, original.charges);
            assert_eq!(
                actual.as_ref().map(|rows| bits(rows)),
                reference.as_ref().map(|rows| bits(rows))
            );
            if cutoff < trace.charges.len() {
                assert_eq!(actual, Err(reason.clone()));
            } else {
                assert_eq!(bits(&actual.unwrap()), bits(&expected));
            }
        }
    }
    let units: usize = trace.charges.iter().sum();
    for allowance in [0, 1, units - 1, units, units + 1] {
        let mut optimized = WorkBudget::new(allowance);
        let mut original = WorkBudget::new(allowance);
        let actual = graph.search(&query, 3, VectorSearch::Exact, |_| true, &mut optimized);
        let expected = full_scan(&graph, &query, 3, |_| true, &mut original);
        assert_eq!(actual, expected);
        assert_eq!(optimized.remaining(), original.remaining());
    }
}

#[test]
fn equal_cutoffs_finish_the_sum_and_keep_full_precision_scores() {
    for dimensions in [1, 15, 16, 17, 65, 257] {
        for coordinate in [
            0.0_f32,
            -0.0,
            f32::from_bits(1),
            -f32::from_bits(1),
            0.1,
            -2.5,
            f32::MAX,
            -f32::MAX,
        ] {
            let query = vec![coordinate; dimensions];
            let mut vector = vec![-coordinate; dimensions];
            // Zero suffixes ensure equality can occur BEFORE the final block.
            for value in vector.iter_mut().skip(16) {
                *value = coordinate;
            }
            let graph = Hnsw::build(
                HnswConfig::new(dimensions, DistanceMetric::SquaredEuclidean).exact_only(),
                [(VId(u128::MAX), vector)],
                &mut WorkBudget::new(usize::MAX),
            )
            .unwrap();
            let norm = graph
                .config
                .validate_vector(&query, &mut WorkBudget::new(usize::MAX))
                .unwrap();
            let reference = graph
                .candidate(&query, norm, 0, &mut WorkBudget::new(usize::MAX))
                .unwrap();
            let cost = reference.cost;
            let below = if cost == 0.0 {
                -f64::from_bits(1)
            } else {
                f64::from_bits(cost.to_bits() - 1)
            };
            let above = f64::from_bits(cost.to_bits() + 1);
            for cutoff in [below, cost, above, f64::INFINITY] {
                let actual = graph
                    .exact_candidate(
                        &query,
                        norm,
                        0,
                        Some(cutoff),
                        &mut WorkBudget::new(usize::MAX),
                    )
                    .unwrap();
                if cost > cutoff {
                    assert!(actual.is_none());
                } else {
                    let actual = actual.unwrap();
                    assert_eq!(actual.cost.to_bits(), reference.cost.to_bits());
                    assert_eq!((actual.id, actual.slot), (reference.id, reference.slot));
                }
            }
        }
    }
}

#[test]
fn nonmonotone_metrics_never_take_the_l2_rejection_path() {
    for metric in [DistanceMetric::NegativeDotProduct, DistanceMetric::Cosine] {
        let graph = graph(129, metric, false);
        let query = vec![1.0; 129];
        let norm = graph
            .config
            .validate_vector(&query, &mut WorkBudget::new(usize::MAX))
            .unwrap();
        for slot in 0..graph.nodes.len() {
            let expected = graph
                .candidate(&query, norm, slot, &mut WorkBudget::new(usize::MAX))
                .unwrap();
            DISTANCE_COORDINATES.with(|used| used.set(0));
            let actual = graph
                .exact_candidate(
                    &query,
                    norm,
                    slot,
                    Some(-f64::MAX),
                    &mut WorkBudget::new(usize::MAX),
                )
                .unwrap()
                .unwrap();
            assert_eq!(actual.cost.to_bits(), expected.cost.to_bits());
            assert_eq!(DISTANCE_COORDINATES.with(std::cell::Cell::get), 129);
        }
    }
}

#[test]
fn validation_still_checks_late_coordinates_before_any_candidate_shortcut() {
    let graph = graph(129, DistanceMetric::SquaredEuclidean, false);
    for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut query = vec![0.0; 129];
        query[128] = invalid;
        for k in [0, 1, 100] {
            assert_eq!(
                graph.search(
                    &query,
                    k,
                    VectorSearch::Exact,
                    |_| false,
                    &mut WorkBudget::new(usize::MAX)
                ),
                Err(BeaconError::NonFinite { coordinate: 128 })
            );
        }
        assert!(matches!(
            Hnsw::build(
                HnswConfig::new(129, DistanceMetric::SquaredEuclidean).exact_only(),
                [(VId(1), vec![0.0; 129]), (VId(2), query)],
                &mut WorkBudget::new(usize::MAX)
            ),
            Err(BeaconError::NonFinite { coordinate: 128 })
        ));
    }
}
