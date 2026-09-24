//! Read-only, generation-pinned authority for an embedded caller.
//!
//! This owns an existing read pin and a verified capability, not a Database or
//! a second snapshot. The host fixes the catalog, branch, clock and native
//! policy once. Request code can supply only query text/arguments and QueryCx.

mod stream;

use super::*;
use crate::EmbeddedReadView;
use fgdb_gql::{GraphSymbol, GraphSymbolKind, ReverseSymbolCatalog};
use fgdb_warden::{Error as AuthorizationError, VerifiedCapability};
use std::sync::Arc;

mod batch;

struct State<'a, Resolver, Clock> {
    view: Option<EmbeddedReadView>,
    capability: VerifiedCapability<'a>,
    branch: String,
    resolver: Resolver,
    policy: GqlQueryPolicy,
    clock: Clock,
    last_now_ms: u64,
}

/// A narrowed, read-only embedded session. Obtain it from
/// `Database::authorized_read_session`; its concrete type may be inferred.
///
/// The original writer can advance or drop without changing this generation.
/// Every statement obtains a fresh PER-EXECUTION permit from the same verified
/// scope; expiry/retirement remain live, including for old historical cuts.
/// The session never lends its privileged view, issuer, resolver or clock and
/// cannot write, refresh its generation, replace its token or widen its policy.
/// Prepared handles are tied to this exact session, not just its namespace.
///
/// Ordinary query/budget failures leave the session usable. Invalid lifetime
/// credentials, backwards time, and unwinding a host callback close it and
/// release its pin. Explicit close/drop never evaluates an unused query.
/// Resident mixed-scope source work remains observable through resource
/// refusal; this is not descriptor/timing noninterference or a durable lease.
pub struct AuthorizedReadSession<'a, Resolver, Clock> {
    state: Option<State<'a, Resolver, Clock>>,
    owner: Arc<()>,
}

/// A template and its original branch selector, privately bound to one session.
/// Neither a raw native plan nor a mutable catalog binding is exported. Reuse
/// binds new argument values and checks current credentials on each execution.
pub struct AuthorizedPreparedRead {
    owner: Arc<()>,
    selector: PreparedGraphBranchText,
    native: PreparedNativeRead,
}

impl core::fmt::Debug for AuthorizedPreparedRead {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AuthorizedPreparedRead([REDACTED])")
    }
}
impl<R, C> core::fmt::Debug for AuthorizedReadSession<'_, R, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AuthorizedReadSession")
            .field("closed", &self.is_closed())
            .field("authority_and_generation", &"[REDACTED]")
            .finish()
    }
}
impl AuthorizedPreparedRead {
    /// Native parameters only; an exclusively branch-routing argument remains
    /// in the retained selector and must still be supplied on every execution.
    pub fn parameter_schema(&self) -> &[fgdb_gql::GqlParameterSpec] {
        self.native.parameter_schema()
    }
    pub fn facade_class(&self) -> crate::NativeReadClass {
        self.native.facade_class()
    }
}

// Passing a closure for resolve_symbol alone would discard reverse catalogs
// and silently break labels()/type(). Forward the complete trusted contract.
struct BorrowedResolver<'a, R>(&'a mut R);
impl<R: GraphSymbolResolver> GraphSymbolResolver for BorrowedResolver<'_, R> {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        self.0.resolve_symbol(kind, name)
    }
    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        self.0.reverse_catalog()
    }
    fn reverse_label(&self, id: fgdb_delta_types::LabelId) -> Option<String> {
        self.0.reverse_label(id)
    }
    fn reverse_relation(&self, id: RelationId) -> Option<String> {
        self.0.reverse_relation(id)
    }
}

fn selector_error(error: fgdb_gql::GraphBranchTextError) -> QueryError {
    QueryError::Unsupported { diagnostics: vec![error.to_string()] }
}
fn check_branch(selected: &fgdb_gql::BoundGraphBranchText<'_>, branch: &str) -> Result<(), QueryError> {
    if selected.branch().is_some_and(|name| name != branch) {
        return Err(QueryError::Authorization(AuthorizationError::ScopeDenied));
    }
    Ok(())
}
fn terminal<T>(result: &Result<T, QueryError>) -> bool {
    matches!(result, Err(QueryError::Authorization(
        AuthorizationError::ExecutionStopped | AuthorizationError::Expired | AuthorizationError::NotYetValid
        | AuthorizationError::AuthorityRetired | AuthorizationError::ClockWentBackwards
    )))
}
fn result_rows(result: QueryResult) -> Result<(QueryResult, usize), QueryError> {
    let count = match &result {
        QueryResult::Rows { rows, .. } => rows.len(),
        QueryResult::Write { .. } => return Err(QueryError::Unsupported {
            diagnostics: vec!["authorized session accepts reads only".to_owned()],
        }),
    };
    Ok((result, count))
}

