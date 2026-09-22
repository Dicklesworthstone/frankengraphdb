//! Checkpointed analytics over immutable decoded rows. Native kernels are
//! differentially checked against the pinned foundation; certificates identify
//! their own source instead of claiming that fnx executed the computation.

use crate::{
    AdapterPath, FNX_IMPLEMENTATION_REVISION, FNX_NUMERIC_PROFILE,
    FNX_SIGNATURE_REGISTRY_VERSION, FnxCallSpec, FnxOutput, GraphView,
    SnapshotBinding, SnapshotGraphView,
};
use fgdb_crypto::{Digest, Hasher};
use fgdb_types::VId;
pub use fnx_algorithms::ComplexityWitness;

const EXECUTION_KERNEL: &str = "fgdb-prism/pagerank-row-cursor-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FnxExecutionLimits {
    pub max_iterations: usize,
    pub max_result_rows: usize,
    /// Admission model: (n + directed adjacency entries) * (max_iter + 1).
    /// Not an observed CPU counter, hard memory quota, or deadline guarantee.
    pub max_estimated_work: usize,
}

#[derive(Debug)]
pub enum FnxExecutionError<C> {
    Cancelled(C),
    LimitExceeded {
        resource: &'static str,
        limit: usize,
        requested: usize,
    },
    SizeOverflow,
    AllocationFailed,
    NegativeWeight,
    NonFiniteWeightSum,
    InvalidNumericResult,
    InvalidUpstreamResult,
    NotConverged {
        max_iterations: usize,
        witness: ComplexityWitness,
    },
}
impl<C: core::fmt::Display> core::fmt::Display for FnxExecutionError<C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Cancelled(error) => write!(f, "Prism call cancelled: {error}"),
            Self::LimitExceeded { resource, limit, requested } => {
                write!(f, "Prism {resource} admission refused: {requested} > {limit}")
            }
            Self::SizeOverflow => f.write_str("Prism work estimate overflow"),
            Self::AllocationFailed => f.write_str("Prism working allocation failed"),
            Self::NegativeWeight => f.write_str("PageRank requires nonnegative projected weights"),
            Self::NonFiniteWeightSum => f.write_str("PageRank outgoing weight sum is non-finite"),
            Self::InvalidNumericResult => f.write_str("invalid PageRank score"),
            Self::InvalidUpstreamResult => f.write_str("analytics output does not cover the projection exactly once"),
            Self::NotConverged { max_iterations, .. } => {
                write!(f, "PageRank did not converge within {max_iterations} iterations")
            }
        }
    }
}
impl<C: core::error::Error + 'static> core::error::Error for FnxExecutionError<C> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Cancelled(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FnxValue {
    Vertex(VId),
    Score(f64),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FnxCertificate {
    pub registry_version: u16,
    /// Pinned foundation used as the differential semantics oracle.
    pub implementation_revision: &'static str,
    /// Actual executing kernel, distinct from the foundation oracle revision.
    pub execution_kernel: &'static str,
    /// Source identity of the kernel and decoded-row implementation. This is
    /// not an executable artifact hash or a signed replay manifest.
    pub kernel_source_digest: Digest,
    pub numeric_profile: &'static str,
    pub snapshot: SnapshotBinding,
    pub projection_digest: Digest,
    pub call_digest: Digest,
    pub result_digest: Digest,
    pub adapter: AdapterPath,
    pub vertices: usize,
    /// Simple projected edges, not the raw multigraph cardinality.
    pub edges: usize,
    pub input_edges: usize,
    pub estimated_work: usize,
    /// Requested vector backing bytes for the kernel, excluding the existing
    /// projection, output rows and allocator overhead. No adjacency is copied.
    pub kernel_workspace_bytes: usize,
    /// The foundation schema, populated from actual completed kernel passes.
    /// Admission, projection and result-conversion work are not loop counters.
    pub witness: ComplexityWitness,
    pub digest: Digest,
}
#[derive(Clone, Debug, PartialEq)]
pub struct FnxResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<FnxValue>>,
    pub certificate: FnxCertificate,
}

fn admit<C>(resource: &'static str, requested: usize, limit: usize) -> Result<(), FnxExecutionError<C>> {
    if requested > limit {
        Err(FnxExecutionError::LimitExceeded { resource, limit, requested })
    } else {
        Ok(())
    }
}
fn reserve<T, C>(length: usize) -> Result<Vec<T>, FnxExecutionError<C>> {
    let mut result = Vec::new();
    result.try_reserve_exact(length).map_err(|_| FnxExecutionError::AllocationFailed)?;
    Ok(result)
}

