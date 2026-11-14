//! An explicit read owner with one pin and one cumulative signed allowance.
//! The ordinary session deliberately has per-execution allowances; this owner
//! instead moves its permit into the shared native executor for each request.

use super::*;

struct State<'issuer, Resolver, Clock> {
    view: EmbeddedReadView,
    permit: ExecutionPermit<'issuer, ReadAccess>,
    scope: PlannerPredicates,
    branch: String,
    resolver: Resolver,
    policy: GqlQueryPolicy,
    clock: Clock,
}

/// Sequential reads of one authorized immutable snapshot. All input, native
/// execution, result rows and checked output spend the SAME signed allowance.
/// The trusted host fixes its resolver, branch, clock and per-statement native
/// policy at admission. No database or request-context borrow is retained.
///
/// Every query error, output refusal, cancellation or unwind terminally closes
/// the owner. Dropping or rolling it back releases the pin without publishing
/// anything. This is an in-process snapshot owner, not a durable transaction
/// lease or a claim of write-conflict validation.
#[must_use = "dropping an authorized read transaction releases its snapshot pin"]
pub struct AuthorizedReadTransaction<'issuer, Resolver, Clock> {
    state: Option<State<'issuer, Resolver, Clock>>,
    basis: CommitSeq,
    completed_statements: u64,
}

impl<R, C> core::fmt::Debug for AuthorizedReadTransaction<'_, R, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AuthorizedReadTransaction")
            .field("active", &self.is_active())
            .field("basis", &self.basis)
            .field("completed_statements", &self.completed_statements)
            .finish_non_exhaustive()
    }
}

impl<R, C> AuthorizedReadTransaction<'_, R, C> {
    #[must_use]
    pub const fn basis(&self) -> CommitSeq {
        self.basis
    }

    #[must_use]
    pub const fn completed_statements(&self) -> u64 {
        self.completed_statements
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.state.is_some()
    }

    /// Discard the read owner. There are no staged mutations to publish.
    pub fn rollback(mut self) {
        drop(self.state.take());
    }
}

impl<R: GraphSymbolResolver, C: FnMut() -> u64> AuthorizedReadTransaction<'_, R, C> {
    /// Execute through the ordinary native selector, compiler and scoped
    /// source. A bound historical cut must equal this transaction's basis.
    pub fn query(
        &mut self,
        cx: &QueryCx,
        text: &str,
        parameters: &GqlParameters,
    ) -> Result<QueryResult, QueryError> {
        self.query_with_output(cx, text, parameters, |result, _| Ok(result), |error| error)
    }

    /// Admit a host-owned result representation before it leaves the native
    /// owner. `output` is trusted host code: it must charge through `work`
    /// before copying data and must not send or publish the value itself.
    /// The returned value is released only after live authority, cancellation,
    /// deferred work and result-row checks succeed. A swallowed meter failure
    /// still terminally refuses the transaction at that final boundary.
    pub fn query_with_output<T, E>(
        &mut self,
        cx: &QueryCx,
        text: &str,
        parameters: &GqlParameters,
        output: impl FnOnce(QueryResult, &mut dyn FnMut(u64) -> Result<(), QueryError>) -> Result<T, E>,
        map_error: impl Fn(QueryError) -> E,
    ) -> Result<T, E> {
        // Take before all fallible work. An error or panic can never restore
        // an owner whose original allowance or last statement was discarded.
        let State {
            view,
            permit,
            scope,
            branch,
            mut resolver,
            policy,
            mut clock,
        } = self.state.take().ok_or_else(|| map_error(stopped()))?;
        let next = self
            .completed_statements
            .checked_add(1)
            .ok_or_else(|| map_error(QueryError::Authorization(AuthorizationError::TooLarge)))?;
        let mut execution = Execution::new(cx, permit, &mut clock as &mut dyn FnMut() -> u64);
        execution.beacon_indexes = Some(view.beacon_indexes.clone());
        execution.required_cut = Some(self.basis);
        let execution = RefCell::new(execution);
        let value = cx.with_restriction(|| -> Result<T, E> {
            execution.borrow_mut().checkpoint().map_err(&map_error)?;
            let result = text_at(
                &view,
                &branch,
                &scope,
                &mut resolver,
                policy,
                &execution,
                text,
                parameters,
            )
            .map_err(&map_error);
            // Resolver and binder errors cannot hide a concurrent retirement
            // or expiry. This also preserves a latched native meter refusal.
            execution.borrow_mut().checkpoint().map_err(&map_error)?;
            let ((result, _), rows) = result?;
            // Reserve rows before a second representation is allocated. The
            // final zero-row delivery below validates callback work without
            // charging these same result rows twice.
            execution
                .borrow_mut()
                .admit_rows(rows)
                .map_err(&map_error)?;
            let admitted = output(result, &mut |units| {
                execution.borrow_mut().work_units(units)
            });
            execution.borrow_mut().checkpoint().map_err(&map_error)?;
            let admitted = admitted?;
            execution.borrow_mut().deliver(0).map_err(&map_error)?;
            Ok(admitted)
        })?;
        // deliver settled every deferred charge. Move exactly this permit back
        // into idle storage, without retaining the just-finished request Cx.
        let permit = execution.into_inner().permit;
        self.state = Some(State {
            view,
            permit,
            scope,
            branch,
            resolver,
            policy,
            clock,
        });
        self.completed_statements = next;
        Ok(value)
    }

    /// Close successfully at the original snapshot after final live checks.
    /// No commit sequence is allocated and no write outcome is manufactured.
    pub fn close(mut self, cx: &QueryCx) -> Result<CommitSeq, QueryError> {
        let State {
            view,
            permit,
            mut clock,
            ..
        } = self.state.take().ok_or_else(stopped)?;
        cx.with_restriction(|| {
            let mut execution = Execution::new(cx, permit, &mut clock);
            execution.deliver(0)?;
            drop(view);
            Ok(self.basis)
        })
    }
}

