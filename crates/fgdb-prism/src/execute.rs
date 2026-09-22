//! Invoke the pinned foundation, preserving its witness and refusing invalid
//! numeric results. This module never reimplements a graph algorithm.

use crate::{
    AdapterPath, FNX_IMPLEMENTATION_REVISION, FNX_NUMERIC_PROFILE,
    FNX_SIGNATURE_REGISTRY_VERSION, FnxCallSpec, FnxOutput, GraphView,
    PROJECTED_WEIGHT_ATTRIBUTE, SnapshotBinding, SnapshotGraphView,
};
use fgdb_crypto::{Digest, Hasher};
use fgdb_types::VId;
pub use fnx_algorithms::ComplexityWitness;

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
    LimitExceeded { resource: &'static str, limit: usize, requested: usize },
    SizeOverflow,
    AllocationFailed,
    NegativeWeight,
    NonFiniteWeightSum,
    InvalidNumericResult,
    InvalidUpstreamResult,
    NotConverged { max_iterations: usize, witness: ComplexityWitness },
}
impl<C: core::fmt::Display> core::fmt::Display for FnxExecutionError<C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Cancelled(error) => write!(f, "Prism call cancelled: {error}"),
            Self::LimitExceeded { resource, limit, requested } => write!(f, "Prism {resource} admission refused: {requested} > {limit}"),
            Self::SizeOverflow => f.write_str("Prism work estimate overflow"),
            Self::AllocationFailed => f.write_str("Prism result allocation failed"),
            Self::NegativeWeight => f.write_str("PageRank requires nonnegative projected weights"),
            Self::NonFiniteWeightSum => f.write_str("PageRank outgoing weight sum is non-finite"),
            Self::InvalidNumericResult => f.write_str("fnx produced an invalid PageRank score"),
            Self::InvalidUpstreamResult => f.write_str("fnx output does not cover the projection exactly once"),
            Self::NotConverged { max_iterations, .. } => write!(f, "PageRank did not converge within {max_iterations} iterations"),
        }
    }
}
impl<C: core::error::Error + 'static> core::error::Error for FnxExecutionError<C> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self { Self::Cancelled(error) => Some(error), _ => None }
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
    pub implementation_revision: &'static str,
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
    /// The original upstream structure, not a reconstructed complexity claim.
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

impl FnxCallSpec {
    /// Execute a fully bound in-core call. Checkpoints cover admission, weight
    /// validation and result conversion. The pinned fnx iteration loop itself
    /// has NO cancellation callback; cancellation during it is observed after
    /// it returns and discards all results. This is not PrismRegion isolation.
    /// fnx working allocations remain subject to the host allocator, not a
    /// registered spill quota. Callers must choose an appropriate in-core cap.
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
        if options.weighted() {
            for source in 0..n {
                checkpoint().map_err(FnxExecutionError::Cancelled)?;
                let mut sum = 0.0;
                for &target in graph.neighbors_indices(source).ok_or(FnxExecutionError::InvalidUpstreamResult)? {
                    let weight = graph.projected_weight(source, target).ok_or(FnxExecutionError::InvalidUpstreamResult)?;
                    if weight < 0.0 { return Err(FnxExecutionError::NegativeWeight); }
                    sum += weight;
                    if !sum.is_finite() { return Err(FnxExecutionError::NonFiniteWeightSum); }
                }
            }
        }
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
        let output = fnx_algorithms::pagerank_with_weight(
            graph, options.alpha(), options.max_iter(), options.tolerance(),
            options.weighted().then_some(PROJECTED_WEIGHT_ATTRIBUTE),
        );
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
        if !output.converged {
            return Err(FnxExecutionError::NotConverged { max_iterations: options.max_iter(), witness: output.witness });
        }
        if output.scores.len() != n { return Err(FnxExecutionError::InvalidUpstreamResult); }
        let mut scores = reserve(n)?;
        scores.resize(n, None);
        for score in output.scores {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            if !score.score.is_finite() || score.score < 0.0 { return Err(FnxExecutionError::InvalidNumericResult); }
            let index = graph.get_node_index(&score.node).ok_or(FnxExecutionError::InvalidUpstreamResult)?;
            if scores[index].replace(score.score).is_some() { return Err(FnxExecutionError::InvalidUpstreamResult); }
        }
        let mut rows = reserve(n)?;
        let mut result_hash = Hasher::new();
        result_hash.update(b"fgdb:prism:result-rows:v1");
        result_hash.update(&(n as u128).to_le_bytes());
        result_hash.update(&self.digest().0);
        for (index, score) in scores.into_iter().enumerate() {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            let score = score.ok_or(FnxExecutionError::InvalidUpstreamResult)?;
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
        let mut hash = Hasher::new();
        hash.update(b"fgdb:prism:call-certificate:v1");
        hash.update(&FNX_SIGNATURE_REGISTRY_VERSION.to_le_bytes());
        hash.update(FNX_IMPLEMENTATION_REVISION.as_bytes());
        hash_text(&mut hash, FNX_NUMERIC_PROFILE);
        hash.update(&graph.digest().0);
        hash.update(&self.digest().0);
        let result_digest = result_hash.finalize();
        hash.update(&result_digest.0);
        hash_text(&mut hash, graph.adapter_path().as_str());
        for number in [n, graph.edge_count(), graph.input_edge_count(), estimated_work,
            output.witness.nodes_touched, output.witness.edges_scanned, output.witness.queue_peak] {
            hash.update(&(number as u128).to_le_bytes());
        }
        hash_text(&mut hash, &output.witness.algorithm);
        hash_text(&mut hash, &output.witness.complexity_claim);
        let certificate = FnxCertificate {
            registry_version: FNX_SIGNATURE_REGISTRY_VERSION,
            implementation_revision: FNX_IMPLEMENTATION_REVISION,
            numeric_profile: FNX_NUMERIC_PROFILE,
            snapshot: graph.binding(), projection_digest: graph.digest(), call_digest: self.digest(),
            result_digest,
            adapter: graph.adapter_path(), vertices: n, edges: graph.edge_count(),
            input_edges: graph.input_edge_count(), estimated_work, witness: output.witness,
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
