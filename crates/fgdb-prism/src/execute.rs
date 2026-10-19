//! Checkpointed analytics over immutable decoded rows. Native kernels are
//! differentially checked against their registered reference semantics;
//! certificates identify their own source instead of claiming that fnx
//! executed the computation.

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
    /// CDLP includes graph-law checks, histogram initialization, all synchronous
    /// passes and result conversion, including a zero-iteration result.
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
    /// Pinned algorithm foundation revision. Native kernels also name their
    /// own execution source; CDLP's oracle is Graphalytics, whose synchronous
    /// label semantics differ from the foundation's community algorithm.
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

/// A kernel's work meter and live cancellation control. `units(n)` spends n
/// logical units, by default through n calls of `unit`. A host may charge a
/// run in one call, with the same unit total and refusal ordinal. A kernel
/// asks for at most [`FNX_METER_RUN`] units at a time.
///
/// `poll` checks live cancellation/authority without spending logical work.
/// It also runs during arithmetic whose work was already charged. A plain
/// cancellation closure handles both kinds of checkpoint; a host that counts
/// work implements this trait to keep the two operations separate. Neither
/// the meter nor its error needs to be `Send`: parallel arithmetic polls it
/// only on the calling thread and joins all workers before returning.
pub trait FnxMeter<C> {
    fn unit(&mut self) -> Result<(), C>;
    fn poll(&mut self) -> Result<(), C>;
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
    fn poll(&mut self) -> Result<(), C> {
        self()
    }
}

/// Walks of at least this many arcs gather on scoped threads.
const PARALLEL_GATHER_ARCS: usize = 1 << 18;
/// The most threads one gather uses.
pub(crate) const MAX_GATHER_THREADS: usize = 8;

/// Precharged arithmetic checks live control after at most this many scalar
/// rows/arcs. This is independent of the logical meter's 64-unit charge runs.
/// A parallel worker must receive a fresh parent acknowledgement before each
/// such chunk, including chunks inside a single high-degree row.
const ARITHMETIC_RUN: usize = 4096;

/// Progress normally arrives within a short arithmetic chunk. Avoid a
/// scheduler yield on every handshake, but back off on an oversubscribed
/// host or while a slower host control callback is running.
fn arithmetic_wait(spins: &mut usize) {
    if *spins < 64 {
        *spins += 1;
        std::hint::spin_loop();
    } else {
        std::thread::yield_now();
    }
}

// Each worker owns its progress line. Keeping unrelated workers off the same
// cache line avoids contending on one counter in the inner arithmetic path.
#[repr(align(64))]
struct ArithmeticWorker {
    requested: std::sync::atomic::AtomicUsize,
    admitted: std::sync::atomic::AtomicUsize,
    done: std::sync::atomic::AtomicBool,
}

/// Run disjoint row ranges without sending the host's meter or error to a
/// worker. A worker reports each bounded chunk and waits for the parent to
/// poll live control before acknowledging it. Merely polling before joins
/// would allow a whole hub row to run after a cancellation.
pub(crate) fn arithmetic_rows<T: Send, C>(
    rows: &mut [T],
    ends: &[usize],
    meter: &mut impl FnxMeter<C>,
    run: impl Fn(usize, &mut [T], &mut dyn FnMut() -> bool) + Sync,
) -> Result<(), FnxExecutionError<C>> {
    if ends.len() <= 1 {
        let mut failure = None;
        run(0, rows, &mut || match meter.poll() {
            Ok(()) => true,
            Err(error) => {
                failure = Some(error);
                false
            }
        });
        return match failure {
            Some(error) => Err(FnxExecutionError::Cancelled(error)),
            None => Ok(()),
        };
    }
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let stop = AtomicBool::new(false);
    let workers: [ArithmeticWorker; MAX_GATHER_THREADS] =
        std::array::from_fn(|_| ArithmeticWorker {
            requested: AtomicUsize::new(0),
            admitted: AtomicUsize::new(0),
            done: AtomicBool::new(false),
        });
    // These guards also run during unwinding: neither a host panic nor a
    // worker panic can strand another worker waiting for an acknowledgement.
    struct StopOnDrop<'a>(&'a AtomicBool);
    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    struct DoneOnDrop<'a> {
        done: &'a AtomicBool,
        stop: &'a AtomicBool,
    }
    impl Drop for DoneOnDrop<'_> {
        fn drop(&mut self) {
            if std::thread::panicking() {
                self.stop.store(true, Ordering::Release);
            }
            self.done.store(true, Ordering::Release);
        }
    }
    std::thread::scope(|scope| {
        let _stop_on_exit = StopOnDrop(&stop);
        let mut rest = rows;
        let mut low = 0;
        for (&high, worker) in ends.iter().zip(&workers) {
            let (chunk, tail) = rest.split_at_mut(high - low);
            let start = low;
            let run = &run;
            let stop = &stop;
            scope.spawn(move || {
                let _done_on_exit = DoneOnDrop {
                    done: &worker.done,
                    stop,
                };
                let mut requested = 0;
                run(start, chunk, &mut || {
                    if stop.load(Ordering::Acquire) {
                        return false;
                    }
                    // This worker is the only writer of its request epoch;
                    // a published store needs no locked read-modify-write.
                    requested += 1;
                    worker.requested.store(requested, Ordering::Release);
                    let mut spins = 0;
                    while worker.admitted.load(Ordering::Acquire) < requested {
                        if stop.load(Ordering::Acquire) {
                            return false;
                        }
                        arithmetic_wait(&mut spins);
                    }
                    !stop.load(Ordering::Acquire)
                });
            });
            rest = tail;
            low = high;
        }
        let mut admitted = [0; MAX_GATHER_THREADS];
        let mut idle_spins = 0;
        loop {
            let mut requested = [0; MAX_GATHER_THREADS];
            let mut pending = false;
            let mut done = true;
            for (at, worker) in workers[..ends.len()].iter().enumerate() {
                requested[at] = worker.requested.load(Ordering::Acquire);
                pending |= requested[at] != admitted[at];
                done &= worker.done.load(Ordering::Acquire);
            }
            if pending || done {
                idle_spins = 0;
                meter.poll().map_err(FnxExecutionError::Cancelled)?;
                // Acknowledge only the requests captured BEFORE this poll.
                // Polling every idle yield would repeatedly enter the host
                // clock/authority while no additional work can start.
                for (at, worker) in workers[..ends.len()].iter().enumerate() {
                    worker.admitted.store(requested[at], Ordering::Release);
                    admitted[at] = requested[at];
                }
            }
            if done {
                return Ok(());
            }
            if !pending {
                arithmetic_wait(&mut idle_spins);
            }
        }
        // scope joins every worker, including on a refusal, before exposing
        // this Result to the caller. Partially filled row buffers stay private.
    })
}

