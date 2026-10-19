//! Differential and interruption laws for the borrowed-CSR eigenvector kernel.

use super::*;
use crate::{
    Directedness, ParallelEdgePolicy, ProjectionEdge, ProjectionLimits, ProjectionSpec,
    SelfLoopPolicy, SnapshotBinding,
};
use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
use std::collections::BTreeMap;

#[derive(Default)]
struct Polls {
    calls: usize,
    refuse_at: Option<usize>,
}
impl FnxMeter<usize> for Polls {
    fn unit(&mut self) -> Result<(), usize> {
        panic!("native arithmetic must not recharge the foundation's logical work")
    }
    fn poll(&mut self) -> Result<(), usize> {
        self.calls += 1;
        if self.refuse_at == Some(self.calls) {
            Err(self.calls)
        } else {
            Ok(())
        }
    }
}

fn identities(n: usize) -> Vec<VId> {
    // Every ID has significant high bits. Input order is the reverse of
    // canonical name/ordinal order, so truncation or insertion-order mapping
    // changes which vertex receives every nonuniform score.
    (0..n)
        .map(|i| VId(((n - i + 1) as u128) << 96 | 1u128 << 80 | (i as u128) << 32 | 0xf00d))
        .collect()
}

fn view_with_ids(ids: &[VId], pairs: &[(usize, usize)]) -> SnapshotGraphView {
    let edges: Vec<ProjectionEdge> = pairs
        .iter()
        .enumerate()
        .map(|(i, &(source, target))| ProjectionEdge {
            eid: EId((1u128 << 112) | i as u128 + 1),
            source: ids[source],
            target: ids[target],
            weight: 1.0,
        })
        .collect();
    SnapshotGraphView::build(
        SnapshotBinding {
            root: ObjectId([29; 32]),
            as_of: CommitSeq(11),
        },
        ids,
        &edges,
        ProjectionSpec {
            directedness: Directedness::Undirected,
            parallel_edges: ParallelEdgePolicy::Reject,
            self_loops: SelfLoopPolicy::Keep,
        },
        ProjectionLimits {
            max_vertices: ids.len(),
            max_input_edges: edges.len(),
            max_adjacency_entries: edges.len() * 2,
            max_workspace_bytes: 1 << 28,
        },
    )
    .unwrap()
}

fn view(n: usize, pairs: &[(usize, usize)]) -> SnapshotGraphView {
    view_with_ids(&identities(n), pairs)
}

/// Reconstruct exactly the incumbent foundation boundary, then call its
/// public checked entry point. This oracle is independent of the native
/// gather and uses the actual pinned Graph and algorithm implementation.
fn oracle(graph: &SnapshotGraphView) -> (Vec<f64>, ComplexityWitness, bool) {
    let names = graph.nodes_ordered();
    let mut copy = fnx_classes::Graph::strict();
    for name in &names {
        copy.add_node(*name);
    }
    for source in 0..names.len() {
        for &target in graph.neighbors_indices(source).unwrap() {
            if source < target {
                copy.add_edge(names[source], names[target]).unwrap();
            }
        }
    }
    let (result, converged) = fnx_algorithms::eigenvector_centrality_with_params(
        &copy,
        EIGENVECTOR_MAX_ITERATIONS,
        EIGENVECTOR_TOLERANCE,
    );
    let by_name: BTreeMap<String, f64> = result
        .scores
        .into_iter()
        .map(|score| (score.node, score.score))
        .collect();
    assert_eq!(by_name.len(), names.len());
    let scores = names.iter().map(|name| by_name[*name]).collect();
    (scores, result.witness, converged)
}

fn assert_oracle(graph: &SnapshotGraphView) -> bool {
    let (expected, expected_witness, converged) = oracle(graph);
    let actual = run(graph, &mut Polls::default());
    if converged {
        let (output, bytes) = actual.unwrap_or_else(|error| panic!("native refused: {error:?}"));
        assert_eq!(output.row_count, expected.len());
        assert_eq!(output.witness, expected_witness);
        let KernelValues::Scores(scores) = output.values else {
            panic!("eigenvector returned a non-score column");
        };
        assert_eq!(
            scores
                .iter()
                .map(|score| score.to_bits())
                .collect::<Vec<_>>(),
            expected
                .iter()
                .map(|score| score.to_bits())
                .collect::<Vec<_>>(),
            "every score bit must match the pinned canonical scatter"
        );
        let result_bytes = scores.capacity() * size_of::<f64>();
        if scores.len() <= 1 {
            assert_eq!(bytes, result_bytes);
        } else {
            assert!(
                bytes >= result_bytes + scores.len() * size_of::<f64>(),
                "both live score buffers must be charged"
            );
        }
    } else {
        match actual {
            Err(FnxExecutionError::NotConverged {
                max_iterations,
                witness,
            }) => {
                assert_eq!(max_iterations, EIGENVECTOR_MAX_ITERATIONS);
                assert_eq!(witness, expected_witness);
            }
            Err(error) => panic!("unexpected native refusal: {error:?}"),
            Ok(_) => panic!("native published scores that the pinned oracle did not converge"),
        }
    }
    converged
}

