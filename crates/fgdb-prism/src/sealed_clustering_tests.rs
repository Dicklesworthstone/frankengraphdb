use super::super::{Error, SealedProjectionError};
use super::*;
use crate::{
    Directedness, ParallelEdgePolicy, ProjectionEdge, ProjectionLimits, ProjectionSpec,
    SelfLoopPolicy, SnapshotBinding, SnapshotGraphView, triangle_statistics,
};
use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
use std::cell::Cell;
use std::convert::Infallible;

struct Fixture {
    rows: Vec<Vec<usize>>,
    retained: Vec<usize>,
    steps: Cell<usize>,
    fail_at: usize,
    opens: Vec<Cell<usize>>,
}
impl Fixture {
    fn new(rows: Vec<Vec<usize>>, retained: Vec<usize>) -> Self {
        let opens = (0..rows.len()).map(|_| Cell::new(0)).collect();
        Self {
            rows,
            retained,
            steps: Cell::new(0),
            fail_at: usize::MAX,
            opens,
        }
    }
    fn step(&self) -> Result<()> {
        let step = self.steps.get() + 1;
        self.steps.set(step);
        if step == self.fail_at {
            Err(Error::Source(SealedProjectionError::UnknownOrdinal(
                usize::MAX,
            )))
        } else {
            Ok(())
        }
    }
    fn reset(&self) {
        self.steps.set(0);
        for count in &self.opens {
            count.set(0);
        }
    }
    fn execute(
        &self,
        coefficients: bool,
        limit: usize,
        control: &mut impl FnMut() -> Result<()>,
    ) -> Result<(KernelOutput, usize)> {
        let history = self.retained.iter().sum();
        let plan = prepare(
            self,
            history,
            |source| Ok(self.retained[source]),
            coefficients,
            limit,
            control,
        )?;
        let work = plan.work;
        let (output, _) = run(self, plan, coefficients, usize::MAX, control)?;
        Ok((output, work))
    }
}
struct FixtureRow<'a> {
    fixture: &'a Fixture,
    targets: &'a [usize],
    at: usize,
}
impl Cursor for FixtureRow<'_> {
    fn next(&mut self) -> Result<Option<(usize, f64)>> {
        self.fixture.step()?;
        let result = self.targets.get(self.at).copied();
        self.at += 1;
        Ok(result.map(|target| (target, -7.0))) // topology is intentionally unweighted
    }
}
impl Rows for Fixture {
    type Cursor<'a>
        = FixtureRow<'a>
    where
        Self: 'a;
    fn node_count(&self) -> usize {
        self.rows.len()
    }
    fn degree(&self, source: usize) -> Option<usize> {
        self.rows.get(source).map(Vec::len)
    }
    fn open(&self, source: usize) -> Result<Self::Cursor<'_>> {
        self.step()?;
        let targets = self
            .rows
            .get(source)
            .ok_or(ExecutionError::InvalidUpstreamResult)?;
        self.opens[source].set(self.opens[source].get() + 1);
        Ok(FixtureRow {
            fixture: self,
            targets,
            at: 0,
        })
    }
}

fn oracle(rows: &[Vec<usize>]) -> (Vec<u64>, Vec<f64>) {
    let n = rows.len();
    let mut counts = vec![0; n];
    for a in 0..n {
        for b in a + 1..n {
            for c in b + 1..n {
                if rows[a].contains(&b) && rows[b].contains(&c) && rows[c].contains(&a) {
                    counts[a] += 1;
                    counts[b] += 1;
                    counts[c] += 1;
                }
            }
        }
    }
    let scores = rows
        .iter()
        .enumerate()
        .map(|(v, row)| {
            let d = row.iter().filter(|&&target| target != v).count();
            if d < 2 {
                0.0
            } else {
                (2 * counts[v]) as f64 / (d * (d - 1)) as f64
            }
        })
        .collect();
    (counts, scores)
}

