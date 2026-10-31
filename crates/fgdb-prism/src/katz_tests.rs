use super::*;
use crate::{
    Directedness, FnxCallSpec, FnxExecutionLimits, FnxValue, ParallelEdgePolicy, ProjectionEdge,
    ProjectionLimits, ProjectionSpec, SelfLoopPolicy, SnapshotBinding,
};
use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
use fnx_classes::{Graph, digraph::DiGraph};
use std::collections::BTreeMap;
use std::convert::Infallible;

fn graph(n: usize, pairs: &[(usize, usize)], direction: Directedness) -> SnapshotGraphView {
    let vertices: Vec<_> = (0..n)
        .map(|index| VId((1u128 << 112) | ((n - index) as u128) << 64 | index as u128))
        .collect();
    let edges: Vec<_> = pairs
        .iter()
        .enumerate()
        .map(|(id, &(source, target))| ProjectionEdge {
            eid: EId((1u128 << 120) | id as u128),
            source: vertices[source],
            target: vertices[target],
            // The unweighted signature must ignore finite signed weights.
            weight: [-7.0, 0.0, 2.0, 11.0][id % 4],
        })
        .collect();
    SnapshotGraphView::build(
        SnapshotBinding {
            root: ObjectId([0x6a; 32]),
            as_of: CommitSeq(7),
        },
        &vertices,
        &edges,
        ProjectionSpec {
            directedness: direction,
            parallel_edges: ParallelEdgePolicy::Sum,
            self_loops: SelfLoopPolicy::Keep,
        },
        ProjectionLimits {
            max_vertices: n,
            max_input_edges: edges.len(),
            max_adjacency_entries: 2 * edges.len(),
            max_workspace_bytes: 1 << 28,
        },
    )
    .unwrap()
}

fn limits() -> FnxExecutionLimits {
    FnxExecutionLimits {
        max_iterations: 1000,
        max_result_rows: 10_000,
        max_estimated_work: 1 << 30,
    }
}

fn bits(result: &crate::FnxResult) -> Vec<u64> {
    result
        .rows
        .iter()
        .map(|row| match row[1] {
            FnxValue::Score(value) => value.to_bits(),
            _ => panic!("score column"),
        })
        .collect()
}

fn compare_pinned(view: &SnapshotGraphView) {
    let names = view.nodes_ordered();
    let expected = if view.is_directed() {
        let mut oracle = DiGraph::strict();
        for name in names.iter().rev() {
            oracle.add_node(*name);
        }
        for source in 0..names.len() {
            for &target in view.neighbors_indices(source).unwrap() {
                oracle.add_edge(names[source], names[target]).unwrap();
            }
        }
        fnx_algorithms::katz_centrality_directed_checked(&oracle)
    } else {
        let mut oracle = Graph::strict();
        for name in names.iter().rev() {
            oracle.add_node(*name);
        }
        for source in 0..names.len() {
            for &target in view.neighbors_indices(source).unwrap() {
                if source <= target {
                    oracle.add_edge(names[source], names[target]).unwrap();
                }
            }
        }
        fnx_algorithms::katz_centrality_checked(&oracle)
    };
    let actual =
        FnxCallSpec::katz_centrality(KatzOptions::default())
            .execute(view, limits(), || Ok::<_, Infallible>(()));
    match (actual, expected) {
        (Ok(actual), Ok(expected)) => {
            assert_eq!(actual.certificate.witness, expected.witness);
            let expected: BTreeMap<_, _> = expected
                .scores
                .into_iter()
                .map(|score| (score.node, score.score.to_bits()))
                .collect();
            let expected: Vec<_> = names.iter().map(|name| expected[*name]).collect();
            assert_eq!(bits(&actual), expected);
            assert_eq!(actual.rows.len(), view.node_count());
            assert_eq!(
                actual.certificate.kernel_workspace_bytes,
                workspace::<Infallible>(view.node_count(), KatzOptions::default()).unwrap()
            );
        }
        (
            Err(FnxExecutionError::NotConverged {
                max_iterations,
                witness,
            }),
            Err(expected),
        ) => {
            assert_eq!(max_iterations, expected.iterations);
            assert_eq!(
                witness.nodes_touched,
                view.node_count() * expected.iterations
            );
            assert_eq!(
                witness.edges_scanned,
                view.outgoing_offsets().last().unwrap() * expected.iterations
            );
        }
        (actual, expected) => panic!("native {actual:?}, pinned {expected:?}"),
    }
}

