//! Exact closeness and harmonic centrality over compressed adjacency.
//!
//! Reuse one FIFO and one distance vector for successive source traversals.
//! Only reached vertices are reset between sources, so isolated populations
//! do not incur a quadratic clear. Scores retain the registered FNX arithmetic:
//! Wasserman-Faust reachable-set correction for closeness, and one reciprocal
//! addition per reached vertex in ascending distance for harmonic centrality.
//! Neither driver retains adjacency or an all-pairs distance matrix.
//! Connected low-diameter projections may advance 64 sources with each row
//! visit. The ordinary first source earns that decision; no probe scan is
//! added. Both drivers share the word frontier and every scalar transition.

use super::{
    ComplexityWitness, Cursor, Delivery, ExecutionError, Finished, FnxAlgorithm, FnxCallSpec,
    FnxExecutionLimits, FnxMemoryLimits, FoundationAlgorithm, KernelOutput, KernelValues, Result,
    ResultAdmission, Rows, SealedGraphView, SealedRows, add, admit, checkpoint,
    directional_pass_work, finish, finish_encoded, mul, require_loop_free_degree, reserve,
};
use crate::sealed_control::Control;
use std::future::Future;
use std::mem::size_of;

const BATCH_SOURCES: usize = 64;
const BATCH_ECCENTRICITY: usize = 7;

#[derive(Clone, Copy)]
struct BatchPolicy {
    pass: usize,
    arcs: usize,
    columns: usize,
    admitted: usize,
}

impl BatchPolicy {
    fn permits(self, nodes: usize, reached: usize, eccentricity: usize) -> bool {
        if nodes < BATCH_SOURCES || reached != nodes || eccentricity > BATCH_ECCENTRICITY {
            return false;
        }
        // Only an undirected projection earns this policy. Its diameter is at
        // most twice the first source's eccentricity. Include the final empty
        // frontier round, every raw source step, per-incidence scalar work,
        // per-source reached-bit additions, clears/scatters and output. The
        // FIRST FIFO is included; switching cannot spend its budget again.
        let bound = || {
            let levels = eccentricity.checked_mul(2)?.checked_add(1)?;
            let first = self
                .pass
                .checked_add(self.arcs)?
                .checked_add(nodes.checked_mul(4)?)?
                .checked_add(8)?;
            let level = self
                .pass
                .checked_add(self.arcs)?
                .checked_add(nodes.checked_mul(4)?)?
                .checked_add(8)?;
            let batch = levels
                .checked_mul(level)?
                .checked_add(nodes.checked_mul(2 * BATCH_SOURCES)?)?
                .checked_add(nodes.checked_mul(4)?)?
                .checked_add(4 * BATCH_SOURCES + 16)?;
            first
                .checked_add(nodes.checked_mul(self.columns.checked_add(24)?)?)?
                .checked_add(4 * BATCH_SOURCES + 16)?
                .checked_add((nodes - 1).div_ceil(BATCH_SOURCES).checked_mul(batch)?)
        };
        bound().is_some_and(|bound| bound <= self.admitted)
    }
}

fn batch_policy(
    graph: &SealedGraphView,
    admitted: usize,
    columns: usize,
) -> Result<Option<BatchPolicy>> {
    // Reaching every vertex from one source proves no all-pairs diameter
    // bound in a directed graph. Never apply the undirected proof there.
    if graph.spec().directedness != super::Directedness::Undirected {
        return Ok(None);
    }
    Ok(Some(BatchPolicy {
        pass: directional_pass_work(
            graph.node_count(),
            graph.scan_incidence_bound(),
            graph.spec().directedness,
        )?,
        arcs: graph.adjacency_entry_count(),
        columns,
        admitted,
    }))
}

fn score(harmonic: bool, reached: usize, sum: usize, reciprocal: f64, n: usize) -> Result<f64> {
    let score = if harmonic {
        reciprocal
    } else if reached <= 1 || sum == 0 {
        0.0
    } else {
        let reachable_minus_one = (reached - 1) as f64;
        let mut score = reachable_minus_one / sum as f64;
        if n > 1 {
            score *= reachable_minus_one / (n - 1) as f64;
        }
        score
    };
    if !score.is_finite() {
        return Err(ExecutionError::InvalidNumericResult.into());
    }
    Ok(score)
}

fn workspace(n: usize) -> Result<usize> {
    mul(n, 2 * size_of::<usize>() + size_of::<f64>())
}

