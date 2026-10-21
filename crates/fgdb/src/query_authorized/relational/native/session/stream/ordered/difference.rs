//! Historical result differences stay inside the ordered session's authority.
//! The existing masked sources and external sorter retain all query semantics.

use super::*;
use crate::NativeResultSpool;

// Delay cursor construction, not source admission. Both sources own the same
// pin before returning a future; the second cursor receives the first's exact
// remaining native allowance. Neither source can mint another signed permit.
type PendingInput<'q> = Box<dyn FnOnce(GqlQueryPolicy) -> OrderedInput<'q> + Send + 'q>;
struct Endpoint<'q> {
    input: PendingInput<'q>,
    tail: ScanSortTail,
}

fn pending<'q, C: FnMut() -> u64 + Send>(
    plan: RowPlan,
    pull: Option<GraphSetRowPull>,
    at: CommitSeq,
    view: &EmbeddedReadView,
    cx: &'q QueryCx,
    execution: Shared<'q, C>,
    max_input_rows: u64,
) -> core::result::Result<PendingInput<'q>, QueryError> {
    let input_policy = move |mut policy: GqlQueryPolicy| {
        policy.rows = match policy.rows.max_snapshot_records() {
            Some(records) => GqlExecutionBudget::new(records, max_input_rows),
            None => GqlExecutionBudget::result_rows(max_input_rows),
        };
        policy
    };
    let control = execution.clone();
    match plan {
        RowPlan::Vertex(plan) => {
            let source = ScopedSource {
                inner: view
                    .vertex_scan_source(cx, at)
                    .map_err(QueryError::Read)?
                    .with_root_equality(&plan, |key| {
                        execution.borrow().permit.predicates().allows_property(key)
                    }),
                execution,
            };
            Ok(Box::new(move |policy| {
                let rows = VertexInput(VertexScanCursor::new(
                    source,
                    plan,
                    input_policy(policy),
                    move || control.borrow_mut().checkpoint(),
                ));
                let input: Box<dyn InputCursor + 'q> = match pull {
                    Some(pull) => Box::new(SetInput { rows, pull }),
                    None => Box::new(rows),
                };
                OrderedInput(input)
            }))
        }
        RowPlan::Edge(plan) => {
            let source = aggregate::edge::ordered_source(at, view, cx, execution)?;
            Ok(Box::new(move |policy| {
                OrderedInput(Box::new(EdgeInput(EdgeScanCursor::new(
                    source,
                    plan,
                    input_policy(policy),
                    move || control.borrow_mut().checkpoint(),
                ))))
            }))
        }
    }
}

fn private_policy(mut policy: GqlQueryPolicy) -> GqlQueryPolicy {
    policy.rows = match policy.rows.max_snapshot_records() {
        Some(records) => GqlExecutionBudget::snapshot_records(records),
        None => GqlExecutionBudget::UNLIMITED,
    };
    policy
}
fn remainder(limit: u64, used: u64) -> Result<u64> {
    limit
        .checked_sub(used)
        .ok_or(NativeSpoolError::IncompleteCursor)
}
fn after_policy(mut policy: GqlQueryPolicy, first: &NativeResultSpool) -> Result<GqlQueryPolicy> {
    if let Some(limit) = policy.rows.max_snapshot_records() {
        policy.rows = GqlExecutionBudget::snapshot_records(remainder(
            limit,
            first.row_stats().snapshot_records,
        )?);
    }
    policy.evaluator.max_work_units = remainder(
        policy.evaluator.max_work_units,
        first.evaluator_stats().work_units,
    )?;
    policy.evaluator.max_scratch_entries = remainder(
        policy.evaluator.max_scratch_entries,
        first.evaluator_stats().scratch_entries,
    )?;
    Ok(policy)
}
fn difference_error(error: GqlQueryError<NativeSpoolError, Cancel>) -> NativeSpoolError {
    match error {
        GqlQueryError::Source(error) => error,
        GqlQueryError::Interrupted(error) => SpillError::Interrupted(error).into(),
        GqlQueryError::Rows(error) => authorized(QueryError::Stream(GqlQueryError::Rows(error))),
        GqlQueryError::Evaluator(error) => {
            authorized(QueryError::Stream(GqlQueryError::Evaluator(error)))
        }
        GqlQueryError::Data(error) => authorized(QueryError::Stream(GqlQueryError::Data(error))),
        GqlQueryError::IdentifiedEdgesRequired => {
            authorized(QueryError::Stream(GqlQueryError::IdentifiedEdgesRequired))
        }
    }
}

