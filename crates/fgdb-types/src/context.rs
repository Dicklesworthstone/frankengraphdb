//! Purpose-typed execution contexts and database obligation vocabulary.
//!
//! FrankenGraphDB narrows one runtime-owned [`asupersync::Cx`] at the
//! composition root and passes only the role wrapper needed by a subsystem.
//! The wrapped context is deliberately private: downstream code can use the
//! named database effects below, but cannot recover that wrapped `Cx` through
//! this API. Asupersync's separate ambient `Cx::current()` surface has a pinned
//! upstream time/random masking limitation documented on [`RestrictedFuture`].
//!
//! The database obligation lifecycle is affine. A live obligation contains an
//! asupersync graded obligation, so dropping it before [`PurposeObligation::abort`]
//! or [`PurposeObligation<Cleanup>::complete`] is a detected leak. Lifecycle
//! evidence is fixed-size and contains only stable enums, a caller-assigned ID,
//! and a resource count; descriptions, paths, tenant identifiers, and payloads
//! never enter the evidence surface.

use std::future::Future;
use std::marker::PhantomData;
use std::num::NonZeroU64;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};

use asupersync::Cx;
use asupersync::cx::cap::{self, CapSet};
use asupersync::obligation::graded::{GradedObligation, Resolution};
use asupersync::record::ObligationKind as FoundationObligationKind;

type LocalDatabaseCaps = CapSet<true, true, false, true, false>;
type ReplicationCaps = CapSet<true, true, false, true, true>;

const FIRST_OBLIGATION_GENERATION: u64 = 1;

#[derive(Debug)]
struct ObligationTracker {
    next_generation: AtomicU64,
    live: AtomicUsize,
}

impl ObligationTracker {
    const fn new() -> Self {
        Self {
            next_generation: AtomicU64::new(FIRST_OBLIGATION_GENERATION),
            live: AtomicUsize::new(0),
        }
    }

    fn acquire_generation(&self) -> Result<ObligationGeneration, ObligationAcquireFailure> {
        let generation = self
            .next_generation
            .try_update(
                Ordering::AcqRel,
                Ordering::Acquire,
                |current| match current {
                    0 => None,
                    u64::MAX => Some(0),
                    _ => Some(current + 1),
                },
            )
            .map_err(|_| ObligationAcquireFailure::GenerationExhausted)?;
        let generation =
            NonZeroU64::new(generation).ok_or(ObligationAcquireFailure::GenerationExhausted)?;
        Ok(ObligationGeneration(generation))
    }

    fn increment_live(&self) -> Result<(), ObligationAcquireFailure> {
        self.live
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map(|_| ())
            .map_err(|_| ObligationAcquireFailure::LiveCounterExhausted)
    }

    fn decrement_live(&self) {
        let decremented = self
            .live
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_sub(1)
            });
        debug_assert!(
            decremented.is_ok(),
            "resolved obligation was absent from the local live tracker"
        );
    }

    fn live(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }
}

enum RestrictionCx {
    Local(Cx<LocalDatabaseCaps>),
    Replication(Cx<ReplicationCaps>),
    None(Cx<cap::None>),
}

/// A future whose ambient asupersync capability mask is narrowed for each poll.
///
/// The guard is deliberately installed per poll and never crosses an await.
/// At asupersync revision `e464a48`, ambient `Cx<cap::All>` I/O and remote
/// accessors honor this runtime mask, but direct time and random accessors do
/// not. This adapter therefore reduces ambient authority but does not claim to
/// close that upstream time/random escape. The explicit purpose wrapper remains
/// the authoritative API passed to delegated code.
pub struct RestrictedFuture<Fut> {
    future: Pin<Box<Fut>>,
    restriction: RestrictionCx,
}

impl<Fut> RestrictedFuture<Fut> {
    fn local(future: Fut, cx: Cx<LocalDatabaseCaps>) -> Self {
        Self {
            future: Box::pin(future),
            restriction: RestrictionCx::Local(cx),
        }
    }

    fn replication(future: Fut, cx: Cx<ReplicationCaps>) -> Self {
        Self {
            future: Box::pin(future),
            restriction: RestrictionCx::Replication(cx),
        }
    }

    fn none(future: Fut, cx: Cx<cap::None>) -> Self {
        Self {
            future: Box::pin(future),
            restriction: RestrictionCx::None(cx),
        }
    }
}

impl<Fut: Future> Future for RestrictedFuture<Fut> {
    type Output = Fut::Output;

    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match &this.restriction {
            RestrictionCx::Local(cx) => {
                let _guard = cx.clone().set_current_restricted();
                this.future.as_mut().poll(task)
            }
            RestrictionCx::Replication(cx) => {
                let _guard = cx.clone().set_current_restricted();
                this.future.as_mut().poll(task)
            }
            RestrictionCx::None(cx) => {
                let _guard = cx.clone().set_current_restricted();
                this.future.as_mut().poll(task)
            }
        }
    }
}

/// Auditable summary of an asupersync type-level capability row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CapabilityRow {
    pub spawn: bool,
    pub time: bool,
    pub random: bool,
    pub io: bool,
    pub remote: bool,
}

/// Capabilities common to local query, transaction, commit, and maintenance
/// work. The role wrapper further restricts which database effects are named.
pub const LOCAL_DATABASE_CAPABILITIES: CapabilityRow = CapabilityRow {
    spawn: true,
    time: true,
    random: false,
    io: true,
    remote: false,
};

/// Replication additionally has remote capability, but never ambient entropy.
pub const REPLICATION_CAPABILITIES: CapabilityRow = CapabilityRow {
    remote: true,
    ..LOCAL_DATABASE_CAPABILITIES
};

/// Merge intent replay has no spawn, clock, entropy, I/O, or remote capability.
pub const MERGE_EVAL_CAPABILITIES: CapabilityRow = CapabilityRow {
    spawn: false,
    time: false,
    random: false,
    io: false,
    remote: false,
};

/// The sole public boundary that narrows a runtime root context into database
/// roles. Keep this value at the composition root; subsystems receive one of
/// its purpose wrappers rather than the set itself.
#[derive(Clone)]
pub struct PurposeContexts {
    query: QueryCx,
    txn: TxnCx,
    commit: CommitCx,
    maint: MaintCx,
    repl: ReplCx,
    merge_eval: MergeEvalCx,
    tracker: Arc<ObligationTracker>,
}

impl PurposeContexts {
    /// Monotonically narrows the type-level capability row for every database
    /// role and drops all type-level effects for deterministic merge evaluation.
    #[must_use]
    pub fn narrow_runtime_root(root: &Cx<cap::All>) -> Self {
        let tracker = Arc::new(ObligationTracker::new());
        Self {
            query: QueryCx {
                inner: root.restrict::<LocalDatabaseCaps>(),
                tracker: Arc::clone(&tracker),
            },
            txn: TxnCx {
                inner: root.restrict::<LocalDatabaseCaps>(),
                tracker: Arc::clone(&tracker),
            },
            commit: CommitCx {
                inner: root.restrict::<LocalDatabaseCaps>(),
                tracker: Arc::clone(&tracker),
            },
            maint: MaintCx {
                inner: root.restrict::<LocalDatabaseCaps>(),
                tracker: Arc::clone(&tracker),
            },
            repl: ReplCx {
                inner: root.restrict::<ReplicationCaps>(),
                tracker: Arc::clone(&tracker),
            },
            merge_eval: MergeEvalCx {
                inner: root.restrict::<cap::None>(),
            },
            tracker,
        }
    }

    /// Number of locally tracked database obligations that have been acquired
    /// and not yet discharged or aborted.
    #[must_use]
    pub fn outstanding_obligations(&self) -> usize {
        self.tracker.live()
    }

    #[must_use]
    pub fn query(&self) -> QueryCx {
        self.query.clone()
    }

    #[must_use]
    pub fn txn(&self) -> TxnCx {
        self.txn.clone()
    }

    #[must_use]
    pub fn commit(&self) -> CommitCx {
        self.commit.clone()
    }

    #[must_use]
    pub fn maint(&self) -> MaintCx {
        self.maint.clone()
    }

    #[must_use]
    pub fn repl(&self) -> ReplCx {
        self.repl.clone()
    }

    #[must_use]
    pub fn merge_eval(&self) -> MergeEvalCx {
        self.merge_eval.clone()
    }
}

mod storage_read_seal {
    pub trait Sealed {}
}

/// Purpose-typed authority for synchronous storage reads.
///
/// Implementations install their existing role restriction while the read is
/// executed. The trait is sealed to the storage-capable database roles; in
/// particular, deterministic merge evaluation cannot satisfy this boundary.
///
/// ```compile_fail
/// use fgdb_types::{MergeEvalCx, StorageReadCx};
///
/// fn requires_storage_read(_cx: &impl StorageReadCx) {}
///
/// fn merge_replay_cannot_read(cx: &MergeEvalCx) {
///     requires_storage_read(cx);
/// }
/// ```
pub trait StorageReadCx: storage_read_seal::Sealed {
    /// Runs a storage read with this role's synchronous restriction installed.
    fn with_restriction<T>(&self, run: impl FnOnce() -> T) -> T;