fn work(n: usize, arcs: usize, pass: usize, columns: usize) -> Result<usize> {
    // The registered graph is simple, undirected and loop-free. A component
    // of c vertices needs at least c-1 edges, so no row can be reached from
    // more than min(V,E+1) sources. Reached-only resets share the same bound.
    // `pass` still prices BOTH compressed faces and all retained/excluded
    // history: reducing the number of visits never makes a visit cheaper.
    let component = n.min(add(arcs / 2, 1)?);
    let traversals = add(pass, add(arcs, mul(n, 4)?)?)?;
    // Initialization/output and every source's start/finish transitions stay
    // linear in the full population, including sources that are isolates.
    add(
        mul(component, traversals)?,
        add(mul(n, add(columns, 11)?)?, add(columns, 16)?)?,
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

fn batch_workspace(n: usize) -> Result<usize> {
    add(
        mul(n, 3 * size_of::<u64>())?,
        mul(BATCH_SOURCES, 2 * size_of::<usize>())?,
    )
}

fn optional_reserved<T>(n: usize, peak: &mut usize, limit: usize) -> Result<Option<Vec<T>>> {
    optional_reserved_with(n, peak, limit, reserve)
}

fn optional_reserved_with<T>(
    n: usize,
    peak: &mut usize,
    limit: usize,
    allocate: impl FnOnce(usize) -> Result<Vec<T>>,
) -> Result<Option<Vec<T>>> {
    if add(*peak, mul(n, size_of::<T>())?)? > limit {
        return Ok(None);
    }
    let values = match allocate(n) {
        Ok(values) => values,
        Err(super::Error::Execution(ExecutionError::AllocationFailed)) => return Ok(None),
        Err(error) => return Err(error),
    };
    let actual = add(*peak, mul(values.capacity(), size_of::<T>())?)?;
    // Excess allocator capacity is a real transient violation, not a reason
    // to return successful scalar fallback. Partial allocations below the
    // allowance remain in the witness even if a later allocation falls back.
    admit("kernel workspace bytes", actual, limit)?;
    *peak = actual;
    Ok(Some(values))
}

#[derive(Clone, Copy)]
enum BatchPhase {
    Initialize(usize),
    Counters(usize),
    Clear(usize),
    Seed(usize),
    Rows(usize),
    Accumulate { at: usize, bits: u64 },
    Commit(usize),
    Finish(usize),
    Complete,
}

struct Batch {
    seen: Vec<u64>,
    frontier: Vec<u64>,
    next: Vec<u64>,
    queue: Vec<usize>,
    next_queue: Vec<usize>,
    reached: Vec<usize>,
    sum_distances: Vec<usize>,
    nodes: usize,
    harmonic: bool,
    source_start: usize,
    sources: usize,
    level: usize,
    reciprocal: f64,
    phase: BatchPhase,
}

impl Batch {
    fn reserve(
        nodes: usize,
        harmonic: bool,
        peak: &mut usize,
        limit: usize,
    ) -> Result<Option<Self>> {
        Self::reserve_with_seen(nodes, harmonic, peak, limit, reserve)
    }

    fn reserve_with_seen(
        nodes: usize,
        harmonic: bool,
        peak: &mut usize,
        limit: usize,
        allocate_seen: impl FnOnce(usize) -> Result<Vec<u64>>,
    ) -> Result<Option<Self>> {
        if add(*peak, batch_workspace(nodes)?)? > limit {
            return Ok(None);
        }
        let Some(seen) = optional_reserved_with(nodes, peak, limit, allocate_seen)? else {
            return Ok(None);
        };
        let Some(frontier) = optional_reserved(nodes, peak, limit)? else {
            return Ok(None);
        };
        let Some(next) = optional_reserved(nodes, peak, limit)? else {
            return Ok(None);
        };
        let Some(reached) = optional_reserved(BATCH_SOURCES, peak, limit)? else {
            return Ok(None);
        };
        let Some(sum_distances) = optional_reserved(BATCH_SOURCES, peak, limit)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            seen,
            frontier,
            next,
            // The two FIFO integer buffers are moved from the first source
            // after this optional reservation succeeds. No row is retained.
            queue: Vec::new(),
            next_queue: Vec::new(),
            reached,
            sum_distances,
            nodes,
            harmonic,
            source_start: 1,
            sources: (nodes - 1).min(BATCH_SOURCES),
            level: 1,
            reciprocal: 0.0,
            phase: BatchPhase::Initialize(0),
        }))
    }

    fn row(&self) -> Option<usize> {
        match self.phase {
            BatchPhase::Rows(at) => self.queue.get(at).copied(),
            _ => None,
        }
    }

    fn visit(&mut self, neighbor: usize, witness: &mut ComplexityWitness) -> Result<()> {
        let vertex = self.row().ok_or(ExecutionError::InvalidUpstreamResult)?;
        if neighbor >= self.nodes {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        if neighbor == vertex {
            return Err(ExecutionError::SelfLoopRefused.into());
        }
        witness.edges_scanned = add(witness.edges_scanned, 1)?;
        let newly_reached = self.frontier[vertex] & !self.seen[neighbor];
        if newly_reached != 0 {
            if self.next[neighbor] == 0 {
                if self.next_queue.len() == self.nodes {
                    return Err(ExecutionError::InvalidUpstreamResult.into());
                }
                self.next_queue.push(neighbor);
                witness.queue_peak = witness.queue_peak.max(self.next_queue.len());
            }
            self.next[neighbor] |= newly_reached;
            self.seen[neighbor] |= newly_reached;
        }
        Ok(())
    }

    fn end_row(&mut self) -> Result<()> {
        let BatchPhase::Rows(at) = self.phase else {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        };
        let vertex = self.row().ok_or(ExecutionError::InvalidUpstreamResult)?;
        self.frontier[vertex] = 0;
        self.phase = BatchPhase::Rows(at + 1);
        Ok(())
    }

    // A scalar transition initializes/clears one vertex, adds one reached
    // source bit, scatters one frontier word, or finalizes one source. Both
    // drivers checkpoint before it, including quantum-one cooperative runs.
    fn scalar_step(
        &mut self,
        scores: &mut Vec<f64>,
        witness: &mut ComplexityWitness,
    ) -> Result<()> {
        self.phase = match self.phase {
            BatchPhase::Initialize(at) if at < self.nodes => {
                self.seen.push(0);
                self.frontier.push(0);
                self.next.push(0);
                if scores.len() < self.nodes {
                    scores.push(0.0);
                }
                BatchPhase::Initialize(at + 1)
            }
            BatchPhase::Initialize(_) => BatchPhase::Counters(0),
            BatchPhase::Counters(at) if at < BATCH_SOURCES => {
                self.reached.push(0);
                self.sum_distances.push(0);
                BatchPhase::Counters(at + 1)
            }
            BatchPhase::Counters(_) => BatchPhase::Seed(0),
            BatchPhase::Clear(at) if at < self.nodes => {
                self.seen[at] = 0;
                self.frontier[at] = 0;
                self.next[at] = 0;
                BatchPhase::Clear(at + 1)
            }
            BatchPhase::Clear(_) => BatchPhase::Seed(0),
            BatchPhase::Seed(lane) if lane < self.sources => {
                let source = self.source_start + lane;
                let bit = 1_u64 << lane;
                self.seen[source] = bit;
                self.frontier[source] = bit;
                self.queue.push(source);
                self.reached[lane] = 1;
                self.sum_distances[lane] = 0;
                scores[source] = 0.0;
                witness.queue_peak = witness.queue_peak.max(self.queue.len());
                BatchPhase::Seed(lane + 1)
            }
            BatchPhase::Seed(_) => BatchPhase::Rows(0),
            BatchPhase::Rows(at) if at == self.queue.len() => {
                if let Some(&vertex) = self.next_queue.first() {
                    self.reciprocal = if self.harmonic {
                        1.0 / self.level as f64
                    } else {
                        0.0
                    };
                    BatchPhase::Accumulate {
                        at: 0,
                        bits: self.next[vertex],
                    }
                } else {
                    BatchPhase::Finish(0)
                }
            }
            BatchPhase::Accumulate { at, bits } if bits != 0 => {
                let lane = bits.trailing_zeros() as usize;
                if lane >= self.sources {
                    return Err(ExecutionError::InvalidUpstreamResult.into());
                }
                self.reached[lane] = add(self.reached[lane], 1)?;
                if self.harmonic {
                    // Each source sees precisely the FIFO's ascending-level
                    // sequence, including repeated additions at equal depth.
                    scores[self.source_start + lane] += self.reciprocal;
                } else {
                    self.sum_distances[lane] = add(self.sum_distances[lane], self.level)?;
                }
                let remaining = bits & (bits - 1);
                if remaining != 0 {
                    BatchPhase::Accumulate {
                        at,
                        bits: remaining,
                    }
                } else if let Some(&vertex) = self.next_queue.get(at + 1) {
                    BatchPhase::Accumulate {
                        at: at + 1,
                        bits: self.next[vertex],
                    }
                } else {
                    BatchPhase::Commit(0)
                }
            }
            BatchPhase::Commit(at) if at < self.next_queue.len() => {
                let vertex = self.next_queue[at];
                self.frontier[vertex] = self.next[vertex];
                self.next[vertex] = 0;
                BatchPhase::Commit(at + 1)
            }
            BatchPhase::Commit(_) => {
                std::mem::swap(&mut self.queue, &mut self.next_queue);
                self.next_queue.clear();
                self.level = add(self.level, 1)?;
                BatchPhase::Rows(0)
            }
            BatchPhase::Finish(lane) if lane < self.sources => {
                let source = self.source_start + lane;
                scores[source] = score(
                    self.harmonic,
                    self.reached[lane],
                    self.sum_distances[lane],
                    scores[source],
                    self.nodes,
                )?;
                witness.nodes_touched = add(witness.nodes_touched, self.reached[lane])?;
                BatchPhase::Finish(lane + 1)
            }
            BatchPhase::Finish(_) => {
                self.source_start += self.sources;
                if self.source_start == self.nodes {
                    BatchPhase::Complete
                } else {
                    self.sources = (self.nodes - self.source_start).min(BATCH_SOURCES);
                    self.level = 1;
                    self.queue.clear();
                    self.next_queue.clear();
                    BatchPhase::Clear(0)
                }
            }
            BatchPhase::Complete => BatchPhase::Complete,
            BatchPhase::Rows(_) | BatchPhase::Accumulate { .. } => {
                return Err(ExecutionError::InvalidUpstreamResult.into());
            }
        };
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Initialize(usize),
    Start(usize),
    Traverse { source: usize, head: usize },
    Reset { next_source: usize, at: usize },
    ChooseBatch,
    Batch,
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
    limit: usize,
    policy: Option<BatchPolicy>,
    batch: Option<Batch>,
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
                complexity_claim: "O(V + C*(V*(1+log(1+H)) + H*(1+log(1+H)+log(1+V)))); C <= min(V,E+1) bounds component size, E is projected undirected edges and H is retained incidence count; O(V) workspace".to_owned(),
                nodes_touched: 0,
                edges_scanned: 0,
                queue_peak: 0,
            },
            peak: live,
            limit,
            policy: None,
            batch: None,
        })
    }

    fn complete(&self) -> bool {
        matches!(self.phase, Phase::Complete)
    }

    fn row(&self) -> Option<usize> {
        match self.phase {
            Phase::Traverse { head, .. } => self.queue.get(head).copied(),
            Phase::Batch => self.batch.as_ref().and_then(Batch::row),
            _ => None,
        }
    }

    fn visit(&mut self, neighbor: usize) -> Result<()> {
        if let Some(batch) = &mut self.batch {
            return batch.visit(neighbor, &mut self.witness);
        }
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
        if let Some(batch) = &mut self.batch {
            return batch.end_row();
        }
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
                self.scores.push(score(
                    self.harmonic,
                    reached,
                    self.sum_distances,
                    self.reciprocal_sum,
                    self.nodes,
                )?);
                if source + 1 == self.nodes {
                    Phase::Complete
                } else if source == 0 && self.policy.is_some() {
                    Phase::ChooseBatch
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
            Phase::ChooseBatch => {
                let eccentricity = self.queue.last().map(|&v| self.distances[v]).unwrap_or(0);
                if self.policy.is_some_and(|policy| {
                    policy.permits(self.nodes, self.queue.len(), eccentricity)
                }) {
                    if let Some(mut batch) =
                        Batch::reserve(self.nodes, self.harmonic, &mut self.peak, self.limit)?
                    {
                        batch.queue = std::mem::take(&mut self.queue);
                        batch.next_queue = std::mem::take(&mut self.distances);
                        batch.queue.clear();
                        batch.next_queue.clear();
                        self.batch = Some(batch);
                        self.witness.algorithm = if self.harmonic {
                            "harmonic_centrality_batch64_cursor_v1"
                        } else {
                            "closeness_centrality_batch64_cursor_v1"
                        }
                        .to_owned();
                        self.witness.complexity_claim = "O(P + ceil((V-1)/64)*(D*P + 64*V)); P is one bounded compressed pass, D <= 2*eccentricity(source 0)+1, O(V) workspace; nodes_touched counts logical source/vertex pairs and edges_scanned counts physical row incidences".to_owned();
                        Phase::Batch
                    } else {
                        Phase::Reset {
                            next_source: 1,
                            at: 0,
                        }
                    }
                } else {
                    Phase::Reset {
                        next_source: 1,
                        at: 0,
                    }
                }
            }
            Phase::Batch => {
                let batch = self
                    .batch
                    .as_mut()
                    .ok_or(ExecutionError::InvalidUpstreamResult)?;
                batch.scalar_step(&mut self.scores, &mut self.witness)?;
                if matches!(batch.phase, BatchPhase::Complete) {
                    Phase::Complete
                } else {
                    Phase::Batch
                }
            }
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

#[cfg(test)]
fn run(
    graph: &impl Rows,
    harmonic: bool,
    limit: usize,
    control: &mut impl FnMut() -> Result<()>,
) -> Result<(KernelOutput, usize)> {
    run_with_policy(graph, harmonic, limit, None, control)
}

fn run_with_policy(
    graph: &impl Rows,
    harmonic: bool,
    limit: usize,
    policy: Option<BatchPolicy>,
    control: &mut impl FnMut() -> Result<()>,
) -> Result<(KernelOutput, usize)> {
    control()?;
    let mut state = Traversals::new(graph.node_count(), harmonic, limit)?;
    state.policy = policy;
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

fn kernel(output: &KernelOutput, harmonic: bool, cooperative: bool) -> &'static str {
    let batched = output.witness.algorithm == "harmonic_centrality_batch64_cursor_v1"
        || output.witness.algorithm == "closeness_centrality_batch64_cursor_v1";
    match (harmonic, batched, cooperative) {
        (false, false, false) => "fgdb-prism/sealed-closeness-fifo-v1",
        (true, false, false) => "fgdb-prism/sealed-harmonic-fifo-v1",
        (false, true, false) => "fgdb-prism/sealed-closeness-batch64-v1",
        (true, true, false) => "fgdb-prism/sealed-harmonic-batch64-v1",
        (false, false, true) => "fgdb-prism/sealed-closeness-fifo-cooperative-v1",
        (true, false, true) => "fgdb-prism/sealed-harmonic-fifo-cooperative-v1",
        (false, true, true) => "fgdb-prism/sealed-closeness-batch64-cooperative-v1",
        (true, true, true) => "fgdb-prism/sealed-harmonic-batch64-cooperative-v1",
    }
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
) -> Result<Finished> {
    let harmonic = harmonic(call)?;
    let estimated = prepare(call, graph, limits, memory, admission)?;
    let rows = SealedRows { cx, graph };
    let mut control = || checkpoint(cx);
    let (output, peak) = run_with_policy(
        &rows,
        harmonic,
        memory.max_kernel_workspace_bytes,
        batch_policy(graph, estimated, call.outputs().len())?,
        &mut control,
    )?;
    let kernel = kernel(&output, harmonic, false);
    finish(
        call,
        graph,
        output,
        kernel,
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
    delivery: Delivery,
    control: &mut super::cooperative::Cooperate<'_, Yield>,
) -> Result<Finished>
where
    Yield: FnMut() -> YieldFuture,
    YieldFuture: Future<Output = ()>,
{
    let harmonic = harmonic(call)?;
    let admission = ResultAdmission::for_delivery(call, limits, memory, delivery)?;
    let estimated = prepare(call, graph, limits, memory, &admission)?;
    control.tick().await?;
    let n = graph.node_count();
    let mut state = Traversals::new(n, harmonic, memory.max_kernel_workspace_bytes)?;
    state.policy = batch_policy(graph, estimated, call.outputs().len())?;
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
    let kernel = kernel(&output, harmonic, true);
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
        output.values,
        output.witness,
        kernel,
        estimated,
        peak,
        &mut || checkpoint(control.cx),
    )
}

#[cfg(test)]
#[path = "sealed_distance_tests.rs"]
mod tests;
