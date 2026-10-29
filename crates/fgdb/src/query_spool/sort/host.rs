//! One external relational executor with host-owned graph and element reads.
//! Correlation changes an explicit argument frame, never the pinned source.

use super::*;
use fgdb_gql::spill_set::{AsyncSpillSetPlan, AsyncSpillSetSourcePlan};
use fgdb_types::CanonicalScalarResolver;

/// Physical allowances belong to the query's scratch owner. Recursive calls
/// receive residual work, while the files retain their append-only quotas.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ExternalOrderLimits {
    pub(crate) group_capacity: usize,
    pub(crate) max_partitions: usize,
    pub(crate) run_rows: usize,
    pub(crate) max_runs: usize,
    pub(crate) page_bytes: usize,
    pub(crate) max_row_bytes: usize,
    pub(crate) max_input_rows: u64,
    pub(crate) max_sort_work: u64,
}

/// A physical owner supplies only its exact-cut graph leaves, reached element
/// reads and live authority controls. Relational semantics and scratch work
/// remain in the shared executor for resident, buffered and signed sources.
pub(crate) trait ExternalOrderHost: Send {
    fn check_snapshot(&mut self, snapshot: CommitSeq) -> Result<()>;
    fn checkpoint(&mut self) -> Result<()>;
    fn native_event(&mut self, event: GlaExecutionEvent) -> Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn leaf<'q, A, B>(
        &'q mut self,
        plan: &'q AsyncSpillSetPlan,
        snapshot: CommitSeq,
        cx: &'q QueryCx,
        policy: GqlQueryPolicy,
        scratch: &'q mut SpillFile<A>,
        destination: &'q mut SpillFile<B>,
        limits: ExternalOrderLimits,
        resolver: Option<&'q (dyn CanonicalScalarResolver + Send + Sync)>,
    ) -> crate::SendFuture<'q, Result<(NativeResultSpool, u64)>>
    where
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q;

    #[allow(clippy::too_many_arguments)]
    fn stages<'q, A, B>(
        &'q mut self,
        plan: &'q AsyncSpillSetPlan,
        input: NativeResultSpool,
        cx: &'q QueryCx,
        policy: GqlQueryPolicy,
        scratch: &'q mut SpillFile<A>,
        destination: &'q mut SpillFile<B>,
        limits: ExternalOrderLimits,
        prior_work: u64,
        resolver: Option<&'q (dyn CanonicalScalarResolver + Send + Sync)>,
    ) -> crate::SendFuture<'q, Result<(NativeResultSpool, u64)>>
    where
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q;
}

pub(super) fn event<H: ExternalOrderHost>(
    host: &mut H,
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    evaluator: &mut GlaExecutionStats,
    event: GlaExecutionEvent,
) -> Result<()> {
    host.native_event(event)?;
    staged::native_event(cx, policy, evaluator, event)
}

/// Attach the caller's live native control while preserving the existing
/// relational kernels and their exact local counter/event order.
pub(super) struct ControlledStats<'a> {
    pub(super) stats: GlaExecutionStats,
    pub(super) control: &'a mut (dyn FnMut(GlaExecutionEvent) -> Result<()> + Send),
}

impl staged::NativeCounter for ControlledStats<'_> {
    fn admit_event(&mut self, event: GlaExecutionEvent) -> Result<()> {
        (self.control)(event)
    }
    fn stats(&mut self) -> &mut GlaExecutionStats {
        &mut self.stats
    }
}

/// Complete one compiled relation into destination. The argument is borrowed
/// only for this invocation. Its owner keeps its byte reservation alive across
/// every awaited child; nested Apply shadows it only for that child's body.
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_relation<'q, H, A, B, C>(
    plan: &'q AsyncSpillSetPlan,
    arguments: Option<&'q GraphValueRow>,
    snapshot: CommitSeq,
    host: &'q mut H,
    cx: &'q QueryCx,
    policy: GqlQueryPolicy,
    scratch: &'q mut SpillFile<A>,
    destination: &'q mut SpillFile<B>,
    auxiliary: &'q mut SpillFile<C>,
    limits: ExternalOrderLimits,
    resolver: Option<&'q (dyn CanonicalScalarResolver + Send + Sync)>,
) -> crate::SendFuture<'q, Result<(NativeResultSpool, u64)>>
where
    H: ExternalOrderHost + 'q,
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    C: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
{
    // Authority, exact-cut and physical-shape admission precede allocation.
    // Otherwise a small pool could disguise a stale/future cut or an invalid
    // limit as a recursive-frame memory failure. Public owners call this only
    // from their polled execution future; unpolled queries still reserve none.
    let admitted = (|| {
        host.checkpoint()?;
        host.check_snapshot(snapshot)?;
        validate_spill_limits(
            limits.run_rows,
            limits.max_runs,
            limits.page_bytes,
            limits.max_sort_work,
        )
    })();
    if let Err(error) = admitted {
        return Box::pin(async move { Err(error) });
    }
    // The plan has a static depth bound; the future itself is still resident
    // query scratch and must be admitted before allocating its recursion box.
    let pool = destination.memory_pool().clone();
    recursion_box(&pool, cx, move || async move {
        let source_policy = input_policy(policy, limits.max_input_rows);
        let (input, work) = source(
            plan,
            arguments,
            snapshot,
            &mut *host,
            cx,
            source_policy,
            &mut *scratch,
            &mut *destination,
            &mut *auxiliary,
            limits,
            resolver,
        )
        .await?;
        host.checkpoint()?;
        host.stages(
            plan,
            input,
            cx,
            policy,
            scratch,
            destination,
            limits,
            work,
            resolver,
        )
        .await
    })
}

