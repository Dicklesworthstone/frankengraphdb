//! Exact eigenvector power iteration over fallible compressed adjacency.
//!
//! On the admitted simple undirected graph, the ascending sources in a row
//! reproduce the pinned FNX `(I + A) * x` scatter addition for addition. Two
//! score vectors suffice; neither a row nor a second graph is materialized.
//! One scalar state machine carries each accumulator across synchronous
//! checkpoints and cooperative yields, including norm and convergence passes.

use super::{
    ComplexityWitness, Cursor, ExecutionError, FnxCallSpec, FnxExecutionLimits, FnxMemoryLimits,
    FnxResult, KernelOutput, KernelValues, Result, ResultAdmission, Rows, SealedGraphView,
    SealedRows, add, admit, checkpoint, directional_pass_work, finish, finish_encoded, mul,
    require_loop_free_degree, reserve,
};
use crate::foundation::{EIGENVECTOR_MAX_ITERATIONS, EIGENVECTOR_TOLERANCE};
use crate::sealed_control::Control;
use std::future::Future;
use std::mem::size_of;

fn workspace(n: usize) -> Result<usize> {
    mul(n, if n <= 1 { 1 } else { 2 } * size_of::<f64>())
}

fn work(n: usize, arcs: usize, pass: usize, columns: usize) -> Result<usize> {
    // Each round includes both compressed faces and all retained/excluded
    // history (`pass`), one contribution per reduced arc, gather/norm/scale/
    // delta scalar passes, and their phase transitions. Preparation, final
    // finite checks and output are separate even for an isolated population.
    let output = add(mul(n, add(columns, 2)?)?, add(columns, 8)?)?;
    let scalar = add(mul(n, 2)?, output)?;
    if n <= 1 {
        return Ok(scalar);
    }
    let iteration = add(pass, add(arcs, add(mul(n, 4)?, 8)?)?)?;
    add(scalar, mul(EIGENVECTOR_MAX_ITERATIONS, iteration)?)
}

fn reserved(n: usize, live: &mut usize, limit: usize) -> Result<Vec<f64>> {
    admit(
        "kernel workspace bytes",
        add(*live, mul(n, size_of::<f64>())?)?,
        limit,
    )?;
    let values = reserve(n)?;
    let actual = add(*live, mul(values.capacity(), size_of::<f64>())?)?;
    // An allocator may exceed try_reserve_exact's requested capacity. Refuse
    // that actual backing before initialization or another allocation.
    admit("kernel workspace bytes", actual, limit)?;
    *live = actual;
    Ok(values)
}

#[derive(Clone, Copy)]
enum Phase {
    Initialize(usize),
    Gather { vertex: usize, sum: f64 },
    Norm { at: usize, sum: f64 },
    Normalize { at: usize, normalizer: f64 },
    Delta { at: usize, sum: f64 },
    Validate(usize),
    Complete,
}

struct Iteration {
    scores: Vec<f64>,
    next_scores: Vec<f64>,
    nodes: usize,
    iterations: usize,
    phase: Phase,
    witness: ComplexityWitness,
    peak: usize,
}

impl Iteration {
    fn new(nodes: usize, limit: usize) -> Result<Self> {
        admit("kernel workspace bytes", workspace(nodes)?, limit)?;
        let mut live = 0;
        let scores = reserved(nodes, &mut live, limit)?;
        let next_scores = if nodes <= 1 {
            Vec::new()
        } else {
            reserved(nodes, &mut live, limit)?
        };
        Ok(Self {
            scores,
            next_scores,
            nodes,
            iterations: 0,
            phase: Phase::Initialize(0),
            witness: ComplexityWitness {
                algorithm: "eigenvector_centrality_power_iteration".to_owned(),
                complexity_claim: "O(k * (|V| + |E|))".to_owned(),
                nodes_touched: 0,
                edges_scanned: 0,
                queue_peak: 0,
            },
            peak: live,
        })
    }

