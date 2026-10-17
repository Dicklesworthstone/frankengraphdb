//! Checkpointed analytics over immutable decoded rows. Native kernels are
//! differentially checked against the pinned foundation; certificates identify
//! their own source instead of claiming that fnx executed the computation.

use crate::{
    AdapterPath, FNX_IMPLEMENTATION_REVISION, FNX_SIGNATURE_REGISTRY_VERSION, FnxAlgorithm,
    FnxCallSpec, FnxGraphKind, FnxOutput, GraphView, SnapshotBinding, SnapshotGraphView,
};
use fgdb_crypto::{Digest, Hasher};
use fgdb_types::VId;
pub use fnx_algorithms::ComplexityWitness;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FnxExecutionLimits {
    /// Iterative kernels only; traversal kernels have a fixed pass count.
    pub max_iterations: usize,
    pub max_result_rows: usize,
    /// Admission model: PageRank (n + arcs) * (max_iter + 1), BFS/CC n + arcs,
    /// WCC n + 2*arcs, SCC 2*(n + arcs). Dijkstra includes heap sift height;
    /// triangle kernels include one lower-degree scan per undirected edge.
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
    UnknownSource(VId),
    GraphKind {
        required: FnxGraphKind,
    },
    NotConverged {
        max_iterations: usize,
        witness: ComplexityWitness,
    },
    /// This procedure's graph law refuses a self-loop. Choose a projection
    /// that drops self-loops; the call never discards one on its own.
    SelfLoopRefused,
}
impl<C> FnxExecutionError<C> {
    /// A cancellation is the caller's own interruption, never a refusal:
    /// `Err(c)` for `Cancelled(c)`, otherwise the same refusal under any
    /// cancellation type. A host can then report refusals uniformly while
    /// surfacing each caller's cancellation in that caller's own type.
    pub fn split_cancel<D>(self) -> Result<FnxExecutionError<D>, C> {
        Ok(match self {
            Self::Cancelled(cancel) => return Err(cancel),
            Self::LimitExceeded {
                resource,
                limit,
                requested,
            } => FnxExecutionError::LimitExceeded {
                resource,
                limit,
                requested,
            },
            Self::SizeOverflow => FnxExecutionError::SizeOverflow,
            Self::AllocationFailed => FnxExecutionError::AllocationFailed,
            Self::NegativeWeight => FnxExecutionError::NegativeWeight,
            Self::NonFiniteWeightSum => FnxExecutionError::NonFiniteWeightSum,
            Self::InvalidNumericResult => FnxExecutionError::InvalidNumericResult,
            Self::InvalidUpstreamResult => FnxExecutionError::InvalidUpstreamResult,
            Self::UnknownSource(vertex) => FnxExecutionError::UnknownSource(vertex),
            Self::GraphKind { required } => FnxExecutionError::GraphKind { required },
            Self::NotConverged {
                max_iterations,
                witness,
            } => FnxExecutionError::NotConverged {
                max_iterations,
                witness,
            },
            Self::SelfLoopRefused => FnxExecutionError::SelfLoopRefused,
        })
    }
}
impl<C: core::fmt::Display> core::fmt::Display for FnxExecutionError<C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Cancelled(error) => write!(f, "Prism call cancelled: {error}"),
            Self::LimitExceeded {
                resource,
                limit,
                requested,
            } => {
                write!(
                    f,
                    "Prism {resource} admission refused: {requested} > {limit}"
                )
            }
            Self::SizeOverflow => f.write_str("Prism work estimate overflow"),
            Self::AllocationFailed => f.write_str("Prism working allocation failed"),
            Self::NegativeWeight => f.write_str("analytics requires nonnegative projected weights"),
            Self::NonFiniteWeightSum => f.write_str("PageRank outgoing weight sum is non-finite"),
            Self::InvalidNumericResult => {
                f.write_str("invalid or unrepresentable analytics numeric result")
            }
            Self::InvalidUpstreamResult => {
                f.write_str("invalid projected analytics state or output")
            }
            Self::UnknownSource(_) => {
                f.write_str("analytics source vertex is absent from the projection")
            }
            Self::GraphKind { required } => {
                write!(f, "analytics requires a {required:?} projection")
            }
            Self::NotConverged { max_iterations, .. } => {
                write!(
                    f,
                    "iterative analytics did not converge within {max_iterations} iterations"
                )
            }
            Self::SelfLoopRefused => {
                f.write_str("this procedure refuses self-loops; project with self-loops dropped")
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
    /// Exact hop or triangle count, never rounded through an f64 score column.
    Integer(u64),
    /// A finite weighted distance, distinct from a centrality score.
    Float(f64),
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

pub(crate) fn admit<C>(
    resource: &'static str,
    requested: usize,
    limit: usize,
) -> Result<(), FnxExecutionError<C>> {
    if requested > limit {
        Err(FnxExecutionError::LimitExceeded {
            resource,
            limit,
            requested,
        })
    } else {
        Ok(())
    }
}
pub(crate) fn reserve<T, C>(length: usize) -> Result<Vec<T>, FnxExecutionError<C>> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(length)
        .map_err(|_| FnxExecutionError::AllocationFailed)?;
    Ok(result)
}

