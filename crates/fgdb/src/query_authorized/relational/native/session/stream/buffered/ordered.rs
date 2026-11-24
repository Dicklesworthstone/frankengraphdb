//! External relations over the same capability-scoped cold leaf source.
//! Native spool owners provide all relational semantics and final delivery;
//! this module only binds the physical source and its affine return slot.

use super::super::ordered::{
    BoundAggregateOrder, Control, LiveControl, authorized, bind_aggregate_definition,
    checked_future, stage_error, terminal_spool,
};
use super::*;
use crate::query::view::{
    ExternalOrderHost, ExternalOrderLimits, SpoolInput, SpoolRow, execute_relation, ordered_input,
    ordered_stages, spool_terminal_aggregate_with_host,
};
use crate::{NativeResultSpool, NativeSpoolError};
use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite};
use fgdb_delta_types::{ElementId, PropertyKeyId};
use fgdb_gql::scan_stream::{ScanKind, ScanState};
use fgdb_gql::spill_set::{AsyncSpillElementSource, AsyncSpillSetPlan, AsyncSpillSetSourcePlan};
use fgdb_gql::{GlaExecutionStats, GqlExecutionStats};
use fgdb_strata::tiered::edge_scan::BufferedEdgeScanState;
use fgdb_strata::tiered::memory::{SpillError, SpillFile};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalarResolver};

#[path = "ordered/elements.rs"]
mod elements;

type Result<T> = core::result::Result<T, NativeSpoolError>;

fn buffered_error(error: AuthorizedBufferedError) -> NativeSpoolError {
    match error {
        // Keep lifetime failures in the canonical terminal classification.
        AuthorizedBufferedError::Query(error) => authorized(error),
        AuthorizedBufferedError::Read(error) => NativeSpoolError::BufferedRead(Box::new(error)),
        error => NativeSpoolError::BufferedSession(Box::new(error)),
    }
}

