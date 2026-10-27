//! Independent fnx/dense controls and real-driver work/cancellation laws.

use super::*;
use crate::{
    Directedness, FnxCallSpec, FnxExecutionLimits, FnxParameters, ParallelEdgePolicy,
    ProjectionEdge, ProjectionLimits, ProjectionSpec, SelfLoopPolicy, SnapshotBinding,
};
use fgdb_types::{CommitSeq, EId, ObjectId, VId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    Unit(usize),
    Poll(usize),
}

#[derive(Default)]
struct Meter {
    units: usize,
    polls: usize,
    stop: Option<Stop>,
}

impl FnxMeter<Stop> for Meter {
    fn unit(&mut self) -> Result<(), Stop> {
        self.units += 1;
        let event = Stop::Unit(self.units);
        if self.stop == Some(event) {
            Err(event)
        } else {
            Ok(())
        }
    }

    fn poll(&mut self) -> Result<(), Stop> {
        self.polls += 1;
        let event = Stop::Poll(self.polls);
        if self.stop == Some(event) {
            Err(event)
        } else {
            Ok(())
        }
    }
}

fn graph(n: usize, pairs: &[(usize, usize)]) -> SnapshotGraphView {
    let vertices: Vec<_> = (0..n)
        .map(|at| {
            if at == 0 {
                VId(0)
            } else if at + 1 == n {
                VId(u128::MAX)
            } else {
                VId((1u128 << 100) + at as u128 * 7919)
            }
        })
        .collect();
    let edges: Vec<_> = pairs
        .iter()
        .enumerate()
        .map(|(at, &(source, target))| ProjectionEdge {
            eid: EId((1u128 << 110) + at as u128),
            source: vertices[source],
            target: vertices[target],
            weight: 1.0,
        })
        .collect();
    SnapshotGraphView::build(
        SnapshotBinding {
            root: ObjectId([73; 32]),
            as_of: CommitSeq(9),
        },
        &vertices,
        &edges,
        ProjectionSpec {
            directedness: Directedness::Undirected,
            parallel_edges: ParallelEdgePolicy::Reject,
            self_loops: SelfLoopPolicy::Keep,
        },
        ProjectionLimits {
            max_vertices: n,
            max_input_edges: edges.len(),
            max_adjacency_entries: edges.len() * 2,
            max_workspace_bytes: 1 << 28,
        },
    )
    .unwrap()
}

fn dense(
    graph: &SnapshotGraphView,
    meter: &mut Meter,
) -> Result<(KernelOutput, usize), FnxExecutionError<Stop>> {
    meter.unit().map_err(FnxExecutionError::Cancelled)?;
    let mut state = Load::new(graph.node_count(), usize::MAX)?;
    // Only the two formerly dense passes differ. The BFS, predecessor
    // backflow and real driver are the production implementation in both.
    state.dense_scans = true;
    drive_load(graph, state, meter)
}

fn scores(output: &KernelOutput) -> &[f64] {
    match &output.values {
        KernelValues::Scores(values) => values,
        _ => panic!("load returns one score per vertex"),
    }
}

fn bits(values: &[f64]) -> Vec<u64> {
    values.iter().map(|value| value.to_bits()).collect()
}

fn compare(graph: &SnapshotGraphView) {
    let mut current_meter = Meter::default();
    let (current, current_peak) = load(graph, &mut current_meter).unwrap();
    let mut prior_meter = Meter::default();
    let (prior, prior_peak) = dense(graph, &mut prior_meter).unwrap();
    assert_eq!(bits(scores(&current)), bits(scores(&prior)));
    assert_eq!(current.witness, prior.witness);
    assert_eq!(current.row_count, prior.row_count);
    assert_eq!(current_peak, prior_peak);
    assert!(current_meter.units <= prior_meter.units);
    assert_eq!(current_meter.polls, prior_meter.polls);
    let n = graph.node_count();
    if n != 0 && current.witness.nodes_touched == n * n {
        // A fully connected traversal keeps the dense pass lengths/locality;
        // only the first, already-initialized reset has disappeared.
        assert_eq!(prior_meter.units - current_meter.units, n);
    }

    // The pinned implementation independently materializes predecessor lists
    // and dense per-source deltas; it does not use our state machine.
    let copy = super::super::copy_graph(graph, &mut Meter::default()).unwrap();
    let oracle = fnx_algorithms::load_centrality(&copy);
    let expected: Vec<_> = oracle.scores.iter().map(|value| value.score).collect();
    assert_eq!(bits(scores(&current)), bits(&expected));
    assert_eq!(current.witness.nodes_touched, oracle.witness.nodes_touched);
    assert_eq!(current.witness.queue_peak, oracle.witness.queue_peak);
}

