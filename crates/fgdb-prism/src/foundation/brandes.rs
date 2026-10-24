//! Exact, source-ordered Brandes betweenness over the admitted undirected CSR.
//!
//! A BFS queue is also its reverse dependency order. Predecessors need no
//! second adjacency structure: they are the neighbors one level nearer the
//! source. Their order within a row updates distinct accumulators; each
//! accumulator still receives contributions in the pinned reverse BFS order.
//! Parallel sources own disjoint scratch and deltas, then the caller reduces
//! those deltas in source order. Live control stays on the calling thread.
//! Logical workers and result windows depend only on the graph and this
//! versioned policy. Physical CPU availability changes how those workers are
//! grouped, never their retained allocations or the certified workspace.

use crate::execute::{
    FnxMeter, KernelOutput, KernelValues, MAX_GATHER_THREADS, arithmetic_rows, reserve,
};
use crate::{FnxExecutionError, SnapshotGraphView};
use fnx_algorithms::{ComplexityWitness, GraphView};

use super::native::vector_bytes;

const PARALLEL_SOURCES: usize = 500;
const DELTA_WINDOW_BYTES: usize = 64 * 1024 * 1024;
const ARITHMETIC_RUN: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkerError {
    Stopped,
    SizeOverflow,
    InvalidProjection,
}

/// One live admission covers at most 4096 initialization, vertex, incidence
/// or dependency operations. A hub therefore cannot run through a refusal.
struct Live<'a> {
    poll: &'a mut dyn FnMut() -> bool,
    remaining: usize,
}

impl<'a> Live<'a> {
    fn new(poll: &'a mut dyn FnMut() -> bool) -> Self {
        Self { poll, remaining: 0 }
    }

    #[inline]
    fn take(&mut self, available: usize) -> Result<usize, WorkerError> {
        if available == 0 {
            return Ok(0);
        }
        if self.remaining == 0 {
            if !(self.poll)() {
                return Err(WorkerError::Stopped);
            }
            self.remaining = ARITHMETIC_RUN;
        }
        let count = available.min(self.remaining);
        self.remaining -= count;
        Ok(count)
    }

    #[inline]
    fn step(&mut self) -> Result<(), WorkerError> {
        self.take(1).map(|_| ())
    }

    fn finish(&mut self) -> Result<(), WorkerError> {
        if (self.poll)() {
            Ok(())
        } else {
            Err(WorkerError::Stopped)
        }
    }
}

fn fill<T: Copy>(mut slots: &mut [T], value: T, live: &mut Live<'_>) -> Result<(), WorkerError> {
    while !slots.is_empty() {
        let count = live.take(slots.len())?;
        let (admitted, rest) = slots.split_at_mut(count);
        admitted.fill(value);
        slots = rest;
    }
    Ok(())
}

struct Worker {
    max_sources: usize,
    first_source: usize,
    sources: usize,
    sigma: Vec<f64>,
    distance: Vec<usize>,
    queue: Vec<usize>,
    deltas: Vec<f64>,
    nodes_touched: usize,
    edges_scanned: usize,
    queue_peak: usize,
    error: Option<WorkerError>,
}

