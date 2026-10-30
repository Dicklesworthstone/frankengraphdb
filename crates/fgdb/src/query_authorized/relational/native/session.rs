//! Read-only, generation-pinned authority for an embedded caller.
//!
//! This owns an existing read pin and a verified capability, not a Database or
//! a second snapshot. The host fixes the catalog, branch, clock and native
//! policy once. Request code can supply only query text/arguments and QueryCx.

#[path = "session/stream.rs"]
mod stream;
pub use stream::{
    AuthorizedAggregateCursor, AuthorizedAsyncAggregateCursor, AuthorizedDifferenceCursor,
    AuthorizedDifferenceExecution, AuthorizedOrderExecution, AuthorizedOrderedReadSession,
    AuthorizedRowCursor, AuthorizedSendAggregateCursor, AuthorizedSendRowCursor,
    AuthorizedSortLimits, AuthorizedSpoolCursor,
};

use super::*;
use crate::EmbeddedReadView;
use fgdb_gql::{GraphSymbol, GraphSymbolKind, ReverseSymbolCatalog};
use fgdb_warden::{Error as AuthorizationError, VerifiedCapability};
use std::sync::Arc;

#[path = "session/analytics.rs"]
mod analytics;
pub use analytics::AuthorizedPreparedFnxCall;
#[path = "session/batch.rs"]
mod batch;
#[path = "session/beacon.rs"]
mod beacon;
pub use beacon::AuthorizedBeaconIndex;
#[path = "session/diff.rs"]
mod diff;
#[path = "session/sealed.rs"]
mod sealed;
pub use sealed::AuthorizedSealedReadSession;
use sealed::{check_selected_cut, selected_cut};

struct State<'a, Resolver, Clock> {
    view: Option<EmbeddedReadView>,
    capability: VerifiedCapability<'a>,
    branch: String,
    resolver: Resolver,
    policy: GqlQueryPolicy,
    clock: Clock,
    last_now_ms: u64,
    sealed: Option<Arc<crate::query::authorized::analytics::sealed::Source>>,
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
    selector: Arc<PreparedGraphBranchText>,
    native: Arc<PreparedNativeRead>,
    lowered: Option<fgdb_gql::GqlMapArgumentText>,
    parameter_schema: Option<Box<[fgdb_gql::GqlParameterSpec]>>,
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
        self.parameter_schema
            .as_deref()
            .unwrap_or_else(|| self.native.parameter_schema())
    }
    pub fn facade_class(&self) -> crate::NativeReadClass {
        self.native.facade_class()
    }

    // Bind only after the caller has admitted this exact owner and live permit.
    // The splice holds names and field paths, never preparation-time values.
    fn parameters<'p>(
        &self,
        params: &'p GqlParameters,
    ) -> Result<std::borrow::Cow<'p, GqlParameters>, QueryError> {
        match &self.lowered {
            Some(lowered) => lowered
                .arguments(params)
                .map(std::borrow::Cow::Owned)
                .map_err(|error| QueryError::Unsupported {
                    diagnostics: vec![error.to_string()],
                }),
            None => Ok(std::borrow::Cow::Borrowed(params)),
        }
    }
}

