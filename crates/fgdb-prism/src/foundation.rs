//! The pinned franken_networkx catalog as Prism kernels.
//!
//! fnx-algorithms' centrality and core entry points take `&fnx_classes::Graph`,
//! so every call crosses an explicit copy of the admitted projection: the
//! labelled DECODED_CACHE boundary, named in the certificate by the row's
//! execution kernel. The foundation computes. Prism admits the call, copies
//! the projection in canonical VId order, maps node names back to VIds, and
//! certifies the result with the foundation's own complexity witness.
//!
//! A foundation call runs to completion. It is bounded at admission by the
//! row's work estimate, and cancellation is observed before and after it,
//! never inside it. An iterative procedure runs through the entry that
//! reports convergence, and it refuses instead of publishing unconverged
//! scores. (fnx's convenience `eigenvector_centrality` and `katz_centrality`
//! discard that flag. Katz stays unregistered because its checked entry
//! drops the result and its witness on non-convergence.)

use crate::execute::{KernelOutput, KernelValues};
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
    checkpoint: &mut impl FnMut() -> Result<(), C>,
) -> Result<KernelOutput, FnxExecutionError<C>> {
    let n = graph.node_count();
    let names = graph.nodes_ordered();
    if names.len() != n {
        return Err(FnxExecutionError::InvalidUpstreamResult);
    }
    let mut copy = Graph::strict();
    for name in &names {
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
        copy.add_node(*name);
    }
    for source in 0..n {
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
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
    checkpoint().map_err(FnxExecutionError::Cancelled)?;
    let output = match algorithm {
        FoundationAlgorithm::DegreeCentrality => {
            let result = fnx_algorithms::degree_centrality(&copy);
            scores(graph, result.scores, result.witness)?
        }
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
        FoundationAlgorithm::CoreNumber => {
            let result = fnx_algorithms::core_number(&copy);
            let mut cores = vec![None; n];
            for entry in result.core_numbers {
                let index = graph
                    .get_node_index(&entry.node)
                    .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
                let core =
                    u64::try_from(entry.core).map_err(|_| FnxExecutionError::SizeOverflow)?;
                if cores[index].replace(core).is_some() {
                    return Err(FnxExecutionError::InvalidUpstreamResult);
                }
            }
            KernelOutput {
                values: KernelValues::Counts(complete(cores)?),
                row_count: n,
                witness: result.witness,
            }
        }
    };
    checkpoint().map_err(FnxExecutionError::Cancelled)?;
    Ok(output)
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
