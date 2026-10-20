//! Deterministic degree and core kernels over the admitted CSR.
//!
//! These kernels borrow the projection. They do not build an fnx Graph, node
//! names, or a second adjacency index. The caller retains the catalog's
//! logical charge sequence; arithmetic checks live control without charging
//! those units again.

use crate::execute::{FnxMeter, KernelOutput, KernelValues, reserve};
use crate::{FnxExecutionError, SnapshotGraphView};
use fnx_algorithms::{ComplexityWitness, GraphView};

/// A work step is one initialization, bucket, vertex or incidence operation.
/// Even a single hub row yields to live authority/cancellation between runs.
const ARITHMETIC_RUN: usize = 4096;

pub(super) struct Arithmetic<'a, M> {
    checkpoint: &'a mut M,
    remaining: usize,
}

impl<'a, M> Arithmetic<'a, M> {
    pub(super) fn new(checkpoint: &'a mut M) -> Self {
        Self {
            checkpoint,
            remaining: 0,
        }
    }

    #[inline]
    pub(super) fn step<C>(&mut self) -> Result<(), FnxExecutionError<C>>
    where
        M: FnxMeter<C>,
    {
        if self.remaining == 0 {
            self.checkpoint
                .poll()
                .map_err(FnxExecutionError::Cancelled)?;
            self.remaining = ARITHMETIC_RUN;
        }
        self.remaining -= 1;
        Ok(())
    }

    pub(super) fn finish<C>(&mut self) -> Result<(), FnxExecutionError<C>>
    where
        M: FnxMeter<C>,
    {
        self.checkpoint.poll().map_err(FnxExecutionError::Cancelled)
    }
}

pub(super) fn vector_bytes<T, C>(values: &Vec<T>) -> Result<usize, FnxExecutionError<C>> {
    values
        .capacity()
        .checked_mul(size_of::<T>())
        .ok_or(FnxExecutionError::SizeOverflow)
}

fn zeroes<C>(
    length: usize,
    arithmetic: &mut Arithmetic<'_, impl FnxMeter<C>>,
) -> Result<Vec<usize>, FnxExecutionError<C>> {
    let mut values = reserve(length)?;
    for _ in 0..length {
        arithmetic.step()?;
        values.push(0);
    }
    Ok(values)
}

pub(super) fn degree<C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    let n = graph.node_count();
    let mut arithmetic = Arithmetic::new(checkpoint);
    arithmetic.step()?;
    let mut values = reserve(n)?;
    // Match the pinned foundation's multiplication by a single reciprocal.
    // Replacing this with one division per vertex changes some score bits.
    let reciprocal = if n > 1 { 1.0 / (n - 1) as f64 } else { 1.0 };
    let mut arcs = 0usize;
    for vertex in 0..n {
        arithmetic.step()?;
        let degree = graph
            .neighbors_indices(vertex)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?
            .len();
        arcs = arcs
            .checked_add(degree)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        values.push(if n == 1 {
            1.0
        } else {
            degree as f64 * reciprocal
        });
    }
    let workspace = vector_bytes(&values)?;
    arithmetic.finish()?;
    Ok((
        KernelOutput {
            values: KernelValues::Scores(values),
            row_count: n,
            witness: ComplexityWitness {
                algorithm: "degree_centrality".to_owned(),
                complexity_claim: "O(|V| + |E|)".to_owned(),
                nodes_touched: n,
                edges_scanned: arcs,
                queue_peak: 0,
            },
        },
        workspace,
    ))
}

