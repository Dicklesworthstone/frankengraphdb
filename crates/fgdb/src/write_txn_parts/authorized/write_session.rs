//! Host-configured mutable sessions over the existing authorized write path.
//! No raw database, issuer, transaction, allocator or host callback escapes.

use super::super::Bound;
use super::{
    Authority, CapabilityToken, CommitCx, Database, Error, Execution, Fault, GqlParameters,
    GraphWriteProgramPolicy, GraphWriteProgramReceipt, Input, PreparedGraphWriteScript, QueryCx,
    RelationId, TxnCx, Vfs, WriteTxnError, admission, checkpoint, statement_limit,
};
use fgdb_gql::{
    BoundGraphWriteScriptBatch, GqlMapArgumentText, GqlParameterSpec, GraphSymbolResolver,
    GraphWriteProgramStats, GraphWriteScriptBatchLocation, GraphWriteStepReceipt,
    PreparedGraphWriteChain, PreparedGraphWriteChainText,
};
use fgdb_types::EmbeddedTxnCompletion;
use fgdb_warden::{ExecutionPermit, VerifiedCapability, WriteAccess};
use std::sync::Arc;

#[cfg(test)]
use fgdb_gql::GraphWriteScriptBatchError;

#[cfg(test)]
use super::{GraphSymbol, GraphSymbolKind};

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

/// A native write template admitted by exactly one mutable session. There is
/// no public constructor, raw script/plan accessor or independently usable
/// authorization. Only parameter-schema metadata is exposed. Values supplied
/// during prepare are checked, but executions bind their own argument values.
pub struct AuthorizedPreparedWrite {
    owner: Arc<()>,
    definition: PreparedDefinition,
    schema: Vec<GqlParameterSpec>,
}

enum PreparedDefinition {
    Script(PreparedGraphWriteScript),
    Chain(Box<PreparedChain>),
}

struct PreparedChain {
    template: PreparedGraphWriteChainText,
    // Only source offsets and field paths survive preparation. Each execution
    // derives fresh operands from its own caller-supplied map values.
    lowered: Option<GqlMapArgumentText>,
}

impl PreparedChain {
    #[allow(clippy::result_large_err)]
    fn bind<Clock: FnMut() -> u64>(
        &self,
        cx: &QueryCx,
        execution: &mut Execution<'_, '_, Clock>,
        params: &GqlParameters,
    ) -> Result<PreparedGraphWriteChain, Fault> {
        cx.with_restriction(|| {
            let derived = match &self.lowered {
                Some(lowered) => {
                    let arguments = lowered.arguments(params);
                    checkpoint(cx, execution).map_err(admission)?;
                    Some(arguments.map_err(|error| super::map_argument_fault(0, &error))?)
                }
                None => None,
            };
            super::bind_chain_template(
                cx,
                execution,
                &self.template,
                derived.as_ref().unwrap_or(params),
            )
            .map_err(|fault| super::restored(self.lowered.as_ref(), fault))
        })
    }
}

/// An entirely bound, same-session ingestion program. It cannot be constructed
/// from a raw program, changed, or executed by another session. Reuse obtains a
/// fresh live permit but performs no catalog lookup, value rebind or deep clone.
pub struct AuthorizedBoundWriteBatch {
    owner: Arc<()>,
    batch: BoundGraphWriteScriptBatch,
}

