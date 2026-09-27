//! Host-configured mutable sessions over the existing authorized write path.
//! No raw database, issuer, transaction, allocator or host callback escapes.

use super::{
    Authority, CapabilityToken, CommitCx, Database, Error, Execution, Fault, GqlParameters,
    GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy, GraphWriteProgramReceipt, Input,
    PreparedGraphWriteScript, QueryCx, RelationId, TxnCx, Vfs, WriteTxnError, admission,
    checkpoint,
};
use fgdb_gql::GraphWriteScriptBatchError;
use fgdb_types::EmbeddedTxnCompletion;
use fgdb_warden::{ExecutionPermit, VerifiedCapability, WriteAccess};
use std::sync::Arc;

type Receipt = (GraphWriteProgramReceipt, EmbeddedTxnCompletion);

struct State<'db, 'issuer, V: Vfs, Resolver, Clock> {
    database: &'db mut Database<V>,
    txn_cx: &'db TxnCx,
    commit_cx: &'db CommitCx,
    capability: VerifiedCapability<'issuer>,
    branch: String,
    resolver: Resolver,
    relation: RelationId,
    policy: GraphWriteProgramPolicy,
    max_statements: usize,
    clock: Clock,
    last_now_ms: u64,
}

/// A capability-only mutable handle constructed by the trusted embedded host.
/// Obtain it from `Database::authorized_write_session`; its type can be inferred.
///
/// Request code supplies only native text, parameter values, this session's own
/// prepared handles and a QueryCx. The issuer, capability, exact branch route,
/// catalog resolver, clock, transaction/commit contexts and native ceilings are
/// fixed at construction. No method exposes or replaces them. There is no
/// Deref, raw WriteBatch, caller-selected identity, begin/commit/rollback handle,
/// privileged fallback, automatic retry, or conversion back to Database.
///
/// Each successful command is one ordinary native autocommit, not a long-lived
/// transaction: later commands see preceding commits, and one command cannot
/// leave a staged prefix for another. Every operation gets one fresh signed
/// PER-EXECUTION allowance, shared by its preparation, binding and execution.
/// The maximum observed clock is retained across operations, so starting a new
/// permit cannot hide backwards host time. Issuer retirement and expiry remain
/// live. Prepared templates are tied to this exact session, not just its token.
///
/// This session deliberately fails closed on ANY error, including bad syntax,
/// parameters or quotas. Unwinding or dropping a polled command future also
/// closes it. A future dropped before its first poll never began an operation.
/// Closed sessions cannot be revived by another request; the host must drop the
/// handle and decide whether to create a new one. Unknown/recovery outcomes are
/// preserved, never called rollback. No callback or auth check follows commit.
///
/// This is an embedded request boundary, not the system-wide fgdb-secure-view
/// architecture. The host still owns raw Database access and a capability-safe
/// resolver. Resident storage/overlay work, sequential allocation metadata,
/// durable audit/revocation, byte-level preemption and full SSI remain outside
/// this subset. The session itself retains no transaction snapshot pin.
pub struct AuthorizedWriteSession<'db, 'issuer, V: Vfs, Resolver, Clock> {
    state: Option<State<'db, 'issuer, V, Resolver, Clock>>,
    owner: Arc<()>,
}

/// A native script template admitted by exactly one mutable session. There is
/// no public constructor, raw script/plan accessor or independently usable
/// authorization. Only parameter-schema metadata is exposed. Values supplied
/// during prepare are checked, but executions bind their own argument values.
pub struct AuthorizedPreparedWrite {
    owner: Arc<()>,
    script: PreparedGraphWriteScript,
}

impl<V: Vfs, R, C> core::fmt::Debug for AuthorizedWriteSession<'_, '_, V, R, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AuthorizedWriteSession")
            .field("closed", &self.is_closed())
            .field("authority_and_database", &"[REDACTED]")
            .finish()
    }
}
impl core::fmt::Debug for AuthorizedPreparedWrite {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AuthorizedPreparedWrite([REDACTED])")
    }
}
impl AuthorizedPreparedWrite {
    pub fn parameter_schema(&self) -> &[fgdb_gql::GqlParameterSpec] {
        self.script.parameter_schema()
    }
}
impl<V: Vfs, R, C> AuthorizedWriteSession<'_, '_, V, R, C> {
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state.is_none()
    }

    /// Permanently close without evaluating a statement or calling host code.
    /// Drop the session to make the host's exclusive database borrow available.
    pub fn close(&mut self) {
        self.state = None;
    }
}

