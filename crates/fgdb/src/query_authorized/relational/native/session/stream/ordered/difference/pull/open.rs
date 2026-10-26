//! Bind two admitted endpoints and open the shared external merge.
use super::*;

impl<R, C, A, B> AuthorizedOrderedReadSession<'_, R, C, A, B>
where
    R: GraphSymbolResolver,
    C: FnMut() -> u64 + Send,
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    /// Open resumable `result(after) - result(before)` delivery under ONE live
    /// capability permit. Both cuts belong to this session's immutable
    /// generation. Scope masking precedes predicates, projection, ordering and
    /// subtraction at each cut; no privileged query_diff or final-row security
    /// filter participates.
    ///
    /// Rebind the exact-session template once and admit both cuts/sources before
    /// returning. The existing ordered Pattern, one-source projected Set pull
    /// and unary relation-stage profiles are supported. Every source and stage
    /// DISTINCT/order/window completes independently at each cut before
    /// subtraction. Carried element properties resolve under the same scope at
    /// that endpoint's cut. Explicit temporal selectors and other profiles
    /// refuse without scratch I/O or eager fallback. The final schema and
    /// endpoints are available on the returned execution before its first poll.
    ///
    /// One signed permit spans endpoint evaluation and every later pull. Native
    /// source-record/work/scratch limits cover both endpoints cumulatively. The
    /// trusted max_sort_work reservation covers both endpoint spill pipelines,
    /// both difference sorts and the eventual merge pulls. Native and signed
    /// row limits charge only final nonzero changed tuples, not endpoint rows or
    /// absolute multiplicities. Source/work/scratch counters remain private.
    ///
    /// Polling the execution completes both endpoints and sorts, then returns a
    /// cursor positioned before its first change. The cursor borrows this session
    /// and its final scratch owner; caller code cannot replace either. Stage all
    /// changes until clean exhaustion. This is ephemeral same-history result
    /// comparison, not cold-source authorization, graph DIFF syntax, durable
    /// resume, a retention lease, cross-branch coverage, or timing isolation.
    #[allow(clippy::too_many_arguments, private_interfaces, unnameable_types)]
    pub fn difference_stream<'q>(
        &'q mut self,
        cx: &'q QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
        before: CommitSeq,
        after: CommitSeq,
    ) -> Result<AuthorizedDifferenceExecution<'q, A>> {
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
            |(plan, tail, _, source_columns, pull, stages), view, cx, execution, policy| {
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
                let columns = stages.as_ref().map_or_else(
                    || source_columns.clone(),
                    |stages| stages.columns().to_vec(),
                );
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
                    columns: source_columns.clone(),
                    stages: stages.as_ref().map(|stages| (stages.clone(), view.clone())),
                    at: before,
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
                    columns: source_columns,
                    stages: stages.map(|stages| (stages, view.clone())),
                    at: after,
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
        let public_columns = columns.clone();
        let future = Box::pin(async move {
            let future = async {
                let input_policy = private_policy(policy);
                let first = complete_endpoint(
                    first,
                    cx,
                    execution.clone(),
                    input_policy,
                    &mut *scratch,
                    &mut *destination,
                    limits,
                    limits.max_sort_work,
                )
                .await?;
                let input_policy = after_policy(input_policy, &first)?;
                let allowance = remainder(limits.max_sort_work, first.work)?;
                let second = complete_endpoint(
                    second,
                    cx,
                    execution.clone(),
                    input_policy,
                    &mut *scratch,
                    &mut *destination,
                    limits,
                    allowance,
                )
                .await?;
                if first.spool.snapshot_seq() != before
                    || second.spool.snapshot_seq() != after
                    || first.spool.columns() != columns
                    || second.spool.columns() != columns
                {
                    return Err(NativeSpoolError::IncompleteCursor);
                }
                let allowance = remainder(allowance, second.work)?;
                first
                    .spool
                    .difference_stream(
                        &second.spool,
                        cx,
                        destination,
                        scratch,
                        limits.run_rows,
                        limits.max_runs,
                        limits.page_bytes,
                        policy.rows.max_result_rows().unwrap_or(u64::MAX),
                        allowance,
                    )
                    .await
                    .map_err(difference_error)
            };
            let inner =
                checked_future(&mut guard, || execution.borrow_mut().checkpoint(), future).await?;
            let control: LiveControl<'q> = Box::new(move |action| {
                let mut execution = execution.borrow_mut();
                match action {
                    Control::Checkpoint => execution.checkpoint(),
                    Control::Deliver(rows) => execution.deliver(rows),
                }
            });
            Ok(AuthorizedDifferenceCursor {
                inner: Some(inner),
                control: Some(control),
                guard: Some(guard),
                columns,
                before,
                after,
                state: ScanState::Open,
            })
        });
        Ok(AuthorizedDifferenceExecution {
            future,
            columns: public_columns,
            before,
            after,
        })
    }
}