#[test]
fn load_reached_passes_match_dense_and_pinned_fnx_on_every_small_topology() {
    for n in 0..=5 {
        let possible: Vec<_> = (0..n)
            .flat_map(|a| (a + 1..n).map(move |b| (a, b)))
            .collect();
        for mask in 0..1usize << possible.len() {
            let pairs: Vec<_> = possible
                .iter()
                .enumerate()
                .filter(|(bit, _)| mask & (1 << bit) != 0)
                .map(|(_, &pair)| pair)
                .collect();
            compare(&graph(n, &pairs));
        }
    }
    // Interleaved components and unequal predecessor multiplicities exercise
    // non-ordinal BFS order, fractional backflow, and untouched positive zero.
    compare(&graph(
        13,
        &[
            (0, 7),
            (0, 9),
            (7, 3),
            (9, 3),
            (7, 11),
            (9, 11),
            (3, 5),
            (11, 5),
            (3, 8),
            (11, 8),
            (5, 12),
            (8, 12),
            (1, 6),
            (6, 10),
        ],
    ));
}

#[test]
fn load_reached_passes_are_linear_for_isolates_and_small_components() {
    let n = 512;
    let isolated = graph(n, &[]);
    let mut meter = Meter::default();
    let (actual, peak) = load(&isolated, &mut meter).unwrap();
    assert!(scores(&actual).iter().all(|value| value.to_bits() == 0));
    assert_eq!(actual.witness.nodes_touched, n);
    assert_eq!(actual.witness.edges_scanned, 0);
    assert_eq!(actual.witness.queue_peak, 1);
    assert_eq!(peak, Load::workspace(n).unwrap());
    assert!(
        meter.units <= 16 * n + 16,
        "{} units for {n} isolates",
        meter.units
    );
    let mut old = Meter::default();
    let (prior, _) = dense(&isolated, &mut old).unwrap();
    assert_eq!(bits(scores(&actual)), bits(scores(&prior)));
    assert!(old.units > 50 * meter.units);

    // A source reaches only its three-vertex component. Their IDs interleave
    // to catch an accidental assumption that the queue is ordinal-sorted.
    let width = 128;
    let mut pairs = Vec::new();
    for component in 0..width {
        pairs.push((component, component + width));
        pairs.push((component + width, component + 2 * width));
    }
    let components = graph(4 * width, &pairs);
    let mut meter = Meter::default();
    let (actual, _) = load(&components, &mut meter).unwrap();
    let mut old = Meter::default();
    let (prior, _) = dense(&components, &mut old).unwrap();
    assert_eq!(bits(scores(&actual)), bits(scores(&prior)));
    assert_eq!(actual.witness, prior.witness);
    assert!(meter.units <= 40 * components.node_count());
    assert!(old.units > 20 * meter.units);
}

