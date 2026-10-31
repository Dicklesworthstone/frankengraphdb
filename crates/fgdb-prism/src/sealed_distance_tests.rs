use super::*;
use crate::SealedProjectionError;
use std::cell::Cell;

struct Fixture {
    rows: Vec<Vec<usize>>,
    opens: Vec<Cell<usize>>,
    source_steps: Cell<usize>,
    fail_at: usize,
}

impl Fixture {
    fn new(rows: Vec<Vec<usize>>) -> Self {
        Self {
            opens: (0..rows.len()).map(|_| Cell::new(0)).collect(),
            rows,
            source_steps: Cell::new(0),
            fail_at: usize::MAX,
        }
    }

    fn step(&self) -> Result<()> {
        let step = self.source_steps.get() + 1;
        self.source_steps.set(step);
        if step == self.fail_at {
            Err(super::super::Error::Source(
                SealedProjectionError::UnknownOrdinal(step),
            ))
        } else {
            Ok(())
        }
    }
}

struct Row<'a> {
    fixture: &'a Fixture,
    targets: std::slice::Iter<'a, usize>,
}

impl Cursor for Row<'_> {
    fn next(&mut self) -> Result<Option<(usize, f64)>> {
        self.fixture.step()?;
        // Both registered procedures ignore finite signed edge weights.
        Ok(self.targets.next().map(|&target| (target, -7.0)))
    }
}

impl Rows for Fixture {
    type Cursor<'a> = Row<'a>;

    fn node_count(&self) -> usize {
        self.rows.len()
    }

    fn degree(&self, source: usize) -> Option<usize> {
        self.rows.get(source).map(Vec::len)
    }

    fn open(&self, source: usize) -> Result<Self::Cursor<'_>> {
        self.step()?;
        self.opens[source].set(self.opens[source].get() + 1);
        Ok(Row {
            fixture: self,
            targets: self.rows[source].iter(),
        })
    }
}

fn rows(n: usize, edges: &[(usize, usize)]) -> Vec<Vec<usize>> {
    let mut rows = vec![Vec::new(); n];
    for &(source, target) in edges {
        rows[source].push(target);
        rows[target].push(source);
    }
    for row in &mut rows {
        row.sort_unstable();
    }
    rows
}

// Independent all-pairs relaxation, with no FIFO, visited reset or production
// traversal state. Sorting reachable distances specifies the registered sum
// order without depending on ties between equally distant vertex identities.
fn distances(rows: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = rows.len();
    let mut distances = vec![vec![usize::MAX; n]; n];
    for source in 0..n {
        distances[source][source] = 0;
        for &target in &rows[source] {
            distances[source][target] = 1;
        }
    }
    for middle in 0..n {
        for source in 0..n {
            for target in 0..n {
                if let Some(candidate) =
                    distances[source][middle].checked_add(distances[middle][target])
                {
                    distances[source][target] = distances[source][target].min(candidate);
                }
            }
        }
    }
    distances
}

fn score_bits(distances: &[Vec<usize>], harmonic: bool) -> Vec<u64> {
    let n = distances.len();
    distances
        .iter()
        .map(|row| {
            let mut reached: Vec<_> = row.iter().copied().filter(|&d| d != usize::MAX).collect();
            reached.sort_unstable();
            let score = if harmonic {
                reached
                    .iter()
                    .copied()
                    .filter(|&d| d != 0)
                    .fold(0.0, |sum, d| sum + 1.0 / d as f64)
            } else if reached.len() <= 1 {
                0.0
            } else {
                let mut score =
                    (reached.len() - 1) as f64 / reached.iter().copied().sum::<usize>() as f64;
                score *= (reached.len() - 1) as f64 / (n - 1) as f64;
                score
            };
            score.to_bits()
        })
        .collect()
}

fn bits(output: &KernelOutput) -> Vec<u64> {
    let KernelValues::Scores(scores) = &output.values else {
        panic!("distance centrality scores");
    };
    scores.iter().map(|score| score.to_bits()).collect()
}