/// The threads a gather over `arcs` arcs uses.
fn gather_threads(arcs: usize) -> usize {
    if arcs < PARALLEL_GATHER_ARCS {
        return 1;
    }
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(MAX_GATHER_THREADS)
}

/// Retain only nonempty row partitions. When fewer than two partitions have
/// incoming arcs, no independent sums can benefit from parallel gathering;
/// use the same live-poll walk serially instead of synchronizing a lone hub.
fn gather_partitions(
    offsets: &[usize],
    n: usize,
    arcs: usize,
    threads: usize,
    ends: &mut [usize; MAX_GATHER_THREADS],
) -> usize {
    let threads = threads.clamp(1, MAX_GATHER_THREADS);
    let (mut low, mut count, mut with_arcs) = (0, 0, 0);
    for chunk in 1..=threads {
        let high = if chunk == threads {
            n
        } else {
            // Divide before multiplying so the partition calculation cannot
            // overflow even when the addressable arc count is near usize::MAX.
            let target = (arcs / threads) * chunk + (arcs % threads) * chunk / threads;
            offsets[..n].partition_point(|&offset| offset < target)
        };
        if high == low {
            continue;
        }
        ends[count] = high;
        count += 1;
        with_arcs += usize::from(offsets[high] > offsets[low]);
        low = high;
    }
    if with_arcs < 2 {
        ends[0] = n;
        1
    } else {
        count
    }
}

