//! Complete two-cut relational execution over one authenticated cold view.
//! The existing buffered program owns every source and intermediate barrier;
//! historical composition owns only endpoint admission and shared allowances.

use super::*;
use crate::{BufferedReadView, PreparedBufferedOrder};
use fgdb_gql::PreparedGraphSet;
use fgdb_gql::spill_set::AsyncSpillSetPlan;
use fgdb_types::CanonicalScalarResolver;

struct Endpoints {
    before: PreparedBufferedOrder,
    after: PreparedBufferedOrder,
}

fn prepare<V: Vfs + Clone>(
    prepared: &PreparedNativeRead,
    view: &BufferedReadView<V>,
    cx: &QueryCx,
    params: &GqlParameters,
    before: CommitSeq,
    after: CommitSeq,
) -> VisitResult<Endpoints> {
    // Apply the SAME publication fence as ordinary buffered sources before
    // binding, even for source-free queries and LIMIT 0. Each actual source
    // retains its existing history/visibility validation during execution.
    let publication = view.frontier();
    for requested in [before, after] {
        if requested > publication {
            return Err(GqlQueryError::Source(NativeSpoolError::BufferedRead(
                Box::new(crate::BufferedReadError::BeyondPublication {
                    requested,
                    publication,
                }),
            )));
        }
    }
    cx.with_restriction(|| {
        cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
        let definition = |error| GqlQueryError::Source(NativeSpoolError::Prepare(Box::new(error)));
        let unsupported = || {
            definition(QueryError::StreamingUnsupported {
                facade: prepared.facade_class(),
            })
        };
        // Pattern-to-relation is the existing lossless typed constructor. The
        // complete pattern's DISTINCT/order/window remains inside its leaf;
        // no synthetic projection, first-source extraction or text rewrite.
        let query = match prepared {
            PreparedNativeRead::Pattern(template) => PreparedGraphSet::from(
                template
                    .bind_parameters(params)
                    .map_err(|error| definition(QueryError::PatternText(error)))?,
            ),
            PreparedNativeRead::Set(template) => template
                .bind_parameters(params)
                .map_err(|error| definition(QueryError::SetText(error)))?,
            // Temporal selectors must not silently replace explicit cuts.
            // Terminal exact aggregates use a different result-cell carrier.
            _ => return Err(unsupported()),
        };
        let plan =
            AsyncSpillSetPlan::compile_with_bounded_walks(&query).map_err(|_| unsupported())?;
        cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
        Ok(Endpoints {
            before: PreparedBufferedOrder::from_relation(plan.clone(), Some(before)),
            after: PreparedBufferedOrder::from_relation(plan, Some(after)),
        })
    })
}