impl core::fmt::Debug for AuthorizedBoundWriteBatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AuthorizedBoundWriteBatch")
            .field("argument_sets", &self.batch.argument_sets())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl AuthorizedBoundWriteBatch {
    #[must_use]
    pub fn argument_sets(&self) -> usize {
        self.batch.argument_sets()
    }

    /// Original input coordinates, not graph records or an authority handle.
    #[must_use]
    pub fn location(&self, statement: usize) -> Option<GraphWriteScriptBatchLocation> {
        self.batch.location(statement)
    }

    /// Slice a complete receipt by input record. This retains the native
    /// shape check; it is not proof of receipt provenance or commit authority.
    #[must_use]
    pub fn record_receipts<'r>(
        &self,
        receipt: &'r GraphWriteProgramReceipt,
        argument_set: usize,
    ) -> Option<&'r [GraphWriteStepReceipt]> {
        self.batch.record_receipts(receipt, argument_set)
    }
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
        &self.schema
    }

    #[allow(clippy::result_large_err)]
    fn batch_script(&self) -> Result<&PreparedGraphWriteScript, Fault> {
        match &self.definition {
            PreparedDefinition::Script(script) => Ok(script),
            // Native chains need a shared batch workspace, not a loop of
            // independent autocommits. Refuse before binding or graph access.
            PreparedDefinition::Chain(_) => {
                Err(admission(WriteTxnError::AuthorizedMutationRefused))
            }
        }
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

// Return only a borrow of the verified authority. Neither mutable clock borrow
// escapes: the tracker below will use them through all phases of this operation.
#[allow(clippy::result_large_err)]
fn begin<'a, C: FnMut() -> u64>(
    capability: &'a VerifiedCapability<'_>,
    branch: &str,
    clock: &mut C,
    last_now_ms: &mut u64,
) -> Result<ExecutionPermit<'a, WriteAccess>, Fault> {
    let now = clock();
    if now < *last_now_ms {
        return Err(admission(WriteTxnError::Authorization(
            Error::ClockWentBackwards,
        )));
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
        // Return the actual sample. The live permit, not this high-water mark,
        // refuses backwards time WITHIN the operation.
        now
    }
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
    ///     |_: fgdb_gql::GraphSymbolKind, _: &str| None,
    ///     RelationId(1), policy, 64, || 100).unwrap();
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
        R: GraphSymbolResolver,
        C: FnMut() -> u64,
    {
        commit_cx
            .checkpoint()
            .map_err(WriteTxnError::Interrupted)
            .map_err(admission)?;
        let refusal = |error| admission(WriteTxnError::Authorization(error));
        if authority.namespace() != self.keys.namespace {
            return Err(refusal(Error::WrongAuthority));
        }
        let now = clock();
        let capability = authority.verify_at(token, branch, now).map_err(refusal)?;
        // Verification alone is not a write grant. Do not retain a read-only
        // or retired capability in a mutable session.
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
    TextBatch(&'a str, &'a [GqlParameters]),
    Prepared(&'a AuthorizedPreparedWrite, &'a GqlParameters),
    Batch(&'a AuthorizedPreparedWrite, &'a [GqlParameters]),
    Bound(&'a AuthorizedBoundWriteBatch),
}

enum SessionBound<'a> {
    Program(Bound<'a>),
    Chain(Box<PreparedGraphWriteChain>),
}

impl<V, R, C> AuthorizedWriteSession<'_, '_, V, R, C>
where
    V: Vfs + Clone,
    R: GraphSymbolResolver + Send,
    C: FnMut() -> u64 + Send,
{
    /// Prepare once through this session's fixed resolver, under a live permit.
    /// All sample arguments and operation rights are checked, but there is no
    /// database read or graph ID reservation. Later execute calls never resolve
    /// these names again and cannot substitute another session's template.
    #[allow(clippy::result_large_err)]
    pub fn prepare(
        &mut self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
    ) -> Result<AuthorizedPreparedWrite, Fault> {
        let mut state = self.state.take().ok_or_else(stopped)?;
        let result = (|| {
            let permit = begin(
                &state.capability,
                &state.branch,
                &mut state.clock,
                &mut state.last_now_ms,
            )?;
            let mut execution = Execution {
                cx: state.commit_cx,
                permit,
                clock: tracked_clock(&mut state.clock, &mut state.last_now_ms),
            };
            let (definition, schema) = cx.with_restriction(|| {
                super::reserve_text(cx, &mut execution, text)?;
                if super::is_chain_admitted(cx, &mut execution, text)? {
                    super::chain_statement_limit(state.max_statements)?;
                    let lowered = GqlMapArgumentText::lower_fields(text, params);
                    let (statement, arguments) = lowered
                        .as_ref()
                        .map_or((text, params), |(recipe, derived)| (recipe.text(), derived));
                    let recipe = lowered.as_ref().map(|(recipe, _)| recipe);
                    let template = super::prepare_chain_admitted(
                        cx,
                        &mut execution,
                        statement,
                        arguments,
                        state.relation,
                        super::BorrowedCatalog(&mut state.resolver),
                    )
                    .map_err(|fault| super::restored(recipe, fault))?;
                    // This API exposes receipts/statistics, not RETURN rows.
                    // Never remove a projection to make a write admissible.
                    if template.returns_rows() {
                        return Err(admission(WriteTxnError::AuthorizedMutationRefused));
                    }
                    let bound =
                        super::bind_chain_template(cx, &mut execution, &template, arguments)
                            .map_err(|fault| super::restored(recipe, fault))?;
                    crate::write_txn::authorized::chain::admit_definition(
                        &bound,
                        None,
                        cx,
                        state.capability.predicates(),
                        &mut execution,
                    )
                    .map_err(Fault::Chain)?;
                    let schema = template.parameter_schema();
                    let schema = if let Some(recipe) = recipe {
                        recipe.field_parameter_schema(&schema)
                    } else {
                        schema
                    };
                    Ok((
                        PreparedDefinition::Chain(Box::new(PreparedChain {
                            template,
                            lowered: lowered.map(|(recipe, _)| recipe),
                        })),
                        schema,
                    ))
                } else {
                    let script = super::prepare_admitted(
                        cx,
                        &mut execution,
                        text,
                        params,
                        state.relation,
                        super::BorrowedCatalog(&mut state.resolver),
                    )?;
                    statement_limit(&script, state.max_statements)?;
                    Input::Script(&script, params).bind(
                        cx,
                        state.capability.predicates(),
                        &mut execution,
                    )?;
                    let schema = script.parameter_schema().to_vec();
                    Ok((PreparedDefinition::Script(script), schema))
                }
            })?;
            checkpoint(cx, &mut execution).map_err(admission)?;
            Ok(AuthorizedPreparedWrite {
                owner: Arc::clone(&self.owner),
                definition,
                schema,
            })
        })();
        if result.is_ok() {
            self.state = Some(state);
        }
        result
    }

    /// Parse, bind and commit native text with no request-supplied authority,
    /// allocator, catalog, route, clock or execution policy. The result is the
    /// ordered script identity receipt plus the original native completion.
    /// UNWIND MERGE/MATCH mutations bind all rows before graph access, under
    /// the same permit as execution. Every expanded row counts against the
    /// host's max_statements; no row or phase receives a fresh allowance.
    /// CREATE/INSERT UNWIND retains its native compiler. A failure closes this
    /// session and preserves original row coordinates and publication causes.
    /// Native chains and FOREACH share one atomic workspace and one allowance;
    /// their receipt contains aggregate statistics with no implicit identity
    /// rows. Explicit RETURN is refused before execution by this receipt API.
    #[allow(clippy::result_large_err)]
    pub async fn query(
        &mut self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
    ) -> Result<Receipt, Fault> {
        self.run(
            cx,
            Request::Text(text, params),
            true,
            GraphWriteProgramReceipt::new,
        )
        .await
    }

    /// Execute the same native text without retaining or delivering identity
    /// receipts. A signed max_rows=0 is usable for ingestion; graph visibility,
    /// binding, mutation and work limits are identical to query(). This does
    /// not suppress errors or turn an uncertain completion into success.
    /// Native chains and FOREACH use the same atomic owner as query(); an
    /// explicit RETURN is refused rather than silently discarding its rows.
    #[allow(clippy::result_large_err)]
    pub async fn query_stats(
        &mut self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
    ) -> Result<(GraphWriteProgramStats, EmbeddedTxnCompletion), Fault> {
        self.run(cx, Request::Text(text, params), false, |stats, _| stats)
            .await
    }

    /// Prepare native write text once, bind every argument set, then commit
    /// one stats-only ingestion program under ONE live execution permit.
    /// Preparation, all record bindings, execution and final admission share
    /// the signed work/node limits and the host's expanded-statement ceiling.
    /// The first argument set declares operand types; every set must satisfy
    /// that same native schema. Empty input, unsupported syntax and a bad final
    /// record refuse before graph observation or engine identity allocation.
    ///
    /// This uses the native write-script compiler, which refuses RETURN;
    /// it never strips or silently skips a requested projection. Successful
    /// ingestion exposes no identity receipts, so max_rows=0 is usable. The
    /// engine allocates identities and publishes at the ordinary single
    /// Chronicle completion boundary. Repeating this call is a new write,
    /// never an idempotent retry or a retained multi-request transaction.
    #[allow(clippy::result_large_err)]
    pub async fn query_batch_stats(
        &mut self,
        cx: &QueryCx,
        text: &str,
        arguments: &[GqlParameters],
    ) -> Result<(GraphWriteProgramStats, EmbeddedTxnCompletion), Fault> {
        self.run(
            cx,
            Request::TextBatch(text, arguments),
            false,
            |stats, _| stats,
        )
        .await
    }

    /// Rebind a template from this exact session, then execute one atomic
    /// program. A foreign template fails before binding or graph observation,
    /// even if issuer, database keys, statement and token happen to match.
    /// A native chain's receipt contains aggregate statistics and no implicit
    /// identity rows, just as query() does for the same native text.
    #[allow(clippy::result_large_err)]
    pub async fn execute(
        &mut self,
        cx: &QueryCx,
        prepared: &AuthorizedPreparedWrite,
        params: &GqlParameters,
    ) -> Result<Receipt, Fault> {
        self.run(
            cx,
            Request::Prepared(prepared, params),
            true,
            GraphWriteProgramReceipt::new,
        )
        .await
    }

    /// Rebind and atomically execute a same-session template without retaining
    /// identity receipts. Preparation values are never reused; the fresh bind,
    /// all native writes and final admission share this execution's allowance.
    #[allow(clippy::result_large_err)]
    pub async fn execute_stats(
        &mut self,
        cx: &QueryCx,
        prepared: &AuthorizedPreparedWrite,
        params: &GqlParameters,
    ) -> Result<(GraphWriteProgramStats, EmbeddedTxnCompletion), Fault> {
        self.run(
            cx,
            Request::Prepared(prepared, params),
            false,
            |stats, _| stats,
        )
        .await
    }

    /// Bind ALL input records and execute one atomic ingestion program. The
    /// host's expanded-statement ceiling is checked before allocation. Binding,
    /// all seven write families and result delivery consume the SAME permit;
    /// neither record nor statement boundaries reset native or signed quotas.
    /// A bad final record or refused final step returns no committed prefix.
    #[allow(clippy::result_large_err)]
    pub async fn execute_batch(
        &mut self,
        cx: &QueryCx,
        prepared: &AuthorizedPreparedWrite,
        arguments: &[GqlParameters],
    ) -> Result<Receipt, Fault> {
        self.run(
            cx,
            Request::Batch(prepared, arguments),
            true,
            GraphWriteProgramReceipt::new,
        )
        .await
    }

    /// The same atomic ingestion without retaining/delivering identity rows.
    /// max_rows=0 remains useful; all mutation, source and work caps still apply.
    #[allow(clippy::result_large_err)]
    pub async fn execute_batch_stats(
        &mut self,
        cx: &QueryCx,
        prepared: &AuthorizedPreparedWrite,
        arguments: &[GqlParameters],
    ) -> Result<(GraphWriteProgramStats, EmbeddedTxnCompletion), Fault> {
        self.run(
            cx,
            Request::Batch(prepared, arguments),
            false,
            |stats, _| stats,
        )
        .await
    }

    /// Bind a reusable finite batch without graph access or identity allocation.
    /// This is its own preparation operation, with its own per-execution permit.
    /// execute_batch instead composes binding and execution under ONE permit.
    /// A late binding/expiry failure discards the entire batch and closes the
    /// session. The returned immutable handle is valid only for this owner.
    #[allow(clippy::result_large_err)]
    pub fn bind_batch(
        &mut self,
        cx: &QueryCx,
        prepared: &AuthorizedPreparedWrite,
        arguments: &[GqlParameters],
    ) -> Result<AuthorizedBoundWriteBatch, Fault> {
        let mut state = self.state.take().ok_or_else(stopped)?;
        let result = (|| {
            let permit = begin(
                &state.capability,
                &state.branch,
                &mut state.clock,
                &mut state.last_now_ms,
            )?;
            let mut execution = Execution {
                cx: state.commit_cx,
                permit,
                clock: tracked_clock(&mut state.clock, &mut state.last_now_ms),
            };
            checkpoint(cx, &mut execution).map_err(admission)?;
            if !Arc::ptr_eq(&self.owner, &prepared.owner) {
                return Err(admission(WriteTxnError::AuthorizedMutationRefused));
            }
            let bound = Input::Batch(prepared.batch_script()?, arguments, state.max_statements)
                .bind(cx, state.capability.predicates(), &mut execution)?;
            checkpoint(cx, &mut execution).map_err(admission)?;
            // Input::Batch is the only producer above. Keep this fail-closed
            // rather than exposing a raw program if its binding contract changes.
            let Bound::Batch(batch) = bound else {
                return Err(admission(WriteTxnError::AuthorizedMutationRefused));
            };
            Ok(AuthorizedBoundWriteBatch {
                owner: Arc::clone(&self.owner),
                batch,
            })
        })();
        if result.is_ok() {
            self.state = Some(state);
        }
        result
    }

    /// Freeze a bounded native UNWIND MERGE/MATCH batch for this exact session.
    /// All rows, scalar kinds, catalog symbols and the host's expanded-statement
    /// cap are admitted under one live preparation permit. No graph observation,
    /// snapshot pin, identity reservation or mutation occurs. The existing native
    /// classifier/binder is the only compiler; a non-batch form is refused rather
    /// than converted into a new batch interpretation or executed as a fallback.
    ///
    /// This owns the supplied values, not a template for later argument sets.
    /// execute_bound_batch[_stats] reuses them without catalog lookup or rebind,
    /// checks live authority again and commits once per invocation. Re-execution
    /// is a new write, NOT an idempotency/retry guarantee. Preparation and each
    /// execution are separate operations with separate per-execution permits;
    /// query[_stats] instead shares ONE permit across both phases.
    ///
    /// Any error or unwind closes the session; no partial batch escapes. The
    /// handle retains native input coordinates, but neither its bound program
    /// nor this session's authority can be exported or used by another owner.
    #[allow(clippy::result_large_err)]
    pub fn bind_unwind_batch(
        &mut self,
        cx: &QueryCx,
        text: &str,
        arguments: &GqlParameters,
    ) -> Result<AuthorizedBoundWriteBatch, Fault> {
        let mut state = self.state.take().ok_or_else(stopped)?;
        let result = (|| {
            let permit = begin(
                &state.capability,
                &state.branch,
                &mut state.clock,
                &mut state.last_now_ms,
            )?;
            let mut execution = Execution {
                cx: state.commit_cx,
                permit,
                clock: tracked_clock(&mut state.clock, &mut state.last_now_ms),
            };
            checkpoint(cx, &mut execution).map_err(admission)?;
            let bound = super::bind_native(
                cx,
                &mut execution,
                text,
                arguments,
                state.relation,
                state.capability.predicates(),
                state.max_statements,
                super::BorrowedCatalog(&mut state.resolver),
            )?;
            checkpoint(cx, &mut execution).map_err(admission)?;
            let Bound::Batch(batch) = bound else {
                return Err(admission(WriteTxnError::AuthorizedMutationRefused));
            };
            Ok(AuthorizedBoundWriteBatch {
                owner: Arc::clone(&self.owner),
                batch,
            })
        })();
        if result.is_ok() {
            self.state = Some(state);
        }
        result
    }

    /// Execute an immutable same-session bound batch without rebind or cloning.
    /// Credentials are live, not inherited as a reusable unchecked permit.
    #[allow(clippy::result_large_err)]
    pub async fn execute_bound_batch(
        &mut self,
        cx: &QueryCx,
        batch: &AuthorizedBoundWriteBatch,
    ) -> Result<Receipt, Fault> {
        self.run(
            cx,
            Request::Bound(batch),
            true,
            GraphWriteProgramReceipt::new,
        )
        .await
    }

    #[allow(clippy::result_large_err)]
    pub async fn execute_bound_batch_stats(
        &mut self,
        cx: &QueryCx,
        batch: &AuthorizedBoundWriteBatch,
    ) -> Result<(GraphWriteProgramStats, EmbeddedTxnCompletion), Fault> {
        self.run(cx, Request::Bound(batch), false, |stats, _| stats)
            .await
    }

    /// Type-erased like the other commit chokepoints (fgdb-a5y6m): every
    /// authorized execute method ends here, so a caller's `Send` proof stops at
    /// `dyn Future + Send` instead of descending the whole authorized write
    /// chain. Without it, `fgdb`'s own lab tests overflowed proving `Send`.
    /// Boxing as `Send` is why this impl requires a `Send` resolver and clock.
    #[allow(clippy::result_large_err)]
    fn run<'a, T: Send + 'a>(
        &'a mut self,
        cx: &'a QueryCx,
        request: Request<'a>,
        returning: bool,
        receipt: impl FnOnce(GraphWriteProgramStats, Vec<GraphWriteStepReceipt>) -> T + Send + 'a,
    ) -> crate::SendFuture<'a, Result<(T, EmbeddedTxnCompletion), Fault>> {
        Box::pin(async move {
            // This move is the fail-closed future/unwind guard. The session has
            // no open state while the operation is pending. Success restores it
            // in a non-fallible tail; dropping this future drops its private
            // state.
            let mut state = self.state.take().ok_or_else(stopped)?;
            let result = state
                .run(cx, request, &self.owner, returning, receipt)
                .await;
            if result.is_ok() {
                self.state = Some(state);
            }
            result
        })
    }
}