fn stopped() -> Fault {
    admission(WriteTxnError::Authorization(Error::ExecutionStopped))
}

#[allow(clippy::result_large_err)]
fn begin<'a, C: FnMut() -> u64>(
    capability: &'a VerifiedCapability<'_>,
    branch: &str,
    clock: &mut C,
    last_now_ms: &mut u64,
) -> Result<ExecutionPermit<'a, WriteAccess>, Fault> {
    let now = clock();
    if now < *last_now_ms {
        return Err(admission(WriteTxnError::Authorization(Error::ClockWentBackwards)));
    }
    *last_now_ms = now;
    capability
        .begin_write_at(branch, now)
        .map_err(|error| admission(WriteTxnError::Authorization(error)))
}

fn tracked_clock<'a, C: FnMut() -> u64>(
    clock: &'a mut C,
    last_now_ms: &'a mut u64,
) -> impl FnMut() -> u64 + 'a {
    move || {
        let now = clock();
        *last_now_ms = (*last_now_ms).max(now);
        now
    }
}

#[allow(clippy::result_large_err)]
fn statement_limit(script: &PreparedGraphWriteScript, limit: usize) -> Result<(), Fault> {
    let observed = script.statements().len() as u128;
    if observed > limit as u128 {
        return Err(Fault::BatchBinding(GraphWriteScriptBatchError::TooManyStatements {
            limit,
            observed,
        }));
    }
    Ok(())
}

impl<V: Vfs + Clone> Database<V> {
    /// Fix a mutable session's authority and all trusted host inputs once.
    ///
    /// Namespace, signature, branch and Write rights are checked before catalog
    /// callbacks or graph observation. No snapshot pin or graph ID is allocated.
    /// Write-only grants can create; selected writes require ReadWrite inside
    /// the ordinary typed preflight. max_statements is an additional host ceiling
    /// per operation (including all records of an ingestion batch), clamped to
    /// the existing native hard limit; it is not writable by request code.
    ///
    /// The host lends both purpose contexts and the exclusive Database borrow
    /// for the session lifetime. The returned type has no raw escape hatch.
    ///
    /// ```compile_fail,E0616
    /// # use fgdb::{Database, MemVfs};
    /// # use fgdb_types::{TxnCx, CommitCx};
    /// # use fgdb_warden::{Authority, CapabilityToken};
    /// # use fgdb_gql::{GqlQueryPolicy, GraphWriteProgramPolicy};
    /// # use fgdb_delta_types::RelationId;
    /// # fn example(db: &mut Database<MemVfs>, t: &TxnCx, c: &CommitCx,
    /// #            issuer: &Authority, token: &CapabilityToken) {
    /// # let policy = GraphWriteProgramPolicy::new(GqlQueryPolicy::new(10,10,100,100),10,10,10);
    /// let session = db.authorized_write_session(t, c, issuer, token, "main",
    ///     |_, _| None, RelationId(1), policy, 64, || 100).unwrap();
    /// let raw_authority_and_database = session.state;
    /// # }
    /// ```
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::result_large_err)]
    pub fn authorized_write_session<'db, 'issuer, R, C>(
        &'db mut self,
        txn_cx: &'db TxnCx,
        commit_cx: &'db CommitCx,
        authority: &'issuer Authority,
        token: &CapabilityToken,
        branch: &str,
        resolver: R,
        relation: RelationId,
        policy: GraphWriteProgramPolicy,
        max_statements: usize,
        mut clock: C,
    ) -> Result<AuthorizedWriteSession<'db, 'issuer, V, R, C>, Fault>
    where
        R: FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
        C: FnMut() -> u64,
    {
        commit_cx.checkpoint().map_err(WriteTxnError::Interrupted).map_err(admission)?;
        let refusal = |error| admission(WriteTxnError::Authorization(error));
        if authority.namespace() != self.keys.namespace {
            return Err(refusal(Error::WrongAuthority));
        }
        let now = clock();
        let capability = authority.verify_at(token, branch, now).map_err(refusal)?;
        capability.begin_write_at(branch, now).map_err(refusal)?;
        Ok(AuthorizedWriteSession {
            state: Some(State {
                database: self,
                txn_cx,
                commit_cx,
                capability,
                branch: branch.to_owned(),
                resolver,
                relation,
                policy,
                max_statements: max_statements.min(PreparedGraphWriteScript::MAX_BATCH_STATEMENTS),
                clock,
                last_now_ms: now,
            }),
            owner: Arc::new(()),
        })
    }
}