#[test]
fn defaults_match_checked_fnx_for_all_small_looped_graphs_and_directions() {
    for direction in [Directedness::Directed, Directedness::Undirected] {
        let max_n = if direction == Directedness::Directed {
            3
        } else {
            4
        };
        for n in 0..=max_n {
            let possible: Vec<_> = (0..n)
                .flat_map(|source| (0..n).map(move |target| (source, target)))
                .filter(|&(source, target)| direction == Directedness::Directed || source <= target)
                .collect();
            for mask in 0usize..(1 << possible.len()) {
                let pairs: Vec<_> = possible
                    .iter()
                    .enumerate()
                    .filter_map(|(bit, &pair)| (mask & (1 << bit) != 0).then_some(pair))
                    .collect();
                compare_pinned(&graph(n, &pairs, direction));
            }
        }
    }
    compare_pinned(&graph(
        7,
        &[(0, 1), (1, 0), (1, 2), (2, 2), (1, 2), (2, 4), (4, 5)],
        Directedness::Reversed,
    ));
}

/// Independent target-major dense matrix oracle. Every destination sums its
/// sources in canonical order, without the production state machine, buffer
/// swapping, row driver, or the FNX implementation's adjacency cache.
fn matrix_reference(view: &SnapshotGraphView, options: KatzOptions) -> Option<(Vec<f64>, usize)> {
    let n = view.node_count();
    if n == 0 {
        return Some((Vec::new(), 0));
    }
    if n == 1 && options == KatzOptions::default() {
        return Some((vec![1.0], 0));
    }
    let matrix: Vec<Vec<bool>> = (0..n)
        .map(|source| {
            (0..n)
                .map(|target| {
                    view.neighbors_indices(source)
                        .unwrap()
                        .binary_search(&target)
                        .is_ok()
                })
                .collect()
        })
        .collect();
    let mut scores = vec![0.0; n];
    for iteration in 1..=options.max_iter() {
        let mut next = vec![0.0; n];
        for target in 0..n {
            let mut sum = 0.0;
            for source in 0..n {
                if matrix[source][target] {
                    sum += scores[source];
                }
            }
            next[target] = options.alpha() * sum + options.beta();
        }
        let delta = next
            .iter()
            .zip(&scores)
            .map(|(a, b)| (a - b).abs())
            .sum::<f64>();
        scores = next;
        if delta < n as f64 * options.tolerance() {
            let norm = scores.iter().map(|x| x * x).sum::<f64>().sqrt();
            let norm = if norm > 0.0 { norm } else { 1.0 };
            return Some((scores.into_iter().map(|x| x / norm).collect(), iteration));
        }
    }
    None
}

#[test]
fn configurable_katz_matches_independent_matrix_oracle_and_works_on_dense_graphs() {
    let mut pairs = vec![(0, 1), (1, 2), (2, 2), (2, 3), (4, 0), (4, 0), (3, 1)];
    for direction in [
        Directedness::Directed,
        Directedness::Reversed,
        Directedness::Undirected,
    ] {
        let view = graph(7, &pairs, direction);
        for options in [
            KatzOptions::new(0.0, -2.0, 100, 1e-12).unwrap(),
            KatzOptions::new(0.05, 0.75, 1000, 1e-12).unwrap(),
            KatzOptions::new(0.05, 0.0, 1, 1e-12).unwrap(),
            KatzOptions::new(0.25, -0.75, 1000, 1e-10).unwrap(),
        ] {
            let (expected, iterations) = matrix_reference(&view, options).unwrap();
            let actual = FnxCallSpec::katz_centrality(options)
                .execute(&view, limits(), || Ok::<_, Infallible>(()))
                .unwrap();
            assert_eq!(
                bits(&actual),
                expected.iter().map(|x| x.to_bits()).collect::<Vec<_>>()
            );
            assert_eq!(actual.certificate.witness.nodes_touched, 7 * iterations);
        }
    }
    pairs = (0..16).flat_map(|a| (0..a).map(move |b| (a, b))).collect();
    let view = graph(16, &pairs, Directedness::Undirected);
    compare_pinned(&view); // alpha=.1 does not converge at degree 15.
    let options = KatzOptions::new(0.025, 1.0, 1000, 1e-12).unwrap();
    let (expected, _) = matrix_reference(&view, options).unwrap();
    let actual = FnxCallSpec::katz_centrality(options)
        .execute(&view, limits(), || Ok::<_, Infallible>(()))
        .unwrap();
    assert_eq!(
        bits(&actual),
        expected.iter().map(|x| x.to_bits()).collect::<Vec<_>>()
    );
    assert_eq!(
        actual.certificate.kernel_workspace_bytes,
        2 * 16 * size_of::<f64>()
    );
}