#[test]
fn every_four_vertex_graph_and_loop_assignment_matches_cubic_and_decoded_oracles() {
    let pairs: Vec<_> = (0..4).flat_map(|s| (s..4).map(move |t| (s, t))).collect();
    for mask in 0..(1 << pairs.len()) {
        let mut rows = vec![Vec::new(); 4];
        let vertices = [VId(0), VId(1), VId(1 << 100), VId(u128::MAX)];
        let mut edges = Vec::new();
        for (bit, &(s, t)) in pairs.iter().enumerate() {
            if mask & (1 << bit) != 0 {
                rows[s].push(t);
                if s != t {
                    rows[t].push(s);
                }
                edges.push(ProjectionEdge {
                    eid: EId(bit as u128),
                    source: vertices[s],
                    target: vertices[t],
                    weight: -7.0,
                });
            }
        }
        for row in &mut rows {
            row.sort_unstable();
        }
        let (counts, scores) = oracle(&rows);
        let decoded = SnapshotGraphView::build(
            SnapshotBinding {
                root: ObjectId([4; 32]),
                as_of: CommitSeq(3),
            },
            &vertices,
            &edges,
            ProjectionSpec {
                directedness: Directedness::Undirected,
                parallel_edges: ParallelEdgePolicy::Reject,
                self_loops: SelfLoopPolicy::Keep,
            },
            ProjectionLimits {
                max_vertices: 4,
                max_input_edges: 10,
                max_adjacency_entries: 16,
                max_workspace_bytes: 1 << 20,
            },
        )
        .unwrap();
        let expected = triangle_statistics(
            &decoded,
            FnxExecutionLimits {
                max_iterations: 0,
                max_result_rows: 4,
                max_estimated_work: usize::MAX,
            },
            || Ok::<(), Infallible>(()),
        )
        .unwrap();
        assert_eq!(expected.triangles, counts);
        assert_eq!(expected.clustering, scores);
        for retained in [
            rows.iter().map(Vec::len).collect(),
            vec![1000, 0, 30, 4],
            vec![0; 4],
            vec![4, 3, 2, 1],
        ] {
            // Arbitrary costs test that orientation is not part of semantics.
            let graph = Fixture::new(rows.clone(), retained);
            let (actual, work) = graph.execute(false, usize::MAX, &mut || Ok(())).unwrap();
            let KernelValues::Counts(actual) = actual.values else {
                panic!("counts");
            };
            assert_eq!(actual, counts, "mask={mask}");
            let (actual, score_work) = graph.execute(true, usize::MAX, &mut || Ok(())).unwrap();
            let KernelValues::Scores(actual) = actual.values else {
                panic!("scores");
            };
            assert_eq!(
                actual.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
                scores.iter().map(|x| x.to_bits()).collect::<Vec<_>>()
            );
            assert_eq!(work, score_work);
        }
    }
}

#[test]
fn retained_cost_orientation_minimizes_the_charged_inner_scans_not_visible_degree() {
    let graph = Fixture::new(vec![vec![1, 2], vec![0, 2], vec![0, 1]], vec![10_000, 2, 2]);
    let history = graph.retained.iter().sum();
    let costs: Vec<_> = graph
        .retained
        .iter()
        .map(|&h| row_work(3, history, h).unwrap())
        .collect();
    let (output, work) = graph.execute(false, usize::MAX, &mut || Ok(())).unwrap();
    let KernelValues::Counts(counts) = output.values else {
        panic!("counts");
    };
    assert_eq!(counts, [1, 1, 1]);
    assert_eq!(
        graph.opens[0].get(),
        2,
        "heavy row is never an inner rescan"
    );
    let inner = costs[0].min(costs[1]) + costs[0].min(costs[2]) + costs[1].min(costs[2]);
    assert_eq!(
        work,
        3 * (32 + 4 * bits(history)) + 2 * costs.iter().sum::<usize>() + 3 * 6 + inner
    );
    let degree_order_inner = costs[0] * 2 + costs[1];
    assert!(
        degree_order_inner > inner,
        "negative control penalizes hidden history"
    );
    graph.reset();
    graph.execute(false, work, &mut || Ok(())).unwrap();
    assert!(matches!(
        graph.execute(false, work - 1, &mut || Ok(())),
        Err(Error::Execution(ExecutionError::LimitExceeded {
            resource: "estimated work",
            ..
        }))
    ));
    graph.reset();
    assert!(graph.execute(false, 0, &mut || Ok(())).is_err());
    assert_eq!(
        graph.steps.get(),
        0,
        "zero budget precedes all source opens"
    );
}

