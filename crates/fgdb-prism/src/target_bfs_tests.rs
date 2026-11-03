use super::*;
use crate::{
    Directedness, FnxCallSpec, FnxExecutionLimits, FnxValue, ParallelEdgePolicy, ProjectionEdge,
    ProjectionLimits, ProjectionSpec, SelfLoopPolicy, SnapshotBinding,
};
use fgdb_types::{CommitSeq, EId, ids::ObjectId};
use std::cell::Cell;
use std::convert::Infallible;

thread_local! {
    static FORCE_FIFO: Cell<bool> = const { Cell::new(false) };
}

pub(super) fn force_fifo() -> bool {
    FORCE_FIFO.get()
}

fn with_fifo<T>(forced: bool, run: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            FORCE_FIFO.set(self.0);
        }
    }
    let _restore = Restore(FORCE_FIFO.replace(forced));
    run()
}

fn vertex(index: usize) -> VId {
    VId((1u128 << 112) | ((index as u128) << 64) | 13)
}

fn graph(n: usize, pairs: &[(usize, usize)], direction: Directedness) -> SnapshotGraphView {
    graph_with_workspace(n, pairs, direction, 1 << 24)
}

fn graph_with_workspace(
    n: usize,
    pairs: &[(usize, usize)],
    direction: Directedness,
    max_workspace_bytes: usize,
) -> SnapshotGraphView {
    let vertices: Vec<_> = (0..n).rev().map(vertex).collect();
    let edges: Vec<_> = pairs
        .iter()
        .enumerate()
        .map(|(id, &(source, target))| ProjectionEdge {
            eid: EId(id as u128),
            source: vertex(source),
            target: vertex(target),
            weight: [-11.0, 0.0, 3.5][id % 3],
        })
        .collect();
    SnapshotGraphView::build(
        SnapshotBinding {
            root: ObjectId([0x7b; 32]),
            as_of: CommitSeq(9),
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
            max_workspace_bytes,
        },
    )
    .unwrap()
}

fn limits(rows: usize) -> FnxExecutionLimits {
    FnxExecutionLimits {
        max_iterations: 0,
        max_result_rows: rows,
        max_estimated_work: 1 << 24,
    }
}

/// Independent min-plus transitive closure over input edges, not the
/// production frontier, bitset, row driver, or all-source implementation.
fn distances(n: usize, pairs: &[(usize, usize)], direction: Directedness) -> Vec<Vec<usize>> {
    let mut distances = vec![vec![n + 1; n]; n];
    for (index, row) in distances.iter_mut().enumerate() {
        row[index] = 0;
    }
    for &(source, target) in pairs {
        if source == target {
            continue;
        }
        match direction {
            Directedness::Directed => distances[source][target] = 1,
            Directedness::Reversed => distances[target][source] = 1,
            Directedness::Undirected => {
                distances[source][target] = 1;
                distances[target][source] = 1;
            }
        }
    }
    for via in 0..n {
        for source in 0..n {
            for target in 0..n {
                distances[source][target] =
                    distances[source][target].min(distances[source][via] + distances[via][target]);
            }
        }
    }
    distances
}