pub(super) fn core<C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    let n = graph.node_count();
    let mut arithmetic = Arithmetic::new(checkpoint);
    arithmetic.step()?;
    if n == 0 {
        arithmetic.finish()?;
        return Ok((
            KernelOutput {
                values: KernelValues::Counts(Vec::new()),
                row_count: 0,
                witness: ComplexityWitness {
                    algorithm: "core_number".to_owned(),
                    complexity_claim: "O(|V| + |E|)".to_owned(),
                    nodes_touched: 0,
                    edges_scanned: 0,
                    queue_peak: 0,
                },
            },
            0,
        ));
    }

    let mut degree = reserve(n)?;
    let mut max_degree = 0usize;
    for vertex in 0..n {
        arithmetic.step()?;
        let count = graph
            .neighbors_indices(vertex)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?
            .len();
        degree.push(count);
        max_degree = max_degree.max(count);
    }
    let bins_len = max_degree
        .checked_add(1)
        .ok_or(FnxExecutionError::SizeOverflow)?;
    let mut bins = zeroes(bins_len, &mut arithmetic)?;
    let mut vertices = zeroes(n, &mut arithmetic)?;
    let mut position = zeroes(n, &mut arithmetic)?;
    let degree_bytes = vector_bytes(&degree)?;
    let bins_bytes = vector_bytes(&bins)?;
    let vertices_bytes = vector_bytes(&vertices)?;
    let position_bytes = vector_bytes(&position)?;
    let scratch_peak = degree_bytes
        .checked_add(bins_bytes)
        .and_then(|bytes| bytes.checked_add(vertices_bytes))
        .and_then(|bytes| bytes.checked_add(position_bytes))
        .ok_or(FnxExecutionError::SizeOverflow)?;

    // Stable counting sort by initial degree. The simple, loop-free graph
    // has max_degree < n, so this is linear and needs no comparison sort.
    for &count in &degree {
        arithmetic.step()?;
        bins[count] += 1;
    }
    let mut start = 0usize;
    for slot in &mut bins {
        arithmetic.step()?;
        let count = *slot;
        *slot = start;
        start = start
            .checked_add(count)
            .ok_or(FnxExecutionError::SizeOverflow)?;
    }
    for vertex in 0..n {
        arithmetic.step()?;
        let count = degree[vertex];
        let slot = bins[count];
        position[vertex] = slot;
        vertices[slot] = vertex;
        bins[count] += 1;
    }
    // Scatter advanced each bin to its end. Restore the bucket starts.
    for count in (1..bins.len()).rev() {
        arithmetic.step()?;
        bins[count] = bins[count - 1];
    }
    bins[0] = 0;

    let mut arcs = 0usize;
    for slot in 0..n {
        arithmetic.step()?;
        let vertex = vertices[slot];
        let row = graph
            .neighbors_indices(vertex)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        for &neighbor in row {
            arithmetic.step()?;
            if degree[neighbor] > degree[vertex] {
                let count = degree[neighbor];
                let from = position[neighbor];
                let to = bins[count];
                let first = vertices[to];
                if neighbor != first {
                    vertices[from] = first;
                    vertices[to] = neighbor;
                    position[neighbor] = to;
                    position[first] = from;
                }
                bins[count] += 1;
                degree[neighbor] -= 1;
            }
        }
        arcs = arcs
            .checked_add(row.len())
            .ok_or(FnxExecutionError::SizeOverflow)?;
    }

    // A peeled vertex's degree never decreases again: the final degree is
    // its core number. Release the bucket index before allocating results.
    drop(bins);
    drop(vertices);
    drop(position);
    let mut counts = reserve(n)?;
    let conversion_peak = degree_bytes
        .checked_add(vector_bytes(&counts)?)
        .ok_or(FnxExecutionError::SizeOverflow)?;
    for count in degree {
        arithmetic.step()?;
        counts.push(u64::try_from(count).map_err(|_| FnxExecutionError::SizeOverflow)?);
    }
    arithmetic.finish()?;
    Ok((
        KernelOutput {
            values: KernelValues::Counts(counts),
            row_count: n,
            witness: ComplexityWitness {
                algorithm: "core_number".to_owned(),
                complexity_claim: "O(|V| + |E|)".to_owned(),
                nodes_touched: n,
                edges_scanned: arcs,
                queue_peak: 0,
            },
        },
        scratch_peak.max(conversion_peak),
    ))
}