#[test]
fn every_planning_counting_and_conversion_checkpoint_discards_partial_results() {
    let graph = Fixture::new(vec![vec![0, 1, 2], vec![0, 2], vec![0, 1]], vec![10, 2, 5]);
    for coefficients in [false, true] {
        let mut calls = 0;
        graph
            .execute(coefficients, usize::MAX, &mut || {
                calls += 1;
                Ok(())
            })
            .unwrap();
        assert!(calls > 30);
        for stop in 1..=calls {
            let mut seen = 0;
            let result = graph.execute(coefficients, usize::MAX, &mut || {
                seen += 1;
                if seen == stop {
                    Err(Error::Cancelled(std::io::Error::other("stop").into()))
                } else {
                    Ok(())
                }
            });
            assert!(matches!(result, Err(Error::Cancelled(_))));
            assert_eq!(seen, stop);
        }
    }
}

#[test]
fn failure_at_every_source_open_pull_and_eof_never_continues() {
    for coefficients in [false, true] {
        let mut graph = Fixture::new(vec![vec![0, 1, 2], vec![0, 2], vec![0, 1]], vec![3, 2, 2]);
        graph
            .execute(coefficients, usize::MAX, &mut || Ok(()))
            .unwrap();
        let operations = graph.steps.get();
        for stop in 1..=operations {
            graph.reset();
            graph.fail_at = stop;
            assert!(matches!(
                graph.execute(coefficients, usize::MAX, &mut || Ok(())),
                Err(Error::Source(SealedProjectionError::UnknownOrdinal(
                    usize::MAX
                )))
            ));
            assert_eq!(graph.steps.get(), stop);
        }
    }
}

#[test]
fn empty_isolated_loop_only_and_malformed_rows_have_explicit_outcomes() {
    for rows in [vec![], vec![vec![]], vec![vec![0]], vec![vec![0], vec![1]]] {
        let graph = Fixture::new(rows.clone(), rows.iter().map(Vec::len).collect());
        for coefficients in [false, true] {
            let (actual, _) = graph
                .execute(coefficients, usize::MAX, &mut || Ok(()))
                .unwrap();
            assert_eq!(actual.row_count, rows.len());
            match actual.values {
                KernelValues::Scores(values) => assert!(values.iter().all(|&x| x == 0.0)),
                KernelValues::Counts(values) => assert!(values.iter().all(|&x| x == 0)),
                _ => panic!("wrong output"),
            }
        }
    }
    for row in [vec![usize::MAX], vec![1], vec![0, 0]] {
        let graph = Fixture::new(vec![row], vec![100]);
        assert!(matches!(
            graph.execute(false, usize::MAX, &mut || Ok(())),
            Err(Error::Execution(ExecutionError::InvalidUpstreamResult))
        ));
    }
    let graph = Fixture::new(vec![vec![1, 0], vec![0]], vec![2, 1]);
    assert!(graph.execute(false, usize::MAX, &mut || Ok(())).is_err());
    for coefficients in [false, true] {
        assert_eq!(workspace(0, coefficients).unwrap(), 0);
        assert!(workspace(usize::MAX, coefficients).is_err());
    }
    assert!(row_work(3, usize::MAX, usize::MAX).is_err());
    let mut work = usize::MAX;
    assert!(charge(&mut work, 1, usize::MAX).is_err());
    assert_eq!(work, usize::MAX);
}

