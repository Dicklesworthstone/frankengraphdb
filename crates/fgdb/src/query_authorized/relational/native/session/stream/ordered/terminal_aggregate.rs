//! Exact terminal aggregates over the same scoped external relation owner.
//! Complete groups retain their wide numeric cells and collection payloads;
//! only final delivered groups consume the signed result-row allowance.

use super::*;
use crate::query::view::spool_terminal_aggregate_with_host;
use crate::{NativeAggregateSpoolCursor, NativeAggregateSpoolRow};
use fgdb_gql::GraphAggregateTextSlot;
use fgdb_gql::spill_aggregate::{AsyncSpillAggregatePlan, SpillAggregateDefinition};

struct BoundAggregateOrder {
    input: AsyncSpillSetPlan,
    definition: SpillAggregateDefinition,
    columns: Vec<String>,
    slots: Vec<GraphAggregateTextSlot>,
    at: CommitSeq,
}

fn bind_aggregate_order(
    prepared: &PreparedNativeRead,
    params: &GqlParameters,
    default: CommitSeq,
) -> core::result::Result<BoundAggregateOrder, QueryError> {
    let unsupported = || QueryError::StreamingUnsupported {
        facade: prepared.facade_class(),
    };
    let (plan, columns, slots, at) = match prepared {
        PreparedNativeRead::Aggregate(template) => {
            let definition = template
                .bind_parameters(params)
                .map_err(QueryError::PatternText)?;
            (
                AsyncSpillAggregatePlan::compile_complete_relation(&definition),
                template.columns(),
                template.output_slots(),
                default,
            )
        }
        PreparedNativeRead::PipelineAggregate(template) => {
            let definition = template
                .bind_relation_parameters(params)
                .map_err(QueryError::PipelineText)?;
            (
                AsyncSpillAggregatePlan::compile_relation(&definition),
                template.columns(),
                template.output_slots(),
                default,
            )
        }
        PreparedNativeRead::TemporalAggregate(template) => {
            let selected = template
                .bind_source_parameters(params)
                .map_err(QueryError::TemporalText)?;
            let plan = match selected.aggregate() {
                fgdb_gql::TemporalAggregatePlan::Graph(definition) => {
                    AsyncSpillAggregatePlan::compile_complete_relation(definition)
                }
                fgdb_gql::TemporalAggregatePlan::Relation(definition) => {
                    AsyncSpillAggregatePlan::compile_relation(definition)
                }
            };
            (
                plan,
                template.columns(),
                template.output_slots(),
                selected.as_of(),
            )
        }
        _ => return Err(unsupported()),
    };
    let AsyncSpillAggregatePlan::Relation { input, definition } =
        plan.map_err(|_| unsupported())?
    else {
        return Err(unsupported());
    };
    // Completed aggregate expressions have no graph-reader capability. Input
    // stages already use ScopedElements, but a late property/metadata lookup
    // must not receive the privileged resident output reader by accident.
    if definition.output_may_read_elements()
        || !crate::query::aggregate_stream::valid_layout(
            columns,
            slots,
            definition.key_columns(),
            definition.aggregate_columns(),
        )
    {
        return Err(unsupported());
    }
    admit_external_sources(&input)?;
    Ok(BoundAggregateOrder {
        input: *input,
        definition,
        columns: columns.to_vec(),
        slots: slots.to_vec(),
        at,
    })
}

struct OpenedAggregateOrder<'q, C> {
    bound: BoundAggregateOrder,
    view: EmbeddedReadView,
    policy: GqlQueryPolicy,
    execution: Shared<'q, C>,
    guard: PinGuard<'q>,
}

impl<R: GraphSymbolResolver, C: FnMut() -> u64 + Send> AuthorizedReadSession<'_, R, C> {
    fn open_aggregate_ordered<'q>(
        &'q mut self,
        cx: &'q QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
        limits: AuthorizedSortLimits,
    ) -> Result<OpenedAggregateOrder<'q, C>> {
        let state = self.state.as_mut().ok_or_else(|| {
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
        let result = open(
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
            &self.owner,
            prepared,
            params,
            *policy,
            bind_aggregate_order,
            |bound, view, _, execution, policy| {
                execution.borrow_mut().reserve_work(limits.max_sort_work)?;
                view.snapshot
                    .check_frontier(bound.at)
                    .map_err(QueryError::Read)?;
                execution.borrow_mut().checkpoint()?;
                Ok((bound, view.clone(), policy, execution))
            },
        );
        match result {
            Ok((bound, view, policy, execution)) => {
                guard.armed = false;
                Ok(OpenedAggregateOrder {
                    bound,
                    view,
                    policy,
                    execution,
                    guard,
                })
            }
            Err(error) => {
                guard.armed = terminal_error(&error);
                Err(authorized(error))
            }
        }
    }
}