#[test]
fn all_simple_graphs_through_five_vertices_match_every_score_bit_and_witness() {
    for n in 0..=5 {
        let possible: Vec<(usize, usize)> = (0..n)
            .flat_map(|source| (source + 1..n).map(move |target| (source, target)))
            .collect();
        for mask in 0..(1usize << possible.len()) {
            let pairs: Vec<(usize, usize)> = possible
                .iter()
                .enumerate()
                .filter_map(|(bit, &edge)| (mask & (1usize << bit) != 0).then_some(edge))
                .collect();
            assert_oracle(&view(n, &pairs));
        }
    }
}

#[test]
fn disconnected_bipartite_and_skewed_graphs_keep_exact_convergence_and_refusal() {
    for n in [17, 65, 129] {
        let path: Vec<(usize, usize)> = (1..n).map(|v| (v - 1, v)).collect();
        assert_oracle(&view(n, &path));
        let star: Vec<(usize, usize)> = (1..n).map(|v| (0, v)).collect();
        assert_oracle(&view(n, &star));
        let bipartite: Vec<(usize, usize)> = (0..3)
            .flat_map(|source| (3..n - 2).map(move |target| (source, target)))
            .collect();
        assert_oracle(&view(n, &bipartite));
    }
    // A long path retains slow modes after 100 iterations; it must refuse
    // with exactly the checked oracle's iteration witness.
    let n = 129;
    let path: Vec<(usize, usize)> = (1..n).map(|v| (v - 1, v)).collect();
    assert!(!assert_oracle(&view(n, &path)));
}

#[test]
fn empty_singleton_and_edgeless_profiles_keep_pinned_special_cases() {
    assert_oracle(&view(0, &[]));
    assert_oracle(&view(1, &[]));
    assert_oracle(&view(7, &[]));
    let (output, bytes) = run(&view(1, &[]), &mut Polls::default()).unwrap();
    assert_eq!(output.witness.nodes_touched, 1);
    assert_eq!(output.witness.edges_scanned, 0);
    assert_eq!(output.witness.queue_peak, 0);
    let KernelValues::Scores(scores) = output.values else {
        panic!("expected scores");
    };
    assert_eq!(scores[0].to_bits(), 1.0_f64.to_bits());
    assert_eq!(bytes, scores.capacity() * size_of::<f64>());
}

#[test]
fn gathering_adds_in_the_pinned_canonical_source_order_without_reassociation() {
    let n = 73;
    let edges: Vec<(usize, usize)> = (0..n)
        .flat_map(|source| {
            (source + 1..n)
                .filter(move |target| (source * 17 + target * 29) % 7 < 3)
                .map(move |target| (source, target))
        })
        .collect();
    let graph = view(n, &edges);
    // A wide exponent range makes changing the order of source additions
    // visible; a uniform starting vector alone would not expose that bug.
    let scores: Vec<f64> = (0..n)
        .map(|i| f64::from_bits((((i * 37) % 96 + 975) as u64) << 52 | i as u64))
        .collect();
    let mut scatter = scores.clone();
    for (source, &score) in scores.iter().enumerate() {
        for &target in graph.neighbors_indices(source).unwrap() {
            scatter[target] += score;
        }
    }
    let mut gathered = vec![f64::NAN; n];
    let mut meter = Polls::default();
    let mut arithmetic = Arithmetic::new(&mut meter);
    gather(&graph, &scores, &mut gathered, &mut arithmetic).unwrap();
    arithmetic.finish::<usize>().unwrap();
    assert_eq!(
        gathered
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>(),
        scatter
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>()
    );
}

#[test]
fn live_cancellation_interrupts_inside_the_first_hub_row() {
    let n = 8194;
    // The center is canonical ordinal zero and its single row spans more
    // than two 4096-step live-control intervals.
    let ids: Vec<VId> = (0..n).map(|i| VId((1u128 << 120) | i as u128)).collect();
    let pairs: Vec<(usize, usize)> = (1..n).map(|i| (0, i)).collect();
    let graph = view_with_ids(&ids, &pairs);
    let scores = vec![1.0 / n as f64; n];
    let mut next = vec![f64::NAN; n];
    let mut meter = Polls {
        calls: 0,
        refuse_at: Some(2),
    };
    let mut arithmetic = Arithmetic::new(&mut meter);
    let result = gather(&graph, &scores, &mut next, &mut arithmetic);
    assert!(matches!(result, Err(FnxExecutionError::Cancelled(2))));
    assert!(
        next.iter().all(|value| value.is_nan()),
        "the second poll must interrupt the first row before any destination is written"
    );
}

#[test]
fn every_live_poll_can_cancel_successful_and_nonconvergent_iterations() {
    let star: Vec<(usize, usize)> = (1..65).map(|i| (0, i)).collect();
    let path: Vec<(usize, usize)> = (1..129).map(|i| (i - 1, i)).collect();
    for graph in [view(65, &star), view(129, &path)] {
        let mut counter = Polls::default();
        let baseline = run(&graph, &mut counter);
        assert!(
            baseline.is_ok() || matches!(baseline, Err(FnxExecutionError::NotConverged { .. }))
        );
        assert!(counter.calls > 2, "this graph must poll during arithmetic");
        for stop in 1..=counter.calls {
            let mut meter = Polls {
                calls: 0,
                refuse_at: Some(stop),
            };
            let result = run(&graph, &mut meter);
            assert!(
                matches!(result, Err(FnxExecutionError::Cancelled(at)) if at == stop),
                "live poll {stop} must refuse before a result is returned"
            );
            assert_eq!(meter.calls, stop);
        }
    }
}