// Passing a closure for resolve_symbol alone would discard reverse catalogs
// and silently break labels()/type(). Forward the complete trusted contract.
struct BorrowedResolver<'a, R>(&'a mut R);
impl<R: GraphSymbolResolver> GraphSymbolResolver for BorrowedResolver<'_, R> {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        self.0.resolve_symbol(kind, name)
    }
    fn time_zones(&self) -> Option<fgdb_gql::GqlTimeZones> {
        self.0.time_zones()
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
    QueryError::Unsupported {
        diagnostics: vec![error.to_string()],
    }
}
fn check_branch(
    selected: &fgdb_gql::BoundGraphBranchText<'_>,
    branch: &str,
) -> Result<(), QueryError> {
    if selected.branch().is_some_and(|name| name != branch) {
        return Err(QueryError::Authorization(AuthorizationError::ScopeDenied));
    }
    Ok(())
}
fn terminal<T>(result: &Result<T, QueryError>) -> bool {
    result.as_ref().err().is_some_and(terminal_error)
}
fn terminal_error(error: &QueryError) -> bool {
    matches!(
        error,
        QueryError::Authorization(
            AuthorizationError::ExecutionStopped
                | AuthorizationError::Expired
                | AuthorizationError::NotYetValid
                | AuthorizationError::AuthorityRetired
                | AuthorizationError::ClockWentBackwards
        )
    )
}
fn result_rows(
    result: QueryResult,
    snapshot_seq: CommitSeq,
) -> Result<((QueryResult, CommitSeq), usize), QueryError> {
    let count = match &result {
        QueryResult::Rows { rows, .. } => rows.len(),
        QueryResult::Write { .. } => {
            return Err(QueryError::Unsupported {
                diagnostics: vec!["authorized session accepts reads only".to_owned()],
            });
        }
    };
    Ok(((result, snapshot_seq), count))
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
            &EmbeddedReadView,
            &str,
            &PlannerPredicates,
            &mut R,
            GqlQueryPolicy,
            &Live<'_, '_, '_>,
        ) -> Result<(T, usize), QueryError>,
    ) -> Result<T, QueryError> {
        self.run_scoped(cx, action, |error| error, terminal)
    }

    // Typed composition errors retain the SAME permit, lifecycle, post-error
    // validity check and delivery boundary. The mapper changes only the error
    // envelope; it cannot replace authority or supply an execution policy.
    fn run_scoped<T, E>(
        &mut self,
        cx: &QueryCx,
        action: impl FnOnce(
            &EmbeddedReadView,
            &str,
            &PlannerPredicates,
            &mut R,
            GqlQueryPolicy,
            &Live<'_, '_, '_>,
        ) -> Result<(T, usize), E>,
        map_error: impl Fn(QueryError) -> E,
        terminal: impl FnOnce(&Result<T, E>) -> bool,
    ) -> Result<T, E> {
        let mut state = self.state.take().ok_or_else(|| {
            map_error(QueryError::Authorization(
                AuthorizationError::ExecutionStopped,
            ))
        })?;
        if state.view.is_none() {
            return Err(map_error(QueryError::Authorization(
                AuthorizationError::ExecutionStopped,
            )));
        }
        let result = (|| {
            let State {
                view,
                capability,
                branch,
                resolver,
                policy,
                clock,
                last_now_ms,
                sealed,
            } = &mut state;
            let now = clock();
            if now < *last_now_ms {
                return Err(map_error(QueryError::Authorization(
                    AuthorizationError::ClockWentBackwards,
                )));
            }
            *last_now_ms = now;
            let permit = capability
                .begin_read_at(branch, now)
                .map_err(QueryError::Authorization)
                .map_err(&map_error)?;
            let mut tracked_clock = || {
                let now = clock();
                *last_now_ms = (*last_now_ms).max(now);
                now // The existing live permit refuses a backwards sample.
            };
            let execution = RefCell::new(Execution::new(
                cx,
                permit,
                &mut tracked_clock as &mut dyn FnMut() -> u64,
            ));
            execution.borrow_mut().checkpoint().map_err(&map_error)?;
            let view = view.as_ref().ok_or_else(|| {
                map_error(QueryError::Authorization(
                    AuthorizationError::ExecutionStopped,
                ))
            })?;
            execution.borrow_mut().beacon_indexes = Some(view.beacon_indexes.clone());
            execution.borrow_mut().sealed = sealed.clone();
            let result = cx.with_restriction(|| {
                action(
                    view,
                    branch,
                    capability.predicates(),
                    resolver,
                    *policy,
                    &execution,
                )
            });
            // Also check after a resolver/parser/binder failed. Such a callback
            // cannot hide expiry/retirement by returning its own error first.
            execution.borrow_mut().checkpoint().map_err(&map_error)?;
            // The action reports final rows not yet reserved. A read batch
            // reserves each completed result on this SAME permit before
            // retaining it, then reports zero here; no result is double charged.
            let (value, unreserved_rows) = result?;
            execution
                .borrow_mut()
                .deliver(unreserved_rows)
                .map_err(&map_error)?;
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
    pub fn query(
        &mut self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
    ) -> Result<QueryResult, QueryError> {
        self.query_with_snapshot(cx, text, params)
            .map(|(result, _)| result)
    }

    /// Execute a read and return the exact sequence selected for its rows.
    ///
    /// The result and sequence share one preparation, pinned generation and
    /// signed execution permit. A temporal selector may choose an older cut;
    /// an ordinary read uses this session's frontier even if the writer has
    /// advanced. EXPLAIN reports this frontier without reading graph rows.
    /// Neither metadata nor rows escape a failed execution or final check.
    pub fn query_with_snapshot(
        &mut self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
    ) -> Result<(QueryResult, CommitSeq), QueryError> {
        self.run(cx, |view, branch, scope, resolver, policy, execution| {
            text_at(
                view, branch, scope, resolver, policy, execution, text, params,
            )
        })
    }

    /// Prepare once using only this session's fixed trusted catalog. Parameters
    /// supply declared types; payloads are rebound on execute. The original
    /// branch selector is retained, including shared/exclusive parameter rules.
    pub fn prepare(
        &mut self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
    ) -> Result<AuthorizedPreparedRead, QueryError> {
        let owner = Arc::clone(&self.owner);
        self.run(cx, |_, branch, _, resolver, _, execution| {
            let lowered = fgdb_gql::GqlMapArgumentText::lower_fields(text, params);
            let (text, params) = lowered
                .as_ref()
                .map_or((text, params), |(lowered, derived)| {
                    (lowered.text(), derived)
                });
            let selector = PreparedGraphBranchText::prepare(text).map_err(selector_error)?;
            let selected = selector.bind_parameters(params).map_err(selector_error)?;
            check_branch(&selected, branch)?;
            refuse_element_identity(selected.statement())?;
            execution.borrow_mut().checkpoint()?;
            let native = PreparedNativeRead::prepare(
                selected.statement(),
                selected.parameters(),
                BorrowedResolver(resolver),
            );
            execution.borrow_mut().checkpoint()?;
            let native = native?;
            let lowered = lowered.map(|(lowered, _)| lowered);
            let parameter_schema = lowered.as_ref().map(|lowered| {
                lowered
                    .field_parameter_schema(native.parameter_schema())
                    .into_boxed_slice()
            });
            Ok((
                AuthorizedPreparedRead {
                    owner,
                    selector: Arc::new(selector),
                    native: Arc::new(native),
                    lowered,
                    parameter_schema,
                },
                0,
            ))
        })
    }

    /// Rebind an exact-session template without re-resolving its symbols.
    /// A template from another session refuses even for the same database and
    /// issuer; no cached plan may silently cross catalog or generation owners.
    pub fn execute(
        &mut self,
        cx: &QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
    ) -> Result<QueryResult, QueryError> {
        self.execute_with_snapshot(cx, prepared, params)
            .map(|(result, _)| result)
    }

    /// Execute an exact-session template with its bound snapshot metadata.
    /// Rebinding a temporal argument changes both rows and sequence together;
    /// the template never captures the sequence from its preparation call.
    pub fn execute_with_snapshot(
        &mut self,
        cx: &QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
    ) -> Result<(QueryResult, CommitSeq), QueryError> {
        let owner = Arc::clone(&self.owner);
        self.run(cx, |view, branch, scope, _, policy, execution| {
            prepared_at(
                view, branch, scope, policy, execution, &owner, prepared, params,
            )
        })
    }

    fn attach_prepared(
        &mut self,
        cx: &QueryCx,
        selector: Arc<PreparedGraphBranchText>,
        native: Arc<PreparedNativeRead>,
    ) -> Result<AuthorizedPreparedRead, QueryError> {
        let owner = Arc::clone(&self.owner);
        self.run(cx, |_, _, _, _, _, _| {
            if crate::query::explain::without_terminator(selector.statement()) != native.statement()
            {
                return Err(QueryError::Authorization(
                    AuthorizationError::WrongAuthority,
                ));
            }
            refuse_element_identity(selector.statement())?;
            Ok((
                AuthorizedPreparedRead {
                    owner,
                    selector,
                    native,
                    lowered: None,
                    parameter_schema: None,
                },
                0,
            ))
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
) -> Result<((QueryResult, CommitSeq), usize), QueryError> {
    // A record field keying a pattern is the constant it holds (fgdb-8srlm).
    let lowered = fgdb_gql::GqlMapArgumentText::lower_fields(text, params);
    let (text, params) = lowered
        .as_ref()
        .map_or((text, params), |(lowered, derived)| {
            (lowered.text(), derived)
        });
    let selector = PreparedGraphBranchText::prepare(text).map_err(selector_error)?;
    let selected = selector.bind_parameters(params).map_err(selector_error)?;
    check_branch(&selected, branch)?;
    refuse_element_identity(selected.statement())?;
    execution.borrow_mut().checkpoint()?;
    if let Some(explain) = crate::query::explain::explain_prefix(selected.statement()) {
        return result_rows(
            explain_authorized(
                explain,
                selected.parameters(),
                BorrowedResolver(resolver),
                || execution.borrow_mut().checkpoint(),
            )?,
            selected_cut(view, execution),
        );
    }
    let prepared = PreparedNativeRead::prepare(
        selected.statement(),
        selected.parameters(),
        BorrowedResolver(resolver),
    );
    execution.borrow_mut().checkpoint()?;
    let prepared = prepared?;
    let at = prepared.bound_snapshot_seq(selected.parameters(), selected_cut(view, execution))?;
    check_selected_cut(execution, at)?;
    result_rows(
        native_at(
            &prepared,
            selected.parameters(),
            &view.snapshot,
            at,
            scope,
            execution,
            policy,
        )?,
        at,
    )
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
) -> Result<((QueryResult, CommitSeq), usize), QueryError> {
    if !Arc::ptr_eq(owner, &prepared.owner) {
        return Err(QueryError::Authorization(
            AuthorizationError::WrongAuthority,
        ));
    }
    let selected = prepared
        .selector
        .bind_parameters(params)
        .map_err(selector_error)?;
    check_branch(&selected, branch)?;
    let params = prepared.parameters(selected.parameters())?;
    let at = prepared
        .native
        .bound_snapshot_seq(&params, selected_cut(view, execution))?;
    check_selected_cut(execution, at)?;
    result_rows(
        native_at(
            &prepared.native,
            &params,
            &view.snapshot,
            at,
            scope,
            execution,
            policy,
        )?,
        at,
    )
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
    /// Bind a retained host-prepared template to a fresh authorized read pin.
    /// The immutable selector and native template are shared without parsing
    /// or catalog callbacks. This privileged Database constructor is the only
    /// attachment seam; the narrowed session cannot accept an arbitrary plan.
    /// Its returned handle remains bound to this exact session.
    ///
    /// The host must validate that the template's catalog belongs to this
    /// database, branch and trusted resolver, just as for raw
    /// PreparedNativeRead::execute_authorized. Matching source text proves
    /// selector/template coherence, not catalog identity. Server hosts must
    /// additionally preserve their connection, authentication-generation and
    /// recovery fences before using this constructor.
    ///
    /// Signature, namespace and read-rights checks precede template inspection.
    /// Mismatched source refuses as WrongAuthority; identity-reading templates
    /// retain the native authorization refusal. Current scope and live permits
    /// still govern every execution. The fresh pin sees the current frontier,
    /// never a generation retained when the template was prepared. A failed
    /// constructor releases its new pin; the shared templates pin no graph.
    #[allow(clippy::too_many_arguments)]
    pub fn authorized_prepared_read_session<'a, R: GraphSymbolResolver, C: FnMut() -> u64>(
        &self,
        cx: &QueryCx,
        authority: &'a Authority,
        token: &CapabilityToken,
        branch: &str,
        resolver: R,
        policy: GqlQueryPolicy,
        clock: C,
        selector: Arc<PreparedGraphBranchText>,
        native: Arc<PreparedNativeRead>,
    ) -> Result<(AuthorizedReadSession<'a, R, C>, AuthorizedPreparedRead), QueryError> {
        let mut session =
            self.authorized_read_session(cx, authority, token, branch, resolver, policy, clock)?;
        let prepared = session.attach_prepared(cx, selector, native)?;
        Ok((session, prepared))
    }

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
        &self,
        cx: &QueryCx,
        authority: &'a Authority,
        token: &CapabilityToken,
        branch: &str,
        resolver: R,
        policy: GqlQueryPolicy,
        clock: C,
    ) -> Result<AuthorizedReadSession<'a, R, C>, QueryError> {
        admit_read_session(
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
    /// Narrow this exact authenticated generation into an owned read session.
    /// The namespace travels with the view; supplying a different issuer cannot
    /// relabel its data. All scope, clock, retirement and per-execution rules are
    /// identical to [`Database::authorized_read_session`]. A live writer is not
    /// borrowed or consulted, so a host must preserve its own recovery fence.
    #[allow(clippy::too_many_arguments)]
    pub fn authorized_read_session<'a, R: GraphSymbolResolver, C: FnMut() -> u64>(
        &self,
        cx: &QueryCx,
        authority: &'a Authority,
        token: &CapabilityToken,
        branch: &str,
        resolver: R,
        policy: GqlQueryPolicy,
        clock: C,
    ) -> Result<AuthorizedReadSession<'a, R, C>, QueryError> {
        admit_read_session(
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

    /// Attach host-owned immutable templates to this exact read generation.
    /// The catalog/source contract and live authority checks are those of
    /// [`Database::authorized_prepared_read_session`]; the host retains its
    /// connection and recovery-generation fences.
    #[allow(clippy::too_many_arguments)]
    pub fn authorized_prepared_read_session<'a, R: GraphSymbolResolver, C: FnMut() -> u64>(
        &self,
        cx: &QueryCx,
        authority: &'a Authority,
        token: &CapabilityToken,
        branch: &str,
        resolver: R,
        policy: GqlQueryPolicy,
        clock: C,
        selector: Arc<PreparedGraphBranchText>,
        native: Arc<PreparedNativeRead>,
    ) -> Result<(AuthorizedReadSession<'a, R, C>, AuthorizedPreparedRead), QueryError> {
        let mut session =
            self.authorized_read_session(cx, authority, token, branch, resolver, policy, clock)?;
        let prepared = session.attach_prepared(cx, selector, native)?;
        Ok((session, prepared))
    }
}

#[allow(clippy::too_many_arguments)]
fn admit_read_session<'a, R: GraphSymbolResolver, C: FnMut() -> u64>(
    namespace: fgdb_types::DatabaseSecurityNamespaceId,
    cx: &QueryCx,
    authority: &'a Authority,
    token: &CapabilityToken,
    branch: &str,
    resolver: R,
    policy: GqlQueryPolicy,
    mut clock: C,
    pin: impl FnOnce() -> Result<EmbeddedReadView, crate::ReadError>,
) -> Result<AuthorizedReadSession<'a, R, C>, QueryError> {
    if authority.namespace() != namespace {
        return Err(QueryError::Authorization(
            AuthorizationError::WrongAuthority,
        ));
    }
    let now = clock();
    let capability = authority
        .verify_at(token, branch, now)
        .map_err(QueryError::Authorization)?;
    let mut last_now_ms = now;
    let view = {
        let permit = capability
            .begin_read_at(branch, now)
            .map_err(QueryError::Authorization)?;
        let mut tracked_clock = || {
            let now = clock();
            last_now_ms = last_now_ms.max(now);
            now
        };
        let mut execution = Execution::new(cx, permit, &mut tracked_clock);
        execution.checkpoint()?;
        let view = pin().map_err(QueryError::Read)?;
        execution.deliver(0)?;
        view
    };
    Ok(AuthorizedReadSession {
        state: Some(State {
            view: Some(view),
            capability,
            branch: branch.to_owned(),
            resolver,
            policy,
            clock,
            last_now_ms,
            sealed: None,
        }),
        owner: Arc::new(()),
    })
}

#[cfg(test)]
#[path = "session/failure_tests.rs"]
mod failure_tests;