fn stopped() -> QueryError {
    QueryError::Authorization(AuthorizationError::ExecutionStopped)
}

impl<V: Vfs + Clone> Database<V> {
    /// Begin a read-only transaction using one pin and one cumulative signed
    /// permit. Signature, namespace and read rights precede pin acquisition.
    /// The issuer stays borrowed so retirement also invalidates idle owners.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_authorized_read<'issuer, R: GraphSymbolResolver, C: FnMut() -> u64>(
        &self,
        cx: &QueryCx,
        authority: &'issuer Authority,
        token: &CapabilityToken,
        branch: &str,
        resolver: R,
        policy: GqlQueryPolicy,
        clock: C,
    ) -> Result<AuthorizedReadTransaction<'issuer, R, C>, QueryError> {
        admit(
            self.keys.namespace,
            cx,
            authority,
            token,
            branch,
            resolver,
            policy,
            clock,
            || self.read_session(),
        )
    }
}

impl EmbeddedReadView {
    /// Narrow this exact generation into a read transaction. The host remains
    /// responsible for its recovery fence, as with an authorized read session.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_authorized_read<'issuer, R: GraphSymbolResolver, C: FnMut() -> u64>(
        &self,
        cx: &QueryCx,
        authority: &'issuer Authority,
        token: &CapabilityToken,
        branch: &str,
        resolver: R,
        policy: GqlQueryPolicy,
        clock: C,
    ) -> Result<AuthorizedReadTransaction<'issuer, R, C>, QueryError> {
        admit(
            self.namespace,
            cx,
            authority,
            token,
            branch,
            resolver,
            policy,
            clock,
            || Ok(self.clone()),
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn admit<'issuer, R: GraphSymbolResolver, C: FnMut() -> u64>(
    namespace: fgdb_types::DatabaseSecurityNamespaceId,
    cx: &QueryCx,
    authority: &'issuer Authority,
    token: &CapabilityToken,
    branch: &str,
    resolver: R,
    policy: GqlQueryPolicy,
    mut clock: C,
    pin: impl FnOnce() -> Result<EmbeddedReadView, crate::ReadError>,
) -> Result<AuthorizedReadTransaction<'issuer, R, C>, QueryError> {
    if authority.namespace() != namespace {
        return Err(QueryError::Authorization(
            AuthorizationError::WrongAuthority,
        ));
    }
    cx.with_restriction(|| {
        let now = clock();
        let verified = authority
            .verify_at(token, branch, now)
            .map_err(QueryError::Authorization)?;
        let scope = verified.predicates().clone();
        let permit = verified
            .into_read_at(branch, now)
            .map_err(QueryError::Authorization)?;
        let (view, permit) = {
            let mut execution = Execution::new(cx, permit, &mut clock);
            execution.checkpoint()?;
            let view = pin().map_err(QueryError::Read)?;
            execution.deliver(0)?;
            (view, execution.permit)
        };
        let basis = view.frontier();
        Ok(AuthorizedReadTransaction {
            state: Some(State {
                view,
                permit,
                scope,
                branch: branch.to_owned(),
                resolver,
                policy,
                clock,
            }),
            basis,
            completed_statements: 0,
        })
    })
}