/// The relation's source, built outside the recursive frame as its own
/// admitted box. Inline, every source kind's future would sit in that frame,
/// and an unoptimized build keeps each of them in its poll frame, so a nested
/// Apply body paid for all of them at every level and overflowed a test
/// thread's 2 MiB stack.
#[allow(clippy::too_many_arguments)]
fn source<'r, H, A, B, C>(
    plan: &'r AsyncSpillSetPlan,
    arguments: Option<&'r GraphValueRow>,
    snapshot: CommitSeq,
    host: &'r mut H,
    cx: &'r QueryCx,
    policy: GqlQueryPolicy,
    scratch: &'r mut SpillFile<A>,
    destination: &'r mut SpillFile<B>,
    auxiliary: &'r mut SpillFile<C>,
    limits: ExternalOrderLimits,
    resolver: Option<&'r (dyn CanonicalScalarResolver + Send + Sync)>,
) -> crate::SendFuture<'r, Result<(NativeResultSpool, u64)>>
where
    H: ExternalOrderHost + 'r,
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'r,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'r,
    C: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'r,
{
    let pool = destination.memory_pool().clone();
    match plan.source() {
        AsyncSpillSetSourcePlan::Values(source) => recursion_box(&pool, cx, move || {
            apply::seed_values(source, snapshot, host, cx, policy, destination, limits)
        }),
        AsyncSpillSetSourcePlan::Arguments(source) => recursion_box(&pool, cx, move || {
            apply::seed_arguments(
                source,
                arguments,
                snapshot,
                host,
                cx,
                policy,
                destination,
                limits,
            )
        }),
        AsyncSpillSetSourcePlan::Apply(source) => recursion_box(&pool, cx, move || {
            apply::execute(
                source,
                arguments,
                snapshot,
                host,
                cx,
                policy,
                scratch,
                destination,
                auxiliary,
                limits,
                resolver,
            )
        }),
        AsyncSpillSetSourcePlan::Binary(source) => recursion_box(&pool, cx, move || {
            binary::execute(
                source,
                arguments,
                snapshot,
                host,
                cx,
                policy,
                scratch,
                destination,
                auxiliary,
                limits.group_capacity,
                limits.max_partitions,
                limits.run_rows,
                limits.max_runs,
                limits.page_bytes,
                limits.max_row_bytes,
                limits.max_input_rows,
                limits.max_sort_work,
                resolver,
            )
        }),
        AsyncSpillSetSourcePlan::Cross(source) => recursion_box(&pool, cx, move || {
            cross::execute(
                source,
                arguments,
                snapshot,
                host,
                cx,
                policy,
                scratch,
                destination,
                auxiliary,
                limits.group_capacity,
                limits.max_partitions,
                limits.run_rows,
                limits.max_runs,
                limits.page_bytes,
                limits.max_row_bytes,
                limits.max_input_rows,
                limits.max_sort_work,
                resolver,
            )
        }),
        AsyncSpillSetSourcePlan::RowJoin(source) => recursion_box(&pool, cx, move || {
            relational_join::execute(
                source,
                arguments,
                snapshot,
                host,
                cx,
                policy,
                scratch,
                destination,
                auxiliary,
                limits.group_capacity,
                limits.max_partitions,
                limits.run_rows,
                limits.max_runs,
                limits.page_bytes,
                limits.max_row_bytes,
                limits.max_input_rows,
                limits.max_sort_work,
                resolver,
            )
        }),
        AsyncSpillSetSourcePlan::Aggregate(source) => recursion_box(&pool, cx, move || {
            crate::query::view::spool::aggregate::spool_group_stage_with_host(
                source,
                arguments,
                snapshot,
                host,
                cx,
                policy,
                scratch,
                destination,
                auxiliary,
                limits,
                resolver,
            )
        }),
        // The host boxes its own leaves.
        AsyncSpillSetSourcePlan::Vertex(_)
        | AsyncSpillSetSourcePlan::Edge(_)
        | AsyncSpillSetSourcePlan::Join(_) => host.leaf(
            plan,
            snapshot,
            cx,
            policy,
            scratch,
            destination,
            limits,
            resolver,
        ),
    }
}

/// Admit a future's resident bytes before allocating its box. The future is
/// built here, so only this instantiation's frame holds it unboxed.
fn recursion_box<'r, F>(
    pool: &fgdb_strata::tiered::memory::MemoryPool,
    cx: &QueryCx,
    make: impl FnOnce() -> F,
) -> crate::SendFuture<'r, Result<(NativeResultSpool, u64)>>
where
    F: core::future::Future<Output = Result<(NativeResultSpool, u64)>> + Send + 'r,
{
    let future = make();
    let bytes = size_of_val(&future)
        .checked_add(size_of::<fgdb_strata::tiered::memory::MemoryCharge>())
        .and_then(|bytes| bytes.checked_add(64));
    let Some(bytes) = bytes else {
        return Box::pin(async { Err(SpillError::SizeOverflow.into()) });
    };
    let reservation = match pool.reserve(cx, bytes) {
        Ok(reservation) => reservation,
        Err(error) => return Box::pin(async move { Err(SpillError::Memory(error).into()) }),
    };
    Box::pin(async move {
        let _reservation = reservation;
        future.await
    })
}