#[test]
fn leaves_and_loop_only_rows_are_validated_once_and_never_reopened_for_counting() {
    let mut rows = vec![Vec::new(); 12];
    for (source, target) in [(0, 1), (1, 2), (2, 0)]
        .into_iter()
        .chain((3..10).map(|leaf| (0, leaf)))
        .chain([(3, 3), (5, 5), (11, 11)])
    {
        rows[source].push(target);
        if source != target {
            rows[target].push(source);
        }
    }
    for row in &mut rows {
        row.sort_unstable();
    }
    let (counts, scores) = oracle(&rows);
    // Expensive hidden history must not make a leaf eligible, and a retained
    // row of length two containing its self-loop still has only one neighbor.
    let graph = Fixture::new(rows, vec![10, 2, 2, 50_000, 1, 80_000, 1, 1, 1, 1, 30, 90]);
    for coefficients in [false, true] {
        graph.reset();
        let (output, _) = graph
            .execute(coefficients, usize::MAX, &mut || Ok(()))
            .unwrap();
        match output.values {
            KernelValues::Counts(actual) => assert_eq!(actual, counts),
            KernelValues::Scores(actual) => assert_eq!(
                actual
                    .iter()
                    .map(|value| value.to_bits())
                    .collect::<Vec<_>>(),
                scores
                    .iter()
                    .map(|value| value.to_bits())
                    .collect::<Vec<_>>()
            ),
            _ => panic!("wrong output"),
        }
        for source in 3..12 {
            assert_eq!(graph.opens[source].get(), 1, "source={source}");
        }
        // Each actual planning/counting open, pull and EOF still propagates
        // its error immediately, including the mandatory leaf validations.
        let operations = graph.steps.get();
        for stop in 1..=operations {
            let mut interrupted = Fixture::new(graph.rows.clone(), graph.retained.clone());
            interrupted.fail_at = stop;
            assert!(matches!(
                interrupted.execute(coefficients, usize::MAX, &mut || Ok(())),
                Err(Error::Source(SealedProjectionError::UnknownOrdinal(
                    usize::MAX
                )))
            ));
            assert_eq!(interrupted.steps.get(), stop);
        }
    }
}

#[test]
fn a_source_needs_two_eligible_lower_neighbors_before_any_repeated_inner_scan() {
    let n = 17;
    let mut rows: Vec<_> = (0..n)
        .map(|source| {
            let mut row = vec![(source + n - 1) % n, (source + 1) % n];
            row.sort_unstable();
            row
        })
        .collect();
    // A separate leaf pair selects pruning; the cycle's non-leaf rows then
    // exercise the independent two-lower-neighbor proof.
    rows.push(vec![n + 1]);
    rows.push(vec![n]);
    let graph = Fixture::new(rows, vec![2; n + 2]);
    let (output, _) = graph.execute(false, usize::MAX, &mut || Ok(())).unwrap();
    let KernelValues::Counts(counts) = output.values else {
        panic!("counts");
    };
    assert_eq!(counts, vec![0; n + 2]);
    assert_eq!(graph.opens.iter().map(Cell::get).sum::<usize>(), 2 * n + 4);
    assert_eq!(output.witness.edges_scanned, 2 * n + 4);
    assert_eq!(output.witness.nodes_touched, n + 2);
}

#[test]
fn isolates_alone_do_not_select_neighbor_pruning_for_the_connected_population() {
    let graph = Fixture::new(
        vec![vec![1, 2], vec![0, 2], vec![0, 1], vec![]],
        vec![2, 2, 2, 1000],
    );
    let plan = prepare(
        &graph,
        graph.retained.iter().sum(),
        |source| Ok(graph.retained[source]),
        false,
        usize::MAX,
        &mut || Ok(()),
    )
    .unwrap();
    assert!(!plan.prune);
    assert_eq!(plan.costs[3], 0);
    let (output, _) = run(&graph, plan, false, usize::MAX, &mut || Ok(())).unwrap();
    let KernelValues::Counts(counts) = output.values else {
        panic!("counts");
    };
    assert_eq!(counts, [1, 1, 1, 0]);
    assert_eq!(graph.opens[3].get(), 2);
    assert_eq!(output.witness.edges_scanned, 11);
}

