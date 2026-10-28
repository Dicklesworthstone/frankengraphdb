use super::*;
use crate::SealedProjectionError;
use std::cell::Cell;

struct Fixture {
    rows: Vec<Vec<usize>>,
    opens: Vec<Cell<usize>>,
    source_steps: Cell<usize>,
    fail_at: usize,
    degrees: Option<Vec<usize>>,
}

impl Fixture {
    fn new(rows: Vec<Vec<usize>>) -> Self {
        Self {
            opens: (0..rows.len()).map(|_| Cell::new(0)).collect(),
            rows,
            source_steps: Cell::new(0),
            fail_at: usize::MAX,
            degrees: None,
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
        Ok(self.targets.next().map(|&target| (target, -7.0)))
    }
}

impl Rows for Fixture {
    type Cursor<'a> = Row<'a>;

    fn node_count(&self) -> usize {
        self.rows.len()
    }

    fn degree(&self, source: usize) -> Option<usize> {
        self.degrees.as_ref().map_or_else(
            || self.rows.get(source).map(Vec::len),
            |degrees| degrees.get(source).copied(),
        )
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

fn reference(rows: &[Vec<usize>]) -> Vec<u64> {
    let mut oracle = fnx_classes::Graph::strict();
    for vertex in 0..rows.len() {
        oracle.add_node(format!("{vertex:032x}"));
    }
    for (source, row) in rows.iter().enumerate() {
        for &target in row {
            if source < target {
                oracle
                    .add_edge(format!("{source:032x}"), format!("{target:032x}"))
                    .unwrap();
            }
        }
    }
    fnx_algorithms::betweenness_centrality(&oracle)
        .scores
        .iter()
        .map(|score| score.score.to_bits())
        .collect()
}

fn bits(output: &KernelOutput) -> Vec<u64> {
    let KernelValues::Scores(scores) = &output.values else {
        panic!("betweenness scores");
    };
    scores.iter().map(|score| score.to_bits()).collect()
}

// Independent all-pairs relaxation, integer path counts by distance, then the
// pairwise fraction of shortest paths passing through each internal vertex.
// There is no FIFO, reverse dependency accumulation or production state here.
fn independent(rows: &[Vec<usize>]) -> (Vec<Vec<usize>>, Vec<f64>) {
    let n = rows.len();
    let mut distance = vec![vec![usize::MAX; n]; n];
    for source in 0..n {
        distance[source][source] = 0;
        for &target in &rows[source] {
            distance[source][target] = 1;
        }
    }
    for middle in 0..n {
        for source in 0..n {
            for target in 0..n {
                if let Some(candidate) =
                    distance[source][middle].checked_add(distance[middle][target])
                {
                    distance[source][target] = distance[source][target].min(candidate);
                }
            }
        }
    }
    let mut paths = vec![vec![0u64; n]; n];
    for source in 0..n {
        paths[source][source] = 1;
        for depth in 1..n {
            for target in 0..n {
                if distance[source][target] == depth {
                    paths[source][target] = rows[target]
                        .iter()
                        .filter(|&&previous| distance[source][previous] == depth - 1)
                        .map(|&previous| paths[source][previous])
                        .sum();
                }
            }
        }
    }
    let mut scores = vec![0.0; n];
    for vertex in 0..n {
        for source in 0..n {
            for target in 0..n {
                if source != target
                    && vertex != source
                    && vertex != target
                    && paths[source][target] != 0
                    && distance[source][vertex].checked_add(distance[vertex][target])
                        == Some(distance[source][target])
                {
                    scores[vertex] += (paths[source][vertex] * paths[vertex][target]) as f64
                        / paths[source][target] as f64;
                }
            }
        }
        if n > 2 {
            scores[vertex] /= ((n - 1) * (n - 2)) as f64;
        }
    }
    (distance, scores)
}

fn small_graphs(mut visit: impl FnMut(usize, usize, Vec<Vec<usize>>)) {
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
            visit(n, mask, rows(n, &edges));
        }
    }
}

#[test]
fn all_small_compressed_betweenness_matches_fnx_bits_and_independent_path_fractions() {
    let mut cases = 0;
    small_graphs(|n, mask, rows| {
        let (distances, expected) = independent(&rows);
        let graph = Fixture::new(rows.clone());
        let mut ticks = 0;
        let (output, peak) = run(&graph, None, workspace(n).unwrap(), &mut || {
            ticks += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(bits(&output), reference(&rows), "n={n}, mask={mask}");
        let KernelValues::Scores(scores) = &output.values else {
            panic!("scores");
        };
        for (actual, expected) in scores.iter().zip(expected) {
            assert!((actual - expected).abs() < 1e-14, "n={n}, mask={mask}");
        }
        assert_eq!(output.row_count, n);
        assert_eq!(peak, workspace(n).unwrap());
        let mut nodes = 0;
        let mut arcs = 0;
        for vertex in 0..n {
            let reached = distances
                .iter()
                .filter(|row| row[vertex] != usize::MAX)
                .count();
            let dependencies = distances
                .iter()
                .filter(|row| row[vertex] != usize::MAX && row[vertex] > 1)
                .count();
            nodes += reached;
            arcs += (reached + dependencies) * rows[vertex].len();
            assert_eq!(graph.opens[vertex].get(), reached + dependencies);
        }
        assert_eq!(output.witness.nodes_touched, nodes);
        assert_eq!(output.witness.edges_scanned, arcs);
        // The shared admission counts graph-size-dependent work; the empty
        // graph retains only fixed checkpoint overhead and admits zero work.
        assert!(n == 0 || ticks <= work(n, rows.iter().map(Vec::len).sum(), 0, 2, false).unwrap());
        cases += 1;
    });
    assert_eq!(cases, 1100);
}

#[test]
fn breadth_states_match_the_pinned_gapbs_pbfs_oracle() {
    // GAPBS 941462c7c42e786e4e006a9211b0576ef78d23f7, unmodified src/bc.cc
    // PBFS, OMP_NUM_THREADS=1. Its queue layers were checked against Floyd and
    // path counts against an independent distance recurrence. Hash little-
    // endian u64 tuples (n, mask, source, node, distance-or-MAX, path_count).
    // GAPBS's sampled/max-normalized/source-inclusive BC scores are NOT the
    // registered FNX result contract; this oracle covers BFS states only.
    let mut digest = 0xcbf2_9ce4_8422_2325u64;
    let mut sources = 0;
    let mut tuples = 0;
    small_graphs(|n, mask, rows| {
        let graph = Fixture::new(rows);
        let mut state = Brandes::new(n, usize::MAX, &mut || Ok(())).unwrap();
        while !state.complete() {
            if let Phase::Dependencies { source, remaining } = state.phase
                && remaining == state.queue.len()
            {
                sources += 1;
                for vertex in 0..n {
                    let count = state.sigma[vertex];
                    assert!(count.is_finite() && count >= 0.0 && count.fract() == 0.0);
                    let depth = if state.distance[vertex] == usize::MAX {
                        u64::MAX
                    } else {
                        state.distance[vertex] as u64
                    };
                    for value in [
                        n as u64,
                        mask as u64,
                        source as u64,
                        vertex as u64,
                        depth,
                        count as u64,
                    ] {
                        for byte in value.to_le_bytes() {
                            digest ^= u64::from(byte);
                            digest = digest.wrapping_mul(0x100_0000_01b3);
                        }
                    }
                    tuples += 1;
                }
            }
            if let Some(vertex) = state.row() {
                let mut row = graph.open(vertex).unwrap();
                while let Some((neighbor, _)) = row.next().unwrap() {
                    state.visit(neighbor).unwrap();
                }
                state.end_row(graph.degree(vertex)).unwrap();
            } else {
                state.scalar_step().unwrap();
            }
        }
    });
    assert_eq!((sources, tuples), (5405, 26705));
    assert_eq!(digest, 0x41ad_b968_0c4d_4375);
}

#[test]
fn paths_hubs_many_shortest_paths_and_disconnected_graphs_keep_reference_bits() {
    let path: Vec<_> = (1..71).map(|vertex| (vertex - 1, vertex)).collect();
    let hub: Vec<_> = (1..71).map(|vertex| (0, vertex)).collect();
    let mut layered = Vec::new();
    for first in (0..60).step_by(3) {
        for source in first..first + 3 {
            for target in first + 3..first + 6 {
                layered.push((source, target));
            }
        }
    }
    for edges in [path, hub, layered] {
        let rows = rows(75, &edges);
        let graph = Fixture::new(rows.clone());
        let (output, peak) = run(&graph, None, usize::MAX, &mut || Ok(())).unwrap();
        assert_eq!(bits(&output), reference(&rows));
        assert_eq!(peak, workspace(rows.len()).unwrap());
        assert!(output.witness.queue_peak <= rows.len());
    }
}

#[test]
fn isolates_use_linear_scalar_work_and_one_row_open_each() {
    let n = 4096;
    let graph = Fixture::new(vec![Vec::new(); n]);
    let mut ticks = 0;
    let (output, peak) = run(&graph, None, usize::MAX, &mut || {
        ticks += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(bits(&output), vec![0.0_f64.to_bits(); n]);
    assert_eq!(output.witness.nodes_touched, n);
    assert_eq!(output.witness.edges_scanned, 0);
    assert_eq!(output.witness.queue_peak, 1);
    assert!(graph.opens.iter().all(|count| count.get() == 1));
    assert!(
        ticks < 12 * n,
        "unreached vertices must not be cleared per source"
    );
    assert_eq!(peak, workspace(n).unwrap());
}

#[test]
fn every_scalar_and_compressed_source_failure_discards_partial_dependencies() {
    let rows = rows(6, &[(0, 1), (0, 2), (1, 3), (2, 3), (3, 4)]);
    let graph = Fixture::new(rows.clone());
    let mut total = 0;
    run(&graph, None, usize::MAX, &mut || {
        total += 1;
        Ok(())
    })
    .unwrap();
    let source_total = graph.source_steps.get();
    for stop in 1..=total {
        let graph = Fixture::new(rows.clone());
        let mut seen = 0;
        let error = run(&graph, None, usize::MAX, &mut || {
            seen += 1;
            if seen == stop {
                Err(super::super::Error::Cancelled(
                    std::io::Error::other("Brandes stop").into(),
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
        let error = run(&graph, None, usize::MAX, &mut || Ok(()))
            .err()
            .expect("source refusal");
        assert!(matches!(error, super::super::Error::Source(
            SealedProjectionError::UnknownOrdinal(actual)) if actual == stop));
        assert_eq!(graph.source_steps.get(), stop);
    }
}

#[test]
fn limits_overflow_and_invalid_numeric_or_source_values_fail_closed() {
    let graph = Fixture::new(rows(3, &[(0, 1)]));
    let exact = workspace(3).unwrap();
    assert!(matches!(
        run(&graph, None, exact - 1, &mut || Ok(())),
        Err(super::super::Error::Execution(
            ExecutionError::LimitExceeded {
                resource: "kernel workspace bytes",
                ..
            }
        ))
    ));
    assert_eq!(graph.source_steps.get(), 0);
    run(&graph, None, exact, &mut || Ok(())).unwrap();
    assert!(matches!(
        run(&Fixture::new(vec![vec![1]]), None, usize::MAX, &mut || Ok(
            ()
        )),
        Err(super::super::Error::Execution(
            ExecutionError::InvalidUpstreamResult
        ))
    ));
    assert!(matches!(
        run(&Fixture::new(vec![vec![0]]), None, usize::MAX, &mut || Ok(
            ()
        )),
        Err(super::super::Error::Execution(
            ExecutionError::SelfLoopRefused
        ))
    ));
    for rows in [
        vec![vec![1, 1], vec![0]],
        vec![vec![2, 1], vec![0], vec![0]],
    ] {
        assert!(matches!(
            run(&Fixture::new(rows), None, usize::MAX, &mut || Ok(())),
            Err(super::super::Error::Execution(
                ExecutionError::InvalidUpstreamResult
            ))
        ));
    }
    for degree in [0, 2] {
        let mut graph = Fixture::new(vec![vec![1], vec![0]]);
        graph.degrees = Some(vec![degree, 1]);
        assert!(matches!(
            run(&graph, None, usize::MAX, &mut || Ok(())),
            Err(super::super::Error::Execution(
                ExecutionError::InvalidUpstreamResult
            ))
        ));
    }
    let mut edges = vec![(0, 1), (0, 2)];
    // Only the last layer overflows: checking final score finiteness alone
    // would accept a finite but undercounted result from source zero.
    for layer in 0..1024 {
        for source in [1 + 2 * layer, 2 + 2 * layer] {
            for target in [3 + 2 * layer, 4 + 2 * layer] {
                edges.push((source, target));
            }
        }
    }
    assert!(matches!(
        run(
            &Fixture::new(rows(2051, &edges)),
            None,
            usize::MAX,
            &mut || Ok(())
        ),
        Err(super::super::Error::Execution(
            ExecutionError::InvalidNumericResult
        ))
    ));
    assert!(workspace(usize::MAX).is_err());
    assert!(work(usize::MAX, 0, 0, 2, false).is_err());
    assert!(work(2, 0, usize::MAX, 2, false).is_err());
    assert!(work(1, usize::MAX, 0, 2, false).is_err());
    assert!(work(1, 0, 0, usize::MAX, false).is_err());
}

#[test]
fn directed_incoming_dependencies_match_pinned_oracle_in_both_orientations() {
    let cases = [
        Vec::<Vec<usize>>::new(),
        vec![vec![]],
        vec![vec![1], vec![2], vec![]],
        vec![vec![], vec![], vec![0, 1], vec![1], vec![5], vec![], vec![]],
        vec![
            vec![1, 5],
            vec![2],
            vec![4],
            vec![2],
            vec![1],
            vec![2],
            vec![],
        ],
    ];
    for rows in cases {
        let transpose = |rows: &[Vec<usize>]| {
            let mut incoming = vec![Vec::new(); rows.len()];
            for (source, row) in rows.iter().enumerate() {
                for &target in row {
                    incoming[target].push(source);
                }
            }
            incoming
        };
        for reversed in [false, true] {
            let outgoing = if reversed {
                transpose(&rows)
            } else {
                rows.clone()
            };
            let incoming = transpose(&outgoing);
            let n = outgoing.len();
            let mut oracle = fnx_classes::digraph::DiGraph::strict();
            for vertex in 0..n {
                oracle.add_node(format!("{vertex:032x}"));
            }
            for (source, row) in outgoing.iter().enumerate() {
                for &target in row {
                    oracle
                        .add_edge(format!("{source:032x}"), format!("{target:032x}"))
                        .unwrap();
                }
            }
            let expected: Vec<_> = fnx_algorithms::betweenness_centrality_directed(&oracle)
                .scores
                .iter()
                .map(|score| score.score.to_bits())
                .collect();
            let graph = Fixture::new(outgoing);
            let reverse = Fixture::new(incoming);
            let (output, peak) = run(
                &graph,
                Some(&reverse),
                workspace(n).unwrap(),
                &mut || Ok(()),
            )
            .unwrap();
            let KernelValues::Scores(scores) = output.values else {
                panic!("scores");
            };
            assert_eq!(
                scores
                    .iter()
                    .map(|score| score.to_bits())
                    .collect::<Vec<_>>(),
                expected,
                "n={n}, reversed={reversed}"
            );
            assert_eq!(peak, workspace(n).unwrap());
        }
    }
    assert!(work(64, 8, 0, 2, true).unwrap() > work(64, 8, 0, 2, false).unwrap());
    assert!(work(1, usize::MAX, 0, 2, true).is_err());
}
