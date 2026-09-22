//! Native CALL kernels over fallible compressed rows. No flat adjacency is
//! constructed, including for a hub row. The decoded implementation remains
//! an independent differential oracle, not a hidden fallback.

use crate::execute::{KernelOutput, KernelValues};
use crate::{
    AdapterPath, ComplexityWitness, FNX_IMPLEMENTATION_REVISION, FNX_SIGNATURE_REGISTRY_VERSION,
    FnxAlgorithm, FnxBindError, FnxCallSpec, FnxCertificate, FnxExecutionError, FnxExecutionLimits,
    FnxOutput, FnxParameters, FnxResult, FnxValue, PageRankOptions, SealedGraphView,
    SealedNeighborCursor, SealedProjectionError,
};
use fgdb_crypto::{Digest, Hasher};
use fgdb_strata::tiered::sealed::SealedError;
use fgdb_types::QueryCx;
use std::convert::Infallible;
use std::mem::size_of;

#[path = "sealed_components.rs"]
mod components;
#[path = "sealed_shortest_path.rs"]
mod shortest_path;

/// Additional admission for native compressed CALLs. These account for
/// graph-size-dependent vector/string backing stores, not allocator overhead,
/// fixed metadata (including witness labels), or RSS.
/// The existing resident Strata image and projection directory are excluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FnxMemoryLimits {
    pub max_kernel_workspace_bytes: usize,
    pub max_result_bytes: usize,
}

#[derive(Debug)]
pub enum FnxSealedExecutionError {
    Bind(FnxBindError),
    Source(SealedProjectionError),
    Cancelled(Box<dyn std::error::Error + Send + Sync>),
    Execution(FnxExecutionError<Infallible>),
    /// No decoded fallback, fabricated incoming rows, or silent graph rewrite.
    UnsupportedAlgorithm(FnxAlgorithm),
}

impl core::fmt::Display for FnxSealedExecutionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Bind(error) => error.fmt(f),
            Self::Source(error) => error.fmt(f),
            Self::Cancelled(error) => write!(f, "Prism compressed call cancelled: {error}"),
            Self::Execution(error) => error.fmt(f),
            Self::UnsupportedAlgorithm(algorithm) => {
                write!(
                    f,
                    "Prism compressed cursor kernel is unavailable for {algorithm:?}"
                )
            }
        }
    }
}
impl std::error::Error for FnxSealedExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Bind(error) => Some(error),
            Self::Source(error) => Some(error),
            Self::Cancelled(error) => Some(error.as_ref()),
            Self::Execution(error) => Some(error),
            Self::UnsupportedAlgorithm(_) => None,
        }
    }
}
impl From<FnxExecutionError<Infallible>> for FnxSealedExecutionError {
    fn from(error: FnxExecutionError<Infallible>) -> Self {
        Self::Execution(error)
    }
}
impl From<SealedProjectionError> for FnxSealedExecutionError {
    fn from(error: SealedProjectionError) -> Self {
        match error {
            SealedProjectionError::Read(SealedError::Interrupted(error)) => Self::Cancelled(error),
            other => Self::Source(other),
        }
    }
}

type Error = FnxSealedExecutionError;
type Result<T> = std::result::Result<T, Error>;
type ExecutionError = FnxExecutionError<Infallible>;

fn checkpoint(cx: &QueryCx) -> Result<()> {
    cx.checkpoint().map_err(|error| Error::Cancelled(error))
}
fn add(left: usize, right: usize) -> Result<usize> {
    left.checked_add(right)
        .ok_or_else(|| ExecutionError::SizeOverflow.into())
}
fn mul(left: usize, right: usize) -> Result<usize> {
    left.checked_mul(right)
        .ok_or_else(|| ExecutionError::SizeOverflow.into())
}
fn admit(resource: &'static str, requested: usize, limit: usize) -> Result<()> {
    crate::execute::admit::<Infallible>(resource, requested, limit).map_err(Into::into)
}
fn reserve<T>(length: usize) -> Result<Vec<T>> {
    crate::execute::reserve::<T, Infallible>(length).map_err(Into::into)
}

// The private row protocol is deliberately fallible and has no &[usize]
// promise. Production uses Strata; tests can run the identical cursor kernels
// over independently built decoded projections without forging a Strata anchor.
trait Cursor {
    fn next(&mut self) -> Result<Option<(usize, f64)>>;
}
trait Rows {
    type Cursor<'a>: Cursor
    where
        Self: 'a;
    fn node_count(&self) -> usize;
    fn degree(&self, source: usize) -> Option<usize>;
    fn open(&self, source: usize) -> Result<Self::Cursor<'_>>;
}
struct SealedRows<'a> {
    cx: &'a QueryCx,
    graph: &'a SealedGraphView,
}
struct SealedRow<'a> {
    cx: &'a QueryCx,
    row: SealedNeighborCursor<'a>,
}
impl Cursor for SealedRow<'_> {
    fn next(&mut self) -> Result<Option<(usize, f64)>> {
        self.row.next(self.cx).map_err(Into::into)
    }
}
impl Rows for SealedRows<'_> {
    type Cursor<'a>
        = SealedRow<'a>
    where
        Self: 'a;
    fn node_count(&self) -> usize {
        self.graph.node_count()
    }
    fn degree(&self, source: usize) -> Option<usize> {
        self.graph.degree(source)
    }
    fn open(&self, source: usize) -> Result<Self::Cursor<'_>> {
        Ok(SealedRow {
            cx: self.cx,
            row: self.graph.neighbor_cursor(self.cx, source)?,
        })
    }
}