pub(crate) enum KernelValues {
    Scores(Vec<f64>),
    Distances(Vec<Option<usize>>),
    Components(Vec<usize>),
    WeightedDistances(Vec<Option<f64>>),
    Counts(Vec<u64>),
}
pub(crate) struct KernelOutput {
    pub values: KernelValues,
    pub row_count: usize,
    pub witness: ComplexityWitness,
}
impl KernelValues {
    fn contains(&self, index: usize) -> bool {
        match self {
            Self::Scores(values) => index < values.len(),
            Self::Distances(values) => values.get(index).is_some_and(Option::is_some),
            Self::Components(values) => index < values.len(),
            Self::WeightedDistances(values) => values.get(index).is_some_and(Option::is_some),
            Self::Counts(values) => index < values.len(),
        }
    }
    fn value<C>(
        &self,
        index: usize,
        field: FnxOutput,
        graph: &SnapshotGraphView,
    ) -> Result<FnxValue, FnxExecutionError<C>> {
        match (field, self) {
            (FnxOutput::Vertex, _) => graph
                .vertex_id(index)
                .map(FnxValue::Vertex)
                .ok_or(FnxExecutionError::InvalidUpstreamResult),
            (FnxOutput::Score, Self::Scores(values)) => values
                .get(index)
                .copied()
                .map(FnxValue::Score)
                .ok_or(FnxExecutionError::InvalidUpstreamResult),
            (FnxOutput::Distance, Self::Distances(values)) => {
                let distance = values
                    .get(index)
                    .copied()
                    .flatten()
                    .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
                Ok(FnxValue::Integer(
                    u64::try_from(distance).map_err(|_| FnxExecutionError::SizeOverflow)?,
                ))
            }
            (FnxOutput::Component, Self::Components(values)) => values
                .get(index)
                .and_then(|&label| graph.vertex_id(label))
                .map(FnxValue::Vertex)
                .ok_or(FnxExecutionError::InvalidUpstreamResult),
            (FnxOutput::Distance, Self::WeightedDistances(values)) => values
                .get(index)
                .copied()
                .flatten()
                .map(FnxValue::Float)
                .ok_or(FnxExecutionError::InvalidUpstreamResult),
            (FnxOutput::Triangles | FnxOutput::Core, Self::Counts(values)) => values
                .get(index)
                .copied()
                .map(FnxValue::Integer)
                .ok_or(FnxExecutionError::InvalidUpstreamResult),
            _ => Err(FnxExecutionError::InvalidUpstreamResult),
        }
    }
}

/// A kernel's cancellable work meter. `units(n)` spends n units at once and
/// by default is n calls of `unit`, so a plain closure meters exactly as one
/// checkpoint per unit. A host may charge a run in one call, with the same
/// unit total and refusal ordinal. A kernel asks for at most
/// [`FNX_METER_RUN`] units at a time, so cancellation is observed at least
/// that often.
pub trait FnxMeter<C> {
    fn unit(&mut self) -> Result<(), C>;
    fn units(&mut self, n: usize) -> Result<(), C> {
        for _ in 0..n {
            self.unit()?;
        }
        Ok(())
    }
}

/// The most units a kernel requests in one [`FnxMeter::units`] call.
pub const FNX_METER_RUN: usize = 64;

impl<C, F: FnMut() -> Result<(), C>> FnxMeter<C> for F {
    fn unit(&mut self) -> Result<(), C> {
        self()
    }
}

/// Walks of at least this many arcs gather on scoped threads.
const PARALLEL_GATHER_ARCS: usize = 1 << 18;
/// The most threads one gather uses.
const MAX_GATHER_THREADS: usize = 8;

/// The threads a gather over `arcs` arcs uses.
fn gather_threads(arcs: usize) -> usize {
    if arcs < PARALLEL_GATHER_ARCS {
        return 1;
    }
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(MAX_GATHER_THREADS)
}