#[test]
fn load_reached_work_bound_admits_isolates_and_keeps_connected_bounds() {
    assert_eq!(Load::source_bound(0, usize::MAX), 0);
    assert_eq!(Load::source_bound(9, usize::MAX), 9);
    assert_eq!(Load::source_bound(9, 0), 1);
    for n in 1..=64usize {
        let arcs = 2 * (n - 1);
        let old_bound = n * (4 * n + 2 * arcs + 8) + 3 * n + 16;
        assert_eq!(load_work(n, arcs), Some(old_bound));
    }
    assert!(load_work(usize::MAX, 0).is_none());
    let n = 512;
    let view = graph(n, &[]);
    let call = FnxCallSpec::bind("CALL fnx.load_centrality()", &FnxParameters::new()).unwrap();
    let work = load_work(n, 0).unwrap();
    assert!(work <= 16 * n + 16);
    let limits = FnxExecutionLimits {
        max_iterations: 0,
        max_result_rows: n,
        max_estimated_work: work,
    };
    let result = call.execute(&view, limits, || Ok::<_, ()>(())).unwrap();
    assert_eq!(
        result.certificate.execution_kernel,
        "fgdb-prism/newman-load-csr-v2"
    );
    assert_eq!(result.certificate.estimated_work, work);
    assert_eq!(result.rows.len(), n);
    assert!(matches!(call.execute(&view, FnxExecutionLimits {
        max_estimated_work: work - 1, ..limits
    }, || Ok::<_, ()>(())), Err(FnxExecutionError::LimitExceeded {
        resource: "estimated work", limit, requested
    }) if limit == work - 1 && requested == work));
}

#[test]
fn load_reached_passes_refuse_every_live_unit_and_leave_no_partial_result() {
    let view = graph(7, &[(0, 4), (4, 2), (2, 6), (1, 5)]);
    let mut complete = Meter::default();
    let (expected, _) = load(&view, &mut complete).unwrap();
    for unit in 1..=complete.units {
        let stop = Stop::Unit(unit);
        let mut meter = Meter {
            stop: Some(stop),
            ..Meter::default()
        };
        assert!(
            matches!(load(&view, &mut meter), Err(FnxExecutionError::Cancelled(at)) if at == stop)
        );
        assert_eq!(meter.units, unit);
        assert_eq!(meter.polls, 0);
    }
    for poll in 1..=complete.polls {
        let stop = Stop::Poll(poll);
        let mut meter = Meter {
            stop: Some(stop),
            ..Meter::default()
        };
        assert!(
            matches!(load(&view, &mut meter), Err(FnxExecutionError::Cancelled(at)) if at == stop)
        );
        assert_eq!(meter.polls, poll);
    }
    let (again, _) = load(&view, &mut Meter::default()).unwrap();
    assert_eq!(bits(scores(&again)), bits(scores(&expected)));
    assert_eq!(again.witness, expected.witness);
}

#[test]
#[ignore = "paired actual Load-driver measurement; use optimized --ignored --nocapture"]
fn load_reached_passes_paired_complete_driver_benchmark() {
    use std::time::Instant;
    for components in [false, true] {
        let n = 4096;
        let pairs: Vec<_> = if components {
            (0..n / 4)
                .flat_map(|start| [(start, start + n / 4), (start + n / 4, start + n / 2)])
                .collect()
        } else {
            Vec::new()
        };
        let view = graph(n, &pairs);
        let mut samples = [Vec::new(), Vec::new()];
        for trial in 0..6 {
            let mut outputs = Vec::new();
            for prior in if trial % 2 == 0 {
                [true, false]
            } else {
                [false, true]
            } {
                let start = Instant::now();
                let mut meter = Meter::default();
                let (result, bytes) = if prior {
                    dense(&view, &mut meter)
                } else {
                    load(&view, &mut meter)
                }
                .unwrap();
                let checksum = scores(&result)
                    .iter()
                    .fold(0u64, |sum, value| sum.wrapping_add(value.to_bits()));
                std::hint::black_box((checksum, bytes, meter.units));
                samples[usize::from(prior)].push(start.elapsed().as_nanos());
                outputs.push(result);
            }
            assert_eq!(bits(scores(&outputs[0])), bits(scores(&outputs[1])));
            assert_eq!(outputs[0].witness, outputs[1].witness);
        }
        eprintln!(
            "load_reached_passes_pair n={n} small_components={components} current_ns={:?} dense_ns={:?}",
            samples[0], samples[1]
        );
    }
}
