//! Explicit capability-scoped ownership of the canonical native workspace.
//! The owner holds no database borrow between calls and never autocommits a
//! statement. Its live allowance, observations and unpublished effects survive
//! successful calls together; any refusal or unwind aborts the whole prefix.

use super::{
    Authority, CapabilityToken, CommitCx, Database, Error, Execution, TxnCx, Vfs, Workspace,
    WriteTxnError, chain,
};
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertStats};
use fgdb_gql::{
    GqlParameters, GqlQueryExecution, GqlQueryPolicy, GraphWriteProgramStats, PreparedGraphSet,
    PreparedGraphWriteChain,
};
use fgdb_types::{CommitSeq, EmbeddedTxnCompletion, QueryCx};
use fgdb_warden::{ExecutionPermit, PlannerPredicates, WriteAccess};

/// An idle execution owns its permit without borrowing its context. During a
/// call, the existing authorized executor borrows the owner's cloned CommitCx;
/// no self-reference or database guard is retained between statements.
struct IdleState<'issuer, Clock> {
    workspace: Workspace,
    permit: ExecutionPermit<'issuer, WriteAccess>,
    clock: Clock,
    used: GraphInsertStats,
    mutation_effects: u64,
}

impl<'issuer, Clock> IdleState<'issuer, Clock> {
    fn enter(self, cx: &CommitCx) -> chain::ChainState<'_, 'issuer, Clock> {
        chain::ChainState {
            workspace: self.workspace,
            execution: Execution {
                cx,
                permit: self.permit,
                clock: self.clock,
            },
            used: self.used,
            mutation_effects: self.mutation_effects,
        }
    }

    fn leave(state: chain::ChainState<'_, 'issuer, Clock>) -> Self {
        Self {
            workspace: state.workspace,
            permit: state.execution.permit,
            clock: state.execution.clock,
            used: state.used,
            mutation_effects: state.mutation_effects,
        }
    }
}

/// A movable, explicit native write transaction admitted by one capability.
///
/// Every statement sees the pinned graph plus this transaction's earlier
/// effects. Statement results remain provisional until `commit` succeeds.
/// Dropping or rolling back the owner discards all unpublished effects and
/// releases the native snapshot obligation. Any failed statement, checked
/// output, cancellation or unwind terminally aborts the whole transaction.
///
/// The trusted host supplies the same database handle for each call; the owner
/// does not hold a database borrow or lock while idle. A different handle or an
/// advanced write basis is refused before graph access. COMMIT uses the native
/// conflict validator and semantic completion, with no implicit replay.
///
/// This is local ownership of the existing embedded transaction. It does not
/// introduce durable ownership leases, reattachment or a separate commit lane.
#[must_use = "dropping an authorized write transaction rolls it back"]
pub struct AuthorizedWriteTransaction<'issuer, Clock> {
    state: Option<IdleState<'issuer, Clock>>,
    cx: CommitCx,
    scope: PlannerPredicates,
    policy: GraphInsertPolicy,
    basis: CommitSeq,
    max_statements: usize,
    completed_statements: usize,
    delivered_rows: u64,
}

impl<Clock> core::fmt::Debug for AuthorizedWriteTransaction<'_, Clock> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AuthorizedWriteTransaction")
            .field("active", &self.state.is_some())
            .field("basis", &self.basis)
            .field("completed_statements", &self.completed_statements)
            .finish_non_exhaustive()
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Begin one capability-scoped explicit transaction. Native policy, signed
    /// limits and `max_statements` apply cumulatively through the final commit.
    /// The context clone shares its cancellation and obligation tracker; no
    /// database or context borrow is retained by the returned owner.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_authorized_write<'issuer, Clock: FnMut() -> u64>(
        &mut self,
        txn_cx: &TxnCx,
        commit_cx: &CommitCx,
        authority: &'issuer Authority,
        token: &CapabilityToken,
        branch: &str,
        policy: GraphInsertPolicy,
        max_statements: usize,
        mut clock: Clock,
    ) -> Result<AuthorizedWriteTransaction<'issuer, Clock>, WriteTxnError> {
        if authority.namespace() != self.keys.namespace {
            return Err(WriteTxnError::Authorization(Error::WrongAuthority));
        }
        commit_cx.with_restriction(|| {
            commit_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
            let now = clock();
            let verified = authority
                .verify_at(token, branch, now)
                .map_err(WriteTxnError::Authorization)?;
            let scope = verified.predicates().clone();
            let permit = verified
                .into_write_at(branch, now)
                .map_err(WriteTxnError::Authorization)?;
            let cx = commit_cx.clone();
            let mut execution = Execution {
                cx: &cx,
                permit,
                clock,
            };
            execution.checkpoint()?;
            let transaction = self.begin(txn_cx).map_err(WriteTxnError::Write)?;
            let basis = transaction.basis();
            let state = IdleState::leave(chain::ChainState::new(transaction, execution));
            Ok(AuthorizedWriteTransaction {
                state: Some(state),
                cx,
                scope,
                policy,
                basis,
                max_statements,
                completed_statements: 0,
                delivered_rows: 0,
            })
        })
    }
}