/// Result admission includes outer rows, typed values, column String headers
/// and alias bytes. BFS/Dijkstra charge newly reached rows BEFORE enqueueing;
/// unreachable vertices do not consume the result allowance.
struct ResultAdmission {
    row_bytes: usize,
    column_bytes: usize,
    max_rows: usize,
    max_bytes: usize,
}
impl ResultAdmission {
    fn new(
        call: &FnxCallSpec,
        limits: FnxExecutionLimits,
        memory: FnxMemoryLimits,
    ) -> Result<Self> {
        let mut column_bytes = mul(call.outputs().len(), size_of::<String>())?;
        for column in call.outputs() {
            column_bytes = add(column_bytes, column.name.len())?;
        }
        let row_bytes = add(
            size_of::<Vec<FnxValue>>(),
            mul(call.outputs().len(), size_of::<FnxValue>())?,
        )?;
        let admission = Self {
            row_bytes,
            column_bytes,
            max_rows: limits.max_result_rows,
            max_bytes: memory.max_result_bytes,
        };
        admission.rows(0)?;
        Ok(admission)
    }
    fn rows(&self, count: usize) -> Result<()> {
        admit("result rows", count, self.max_rows)?;
        admit(
            "result bytes",
            add(self.column_bytes, mul(count, self.row_bytes)?)?,
            self.max_bytes,
        )
    }
}

// H is the retained-incidence bound, NOT the reduced edge count. Each row
// lookup and each full-width endpoint binary search participates in this
// conservative admission model. This is not an observed CPU/deadline counter.
fn pass_work(n: usize, retained: usize) -> Result<usize> {
    if n == 0 {
        return Ok(0);
    }
    let row_search = (usize::BITS - retained.leading_zeros()) as usize;
    let endpoint_search = (usize::BITS - n.leading_zeros()) as usize;
    add(
        mul(n, add(6, row_search)?)?,
        mul(retained, add(1, endpoint_search)?)?,
    )
}

