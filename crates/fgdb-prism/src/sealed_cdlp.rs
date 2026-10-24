//! Synchronous Graphalytics label propagation over compressed rows.
//!
//! Labels and epoch-tagged vote counters occupy four O(|V|) integer vectors.
//! Directed graphs visit separately reduced outgoing and incoming rows, so a
//! reciprocal neighbor contributes twice. The reverse projection retains a
//! bounded compressed locator index, never a decoded adjacency copy. Its
//! construction peak participates in the kernel's workspace allowance.

use super::{
    ComplexityWitness, Cursor, Directedness, ExecutionError, FnxAlgorithm, FnxCallSpec,
    FnxExecutionLimits, FnxMemoryLimits, FnxResult, KernelOutput, KernelValues, Result,
    ResultAdmission, Rows, SealedGraphView, SealedRows, add, admit, checkpoint,
    directional_pass_work, finish, mul, reserve,
};
use crate::sealed_control::Control;
use std::mem::size_of;

fn workspace(n: usize, iterations: usize) -> Result<usize> {
    mul(
        n,
        mul(if iterations == 0 { 1 } else { 4 }, size_of::<usize>())?,
    )
}

fn work(
    n: usize,
    retained: usize,
    arcs: usize,
    direction: Directedness,
    iterations: usize,
    columns: usize,
) -> Result<usize> {
    let forward = directional_pass_work(n, retained, direction)?;
    // Loop validation, buffer initialization, and final canonical output.
    let base = add(
        forward,
        add(arcs, add(mul(n, add(12, columns)?)?, add(32, columns)?)?)?,
    )?;
    if n == 0 || iterations == 0 {
        return Ok(base);
    }
    let (reverse, setup, incidences) = if direction == Directedness::Undirected {
        (0, 0, arcs)
    } else {
        let reversed = if direction == Directedness::Directed {
            Directedness::Reversed
        } else {
            Directedness::Directed
        };
        let reverse = directional_pass_work(n, retained, reversed)?;
        // The temporary reverse projection authenticates/counts its rows.
        // A new incoming locator index also collects, heapsorts and encodes
        // retained incidences. Charge that construction even when an already
        // reversed source only needs its original outgoing face.
        let height = (usize::BITS - retained.leading_zeros()) as usize;
        let index = mul(retained, add(32, mul(height, 4)?)?)?;
        let setup = add(mul(reverse, 2)?, add(index, mul(n, 8)?)?)?;
        (reverse, setup, mul(arcs, 2)?)
    };
    let round = add(
        add(forward, reverse)?,
        add(incidences, add(mul(n, 2)?, 4)?)?,
    )?;
    add(base, add(setup, mul(round, iterations)?)?)
}

fn reserved(n: usize, live: &mut usize, limit: usize) -> Result<Vec<usize>> {
    admit(
        "kernel workspace bytes",
        add(*live, mul(n, size_of::<usize>())?)?,
        limit,
    )?;
    let values = reserve(n)?;
    let actual = add(*live, mul(values.capacity(), size_of::<usize>())?)?;
    // An allocator may exceed try_reserve_exact's request. Check the actual
    // backing before initializing it or allocating the next vector.
    admit("kernel workspace bytes", actual, limit)?;
    *live = actual;
    Ok(values)
}

struct Labels {
    labels: Vec<usize>,
    next: Vec<usize>,
    counts: Vec<usize>,
    generations: Vec<usize>,
    generation: usize,
    peak: usize,
    witness: ComplexityWitness,
}

impl Labels {
    fn new(n: usize, iterations: usize, limit: usize) -> Result<Self> {
        admit("kernel workspace bytes", workspace(n, iterations)?, limit)?;
        let mut live = 0;
        let labels = reserved(n, &mut live, limit)?;
        let (next, counts, generations) = if iterations == 0 || n == 0 {
            (Vec::new(), Vec::new(), Vec::new())
        } else {
            (
                reserved(n, &mut live, limit)?,
                reserved(n, &mut live, limit)?,
                reserved(n, &mut live, limit)?,
            )
        };
        Ok(Self {
            labels,
            next,
            counts,
            generations,
            generation: 0,
            peak: live,
            witness: ComplexityWitness {
                algorithm: "cdlp_synchronous_minimum_vid_cursor".to_owned(),
                complexity_claim: "O(|V| + H*(1+log(1+H)) + k*(|V|*(1+log(1+H)) + H*(1+log(1+|V|)))) compressed incidence visits; O(|V|) label workspace plus a bounded compressed reverse index".to_owned(),
                nodes_touched: 0,
                edges_scanned: 0,
                queue_peak: 0,
            },
        })
    }

