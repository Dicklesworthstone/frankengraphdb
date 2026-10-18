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
    /// Triangle kernels include forward-CSR construction and a lower-degree
    /// row bound per undirected edge, plus initialization and conversion.
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
    /// Peak vector backing bytes for the kernel, excluding the existing
    /// projection, output rows and allocator overhead. Includes temporary
    /// kernel indexes and normalized weights; PageRank records its reserved
    /// capacities at the allocation peak.
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
/// by default is n calls of `unit`. A host may charge a run in one call,
/// with the same unit total and refusal ordinal. A kernel asks for at most
/// [`FNX_METER_RUN`] units at a time.
///
/// `poll` checks live cancellation without charging work. Precharged phases
/// use it during their actual arithmetic, including inside a single hub row.
/// Hosts must keep their resource counters in `unit`/`units`, not in `poll`.
/// A plain closure is a cancellation checkpoint: it is called for both work
/// units and live polls, so its invocation count is not a logical work count.
pub trait FnxMeter<C> {
    fn poll(&mut self) -> Result<(), C>;
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
    fn poll(&mut self) -> Result<(), C> {
        self()
    }
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

/// Visit disjoint row ranges while the parent alone owns the host meter.
/// Workers poll a shared stop bit; they never borrow or send the host's
/// capability, meter, cancellation value or resource counters. The parent
/// computes the final range and keeps polling if workers outlast it (as on
/// a hub assigned to an earlier range). A refusal signals every worker
/// before the scope joins, so joining cannot hide another whole graph pass.
fn live_parallel_rows<C>(
    rows: &mut [f64],
    threads: usize,
    boundary: impl Fn(usize) -> usize,
    meter: &mut impl FnxMeter<C>,
    visit: impl Fn(usize, &mut [f64], &mut dyn FnMut() -> bool) + Sync,
) -> Result<(), FnxExecutionError<C>> {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct StopOnDrop<'a>(&'a AtomicBool);
    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    struct Running<'a>(&'a AtomicUsize);
    impl Drop for Running<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::Release);
        }
    }

    meter.poll().map_err(FnxExecutionError::Cancelled)?;
    let stopped = AtomicBool::new(false);
    let running = AtomicUsize::new(0);
    let mut cancelled = None;
    let mut parent_poll = || {
        if cancelled.is_some() {
            return false;
        }
        if let Err(error) = meter.poll() {
            cancelled = Some(error);
            stopped.store(true, Ordering::Release);
            return false;
        }
        true
    };
    if threads <= 1 {
        visit(0, rows, &mut parent_poll);
        parent_poll();
    } else {
        std::thread::scope(|scope| {
            // Also stops workers before unwinding through the scope if the
            // parent host or arithmetic panics. Running drains on panic too.
            let _stop = StopOnDrop(&stopped);
            let mut rest = rows;
            let mut low = 0;
            for chunk in 1..threads {
                let high = boundary(chunk).max(low);
                let (part, tail) = rest.split_at_mut(high - low);
                if !part.is_empty() {
                    running.fetch_add(1, Ordering::Relaxed);
                    let guard = Running(&running);
                    let visit = &visit;
                    let stopped = &stopped;
                    let start = low;
                    scope.spawn(move || {
                        let _running = guard;
                        visit(start, part, &mut || !stopped.load(Ordering::Acquire));
                    });
                }
                rest = tail;
                low = high;
            }
            visit(low, rest, &mut parent_poll);
            while running.load(Ordering::Acquire) != 0 {
                if !parent_poll() {
                    return;
                }
                std::thread::yield_now();
            }
            parent_poll();
        });
    }
    match cancelled {
        Some(error) => Err(FnxExecutionError::Cancelled(error)),
        None => Ok(()),
    }
}

/// Bound actual arithmetic independently of its already charged work. A run
/// spans rows so short rows do not each pay for a live host checkpoint.
struct LiveRun {
    left: usize,
}
impl LiveRun {
    fn new() -> Self {
        Self { left: 0 }
    }
    fn take(&mut self, count: usize, poll: &mut dyn FnMut() -> bool) -> Option<usize> {
        if self.left == 0 {
            if !poll() {
                return None;
            }
            self.left = FNX_METER_RUN;
        }
        let taken = count.min(self.left);
        self.left -= taken;
        Some(taken)
    }
}