/// Preserve the pinned fnx scalar evaluation order exactly: ascending fixed-
/// width vertex names, ascending targets, division before multiplication,
/// dangling-mass redistribution, then an ordered L1 convergence test. Repeating
/// a row's division avoids fnx's second O(m) normalized-adjacency allocation.
/// All O(n)/O(m) walks checkpoint, including individual edges in a hub row.
fn pagerank<C>(
    graph: &SnapshotGraphView,
    options: crate::PageRankOptions,
    arcs: usize,
    checkpoint: &mut impl FnMut() -> Result<(), C>,
) -> Result<(Vec<f64>, ComplexityWitness), FnxExecutionError<C>> {
    let n = graph.node_count();
    let mut witness = ComplexityWitness {
        algorithm: "pagerank_power_iteration".to_owned(),
        complexity_claim: "O(k * (|V| + |E|))".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 0,
    };
    if n == 0 {
        return Ok((Vec::new(), witness));
    }
    let mut sums = reserve(n)?;
    let mut ranks = reserve(n)?;
    let mut next = reserve(n)?;
    let population = n as f64;
    for source in 0..n {
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
        let (targets, weights) = graph.projected_row(source).ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        let mut sum = 0.0;
        if options.weighted() {
            for &weight in weights {
                checkpoint().map_err(FnxExecutionError::Cancelled)?;
                if weight < 0.0 {
                    return Err(FnxExecutionError::NegativeWeight);
                }
                sum += weight;
                if !sum.is_finite() {
                    return Err(FnxExecutionError::NonFiniteWeightSum);
                }
            }
        } else {
            sum = targets.len() as f64;
        }
        sums.push(sum);
        ranks.push(1.0 / population);
        next.push(0.0);
    }
    let base = (1.0 - options.alpha()) / population;
    for _ in 0..options.max_iter() {
        let mut dangling_mass = 0.0;
        for source in 0..n {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            if sums[source] == 0.0 {
                dangling_mass += ranks[source];
            }
        }
        let initial = base + options.alpha() * dangling_mass / population;
        for value in &mut next {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            *value = initial;
        }
        for source in 0..n {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            let (targets, weights) = graph.projected_row(source).ok_or(FnxExecutionError::InvalidUpstreamResult)?;
            let push = options.alpha() * ranks[source];
            let sum = sums[source];
            for (offset, &target) in targets.iter().enumerate() {
                checkpoint().map_err(FnxExecutionError::Cancelled)?;
                let share = if options.weighted() {
                    if sum > 0.0 { weights[offset] / sum } else { weights[offset] }
                } else if sum > 0.0 {
                    1.0 / sum
                } else {
                    0.0
                };
                next[target] += push * share;
            }
        }
        let mut delta = 0.0;
        for index in 0..n {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            if !next[index].is_finite() || next[index] < 0.0 {
                return Err(FnxExecutionError::InvalidNumericResult);
            }
            delta += (next[index] - ranks[index]).abs();
        }
        // Swapping changes no arithmetic and avoids an uncheckpointed O(n)
        // copy. The old ranks buffer is overwritten during the next pass.
        std::mem::swap(&mut ranks, &mut next);
        witness.nodes_touched = witness.nodes_touched.checked_add(n).ok_or(FnxExecutionError::SizeOverflow)?;
        witness.edges_scanned = witness.edges_scanned.checked_add(arcs).ok_or(FnxExecutionError::SizeOverflow)?;
        if delta < population * options.tolerance() {
            return Ok((ranks, witness));
        }
    }
    Err(FnxExecutionError::NotConverged { max_iterations: options.max_iter(), witness })
}