    /// Wraps an asynchronous storage read so this role's restriction is
    /// installed for every poll. The async counterpart of
    /// [`with_restriction`](Self::with_restriction), with the same
    /// [`RestrictedFuture`] time/random caveat.
    fn with_restriction_async<Fut: Future>(&self, future: Fut) -> RestrictedFuture<Fut>;
}

/// Query-only effects.
///
/// ```compile_fail
/// use fgdb_types::{ObligationId, PurposeContexts};
/// fn illegal(contexts: &PurposeContexts) {
///     let query = contexts.query();
///     let id = ObligationId::new(1).unwrap();
///     let bytes = std::num::NonZeroU64::new(1).unwrap();
///     let _ = query.reserve_prepared_bytes(id, bytes);
/// }
/// ```
#[derive(Clone)]
pub struct QueryCx {
    inner: Cx<LocalDatabaseCaps>,
    tracker: Arc<ObligationTracker>,
}

impl QueryCx {
    #[must_use]
    pub const fn capabilities(&self) -> CapabilityRow {
        LOCAL_DATABASE_CAPABILITIES
    }

    pub fn checkpoint(&self) -> Result<(), Box<asupersync::error::Error>> {
        self.inner.checkpoint().map_err(Box::new)
    }

    /// Runs synchronous delegated code with the local role mask installed as
    /// the ambient asupersync restriction for the duration of the call.
    ///
    /// Direct ambient time/random access remains an upstream limitation at the
    /// pinned asupersync revision; callers must still pass only this wrapper.
    pub fn with_restriction<T>(&self, run: impl FnOnce() -> T) -> T {
        let _guard = self.inner.clone().set_current_restricted();
        run()
    }

    #[must_use]
    pub fn with_restriction_async<Fut: Future>(&self, future: Fut) -> RestrictedFuture<Fut> {
        RestrictedFuture::local(future, self.inner.clone())
    }

    #[must_use]
    pub fn outstanding_obligations(&self) -> usize {
        self.tracker.live()
    }

    pub fn pin_snapshot(
        &self,
        id: ObligationId,
    ) -> Result<PurposeObligation<Acquired>, ObligationAcquireError> {
        acquire(
            &self.inner,
            Arc::clone(&self.tracker),
            id,
            ContextRole::Query,
            DatabaseObligationKind::PinSnapshot,
            1,
        )
    }
}

impl storage_read_seal::Sealed for QueryCx {}

impl StorageReadCx for QueryCx {
    fn with_restriction<T>(&self, run: impl FnOnce() -> T) -> T {
        QueryCx::with_restriction(self, run)
    }

    fn with_restriction_async<Fut: Future>(&self, future: Fut) -> RestrictedFuture<Fut> {
        QueryCx::with_restriction_async(self, future)
    }
}

/// Transaction effects.
///
/// ```compile_fail
/// use fgdb_types::{ObligationId, PurposeContexts};
/// fn illegal(contexts: &PurposeContexts) {
///     let txn = contexts.txn();
///     let id = ObligationId::new(1).unwrap();
///     let bytes = std::num::NonZeroU64::new(1).unwrap();
///     let _ = txn.reserve_raft_payload_space(id, bytes);
/// }
/// ```
#[derive(Clone)]
pub struct TxnCx {
    inner: Cx<LocalDatabaseCaps>,
    tracker: Arc<ObligationTracker>,
}

impl TxnCx {
    #[must_use]
    pub const fn capabilities(&self) -> CapabilityRow {
        LOCAL_DATABASE_CAPABILITIES
    }

    pub fn checkpoint(&self) -> Result<(), Box<asupersync::error::Error>> {
        self.inner.checkpoint().map_err(Box::new)
    }

    pub fn with_restriction<T>(&self, run: impl FnOnce() -> T) -> T {
        let _guard = self.inner.clone().set_current_restricted();
        run()
    }

    #[must_use]
    pub fn with_restriction_async<Fut: Future>(&self, future: Fut) -> RestrictedFuture<Fut> {
        RestrictedFuture::local(future, self.inner.clone())
    }

    #[must_use]
    pub fn outstanding_obligations(&self) -> usize {
        self.tracker.live()
    }

    pub fn pin_snapshot(
        &self,
        id: ObligationId,
    ) -> Result<PurposeObligation<Acquired>, ObligationAcquireError> {
        acquire(
            &self.inner,
            Arc::clone(&self.tracker),
            id,
            ContextRole::Txn,
            DatabaseObligationKind::PinSnapshot,
            1,
        )
    }

    pub fn reserve_prepared_bytes(
        &self,
        id: ObligationId,
        bytes: NonZeroU64,
    ) -> Result<PurposeObligation<Acquired>, ObligationAcquireError> {
        acquire(
            &self.inner,
            Arc::clone(&self.tracker),
            id,
            ContextRole::Txn,
            DatabaseObligationKind::ReservePreparedBytes,
            bytes.get(),
        )
    }
}

impl storage_read_seal::Sealed for TxnCx {}

impl StorageReadCx for TxnCx {
    fn with_restriction<T>(&self, run: impl FnOnce() -> T) -> T {
        TxnCx::with_restriction(self, run)
    }

    fn with_restriction_async<Fut: Future>(&self, future: Fut) -> RestrictedFuture<Fut> {
        TxnCx::with_restriction_async(self, future)
    }
}

/// Commit-coordinator effects.
///
/// ```compile_fail
/// use fgdb_types::{ObligationId, PurposeContexts};
/// fn illegal(contexts: &PurposeContexts) {
///     let commit = contexts.commit();
///     let id = ObligationId::new(1).unwrap();
///     let _ = commit.pin_snapshot(id);
/// }
/// ```
#[derive(Clone)]
pub struct CommitCx {
    inner: Cx<LocalDatabaseCaps>,
    tracker: Arc<ObligationTracker>,
}

impl CommitCx {
    #[must_use]
    pub const fn capabilities(&self) -> CapabilityRow {
        LOCAL_DATABASE_CAPABILITIES
    }

    pub fn checkpoint(&self) -> Result<(), Box<asupersync::error::Error>> {
        self.inner.checkpoint().map_err(Box::new)
    }

    pub fn with_restriction<T>(&self, run: impl FnOnce() -> T) -> T {
        let _guard = self.inner.clone().set_current_restricted();
        run()
    }

    #[must_use]
    pub fn with_restriction_async<Fut: Future>(&self, future: Fut) -> RestrictedFuture<Fut> {
        RestrictedFuture::local(future, self.inner.clone())
    }

    #[must_use]
    pub fn outstanding_obligations(&self) -> usize {
        self.tracker.live()
    }

    pub fn reserve_prepared_bytes(
        &self,
        id: ObligationId,
        bytes: NonZeroU64,
    ) -> Result<PurposeObligation<Acquired>, ObligationAcquireError> {
        acquire(
            &self.inner,
            Arc::clone(&self.tracker),
            id,
            ContextRole::Commit,
            DatabaseObligationKind::ReservePreparedBytes,
            bytes.get(),
        )
    }

    pub fn reserve_raft_payload_space(
        &self,
        id: ObligationId,
        bytes: NonZeroU64,
    ) -> Result<PurposeObligation<Acquired>, ObligationAcquireError> {
        acquire(
            &self.inner,
            Arc::clone(&self.tracker),
            id,
            ContextRole::Commit,
            DatabaseObligationKind::ReserveRaftPayloadSpace,
            bytes.get(),
        )
    }

    pub fn publish_segment(
        &self,
        id: ObligationId,
    ) -> Result<PurposeObligation<Acquired>, ObligationAcquireError> {
        acquire(
            &self.inner,
            Arc::clone(&self.tracker),
            id,
            ContextRole::Commit,
            DatabaseObligationKind::PublishSegment,
            1,
        )
    }
}

impl storage_read_seal::Sealed for CommitCx {}

impl StorageReadCx for CommitCx {
    fn with_restriction<T>(&self, run: impl FnOnce() -> T) -> T {
        CommitCx::with_restriction(self, run)
    }

    fn with_restriction_async<Fut: Future>(&self, future: Fut) -> RestrictedFuture<Fut> {
        CommitCx::with_restriction_async(self, future)
    }
}

/// Maintenance effects.
///
/// ```compile_fail
/// use fgdb_types::{ObligationId, PurposeContexts};
/// fn illegal(contexts: &PurposeContexts) {
///     let maint = contexts.maint();
///     let id = ObligationId::new(1).unwrap();
///     let bytes = std::num::NonZeroU64::new(1).unwrap();
///     let _ = maint.reserve_prepared_bytes(id, bytes);
/// }
/// ```
#[derive(Clone)]
pub struct MaintCx {
    inner: Cx<LocalDatabaseCaps>,
    tracker: Arc<ObligationTracker>,
}

impl MaintCx {
    #[must_use]
    pub const fn capabilities(&self) -> CapabilityRow {
        LOCAL_DATABASE_CAPABILITIES
    }

    pub fn checkpoint(&self) -> Result<(), Box<asupersync::error::Error>> {
        self.inner.checkpoint().map_err(Box::new)
    }