/// One PageRank pass as a gather: each target starts at the pass's initial
/// value and adds what every arc into it carries, its sources ascending. That is the order a source-ascending
/// scatter adds them in, and each arc's value is the scatter's `push * share`
/// (`contrib[source]` alone when unweighted, `contrib[source] * shares[arc]`
/// when weighted), so every sum is the scatter's floating-point sequence.
/// Several threads each own a contiguous range of targets with about equal
/// incoming arcs; the result is bit-identical on every thread count.
#[allow(clippy::too_many_arguments)]
fn gather_pass<C>(
    offsets: &[usize],
    sources: &[usize],
    contrib: &[f64],
    shares: Option<&[f64]>,
    initial: f64,
    next: &mut [f64],
    threads: usize,
    meter: &mut impl FnxMeter<C>,
) -> Result<(), FnxExecutionError<C>> {
    let n = next.len();
    let arcs = sources.len();
    let gather = |low: usize, rows: &mut [f64], poll: &mut dyn FnMut() -> bool| {
        if sources.is_empty() {
            // No target adds anything to its initial value. Keep the scalar
            // row walk's live-poll boundaries while filling admitted chunks.
            for slots in rows.chunks_mut(ARITHMETIC_RUN) {
                if !poll() {
                    return;
                }
                slots.fill(initial);
            }
            return;
        }
        let mut remaining = 0;
        for (at, slot) in rows.iter_mut().enumerate() {
            if remaining == 0 {
                if !poll() {
                    return;
                }
                remaining = ARITHMETIC_RUN;
            }
            remaining -= 1;
            let target = low + at;
            let mut sum = initial;
            let (mut start, end) = (offsets[target], offsets[target + 1]);
            while start < end {
                if remaining == 0 {
                    if !poll() {
                        return;
                    }
                    remaining = ARITHMETIC_RUN;
                }
                let count = remaining.min(end - start);
                let high = start + count;
                match shares {
                    None => {
                        for &source in &sources[start..high] {
                            sum += contrib[source];
                        }
                    }
                    Some(shares) => {
                        for arc in start..high {
                            sum += contrib[sources[arc]] * shares[arc];
                        }
                    }
                }
                start = high;
                remaining -= count;
            }
            *slot = sum;
        }
    };
    let mut ends = [n; MAX_GATHER_THREADS];
    let partitions = gather_partitions(offsets, n, arcs, threads, &mut ends);
    arithmetic_rows(next, &ends[..partitions], meter, gather)
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
    let fill = |low: usize, rows: &mut [f64], poll: &mut dyn FnMut() -> bool| {
        for (chunk, rows) in rows.chunks_mut(ARITHMETIC_RUN).enumerate() {
            if !poll() {
                return;
            }
            for (at, slot) in rows.iter_mut().enumerate() {
                let index = low + chunk * ARITHMETIC_RUN + at;
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
    };
    let n = contrib.len();
    let threads = threads.clamp(1, MAX_GATHER_THREADS);
    let mut ends = [n; MAX_GATHER_THREADS];
    for chunk in 1..threads {
        ends[chunk - 1] = (n / threads) * chunk + (n % threads) * chunk / threads;
    }
    arithmetic_rows(contrib, &ends[..threads], meter, fill)
}

/// Dangling mass is a scalar reduction in ascending source order. Even an
/// all-isolate graph remains interruptible after precharging its row walk.
fn dangling_mass<C>(
    dangling: &[usize],
    ranks: &[f64],
    meter: &mut impl FnxMeter<C>,
) -> Result<f64, FnxExecutionError<C>> {
    let mut sum = 0.0;
    for run in dangling.chunks(ARITHMETIC_RUN) {
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
    pagerank_with_threads(
        graph,
        options,
        arcs,
        gather_threads(graph.incoming_face().1.len()),
        meter,
    )
}

fn pagerank_with_threads<C>(
    graph: &SnapshotGraphView,
    options: crate::PageRankOptions,
    arcs: usize,
    threads: usize,
    meter: &mut impl FnxMeter<C>,
) -> Result<(Vec<f64>, ComplexityWitness, usize), FnxExecutionError<C>> {
    let n = graph.node_count();
    let mut witness = ComplexityWitness {
        algorithm: options.witness_algorithm().to_owned(),
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
    // Contributions visit each source once, independent of its degree. A
    // graph with many arcs can justify parallel gathering while its much
    // smaller source fill is faster without another scoped worker group.
    let contribution_threads = gather_threads(n).min(threads);
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
            let run = ARITHMETIC_RUN.min(in_sources.len() - shares.len());
            shares.resize(shares.len() + run, 0.0);
        }
        cursor = reserve(n)?;
        for run in in_offsets[..n].chunks(ARITHMETIC_RUN) {
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
                if at != 0 && at % ARITHMETIC_RUN == 0 {
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
    for iteration in 0..options.max_iter() {
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
            contribution_threads,
            meter,
        )?;
        // The walk's units are one stream: a row's unit, then its arcs, row
        // after row, n + m in all. They are charged first, in runs of
        // FNX_METER_RUN across rows, so the meter sees exactly the calls a
        // row-at-a-time walk makes and a refusal stops before any of this
        // pass's arithmetic. Live, uncharged polls during the gather keep
        // cancellation and authority checks interleaved with the real work.
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
            meter,
        )?;
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
        if options.iteration_complete(iteration + 1, delta, population) {
            return Ok((ranks, witness, workspace_bytes));
        }
    }
    Err(FnxExecutionError::NotConverged {
        max_iterations: options.max_iter(),
        witness,
    })
}

impl FnxCallSpec {
    /// Execute a bound in-core call. Native kernels check live cancellation
    /// within graph passes; catalog entries still delegated to fnx observe
    /// it before and after the foundation computation. Kernel workspace
    /// depends on the kernel; weighted PageRank retains O(n+m) scratch while
    /// borrowing the projection's adjacency.
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
        match algorithm {
            FnxAlgorithm::PageRank(options) => {
                admit("iterations", options.max_iter(), limits.max_iterations)?;
            }
            FnxAlgorithm::Cdlp { max_iter } => {
                admit("iterations", max_iter, limits.max_iterations)?;
            }
            _ => {}
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
                Some(crate::clustering::estimated_work(graph, &mut checkpoint)?)
            }
            FnxAlgorithm::Foundation(algorithm) => {
                crate::foundation::estimated_work(algorithm, n, arcs)
            }
            FnxAlgorithm::Cdlp { max_iter } => {
                crate::traversal::cdlp_estimated_work(n, arcs, max_iter, graph.is_directed())
            }
            _ => n.checked_add(arcs),
        }
        .ok_or(FnxExecutionError::SizeOverflow)?;
        admit("estimated work", estimated_work, limits.max_estimated_work)?;
        let word = std::mem::size_of::<usize>();
        let mut kernel_workspace_bytes = match algorithm {
            // Successful PageRank returns its measured allocation peak below.
            FnxAlgorithm::PageRank(_) | FnxAlgorithm::Cdlp { .. } => 0,
            FnxAlgorithm::SingleSourceDijkstraPathLength(_) => {
                crate::shortest_path::workspace_bytes::<C>(n)?
            }
            FnxAlgorithm::Triangles | FnxAlgorithm::ClusteringCoefficient => {
                crate::clustering::workspace_bytes::<C>(n, graph.edge_count())?
            }
            // Native catalog kernels return their reserved vector peak;
            // delegated entries retain the explicit graph-copy estimate.
            FnxAlgorithm::Foundation(_) => 0,
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
                let output = crate::shortest_path::run(
                    graph,
                    options,
                    limits.max_result_rows,
                    &mut checkpoint,
                )?;
                KernelOutput {
                    values: KernelValues::WeightedDistances(output.distances),
                    row_count: output.row_count,
                    witness: output.witness,
                }
            }
            FnxAlgorithm::Triangles | FnxAlgorithm::ClusteringCoefficient => {
                let output = crate::clustering::run(graph, &mut checkpoint)?;
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
                let (output, workspace_bytes) =
                    crate::foundation::run(graph, algorithm, arcs, &mut checkpoint)?;
                kernel_workspace_bytes = workspace_bytes;
                output
            }
            FnxAlgorithm::Cdlp { max_iter } => {
                let (output, workspace_bytes) =
                    crate::traversal::cdlp(graph, max_iter, &mut checkpoint)?;
                kernel_workspace_bytes = workspace_bytes;
                output
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
        hash_text(&mut hash, include_str!("foundation/native.rs"));
        hash_text(&mut hash, include_str!("foundation/eigenvector.rs"));
        hash_text(&mut hash, include_str!("foundation/brandes.rs"));
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
    fn gather_partitions_require_independent_arc_ranges_and_never_spawn_empty_rows() {
        fn check(offsets: &[usize], threads: usize, expected: &[usize]) {
            let n = offsets.len() - 1;
            let mut ends = [0; MAX_GATHER_THREADS];
            let count = gather_partitions(offsets, n, offsets[n], threads, &mut ends);
            let ends = &ends[..count];
            assert_eq!(ends, expected);
            assert_eq!(ends.last(), Some(&n));
            assert!(n == 0 || ends[0] > 0);
            assert!(ends.windows(2).all(|pair| pair[0] < pair[1]));
            if count > 1 {
                let mut low = 0;
                let with_arcs = ends
                    .iter()
                    .filter(|&&high| {
                        let present = offsets[high] > offsets[low];
                        low = high;
                        present
                    })
                    .count();
                assert!(with_arcs >= 2);
            }
        }

        check(&[0], 8, &[0]);
        check(&[0, 0, 0, 0], 8, &[3]);
        check(&[0, 1, 2], 8, &[1, 2]);
        // Repeated arc targets collapse empty workers, while the final row
        // without arcs still receives its initial value in a nonempty range.
        check(&[0, 0, 4, 4, 4, 8, 8], 8, &[2, 5, 6]);
        let uniform: Vec<usize> = (0..=16).collect();
        check(&uniform, 8, &[2, 4, 6, 8, 10, 12, 14, 16]);
        check(&uniform, usize::MAX, &[2, 4, 6, 8, 10, 12, 14, 16]);
        check(&uniform, 0, &[16]);
        // A single hub cannot be split across independent row sums,
        // regardless of where its leading and trailing empty rows occur.
        for hub in 0..17 {
            let offsets: Vec<usize> = (0..=17)
                .map(|row| {
                    if row <= hub {
                        0
                    } else {
                        3 * ARITHMETIC_RUN + 137
                    }
                })
                .collect();
            for threads in 1..=8 {
                check(&offsets, threads, &[17]);
            }
        }
    }

    struct EmptyGatherMeter {
        polls: usize,
        stop: Option<usize>,
    }

    impl FnxMeter<usize> for EmptyGatherMeter {
        fn unit(&mut self) -> Result<(), usize> {
            panic!("precharged arithmetic must not spend another unit")
        }

        fn poll(&mut self) -> Result<(), usize> {
            self.polls += 1;
            if self.stop == Some(self.polls) {
                Err(self.polls)
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn empty_gather_preserves_initial_bits_and_live_polls_on_every_thread_count() {
        for n in [0, 1, 31, 65_536] {
            let offsets = vec![0; n + 1];
            for initial in [0.0, -0.0, 0.25, f64::from_bits(0x7ff8_0000_0000_0042)] {
                for weighted in [false, true] {
                    for threads in 1..=MAX_GATHER_THREADS {
                        let mut actual = vec![f64::NAN; n];
                        let mut meter = EmptyGatherMeter {
                            polls: 0,
                            stop: None,
                        };
                        gather_pass(
                            &offsets,
                            &[],
                            &[],
                            weighted.then_some(&[] as &[f64]),
                            initial,
                            &mut actual,
                            threads,
                            &mut meter,
                        )
                        .unwrap();
                        assert!(
                            actual
                                .iter()
                                .all(|value| value.to_bits() == initial.to_bits())
                        );
                        // Zero arcs select the serial live-poll walk for every
                        // requested thread count, with no empty worker polls.
                        assert_eq!(meter.polls, n.div_ceil(ARITHMETIC_RUN));
                    }
                }
            }
        }
    }

    #[test]
    fn empty_gather_bulk_fill_preserves_each_live_refusal_prefix() {
        let n = 2 * ARITHMETIC_RUN + 1;
        let offsets = vec![0; n + 1];
        for weighted in [false, true] {
            for threads in 1..=MAX_GATHER_THREADS {
                for stop in 1..=3 {
                    let mut actual = vec![f64::NAN; n];
                    let mut meter = EmptyGatherMeter {
                        polls: 0,
                        stop: Some(stop),
                    };
                    let result = gather_pass(
                        &offsets,
                        &[],
                        &[],
                        weighted.then_some(&[] as &[f64]),
                        0.25,
                        &mut actual,
                        threads,
                        &mut meter,
                    );
                    assert!(matches!(result, Err(FnxExecutionError::Cancelled(at)) if at == stop));
                    let filled = (stop - 1) * ARITHMETIC_RUN;
                    assert_eq!(meter.polls, stop);
                    assert!(actual[..filled].iter().all(|&value| value == 0.25));
                    assert!(actual[filled..].iter().all(|value| value.is_nan()));
                }
            }
        }
    }

    /// **A GATHER'S SUMS DO NOT DEPEND ON ITS THREADS.** Over a 2,000-vertex
    /// graph with a hub, with and without per-arc shares, every thread count
    /// from 1 to 8 leaves every target's sum bit-identical to one thread's,
    /// whatever the chunk boundaries. Full fixed-step PageRank also retains
    /// every score bit, witness count and scratch bound across those workers.
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
            gather_pass(
                offsets,
                sources,
                &contrib,
                shares,
                0.25,
                &mut one,
                1,
                &mut || Ok::<(), Infallible>(()),
            )
            .unwrap();
            assert_ne!(one, start, "the gather added something");
            for threads in 2..=8 {
                let mut many = start.clone();
                gather_pass(
                    offsets,
                    sources,
                    &contrib,
                    shares,
                    0.25,
                    &mut many,
                    threads,
                    &mut || Ok::<(), Infallible>(()),
                )
                .unwrap();
                assert!(
                    one.iter()
                        .zip(&many)
                        .all(|(a, b)| a.to_bits() == b.to_bits()),
                    "weighted={weighted} {threads} threads"
                );
            }
            for iterations in [2, 7] {
                let options =
                    PageRankOptions::fixed_iterations(0.85, iterations, weighted).unwrap();
                let (one, witness, workspace) =
                    pagerank_with_threads(&graph, options, sources.len(), 1, &mut || {
                        Ok::<(), Infallible>(())
                    })
                    .unwrap();
                assert_eq!(witness.nodes_touched, n * iterations);
                assert_eq!(witness.edges_scanned, sources.len() * iterations);
                for threads in 2..=8 {
                    let (many, many_witness, many_workspace) =
                        pagerank_with_threads(&graph, options, sources.len(), threads, &mut || {
                            Ok::<(), Infallible>(())
                        })
                        .unwrap();
                    assert!(
                        one.iter()
                            .zip(&many)
                            .all(|(a, b)| a.to_bits() == b.to_bits()),
                        "fixed iterations={iterations} weighted={weighted} threads={threads}"
                    );
                    assert_eq!(many_witness, witness);
                    assert_eq!(many_workspace, workspace);
                }
            }
        }
    }

    /// An interruption comes from a live poll after real arithmetic has
    /// started, with no logical work charge, even inside one hub's row.
    #[test]
    fn live_polls_interrupt_contributions_and_a_partial_hub_gather() {
        struct LiveOnly {
            polls: usize,
        }
        impl FnxMeter<&'static str> for LiveOnly {
            fn unit(&mut self) -> Result<(), &'static str> {
                panic!("precharged arithmetic must not spend another unit")
            }
            fn poll(&mut self) -> Result<(), &'static str> {
                self.polls += 1;
                if self.polls == 2 {
                    Err("live stop")
                } else {
                    Ok(())
                }
            }
        }

        let n = 3 * ARITHMETIC_RUN;
        let ranks = vec![0.5; n];
        let offsets: Vec<usize> = (0..=n).collect();
        let options = PageRankOptions::new(0.85, 100, 1e-12, false).unwrap();
        let mut values = vec![f64::NAN; n];
        let mut meter = LiveOnly { polls: 0 };
        assert!(matches!(
            contributions(options, &ranks, &offsets, &mut values, 1, &mut meter),
            Err(FnxExecutionError::Cancelled("live stop"))
        ));
        assert!(values[..ARITHMETIC_RUN].iter().all(|&x| x == 0.425));
        assert!(values[ARITHMETIC_RUN..].iter().all(|x| x.is_nan()));

        let sources = vec![0; n];
        let shares = vec![0.5; n];
        for weighted in [false, true] {
            let mut values = [f64::NAN; 2];
            let mut meter = LiveOnly { polls: 0 };
            assert!(matches!(
                gather_pass(
                    &[0, 1, n],
                    &sources,
                    &[0.5, 0.0],
                    weighted.then_some(shares.as_slice()),
                    0.25,
                    &mut values,
                    1,
                    &mut meter,
                ),
                Err(FnxExecutionError::Cancelled("live stop"))
            ));
            assert_eq!(values[0], if weighted { 0.5 } else { 0.75 });
            assert!(values[1].is_nan(), "the unfinished hub is not published");
            assert_eq!(meter.polls, 2);
        }

        // Every requested thread count takes the serial live-poll path when
        // only one partition has incoming arcs. Cancellation still reaches
        // the unfinished hub before either it or its trailing row is stored.
        for threads in 1..=8 {
            for weighted in [false, true] {
                let mut values = [f64::NAN; 3];
                let mut meter = LiveOnly { polls: 0 };
                assert!(matches!(
                    gather_pass(
                        &[0, 0, n, n],
                        &sources,
                        &[0.5, 0.0, 0.0],
                        weighted.then_some(shares.as_slice()),
                        0.25,
                        &mut values,
                        threads,
                        &mut meter,
                    ),
                    Err(FnxExecutionError::Cancelled("live stop"))
                ));
                assert_eq!(values[0], 0.25);
                assert!(values[1..].iter().all(|value| value.is_nan()));
                assert_eq!(meter.polls, 2);
            }
        }
    }

    #[test]
    fn gather_live_windows_preserve_the_scalar_completed_row_prefix() {
        struct LiveOnly {
            polls: usize,
            stop: usize,
        }
        impl FnxMeter<&'static str> for LiveOnly {
            fn unit(&mut self) -> Result<(), &'static str> {
                panic!("precharged arithmetic must not spend another unit")
            }
            fn poll(&mut self) -> Result<(), &'static str> {
                self.polls += 1;
                if self.polls == self.stop {
                    Err("live stop")
                } else {
                    Ok(())
                }
            }
        }

        let n = 10_000;
        let mut offsets = vec![0];
        for row in 0..n {
            let degree = match row {
                137 => ARITHMETIC_RUN + 37,
                6549 => 3 * ARITHMETIC_RUN + 55,
                _ => row * 7 % 5,
            };
            offsets.push(offsets.last().unwrap() + degree);
        }
        let arcs = *offsets.last().unwrap();
        let sources: Vec<_> = (0..arcs).map(|arc| arc * 13 % 19).collect();
        let contrib: Vec<_> = (0..19).map(|source| 1.0 / (source + 3) as f64).collect();
        let shares: Vec<_> = (0..arcs).map(|arc| (arc % 7) as f64 / 8.0).collect();
        for weighted in [false, true] {
            let mut expected = vec![0.125; n];
            for (row, slot) in expected.iter_mut().enumerate() {
                for arc in offsets[row]..offsets[row + 1] {
                    *slot += if weighted {
                        contrib[sources[arc]] * shares[arc]
                    } else {
                        contrib[sources[arc]]
                    };
                }
            }
            let total = n + arcs;
            for stop in 1..=total.div_ceil(ARITHMETIC_RUN) {
                let mut actual = vec![f64::NAN; n];
                let mut meter = LiveOnly { polls: 0, stop };
                let result = gather_pass(
                    &offsets,
                    &sources,
                    &contrib,
                    weighted.then_some(shares.as_slice()),
                    0.125,
                    &mut actual,
                    1,
                    &mut meter,
                );
                assert!(matches!(
                    result,
                    Err(FnxExecutionError::Cancelled("live stop"))
                ));
                assert_eq!(meter.polls, stop);
                // Polls admit windows before arithmetic starts. The scalar
                // prefix includes one operation per row and per arc, and a
                // row completed exactly at the cut must already be visible.
                let cutoff = (stop - 1) * ARITHMETIC_RUN;
                for row in 0..n {
                    if row + 1 + offsets[row + 1] <= cutoff {
                        assert_eq!(actual[row].to_bits(), expected[row].to_bits());
                    } else {
                        assert!(actual[row].is_nan());
                    }
                }
            }
        }
    }

    /// The meter and its cancellation payload deliberately contain Rc, so
    /// neither can be sent to a worker. Cancellation is triggered by actual
    /// completed worker chunks, not precharge or a clock/sleep assumption.
    #[test]
    fn parallel_arithmetic_reports_progress_and_joins_on_live_cancellation() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let completed = AtomicUsize::new(0);
        let active = AtomicUsize::new(0);
        let token = std::rc::Rc::new(std::cell::Cell::new(0));
        struct LiveParent<'a> {
            owner: std::thread::ThreadId,
            completed: &'a AtomicUsize,
            token: std::rc::Rc<std::cell::Cell<usize>>,
        }
        impl FnxMeter<std::rc::Rc<std::cell::Cell<usize>>> for LiveParent<'_> {
            fn unit(&mut self) -> Result<(), std::rc::Rc<std::cell::Cell<usize>>> {
                panic!("precharged arithmetic must not spend another unit")
            }
            fn poll(&mut self) -> Result<(), std::rc::Rc<std::cell::Cell<usize>>> {
                assert_eq!(std::thread::current().id(), self.owner);
                let completed = self.completed.load(Ordering::Acquire);
                if completed >= 2 {
                    self.token.set(completed);
                    Err(std::rc::Rc::clone(&self.token))
                } else {
                    Ok(())
                }
            }
        }
        struct Active<'a>(&'a AtomicUsize);
        impl Drop for Active<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Release);
            }
        }
        let per_worker = 32 * ARITHMETIC_RUN;
        let mut rows = vec![0.0; MAX_GATHER_THREADS * per_worker];
        let ends: Vec<usize> = (1..=MAX_GATHER_THREADS).map(|i| i * per_worker).collect();
        let mut meter = LiveParent {
            owner: std::thread::current().id(),
            completed: &completed,
            token: std::rc::Rc::clone(&token),
        };
        let result = arithmetic_rows(&mut rows, &ends, &mut meter, |_, rows, poll| {
            active.fetch_add(1, Ordering::Release);
            let _active = Active(&active);
            for chunk in rows.chunks_mut(ARITHMETIC_RUN) {
                if !poll() {
                    return;
                }
                chunk.fill(1.0);
                completed.fetch_add(1, Ordering::Release);
            }
        });
        let Err(FnxExecutionError::Cancelled(error)) = result else {
            panic!("worker progress must trigger cancellation")
        };
        assert!(std::rc::Rc::ptr_eq(&error, &token));
        assert_eq!(active.load(Ordering::Acquire), 0, "all workers joined");
        let after = completed.load(Ordering::Acquire);
        assert!(after >= 2);
        assert!(after <= token.get() + MAX_GATHER_THREADS);
        assert!(after < MAX_GATHER_THREADS * 32);
        assert_eq!(
            rows.iter().filter(|&&x| x == 1.0).count(),
            after * ARITHMETIC_RUN
        );
    }

    #[test]
    fn parallel_arithmetic_joins_peers_when_host_or_worker_unwinds() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        struct Active<'a>(&'a AtomicUsize);
        impl Drop for Active<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Release);
            }
        }
        struct PanicParent<'a> {
            owner: std::thread::ThreadId,
            completed: &'a AtomicUsize,
            at_panic: &'a AtomicUsize,
            host_panics: bool,
        }
        impl FnxMeter<Infallible> for PanicParent<'_> {
            fn unit(&mut self) -> Result<(), Infallible> {
                panic!("precharged arithmetic must not spend another unit")
            }
            fn poll(&mut self) -> Result<(), Infallible> {
                assert_eq!(std::thread::current().id(), self.owner);
                let completed = self.completed.load(Ordering::Acquire);
                if self.host_panics && completed >= 2 {
                    self.at_panic.store(completed, Ordering::Release);
                    // No global panic-hook mutation or timing dependency.
                    std::panic::resume_unwind(Box::new("host poll panic"));
                }
                Ok(())
            }
        }
        for host_panics in [true, false] {
            let completed = AtomicUsize::new(0);
            let active = AtomicUsize::new(0);
            let at_panic = AtomicUsize::new(0);
            let worker_panicked = AtomicBool::new(false);
            let per_worker = 32 * ARITHMETIC_RUN;
            let mut rows = vec![0.0; MAX_GATHER_THREADS * per_worker];
            let ends: Vec<usize> = (1..=MAX_GATHER_THREADS).map(|i| i * per_worker).collect();
            let mut meter = PanicParent {
                owner: std::thread::current().id(),
                completed: &completed,
                at_panic: &at_panic,
                host_panics,
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                arithmetic_rows(&mut rows, &ends, &mut meter, |_, rows, poll| {
                    active.fetch_add(1, Ordering::Release);
                    let _active = Active(&active);
                    for chunk in rows.chunks_mut(ARITHMETIC_RUN) {
                        if !poll() {
                            return;
                        }
                        chunk.fill(1.0);
                        let count = completed.fetch_add(1, Ordering::AcqRel) + 1;
                        // The first worker to complete actual arithmetic is
                        // the panic source, regardless of scheduling order.
                        if !host_panics && count == 2 {
                            worker_panicked.store(true, Ordering::Release);
                            std::panic::resume_unwind(Box::new("worker arithmetic panic"));
                        }
                    }
                })
            }));
            assert!(result.is_err(), "host_panics={host_panics}");
            assert_eq!(active.load(Ordering::Acquire), 0, "all workers joined");
            let after = completed.load(Ordering::Acquire);
            assert!(after < MAX_GATHER_THREADS * 32, "peers stopped early");
            if host_panics {
                assert!(after <= at_panic.load(Ordering::Acquire) + MAX_GATHER_THREADS);
            } else {
                assert!(worker_panicked.load(Ordering::Acquire));
            }
            assert_eq!(
                rows.iter().filter(|&&value| value == 1.0).count(),
                after * ARITHMETIC_RUN
            );
        }
    }

    #[test]
    fn dangling_arithmetic_checks_live_cancellation_without_recharging_rows() {
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
        let n = 3 * ARITHMETIC_RUN + 1;
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
        assert_eq!(complete.polls, n.div_ceil(ARITHMETIC_RUN));
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
    fn dangling_arithmetic_polls_without_changing_logical_charges() {
        struct DanglingMeter {
            units: usize,
            phase: usize,
            polls: usize,
        }
        impl FnxMeter<&'static str> for DanglingMeter {
            fn unit(&mut self) -> Result<(), &'static str> {
                self.units += 1;
                Ok(())
            }
            fn poll(&mut self) -> Result<(), &'static str> {
                if self.units == self.phase {
                    self.polls += 1;
                    if self.polls == 2 {
                        return Err("dangling stop");
                    }
                }
                Ok(())
            }
        }
        let n = 3 * ARITHMETIC_RUN;
        let vertices: Vec<VId> = (0..n as u128).map(VId).collect();
        let graph = SnapshotGraphView::build(
            SnapshotBinding {
                root: ObjectId([2; 32]),
                as_of: CommitSeq(3),
            },
            &vertices,
            &[],
            ProjectionSpec {
                directedness: Directedness::Directed,
                parallel_edges: ParallelEdgePolicy::Sum,
                self_loops: SelfLoopPolicy::Keep,
            },
            ProjectionLimits {
                max_vertices: n,
                max_input_edges: 0,
                max_adjacency_entries: 0,
                max_workspace_bytes: 1 << 26,
            },
        )
        .unwrap();
        let mut meter = DanglingMeter {
            units: 0,
            phase: 2 * n,
            polls: 0,
        };
        let result = pagerank(&graph, PageRankOptions::default(), 0, &mut meter);
        assert!(matches!(
            result,
            Err(FnxExecutionError::Cancelled("dangling stop"))
        ));
        assert_eq!(meter.units, 2 * n, "live checks spend no work units");
        assert_eq!(meter.polls, 2);
    }

    /// An independent unchunked scalar oracle checks the exact floating-point
    /// order across several live-poll boundaries inside a single hub row.
    #[test]
    fn gather_matches_unchunked_scalar_bits_across_large_hub_quanta() {
        let hub = 7usize;
        let hub_degree = 3 * ARITHMETIC_RUN + 137;
        let n = hub_degree + 19;
        let mut offsets = Vec::with_capacity(n + 1);
        let mut sources = Vec::new();
        offsets.push(0);
        for target in 0..n {
            let degree = if target == hub {
                hub_degree
            } else if target == n / 2 {
                ARITHMETIC_RUN + 19
            } else {
                target % 5
            };
            let start = (target * 17 + 11) % (n - degree + 1);
            sources.extend(start..start + degree);
            offsets.push(sources.len());
        }
        let contrib: Vec<f64> = (0..n)
            .map(|vertex| match vertex % 4 {
                0 => 1.0 / (vertex as f64 + 3.0),
                1 => 1e-15 / ((vertex % 29 + 3) as f64),
                _ => (vertex % 97 + 1) as f64 / (n as f64 * 113.0),
            })
            .collect();
        let shares: Vec<f64> = (0..sources.len())
            .map(|arc| ((arc * 13 + 5) % 37 + 1) as f64 / 41.0)
            .collect();
        let initial = 1.0 / 7.0;

        for weighted in [false, true] {
            let expected: Vec<f64> = offsets
                .windows(2)
                .map(|bounds| {
                    let mut sum = initial;
                    for arc in bounds[0]..bounds[1] {
                        let value = contrib[sources[arc]];
                        if weighted {
                            sum += value * shares[arc];
                        } else {
                            sum += value;
                        }
                    }
                    sum
                })
                .collect();
            for threads in 1..=8 {
                let mut actual = vec![f64::NAN; n];
                gather_pass(
                    &offsets,
                    &sources,
                    &contrib,
                    weighted.then_some(shares.as_slice()),
                    initial,
                    &mut actual,
                    threads,
                    &mut || Ok::<(), Infallible>(()),
                )
                .unwrap();
                for (row, (&actual, &expected)) in actual.iter().zip(&expected).enumerate() {
                    assert_eq!(
                        actual.to_bits(),
                        expected.to_bits(),
                        "weighted={weighted} threads={threads} row={row}"
                    );
                }
            }
        }
    }

    /// Paired arithmetic-only control for the cost of live polling. The
    /// predecessor runs in this same invocation with the same incoming CSR,
    /// scalar order, thread count and complete output consumption. These are
    /// raw machine-local samples, not database-query or GAPBS throughput.
    #[test]
    #[ignore = "paired PageRank gather measurement; run with --ignored --nocapture"]
    fn pagerank_live_gather_paired_measurement() {
        fn predecessor(
            offsets: &[usize],
            sources: &[usize],
            contributions: &[f64],
            next: &mut [f64],
            threads: usize,
        ) {
            let gather = |low: usize, rows: &mut [f64]| {
                for (at, slot) in rows.iter_mut().enumerate() {
                    let target = low + at;
                    let mut sum = 0.25;
                    for &source in &sources[offsets[target]..offsets[target + 1]] {
                        sum += contributions[source];
                    }
                    *slot = sum;
                }
            };
            if threads == 1 {
                gather(0, next);
                return;
            }
            let n = next.len();
            let arcs = sources.len();
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
        let n = 65_536;
        let m = n * 16;
        let sources: Vec<usize> = (0..m).map(|arc| (arc * 17 + arc / 16) % n).collect();
        let contrib: Vec<f64> = (0..n).map(|at| 1.0 / (at + 1) as f64).collect();
        for shape in ["short_rows", "giant_hub"] {
            let offsets: Vec<usize> = (0..=n)
                .map(|at| {
                    if shape == "giant_hub" && at > 0 {
                        m
                    } else {
                        at * 16
                    }
                })
                .collect();
            for threads in [1, 8] {
                let mut prior = vec![0.0; n];
                let mut live = vec![0.0; n];
                let mut prior_samples = Vec::new();
                let mut live_samples = Vec::new();
                for trial in 0..10 {
                    for new in if trial % 2 == 0 {
                        [false, true]
                    } else {
                        [true, false]
                    } {
                        let started = std::time::Instant::now();
                        let rows = if new {
                            gather_pass(
                                &offsets,
                                &sources,
                                &contrib,
                                None,
                                0.25,
                                &mut live,
                                threads,
                                &mut || {
                                    std::hint::black_box(());
                                    Ok::<(), Infallible>(())
                                },
                            )
                            .unwrap();
                            &live
                        } else {
                            predecessor(&offsets, &sources, &contrib, &mut prior, threads);
                            &prior
                        };
                        let digest = rows
                            .iter()
                            .fold(0u64, |sum, value| sum.wrapping_add(value.to_bits()));
                        std::hint::black_box(digest);
                        let elapsed = started.elapsed().as_nanos();
                        if trial != 0 {
                            if new {
                                live_samples.push(elapsed);
                            } else {
                                prior_samples.push(elapsed);
                            }
                        }
                    }
                    assert!(
                        live.iter()
                            .zip(&prior)
                            .all(|(a, b)| a.to_bits() == b.to_bits())
                    );
                }
                println!(
                    "pagerank_gather shape={shape} vertices={n} arcs={m} requested_threads={threads} arithmetic_run={ARITHMETIC_RUN} predecessor_ns={prior_samples:?} live_ns={live_samples:?}"
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