type Live<'cx, 'permit, 'clock> = RefCell<Execution<'cx, 'permit, &'clock mut dyn FnMut() -> u64>>;

impl<R: GraphSymbolResolver, C: FnMut() -> u64> AuthorizedReadSession<'_, R, C> {
    // Taking the state is the unwind guard: no callback runs while the caller
    // can observe an open session without a completed return path. On ordinary
    // errors we restore it, retaining the maximum clock sampled by this call.
    fn run<T>(
        &mut self,
        cx: &QueryCx,
        action: impl FnOnce(
            &EmbeddedReadView, &str, &PlannerPredicates, &mut R,
            GqlQueryPolicy, &Live<'_, '_, '_>,
        ) -> Result<(T, usize), QueryError>,
    ) -> Result<T, QueryError> {
        let mut state = self.state.take().ok_or(QueryError::Authorization(
            AuthorizationError::ExecutionStopped,
        ))?;
        if state.view.is_none() {
            return Err(QueryError::Authorization(AuthorizationError::ExecutionStopped));
        }
        let result = (|| {
            let State { view, capability, branch, resolver, policy, clock, last_now_ms } = &mut state;
            let now = clock();
            if now < *last_now_ms {
                return Err(QueryError::Authorization(AuthorizationError::ClockWentBackwards));
            }
            *last_now_ms = now;
            let permit = capability.begin_read_at(branch, now).map_err(QueryError::Authorization)?;
            let mut tracked_clock = || {
                let now = clock();
                *last_now_ms = (*last_now_ms).max(now);
                now // The existing live permit refuses a backwards sample.
            };
            let execution = RefCell::new(Execution::new(
                cx, permit, &mut tracked_clock as &mut dyn FnMut() -> u64,
            ));
            execution.borrow_mut().checkpoint()?;
            let view = view.as_ref().ok_or(QueryError::Authorization(
                AuthorizationError::ExecutionStopped,
            ))?;
            let result = cx.with_restriction(|| action(
                view, branch, capability.predicates(), resolver, *policy, &execution,
            ));
            // Also check after a resolver/parser/binder failed. Such a callback
            // cannot hide expiry/retirement by returning its own error first.
            execution.borrow_mut().checkpoint()?;
            // The action reports final rows not yet reserved. A read batch
            // reserves each completed result on this SAME permit before
            // retaining it, then reports zero here; no result is double charged.
            let (value, unreserved_rows) = result?;
            execution.borrow_mut().deliver(unreserved_rows)?;
            Ok(value)
        })();
        if !terminal(&result) {
            self.state = Some(state);
        }
        result
    }

    /// Execute all seven existing native read classes against this one pin.
    /// Preparation and evaluation share a permit. Request arguments cannot
    /// choose another issuer, policy, catalog, branch mapping or clock.
    pub fn query(&mut self, cx: &QueryCx, text: &str, params: &GqlParameters) -> Result<QueryResult, QueryError> {
        self.run(cx, |view, branch, scope, resolver, policy, execution| {
            text_at(view, branch, scope, resolver, policy, execution, text, params)
        })
    }

    /// Prepare once using only this session's fixed trusted catalog. Parameters
    /// supply declared types; payloads are rebound on execute. The original
    /// branch selector is retained, including shared/exclusive parameter rules.
    pub fn prepare(&mut self, cx: &QueryCx, text: &str, params: &GqlParameters) -> Result<AuthorizedPreparedRead, QueryError> {
        let owner = Arc::clone(&self.owner);
        self.run(cx, |_, branch, _, resolver, _, execution| {
            let selector = PreparedGraphBranchText::prepare(text).map_err(selector_error)?;
            let selected = selector.bind_parameters(params).map_err(selector_error)?;
            check_branch(&selected, branch)?;
            execution.borrow_mut().checkpoint()?;
            let native = PreparedNativeRead::prepare(
                selected.statement(), selected.parameters(), BorrowedResolver(resolver),
            );
            execution.borrow_mut().checkpoint()?;
            Ok((AuthorizedPreparedRead { owner, selector, native: native? }, 0))
        })
    }

    /// Rebind an exact-session template without re-resolving its symbols.
    /// A template from another session refuses even for the same database and
    /// issuer; no cached plan may silently cross catalog or generation owners.
    pub fn execute(&mut self, cx: &QueryCx, prepared: &AuthorizedPreparedRead, params: &GqlParameters) -> Result<QueryResult, QueryError> {
        let owner = Arc::clone(&self.owner);
        self.run(cx, |view, branch, scope, _, policy, execution| {
            prepared_at(view, branch, scope, policy, execution, &owner, prepared, params)
        })
    }
}