    pub fn with_restriction<T>(&self, run: impl FnOnce() -> T) -> T {
        let _guard = self.inner.clone().set_current_restricted();
        run()
    }

    #[must_use]
    pub fn with_restriction_async<Fut: Future>(&self, future: Fut) -> RestrictedFuture<Fut> {
        RestrictedFuture::local(future, self.inner.clone())
    }

    #[must_use]
    pub fn outstanding_obligations(&self) -> usize {
        self.tracker.live()
    }

    pub fn publish_segment(
        &self,
        id: ObligationId,
    ) -> Result<PurposeObligation<Acquired>, ObligationAcquireError> {
        acquire(
            &self.inner,
            Arc::clone(&self.tracker),
            id,
            ContextRole::Maint,
            DatabaseObligationKind::PublishSegment,
            1,
        )
    }
}

impl storage_read_seal::Sealed for MaintCx {}

impl StorageReadCx for MaintCx {
    fn with_restriction<T>(&self, run: impl FnOnce() -> T) -> T {
        MaintCx::with_restriction(self, run)
    }

    fn with_restriction_async<Fut: Future>(&self, future: Fut) -> RestrictedFuture<Fut> {
        MaintCx::with_restriction_async(self, future)
    }
}

/// Replication effects.
///
/// ```compile_fail
/// use fgdb_types::{ObligationId, PurposeContexts};
/// fn illegal(contexts: &PurposeContexts) {
///     let repl = contexts.repl();
///     let id = ObligationId::new(1).unwrap();
///     let _ = repl.pin_snapshot(id);
/// }
/// ```
#[derive(Clone)]
pub struct ReplCx {
    inner: Cx<ReplicationCaps>,
    tracker: Arc<ObligationTracker>,
}

impl ReplCx {
    #[must_use]
    pub const fn capabilities(&self) -> CapabilityRow {
        REPLICATION_CAPABILITIES
    }

    pub fn checkpoint(&self) -> Result<(), Box<asupersync::error::Error>> {
        self.inner.checkpoint().map_err(Box::new)
    }

    pub fn with_restriction<T>(&self, run: impl FnOnce() -> T) -> T {
        let _guard = self.inner.clone().set_current_restricted();
        run()
    }

    #[must_use]
    pub fn with_restriction_async<Fut: Future>(&self, future: Fut) -> RestrictedFuture<Fut> {
        RestrictedFuture::replication(future, self.inner.clone())
    }

    #[must_use]
    pub fn outstanding_obligations(&self) -> usize {
        self.tracker.live()
    }

    pub fn reserve_raft_payload_space(
        &self,
        id: ObligationId,
        bytes: NonZeroU64,
    ) -> Result<PurposeObligation<Acquired>, ObligationAcquireError> {
        acquire(
            &self.inner,
            Arc::clone(&self.tracker),
            id,
            ContextRole::Repl,
            DatabaseObligationKind::ReserveRaftPayloadSpace,
            bytes.get(),
        )
    }

    pub fn publish_segment(
        &self,
        id: ObligationId,
    ) -> Result<PurposeObligation<Acquired>, ObligationAcquireError> {
        acquire(
            &self.inner,
            Arc::clone(&self.tracker),
            id,
            ContextRole::Repl,
            DatabaseObligationKind::PublishSegment,
            1,
        )
    }
}

impl storage_read_seal::Sealed for ReplCx {}

impl StorageReadCx for ReplCx {
    fn with_restriction<T>(&self, run: impl FnOnce() -> T) -> T {
        ReplCx::with_restriction(self, run)
    }

    fn with_restriction_async<Fut: Future>(&self, future: Fut) -> RestrictedFuture<Fut> {
        ReplCx::with_restriction_async(self, future)
    }
}

/// Capability-empty context for deterministic intent replay.
///
/// The wrapper has no clock, entropy, filesystem/network I/O, spawn, or remote
/// method, and its private foundation context is statically `cap::None`.
///
/// ```compile_fail
/// use fgdb_types::PurposeContexts;
/// fn illegal(contexts: &PurposeContexts) {
///     let merge = contexts.merge_eval();
///     let _ = merge.now();
/// }
/// ```
#[derive(Clone)]
pub struct MergeEvalCx {
    inner: Cx<cap::None>,
}

impl MergeEvalCx {
    #[must_use]
    pub const fn capabilities(&self) -> CapabilityRow {
        MERGE_EVAL_CAPABILITIES
    }

    /// Cancellation remains observable even though all effect capabilities
    /// are absent.
    pub fn checkpoint(&self) -> Result<(), Box<asupersync::error::Error>> {
        self.inner.checkpoint().map_err(Box::new)
    }

    /// Installs the empty runtime mask while synchronous merge evaluation runs.
    ///
    /// The pinned asupersync revision does not make its direct ambient
    /// `Cx<cap::All>` time/random methods consult the runtime mask. This scope
    /// therefore blocks mask-aware ambient I/O/remote access but is not, by
    /// itself, a proof against ambient clock or entropy lookup. Merge evaluators
    /// must receive only `MergeEvalCx`, and must not call `Cx::current()`.
    pub fn with_restriction<T>(&self, run: impl FnOnce() -> T) -> T {
        let _guard = self.inner.clone().set_current_restricted();
        run()
    }

    #[must_use]
    pub fn with_restriction_async<Fut: Future>(&self, future: Fut) -> RestrictedFuture<Fut> {
        RestrictedFuture::none(future, self.inner.clone())
    }
}

/// Stable, caller-assigned identity for one database obligation.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ObligationId(NonZeroU64);

impl ObligationId {
    pub fn new(value: u64) -> Result<Self, InvalidObligationId> {
        NonZeroU64::new(value).map(Self).ok_or(InvalidObligationId)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Zero is reserved as the absent/uninitialized obligation identity.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InvalidObligationId;

impl std::fmt::Display for InvalidObligationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("obligation ID must be nonzero")
    }
}

impl std::error::Error for InvalidObligationId {}

/// Tracker-assigned generation that disambiguates reused caller identities.
///
/// There is intentionally no public constructor: generations come only from a
/// shared [`PurposeContexts`] tracker.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ObligationGeneration(NonZeroU64);

impl ObligationGeneration {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Registered database-level obligation vocabulary in the W1 foundation.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DatabaseObligationKind {
    PinSnapshot,
    ReservePreparedBytes,
    ReserveRaftPayloadSpace,
    PublishSegment,
}

/// The purpose wrapper that was legally able to create an obligation.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ContextRole {
    Query,
    Txn,
    Commit,
    Maint,
    Repl,
}

impl DatabaseObligationKind {
    const fn foundation_kind(self) -> FoundationObligationKind {
        match self {
            Self::PinSnapshot => FoundationObligationKind::Lease,
            Self::ReservePreparedBytes | Self::ReserveRaftPayloadSpace => {
                FoundationObligationKind::SemaphorePermit
            }
            Self::PublishSegment => FoundationObligationKind::IoOp,
        }
    }

    const fn redacted_description(self) -> &'static str {
        match self {
            Self::PinSnapshot => "fgdb:pin_snapshot",
            Self::ReservePreparedBytes => "fgdb:reserve_prepared_bytes",
            Self::ReserveRaftPayloadSpace => "fgdb:reserve_raft_payload_space",
            Self::PublishSegment => "fgdb:publish_segment",
        }
    }
}

/// Stable lifecycle boundaries used by cancellation tests and replay logs.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ObligationStage {
    Acquisition,
    Transfer,
    Publication,
    Cleanup,
    Resolution,
}

/// Boundary about to be crossed when cancellation was observed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ObligationBoundary {
    Acquisition,
    Transfer,
    Publication,
    Cleanup,
    Completion,
}

/// How a database obligation was discharged.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ObligationResolution {
    /// The lifecycle token was legally discharged. This does not, by itself,
    /// prove that the named database effect executed or became durable.
    Discharged,
    Aborted,
}

/// One fixed-size, secret-free lifecycle record.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ObligationLifecycleEvent {
    id: ObligationId,
    generation: ObligationGeneration,
    task_id: u64,
    region_id: u64,
    role: ContextRole,
    kind: DatabaseObligationKind,
    stage: ObligationStage,
    units: u64,
    resolution: Option<ObligationResolution>,
}

impl ObligationLifecycleEvent {
    #[must_use]
    pub const fn id(&self) -> ObligationId {
        self.id
    }

    #[must_use]
    pub const fn generation(&self) -> ObligationGeneration {
        self.generation
    }

    #[must_use]
    pub const fn task_id(&self) -> u64 {
        self.task_id
    }

    #[must_use]
    pub const fn region_id(&self) -> u64 {
        self.region_id
    }

    #[must_use]
    pub const fn role(&self) -> ContextRole {
        self.role
    }

    #[must_use]
    pub const fn kind(&self) -> DatabaseObligationKind {
        self.kind
    }

    #[must_use]
    pub const fn stage(&self) -> ObligationStage {
        self.stage
    }

    #[must_use]
    pub const fn units(&self) -> u64 {
        self.units
    }

