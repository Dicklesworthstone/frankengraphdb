//! Exact distance centralities over the admitted undirected CSR.
//!
//! The pinned foundation's multi-source BFS advances 64--512 sources in each
//! word-parallel traversal. Keep that advantage: low-diameter graphs use the
//! same bit operations and high-diameter or disconnected graphs use independent
//! FIFO source traversals. A fixed eight-worker policy selects source batches;
//! fewer physical workers change scheduling, never the witness or score bits.
//! Harmonic sums replay one addition per reached vertex in ascending distance.
//! Worker replication targets 64 MiB of aggregate scratch, with one mandatory
//! worker as the minimum. Partitioned results and final output are separate.
//! Retained logical workers follow the graph and versioned policy even on one
//! CPU; physical threads only group that fixed layout. Workspace evidence must
//! not change when a certificate is replayed under different CPU availability.

use crate::execute::{
    FnxMeter, KernelOutput, KernelValues, MAX_GATHER_THREADS, arithmetic_rows, reserve,
};
use crate::{FnxExecutionError, SnapshotGraphView};
use fnx_algorithms::{ComplexityWitness, GraphView};

use super::native::{Arithmetic, vector_bytes};

const PARALLEL_SOURCES: usize = 500;
const POLICY_THREADS: usize = 8;
const ARITHMETIC_RUN: usize = 4096;
const WORKER_SCRATCH_TARGET: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkerError {
    Stopped,
    SizeOverflow,
    InvalidProjection,
}

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