impl<V: Vfs + Clone> BufferedReadPublication<V> {
    /// Bind a cold publication and three trusted scratch files to one signed
    /// read session. Authentication and exact-cut catalog admission precede
    /// physical-limit inspection, as for the ordinary buffered constructor.
    /// Files must be private host-owned backends; request code cannot attach
    /// another file or extract their private input/result spools.
    ///
    /// Native vertex and single-edge leaves, including any/finite relation
    /// selectors, compose through the existing external set, row-join,
    /// stage, CALL and grouping owners. Carried identities can read selected
    /// properties through a same-cut scoped point reader. Complete element
    /// metadata, walks, probes, fixed-edge joins and
    /// procedures refuse recursively before candidate or scratch access.
    /// Selected leaf properties, including both endpoints of a single edge,
    /// retain the same staged visibility admission before value decoding.
    /// This is an explicit cold execution capability, not a server default or
    /// a claim that the result value itself can exceed its admitted row bound.
    #[allow(clippy::too_many_arguments)]
    pub async fn into_authorized_ordered_read_session<'a, C, A, B, D>(
        self,
        commit_cx: &CommitCx,
        cx: &QueryCx,
        authority: &'a Authority,
        token: &CapabilityToken,
        branch: &str,
        at: CommitSeq,
        policy: GqlQueryPolicy,
        clock: C,
        scratch: SpillFile<A>,
        destination: SpillFile<B>,
        auxiliary: SpillFile<D>,
        limits: AuthorizedSortLimits,
    ) -> Result<
        AuthorizedOrderedReadSession<
            'a,
            impl GraphSymbolResolver + use<V, C, A, B, D>,
            C,
            A,
            B,
            D,
            AuthorizedBufferedReadSource<V>,
        >,
    >
    where
        C: FnMut() -> u64,
    {
        self.into_authorized_ordered_read_session_with_resolver(
            commit_cx,
            cx,
            authority,
            token,
            branch,
            at,
            policy,
            clock,
            |catalog| Ok(Catalog(catalog)),
            scratch,
            destination,
            auxiliary,
            limits,
        )
        .await
    }

    /// Bind a trusted host resolver and external scratch to one authenticated
    /// cold publication. The callback receives the exact-cut catalog once,
    /// after capability admission and before graph admission. Host aliases
    /// cannot change the selected graph cut or the signed scope.
    ///
    /// The session owns the three files for its entire lifetime. Resolver
    /// refusal and late credential failures precede sort-limit inspection;
    /// every failed constructor releases its publication and scratch owners.
    #[allow(clippy::too_many_arguments)]
    pub async fn into_authorized_ordered_read_session_with_resolver<
        'a,
        R: GraphSymbolResolver,
        C: FnMut() -> u64,
        A,
        B,
        D,
    >(
        self,
        commit_cx: &CommitCx,
        cx: &QueryCx,
        authority: &'a Authority,
        token: &CapabilityToken,
        branch: &str,
        at: CommitSeq,
        policy: GqlQueryPolicy,
        clock: C,
        bind_catalog: impl FnOnce(SymbolCatalog) -> core::result::Result<R, String>,
        scratch: SpillFile<A>,
        destination: SpillFile<B>,
        auxiliary: SpillFile<D>,
        limits: AuthorizedSortLimits,
    ) -> Result<AuthorizedOrderedReadSession<'a, R, C, A, B, D, AuthorizedBufferedReadSource<V>>>
    {
        let session = self
            .into_authorized_read_session_with_resolver(
                commit_cx,
                cx,
                authority,
                token,
                branch,
                at,
                policy,
                clock,
                bind_catalog,
            )
            .await
            .map_err(buffered_error)?;
        limits.validate()?;
        Ok(AuthorizedOrderedReadSession {
            session,
            scratch,
            destination,
            auxiliary,
            limits,
        })
    }

    /// Reattach a trusted host's immutable prepared template to an ordered
    /// cold session without parsing or rebinding its symbol names. The host
    /// must validate the template's database, branch, generation and frozen
    /// catalog; `bind_catalog` verifies the latter against the selected cut.
    ///
    /// Template ownership, map-argument lowering and temporal parameters use
    /// the same canonical admission as prepared buffered source sessions.
    /// The returned handle belongs only to this session and retains no graph
    /// storage independently. Execution rechecks live authority and arguments.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub async fn into_authorized_prepared_ordered_read_session_with_resolver<
        'a,
        R: GraphSymbolResolver,
        C: FnMut() -> u64,
        A,
        B,
        D,
    >(
        self,
        commit_cx: &CommitCx,
        cx: &QueryCx,
        authority: &'a Authority,
        token: &CapabilityToken,
        branch: &str,
        at: CommitSeq,
        policy: GqlQueryPolicy,
        clock: C,
        bind_catalog: impl FnOnce(SymbolCatalog) -> core::result::Result<R, String>,
        selector: Arc<PreparedGraphBranchText>,
        native: Arc<PreparedNativeRead>,
        lowered: Option<Arc<fgdb_gql::GqlMapArgumentText>>,
        scratch: SpillFile<A>,
        destination: SpillFile<B>,
        auxiliary: SpillFile<D>,
        limits: AuthorizedSortLimits,
    ) -> Result<(
        AuthorizedOrderedReadSession<'a, R, C, A, B, D, AuthorizedBufferedReadSource<V>>,
        AuthorizedPreparedRead,
    )> {
        let (session, prepared) = self
            .into_authorized_prepared_read_session_with_resolver(
                commit_cx,
                cx,
                authority,
                token,
                branch,
                at,
                policy,
                clock,
                bind_catalog,
                selector,
                native,
                lowered,
            )
            .await
            .map_err(buffered_error)?;
        limits.validate()?;
        Ok((
            AuthorizedOrderedReadSession {
                session,
                scratch,
                destination,
                auxiliary,
                limits,
            },
            prepared,
        ))
    }
}

fn unsupported(facade: crate::NativeReadClass) -> NativeSpoolError {
    authorized(QueryError::StreamingUnsupported { facade })
}

