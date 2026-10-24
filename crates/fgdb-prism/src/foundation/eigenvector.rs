//! Pinned eigenvector power iteration over the admitted CSR.
//!
//! The fnx catalog sorts fixed-width lowercase vertex names before scattering
//! each source score. Those names have the same order as Prism's full-width
//! vertex ordinals. On the admitted simple undirected graph, row t is exactly
//! the ascending list of sources that contribute to destination t. Gathering
//! that row, starting with x[t], therefore reproduces every destination's
//! sequence of floating-point additions in fnx's (I + A) * x scatter.
//!
//! Only the two score buffers are owned here. Neither a string-keyed Graph nor
//! another CSR is built, and swapping the buffers avoids copying each result
//! back. Norm and delta still accumulate in canonical order without partial
//! sums. Live polls also cover initialization, hub rows, and reductions.

use super::native::{Arithmetic, vector_bytes};
use super::{EIGENVECTOR_MAX_ITERATIONS, EIGENVECTOR_TOLERANCE};
use crate::execute::{KernelOutput, KernelValues, reserve};
use crate::{FnxExecutionError, FnxMeter, SnapshotGraphView};
use fnx_algorithms::{ComplexityWitness, GraphView};

/// The parent has already admitted the undirected simple projection, refused
/// self-loops, and charged the legacy foundation checkpoints. Arithmetic only
/// polls live control; it never spends the same logical work a second time.
pub(super) fn run<C>(
    graph: &SnapshotGraphView,
    meter: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    let n = graph.node_count();
    let mut arithmetic = Arithmetic::new(meter);
    arithmetic.step()?;
    let mut scores = reserve(n)?;
    let initial = if n <= 1 { 1.0 } else { 1.0 / n as f64 };
    for _ in 0..n {
        arithmetic.step()?;
        scores.push(initial);
    }
    let mut workspace_bytes = vector_bytes(&scores)?;
    let mut witness = ComplexityWitness {
        algorithm: "eigenvector_centrality_power_iteration".to_owned(),
        complexity_claim: "O(k * (|V| + |E|))".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 0,
    };

    // fnx answers these without an iteration, including singleton score 1.
    if n <= 1 {
        witness.nodes_touched = n;
        arithmetic.finish()?;
        return Ok((
            KernelOutput {
                values: KernelValues::Scores(scores),
                row_count: n,
                witness,
            },
            workspace_bytes,
        ));
    }

    let mut next_scores = reserve(n)?;
    for _ in 0..n {
        arithmetic.step()?;
        next_scores.push(0.0);
    }
    workspace_bytes = workspace_bytes
        .checked_add(vector_bytes(&next_scores)?)
        .ok_or(FnxExecutionError::SizeOverflow)?;
    let arcs = graph
        .outgoing_offsets()
        .last()
        .copied()
        .ok_or(FnxExecutionError::InvalidUpstreamResult)?;

    for _ in 0..EIGENVECTOR_MAX_ITERATIONS {
        gather(graph, &scores, &mut next_scores, &mut arithmetic)?;
        witness.nodes_touched = witness
            .nodes_touched
            .checked_add(n)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        witness.edges_scanned = witness
            .edges_scanned
            .checked_add(arcs)
            .ok_or(FnxExecutionError::SizeOverflow)?;

        // Carry one scalar accumulator across every poll. Summing chunks
        // separately would reassociate fnx's f64 additions.
        let mut norm_squared = 0.0;
        for &value in &next_scores {
            arithmetic.step()?;
            norm_squared += value * value;
        }
        let norm = norm_squared.sqrt();
        let normalizer = if norm > 0.0 { norm } else { 1.0 };
        for value in &mut next_scores {
            arithmetic.step()?;
            *value /= normalizer;
        }

        let mut delta = 0.0;
        for (&left, &right) in next_scores.iter().zip(&scores) {
            arithmetic.step()?;
            delta += (left - right).abs();
        }
        std::mem::swap(&mut scores, &mut next_scores);
        if delta < n as f64 * EIGENVECTOR_TOLERANCE {
            for &value in &scores {
                arithmetic.step()?;
                if !value.is_finite() {
                    return Err(FnxExecutionError::InvalidNumericResult);
                }
            }
            arithmetic.finish()?;
            return Ok((
                KernelOutput {
                    values: KernelValues::Scores(scores),
                    row_count: n,
                    witness,
                },
                workspace_bytes,
            ));
        }
    }

    arithmetic.finish()?;
    Err(FnxExecutionError::NotConverged {
        max_iterations: EIGENVECTOR_MAX_ITERATIONS,
        witness,
    })
}

/// On this undirected projection, an outgoing row is also the incoming row.
/// Each accumulator remains local until its whole row has been admitted by
/// live polls. This includes polls inside a single high-degree row.
fn gather<C>(
    graph: &SnapshotGraphView,
    scores: &[f64],
    next_scores: &mut [f64],
    arithmetic: &mut Arithmetic<'_, impl FnxMeter<C>>,
) -> Result<(), FnxExecutionError<C>> {
    for (target, next_score) in next_scores.iter_mut().enumerate() {
        arithmetic.step()?;
        let mut sum = scores[target];
        let sources = graph
            .neighbors_indices(target)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        for &source in sources {
            arithmetic.step()?;
            sum += scores[source];
        }
        *next_score = sum;
    }
    Ok(())
}

#[cfg(test)]
#[path = "eigenvector_tests.rs"]
mod tests;