/// One PageRank pass as a gather: each target starts at the pass's initial
/// value and adds what every arc into it carries, its sources ascending. That is the order a source-ascending
/// scatter adds them in, and each arc's value is the scatter's `push * share`
/// (`contrib[source]` alone when unweighted, `contrib[source] * shares[arc]`
/// when weighted), so every sum is the scatter's floating-point sequence.
/// Several threads each own a contiguous range of targets with about equal
/// incoming arcs; the result is bit-identical on every thread count.
fn gather_pass(
    offsets: &[usize],
    sources: &[usize],
    contrib: &[f64],
    shares: Option<&[f64]>,
    initial: f64,
    next: &mut [f64],
    threads: usize,
) {
    let n = next.len();
    let arcs = sources.len();
    let gather = |low: usize, rows: &mut [f64]| {
        for (at, slot) in rows.iter_mut().enumerate() {
            let target = low + at;
            let mut sum = initial;
            let (start, end) = (offsets[target], offsets[target + 1]);
            match shares {
                None => {
                    for &source in &sources[start..end] {
                        sum += contrib[source];
                    }
                }
                Some(shares) => {
                    for arc in start..end {
                        sum += contrib[sources[arc]] * shares[arc];
                    }
                }
            }
            *slot = sum;
        }
    };
    if threads <= 1 {
        gather(0, next);
        return;
    }
    std::thread::scope(|scope| {
        let mut rest = next;
        let mut low = 0;
        for chunk in 1..=threads {
            let high = if chunk == threads {
                n
            } else {
                offsets[..n]
                    .partition_point(|&offset| offset < chunk * arcs / threads)
                    .max(low)
            };
            let (rows, tail) = rest.split_at_mut(high - low);
            let start = low;
            scope.spawn(move || gather(start, rows));
            rest = tail;
            low = high;
        }
    });
}

/// Spend `len` units in runs of FNX_METER_RUN: the calls `metered` makes for
/// a walk of `len` items, made before the walk.
fn charge_runs<C>(len: usize, meter: &mut impl FnxMeter<C>) -> Result<(), FnxExecutionError<C>> {
    let mut start = 0;
    while start < len {
        let end = len.min(start + FNX_METER_RUN);
        meter
            .units(end - start)
            .map_err(FnxExecutionError::Cancelled)?;
        start = end;
    }
    Ok(())
}

/// Each source's per-arc value for this pass: the scatter's push * share,
/// push alpha times rank, share 1 / sum unweighted (the weighted share is per
/// arc). Independent per source, so several threads give identical bits.
fn contributions(
    options: crate::PageRankOptions,
    ranks: &[f64],
    sums: &[f64],
    contrib: &mut [f64],
    threads: usize,
) {
    let fill = |low: usize, rows: &mut [f64]| {
        for (at, slot) in rows.iter_mut().enumerate() {
            let index = low + at;
            let push = options.alpha() * ranks[index];
            *slot = if options.weighted() {
                push
            } else if sums[index] > 0.0 {
                push * (1.0 / sums[index])
            } else {
                0.0
            };
        }
    };
    let n = contrib.len();
    if threads <= 1 {
        fill(0, contrib);
        return;
    }
    let chunk = n.div_ceil(threads);
    std::thread::scope(|scope| {
        for (at, rows) in contrib.chunks_mut(chunk.max(1)).enumerate() {
            scope.spawn(move || fill(at * chunk, rows));
        }
    });
}