fn pagerank(
    graph: &impl Rows,
    options: PageRankOptions,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<KernelOutput> {
    let n = graph.node_count();
    let mut witness = ComplexityWitness {
        algorithm: "pagerank_power_iteration".to_owned(),
        complexity_claim: "O(k * (|V| log(1+H) + H log(1+|V|))) compressed row visits".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 0,
    };
    if n == 0 {
        return Ok(KernelOutput {
            values: KernelValues::Scores(Vec::new()),
            row_count: 0,
            witness,
        });
    }
    let mut sums = reserve(n)?;
    let mut ranks = reserve(n)?;
    let mut next = reserve(n)?;
    let population = n as f64;
    for source in 0..n {
        checkpoint()?;
        let mut sum = 0.0;
        if options.weighted() {
            let mut row = graph.open(source)?;
            while let Some((_, weight)) = row.next()? {
                checkpoint()?;
                if weight < 0.0 {
                    return Err(ExecutionError::NegativeWeight.into());
                }
                sum += weight;
                if !sum.is_finite() {
                    return Err(ExecutionError::NonFiniteWeightSum.into());
                }
            }
        } else {
            sum = graph
                .degree(source)
                .ok_or(ExecutionError::InvalidUpstreamResult)? as f64;
        }
        sums.push(sum);
        ranks.push(1.0 / population);
        next.push(0.0);
    }
    let base = (1.0 - options.alpha()) / population;
    for _ in 0..options.max_iter() {
        let mut dangling_mass = 0.0;
        for source in 0..n {
            checkpoint()?;
            if sums[source] == 0.0 {
                dangling_mass += ranks[source];
            }
        }
        let initial = base + options.alpha() * dangling_mass / population;
        for value in &mut next {
            checkpoint()?;
            *value = initial;
        }
        for source in 0..n {
            checkpoint()?;
            let push = options.alpha() * ranks[source];
            let sum = sums[source];
            let mut row = graph.open(source)?;
            while let Some((target, weight)) = row.next()? {
                checkpoint()?;
                // Match the pinned fnx scalar evaluation order, including
                // division before multiplication and ordered dangling mass.
                let share = if options.weighted() {
                    if sum > 0.0 { weight / sum } else { weight }
                } else if sum > 0.0 {
                    1.0 / sum
                } else {
                    0.0
                };
                let value = next
                    .get_mut(target)
                    .ok_or(ExecutionError::InvalidUpstreamResult)?;
                *value += push * share;
                witness.edges_scanned = add(witness.edges_scanned, 1)?;
            }
        }
        let mut delta = 0.0;
        for index in 0..n {
            checkpoint()?;
            if !next[index].is_finite() || next[index] < 0.0 {
                return Err(ExecutionError::InvalidNumericResult.into());
            }
            delta += (next[index] - ranks[index]).abs();
        }
        std::mem::swap(&mut ranks, &mut next);
        witness.nodes_touched = add(witness.nodes_touched, n)?;
        if delta < population * options.tolerance() {
            return Ok(KernelOutput {
                values: KernelValues::Scores(ranks),
                row_count: n,
                witness,
            });
        }
    }
    Err(ExecutionError::NotConverged {
        max_iterations: options.max_iter(),
        witness,
    }
    .into())
}

fn bfs(
    graph: &impl Rows,
    source: usize,
    cutoff: Option<usize>,
    admission: &ResultAdmission,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<KernelOutput> {
    admission.rows(1)?;
    let n = graph.node_count();
    let mut distances = reserve(n)?;
    let mut queue = reserve(n)?;
    for _ in 0..n {
        checkpoint()?;
        distances.push(None);
    }
    *distances
        .get_mut(source)
        .ok_or(ExecutionError::InvalidUpstreamResult)? = Some(0usize);
    queue.push(source);
    let mut head = 0usize;
    let mut witness = ComplexityWitness {
        algorithm: "single_source_shortest_path_length_bfs".to_owned(),
        complexity_claim: "O(|V| log(1+H) + H log(1+|V|)) compressed row visits".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 1,
    };
    while head < queue.len() {
        checkpoint()?;
        let vertex = queue[head];
        head += 1;
        witness.nodes_touched = add(witness.nodes_touched, 1)?;
        let depth = distances[vertex].ok_or(ExecutionError::InvalidUpstreamResult)?;
        if cutoff.is_some_and(|cutoff| depth >= cutoff) {
            continue;
        }
        let next_depth = add(depth, 1)?;
        let mut row = graph.open(vertex)?;
        while let Some((neighbor, _)) = row.next()? {
            checkpoint()?;
            witness.edges_scanned = add(witness.edges_scanned, 1)?;
            let distance = distances
                .get_mut(neighbor)
                .ok_or(ExecutionError::InvalidUpstreamResult)?;
            if distance.is_none() {
                admission.rows(add(queue.len(), 1)?)?;
                *distance = Some(next_depth);
                queue.push(neighbor);
                witness.queue_peak = witness.queue_peak.max(queue.len() - head);
            }
        }
    }
    Ok(KernelOutput {
        values: KernelValues::Distances(distances),
        row_count: queue.len(),
        witness,
    })
}

impl SealedGraphView {
    /// Bind the same registered CALL/YIELD language, then execute without a
    /// decoded projection. The trusted host must first supply admitted vertices.
    pub fn call_fnx(
        &self,
        cx: &QueryCx,
        text: &str,
        parameters: &FnxParameters,
        limits: FnxExecutionLimits,
        memory: FnxMemoryLimits,
    ) -> Result<FnxResult> {
        let call = FnxCallSpec::bind(text, parameters).map_err(Error::Bind)?;
        call.execute_sealed(cx, self, limits, memory)
    }
}

impl FnxCallSpec {
    pub fn supports_sealed_execution(&self) -> bool {
        matches!(
            self.algorithm(),
            FnxAlgorithm::PageRank(_)
                | FnxAlgorithm::SingleSourceShortestPathLength { .. }
                | FnxAlgorithm::SingleSourceDijkstraPathLength(_)
                | FnxAlgorithm::WeaklyConnectedComponents
                | FnxAlgorithm::StronglyConnectedComponents
        )
    }

    /// Execute PageRank, outgoing hop/weighted distances, or directed weak/strong
    /// components directly from authenticated compressed rows. Every graph pass
    /// and row pull is fallible; component labels and sources are stable VIds.
    /// Unsupported procedures refuse; they never allocate decoded adjacency.
    /// This synchronous API does not claim async scheduling or disk spill.
    pub fn execute_sealed(
        &self,
        cx: &QueryCx,
        graph: &SealedGraphView,
        limits: FnxExecutionLimits,
        memory: FnxMemoryLimits,
    ) -> Result<FnxResult> {
        checkpoint(cx)?;
        if !self.supports_sealed_execution() {
            return Err(Error::UnsupportedAlgorithm(self.algorithm()));
        }
        let n = graph.node_count();
        let admission = ResultAdmission::new(self, limits, memory)?;
        let pass = pass_work(n, graph.scan_incidence_bound())?;
        let (kernel, estimated_work, workspace, source) = match self.algorithm() {
            FnxAlgorithm::PageRank(options) => {
                admit("iterations", options.max_iter(), limits.max_iterations)?;
                admission.rows(n)?;
                (
                    "fgdb-prism/sealed-pagerank-v1",
                    mul(pass, add(options.max_iter(), 1)?)?,
                    mul(n, 3 * size_of::<f64>())?,
                    None,
                )
            }
            FnxAlgorithm::SingleSourceShortestPathLength { source, .. } => {
                let ordinal = graph
                    .vertex_ordinal(source)
                    .ok_or(ExecutionError::UnknownSource(source))?;
                admission.rows(1)?;
                (
                    "fgdb-prism/sealed-bfs-v1",
                    pass,
                    mul(n, size_of::<usize>() + size_of::<Option<usize>>())?,
                    Some(ordinal),
                )
            }
            FnxAlgorithm::SingleSourceDijkstraPathLength(options) => {
                let ordinal = graph
                    .vertex_ordinal(options.source())
                    .ok_or(ExecutionError::UnknownSource(options.source()))?;
                admission.rows(1)?;
                (
                    "fgdb-prism/sealed-dijkstra-indexed-heap-v1",
                    shortest_path::work(n, graph.edge_count(), pass)?,
                    shortest_path::workspace(n)?,
                    Some(ordinal),
                )
            }
            algorithm @ (FnxAlgorithm::WeaklyConnectedComponents
            | FnxAlgorithm::StronglyConnectedComponents) => {
                admission.rows(n)?;
                let strong = matches!(algorithm, FnxAlgorithm::StronglyConnectedComponents);
                let kernel = if strong {
                    "fgdb-prism/sealed-tarjan-v1"
                } else {
                    "fgdb-prism/sealed-union-find-v1"
                };
                (
                    kernel,
                    components::work(n, graph.edge_count(), pass, strong)?,
                    components::workspace(n, strong)?,
                    None,
                )
            }
            other => return Err(Error::UnsupportedAlgorithm(other)),
        };
        admit("estimated work", estimated_work, limits.max_estimated_work)?;
        admit(
            "kernel workspace bytes",
            workspace,
            memory.max_kernel_workspace_bytes,
        )?;
        let rows = SealedRows { cx, graph };
        let mut control = || checkpoint(cx);
        let output = match self.algorithm() {
            FnxAlgorithm::PageRank(options) => pagerank(&rows, options, &mut control)?,
            FnxAlgorithm::SingleSourceShortestPathLength { cutoff, .. } => bfs(
                &rows,
                source.ok_or(ExecutionError::InvalidUpstreamResult)?,
                cutoff,
                &admission,
                &mut control,
            )?,
            FnxAlgorithm::SingleSourceDijkstraPathLength(options) => shortest_path::run(
                &rows,
                source.ok_or(ExecutionError::InvalidUpstreamResult)?,
                options,
                &admission,
                &mut control,
            )?,
            FnxAlgorithm::WeaklyConnectedComponents => {
                components::weak(&rows, &admission, &mut control)?
            }
            FnxAlgorithm::StronglyConnectedComponents => {
                components::strong(&rows, &admission, &mut control)?
            }
            other => return Err(Error::UnsupportedAlgorithm(other)),
        };
        finish(
            self,
            graph,
            output,
            kernel,
            estimated_work,
            workspace,
            &admission,
            &mut control,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn finish(
    call: &FnxCallSpec,
    graph: &SealedGraphView,
    output: KernelOutput,
    kernel: &'static str,
    estimated_work: usize,
    workspace: usize,
    admission: &ResultAdmission,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<FnxResult> {
    checkpoint()?;
    admission.rows(output.row_count)?;
    let mut rows = reserve(output.row_count)?;
    let mut result_hash = Hasher::new();
    result_hash.update(b"fgdb:prism:result-rows:v3");
    result_hash.update(&(output.row_count as u128).to_le_bytes());
    result_hash.update(&call.digest().0);
    for index in 0..graph.node_count() {
        checkpoint()?;
        let value = match &output.values {
            KernelValues::Scores(values) => {
                let value = *values
                    .get(index)
                    .ok_or(ExecutionError::InvalidUpstreamResult)?;
                if !value.is_finite() || value < 0.0 {
                    return Err(ExecutionError::InvalidNumericResult.into());
                }
                FnxValue::Score(value)
            }
            KernelValues::Distances(values) => {
                let Some(distance) = *values
                    .get(index)
                    .ok_or(ExecutionError::InvalidUpstreamResult)?
                else {
                    continue;
                };
                FnxValue::Integer(
                    u64::try_from(distance).map_err(|_| ExecutionError::SizeOverflow)?,
                )
            }
            KernelValues::Components(values) => {
                let label = *values
                    .get(index)
                    .ok_or(ExecutionError::InvalidUpstreamResult)?;
                FnxValue::Vertex(
                    graph
                        .vertex_id(label)
                        .ok_or(ExecutionError::InvalidUpstreamResult)?,
                )
            }
            KernelValues::WeightedDistances(values) => {
                let Some(distance) = *values
                    .get(index)
                    .ok_or(ExecutionError::InvalidUpstreamResult)?
                else {
                    continue;
                };
                if !distance.is_finite() || distance < 0.0 {
                    return Err(ExecutionError::InvalidNumericResult.into());
                }
                FnxValue::Float(distance)
            }
            _ => return Err(ExecutionError::InvalidUpstreamResult.into()),
        };
        let vertex = graph
            .vertex_id(index)
            .ok_or(ExecutionError::InvalidUpstreamResult)?;
        let mut row = reserve(call.outputs().len())?;
        for column in call.outputs() {
            checkpoint()?;
            let value = match (column.field, value) {
                (FnxOutput::Vertex, _) => FnxValue::Vertex(vertex),
                (FnxOutput::Score, FnxValue::Score(value)) => FnxValue::Score(value),
                (FnxOutput::Distance, FnxValue::Integer(value)) => FnxValue::Integer(value),
                (FnxOutput::Distance, FnxValue::Float(value)) => FnxValue::Float(value),
                (FnxOutput::Component, FnxValue::Vertex(value)) => FnxValue::Vertex(value),
                _ => return Err(ExecutionError::InvalidUpstreamResult.into()),
            };
            match value {
                FnxValue::Vertex(vertex) => {
                    result_hash.update(&[0]);
                    result_hash.update(&vertex.0.to_le_bytes());
                }
                FnxValue::Score(score) => {
                    result_hash.update(&[1]);
                    result_hash.update(&score.to_bits().to_le_bytes());
                }
                FnxValue::Integer(value) => {
                    result_hash.update(&[2]);
                    result_hash.update(&value.to_le_bytes());
                }
                FnxValue::Float(value) => {
                    result_hash.update(&[3]);
                    result_hash.update(&value.to_bits().to_le_bytes());
                }
            }
            row.push(value);
        }
        rows.push(row);
    }
    if rows.len() != output.row_count {
        return Err(ExecutionError::InvalidUpstreamResult.into());
    }
    let mut columns = reserve(call.outputs().len())?;
    for column in call.outputs() {
        checkpoint()?;
        let mut name = String::new();
        name.try_reserve_exact(column.name.len())
            .map_err(|_| ExecutionError::AllocationFailed)?;
        name.push_str(&column.name);
        columns.push(name);
    }
    let result_digest = result_hash.finalize();
    let kernel_source_digest = source_digest();
    let numeric_profile = call.numeric_profile();
    let adapter = AdapterPath::CompressedCursor;
    let witness = output.witness;
    let mut hash = Hasher::new();
    hash.update(b"fgdb:prism:call-certificate:v3");
    hash.update(&FNX_SIGNATURE_REGISTRY_VERSION.to_le_bytes());
    hash.update(FNX_IMPLEMENTATION_REVISION.as_bytes());
    hash_text(&mut hash, kernel);
    hash.update(&kernel_source_digest.0);
    hash_text(&mut hash, numeric_profile);
    hash.update(&graph.digest().0);
    hash.update(&call.digest().0);
    hash.update(&result_digest.0);
    hash_text(&mut hash, adapter.as_str());
    for number in [
        graph.node_count(),
        graph.edge_count(),
        graph.input_edge_count(),
        estimated_work,
        workspace,
        witness.nodes_touched,
        witness.edges_scanned,
        witness.queue_peak,
    ] {
        hash.update(&(number as u128).to_le_bytes());
    }
    hash_text(&mut hash, &witness.algorithm);
    hash_text(&mut hash, &witness.complexity_claim);
    checkpoint()?;
    Ok(FnxResult {
        columns,
        rows,
        certificate: FnxCertificate {
            registry_version: FNX_SIGNATURE_REGISTRY_VERSION,
            implementation_revision: FNX_IMPLEMENTATION_REVISION,
            execution_kernel: kernel,
            kernel_source_digest,
            numeric_profile,
            snapshot: graph.binding(),
            projection_digest: graph.digest(),
            call_digest: call.digest(),
            result_digest,
            adapter,
            vertices: graph.node_count(),
            edges: graph.edge_count(),
            input_edges: graph.input_edge_count(),
            estimated_work,
            kernel_workspace_bytes: workspace,
            witness,
            digest: hash.finalize(),
        },
    })
}
fn hash_text(hash: &mut Hasher, text: &str) {
    hash.update(&(text.len() as u128).to_le_bytes());
    hash.update(text.as_bytes());
}
fn source_digest() -> Digest {
    static DIGEST: std::sync::OnceLock<Digest> = std::sync::OnceLock::new();
    *DIGEST.get_or_init(|| {
        let mut hash = Hasher::new();
        hash.update(b"fgdb:prism:sealed-kernel-source:v1");
        for source in [
            include_str!("sealed_execute.rs"),
            include_str!("sealed_components.rs"),
            include_str!("sealed_shortest_path.rs"),
            include_str!("shortest_path.rs"),
            include_str!("sealed.rs"),
            include_str!("call.rs"),
            include_str!("input.rs"),
            include_str!("projection.rs"),
        ] {
            hash_text(&mut hash, source);
        }
        hash.finalize()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Directedness, GraphView, PROJECTED_WEIGHT_ATTRIBUTE, ParallelEdgePolicy, ProjectionEdge,
        ProjectionLimits, ProjectionSpec, SelfLoopPolicy, SnapshotBinding, SnapshotGraphView,
    };
    use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};

    struct DecodedRows<'a>(&'a SnapshotGraphView);
    struct DecodedRow<'a> {
        row: std::iter::Zip<std::slice::Iter<'a, usize>, std::slice::Iter<'a, f64>>,
    }
    impl Cursor for DecodedRow<'_> {
        fn next(&mut self) -> Result<Option<(usize, f64)>> {
            Ok(self.row.next().map(|(&target, &weight)| (target, weight)))
        }
    }
    impl Rows for DecodedRows<'_> {
        type Cursor<'a>
            = DecodedRow<'a>
        where
            Self: 'a;
        fn node_count(&self) -> usize {
            self.0.node_count()
        }
        fn degree(&self, source: usize) -> Option<usize> {
            self.0.neighbors_indices(source).map(<[usize]>::len)
        }
        fn open(&self, source: usize) -> Result<Self::Cursor<'_>> {
            let (targets, weights) = self
                .0
                .projected_row(source)
                .ok_or(ExecutionError::InvalidUpstreamResult)?;
            Ok(DecodedRow {
                row: targets.iter().zip(weights),
            })
        }
    }
    fn projection(mask: u16) -> SnapshotGraphView {
        let vertices = [VId(0), VId(17), VId(u128::MAX)];
        let edges: Vec<_> = (0..9)
            .filter(|bit| mask & (1 << bit) != 0)
            .map(|bit| ProjectionEdge {
                eid: EId(bit as u128),
                source: vertices[bit / 3],
                target: vertices[bit % 3],
                weight: [0.0, 0.1, 0.3, 1.0, 7.0][bit % 5],
            })
            .collect();
        SnapshotGraphView::build(
            SnapshotBinding {
                root: ObjectId([2; 32]),
                as_of: CommitSeq(3),
            },
            &vertices,
            &edges,
            ProjectionSpec {
                directedness: Directedness::Directed,
                parallel_edges: ParallelEdgePolicy::Sum,
                self_loops: SelfLoopPolicy::Keep,
            },
            ProjectionLimits {
                max_vertices: 3,
                max_input_edges: 9,
                max_adjacency_entries: 9,
                max_workspace_bytes: 1 << 20,
            },
        )
        .unwrap()
    }
    fn limits() -> FnxExecutionLimits {
        FnxExecutionLimits {
            max_iterations: 1000,
            max_result_rows: 3,
            max_estimated_work: 1 << 24,
        }
    }
    fn memory() -> FnxMemoryLimits {
        FnxMemoryLimits {
            max_kernel_workspace_bytes: 1 << 20,
            max_result_bytes: 1 << 20,
        }
    }
    fn bfs_call(source: VId, cutoff: Option<usize>) -> FnxCallSpec {
        let cutoff = cutoff.map_or_else(|| "NULL".to_owned(), |value| value.to_string());
        FnxCallSpec::bind(
            &format!(
                "CALL fnx.single_source_shortest_path_length({}, {cutoff}) YIELD vertex,distance",
                source.0,
            ),
            &FnxParameters::new(),
        )
        .unwrap()
    }

    #[test]
    fn cursor_pagerank_is_bit_identical_to_pinned_fnx_on_every_three_node_topology() {
        for mask in 0..512 {
            let graph = projection(mask);
            for weighted in [false, true] {
                let options = PageRankOptions::new(0.85, 1000, 1e-12, weighted).unwrap();
                let oracle = fnx_algorithms::pagerank_with_weight(
                    &graph,
                    options.alpha(),
                    options.max_iter(),
                    options.tolerance(),
                    weighted.then_some(PROJECTED_WEIGHT_ATTRIBUTE),
                );
                assert!(oracle.converged);
                let output = pagerank(&DecodedRows(&graph), options, &mut || Ok(())).unwrap();
                let KernelValues::Scores(scores) = output.values else {
                    panic!("scores");
                };
                for expected in oracle.scores {
                    let ordinal = graph.get_node_index(&expected.node).unwrap();
                    assert_eq!(
                        scores[ordinal].to_bits(),
                        expected.score.to_bits(),
                        "mask={mask} weighted={weighted}"
                    );
                }
                assert_eq!(output.witness.nodes_touched, oracle.witness.nodes_touched);
                assert_eq!(output.witness.edges_scanned, oracle.witness.edges_scanned);
            }
        }
    }

    #[test]
    fn cursor_bfs_matches_registered_decoded_calls_including_cutoffs_and_full_width_ids() {
        for mask in 0..512 {
            let graph = projection(mask);
            for source in 0..3 {
                for cutoff in [None, Some(0), Some(1), Some(2), Some(1000)] {
                    let call = bfs_call(graph.vertex_id(source).unwrap(), cutoff);
                    let admission = ResultAdmission::new(&call, limits(), memory()).unwrap();
                    let output = bfs(
                        &DecodedRows(&graph),
                        source,
                        cutoff,
                        &admission,
                        &mut || Ok(()),
                    )
                    .unwrap();
                    let expected = call
                        .execute(&graph, limits(), || Ok::<(), Infallible>(()))
                        .unwrap();
                    let KernelValues::Distances(distances) = output.values else {
                        panic!("distances");
                    };
                    let rows: Vec<_> = distances
                        .iter()
                        .enumerate()
                        .filter_map(|(index, distance)| {
                            distance.map(|distance| {
                                vec![
                                    FnxValue::Vertex(graph.vertex_id(index).unwrap()),
                                    FnxValue::Integer(u64::try_from(distance).unwrap()),
                                ]
                            })
                        })
                        .collect();
                    assert_eq!(
                        rows, expected.rows,
                        "mask={mask} source={source} cutoff={cutoff:?}"
                    );
                    assert_eq!(output.row_count, rows.len());
                    assert_eq!(
                        output.witness.nodes_touched,
                        expected.certificate.witness.nodes_touched
                    );
                    assert_eq!(
                        output.witness.edges_scanned,
                        expected.certificate.witness.edges_scanned
                    );
                    assert_eq!(
                        output.witness.queue_peak,
                        expected.certificate.witness.queue_peak
                    );
                }
            }
        }
    }

    #[test]
    fn every_kernel_checkpoint_refuses_without_a_successful_prefix() {
        let graph = projection(2);
        let call = bfs_call(VId(0), None);
        let admission = ResultAdmission::new(&call, limits(), memory()).unwrap();
        let options = PageRankOptions::new(0.85, 1000, 1e-12, true).unwrap();
        for rank in [false, true] {
            let run = |control: &mut dyn FnMut() -> Result<()>| {
                if rank {
                    pagerank(&DecodedRows(&graph), options, &mut || control())
                } else {
                    bfs(&DecodedRows(&graph), 0, None, &admission, &mut || control())
                }
            };
            let mut count = 0;
            run(&mut || {
                count += 1;
                Ok(())
            })
            .unwrap();
            assert!(count > 3);
            for stop in 1..=count {
                let mut seen = 0;
                let result = run(&mut || {
                    seen += 1;
                    if seen == stop {
                        Err(Error::Cancelled(std::io::Error::other("test stop").into()))
                    } else {
                        Ok(())
                    }
                });
                assert!(matches!(result, Err(Error::Cancelled(_))));
                assert_eq!(seen, stop);
            }
        }
    }

    #[test]
    fn result_bytes_and_rows_admit_reachability_not_the_vertex_population() {
        let isolated = projection(0);
        let connected = projection(511);
        let call = bfs_call(VId(0), None);
        let mut admission = ResultAdmission::new(&call, limits(), memory()).unwrap();
        admission.max_rows = 1;
        admission.max_bytes = admission.column_bytes + admission.row_bytes;
        assert_eq!(
            bfs(&DecodedRows(&isolated), 0, None, &admission, &mut || Ok(()))
                .unwrap()
                .row_count,
            1
        );
        assert!(matches!(
            bfs(
                &DecodedRows(&connected),
                0,
                None,
                &admission,
                &mut || Ok(())
            ),
            Err(Error::Execution(ExecutionError::LimitExceeded {
                resource: "result rows",
                ..
            }))
        ));
        admission.max_rows = 3;
        assert!(matches!(
            bfs(
                &DecodedRows(&connected),
                0,
                None,
                &admission,
                &mut || Ok(())
            ),
            Err(Error::Execution(ExecutionError::LimitExceeded {
                resource: "result bytes",
                ..
            }))
        ));
        admission.max_bytes -= 1;
        assert!(admission.rows(1).is_err());
    }

    #[test]
    fn numeric_refusals_and_nonconvergence_are_not_successful_scores() {
        let graph = projection(2);
        let options = PageRankOptions::new(0.85, 1, 1e-30, true).unwrap();
        assert!(matches!(
            pagerank(&DecodedRows(&graph), options, &mut || Ok(())),
            Err(Error::Execution(ExecutionError::NotConverged { .. }))
        ));
        let vertices = [VId(0), VId(1)];
        let graph = SnapshotGraphView::build(
            SnapshotBinding {
                root: ObjectId([0; 32]),
                as_of: CommitSeq(1),
            },
            &vertices,
            &[ProjectionEdge {
                eid: EId(1),
                source: VId(0),
                target: VId(1),
                weight: -1.0,
            }],
            ProjectionSpec {
                directedness: Directedness::Directed,
                parallel_edges: ParallelEdgePolicy::Sum,
                self_loops: SelfLoopPolicy::Keep,
            },
            ProjectionLimits {
                max_vertices: 2,
                max_input_edges: 1,
                max_adjacency_entries: 1,
                max_workspace_bytes: 1 << 20,
            },
        )
        .unwrap();
        assert!(matches!(
            pagerank(
                &DecodedRows(&graph),
                PageRankOptions::default(),
                &mut || Ok(())
            ),
            Err(Error::Execution(ExecutionError::NegativeWeight))
        ));
    }

    #[test]
    fn retained_history_and_checked_arithmetic_participate_in_admission() {
        assert_eq!(pass_work(0, usize::MAX).unwrap(), 0);
        assert!(pass_work(3, 1000).unwrap() > pass_work(3, 3).unwrap());
        assert!(matches!(
            pass_work(usize::MAX, usize::MAX),
            Err(Error::Execution(ExecutionError::SizeOverflow))
        ));
        assert!(matches!(
            mul(usize::MAX, 2),
            Err(Error::Execution(ExecutionError::SizeOverflow))
        ));
    }

    fn component_call(strong: bool) -> FnxCallSpec {
        let name = if strong {
            "strongly_connected_components"
        } else {
            "weakly_connected_components"
        };
        FnxCallSpec::bind(
            &format!("CALL fnx.{name}() YIELD vertex,component"),
            &FnxParameters::new(),
        )
        .unwrap()
    }
    fn run_components(
        graph: &impl Rows,
        strong: bool,
        admission: &ResultAdmission,
        checkpoint: &mut impl FnMut() -> Result<()>,
    ) -> Result<KernelOutput> {
        if strong {
            components::strong(graph, admission, checkpoint)
        } else {
            components::weak(graph, admission, checkpoint)
        }
    }

    #[test]
    fn cursor_components_match_decoded_calls_on_all_three_node_topologies() {
        for strong in [false, true] {
            let call = component_call(strong);
            assert!(call.supports_sealed_execution());
            let admission = ResultAdmission::new(&call, limits(), memory()).unwrap();
            for mask in 0..512 {
                let graph = projection(mask);
                let output =
                    run_components(&DecodedRows(&graph), strong, &admission, &mut || Ok(()))
                        .unwrap();
                let expected = call
                    .execute(&graph, limits(), || Ok::<(), Infallible>(()))
                    .unwrap();
                assert_eq!(output.row_count, 3);
                // Each directed edge and vertex is visited exactly once, even
                // for WCC; no synthetic reverse adjacency is scanned.
                assert_eq!(output.witness.nodes_touched, 3);
                assert_eq!(output.witness.edges_scanned, graph.edge_count());
                let KernelValues::Components(labels) = output.values else {
                    panic!("components");
                };
                let rows: Vec<_> = labels
                    .iter()
                    .enumerate()
                    .map(|(vertex, &label)| {
                        vec![
                            FnxValue::Vertex(graph.vertex_id(vertex).unwrap()),
                            FnxValue::Vertex(graph.vertex_id(label).unwrap()),
                        ]
                    })
                    .collect();
                assert_eq!(rows, expected.rows, "mask={mask} strong={strong}");
            }
        }
    }

    #[test]
    fn every_component_checkpoint_cancels_without_publishing_labels() {
        let graph = projection(511);
        for strong in [false, true] {
            let admission =
                ResultAdmission::new(&component_call(strong), limits(), memory()).unwrap();
            let mut count = 0;
            run_components(&DecodedRows(&graph), strong, &admission, &mut || {
                count += 1;
                Ok(())
            })
            .unwrap();
            assert!(count > 20);
            for stop in 1..=count {
                let mut seen = 0;
                let result = run_components(&DecodedRows(&graph), strong, &admission, &mut || {
                    seen += 1;
                    if seen == stop {
                        Err(Error::Cancelled(
                            std::io::Error::other("component stop").into(),
                        ))
                    } else {
                        Ok(())
                    }
                });
                assert!(matches!(result, Err(Error::Cancelled(_))));
                assert_eq!(seen, stop);
            }
        }
    }

    struct PathRows {
        n: usize,
        cycle: bool,
    }
    struct PathRow(Option<(usize, f64)>);
    impl Cursor for PathRow {
        fn next(&mut self) -> Result<Option<(usize, f64)>> {
            Ok(self.0.take())
        }
    }
    impl Rows for PathRows {
        type Cursor<'a>
            = PathRow
        where
            Self: 'a;
        fn node_count(&self) -> usize {
            self.n
        }
        fn degree(&self, source: usize) -> Option<usize> {
            if source >= self.n {
                return None;
            }
            Some(usize::from(source + 1 < self.n || self.cycle))
        }
        fn open(&self, source: usize) -> Result<PathRow> {
            if source >= self.n {
                return Err(ExecutionError::InvalidUpstreamResult.into());
            }
            Ok(PathRow(if source + 1 < self.n {
                Some((source + 1, 1.0))
            } else if self.cycle {
                Some((0, 1.0))
            } else {
                None
            }))
        }
    }

    #[test]
    fn deep_component_dfs_is_iterative_and_does_not_materialize_rows() {
        let n = 20_000;
        for strong in [false, true] {
            let admission = ResultAdmission::new(
                &component_call(strong),
                FnxExecutionLimits {
                    max_result_rows: n,
                    ..limits()
                },
                FnxMemoryLimits {
                    max_result_bytes: usize::MAX,
                    ..memory()
                },
            )
            .unwrap();
            for cycle in [false, true] {
                let output =
                    run_components(&PathRows { n, cycle }, strong, &admission, &mut || Ok(()))
                        .unwrap();
                assert_eq!(output.witness.edges_scanned, if cycle { n } else { n - 1 });
                assert_eq!(output.witness.queue_peak, if strong { n } else { 0 });
                let KernelValues::Components(labels) = output.values else {
                    panic!("components");
                };
                for (vertex, label) in labels.into_iter().enumerate() {
                    assert_eq!(label, if strong && !cycle { vertex } else { 0 });
                }
            }
        }
    }

    fn fault_step(calls: &std::cell::Cell<usize>, fail_at: usize) -> Result<()> {
        let next = calls.get() + 1;
        calls.set(next);
        if next == fail_at {
            Err(Error::Source(SealedProjectionError::UnknownOrdinal(
                usize::MAX,
            )))
        } else {
            Ok(())
        }
    }
    struct FaultRows<'a> {
        graph: &'a SnapshotGraphView,
        calls: std::cell::Cell<usize>,
        fail_at: usize,
    }
    struct FaultRow<'a> {
        row: DecodedRow<'a>,
        calls: &'a std::cell::Cell<usize>,
        fail_at: usize,
    }
    impl Cursor for FaultRow<'_> {
        fn next(&mut self) -> Result<Option<(usize, f64)>> {
            fault_step(self.calls, self.fail_at)?;
            self.row.next()
        }
    }
    impl Rows for FaultRows<'_> {
        type Cursor<'a>
            = FaultRow<'a>
        where
            Self: 'a;
        fn node_count(&self) -> usize {
            self.graph.node_count()
        }
        fn degree(&self, source: usize) -> Option<usize> {
            self.graph.neighbors_indices(source).map(<[usize]>::len)
        }
        fn open(&self, source: usize) -> Result<Self::Cursor<'_>> {
            fault_step(&self.calls, self.fail_at)?;
            let (targets, weights) = self
                .graph
                .projected_row(source)
                .ok_or(ExecutionError::InvalidUpstreamResult)?;
            Ok(FaultRow {
                row: DecodedRow {
                    row: targets.iter().zip(weights),
                },
                calls: &self.calls,
                fail_at: self.fail_at,
            })
        }
    }

    #[test]
    fn every_component_source_open_and_pull_failure_is_terminal() {
        let graph = projection(511);
        for strong in [false, true] {
            let admission =
                ResultAdmission::new(&component_call(strong), limits(), memory()).unwrap();
            let source = FaultRows {
                graph: &graph,
                calls: std::cell::Cell::new(0),
                fail_at: usize::MAX,
            };
            run_components(&source, strong, &admission, &mut || Ok(())).unwrap();
            let operations = source.calls.get();
            assert_eq!(operations, 3 + 9 + 3); // opens, edges, EOFs
            for fail_at in 1..=operations {
                let source = FaultRows {
                    graph: &graph,
                    calls: std::cell::Cell::new(0),
                    fail_at,
                };
                assert!(matches!(
                    run_components(&source, strong, &admission, &mut || Ok(())),
                    Err(Error::Source(SealedProjectionError::UnknownOrdinal(
                        usize::MAX
                    )))
                ));
                assert_eq!(
                    source.calls.get(),
                    fail_at,
                    "no cursor may resume after failure"
                );
            }
        }
    }

    #[test]
    fn component_admission_precedes_source_access_and_counts_isolates() {
        let graph = projection(0);
        for strong in [false, true] {
            let mut admission =
                ResultAdmission::new(&component_call(strong), limits(), memory()).unwrap();
            let source = FaultRows {
                graph: &graph,
                calls: std::cell::Cell::new(0),
                fail_at: 1,
            };
            admission.max_rows = 2;
            assert!(matches!(
                run_components(&source, strong, &admission, &mut || Ok(())),
                Err(Error::Execution(ExecutionError::LimitExceeded {
                    resource: "result rows",
                    ..
                }))
            ));
            admission.max_rows = 3;
            admission.max_bytes = admission.column_bytes + 3 * admission.row_bytes - 1;
            assert!(matches!(
                run_components(&source, strong, &admission, &mut || Ok(())),
                Err(Error::Execution(ExecutionError::LimitExceeded {
                    resource: "result bytes",
                    ..
                }))
            ));
            assert_eq!(source.calls.get(), 0);
            assert!(components::workspace(usize::MAX, strong).is_err());
            assert!(components::work(usize::MAX, usize::MAX, usize::MAX, strong).is_err());
            assert_eq!(components::workspace(0, strong).unwrap(), 0);
        }
        assert_eq!(
            components::workspace(3, false).unwrap(),
            6 * size_of::<usize>()
        );
    }

    #[test]
    fn empty_component_graphs_succeed_and_invalid_ordinals_refuse() {
        struct InvalidRows;
        impl Rows for InvalidRows {
            type Cursor<'a>
                = PathRow
            where
                Self: 'a;
            fn node_count(&self) -> usize {
                1
            }
            fn degree(&self, _: usize) -> Option<usize> {
                Some(1)
            }
            fn open(&self, _: usize) -> Result<PathRow> {
                Ok(PathRow(Some((usize::MAX, 1.0))))
            }
        }
        for strong in [false, true] {
            let admission =
                ResultAdmission::new(&component_call(strong), limits(), memory()).unwrap();
            let output = run_components(
                &PathRows { n: 0, cycle: false },
                strong,
                &admission,
                &mut || Ok(()),
            )
            .unwrap();
            assert_eq!(output.row_count, 0);
            let KernelValues::Components(labels) = output.values else {
                panic!("components");
            };
            assert!(labels.is_empty());
            assert!(matches!(
                run_components(&InvalidRows, strong, &admission, &mut || Ok(())),
                Err(Error::Execution(ExecutionError::InvalidUpstreamResult))
            ));
        }
    }
}