impl<'a, R, C, A, B, D> AuthorizedOrderedReadSession<'a, R, C, A, B, D>
where
    R: GraphSymbolResolver,
    C: FnMut() -> u64 + Send,
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    D: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    /// Execute a terminal COUNT/SUM/AVG/MIN/MAX/COLLECT through bounded
    /// external grouping and the query's own HAVING, DISTINCT, order and page.
    /// Native graph and complete relational inputs use this session's masked
    /// sources, exact historical cut and one cumulative execution permit.
    /// Input occurrences use the host's private max_input_rows allowance;
    /// signed rows are charged only when a final group is delivered.
    ///
    /// Counts remain u64, integer sums i128 and averages exact rationals.
    /// Collections keep native occurrence order and duplicates, with separate
    /// per-value memory/row admission. Every reached input and completed-group
    /// expression finishes before delivery, including under final LIMIT zero.
    /// No ordinary-row conversion narrows a wide sum or rounded average.
    ///
    /// Binding, ownership, source-profile and historical admission precede any
    /// scratch access. The unpolled future reads no graph candidates or files.
    /// Every poll and final delivery retains the ordinary live authority and
    /// cancellation checks. A refused selected path never falls back to an
    /// eager query. Terminal output expressions requiring element-property or
    /// metadata lookups refuse; supported input-stage lookups remain scoped.
    /// The underlying immutable graph remains resident, as for ordered rows.
    pub fn execute_ordered_aggregate<'q>(
        &'q mut self,
        cx: &'q QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
    ) -> Result<AuthorizedAggregateOrderExecution<'q, B>> {
        let Self {
            session,
            scratch,
            destination,
            auxiliary,
            limits,
        } = self;
        let limits = *limits;
        let opened = session.open_aggregate_ordered(cx, prepared, params, limits)?;
        let columns = opened.bound.columns.clone();
        let at = opened.bound.at;
        let future = Box::pin(async move {
            let OpenedAggregateOrder {
                bound,
                view,
                policy,
                execution,
                mut guard,
            } = opened;
            let future = async {
                let mut host = ScopedOrderHost {
                    view,
                    cx,
                    at,
                    execution: execution.clone(),
                };
                spool_terminal_aggregate_with_host(
                    &bound.input,
                    &bound.definition,
                    &bound.columns,
                    &bound.slots,
                    at,
                    &mut host,
                    cx,
                    policy,
                    scratch,
                    destination,
                    auxiliary,
                    limits.external(),
                    None,
                )
                .await
                .map_err(NativeSpoolError::from)
                .map_err(stage_error)
            };
            let (spool, _) =
                checked_future(&mut guard, || execution.borrow_mut().checkpoint(), future).await?;
            guard.armed = true;
            let admitted = (|| {
                let bytes =
                    u64::try_from(spool.encoded_len()).map_err(|_| SpillError::SizeOverflow)?;
                execution
                    .borrow_mut()
                    .reserve_work(bytes)
                    .map_err(authorized)?;
                execution.borrow_mut().checkpoint().map_err(authorized)
            })();
            if let Err(error) = admitted {
                guard.armed = terminal_spool(&error);
                return Err(error);
            }
            guard.armed = false;
            let columns = spool.columns().to_vec();
            let slots = spool.output_slots().to_vec();
            let keys = spool.key_columns().to_vec();
            let aggregates = spool.aggregate_columns().to_vec();
            let reader = spool.reader(destination, None);
            let control: LiveControl<'q> = Box::new(move |action| {
                let mut execution = execution.borrow_mut();
                match action {
                    Control::Checkpoint => execution.checkpoint(),
                    Control::Deliver(rows) => execution.deliver(rows),
                }
            });
            Ok(AuthorizedAggregateSpoolCursor {
                reader: Some(reader),
                control: Some(control),
                guard: Some(guard),
                cx,
                columns,
                slots,
                keys,
                aggregates,
                at,
                state: ScanState::Open,
            })
        });
        Ok(AuthorizedAggregateOrderExecution {
            future,
            columns,
            at,
        })
    }
}