    #[must_use]
    pub const fn resolution(&self) -> Option<ObligationResolution> {
        self.resolution
    }
}

const MAX_OBLIGATION_EVENTS: usize = 5;

struct ObligationCore {
    /// This foundation token owns one short, fixed vocabulary `String` inside
    /// asupersync. That bounded foundation allocation is not a RuntimeState
    /// obligation-table registration and is not visible to LabRuntime's leak
    /// oracle; `tracker` below is the measured local accounting source.
    token: GradedObligation,
    cancel_probe: Cx<cap::None>,
    tracker: Arc<ObligationTracker>,
    id: ObligationId,
    generation: ObligationGeneration,
    task_id: u64,
    region_id: u64,
    role: ContextRole,
    kind: DatabaseObligationKind,
    units: u64,
    events: [Option<ObligationLifecycleEvent>; MAX_OBLIGATION_EVENTS],
    event_count: usize,
}

impl ObligationCore {
    #[allow(clippy::too_many_arguments)]
    fn new(
        cancel_probe: Cx<cap::None>,
        tracker: Arc<ObligationTracker>,
        id: ObligationId,
        generation: ObligationGeneration,
        task_id: u64,
        region_id: u64,
        role: ContextRole,
        kind: DatabaseObligationKind,
        units: u64,
    ) -> Self {
        let token = GradedObligation::reserve(kind.foundation_kind(), kind.redacted_description());
        let mut core = Self {
            token,
            cancel_probe,
            tracker,
            id,
            generation,
            task_id,
            region_id,
            role,
            kind,
            units,
            events: [None; MAX_OBLIGATION_EVENTS],
            event_count: 0,
        };
        core.record(ObligationStage::Acquisition, None);
        core
    }

    fn record(&mut self, stage: ObligationStage, resolution: Option<ObligationResolution>) {
        debug_assert!(self.event_count < MAX_OBLIGATION_EVENTS);
        self.events[self.event_count] = Some(ObligationLifecycleEvent {
            id: self.id,
            generation: self.generation,
            task_id: self.task_id,
            region_id: self.region_id,
            role: self.role,
            kind: self.kind,
            stage,
            units: self.units,
            resolution,
        });
        self.event_count += 1;
    }

    fn resolve(mut self, resolution: ObligationResolution) -> ObligationReceipt {
        self.record(ObligationStage::Resolution, Some(resolution));
        let foundation_resolution = match resolution {
            ObligationResolution::Discharged => Resolution::Commit,
            ObligationResolution::Aborted => Resolution::Abort,
        };
        let _proof = self.token.resolve(foundation_resolution);
        self.tracker.decrement_live();
        ObligationReceipt {
            id: self.id,
            generation: self.generation,
            task_id: self.task_id,
            region_id: self.region_id,
            role: self.role,
            kind: self.kind,
            units: self.units,
            resolution,
            events: self.events,
            event_count: self.event_count,
        }
    }
}

/// Obligation immediately after acquisition.
#[derive(Debug)]
pub enum Acquired {}
/// Obligation after ownership/resource transfer.
#[derive(Debug)]
pub enum Transferred {}
/// Obligation after the publication boundary.
#[derive(Debug)]
pub enum Published {}
/// Obligation after deterministic cleanup is complete.
#[derive(Debug)]
pub enum Cleanup {}

/// Affine database obligation. The state parameter makes boundary order
/// unrepresentable out of sequence.
///
/// Skipping the transfer boundary is a type error because `publish` exists
/// only on [`PurposeObligation<Transferred>`]:
///
/// ```compile_fail,E0599
/// use fgdb_types::{Acquired, PurposeObligation};
///
/// fn skip_transfer(obligation: PurposeObligation<Acquired>) {
///     let _ = obligation.publish();
/// }
/// ```
///
/// The complete legal transition chain type-checks:
///
/// ```
/// use fgdb_types::{
///     Acquired, ObligationCancellationError, ObligationReceipt, PurposeObligation,
/// };
///
/// fn complete_in_order(
///     obligation: PurposeObligation<Acquired>,
/// ) -> Result<ObligationReceipt, ObligationCancellationError> {
///     obligation.transfer()?.publish()?.cleanup()?.complete()
/// }
/// ```
#[must_use = "database obligations must be completed or aborted"]
pub struct PurposeObligation<State> {
    core: ObligationCore,
    _state: PhantomData<State>,
}

impl<State> PurposeObligation<State> {
    fn transition<Next>(
        mut self,
        boundary: ObligationBoundary,
        stage: ObligationStage,
    ) -> Result<PurposeObligation<Next>, ObligationCancellationError> {
        if let Err(source) = self.core.cancel_probe.checkpoint() {
            let receipt = self.core.resolve(ObligationResolution::Aborted);
            return Err(ObligationCancellationError {
                source: Box::new(source),
                attempted_boundary: boundary,
                receipt: Box::new(receipt),
            });
        }
        self.core.record(stage, None);
        Ok(PurposeObligation {
            core: self.core,
            _state: PhantomData,
        })
    }

    /// Cancellation at any live boundary deterministically aborts the
    /// foundation obligation and returns its complete redacted evidence.
    #[must_use]
    pub fn abort(self) -> ObligationReceipt {
        self.core.resolve(ObligationResolution::Aborted)
    }

    #[must_use]
    pub fn id(&self) -> ObligationId {
        self.core.id
    }

    #[must_use]
    pub fn kind(&self) -> DatabaseObligationKind {
        self.core.kind
    }
}

impl PurposeObligation<Acquired> {
    pub fn transfer(self) -> Result<PurposeObligation<Transferred>, ObligationCancellationError> {
        self.transition(ObligationBoundary::Transfer, ObligationStage::Transfer)
    }
}

impl PurposeObligation<Transferred> {
    pub fn publish(self) -> Result<PurposeObligation<Published>, ObligationCancellationError> {
        self.transition(
            ObligationBoundary::Publication,
            ObligationStage::Publication,
        )
    }
}

impl PurposeObligation<Published> {
    pub fn cleanup(self) -> Result<PurposeObligation<Cleanup>, ObligationCancellationError> {
        self.transition(ObligationBoundary::Cleanup, ObligationStage::Cleanup)
    }
}

impl PurposeObligation<Cleanup> {
    pub fn complete(self) -> Result<ObligationReceipt, ObligationCancellationError> {
        if let Err(source) = self.core.cancel_probe.checkpoint() {
            let receipt = self.core.resolve(ObligationResolution::Aborted);
            return Err(ObligationCancellationError {
                source: Box::new(source),
                attempted_boundary: ObligationBoundary::Completion,
                receipt: Box::new(receipt),
            });
        }
        Ok(self.core.resolve(ObligationResolution::Discharged))
    }
}

/// Complete, fixed-size proof that one obligation's local lifecycle token
/// reached a terminal state.
///
/// This proves discharge or abort of the obligation token only. It is not
/// evidence that the named database effect executed, became visible, or became
/// durable; those claims require their operation-specific receipts.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ObligationReceipt {
    id: ObligationId,
    generation: ObligationGeneration,
    task_id: u64,
    region_id: u64,
    role: ContextRole,
    kind: DatabaseObligationKind,
    units: u64,
    resolution: ObligationResolution,
    events: [Option<ObligationLifecycleEvent>; MAX_OBLIGATION_EVENTS],
    event_count: usize,
}

impl ObligationReceipt {
    #[must_use]
    pub const fn id(&self) -> ObligationId {
        self.id
    }

    #[must_use]
    pub const fn generation(&self) -> ObligationGeneration {
        self.generation
    }

    #[must_use]
    pub const fn task_id(&self) -> u64 {
        self.task_id
    }

    #[must_use]
    pub const fn region_id(&self) -> u64 {
        self.region_id
    }

    #[must_use]
    pub const fn role(&self) -> ContextRole {
        self.role
    }

    #[must_use]
    pub const fn kind(&self) -> DatabaseObligationKind {
        self.kind
    }

    #[must_use]
    pub const fn units(&self) -> u64 {
        self.units
    }

    #[must_use]
    pub const fn resolution(&self) -> ObligationResolution {
        self.resolution
    }

    pub fn events(&self) -> impl DoubleEndedIterator<Item = &ObligationLifecycleEvent> + '_ {
        self.events[..self.event_count]
            .iter()
            .filter_map(Option::as_ref)
    }
}

/// Cancellation observed while attempting to cross a live obligation boundary.
///
/// The contained receipt is always terminal and aborted; callers can retain it
/// as ordered redacted evidence without risking an armed-token drop.
#[derive(Debug)]
pub struct ObligationCancellationError {
    source: Box<asupersync::error::Error>,
    attempted_boundary: ObligationBoundary,
    receipt: Box<ObligationReceipt>,
}

impl ObligationCancellationError {
    #[must_use]
    pub const fn attempted_boundary(&self) -> ObligationBoundary {
        self.attempted_boundary
    }

    #[must_use]
    pub fn receipt(&self) -> &ObligationReceipt {
        self.receipt.as_ref()
    }

    #[must_use]
    pub fn into_receipt(self) -> ObligationReceipt {
        *self.receipt
    }
}

