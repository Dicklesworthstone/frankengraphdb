//! Exact, source-ordered Brandes betweenness over the admitted CSR.
//!
//! A BFS queue is also its reverse dependency order. Predecessors use the
//! incoming CSR face, shared with the outgoing face for undirected inputs,
//! and are one level nearer the source. Their row order updates distinct accumulators; each
//! accumulator still receives contributions in the pinned reverse BFS order.
//! Parallel sources own disjoint scratch and source-ordered component deltas,
//! then the caller reduces those deltas in source order. Component membership
//! is shared across sources. Only reached vertices are reset: disconnected
//! graphs never pay a whole-population clear or reduction per isolated source.
//! Live control stays on the calling thread.
//! Logical workers and result windows depend only on the graph and this
//! versioned policy. Physical CPU availability changes how those workers are
//! grouped, never their retained allocations or the certified workspace.

use crate::execute::{
    FnxMeter, KernelOutput, KernelValues, MAX_GATHER_THREADS, arithmetic_rows, reserve,
};
use crate::{FnxExecutionError, SnapshotGraphView};
use fnx_algorithms::{ComplexityWitness, GraphView};

use super::native::{Arithmetic, vector_bytes};

const PARALLEL_SOURCES: usize = 500;
const DELTA_WINDOW_BYTES: usize = 64 * 1024 * 1024;
const ARITHMETIC_RUN: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkerError {
    Stopped,
    SizeOverflow,
    InvalidProjection,
    InvalidNumericResult,
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

/// One membership directory, shared by every source and worker. This is an
/// exact decomposition, not a sampled policy choice. A source's nonzero
/// dependencies cannot leave its weak component, so its retained row
/// needs only that component's scalar slots; no target ID is copied per source.
struct Components {
    ordinals: Vec<usize>,
    component_of: Vec<usize>,
    vertices: Vec<usize>,
    offsets: Vec<usize>,
    largest: usize,
}

impl Components {
    fn new<C>(
        graph: &SnapshotGraphView,
        checkpoint: &mut impl FnxMeter<C>,
    ) -> Result<Self, FnxExecutionError<C>> {
        let n = graph.node_count();
        let mut ordinals = filled(n, usize::MAX, checkpoint)?;
        let mut component_of = filled(n, 0usize, checkpoint)?;
        checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
        let mut vertices = reserve(n)?;
        checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
        let mut offsets = reserve(n.checked_add(1).ok_or(FnxExecutionError::SizeOverflow)?)?;
        offsets.push(0);
        let mut largest = 0;
        let mut arithmetic = Arithmetic::new(checkpoint);
        for source in 0..n {
            arithmetic.step()?;
            if ordinals[source] != usize::MAX {
                continue;
            }
            let start = vertices.len();
            let component = offsets.len() - 1;
            ordinals[source] = start;
            component_of[source] = component;
            vertices.push(source);
            let mut head = start;
            while head < vertices.len() {
                arithmetic.step()?;
                let vertex = vertices[head];
                head += 1;
                let outgoing = graph
                    .neighbors_indices(vertex)
                    .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
                let incoming = if graph.is_directed() {
                    let (offsets, sources) = graph.incoming_face();
                    sources
                        .get(offsets[vertex]..offsets[vertex + 1])
                        .ok_or(FnxExecutionError::InvalidUpstreamResult)?
                } else {
                    &[]
                };
                // Directed reachability is not a partition. Union both faces
                // so source windows share one exact weak-component directory.
                for &neighbor in outgoing.iter().chain(incoming) {
                    arithmetic.step()?;
                    let ordinal = ordinals
                        .get_mut(neighbor)
                        .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
                    if *ordinal == usize::MAX {
                        *ordinal = vertices.len();
                        component_of[neighbor] = component;
                        vertices.push(neighbor);
                    }
                }
            }
            largest = largest.max(vertices.len() - start);
            offsets.push(vertices.len());
        }
        arithmetic.finish()?;
        Ok(Self {
            ordinals,
            component_of,
            vertices,
            offsets,
            largest,
        })
    }

    fn connected(&self) -> bool {
        self.offsets.len() == 2
    }

    fn range(&self, source: usize) -> Option<std::ops::Range<usize>> {
        let component = *self.component_of.get(source)?;
        Some(*self.offsets.get(component)?..*self.offsets.get(component + 1)?)
    }

    fn bytes<C>(&self) -> Result<usize, FnxExecutionError<C>> {
        vector_bytes(&self.ordinals)?
            .checked_add(vector_bytes(&self.component_of)?)
            .and_then(|bytes| {
                bytes.checked_add(self.vertices.capacity().checked_mul(size_of::<usize>())?)
            })
            .and_then(|bytes| {
                bytes.checked_add(self.offsets.capacity().checked_mul(size_of::<usize>())?)
            })
            .ok_or(FnxExecutionError::SizeOverflow)
    }
}

impl Worker {
    fn new<C>(
        n: usize,
        max_sources: usize,
        largest_component: usize,
        checkpoint: &mut impl FnxMeter<C>,
    ) -> Result<Self, FnxExecutionError<C>> {
        let delta_limit = largest_component
            .checked_mul(max_sources)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        let sigma = filled(n, 0.0, checkpoint)?;
        let distance = filled(n, usize::MAX, checkpoint)?;
        checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
        let queue = reserve(n)?;
        checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
        // Connected sources use the incumbent's contiguous dense fills.
        // Disconnected sources initialize only their current component rows
        // during real work, not this reusable window's full upper bound.
        let deltas = if largest_component == n {
            filled(delta_limit, 0.0, checkpoint)?
        } else {
            reserve(delta_limit)?
        };
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
        components: &Components,
        poll: &mut dyn FnMut() -> bool,
    ) -> Result<(), WorkerError> {
        // Connected inputs use the original identity-indexed dense rows.
        // Specialization removes a directory lookup from every dependency
        // update on that common path; this does not change arithmetic order.
        if components.connected() {
            self.run_components::<true>(graph, components, poll)
        } else {
            self.run_components::<false>(graph, components, poll)
        }
    }

    fn run_components<const CONNECTED: bool>(
        &mut self,
        graph: &SnapshotGraphView,
        components: &Components,
        poll: &mut dyn FnMut() -> bool,
    ) -> Result<(), WorkerError> {
        let n = graph.node_count();
        if self.sources > self.max_sources
            || self
                .first_source
                .checked_add(self.sources)
                .is_none_or(|end| end > n)
        {
            return Err(WorkerError::InvalidProjection);
        }
        let mut live = Live::new(poll);
        self.nodes_touched = 0;
        self.edges_scanned = 0;
        self.queue_peak = 0;
        if !CONNECTED {
            self.deltas.clear();
        }
        for at in 0..self.sources {
            let source = self.first_source + at;
            let component = if CONNECTED {
                0..n
            } else {
                components
                    .range(source)
                    .ok_or(WorkerError::InvalidProjection)?
            };
            let start = if CONNECTED {
                at.checked_mul(n).ok_or(WorkerError::SizeOverflow)?
            } else {
                self.deltas.len()
            };
            let end = start
                .checked_add(component.len())
                .ok_or(WorkerError::SizeOverflow)?;
            if end > self.deltas.capacity() {
                return Err(WorkerError::InvalidProjection);
            }
            if !CONNECTED {
                while self.deltas.len() < end {
                    let count = live.take(end - self.deltas.len())?;
                    self.deltas.resize(self.deltas.len() + count, 0.0);
                }
            }
            let delta = &mut self.deltas[start..end];
            let delta_index = |vertex| {
                if CONNECTED {
                    vertex
                } else {
                    components.ordinals[vertex] - component.start
                }
            };
            if CONNECTED {
                // The weak component spans the population. Preserve the
                // incumbent's streaming initialization and keep these stores
                // out of the BFS hot loop, even for a partial directed reach.
                fill(&mut self.sigma, 0.0, &mut live)?;
                fill(&mut self.distance, usize::MAX, &mut live)?;
                fill(delta, 0.0, &mut live)?;
            } else {
                // The previous BFS queue is exactly the set whose distances
                // need resetting, including across result windows. Counts are
                // overwritten on discovery; the dependency row was initialized
                // above and cannot retain a prior component's values.
                for &vertex in &self.queue {
                    live.step()?;
                    self.distance[vertex] = usize::MAX;
                }
            }
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
                            if !CONNECTED {
                                self.sigma[neighbor] = 0.0;
                            }
                            self.queue.push(neighbor);
                            self.queue_peak = self.queue_peak.max(self.queue.len() - head);
                        }
                        if self.distance[neighbor] == next_distance {
                            self.sigma[neighbor] += sigma_vertex;
                            // A last-layer overflow can leave every dependency
                            // finite: finite / Inf becomes zero and silently
                            // loses paths. Refuse before using that count.
                            if !self.sigma[neighbor].is_finite() {
                                return Err(WorkerError::InvalidNumericResult);
                            }
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
                        delta[delta_index(vertex)] = 0.0;
                    }
                    continue;
                }
                let sigma_vertex = self.sigma[vertex];
                let delta_vertex = delta[delta_index(vertex)];
                if sigma_vertex > 0.0 {
                    let previous_distance = self.distance[vertex] - 1;
                    let (offsets, sources) = graph.incoming_face();
                    let mut row = sources
                        .get(offsets[vertex]..offsets[vertex + 1])
                        .ok_or(WorkerError::InvalidProjection)?;
                    while !row.is_empty() {
                        let count = live.take(row.len())?;
                        let (admitted, rest) = row.split_at(count);
                        for &predecessor in admitted {
                            if self.distance[predecessor] == previous_distance {
                                // Preserve division then multiplication. A
                                // hoisted reciprocal changes canonical bits.
                                delta[delta_index(predecessor)] +=
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

    let components = Components::new(graph, checkpoint)?;
    let window = window.clamp(1, n);
    // Retain the same logical layout even when one CPU executes every source.
    // Source rows share component target IDs and hold only scalar values.
    // The existing n-wide window bound therefore remains conservative, and
    // small components shrink the reserved backing without changing scheduling.
    let logical_workers = window.min(MAX_GATHER_THREADS);
    let threads = threads.clamp(1, MAX_GATHER_THREADS).min(logical_workers);
    checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
    let mut workers = reserve(logical_workers)?;
    for worker in 0..logical_workers {
        // Divide the bounded result window exactly: rounding every worker's
        // allotment upward would exceed the advertised aggregate delta bound.
        let sources = window / logical_workers + usize::from(worker < window % logical_workers);
        workers.push(Worker::new(n, sources, components.largest, checkpoint)?);
    }
    let component_bytes = components.bytes::<C>()?;
    let mut workspace = vector_bytes(&scores)?
        .checked_add(vector_bytes(&workers)?)
        .and_then(|bytes| bytes.checked_add(component_bytes))
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
                    worker.error = worker.run(graph, &components, poll).err();
                    if worker.error.is_some() {
                        break;
                    }
                }
            },
        )?;

        // Both within and across windows, this is exactly source 0..n.
        // Each target's source additions retain their order. The shared member
        // directory may visit distinct targets in discovery order, but never
        // reorders any target's additions. Connected graphs keep contiguous
        // identity-indexed rows. Poll at most 4096 additions apart.
        let mut remaining = 0;
        for worker in &workers[..active] {
            if let Some(error) = worker.error {
                return Err(match error {
                    WorkerError::SizeOverflow => FnxExecutionError::SizeOverflow,
                    WorkerError::InvalidNumericResult => FnxExecutionError::InvalidNumericResult,
                    WorkerError::InvalidProjection | WorkerError::Stopped => {
                        FnxExecutionError::InvalidUpstreamResult
                    }
                });
            }
            let mut consumed = 0usize;
            for source in worker.first_source..worker.first_source + worker.sources {
                let component = if components.connected() {
                    0..n
                } else {
                    components
                        .range(source)
                        .ok_or(FnxExecutionError::InvalidUpstreamResult)?
                };
                let end = consumed
                    .checked_add(component.len())
                    .ok_or(FnxExecutionError::SizeOverflow)?;
                let delta = worker
                    .deltas
                    .get(consumed..end)
                    .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
                let mut low = 0;
                while low < delta.len() {
                    if remaining == 0 {
                        checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
                        remaining = ARITHMETIC_RUN;
                    }
                    let count = remaining.min(delta.len() - low);
                    let high = low + count;
                    if components.connected() {
                        for (score, contribution) in
                            scores[low..high].iter_mut().zip(&delta[low..high])
                        {
                            *score += *contribution;
                        }
                    } else {
                        let vertices =
                            &components.vertices[component.start + low..component.start + high];
                        for (&vertex, &contribution) in vertices.iter().zip(&delta[low..high]) {
                            scores[vertex] += contribution;
                        }
                    }
                    remaining -= count;
                    low = high;
                }
                consumed = end;
            }
            if !components.connected() && consumed != worker.deltas.len() {
                return Err(FnxExecutionError::InvalidUpstreamResult);
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