impl<V, R, C> State<'_, '_, V, R, C>
where
    V: Vfs + Clone,
    R: GraphSymbolResolver + Send,
    C: FnMut() -> u64 + Send,
{
    #[allow(clippy::result_large_err)]
    async fn run<T: Send>(
        &mut self,
        cx: &QueryCx,
        request: Request<'_>,
        owner: &Arc<()>,
        returning: bool,
        receipt: impl FnOnce(GraphWriteProgramStats, Vec<GraphWriteStepReceipt>) -> T + Send,
    ) -> Result<(T, EmbeddedTxnCompletion), Fault> {
        // Split the borrows explicitly. The permit/clock must remain live over
        // native completion without borrowing the database or resolver through
        // a second mutable borrow of the entire session state.
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
        commit_cx
            .with_restriction_async(async {
                checkpoint(cx, &mut execution).map_err(admission)?;
                let bound = match request {
                    Request::Text(text, params) => {
                        let chain = cx.with_restriction(|| {
                            super::reserve_text(cx, &mut execution, text)?;
                            super::is_chain_admitted(cx, &mut execution, text)
                        })?;
                        if chain {
                            super::chain_statement_limit(*max_statements)?;
                            let chain = super::bind_chain_admitted(
                                cx,
                                &mut execution,
                                text,
                                params,
                                *relation,
                                super::BorrowedCatalog(resolver),
                            )?;
                            if chain.returns_rows() {
                                return Err(admission(WriteTxnError::AuthorizedMutationRefused));
                            }
                            SessionBound::Chain(Box::new(chain))
                        } else {
                            SessionBound::Program(super::bind_native_admitted(
                                cx,
                                &mut execution,
                                text,
                                params,
                                *relation,
                                capability.predicates(),
                                *max_statements,
                                super::BorrowedCatalog(resolver),
                            )?)
                        }
                    }
                    Request::TextBatch(text, arguments) => {
                        let first = arguments.first().ok_or(Fault::BatchBinding(
                            fgdb_gql::GraphWriteScriptBatchError::Empty,
                        ))?;
                        cx.with_restriction(|| {
                            super::reserve_text(cx, &mut execution, text)?;
                            if super::is_chain_admitted(cx, &mut execution, text)? {
                                return Err(admission(WriteTxnError::AuthorizedMutationRefused));
                            }
                            Ok(())
                        })?;
                        // Map arguments standing for property maps splice
                        // with the first set's keys, and every set must carry
                        // exactly those keys (fgdb-0s2pq). The original text
                        // pays the byte bill; each fresh key is a declaration.
                        let lowered = fgdb_gql::GqlMapArgumentText::lower(text, first);
                        let derived = match &lowered {
                            Some((recipe, _)) => Some(
                                (arguments.iter().enumerate())
                                    .map(|(set, arguments)| {
                                        recipe
                                            .arguments(arguments)
                                            .map_err(|error| super::map_argument_fault(set, &error))
                                    })
                                    .collect::<Result<Vec<_>, _>>()?,
                            ),
                            None => None,
                        };
                        let recipe = lowered.as_ref().map(|(recipe, _)| recipe);
                        let (statement, sets) = match (recipe, &derived) {
                            (Some(recipe), Some(sets)) => (recipe.text(), sets.as_slice()),
                            _ => (text, arguments),
                        };
                        let script = cx
                            .with_restriction(|| {
                                super::prepare_admitted(
                                    cx,
                                    &mut execution,
                                    statement,
                                    &sets[0],
                                    *relation,
                                    super::BorrowedCatalog(resolver),
                                )
                            })
                            .map_err(|fault| super::restored(recipe, fault))?;
                        let Bound::Batch(batch) = Input::Batch(&script, sets, *max_statements)
                            .bind(cx, capability.predicates(), &mut execution)
                            .map_err(|fault| super::restored(recipe, fault))?
                        else {
                            unreachable!("text batch input always returns an owned batch")
                        };
                        SessionBound::Program(Bound::Batch(batch))
                    }
                    Request::Prepared(prepared, params) => {
                        if !Arc::ptr_eq(owner, &prepared.owner) {
                            return Err(admission(WriteTxnError::AuthorizedMutationRefused));
                        }
                        match &prepared.definition {
                            PreparedDefinition::Script(script) => {
                                statement_limit(script, *max_statements)?;
                                SessionBound::Program(Input::Script(script, params).bind(
                                    cx,
                                    capability.predicates(),
                                    &mut execution,
                                )?)
                            }
                            PreparedDefinition::Chain(prepared) => {
                                super::chain_statement_limit(*max_statements)?;
                                let chain = prepared.bind(cx, &mut execution, params)?;
                                if chain.returns_rows() {
                                    return Err(admission(
                                        WriteTxnError::AuthorizedMutationRefused,
                                    ));
                                }
                                SessionBound::Chain(Box::new(chain))
                            }
                        }
                    }
                    Request::Batch(prepared, arguments) => {
                        if !Arc::ptr_eq(owner, &prepared.owner) {
                            return Err(admission(WriteTxnError::AuthorizedMutationRefused));
                        }
                        SessionBound::Program(
                            Input::Batch(prepared.batch_script()?, arguments, *max_statements)
                                .bind(cx, capability.predicates(), &mut execution)?,
                        )
                    }
                    Request::Bound(bound) => {
                        if !Arc::ptr_eq(owner, &bound.owner) {
                            return Err(admission(WriteTxnError::AuthorizedMutationRefused));
                        }
                        SessionBound::Program(Input::Bound(&bound.batch).bind(
                            cx,
                            capability.predicates(),
                            &mut execution,
                        )?)
                    }
                };
                // A second erased boundary under `run`'s (fgdb-a5y6m): one
                // `Send` proof from the session down to here, another from the
                // program completion down, so neither descends the whole chain.
                match bound {
                    SessionBound::Program(bound) => {
                        let completion: crate::SendFuture<'_, _> =
                            Box::pin(database.complete_authorized_program(
                                txn_cx,
                                cx,
                                commit_cx,
                                bound.program(),
                                *policy,
                                capability.predicates(),
                                &mut execution,
                                returning,
                                receipt,
                            ));
                        completion.await.map_err(|error| bound.error(error))
                    }
                    SessionBound::Chain(chain) => {
                        let mut result = None;
                        let completion: crate::SendFuture<'_, _> =
                            Box::pin(database.complete_authorized_chain(
                                txn_cx,
                                cx,
                                commit_cx,
                                &chain,
                                None,
                                policy.insertion_policy(),
                                capability.predicates(),
                                execution,
                                |stats, _, work| {
                                    // Chains publish only their aggregate effects;
                                    // private carried identities are not RETURN.
                                    // Construct even a stats receipt before commit.
                                    work(1)?;
                                    result = Some(receipt(stats, Vec::new()));
                                    Ok(())
                                },
                            ));
                        let (_, _, completion) = completion.await.map_err(Fault::Chain)?;
                        Ok((
                            result.expect("chain receipt was admitted before completion"),
                            completion,
                        ))
                    }
                }
                // Nothing fallible, no authorization sampling, no callback and no
                // receipt allocation after the existing completion boundary.
            })
            .await
    }
}

#[cfg(test)]
#[path = "write_session_tests.rs"]
mod tests;
