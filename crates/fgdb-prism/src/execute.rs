//! Checkpointed analytics over immutable decoded rows. Native kernels are
//! differentially checked against their registered reference semantics;
//! certificates identify their own source instead of claiming that fnx
//! executed the computation.

use crate::{
    AdapterPath, BfsDecisionCard, ComponentDecisionCard, FNX_IMPLEMENTATION_REVISION,
    FNX_SIGNATURE_REGISTRY_VERSION, FnxAlgorithm, FnxCallSpec, FnxGraphKind, FnxOutput, GraphView,
    SealedGraphView, SnapshotBinding, SnapshotGraphView,
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
    /// WCC n + 2*arcs, SCC 2*(n + arcs). Dijkstra includes heap sift height,
    /// or 5*n+2*arcs when proven unit weights permit an exact FIFO traversal.
    /// Triangle kernels include forward-CSR construction and a lower-degree
    /// row bound per undirected edge, plus initialization and conversion.
    /// CDLP includes graph-law checks, histogram initialization, all synchronous
    /// passes and result conversion, including a zero-iteration result.
    /// Directed clustering includes weak-neighbor preparation and bounds each
    /// forward intersection by the smaller endpoint's incidence degree.
    /// Directed degree admits n reads from the incoming or outgoing degree
    /// directory; projection admission is separate.
    /// HITS includes both row passes and all scalar normalization passes for
    /// its fixed 100-update cap, plus initialization and output.
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

/// One canonical result cell, including its type tag. Feeding the complete
/// fixed-width record at once preserves the v3 transcript while avoiding two
/// tiny hasher updates per cell. No row buffer or new checkpoint is needed.
#[inline]
pub(crate) fn hash_result_value(hash: &mut Hasher, value: FnxValue) {
    let mut bytes = [0_u8; 17];
    let len = match value {
        FnxValue::Vertex(vertex) => {
            bytes[1..].copy_from_slice(&vertex.0.to_le_bytes());
            17
        }
        FnxValue::Score(score) => {
            bytes[0] = 1;
            bytes[1..9].copy_from_slice(&score.to_bits().to_le_bytes());
            9
        }
        FnxValue::Integer(value) => {
            bytes[0] = 2;
            bytes[1..9].copy_from_slice(&value.to_le_bytes());
            9
        }
        FnxValue::Float(value) => {
            bytes[0] = 3;
            bytes[1..9].copy_from_slice(&value.to_bits().to_le_bytes());
            9
        }
    };
    hash.update(&bytes[..len]);
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
    /// kernel indexes, normalized weights and traversal decision evidence;
    /// adaptive traversals record reserved capacities at the allocation peak.
    pub kernel_workspace_bytes: usize,
    /// Replayable physical direction choices when decoded BFS completes a
    /// pull level. Absent when no pull was executed, including other kernels
    /// and compressed-row execution.
    pub bfs_decision_card: Option<BfsDecisionCard>,
    /// Replayable physical choices when decoded CC/WCC completes a pull.
    /// Absent for ordinary component BFS and compressed-row execution.
    pub component_decision_card: Option<ComponentDecisionCard>,
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

/// A private destination for decoded result rows. The executor borrows one
/// reusable row buffer; a sink may copy or convert its cells directly into
/// the host's final representation without retaining a second result table.
///
/// Rows are provisional until [`FnxCallSpec::execute_into`] returns a
/// certificate. On any error, the sink's entire partial state must be
/// discarded. This is not a streaming delivery or publication interface.
pub trait FnxRowSink<C> {
    /// Start a new result with its exact admitted shape.
    fn begin(&mut self, rows: usize, columns: usize) -> Result<(), FnxExecutionError<C>>;

    /// Consume one borrowed row in canonical output order. The slice is
    /// reused for the next row and must not be retained by the sink.
    fn row(&mut self, values: &[FnxValue]) -> Result<(), FnxExecutionError<C>>;
}

impl<C> FnxRowSink<C> for Vec<Vec<FnxValue>> {
    fn begin(&mut self, rows: usize, _: usize) -> Result<(), FnxExecutionError<C>> {
        *self = reserve(rows)?;
        Ok(())
    }

    fn row(&mut self, values: &[FnxValue]) -> Result<(), FnxExecutionError<C>> {
        let mut row = reserve(values.len())?;
        row.extend_from_slice(values);
        self.push(row);
        Ok(())
    }
}

// Keep the standalone collector's original allocation/move loop. Mapped
// hosts reuse a row buffer, but raw FnX callers need neither a per-cell copy
// nor a dynamic sink call for each result row.
enum ResultSink<'a, C> {
    Collected(&'a mut Vec<Vec<FnxValue>>),
    Mapped(&'a mut dyn FnxRowSink<C>),
    Retained {
        values: &'a mut Option<KernelValues>,
        max_integer: &'a mut Option<u64>,
        rows: &'a mut usize,
    },
}

/// A completed, certified result retaining the kernel's existing value arrays.
/// Rows are decoded on demand without an additional table of row allocations.
/// The decoded or compressed projection is clone-shared; algorithm vectors and
/// that projection remain in memory. Pulling completed rows is synchronous and
/// does not make the retained result an external-memory representation.
pub struct FnxRowCursor {
    graph: RowProjection,
    fields: Vec<FnxOutput>,
    values: KernelValues,
    next_index: usize,
    remaining: usize,
    max_integer: Option<u64>,
    certificate: FnxCertificate,
}

enum RowProjection {
    Decoded(SnapshotGraphView),
    Sealed(SealedGraphView),
}

impl RowProjection {
    fn vertex_ids(&self) -> &[VId] {
        match self {
            Self::Decoded(graph) => graph.vertex_ids(),
            Self::Sealed(graph) => graph.vertex_ids(),
        }
    }
}

impl core::fmt::Debug for FnxRowCursor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FnxRowCursor")
            .field("remaining", &self.remaining)
            .finish_non_exhaustive()
    }
}

impl FnxRowCursor {
    fn completed<C>(
        call: &FnxCallSpec,
        graph: RowProjection,
        values: KernelValues,
        remaining: usize,
        max_integer: Option<u64>,
        certificate: FnxCertificate,
    ) -> Result<Self, FnxExecutionError<C>> {
        let mut fields = reserve(call.outputs().len())?;
        fields.extend(call.outputs().iter().map(|column| column.field));
        Ok(Self {
            graph,
            fields,
            values,
            next_index: 0,
            remaining,
            max_integer,
            certificate,
        })
    }

    /// Called only after the compressed encoder has validated every value and
    /// completed the certificate and final live check. No guard enters the cursor.
    pub(crate) fn completed_sealed(
        fields: Vec<FnxOutput>,
        graph: &SealedGraphView,
        values: KernelValues,
        remaining: usize,
        max_integer: Option<u64>,
        certificate: FnxCertificate,
    ) -> Self {
        Self {
            graph: RowProjection::Sealed(graph.clone()),
            fields,
            values,
            next_index: 0,
            remaining,
            max_integer,
            certificate,
        }
    }

    /// The same completed certificate as the collected and mapped paths.
    pub fn certificate(&self) -> &FnxCertificate {
        &self.certificate
    }

    /// Exact number of rows still available, including sparse reachability.
    pub fn remaining(&self) -> usize {
        self.remaining
    }

    /// Maximum selected integer cell, checked during canonical result hashing.
    /// Hosts with a narrower integer domain can reject the complete result
    /// before consuming any row. Unselected count/distance fields do not enter it.
    pub fn max_integer(&self) -> Option<u64> {
        self.max_integer
    }

    /// Fill a reusable row buffer. Every examined vertex checks live control,
    /// including unreachable gaps; a cancellation leaves the current vertex
    /// unconsumed. The buffer is empty on exhaustion and provisional on error.
    /// No successful call can fail a value-domain validation: the complete
    /// immutable result was already validated and hashed before construction.
    pub fn next_row<C>(
        &mut self,
        row: &mut Vec<FnxValue>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<bool, FnxExecutionError<C>> {
        row.clear();
        while self.next_index < self.graph.vertex_ids().len() {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            let index = self.next_index;
            if !self.values.contains(index) {
                self.next_index += 1;
                continue;
            }
            row.try_reserve(self.fields.len())
                .map_err(|_| FnxExecutionError::AllocationFailed)?;
            for &field in &self.fields {
                row.push(self.values.value(index, field, self.graph.vertex_ids())?);
            }
            self.next_index += 1;
            self.remaining -= 1;
            return Ok(true);
        }
        debug_assert_eq!(self.remaining, 0);
        Ok(false)
    }
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
    HubsAndAuthorities {
        hubs: Vec<f64>,
        authorities: Vec<f64>,
    },
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
            Self::HubsAndAuthorities { hubs, authorities } => {
                index < hubs.len() && index < authorities.len()
            }
            Self::Distances(values) => values.get(index).is_some_and(Option::is_some),
            Self::Components(values) => index < values.len(),
            Self::WeightedDistances(values) => values.get(index).is_some_and(Option::is_some),
            Self::Counts(values) => index < values.len(),
        }
    }
    pub(crate) fn value<C>(
        &self,
        index: usize,
        field: FnxOutput,
        vertices: &[VId],
    ) -> Result<FnxValue, FnxExecutionError<C>> {
        match (field, self) {
            (FnxOutput::Vertex, _) => vertices
                .get(index)
                .copied()
                .map(FnxValue::Vertex)
                .ok_or(FnxExecutionError::InvalidUpstreamResult),
            (FnxOutput::Score, Self::Scores(values)) => values
                .get(index)
                .copied()
                .map(FnxValue::Score)
                .ok_or(FnxExecutionError::InvalidUpstreamResult),
            (FnxOutput::Hub, Self::HubsAndAuthorities { hubs, .. }) => hubs
                .get(index)
                .copied()
                .map(FnxValue::Score)
                .ok_or(FnxExecutionError::InvalidUpstreamResult),
            (FnxOutput::Authority, Self::HubsAndAuthorities { authorities, .. }) => authorities
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
                .and_then(|&label| vertices.get(label).copied())
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

/// Per-vertex restart mass without an additional graph-sized vector. Uniform
/// calls retain the original scalar expressions; personalization changes only
/// the seed's initial accumulator, before any source-ordered contributions.
#[derive(Clone, Copy)]
pub(crate) struct PageRankRestart {
    source: Option<usize>,
    value: f64,
}

impl PageRankRestart {
    pub(crate) fn new(
        source: Option<usize>,
        alpha: f64,
        population: f64,
        dangling_mass: f64,
    ) -> Self {
        let value = if source.is_some() {
            (1.0 - alpha) + alpha * dangling_mass
        } else {
            (1.0 - alpha) / population + alpha * dangling_mass / population
        };
        Self { source, value }
    }

    pub(crate) fn at(self, target: usize) -> f64 {
        if self.source.is_none_or(|source| source == target) {
            self.value
        } else {
            0.0
        }
    }

    fn fill(self, low: usize, slots: &mut [f64]) {
        if let Some(source) = self.source {
            slots.fill(0.0);
            if let Some(at) = source.checked_sub(low).and_then(|at| slots.get_mut(at)) {
                *at = self.value;
            }
        } else {
            slots.fill(self.value);
        }
    }
}

impl From<f64> for PageRankRestart {
    fn from(value: f64) -> Self {
        Self {
            source: None,
            value,
        }
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
    initial: impl Into<PageRankRestart>,
    next: &mut [f64],
    threads: usize,
    meter: &mut impl FnxMeter<C>,
) -> Result<(), FnxExecutionError<C>> {
    let n = next.len();
    let arcs = sources.len();
    let initial = initial.into();
    let gather = |low: usize, rows: &mut [f64], poll: &mut dyn FnMut() -> bool| {
        if sources.is_empty() {
            // No target adds anything to its initial value. Keep the scalar
            // row walk's live-poll boundaries while filling admitted chunks.
            for (chunk, slots) in rows.chunks_mut(ARITHMETIC_RUN).enumerate() {
                if !poll() {
                    return;
                }
                initial.fill(low + chunk * ARITHMETIC_RUN, slots);
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
            let mut sum = initial.at(target);
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
    alpha: f64,
    per_arc_weights: bool,
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
                let push = alpha * ranks[index];
                *slot = if per_arc_weights {
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
/// execution retains one normalized share per incoming arc unless immutable
/// final weights are all one. Unit-weight execution borrows CSR degrees;
/// its weighted call identity and logical validation charges are unchanged.
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
    let source = options
        .source()
        .map(|source| {
            graph
                .vertex_ordinal(source)
                .ok_or(FnxExecutionError::UnknownSource(source))
        })
        .transpose()?;
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
    // Summing ones agrees bit-for-bit with the integer degree conversion only
    // through 2^53. Keep the ordinary weighted path above that exactness bound.
    let unit_weights =
        graph.has_unit_weights() && graph.component_degree_bound(false) as u128 <= (1u128 << 53);
    #[cfg(test)]
    let unit_weights = unit_weights && !tests::force_weighted_shares();
    let per_arc_weights = options.weighted() && !unit_weights;
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
    if per_arc_weights && face_ok {
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
        if per_arc_weights {
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
            if options.weighted() {
                // The immutable evidence removes arithmetic and allocation,
                // not any unit of the weighted call's validation transcript.
                charge_runs(targets.len(), meter)?;
            }
            sum = targets.len() as f64;
        }
        if face_ok {
            face_ok = in_offsets[source] <= in_offsets[source + 1];
        }
        if per_arc_weights && face_ok {
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
    for iteration in 0..options.max_iter() {
        // Each pass spends one unit per vertex or arc, as before, but asks
        // the meter for up to FNX_METER_RUN of them at once; the arithmetic
        // and its order are unchanged.
        // The dangling walk still spends a unit per row, charged first; it
        // adds exactly the dangling rows' ranks, ascending, as a full walk
        // that skips the others does.
        charge_runs(n, meter)?;
        let dangling_mass = dangling_mass(&dangling, &ranks, meter)?;
        let initial = PageRankRestart::new(source, options.alpha(), population, dangling_mass);
        // The initial values are the gather's starting sums; the
        // contribution walk spends its unit per row first.
        charge_runs(n, meter)?;
        contributions(
            options.alpha(),
            per_arc_weights,
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
            per_arc_weights.then_some(shares.as_slice()),
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
    /// Execute a bound in-core call. Native kernels, including catalog entries,
    /// check live cancellation within graph passes. Kernel workspace
    /// depends on the kernel; weighted PageRank retains O(n+m) scratch while
    /// borrowing the projection's adjacency.
    /// The callback may cancel but this synchronous API does not provide an
    /// asynchronous bulkhead, a deadline guarantee or external-memory spill.
    pub fn execute<C>(
        &self,
        graph: &SnapshotGraphView,
        limits: FnxExecutionLimits,
        checkpoint: impl FnxMeter<C>,
    ) -> Result<FnxResult, FnxExecutionError<C>> {
        let mut rows = Vec::new();
        let certificate =
            self.execute_with_sink(graph, limits, checkpoint, ResultSink::Collected(&mut rows))?;
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

    /// Execute the same admitted call and canonical certificate as
    /// [`Self::execute`], converting rows directly into a host-owned sink.
    /// Result hashing, limits and live meter calls do not depend on the sink's
    /// representation. Its rows remain private and discard-only until this
    /// method returns successfully, including the final cancellation check.
    pub fn execute_into<C>(
        &self,
        graph: &SnapshotGraphView,
        limits: FnxExecutionLimits,
        checkpoint: impl FnxMeter<C>,
        sink: &mut dyn FnxRowSink<C>,
    ) -> Result<FnxCertificate, FnxExecutionError<C>> {
        self.execute_with_sink(graph, limits, checkpoint, ResultSink::Mapped(sink))
    }

    /// Complete the kernel, result hash and certificate before returning an
    /// owned cursor over its existing value arrays. The same logical meter
    /// trace and cancellation boundary as [`Self::execute`] is retained.
    /// Cloning the admitted projection shares its immutable storage.
    pub fn execute_rows<C>(
        &self,
        graph: &SnapshotGraphView,
        limits: FnxExecutionLimits,
        checkpoint: impl FnxMeter<C>,
    ) -> Result<FnxRowCursor, FnxExecutionError<C>> {
        let mut values = None;
        let mut max_integer = None;
        let mut remaining = 0;
        let certificate = self.execute_with_sink(
            graph,
            limits,
            checkpoint,
            ResultSink::Retained {
                values: &mut values,
                max_integer: &mut max_integer,
                rows: &mut remaining,
            },
        )?;
        let values = values.ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        FnxRowCursor::completed(
            self,
            RowProjection::Decoded(graph.clone()),
            values,
            remaining,
            max_integer,
            certificate,
        )
    }

    fn hash_retained_rows<C>(
        &self,
        graph: &SnapshotGraphView,
        values: &KernelValues,
        result_hash: &mut Hasher,
        checkpoint: &mut impl FnxMeter<C>,
    ) -> Result<(usize, Option<u64>), FnxExecutionError<C>> {
        let mut emitted = 0usize;
        let mut max_integer = None;
        for index in 0..graph.node_count() {
            checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
            if !values.contains(index) {
                continue;
            }
            for column in self.outputs() {
                let value = values.value(index, column.field, graph.vertex_ids())?;
                hash_result_value(result_hash, value);
                if let FnxValue::Integer(value) = value {
                    max_integer = Some(max_integer.map_or(value, |prior: u64| prior.max(value)));
                }
            }
            emitted += 1;
        }
        Ok((emitted, max_integer))
    }

    fn execute_with_sink<C>(
        &self,
        graph: &SnapshotGraphView,
        limits: FnxExecutionLimits,
        mut checkpoint: impl FnxMeter<C>,
        sink: ResultSink<'_, C>,
    ) -> Result<FnxCertificate, FnxExecutionError<C>> {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        let algorithm = self.algorithm();
        let signature = self.signature();
        let mut execution_kernel = signature.execution_kernel;
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
            FnxAlgorithm::HitsCentrality => {
                admit("iterations", crate::hits::MAX_ITERATIONS, limits.max_iterations)?;
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
        // The builder already checked every row boundary, and its final
        // immutable offset is the exact arc count (including both directions of
        // undirected edges, but only one incidence per self-loop). Retain the
        // scan's logical admission units and refusal order without reopening
        // every row on every CALL. A scalar meter still sees every unit.
        charge_runs(n, &mut checkpoint)?;
        let arcs = *graph
            .outgoing_offsets()
            .last()
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
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
                Some(crate::shortest_path::decoded_work::<C>(graph, arcs)?)
            }
            FnxAlgorithm::Triangles | FnxAlgorithm::ClusteringCoefficient => {
                Some(crate::clustering::estimated_work(graph, &mut checkpoint)?)
            }
            FnxAlgorithm::DirectedClusteringCoefficient => Some(
                crate::directed_clustering::estimated_work(graph, &mut checkpoint)?,
            ),
            FnxAlgorithm::InDegreeCentrality | FnxAlgorithm::OutDegreeCentrality => Some(n),
            FnxAlgorithm::HitsCentrality => {
                Some(crate::hits::estimated_work::<C>(n, arcs, n, self.outputs().len())?)
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
            FnxAlgorithm::PageRank(_)
            | FnxAlgorithm::Cdlp { .. }
            | FnxAlgorithm::SingleSourceDijkstraPathLength(_) => 0,
            FnxAlgorithm::Triangles | FnxAlgorithm::ClusteringCoefficient => {
                crate::clustering::workspace_bytes::<C>(n, graph.edge_count())?
            }
            // Native catalog kernels return their reserved vector peak.
            FnxAlgorithm::Foundation(_)
            | FnxAlgorithm::DirectedClusteringCoefficient
            | FnxAlgorithm::InDegreeCentrality
            | FnxAlgorithm::OutDegreeCentrality
            | FnxAlgorithm::HitsCentrality => 0,
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
        let mut bfs_decision_card = None;
        let mut component_decision_card = None;
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
                let run = crate::shortest_path::run(
                    graph,
                    options,
                    limits.max_result_rows,
                    &mut checkpoint,
                )?;
                kernel_workspace_bytes = run.workspace_bytes;
                execution_kernel = run.execution_kernel;
                let output = run.output;
                KernelOutput {
                    values: KernelValues::WeightedDistances(output.distances),
                    row_count: output.row_count,
                    witness: output.witness,
                }
            }
            FnxAlgorithm::Triangles => {
                let (counts, witness) = crate::clustering::counts(graph, &mut checkpoint)?;
                KernelOutput {
                    values: KernelValues::Counts(counts),
                    row_count: n,
                    witness,
                }
            }
            FnxAlgorithm::ClusteringCoefficient => {
                let output = crate::clustering::run(graph, &mut checkpoint)?;
                KernelOutput {
                    values: KernelValues::Scores(output.clustering),
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
            FnxAlgorithm::InDegreeCentrality | FnxAlgorithm::OutDegreeCentrality => {
                let (output, workspace_bytes) = crate::foundation::directed_degree(
                    graph,
                    matches!(algorithm, FnxAlgorithm::InDegreeCentrality),
                    &mut checkpoint,
                )?;
                kernel_workspace_bytes = workspace_bytes;
                output
            }
            FnxAlgorithm::Cdlp { max_iter } => {
                let (output, workspace_bytes) =
                    crate::traversal::cdlp(graph, max_iter, &mut checkpoint)?;
                kernel_workspace_bytes = workspace_bytes;
                output
            }
            FnxAlgorithm::DirectedClusteringCoefficient => {
                let (output, workspace_bytes) =
                    crate::directed_clustering::run(graph, &mut checkpoint)?;
                kernel_workspace_bytes = workspace_bytes;
                output
            }
            FnxAlgorithm::HitsCentrality => {
                let (output, workspace_bytes) = crate::hits::run(graph, &mut checkpoint)?;
                kernel_workspace_bytes = workspace_bytes;
                output
            }
            _ => {
                let traversal = crate::traversal::run(
                    graph,
                    algorithm,
                    limits.max_result_rows,
                    &mut checkpoint,
                )?;
                if let Some(bytes) = traversal.workspace_bytes {
                    kernel_workspace_bytes = bytes;
                }
                bfs_decision_card = traversal.bfs_decision_card;
                component_decision_card = traversal.component_decision_card;
                traversal.output
            }
        };
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        let mut result_hash = Hasher::new();
        result_hash.update(b"fgdb:prism:result-rows:v3");
        result_hash.update(&(output.row_count as u128).to_le_bytes());
        result_hash.update(&self.digest().0);
        let mut retained = None;
        let emitted = match sink {
            ResultSink::Collected(rows) => {
                *rows = reserve(output.row_count)?;
                for index in 0..n {
                    checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
                    if !output.values.contains(index) {
                        continue;
                    }
                    let mut row = reserve(self.outputs().len())?;
                    for column in self.outputs() {
                        let value = output
                            .values
                            .value(index, column.field, graph.vertex_ids())?;
                        hash_result_value(&mut result_hash, value);
                        row.push(value);
                    }
                    rows.push(row);
                }
                rows.len()
            }
            ResultSink::Mapped(sink) => {
                sink.begin(output.row_count, self.outputs().len())?;
                let mut row = Vec::new();
                let mut emitted = 0usize;
                for index in 0..n {
                    checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
                    if !output.values.contains(index) {
                        continue;
                    }
                    if emitted == 0 {
                        row = reserve(self.outputs().len())?;
                    }
                    row.clear();
                    for column in self.outputs() {
                        let value = output
                            .values
                            .value(index, column.field, graph.vertex_ids())?;
                        hash_result_value(&mut result_hash, value);
                        row.push(value);
                    }
                    sink.row(&row)?;
                    emitted += 1;
                }
                emitted
            }
            ResultSink::Retained {
                values,
                max_integer,
                rows,
            } => {
                *rows = output.row_count;
                let (emitted, maximum) = self.hash_retained_rows(
                    graph,
                    &output.values,
                    &mut result_hash,
                    &mut checkpoint,
                )?;
                *max_integer = maximum;
                retained = Some(values);
                emitted
            }
        };
        if emitted != output.row_count {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        let witness = output.witness;
        let kernel_source_digest = kernel_source_digest();
        let mut hash = Hasher::new();
        hash.update(b"fgdb:prism:call-certificate:v5");
        hash.update(&FNX_SIGNATURE_REGISTRY_VERSION.to_le_bytes());
        hash.update(FNX_IMPLEMENTATION_REVISION.as_bytes());
        hash_text(&mut hash, execution_kernel);
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
        hash_bfs_decision_card(&mut hash, bfs_decision_card.as_ref(), &mut || {
            checkpoint.unit()
        })
        .map_err(FnxExecutionError::Cancelled)?;
        hash_component_decision_card(&mut hash, component_decision_card.as_ref(), &mut || {
            checkpoint.unit()
        })
        .map_err(FnxExecutionError::Cancelled)?;
        let certificate = FnxCertificate {
            registry_version: FNX_SIGNATURE_REGISTRY_VERSION,
            implementation_revision: FNX_IMPLEMENTATION_REVISION,
            execution_kernel,
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
            bfs_decision_card,
            component_decision_card,
            witness,
            digest: hash.finalize(),
        };
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        if let Some(retained) = retained {
            *retained = Some(output.values);
        }
        Ok(certificate)
    }
}
fn hash_text(hash: &mut Hasher, text: &str) {
    hash.update(&(text.len() as u128).to_le_bytes());
    hash.update(text.as_bytes());
}

/// The certificate binds the optional policy and every ordered, completed
/// pull decision. Like row conversion, evidence encoding has a checkpoint for
/// each bounded record; absent evidence adds no work units.
pub(crate) fn hash_bfs_decision_card<C>(
    hash: &mut Hasher,
    card: Option<&BfsDecisionCard>,
    checkpoint: &mut impl FnMut() -> Result<(), C>,
) -> Result<(), C> {
    let Some(card) = card else {
        hash.update(&[0]);
        return Ok(());
    };
    hash.update(&[1]);
    hash.update(&card.policy_version.to_le_bytes());
    hash_text(hash, card.fallback);
    for number in [
        card.dwell_levels,
        card.pull_cost_multiplier,
        card.pulls.len(),
    ] {
        hash.update(&(number as u128).to_le_bytes());
    }
    for decision in &card.pulls {
        checkpoint()?;
        for number in [
            decision.depth,
            decision.frontier_vertices,
            decision.frontier_arcs,
            decision.unseen_incoming_arcs,
            decision.vertices,
            decision.benefit_lower,
            decision.benefit_upper,
            decision.candidate_visits,
            decision.incoming_visits,
        ] {
            hash.update(&(number as u128).to_le_bytes());
        }
    }
    Ok(())
}

/// The v5 certificate appends connectivity choices after the BFS policy.
/// Absence costs no logical units; each completed record has one checkpoint.
pub(crate) fn hash_component_decision_card<C>(
    hash: &mut Hasher,
    card: Option<&ComponentDecisionCard>,
    checkpoint: &mut impl FnMut() -> Result<(), C>,
) -> Result<(), C> {
    let Some(card) = card else {
        hash.update(&[0]);
        return Ok(());
    };
    hash.update(&[1]);
    hash.update(&card.policy_version.to_le_bytes());
    hash_text(hash, card.fallback);
    for number in [
        card.dwell_frontiers,
        card.pull_cost_multiplier,
        card.pulls.len(),
    ] {
        hash.update(&(number as u128).to_le_bytes());
    }
    for decision in &card.pulls {
        checkpoint()?;
        for number in [
            decision.root,
            decision.frontier_vertices,
            decision.frontier_incidences,
            decision.unseen_incidences,
            decision.vertices,
            decision.benefit_lower,
            decision.benefit_upper,
            decision.candidate_visits,
            decision.incidence_visits,
        ] {
            hash.update(&(number as u128).to_le_bytes());
        }
    }
    Ok(())
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
        hash_text(&mut hash, include_str!("directed_clustering.rs"));
        hash_text(&mut hash, include_str!("foundation.rs"));
        hash_text(&mut hash, include_str!("foundation/native.rs"));
        hash_text(&mut hash, include_str!("foundation/eigenvector.rs"));
        hash_text(&mut hash, include_str!("foundation/brandes.rs"));
        hash_text(&mut hash, include_str!("foundation/distance.rs"));
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
    use std::mem::size_of;

    thread_local! {
        // Retain the predecessor as a differential and paired-measurement
        // control. This switch does not exist in production builds.
        static FORCE_WEIGHTED_SHARES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    pub(super) fn force_weighted_shares() -> bool {
        FORCE_WEIGHTED_SHARES.with(std::cell::Cell::get)
    }

    fn with_weighted_shares<T>(forced: bool, run: impl FnOnce() -> T) -> T {
        struct Reset(bool);
        impl Drop for Reset {
            fn drop(&mut self) {
                FORCE_WEIGHTED_SHARES.with(|mode| mode.set(self.0));
            }
        }
        let _reset = Reset(FORCE_WEIGHTED_SHARES.with(|mode| mode.replace(forced)));
        run()
    }

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

    struct RecordingMeter<'a, const RUNS: bool>(&'a mut Recording);

    impl<const RUNS: bool> FnxMeter<&'static str> for RecordingMeter<'_, RUNS> {
        fn poll(&mut self) -> Result<(), &'static str> {
            self.0.poll()
        }
        fn unit(&mut self) -> Result<(), &'static str> {
            self.0.unit()
        }
        fn units(&mut self, n: usize) -> Result<(), &'static str> {
            if RUNS {
                self.0.units(n)
            } else {
                for _ in 0..n {
                    self.unit()?;
                }
                Ok(())
            }
        }
    }

    #[test]
    fn bfs_decision_cards_bind_ordered_records_and_checkpoint_before_each_record() {
        let card = BfsDecisionCard {
            policy_version: 0x0102,
            fallback: "top_down",
            dwell_levels: 3,
            pull_cost_multiplier: 4,
            pulls: vec![
                crate::BfsPullDecision {
                    depth: 5,
                    frontier_vertices: 6,
                    frontier_arcs: 7,
                    unseen_incoming_arcs: 8,
                    vertices: 9,
                    benefit_lower: 10,
                    benefit_upper: 11,
                    candidate_visits: 12,
                    incoming_visits: 13,
                },
                crate::BfsPullDecision {
                    depth: 14,
                    frontier_vertices: 15,
                    frontier_arcs: 16,
                    unseen_incoming_arcs: 17,
                    vertices: 18,
                    benefit_lower: 19,
                    benefit_upper: 20,
                    candidate_visits: 21,
                    incoming_visits: usize::MAX,
                },
            ],
        };
        // Explicit protocol bytes, independently of the streaming helper:
        // presence, u16 policy, text length/text, three u128 header integers,
        // then nine u128 fields per ordered pull. Distinct values expose an
        // omitted, reordered or narrowed field; the last exercises a full word.
        let mut transcript = vec![1, 2, 1];
        transcript.extend_from_slice(&8_u128.to_le_bytes());
        transcript.extend_from_slice(b"top_down");
        for number in [3_u128, 4, 2].into_iter().chain(5..=21) {
            transcript.extend_from_slice(&number.to_le_bytes());
        }
        transcript.extend_from_slice(&(usize::MAX as u128).to_le_bytes());
        for stop in [None, Some(1), Some(2)] {
            let mut observed = 0;
            let mut actual = Hasher::new();
            let result = hash_bfs_decision_card(&mut actual, Some(&card), &mut || {
                observed += 1;
                if stop == Some(observed) {
                    Err("stop")
                } else {
                    Ok(())
                }
            });
            let prefix = stop.map_or(transcript.len(), |stop| 75 + (stop - 1) * 144);
            let mut expected = Hasher::new();
            expected.update(&transcript[..prefix]);
            assert_eq!(actual.finalize(), expected.finalize(), "stop={stop:?}");
            assert_eq!(observed, stop.unwrap_or(2));
            assert_eq!(result, stop.map_or(Ok(()), |_| Err("stop")));
        }
        let mut absent = Hasher::new();
        hash_bfs_decision_card(&mut absent, None, &mut || -> Result<(), Infallible> {
            panic!("absent evidence must not add a checkpoint")
        })
        .unwrap();
        let mut expected = Hasher::new();
        expected.update(&[0]);
        assert_eq!(absent.finalize(), expected.finalize());
    }

    #[test]
    fn component_decision_cards_bind_ordered_fields_and_checkpoint_each_record() {
        let card = ComponentDecisionCard {
            policy_version: 0x0102,
            fallback: "component_bfs",
            dwell_frontiers: 3,
            pull_cost_multiplier: 4,
            pulls: vec![crate::ComponentPullDecision {
                root: 5,
                frontier_vertices: 6,
                frontier_incidences: 7,
                unseen_incidences: 8,
                vertices: 9,
                benefit_lower: 10,
                benefit_upper: 11,
                candidate_visits: 12,
                incidence_visits: usize::MAX,
            }],
        };
        let mut transcript = vec![1, 2, 1];
        transcript.extend_from_slice(&13_u128.to_le_bytes());
        transcript.extend_from_slice(b"component_bfs");
        for number in [3_u128, 4, 1] {
            transcript.extend_from_slice(&number.to_le_bytes());
        }
        let header = transcript.len();
        for number in 5_u128..=12 {
            transcript.extend_from_slice(&number.to_le_bytes());
        }
        transcript.extend_from_slice(&(usize::MAX as u128).to_le_bytes());
        for refuse in [false, true] {
            let mut actual = Hasher::new();
            let mut calls = 0;
            let result = hash_component_decision_card(&mut actual, Some(&card), &mut || {
                calls += 1;
                if refuse { Err("stop") } else { Ok(()) }
            });
            let mut expected = Hasher::new();
            expected.update(&transcript[..if refuse { header } else { transcript.len() }]);
            assert_eq!(actual.finalize(), expected.finalize());
            assert_eq!(result, if refuse { Err("stop") } else { Ok(()) });
            assert_eq!(calls, 1);
        }
        let mut actual = Hasher::new();
        hash_component_decision_card(&mut actual, None, &mut || -> Result<(), Infallible> {
            panic!("absent evidence adds no checkpoint")
        })
        .unwrap();
        let mut expected = Hasher::new();
        expected.update(&[0]);
        assert_eq!(actual.finalize(), expected.finalize());
    }

    #[test]
    fn public_bfs_certificates_bind_decisions_and_measured_evidence_workspace() {
        let n = 16;
        let vertices: Vec<_> = (0..n).map(|i| VId((1 << 113) + i as u128)).collect();
        let mut edges = Vec::new();
        for &source in &vertices {
            for &target in &vertices {
                if source != target {
                    edges.push(ProjectionEdge {
                        eid: EId(edges.len() as u128),
                        source,
                        target,
                        weight: 1.0,
                    });
                }
            }
        }
        let graph = SnapshotGraphView::build(
            SnapshotBinding {
                root: ObjectId([0x31; 32]),
                as_of: CommitSeq(17),
            },
            &vertices,
            &edges,
            ProjectionSpec {
                directedness: Directedness::Directed,
                parallel_edges: ParallelEdgePolicy::CollapseUnit,
                self_loops: SelfLoopPolicy::Keep,
            },
            ProjectionLimits {
                max_vertices: n,
                max_input_edges: edges.len(),
                max_adjacency_entries: edges.len(),
                max_workspace_bytes: 1 << 20,
            },
        )
        .unwrap();
        let limits = FnxExecutionLimits {
            max_iterations: 1,
            max_result_rows: n,
            max_estimated_work: n + edges.len(),
        };
        let text = |bytes: &mut Vec<u8>, value: &str| {
            bytes.extend_from_slice(&(value.len() as u128).to_le_bytes());
            bytes.extend_from_slice(value.as_bytes());
        };
        for cutoff in [None, Some(0)] {
            let call = FnxCallSpec::single_source_shortest_path_length(vertices[0], cutoff);
            let result = call
                .execute(&graph, limits, || Ok::<(), Infallible>(()))
                .unwrap();
            let certificate = &result.certificate;
            let reached = if cutoff.is_none() { n } else { 1 };
            let expected_rows: Vec<_> = vertices[..reached]
                .iter()
                .enumerate()
                .map(|(at, &vertex)| {
                    vec![
                        FnxValue::Vertex(vertex),
                        FnxValue::Integer(u64::from(at != 0)),
                    ]
                })
                .collect();
            assert_eq!(result.rows, expected_rows);
            let mut evidence = Vec::new();
            if cutoff.is_none() {
                let card = certificate.bfs_decision_card.as_ref().expect("dense pull");
                assert_eq!(card.policy_version, 1);
                assert_eq!(card.fallback, "top_down");
                assert_eq!(card.dwell_levels, 1);
                assert_eq!(card.pull_cost_multiplier, 2);
                assert_eq!(
                    card.pulls,
                    [crate::BfsPullDecision {
                        depth: 1,
                        frontier_vertices: 15,
                        frontier_arcs: 225,
                        unseen_incoming_arcs: 0,
                        vertices: 16,
                        benefit_lower: 209,
                        benefit_upper: 209,
                        candidate_visits: 16,
                        incoming_visits: 0,
                    }]
                );
                let vectors = n * (size_of::<usize>() + size_of::<Option<usize>>());
                let decision_bytes = card.pulls.capacity() * size_of::<crate::BfsPullDecision>();
                assert!(certificate.kernel_workspace_bytes >= vectors + decision_bytes);
                // Known dense-graph decision bytes, independent of the
                // certificate helper and the returned evidence's field values.
                evidence.extend_from_slice(&[1, 1, 0]);
                text(&mut evidence, "top_down");
                for number in [1_u128, 2, 1, 1, 15, 225, 0, 16, 209, 209, 16, 0] {
                    evidence.extend_from_slice(&number.to_le_bytes());
                }
            } else {
                assert!(certificate.bfs_decision_card.is_none());
                evidence.push(0);
            }
            assert!(certificate.component_decision_card.is_none());
            evidence.push(0);
            // Reconstruct the entire v5 envelope independently; this detects
            // dropped propagation or a card omitted from the public digest.
            let mut transcript = b"fgdb:prism:call-certificate:v5".to_vec();
            transcript.extend_from_slice(&certificate.registry_version.to_le_bytes());
            transcript.extend_from_slice(certificate.implementation_revision.as_bytes());
            text(&mut transcript, certificate.execution_kernel);
            transcript.extend_from_slice(&certificate.kernel_source_digest.0);
            text(&mut transcript, certificate.numeric_profile);
            transcript.extend_from_slice(&certificate.projection_digest.0);
            transcript.extend_from_slice(&certificate.call_digest.0);
            transcript.extend_from_slice(&certificate.result_digest.0);
            text(&mut transcript, certificate.adapter.as_str());
            for number in [
                certificate.vertices,
                certificate.edges,
                certificate.input_edges,
                certificate.estimated_work,
                certificate.kernel_workspace_bytes,
                certificate.witness.nodes_touched,
                certificate.witness.edges_scanned,
                certificate.witness.queue_peak,
            ] {
                transcript.extend_from_slice(&(number as u128).to_le_bytes());
            }
            text(&mut transcript, &certificate.witness.algorithm);
            text(&mut transcript, &certificate.witness.complexity_claim);
            transcript.extend_from_slice(&evidence);
            let mut expected = Hasher::new();
            expected.update(&transcript);
            assert_eq!(certificate.digest, expected.finalize(), "cutoff={cutoff:?}");
        }
    }

    #[test]
    fn result_cell_records_preserve_type_tags_float_bits_and_chunk_boundaries() {
        // The transcript oracle supplies explicit tags and bit patterns,
        // independently of the production encoder's enum match and buffer.
        let cases = [
            (FnxValue::Vertex(VId(0)), 0, 0_u128, 16),
            (FnxValue::Vertex(VId(u128::MAX)), 0, u128::MAX, 16),
            (FnxValue::Vertex(VId(1 << 97)), 0, 1 << 97, 16),
            (FnxValue::Score(0.0), 1, 0, 8),
            (FnxValue::Score(-0.0), 1, 1 << 63, 8),
            (
                FnxValue::Score(f64::from_bits(0x3fef_ffff_ffff_ffff)),
                1,
                0x3fef_ffff_ffff_ffff,
                8,
            ),
            (FnxValue::Integer(0), 2, 0, 8),
            (FnxValue::Integer(u64::MAX), 2, u64::MAX as u128, 8),
            (FnxValue::Float(-0.0), 3, 1 << 63, 8),
            (
                FnxValue::Float(f64::from_bits(0x7ff8_0000_0000_0042)),
                3,
                0x7ff8_0000_0000_0042,
                8,
            ),
        ];
        for prefix in [0, 1, 7, 16, 17, 63, 64, 65, 1023, 1024, 1025] {
            let mut transcript = vec![0xa5; prefix];
            let mut actual = Hasher::new();
            actual.update(&transcript);
            for _ in 0..33 {
                for &(value, tag, bits, width) in &cases {
                    transcript.push(tag);
                    transcript.extend_from_slice(&bits.to_le_bytes()[..width]);
                    hash_result_value(&mut actual, value);
                }
            }
            let mut expected = Hasher::new();
            expected.update(&transcript);
            assert_eq!(actual.finalize(), expected.finalize(), "prefix={prefix}");
        }
    }

    #[test]
    fn public_call_result_digests_match_the_independent_canonical_transcript() {
        let graph = projection(511, Directedness::Directed);
        let limits = FnxExecutionLimits {
            max_iterations: 1000,
            max_result_rows: 3,
            max_estimated_work: 1 << 20,
        };
        let calls = [
            FnxCallSpec::pagerank(PageRankOptions::default()),
            FnxCallSpec::weakly_connected_components(),
            FnxCallSpec::single_source_shortest_path_length(VId(0), None),
            FnxCallSpec::single_source_dijkstra_path_length(
                crate::DijkstraOptions::new(VId(0), None).unwrap(),
            ),
            FnxCallSpec::from_arguments(
                "fnx",
                "pagerank",
                &[],
                &[("score", "rank"), ("vertex", "node")],
            )
            .unwrap(),
        ];
        for call in calls {
            let result = call
                .execute(&graph, limits, || Ok::<(), Infallible>(()))
                .unwrap();
            let mut transcript = b"fgdb:prism:result-rows:v3".to_vec();
            transcript.extend_from_slice(&(result.rows.len() as u128).to_le_bytes());
            transcript.extend_from_slice(&call.digest().0);
            for row in &result.rows {
                for &value in row {
                    // Decode the returned typed cells into the specified
                    // transcript without calling the production hash helper.
                    match value {
                        FnxValue::Vertex(vertex) => {
                            transcript.push(0);
                            transcript.extend_from_slice(&vertex.0.to_le_bytes());
                        }
                        FnxValue::Score(score) => {
                            transcript.push(1);
                            transcript.extend_from_slice(&score.to_bits().to_le_bytes());
                        }
                        FnxValue::Integer(value) => {
                            transcript.push(2);
                            transcript.extend_from_slice(&value.to_le_bytes());
                        }
                        FnxValue::Float(value) => {
                            transcript.push(3);
                            transcript.extend_from_slice(&value.to_bits().to_le_bytes());
                        }
                    }
                }
            }
            let mut expected = Hasher::new();
            expected.update(&transcript);
            assert_eq!(result.certificate.result_digest, expected.finalize());
        }
    }

    #[test]
    fn direct_row_sink_preserves_cells_certificate_and_every_meter_refusal() {
        #[derive(Default)]
        struct FlatRows {
            shape: Option<(usize, usize)>,
            cells: Vec<FnxValue>,
            buffer: Option<usize>,
        }
        impl FnxRowSink<&'static str> for FlatRows {
            fn begin(
                &mut self,
                rows: usize,
                columns: usize,
            ) -> Result<(), FnxExecutionError<&'static str>> {
                self.shape = Some((rows, columns));
                Ok(())
            }
            fn row(&mut self, values: &[FnxValue]) -> Result<(), FnxExecutionError<&'static str>> {
                assert_eq!(values.len(), self.shape.unwrap().1);
                let address = values.as_ptr() as usize;
                if let Some(first) = self.buffer {
                    assert_eq!(address, first, "one shared FnX row allocation");
                }
                self.buffer = Some(address);
                self.cells.extend_from_slice(values);
                Ok(())
            }
        }
        struct Trace {
            events: Vec<Option<usize>>,
            stop: Option<usize>,
        }
        impl Trace {
            fn event(&mut self, event: Option<usize>) -> Result<(), &'static str> {
                self.events.push(event);
                if self.stop == Some(self.events.len()) {
                    Err("stop")
                } else {
                    Ok(())
                }
            }
        }
        impl FnxMeter<&'static str> for &mut Trace {
            fn unit(&mut self) -> Result<(), &'static str> {
                self.units(1)
            }
            fn units(&mut self, n: usize) -> Result<(), &'static str> {
                self.event(Some(n))
            }
            fn poll(&mut self) -> Result<(), &'static str> {
                self.event(None)
            }
        }
        let limits = FnxExecutionLimits {
            max_iterations: 1000,
            max_result_rows: 3,
            max_estimated_work: 1 << 20,
        };
        let fresh = |stop| Trace {
            events: Vec::new(),
            stop,
        };
        let sparse = FnxCallSpec::from_arguments(
            "fnx",
            "single_source_shortest_path_length",
            &[crate::FnxArgument::Vertex(VId(0))],
            &[("distance", "hops"), ("vertex", "node")],
        )
        .unwrap();
        let rank =
            FnxCallSpec::from_arguments("fnx", "pagerank", &[], &[("score", "rank")]).unwrap();
        // All cell domains, a sparse result, selected/reordered yields and
        // full-width IDs. The loop-free triangle also exercises count/float
        // kernels without conflating projection-law refusals with output.
        for (graph, call) in [
            (projection(2, Directedness::Directed), sparse),
            (projection(238, Directedness::Directed), rank),
            (
                projection(238, Directedness::Directed),
                FnxCallSpec::weakly_connected_components(),
            ),
            (
                projection(2, Directedness::Directed),
                FnxCallSpec::single_source_dijkstra_path_length(
                    crate::DijkstraOptions::new(VId(0), None).unwrap(),
                ),
            ),
            (
                projection(238, Directedness::Undirected),
                FnxCallSpec::triangles(),
            ),
            (
                projection(238, Directedness::Undirected),
                FnxCallSpec::clustering_coefficient(),
            ),
        ] {
            let mut collected_trace = fresh(None);
            let collected = call.execute(&graph, limits, &mut collected_trace).unwrap();
            let mut direct_trace = fresh(None);
            let mut direct = FlatRows::default();
            let certificate = call
                .execute_into(&graph, limits, &mut direct_trace, &mut direct)
                .unwrap();
            assert_eq!(certificate, collected.certificate);
            let mut retained_trace = fresh(None);
            let mut retained = call
                .execute_rows(&graph, limits, &mut retained_trace)
                .unwrap();
            assert_eq!(retained.certificate(), &collected.certificate);
            assert_eq!(retained_trace.events, collected_trace.events);
            assert_eq!(retained.remaining(), collected.rows.len());
            let expected_max = collected
                .rows
                .iter()
                .flatten()
                .filter_map(|value| match value {
                    FnxValue::Integer(value) => Some(*value),
                    _ => None,
                })
                .max();
            assert_eq!(retained.max_integer(), expected_max);
            let mut row = Vec::new();
            for (index, expected) in collected.rows.iter().enumerate() {
                assert!(retained.next_row(&mut row, || Ok::<_, ()>(())).unwrap());
                assert_eq!(&row, expected);
                assert_eq!(retained.remaining(), collected.rows.len() - index - 1);
            }
            assert!(!retained.next_row(&mut row, || Ok::<_, ()>(())).unwrap());
            assert!(row.is_empty());
            assert_eq!(
                direct.shape,
                Some((collected.rows.len(), call.outputs().len()))
            );
            assert_eq!(
                direct.cells,
                collected.rows.into_iter().flatten().collect::<Vec<_>>()
            );
            assert_eq!(direct_trace.events, collected_trace.events);
            for stop in 1..=collected_trace.events.len() {
                let (mut collected_trace, mut direct_trace) =
                    (fresh(Some(stop)), fresh(Some(stop)));
                assert!(matches!(
                    call.execute(&graph, limits, &mut collected_trace),
                    Err(FnxExecutionError::Cancelled("stop"))
                ));
                assert!(matches!(
                    call.execute_into(&graph, limits, &mut direct_trace, &mut FlatRows::default(),),
                    Err(FnxExecutionError::Cancelled("stop"))
                ));
                assert_eq!(direct_trace.events, collected_trace.events, "stop={stop}");
                let mut retained_trace = fresh(Some(stop));
                assert!(matches!(
                    call.execute_rows(&graph, limits, &mut retained_trace),
                    Err(FnxExecutionError::Cancelled("stop"))
                ));
                assert_eq!(retained_trace.events, collected_trace.events, "stop={stop}");
            }
            for refused in [
                FnxExecutionLimits {
                    max_result_rows: 0,
                    ..limits
                },
                FnxExecutionLimits {
                    max_estimated_work: 0,
                    ..limits
                },
            ] {
                let mut sink = FlatRows::default();
                let expected = call.execute(&graph, refused, &mut fresh(None)).unwrap_err();
                let actual = call
                    .execute_into(&graph, refused, &mut fresh(None), &mut sink)
                    .unwrap_err();
                assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
                let retained = call
                    .execute_rows(&graph, refused, &mut fresh(None))
                    .unwrap_err();
                assert_eq!(format!("{retained:?}"), format!("{expected:?}"));
                assert_eq!(sink.shape, None, "refusal must precede output allocation");
            }
        }
        let graph = SnapshotGraphView::build(
            SnapshotBinding {
                root: ObjectId([2; 32]),
                as_of: CommitSeq(3),
            },
            &[],
            &[],
            ProjectionSpec {
                directedness: Directedness::Directed,
                parallel_edges: ParallelEdgePolicy::Reject,
                self_loops: SelfLoopPolicy::Keep,
            },
            ProjectionLimits {
                max_vertices: 0,
                max_input_edges: 0,
                max_adjacency_entries: 0,
                max_workspace_bytes: 1 << 20,
            },
        )
        .unwrap();
        let call = FnxCallSpec::weakly_connected_components();
        let mut sink = FlatRows::default();
        assert_eq!(
            call.execute_into(&graph, limits, &mut fresh(None), &mut sink)
                .unwrap(),
            call.execute(&graph, limits, &mut fresh(None))
                .unwrap()
                .certificate,
        );
        assert_eq!(sink.shape, Some((0, 2)));
        assert_eq!(sink.buffer, None);
        let mut retained = call.execute_rows(&graph, limits, &mut fresh(None)).unwrap();
        assert_eq!(retained.remaining(), 0);
        assert_eq!(retained.max_integer(), None);
        assert!(
            !retained
                .next_row(&mut Vec::new(), || Ok::<_, ()>(()))
                .unwrap()
        );
    }

    #[test]
    fn completed_rows_check_sparse_gaps_and_retry_without_losing_a_vertex() {
        let graph = projection(2, Directedness::Directed);
        let call = FnxCallSpec::single_source_shortest_path_length(VId(0), Some(0));
        let limits = FnxExecutionLimits {
            max_iterations: 1,
            max_result_rows: 3,
            max_estimated_work: 1000,
        };
        let expected = call.execute(&graph, limits, || Ok::<_, ()>(())).unwrap();
        let mut cursor = call
            .execute_rows(&graph, limits, || Ok::<_, ()>(()))
            .unwrap();
        drop(graph);
        let mut row = Vec::new();
        assert!(matches!(
            cursor.next_row(&mut row, || Err("cancelled")),
            Err(FnxExecutionError::Cancelled("cancelled"))
        ));
        assert_eq!(cursor.remaining(), 1);
        assert!(cursor.next_row(&mut row, || Ok::<_, ()>(())).unwrap());
        assert_eq!(row, expected.rows[0]);
        let mut checked = 0;
        assert!(matches!(
            cursor.next_row(&mut row, || {
                checked += 1;
                if checked == 2 { Err("gap") } else { Ok(()) }
            }),
            Err(FnxExecutionError::Cancelled("gap"))
        ));
        assert_eq!(cursor.remaining(), 0);
        assert!(row.is_empty());
        let mut retry = 0;
        assert!(
            !cursor
                .next_row(&mut row, || {
                    retry += 1;
                    Ok::<_, ()>(())
                })
                .unwrap()
        );
        assert_eq!(retry, 1, "the refused unreachable ordinal is retried");
        assert!(
            !cursor
                .next_row(&mut row, || -> Result<(), ()> {
                    panic!("exhaustion does not rescan")
                })
                .unwrap()
        );
    }

    #[test]
    fn retained_hash_validates_selected_full_width_integers_before_any_row_pull() {
        let graph = projection(238, Directedness::Undirected);
        let values = KernelValues::Counts(vec![0, i64::MAX as u64, u64::MAX]);
        for (yields, maximum) in [
            (vec![("vertex", "v"), ("triangles", "t")], Some(u64::MAX)),
            (vec![("triangles", "t")], Some(u64::MAX)),
            (vec![("vertex", "v")], None),
        ] {
            let call = FnxCallSpec::from_arguments("fnx", "triangles", &[], &yields).unwrap();
            let mut actual = Hasher::new();
            let (rows, observed) = call
                .hash_retained_rows(&graph, &values, &mut actual, &mut || Ok::<_, ()>(()))
                .unwrap();
            assert_eq!(rows, 3);
            assert_eq!(observed, maximum);
            let mut bytes = Vec::new();
            for (vertex, count) in [VId(0), VId(17), VId(u128::MAX)].into_iter().zip([
                0_u64,
                i64::MAX as u64,
                u64::MAX,
            ]) {
                for &(field, _) in &yields {
                    match field {
                        "vertex" => {
                            bytes.push(0);
                            bytes.extend_from_slice(&vertex.0.to_le_bytes());
                        }
                        "triangles" => {
                            bytes.push(2);
                            bytes.extend_from_slice(&count.to_le_bytes());
                        }
                        _ => unreachable!(),
                    }
                }
            }
            let mut oracle = Hasher::new();
            oracle.update(&bytes);
            assert_eq!(actual.finalize(), oracle.finalize());
        }
    }

    #[test]
    fn call_arc_admission_uses_exact_incidence_counts_in_every_projection_direction() {
        let call = FnxCallSpec::single_source_shortest_path_length(VId(0), None);
        let limits = FnxExecutionLimits {
            max_iterations: 1000,
            max_result_rows: 3,
            max_estimated_work: 0,
        };
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            for mask in 0..512 {
                let graph = projection(mask, direction);
                let arcs: usize = (0..graph.node_count())
                    .map(|vertex| graph.neighbors_indices(vertex).unwrap().len())
                    .sum();
                let mut meter = Recording {
                    calls: Vec::new(),
                    stop: None,
                    seen: 0,
                    polls: 0,
                };
                let result = call.execute(&graph, limits, RecordingMeter::<true>(&mut meter));
                assert!(matches!(
                    result,
                    Err(FnxExecutionError::LimitExceeded {
                        resource: "estimated work",
                        limit: 0,
                        requested,
                    }) if requested == graph.node_count() + arcs
                ));
                assert_eq!(meter.seen, 1 + graph.node_count());
                assert_eq!(meter.calls, [1, 3]);
            }
        }
    }

    #[test]
    fn complete_call_runs_preserve_scalar_results_and_every_work_refusal() {
        // Two complete runs plus a tail, isolates, a self-loop, and wide IDs.
        let n = 2 * FNX_METER_RUN + 3;
        let vertices: Vec<_> = (0..n)
            .map(|index| VId(((index as u128) << 64) | (n - index) as u128))
            .collect();
        let edges: Vec<_> = [(0, 0), (0, 1), (1, 2), (n - 1, n - 1)]
            .into_iter()
            .enumerate()
            .map(|(index, (source, target))| ProjectionEdge {
                eid: EId(u128::MAX - index as u128),
                source: vertices[source],
                target: vertices[target],
                weight: 1.0,
            })
            .collect();
        let graph = SnapshotGraphView::build(
            SnapshotBinding {
                root: ObjectId([0x4a; 32]),
                as_of: CommitSeq(7),
            },
            &vertices,
            &edges,
            ProjectionSpec {
                directedness: Directedness::Directed,
                parallel_edges: ParallelEdgePolicy::CollapseUnit,
                self_loops: SelfLoopPolicy::Keep,
            },
            ProjectionLimits {
                max_vertices: n,
                max_input_edges: edges.len(),
                max_adjacency_entries: edges.len(),
                max_workspace_bytes: 1 << 20,
            },
        )
        .unwrap();
        let limits = FnxExecutionLimits {
            max_iterations: 100,
            max_result_rows: n,
            max_estimated_work: 1 << 20,
        };
        let fresh = |stop| Recording {
            calls: Vec::new(),
            stop,
            seen: 0,
            polls: 0,
        };
        for call in [
            FnxCallSpec::single_source_shortest_path_length(vertices[0], None),
            FnxCallSpec::weakly_connected_components(),
        ] {
            let (mut scalar, mut runs) = (fresh(None), fresh(None));
            let expected = call
                .execute(&graph, limits, RecordingMeter::<false>(&mut scalar))
                .unwrap();
            let actual = call
                .execute(&graph, limits, RecordingMeter::<true>(&mut runs))
                .unwrap();
            assert_eq!(actual, expected);
            assert_eq!((runs.seen, runs.polls), (scalar.seen, scalar.polls));
            assert_eq!(&runs.calls[..4], &[1, FNX_METER_RUN, FNX_METER_RUN, 3]);
            assert!(runs.calls.iter().all(|&run| run <= FNX_METER_RUN));
            for stop in 1..=scalar.seen {
                let (mut scalar, mut runs) = (fresh(Some(stop)), fresh(Some(stop)));
                let expected = call.execute(&graph, limits, RecordingMeter::<false>(&mut scalar));
                let actual = call.execute(&graph, limits, RecordingMeter::<true>(&mut runs));
                assert!(matches!(
                    expected,
                    Err(FnxExecutionError::Cancelled("stop"))
                ));
                assert!(matches!(actual, Err(FnxExecutionError::Cancelled("stop"))));
                assert_eq!(runs.seen, stop);
                assert_eq!((runs.seen, runs.polls), (scalar.seen, scalar.polls));
            }
            // An estimate refusal still occurs only after every admission
            // unit, and the last unit's cancellation wins over that refusal.
            let refused = FnxExecutionLimits {
                max_estimated_work: 0,
                ..limits
            };
            for stop in [n + 1, n + 2] {
                let mut meter = fresh(Some(stop));
                let result = call.execute(&graph, refused, RecordingMeter::<true>(&mut meter));
                if stop == n + 1 {
                    assert!(matches!(result, Err(FnxExecutionError::Cancelled("stop"))));
                } else {
                    assert!(matches!(
                        result,
                        Err(FnxExecutionError::LimitExceeded { .. })
                    ));
                }
                assert_eq!(meter.seen, n + 1);
            }
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
        assert!(graph.has_unit_weights());
        let per_pass = |units: usize| units.div_ceil(FNX_METER_RUN);
        let unit_initialization_calls = n
            + (0..n)
                .map(|node| per_pass(graph.neighbors_indices(node).unwrap().len()))
                .sum::<usize>();
        assert!(unit_initialization_calls < n + m);
        for (weighted, forced_shares) in [(false, false), (true, false), (true, true)] {
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
            let (scores, witness, _) =
                with_weighted_shares(forced_shares, || pagerank(&graph, options, m, &mut meter))
                    .unwrap();
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
            assert!(
                meter
                    .calls
                    .iter()
                    .all(|&run| (1..=FNX_METER_RUN).contains(&run))
            );
            // Proven unit weights still charge every weighted validation unit,
            // but batch each row's arcs. General weighted shares retain one
            // callback per arc. Check both paths against the same fnx oracle.
            let initialization_calls = if weighted && !forced_shares {
                unit_initialization_calls
            } else {
                initialization
            };
            assert_eq!(
                meter.calls[..initialization_calls].iter().sum::<usize>(),
                initialization,
                "initialization callbacks retain every logical unit"
            );
            // Four metered passes per iteration, each about (units / 64) calls.
            assert_eq!(
                meter.calls.len(),
                initialization_calls + passes * (3 * per_pass(n) + per_pass(n + m)),
                "the arc walk is one stream of runs: weighted={weighted} forced_shares={forced_shares}"
            );
            for stop in (1..=meter.seen).step_by(97) {
                let mut refusing = Recording {
                    calls: Vec::new(),
                    stop: Some(stop),
                    seen: 0,
                    polls: 0,
                };
                let result = with_weighted_shares(forced_shares, || {
                    pagerank(&graph, options, m, &mut refusing)
                });
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
                contributions(
                    options.alpha(),
                    weighted,
                    &ranks,
                    &offsets,
                    &mut actual,
                    threads,
                    &mut || Ok::<(), Infallible>(()),
                )
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
    fn personalized_empty_gather_places_mass_once_across_live_chunks() {
        let n = 2 * ARITHMETIC_RUN + 3;
        let offsets = vec![0; n + 1];
        for seed in [
            0,
            ARITHMETIC_RUN - 1,
            ARITHMETIC_RUN,
            ARITHMETIC_RUN + 1,
            n - 1,
        ] {
            for threads in 1..=8 {
                let mut complete = vec![f64::NAN; n];
                let initial = PageRankRestart::new(Some(seed), 0.85, n as f64, 0.5);
                let mut meter = EmptyGatherMeter {
                    polls: 0,
                    stop: None,
                };
                gather_pass(
                    &offsets,
                    &[],
                    &[],
                    None,
                    initial,
                    &mut complete,
                    threads,
                    &mut meter,
                )
                .unwrap();
                for (target, &value) in complete.iter().enumerate() {
                    assert_eq!(
                        value,
                        if target == seed {
                            (1.0 - 0.85) + 0.85 * 0.5
                        } else {
                            0.0
                        }
                    );
                }
                for stop in 1..=meter.polls {
                    let mut partial = vec![f64::NAN; n];
                    let mut refusal = EmptyGatherMeter {
                        polls: 0,
                        stop: Some(stop),
                    };
                    assert!(matches!(
                        gather_pass(&offsets, &[], &[], None, initial, &mut partial, threads, &mut refusal),
                        Err(FnxExecutionError::Cancelled(at)) if at == stop
                    ));
                    let filled = (stop - 1) * ARITHMETIC_RUN;
                    assert_eq!(&partial[..filled], &complete[..filled]);
                    assert!(partial[filled..].iter().all(|value| value.is_nan()));
                }
            }
        }
    }

    #[test]
    fn personalized_fixed_updates_keep_logical_charges_workspace_and_every_refusal() {
        struct Live {
            polls: usize,
            stop: Option<usize>,
        }
        impl FnxMeter<usize> for Live {
            fn unit(&mut self) -> Result<(), usize> {
                Ok(())
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
        for mask in [0, 2, 0b111_010_111] {
            let graph = projection(mask, Directedness::Directed);
            let arcs = graph.incoming_face().1.len();
            for weighted in [false, true] {
                let options = PageRankOptions::fixed_iterations(0.85, 3, weighted).unwrap();
                let seeded = options.with_source(VId(u128::MAX));
                let recorder = || Recording {
                    calls: Vec::new(),
                    stop: None,
                    seen: 0,
                    polls: 0,
                };
                let mut uniform_meter = recorder();
                let uniform =
                    pagerank_with_threads(&graph, options, arcs, 1, &mut uniform_meter).unwrap();
                let mut seeded_meter = recorder();
                let personalized =
                    pagerank_with_threads(&graph, seeded, arcs, 1, &mut seeded_meter).unwrap();
                assert_eq!(uniform_meter.calls, seeded_meter.calls);
                assert_eq!(uniform_meter.seen, seeded_meter.seen);
                assert_eq!(uniform_meter.polls, seeded_meter.polls);
                assert_eq!(uniform.2, personalized.2);
                for stop in 1..=seeded_meter.seen {
                    let mut meter = Recording {
                        stop: Some(stop),
                        ..recorder()
                    };
                    assert!(matches!(
                        pagerank_with_threads(&graph, seeded, arcs, 1, &mut meter),
                        Err(FnxExecutionError::Cancelled("stop"))
                    ));
                    assert_eq!(meter.seen, stop);
                }
                let mut live = Live {
                    polls: 0,
                    stop: None,
                };
                pagerank_with_threads(&graph, seeded, arcs, 1, &mut live).unwrap();
                for stop in 1..=live.polls {
                    let mut meter = Live {
                        polls: 0,
                        stop: Some(stop),
                    };
                    assert!(
                        matches!(pagerank_with_threads(&graph, seeded, arcs, 1, &mut meter),
                        Err(FnxExecutionError::Cancelled(at)) if at == stop)
                    );
                    assert_eq!(meter.polls, stop);
                }
            }
        }
    }

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
                for options in [
                    options,
                    options.with_source(VId(0)),
                    options.with_source(VId(7)),
                    options.with_source(VId((n - 1) as u128)),
                ] {
                    let (one, witness, workspace) =
                        pagerank_with_threads(&graph, options, sources.len(), 1, &mut || {
                            Ok::<(), Infallible>(())
                        })
                        .unwrap();
                    assert_eq!(witness.nodes_touched, n * iterations);
                    assert_eq!(witness.edges_scanned, sources.len() * iterations);
                    for threads in 2..=8 {
                        let (many, many_witness, many_workspace) = pagerank_with_threads(
                            &graph,
                            options,
                            sources.len(),
                            threads,
                            &mut || Ok::<(), Infallible>(()),
                        )
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
            contributions(
                options.alpha(),
                false,
                &ranks,
                &offsets,
                &mut values,
                1,
                &mut meter
            ),
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

    fn unit_projection(mask: u16, direction: Directedness) -> SnapshotGraphView {
        let vertices = [VId(0), VId(17), VId(u128::MAX)];
        let edges: Vec<_> = (0..9)
            .filter(|bit| mask & (1 << bit) != 0)
            .map(|bit| ProjectionEdge {
                eid: EId((bit as u128) << 64 | 7),
                source: vertices[bit / 3],
                target: vertices[bit % 3],
                weight: 1.0,
            })
            .collect();
        SnapshotGraphView::build(
            SnapshotBinding {
                root: ObjectId([0x31; 32]),
                as_of: CommitSeq(7),
            },
            &vertices,
            &edges,
            ProjectionSpec {
                directedness: direction,
                parallel_edges: ParallelEdgePolicy::CollapseUnit,
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

    #[test]
    fn unit_weight_pagerank_matches_weighted_predecessor_and_fnx_on_every_small_topology() {
        for mask in 0..512 {
            for direction in [
                Directedness::Directed,
                Directedness::Reversed,
                Directedness::Undirected,
            ] {
                let graph = unit_projection(mask, direction);
                assert!(graph.has_unit_weights());
                let arcs = graph.incoming_face().1.len();
                for options in [
                    PageRankOptions::new(0.85, 1000, 1e-12, true).unwrap(),
                    PageRankOptions::global_l1(0.85, 1000, 1e-12, true).unwrap(),
                    PageRankOptions::fixed_iterations(0.85, 17, true).unwrap(),
                ] {
                    let run = || {
                        pagerank_with_threads(&graph, options, arcs, 1, &mut || {
                            Ok::<(), Infallible>(())
                        })
                        .unwrap()
                    };
                    let (old, old_witness, old_workspace) = with_weighted_shares(true, run);
                    let (new, new_witness, new_workspace) = run();
                    assert!(
                        old.iter()
                            .zip(&new)
                            .all(|(a, b)| a.to_bits() == b.to_bits()),
                        "mask={mask} direction={direction:?} options={options:?}"
                    );
                    assert_eq!(new_witness, old_witness);
                    assert_eq!(
                        new_workspace,
                        3 * (3 * size_of::<f64>() + size_of::<usize>())
                    );
                    assert_eq!(
                        old_workspace - new_workspace,
                        arcs * size_of::<f64>() + 3 * size_of::<usize>()
                    );
                    if options.stopping_rule() == crate::PageRankStoppingRule::Convergence {
                        let oracle = fnx_algorithms::pagerank_with_weight(
                            &graph,
                            options.alpha(),
                            options.max_iter(),
                            options.tolerance(),
                            Some(PROJECTED_WEIGHT_ATTRIBUTE),
                        );
                        assert!(oracle.converged);
                        assert_eq!(new_witness, oracle.witness);
                        for score in oracle.scores {
                            assert_eq!(
                                new[graph.get_node_index(&score.node).unwrap()].to_bits(),
                                score.score.to_bits(),
                                "FNX mask={mask} direction={direction:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn unit_weight_arithmetic_remains_cancellable_at_every_live_poll() {
        struct Live {
            units: usize,
            polls: usize,
            stop: Option<usize>,
        }
        impl FnxMeter<usize> for Live {
            fn unit(&mut self) -> Result<(), usize> {
                self.units += 1;
                Ok(())
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
        let n = ARITHMETIC_RUN + 13;
        let vertices: Vec<_> = (0..n).map(|i| VId((i as u128) << 64 | 17)).collect();
        let edges: Vec<_> = (1..n)
            .map(|i| ProjectionEdge {
                eid: EId(u128::MAX - i as u128),
                source: vertices[i],
                target: vertices[0],
                weight: 1.0,
            })
            .collect();
        let hub = SnapshotGraphView::build(
            SnapshotBinding {
                root: ObjectId([0x33; 32]),
                as_of: CommitSeq(7),
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
                max_adjacency_entries: edges.len(),
                max_workspace_bytes: 8 << 20,
            },
        )
        .unwrap();
        for graph in [unit_projection(0b111_001_010, Directedness::Directed), hub] {
            assert!(graph.has_unit_weights());
            let options = PageRankOptions::fixed_iterations(0.85, 2, true).unwrap();
            let arcs = graph.incoming_face().1.len();
            let mut live = Live {
                units: 0,
                polls: 0,
                stop: None,
            };
            pagerank_with_threads(&graph, options, arcs, 1, &mut live).unwrap();
            assert!(live.polls > 0);
            assert_eq!(
                live.units,
                graph.node_count() + arcs + 2 * (4 * graph.node_count() + arcs)
            );
            for stop in 1..=live.polls {
                let mut live = Live {
                    units: 0,
                    polls: 0,
                    stop: Some(stop),
                };
                assert!(
                    matches!(pagerank_with_threads(&graph, options, arcs, 1, &mut live), Err(FnxExecutionError::Cancelled(at)) if at == stop)
                );
                assert_eq!(live.polls, stop);
            }
        }
    }

    #[test]
    fn unit_weight_full_call_keeps_identity_and_every_logical_refusal() {
        let graph = unit_projection(0b111_001_010, Directedness::Directed);
        let call = FnxCallSpec::pagerank(PageRankOptions::fixed_iterations(0.85, 7, true).unwrap());
        let limits = FnxExecutionLimits {
            max_iterations: 7,
            max_result_rows: 3,
            max_estimated_work: 1000,
        };
        let fresh = |stop| Recording {
            calls: Vec::new(),
            stop,
            seen: 0,
            polls: 0,
        };
        let mut prior_meter = fresh(None);
        let prior = with_weighted_shares(true, || {
            call.execute(&graph, limits, RecordingMeter::<true>(&mut prior_meter))
        })
        .unwrap();
        let mut meter = fresh(None);
        let new = call
            .execute(&graph, limits, RecordingMeter::<true>(&mut meter))
            .unwrap();
        assert_eq!(new.rows, prior.rows);
        assert_eq!(new.certificate.call_digest, prior.certificate.call_digest);
        assert_eq!(
            new.certificate.result_digest,
            prior.certificate.result_digest
        );
        assert_eq!(
            new.certificate.estimated_work,
            prior.certificate.estimated_work
        );
        assert_eq!(new.certificate.witness, prior.certificate.witness);
        assert_eq!(meter.seen, prior_meter.seen);
        assert!(meter.calls.iter().all(|&units| units <= FNX_METER_RUN));
        assert!(new.certificate.kernel_workspace_bytes < prior.certificate.kernel_workspace_bytes);
        for stop in 1..=meter.seen {
            let mut prior = fresh(Some(stop));
            let old = with_weighted_shares(true, || {
                call.execute(&graph, limits, RecordingMeter::<true>(&mut prior))
            });
            let mut actual = fresh(Some(stop));
            let new = call.execute(&graph, limits, RecordingMeter::<true>(&mut actual));
            assert!(matches!(old, Err(FnxExecutionError::Cancelled("stop"))));
            assert!(matches!(new, Err(FnxExecutionError::Cancelled("stop"))));
            assert_eq!((prior.seen, actual.seen), (stop, stop));
        }
        for refused in [
            FnxExecutionLimits {
                max_result_rows: 2,
                ..limits
            },
            FnxExecutionLimits {
                max_iterations: 6,
                ..limits
            },
            FnxExecutionLimits {
                max_estimated_work: 0,
                ..limits
            },
        ] {
            let old = with_weighted_shares(true, || {
                call.execute(&graph, refused, RecordingMeter::<true>(&mut fresh(None)))
            });
            let new = call.execute(&graph, refused, RecordingMeter::<true>(&mut fresh(None)));
            assert_eq!(format!("{old:?}"), format!("{new:?}"));
            assert!(matches!(new, Err(FnxExecutionError::LimitExceeded { .. })));
        }
    }

    #[test]
    fn non_unit_weights_keep_the_weighted_path_and_error_precedence() {
        for weights in [[0.0, 1.0], [0.5, 2.0], [-1.0, 1.0], [f64::MAX, f64::MAX]] {
            let graph = SnapshotGraphView::build(
                SnapshotBinding {
                    root: ObjectId([0x32; 32]),
                    as_of: CommitSeq(7),
                },
                &[VId(1), VId(2), VId(3)],
                &[
                    ProjectionEdge {
                        eid: EId(1),
                        source: VId(1),
                        target: VId(2),
                        weight: weights[0],
                    },
                    ProjectionEdge {
                        eid: EId(2),
                        source: VId(1),
                        target: VId(3),
                        weight: weights[1],
                    },
                ],
                ProjectionSpec {
                    directedness: Directedness::Directed,
                    parallel_edges: ParallelEdgePolicy::Sum,
                    self_loops: SelfLoopPolicy::Keep,
                },
                ProjectionLimits {
                    max_vertices: 3,
                    max_input_edges: 2,
                    max_adjacency_entries: 2,
                    max_workspace_bytes: 1 << 20,
                },
            )
            .unwrap();
            assert!(!graph.has_unit_weights());
            let options = PageRankOptions::fixed_iterations(0.85, 2, true).unwrap();
            let run = |stop| {
                let mut meter = Recording {
                    calls: Vec::new(),
                    stop,
                    seen: 0,
                    polls: 0,
                };
                let result = pagerank_with_threads(&graph, options, 2, 1, &mut meter);
                (format!("{result:?}"), meter.calls, meter.seen, meter.polls)
            };
            let before = with_weighted_shares(true, || run(None));
            assert_eq!(run(None), before);
            for stop in 1..=before.2 {
                assert_eq!(
                    run(Some(stop)),
                    with_weighted_shares(true, || run(Some(stop)))
                );
            }
            if weights[0] < 0.0 {
                assert!(before.0.contains("NegativeWeight"));
            } else if weights[0] == f64::MAX {
                assert!(before.0.contains("NonFiniteWeightSum"));
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
                let weighted_peak = if weighted && !graph.has_unit_weights() {
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