/// One admitted external aggregate, with public schema before first demand.
/// Keep this exact future while a host services cancellation or flow control.
/// Dropping it never completes pending work or exposes a private scratch run.
pub struct AuthorizedAggregateOrderExecution<'q, F> {
    future:
        Pin<Box<dyn Future<Output = Result<AuthorizedAggregateSpoolCursor<'q, F>>> + Send + 'q>>,
    columns: Vec<String>,
    at: CommitSeq,
}

impl<F> core::fmt::Debug for AuthorizedAggregateOrderExecution<'_, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AuthorizedAggregateOrderExecution([REDACTED])")
    }
}

impl<F> AuthorizedAggregateOrderExecution<'_, F> {
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    pub fn snapshot_seq(&self) -> CommitSeq {
        self.at
    }
}

impl<'q, F> Future for AuthorizedAggregateOrderExecution<'q, F> {
    type Output = Result<AuthorizedAggregateSpoolCursor<'q, F>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().future.as_mut().poll(cx)
    }
}

/// Capability-checked exact aggregate rows from a completed private spool.
/// RETURN slots refer to keys()/values() on each row without cloning repeated
/// output values. Each returned row owns its decoded memory reservation.
/// Scratch handles, source counters and a reusable permit never escape.
pub struct AuthorizedAggregateSpoolCursor<'q, F> {
    reader: Option<NativeAggregateSpoolCursor<'q, F>>,
    control: Option<LiveControl<'q>>,
    guard: Option<PinGuard<'q>>,
    cx: &'q QueryCx,
    columns: Vec<String>,
    slots: Vec<GraphAggregateTextSlot>,
    keys: Vec<String>,
    aggregates: Vec<String>,
    at: CommitSeq,
    state: ScanState,
}

impl<F> core::fmt::Debug for AuthorizedAggregateSpoolCursor<'_, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AuthorizedAggregateSpoolCursor")
            .field("state", &self.state)
            .field("authority_rows_and_scratch", &"[REDACTED]")
            .finish()
    }
}

impl<F> AuthorizedAggregateSpoolCursor<'_, F> {
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    pub fn output_slots(&self) -> &[GraphAggregateTextSlot] {
        &self.slots
    }

    pub fn key_columns(&self) -> &[String] {
        &self.keys
    }

    pub fn aggregate_columns(&self) -> &[String] {
        &self.aggregates
    }

    pub fn snapshot_seq(&self) -> CommitSeq {
        self.at
    }

    pub fn state(&self) -> ScanState {
        self.state
    }

    pub fn close(&mut self) {
        if self.state == ScanState::Open {
            self.state = ScanState::Closed;
            if let Some(guard) = &mut self.guard {
                guard.armed = false;
            }
        }
        self.reader.take();
        self.control.take();
        self.guard.take();
    }
}

impl<F: AsyncRead + AsyncWrite + AsyncSeek + Unpin> AuthorizedAggregateSpoolCursor<'_, F> {
    /// Decode one exact aggregate row under the retained permit. Pending drop,
    /// source/decode failure and delivery refusal permanently fuse this cursor.
    /// A final authority check precedes exposing the independently charged row.
    pub async fn next_row(&mut self) -> Result<Option<NativeAggregateSpoolRow>> {
        if self.state != ScanState::Open {
            return Ok(None);
        }
        self.state = ScanState::Failed;
        let mut reader = self
            .reader
            .take()
            .expect("open aggregate cursor owns its reader");
        let mut control = self
            .control
            .take()
            .expect("open aggregate cursor owns its permit");
        let guard = self
            .guard
            .as_mut()
            .expect("open aggregate cursor guards its pin");
        let outcome = checked_future(guard, || control(Control::Checkpoint), async {
            reader
                .next_row(self.cx)
                .await
                .map_err(NativeSpoolError::from)
        })
        .await;
        let outcome = match outcome {
            Ok(row) => {
                guard.armed = true;
                control(Control::Deliver(usize::from(row.is_some())))
                    .map(|()| row)
                    .map_err(authorized)
            }
            Err(error) => Err(error),
        };
        match outcome {
            Ok(row) => {
                guard.armed = false;
                if row.is_none() || reader.state() == ScanState::Exhausted {
                    self.state = ScanState::Exhausted;
                    self.guard.take();
                } else {
                    self.state = ScanState::Open;
                    self.reader = Some(reader);
                    self.control = Some(control);
                }
                Ok(row)
            }
            Err(error) => {
                guard.armed = terminal_spool(&error);
                self.guard.take();
                Err(error)
            }
        }
    }
}