impl std::fmt::Display for ObligationCancellationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "context cancelled at {:?} obligation boundary: {}",
            self.attempted_boundary, self.source
        )
    }
}

impl std::error::Error for ObligationCancellationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Cancellation observed before a foundation obligation was armed.
#[derive(Debug)]
pub enum ObligationAcquireError {
    Cancelled {
        source: Box<asupersync::error::Error>,
    },
    GenerationExhausted,
    LiveCounterExhausted,
}

impl ObligationAcquireError {
    #[must_use]
    pub const fn attempted_boundary(&self) -> ObligationBoundary {
        ObligationBoundary::Acquisition
    }

    #[must_use]
    pub fn into_source(self) -> Option<asupersync::error::Error> {
        match self {
            Self::Cancelled { source } => Some(*source),
            Self::GenerationExhausted | Self::LiveCounterExhausted => None,
        }
    }
}

impl std::fmt::Display for ObligationAcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled { source } => write!(
                f,
                "context cancelled before obligation acquisition: {source}"
            ),
            Self::GenerationExhausted => f.write_str("obligation generation space exhausted"),
            Self::LiveCounterExhausted => f.write_str("live obligation counter exhausted"),
        }
    }
}

impl std::error::Error for ObligationAcquireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Cancelled { source } => Some(source.as_ref()),
            Self::GenerationExhausted | Self::LiveCounterExhausted => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ObligationAcquireFailure {
    GenerationExhausted,
    LiveCounterExhausted,
}

impl From<ObligationAcquireFailure> for ObligationAcquireError {
    fn from(failure: ObligationAcquireFailure) -> Self {
        match failure {
            ObligationAcquireFailure::GenerationExhausted => Self::GenerationExhausted,
            ObligationAcquireFailure::LiveCounterExhausted => Self::LiveCounterExhausted,
        }
    }
}