// Every branch and correlated body is inspected, including one hidden by an
// empty input or LIMIT zero. No output-stage reader can gain unscoped storage
// authority just because a leaf was already visible.
fn admit(plan: &AsyncSpillSetPlan, facade: crate::NativeReadClass) -> Result<()> {
    if plan
        .stages()
        .iter()
        .any(|stage| stage.may_read_element_metadata())
    {
        return Err(unsupported(facade));
    }
    match plan.source() {
        AsyncSpillSetSourcePlan::Values(_) | AsyncSpillSetSourcePlan::Arguments(_) => Ok(()),
        AsyncSpillSetSourcePlan::Vertex(plan) => {
            if matches!(plan.property_demand(), VertexScanPropertyDemand::Complete) {
                Err(unsupported(facade))
            } else {
                Ok(())
            }
        }
        AsyncSpillSetSourcePlan::Edge(plan) => {
            if plan.selected_property_demand().is_none() {
                Err(unsupported(facade))
            } else {
                Ok(())
            }
        }
        AsyncSpillSetSourcePlan::Binary(plan) => {
            admit(plan.left(), facade)?;
            admit(plan.right(), facade)
        }
        AsyncSpillSetSourcePlan::Cross(plan) => {
            admit(plan.left(), facade)?;
            admit(plan.right(), facade)
        }
        AsyncSpillSetSourcePlan::RowJoin(plan) => {
            admit(plan.left(), facade)?;
            admit(plan.right(), facade)
        }
        AsyncSpillSetSourcePlan::Apply(plan) => {
            admit(plan.left(), facade)?;
            admit(plan.right(), facade)
        }
        AsyncSpillSetSourcePlan::Aggregate(plan) => {
            if plan.definition().output_may_read_elements() {
                return Err(unsupported(facade));
            }
            admit(plan.input(), facade)
        }
        AsyncSpillSetSourcePlan::VertexProbe(_)
        | AsyncSpillSetSourcePlan::Join(_)
        | AsyncSpillSetSourcePlan::Walk(_)
        | AsyncSpillSetSourcePlan::Procedure(_) => Err(unsupported(facade)),
    }
}

fn bind_relation(
    prepared: &PreparedNativeRead,
    params: &GqlParameters,
    default: CommitSeq,
) -> Result<(AsyncSpillSetPlan, CommitSeq)> {
    let (plan, at) = match prepared {
        PreparedNativeRead::Pattern(template) => {
            let pattern = template
                .bind_parameters(params)
                .map_err(QueryError::PatternText)
                .map_err(authorized)?;
            (AsyncSpillSetPlan::compile_pattern(pattern), default)
        }
        PreparedNativeRead::TemporalPattern(template) => {
            let bound = template
                .bind_parameters(params)
                .map_err(QueryError::TemporalText)
                .map_err(authorized)?;
            (
                AsyncSpillSetPlan::compile_pattern(bound.pattern().clone()),
                bound.as_of(),
            )
        }
        PreparedNativeRead::Set(template) => {
            let query = template
                .bind_parameters(params)
                .map_err(QueryError::SetText)
                .map_err(authorized)?;
            (AsyncSpillSetPlan::compile(&query), default)
        }
        PreparedNativeRead::TemporalSet(template) => {
            let bound = template
                .bind_parameters(params)
                .map_err(QueryError::TemporalSetText)
                .map_err(authorized)?;
            (AsyncSpillSetPlan::compile(bound.query()), bound.as_of())
        }
        _ => return Err(unsupported(prepared.facade_class())),
    };
    let plan = plan.map_err(|_| unsupported(prepared.facade_class()))?;
    admit(&plan, prepared.facade_class())?;
    Ok((plan, at))
}

fn bind_aggregate(
    prepared: &PreparedNativeRead,
    params: &GqlParameters,
    default: CommitSeq,
) -> Result<(BoundAggregateOrder, CommitSeq)> {
    let bound = bind_aggregate_definition(prepared, params, default).map_err(authorized)?;
    admit(&bound.input, prepared.facade_class())?;
    let at = bound.at;
    Ok((bound, at))
}

struct Opened<'q, V: Vfs, C, T> {
    bound: T,
    returned: Returned<V>,
    at: CommitSeq,
    policy: GqlQueryPolicy,
    execution: Shared<'q, C>,
    guard: ReturnGuard<'q, V>,
}