#[test]
fn every_small_target_and_cutoff_matches_independent_min_plus_closure() {
    for direction in [
        Directedness::Directed,
        Directedness::Reversed,
        Directedness::Undirected,
    ] {
        for n in 1..=3 {
            let possible: Vec<_> = (0..n)
                .flat_map(|source| (0..n).map(move |target| (source, target)))
                .collect();
            for mask in 0usize..(1 << possible.len()) {
                let pairs: Vec<_> = possible
                    .iter()
                    .enumerate()
                    .filter_map(|(bit, &pair)| (mask & (1 << bit) != 0).then_some(pair))
                    .collect();
                let view = graph(n, &pairs, direction);
                let expected = distances(n, &pairs, direction);
                for (source, row) in expected.iter().enumerate() {
                    for (target, &distance) in row.iter().enumerate() {
                        for cutoff in [None, Some(0), Some(1), Some(n)] {
                            let expected = (distance < n
                                && cutoff.is_none_or(|limit| distance <= limit))
                            .then(|| {
                                vec![
                                    FnxValue::Vertex(vertex(target)),
                                    FnxValue::Integer(distance as u64),
                                ]
                            });
                            let actual = FnxCallSpec::shortest_path_length(
                                vertex(source),
                                vertex(target),
                                cutoff,
                            )
                            .execute(&view, limits(1), || Ok::<_, Infallible>(()))
                            .unwrap();
                            assert_eq!(
                                actual.rows,
                                expected.into_iter().collect::<Vec<_>>(),
                                "{direction:?} n={n} mask={mask} source={source} target={target} cutoff={cutoff:?}"
                            );
                            assert!(
                                actual.certificate.witness.edges_scanned
                                    <= view.outgoing_offsets()[n]
                            );
                            assert!(actual.certificate.witness.nodes_touched <= n);
                            assert_eq!(
                                actual.certificate.kernel_workspace_bytes,
                                workspace::<Infallible>(n, source, target, cutoff).unwrap()
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn bitmap_word_boundaries_charge_exact_capacity_and_never_open_a_target_row() {
    for n in [2, 63, 64, 65, 127, 128, 129] {
        let pairs: Vec<_> = (0..n - 1).map(|source| (source, source + 1)).collect();
        let view = graph(n, &pairs, Directedness::Directed);
        let required = workspace::<Infallible>(n, 0, n - 1, None).unwrap();
        assert_eq!(
            required,
            n * size_of::<usize>() + n.div_ceil(64) * size_of::<u64>()
        );
        assert!(matches!(
            Search::new::<Infallible>(n, 0, n - 1, None, required - 1),
            Err(FnxExecutionError::LimitExceeded {
                resource: "kernel workspace bytes",
                ..
            })
        ));
        let state = Search::new::<Infallible>(n, 0, n - 1, None, required).unwrap();
        assert_eq!(state.workspace, required);
        let result = FnxCallSpec::shortest_path_length(vertex(0), vertex(n - 1), None)
            .execute(&view, limits(1), || Ok::<_, Infallible>(()))
            .unwrap();
        assert_eq!(
            result.rows,
            vec![vec![
                FnxValue::Vertex(vertex(n - 1)),
                FnxValue::Integer((n - 1) as u64)
            ]]
        );
        assert_eq!(result.certificate.witness.edges_scanned, n - 1);
        assert_eq!(result.certificate.witness.queue_peak, 1);
    }
}

#[test]
fn one_hop_state_does_not_allocate_population_sized_buffers() {
    for n in [2, 65, 1 << 30, usize::MAX] {
        let target = n - 1;
        let mut state = OneHop::new::<Infallible>(n, 0, target).unwrap();
        assert_eq!(workspace::<Infallible>(n, 0, target, Some(1)).unwrap(), 0);
        // A loop, repeated target and trailing neighbor do not need visited
        // state. Every incidence is still counted before the result settles.
        for neighbor in [0, target, target, 0] {
            state.visit::<Infallible>(neighbor).unwrap();
        }
        let output = state.finish();
        assert!(matches!(output.values,
            KernelValues::TargetHopDistance { target: actual, distance: Some(1) }
                if actual == target));
        assert_eq!(output.row_count, 1);
        assert_eq!(output.witness.nodes_touched, 1);
        assert_eq!(output.witness.edges_scanned, 4);
        assert_eq!(output.witness.queue_peak, 0);
    }
}

#[test]
fn one_hop_decoded_kernel_work_depends_only_on_the_source_row() {
    let mut expected_checks = None;
    for n in [3, 65, 8193] {
        let view = graph(n, &[(0, 0), (0, 1), (0, 2)], Directedness::Directed);
        for target in [1, n - 1] {
            let mut checks = 0;
            let (output, bytes) = run(&view, vertex(0), vertex(target), Some(1), 1, &mut || {
                checks += 1;
                Ok::<_, Infallible>(())
            })
            .unwrap();
            assert_eq!(output.row_count, usize::from(target <= 2));
            assert_eq!(output.witness.nodes_touched, 1);
            assert_eq!(output.witness.edges_scanned, 3);
            assert_eq!(bytes, 0);
            assert_eq!(*expected_checks.get_or_insert(checks), checks);
            let result = FnxCallSpec::shortest_path_length(vertex(0), vertex(target), Some(1))
                .execute(&view, limits(1), || Ok::<_, Infallible>(()))
                .unwrap();
            assert_eq!(result.certificate.kernel_workspace_bytes, 0);
            assert_eq!(result.certificate.witness, output.witness);
        }
    }
}

#[test]
fn one_hop_public_calls_match_the_fifo_predecessor_and_leave_other_cutoffs_unchanged() {
    for direction in [
        Directedness::Directed,
        Directedness::Reversed,
        Directedness::Undirected,
    ] {
        let view = graph(
            65,
            &[(0, 0), (0, 1), (0, 2), (0, 1), (1, 3), (2, 4)],
            direction,
        );
        for (source, target) in [(0, 0), (0, 1), (0, 4), (0, 64), (1, 0), (64, 0)] {
            for cutoff in [None, Some(0), Some(1), Some(2)] {
                let call =
                    FnxCallSpec::shortest_path_length(vertex(source), vertex(target), cutoff);
                let execute = || {
                    call.execute(&view, limits(1), || Ok::<_, Infallible>(()))
                        .unwrap()
                };
                let original = with_fifo(true, execute);
                let current = execute();
                assert_eq!(current.columns, original.columns);
                assert_eq!(current.rows, original.rows);
                assert_eq!(
                    current.certificate.result_digest,
                    original.certificate.result_digest
                );
                assert_eq!(
                    current.certificate.estimated_work,
                    original.certificate.estimated_work
                );
                if cutoff != Some(1) || source == target {
                    assert_eq!(current, original);
                } else {
                    assert_eq!(current.certificate.kernel_workspace_bytes, 0);
                    assert!(original.certificate.kernel_workspace_bytes > 0);
                    assert_eq!(
                        current.certificate.witness.edges_scanned,
                        original.certificate.witness.edges_scanned
                    );
                }
            }
        }
    }
}

/// Same-invocation maintenance comparison of complete public calls against the
/// preserved FIFO path. Includes ordinary immutable-graph admission, certified
/// execution and typed-result consumption. Projection construction, host permit
/// checks, durable I/O and wire encoding are excluded; no database/GAPBS claim.
#[test]
#[ignore = "manual paired public-CALL measurement; run with --release --nocapture"]
fn one_hop_public_call_paired_measurement() {
    use std::hint::black_box;
    use std::time::Instant;

    struct Meter<'a> {
        units: &'a Cell<usize>,
        polls: &'a Cell<usize>,
    }
    impl FnxMeter<Infallible> for Meter<'_> {
        fn unit(&mut self) -> Result<(), Infallible> {
            self.units.set(self.units.get() + 1);
            Ok(())
        }
        fn units(&mut self, count: usize) -> Result<(), Infallible> {
            self.units.set(self.units.get() + count);
            Ok(())
        }
        fn poll(&mut self) -> Result<(), Infallible> {
            self.polls.set(self.polls.get() + 1);
            Ok(())
        }
    }

    for n in [4096, 65_536, 262_144] {
        for degree in [2, 128] {
            let pairs: Vec<_> = (0..=degree).map(|target| (0, target)).collect();
            let view = graph_with_workspace(n, &pairs, Directedness::Directed, 1 << 29);
            for target in [degree, n - 1] {
                let call = FnxCallSpec::shortest_path_length(vertex(0), vertex(target), Some(1));
                // Warm shared immutable source hashing and both allocation paths.
                for forced in [true, false] {
                    with_fifo(forced, || {
                        black_box(
                            call.execute(&view, limits(1), || Ok::<_, Infallible>(()))
                                .unwrap(),
                        );
                    });
                }
                let mut samples = [Vec::new(), Vec::new()];
                let mut work = [0; 2];
                let mut workspace = [0; 2];
                let mut expected = None;
                for trial in 0..21 {
                    for turn in 0..2 {
                        let participant = (trial + turn) % 2;
                        let units = Cell::new(0);
                        let polls = Cell::new(0);
                        let (result, elapsed) = with_fifo(participant == 0, || {
                            let start = Instant::now();
                            let result = call
                                .execute(
                                    black_box(&view),
                                    limits(1),
                                    Meter {
                                        units: &units,
                                        polls: &polls,
                                    },
                                )
                                .unwrap();
                            assert!(
                                result.rows.iter().flatten().all(|value| matches!(
                                    value,
                                    FnxValue::Vertex(_) | FnxValue::Integer(_)
                                )),
                                "integer target result"
                            );
                            let consumed =
                                result.rows.iter().flatten().fold(0u128, |sum, value| {
                                    sum ^ match value {
                                        FnxValue::Vertex(vertex) => vertex.0,
                                        FnxValue::Integer(distance) => u128::from(*distance),
                                        _ => 0,
                                    }
                                });
                            black_box((consumed, &result.certificate));
                            (result, start.elapsed().as_nanos())
                        });
                        samples[participant].push(elapsed);
                        work[participant] = units.get();
                        workspace[participant] = result.certificate.kernel_workspace_bytes;
                        let identity = (
                            result.columns,
                            result.rows,
                            result.certificate.result_digest,
                        );
                        assert_eq!(expected.get_or_insert_with(|| identity.clone()), &identity);
                    }
                }
                for values in &mut samples {
                    values.sort_unstable();
                }
                eprintln!(
                    "one_hop_public_call n={n} degree={} reachable={} pairs=21 old_p50_ns={} new_p50_ns={} old_p95_ns={} new_p95_ns={} old_workspace={} new_workspace={} old_units={} new_units={} parity=exact_rows_and_result_digest scope=prebuilt_projection_call_maintenance",
                    degree + 1,
                    target == degree,
                    samples[0][10],
                    samples[1][10],
                    samples[0][19],
                    samples[1][19],
                    workspace[0],
                    workspace[1],
                    work[0],
                    work[1],
                );
            }
        }
    }
}

#[test]
fn target_only_result_admission_endpoint_errors_and_trivial_workspace_are_exact() {
    let view = graph(
        5,
        &[(0, 1), (1, 2), (2, 3), (0, 0), (0, 1)],
        Directedness::Directed,
    );
    let unreachable = FnxCallSpec::shortest_path_length(vertex(0), vertex(4), None);
    assert!(
        unreachable
            .execute(&view, limits(0), || Ok::<_, Infallible>(()))
            .unwrap()
            .rows
            .is_empty()
    );
    let reached = FnxCallSpec::shortest_path_length(vertex(0), vertex(3), None);
    assert!(matches!(
        reached.execute(&view, limits(0), || Ok::<_, Infallible>(())),
        Err(FnxExecutionError::LimitExceeded {
            resource: "result rows",
            requested: 1,
            limit: 0
        })
    ));
    for (source, target, cutoff, count) in [(0, 0, None, 1), (0, 0, Some(0), 1), (0, 3, Some(0), 0)]
    {
        let state = Search::new::<Infallible>(5, source, target, cutoff, 0).unwrap();
        assert_eq!(state.row_count(), count);
        assert_eq!(state.workspace, 0);
        let result = FnxCallSpec::shortest_path_length(vertex(source), vertex(target), cutoff)
            .execute(&view, limits(count), || Ok::<_, Infallible>(()))
            .unwrap();
        assert_eq!(result.rows.len(), count);
        assert_eq!(result.certificate.kernel_workspace_bytes, 0);
        assert_eq!(result.certificate.witness.edges_scanned, 0);
    }
    let missing = VId(u128::MAX);
    assert!(
        matches!(FnxCallSpec::shortest_path_length(missing, missing, Some(0))
        .execute(&view, limits(0), || Ok::<_, Infallible>(())), Err(FnxExecutionError::UnknownSource(id)) if id == missing)
    );
    assert!(
        matches!(FnxCallSpec::shortest_path_length(vertex(0), missing, Some(0))
        .execute(&view, limits(0), || Ok::<_, Infallible>(())), Err(FnxExecutionError::UnknownTarget(id)) if id == missing)
    );
}

#[test]
fn an_early_target_avoids_the_rest_of_the_reachable_component() {
    let n = 257;
    let pairs: Vec<_> = (0..n - 1).map(|source| (source, source + 1)).collect();
    let view = graph(n, &pairs, Directedness::Directed);
    let targeted = FnxCallSpec::shortest_path_length(vertex(0), vertex(1), None)
        .execute(&view, limits(1), || Ok::<_, Infallible>(()))
        .unwrap();
    let complete = FnxCallSpec::single_source_shortest_path_length(vertex(0), None)
        .execute(&view, limits(n), || Ok::<_, Infallible>(()))
        .unwrap();
    assert_eq!(targeted.rows, vec![complete.rows[1].clone()]);
    assert_eq!(targeted.certificate.witness.nodes_touched, 2);
    assert_eq!(targeted.certificate.witness.edges_scanned, 1);
    assert_eq!(complete.certificate.witness.edges_scanned, n - 1);
    assert!(
        targeted.certificate.kernel_workspace_bytes < complete.certificate.kernel_workspace_bytes
    );
}

#[test]
fn every_decoded_checkpoint_refuses_without_a_partial_result() {
    let view = graph(
        7,
        &[(0, 1), (0, 2), (1, 3), (2, 4), (3, 5), (4, 6)],
        Directedness::Directed,
    );
    for (target, cutoff) in [(5, None), (5, Some(1)), (1, Some(1))] {
        let call = FnxCallSpec::shortest_path_length(vertex(0), vertex(target), cutoff);
        let mut total = 0;
        call.execute(&view, limits(1), || {
            total += 1;
            Ok::<_, usize>(())
        })
        .unwrap();
        for stop in 1..=total {
            let mut seen = 0;
            assert!(matches!(call.execute(&view, limits(1), || {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            }), Err(FnxExecutionError::Cancelled(at)) if at == stop));
            assert_eq!(seen, stop);
        }
    }
}