enum Request<'a> {
    Text(&'a str, &'a GqlParameters),
    Prepared(&'a AuthorizedPreparedWrite, &'a GqlParameters),
}

impl<V, R, C> AuthorizedWriteSession<'_, '_, V, R, C>
where
    V: Vfs + Clone,
    R: FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    C: FnMut() -> u64,
{
    #[allow(clippy::result_large_err)]
    pub fn prepare(
        &mut self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
    ) -> Result<AuthorizedPreparedWrite, Fault> {
        let mut state = self.state.take().ok_or_else(stopped)?;
        let result = (|| {
            let permit = begin(&state.capability, &state.branch, &mut state.clock, &mut state.last_now_ms)?;
            let mut execution = Execution {
                cx: state.commit_cx,
                permit,
                clock: tracked_clock(&mut state.clock, &mut state.last_now_ms),
            };
            let script = super::prepare(cx, &mut execution, text, params, state.relation, &mut state.resolver)?;
            statement_limit(&script, state.max_statements)?;
            Input::Script(&script, params).bind(cx, state.capability.predicates(), &mut execution)?;
            checkpoint(cx, &mut execution).map_err(admission)?;
            Ok(AuthorizedPreparedWrite { owner: Arc::clone(&self.owner), script })
        })();
        if result.is_ok() {
            self.state = Some(state);
        }
        result
    }

    #[allow(clippy::result_large_err)]
    pub async fn query(
        &mut self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
    ) -> Result<Receipt, Fault> {
        self.run(cx, Request::Text(text, params)).await
    }

    #[allow(clippy::result_large_err)]
    pub async fn execute(
        &mut self,
        cx: &QueryCx,
        prepared: &AuthorizedPreparedWrite,
        params: &GqlParameters,
    ) -> Result<Receipt, Fault> {
        self.run(cx, Request::Prepared(prepared, params)).await
    }

    #[allow(clippy::result_large_err)]
    async fn run(&mut self, cx: &QueryCx, request: Request<'_>) -> Result<Receipt, Fault> {
        let mut state = self.state.take().ok_or_else(stopped)?;
        let result = state.run(cx, request, &self.owner).await;
        if result.is_ok() {
            self.state = Some(state);
        }
        result
    }
}

impl<V, R, C> State<'_, '_, V, R, C>
where
    V: Vfs + Clone,
    R: FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    C: FnMut() -> u64,
{
    #[allow(clippy::result_large_err)]
    async fn run(
        &mut self,
        cx: &QueryCx,
        request: Request<'_>,
        owner: &Arc<()>,
    ) -> Result<Receipt, Fault> {
        let State {
            database,
            txn_cx,
            commit_cx,
            capability,
            branch,
            resolver,
            relation,
            policy,
            max_statements,
            clock,
            last_now_ms,
        } = self;
        let (txn_cx, commit_cx) = (*txn_cx, *commit_cx);
        let permit = begin(capability, branch, clock, last_now_ms)?;
        let mut execution = Execution {
            cx: commit_cx,
            permit,
            clock: tracked_clock(clock, last_now_ms),
        };
        commit_cx.with_restriction_async(async {
            checkpoint(cx, &mut execution).map_err(admission)?;
            let parsed;
            let (script, params) = match request {
                Request::Text(text, params) => {
                    parsed = super::prepare(cx, &mut execution, text, params, *relation, resolver)?;
                    (&parsed, params)
                }
                Request::Prepared(prepared, params) => {
                    if !Arc::ptr_eq(owner, &prepared.owner) {
                        return Err(admission(WriteTxnError::AuthorizedMutationRefused));
                    }
                    (&prepared.script, params)
                }
            };
            statement_limit(script, *max_statements)?;
            let bound = Input::Script(script, params).bind(cx, capability.predicates(), &mut execution)?;
            database.complete_authorized_program(
                txn_cx, cx, commit_cx, bound.program(), *policy,
                capability.predicates(), &mut execution, true,
                GraphWriteProgramReceipt::new,
            ).await.map_err(|error| bound.error(error))
        }).await
    }
}

#[cfg(test)]
#[path = "write_session_tests.rs"]
mod tests;