impl<R: GraphSymbolResolver, C: FnMut() -> u64 + Send, V: Vfs + Clone>
    AuthorizedReadSession<'_, R, C, AuthorizedBufferedReadSource<V>>
{
    fn open_external<'q, T>(
        &'q mut self,
        cx: &'q QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
        limits: AuthorizedSortLimits,
        bind: impl FnOnce(&PreparedNativeRead, &GqlParameters, CommitSeq) -> Result<(T, CommitSeq)>,
    ) -> Result<Opened<'q, V, C, T>> {
        let owner = &self.owner;
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
        let returned = Arc::new(Mutex::new(None));
        let mut guard = ReturnGuard {
            pin: view,
            returned: Some(returned.clone()),
            armed: true,
        };
        let result = (|| {
            let backend = guard.pin.as_ref().ok_or_else(|| {
                authorized(QueryError::Authorization(
                    AuthorizationError::ExecutionStopped,
                ))
            })?;
            let now = clock();
            if now < *last_now_ms {
                return Err(authorized(QueryError::Authorization(
                    AuthorizationError::ClockWentBackwards,
                )));
            }
            *last_now_ms = now;
            let permit = capability
                .begin_read_at(branch, now)
                .map_err(QueryError::Authorization)
                .map_err(authorized)?;
            let execution = Shared(Arc::new(Mutex::new(Some(Execution::tracking(
                cx,
                permit,
                clock,
                last_now_ms,
            )))));
            execution.borrow_mut().poll_fresh().map_err(authorized)?;
            let selected = (|| {
                prepared
                    .admit_input(owner, params, &mut execution.borrow_mut())
                    .map_err(authorized)?;
                let selected = prepared
                    .selector
                    .bind_parameters(params)
                    .map_err(selector_error)
                    .map_err(authorized)?;
                check_branch(&selected, branch).map_err(authorized)?;
                let params = prepared
                    .parameters(selected.parameters())
                    .map_err(authorized)?;
                bind(&prepared.native, &params, backend.at)
            })();
            execution.borrow_mut().poll_fresh().map_err(authorized)?;
            let (bound, at) = selected?;
            if at != backend.at {
                return Err(buffered_error(AuthorizedBufferedError::DifferentCut {
                    requested: at,
                    bound: backend.at,
                }));
            }
            execution
                .borrow_mut()
                .reserve_work(limits.max_sort_work)
                .map_err(authorized)?;
            execution.borrow_mut().poll_fresh().map_err(authorized)?;
            // An unpolled future owns no scan or scratch operation. Its guard
            // simply restores this exact backend from the affine return slot.
            let backend = guard.pin.take().expect("admitted backend is pinned");
            *returned.lock().unwrap_or_else(|error| error.into_inner()) = Some(backend);
            Ok((bound, at, execution))
        })();
        match result {
            Ok((bound, at, execution)) => {
                guard.armed = false;
                Ok(Opened {
                    bound,
                    returned,
                    at,
                    policy: *policy,
                    execution,
                    guard,
                })
            }
            Err(error) => {
                guard.armed = terminal_spool(&error);
                Err(error)
            }
        }
    }
}

fn selected_keys(keys: PropertySelection, scope: &PlannerPredicates) -> PropertySelection {
    let mut visible = PropertySelection::EMPTY;
    for &key in keys.keys() {
        if scope.allows_property(key) {
            visible
                .insert(key)
                .expect("a subset of a bounded selection");
        }
    }
    visible
}

// Keep both native allocation reservations alive through encoding and awaited
// appends in the canonical spool drain. This adapter never delivers a signed
// result row; only the final public cursor spends that allowance.
struct Input<'q, V: Vfs + Clone, C: FnMut() -> u64 + Send> {
    native: Native<'q, V, C>,
    cx: &'q QueryCx,
    _metadata: MemoryCharge,
}