#[test]
fn every_small_distance_centrality_matches_pinned_fnx_and_independent_distances() {
    let mut cases = 0;
    for n in 0..=5 {
        let pairs: Vec<_> = (0..n)
            .flat_map(|source| ((source + 1)..n).map(move |target| (source, target)))
            .collect();
        for mask in 0usize..(1 << pairs.len()) {
            let edges: Vec<_> = pairs
                .iter()
                .enumerate()
                .filter(|(bit, _)| mask & (1 << bit) != 0)
                .map(|(_, &edge)| edge)
                .collect();
            let rows = rows(n, &edges);
            let distances = distances(&rows);
            let mut oracle = fnx_classes::Graph::strict();
            for v in 0..n {
                oracle.add_node(format!("{v:032x}"));
            }
            for &(source, target) in &edges {
                oracle
                    .add_edge(format!("{source:032x}"), format!("{target:032x}"))
                    .unwrap();
            }
            for (harmonic, reference) in [
                (false, fnx_algorithms::closeness_centrality(&oracle).scores),
                (true, fnx_algorithms::harmonic_centrality(&oracle).scores),
            ] {
                let independent = score_bits(&distances, harmonic);
                assert_eq!(
                    reference
                        .iter()
                        .map(|score| score.score.to_bits())
                        .collect::<Vec<_>>(),
                    independent,
                    "oracle n={n}, mask={mask}, harmonic={harmonic}"
                );
                let graph = Fixture::new(rows.clone());
                let mut ticks = 0;
                let (output, peak) = run(&graph, harmonic, workspace(n).unwrap(), &mut || {
                    ticks += 1;
                    Ok(())
                })
                .unwrap();
                assert_eq!(
                    bits(&output),
                    independent,
                    "n={n}, mask={mask}, harmonic={harmonic}"
                );
                assert_eq!(output.row_count, n);
                assert_eq!(peak, workspace(n).unwrap());
                let visits = distances
                    .iter()
                    .flat_map(|row| row.iter().enumerate())
                    .filter(|&(_, &distance)| distance != usize::MAX);
                let mut nodes = 0;
                let mut arcs = 0;
                for (vertex, _) in visits {
                    nodes += 1;
                    arcs += rows[vertex].len();
                }
                assert_eq!(output.witness.nodes_touched, nodes);
                assert_eq!(output.witness.edges_scanned, arcs);
                for vertex in 0..n {
                    let reached_from = distances
                        .iter()
                        .filter(|row| row[vertex] != usize::MAX)
                        .count();
                    assert_eq!(graph.opens[vertex].get(), reached_from);
                }
                let projected_arcs = rows.iter().map(Vec::len).sum();
                // A complete fixture pass costs one open and one terminal
                // pull per vertex, plus each incidence. Count that source
                // work alongside all state-machine ticks, not just the
                // projected edge visits the optimized kernel reports.
                let source_pass = 2 * n + projected_arcs;
                assert!(
                    ticks + graph.source_steps.get()
                        <= work(n, projected_arcs, source_pass, 2, false).unwrap()
                );
            }
            cases += 1;
        }
    }
    assert_eq!(cases, 1100);
}

#[test]
fn long_paths_disconnected_components_and_hubs_preserve_score_bits() {
    let path: Vec<_> = (1..67).map(|vertex| (vertex - 1, vertex)).collect();
    let hub: Vec<_> = (1..67).map(|vertex| (0, vertex)).collect();
    let mut disconnected = Vec::new();
    for first in [0, 13, 31] {
        for vertex in first + 1..first + 9 {
            disconnected.push((vertex - 1, vertex));
        }
    }
    for edges in [path, hub, disconnected] {
        let rows = rows(71, &edges);
        let distances = distances(&rows);
        for harmonic in [false, true] {
            let graph = Fixture::new(rows.clone());
            let (output, peak) = run(&graph, harmonic, usize::MAX, &mut || Ok(())).unwrap();
            assert_eq!(bits(&output), score_bits(&distances, harmonic));
            assert_eq!(peak, workspace(rows.len()).unwrap());
            assert!(output.witness.queue_peak <= rows.len());
        }
    }
}