    fn complete(&self) -> bool {
        matches!(self.phase, Phase::Complete)
    }

    fn row(&self) -> Option<usize> {
        match self.phase {
            Phase::Gather { vertex, .. } => Some(vertex),
            _ => None,
        }
    }

    fn visit(&mut self, neighbor: usize) -> Result<()> {
        let Phase::Gather { vertex, sum } = self.phase else {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        };
        if vertex == neighbor {
            return Err(ExecutionError::SelfLoopRefused.into());
        }
        let score = *self
            .scores
            .get(neighbor)
            .ok_or(ExecutionError::InvalidUpstreamResult)?;
        self.witness.edges_scanned = add(self.witness.edges_scanned, 1)?;
        self.phase = Phase::Gather {
            vertex,
            sum: sum + score,
        };
        Ok(())
    }

    fn end_row(&mut self) -> Result<()> {
        let Phase::Gather { vertex, sum } = self.phase else {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        };
        self.next_scores[vertex] = sum;
        self.phase = if vertex + 1 < self.nodes {
            Phase::Gather {
                vertex: vertex + 1,
                sum: self.scores[vertex + 1],
            }
        } else {
            self.iterations = add(self.iterations, 1)?;
            self.witness.nodes_touched = add(self.witness.nodes_touched, self.nodes)?;
            Phase::Norm { at: 0, sum: 0.0 }
        };
        Ok(())
    }

    // At most one vertex operation or fixed-size phase transition per call.
    // In particular no fill, reduction or result validation hides a long pass.
    fn scalar_step(&mut self) -> Result<()> {
        self.phase = match self.phase {
            Phase::Initialize(at) if at < self.nodes => {
                self.scores.push(if self.nodes <= 1 {
                    1.0
                } else {
                    1.0 / self.nodes as f64
                });
                if self.nodes > 1 {
                    self.next_scores.push(0.0);
                }
                Phase::Initialize(at + 1)
            }
            Phase::Initialize(_) if self.nodes <= 1 => {
                // The pinned entry returns empty/singleton answers without
                // an iteration; the singleton score is exactly one.
                self.witness.nodes_touched = self.nodes;
                Phase::Complete
            }
            Phase::Initialize(_) => Phase::Gather {
                vertex: 0,
                sum: self.scores[0],
            },
            Phase::Norm { at, sum } if at < self.nodes => {
                let value = self.next_scores[at];
                Phase::Norm {
                    at: at + 1,
                    sum: sum + value * value,
                }
            }
            Phase::Norm { sum, .. } => {
                let norm = sum.sqrt();
                Phase::Normalize {
                    at: 0,
                    normalizer: if norm > 0.0 { norm } else { 1.0 },
                }
            }
            Phase::Normalize { at, normalizer } if at < self.nodes => {
                self.next_scores[at] /= normalizer;
                Phase::Normalize {
                    at: at + 1,
                    normalizer,
                }
            }
            Phase::Normalize { .. } => Phase::Delta { at: 0, sum: 0.0 },
            Phase::Delta { at, sum } if at < self.nodes => Phase::Delta {
                at: at + 1,
                sum: sum + (self.next_scores[at] - self.scores[at]).abs(),
            },
            Phase::Delta { sum, .. } => {
                std::mem::swap(&mut self.scores, &mut self.next_scores);
                if sum < self.nodes as f64 * EIGENVECTOR_TOLERANCE {
                    Phase::Validate(0)
                } else if self.iterations == EIGENVECTOR_MAX_ITERATIONS {
                    return Err(ExecutionError::NotConverged {
                        max_iterations: EIGENVECTOR_MAX_ITERATIONS,
                        witness: self.witness.clone(),
                    }
                    .into());
                } else {
                    Phase::Gather {
                        vertex: 0,
                        sum: self.scores[0],
                    }
                }
            }
            Phase::Validate(at) if at < self.nodes => {
                if !self.scores[at].is_finite() {
                    return Err(ExecutionError::InvalidNumericResult.into());
                }
                Phase::Validate(at + 1)
            }
            Phase::Validate(_) => Phase::Complete,
            Phase::Gather { .. } => return Err(ExecutionError::InvalidUpstreamResult.into()),
            Phase::Complete => Phase::Complete,
        };
        Ok(())
    }