impl<Clock: FnMut() -> u64> AuthorizedWriteTransaction<'_, Clock> {
    /// The original native snapshot basis; staging never changes it.
    #[must_use]
    pub const fn basis(&self) -> CommitSeq {
        self.basis
    }

    /// A failed statement permanently makes this false.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.state.is_some()
    }

    /// Cumulative successful statement costs and effects. Results remain
    /// provisional; these statistics are not a commit receipt.
    #[must_use]
    pub fn statistics(&self) -> Option<GraphWriteProgramStats> {
        let state = self.state.as_ref()?;
        let mut selection = state.used.selection;
        selection.result_rows = self.delivered_rows;
        Some(GraphWriteProgramStats {
            completed_statements: self.completed_statements,
            selection,
            evaluator: state.used.evaluator,
            target_vertex_visits: 0,
            target_edge_visits: 0,
            mutation_effects: state.mutation_effects,
            created_vertices: state.used.created_vertices,
            created_edges: state.used.created_edges,
        })
    }

    fn statement_policy(&self) -> GraphInsertPolicy {
        let mut policy = self.policy;
        policy.query = GqlQueryPolicy::new(
            policy.query.rows.max_snapshot_records().unwrap_or(u64::MAX),
            policy
                .query
                .rows
                .max_result_rows()
                .unwrap_or(u64::MAX)
                .saturating_sub(self.delivered_rows),
            policy.query.evaluator.max_work_units,
            policy.query.evaluator.max_scratch_entries,
        );
        policy
    }

    fn admit_statement<V: Vfs + Clone>(
        &self,
        state: &mut chain::ChainState<'_, '_, Clock>,
        database: &Database<V>,
        query_cx: &QueryCx,
    ) -> Result<(), WriteTxnError> {
        // Ownership precedes even the foreign handle's health/frontier check.
        state.workspace.transaction().ensure_database(database)?;
        query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
        state.execution.checkpoint()?;
        if self.completed_statements >= self.max_statements {
            return Err(WriteTxnError::StatementLimit {
                limit: self.max_statements,
            });
        }
        let live = database.frontier().map_err(WriteTxnError::from)?;
        if live != self.basis {
            return Err(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live,
            });
        }
        Ok(())
    }

    /// Explicit statements return before the final commit checkpoints. Check
    /// again after the host's fallible output admission, while this call still
    /// owns the workspace, so expiry, retirement or cancellation during
    /// hydration cannot release rows or restore an active transaction.
    fn finish_statement(
        state: &mut chain::ChainState<'_, '_, Clock>,
        query_cx: &QueryCx,
    ) -> Result<(), WriteTxnError> {
        query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
        state.execution.checkpoint()
    }

    /// Prepare one statement under this transaction's retained allowance.
    ///
    /// The trusted host supplies the compiler and capability-safe catalog, as
    /// it does for native authorized autocommit. Original text, canonical
    /// argument bytes and declarations are charged before compilation. The
    /// callback charges additional catalog/binding work through the meter. It receives
    /// the original non-widenable scope and parameters with a fresh observation
    /// of the database's trusted statement clock, never a client-supplied one.
    /// No prepared definition grants authority: `stage_chain` and `read` still
    /// admit the complete definition before observing or changing graph state.
    ///
    /// Preparation does not count as a successfully executed statement. Any
    /// failure, swallowed meter refusal, cancellation or unwind nevertheless
    /// aborts the whole unpublished prefix. A host may retain its own detailed
    /// compiler diagnostic and return `AuthorizedMutationRefused` here; live
    /// authority and cancellation are checked before that diagnostic escapes.
    /// The meter retains its first refusal and returns a terminal marker to
    /// the callback, so a compiler's optional resolver protocol cannot replace
    /// an authorization failure with an unknown-symbol diagnostic.
    pub fn prepare_statement<V: Vfs + Clone, T>(
        &mut self,
        database: &Database<V>,
        query_cx: &QueryCx,
        text: &str,
        parameters: &GqlParameters,
        prepare: impl FnOnce(
            &GqlParameters,
            &PlannerPredicates,
            &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
        ) -> Result<T, WriteTxnError>,
    ) -> Result<T, WriteTxnError> {
        let idle = self.state.take().ok_or(WriteTxnError::Finished)?;
        let mut state = idle.enter(&self.cx);
        let prepared = self.cx.with_restriction(|| {
            self.admit_statement(&mut state, database, query_cx)?;
            state
                .execution
                .admit_input(query_cx, text, std::slice::from_ref(parameters))?;
            if fgdb_gql::reads_element_identity(text) {
                return Err(WriteTxnError::Authorization(Error::ScopeDenied));
            }
            // Calendar observation precedes the pure-query restriction, as it
            // does in native authorized autocommit. The retained permit has
            // already authorized this statement, and is checked again after
            // the host clock callback before the compiler sees its result.
            let captured = crate::query::capture_authorized_statement_parameters(
                database.statement_clock.as_ref(),
                query_cx,
                fgdb_gql::statement_time_required(text),
                parameters,
            );
            state.execution.checkpoint()?;
            let captured = captured.map_err(|error| match error {
                crate::StatementTimeError::Cancelled(error) => WriteTxnError::Interrupted(error),
                error => WriteTxnError::StatementTime(error),
            })?;
            query_cx.with_restriction(|| {
                for _ in captured.parameter_types() {
                    query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                    state.execution.checkpoint()?;
                }
                let mut failure = None;
                let result = prepare(&captured, &self.scope, &mut |units| {
                    if failure.is_some() {
                        return Err(WriteTxnError::AuthorizedMutationRefused);
                    }
                    query_cx
                        .checkpoint()
                        .map_err(WriteTxnError::Interrupted)
                        .and_then(|()| state.execution.work(units))
                        .map_err(|error| {
                            failure = Some(error);
                            WriteTxnError::AuthorizedMutationRefused
                        })
                });
                if let Some(error) = failure {
                    return Err(error);
                }
                query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                state.execution.checkpoint()?;
                result
            })
        })?;
        self.state = Some(IdleState::leave(state));
        Ok(prepared)
    }

    /// Stage a prepared native statement, including ordinary single CREATE,
    /// SET, DELETE and MERGE or a chain with WITH/read phases. The compiler
    /// entrypoint is `PreparedGraphWriteChainText::prepare_statement` followed by binding;
    /// no `is_chain` routing predicate is needed for this explicit API.
    ///
    /// The checked callback may hydrate graph values through the same private
    /// overlay. Its output is transaction-local and must not be represented as
    /// committed. A failed callback or swallowed meter/metadata failure aborts
    /// every earlier statement. No publication occurs in this method.
    pub fn stage_chain<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        query_cx: &QueryCx,
        statement: &PreparedGraphWriteChain,
        admit: impl FnOnce(
            &[GraphValueRow],
            &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
            &mut dyn crate::AuthorizedWriteOutput,
        ) -> Result<(), WriteTxnError>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, chain::Fault> {
        // Leave the owner terminal until the entire call has succeeded.
        let idle = self
            .state
            .take()
            .ok_or_else(|| chain::source(WriteTxnError::Finished))?;
        let mut state = idle.enter(&self.cx);
        let policy = self.statement_policy();
        let (state, result) = self.cx.with_restriction(|| {
            self.admit_statement(&mut state, database, query_cx)
                .map_err(chain::source)?;
            chain::admit_definition(statement, None, query_cx, &self.scope, &mut state.execution)?;
            let (mut state, result) = database.stage_authorized_chains(
                query_cx,
                core::slice::from_ref(statement),
                None,
                policy,
                &self.scope,
                state,
                &mut None,
                |_, rows, work, output| admit(rows, work, output),
            )?;
            Self::finish_statement(&mut state, query_cx).map_err(chain::source)?;
            state
                .workspace
                .transaction()
                .retain_write_classification(database)
                .map_err(chain::source)?;
            Ok::<_, chain::Fault>((state, result))
        })?;
        self.delivered_rows = self.delivered_rows.saturating_add(result.rows.result_rows);
        self.completed_statements += 1;
        self.state = Some(IdleState::leave(state));
        Ok(result)
    }

    /// Execute a standalone prepared read over the same authorized transaction
    /// overlay. Read authority is required even for a source-free projection.
    /// Its rows, source visits, evaluator work and hydrated output spend the
    /// same cumulative allowance as all preceding writes.
    pub fn read<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        query_cx: &QueryCx,
        statement: &PreparedGraphSet,
        admit: impl FnOnce(
            &[GraphValueRow],
            &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
            &mut dyn crate::AuthorizedWriteOutput,
        ) -> Result<(), WriteTxnError>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, chain::Fault> {
        let idle = self
            .state
            .take()
            .ok_or_else(|| chain::source(WriteTxnError::Finished))?;
        let mut state = idle.enter(&self.cx);
        let policy = self.statement_policy();
        let (state, result) = self.cx.with_restriction(|| {
            self.admit_statement(&mut state, database, query_cx)
                .map_err(chain::source)?;
            let (mut state, result) = database.read_authorized_chain_state(
                query_cx,
                statement,
                policy,
                &self.scope,
                state,
                admit,
            )?;
            Self::finish_statement(&mut state, query_cx).map_err(chain::source)?;
            Ok::<_, chain::Fault>((state, result))
        })?;
        self.delivered_rows = self.delivered_rows.saturating_add(result.rows.result_rows);
        self.completed_statements += 1;
        self.state = Some(IdleState::leave(state));
        Ok(result)
    }

    /// Execute an exact terminal aggregate over the authorized staged graph.
    /// The source-aware definition retains every pattern, row stage and group
    /// clause. Counts, wide sums and rational means are returned in their
    /// native exact domains; the checked callback may hydrate collected graph
    /// values through the same private overlay as ordinary statement output.
    /// All source, evaluator, output and statement allowances remain cumulative.
    pub fn read_aggregate<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        query_cx: &QueryCx,
        statement: &fgdb_gql::PreparedGraphSetAggregate,
        admit: impl FnOnce(
            &[fgdb_gql::GraphAggregateRow],
            &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
            &mut dyn crate::AuthorizedWriteOutput,
        ) -> Result<(), WriteTxnError>,
    ) -> Result<GqlQueryExecution<fgdb_gql::GraphAggregateRow>, chain::Fault> {
        let idle = self
            .state
            .take()
            .ok_or_else(|| chain::source(WriteTxnError::Finished))?;
        let mut state = idle.enter(&self.cx);
        let policy = self.statement_policy();
        let (state, result) = self.cx.with_restriction(|| {
            self.admit_statement(&mut state, database, query_cx)
                .map_err(chain::source)?;
            let (mut state, result) = database.read_authorized_chain_aggregate_state(
                query_cx,
                statement,
                policy,
                &self.scope,
                state,
                admit,
            )?;
            Self::finish_statement(&mut state, query_cx).map_err(chain::source)?;
            Ok::<_, chain::Fault>((state, result))
        })?;
        self.delivered_rows = self.delivered_rows.saturating_add(result.rows.result_rows);
        self.completed_statements += 1;
        self.state = Some(IdleState::leave(state));
        Ok(result)
    }

    /// Publish once through the canonical native completion owner.
    /// A successful write statement takes the write terminal path even if it
    /// matched no rows or produced no effects. Only an exclusively read-only
    /// transaction returns ReadClosed.
    /// Live authority and cancellation are checked by the native completion
    /// checkpoints before publication. Semantic conflict/unknown outcomes keep
    /// their native meaning; this method never retries or replays statements.
    pub async fn commit<V: Vfs + Clone>(
        mut self,
        database: &mut Database<V>,
    ) -> Result<EmbeddedTxnCompletion, WriteTxnError> {
        let idle = self.state.take().ok_or(WriteTxnError::Finished)?;
        let mut state = idle.enter(&self.cx);
        self.cx
            .with_restriction_async(async {
                state.workspace.transaction().ensure_database(database)?;
                state.execution.checkpoint()?;
                state.execution.bind_decision(state.workspace.transaction());
                state
                    .workspace
                    .transaction()
                    .complete_controlled(database, &self.cx, None, false, || {
                        state.execution.checkpoint()
                    })
                    .await
            })
            .await
    }

    /// Explicitly discard the unpublished prefix and release its snapshot pin.
    /// Dropping the owner has exactly the same native rollback behavior.
    pub fn rollback(mut self) {
        drop(self.state.take());
    }
}
