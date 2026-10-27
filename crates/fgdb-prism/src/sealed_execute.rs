//! Native CALL kernels over fallible compressed rows. No flat adjacency is
//! constructed, including for a hub row. The decoded implementation remains
//! an independent differential oracle, not a hidden fallback.

use crate::execute::{
    KernelOutput, KernelValues, PageRankRestart, hash_bfs_decision_card,
    hash_component_decision_card, hash_result_value,
};
use crate::sealed_control::Control;
use crate::{
    AdapterPath, BfsDecisionCard, ComplexityWitness, Directedness, FNX_IMPLEMENTATION_REVISION,
    FNX_SIGNATURE_REGISTRY_VERSION, FnxAlgorithm, FnxBindError, FnxCallSpec, FnxCertificate,
    FnxExecutionError, FnxExecutionLimits, FnxGraphKind, FnxOutput, FnxParameters, FnxResult,
    FnxRowCursor, FnxValue, FoundationAlgorithm, PageRankOptions, SealedGraphView,
    SealedNeighborCursor, SealedProjectionError,
};
use fgdb_crypto::{Digest, Hasher};
use fgdb_strata::tiered::sealed::SealedError;
use fgdb_types::QueryCx;
use std::cell::RefCell;
use std::convert::Infallible;
use std::mem::size_of;

#[path = "sealed_betweenness.rs"]
mod betweenness;
#[path = "sealed_betweenness_cooperative.rs"]
mod betweenness_cooperative;
#[path = "sealed_cdlp.rs"]
mod cdlp;
#[path = "sealed_clustering.rs"]
mod clustering;
#[path = "sealed_components.rs"]
mod components;
#[path = "sealed_cooperative.rs"]
mod cooperative;
#[path = "sealed_core.rs"]
mod core_number;
#[path = "sealed_directed_clustering.rs"]
mod directed_clustering;
#[path = "sealed_bfs.rs"]
mod directional_bfs;
#[path = "sealed_distance.rs"]
mod distance;
#[path = "sealed_eigenvector.rs"]
mod eigenvector;
#[path = "sealed_hits.rs"]
mod hits;
#[path = "sealed_load.rs"]
mod load;
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
    /// No synchronous fallback when yielding execution was requested.
    UnsupportedCooperativeAlgorithm(FnxAlgorithm),
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
            Self::UnsupportedCooperativeAlgorithm(algorithm) => write!(
                f,
                "Prism cooperative compressed kernel is unavailable for {algorithm:?}"
            ),
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
            Self::UnsupportedAlgorithm(_) | Self::UnsupportedCooperativeAlgorithm(_) => None,
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

#[derive(Clone, Copy)]
enum Delivery {
    Collected,
    Retained,
}

// Delivery is selected before execution, but no completed cursor or collected
// result is exposed until all values, evidence and the final live check pass.
#[derive(Debug)]
enum Finished {
    Collected(FnxResult),
    Retained(FnxRowCursor),
}

impl Finished {
    fn collected(self) -> Result<FnxResult> {
        match self {
            Self::Collected(result) => Ok(result),
            Self::Retained(_) => Err(ExecutionError::InvalidUpstreamResult.into()),
        }
    }

    fn retained(self) -> Result<FnxRowCursor> {
        match self {
            Self::Retained(cursor) => Ok(cursor),
            Self::Collected(_) => Err(ExecutionError::InvalidUpstreamResult.into()),
        }
    }
}