    fn prepare(
        &mut self,
        graph: &impl Rows,
        iterations: usize,
        control: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        // The registered loop-free graph law also applies to zero updates.
        for source in 0..graph.node_count() {
            control()?;
            let mut row = graph.open(source)?;
            while let Some((target, _)) = row.next()? {
                control()?;
                if target >= graph.node_count() {
                    return Err(ExecutionError::InvalidUpstreamResult.into());
                }
                if target == source {
                    return Err(ExecutionError::SelfLoopRefused.into());
                }
            }
        }
        for vertex in 0..graph.node_count() {
            control()?;
            self.labels.push(vertex);
        }
        if iterations != 0 {
            for values in [&mut self.next, &mut self.counts, &mut self.generations] {
                for _ in 0..graph.node_count() {
                    control()?;
                    values.push(0);
                }
            }
        }
        Ok(())
    }

    fn votes(
        &mut self,
        mut row: impl Cursor,
        best: &mut usize,
        best_count: &mut usize,
        control: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        while let Some((neighbor, _)) = row.next()? {
            control()?;
            let label = *self
                .labels
                .get(neighbor)
                .ok_or(ExecutionError::InvalidUpstreamResult)?;
            if self.generations[label] != self.generation {
                self.generations[label] = self.generation;
                self.counts[label] = 0;
            }
            let count = add(self.counts[label], 1)?;
            self.counts[label] = count;
            if count > *best_count || (count == *best_count && label < *best) {
                *best = label;
                *best_count = count;
            }
            self.witness.edges_scanned = add(self.witness.edges_scanned, 1)?;
        }
        Ok(())
    }

    fn iterate<G: Rows>(
        mut self,
        graph: &G,
        reverse: Option<&G>,
        iterations: usize,
        control: &mut impl FnMut() -> Result<()>,
    ) -> Result<(KernelOutput, usize)> {
        let n = graph.node_count();
        if reverse.is_some_and(|reverse| reverse.node_count() != n) {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        if n != 0 {
            for _ in 0..iterations {
                control()?;
                for source in 0..n {
                    control()?;
                    self.generation = add(self.generation, 1)?;
                    let mut best = self.labels[source];
                    let mut best_count = 0;
                    self.votes(graph.open(source)?, &mut best, &mut best_count, control)?;
                    if let Some(reverse) = reverse {
                        self.votes(reverse.open(source)?, &mut best, &mut best_count, control)?;
                    }
                    self.next[source] = best;
                    self.witness.nodes_touched = add(self.witness.nodes_touched, 1)?;
                }
                // No vertex in this round can see another vertex's new label.
                std::mem::swap(&mut self.labels, &mut self.next);
            }
        }
        control()?;
        Ok((
            KernelOutput {
                values: KernelValues::Components(self.labels),
                row_count: n,
                witness: self.witness,
            },
            self.peak,
        ))
    }
}

pub(super) fn execute(
    call: &FnxCallSpec,
    cx: &Control<'_>,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    admission: &ResultAdmission,
) -> Result<FnxResult> {
    let FnxAlgorithm::Cdlp { max_iter } = call.algorithm() else {
        return Err(super::Error::UnsupportedAlgorithm(call.algorithm()));
    };
    let n = graph.node_count();
    admission.rows(n)?;
    admit("iterations", max_iter, limits.max_iterations)?;
    let estimated = work(
        n,
        graph.scan_incidence_bound(),
        graph.adjacency_entry_count(),
        graph.spec().directedness,
        max_iter,
        call.outputs().len(),
    )?;
    admit("estimated work", estimated, limits.max_estimated_work)?;
    let mut control = || checkpoint(cx);
    let rows = SealedRows { cx, graph };
    let mut state = Labels::new(n, max_iter, memory.max_kernel_workspace_bytes)?;
    state.prepare(&rows, max_iter, &mut control)?;
    let reverse =
        if n != 0 && max_iter != 0 && graph.spec().directedness != Directedness::Undirected {
            let remaining = memory
                .max_kernel_workspace_bytes
                .checked_sub(state.peak)
                .ok_or(ExecutionError::SizeOverflow)?;
            let reverse = graph.reverse_projection_controlled(cx, remaining)?;
            state.peak = add(state.peak, reverse.charged_workspace_bytes())?;
            admit(
                "kernel workspace bytes",
                state.peak,
                memory.max_kernel_workspace_bytes,
            )?;
            Some(reverse)
        } else {
            None
        };
    let reverse_rows = reverse.as_ref().map(|graph| SealedRows { cx, graph });
    let (output, peak) = state.iterate(&rows, reverse_rows.as_ref(), max_iter, &mut control)?;
    drop(reverse);
    finish(
        call,
        graph,
        output,
        "fgdb-prism/sealed-cdlp-synchronous-v1",
        estimated,
        peak,
        admission,
        &mut control,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ParallelEdgePolicy, ProjectionEdge, ProjectionLimits, ProjectionSpec,
        SealedProjectionError, SelfLoopPolicy, SnapshotBinding, SnapshotGraphView,
    };
    use fgdb_types::{CommitSeq, EId, ObjectId, VId};
    use fnx_algorithms::GraphView;
    use std::cell::Cell;
    use std::convert::Infallible;

    struct Fixture {
        rows: Vec<Vec<usize>>,
        source_steps: Cell<usize>,
        fail_at: usize,
    }

    impl Fixture {
        fn new(rows: Vec<Vec<usize>>) -> Self {
            Self {
                rows,
                source_steps: Cell::new(0),
                fail_at: usize::MAX,
            }
        }

        fn step(&self) -> Result<()> {
            let next = self.source_steps.get() + 1;
            self.source_steps.set(next);
            if next == self.fail_at {
                return Err(super::super::Error::Source(
                    SealedProjectionError::UnknownOrdinal(usize::MAX),
                ));
            }
            Ok(())
        }
    }

    struct Row<'a> {
        graph: &'a Fixture,
        values: std::slice::Iter<'a, usize>,
    }