#[test]
fn isolates_only_initialize_once_and_never_clear_unreached_vertices() {
    let n = 4096;
    let pass = directional_pass_work(n, 0, crate::Directedness::Undirected).unwrap();
    let estimated = work(n, 0, pass, 2, false).unwrap();
    assert_eq!(estimated, 33 * n + 18);
    assert!(estimated < n * n);
    for harmonic in [false, true] {
        let graph = Fixture::new(vec![Vec::new(); n]);
        let mut ticks = 0;
        let (output, peak) = run(&graph, harmonic, usize::MAX, &mut || {
            ticks += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(bits(&output), vec![0.0_f64.to_bits(); n]);
        assert_eq!(output.witness.nodes_touched, n);
        assert_eq!(output.witness.edges_scanned, 0);
        assert_eq!(output.witness.queue_peak, 1);
        assert!(graph.opens.iter().all(|count| count.get() == 1));
        assert!(ticks < 8 * n, "scalar work on isolates must stay linear");
        assert!(ticks + graph.source_steps.get() < estimated);
        assert_eq!(peak, workspace(n).unwrap());
    }
}

#[test]
fn sparse_components_admit_all_source_and_retained_history_work() {
    let n = 4096;
    let pairs: Vec<_> = (0..32).map(|pair| (2 * pair, 2 * pair + 1)).collect();
    let projected_arcs = 2 * pairs.len();
    let graph_rows = rows(n, &pairs);
    // Excluded endpoints or retained invisible versions may greatly outnumber
    // the reduced arcs. The entire physical pass must remain in admission.
    let retained = 8192;
    let pass = directional_pass_work(n, retained, crate::Directedness::Undirected).unwrap();
    let estimated = work(n, projected_arcs, pass, 2, false).unwrap();
    let component_bound = pairs.len() + 1;
    let fixed = n * 13 + 18;
    assert!(estimated >= component_bound * pass + fixed);
    let all_source_bound = n * (pass + projected_arcs + 4 * n + 8) + n * 5 + 18;
    assert!(estimated < all_source_bound / 100);

    for harmonic in [false, true] {
        let graph = Fixture::new(graph_rows.clone());
        let mut ticks = 0;
        let (output, _) = run(&graph, harmonic, usize::MAX, &mut || {
            ticks += 1;
            Ok(())
        })
        .unwrap();
        let connected_score: f64 = if harmonic { 1.0 } else { 1.0 / (n - 1) as f64 };
        let mut expected = vec![0.0_f64.to_bits(); n];
        expected[..2 * pairs.len()].fill(connected_score.to_bits());
        assert_eq!(bits(&output), expected);
        assert_eq!(output.witness.nodes_touched, n + 2 * pairs.len());
        assert_eq!(output.witness.edges_scanned, 4 * pairs.len());
        assert!(ticks + graph.source_steps.get() <= estimated);
        // The scalar/source bound is valid even without retained history.
        assert!(
            ticks + graph.source_steps.get()
                <= work(n, projected_arcs, 2 * n + projected_arcs, 2, false).unwrap()
        );
    }
}

#[test]
fn every_distance_scalar_and_source_refusal_discards_partial_scores() {
    let rows = rows(5, &[(0, 1), (0, 2), (1, 2), (2, 3)]);
    for harmonic in [false, true] {
        let graph = Fixture::new(rows.clone());
        let mut total = 0;
        run(&graph, harmonic, usize::MAX, &mut || {
            total += 1;
            Ok(())
        })
        .unwrap();
        let source_total = graph.source_steps.get();
        for stop in 1..=total {
            let graph = Fixture::new(rows.clone());
            let mut seen = 0;
            let error = run(&graph, harmonic, usize::MAX, &mut || {
                seen += 1;
                if seen == stop {
                    Err(super::super::Error::Cancelled(
                        std::io::Error::other("distance stop").into(),
                    ))
                } else {
                    Ok(())
                }
            })
            .err()
            .expect("scalar refusal");
            assert!(matches!(error, super::super::Error::Cancelled(_)));
            assert_eq!(seen, stop);
        }
        for stop in 1..=source_total {
            let mut graph = Fixture::new(rows.clone());
            graph.fail_at = stop;
            let error = run(&graph, harmonic, usize::MAX, &mut || Ok(()))
                .err()
                .expect("source refusal");
            assert!(matches!(error, super::super::Error::Source(
                SealedProjectionError::UnknownOrdinal(actual)) if actual == stop));
            assert_eq!(graph.source_steps.get(), stop);
        }
    }
}

#[test]
fn workspace_overflow_and_malformed_rows_fail_before_a_result() {
    for harmonic in [false, true] {
        let graph = Fixture::new(rows(3, &[(0, 1)]));
        let exact = workspace(3).unwrap();
        assert!(matches!(
            run(&graph, harmonic, exact - 1, &mut || Ok(())),
            Err(super::super::Error::Execution(
                ExecutionError::LimitExceeded {
                    resource: "kernel workspace bytes",
                    ..
                }
            ))
        ));
        assert_eq!(graph.source_steps.get(), 0);
        run(&graph, harmonic, exact, &mut || Ok(())).unwrap();
        assert!(matches!(
            run(
                &Fixture::new(vec![vec![1]]),
                harmonic,
                usize::MAX,
                &mut || Ok(())
            ),
            Err(super::super::Error::Execution(
                ExecutionError::InvalidUpstreamResult
            ))
        ));
        assert!(matches!(
            run(
                &Fixture::new(vec![vec![0]]),
                harmonic,
                usize::MAX,
                &mut || Ok(())
            ),
            Err(super::super::Error::Execution(
                ExecutionError::SelfLoopRefused
            ))
        ));
    }
    assert!(workspace(usize::MAX).is_err());
    assert!(work(usize::MAX, 0, 0, 2, false).is_err());
    assert!(work(2, 0, usize::MAX, 2, false).is_err());
    assert!(work(1, usize::MAX, 0, 2, false).is_err());
    assert!(work(1, 0, 0, usize::MAX, false).is_err());
}

fn policy(rows: &[Vec<usize>]) -> BatchPolicy {
    let nodes = rows.len();
    let arcs = rows.iter().map(Vec::len).sum();
    let pass = 2 * nodes + arcs;
    BatchPolicy {
        pass,
        arcs,
        columns: 2,
        admitted: work(nodes, arcs, pass, 2, false).unwrap(),
    }
}

#[test]
fn batch_source_boundaries_relabelled_hubs_and_cliques_match_independent_distances() {
    for n in [63, 64, 65, 127, 128, 129, 130] {
        let hub: Vec<_> = (1..n).map(|v| (0, v)).collect();
        let relabelled: Vec<_> = (0..n - 1).map(|v| (v, n - 1)).collect();
        let clique: Vec<_> = (0..n)
            .flat_map(|u| (u + 1..n).map(move |v| (u, v)))
            .collect();
        let mut low_diameter = hub.clone();
        low_diameter.extend((1..n - 1).map(|v| (v, v + 1)));
        // Depths three and five make repeated reciprocal additions visibly
        // different from a count-times-reciprocal shortcut in the last bits.
        let tree: Vec<_> = (1..n).map(|v| ((v - 1) / 3, v)).collect();
        for edges in [hub, relabelled, clique, low_diameter, tree] {
            let rows = rows(n, &edges);
            let independent = distances(&rows);
            let policy = policy(&rows);
            for harmonic in [false, true] {
                let graph = Fixture::new(rows.clone());
                let mut ticks = 0;
                let (actual, peak) =
                    run_with_policy(&graph, harmonic, usize::MAX, Some(policy), &mut || {
                        ticks += 1;
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(bits(&actual), score_bits(&independent, harmonic));
                assert_eq!(actual.witness.nodes_touched, n * n);
                assert!(ticks + graph.source_steps.get() <= policy.admitted);
                let baseline = Fixture::new(rows.clone());
                let (scalar, _) = run(&baseline, harmonic, usize::MAX, &mut || Ok(())).unwrap();
                assert_eq!(bits(&actual), bits(&scalar));
                if n < BATCH_SOURCES {
                    assert_eq!(actual.witness, scalar.witness);
                    assert_eq!(peak, workspace(n).unwrap());
                    assert_eq!(graph.source_steps.get(), baseline.source_steps.get());
                } else {
                    assert!(actual.witness.algorithm.ends_with("_batch64_cursor_v1"));
                    assert!(actual.witness.edges_scanned * 4 < scalar.witness.edges_scanned);
                    assert!(graph.source_steps.get() * 4 < baseline.source_steps.get());
                    assert_eq!(peak, workspace(n).unwrap() + batch_workspace(n).unwrap());
                }
            }
        }
    }
}

#[test]
fn batch_selection_preserves_scalar_work_and_memory_fallbacks() {
    let n = 129;
    let star: Vec<_> = (1..n).map(|v| (0, v)).collect();
    let path: Vec<_> = (1..n).map(|v| (v - 1, v)).collect();
    let disconnected: Vec<_> = (1..n - 1).map(|v| (0, v)).collect();
    for (edges, constrained_memory, constrained_work) in [
        (star.clone(), true, false),
        (star, false, true),
        (path, false, false),
        (disconnected, false, false),
    ] {
        let rows = rows(n, &edges);
        let mut policy = policy(&rows);
        if constrained_work {
            policy.admitted = 1;
        }
        let limit = if constrained_memory {
            workspace(n).unwrap() + batch_workspace(n).unwrap() - 1
        } else {
            usize::MAX
        };
        for harmonic in [false, true] {
            let graph = Fixture::new(rows.clone());
            let baseline = Fixture::new(rows.clone());
            let (expected, baseline_peak) =
                run(&baseline, harmonic, limit, &mut || Ok(())).unwrap();
            let (actual, peak) =
                run_with_policy(&graph, harmonic, limit, Some(policy), &mut || Ok(())).unwrap();
            assert_eq!(bits(&actual), bits(&expected));
            assert_eq!(actual.witness, expected.witness);
            assert_eq!(peak, baseline_peak);
            assert_eq!(graph.source_steps.get(), baseline.source_steps.get());
        }
    }
    let overflowing = BatchPolicy {
        pass: usize::MAX,
        arcs: 0,
        columns: 2,
        admitted: usize::MAX,
    };
    assert!(!overflowing.permits(n, n, 1));
    assert!(batch_workspace(usize::MAX).is_err());
}

#[test]
fn optional_batch_backing_records_partial_peak_and_propagates_real_failures() {
    let n = 64;
    let baseline = workspace(n).unwrap();
    let extra = batch_workspace(n).unwrap();
    let limit = baseline + extra;
    let mut peak = baseline;
    assert!(
        Batch::reserve_with_seen(n, false, &mut peak, limit, |_| {
            reserve(extra / size_of::<u64>())
        })
        .unwrap()
        .is_none()
    );
    assert_eq!(peak, limit, "the temporary oversized first buffer was live");
    let mut peak = baseline;
    assert!(matches!(
        Batch::reserve_with_seen(n, false, &mut peak, limit, |_| {
            reserve(extra / size_of::<u64>() + 1)
        }),
        Err(super::super::Error::Execution(ExecutionError::LimitExceeded {
            resource: "kernel workspace bytes",
            requested,
            limit: actual_limit,
        })) if requested == limit + size_of::<u64>() && actual_limit == limit
    ));
    let mut peak = baseline;
    assert!(
        Batch::reserve_with_seen(n, false, &mut peak, limit, |_| {
            Err(super::super::Error::Execution(
                ExecutionError::AllocationFailed,
            ))
        })
        .unwrap()
        .is_none()
    );
    assert_eq!(peak, baseline);
    assert!(matches!(
        Batch::reserve_with_seen(n, false, &mut peak, limit, |_| {
            Err(super::super::Error::Execution(
                ExecutionError::InvalidUpstreamResult,
            ))
        }),
        Err(super::super::Error::Execution(
            ExecutionError::InvalidUpstreamResult
        ))
    ));
}

#[test]
fn every_batch_scalar_and_source_refusal_discards_partial_scores() {
    let n = 64;
    let rows = rows(n, &(1..n).map(|v| (0, v)).collect::<Vec<_>>());
    let policy = policy(&rows);
    for harmonic in [false, true] {
        let graph = Fixture::new(rows.clone());
        let mut total = 0;
        let (complete, _) =
            run_with_policy(&graph, harmonic, usize::MAX, Some(policy), &mut || {
                total += 1;
                Ok(())
            })
            .unwrap();
        assert!(complete.witness.algorithm.ends_with("_batch64_cursor_v1"));
        let source_total = graph.source_steps.get();
        for stop in 1..=total {
            let graph = Fixture::new(rows.clone());
            let mut seen = 0;
            let error = run_with_policy(&graph, harmonic, usize::MAX, Some(policy), &mut || {
                seen += 1;
                if seen == stop {
                    Err(super::super::Error::Cancelled(
                        std::io::Error::other("batch distance stop").into(),
                    ))
                } else {
                    Ok(())
                }
            })
            .err()
            .expect("scalar refusal");
            assert!(matches!(error, super::super::Error::Cancelled(_)));
            assert_eq!(seen, stop);
        }
        for stop in 1..=source_total {
            let mut graph = Fixture::new(rows.clone());
            graph.fail_at = stop;
            let error = run_with_policy(&graph, harmonic, usize::MAX, Some(policy), &mut || Ok(()))
                .err()
                .expect("source refusal");
            assert!(matches!(error, super::super::Error::Source(
                SealedProjectionError::UnknownOrdinal(actual)) if actual == stop));
            assert_eq!(graph.source_steps.get(), stop);
        }
    }
}

#[test]
fn batch_rows_validate_targets_and_loops_after_optional_selection() {
    let n = 64;
    let graph = Fixture::new(rows(n, &(1..n).map(|v| (0, v)).collect::<Vec<_>>()));
    for harmonic in [false, true] {
        let mut state = Traversals::new(n, harmonic, usize::MAX).unwrap();
        state.policy = Some(policy(&graph.rows));
        // Finish the earned scalar source and all batch initialization using
        // the real transitions. A source that later corrupts a row still
        // cannot smuggle an invalid ordinal or loop into the word frontier.
        while !matches!(state.phase, Phase::Batch) || state.row().is_none() {
            if let Some(vertex) = state.row() {
                let mut row = graph.open(vertex).unwrap();
                while let Some((neighbor, _)) = row.next().unwrap() {
                    state.visit(neighbor).unwrap();
                }
                state.end_row().unwrap();
            } else {
                state.scalar_step().unwrap();
            }
            assert!(!state.complete());
        }
        let source = state.row().unwrap();
        assert!(matches!(
            state.visit(n),
            Err(super::super::Error::Execution(
                ExecutionError::InvalidUpstreamResult
            ))
        ));
        assert!(matches!(
            state.visit(source),
            Err(super::super::Error::Execution(
                ExecutionError::SelfLoopRefused
            ))
        ));
    }
}

#[test]
fn directed_incoming_traversals_match_independent_distances_and_work_bounds() {
    let mut cases = 0;
    for n in 0..=4 {
        let pairs: Vec<_> = (0..n)
            .flat_map(|source| {
                (0..n)
                    .filter(move |&target| source != target)
                    .map(move |target| (source, target))
            })
            .collect();
        for mask in 0usize..(1 << pairs.len()) {
            let mut outgoing = vec![Vec::new(); n];
            let mut incoming = vec![Vec::new(); n];
            for (bit, &(source, target)) in pairs.iter().enumerate() {
                if mask & (1 << bit) != 0 {
                    outgoing[source].push(target);
                    incoming[target].push(source);
                }
            }
            let shortest = distances(&outgoing);
            let into: Vec<Vec<_>> = (0..n)
                .map(|target| (0..n).map(|source| shortest[source][target]).collect())
                .collect();
            let arcs = outgoing.iter().map(Vec::len).sum::<usize>();
            let source_pass = 2 * n + arcs;
            for harmonic in [false, true] {
                let graph = Fixture::new(incoming.clone());
                let mut ticks = 0;
                let (output, peak) = run(&graph, harmonic, workspace(n).unwrap(), &mut || {
                    ticks += 1;
                    Ok(())
                })
                .unwrap();
                assert_eq!(
                    bits(&output),
                    score_bits(&into, harmonic),
                    "n={n}, mask={mask}, harmonic={harmonic}"
                );
                assert_eq!(output.row_count, n);
                assert_eq!(peak, workspace(n).unwrap());
                assert!(
                    ticks + graph.source_steps.get()
                        <= work(n, arcs, source_pass, 2, true).unwrap(),
                    "n={n}, mask={mask}, harmonic={harmonic}"
                );
            }
            cases += 1;
        }
    }
    assert_eq!(cases, 4166);
    assert!(work(64, 8, 0, 2, true).unwrap() > work(64, 8, 0, 2, false).unwrap());
    assert!(work(1, usize::MAX, 0, 2, true).is_err());
}
