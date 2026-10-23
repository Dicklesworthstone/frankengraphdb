//! Exact closeness and harmonic centrality over compressed adjacency.
//!
//! Reuse one FIFO and one distance vector for successive source traversals.
//! Only reached vertices are reset between sources, so isolated populations
//! do not incur a quadratic clear. Scores retain the registered FNX arithmetic:
//! Wasserman-Faust reachable-set correction for closeness, and one reciprocal
//! addition per reached vertex in ascending distance for harmonic centrality.
//! Neither driver retains adjacency or an all-pairs distance matrix.

use super::{
    ComplexityWitness, Cursor, ExecutionError, FnxAlgorithm, FnxCallSpec, FnxExecutionLimits,
    FnxMemoryLimits, FnxResult, FoundationAlgorithm, KernelOutput, KernelValues, Result,
    ResultAdmission, Rows, SealedGraphView, SealedRows, add, admit, checkpoint,
    directional_pass_work, finish, finish_encoded, mul, require_loop_free_degree, reserve,
};
use crate::sealed_control::Control;
use std::future::Future;
use std::mem::size_of;

fn workspace(n: usize) -> Result<usize> {
    mul(n, 2 * size_of::<usize>() + size_of::<f64>())
}

fn work(n: usize, arcs: usize, pass: usize, columns: usize) -> Result<usize> {
    // Each source can open every row once. `pass` prices BOTH compressed
    // faces and retained/excluded history, independent of the reduced arcs.
    // Also include every reduced-arc visit, queue/score/reset scalar step,
    // initialization, output fields and fixed phase transitions.
    let source = add(pass, add(arcs, add(mul(n, 4)?, 8)?)?)?;
    add(
        mul(n, source)?,
        add(mul(n, add(columns, 3)?)?, add(columns, 16)?)?,
    )
}

fn reserved<T>(n: usize, live: &mut usize, limit: usize) -> Result<Vec<T>> {
    admit(
        "kernel workspace bytes",
        add(*live, mul(n, size_of::<T>())?)?,
        limit,
    )?;
    let values = reserve(n)?;
    let actual = add(*live, mul(values.capacity(), size_of::<T>())?)?;
    // try_reserve_exact may return a larger backing store. Admit that actual
    // capacity before initializing it or making another allocation.
    admit("kernel workspace bytes", actual, limit)?;
    *live = actual;
    Ok(values)
}

#[derive(Clone, Copy)]
enum Phase {
    Initialize(usize),
    Start(usize),
    Traverse { source: usize, head: usize },
    Reset { next_source: usize, at: usize },
    Complete,
}

struct Traversals {
    distances: Vec<usize>,
    queue: Vec<usize>,
    scores: Vec<f64>,
    nodes: usize,
    harmonic: bool,
    sum_distances: usize,
    reciprocal_sum: f64,
    phase: Phase,
    witness: ComplexityWitness,
    peak: usize,
}