// Single statements and batches use exactly the same selector, classification,
// parameter binding and scoped execution path, not a second dispatcher.
#[allow(clippy::too_many_arguments)]
fn text_at<R: GraphSymbolResolver>(
    view: &EmbeddedReadView,
    branch: &str,
    scope: &PlannerPredicates,
    resolver: &mut R,
    policy: GqlQueryPolicy,
    execution: &Live<'_, '_, '_>,
    text: &str,
    params: &GqlParameters,
) -> Result<(QueryResult, usize), QueryError> {
    let selector = PreparedGraphBranchText::prepare(text).map_err(selector_error)?;
    let selected = selector.bind_parameters(params).map_err(selector_error)?;
    check_branch(&selected, branch)?;
    execution.borrow_mut().checkpoint()?;
    let prepared = PreparedNativeRead::prepare(
        selected.statement(), selected.parameters(), BorrowedResolver(resolver),
    );
    execution.borrow_mut().checkpoint()?;
    result_rows(native_at(
        &prepared?, selected.parameters(), &view.snapshot, view.frontier(),
        scope, execution, policy,
    )?)
}

#[allow(clippy::too_many_arguments)]
fn prepared_at(
    view: &EmbeddedReadView,
    branch: &str,
    scope: &PlannerPredicates,
    policy: GqlQueryPolicy,
    execution: &Live<'_, '_, '_>,
    owner: &Arc<()>,
    prepared: &AuthorizedPreparedRead,
    params: &GqlParameters,
) -> Result<(QueryResult, usize), QueryError> {
    if !Arc::ptr_eq(owner, &prepared.owner) {
        return Err(QueryError::Authorization(AuthorizationError::WrongAuthority));
    }
    let selected = prepared.selector.bind_parameters(params).map_err(selector_error)?;
    check_branch(&selected, branch)?;
    result_rows(native_at(
        &prepared.native, selected.parameters(), &view.snapshot, view.frontier(),
        scope, execution, policy,
    )?)
}
impl<R, C> AuthorizedReadSession<'_, R, C> {
    /// Release the generation and trusted state, with no source reads or clock
    /// callbacks. Closing twice is harmless; subsequent operations refuse.
    pub fn close(&mut self) {
        self.state = None;
    }
    pub fn is_closed(&self) -> bool {
        self.state.as_ref().is_none_or(|state| state.view.is_none())
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Narrow a newly admitted immutable generation into an owned read-only
    /// session. The host, not a token holder, chooses all authority inputs.
    /// Signature/namespace/rights checks precede the read pin and any catalog
    /// callback. Opening scans no graph rows. Neither database nor token is
    /// borrowed afterward; the issuer remains borrowed for live retirement.
    ///
    /// Per-statement native and signed limits are fixed at construction, not
    /// caller-selected on query. Close/drop releases this pin; prepared handles
    /// alone retain neither the graph nor capability. To adopt newer data or a
    /// different scope/catalog, the host must create another session.
    /// This does not authorize the original Database or create a server lease.
    #[allow(clippy::too_many_arguments)]
    pub fn authorized_read_session<'a, R: GraphSymbolResolver, C: FnMut() -> u64>(
        &self, cx: &QueryCx, authority: &'a Authority, token: &CapabilityToken,
        branch: &str, resolver: R, policy: GqlQueryPolicy, mut clock: C,
    ) -> Result<AuthorizedReadSession<'a, R, C>, QueryError> {
        if authority.namespace() != self.keys.namespace {
            return Err(QueryError::Authorization(AuthorizationError::WrongAuthority));
        }
        let now = clock();
        let capability = authority.verify_at(token, branch, now).map_err(QueryError::Authorization)?;
        let mut last_now_ms = now;
        let view = {
            let permit = capability.begin_read_at(branch, now).map_err(QueryError::Authorization)?;
            let mut tracked_clock = || {
                let now = clock();
                last_now_ms = last_now_ms.max(now);
                now
            };
            let mut execution = Execution::new(cx, permit, &mut tracked_clock);
            execution.checkpoint()?;
            let view = self.read_session().map_err(QueryError::Read)?;
            execution.deliver(0)?;
            view
        };
        Ok(AuthorizedReadSession {
            state: Some(State {
                view: Some(view), capability, branch: branch.to_owned(), resolver, policy, clock, last_now_ms,
            }),
            owner: Arc::new(()),
        })
    }
}

#[cfg(test)]
mod failure_tests;