impl<R, C, A, B> AuthorizedOrderedReadSession<'_, R, C, A, B>
where
    R: GraphSymbolResolver,
    C: FnMut() -> u64 + Send,
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    /// Visit result(after) - result(before) under ONE live capability permit.
    /// Both cuts belong to this session's immutable generation. Scope masking
    /// precedes predicates, projection, ordering and difference at EACH cut;
    /// an element becoming visible or hidden therefore has the ordinary masked
    /// result semantics. No privileged query_diff or unmasked source is used.
    ///
    /// Rebind the exact-session template once synchronously. Admit both cuts
    /// and physical sources before returning the future. The caller cannot
    /// replace the catalog, clock, policy or private scratch files. Temporal
    /// selectors refuse instead of overriding either argument. The existing
    /// ordered Pattern and one-source row-projection profiles are supported;
    /// unsupported sets/aggregates refuse without an eager fallback.
    ///
    /// Each endpoint completes its full DISTINCT/order/window before the
    /// existing external merge compares visible tuples. Native source-record,
    /// work and scratch allowances cover BOTH endpoints. One max_sort_work
    /// allowance, reserved in full from signed work, covers both endpoint
    /// pipelines AND both difference sorts, merge and callback checkpoints.
    /// max_input_rows is per endpoint; max_runs retains the sorter's meaning.
    /// File quotas stay append-only and shared pools retain their own bounds.
    /// Native and signed output-row limits count only nonzero changed tuples,
    /// never the private endpoints or the absolute signed multiplicities.
    ///
    /// Each callback receives an exact signed multiplicity and a charged
    /// canonical value-row frame. Signed delivery admission precedes invoking
    /// it, and its future completes before another change is delivered. Every
    /// future poll and callback completion rechecks live authority. No source,
    /// work or scratch statistics escape; success returns only public columns.
    /// The graph remains resident; this adds authorized result spill, not a
    /// cold authorized view, descriptor/timing noninterference or cross-branch
    /// coverage. The underlying sources retain their existing side channels.
    ///
    /// Both endpoint results finish before callbacks. A later comparison,
    /// callback, expiry or cancellation failure still invalidates a delivered
    /// prefix: stage all changes until successful completion. Retrying can
    /// repeat callbacks. Drop does not drain, acknowledge or unpoison scratch.
    /// Host callback unwind and terminal credentials close the session; ordinary
    /// query failures preserve it, subject to the files' existing poison rules.
    #[allow(clippy::too_many_arguments)]
    pub fn visit_difference<'q, F, Fut>(
        &'q mut self,
        cx: &'q QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
        before: CommitSeq,
        after: CommitSeq,
        mut visitor: F,
    ) -> Result<crate::SendFuture<'q, Result<Vec<String>>>>
    where
        F: FnMut(i128, TrackedBytes) -> Fut + Send + 'q,
        Fut: Future<Output = Result<()>> + Send + 'q,
    {
        let Self {
            session,
            scratch,
            destination,
            limits,
        } = self;
        let limits = *limits;
        let state = session.state.as_mut().ok_or_else(|| {
            authorized(QueryError::Authorization(
                AuthorizationError::ExecutionStopped,
            ))
        })?;
        let State {
            view,
            capability,
            branch,
            policy,
            clock,
            last_now_ms,
            ..
        } = state;
        let mut guard = PinGuard {
            pin: view,
            armed: true,
        };
        let opened = open(
            guard.pin.as_ref().ok_or_else(|| {
                authorized(QueryError::Authorization(
                    AuthorizationError::ExecutionStopped,
                ))
            })?,
            capability,
            clock,
            last_now_ms,
            cx,
            branch,
            &session.owner,
            prepared,
            params,
            *policy,
            |native, params, _| {
                refuse_element_identity(native.statement())?;
                if !matches!(
                    native,
                    PreparedNativeRead::Pattern(_) | PreparedNativeRead::Set(_)
                ) {
                    return Err(QueryError::StreamingUnsupported {
                        facade: native.facade_class(),
                    });
                }
                bind_ordered(native, params, before)
            },
            |(plan, tail, _, columns, pull), view, cx, execution, policy| {
                view.snapshot
                    .check_frontier(before)
                    .map_err(QueryError::Read)?;
                view.snapshot
                    .check_frontier(after)
                    .map_err(QueryError::Read)?;
                execution.borrow_mut().reserve_work(limits.max_sort_work)?;
                let second_plan = match &plan {
                    RowPlan::Vertex(plan) => RowPlan::Vertex(plan.clone()),
                    RowPlan::Edge(plan) => RowPlan::Edge(plan.clone()),
                };
                let second_pull = pull.clone();
                let first = Endpoint {
                    input: pending(
                        plan,
                        pull,
                        before,
                        view,
                        cx,
                        execution.clone(),
                        limits.max_input_rows,
                    )?,
                    tail: tail.clone(),
                };
                let second = Endpoint {
                    input: pending(
                        second_plan,
                        second_pull,
                        after,
                        view,
                        cx,
                        execution.clone(),
                        limits.max_input_rows,
                    )?,
                    tail,
                };
                execution.borrow_mut().checkpoint()?;
                Ok((first, second, columns, policy, execution))
            },
        );
        let (first, second, columns, policy, execution) = match opened {
            Ok(opened) => {
                guard.armed = false;
                opened
            }
            Err(error) => {
                guard.armed = terminal_error(&error);
                return Err(authorized(error));
            }
        };
        Ok(Box::pin(async move {
            let future = async {
                let input_policy = private_policy(policy);
                let (first, used) = ordered_input(
                    cx,
                    columns.clone(),
                    (first.input)(input_policy),
                    first.tail,
                    input_policy,
                    &mut *scratch,
                    &mut *destination,
                    limits.run_rows,
                    limits.max_runs,
                    limits.page_bytes,
                    limits.max_row_bytes,
                    limits.max_sort_work,
                )
                .await?;
                let input_policy = after_policy(input_policy, &first)?;
                let allowance = remainder(limits.max_sort_work, used)?;
                let (second, spent) = ordered_input(
                    cx,
                    columns.clone(),
                    (second.input)(input_policy),
                    second.tail,
                    input_policy,
                    &mut *scratch,
                    &mut *destination,
                    limits.run_rows,
                    limits.max_runs,
                    limits.page_bytes,
                    limits.max_row_bytes,
                    allowance,
                )
                .await?;
                if first.snapshot_seq() != before || second.snapshot_seq() != after {
                    return Err(NativeSpoolError::IncompleteCursor);
                }
                let allowance = remainder(allowance, spent)?;
                first
                    .visit_difference(
                        &second,
                        cx,
                        destination,
                        scratch,
                        limits.run_rows,
                        limits.max_runs,
                        limits.page_bytes,
                        policy.rows.max_result_rows().unwrap_or(u64::MAX),
                        allowance,
                        |weight, row| {
                            // Drop the mutex guard BEFORE invoking application code.
                            let admission = execution.borrow_mut().deliver(1).map_err(authorized);
                            let callback = admission.map(|()| visitor(weight, row));
                            let execution = execution.clone();
                            async move {
                                let outcome = callback?.await;
                                // A callback cannot hide retirement behind its own error.
                                execution.borrow_mut().checkpoint().map_err(authorized)?;
                                outcome
                            }
                        },
                    )
                    .await
                    .map_err(difference_error)?;
                Ok(columns)
            };
            checked_future(&mut guard, || execution.borrow_mut().checkpoint(), future).await
        }))
    }
}

#[cfg(test)]
#[path = "difference/tests.rs"]
mod tests;