fn filled<T: Copy, C>(
    length: usize,
    value: T,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<Vec<T>, FnxExecutionError<C>> {
    checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
    let mut values = reserve(length)?;
    while values.len() < length {
        checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
        values.resize(
            values.len() + (length - values.len()).min(ARITHMETIC_RUN),
            value,
        );
    }
    Ok(values)
}

fn closeness_score(reached: usize, sum_dist: usize, n: usize) -> f64 {
    if reached <= 1 || sum_dist == 0 {
        return 0.0;
    }
    let reachable_minus_one = (reached - 1) as f64;
    let mut score = reachable_minus_one / sum_dist as f64;
    if n > 1 {
        score *= reachable_minus_one / (n - 1) as f64;
    }
    score
}

fn chunk_width(n: usize) -> usize {
    (1..=8)
        .rev()
        .find(|&width| n.div_ceil(64 * width) >= POLICY_THREADS)
        .unwrap_or(1)
}

/// Versioned, deterministic copy of the pinned foundation's expected-loss
/// gate. Its policy parallelism is fixed rather than read from a Rayon pool.
/// Return zero for the independent-source path and the probe allocation peak.
fn lane_width<C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(usize, usize), FnxExecutionError<C>> {
    let n = graph.node_count();
    if n < PARALLEL_SOURCES {
        return Ok((n.div_ceil(64).clamp(1, 8), 0));
    }
    if n > u32::MAX as usize {
        return Ok((0, 0));
    }
    let width = chunk_width(n);
    let lanes = 64 * width;
    let chunks = n.div_ceil(lanes);
    if chunks * 4 < POLICY_THREADS {
        return Ok((0, 0));
    }
    let max_eccentricity = (lanes - 4) / 8;
    let mut distance = filled(n, usize::MAX, checkpoint)?;
    checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
    let mut queue = reserve(n)?;
    let workspace = vector_bytes(&distance)?
        .checked_add(vector_bytes(&queue)?)
        .ok_or(FnxExecutionError::SizeOverflow)?;
    let mut arithmetic = Arithmetic::new(checkpoint);
    arithmetic.step()?;
    distance[0] = 0;
    queue.push(0);
    let mut head = 0;
    let mut eccentricity = 0;
    while head < queue.len() {
        arithmetic.step()?;
        let source = queue[head];
        head += 1;
        let next_distance = distance[source] + 1;
        for &target in graph
            .neighbors_indices(source)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?
        {
            arithmetic.step()?;
            if distance[target] == usize::MAX {
                // The pinned gate rejects any larger eccentricity. Its
                // decision needs no scan of the rest of this source's row.
                if next_distance > max_eccentricity {
                    arithmetic.finish()?;
                    return Ok((0, workspace));
                }
                distance[target] = next_distance;
                eccentricity = next_distance;
                queue.push(target);
            }
        }
    }
    arithmetic.finish()?;
    if queue.len() != n {
        return Ok((0, workspace));
    }
    let levels_upper_bound = 2 * eccentricity + 1;
    if levels_upper_bound * 4 > lanes {
        return Ok((0, workspace));
    }
    let work_gain = lanes as f64 / levels_upper_bound.min(lanes) as f64;
    let utilisation = chunks.min(POLICY_THREADS) as f64 / POLICY_THREADS as f64;
    Ok((
        if work_gain * utilisation >= 1.25 {
            width
        } else {
            0
        },
        workspace,
    ))
}

/// `W == 0` is the scalar FIFO path; other instantiations use W u64 lanes.
/// Every worker owns a contiguous, batch-aligned range of result slots.
struct Worker<const W: usize, const HARMONIC: bool> {
    first_source: usize,
    scores: Vec<f64>,
    distance: Vec<usize>,
    seen: Vec<[u64; W]>,
    frontier: Vec<[u64; W]>,
    next: Vec<[u64; W]>,
    queue: Vec<usize>,
    next_queue: Vec<usize>,
    reached: Vec<usize>,
    sum_dist: Vec<usize>,
    nodes_touched: usize,
    edges_scanned: usize,
    queue_peak: usize,
    error: Option<WorkerError>,
}

/// Bound replicated scratch without changing source-batch policy. The target
/// covers requested vector capacities and worker descriptors; it is not an
/// allocator/RSS quota. One worker remains necessary when its minimum alone
/// exceeds the target. Source scores partition n slots across all workers.
fn worker_count<const W: usize, const HARMONIC: bool, C>(
    n: usize,
) -> Result<usize, FnxExecutionError<C>> {
    if n == 0 {
        return Ok(1);
    }
    let word = size_of::<usize>();
    let row_bytes = if W == 0 {
        2usize.checked_mul(word)
    } else {
        3usize
            .checked_mul(size_of::<[u64; W]>())
            .and_then(|bytes| bytes.checked_add(2usize.checked_mul(word)?))
    }
    .ok_or(FnxExecutionError::SizeOverflow)?;
    let width = (W * 64).max(1);
    let batch_bytes = if W == 0 {
        0
    } else {
        width
            .min(n)
            .checked_mul(word)
            .and_then(|bytes| bytes.checked_mul(if HARMONIC { 1 } else { 2 }))
            .ok_or(FnxExecutionError::SizeOverflow)?
    };
    let per_worker = n
        .checked_mul(row_bytes)
        .and_then(|bytes| bytes.checked_add(batch_bytes))
        .and_then(|bytes| bytes.checked_add(size_of::<Worker<W, HARMONIC>>()))
        .ok_or(FnxExecutionError::SizeOverflow)?;
    let policy_workers = if n < PARALLEL_SOURCES {
        1
    } else {
        POLICY_THREADS.min(MAX_GATHER_THREADS)
    };
    Ok(policy_workers
        .min(n.div_ceil(width))
        .min((WORKER_SCRATCH_TARGET / per_worker).max(1)))
}

impl<const W: usize, const HARMONIC: bool> Worker<W, HARMONIC> {
    fn new<C>(
        n: usize,
        first_source: usize,
        sources: usize,
        checkpoint: &mut impl FnxMeter<C>,
    ) -> Result<Self, FnxExecutionError<C>> {
        let mut worker = Self {
            first_source,
            scores: filled(sources, 0.0, checkpoint)?,
            distance: Vec::new(),
            seen: Vec::new(),
            frontier: Vec::new(),
            next: Vec::new(),
            queue: Vec::new(),
            next_queue: Vec::new(),
            reached: Vec::new(),
            sum_dist: Vec::new(),
            nodes_touched: 0,
            edges_scanned: 0,
            queue_peak: 0,
            error: None,
        };
        checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
        worker.queue = reserve(n)?;
        if W == 0 {
            worker.distance = filled(n, usize::MAX, checkpoint)?;
        } else {
            worker.seen = filled(n, [0; W], checkpoint)?;
            worker.frontier = filled(n, [0; W], checkpoint)?;
            worker.next = filled(n, [0; W], checkpoint)?;
            checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
            worker.next_queue = reserve(n)?;
            let batch = (W * 64).min(sources);
            worker.reached = filled(batch, 0, checkpoint)?;
            if !HARMONIC {
                worker.sum_dist = filled(batch, 0, checkpoint)?;
            }
        }
        Ok(worker)
    }

    fn bytes<C>(&self) -> Result<usize, FnxExecutionError<C>> {
        let sizes = [
            vector_bytes(&self.scores)?,
            vector_bytes(&self.distance)?,
            vector_bytes(&self.seen)?,
            vector_bytes(&self.frontier)?,
            vector_bytes(&self.next)?,
            vector_bytes(&self.queue)?,
            vector_bytes(&self.next_queue)?,
            vector_bytes(&self.reached)?,
            vector_bytes(&self.sum_dist)?,
        ];
        sizes.into_iter().try_fold(0usize, |sum, bytes| {
            sum.checked_add(bytes)
                .ok_or(FnxExecutionError::SizeOverflow)
        })
    }

    fn run(
        &mut self,
        graph: &SnapshotGraphView,
        poll: &mut dyn FnMut() -> bool,
    ) -> Result<(), WorkerError> {
        let mut live = Live::new(poll);
        if W == 0 {
            self.run_sources(graph, &mut live)?;
        } else {
            for start in (0..self.scores.len()).step_by(W * 64) {
                self.run_batch(graph, start, &mut live)?;
            }
        }
        live.finish()
    }

    fn run_sources(
        &mut self,
        graph: &SnapshotGraphView,
        live: &mut Live<'_>,
    ) -> Result<(), WorkerError> {
        let n = graph.node_count();
        for at in 0..self.scores.len() {
            fill(&mut self.distance, usize::MAX, live)?;
            live.step()?;
            self.queue.clear();
            let source = self.first_source + at;
            self.queue.push(source);
            self.distance[source] = 0;
            self.queue_peak = self.queue_peak.max(1);
            let mut head = 0;
            let mut sum_dist = 0usize;
            let mut harmonic = 0.0;
            while head < self.queue.len() {
                live.step()?;
                let vertex = self.queue[head];
                head += 1;
                let distance = self.distance[vertex];
                if HARMONIC {
                    if distance > 0 {
                        harmonic += 1.0 / distance as f64;
                    }
                } else {
                    sum_dist = sum_dist
                        .checked_add(distance)
                        .ok_or(WorkerError::SizeOverflow)?;
                }
                let next_distance = distance.checked_add(1).ok_or(WorkerError::SizeOverflow)?;
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
            self.scores[at] = if HARMONIC {
                harmonic
            } else {
                closeness_score(self.queue.len(), sum_dist, n)
            };
        }
        Ok(())
    }

    fn run_batch(
        &mut self,
        graph: &SnapshotGraphView,
        start: usize,
        live: &mut Live<'_>,
    ) -> Result<(), WorkerError> {
        let n = graph.node_count();
        let count = (self.scores.len() - start).min(W * 64);
        let zero = [0u64; W];
        fill(&mut self.seen, zero, live)?;
        fill(&mut self.frontier, zero, live)?;
        self.queue.clear();
        for j in 0..count {
            live.step()?;
            let source = self.first_source + start + j;
            let bit = 1u64 << (j % 64);
            self.seen[source][j / 64] = bit;
            self.frontier[source][j / 64] = bit;
            self.queue.push(source);
            self.reached[j] = 1;
            if HARMONIC {
                self.scores[start + j] = 0.0;
            } else {
                self.sum_dist[j] = 0;
            }
        }
        self.queue_peak = self.queue_peak.max(count);
        let mut level = 1usize;
        while !self.queue.is_empty() {
            self.next_queue.clear();
            for index in 0..self.queue.len() {
                live.step()?;
                let vertex = self.queue[index];
                let frontier = self.frontier[vertex];
                self.frontier[vertex] = zero;
                if frontier == zero {
                    continue;
                }
                let mut row = graph
                    .neighbors_indices(vertex)
                    .ok_or(WorkerError::InvalidProjection)?;
                while !row.is_empty() {
                    let admitted_count = live.take(row.len())?;
                    let (admitted, rest) = row.split_at(admitted_count);
                    for &neighbor in admitted {
                        let seen = &self.seen[neighbor];
                        let mut newly = zero;
                        let mut any = 0;
                        for lane in 0..W {
                            let bits = frontier[lane] & !seen[lane];
                            newly[lane] = bits;
                            any |= bits;
                        }
                        if any != 0 {
                            let next = &mut self.next[neighbor];
                            let mut was_empty = 0;
                            for lane in 0..W {
                                was_empty |= next[lane];
                                next[lane] |= newly[lane];
                            }
                            if was_empty == 0 {
                                self.next_queue.push(neighbor);
                            }
                            for (seen, bits) in self.seen[neighbor].iter_mut().zip(newly) {
                                *seen |= bits;
                            }
                        }
                    }
                    self.edges_scanned = self
                        .edges_scanned
                        .checked_add(admitted_count)
                        .ok_or(WorkerError::SizeOverflow)?;
                    row = rest;
                }
            }
            let reciprocal = if HARMONIC { 1.0 / level as f64 } else { 0.0 };
            for &vertex in &self.next_queue {
                live.step()?;
                let newly = self.next[vertex];
                for (lane, word) in newly.into_iter().enumerate() {
                    let mut bits = word;
                    while bits != 0 {
                        live.step()?;
                        let j = lane * 64 + bits.trailing_zeros() as usize;
                        self.reached[j] = self.reached[j]
                            .checked_add(1)
                            .ok_or(WorkerError::SizeOverflow)?;
                        if HARMONIC {
                            // Repeating this addition is required. Replacing
                            // c equal addends with c * reciprocal changes bits.
                            self.scores[start + j] += reciprocal;
                        } else {
                            self.sum_dist[j] = self.sum_dist[j]
                                .checked_add(level)
                                .ok_or(WorkerError::SizeOverflow)?;
                        }
                        bits &= bits - 1;
                    }
                }
                self.frontier[vertex] = newly;
                self.next[vertex] = zero;
            }
            self.queue_peak = self.queue_peak.max(self.next_queue.len());
            std::mem::swap(&mut self.queue, &mut self.next_queue);
            level = level.checked_add(1).ok_or(WorkerError::SizeOverflow)?;
        }
        for j in 0..count {
            live.step()?;
            self.nodes_touched = self
                .nodes_touched
                .checked_add(self.reached[j])
                .ok_or(WorkerError::SizeOverflow)?;
            if !HARMONIC {
                self.scores[start + j] = closeness_score(self.reached[j], self.sum_dist[j], n);
            }
        }
        Ok(())
    }
}

fn run_scheduled<const W: usize, const HARMONIC: bool, C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnxMeter<C>,
    threads: usize,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    let n = graph.node_count();
    let logical_workers = worker_count::<W, HARMONIC, C>(n)?;
    let threads = threads.clamp(1, MAX_GATHER_THREADS).min(logical_workers);
    let algorithm = if HARMONIC {
        "harmonic_centrality"
    } else {
        "closeness_centrality"
    };
    let mut witness = ComplexityWitness {
        algorithm: algorithm.to_owned(),
        complexity_claim: "O(|V| * (|V| + |E|))".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 0,
    };
    let mut scores = filled(n, 0.0, checkpoint)?;
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
    let width = (W * 64).max(1);
    let batches = n.div_ceil(width);
    checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
    let mut workers = reserve(logical_workers)?;
    let mut first_batch = 0;
    for worker in 0..logical_workers {
        let count = batches / logical_workers + usize::from(worker < batches % logical_workers);
        let first = first_batch * width;
        first_batch += count;
        let end = first_batch
            .checked_mul(width)
            .ok_or(FnxExecutionError::SizeOverflow)?
            .min(n);
        workers.push(Worker::<W, HARMONIC>::new(
            n,
            first,
            end - first,
            checkpoint,
        )?);
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
    for (index, end) in ends[..threads].iter_mut().enumerate() {
        *end = logical_workers * (index + 1) / threads;
    }
    arithmetic_rows(
        &mut workers,
        &ends[..threads],
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
    for worker in &workers {
        if let Some(error) = worker.error {
            return Err(match error {
                WorkerError::SizeOverflow => FnxExecutionError::SizeOverflow,
                WorkerError::InvalidProjection | WorkerError::Stopped => {
                    FnxExecutionError::InvalidUpstreamResult
                }
            });
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
        let output = &mut scores[worker.first_source..worker.first_source + worker.scores.len()];
        for (destination, source) in output
            .chunks_mut(ARITHMETIC_RUN)
            .zip(worker.scores.chunks(ARITHMETIC_RUN))
        {
            checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
            for &score in source {
                if !score.is_finite() {
                    return Err(FnxExecutionError::InvalidNumericResult);
                }
            }
            destination.copy_from_slice(source);
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

pub(super) fn run<const HARMONIC: bool, C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    let (width, probe_workspace) = lane_width(graph, checkpoint)?;
    let threads = if graph.node_count() < PARALLEL_SOURCES {
        1
    } else {
        std::thread::available_parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
            .min(MAX_GATHER_THREADS)
    };
    let (output, workspace) = match width {
        0 => run_scheduled::<0, HARMONIC, C>(graph, checkpoint, threads)?,
        1 => run_scheduled::<1, HARMONIC, C>(graph, checkpoint, threads)?,
        2 => run_scheduled::<2, HARMONIC, C>(graph, checkpoint, threads)?,
        3 => run_scheduled::<3, HARMONIC, C>(graph, checkpoint, threads)?,
        4 => run_scheduled::<4, HARMONIC, C>(graph, checkpoint, threads)?,
        5 => run_scheduled::<5, HARMONIC, C>(graph, checkpoint, threads)?,
        6 => run_scheduled::<6, HARMONIC, C>(graph, checkpoint, threads)?,
        7 => run_scheduled::<7, HARMONIC, C>(graph, checkpoint, threads)?,
        _ => run_scheduled::<8, HARMONIC, C>(graph, checkpoint, threads)?,
    };
    Ok((output, workspace.max(probe_workspace)))
}

#[cfg(test)]
#[path = "distance_tests.rs"]
mod tests;
