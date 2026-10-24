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
                assert!(ticks <= work(n, rows.iter().map(Vec::len).sum(), 0, 2).unwrap());
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
        assert_eq!(peak, workspace(n).unwrap());
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
    assert!(work(usize::MAX, 0, 0, 2).is_err());
    assert!(work(2, 0, usize::MAX, 2).is_err());
    assert!(work(1, usize::MAX, 0, 2).is_err());
    assert!(work(1, 0, 0, usize::MAX).is_err());
}