impl PreparedNativeRead {
    /// Bind one query once and compare two exact revisions through a cold view.
    /// This is the buffered counterpart of spool_difference_in_view: no
    /// resident Snapshot, eager endpoint result, or final result bag is built.
    /// Both publication checks and physical compilation happen synchronously before
    /// graph reads or scratch reservations. The future owns its bound program;
    /// this template and the parameter map may change/drop immediately.
    ///
    /// Each endpoint uses PreparedBufferedOrder's COMPLETE relational executor.
    /// Its admitted vertex, identified-edge/fixed-hop, bounded-path, source-free,
    /// set, Cartesian/keyed/outer join and unary stage profiles keep their own
    /// predicates, projections, UNWIND, DISTINCT, hidden ordering keys and
    /// local windows. Supported intermediate grouping converts to the native
    /// row domain BEFORE subsequent stages. Terminal exact aggregate carriers,
    /// procedures and other unsupported physical shapes refuse during compile;
    /// there is no eager fallback. Explicit temporal selectors refuse rather
    /// than overriding either requested cut. Equal cuts still execute twice.
    ///
    /// Native source-record/evaluator-work/evaluator-scratch quotas cover both
    /// complete endpoints. The second gets only the first's measured remainder.
    /// max_input_rows keeps the buffered executor's private occurrence bound;
    /// group_capacity/max_partitions and max_runs retain their per-reducer and
    /// per-sort meanings. Only the final nonzero difference spends ResultRows.
    /// One additional max_work_units covers encoding, both complete endpoint
    /// pipelines, both canonical difference sorts and every later cursor pull.
    /// Physical work refusals retain that original limit, not a reset allowance.
    ///
    /// All THREE files must be distinct query-private owners. Endpoint results
    /// append to source, using scratch and auxiliary for the existing staged
    /// algorithms; final difference inputs finish in scratch. Successful and
    /// abandoned intermediate extents keep their append quotas. Use children
    /// of a common MemoryPool for a combined charged residency bound. Graph
    /// metadata/rows use the cold view's pool; spill pages, decoded stage rows,
    /// encodings and sort buffers use the existing file pools. Initial recovery,
    /// bound plans/catalogs, and application-owned output retain their existing
    /// bounds. This is not unlimited database-open scalability or a GC lease.
    ///
    /// resolver carries pinned scalar artifacts through BOTH endpoint pipelines
    /// (None for built-in domains). Name resolution is not repeated. As for
    /// ordinary buffered execution, it must not substitute locale/timezone or
    /// artifact generations between calls. It is not retained by the cursor.
    ///
    /// The future exclusively borrows view and all three files until opening
    /// completes. The returned cursor borrows ONLY scratch and cx: view, source,
    /// auxiliary, writer, template and parameters may then drop. Stage every
    /// delivered change until successful exhaustion. Errors, dropped in-flight
    /// work, file poisoning, close and retry keep the existing cursor rules.
    /// This owner-level same-history API grants no capability and supplies no
    /// cross-branch proof, durable resume or raw graph DIFF language surface.
    #[allow(clippy::too_many_arguments)]
    pub fn spool_buffered_difference_in_view<'q, 's, V, A, B, C>(
        &self,
        view: &'s mut BufferedReadView<V>,
        cx: &'q QueryCx,
        params: &GqlParameters,
        before: CommitSeq,
        after: CommitSeq,
        policy: GqlQueryPolicy,
        source: &'s mut SpillFile<A>,
        scratch: &'q mut SpillFile<B>,
        auxiliary: &'s mut SpillFile<C>,
        group_capacity: usize,
        max_partitions: usize,
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_row_bytes: usize,
        max_input_rows: u64,
        max_work_units: u64,
        resolver: Option<&'s (dyn CanonicalScalarResolver + Send + Sync)>,
    ) -> crate::SendFuture<'s, VisitResult<NativeSpoolDifference<'q, B>>>
    where
        'q: 's,
        V: Vfs + Clone + 's,
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 's,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        C: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 's,
    {
        let opened = prepare(self, view, cx, params, before, after);
        Box::pin(async move {
            let endpoints = opened?;
            let input_policy = private_policy(policy);
            let (before_result, used) = native(
                endpoints
                    .before
                    .spool_in_view_with_resolver(
                        view,
                        cx,
                        input_policy,
                        &mut *scratch,
                        &mut *source,
                        &mut *auxiliary,
                        group_capacity,
                        max_partitions,
                        run_rows,
                        max_runs,
                        page_bytes,
                        max_row_bytes,
                        max_input_rows,
                        max_work_units,
                        resolver,
                    )
                    .await,
            )?;
            let input_policy = native(remaining(input_policy, &before_result))?;
            let allowance = native(subtract(max_work_units, used))?;
            let (after_result, spent) = native(
                endpoints
                    .after
                    .spool_in_view_with_resolver(
                        view,
                        cx,
                        input_policy,
                        &mut *scratch,
                        &mut *source,
                        &mut *auxiliary,
                        group_capacity,
                        max_partitions,
                        run_rows,
                        max_runs,
                        page_bytes,
                        max_row_bytes,
                        max_input_rows,
                        allowance,
                        resolver,
                    )
                    .await
                    .map_err(|error| physical_error(error, used, max_work_units)),
            )?;
            if before_result.snapshot_seq() != before || after_result.snapshot_seq() != after {
                return Err(GqlQueryError::Source(NativeSpoolError::IncompleteCursor));
            }
            // Endpoints cannot change through this exclusive view borrow.
            // Drop all bound programs before the file-only cursor escapes.
            drop(endpoints);
            let used = used
                .checked_add(spent)
                .ok_or(GqlQueryError::Source(SpillError::SizeOverflow.into()))?;
            before_result
                .difference_continuing(
                    &after_result,
                    cx,
                    source,
                    scratch,
                    run_rows,
                    max_runs,
                    page_bytes,
                    policy.rows.max_result_rows().unwrap_or(u64::MAX),
                    max_work_units,
                    used,
                )
                .await
        })
    }
}

#[cfg(test)]
#[path = "buffered/tests.rs"]
mod tests;
