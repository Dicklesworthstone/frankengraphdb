//! The pinned franken_networkx catalog and its differential native kernels.
//!
//! Degree centrality and core decomposition borrow the admitted CSR directly.
//! They preserve the catalog's logical charges and check live control during
//! arithmetic. Certificates identify these native kernels and their actual
//! vector allocation peak; the projection remains an in-core DECODED_CACHE.
//!
//! The remaining foundation entry points take `&fnx_classes::Graph` and need
//! a second, string-keyed graph copy. Those calls observe cancellation before
//! and after the foundation computation. Iterative procedures use the entry
//! that reports convergence and refuse instead of publishing unconverged
//! scores. Katz remains unregistered because its checked entry drops the
//! result and witness on non-convergence.

mod native;

use crate::execute::{FnxMeter, KernelOutput, KernelValues};
use crate::{FnxExecutionError, FoundationAlgorithm, SnapshotGraphView};
use fnx_algorithms::{CentralityScore, ComplexityWitness, GraphView};
use fnx_classes::Graph;

const EIGENVECTOR_MAX_ITERATIONS: usize = 100;
const EIGENVECTOR_TOLERANCE: f64 = 1.0e-6;

/// Admission-time work bound over `n` vertices and `arcs` adjacency entries
/// (an undirected edge counts in both rows).
pub(crate) fn estimated_work(
    algorithm: FoundationAlgorithm,
    n: usize,
    arcs: usize,
) -> Option<usize> {
    let step = n.checked_add(arcs)?;
    match algorithm {
        FoundationAlgorithm::DegreeCentrality | FoundationAlgorithm::CoreNumber => Some(step),
        FoundationAlgorithm::ClosenessCentrality
        | FoundationAlgorithm::HarmonicCentrality
        | FoundationAlgorithm::BetweennessCentrality => n.checked_mul(step),
        FoundationAlgorithm::EigenvectorCentrality => {
            step.checked_mul(EIGENVECTOR_MAX_ITERATIONS + 1)
        }
    }
}

/// The copy's own backing: one owned 32-hex name per vertex, and one entry
/// per adjacency slot. fnx's internal working vectors are not counted.
pub(crate) fn copy_bytes(n: usize, arcs: usize) -> Option<usize> {
    let per_vertex = 32usize.checked_add(size_of::<String>())?;
    n.checked_mul(per_vertex)?
        .checked_add(arcs.checked_mul(size_of::<usize>())?)
}

pub(crate) fn run<C>(
    graph: &SnapshotGraphView,
    algorithm: FoundationAlgorithm,
    arcs: usize,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    let n = graph.node_count();
    if matches!(
        algorithm,
        FoundationAlgorithm::DegreeCentrality | FoundationAlgorithm::CoreNumber
    ) {
        // Retain the copy path's node and source-row charge phases, including
        // the exact self-loop refusal checkpoint. No node names are created.
        for _ in 0..n {
            checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        }
        for source in 0..n {
            checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
            let row = graph
                .neighbors_indices(source)
                .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
            // Projection rows are sorted, simple and immutable. A degree
            // query need not scan every arc merely to reject a self-loop.
            if row.binary_search(&source).is_ok() {
                return Err(FnxExecutionError::SelfLoopRefused);
            }
        }
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        let result = match algorithm {
            FoundationAlgorithm::DegreeCentrality => native::degree(graph, checkpoint)?,
            FoundationAlgorithm::CoreNumber => native::core(graph, checkpoint)?,
            _ => return Err(FnxExecutionError::InvalidUpstreamResult),
        };
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        return Ok(result);
    }

    let workspace = copy_bytes(n, arcs).ok_or(FnxExecutionError::SizeOverflow)?;
    let names = graph.nodes_ordered();
    if names.len() != n {
        return Err(FnxExecutionError::InvalidUpstreamResult);
    }
    let mut copy = Graph::strict();
    for name in &names {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        copy.add_node(*name);
    }
    for source in 0..n {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        let row = graph
            .neighbors_indices(source)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        for &target in row {
            if target == source {
                return Err(FnxExecutionError::SelfLoopRefused);
            }
            // Undirected edges are canonical (source < target); each is added
            // once whether its row is stored in one direction or both.
            if source < target {
                copy.add_edge(names[source], names[target])
                    .map_err(|_| FnxExecutionError::InvalidUpstreamResult)?;
            }
        }
    }
    checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
    let output = match algorithm {
        FoundationAlgorithm::ClosenessCentrality => {
            let result = fnx_algorithms::closeness_centrality(&copy);
            scores(graph, result.scores, result.witness)?
        }
        FoundationAlgorithm::HarmonicCentrality => {
            let result = fnx_algorithms::harmonic_centrality(&copy);
            scores(graph, result.scores, result.witness)?
        }
        FoundationAlgorithm::BetweennessCentrality => {
            let result = fnx_algorithms::betweenness_centrality(&copy);
            scores(graph, result.scores, result.witness)?
        }
        FoundationAlgorithm::EigenvectorCentrality => {
            let (result, converged) = fnx_algorithms::eigenvector_centrality_with_params(
                &copy,
                EIGENVECTOR_MAX_ITERATIONS,
                EIGENVECTOR_TOLERANCE,
            );
            if !converged {
                return Err(FnxExecutionError::NotConverged {
                    max_iterations: EIGENVECTOR_MAX_ITERATIONS,
                    witness: result.witness,
                });
            }
            scores(graph, result.scores, result.witness)?
        }
        FoundationAlgorithm::DegreeCentrality | FoundationAlgorithm::CoreNumber => {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
    };
    checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
    Ok((output, workspace))
}

/// One finite score per projected vertex, indexed by its canonical ordinal.
fn scores<C>(
    graph: &SnapshotGraphView,
    scores: Vec<CentralityScore>,
    witness: ComplexityWitness,
) -> Result<KernelOutput, FnxExecutionError<C>> {
    let n = graph.node_count();
    let mut values = vec![None; n];
    for entry in scores {
        let index = graph
            .get_node_index(&entry.node)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        if !entry.score.is_finite() {
            return Err(FnxExecutionError::InvalidNumericResult);
        }
        if values[index].replace(entry.score).is_some() {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
    }
    Ok(KernelOutput {
        values: KernelValues::Scores(complete(values)?),
        row_count: n,
        witness,
    })
}

/// Every vertex answered exactly once; a missing one is an upstream defect.
fn complete<T, C>(values: Vec<Option<T>>) -> Result<Vec<T>, FnxExecutionError<C>> {
    values
        .into_iter()
        .collect::<Option<Vec<T>>>()
        .ok_or(FnxExecutionError::InvalidUpstreamResult)
}