impl Traversals {
    fn new(nodes: usize, harmonic: bool, limit: usize) -> Result<Self> {
        admit("kernel workspace bytes", workspace(nodes)?, limit)?;
        let mut live = 0;
        let distances = reserved(nodes, &mut live, limit)?;
        let queue = reserved(nodes, &mut live, limit)?;
        let scores = reserved(nodes, &mut live, limit)?;
        Ok(Self {
            distances,
            queue,
            scores,
            nodes,
            harmonic,
            sum_distances: 0,
            reciprocal_sum: 0.0,
            phase: Phase::Initialize(0),
            witness: ComplexityWitness {
                algorithm: if harmonic {
                    "harmonic_centrality_fifo_cursor"
                } else {
                    "closeness_centrality_fifo_cursor"
                }
                .to_owned(),
                complexity_claim: "O(|V| * (|V| + |E|)) scalar work plus O(|V| * (|V| log(1+H) + H log(1+|V|))) compressed row visits; O(|V|) workspace".to_owned(),
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
            Phase::Traverse { head, .. } => self.queue.get(head).copied(),
            _ => None,
        }
    }

    fn visit(&mut self, neighbor: usize) -> Result<()> {
        let Phase::Traverse { head, .. } = self.phase else {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        };
        let vertex = self
            .queue
            .get(head)
            .copied()
            .ok_or(ExecutionError::InvalidUpstreamResult)?;
        if neighbor >= self.nodes {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        if neighbor == vertex {
            return Err(ExecutionError::SelfLoopRefused.into());
        }
        self.witness.edges_scanned = add(self.witness.edges_scanned, 1)?;
        if self.distances[neighbor] == usize::MAX {
            self.distances[neighbor] = add(self.distances[vertex], 1)?;
            self.queue.push(neighbor);
            self.witness.queue_peak = self.witness.queue_peak.max(self.queue.len() - head - 1);
        }
        Ok(())
    }

    fn end_row(&mut self) -> Result<()> {
        let Phase::Traverse { source, head } = self.phase else {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        };
        let vertex = self
            .queue
            .get(head)
            .copied()
            .ok_or(ExecutionError::InvalidUpstreamResult)?;
        let distance = self.distances[vertex];
        if self.harmonic {
            if distance != 0 {
                // Do not replace equal-distance additions with a multiplied
                // count: the registered ascending-distance sum fixes the bits.
                self.reciprocal_sum += 1.0 / distance as f64;
            }
        } else {
            self.sum_distances = add(self.sum_distances, distance)?;
        }
        self.witness.nodes_touched = add(self.witness.nodes_touched, 1)?;
        self.phase = Phase::Traverse {
            source,
            head: head + 1,
        };
        Ok(())
    }

    // At most one initialization, source transition or reached-vertex reset.
    // All graph-sized scalar work therefore participates in the shared fuel.
    fn scalar_step(&mut self) -> Result<()> {
        self.phase = match self.phase {
            Phase::Initialize(at) if at < self.nodes => {
                self.distances.push(usize::MAX);
                Phase::Initialize(at + 1)
            }
            Phase::Initialize(_) if self.nodes == 0 => Phase::Complete,
            Phase::Initialize(_) => Phase::Start(0),
            Phase::Start(source) => {
                self.queue.clear();
                self.queue.push(source);
                self.distances[source] = 0;
                self.sum_distances = 0;
                self.reciprocal_sum = 0.0;
                self.witness.queue_peak = self.witness.queue_peak.max(1);
                Phase::Traverse { source, head: 0 }
            }
            Phase::Traverse { source, head } if head == self.queue.len() => {
                let reached = self.queue.len();
                let score = if self.harmonic {
                    self.reciprocal_sum
                } else if reached <= 1 || self.sum_distances == 0 {
                    0.0
                } else {
                    let reachable_minus_one = (reached - 1) as f64;
                    let mut score = reachable_minus_one / self.sum_distances as f64;
                    if self.nodes > 1 {
                        score *= reachable_minus_one / (self.nodes - 1) as f64;
                    }
                    score
                };
                if !score.is_finite() {
                    return Err(ExecutionError::InvalidNumericResult.into());
                }
                self.scores.push(score);
                if source + 1 == self.nodes {
                    Phase::Complete
                } else {
                    Phase::Reset {
                        next_source: source + 1,
                        at: 0,
                    }
                }
            }
            Phase::Reset { next_source, at } if at < self.queue.len() => {
                self.distances[self.queue[at]] = usize::MAX;
                Phase::Reset {
                    next_source,
                    at: at + 1,
                }
            }
            Phase::Reset { next_source, .. } => Phase::Start(next_source),
            Phase::Complete => Phase::Complete,
            Phase::Traverse { .. } => {
                return Err(ExecutionError::InvalidUpstreamResult.into());
            }
        };
        Ok(())
    }

    fn finish(self) -> Result<(KernelOutput, usize)> {
        if !self.complete() || self.scores.len() != self.nodes {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        Ok((
            KernelOutput {
                values: KernelValues::Scores(self.scores),
                row_count: self.nodes,
                witness: self.witness,
            },
            self.peak,
        ))
    }
}

fn run(
    graph: &impl Rows,
    harmonic: bool,
    limit: usize,
    control: &mut impl FnMut() -> Result<()>,
) -> Result<(KernelOutput, usize)> {
    control()?;
    let mut state = Traversals::new(graph.node_count(), harmonic, limit)?;
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
    state.finish()
}

fn harmonic(call: &FnxCallSpec) -> Result<bool> {
    match call.algorithm() {
        FnxAlgorithm::Foundation(FoundationAlgorithm::ClosenessCentrality) => Ok(false),
        FnxAlgorithm::Foundation(FoundationAlgorithm::HarmonicCentrality) => Ok(true),
        _ => Err(ExecutionError::InvalidUpstreamResult.into()),
    }
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

pub(super) fn execute(
    call: &FnxCallSpec,
    cx: &Control<'_>,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    admission: &ResultAdmission,
) -> Result<FnxResult> {
    let harmonic = harmonic(call)?;
    let estimated = prepare(call, graph, limits, memory, admission)?;
    let rows = SealedRows { cx, graph };
    let mut control = || checkpoint(cx);
    let (output, peak) = run(
        &rows,
        harmonic,
        memory.max_kernel_workspace_bytes,
        &mut control,
    )?;
    finish(
        call,
        graph,
        output,
        if harmonic {
            "fgdb-prism/sealed-harmonic-fifo-v1"
        } else {
            "fgdb-prism/sealed-closeness-fifo-v1"
        },
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
    let harmonic = harmonic(call)?;
    let admission = ResultAdmission::new(call, limits, memory)?;
    let estimated = prepare(call, graph, limits, memory, &admission)?;
    control.tick().await?;
    let n = graph.node_count();
    let mut state = Traversals::new(n, harmonic, memory.max_kernel_workspace_bytes)?;
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
    let (output, peak) = state.finish()?;
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
        if harmonic {
            "fgdb-prism/sealed-harmonic-fifo-cooperative-v1"
        } else {
            "fgdb-prism/sealed-closeness-fifo-cooperative-v1"
        },
        estimated,
        peak,
        &mut || checkpoint(control.cx),
    )
}

#[cfg(test)]
#[path = "sealed_distance_tests.rs"]
mod tests;