#[test]
fn all_eligible_rows_enumerate_marks_without_reopening_sources() {
    let n = 17;
    let rows: Vec<_> = (0..n)
        .map(|source| {
            let mut row = vec![(source + n - 1) % n, (source + 1) % n];
            row.sort_unstable();
            row
        })
        .collect();
    let graph = Fixture::new(rows, vec![2; n]);
    let mut controls = 0;
    let (output, _) = graph
        .execute(false, usize::MAX, &mut || {
            controls += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        output.witness.algorithm,
        "triangles_retained_cost_oriented_bitmap"
    );
    assert_eq!(output.witness.edges_scanned, 4 * n - 1);
    assert_eq!(output.witness.nodes_touched, n);
    assert_eq!(graph.opens.iter().map(Cell::get).sum::<usize>(), 3 * n);
    // The first out-of-range neighbor remains a visit; its sorted suffix and
    // the inner cursor's unnecessary EOF pull are no longer performed.
    assert_eq!(graph.steps.get(), 11 * n - 1);
    assert_eq!(controls, 13 * n);
}

#[test]
fn bipartite_rows_stop_after_the_first_neighbor_beyond_all_marks() {
    for d in [2, 3, 16, 64, 129] {
        let n = 2 * d;
        let rows: Vec<_> = (0..n)
            .map(|source| {
                if source < d {
                    (d..n).collect()
                } else {
                    (0..d).collect()
                }
            })
            .collect();
        let graph = Fixture::new(rows, vec![d; n]);
        for coefficients in [false, true] {
            graph.reset();
            let (output, work) = graph
                .execute(coefficients, usize::MAX, &mut || Ok(()))
                .unwrap();
            assert_eq!(
                output.witness.algorithm,
                "triangles_retained_cost_oriented_bitmap"
            );
            // K(d,d) has no triangles and no degree-one rows. Every higher
            // partition source marks the whole lower partition. Each inner
            // row's very first neighbor is beyond that entire marked set.
            match output.values {
                KernelValues::Counts(counts) => assert_eq!(counts, vec![0; n]),
                KernelValues::Scores(scores) => assert_eq!(
                    scores
                        .iter()
                        .map(|score| score.to_bits())
                        .collect::<Vec<_>>(),
                    vec![0.0f64.to_bits(); n]
                ),
                _ => panic!("wrong output"),
            }
            // These counters observe real cursor opens/pulls, including full
            // planning. They distinguish quadratic work from the former d^3
            // inner visits without depending on wall-clock timing.
            assert_eq!(output.witness.edges_scanned, 3 * d * d, "d={d}");
            assert_eq!(
                graph.opens.iter().map(Cell::get).sum::<usize>(),
                4 * d + d * d
            );
            assert_eq!(graph.steps.get(), 6 * d * d + 8 * d);
            let history = 2 * d * d;
            let row_cost = row_work(n, history, d).unwrap();
            assert_eq!(
                work,
                n * (32 + 4 * bits(history)) + (2 * n + d * d) * row_cost + 6 * d * d,
                "admission still covers every possible full inner scan"
            );
            graph.execute(coefficients, work, &mut || Ok(())).unwrap();
            assert!(matches!(
                graph.execute(coefficients, work - 1, &mut || Ok(())),
                Err(Error::Execution(ExecutionError::LimitExceeded {
                    resource: "estimated work",
                    ..
                }))
            ));
        }
    }
}

#[test]
fn retained_history_can_place_a_closing_neighbor_above_the_inner_source_ordinal() {
    let graph = Fixture::new(
        vec![vec![1, 2], vec![0, 2], vec![0, 1]],
        vec![30_000, 2_000, 2],
    );
    // Source 0 marks 1 and 2, then row 1 must reach 2 to close the triangle.
    // A cutoff at the inner source's ordinal, as opposed to the actual maximum
    // mark, would silently lose it when retained cost reverses ordinal order.
    for coefficients in [false, true] {
        let (output, _) = graph
            .execute(coefficients, usize::MAX, &mut || Ok(()))
            .unwrap();
        match output.values {
            KernelValues::Counts(counts) => assert_eq!(counts, [1, 1, 1]),
            KernelValues::Scores(scores) => assert_eq!(
                scores
                    .iter()
                    .map(|score| score.to_bits())
                    .collect::<Vec<_>>(),
                [1.0f64.to_bits(); 3]
            ),
            _ => panic!("wrong output"),
        }
    }
}

#[test]
fn random_relabelings_and_retained_history_match_independent_triples() {
    let mut state = 0x7172_616e_676c_6573u64;
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };
    for n in [9, 17, 65] {
        for sample in 0..8 {
            let mut original = vec![Vec::new(); n];
            for source in 0..n {
                for target in source..n {
                    if next() % 5 < 2 {
                        original[source].push(target);
                        if source != target {
                            original[target].push(source);
                        }
                    }
                }
            }
            for row in &mut original {
                row.sort_unstable();
            }
            let (original_counts, original_scores) = oracle(&original);
            for reverse in [false, true] {
                let ordinal = |vertex| if reverse { n - 1 - vertex } else { vertex };
                let mut rows = vec![Vec::new(); n];
                for (source, row) in original.iter().enumerate() {
                    for &target in row.iter().rev() {
                        rows[ordinal(source)].push(ordinal(target));
                    }
                }
                for row in &mut rows {
                    row.sort_unstable();
                }
                let retained: Vec<_> = rows
                    .iter()
                    .map(|row| row.len() + (next() % 4096) as usize)
                    .collect();
                let graph = Fixture::new(rows, retained);
                for coefficients in [false, true] {
                    let (output, _) = graph
                        .execute(coefficients, usize::MAX, &mut || Ok(()))
                        .unwrap();
                    for vertex in 0..n {
                        let at = ordinal(vertex);
                        match &output.values {
                            KernelValues::Counts(counts) => assert_eq!(
                                counts[at], original_counts[vertex],
                                "n={n}, sample={sample}, reverse={reverse}, vertex={vertex}"
                            ),
                            KernelValues::Scores(scores) => assert_eq!(
                                scores[at].to_bits(),
                                original_scores[vertex].to_bits(),
                                "n={n}, sample={sample}, reverse={reverse}, vertex={vertex}"
                            ),
                            _ => panic!("wrong output"),
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn planning_validates_suffixes_that_the_counting_bound_would_skip() {
    for suffix in [vec![3, 5, 4], vec![3, 4, usize::MAX]] {
        let mut rows = vec![
            vec![3, 4, 5],
            vec![3, 4, 5],
            vec![3, 4, 5],
            vec![0, 1, 2],
            vec![0, 1, 2],
            vec![0, 1, 2],
        ];
        rows[0] = suffix;
        let graph = Fixture::new(rows, vec![3; 6]);
        assert!(matches!(
            graph.execute(false, usize::MAX, &mut || Ok(())),
            Err(Error::Execution(ExecutionError::InvalidUpstreamResult))
        ));
    }
}

#[test]
fn marks_cross_word_boundaries_and_survive_every_inner_scan_until_source_completion() {
    let n = 193;
    let clique = [0, 1, 63, 64, 65, 127, 128, 191, 192];
    for leaves in [false, true] {
        let mut rows = vec![Vec::new(); n];
        for (source, target) in clique
            .iter()
            .enumerate()
            .flat_map(|(i, &source)| clique[i + 1..].iter().map(move |&target| (source, target)))
            .chain([
                (2, 62),
                (62, 126),
                (126, 2),
                (63, 129),
                (129, 190),
                (190, 63),
            ])
            .chain([(64, 64), (4, 4)])
            .chain(leaves.then_some((3, 64)))
        {
            rows[source].push(target);
            if source != target {
                rows[target].push(source);
            }
        }
        for row in &mut rows {
            row.sort_unstable();
        }
        let (counts, scores) = oracle(&rows);
        for heavy in [None, Some(0), Some(64), Some(192)] {
            let mut retained: Vec<_> = rows.iter().map(Vec::len).collect();
            if let Some(heavy) = heavy {
                retained[heavy] = 1_000_000;
            }
            let graph = Fixture::new(rows.clone(), retained);
            let (output, _) = graph.execute(false, usize::MAX, &mut || Ok(())).unwrap();
            let KernelValues::Counts(actual) = output.values else {
                panic!("counts");
            };
            assert_eq!(actual, counts, "leaves={leaves}, heavy={heavy:?}");
            let (output, _) = graph.execute(true, usize::MAX, &mut || Ok(())).unwrap();
            let KernelValues::Scores(actual) = output.values else {
                panic!("scores");
            };
            assert_eq!(
                actual
                    .iter()
                    .map(|value| value.to_bits())
                    .collect::<Vec<_>>(),
                scores
                    .iter()
                    .map(|value| value.to_bits())
                    .collect::<Vec<_>>(),
                "leaves={leaves}, heavy={heavy:?}"
            );
        }
    }
}

#[test]
fn word_rounding_and_actual_capacities_control_workspace_admission() {
    assert_eq!(bitmap_words(usize::MAX), usize::MAX / 64 + 1);
    for n in [0, 1, 63, 64, 65, 127, 128, 129] {
        for coefficients in [false, true] {
            let mut rows = vec![Vec::new(); n];
            if n >= 3 {
                rows[0] = vec![1, 2];
                rows[1] = vec![0, 2];
                rows[2] = vec![0, 1];
            }
            let graph = Fixture::new(rows, vec![6; n]);
            let prepare_plan = || {
                prepare(
                    &graph,
                    6 * n,
                    |source| Ok(graph.retained[source]),
                    coefficients,
                    usize::MAX,
                    &mut || Ok(()),
                )
                .unwrap()
            };
            let expected = workspace(n, coefficients).unwrap();
            let (output, actual) = run(&graph, prepare_plan(), coefficients, expected, &mut || {
                Ok(())
            })
            .unwrap();
            assert_eq!(output.row_count, n);
            assert_eq!(actual, expected);
            if n == 0 {
                continue;
            }
            assert!(matches!(
                run(
                    &graph,
                    prepare_plan(),
                    coefficients,
                    expected - 1,
                    &mut || Ok(())
                ),
                Err(Error::Execution(ExecutionError::LimitExceeded {
                    resource: "kernel workspace bytes",
                    ..
                }))
            ));
            // A Vec may retain more capacity than its length. Such backing
            // storage must be charged even though only n costs are consulted.
            let mut extra = prepare_plan();
            extra.costs.reserve_exact(n);
            assert!(extra.costs.capacity() > n);
            assert!(matches!(
                run(&graph, extra, coefficients, expected, &mut || Ok(())),
                Err(Error::Execution(ExecutionError::LimitExceeded {
                    resource: "kernel workspace bytes",
                    ..
                }))
            ));
        }
    }
}

#[test]
fn twenty_thousand_vertex_hub_keeps_vertex_workspace_and_avoids_all_leaf_rescans() {
    struct Star {
        n: usize,
        opens: Cell<usize>,
    }
    struct StarRow {
        source: usize,
        n: usize,
        at: usize,
    }
    impl Cursor for StarRow {
        fn next(&mut self) -> Result<Option<(usize, f64)>> {
            let target = if self.source == 0 {
                (self.at + 1 < self.n).then_some(self.at + 1)
            } else {
                (self.at == 0).then_some(0)
            };
            self.at += 1;
            Ok(target.map(|target| (target, 1.0)))
        }
    }
    impl Rows for Star {
        type Cursor<'a>
            = StarRow
        where
            Self: 'a;
        fn node_count(&self) -> usize {
            self.n
        }
        fn degree(&self, source: usize) -> Option<usize> {
            (source < self.n).then_some(if source == 0 { self.n - 1 } else { 1 })
        }
        fn open(&self, source: usize) -> Result<StarRow> {
            self.opens.set(self.opens.get() + 1);
            Ok(StarRow {
                source,
                n: self.n,
                at: 0,
            })
        }
    }
    let graph = Star {
        n: 20_000,
        opens: Cell::new(0),
    };
    let plan = prepare(
        &graph,
        graph.n - 1,
        |v| Ok(graph.degree(v).unwrap()),
        true,
        usize::MAX,
        &mut || Ok(()),
    )
    .unwrap();
    let (output, actual_workspace) = run(&graph, plan, true, usize::MAX, &mut || Ok(())).unwrap();
    assert_eq!(graph.opens.get(), graph.n + 1);
    assert_eq!(output.witness.edges_scanned, graph.n - 1);
    assert_eq!(output.witness.nodes_touched, graph.n);
    let KernelValues::Scores(scores) = output.values else {
        panic!("scores");
    };
    assert_eq!(scores, vec![0.0; graph.n]);
    assert_eq!(
        workspace(graph.n, true).unwrap(),
        (graph.n * (2 * size_of::<usize>() + size_of::<u64>())
            + graph.n.div_ceil(64) * (size_of::<u64>() + size_of::<usize>()))
        .max(graph.n * (size_of::<usize>() + size_of::<u64>() + size_of::<f64>()))
    );
    assert_eq!(actual_workspace, workspace(graph.n, true).unwrap());
}