/// `units` over `0..len` in runs, then `visit` each index of the run.
fn metered<C>(
    len: usize,
    meter: &mut impl FnxMeter<C>,
    mut visit: impl FnMut(usize) -> Result<(), FnxExecutionError<C>>,
) -> Result<(), FnxExecutionError<C>> {
    let mut start = 0;
    while start < len {
        let end = len.min(start + FNX_METER_RUN);
        meter
            .units(end - start)
            .map_err(FnxExecutionError::Cancelled)?;
        for index in start..end {
            visit(index)?;
        }
        start = end;
    }
    Ok(())
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
    meter: &mut impl FnxMeter<C>,
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
    // The pass gathers over the incoming face. A weighted arc's share is the
    // scatter's, placed at the arc's incoming position during the charged
    // walk below; the face is checked there row by row.
    let (in_offsets, in_sources) = graph.incoming_face();
    let mut face_ok = in_offsets.len() == n + 1 && in_offsets[n] == in_sources.len();
    let threads = gather_threads(in_sources.len());
    let mut contrib = reserve(n)?;
    // The rows with nothing to spread, ascending: each pass's dangling mass
    // sums exactly these, in the order a full walk would meet them.
    let mut dangling: Vec<usize> = reserve(n)?;
    let mut shares: Vec<f64> = Vec::new();
    let mut cursor: Vec<usize> = Vec::new();
    if options.weighted() && face_ok {
        shares = reserve(in_sources.len())?;
        shares.resize(in_sources.len(), 0.0);
        cursor = reserve(n)?;
        cursor.extend_from_slice(&in_offsets[..n]);
    }
    let population = n as f64;
    // Every pass's row-and-arc walk spends one unit per row plus one per arc.
    let mut row_units = 0usize;
    for source in 0..n {
        meter.unit().map_err(FnxExecutionError::Cancelled)?;
        let (targets, weights) = graph
            .projected_row(source)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        row_units = row_units
            .checked_add(1 + targets.len())
            .ok_or(FnxExecutionError::SizeOverflow)?;
        let mut sum = 0.0;
        if options.weighted() {
            for &weight in weights {
                meter.unit().map_err(FnxExecutionError::Cancelled)?;
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
        if face_ok {
            face_ok = in_offsets[source] <= in_offsets[source + 1];
        }
        if options.weighted() && face_ok {
            for (at, &target) in targets.iter().enumerate() {
                let share = if sum > 0.0 {
                    weights[at] / sum
                } else {
                    weights[at]
                };
                let slot = cursor[target];
                if slot >= in_offsets[target + 1] || in_sources[slot] != source {
                    face_ok = false;
                    break;
                }
                shares[slot] = share;
                cursor[target] = slot + 1;
            }
        }
        if sum == 0.0 {
            dangling.push(source);
        }
        sums.push(sum);
        ranks.push(1.0 / population);
        next.push(0.0);
        contrib.push(0.0);
    }
    if !face_ok {
        return Err(FnxExecutionError::InvalidUpstreamResult);
    }
    drop(cursor);
    let base = (1.0 - options.alpha()) / population;
    for _ in 0..options.max_iter() {
        // Each pass spends one unit per vertex or arc, as before, but asks
        // the meter for up to FNX_METER_RUN of them at once; the arithmetic
        // and its order are unchanged.
        // The dangling walk still spends a unit per row, charged first; it
        // adds exactly the dangling rows' ranks, ascending, as a full walk
        // that skips the others does.
        charge_runs(n, meter)?;
        let mut dangling_mass = 0.0;
        for &source in &dangling {
            dangling_mass += ranks[source];
        }
        let initial = base + options.alpha() * dangling_mass / population;
        // The initial values are the gather's starting sums; the
        // contribution walk spends its unit per row first.
        charge_runs(n, meter)?;
        contributions(options, &ranks, &sums, &mut contrib, threads);
        // The walk's units are one stream: a row's unit, then its arcs, row
        // after row, n + m in all. They are charged first, in runs of
        // FNX_METER_RUN across rows, so the meter sees exactly the calls a
        // row-at-a-time walk makes and a refusal stops before any of this
        // pass's arithmetic; the gather then needs no meter and may run on
        // several threads.
        let mut unpaid = row_units;
        while unpaid > 0 {
            let run = FNX_METER_RUN.min(unpaid);
            meter.units(run).map_err(FnxExecutionError::Cancelled)?;
            unpaid -= run;
        }
        gather_pass(
            in_offsets,
            in_sources,
            &contrib,
            options.weighted().then_some(shares.as_slice()),
            initial,
            &mut next,
            threads,
        );
        let mut delta = 0.0;
        metered(n, meter, |index| {
            if !next[index].is_finite() || next[index] < 0.0 {
                return Err(FnxExecutionError::InvalidNumericResult);
            }
            delta += (next[index] - ranks[index]).abs();
            Ok(())
        })?;
        // Swapping changes no arithmetic and avoids an uncheckpointed O(n)
        // copy. The old ranks buffer is overwritten during the next pass.
        std::mem::swap(&mut ranks, &mut next);
        witness.nodes_touched = witness
            .nodes_touched
            .checked_add(n)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        witness.edges_scanned = witness
            .edges_scanned
            .checked_add(arcs)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        if delta < population * options.tolerance() {
            return Ok((ranks, witness));
        }
    }
    Err(FnxExecutionError::NotConverged {
        max_iterations: options.max_iter(),
        witness,
    })
}

impl FnxCallSpec {
    /// Execute a bound in-core call with cancellation inside every graph pass,
    /// not merely before/after an uninterruptible foundation call. Kernel
    /// workspace is O(n); projection adjacency remains borrowed.
    /// The callback may cancel but this synchronous API does not provide an
    /// asynchronous bulkhead, a deadline guarantee or external-memory spill.
    pub fn execute<C>(
        &self,
        graph: &SnapshotGraphView,
        limits: FnxExecutionLimits,
        mut checkpoint: impl FnxMeter<C>,
    ) -> Result<FnxResult, FnxExecutionError<C>> {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        let algorithm = self.algorithm();
        let signature = self.signature();
        let numeric_profile = self.numeric_profile();
        let n = graph.node_count();
        match signature.graph_kind {
            FnxGraphKind::Directed if !graph.is_directed() => {
                return Err(FnxExecutionError::GraphKind {
                    required: FnxGraphKind::Directed,
                });
            }
            FnxGraphKind::Undirected if graph.is_directed() => {
                return Err(FnxExecutionError::GraphKind {
                    required: FnxGraphKind::Undirected,
                });
            }
            _ => {}
        }
        if let FnxAlgorithm::PageRank(options) = algorithm {
            admit("iterations", options.max_iter(), limits.max_iterations)?;
        }
        if !matches!(
            algorithm,
            FnxAlgorithm::SingleSourceShortestPathLength { .. }
                | FnxAlgorithm::SingleSourceDijkstraPathLength(_)
        ) {
            admit("result rows", n, limits.max_result_rows)?;
        }
        let mut arcs = 0usize;
        for source in 0..n {
            checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
            let neighbors = graph
                .neighbors_indices(source)
                .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
            arcs = arcs
                .checked_add(neighbors.len())
                .ok_or(FnxExecutionError::SizeOverflow)?;
        }
        let estimated_work = match algorithm {
            FnxAlgorithm::PageRank(options) => n.checked_add(arcs).and_then(|step| {
                options
                    .max_iter()
                    .checked_add(1)
                    .and_then(|iterations| step.checked_mul(iterations))
            }),
            FnxAlgorithm::WeaklyConnectedComponents => {
                arcs.checked_mul(2).and_then(|arcs| n.checked_add(arcs))
            }
            FnxAlgorithm::StronglyConnectedComponents => {
                n.checked_add(arcs).and_then(|step| step.checked_mul(2))
            }
            FnxAlgorithm::SingleSourceDijkstraPathLength(_) => {
                Some(crate::shortest_path::estimated_work::<C>(n, arcs)?)
            }
            FnxAlgorithm::Triangles | FnxAlgorithm::ClusteringCoefficient => {
                Some(crate::clustering::estimated_work(graph, &mut || {
                    checkpoint.unit()
                })?)
            }
            FnxAlgorithm::Foundation(algorithm) => {
                crate::foundation::estimated_work(algorithm, n, arcs)
            }
            _ => n.checked_add(arcs),
        }
        .ok_or(FnxExecutionError::SizeOverflow)?;
        admit("estimated work", estimated_work, limits.max_estimated_work)?;
        let word = std::mem::size_of::<usize>();
        let kernel_workspace_bytes = match algorithm {
            FnxAlgorithm::SingleSourceDijkstraPathLength(_) => {
                crate::shortest_path::workspace_bytes::<C>(n)?
            }
            FnxAlgorithm::Triangles | FnxAlgorithm::ClusteringCoefficient => {
                crate::clustering::workspace_bytes::<C>(n)?
            }
            FnxAlgorithm::Foundation(_) => {
                crate::foundation::copy_bytes(n, arcs).ok_or(FnxExecutionError::SizeOverflow)?
            }
            _ => {
                let bytes_per_vertex = match algorithm {
                    FnxAlgorithm::PageRank(_) => 3 * std::mem::size_of::<f64>(),
                    FnxAlgorithm::SingleSourceShortestPathLength { .. } => {
                        word + std::mem::size_of::<Option<usize>>()
                    }
                    FnxAlgorithm::StronglyConnectedComponents => {
                        4 * word + std::mem::size_of::<bool>()
                    }
                    _ => 2 * word,
                };
                n.checked_mul(bytes_per_vertex)
                    .ok_or(FnxExecutionError::SizeOverflow)?
            }
        };
        let output = match algorithm {
            FnxAlgorithm::PageRank(options) => {
                let (scores, witness) = pagerank(graph, options, arcs, &mut checkpoint)?;
                KernelOutput {
                    values: KernelValues::Scores(scores),
                    row_count: n,
                    witness,
                }
            }
            FnxAlgorithm::SingleSourceDijkstraPathLength(options) => {
                let output =
                    crate::shortest_path::run(graph, options, limits.max_result_rows, &mut || {
                        checkpoint.unit()
                    })?;
                KernelOutput {
                    values: KernelValues::WeightedDistances(output.distances),
                    row_count: output.row_count,
                    witness: output.witness,
                }
            }
            FnxAlgorithm::Triangles | FnxAlgorithm::ClusteringCoefficient => {
                let output = crate::clustering::run(graph, &mut || checkpoint.unit())?;
                let values = if matches!(algorithm, FnxAlgorithm::Triangles) {
                    KernelValues::Counts(output.triangles)
                } else {
                    KernelValues::Scores(output.clustering)
                };
                KernelOutput {
                    values,
                    row_count: n,
                    witness: output.witness,
                }
            }
            FnxAlgorithm::Foundation(algorithm) => {
                crate::foundation::run(graph, algorithm, &mut || checkpoint.unit())?
            }
            _ => crate::traversal::run(graph, algorithm, limits.max_result_rows, &mut || {
                checkpoint.unit()
            })?,
        };
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        let mut rows = reserve(output.row_count)?;
        let mut result_hash = Hasher::new();
        result_hash.update(b"fgdb:prism:result-rows:v3");
        result_hash.update(&(output.row_count as u128).to_le_bytes());
        result_hash.update(&self.digest().0);
        for index in 0..n {
            checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
            if !output.values.contains(index) {
                continue;
            }
            let mut row = reserve(self.outputs().len())?;
            for column in self.outputs() {
                let value = output.values.value(index, column.field, graph)?;
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
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        let witness = output.witness;
        let kernel_source_digest = kernel_source_digest();
        let mut hash = Hasher::new();
        hash.update(b"fgdb:prism:call-certificate:v3");
        hash.update(&FNX_SIGNATURE_REGISTRY_VERSION.to_le_bytes());
        hash.update(FNX_IMPLEMENTATION_REVISION.as_bytes());
        hash_text(&mut hash, signature.execution_kernel);
        hash.update(&kernel_source_digest.0);
        hash_text(&mut hash, numeric_profile);
        hash.update(&graph.digest().0);
        hash.update(&self.digest().0);
        let result_digest = result_hash.finalize();
        hash.update(&result_digest.0);
        hash_text(&mut hash, graph.adapter_path().as_str());
        for number in [
            n,
            graph.edge_count(),
            graph.input_edge_count(),
            estimated_work,
            kernel_workspace_bytes,
            witness.nodes_touched,
            witness.edges_scanned,
            witness.queue_peak,
        ] {
            hash.update(&(number as u128).to_le_bytes());
        }
        hash_text(&mut hash, &witness.algorithm);
        hash_text(&mut hash, &witness.complexity_claim);
        let certificate = FnxCertificate {
            registry_version: FNX_SIGNATURE_REGISTRY_VERSION,
            implementation_revision: FNX_IMPLEMENTATION_REVISION,
            execution_kernel: signature.execution_kernel,
            kernel_source_digest,
            numeric_profile,
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
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        Ok(FnxResult {
            columns: self
                .outputs()
                .iter()
                .map(|column| column.name.clone())
                .collect(),
            rows,
            certificate,
        })
    }
}
fn hash_text(hash: &mut Hasher, text: &str) {
    hash.update(&(text.len() as u128).to_le_bytes());
    hash.update(text.as_bytes());
}

fn kernel_source_digest() -> Digest {
    // The embedded sources are immutable. Do not hash the same source bundle
    // on every tiny graph call (or every differential fixture).
    static DIGEST: std::sync::OnceLock<Digest> = std::sync::OnceLock::new();
    *DIGEST.get_or_init(|| {
        let mut hash = Hasher::new();
        hash.update(b"fgdb:prism:kernel-source:v1");
        hash_text(&mut hash, include_str!("execute.rs"));
        hash_text(&mut hash, include_str!("projection.rs"));
        hash_text(&mut hash, include_str!("traversal.rs"));
        hash_text(&mut hash, include_str!("call.rs"));
        hash_text(&mut hash, include_str!("shortest_path.rs"));
        hash_text(&mut hash, include_str!("clustering.rs"));
        hash_text(&mut hash, include_str!("foundation.rs"));
        hash.finalize()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Directedness, PROJECTED_WEIGHT_ATTRIBUTE, PageRankOptions, ParallelEdgePolicy,
        ProjectionEdge, ProjectionLimits, ProjectionSpec, SelfLoopPolicy,
    };
    use fgdb_types::{CommitSeq, EId, ids::ObjectId};
    use std::convert::Infallible;

    fn projection(mask: u16, direction: Directedness) -> SnapshotGraphView {
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
                directedness: direction,
                parallel_edges: ParallelEdgePolicy::Sum,
                self_loops: SelfLoopPolicy::Keep,
            },
            ProjectionLimits {
                max_vertices: 3,
                max_input_edges: 9,
                max_adjacency_entries: 18,
                max_workspace_bytes: 1 << 20,
            },
        )
        .unwrap()
    }

    /// A meter that records each call's size and refuses at one ordinal,
    /// exactly: it sees every unit before the refused one.
    struct Recording {
        calls: Vec<usize>,
        stop: Option<usize>,
        seen: usize,
    }

    impl FnxMeter<&'static str> for Recording {
        fn unit(&mut self) -> Result<(), &'static str> {
            self.units(1)
        }
        fn units(&mut self, n: usize) -> Result<(), &'static str> {
            self.calls.push(n);
            if let Some(stop) = self.stop
                && self.seen + n >= stop
            {
                self.seen = stop;
                return Err("stop");
            }
            self.seen += n;
            Ok(())
        }
    }

    /// **PAGERANK'S ARC WALK IS ONE STREAM OF RUNS ACROSS ROWS.** On a
    /// 300-vertex graph of short rows plus a 150-arc hub, the kernel stays
    /// bit-identical to fnx. Every meter call is at most FNX_METER_RUN units,
    /// the walk makes about (n + m) / 64 calls per pass instead of one per row,
    /// and the total is unchanged: n, then 4n + m per pass. Refusing at any
    /// ordinal cancels there, with every earlier unit seen.
    #[test]
    fn pagerank_charges_its_arc_walk_in_runs_across_rows() {
        let n = 300usize;
        let vertices: Vec<VId> = (0..n as u128).map(VId).collect();
        let mut edges = Vec::new();
        for at in 0..n {
            for step in 1..=(at % 3) {
                edges.push((at, (at + step * 7) % n));
            }
        }
        for target in 1..=150 {
            edges.push((0, target));
        }
        let edges: Vec<ProjectionEdge> = edges
            .into_iter()
            .enumerate()
            .map(|(eid, (source, target))| ProjectionEdge {
                eid: EId(eid as u128),
                source: vertices[source],
                target: vertices[target],
                weight: 1.0,
            })
            .collect();
        let graph = SnapshotGraphView::build(
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
                max_vertices: n,
                max_input_edges: edges.len(),
                max_adjacency_entries: 2 * edges.len(),
                max_workspace_bytes: 1 << 24,
            },
        )
        .unwrap();
        let m: usize = (0..n)
            .map(|node| graph.neighbors_indices(node).unwrap().len())
            .sum();
        let options = PageRankOptions::new(0.85, 1000, 1e-12, false).unwrap();
        let oracle = fnx_algorithms::pagerank_with_weight(&graph, 0.85, 1000, 1e-12, None::<&str>);
        let mut meter = Recording {
            calls: Vec::new(),
            stop: None,
            seen: 0,
        };
        let (scores, witness) = pagerank(&graph, options, m, &mut meter).unwrap();
        for score in &oracle.scores {
            let ordinal = graph.get_node_index(&score.node).unwrap();
            assert_eq!(scores[ordinal].to_bits(), score.score.to_bits());
        }
        assert_eq!(witness, oracle.witness);
        let passes = witness.nodes_touched / n;
        assert!(passes > 3, "{passes}");
        assert_eq!(meter.seen, n + passes * (4 * n + m));
        assert!(meter.calls.iter().all(|&run| run <= FNX_METER_RUN));
        // Four metered passes per iteration, each about (units / 64) calls.
        let per_pass = |units: usize| units.div_ceil(FNX_METER_RUN);
        assert_eq!(
            meter.calls.len(),
            n + passes * (3 * per_pass(n) + per_pass(n + m)),
            "the arc walk is one stream of runs"
        );
        for stop in (1..=meter.seen).step_by(97) {
            let mut refusing = Recording {
                calls: Vec::new(),
                stop: Some(stop),
                seen: 0,
            };
            let result = pagerank(&graph, options, m, &mut refusing);
            assert!(matches!(result, Err(FnxExecutionError::Cancelled("stop"))));
            assert_eq!(refusing.seen, stop);
        }
    }

    /// **A GATHER'S SUMS DO NOT DEPEND ON ITS THREADS.** Over a 2,000-vertex
    /// graph with a hub, with and without per-arc shares, every thread count
    /// from 1 to 8 leaves every target's sum bit-identical to one thread's,
    /// whatever the chunk boundaries.
    #[test]
    fn gather_is_bit_identical_on_every_thread_count() {
        let n = 2_000usize;
        let vertices: Vec<VId> = (0..n as u128).map(VId).collect();
        let mut state = 0x9e37_79b9_u64;
        let mut next_random = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut pairs = Vec::new();
        for source in 0..n {
            for _ in 0..(next_random() % 9) {
                pairs.push((source, (next_random() % n as u64) as usize));
            }
        }
        for target in 0..n / 2 {
            pairs.push((target, 7));
        }
        let edges: Vec<ProjectionEdge> = pairs
            .iter()
            .enumerate()
            .map(|(eid, &(source, target))| ProjectionEdge {
                eid: EId(eid as u128),
                source: vertices[source],
                target: vertices[target],
                weight: [0.0, 0.5, 1.0, 3.25, 7.0][eid % 5],
            })
            .collect();
        let graph = SnapshotGraphView::build(
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
                max_vertices: n,
                max_input_edges: edges.len(),
                max_adjacency_entries: 2 * edges.len(),
                max_workspace_bytes: 1 << 26,
            },
        )
        .unwrap();
        let (offsets, sources) = graph.incoming_face();
        let contrib: Vec<f64> = (0..n).map(|at| 1.0 / (at as f64 + 3.0)).collect();
        let shares: Vec<f64> = (0..sources.len())
            .map(|arc| ((arc % 13) as f64 + 0.5) / 7.0)
            .collect();
        let start: Vec<f64> = (0..n).map(|at| (at as f64).sqrt() * 1e-3).collect();
        for weighted in [false, true] {
            let shares = weighted.then_some(shares.as_slice());
            let mut one = start.clone();
            gather_pass(offsets, sources, &contrib, shares, 0.25, &mut one, 1);
            assert_ne!(one, start, "the gather added something");
            for threads in 2..=8 {
                let mut many = start.clone();
                gather_pass(offsets, sources, &contrib, shares, 0.25, &mut many, threads);
                assert!(
                    one.iter()
                        .zip(&many)
                        .all(|(a, b)| a.to_bits() == b.to_bits()),
                    "weighted={weighted} {threads} threads"
                );
            }
        }
    }

    #[test]
    fn cooperative_kernel_is_bit_identical_to_fnx_on_all_three_node_topologies() {
        for mask in 0..512 {
            for direction in [
                Directedness::Directed,
                Directedness::Reversed,
                Directedness::Undirected,
            ] {
                let graph = projection(mask, direction);
                let arcs = (0..3)
                    .map(|node| graph.neighbors_indices(node).unwrap().len())
                    .sum();
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
                    let (scores, witness) =
                        pagerank(&graph, options, arcs, &mut || Ok::<(), Infallible>(())).unwrap();
                    for score in oracle.scores {
                        let ordinal = graph.get_node_index(&score.node).unwrap();
                        assert_eq!(
                            scores[ordinal].to_bits(),
                            score.score.to_bits(),
                            "{mask} {direction:?} weighted={weighted}"
                        );
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
        })
        .unwrap();
        // Initial validation takes n + m = 4 calls; subsequent callbacks
        // therefore prove cancellation INSIDE the power-iteration loop.
        assert!(checkpoints > 20);
        for stop in 1..=checkpoints {
            let mut observed = 0;
            let result = pagerank(&graph, options, 1, &mut || {
                observed += 1;
                if observed == stop {
                    Err("stop")
                } else {
                    Ok(())
                }
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
        let result = call
            .execute(
                &graph,
                FnxExecutionLimits {
                    max_iterations: 100,
                    max_result_rows: 3,
                    max_estimated_work: 10000,
                },
                || Ok::<(), Infallible>(()),
            )
            .unwrap();
        assert_eq!(
            result.certificate.execution_kernel,
            call.signature().execution_kernel
        );
        assert_eq!(
            result.certificate.kernel_workspace_bytes,
            3 * 3 * std::mem::size_of::<f64>()
        );
    }
}