/// Allocation itself remains synchronous. Poll before it, then initialize
/// only bounded ranges of its reserved backing between live checks.
fn filled<T: Copy, C>(
    length: usize,
    value: T,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<Vec<T>, FnxExecutionError<C>> {
    checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
    let mut values = reserve(length)?;
    while values.len() < length {
        checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
        let count = (length - values.len()).min(ARITHMETIC_RUN);
        values.resize(values.len() + count, value);
    }
    Ok(values)
}

impl Worker {
    fn new<C>(
        n: usize,
        max_sources: usize,
        checkpoint: &mut impl FnxMeter<C>,
    ) -> Result<Self, FnxExecutionError<C>> {
        let delta_len = n
            .checked_mul(max_sources)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        let sigma = filled(n, 0.0, checkpoint)?;
        let distance = filled(n, usize::MAX, checkpoint)?;
        checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
        let queue = reserve(n)?;
        let deltas = filled(delta_len, 0.0, checkpoint)?;
        Ok(Self {
            max_sources,
            first_source: 0,
            sources: 0,
            sigma,
            distance,
            queue,
            deltas,
            nodes_touched: 0,
            edges_scanned: 0,
            queue_peak: 0,
            error: None,
        })
    }

    fn bytes<C>(&self) -> Result<usize, FnxExecutionError<C>> {
        vector_bytes(&self.sigma)?
            .checked_add(vector_bytes(&self.distance)?)
            .and_then(|bytes| {
                bytes.checked_add(self.queue.capacity().checked_mul(size_of::<usize>())?)
            })
            .and_then(|bytes| {
                bytes.checked_add(self.deltas.capacity().checked_mul(size_of::<f64>())?)
            })
            .ok_or(FnxExecutionError::SizeOverflow)
    }

    fn run(
        &mut self,
        graph: &SnapshotGraphView,
        poll: &mut dyn FnMut() -> bool,
    ) -> Result<(), WorkerError> {
        let n = graph.node_count();
        let mut live = Live::new(poll);
        self.nodes_touched = 0;
        self.edges_scanned = 0;
        self.queue_peak = 0;
        for at in 0..self.sources {
            let source = self.first_source + at;
            let delta = &mut self.deltas[at * n..(at + 1) * n];
            fill(&mut self.sigma, 0.0, &mut live)?;
            fill(&mut self.distance, usize::MAX, &mut live)?;
            fill(delta, 0.0, &mut live)?;
            live.step()?;
            self.queue.clear();
            self.sigma[source] = 1.0;
            self.distance[source] = 0;
            self.queue.push(source);
            self.queue_peak = self.queue_peak.max(1);

            let mut head = 0;
            while head < self.queue.len() {
                live.step()?;
                let vertex = self.queue[head];
                head += 1;
                let next_distance = self.distance[vertex] + 1;
                let sigma_vertex = self.sigma[vertex];
                let mut row = graph
                    .neighbors_indices(vertex)
                    .ok_or(WorkerError::InvalidProjection)?;
                while !row.is_empty() {
                    let count = live.take(row.len())?;
                    let (admitted, rest) = row.split_at(count);
                    for &neighbor in admitted {
                        if self.distance[neighbor] == usize::MAX {
                            self.distance[neighbor] = next_distance;
                            self.queue.push(neighbor);
                            self.queue_peak = self.queue_peak.max(self.queue.len() - head);
                            self.sigma[neighbor] += sigma_vertex;
                        } else if self.distance[neighbor] == next_distance {
                            self.sigma[neighbor] += sigma_vertex;
                        }
                    }
                    self.edges_scanned = self
                        .edges_scanned
                        .checked_add(count)
                        .ok_or(WorkerError::SizeOverflow)?;
                    row = rest;
                }
            }
            self.nodes_touched = self
                .nodes_touched
                .checked_add(self.queue.len())
                .ok_or(WorkerError::SizeOverflow)?;

            // FIFO discovery/pop order is precisely the pinned stack order.
            // Retaining that queue avoids a second O(n) stack per worker.
            for &vertex in self.queue.iter().rev() {
                live.step()?;
                if self.distance[vertex] <= 1 {
                    // A level-one vertex can update only the source, whose
                    // dependency is excluded and never propagates further.
                    // Keep the level-one vertex's own dependency untouched.
                    if vertex == source {
                        delta[vertex] = 0.0;
                    }
                    continue;
                }
                let sigma_vertex = self.sigma[vertex];
                let delta_vertex = delta[vertex];
                if sigma_vertex > 0.0 {
                    let previous_distance = self.distance[vertex] - 1;
                    let mut row = graph
                        .neighbors_indices(vertex)
                        .ok_or(WorkerError::InvalidProjection)?;
                    while !row.is_empty() {
                        let count = live.take(row.len())?;
                        let (admitted, rest) = row.split_at(count);
                        for &predecessor in admitted {
                            if self.distance[predecessor] == previous_distance {
                                // Preserve division then multiplication. A
                                // hoisted reciprocal changes canonical bits.
                                delta[predecessor] +=
                                    (self.sigma[predecessor] / sigma_vertex) * (1.0 + delta_vertex);
                            }
                        }
                        row = rest;
                    }
                }
            }
        }
        live.finish()
    }
}

pub(super) fn run<C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    let n = graph.node_count();
    let threads = if n < PARALLEL_SOURCES {
        1
    } else {
        std::thread::available_parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
            .min(MAX_GATHER_THREADS)
    };
    run_with_threads(graph, checkpoint, threads)
}

fn source_window<C>(n: usize) -> Result<usize, FnxExecutionError<C>> {
    let bytes_per_delta = n
        .checked_mul(size_of::<f64>())
        .ok_or(FnxExecutionError::SizeOverflow)?;
    Ok(if n < PARALLEL_SOURCES {
        1
    } else {
        (DELTA_WINDOW_BYTES / bytes_per_delta).max(1).min(n)
    })
}

fn run_with_threads<C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnxMeter<C>,
    threads: usize,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    let window = source_window::<C>(graph.node_count())?;
    run_with_schedule(graph, checkpoint, threads, window)
}