impl<V: Vfs + Clone, C: FnMut() -> u64 + Send> SpoolInput for Input<'_, V, C> {
    fn pull(&mut self) -> impl core::future::Future<Output = Option<Result<SpoolRow>>> + Send {
        let future: crate::SendFuture<'_, _> = Box::pin(async move {
            self.cx
                .with_restriction_async(self.native.next())
                .await
                .map(|result| {
                    result
                        .map(|row| {
                            let (row, guard) = row.into_parts();
                            let (copy, evaluation) = guard.into_parts();
                            SpoolRow::buffered_with_evaluation(row, copy, evaluation)
                        })
                        .map_err(buffered_error)
                })
        });
        future
    }

    fn spool_state(&self) -> ScanState {
        match self.native.state() {
            VertexScanState::Open => ScanState::Open,
            VertexScanState::Exhausted => ScanState::Exhausted,
            VertexScanState::Closed => ScanState::Closed,
            VertexScanState::Failed => ScanState::Failed,
        }
    }

    fn spool_stats(&self) -> (CommitSeq, ScanKind, GqlExecutionStats, GlaExecutionStats) {
        match &self.native {
            Native::Vertex(cursor) => (
                cursor.snapshot_seq(),
                ScanKind::Vertex,
                cursor.row_stats(),
                cursor.evaluator_stats(),
            ),
            Native::Edge(cursor) => (
                cursor.snapshot_seq(),
                ScanKind::Edge,
                cursor.row_stats(),
                cursor.evaluator_stats(),
            ),
        }
    }
}

// Row-only programs retain their reader-free path. The cached effect selects
// the scoped point owner only when a stage can request a selected property.
struct NoElements;
impl AsyncSpillElementSource for NoElements {
    type Error = NativeSpoolError;
    type Property = CanonicalPropertyValue;

    async fn element_property(
        &mut self,
        _element: ElementId,
        _key: PropertyKeyId,
    ) -> Option<core::result::Result<Option<Self::Property>, Self::Error>> {
        None
    }
}

struct Host<'q, V: Vfs, C> {
    // A single point source spans every stage/CALL batch in this execution.
    // Detachment releases the exclusive borrow, never its physical allowance.
    points: Option<BufferedEdgeScanState>,
    returned: Returned<V>,
    cx: &'q QueryCx,
    at: CommitSeq,
    execution: Shared<'q, C>,
}

impl<'q, V: Vfs + Clone, C: FnMut() -> u64 + Send> Host<'q, V, C> {
    fn input(
        &mut self,
        plan: &AsyncSpillSetPlan,
        policy: GqlQueryPolicy,
    ) -> Result<Input<'q, V, C>> {
        let selected = {
            let live = self.execution.borrow();
            let scope = live.permit.predicates();
            match plan.source() {
                AsyncSpillSetSourcePlan::Vertex(plan) => {
                    let keys = match plan.property_demand() {
                        VertexScanPropertyDemand::Topology => PropertySelection::EMPTY,
                        VertexScanPropertyDemand::Properties(keys) => selected_keys(keys, scope),
                        VertexScanPropertyDemand::Complete => {
                            return Err(unsupported(crate::NativeReadClass::Set));
                        }
                    };
                    Plan::vertex(plan.clone(), keys)
                }
                AsyncSpillSetSourcePlan::Edge(plan) => {
                    let mut keys = plan
                        .selected_property_demand()
                        .ok_or_else(|| unsupported(crate::NativeReadClass::Set))?;
                    keys.edge = selected_keys(keys.edge, scope);
                    keys.endpoints = selected_keys(keys.endpoints, scope);
                    Plan::edge(plan.clone(), keys)
                }
                _ => return Err(unsupported(crate::NativeReadClass::Set)),
            }
        };
        let backend = take_returned(&self.returned).ok_or(NativeSpoolError::IncompleteCursor)?;
        let mut slot = Some(backend);
        let opened = selected.open(&mut slot, &self.returned, self.cx, &self.execution, policy);
        if let Some(backend) = slot {
            *self
                .returned
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(backend);
        }
        let (native, metadata) = opened
            .map_err(AuthorizedBufferedError::Read)
            .map_err(buffered_error)?;
        Ok(Input {
            native,
            cx: self.cx,
            _metadata: metadata,
        })
    }
}