/// The gather keeps one scalar accumulator per target across live-check
/// boundaries: no partial sums, regrouping or reordered floating addition.
struct Gather<'a> {
    offsets: &'a [usize],
    sources: &'a [usize],
    contrib: &'a [f64],
    shares: Option<&'a [f64]>,
    initial: f64,
}
impl Gather<'_> {
    fn rows(&self, low: usize, rows: &mut [f64], poll: &mut dyn FnMut() -> bool) {
        let mut run = LiveRun::new();
        if self.sources.is_empty() {
            // Every target keeps its initial value. Fill only the rows that
            // the live run admits, preserving each poll and refusal boundary.
            let mut rest = rows;
            while !rest.is_empty() {
                let Some(taken) = run.take(rest.len(), poll) else {
                    return;
                };
                let (slots, tail) = rest.split_at_mut(taken);
                slots.fill(self.initial);
                rest = tail;
            }
            return;
        }
        for (at, slot) in rows.iter_mut().enumerate() {
            if run.take(1, poll).is_none() {
                return;
            }
            let target = low + at;
            let mut sum = self.initial;
            let (mut start, end) = (self.offsets[target], self.offsets[target + 1]);
            while start < end {
                let Some(taken) = run.take(end - start, poll) else {
                    return;
                };
                let high = start + taken;
                match self.shares {
                    None => {
                        for &source in &self.sources[start..high] {
                            sum += self.contrib[source];
                        }
                    }
                    Some(shares) => {
                        for (&source, &share) in
                            self.sources[start..high].iter().zip(&shares[start..high])
                        {
                            sum += self.contrib[source] * share;
                        }
                    }
                }
                start = high;
            }
            *slot = sum;
        }
    }

    /// Each target receives sources ascending, exactly as the pinned scalar
    /// scatter. Disjoint ranges need no reduction or synchronization of sums.
    fn pass<C>(
        &self,
        next: &mut [f64],
        threads: usize,
        meter: &mut impl FnxMeter<C>,
    ) -> Result<(), FnxExecutionError<C>> {
        let n = next.len();
        let arcs = self.sources.len();
        live_parallel_rows(
            next,
            threads,
            |chunk| self.offsets[..n].partition_point(|&offset| offset < chunk * arcs / threads),
            meter,
            |low, rows, poll| self.rows(low, rows, poll),
        )
    }
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
fn contributions<C>(
    options: crate::PageRankOptions,
    ranks: &[f64],
    out_offsets: &[usize],
    contrib: &mut [f64],
    threads: usize,
    meter: &mut impl FnxMeter<C>,
) -> Result<(), FnxExecutionError<C>> {
    let n = contrib.len();
    let chunk = n.div_ceil(threads.max(1));
    live_parallel_rows(
        contrib,
        threads,
        |part| n.min(part * chunk),
        meter,
        |low, rows, poll| {
            for (run, slots) in rows.chunks_mut(FNX_METER_RUN).enumerate() {
                if !poll() {
                    return;
                }
                for (at, slot) in slots.iter_mut().enumerate() {
                    let index = low + run * FNX_METER_RUN + at;
                    let push = options.alpha() * ranks[index];
                    *slot = if options.weighted() {
                        push
                    } else {
                        let degree = out_offsets[index + 1] - out_offsets[index];
                        if degree > 0 {
                            push * (1.0 / degree as f64)
                        } else {
                            0.0
                        }
                    };
                }
            }
        },
    )
}