fn run_with_schedule<C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnxMeter<C>,
    threads: usize,
    window: usize,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    let n = graph.node_count();
    let mut scores = filled(n, 0.0, checkpoint)?;
    let mut witness = ComplexityWitness {
        algorithm: "brandes_betweenness_centrality".to_owned(),
        complexity_claim: "O(|V| * |E|)".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 0,
    };
    if n == 0 {
        checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
        return Ok((
            KernelOutput {
                values: KernelValues::Scores(scores),
                row_count: 0,
                witness,
            },
            0,
        ));
    }

    let window = window.clamp(1, n);
    // Retain the same logical layout even when one CPU executes every source.
    // The window contains useful result slots only: small graphs do not pad
    // their deltas to 64 MiB. Physical scheduling never changes this capacity.
    let logical_workers = window.min(MAX_GATHER_THREADS);
    let threads = threads.clamp(1, MAX_GATHER_THREADS).min(logical_workers);
    checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
    let mut workers = reserve(logical_workers)?;
    for worker in 0..logical_workers {
        // Divide the bounded result window exactly: rounding every worker's
        // allotment upward would exceed the advertised aggregate delta bound.
        let sources = window / logical_workers + usize::from(worker < window % logical_workers);
        workers.push(Worker::new(n, sources, checkpoint)?);
    }
    let mut workspace = vector_bytes(&scores)?
        .checked_add(vector_bytes(&workers)?)
        .ok_or(FnxExecutionError::SizeOverflow)?;
    for worker in &workers {
        workspace = workspace
            .checked_add(worker.bytes()?)
            .ok_or(FnxExecutionError::SizeOverflow)?;
    }

    let mut ends = [0; MAX_GATHER_THREADS];
    let mut next_source = 0;
    while next_source < n {
        let mut active = 0;
        for worker in &mut workers {
            worker.first_source = next_source;
            worker.sources = worker.max_sources.min(n - next_source);
            worker.error = None;
            next_source += worker.sources;
            active += usize::from(worker.sources != 0);
        }
        let physical_workers = threads.min(active);
        for (at, end) in ends[..physical_workers].iter_mut().enumerate() {
            *end = active * (at + 1) / physical_workers;
        }
        arithmetic_rows(
            &mut workers[..active],
            &ends[..physical_workers],
            checkpoint,
            |_, workers, poll| {
                for worker in workers {
                    worker.error = worker.run(graph, poll).err();
                    if worker.error.is_some() {
                        break;
                    }
                }
            },
        )?;

        // Both within and across windows, this is exactly source 0..n.
        // Independent targets may vectorize; each target's source additions
        // must retain their order. Poll at most 4096 additions apart.
        let mut remaining = 0;
        for worker in &workers[..active] {
            if let Some(error) = worker.error {
                return Err(match error {
                    WorkerError::SizeOverflow => FnxExecutionError::SizeOverflow,
                    WorkerError::InvalidProjection | WorkerError::Stopped => {
                        FnxExecutionError::InvalidUpstreamResult
                    }
                });
            }
            for delta in worker.deltas[..worker.sources * n].chunks_exact(n) {
                let mut low = 0;
                while low < n {
                    if remaining == 0 {
                        checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
                        remaining = ARITHMETIC_RUN;
                    }
                    let count = remaining.min(n - low);
                    let high = low + count;
                    for (score, contribution) in scores[low..high].iter_mut().zip(&delta[low..high])
                    {
                        *score += *contribution;
                    }
                    remaining -= count;
                    low = high;
                }
            }
            witness.nodes_touched = witness
                .nodes_touched
                .checked_add(worker.nodes_touched)
                .ok_or(FnxExecutionError::SizeOverflow)?;
            witness.edges_scanned = witness
                .edges_scanned
                .checked_add(worker.edges_scanned)
                .ok_or(FnxExecutionError::SizeOverflow)?;
            witness.queue_peak = witness.queue_peak.max(worker.queue_peak);
        }
    }

    let pair_base = n.saturating_sub(1);
    let scale = if pair_base < 2 {
        1.0
    } else {
        let pairs = pair_base
            .checked_mul(pair_base - 1)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        1.0 / pairs as f64
    };
    for slots in scores.chunks_mut(ARITHMETIC_RUN) {
        checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
        for score in slots {
            *score *= scale;
            if !score.is_finite() {
                return Err(FnxExecutionError::InvalidNumericResult);
            }
        }
    }
    checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
    Ok((
        KernelOutput {
            values: KernelValues::Scores(scores),
            row_count: n,
            witness,
        },
        workspace,
    ))
}

#[cfg(test)]
#[path = "brandes_tests.rs"]
mod tests;