fn acquire<Caps>(
    cx: &Cx<Caps>,
    tracker: Arc<ObligationTracker>,
    id: ObligationId,
    role: ContextRole,
    kind: DatabaseObligationKind,
    units: u64,
) -> Result<PurposeObligation<Acquired>, ObligationAcquireError>
where
    cap::None: cap::SubsetOf<Caps>,
{
    cx.checkpoint()
        .map_err(|source| ObligationAcquireError::Cancelled {
            source: Box::new(source),
        })?;
    let generation = tracker.acquire_generation()?;
    tracker.increment_live()?;
    let cancel_probe = cx.restrict::<cap::None>();
    Ok(PurposeObligation {
        core: ObligationCore::new(
            cancel_probe,
            tracker,
            id,
            generation,
            cx.task_id().as_u64(),
            cx.region_id().as_u64(),
            role,
            kind,
            units,
        ),
        _state: PhantomData,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use asupersync::lab::{LabRunReport, run_async_under_lab};
    use asupersync::{CancelKind, CancelReason};

    fn assert_clean_lab_report(report: &LabRunReport) {
        assert!(report.quiescent, "lab run did not quiesce: {report:?}");
        assert!(
            report.oracle_report.total > 0,
            "lab run produced no oracle coverage: {report:?}"
        );
        assert!(
            report.oracle_report.all_passed(),
            "lab oracle failed: {report:?}"
        );
        for invariant in ["obligation_leak", "quiescence"] {
            let entry = report.oracle_report.entry(invariant);
            assert!(
                entry.is_some(),
                "lab report omitted {invariant}: {report:?}"
            );
            let Some(entry) = entry else {
                continue;
            };
            assert!(entry.passed, "lab oracle {invariant} failed: {report:?}");
        }
        assert!(
            report.invariant_violations.is_empty(),
            "lab invariant violation: {report:?}"
        );
    }

    fn under_lab<T, F>(seed: u64, test: F) -> T
    where
        T: Send + 'static,
        F: FnOnce(PurposeContexts, Cx) -> T + Send + 'static,
    {
        let (output, report) = run_async_under_lab(seed, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            test(contexts, root)
        });
        assert_clean_lab_report(&report);
        output
    }

    fn under_cancelled_lab<F>(seed: u64, test: F)
    where
        F: FnOnce(PurposeContexts, Cx) + Send + 'static,
    {
        let ((), report) = run_async_under_lab(seed, |root| async move {
            let mut handle = root
                .spawn(move |child| async move {
                    let contexts = PurposeContexts::narrow_runtime_root(&child);
                    test(contexts, child);
                })
                .expect("lab child spawn must be available");
            let joined = handle.join(&root).await;
            assert_eq!(
                joined,
                Ok(()),
                "a child that acknowledged cancellation and completed cleanup must preserve its returned value"
            );
        });
        assert_clean_lab_report(&report);
    }

    fn id(value: u64) -> ObligationId {
        ObligationId::new(value).unwrap()
    }

    fn bytes(value: u64) -> NonZeroU64 {
        NonZeroU64::new(value).unwrap()
    }

    fn stages(receipt: &ObligationReceipt) -> Vec<ObligationStage> {
        receipt
            .events()
            .map(ObligationLifecycleEvent::stage)
            .collect()
    }

    fn cancel(root: &Cx, boundary: ObligationBoundary) {
        let message = match boundary {
            ObligationBoundary::Acquisition => "cancel at acquisition fixture",
            ObligationBoundary::Transfer => "cancel at transfer fixture",
            ObligationBoundary::Publication => "cancel at publication fixture",
            ObligationBoundary::Cleanup => "cancel at cleanup fixture",
            ObligationBoundary::Completion => "cancel at completion fixture",
        };
        root.set_cancel_reason(CancelReason::new(CancelKind::User).with_message(message));
    }

    fn cancellation_error<T>(
        result: Result<T, ObligationCancellationError>,
    ) -> ObligationCancellationError {
        assert!(result.is_err(), "cancelled boundary unexpectedly succeeded");
        result.err().expect("error presence asserted above")
    }

    #[test]
    fn capability_rows_are_narrow_and_merge_is_empty() {
        under_lab(1, |contexts, _root| {
            assert_eq!(contexts.query().capabilities(), LOCAL_DATABASE_CAPABILITIES);
            assert_eq!(contexts.txn().capabilities(), LOCAL_DATABASE_CAPABILITIES);
            assert_eq!(
                contexts.commit().capabilities(),
                LOCAL_DATABASE_CAPABILITIES
            );
            assert_eq!(contexts.maint().capabilities(), LOCAL_DATABASE_CAPABILITIES);
            assert_eq!(contexts.repl().capabilities(), REPLICATION_CAPABILITIES);
            assert_eq!(
                contexts.merge_eval().capabilities(),
                MERGE_EVAL_CAPABILITIES
            );
            assert!(!contexts.query().capabilities().random);
            assert!(!contexts.repl().capabilities().random);
            assert!(!contexts.merge_eval().capabilities().time);
            assert!(!contexts.merge_eval().capabilities().io);
            assert!(!contexts.merge_eval().capabilities().remote);

            fn requires_none(_: &Cx<cap::None>) {}
            requires_none(&contexts.merge_eval.inner);
        });
    }

    #[test]
    fn role_methods_create_the_registered_obligation_kinds() {
        under_lab(2, |contexts, _root| {
            let cases = [
                contexts.query().pin_snapshot(id(1)).unwrap(),
                contexts
                    .txn()
                    .reserve_prepared_bytes(id(2), bytes(64))
                    .unwrap(),
                contexts
                    .commit()
                    .reserve_raft_payload_space(id(3), bytes(128))
                    .unwrap(),
                contexts.maint().publish_segment(id(4)).unwrap(),
                contexts.repl().publish_segment(id(5)).unwrap(),
            ];
            let kinds: Vec<_> = cases.iter().map(PurposeObligation::kind).collect();
            assert_eq!(
                kinds,
                [
                    DatabaseObligationKind::PinSnapshot,
                    DatabaseObligationKind::ReservePreparedBytes,
                    DatabaseObligationKind::ReserveRaftPayloadSpace,
                    DatabaseObligationKind::PublishSegment,
                    DatabaseObligationKind::PublishSegment,
                ]
            );
            for obligation in cases {
                let _receipt = obligation.abort();
            }
            assert_eq!(contexts.outstanding_obligations(), 0);
        });
    }

    #[test]
    fn cancellation_at_every_boundary_resolves_without_leaks() {
        under_cancelled_lab(30, |contexts, root| {
            cancel(&root, ObligationBoundary::Acquisition);
            let error = match contexts.query().pin_snapshot(id(10)) {
                Ok(obligation) => {
                    let _receipt = obligation.abort();
                    None
                }
                Err(error) => Some(error),
            };
            assert!(
                error.is_some(),
                "cancelled acquisition unexpectedly succeeded"
            );
            let Some(error) = error else {
                return;
            };
            assert_eq!(error.attempted_boundary(), ObligationBoundary::Acquisition);
            assert_eq!(contexts.outstanding_obligations(), 0);
        });

        under_cancelled_lab(31, |contexts, root| {
            let obligation = contexts.query().pin_snapshot(id(11)).unwrap();
            assert_eq!(contexts.outstanding_obligations(), 1);
            cancel(&root, ObligationBoundary::Transfer);
            let error = cancellation_error(obligation.transfer());
            assert_eq!(error.attempted_boundary(), ObligationBoundary::Transfer);
            let receipt = error.into_receipt();
            assert_eq!(
                stages(&receipt),
                [ObligationStage::Acquisition, ObligationStage::Resolution]
            );
            assert_eq!(receipt.resolution(), ObligationResolution::Aborted);
            assert_eq!(contexts.outstanding_obligations(), 0);
        });

        under_cancelled_lab(32, |contexts, root| {
            let obligation = contexts
                .query()
                .pin_snapshot(id(12))
                .unwrap()
                .transfer()
                .unwrap();
            cancel(&root, ObligationBoundary::Publication);
            let error = cancellation_error(obligation.publish());
            assert_eq!(error.attempted_boundary(), ObligationBoundary::Publication);
            let receipt = error.into_receipt();
            assert_eq!(
                stages(&receipt),
                [
                    ObligationStage::Acquisition,
                    ObligationStage::Transfer,
                    ObligationStage::Resolution,
                ]
            );
            assert_eq!(contexts.outstanding_obligations(), 0);
        });

        under_cancelled_lab(33, |contexts, root| {
            let obligation = contexts
                .query()
                .pin_snapshot(id(13))
                .unwrap()
                .transfer()
                .unwrap()
                .publish()
                .unwrap();
            cancel(&root, ObligationBoundary::Cleanup);
            let error = cancellation_error(obligation.cleanup());
            assert_eq!(error.attempted_boundary(), ObligationBoundary::Cleanup);
            let receipt = error.into_receipt();
            assert_eq!(
                stages(&receipt),
                [
                    ObligationStage::Acquisition,
                    ObligationStage::Transfer,
                    ObligationStage::Publication,
                    ObligationStage::Resolution,
                ]
            );
            assert_eq!(contexts.outstanding_obligations(), 0);
        });

        under_cancelled_lab(34, |contexts, root| {
            let obligation = contexts
                .query()
                .pin_snapshot(id(14))
                .unwrap()
                .transfer()
                .unwrap()
                .publish()
                .unwrap()
                .cleanup()
                .unwrap();
            cancel(&root, ObligationBoundary::Completion);
            let error = cancellation_error(obligation.complete());
            assert_eq!(error.attempted_boundary(), ObligationBoundary::Completion);
            assert_eq!(error.receipt().resolution(), ObligationResolution::Aborted);
            assert_eq!(contexts.outstanding_obligations(), 0);
        });
    }

    #[test]
    fn complete_lifecycle_is_ordered_and_redacted() {
        under_lab(4, |contexts, _root| {
            let receipt = contexts
                .commit()
                .reserve_prepared_bytes(id(21), bytes(4096))
                .unwrap()
                .transfer()
                .unwrap()
                .publish()
                .unwrap()
                .cleanup()
                .unwrap()
                .complete()
                .unwrap();
            assert_eq!(receipt.id(), id(21));
            assert_eq!(receipt.units(), 4096);
            assert_eq!(receipt.resolution(), ObligationResolution::Discharged);
            assert_eq!(contexts.outstanding_obligations(), 0);
            assert_eq!(
                stages(&receipt),
                [
                    ObligationStage::Acquisition,
                    ObligationStage::Transfer,
                    ObligationStage::Publication,
                    ObligationStage::Cleanup,
                    ObligationStage::Resolution,
                ]
            );
            let debug = format!("{:?}", receipt.events().collect::<Vec<_>>());
            for forbidden in ["path", "tenant", "payload", "description"] {
                assert!(!debug.contains(forbidden));
            }
        });
    }

    #[test]
    fn cancellation_before_acquisition_arms_no_obligation() {
        under_cancelled_lab(5, |contexts, root| {
            root.set_cancel_reason(
                CancelReason::new(CancelKind::User).with_message("context fixture cancellation"),
            );
            let error = match contexts.query().pin_snapshot(id(30)) {
                Ok(obligation) => {
                    let _receipt = obligation.abort();
                    None
                }
                Err(error) => Some(error),
            };
            assert!(
                error.is_some(),
                "cancelled context unexpectedly acquired an obligation"
            );
            let Some(error) = error else {
                return;
            };
            assert!(
                error
                    .to_string()
                    .contains("cancelled before obligation acquisition")
            );
        });
    }

    #[test]
    fn zero_obligation_identity_is_rejected() {
        assert_eq!(ObligationId::new(0), Err(InvalidObligationId));
    }

    #[test]
    fn tracker_counts_live_obligations_and_assigns_unique_generations() {
        under_lab(6, |contexts, _root| {
            let first = contexts.query().pin_snapshot(id(40)).unwrap();
            let second = contexts.query().pin_snapshot(id(40)).unwrap();
            assert_eq!(contexts.outstanding_obligations(), 2);

            let first_receipt = first.abort();
            assert_eq!(contexts.outstanding_obligations(), 1);
            let second_receipt = second.abort();
            assert_eq!(contexts.outstanding_obligations(), 0);
            assert_ne!(first_receipt.generation(), second_receipt.generation());
            assert_eq!(first_receipt.task_id(), second_receipt.task_id());
            assert_eq!(first_receipt.region_id(), second_receipt.region_id());
        });
    }

    #[test]
    fn synchronous_scope_installs_an_ambient_restriction() {
        under_lab(7, |contexts, _root| {
            assert!(!Cx::is_restricted());
            contexts.merge_eval().with_restriction(|| {
                assert!(Cx::is_restricted());
            });
            assert!(!Cx::is_restricted());
        });
    }

    #[test]
    fn storage_read_roles_implement_the_shared_contract() {
        fn assert_storage_read_role<T: StorageReadCx>() {}

        assert_storage_read_role::<QueryCx>();
        assert_storage_read_role::<TxnCx>();
        assert_storage_read_role::<CommitCx>();
        assert_storage_read_role::<MaintCx>();
        assert_storage_read_role::<ReplCx>();
    }

    // ---------------------------------------------------------------------
    // Lifecycle laws.
    //
    // The tests above each walk one hand-picked path with one hand-picked id.
    // The tests below quantify over the whole acquisition surface instead: all
    // nine acquisition methods across all five obligation-bearing roles, and
    // every terminal depth the affine chain admits. They are laws, not
    // examples, so a new acquisition method or a new boundary is expected to
    // be added to `every_acquisition` / `BOUNDARY_CHAIN` rather than to be
    // silently exempt from them.
    // ---------------------------------------------------------------------

    /// The non-terminal boundary chain, in order. `Resolution` is not a member:
    /// it is the single terminal event appended by `ObligationCore::resolve`.
    const BOUNDARY_CHAIN: [ObligationStage; 4] = [
        ObligationStage::Acquisition,
        ObligationStage::Transfer,
        ObligationStage::Publication,
        ObligationStage::Cleanup,
    ];

    /// Terminal depths: 0..=3 abort after that many boundaries have been
    /// crossed, and 4 is the fully discharged path.
    const TERMINAL_DEPTHS: [usize; 5] = [0, 1, 2, 3, 4];

    /// Drive one obligation to a terminal state at `depth`.
    ///
    /// The typestate makes each depth a distinct type, so this cannot be a
    /// loop over a runtime index inside the caller — that is precisely why the
    /// existing tests enumerate paths by hand. Collapsing the dispatch here
    /// lets the *laws* quantify over depth even though the *types* cannot.
    fn terminate_at_depth(
        obligation: PurposeObligation<Acquired>,
        depth: usize,
    ) -> ObligationReceipt {
        match depth {
            0 => obligation.abort(),
            1 => obligation.transfer().unwrap().abort(),
            2 => obligation.transfer().unwrap().publish().unwrap().abort(),
            3 => obligation
                .transfer()
                .unwrap()
                .publish()
                .unwrap()
                .cleanup()
                .unwrap()
                .abort(),
            4 => obligation
                .transfer()
                .unwrap()
                .publish()
                .unwrap()
                .cleanup()
                .unwrap()
                .complete()
                .unwrap(),
            _ => {
                assert!(depth <= 4, "no terminal path at depth {depth}");
                obligation.abort()
            }
        }
    }

    /// Every acquisition method the context surface offers, with the
    /// role/kind/units triple the source assigns to it.
    #[allow(clippy::type_complexity)]
    fn every_acquisition(
        contexts: &PurposeContexts,
        base: u64,
    ) -> Vec<(
        &'static str,
        ContextRole,
        DatabaseObligationKind,
        u64,
        PurposeObligation<Acquired>,
    )> {
        vec![
            (
                "query.pin_snapshot",
                ContextRole::Query,
                DatabaseObligationKind::PinSnapshot,
                1,
                contexts.query().pin_snapshot(id(base + 1)).unwrap(),
            ),
            (
                "txn.pin_snapshot",
                ContextRole::Txn,
                DatabaseObligationKind::PinSnapshot,
                1,
                contexts.txn().pin_snapshot(id(base + 2)).unwrap(),
            ),
            (
                "txn.reserve_prepared_bytes",
                ContextRole::Txn,
                DatabaseObligationKind::ReservePreparedBytes,
                16,
                contexts
                    .txn()
                    .reserve_prepared_bytes(id(base + 3), bytes(16))
                    .unwrap(),
            ),
            (
                "commit.reserve_prepared_bytes",
                ContextRole::Commit,
                DatabaseObligationKind::ReservePreparedBytes,
                32,
                contexts
                    .commit()
                    .reserve_prepared_bytes(id(base + 4), bytes(32))
                    .unwrap(),
            ),
            (
                "commit.reserve_raft_payload_space",
                ContextRole::Commit,
                DatabaseObligationKind::ReserveRaftPayloadSpace,
                64,
                contexts
                    .commit()
                    .reserve_raft_payload_space(id(base + 5), bytes(64))
                    .unwrap(),
            ),
            (
                "commit.publish_segment",
                ContextRole::Commit,
                DatabaseObligationKind::PublishSegment,
                1,
                contexts.commit().publish_segment(id(base + 6)).unwrap(),
            ),
            (
                "maint.publish_segment",
                ContextRole::Maint,
                DatabaseObligationKind::PublishSegment,
                1,
                contexts.maint().publish_segment(id(base + 7)).unwrap(),
            ),
            (
                "repl.reserve_raft_payload_space",
                ContextRole::Repl,
                DatabaseObligationKind::ReserveRaftPayloadSpace,
                128,
                contexts
                    .repl()
                    .reserve_raft_payload_space(id(base + 8), bytes(128))
                    .unwrap(),
            ),
            (
                "repl.publish_segment",
                ContextRole::Repl,
                DatabaseObligationKind::PublishSegment,
                1,
                contexts.repl().publish_segment(id(base + 9)).unwrap(),
            ),
        ]
    }

    /// L1. Every receipt's stage evidence is a prefix of [`BOUNDARY_CHAIN`]
    /// followed by exactly one terminal `Resolution`, at every terminal depth
    /// and for every acquisition method.
    ///
    /// The existing cancellation test pins four hand-written stage vectors;
    /// this derives the expected vector from the chain itself, so it also
    /// catches a stage recorded out of chain order, a duplicated stage, and a
    /// receipt that overruns the fixed-size evidence array.
    #[test]
    fn stage_evidence_is_a_chain_prefix_plus_one_resolution_at_every_depth() {
        under_lab(200, |contexts, _root| {
            for depth in TERMINAL_DEPTHS {
                for (label, _role, _kind, _units, obligation) in
                    every_acquisition(&contexts, 100 * depth as u64)
                {
                    let receipt = terminate_at_depth(obligation, depth);
                    let observed = stages(&receipt);

                    let mut expected: Vec<ObligationStage> =
                        BOUNDARY_CHAIN[..=depth.min(BOUNDARY_CHAIN.len() - 1)].to_vec();
                    expected.push(ObligationStage::Resolution);
                    assert_eq!(observed, expected, "{label} at depth {depth}");

                    // Exactly one Resolution, and it is last.
                    assert_eq!(
                        observed
                            .iter()
                            .filter(|stage| **stage == ObligationStage::Resolution)
                            .count(),
                        1,
                        "{label} at depth {depth}"
                    );
                    assert_eq!(
                        observed.last(),
                        Some(&ObligationStage::Resolution),
                        "{label} at depth {depth}"
                    );

                    // No stage is recorded twice.
                    let distinct: HashSet<ObligationStage> = observed.iter().copied().collect();
                    assert_eq!(distinct.len(), observed.len(), "{label} at depth {depth}");

                    // The fixed-size evidence array is never overrun.
                    assert!(
                        observed.len() <= MAX_OBLIGATION_EVENTS,
                        "{label} at depth {depth} recorded {} events",
                        observed.len()
                    );

                    // Only the terminal event carries a resolution, and it
                    // agrees with the receipt.
                    let expected_resolution = if depth == 4 {
                        ObligationResolution::Discharged
                    } else {
                        ObligationResolution::Aborted
                    };
                    assert_eq!(
                        receipt.resolution(),
                        expected_resolution,
                        "{label} at depth {depth}"
                    );
                    let resolutions: Vec<Option<ObligationResolution>> = receipt
                        .events()
                        .map(ObligationLifecycleEvent::resolution)
                        .collect();
                    for (index, resolution) in resolutions.iter().enumerate() {
                        let want = if index + 1 == resolutions.len() {
                            Some(expected_resolution)
                        } else {
                            None
                        };
                        assert_eq!(*resolution, want, "{label} at depth {depth} event {index}");
                    }

                    // Every event carries the same identity as its receipt.
                    for event in receipt.events() {
                        assert_eq!(event.id(), receipt.id(), "{label} at depth {depth}");
                        assert_eq!(
                            event.generation(),
                            receipt.generation(),
                            "{label} at depth {depth}"
                        );
                        assert_eq!(event.role(), receipt.role(), "{label} at depth {depth}");
                        assert_eq!(event.kind(), receipt.kind(), "{label} at depth {depth}");
                        assert_eq!(event.units(), receipt.units(), "{label} at depth {depth}");
                        assert_eq!(
                            event.task_id(),
                            receipt.task_id(),
                            "{label} at depth {depth}"
                        );
                        assert_eq!(
                            event.region_id(),
                            receipt.region_id(),
                            "{label} at depth {depth}"
                        );
                    }
                }
                assert_eq!(contexts.outstanding_obligations(), 0, "depth {depth}");
            }
        });
    }

    /// L2. Generations are dense and strictly increasing from
    /// `FIRST_OBLIGATION_GENERATION` across an interleaving of every role, not
    /// merely distinct within one role.
    ///
    /// `narrow_runtime_root` mints a fresh tracker, so the first generation
    /// handed out by a given `PurposeContexts` is pinned, not incidental.
    #[test]
    fn generations_are_dense_and_strictly_increasing_across_every_role() {
        under_lab(201, |contexts, _root| {
            let acquisitions = every_acquisition(&contexts, 0);
            assert_eq!(contexts.outstanding_obligations(), acquisitions.len());

            let receipts: Vec<ObligationReceipt> = acquisitions
                .into_iter()
                .map(|(_label, _role, _kind, _units, obligation)| obligation.abort())
                .collect();
            let generations: Vec<u64> = receipts
                .iter()
                .map(|receipt| receipt.generation().get())
                .collect();

            for pair in generations.windows(2) {
                assert!(
                    pair[0] < pair[1],
                    "generations not increasing: {generations:?}"
                );
            }
            let distinct: HashSet<u64> = generations.iter().copied().collect();
            assert_eq!(distinct.len(), generations.len(), "{generations:?}");

            // The first generation is written as a literal, deliberately.
            // Deriving it from `FIRST_OBLIGATION_GENERATION` would make this
            // assertion self-referential: a change to that constant would move
            // the expectation with the behaviour and the law would never fire.
            let expected: Vec<u64> = (1..=generations.len() as u64).collect();
            assert_eq!(generations, expected);
            assert_eq!(FIRST_OBLIGATION_GENERATION, expected[0]);

            // A second contexts instance restarts the sequence: the counter is
            // per-root, not global.
            assert_eq!(contexts.outstanding_obligations(), 0);
        });
    }

    /// L3. The live count is conserved: it tracks acquisitions minus
    /// terminations at every step, and every role's view of it agrees, because
    /// all obligation-bearing roles share one tracker.
    #[test]
    fn outstanding_count_is_conserved_and_agrees_across_every_role_view() {
        under_lab(202, |contexts, _root| {
            let views: [fn(&PurposeContexts) -> usize; 5] = [
                |c| c.query().outstanding_obligations(),
                |c| c.txn().outstanding_obligations(),
                |c| c.commit().outstanding_obligations(),
                |c| c.maint().outstanding_obligations(),
                |c| c.repl().outstanding_obligations(),
            ];
            let assert_all_views_agree = |expected: usize, at: &str| {
                assert_eq!(contexts.outstanding_obligations(), expected, "{at}");
                for (index, view) in views.iter().enumerate() {
                    assert_eq!(view(&contexts), expected, "role view {index} at {at}");
                }
            };

            assert_all_views_agree(0, "start");

            // `every_acquisition` builds its obligations eagerly, so it cannot
            // be used to observe the count rising one acquisition at a time.
            // These are deferred on purpose.
            #[allow(clippy::type_complexity)]
            let deferred: Vec<(
                &str,
                Box<dyn FnOnce() -> PurposeObligation<Acquired> + '_>,
            )> = vec![
                (
                    "query.pin_snapshot",
                    Box::new(|| contexts.query().pin_snapshot(id(1)).unwrap()),
                ),
                (
                    "txn.pin_snapshot",
                    Box::new(|| contexts.txn().pin_snapshot(id(2)).unwrap()),
                ),
                (
                    "txn.reserve_prepared_bytes",
                    Box::new(|| {
                        contexts
                            .txn()
                            .reserve_prepared_bytes(id(3), bytes(16))
                            .unwrap()
                    }),
                ),
                (
                    "commit.reserve_prepared_bytes",
                    Box::new(|| {
                        contexts
                            .commit()
                            .reserve_prepared_bytes(id(4), bytes(32))
                            .unwrap()
                    }),
                ),
                (
                    "commit.reserve_raft_payload_space",
                    Box::new(|| {
                        contexts
                            .commit()
                            .reserve_raft_payload_space(id(5), bytes(64))
                            .unwrap()
                    }),
                ),
                (
                    "commit.publish_segment",
                    Box::new(|| contexts.commit().publish_segment(id(6)).unwrap()),
                ),
                (
                    "maint.publish_segment",
                    Box::new(|| contexts.maint().publish_segment(id(7)).unwrap()),
                ),
                (
                    "repl.reserve_raft_payload_space",
                    Box::new(|| {
                        contexts
                            .repl()
                            .reserve_raft_payload_space(id(8), bytes(128))
                            .unwrap()
                    }),
                ),
                (
                    "repl.publish_segment",
                    Box::new(|| contexts.repl().publish_segment(id(9)).unwrap()),
                ),
            ];

            let mut live: Vec<PurposeObligation<Acquired>> = Vec::new();
            for (index, (label, acquire_one)) in deferred.into_iter().enumerate() {
                live.push(acquire_one());
                assert_all_views_agree(index + 1, label);
            }

            let acquired = live.len();
            for (index, obligation) in live.into_iter().enumerate() {
                let _receipt = obligation.abort();
                assert_all_views_agree(acquired - index - 1, "termination");
            }

            assert_all_views_agree(0, "end");
        });
    }

    /// The complete identifier vocabulary the redacted evidence surface is
    /// allowed to render: the two record type names, their field names, and
    /// the variants of the four stable enums they carry. Numbers are always
    /// allowed; free-form text never is.
    ///
    /// Adding a descriptive `String` field to either record — the exact leak
    /// the module header forbids — introduces a token outside this set.
    const EVIDENCE_VOCABULARY: [&str; 27] = [
        // record and newtype names
        "ObligationReceipt",
        "ObligationLifecycleEvent",
        "ObligationId",
        "ObligationGeneration",
        "Some",
        "None",
        // field names
        "id",
        "generation",
        "task_id",
        "region_id",
        "role",
        "kind",
        "stage",
        "units",
        "resolution",
        "events",
        "event_count",
        // ContextRole
        "Query",
        "Txn",
        "Commit",
        "Maint",
        "Repl",
        // DatabaseObligationKind
        "PinSnapshot",
        "ReservePreparedBytes",
        "ReserveRaftPayloadSpace",
        "PublishSegment",
        // ObligationStage and ObligationResolution are covered by the stage
        // and resolution spellings below.
        "Acquisition",
    ];

    /// Identifier-like tokens in `rendered` that are not in the allowed
    /// vocabulary. Digits and punctuation are ignored; this is a whitelist over
    /// identifiers, not a blacklist over words — a blacklist cannot work here,
    /// because the legitimate kind name `ReserveRaftPayloadSpace` contains
    /// "payload" and the legitimate stage `Publication` contains "public".
    fn foreign_evidence_tokens(rendered: &str) -> Vec<String> {
        let allowed: HashSet<&str> = EVIDENCE_VOCABULARY.into_iter().collect();
        let stage_and_resolution: HashSet<String> = [
            format!("{:?}", ObligationStage::Acquisition),
            format!("{:?}", ObligationStage::Transfer),
            format!("{:?}", ObligationStage::Publication),
            format!("{:?}", ObligationStage::Cleanup),
            format!("{:?}", ObligationStage::Resolution),
            format!("{:?}", ObligationResolution::Discharged),
            format!("{:?}", ObligationResolution::Aborted),
        ]
        .into_iter()
        .collect();

        let mut foreign = Vec::new();
        let mut token = String::new();
        let flush = |token: &mut String, foreign: &mut Vec<String>| {
            if !token.is_empty() {
                let candidate = std::mem::take(token);
                let is_number = candidate.chars().all(|c| c.is_ascii_digit());
                if !is_number
                    && !allowed.contains(candidate.as_str())
                    && !stage_and_resolution.contains(&candidate)
                {
                    foreign.push(candidate);
                }
            }
        };
        for ch in rendered.chars() {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                token.push(ch);
            } else {
                flush(&mut token, &mut foreign);
            }
        }
        flush(&mut token, &mut foreign);
        foreign
    }

    /// L4. The evidence surface renders only its closed vocabulary, for every
    /// acquisition method at every terminal depth — not just for the one
    /// discharged receipt the existing lifecycle test inspects.
    ///
    /// The instrument is controlled in both directions inside the test: a
    /// fabricated render must be rejected, and a real one must be accepted.
    /// Without the negative control a checker that silently matched nothing
    /// would pass this test on every input.
    #[test]
    fn evidence_renders_only_its_closed_vocabulary_for_every_method_and_depth() {
        // Negative control: a fabricated leak is caught — both the foreign
        // field name and its free-form value.
        assert_eq!(
            foreign_evidence_tokens("ObligationReceipt { tenant: acme_corp }"),
            vec!["tenant".to_string(), "acme_corp".to_string()],
            "vocabulary checker failed to reject a fabricated leak"
        );
        // Positive control: the closed vocabulary and bare numbers are clean.
        assert!(
            foreign_evidence_tokens("ObligationReceipt { units: 4096, role: Commit }").is_empty(),
            "vocabulary checker rejected a legitimate render"
        );

        under_lab(203, |contexts, _root| {
            for depth in TERMINAL_DEPTHS {
                for (label, role, kind, _units, obligation) in
                    every_acquisition(&contexts, 100 * depth as u64)
                {
                    let receipt = terminate_at_depth(obligation, depth);
                    let rendered =
                        format!("{receipt:?} {:?}", receipt.events().collect::<Vec<_>>());

                    let foreign = foreign_evidence_tokens(&rendered);
                    assert!(
                        foreign.is_empty(),
                        "{label} at depth {depth} rendered foreign tokens {foreign:?}: {rendered}"
                    );

                    // Non-vacuity: the surface really is rendering the stable
                    // enums, so the assertion above is testing something.
                    assert!(
                        rendered.contains(&format!("{role:?}")),
                        "{label} at depth {depth} omitted its role: {rendered}"
                    );
                    assert!(
                        rendered.contains(&format!("{kind:?}")),
                        "{label} at depth {depth} omitted its kind: {rendered}"
                    );
                    assert!(
                        receipt.events().count() >= 2,
                        "{label} at depth {depth} rendered too few events"
                    );
                }
                assert_eq!(contexts.outstanding_obligations(), 0, "depth {depth}");
            }
        });
    }

    /// L5. `units` round-trips the requested reservation for the byte-carrying
    /// kinds across the whole `NonZeroU64` range, and is the fixed sentinel `1`
    /// for the kinds that reserve no bytes.
    ///
    /// The existing lifecycle test pins this at the single value 4096.
    #[test]
    fn units_round_trip_the_requested_reservation_for_every_byte_carrying_kind() {
        const REQUESTS: [u64; 8] = [1, 2, 3, 7, 63, 4096, u64::MAX - 1, u64::MAX];

        under_lab(204, |contexts, _root| {
            for (index, request) in REQUESTS.into_iter().enumerate() {
                let base = 10 * index as u64;
                let reservations = [
                    (
                        "txn.reserve_prepared_bytes",
                        contexts
                            .txn()
                            .reserve_prepared_bytes(id(base + 1), bytes(request))
                            .unwrap(),
                    ),
                    (
                        "commit.reserve_prepared_bytes",
                        contexts
                            .commit()
                            .reserve_prepared_bytes(id(base + 2), bytes(request))
                            .unwrap(),
                    ),
                    (
                        "commit.reserve_raft_payload_space",
                        contexts
                            .commit()
                            .reserve_raft_payload_space(id(base + 3), bytes(request))
                            .unwrap(),
                    ),
                    (
                        "repl.reserve_raft_payload_space",
                        contexts
                            .repl()
                            .reserve_raft_payload_space(id(base + 4), bytes(request))
                            .unwrap(),
                    ),
                ];
                for (label, obligation) in reservations {
                    let receipt = obligation.abort();
                    assert_eq!(receipt.units(), request, "{label} with {request}");
                    for event in receipt.events() {
                        assert_eq!(event.units(), request, "{label} with {request}");
                    }
                }
            }

            // The kinds that reserve no bytes report the fixed sentinel, and it
            // does not vary with the id they were acquired under.
            for (label, _role, kind, expected_units, obligation) in
                every_acquisition(&contexts, 900)
            {
                let receipt = obligation.abort();
                assert_eq!(receipt.units(), expected_units, "{label}");
                if !matches!(
                    kind,
                    DatabaseObligationKind::ReservePreparedBytes
                        | DatabaseObligationKind::ReserveRaftPayloadSpace
                ) {
                    assert_eq!(receipt.units(), 1, "{label} is not byte-carrying");
                }
            }

            assert_eq!(contexts.outstanding_obligations(), 0);
        });
    }
}