impl<V: Vfs + Clone, C: FnMut() -> u64 + Send> ExternalOrderHost for Host<'_, V, C> {
    fn check_snapshot(&mut self, snapshot: CommitSeq) -> Result<()> {
        if snapshot != self.at {
            return Err(buffered_error(AuthorizedBufferedError::DifferentCut {
                requested: snapshot,
                bound: self.at,
            }));
        }
        Ok(())
    }

    fn checkpoint(&mut self) -> Result<()> {
        self.execution.borrow_mut().poll_fresh().map_err(authorized)
    }

    fn native_event(&mut self, _event: GlaExecutionEvent) -> Result<()> {
        self.execution.borrow_mut().checkpoint().map_err(authorized)
    }

    fn leaf<'q, A, B>(
        &'q mut self,
        plan: &'q AsyncSpillSetPlan,
        _arguments: Option<&'q GraphValueRow>,
        snapshot: CommitSeq,
        cx: &'q QueryCx,
        policy: GqlQueryPolicy,
        scratch: &'q mut SpillFile<A>,
        destination: &'q mut SpillFile<B>,
        limits: ExternalOrderLimits,
        _resolver: Option<&'q (dyn CanonicalScalarResolver + Send + Sync)>,
    ) -> crate::SendFuture<'q, Result<(NativeResultSpool, u64)>>
    where
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    {
        Box::pin(async move {
            self.checkpoint()?;
            self.check_snapshot(snapshot)?;
            let input = self.input(plan, policy)?;
            ordered_input(
                cx,
                plan.source_columns().to_vec(),
                input,
                plan.source_tail().ok_or(SpillError::InvalidRun)?.clone(),
                policy,
                scratch,
                destination,
                limits.run_rows,
                limits.max_runs,
                limits.page_bytes,
                limits.max_row_bytes,
                limits.max_sort_work,
            )
            .await
        })
    }

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
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    {
        Box::pin(async move {
            self.check_snapshot(input.snapshot_seq())?;
            let execution = self.execution.clone();
            if !plan.stages().iter().any(|stage| stage.may_read_elements()) {
                return ordered_stages(
                    plan,
                    input,
                    &mut NoElements,
                    &mut || execution.borrow_mut().checkpoint().map_err(authorized),
                    cx,
                    policy,
                    scratch,
                    destination,
                    limits.run_rows,
                    limits.max_runs,
                    limits.page_bytes,
                    limits.max_row_bytes,
                    limits.max_input_rows,
                    limits.max_sort_work,
                    prior_work,
                    resolver,
                )
                .await
                .map_err(stage_error);
            }
            self.checkpoint()?;
            // This lexical checkout outlives the borrowed point reader. On
            // pending-drop, the reader retires before the backend is parked
            // for the outer execution guard to close the affine session.
            let mut checkout = elements::Checkout::new(self.returned.clone())?;
            let mut elements = elements::Elements::new(
                checkout.view(),
                self.points.take(),
                self.cx,
                self.at,
                execution.clone(),
                policy.rows,
                input.row_stats().snapshot_records,
            )?;
            let (spool, work) = ordered_stages(
                plan,
                input,
                &mut elements,
                &mut || execution.borrow_mut().checkpoint().map_err(authorized),
                cx,
                policy,
                scratch,
                destination,
                limits.run_rows,
                limits.max_runs,
                limits.page_bytes,
                limits.max_row_bytes,
                limits.max_input_rows,
                limits.max_sort_work,
                prior_work,
                resolver,
            )
            .await
            .map_err(stage_error)?;
            let (state, records) = elements.into_state();
            self.points = Some(state);
            Ok((spool.with_snapshot_records(records)?, work))
        })
    }
}

impl<'a, R, C, A, B, D, V>
    AuthorizedOrderedReadSession<'a, R, C, A, B, D, AuthorizedBufferedReadSource<V>>