    fn finish(self) -> (KernelOutput, usize) {
        (
            KernelOutput {
                values: KernelValues::Scores(self.scores),
                row_count: self.nodes,
                witness: self.witness,
            },
            self.peak,
        )
    }
}

fn run(
    graph: &impl Rows,
    limit: usize,
    control: &mut impl FnMut() -> Result<()>,
) -> Result<(KernelOutput, usize)> {
    control()?;
    let mut state = Iteration::new(graph.node_count(), limit)?;
    while !state.complete() {
        control()?;
        if let Some(vertex) = state.row() {
            let mut row = graph.open(vertex)?;
            while let Some((neighbor, _)) = row.next()? {
                control()?;
                state.visit(neighbor)?;
            }
            state.end_row()?;
        } else {
            state.scalar_step()?;
        }
    }
    control()?;
    Ok(state.finish())
}

fn prepare(
    call: &FnxCallSpec,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    admission: &ResultAdmission,
) -> Result<usize> {
    let n = graph.node_count();
    admission.rows(n)?;
    let pass = directional_pass_work(n, graph.scan_incidence_bound(), graph.spec().directedness)?;
    let estimated = work(n, graph.adjacency_entry_count(), pass, call.outputs().len())?;
    admit("estimated work", estimated, limits.max_estimated_work)?;
    admit(
        "kernel workspace bytes",
        workspace(n)?,
        memory.max_kernel_workspace_bytes,
    )?;
    require_loop_free_degree(graph.edge_count(), graph.adjacency_entry_count())?;
    Ok(estimated)
}

fn compressed_witness(output: &mut KernelOutput) {
    output
        .witness
        .complexity_claim
        .push_str(" plus O(k * (|V| log(1+H) + H log(1+|V|))) compressed row visits");
}

pub(super) fn execute(
    call: &FnxCallSpec,
    cx: &Control<'_>,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    admission: &ResultAdmission,
) -> Result<FnxResult> {
    let estimated = prepare(call, graph, limits, memory, admission)?;
    let rows = SealedRows { cx, graph };
    let mut control = || checkpoint(cx);
    let (mut output, peak) = run(&rows, memory.max_kernel_workspace_bytes, &mut control)?;
    compressed_witness(&mut output);
    finish(
        call,
        graph,
        output,
        "fgdb-prism/sealed-eigenvector-gather-v1",
        estimated,
        peak,
        admission,
        &mut control,
    )
}