/// Dangling mass is a scalar reduction in ascending source order. Even an
/// all-isolate graph remains interruptible after precharging its row walk.
fn dangling_mass<C>(
    dangling: &[usize],
    ranks: &[f64],
    meter: &mut impl FnxMeter<C>,
) -> Result<f64, FnxExecutionError<C>> {
    let mut sum = 0.0;
    for run in dangling.chunks(FNX_METER_RUN) {
        meter.poll().map_err(FnxExecutionError::Cancelled)?;
        for &source in run {
            sum += ranks[source];
        }
    }
    Ok(sum)
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
/// dangling-mass redistribution, then an ordered L1 convergence test. Weighted
/// execution retains one normalized share per incoming arc; unweighted
/// execution borrows CSR offsets instead of allocating per-vertex sums.
fn pagerank<C>(
    graph: &SnapshotGraphView,
    options: crate::PageRankOptions,
    arcs: usize,
    meter: &mut impl FnxMeter<C>,
) -> Result<(Vec<f64>, ComplexityWitness, usize), FnxExecutionError<C>> {
    let n = graph.node_count();
    let mut witness = ComplexityWitness {
        algorithm: "pagerank_power_iteration".to_owned(),
        complexity_claim: "O(k * (|V| + |E|))".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 0,
    };
    if n == 0 {
        return Ok((Vec::new(), witness, 0));
    }
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
        while shares.len() < in_sources.len() {
            meter.poll().map_err(FnxExecutionError::Cancelled)?;
            let run = FNX_METER_RUN.min(in_sources.len() - shares.len());
            shares.resize(shares.len() + run, 0.0);
        }
        cursor = reserve(n)?;
        for run in in_offsets[..n].chunks(FNX_METER_RUN) {
            meter.poll().map_err(FnxExecutionError::Cancelled)?;
            cursor.extend_from_slice(run);
        }
    }
    // All backing stores coexist here. Derive the certificate from the
    // actual reserved capacities, including the weighted transpose cursor
    // that is dropped before iteration; a fixed "three vectors" model hid
    // both O(n) scratch and the O(m) normalized shares.
    let workspace_bytes = [
        (ranks.capacity(), std::mem::size_of::<f64>()),
        (next.capacity(), std::mem::size_of::<f64>()),
        (contrib.capacity(), std::mem::size_of::<f64>()),
        (dangling.capacity(), std::mem::size_of::<usize>()),
        (shares.capacity(), std::mem::size_of::<f64>()),
        (cursor.capacity(), std::mem::size_of::<usize>()),
    ]
    .into_iter()
    .try_fold(0usize, |bytes, (capacity, width)| {
        capacity
            .checked_mul(width)
            .and_then(|allocation| bytes.checked_add(allocation))
            .ok_or(FnxExecutionError::SizeOverflow)
    })?;
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
                if at % FNX_METER_RUN == 0 {
                    meter.poll().map_err(FnxExecutionError::Cancelled)?;
                }
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
        let dangling_mass = dangling_mass(&dangling, &ranks, meter)?;
        let initial = base + options.alpha() * dangling_mass / population;
        // The initial values are the gather's starting sums; the
        // contribution walk spends its unit per row first.
        charge_runs(n, meter)?;
        contributions(
            options,
            &ranks,
            graph.outgoing_offsets(),
            &mut contrib,
            threads,
            meter,
        )?;
        // The walk's units are one stream: a row's unit, then its arcs, row
        // after row, n + m in all. They are charged first, in runs of
        // FNX_METER_RUN across rows, so the meter sees exactly the calls a
        // row-at-a-time walk makes and a refusal stops before any of this
        // pass's arithmetic. The gather's separate live polls charge no
        // units; only the parent touches the host meter when using workers.
        let mut unpaid = row_units;
        while unpaid > 0 {
            let run = FNX_METER_RUN.min(unpaid);
            meter.units(run).map_err(FnxExecutionError::Cancelled)?;
            unpaid -= run;
        }
        Gather {
            offsets: in_offsets,
            sources: in_sources,
            contrib: &contrib,
            shares: options.weighted().then_some(shares.as_slice()),
            initial,
        }
        .pass(&mut next, threads, meter)?;
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
            return Ok((ranks, witness, workspace_bytes));
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
    /// workspace depends on the kernel; weighted PageRank retains O(n+m)
    /// scratch while borrowing the projection's adjacency.
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
        let mut kernel_workspace_bytes = match algorithm {
            // Successful PageRank returns its measured allocation peak below.
            FnxAlgorithm::PageRank(_) => 0,
            FnxAlgorithm::SingleSourceDijkstraPathLength(_) => {
                crate::shortest_path::workspace_bytes::<C>(n)?
            }
            FnxAlgorithm::Triangles | FnxAlgorithm::ClusteringCoefficient => {
                crate::clustering::workspace_bytes::<C>(n, graph.edge_count())?
            }
            FnxAlgorithm::Foundation(_) => {
                crate::foundation::copy_bytes(n, arcs).ok_or(FnxExecutionError::SizeOverflow)?
            }
            _ => {
                let bytes_per_vertex = match algorithm {
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
                let (scores, witness, workspace_bytes) =
                    pagerank(graph, options, arcs, &mut checkpoint)?;
                kernel_workspace_bytes = workspace_bytes;
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
            _ => crate::traversal::run(graph, algorithm, limits.max_result_rows, &mut checkpoint)?,
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
        polls: usize,
    }

    impl FnxMeter<&'static str> for Recording {
        fn poll(&mut self) -> Result<(), &'static str> {
            self.polls += 1;
            Ok(())
        }
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

    /// Arithmetic helpers are already charged. Their only legal host call
    /// is a live poll, and cancellation is independent of the work counter.
    struct LiveOnly {
        polls: usize,
        stop: Option<usize>,
    }
    impl FnxMeter<&'static str> for LiveOnly {
        fn poll(&mut self) -> Result<(), &'static str> {
            self.polls += 1;
            if self.stop == Some(self.polls) {
                Err("live cancellation")
            } else {
                Ok(())
            }
        }
        fn unit(&mut self) -> Result<(), &'static str> {
            panic!("already charged arithmetic must not charge work again")
        }
    }

    #[test]
    fn live_cancellation_interrupts_contribution_arithmetic_after_a_bounded_prefix() {
        let n = 4 * FNX_METER_RUN + 1;
        let ranks: Vec<f64> = (0..n).map(|row| (row + 1) as f64 / 1024.0).collect();
        let offsets: Vec<usize> = (0..=n).map(|row| 3 * row).collect();
        for weighted in [false, true] {
            let mut values = vec![f64::NAN; n];
            let mut meter = LiveOnly {
                polls: 0,
                stop: Some(4),
            };
            let result = contributions(
                PageRankOptions::new(0.85, 100, 1e-12, weighted).unwrap(),
                &ranks,
                &offsets,
                &mut values,
                1,
                &mut meter,
            );
            assert!(matches!(
                result,
                Err(FnxExecutionError::Cancelled("live cancellation"))
            ));
            assert_eq!(meter.polls, 4, "no host call follows cancellation");
            for row in 0..2 * FNX_METER_RUN {
                let push = 0.85 * ranks[row];
                let expected = if weighted { push } else { push * (1.0 / 3.0) };
                assert_eq!(values[row].to_bits(), expected.to_bits());
            }
            assert!(
                values[2 * FNX_METER_RUN..]
                    .iter()
                    .all(|value| value.is_nan())
            );
        }
    }

    #[test]
    fn live_cancellation_interrupts_a_single_incoming_hub_without_publishing_it() {
        let hub = 4 * FNX_METER_RUN;
        let offsets = [0, 1, hub + 1, hub + 2];
        let sources = vec![0; hub + 2];
        let shares = vec![0.5; hub + 2];
        for weighted in [false, true] {
            let mut values = vec![f64::NAN; 3];
            let mut meter = LiveOnly {
                polls: 0,
                stop: Some(3),
            };
            let result = Gather {
                offsets: &offsets,
                sources: &sources,
                contrib: &[2.0],
                shares: weighted.then_some(shares.as_slice()),
                initial: 0.25,
            }
            .pass(&mut values, 1, &mut meter);
            assert!(matches!(
                result,
                Err(FnxExecutionError::Cancelled("live cancellation"))
            ));
            assert_eq!(meter.polls, 3);
            assert_eq!(values[0], if weighted { 1.25 } else { 2.25 });
            assert!(
                values[1].is_nan(),
                "the interrupted hub has no complete value"
            );
            assert!(values[2].is_nan(), "later rows remain untouched");
        }
    }

    #[test]
    fn dangling_arithmetic_checks_live_cancellation_without_recharging_rows() {
        let n = 3 * FNX_METER_RUN + 1;
        let ranks: Vec<f64> = (0..n).map(|row| 1.0 / (row + 7) as f64).collect();
        let dangling: Vec<usize> = (0..n).collect();
        let mut complete = LiveOnly {
            polls: 0,
            stop: None,
        };
        let result = dangling_mass(&dangling, &ranks, &mut complete).unwrap();
        let mut expected = 0.0;
        for rank in &ranks {
            expected += rank;
        }
        assert_eq!(result.to_bits(), expected.to_bits());
        assert_eq!(complete.polls, n.div_ceil(FNX_METER_RUN));
        let mut meter = LiveOnly {
            polls: 0,
            stop: Some(2),
        };
        assert!(matches!(
            dangling_mass(&dangling, &ranks, &mut meter),
            Err(FnxExecutionError::Cancelled("live cancellation"))
        ));
        assert_eq!(meter.polls, 2);
    }

    #[test]
    fn a_hub_worker_is_cancelled_by_the_non_send_parent_meter_and_joined() {
        use std::cell::Cell;
        use std::rc::Rc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        struct Host<'a> {
            owner: std::thread::ThreadId,
            reached_hub: &'a AtomicBool,
            // Both the meter and its error are intentionally non-Send.
            polls: Rc<Cell<usize>>,
        }
        impl FnxMeter<Rc<Cell<usize>>> for Host<'_> {
            fn poll(&mut self) -> Result<(), Rc<Cell<usize>>> {
                assert_eq!(std::thread::current().id(), self.owner);
                self.polls.set(self.polls.get() + 1);
                if self.reached_hub.load(Ordering::Acquire) {
                    Err(Rc::clone(&self.polls))
                } else {
                    Ok(())
                }
            }
            fn unit(&mut self) -> Result<(), Rc<Cell<usize>>> {
                panic!("worker arithmetic may not charge the parent meter")
            }
        }

        // Row 0 completes on a worker before it enters row 1's hub. Row 2
        // follows that hub; the parent has only row 3 and finishes early.
        let hub = 8 * FNX_METER_RUN;
        let offsets = [0, 1, hub + 1, hub + 2, hub + 3];
        let sources = vec![0; hub + 3];
        let shares = vec![0.5; hub + 3];
        for weighted in [false, true] {
            let gather = Gather {
                offsets: &offsets,
                sources: &sources,
                contrib: &[2.0],
                shares: weighted.then_some(shares.as_slice()),
                initial: 0.25,
            };
            let reached_hub = AtomicBool::new(false);
            let worker_exited = AtomicBool::new(false);
            let polls = Rc::new(Cell::new(0));
            let mut meter = Host {
                owner: std::thread::current().id(),
                reached_hub: &reached_hub,
                polls: Rc::clone(&polls),
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut values = vec![f64::NAN; 4];
            let result = live_parallel_rows(
                &mut values,
                2,
                |_| 3,
                &mut meter,
                |low, rows, poll| {
                    let mut runs = 0;
                    gather.rows(low, rows, &mut || {
                        runs += 1;
                        if low == 0 && runs == 2 {
                            // The worker has completed real gather arithmetic
                            // before requesting cancellation. Keep it at this
                            // actual hub checkpoint until its parent polls.
                            reached_hub.store(true, Ordering::Release);
                            while poll() {
                                assert!(
                                    Instant::now() < deadline,
                                    "parent did not poll while waiting for its hub worker"
                                );
                                std::thread::yield_now();
                            }
                            return false;
                        }
                        poll()
                    });
                    if low == 0 {
                        assert_eq!(runs, 2, "no worker chunk follows the stop signal");
                        worker_exited.store(true, Ordering::Release);
                    }
                },
            );
            match result {
                Err(FnxExecutionError::Cancelled(error)) => assert!(Rc::ptr_eq(&error, &polls)),
                other => panic!("expected the parent's live cancellation, got {other:?}"),
            }
            assert!(
                worker_exited.load(Ordering::Acquire),
                "return waits for worker exit"
            );
            assert_eq!(values[0], if weighted { 1.25 } else { 2.25 });
            assert!(values[1].is_nan());
            assert!(values[2].is_nan());
            // A cancellation may reach the parent before its own row starts;
            // any completed parent value must still have the scalar bits.
            assert!(values[3].is_nan() || values[3] == values[0]);
        }
    }

    /// **PAGERANK'S ARC WALK IS ONE STREAM OF RUNS ACROSS ROWS.** On a
    /// 300-vertex graph of short rows plus a 150-arc hub, the kernel stays
    /// bit-identical to fnx. Every meter call is at most FNX_METER_RUN units,
    /// the walk makes about (n + m) / 64 calls per pass instead of one per row,
    /// and the total is unchanged: n (n + m when weighted), then 4n + m per
    /// pass. Live arithmetic polls add no work; refusing at any work ordinal
    /// cancels there, with every earlier unit seen.
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
        for weighted in [false, true] {
            let options = PageRankOptions::new(0.85, 1000, 1e-12, weighted).unwrap();
            let oracle = fnx_algorithms::pagerank_with_weight(
                &graph,
                0.85,
                1000,
                1e-12,
                weighted.then_some(PROJECTED_WEIGHT_ATTRIBUTE),
            );
            let mut meter = Recording {
                calls: Vec::new(),
                stop: None,
                seen: 0,
                polls: 0,
            };
            let (scores, witness, _) = pagerank(&graph, options, m, &mut meter).unwrap();
            for score in &oracle.scores {
                let ordinal = graph.get_node_index(&score.node).unwrap();
                assert_eq!(scores[ordinal].to_bits(), score.score.to_bits());
            }
            assert_eq!(witness, oracle.witness);
            let passes = witness.nodes_touched / n;
            assert!(passes > 3, "{passes}");
            let initialization = n + if weighted { m } else { 0 };
            assert_eq!(meter.seen, initialization + passes * (4 * n + m));
            assert!(
                meter.polls > passes,
                "live polls actually ran between charges"
            );
            assert!(meter.calls.iter().all(|&run| run <= FNX_METER_RUN));
            // Four metered passes per iteration, each about (units / 64) calls.
            let per_pass = |units: usize| units.div_ceil(FNX_METER_RUN);
            assert_eq!(
                meter.calls.len(),
                initialization + passes * (3 * per_pass(n) + per_pass(n + m)),
                "the arc walk is one stream of runs"
            );
            for stop in (1..=meter.seen).step_by(97) {
                let mut refusing = Recording {
                    calls: Vec::new(),
                    stop: Some(stop),
                    seen: 0,
                    polls: 0,
                };
                let result = pagerank(&graph, options, m, &mut refusing);
                assert!(matches!(result, Err(FnxExecutionError::Cancelled("stop"))));
                assert_eq!(refusing.seen, stop);
            }
        }
    }

    #[test]
    fn contributions_borrow_irregular_degrees_with_scalar_bits_on_every_thread_count() {
        // Thirty-one rows leave an uneven final chunk for every thread count
        // from two through eight. Repeated offsets include dangling rows at
        // the beginning, middle and end, with different degrees at chunk edges.
        let degrees = [
            0usize, 1, 3, 0, 17, 2, 0, 1, 9, 4, 0, 6, 1, 0, 11, 2, 0, 5, 2, 13, 0, 7, 1, 0, 3, 19,
            0, 2, 1, 4, 0,
        ];
        let mut offsets = vec![0];
        for degree in degrees {
            offsets.push(offsets.last().copied().unwrap() + degree);
        }
        let ranks: Vec<f64> = (0..degrees.len())
            .map(|at| (17 * at + 1) as f64 / ((at + 3) as f64 * 4096.0))
            .collect();
        // The incumbent retained these floating-point degree sums. Keep its
        // reciprocal-then-multiply arithmetic independent of the CSR offsets.
        let sums: Vec<f64> = degrees.iter().map(|&degree| degree as f64).collect();
        for weighted in [false, true] {
            let options = PageRankOptions::new(0.85, 100, 1e-12, weighted).unwrap();
            let expected: Vec<f64> = ranks
                .iter()
                .zip(&sums)
                .map(|(&rank, &sum)| {
                    let push = options.alpha() * rank;
                    if weighted {
                        push
                    } else if sum > 0.0 {
                        push * (1.0 / sum)
                    } else {
                        0.0
                    }
                })
                .collect();
            for threads in 1..=8 {
                let mut actual = vec![f64::NAN; ranks.len()];
                contributions(options, &ranks, &offsets, &mut actual, threads, &mut || {
                    Ok::<(), Infallible>(())
                })
                .unwrap();
                for (row, (&actual, &expected)) in actual.iter().zip(&expected).enumerate() {
                    assert_eq!(
                        actual.to_bits(),
                        expected.to_bits(),
                        "row={row} weighted={weighted} threads={threads}",
                    );
                }
            }
        }
    }

    #[test]
    fn empty_gather_preserves_initial_bits_and_live_polls_on_every_thread_count() {
        for n in [0, 1, 31, 65_536] {
            let offsets = vec![0; n + 1];
            for initial in [0.0, -0.0, 0.25, f64::from_bits(0x7ff8_0000_0000_0042)] {
                for weighted in [false, true] {
                    let gather = Gather {
                        offsets: &offsets,
                        sources: &[],
                        contrib: &[],
                        shares: weighted.then_some(&[]),
                        initial,
                    };
                    for threads in 1..=MAX_GATHER_THREADS {
                        let mut actual = vec![f64::NAN; n];
                        let mut meter = LiveOnly {
                            polls: 0,
                            stop: None,
                        };
                        gather.pass(&mut actual, threads, &mut meter).unwrap();
                        assert!(actual.iter().all(|value| value.to_bits() == initial.to_bits()));
                        assert_eq!(meter.polls, 2 + n.div_ceil(FNX_METER_RUN));
                    }
                }
            }
        }
    }

    #[test]
    fn empty_gather_bulk_fill_preserves_each_live_refusal_prefix() {
        let n = 2 * FNX_METER_RUN + 1;
        let offsets = vec![0; n + 1];
        for weighted in [false, true] {
            let gather = Gather {
                offsets: &offsets,
                sources: &[],
                contrib: &[],
                shares: weighted.then_some(&[]),
                initial: 0.25,
            };
            for stop in 1..=3 {
                let mut actual = vec![f64::NAN; n];
                let mut polls = 0;
                gather.rows(0, &mut actual, &mut || {
                    polls += 1;
                    polls != stop
                });
                let filled = (stop - 1) * FNX_METER_RUN;
                assert_eq!(polls, stop);
                assert!(actual[..filled].iter().all(|&value| value == 0.25));
                assert!(actual[filled..].iter().all(|value| value.is_nan()));
            }
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
            let gather = Gather {
                offsets,
                sources,
                contrib: &contrib,
                shares,
                initial: 0.25,
            };
            gather
                .pass(&mut one, 1, &mut || Ok::<(), Infallible>(()))
                .unwrap();
            assert_ne!(one, start, "the gather added something");
            for threads in 2..=8 {
                let mut many = start.clone();
                gather
                    .pass(&mut many, threads, &mut || Ok::<(), Infallible>(()))
                    .unwrap();
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
                    let (scores, witness, _) =
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
            3 * (3 * std::mem::size_of::<f64>() + 2 * std::mem::size_of::<usize>())
                + graph.incoming_face().1.len() * std::mem::size_of::<f64>()
        );
    }

    #[test]
    fn pagerank_certifies_weighted_peak_scratch_and_unweighted_degree_borrowing() {
        let limits = FnxExecutionLimits {
            max_iterations: 1000,
            max_result_rows: 3,
            max_estimated_work: 100000,
        };
        // Isolates, a dangling destination, and a complete graph exercise
        // zero-length shares, degree-zero handling, and a real transpose.
        for mask in [0, 2, 511] {
            let graph = projection(mask, Directedness::Directed);
            let arcs = graph.incoming_face().1.len();
            for weighted in [false, true] {
                let result = FnxCallSpec::pagerank(
                    PageRankOptions::new(0.85, 1000, 1e-12, weighted).unwrap(),
                )
                .execute(&graph, limits, || Ok::<(), Infallible>(()))
                .unwrap();
                let n = graph.node_count();
                let retained = n * (3 * std::mem::size_of::<f64>() + std::mem::size_of::<usize>());
                let weighted_peak = if weighted {
                    arcs * std::mem::size_of::<f64>() + n * std::mem::size_of::<usize>()
                } else {
                    0
                };
                assert_eq!(
                    result.certificate.kernel_workspace_bytes,
                    retained + weighted_peak,
                    "mask={mask} weighted={weighted}",
                );
            }
        }
    }

    #[test]
    fn empty_pagerank_certifies_zero_kernel_workspace() {
        let graph = SnapshotGraphView::build(
            SnapshotBinding {
                root: ObjectId([2; 32]),
                as_of: CommitSeq(3),
            },
            &[],
            &[],
            ProjectionSpec {
                directedness: Directedness::Directed,
                parallel_edges: ParallelEdgePolicy::Sum,
                self_loops: SelfLoopPolicy::Keep,
            },
            ProjectionLimits {
                max_vertices: 0,
                max_input_edges: 0,
                max_adjacency_entries: 0,
                max_workspace_bytes: 1024,
            },
        )
        .unwrap();
        for weighted in [false, true] {
            let result =
                FnxCallSpec::pagerank(PageRankOptions::new(0.85, 1, 1e-12, weighted).unwrap())
                    .execute(
                        &graph,
                        FnxExecutionLimits {
                            max_iterations: 1,
                            max_result_rows: 0,
                            max_estimated_work: 0,
                        },
                        || Ok::<(), Infallible>(()),
                    )
                    .unwrap();
            assert!(result.rows.is_empty());
            let certificate = result.certificate;
            assert_eq!(certificate.vertices, 0);
            assert_eq!(certificate.edges, 0);
            assert_eq!(certificate.kernel_workspace_bytes, 0);
            assert_eq!(certificate.witness.nodes_touched, 0);
            assert_eq!(certificate.witness.edges_scanned, 0);
        }
    }
}