    impl Cursor for Row<'_> {
        fn next(&mut self) -> Result<Option<(usize, f64)>> {
            self.graph.step()?;
            Ok(self.values.next().map(|&target| (target, -17.0)))
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

        fn open(&self, source: usize) -> Result<Row<'_>> {
            self.step()?;
            let values = self
                .rows
                .get(source)
                .ok_or(ExecutionError::InvalidUpstreamResult)?;
            Ok(Row {
                graph: self,
                values: values.iter(),
            })
        }
    }

    fn run(
        graph: &Fixture,
        reverse: Option<&Fixture>,
        iterations: usize,
        limit: usize,
        control: &mut impl FnMut() -> Result<()>,
    ) -> Result<(KernelOutput, usize)> {
        let mut state = Labels::new(graph.node_count(), iterations, limit)?;
        state.prepare(graph, iterations, control)?;
        state.iterate(graph, reverse, iterations, control)
    }

    fn component_labels(output: KernelOutput) -> Vec<usize> {
        let KernelValues::Components(labels) = output.values else {
            panic!("component labels")
        };
        labels
    }

    // Deliberately uses a fresh map per vertex and stable identities, not the
    // production ordinal histogram or its epoch state.
    fn reference(graph: &SnapshotGraphView, iterations: usize) -> Vec<VId> {
        let mut labels = graph.vertex_ids().to_vec();
        for _ in 0..iterations {
            let mut next = labels.clone();
            for (vertex, label) in next.iter_mut().enumerate() {
                let mut votes = std::collections::BTreeMap::<VId, usize>::new();
                for row in [
                    graph.neighbors_indices(vertex).unwrap(),
                    if graph.is_directed() {
                        graph.in_neighbors_indices(vertex).unwrap()
                    } else {
                        &[]
                    },
                ] {
                    for &neighbor in row {
                        *votes.entry(labels[neighbor]).or_default() += 1;
                    }
                }
                if let Some((&winner, _)) = votes
                    .iter()
                    .max_by(|(a, x), (b, y)| x.cmp(y).then_with(|| b.cmp(a)))
                {
                    *label = winner;
                }
            }
            labels = next;
        }
        labels
    }

    #[test]
    fn every_small_directed_reversed_and_undirected_topology_matches_decoded_and_identity_oracle() {
        let ids = [VId(0), VId(1 << 96), VId(u128::MAX)];
        let mut cases = 0;
        for n in 0..=3 {
            let pairs: Vec<_> = (0..n)
                .flat_map(|s| (0..n).filter(move |&t| t != s).map(move |t| (s, t)))
                .collect();
            for mask in 0usize..(1 << pairs.len()) {
                let edges: Vec<_> = pairs
                    .iter()
                    .enumerate()
                    .filter(|(bit, _)| mask & (1 << bit) != 0)
                    .map(|(bit, &(s, t))| ProjectionEdge {
                        eid: EId(bit as u128),
                        source: ids[s],
                        target: ids[t],
                        weight: -3.0,
                    })
                    .collect();
                for direction in [
                    Directedness::Directed,
                    Directedness::Reversed,
                    Directedness::Undirected,
                ] {
                    let graph = SnapshotGraphView::build(
                        SnapshotBinding {
                            root: ObjectId([5; 32]),
                            as_of: CommitSeq(7),
                        },
                        &ids[..n],
                        &edges,
                        ProjectionSpec {
                            directedness: direction,
                            parallel_edges: ParallelEdgePolicy::CollapseUnit,
                            self_loops: SelfLoopPolicy::Reject,
                        },
                        ProjectionLimits {
                            max_vertices: n,
                            max_input_edges: edges.len(),
                            max_adjacency_entries: 2 * edges.len(),
                            max_workspace_bytes: 1 << 20,
                        },
                    )
                    .unwrap();
                    let outgoing = Fixture::new(
                        (0..n)
                            .map(|v| graph.neighbors_indices(v).unwrap().to_vec())
                            .collect(),
                    );
                    let incoming = Fixture::new(
                        (0..n)
                            .map(|v| graph.in_neighbors_indices(v).unwrap().to_vec())
                            .collect(),
                    );
                    for iterations in [0, 1, 2, 5] {
                        let mut ticks = 0;
                        let (actual, peak) = run(
                            &outgoing,
                            graph.is_directed().then_some(&incoming),
                            iterations,
                            workspace(n, iterations).unwrap(),
                            &mut || {
                                ticks += 1;
                                Ok(())
                            },
                        )
                        .unwrap();
                        let (expected, _) = crate::traversal::cdlp(&graph, iterations, &mut || {
                            Ok::<(), Infallible>(())
                        })
                        .unwrap();
                        assert_eq!(actual.witness.nodes_touched, expected.witness.nodes_touched);
                        assert_eq!(actual.witness.edges_scanned, expected.witness.edges_scanned);
                        let labels = component_labels(actual);
                        assert_eq!(
                            labels,
                            component_labels(expected),
                            "n={n} mask={mask} direction={direction:?} k={iterations}"
                        );
                        assert_eq!(
                            labels.iter().map(|&v| ids[v]).collect::<Vec<_>>(),
                            reference(&graph, iterations)
                        );
                        assert_eq!(peak, workspace(n, iterations).unwrap());
                        assert!(
                            ticks
                                <= work(
                                    n,
                                    edges.len(),
                                    outgoing.rows.iter().map(Vec::len).sum(),
                                    direction,
                                    iterations,
                                    2,
                                )
                                .unwrap()
                        );
                        cases += 1;
                    }
                }
            }
        }
        assert_eq!(cases, 840);
    }

    #[test]
    fn reciprocal_votes_are_not_collapsed_into_a_neighbor_union() {
        // Vertex 0 sees label 2 twice and label 1 once. An undirected union
        // would choose the smaller label 1, which is the wrong directed answer.
        let graph = Fixture::new(vec![vec![1, 2], vec![], vec![0], vec![]]);
        let reverse = Fixture::new(vec![vec![2], vec![0], vec![0], vec![]]);
        let (output, _) = run(&graph, Some(&reverse), 1, usize::MAX, &mut || Ok(())).unwrap();
        assert_eq!(component_labels(output), vec![2, 0, 0, 3]);
        let (output, _) = run(&graph, Some(&reverse), 2, usize::MAX, &mut || Ok(())).unwrap();
        assert_eq!(component_labels(output), vec![0, 2, 2, 3]);
    }

    #[test]
    fn official_graphalytics_examples_match_published_two_iteration_labels() {
        // Independent published inputs/results, pinned at:
        // https://github.com/ldbc/ldbc_graphalytics/tree/7b8bde76cf7aab5e90b25ecd4b38829e2f98b292
        // graphalytics-validation/src/main/resources/validation-graphs/example/
        // example-directed.e: 5689a64fb63bfb8fdcf9d32d6fb7b902acecaa13
        // example-directed.v: f00c965d8307308469e537302baa73048488f162
        // example-directed-CDLP: 3b5b10516c9bfc5e6981b4b1a80dedd1ab627a85
        // example-undirected.e: cbe8180fc77ed7629d690e2e019f20f90c5b23b8
        // example-undirected.v: 4fd875adfd59ece89ca3d725c0714e945ce446cc
        // example-undirected-CDLP: e14119f8a81a13ad163ca9143567bfce454727c0
        // config-template/graphs/example-{directed,undirected}.properties
        // set cdlp.max-iterations=2 (blobs ececa9b7711fe426ce88b116752e1ef9ab5437f0
        // and 67d41b99669e69d587fffc1542eb35dc469307b6). Weights are irrelevant.
        fn check(vertices: &[u128], edges: &[(u128, u128)], expected: &[u128], directed: bool) {
            let mut outgoing = vec![Vec::new(); vertices.len()];
            let mut incoming = vec![Vec::new(); vertices.len()];
            for &(source, target) in edges {
                let source = vertices.binary_search(&source).unwrap();
                let target = vertices.binary_search(&target).unwrap();
                outgoing[source].push(target);
                incoming[target].push(source);
                if !directed {
                    outgoing[target].push(source);
                }
            }
            for rows in [&mut outgoing, &mut incoming] {
                for row in rows {
                    row.sort_unstable();
                }
            }
            let outgoing = Fixture::new(outgoing);
            let incoming = Fixture::new(incoming);
            let (actual, _) = run(
                &outgoing,
                directed.then_some(&incoming),
                2,
                workspace(vertices.len(), 2).unwrap(),
                &mut || Ok(()),
            )
            .unwrap();
            assert_eq!(
                component_labels(actual)
                    .into_iter()
                    .map(|ordinal| vertices[ordinal])
                    .collect::<Vec<_>>(),
                expected
            );
        }
        check(
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
            &[
                (1, 3),
                (1, 5),
                (2, 4),
                (2, 5),
                (2, 10),
                (3, 1),
                (3, 5),
                (3, 8),
                (3, 10),
                (5, 3),
                (5, 4),
                (5, 8),
                (6, 3),
                (6, 4),
                (7, 4),
                (8, 1),
                (9, 4),
            ],
            &[1, 2, 3, 4, 1, 1, 2, 3, 2, 1],
            true,
        );
        check(
            &[2, 3, 4, 5, 6, 7, 8, 9, 10],
            &[
                (2, 3),
                (2, 4),
                (3, 4),
                (3, 5),
                (3, 8),
                (5, 6),
                (5, 8),
                (6, 7),
                (6, 8),
                (6, 9),
                (6, 10),
                (7, 9),
            ],
            &[2, 3, 2, 2, 6, 5, 2, 5, 5],
            false,
        );
    }

    #[test]
    fn every_scalar_checkpoint_and_both_source_faces_can_refuse_without_a_result() {
        let rows = vec![vec![1, 2], vec![2], vec![0], vec![]];
        let reverse = vec![vec![2], vec![0], vec![0, 1], vec![]];
        let outgoing = Fixture::new(rows.clone());
        let incoming = Fixture::new(reverse.clone());
        let mut ticks = 0;
        let (expected, _) = run(&outgoing, Some(&incoming), 3, usize::MAX, &mut || {
            ticks += 1;
            Ok(())
        })
        .unwrap();
        let expected = component_labels(expected);
        let source_steps = [outgoing.source_steps.get(), incoming.source_steps.get()];
        assert!(ticks > 20);
        assert!(source_steps.iter().all(|&steps| steps > 10));
        for stop in 1..=ticks {
            let mut seen = 0;
            assert!(
                run(&outgoing, Some(&incoming), 3, usize::MAX, &mut || {
                    seen += 1;
                    if seen == stop {
                        Err(super::super::Error::Source(
                            SealedProjectionError::UnknownOrdinal(usize::MAX),
                        ))
                    } else {
                        Ok(())
                    }
                })
                .is_err()
            );
            assert_eq!(seen, stop);
        }
        for (face, steps) in source_steps.into_iter().enumerate() {
            for stop in 1..=steps {
                let mut outgoing = Fixture::new(rows.clone());
                let mut incoming = Fixture::new(reverse.clone());
                if face == 0 {
                    outgoing.fail_at = stop;
                } else {
                    incoming.fail_at = stop;
                }
                assert!(run(&outgoing, Some(&incoming), 3, usize::MAX, &mut || Ok(())).is_err());
                let stopped = if face == 0 { &outgoing } else { &incoming };
                assert_eq!(stopped.source_steps.get(), stop);
            }
        }
        assert_eq!(
            component_labels(
                run(&outgoing, Some(&incoming), 3, usize::MAX, &mut || Ok(()))
                    .unwrap()
                    .0
            ),
            expected
        );
    }

    #[test]
    fn zero_rounds_loops_exact_workspace_and_checked_arithmetic_are_enforced() {
        let graph = Fixture::new(vec![vec![1], vec![], vec![]]);
        let reverse = Fixture::new(vec![vec![], vec![0], vec![]]);
        for iterations in [0, 2] {
            let bytes = workspace(3, iterations).unwrap();
            let (output, peak) =
                run(&graph, Some(&reverse), iterations, bytes, &mut || Ok(())).unwrap();
            assert_eq!(peak, bytes);
            if iterations == 0 {
                assert_eq!(component_labels(output), vec![0, 1, 2]);
            }
            assert!(matches!(
                run(
                    &graph,
                    Some(&reverse),
                    iterations,
                    bytes - 1,
                    &mut || Ok(())
                ),
                Err(super::super::Error::Execution(
                    ExecutionError::LimitExceeded {
                        resource: "kernel workspace bytes",
                        ..
                    }
                ))
            ));
            let loops = Fixture::new(vec![vec![0]]);
            assert!(matches!(
                run(&loops, None, iterations, usize::MAX, &mut || Ok(())),
                Err(super::super::Error::Execution(
                    ExecutionError::SelfLoopRefused
                ))
            ));
        }
        let empty = Fixture::new(vec![]);
        let (output, peak) = run(&empty, None, usize::MAX, 0, &mut || Ok(())).unwrap();
        assert!(component_labels(output).is_empty());
        assert_eq!(peak, 0);
        let invalid = Fixture::new(vec![vec![2], vec![]]);
        assert!(matches!(
            run(&invalid, None, 1, usize::MAX, &mut || Ok(())),
            Err(super::super::Error::Execution(
                ExecutionError::InvalidUpstreamResult
            ))
        ));
        assert!(workspace(usize::MAX, 1).is_err());
        assert!(work(2, 2, 2, Directedness::Directed, usize::MAX, 2).is_err());
        assert!(work(2, usize::MAX, 2, Directedness::Directed, 1, 2).is_err());
        let mut state = Labels::new(3, 1, usize::MAX).unwrap();
        state.prepare(&graph, 1, &mut || Ok(())).unwrap();
        state.generation = usize::MAX;
        assert!(matches!(
            state.iterate(&graph, Some(&reverse), 1, &mut || Ok(())),
            Err(super::super::Error::Execution(ExecutionError::SizeOverflow))
        ));
    }

    #[test]
    fn hub_and_isolates_use_fixed_linear_label_workspace() {
        let leaves = 8191;
        let n = leaves + 2;
        let mut rows = vec![Vec::new(); n];
        let mut reverse = vec![Vec::new(); n];
        rows[0].extend(1..=leaves);
        for neighbors in reverse.iter_mut().take(leaves + 1).skip(1) {
            neighbors.push(0);
        }
        let outgoing = Fixture::new(rows);
        let incoming = Fixture::new(reverse);
        let (output, peak) = run(
            &outgoing,
            Some(&incoming),
            2,
            workspace(n, 2).unwrap(),
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(peak, 4 * n * size_of::<usize>());
        assert_eq!(output.witness.nodes_touched, 2 * n);
        assert_eq!(output.witness.edges_scanned, 4 * leaves);
        let labels = component_labels(output);
        assert_eq!(labels[0], 0);
        assert!(labels[1..=leaves].iter().all(|&label| label == 1));
        assert_eq!(labels[n - 1], n - 1);
    }
}