where
    R: GraphSymbolResolver,
    C: FnMut() -> u64 + Send,
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    D: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    V: Vfs + Clone,
{
    /// Complete an admitted cold relation through the canonical external
    /// operators. Binding and recursive source/effect admission are synchronous
    /// and precede candidate/scratch access. Private input uses max_input_rows;
    /// signed result rows are consumed only by the final live-checked cursor.
    /// Dropping a polled pending future closes this affine cold session.
    pub fn execute_ordered<'q>(
        &'q mut self,
        cx: &'q QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
    ) -> Result<AuthorizedOrderExecution<'q, B, AuthorizedBufferedReadSource<V>>> {
        let Self {
            session,
            scratch,
            destination,
            auxiliary,
            limits,
        } = self;
        let limits = *limits;
        let opened = session.open_external(cx, prepared, params, limits, bind_relation)?;
        let columns = opened.bound.columns().to_vec();
        let at = opened.at;
        let future = Box::pin(async move {
            let Opened {
                bound,
                returned,
                at,
                policy,
                execution,
                mut guard,
            } = opened;
            // The inner future owns all active sources and drops before guard.
            // A Source parks its backend before the guard restores/discards it.
            let future = async {
                let mut host = Host {
                    points: None,
                    returned,
                    cx,
                    at,
                    execution: execution.clone(),
                };
                execute_relation(
                    &bound,
                    None,
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
                .map_err(stage_error)
            };
            let (spool, _) =
                checked_future(&mut guard, || execution.borrow_mut().poll_fresh(), future).await?;
            guard.armed = true;
            let admitted = (|| {
                let bytes =
                    u64::try_from(spool.encoded_len()).map_err(|_| SpillError::SizeOverflow)?;
                execution
                    .borrow_mut()
                    .reserve_work(bytes)
                    .map_err(authorized)?;
                execution.borrow_mut().poll_fresh().map_err(authorized)
            })();
            if let Err(error) = admitted {
                guard.armed = terminal_spool(&error);
                return Err(error);
            }
            guard.armed = false;
            let columns = spool.columns().to_vec();
            let reader = spool.reader(destination);
            let control: LiveControl<'q> = Box::new(move |action| {
                let mut execution = execution.borrow_mut();
                match action {
                    Control::Checkpoint => execution.poll_fresh(),
                    Control::Deliver(rows) => execution.deliver(rows),
                }
            });
            Ok(AuthorizedSpoolCursor {
                reader: Some(reader),
                control: Some(control),
                guard: Some(guard),
                cx,
                columns,
                at,
                state: ScanState::Open,
            })
        });
        Ok(AuthorizedOrderExecution {
            future,
            columns,
            at,
        })
    }

    /// Complete exact terminal aggregates over the same scoped cold relation.
    /// Numeric cells and collection occurrence semantics stay in the native
    /// external aggregate owner. Only delivered groups spend signed rows.
    /// Terminal and intermediate output expressions requiring an element
    /// reader refuse during recursive preflight, including under LIMIT zero.
    pub fn execute_ordered_aggregate<'q>(
        &'q mut self,
        cx: &'q QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
    ) -> Result<AuthorizedAggregateOrderExecution<'q, B, AuthorizedBufferedReadSource<V>>> {
        let Self {
            session,
            scratch,
            destination,
            auxiliary,
            limits,
        } = self;
        let limits = *limits;
        let opened = session.open_external(cx, prepared, params, limits, bind_aggregate)?;
        let columns = opened.bound.columns.clone();
        let at = opened.at;
        let future = Box::pin(async move {
            let Opened {
                bound,
                returned,
                at,
                policy,
                execution,
                mut guard,
            } = opened;
            let future = async {
                let mut host = Host {
                    points: None,
                    returned,
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
                checked_future(&mut guard, || execution.borrow_mut().poll_fresh(), future).await?;
            guard.armed = true;
            let admitted = (|| {
                let bytes =
                    u64::try_from(spool.encoded_len()).map_err(|_| SpillError::SizeOverflow)?;
                execution
                    .borrow_mut()
                    .reserve_work(bytes)
                    .map_err(authorized)?;
                execution.borrow_mut().poll_fresh().map_err(authorized)
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
                    Control::Checkpoint => execution.poll_fresh(),
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