impl FnxCallSpec {
    /// Execute a bound in-core call with cancellation inside every graph pass,
    /// not merely before/after an uninterruptible foundation call. Kernel
    /// workspace is three O(n) vectors; projection adjacency remains borrowed.
    /// The callback may cancel but this synchronous API does not provide an
    /// asynchronous bulkhead, a deadline guarantee or external-memory spill.
    pub fn execute<C>(
        &self,
        graph: &SnapshotGraphView,
        limits: FnxExecutionLimits,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<FnxResult, FnxExecutionError<C>> {
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
        let options = self.options();
        let n = graph.node_count();
        admit("iterations", options.max_iter(), limits.max_iterations)?;
        admit("result rows", n, limits.max_result_rows)?;
        let mut arcs = 0usize;
        for source in 0..n {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            let neighbors = graph.neighbors_indices(source).ok_or(FnxExecutionError::InvalidUpstreamResult)?;
            arcs = arcs.checked_add(neighbors.len()).ok_or(FnxExecutionError::SizeOverflow)?;
        }
        let estimated_work = n.checked_add(arcs)
            .and_then(|step| options.max_iter().checked_add(1).and_then(|iterations| step.checked_mul(iterations)))
            .ok_or(FnxExecutionError::SizeOverflow)?;
        admit("estimated work", estimated_work, limits.max_estimated_work)?;
        let kernel_workspace_bytes = n.checked_mul(3 * std::mem::size_of::<f64>())
            .ok_or(FnxExecutionError::SizeOverflow)?;
        let (scores, witness) = pagerank(graph, options, arcs, &mut checkpoint)?;
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
        let mut rows = reserve(n)?;
        let mut result_hash = Hasher::new();
        result_hash.update(b"fgdb:prism:result-rows:v1");
        result_hash.update(&(n as u128).to_le_bytes());
        result_hash.update(&self.digest().0);
        for (index, score) in scores.into_iter().enumerate() {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            let vertex = graph.vertex_id(index).ok_or(FnxExecutionError::InvalidUpstreamResult)?;
            let mut row = reserve(self.outputs().len())?;
            for column in self.outputs() {
                row.push(match column.field {
                    FnxOutput::Vertex => {
                        result_hash.update(&vertex.0.to_le_bytes());
                        FnxValue::Vertex(vertex)
                    }
                    FnxOutput::Score => {
                        result_hash.update(&score.to_bits().to_le_bytes());
                        FnxValue::Score(score)
                    }
                });
            }
            rows.push(row);
        }
        let mut source_hash = Hasher::new();
        source_hash.update(b"fgdb:prism:kernel-source:v1");
        hash_text(&mut source_hash, include_str!("execute.rs"));
        hash_text(&mut source_hash, include_str!("projection.rs"));
        let kernel_source_digest = source_hash.finalize();
        let mut hash = Hasher::new();
        hash.update(b"fgdb:prism:call-certificate:v2");
        hash.update(&FNX_SIGNATURE_REGISTRY_VERSION.to_le_bytes());
        hash.update(FNX_IMPLEMENTATION_REVISION.as_bytes());
        hash_text(&mut hash, EXECUTION_KERNEL);
        hash.update(&kernel_source_digest.0);
        hash_text(&mut hash, FNX_NUMERIC_PROFILE);
        hash.update(&graph.digest().0);
        hash.update(&self.digest().0);
        let result_digest = result_hash.finalize();
        hash.update(&result_digest.0);
        hash_text(&mut hash, graph.adapter_path().as_str());
        for number in [n, graph.edge_count(), graph.input_edge_count(), estimated_work,
            kernel_workspace_bytes, witness.nodes_touched, witness.edges_scanned, witness.queue_peak] {
            hash.update(&(number as u128).to_le_bytes());
        }
        hash_text(&mut hash, &witness.algorithm);
        hash_text(&mut hash, &witness.complexity_claim);
        let certificate = FnxCertificate {
            registry_version: FNX_SIGNATURE_REGISTRY_VERSION,
            implementation_revision: FNX_IMPLEMENTATION_REVISION,
            execution_kernel: EXECUTION_KERNEL,
            kernel_source_digest,
            numeric_profile: FNX_NUMERIC_PROFILE,
            snapshot: graph.binding(),
            projection_digest: graph.digest(),
            call_digest: self.digest(),
            result_digest,
            adapter: graph.adapter_path(),
            vertices: n,
            edges: graph.edge_count(),
            input_edges: graph.input_edge_count(),
            estimated_work,
            kernel_workspace_bytes,
            witness,
            digest: hash.finalize(),
        };
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
        Ok(FnxResult {
            columns: self.outputs().iter().map(|column| column.name.clone()).collect(),
            rows,
            certificate,
        })
    }
}
fn hash_text(hash: &mut Hasher, text: &str) {
    hash.update(&(text.len() as u128).to_le_bytes());
    hash.update(text.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Directedness, PageRankOptions, ParallelEdgePolicy, ProjectionEdge,
        ProjectionLimits, ProjectionSpec, SelfLoopPolicy, PROJECTED_WEIGHT_ATTRIBUTE};
    use fgdb_types::{CommitSeq, EId, ids::ObjectId};
    use std::convert::Infallible;

    fn projection(mask: u16, direction: Directedness) -> SnapshotGraphView {
        let vertices = [VId(0), VId(17), VId(u128::MAX)];
        let edges: Vec<_> = (0..9).filter(|bit| mask & (1 << bit) != 0).map(|bit| {
            ProjectionEdge {
                eid: EId(bit as u128), source: vertices[bit / 3], target: vertices[bit % 3],
                weight: [0.0, 0.1, 0.3, 1.0, 7.0][bit % 5],
            }
        }).collect();
        SnapshotGraphView::build(
            SnapshotBinding { root: ObjectId([2; 32]), as_of: CommitSeq(3) },
            &vertices, &edges,
            ProjectionSpec { directedness: direction, parallel_edges: ParallelEdgePolicy::Sum,
                self_loops: SelfLoopPolicy::Keep },
            ProjectionLimits { max_vertices: 3, max_input_edges: 9,
                max_adjacency_entries: 18, max_workspace_bytes: 1 << 20 },
        ).unwrap()
    }

    #[test]
    fn cooperative_kernel_is_bit_identical_to_fnx_on_all_three_node_topologies() {
        for mask in 0..512 {
            for direction in [Directedness::Directed, Directedness::Reversed, Directedness::Undirected] {
                let graph = projection(mask, direction);
                let arcs = (0..3).map(|node| graph.neighbors_indices(node).unwrap().len()).sum();
                for weighted in [false, true] {
                    let options = PageRankOptions::new(0.85, 1000, 1e-12, weighted).unwrap();
                    let oracle = fnx_algorithms::pagerank_with_weight(
                        &graph, options.alpha(), options.max_iter(), options.tolerance(),
                        weighted.then_some(PROJECTED_WEIGHT_ATTRIBUTE),
                    );
                    assert!(oracle.converged);
                    let (scores, witness) = pagerank(&graph, options, arcs, &mut || Ok::<(), Infallible>(())).unwrap();
                    for score in oracle.scores {
                        let ordinal = graph.get_node_index(&score.node).unwrap();
                        assert_eq!(scores[ordinal].to_bits(), score.score.to_bits(), "{mask} {direction:?} weighted={weighted}");
                    }
                    assert_eq!(witness, oracle.witness);
                }
            }
        }
    }

    #[test]
    fn cancellation_reaches_iteration_and_every_partial_result_is_discarded() {
        let graph = projection(2, Directedness::Directed);
        let options = PageRankOptions::new(0.85, 1000, 1e-12, true).unwrap();
        let mut checkpoints = 0;
        pagerank(&graph, options, 1, &mut || {
            checkpoints += 1;
            Ok::<(), &'static str>(())
        }).unwrap();
        // Initial validation takes n + m = 4 calls; subsequent callbacks
        // therefore prove cancellation INSIDE the power-iteration loop.
        assert!(checkpoints > 20);
        for stop in 1..=checkpoints {
            let mut observed = 0;
            let result = pagerank(&graph, options, 1, &mut || {
                observed += 1;
                if observed == stop { Err("stop") } else { Ok(()) }
            });
            assert!(matches!(result, Err(FnxExecutionError::Cancelled("stop"))));
            assert_eq!(observed, stop);
        }
    }

    #[test]
    fn borrowed_rows_and_certificate_identify_the_executing_kernel() {
        let graph = projection(511, Directedness::Undirected);
        let cloned = graph.clone();
        for node in 0..3 {
            let (targets, weights) = graph.projected_row(node).unwrap();
            let (shared_targets, shared_weights) = cloned.projected_row(node).unwrap();
            assert_eq!(targets.len(), weights.len());
            assert!(std::ptr::eq(targets, shared_targets));
            assert!(std::ptr::eq(weights, shared_weights));
            for (&target, &weight) in targets.iter().zip(weights) {
                assert_eq!(graph.projected_weight(node, target), Some(weight));
            }
        }
        assert!(graph.projected_row(usize::MAX).is_none());
        let call = FnxCallSpec::pagerank(PageRankOptions::default());
        let result = call.execute(&graph, FnxExecutionLimits {
            max_iterations: 100, max_result_rows: 3, max_estimated_work: 10000,
        }, || Ok::<(), Infallible>(())).unwrap();
        assert_eq!(result.certificate.execution_kernel, EXECUTION_KERNEL);
        assert_eq!(result.certificate.kernel_workspace_bytes, 3 * 3 * std::mem::size_of::<f64>());
    }
}