fn checkpoint(cx: &Control<'_>) -> Result<()> {
    cx.checkpoint().map_err(Into::into)
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
    /// Only an admitted symmetric projection can prove that a zero outgoing
    /// degree is an isolate. Unknown/directed providers must return false.
    fn is_undirected(&self) -> bool {
        false
    }
    /// A trusted fact from complete projection admission, never a query hint.
    /// Unknown row providers keep the general weighted implementation.
    fn has_unit_weights(&self) -> bool {
        false
    }
    fn open(&self, source: usize) -> Result<Self::Cursor<'_>>;
}
struct SealedRows<'a> {
    cx: &'a Control<'a>,
    graph: &'a SealedGraphView,
}
struct SealedRow<'a> {
    cx: &'a Control<'a>,
    row: SealedNeighborCursor<'a>,
}
impl Cursor for SealedRow<'_> {
    fn next(&mut self) -> Result<Option<(usize, f64)>> {
        self.row.next_controlled(self.cx).map_err(Into::into)
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
    fn is_undirected(&self) -> bool {
        self.graph.spec().directedness == Directedness::Undirected
    }
    fn has_unit_weights(&self) -> bool {
        self.graph.has_unit_weights()
    }
    fn open(&self, source: usize) -> Result<Self::Cursor<'_>> {
        Ok(SealedRow {
            cx: self.cx,
            row: self
                .graph
                .neighbor_cursor_controlled(self.cx, source, None)?,
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
    delivery: Delivery,
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
            delivery: Delivery::Collected,
        };
        admission.rows(0)?;
        Ok(admission)
    }

    fn for_delivery(
        call: &FnxCallSpec,
        limits: FnxExecutionLimits,
        memory: FnxMemoryLimits,
        delivery: Delivery,
    ) -> Result<Self> {
        let mut admission = Self::new(call, limits, memory)?;
        admission.delivery = delivery;
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

// Incoming locators additionally search the source descriptor prefix directory.
// An undirected pass visits both faces and merges RAW incidences, not reduced
// edges. H includes excluded endpoints, relations and invisible history.
fn directional_pass_work(n: usize, retained: usize, direction: Directedness) -> Result<usize> {
    let forward = pass_work(n, retained)?;
    if n == 0 || direction == Directedness::Directed {
        return Ok(forward);
    }
    let height = (usize::BITS - retained.leading_zeros()) as usize;
    let incoming = add(forward, add(mul(retained, add(8, height)?)?, mul(n, 4)?)?)?;
    if direction == Directedness::Undirected {
        add(add(forward, incoming)?, mul(retained, 2)?)
    } else {
        Ok(incoming)
    }
}

// Sealing already authenticated and reduced every selected row. Degree
// centrality can consume that immutable directory without opening incidences.
// A simple undirected non-loop contributes two adjacency entries, and a loop
// contributes one. This exact parity/count law detects every retained loop
// without multiplying a potentially full-width count.
fn require_loop_free_degree(edges: usize, adjacency_entries: usize) -> Result<()> {
    if !adjacency_entries.is_multiple_of(2) || adjacency_entries / 2 != edges {
        return Err(ExecutionError::SelfLoopRefused.into());
    }
    Ok(())
}

fn execution_pass_work(call: &FnxCallSpec, graph: &SealedGraphView) -> Result<usize> {
    if matches!(
        call.algorithm(),
        FnxAlgorithm::Foundation(FoundationAlgorithm::DegreeCentrality)
            | FnxAlgorithm::OutDegreeCentrality
    ) {
        // One admitted degree per vertex, initialization and completion.
        add(graph.node_count(), 2)
    } else {
        directional_pass_work(
            graph.node_count(),
            graph.scan_incidence_bound(),
            graph.spec().directedness,
        )
    }
}

// Synchronous and cooperative drivers share the normalization, owned output
// and witness. They differ only in how they checkpoint/yield between degrees.
struct DegreeCentrality {
    values: Vec<f64>,
    nodes: usize,
    reciprocal: f64,
}
impl DegreeCentrality {
    fn new(nodes: usize) -> Result<Self> {
        Ok(Self {
            values: reserve(nodes)?,
            nodes,
            // The pinned fnx kernel multiplies by this reciprocal. Per-row
            // division can produce different score bits.
            reciprocal: if nodes > 1 {
                1.0 / (nodes - 1) as f64
            } else {
                1.0
            },
        })
    }

    fn push(&mut self, degree: Option<usize>) -> Result<()> {
        let degree = degree.ok_or(ExecutionError::InvalidUpstreamResult)?;
        if self.values.len() == self.nodes || degree >= self.nodes {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        self.values.push(if self.nodes == 1 {
            1.0
        } else {
            degree as f64 * self.reciprocal
        });
        Ok(())
    }

    fn push_directed(&mut self, degree: Option<usize>) -> Result<()> {
        let degree = degree.ok_or(ExecutionError::InvalidUpstreamResult)?;
        if self.values.len() == self.nodes || degree > self.nodes {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        self.values.push(if self.nodes == 1 {
            1.0
        } else {
            // The pinned directed routines divide, rather than multiply by a
            // reciprocal. Preserve their bits, including self-loop degrees.
            degree as f64 / (self.nodes - 1) as f64
        });
        Ok(())
    }

    fn finish(self) -> Result<KernelOutput> {
        self.finish_with_witness(
            "degree_centrality",
            0,
            "O(|V|) admitted degree-directory reads",
        )
    }

    fn finish_directed(self, incoming: bool, edges: usize) -> Result<KernelOutput> {
        self.finish_with_witness(
            if incoming {
                "fgdb_in_degree_centrality"
            } else {
                "fgdb_out_degree_centrality"
            },
            edges,
            if incoming {
                "O(|V| log(1+H) + H log(1+|V|)) compressed row visits; H = retained incidences"
            } else {
                "O(|V|) admitted degree-directory reads"
            },
        )
    }

    fn finish_with_witness(
        self,
        algorithm: &str,
        edges: usize,
        complexity: &str,
    ) -> Result<KernelOutput> {
        if self.values.len() != self.nodes {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        Ok(KernelOutput {
            values: KernelValues::Scores(self.values),
            row_count: self.nodes,
            witness: ComplexityWitness {
                algorithm: algorithm.to_owned(),
                complexity_claim: complexity.to_owned(),
                nodes_touched: self.nodes,
                edges_scanned: edges,
                queue_peak: 0,
            },
        })
    }
}

fn degree_centrality(
    graph: &impl Rows,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<KernelOutput> {
    checkpoint()?;
    let mut output = DegreeCentrality::new(graph.node_count())?;
    for source in 0..graph.node_count() {
        checkpoint()?;
        output.push(graph.degree(source))?;
    }
    checkpoint()?;
    output.finish()
}

fn directed_degree_centrality(
    graph: &impl Rows,
    incoming: bool,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<KernelOutput> {
    checkpoint()?;
    let n = graph.node_count();
    let mut counts = reserve(if incoming { n } else { 0 })?;
    let mut edges = 0usize;
    if incoming {
        for _ in 0..n {
            checkpoint()?;
            counts.push(0usize);
        }
        for source in 0..n {
            checkpoint()?;
            let mut row = graph.open(source)?;
            while let Some((target, _)) = row.next()? {
                checkpoint()?;
                let count = counts
                    .get_mut(target)
                    .ok_or(ExecutionError::InvalidUpstreamResult)?;
                *count = add(*count, 1)?;
                edges = add(edges, 1)?;
            }
        }
    }
    let mut output = DegreeCentrality::new(n)?;
    for vertex in 0..n {
        checkpoint()?;
        output.push_directed(if incoming {
            counts.get(vertex).copied()
        } else {
            graph.degree(vertex)
        })?;
    }
    checkpoint()?;
    output.finish_directed(incoming, edges)
}

fn pagerank(
    graph: &impl Rows,
    options: PageRankOptions,
    source: Option<usize>,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<KernelOutput> {
    let n = graph.node_count();
    if source.is_some_and(|source| source >= n) || source.is_some() != options.source().is_some() {
        return Err(ExecutionError::InvalidUpstreamResult.into());
    }
    let mut witness = ComplexityWitness {
        algorithm: options.witness_algorithm().to_owned(),
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
    for iteration in 0..options.max_iter() {
        let mut dangling_mass = 0.0;
        for source in 0..n {
            checkpoint()?;
            if sums[source] == 0.0 {
                dangling_mass += ranks[source];
            }
        }
        let initial = PageRankRestart::new(source, options.alpha(), population, dangling_mass);
        for (target, value) in next.iter_mut().enumerate() {
            checkpoint()?;
            *value = initial.at(target);
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
        if options.iteration_complete(iteration + 1, delta, population) {
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
                | FnxAlgorithm::ConnectedComponents
                | FnxAlgorithm::WeaklyConnectedComponents
                | FnxAlgorithm::StronglyConnectedComponents
                | FnxAlgorithm::Cdlp { .. }
                | FnxAlgorithm::Triangles
                | FnxAlgorithm::ClusteringCoefficient
                | FnxAlgorithm::DirectedClusteringCoefficient
                | FnxAlgorithm::InDegreeCentrality
                | FnxAlgorithm::OutDegreeCentrality
                | FnxAlgorithm::HitsCentrality
                | FnxAlgorithm::Foundation(
                    FoundationAlgorithm::DegreeCentrality
                        | FoundationAlgorithm::ClosenessCentrality
                        | FoundationAlgorithm::HarmonicCentrality
                        | FoundationAlgorithm::BetweennessCentrality
                        | FoundationAlgorithm::CoreNumber
                        | FoundationAlgorithm::EigenvectorCentrality
                        | FoundationAlgorithm::LoadCentrality
                )
        )
    }

    /// Preflight the registered graph-kind law before a host seals a source.
    /// Execution repeats this check, so using a prepared projection cannot
    /// bypass it. No graph content or resource allocation is required.
    pub fn validate_sealed_projection(&self, direction: Directedness) -> Result<()> {
        if !self.supports_sealed_execution() {
            return Err(Error::UnsupportedAlgorithm(self.algorithm()));
        }
        let required = self.signature().graph_kind;
        let compatible = match required {
            FnxGraphKind::Any => true,
            FnxGraphKind::Directed => direction != Directedness::Undirected,
            FnxGraphKind::Undirected => direction == Directedness::Undirected,
        };
        if !compatible {
            return Err(ExecutionError::GraphKind { required }.into());
        }
        Ok(())
    }

    /// Execute registered ranks, distances, components and local triangles or
    /// clustering, CDLP communities, centralities and core numbers from
    /// authenticated compressed rows.
    /// Degree centrality uses the admitted degree directory without opening
    /// rows; core peeling opens each row once with O(|V|) integer workspace.
    /// Eigenvector iteration keeps two score vectors and canonical reductions.
    /// Closeness and harmonic centrality reuse one FIFO and distance vector
    /// for all sources; their retained-history work bound applies to every pass.
    /// Exact betweenness shares source-ordered Brandes arithmetic with the native
    /// kernel and reopens compressed rows for dependencies using O(|V|) workspace.
    /// Every graph pass and row pull is fallible; identities remain stable VIds.
    /// Unsupported procedures refuse; they never allocate decoded adjacency.
    /// This synchronous API does not claim async scheduling or disk spill.
    pub fn execute_sealed(
        &self,
        cx: &QueryCx,
        graph: &SealedGraphView,
        limits: FnxExecutionLimits,
        memory: FnxMemoryLimits,
    ) -> Result<FnxResult> {
        self.execute_sealed_with_checkpoint(cx, graph, limits, memory, || Ok(()))
    }

    /// Execute the ordinary compressed kernels under an additional live guard.
    /// It spans admission, every raw row open/pull, kernel passes and result
    /// conversion. QueryCx cancellation always runs independently. A refusal
    /// returns no result prefix and preserves its SealedProjectionError cause.
    /// This does not authorize a caller-supplied graph or retain the guard in it.
    pub fn execute_sealed_with_checkpoint(
        &self,
        cx: &QueryCx,
        graph: &SealedGraphView,
        limits: FnxExecutionLimits,
        memory: FnxMemoryLimits,
        guard: impl FnMut() -> std::result::Result<(), SealedProjectionError>,
    ) -> Result<FnxResult> {
        let guard = RefCell::new(guard);
        let invoke = || (guard.borrow_mut())();
        let control = Control::new(cx, &invoke);
        self.execute_controlled(&control, graph, limits, memory, Delivery::Collected)?
            .collected()
    }

    /// Complete the compressed kernel, value validation and certificate before
    /// returning a cursor over its existing value arrays. Logical result rows
    /// and bytes keep the collected API's admission, even though no row table
    /// is allocated. The cursor shares the original immutable projection.
    /// Pulling rows needs a fresh caller checkpoint for every examined vertex;
    /// hosts must budget that additional pass, including unreachable gaps.
    pub fn execute_sealed_rows(
        &self,
        cx: &QueryCx,
        graph: &SealedGraphView,
        limits: FnxExecutionLimits,
        memory: FnxMemoryLimits,
    ) -> Result<FnxRowCursor> {
        self.execute_sealed_rows_with_checkpoint(cx, graph, limits, memory, || Ok(()))
    }

    /// Retain completed values under the same execution-local live guard as
    /// `execute_sealed_with_checkpoint`. Neither the guard nor a permit enters
    /// the returned cursor. Its consumer supplies current control on each pull.
    pub fn execute_sealed_rows_with_checkpoint(
        &self,
        cx: &QueryCx,
        graph: &SealedGraphView,
        limits: FnxExecutionLimits,
        memory: FnxMemoryLimits,
        guard: impl FnMut() -> std::result::Result<(), SealedProjectionError>,
    ) -> Result<FnxRowCursor> {
        let guard = RefCell::new(guard);
        let invoke = || (guard.borrow_mut())();
        let control = Control::new(cx, &invoke);
        self.execute_controlled(&control, graph, limits, memory, Delivery::Retained)?
            .retained()
    }

    fn execute_controlled(
        &self,
        cx: &Control<'_>,
        graph: &SealedGraphView,
        limits: FnxExecutionLimits,
        memory: FnxMemoryLimits,
        delivery: Delivery,
    ) -> Result<Finished> {
        checkpoint(cx)?;
        self.validate_sealed_projection(graph.spec().directedness)?;
        let n = graph.node_count();
        let admission = ResultAdmission::for_delivery(self, limits, memory, delivery)?;
        if directional_bfs::eligible(self, graph, limits, memory, &admission)
            && let Some(result) =
                directional_bfs::execute(self, cx, graph, limits, memory, &admission)?
        {
            return Ok(result);
        }
        if matches!(self.algorithm(), FnxAlgorithm::Cdlp { .. }) {
            return cdlp::execute(self, cx, graph, limits, memory, &admission);
        }
        if matches!(self.algorithm(), FnxAlgorithm::HitsCentrality) {
            return hits::execute(self, cx, graph, limits, memory, &admission);
        }
        if matches!(
            self.algorithm(),
            FnxAlgorithm::DirectedClusteringCoefficient
        ) {
            return directed_clustering::execute(self, cx, graph, limits, memory, &admission);
        }
        if matches!(
            self.algorithm(),
            FnxAlgorithm::Triangles | FnxAlgorithm::ClusteringCoefficient
        ) {
            return clustering::execute(self, cx, graph, limits, memory, &admission);
        }
        if matches!(
            self.algorithm(),
            FnxAlgorithm::Foundation(FoundationAlgorithm::CoreNumber)
        ) {
            return core_number::execute(self, cx, graph, limits, memory, &admission);
        }
        if matches!(
            self.algorithm(),
            FnxAlgorithm::Foundation(FoundationAlgorithm::EigenvectorCentrality)
        ) {
            return eigenvector::execute(self, cx, graph, limits, memory, &admission);
        }
        if matches!(
            self.algorithm(),
            FnxAlgorithm::Foundation(
                FoundationAlgorithm::ClosenessCentrality | FoundationAlgorithm::HarmonicCentrality
            )
        ) {
            return distance::execute(self, cx, graph, limits, memory, &admission);
        }
        if matches!(
            self.algorithm(),
            FnxAlgorithm::Foundation(FoundationAlgorithm::BetweennessCentrality)
        ) {
            return betweenness::execute(self, cx, graph, limits, memory, &admission);
        }
        if matches!(
            self.algorithm(),
            FnxAlgorithm::Foundation(FoundationAlgorithm::LoadCentrality)
        ) {
            return load::execute(self, cx, graph, limits, memory, &admission);
        }
        let pass = execution_pass_work(self, graph)?;
        let (kernel, estimated_work, workspace, source) = match self.algorithm() {
            FnxAlgorithm::InDegreeCentrality | FnxAlgorithm::OutDegreeCentrality => {
                admission.rows(n)?;
                let incoming = matches!(self.algorithm(), FnxAlgorithm::InDegreeCentrality);
                (
                    if incoming {
                        "fgdb-prism/sealed-in-degree-v1"
                    } else {
                        "fgdb-prism/sealed-out-degree-directory-v1"
                    },
                    pass,
                    mul(
                        n,
                        size_of::<f64>() + if incoming { size_of::<usize>() } else { 0 },
                    )?,
                    None,
                )
            }
            FnxAlgorithm::Foundation(FoundationAlgorithm::DegreeCentrality) => {
                admission.rows(n)?;
                (
                    "fgdb-prism/sealed-degree-directory-v1",
                    pass,
                    mul(n, size_of::<f64>())?,
                    None,
                )
            }
            FnxAlgorithm::PageRank(options) => {
                admit("iterations", options.max_iter(), limits.max_iterations)?;
                admission.rows(n)?;
                let source = options
                    .source()
                    .map(|source| {
                        graph
                            .vertex_ordinal(source)
                            .ok_or(ExecutionError::UnknownSource(source))
                    })
                    .transpose()?;
                (
                    "fgdb-prism/sealed-pagerank-v1",
                    mul(pass, add(options.max_iter(), 1)?)?,
                    mul(n, 3 * size_of::<f64>())?,
                    source,
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
                let unit = shortest_path::uses_unit_fifo(n, graph.has_unit_weights());
                (
                    if unit {
                        "fgdb-prism/sealed-dijkstra-unit-fifo-v1"
                    } else {
                        "fgdb-prism/sealed-dijkstra-indexed-heap-v1"
                    },
                    shortest_path::work(n, graph.adjacency_entry_count(), pass, unit)?,
                    shortest_path::workspace(n, unit)?,
                    Some(ordinal),
                )
            }
            algorithm @ (FnxAlgorithm::ConnectedComponents
            | FnxAlgorithm::WeaklyConnectedComponents
            | FnxAlgorithm::StronglyConnectedComponents) => {
                admission.rows(n)?;
                let strong = matches!(algorithm, FnxAlgorithm::StronglyConnectedComponents);
                let kernel = if strong {
                    "fgdb-prism/sealed-tarjan-v1"
                } else {
                    "fgdb-prism/sealed-union-find-complete-v2"
                };
                (
                    kernel,
                    components::work(n, graph.adjacency_entry_count(), pass, strong)?,
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
        if matches!(
            self.algorithm(),
            FnxAlgorithm::Foundation(FoundationAlgorithm::DegreeCentrality)
        ) {
            require_loop_free_degree(graph.edge_count(), graph.adjacency_entry_count())?;
        }
        let rows = SealedRows { cx, graph };
        let mut control = || checkpoint(cx);
        let output = match self.algorithm() {
            FnxAlgorithm::InDegreeCentrality | FnxAlgorithm::OutDegreeCentrality => {
                directed_degree_centrality(
                    &rows,
                    matches!(self.algorithm(), FnxAlgorithm::InDegreeCentrality),
                    &mut control,
                )?
            }
            FnxAlgorithm::Foundation(FoundationAlgorithm::DegreeCentrality) => {
                degree_centrality(&rows, &mut control)?
            }
            FnxAlgorithm::PageRank(options) => pagerank(&rows, options, source, &mut control)?,
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
            FnxAlgorithm::ConnectedComponents | FnxAlgorithm::WeaklyConnectedComponents => {
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
) -> Result<Finished> {
    finish_bfs(
        call,
        graph,
        output,
        kernel,
        estimated_work,
        workspace,
        admission,
        None,
        checkpoint,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish_bfs(
    call: &FnxCallSpec,
    graph: &SealedGraphView,
    output: KernelOutput,
    kernel: &'static str,
    estimated_work: usize,
    workspace: usize,
    admission: &ResultAdmission,
    bfs_decision_card: Option<BfsDecisionCard>,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<Finished> {
    checkpoint()?;
    let mut encoded = EncodedRows::new(call, output.row_count, admission)?;
    for index in 0..graph.node_count() {
        encoded.push(call, graph, &output.values, index, checkpoint)?;
    }
    finish_encoded_bfs(
        call,
        graph,
        encoded,
        output.values,
        output.witness,
        kernel,
        estimated_work,
        workspace,
        bfs_decision_card,
        checkpoint,
    )
}

// Shared row/hash implementation for synchronous and cooperative execution.
// Each push handles one vertex; cooperative callers yield BETWEEN pushes.
// No rows escape this private builder before the final certificate is complete.
struct EncodedRows {
    rows: Option<Vec<Vec<FnxValue>>>,
    result_hash: Hasher,
    row_count: usize,
    emitted: usize,
    max_integer: Option<u64>,
}
impl EncodedRows {
    fn new(call: &FnxCallSpec, row_count: usize, admission: &ResultAdmission) -> Result<Self> {
        admission.rows(row_count)?;
        let rows = match admission.delivery {
            Delivery::Collected => Some(reserve(row_count)?),
            Delivery::Retained => None,
        };
        let mut result_hash = Hasher::new();
        result_hash.update(b"fgdb:prism:result-rows:v3");
        result_hash.update(&(row_count as u128).to_le_bytes());
        result_hash.update(&call.digest().0);
        Ok(Self {
            rows,
            result_hash,
            row_count,
            emitted: 0,
            max_integer: None,
        })
    }

    fn push(
        &mut self,
        call: &FnxCallSpec,
        graph: &SealedGraphView,
        values: &KernelValues,
        index: usize,
        checkpoint: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        self.push_vertices(call, graph.vertex_ids(), values, index, checkpoint)
    }

    fn push_vertices(
        &mut self,
        call: &FnxCallSpec,
        vertices: &[fgdb_types::VId],
        values: &KernelValues,
        index: usize,
        checkpoint: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        checkpoint()?;
        let value = match values {
            KernelValues::HubsAndAuthorities { hubs, authorities } => {
                let hub = *hubs.get(index).ok_or(ExecutionError::InvalidUpstreamResult)?;
                let authority = *authorities.get(index).ok_or(ExecutionError::InvalidUpstreamResult)?;
                if !hub.is_finite() || hub < 0.0 || !authority.is_finite() || authority < 0.0 {
                    return Err(ExecutionError::InvalidNumericResult.into());
                }
                FnxValue::Score(hub)
            }
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
                    return Ok(());
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
                    vertices
                        .get(label)
                        .copied()
                        .ok_or(ExecutionError::InvalidUpstreamResult)?,
                )
            }
            KernelValues::WeightedDistances(values) => {
                let Some(distance) = *values
                    .get(index)
                    .ok_or(ExecutionError::InvalidUpstreamResult)?
                else {
                    return Ok(());
                };
                if !distance.is_finite() || distance < 0.0 {
                    return Err(ExecutionError::InvalidNumericResult.into());
                }
                FnxValue::Float(distance)
            }
            KernelValues::Counts(values) => FnxValue::Integer(
                *values
                    .get(index)
                    .ok_or(ExecutionError::InvalidUpstreamResult)?,
            ),
        };
        let vertex = vertices
            .get(index)
            .copied()
            .ok_or(ExecutionError::InvalidUpstreamResult)?;
        // Refuse an extra row before allocating even its bounded field vector.
        if self.emitted == self.row_count {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        let mut row = self
            .rows
            .as_ref()
            .map(|_| reserve(call.outputs().len()))
            .transpose()?;
        let result_hash = &mut self.result_hash;
        for column in call.outputs() {
            checkpoint()?;
            let value = match (column.field, value) {
                (FnxOutput::Vertex, _) => FnxValue::Vertex(vertex),
                (FnxOutput::Hub | FnxOutput::Authority, _) => {
                    values.value::<Infallible>(index, column.field, vertices)?
                }
                (FnxOutput::Score, FnxValue::Score(value)) => FnxValue::Score(value),
                (FnxOutput::Distance, FnxValue::Integer(value)) => FnxValue::Integer(value),
                (FnxOutput::Distance, FnxValue::Float(value)) => FnxValue::Float(value),
                (FnxOutput::Component, FnxValue::Vertex(value)) => FnxValue::Vertex(value),
                (FnxOutput::Triangles, FnxValue::Integer(value)) => FnxValue::Integer(value),
                (FnxOutput::Core, FnxValue::Integer(value)) => FnxValue::Integer(value),
                _ => return Err(ExecutionError::InvalidUpstreamResult.into()),
            };
            hash_result_value(result_hash, value);
            if let Some(row) = &mut row {
                row.push(value);
            } else if let FnxValue::Integer(value) = value {
                self.max_integer = Some(self.max_integer.map_or(value, |prior| prior.max(value)));
            }
        }
        if let (Some(rows), Some(row)) = (&mut self.rows, row) {
            rows.push(row);
        }
        self.emitted = add(self.emitted, 1)?;
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_encoded(
    call: &FnxCallSpec,
    graph: &SealedGraphView,
    encoded: EncodedRows,
    values: KernelValues,
    witness: ComplexityWitness,
    kernel: &'static str,
    estimated_work: usize,
    workspace: usize,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<Finished> {
    finish_encoded_bfs(
        call,
        graph,
        encoded,
        values,
        witness,
        kernel,
        estimated_work,
        workspace,
        None,
        checkpoint,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish_encoded_bfs(
    call: &FnxCallSpec,
    graph: &SealedGraphView,
    encoded: EncodedRows,
    values: KernelValues,
    mut witness: ComplexityWitness,
    kernel: &'static str,
    estimated_work: usize,
    workspace: usize,
    bfs_decision_card: Option<BfsDecisionCard>,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<Finished> {
    let EncodedRows {
        rows,
        result_hash,
        row_count,
        emitted,
        max_integer,
    } = encoded;
    if emitted != row_count {
        return Err(ExecutionError::InvalidUpstreamResult.into());
    }
    let mut columns = if rows.is_some() {
        reserve(call.outputs().len())?
    } else {
        Vec::new()
    };
    let mut fields = if rows.is_none() {
        reserve(call.outputs().len())?
    } else {
        Vec::new()
    };
    for column in call.outputs() {
        checkpoint()?;
        if rows.is_some() {
            let mut name = String::new();
            name.try_reserve_exact(column.name.len())
                .map_err(|_| ExecutionError::AllocationFailed)?;
            name.push_str(&column.name);
            columns.push(name);
        } else {
            fields.push(column.field);
        }
    }
    let result_digest = result_hash.finalize();
    let kernel_source_digest = source_digest();
    let numeric_profile = call.numeric_profile();
    let adapter = AdapterPath::CompressedCursor;
    if graph.spec().directedness != Directedness::Directed
        && !matches!(
            call.algorithm(),
            FnxAlgorithm::Triangles
                | FnxAlgorithm::ClusteringCoefficient
                | FnxAlgorithm::Foundation(FoundationAlgorithm::DegreeCentrality)
                | FnxAlgorithm::OutDegreeCentrality
        )
    {
        witness
            .complexity_claim
            .push_str("; plus O(p * H * log(1+H)) incoming locator work, p = graph passes");
    }
    let mut hash = Hasher::new();
    hash.update(b"fgdb:prism:call-certificate:v5");
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
    hash_bfs_decision_card(&mut hash, bfs_decision_card.as_ref(), checkpoint)?;
    hash_component_decision_card(&mut hash, None, checkpoint)?;
    checkpoint()?;
    let certificate = FnxCertificate {
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
        bfs_decision_card,
        component_decision_card: None,
        witness,
        digest: hash.finalize(),
    };
    match rows {
        Some(rows) => Ok(Finished::Collected(FnxResult {
            columns,
            rows,
            certificate,
        })),
        None => Ok(Finished::Retained(FnxRowCursor::completed_sealed(
            fields,
            graph,
            values,
            row_count,
            max_integer,
            certificate,
        ))),
    }
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
            include_str!("execute.rs"),
            include_str!("sealed_execute.rs"),
            include_str!("sealed_bfs.rs"),
            include_str!("sealed_cooperative.rs"),
            include_str!("sealed_betweenness.rs"),
            include_str!("sealed_betweenness_cooperative.rs"),
            include_str!("sealed_core.rs"),
            include_str!("sealed_distance.rs"),
            include_str!("sealed_load.rs"),
            include_str!("sealed_eigenvector.rs"),
            include_str!("foundation.rs"),
            include_str!("foundation/native.rs"),
            include_str!("sealed_clustering.rs"),
            include_str!("sealed_directed_clustering.rs"),
            include_str!("sealed_cdlp.rs"),
            include_str!("sealed_components.rs"),
            include_str!("sealed_shortest_path.rs"),
            include_str!("shortest_path.rs"),
            include_str!("sealed.rs"),
            include_str!("sealed_direction.rs"),
            include_str!("sealed_control.rs"),
            include_str!("../../fgdb-strata/src/tiered/sealed/mod.rs"),
            include_str!("../../fgdb-strata/src/tiered/sealed/incoming.rs"),
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

    #[test]
    fn retained_encoding_has_no_row_table_and_preserves_domains_hashes_and_refusal_order() {
        let vertices = [VId(0), VId(17), VId(u128::MAX)];
        let parameters =
            FnxParameters::from([("source".to_owned(), crate::FnxArgument::Vertex(vertices[0]))]);
        let bind = |text| FnxCallSpec::bind(text, &parameters).unwrap();
        let cases = [
            (
                bind("CALL fnx.pagerank() YIELD score AS rank, vertex AS id"),
                KernelValues::Scores(vec![0.0, 0.25, 0.75]),
                vec![
                    vec![FnxValue::Score(0.0), FnxValue::Vertex(vertices[0])],
                    vec![FnxValue::Score(0.25), FnxValue::Vertex(vertices[1])],
                    vec![FnxValue::Score(0.75), FnxValue::Vertex(vertices[2])],
                ],
            ),
            (
                bind(
                    "CALL fnx.single_source_shortest_path_length($source) YIELD distance AS hops, vertex AS id",
                ),
                KernelValues::Distances(vec![None, Some(0), Some(9)]),
                vec![
                    vec![FnxValue::Integer(0), FnxValue::Vertex(vertices[1])],
                    vec![FnxValue::Integer(9), FnxValue::Vertex(vertices[2])],
                ],
            ),
            (
                bind("CALL fnx.connected_components() YIELD component AS label, vertex AS id"),
                KernelValues::Components(vec![2, 2, 2]),
                vertices
                    .iter()
                    .map(|&vertex| vec![FnxValue::Vertex(vertices[2]), FnxValue::Vertex(vertex)])
                    .collect(),
            ),
            (
                bind(
                    "CALL fnx.single_source_dijkstra_path_length($source) YIELD distance AS cost, vertex AS id",
                ),
                KernelValues::WeightedDistances(vec![Some(0.0), None, Some(0.5)]),
                vec![
                    vec![FnxValue::Float(0.0), FnxValue::Vertex(vertices[0])],
                    vec![FnxValue::Float(0.5), FnxValue::Vertex(vertices[2])],
                ],
            ),
            (
                bind("CALL fnx.triangles() YIELD triangles AS count, vertex AS id"),
                KernelValues::Counts(vec![0, (1_u64 << 53) + 1, u64::MAX]),
                vec![
                    vec![FnxValue::Integer(0), FnxValue::Vertex(vertices[0])],
                    vec![
                        FnxValue::Integer((1_u64 << 53) + 1),
                        FnxValue::Vertex(vertices[1]),
                    ],
                    vec![FnxValue::Integer(u64::MAX), FnxValue::Vertex(vertices[2])],
                ],
            ),
        ];
        for (call, values, expected) in cases {
            let mut expected_hash = Hasher::new();
            expected_hash.update(b"fgdb:prism:result-rows:v3");
            expected_hash.update(&(expected.len() as u128).to_le_bytes());
            expected_hash.update(&call.digest().0);
            for row in &expected {
                for value in row {
                    hash_result_value(&mut expected_hash, *value);
                }
            }
            let expected_hash = expected_hash.finalize();
            let mut previous_checks = None;
            for delivery in [Delivery::Collected, Delivery::Retained] {
                let admission =
                    ResultAdmission::for_delivery(&call, limits(), memory(), delivery).unwrap();
                let mut encoded = EncodedRows::new(&call, expected.len(), &admission).unwrap();
                let mut checks = 0;
                for index in 0..vertices.len() {
                    encoded
                        .push_vertices(&call, &vertices, &values, index, &mut || {
                            checks += 1;
                            Ok(())
                        })
                        .unwrap();
                }
                assert_eq!(encoded.emitted, expected.len());
                match delivery {
                    Delivery::Collected => assert_eq!(encoded.rows.as_ref(), Some(&expected)),
                    Delivery::Retained => {
                        assert!(
                            encoded.rows.is_none(),
                            "no complete output row table is retained"
                        );
                        assert_eq!(
                            encoded.max_integer,
                            expected
                                .iter()
                                .flatten()
                                .filter_map(|value| match value {
                                    FnxValue::Integer(value) => Some(*value),
                                    _ => None,
                                })
                                .max()
                        );
                    }
                }
                assert_eq!(encoded.result_hash.finalize(), expected_hash);
                if let Some(previous) = previous_checks {
                    assert_eq!(checks, previous);
                }
                previous_checks = Some(checks);
                for stop in 1..=checks {
                    let mut encoded = EncodedRows::new(&call, expected.len(), &admission).unwrap();
                    let mut calls = 0;
                    let result = (0..vertices.len()).try_for_each(|index| {
                        encoded.push_vertices(&call, &vertices, &values, index, &mut || {
                            calls += 1;
                            if calls == stop {
                                Err(Error::Source(SealedProjectionError::UnknownOrdinal(stop)))
                            } else {
                                Ok(())
                            }
                        })
                    });
                    assert!(
                        matches!(result, Err(Error::Source(SealedProjectionError::UnknownOrdinal(at))) if at == stop)
                    );
                    assert_eq!(calls, stop);
                }
                let exact = admission.column_bytes + expected.len() * admission.row_bytes;
                let exact_memory = FnxMemoryLimits {
                    max_result_bytes: exact,
                    ..memory()
                };
                let exact_limits = FnxExecutionLimits {
                    max_result_rows: expected.len(),
                    ..limits()
                };
                assert!(
                    EncodedRows::new(
                        &call,
                        expected.len(),
                        &ResultAdmission::for_delivery(&call, exact_limits, exact_memory, delivery)
                            .unwrap()
                    )
                    .is_ok()
                );
                for (row_limit, byte_limit, resource) in [
                    (expected.len() - 1, exact, "result rows"),
                    (expected.len(), exact - 1, "result bytes"),
                ] {
                    let admission = ResultAdmission::for_delivery(
                        &call,
                        FnxExecutionLimits {
                            max_result_rows: row_limit,
                            ..limits()
                        },
                        FnxMemoryLimits {
                            max_result_bytes: byte_limit,
                            ..memory()
                        },
                        delivery,
                    )
                    .unwrap();
                    assert!(
                        matches!(EncodedRows::new(&call, expected.len(), &admission),
                        Err(Error::Execution(ExecutionError::LimitExceeded {resource: actual, ..})) if actual == resource)
                    );
                }
            }
        }
    }

    #[test]
    fn retained_vertex_only_outputs_validate_unselected_values_and_ignore_unselected_integer_maxima()
     {
        let call = FnxCallSpec::bind(
            "CALL fnx.pagerank() YIELD vertex AS id",
            &FnxParameters::new(),
        )
        .unwrap();
        let vertices = [VId(0), VId(17), VId(u128::MAX)];
        for delivery in [Delivery::Collected, Delivery::Retained] {
            let admission =
                ResultAdmission::for_delivery(&call, limits(), memory(), delivery).unwrap();
            for values in [
                KernelValues::Scores(vec![0.0, 1.0, f64::NAN]),
                KernelValues::Scores(vec![0.0, 1.0, -1.0]),
                KernelValues::WeightedDistances(vec![Some(0.0), Some(1.0), Some(f64::INFINITY)]),
                KernelValues::Components(vec![0, 1, 3]),
            ] {
                let mut encoded = EncodedRows::new(&call, 3, &admission).unwrap();
                for index in 0..2 {
                    encoded
                        .push_vertices(&call, &vertices, &values, index, &mut || Ok(()))
                        .unwrap();
                }
                assert!(matches!(
                    encoded.push_vertices(&call, &vertices, &values, 2, &mut || Ok(())),
                    Err(Error::Execution(
                        ExecutionError::InvalidNumericResult
                            | ExecutionError::InvalidUpstreamResult
                    ))
                ));
                assert_eq!(encoded.emitted, 2);
            }
            let mut encoded = EncodedRows::new(&call, 3, &admission).unwrap();
            for index in 0..3 {
                encoded
                    .push_vertices(
                        &call,
                        &vertices,
                        &KernelValues::Counts(vec![u64::MAX; 3]),
                        index,
                        &mut || Ok(()),
                    )
                    .unwrap();
            }
            assert_eq!(encoded.max_integer, None);
            assert_eq!(encoded.emitted, 3);
        }
    }

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
        fn is_undirected(&self) -> bool {
            !self.0.is_directed()
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

    // The degree driver may read admitted metadata but cannot decode a row.
    struct DegreeRows<'a>(&'a SnapshotGraphView);
    impl Rows for DegreeRows<'_> {
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
        fn open(&self, _: usize) -> Result<Self::Cursor<'_>> {
            panic!("degree centrality must not open an incidence cursor")
        }
    }

    #[test]
    fn degree_directory_matches_pinned_fnx_bits_on_every_small_simple_graph() {
        for n in 0..=5 {
            let vertices: Vec<_> = (0..n).map(|i| VId(u128::MAX - i as u128)).collect();
            let pairs: Vec<_> = (0..n)
                .flat_map(|source| ((source + 1)..n).map(move |target| (source, target)))
                .collect();
            for mask in 0usize..(1 << pairs.len()) {
                let edges: Vec<_> = pairs
                    .iter()
                    .enumerate()
                    .filter(|(bit, _)| mask & (1usize << *bit) != 0)
                    .map(|(bit, &(source, target))| ProjectionEdge {
                        eid: EId(bit as u128),
                        source: vertices[source],
                        target: vertices[target],
                        weight: 1.0,
                    })
                    .collect();
                let graph = SnapshotGraphView::build(
                    SnapshotBinding {
                        root: ObjectId([0x61; 32]),
                        as_of: CommitSeq(2),
                    },
                    &vertices,
                    &edges,
                    ProjectionSpec {
                        directedness: Directedness::Undirected,
                        parallel_edges: ParallelEdgePolicy::CollapseUnit,
                        self_loops: SelfLoopPolicy::Keep,
                    },
                    ProjectionLimits {
                        max_vertices: 5,
                        max_input_edges: 10,
                        max_adjacency_entries: 20,
                        max_workspace_bytes: 1 << 20,
                    },
                )
                .unwrap();
                let mut oracle = fnx_classes::Graph::strict();
                for index in 0..n {
                    oracle.add_node(graph.get_node_name(index).unwrap());
                }
                for edge in &edges {
                    let source = graph.vertex_ordinal(edge.source).unwrap();
                    let target = graph.vertex_ordinal(edge.target).unwrap();
                    oracle
                        .add_edge(
                            graph.get_node_name(source).unwrap(),
                            graph.get_node_name(target).unwrap(),
                        )
                        .unwrap();
                }
                let expected = fnx_algorithms::degree_centrality(&oracle);
                let output = degree_centrality(&DegreeRows(&graph), &mut || Ok(())).unwrap();
                let KernelValues::Scores(scores) = output.values else {
                    panic!("degree scores");
                };
                assert_eq!(scores.len(), expected.scores.len());
                for entry in expected.scores {
                    let index = graph.get_node_index(&entry.node).unwrap();
                    assert_eq!(
                        scores[index].to_bits(),
                        entry.score.to_bits(),
                        "n={n}, mask={mask}, vertex={index}"
                    );
                }
                assert_eq!(output.row_count, n);
                assert_eq!(output.witness.nodes_touched, n);
                assert_eq!(output.witness.edges_scanned, 0);
                assert_eq!(output.witness.queue_peak, 0);
            }
        }
    }

    #[test]
    fn degree_loop_guard_uses_exact_reduced_counts_without_overflow() {
        for nonloops in 0..10 {
            for loops in 0..10 {
                let result = require_loop_free_degree(nonloops + loops, 2 * nonloops + loops);
                if loops == 0 {
                    result.unwrap();
                } else {
                    assert!(matches!(
                        result,
                        Err(Error::Execution(ExecutionError::SelfLoopRefused))
                    ));
                }
            }
        }
        require_loop_free_degree(usize::MAX / 2, usize::MAX - 1).unwrap();
        assert!(matches!(
            require_loop_free_degree(usize::MAX, usize::MAX),
            Err(Error::Execution(ExecutionError::SelfLoopRefused))
        ));
    }

    #[test]
    fn every_degree_directory_checkpoint_refuses_without_returning_scores() {
        let graph = projection(0);
        let mut total = 0;
        degree_centrality(&DegreeRows(&graph), &mut || {
            total += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(total, graph.node_count() + 2);
        for stop in 1..=total {
            let mut seen = 0;
            let result = degree_centrality(&DegreeRows(&graph), &mut || {
                seen += 1;
                if seen == stop {
                    Err(Error::Cancelled(
                        std::io::Error::other("degree stop").into(),
                    ))
                } else {
                    Ok(())
                }
            });
            assert!(matches!(result, Err(Error::Cancelled(_))));
            assert_eq!(seen, stop);
        }
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
                let output = pagerank(&DecodedRows(&graph), options, None, &mut || Ok(())).unwrap();
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
    fn cursor_fixed_iteration_pagerank_matches_decoded_bits_on_every_three_node_topology() {
        for mask in 0..512 {
            let graph = projection(mask);
            for weighted in [false, true] {
                for iterations in [1, 2, 5] {
                    let options =
                        PageRankOptions::fixed_iterations(0.85, iterations, weighted).unwrap();
                    for source in [None, Some(0), Some(2)] {
                        let options = source.map_or(options, |source| {
                            options.with_source(graph.vertex_id(source).unwrap())
                        });
                        let decoded = FnxCallSpec::pagerank(options)
                            .execute(&graph, limits(), || Ok::<(), std::convert::Infallible>(()))
                            .unwrap();
                        let output =
                            pagerank(&DecodedRows(&graph), options, source, &mut || Ok(()))
                                .unwrap();
                        let KernelValues::Scores(scores) = output.values else {
                            panic!("scores");
                        };
                        assert_eq!(scores.len(), decoded.rows.len());
                        for (&actual, row) in scores.iter().zip(&decoded.rows) {
                            let crate::FnxValue::Score(expected) = row[1] else {
                                panic!("decoded score");
                            };
                            assert_eq!(
                                actual.to_bits(),
                                expected.to_bits(),
                                "mask={mask} weighted={weighted} iterations={iterations}"
                            );
                        }
                        assert_eq!(output.witness.algorithm, options.witness_algorithm());
                        assert_eq!(
                            output.witness.nodes_touched,
                            decoded.certificate.witness.nodes_touched
                        );
                        assert_eq!(
                            output.witness.edges_scanned,
                            decoded.certificate.witness.edges_scanned
                        );
                        assert_eq!(output.witness.nodes_touched, 3 * iterations);
                    }
                }
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
                    pagerank(&DecodedRows(&graph), options, None, &mut || control())
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
            pagerank(&DecodedRows(&graph), options, None, &mut || Ok(())),
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
                None,
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

    #[test]
    fn incoming_work_accounts_for_both_faces_and_retained_locator_searches() {
        for n in [1, 3, 100] {
            for history in [0, 1, 8, 10_000] {
                let directed = directional_pass_work(n, history, Directedness::Directed).unwrap();
                let reversed = directional_pass_work(n, history, Directedness::Reversed).unwrap();
                let undirected =
                    directional_pass_work(n, history, Directedness::Undirected).unwrap();
                assert_eq!(directed, pass_work(n, history).unwrap());
                assert!(reversed >= directed);
                assert!(undirected >= directed + reversed);
            }
        }
        for direction in [Directedness::Reversed, Directedness::Undirected] {
            assert_eq!(directional_pass_work(0, usize::MAX, direction).unwrap(), 0);
            assert!(directional_pass_work(1, usize::MAX, direction).is_err());
        }
    }

    #[test]
    fn compressed_calls_enforce_the_same_registered_graph_kinds_before_source_access() {
        let undirected = FnxCallSpec::connected_components();
        assert!(undirected.supports_sealed_execution());
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            assert_eq!(
                undirected.validate_sealed_projection(direction).is_ok(),
                direction == Directedness::Undirected
            );
            for call in [
                FnxCallSpec::weakly_connected_components(),
                FnxCallSpec::strongly_connected_components(),
            ] {
                assert_eq!(
                    call.validate_sealed_projection(direction).is_ok(),
                    direction != Directedness::Undirected
                );
            }
            for call in [
                FnxCallSpec::pagerank(PageRankOptions::default()),
                bfs_call(VId(0), None),
            ] {
                call.validate_sealed_projection(direction).unwrap();
            }
            for call in [
                FnxCallSpec::triangles(),
                FnxCallSpec::clustering_coefficient(),
                FnxCallSpec::bind(
                    "CALL fnx.degree_centrality() YIELD vertex,score",
                    &FnxParameters::new(),
                )
                .unwrap(),
                FnxCallSpec::bind(
                    "CALL fnx.core_number() YIELD vertex,core",
                    &FnxParameters::new(),
                )
                .unwrap(),
                FnxCallSpec::bind(
                    "CALL fnx.eigenvector_centrality() YIELD vertex,score",
                    &FnxParameters::new(),
                )
                .unwrap(),
            ] {
                assert!(call.supports_sealed_execution());
                assert_eq!(
                    call.validate_sealed_projection(direction).is_ok(),
                    direction == Directedness::Undirected
                );
            }
        }
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
                // Independently reconstruct the first complete row prefix proving
                // universal weak reachability. SCC still requires every row.
                let (nodes, edges) = if strong {
                    (3, graph.edge_count())
                } else {
                    let mut reach = [1_u8, 2, 4];
                    let (mut nodes, mut edges) = (0, 0);
                    for source in 0..3 {
                        nodes += 1;
                        for target in 0..3 {
                            if mask & (1 << (3 * source + target)) == 0 {
                                continue;
                            }
                            edges += 1;
                            let joined = reach[source] | reach[target];
                            for reachable in &mut reach {
                                if *reachable & joined != 0 {
                                    *reachable = joined;
                                }
                            }
                        }
                        if reach.iter().all(|&reachable| reachable == 7) {
                            break;
                        }
                    }
                    (nodes, edges)
                };
                assert_eq!(output.witness.nodes_touched, nodes);
                assert_eq!(output.witness.edges_scanned, edges);
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
                assert_eq!(
                    output.witness.edges_scanned,
                    if strong && cycle { n } else { n - 1 }
                );
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