pub(super) async fn execute_cooperative<Yield, YieldFuture>(
    call: &FnxCallSpec,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    control: &mut super::cooperative::Cooperate<'_, Yield>,
) -> Result<FnxResult>
where
    Yield: FnMut() -> YieldFuture,
    YieldFuture: Future<Output = ()>,
{
    let admission = ResultAdmission::new(call, limits, memory)?;
    let estimated = prepare(call, graph, limits, memory, &admission)?;
    control.tick().await?;
    let n = graph.node_count();
    let mut state = Iteration::new(n, memory.max_kernel_workspace_bytes)?;
    while !state.complete() {
        control.tick().await?;
        if let Some(vertex) = state.row() {
            let mut row = graph.neighbor_cursor_controlled(control.cx, vertex, None)?;
            while let Some((neighbor, _)) = control.next(&mut row).await? {
                control.tick().await?;
                state.visit(neighbor)?;
            }
            state.end_row()?;
        } else {
            state.scalar_step()?;
        }
    }
    control.tick().await?;
    let (mut output, peak) = state.finish();
    compressed_witness(&mut output);
    let mut encoded = super::EncodedRows::new(call, n, &admission)?;
    for vertex in 0..n {
        control.tick().await?;
        encoded.push(call, graph, &output.values, vertex, &mut || {
            checkpoint(control.cx)
        })?;
    }
    control.tick().await?;
    finish_encoded(
        call,
        graph,
        encoded,
        output.witness,
        "fgdb-prism/sealed-eigenvector-gather-cooperative-v1",
        estimated,
        peak,
        &mut || checkpoint(control.cx),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Directedness, FnxExecutionError, FnxParameters, FnxValue, GraphView, ParallelEdgePolicy,
        ProjectionEdge, ProjectionLimits, ProjectionSpec, SealedProjectionError, SelfLoopPolicy,
        SnapshotBinding, SnapshotGraphView,
    };
    use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::convert::Infallible;

    struct Fixture {
        graph: SnapshotGraphView,
        opens: Vec<Cell<usize>>,
        source_steps: Cell<usize>,
        fail_at: Cell<usize>,
    }

    impl Fixture {
        fn new(n: usize, pairs: &[(usize, usize)]) -> Self {
            // Deliberately reverse input identities, with nonzero high bits.
            // Canonical FNX names and dense ordinals must keep the same order.
            let vertices: Vec<_> = (0..n)
                .map(|v| VId(((n - v) as u128) << 96 | (1u128 << 80) | v as u128))
                .collect();
            let edges: Vec<_> = pairs
                .iter()
                .enumerate()
                .map(|(id, &(source, target))| ProjectionEdge {
                    eid: EId((1u128 << 112) | id as u128),
                    source: vertices[source],
                    target: vertices[target],
                    // This registered unweighted procedure ignores signed
                    // finite weights, including those in compressed rows.
                    weight: -7.0,
                })
                .collect();
            let graph = SnapshotGraphView::build(
                SnapshotBinding {
                    root: ObjectId([0x5a; 32]),
                    as_of: CommitSeq(11),
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
            .unwrap();
            Self {
                graph,
                opens: (0..n).map(|_| Cell::new(0)).collect(),
                source_steps: Cell::new(0),
                fail_at: Cell::new(usize::MAX),
            }
        }

        fn step(&self) -> Result<()> {
            let next = self.source_steps.get() + 1;
            self.source_steps.set(next);
            if next == self.fail_at.get() {
                return Err(super::super::Error::Source(
                    SealedProjectionError::UnknownOrdinal(next),
                ));
            }
            Ok(())
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
            self.graph.node_count()
        }

        fn degree(&self, source: usize) -> Option<usize> {
            self.graph.neighbors_indices(source).map(<[_]>::len)
        }

        fn open(&self, source: usize) -> Result<Self::Cursor<'_>> {
            self.step()?;
            self.opens[source].set(self.opens[source].get() + 1);
            Ok(Row {
                fixture: self,
                targets: self.graph.neighbors_indices(source).unwrap().iter(),
            })
        }
    }

    fn limits() -> FnxExecutionLimits {
        FnxExecutionLimits {
            // Fixed-signature eigenvector settings remain part of its
            // registered numeric profile, as in the decoded foundation path.
            max_iterations: 0,
            max_result_rows: 1000,
            max_estimated_work: 1 << 30,
        }
    }

    fn compare(fixture: &Fixture) -> bool {
        let names = fixture.graph.nodes_ordered();
        let mut oracle = fnx_classes::Graph::strict();
        for name in &names {
            oracle.add_node(*name);
        }
        for source in 0..names.len() {
            for &target in fixture.graph.neighbors_indices(source).unwrap() {
                if source < target {
                    oracle.add_edge(names[source], names[target]).unwrap();
                }
            }
        }
        let (expected, converged) = fnx_algorithms::eigenvector_centrality_with_params(
            &oracle,
            EIGENVECTOR_MAX_ITERATIONS,
            EIGENVECTOR_TOLERANCE,
        );
        let call = FnxCallSpec::bind(
            "CALL fnx.eigenvector_centrality() YIELD vertex,score",
            &FnxParameters::new(),
        )
        .unwrap();
        let native = call.execute(&fixture.graph, limits(), || Ok::<(), Infallible>(()));
        let actual = run(fixture, usize::MAX, &mut || Ok(()));
        if converged {
            let (output, peak) = actual.unwrap();
            let native = native.unwrap();
            assert_eq!(output.witness, expected.witness);
            assert_eq!(output.witness, native.certificate.witness);
            let KernelValues::Scores(scores) = output.values else {
                panic!("expected eigenvector scores");
            };
            let by_name: BTreeMap<_, _> = expected
                .scores
                .into_iter()
                .map(|score| (score.node, score.score.to_bits()))
                .collect();
            assert_eq!(scores.len(), names.len());
            for (vertex, (name, score)) in names.iter().zip(&scores).enumerate() {
                assert_eq!(score.to_bits(), by_name[*name]);
                let FnxValue::Score(native_score) = native.rows[vertex][1] else {
                    panic!("decoded score column");
                };
                assert_eq!(score.to_bits(), native_score.to_bits());
                assert_eq!(
                    native.rows[vertex][0],
                    FnxValue::Vertex(fixture.graph.vertex_id(vertex).unwrap())
                );
            }
            assert_eq!(peak, native.certificate.kernel_workspace_bytes);
            assert!(peak >= workspace(names.len()).unwrap());
            let iterations = if names.len() <= 1 {
                0
            } else {
                output.witness.nodes_touched / names.len()
            };
            assert!(fixture.opens.iter().all(|opens| opens.get() == iterations));
        } else {
            for witness in [
                match actual {
                    Err(super::super::Error::Execution(ExecutionError::NotConverged {
                        max_iterations,
                        witness,
                    })) => {
                        assert_eq!(max_iterations, EIGENVECTOR_MAX_ITERATIONS);
                        witness
                    }
                    Err(error) => panic!("unexpected compressed refusal: {error:?}"),
                    Ok(_) => panic!("compressed kernel published unconverged scores"),
                },
                match native {
                    Err(FnxExecutionError::NotConverged {
                        max_iterations,
                        witness,
                    }) => {
                        assert_eq!(max_iterations, EIGENVECTOR_MAX_ITERATIONS);
                        witness
                    }
                    other => panic!("decoded oracle must refuse without scores: {other:?}"),
                },
            ] {
                assert_eq!(witness, expected.witness);
            }
            assert!(
                fixture
                    .opens
                    .iter()
                    .all(|opens| opens.get() == EIGENVECTOR_MAX_ITERATIONS)
            );
        }
        converged
    }

    #[test]
    fn every_simple_graph_through_five_vertices_matches_pinned_fnx_and_native_bits() {
        let mut cases = 0;
        for n in 0..=5 {
            let possible: Vec<_> = (0..n)
                .flat_map(|source| (source + 1..n).map(move |target| (source, target)))
                .collect();
            for mask in 0..(1usize << possible.len()) {
                let pairs: Vec<_> = possible
                    .iter()
                    .enumerate()
                    .filter_map(|(bit, &pair)| (mask & (1 << bit) != 0).then_some(pair))
                    .collect();
                compare(&Fixture::new(n, &pairs));
                cases += 1;
            }
        }
        assert_eq!(cases, 1100);
    }

    #[test]
    fn disconnected_bipartite_and_skewed_profiles_keep_convergence_and_refusal() {
        for n in [7, 17, 65, 129] {
            compare(&Fixture::new(n, &[]));
            let path: Vec<_> = (1..n).map(|v| (v - 1, v)).collect();
            let converged = compare(&Fixture::new(n, &path));
            if n == 129 {
                assert!(
                    !converged,
                    "slow path must exhaust the registered 100 rounds"
                );
            }
            let star: Vec<_> = (1..n).map(|v| (0, v)).collect();
            compare(&Fixture::new(n, &star));
            let bipartite: Vec<_> = (0..3)
                .flat_map(|source| (3..n - 2).map(move |target| (source, target)))
                .collect();
            compare(&Fixture::new(n, &bipartite));
        }
    }

    #[test]
    fn canonical_gather_keeps_adversarial_score_additions_without_reassociation() {
        let n = 73;
        let pairs: Vec<_> = (0..n)
            .flat_map(|source| {
                (source + 1..n)
                    .filter(move |target| (source * 17 + target * 29) % 7 < 3)
                    .map(move |target| (source, target))
            })
            .collect();
        let fixture = Fixture::new(n, &pairs);
        let mut state = Iteration::new(n, usize::MAX).unwrap();
        while state.row().is_none() {
            state.scalar_step().unwrap();
        }
        for (i, score) in state.scores.iter_mut().enumerate() {
            *score = f64::from_bits((((i * 37) % 96 + 975) as u64) << 52 | i as u64);
        }
        let mut scatter = state.scores.clone();
        for (source, &score) in state.scores.iter().enumerate() {
            for &target in fixture.graph.neighbors_indices(source).unwrap() {
                scatter[target] += score;
            }
        }
        state.phase = Phase::Gather {
            vertex: 0,
            sum: state.scores[0],
        };
        while let Some(vertex) = state.row() {
            let mut row = fixture.open(vertex).unwrap();
            while let Some((neighbor, _)) = row.next().unwrap() {
                state.visit(neighbor).unwrap();
            }
            state.end_row().unwrap();
        }
        assert_eq!(
            state
                .next_scores
                .iter()
                .map(|x| x.to_bits())
                .collect::<Vec<_>>(),
            scatter.iter().map(|x| x.to_bits()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn every_scalar_and_source_refusal_discards_the_partial_iteration() {
        let fixture = Fixture::new(3, &[(0, 1), (1, 2)]);
        let mut checkpoints = 0;
        run(&fixture, usize::MAX, &mut || {
            checkpoints += 1;
            Ok(())
        })
        .unwrap();
        let source_steps = fixture.source_steps.get();
        for stop in 1..=checkpoints {
            let mut seen = 0;
            let actual = run(&fixture, usize::MAX, &mut || {
                seen += 1;
                if seen == stop {
                    Err(super::super::Error::Source(
                        SealedProjectionError::UnknownOrdinal(stop),
                    ))
                } else {
                    Ok(())
                }
            });
            assert!(matches!(actual, Err(super::super::Error::Source(
                SealedProjectionError::UnknownOrdinal(at))) if at == stop));
            assert_eq!(seen, stop);
        }
        for stop in 1..=source_steps {
            fixture.source_steps.set(0);
            fixture.fail_at.set(stop);
            let actual = run(&fixture, usize::MAX, &mut || Ok(()));
            assert!(matches!(actual, Err(super::super::Error::Source(
                SealedProjectionError::UnknownOrdinal(at))) if at == stop));
            assert_eq!(fixture.source_steps.get(), stop);
        }
    }

    #[test]
    fn admission_arithmetic_and_loop_laws_fail_closed() {
        assert!(workspace(usize::MAX).is_err());
        assert!(work(2, usize::MAX, 1, 2).is_err());
        assert!(work(2, 1, usize::MAX, 2).is_err());
        assert!(work(2, 1, 1, usize::MAX).is_err());
        for n in [1, 3, 512] {
            let bytes = workspace(n).unwrap();
            assert!(matches!(
                Iteration::new(n, bytes - 1),
                Err(super::super::Error::Execution(
                    ExecutionError::LimitExceeded {
                        resource: "kernel workspace bytes",
                        ..
                    }
                ))
            ));
            let state = Iteration::new(n, bytes).unwrap();
            assert_eq!(state.peak, bytes);
        }
        let fixture = Fixture::new(2, &[(0, 0)]);
        assert!(matches!(
            run(&fixture, usize::MAX, &mut || Ok(())),
            Err(super::super::Error::Execution(
                ExecutionError::SelfLoopRefused
            ))
        ));
    }
}
