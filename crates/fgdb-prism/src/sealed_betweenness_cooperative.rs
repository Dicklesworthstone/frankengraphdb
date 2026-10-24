//! Cooperatively scheduled, source-ordered Brandes over compressed adjacency.
//!
//! The FIFO is also the reverse dependency order. A second row scan finds
//! predecessors one level nearer the source, so neither decoded adjacency nor
//! per-edge predecessor lists are retained. Each target receives contributions
//! in the registered reverse-BFS and source order, with division before
//! multiplication. Only reached vertices are reduced/reset between sources.

use super::betweenness::{prepare, witness, workspace};
use super::{
    ComplexityWitness, ExecutionError, FnxCallSpec, FnxExecutionLimits, FnxMemoryLimits, FnxResult,
    KernelOutput, KernelValues, Result, ResultAdmission, SealedGraphView, add, admit, checkpoint,
    finish_encoded, mul, reserve,
};
#[cfg(test)]
use super::{Cursor, Rows, betweenness::work};
use std::future::Future;
use std::mem::size_of;

fn reserved<T>(
    n: usize,
    live: &mut usize,
    limit: usize,
    control: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<T>> {
    control()?;
    admit(
        "kernel workspace bytes",
        add(*live, mul(n, size_of::<T>())?)?,
        limit,
    )?;
    let values = reserve(n)?;
    let actual = add(*live, mul(values.capacity(), size_of::<T>())?)?;
    admit("kernel workspace bytes", actual, limit)?;
    *live = actual;
    Ok(values)
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Initialize(usize),
    Start(usize),
    Breadth { source: usize, head: usize },
    Dependencies { source: usize, remaining: usize },
    Reduce { source: usize, at: usize },
    Normalize(usize),
    Complete,
}

struct Brandes {
    distance: Vec<usize>,
    queue: Vec<usize>,
    sigma: Vec<f64>,
    dependency: Vec<f64>,
    scores: Vec<f64>,
    nodes: usize,
    scale: f64,
    phase: Phase,
    row_previous: Option<usize>,
    row_seen: usize,
    witness: ComplexityWitness,
    peak: usize,
}

impl Brandes {
    fn new(nodes: usize, limit: usize, control: &mut impl FnMut() -> Result<()>) -> Result<Self> {
        admit("kernel workspace bytes", workspace(nodes)?, limit)?;
        let pair_base = nodes.saturating_sub(1);
        let scale = if pair_base < 2 {
            1.0
        } else {
            1.0 / mul(pair_base, pair_base - 1)? as f64
        };
        let mut live = 0;
        let distance = reserved(nodes, &mut live, limit, control)?;
        let queue = reserved(nodes, &mut live, limit, control)?;
        let sigma = reserved(nodes, &mut live, limit, control)?;
        let dependency = reserved(nodes, &mut live, limit, control)?;
        let scores = reserved(nodes, &mut live, limit, control)?;
        Ok(Self {
            distance,
            queue,
            sigma,
            dependency,
            scores,
            nodes,
            scale,
            phase: Phase::Initialize(0),
            row_previous: None,
            row_seen: 0,
            witness: witness(),
            peak: live,
        })
    }

    fn complete(&self) -> bool {
        matches!(self.phase, Phase::Complete)
    }

    fn row(&self) -> Option<usize> {
        match self.phase {
            Phase::Breadth { head, .. } => self.queue.get(head).copied(),
            Phase::Dependencies { remaining, .. } if remaining != 0 => {
                let vertex = self.queue[remaining - 1];
                // Level one can contribute only to the excluded source.
                // Its own accumulated dependency is still reduced below.
                (self.distance[vertex] > 1).then_some(vertex)
            }
            _ => None,
        }
    }

    fn visit(&mut self, neighbor: usize) -> Result<()> {
        let vertex = self.row().ok_or(ExecutionError::InvalidUpstreamResult)?;
        if neighbor >= self.nodes || self.row_previous.is_some_and(|last| last >= neighbor) {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        if neighbor == vertex {
            return Err(ExecutionError::SelfLoopRefused.into());
        }
        self.row_previous = Some(neighbor);
        self.row_seen = add(self.row_seen, 1)?;
        self.witness.edges_scanned = add(self.witness.edges_scanned, 1)?;
        match self.phase {
            Phase::Breadth { head, .. } => {
                let next = add(self.distance[vertex], 1)?;
                if self.distance[neighbor] == usize::MAX {
                    self.distance[neighbor] = next;
                    if self.queue.len() == self.nodes {
                        return Err(ExecutionError::InvalidUpstreamResult.into());
                    }
                    self.queue.push(neighbor);
                    self.witness.queue_peak =
                        self.witness.queue_peak.max(self.queue.len() - head - 1);
                }
                if self.distance[neighbor] == next {
                    self.sigma[neighbor] += self.sigma[vertex];
                    // Overflow only on the final BFS layer can leave every
                    // dependency finite while silently discarding paths.
                    if !self.sigma[neighbor].is_finite() {
                        return Err(ExecutionError::InvalidNumericResult.into());
                    }
                }
            }
            Phase::Dependencies { .. } => {
                if self.distance[neighbor] == self.distance[vertex] - 1 && self.sigma[vertex] > 0.0
                {
                    // A precomputed reciprocal changes the registered bits.
                    self.dependency[neighbor] += (self.sigma[neighbor] / self.sigma[vertex])
                        * (1.0 + self.dependency[vertex]);
                    if !self.dependency[neighbor].is_finite() {
                        return Err(ExecutionError::InvalidNumericResult.into());
                    }
                }
            }
            _ => return Err(ExecutionError::InvalidUpstreamResult.into()),
        }
        Ok(())
    }

    fn end_row(&mut self, degree: Option<usize>) -> Result<()> {
        if degree != Some(self.row_seen) {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        self.row_previous = None;
        self.row_seen = 0;
        self.phase = match self.phase {
            Phase::Breadth { source, head } if head < self.queue.len() => {
                self.witness.nodes_touched = add(self.witness.nodes_touched, 1)?;
                Phase::Breadth {
                    source,
                    head: head + 1,
                }
            }
            Phase::Dependencies { source, remaining } if remaining != 0 => Phase::Dependencies {
                source,
                remaining: remaining - 1,
            },
            _ => return Err(ExecutionError::InvalidUpstreamResult.into()),
        };
        Ok(())
    }

    // Every scalar step is constant work. Both drivers share this state and
    // account scalar work alongside raw compressed-history traversal fuel.
    fn scalar_step(&mut self) -> Result<()> {
        self.phase = match self.phase {
            Phase::Initialize(at) if at < self.nodes => {
                self.distance.push(usize::MAX);
                self.sigma.push(0.0);
                self.dependency.push(0.0);
                self.scores.push(0.0);
                Phase::Initialize(at + 1)
            }
            Phase::Initialize(_) if self.nodes == 0 => Phase::Complete,
            Phase::Initialize(_) => Phase::Start(0),
            Phase::Start(source) => {
                self.queue.clear();
                self.queue.push(source);
                self.distance[source] = 0;
                self.sigma[source] = 1.0;
                self.witness.queue_peak = self.witness.queue_peak.max(1);
                Phase::Breadth { source, head: 0 }
            }
            Phase::Breadth { source, head } if head == self.queue.len() => Phase::Dependencies {
                source,
                remaining: self.queue.len(),
            },
            Phase::Dependencies {
                source,
                remaining: 0,
            } => Phase::Reduce { source, at: 0 },
            Phase::Dependencies { source, remaining }
                if self.distance[self.queue[remaining - 1]] <= 1 =>
            {
                Phase::Dependencies {
                    source,
                    remaining: remaining - 1,
                }
            }
            Phase::Reduce { source, at } if at < self.queue.len() => {
                let vertex = self.queue[at];
                if vertex != source {
                    self.scores[vertex] += self.dependency[vertex];
                    if !self.scores[vertex].is_finite() {
                        return Err(ExecutionError::InvalidNumericResult.into());
                    }
                }
                self.distance[vertex] = usize::MAX;
                self.sigma[vertex] = 0.0;
                self.dependency[vertex] = 0.0;
                Phase::Reduce { source, at: at + 1 }
            }
            Phase::Reduce { source, .. } if source + 1 < self.nodes => Phase::Start(source + 1),
            Phase::Reduce { .. } => Phase::Normalize(0),
            Phase::Normalize(at) if at < self.nodes => {
                self.scores[at] *= self.scale;
                if !self.scores[at].is_finite() {
                    return Err(ExecutionError::InvalidNumericResult.into());
                }
                Phase::Normalize(at + 1)
            }
            Phase::Normalize(_) | Phase::Complete => Phase::Complete,
            Phase::Breadth { .. } | Phase::Dependencies { .. } => {
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

#[cfg(test)]
fn run(
    graph: &impl Rows,
    limit: usize,
    control: &mut impl FnMut() -> Result<()>,
) -> Result<(KernelOutput, usize)> {
    control()?;
    let mut state = Brandes::new(graph.node_count(), limit, control)?;
    while !state.complete() {
        control()?;
        if let Some(vertex) = state.row() {
            let mut row = graph.open(vertex)?;
            while let Some((neighbor, _)) = row.next()? {
                control()?;
                state.visit(neighbor)?;
            }
            state.end_row(graph.degree(vertex))?;
        } else {
            state.scalar_step()?;
        }
    }
    control()?;
    state.finish()
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
    let mut state = Brandes::new(n, memory.max_kernel_workspace_bytes, &mut || {
        checkpoint(control.cx)
    })?;
    while !state.complete() {
        control.tick().await?;
        if let Some(vertex) = state.row() {
            let mut row = graph.neighbor_cursor_controlled(control.cx, vertex, None)?;
            while let Some((neighbor, _)) = control.next(&mut row).await? {
                control.tick().await?;
                state.visit(neighbor)?;
            }
            state.end_row(graph.degree(vertex))?;
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
        "fgdb-prism/sealed-brandes-source-ordered-cooperative-v1",
        estimated,
        peak,
        &mut || checkpoint(control.cx),
    )
}

#[cfg(test)]
#[path = "sealed_betweenness_cooperative_tests.rs"]
mod tests;