#[test]
fn custom_singletons_execute_recurrence_and_unrepresentable_norm_never_publishes_scores() {
    for pairs in [vec![], vec![(0, 0)]] {
        let view = graph(1, &pairs, Directedness::Directed);
        for options in [
            KatzOptions::new(0.1, -2.0, 1000, 1e-10).unwrap(),
            KatzOptions::new(0.1, 0.0, 1, 1e-10).unwrap(),
        ] {
            let (expected, iterations) = matrix_reference(&view, options).unwrap();
            let actual = FnxCallSpec::katz_centrality(options)
                .execute(&view, limits(), || Ok::<_, Infallible>(()))
                .unwrap();
            assert_eq!(
                bits(&actual),
                expected.iter().map(|x| x.to_bits()).collect::<Vec<_>>()
            );
            assert_eq!(actual.certificate.witness.nodes_touched, iterations);
            assert_eq!(
                actual.certificate.kernel_workspace_bytes,
                2 * size_of::<f64>()
            );
        }
    }
    let view = graph(1, &[(0, 0)], Directedness::Directed);
    let call = FnxCallSpec::katz_centrality(KatzOptions::new(1.0, 1.0, 9, 1e-12).unwrap());
    assert!(matches!(
        call.execute(&view, limits(), || Ok::<_, Infallible>(())),
        Err(FnxExecutionError::NotConverged { max_iterations: 9, witness })
            if witness.nodes_touched == 9 && witness.edges_scanned == 9
    ));
    for beta in [f64::MAX, f64::MIN_POSITIVE] {
        let call = FnxCallSpec::katz_centrality(KatzOptions::new(0.0, beta, 2, 1e-12).unwrap());
        assert!(matches!(
            call.execute(&view, limits(), || Ok::<_, Infallible>(())),
            Err(FnxExecutionError::InvalidNumericResult)
        ));
    }
}

#[test]
fn every_decoded_live_cut_refuses_before_result_and_scalar_work_is_admitted() {
    let view = graph(4, &[(0, 1), (1, 2), (2, 0)], Directedness::Directed);
    let options = KatzOptions::new(0.125, 1.0, 100, 1e-10).unwrap();
    let call = FnxCallSpec::katz_centrality(options);
    let mut checks = 0;
    let result = call
        .execute(&view, limits(), || {
            checks += 1;
            Ok::<_, usize>(())
        })
        .unwrap();
    assert!(checks <= result.certificate.estimated_work);
    for stop in 1..=checks {
        let mut seen = 0;
        let actual = call.execute(&view, limits(), || {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(actual, Err(FnxExecutionError::Cancelled(at)) if at == stop));
        assert_eq!(seen, stop);
    }
    let exact = FnxExecutionLimits {
        max_iterations: options.max_iter(),
        max_result_rows: view.node_count(),
        max_estimated_work: result.certificate.estimated_work,
    };
    for resource in ["iterations", "result rows", "estimated work"] {
        let mut refused = exact;
        match resource {
            "iterations" => refused.max_iterations -= 1,
            "result rows" => refused.max_result_rows -= 1,
            _ => refused.max_estimated_work -= 1,
        }
        assert!(matches!(
            call.execute(&view, refused, || Ok::<_, Infallible>(())),
            Err(FnxExecutionError::LimitExceeded { resource: actual, .. }) if resource == actual
        ));
    }
    assert!(matches!(
        Iteration::new::<Infallible>(4, options, 8 * 4 * 2 - 1),
        Err(FnxExecutionError::LimitExceeded {
            resource: "kernel workspace bytes",
            ..
        })
    ));
    assert!(matches!(
        estimated_work::<Infallible>(usize::MAX, 0, 0, 2, options),
        Err(FnxExecutionError::SizeOverflow)
    ));
}
