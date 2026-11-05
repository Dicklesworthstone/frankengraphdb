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

#[derive(Default)]
struct RunMeter {
    units: usize,
    polls: usize,
    calls: usize,
    largest_run: usize,
    stop: Option<Stop>,
    stopped: bool,
}

impl FnxMeter<Stop> for RunMeter {
    fn unit(&mut self) -> Result<(), Stop> {
        self.units(1)
    }

    fn poll(&mut self) -> Result<(), Stop> {
        assert!(!self.stopped, "no callback after refusal");
        self.polls += 1;
        let event = Stop::Poll(self.polls);
        if self.stop == Some(event) {
            self.stopped = true;
            Err(event)
        } else {
            Ok(())
        }
    }

    fn units(&mut self, count: usize) -> Result<(), Stop> {
        assert!(!self.stopped, "no callback after refusal");
        assert!((1..=FNX_METER_RUN).contains(&count));
        self.calls += 1;
        self.largest_run = self.largest_run.max(count);
        if let Some(Stop::Unit(stop)) = self.stop
            && self.units + count >= stop
        {
            self.units = stop;
            self.stopped = true;
            return Err(Stop::Unit(stop));
        }
        self.units += count;
        Ok(())
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

// The pre-batching production driver. Keep this independently scalar so a
// regression in run sizing or error precedence cannot affect both controls.
fn scalar_load<C>(
    graph: &SnapshotGraphView,
    meter: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    meter.unit().map_err(FnxExecutionError::Cancelled)?;
    drive_scalar_load(graph, Load::new(graph.node_count(), usize::MAX)?, meter)
}

fn drive_scalar_load<C>(
    graph: &SnapshotGraphView,
    mut state: Load,
    meter: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    loop {
        meter.unit().map_err(FnxExecutionError::Cancelled)?;
        match state.step()? {
            LoadStep::Scalar => {}
            LoadStep::Complete => break,
            LoadStep::Row(vertex) => {
                for &neighbor in graph
                    .neighbors_indices(vertex)
                    .ok_or(FnxExecutionError::InvalidUpstreamResult)?
                {
                    meter.unit().map_err(FnxExecutionError::Cancelled)?;
                    state.visit(neighbor)?;
                }
                state.end_row()?;
            }
        }
    }
    meter.poll().map_err(FnxExecutionError::Cancelled)?;
    state.finish()
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

fn same_result(
    actual: Result<(KernelOutput, usize), FnxExecutionError<Stop>>,
    expected: Result<(KernelOutput, usize), FnxExecutionError<Stop>>,
) {
    match (actual, expected) {
        (Ok((actual, actual_peak)), Ok((expected, expected_peak))) => {
            assert_eq!(bits(scores(&actual)), bits(scores(&expected)));
            assert_eq!(actual.witness, expected.witness);
            assert_eq!(actual.row_count, expected.row_count);
            assert_eq!(actual_peak, expected_peak);
        }
        (Err(actual), Err(expected)) => {
            let same = match (&actual, &expected) {
                (FnxExecutionError::Cancelled(actual), FnxExecutionError::Cancelled(expected)) => {
                    actual == expected
                }
                (FnxExecutionError::SelfLoopRefused, FnxExecutionError::SelfLoopRefused)
                | (FnxExecutionError::SizeOverflow, FnxExecutionError::SizeOverflow)
                | (
                    FnxExecutionError::InvalidNumericResult,
                    FnxExecutionError::InvalidNumericResult,
                ) => true,
                _ => false,
            };
            assert!(same, "{actual:?} != {expected:?}");
        }
        (actual, expected) => assert_eq!(
            actual.is_ok(),
            expected.is_ok(),
            "batched and scalar completion disagree"
        ),
    }
}

fn compare_prefix(graph: &SnapshotGraphView, stop: Option<Stop>) {
    let mut actual = RunMeter {
        stop,
        ..RunMeter::default()
    };
    let mut expected = Meter {
        stop,
        ..Meter::default()
    };
    same_result(load(graph, &mut actual), scalar_load(graph, &mut expected));
    assert_eq!(actual.units, expected.units, "cutoff: {stop:?}");
    assert_eq!(actual.polls, expected.polls, "cutoff: {stop:?}");
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
fn load_row_runs_match_scalar_at_every_small_refusal_prefix() {
    for view in [
        graph(0, &[]),
        graph(1, &[]),
        graph(7, &[(0, 4), (4, 2), (2, 6), (1, 5)]),
        graph(7, &[(0, 1), (0, 2), (1, 3), (2, 3), (3, 4), (3, 5)]),
    ] {
        let mut complete = Meter::default();
        scalar_load(&view, &mut complete).unwrap();
        compare_prefix(&view, None);
        for unit in 1..=complete.units + 1 {
            compare_prefix(&view, Some(Stop::Unit(unit)));
        }
        for poll in 1..=complete.polls + 1 {
            compare_prefix(&view, Some(Stop::Poll(poll)));
        }

        // A plain FnMut has no bulk override. Its default units(n) must
        // still call every scalar checkpoint, including the final poll.
        for stop in 1..=complete.units + complete.polls + 1 {
            let mut actual_calls = 0;
            let mut expected_calls = 0;
            let actual = load(&view, &mut || {
                actual_calls += 1;
                if actual_calls == stop {
                    Err(Stop::Unit(actual_calls))
                } else {
                    Ok(())
                }
            });
            let expected = scalar_load(&view, &mut || {
                expected_calls += 1;
                if expected_calls == stop {
                    Err(Stop::Unit(expected_calls))
                } else {
                    Ok(())
                }
            });
            same_result(actual, expected);
            assert_eq!(actual_calls, expected_calls);
        }
    }
}

#[test]
fn load_row_runs_reduce_host_calls_across_wide_bfs_and_backflow_rows() {
    // Both sides of this component have many equal-length predecessors;
    // its wide rows are used in BFS and reverse flow. Disconnected and
    // isolated vertices retain the sparse reset/reduction behavior too.
    let hubs = 8;
    let connected = hubs + FNX_METER_RUN + 7;
    let mut pairs: Vec<_> = (0..hubs)
        .flat_map(|hub| (hubs..connected).map(move |leaf| (hub, leaf)))
        .collect();
    pairs.extend([
        (connected, connected + 1),
        (connected + 1, connected + 2),
        (connected, connected + 2),
    ]);
    let view = graph(connected + 4, &pairs);
    let mut actual = RunMeter::default();
    let mut expected = Meter::default();
    same_result(load(&view, &mut actual), scalar_load(&view, &mut expected));
    assert_eq!(actual.units, expected.units);
    assert_eq!(actual.polls, expected.polls);
    assert_eq!(actual.largest_run, FNX_METER_RUN);
    assert!(actual.calls * 2 < actual.units);

    // Exhaust the first wide row's prefix across 64 and its short tail,
    // then sample throughout all sources, reverse passes and completion.
    for unit in (1..=view.node_count() + 2 * FNX_METER_RUN + 6)
        .chain((1..=expected.units).step_by(expected.units / 31))
        .chain(expected.units - 2..=expected.units + 1)
    {
        compare_prefix(&view, Some(Stop::Unit(unit)));
    }
    compare_prefix(&view, Some(Stop::Poll(1)));
    compare_prefix(&view, Some(Stop::Poll(2)));
}

#[test]
fn load_row_runs_never_charge_past_a_self_loop() {
    let n = 2 * FNX_METER_RUN + 3;
    for hub in [0, FNX_METER_RUN, n - 1] {
        // Put the loop at the beginning, a run boundary, and the end of
        // a wide row, including vertices after the refusing incidence.
        let pairs: Vec<_> = (0..n).map(|neighbor| (hub, neighbor)).collect();
        let view = graph(n, &pairs);
        let mut complete = Meter::default();
        assert!(matches!(
            scalar_load(&view, &mut complete),
            Err(FnxExecutionError::SelfLoopRefused)
        ));
        assert_eq!(complete.polls, 0);
        compare_prefix(&view, None);
        for unit in 1..=complete.units + 2 {
            compare_prefix(&view, Some(Stop::Unit(unit)));
        }
        compare_prefix(&view, Some(Stop::Poll(1)));
    }
}

#[test]
fn load_row_runs_keep_witness_overflow_on_its_original_unit() {
    let view = graph(7, &[(0, 1), (0, 2), (0, 3), (0, 4), (0, 5)]);
    for nodes in [false, true] {
        for remaining in 0..=7 {
            let make_state = || {
                let mut state = Load::new::<Stop>(view.node_count(), usize::MAX).unwrap();
                if nodes {
                    state.witness.nodes_touched = usize::MAX - remaining;
                } else {
                    state.witness.edges_scanned = usize::MAX - remaining;
                }
                state
            };
            let mut complete = Meter::default();
            assert!(matches!(
                drive_scalar_load(&view, make_state(), &mut complete),
                Err(FnxExecutionError::SizeOverflow)
            ));
            // Include a cancellation immediately AFTER the arithmetic
            // failure: it must not overtake the earlier overflow.
            for stop in std::iter::once(None)
                .chain((1..=complete.units + 2).map(|unit| Some(Stop::Unit(unit))))
            {
                let mut actual = RunMeter {
                    stop,
                    ..RunMeter::default()
                };
                let mut expected = Meter {
                    stop,
                    ..Meter::default()
                };
                same_result(
                    drive_load(&view, make_state(), &mut actual),
                    drive_scalar_load(&view, make_state(), &mut expected),
                );
                assert_eq!(actual.units, expected.units);
                assert_eq!(actual.polls, expected.polls);
            }
        }
    }
}

#[test]
fn load_row_runs_leave_numeric_refusal_before_later_checkpoints() {
    let view = graph(4, &[(0, 1), (1, 2), (2, 3)]);
    let make_state = || {
        let mut state = Load::new::<Stop>(view.node_count(), usize::MAX).unwrap();
        while !matches!(state.phase, LoadPhase::Reduce(0)) {
            if let LoadStep::Row(vertex) = state.step::<Stop>().unwrap() {
                for &neighbor in view.neighbors_indices(vertex).unwrap() {
                    state.visit::<Stop>(neighbor).unwrap();
                }
                state.end_row::<Stop>().unwrap();
            }
        }
        state.scores[0] = f64::MAX;
        state.between[0] = f64::MAX;
        state
    };
    for stop in [
        None,
        Some(Stop::Unit(1)),
        Some(Stop::Unit(2)),
        Some(Stop::Poll(1)),
    ] {
        let mut actual = RunMeter {
            stop,
            ..RunMeter::default()
        };
        let mut expected = Meter {
            stop,
            ..Meter::default()
        };
        same_result(
            drive_load(&view, make_state(), &mut actual),
            drive_scalar_load(&view, make_state(), &mut expected),
        );
        assert_eq!(actual.units, 1);
        assert_eq!(actual.units, expected.units);
        assert_eq!(actual.polls, 0);
        assert_eq!(actual.polls, expected.polls);
    }
}

/// Compare complete decoded drivers in one invocation. Projection admission
/// is outside the timer; allocation, all sources, normalization, scratch
/// release and score consumption are inside. Results are retained until the
/// pair's exact comparison. The opaque counting meter measures actual host
/// crossings, not the database's authority/clock implementation or CALL.
#[test]
#[ignore = "paired Load-driver measurement; optimized, --ignored --nocapture --test-threads=1"]
fn load_row_runs_paired_complete_driver_measurement() {
    use std::hint::black_box;
    use std::time::Instant;

    #[derive(Default)]
    struct OpaqueMeter {
        units: usize,
        calls: usize,
        polls: usize,
    }
    impl FnxMeter<Stop> for OpaqueMeter {
        fn unit(&mut self) -> Result<(), Stop> {
            self.units(1)
        }

        #[inline(never)]
        fn poll(&mut self) -> Result<(), Stop> {
            self.polls = black_box(self.polls + 1);
            Ok(())
        }

        #[inline(never)]
        fn units(&mut self, count: usize) -> Result<(), Stop> {
            self.calls = black_box(self.calls + 1);
            self.units = black_box(self.units + count);
            Ok(())
        }
    }

    // About 65k incidences each, without an unbounded all-sources connected
    // workload. Interleaved IDs avoid assuming contiguous component cells.
    // The sparse rings have degree eight; hub rows cross the 64-unit bound.
    for hub in [false, true] {
        let components = 128;
        let width = if hub { 128 } else { 64 };
        let n = components * width;
        let mut pairs = Vec::new();
        for component in 0..components {
            let vertex = |local| local * components + component;
            if hub {
                for leaf in 1..width {
                    pairs.push((vertex(0), vertex(leaf)));
                    pairs.push((vertex(leaf), vertex(1 + leaf % (width - 1))));
                }
            } else {
                for source in 0..width {
                    for step in 1..=4 {
                        pairs.push((vertex(source), vertex((source + step) % width)));
                    }
                }
            }
        }
        let shape = if hub {
            "hub_ring_communities"
        } else {
            "sparse_ring_communities"
        };
        let view = graph(n, &pairs);
        let incidences: usize = (0..n)
            .map(|vertex| view.neighbors_indices(vertex).unwrap().len())
            .sum();
        assert!((64_000..=66_000).contains(&incidences));

        for opaque in [false, true] {
            let meter_name = if opaque {
                "opaque_counting"
            } else {
                "plain_fnmut"
            };
            let mut samples = [Vec::new(), Vec::new()];
            let mut accounting = [(0, 0, 0); 2];
            // One warm pair, then three measured pairs with alternating order.
            for trial in 0..4 {
                let mut outputs = Vec::new();
                for step in 0..2 {
                    let mode = (trial + step) % 2;
                    let mut meter = OpaqueMeter::default();
                    let mut closure_calls = 0;
                    let start = Instant::now();
                    let output = if opaque {
                        if mode == 0 {
                            scalar_load(&view, &mut meter)
                        } else {
                            load(&view, &mut meter)
                        }
                    } else {
                        let mut checkpoint = || {
                            closure_calls += 1;
                            Ok::<_, Stop>(())
                        };
                        if mode == 0 {
                            scalar_load(&view, &mut checkpoint)
                        } else {
                            load(&view, &mut checkpoint)
                        }
                    }
                    .unwrap();
                    let checksum = scores(&output.0)
                        .iter()
                        .fold(0u64, |sum, value| sum.wrapping_add(value.to_bits()));
                    black_box((checksum, output.1, meter.units, closure_calls));
                    let elapsed = start.elapsed().as_nanos();
                    accounting[mode] = if opaque {
                        (meter.units, meter.polls, meter.calls)
                    } else {
                        // Each successful driver ends with exactly one poll;
                        // a plain closure sees it alongside the charged units.
                        (closure_calls - 1, 1, closure_calls - 1)
                    };
                    if trial != 0 {
                        samples[mode].push(elapsed);
                    }
                    outputs.push(output);
                }
                let second = outputs.pop().unwrap();
                let first = outputs.pop().unwrap();
                same_result(Ok(first), Ok(second));
                assert_eq!(accounting[0].0, accounting[1].0);
                assert_eq!(accounting[0].1, accounting[1].1);
                if opaque {
                    assert!(accounting[1].2 < accounting[0].2);
                } else {
                    assert_eq!(accounting[0].2, accounting[1].2);
                }
            }
            let mut ordered = samples.clone();
            ordered[0].sort_unstable();
            ordered[1].sort_unstable();
            eprintln!(
                "load_row_run_pair shape={shape} meter={meter_name} vertices={n} incidences={incidences} logical_units={} polls={} scalar_work_callbacks={} batched_work_callbacks={} scalar_median_ns={} batched_median_ns={} scalar_ns={:?} batched_ns={:?}",
                accounting[0].0,
                accounting[0].1,
                accounting[0].2,
                accounting[1].2,
                ordered[0][1],
                ordered[1][1],
                samples[0],
                samples[1]
            );
        }
    }
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
