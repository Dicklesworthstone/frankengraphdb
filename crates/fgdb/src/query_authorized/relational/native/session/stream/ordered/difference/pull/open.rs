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
    /// returning. The existing ordered Pattern and one-source projected Set
    /// profiles are supported. Explicit temporal selectors and unsupported
    /// profiles refuse without scratch I/O or eager fallback. The schema and
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
            |(plan, tail, _, columns, phase), view, cx, execution, policy| {
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
                let (second_pull, second_order) =
                    phase.clone().into_source(columns.clone(), tail.clone());
                let (pull, first_order) = phase.into_source(columns.clone(), tail);
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
                    order: first_order,
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
                    order: second_order,
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
                let control = execution.clone();
                let (first, used) = ordered_program(
                    cx,
                    first.order,
                    (first.input)(input_policy),
                    input_policy,
                    (&mut *scratch, &mut *destination),
                    limits.into(),
                    move |_| control.borrow_mut().checkpoint().map_err(authorized),
                )
                .await
                .map_err(projection_error)?;
                let input_policy = after_policy(input_policy, &first)?;
                let allowance = remainder(limits.max_sort_work, used)?;
                let control = execution.clone();
                let (second, spent) = ordered_program(
                    cx,
                    second.order,
                    (second.input)(input_policy),
                    input_policy,
                    (&mut *scratch, &mut *destination),
                    SpoolOrderLimits {
                        max_sort_work: allowance,
                        ..limits.into()
                    },
                    move |_| control.borrow_mut().checkpoint().map_err(authorized),
                )
                .await
                .map_err(projection_error)?;
                if first.snapshot_seq() != before || second.snapshot_seq() != after {
                    return Err(NativeSpoolError::IncompleteCursor);
                }
                let allowance = remainder(allowance, spent)?;
                first
                    .difference_stream(
                        &second,
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
