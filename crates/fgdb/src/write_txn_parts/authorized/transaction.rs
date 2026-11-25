//! Explicit capability-scoped ownership of the canonical native workspace.
//! The owner holds no database borrow between calls and never autocommits a
//! statement. Its live allowance, observations and unpublished effects survive
//! successful calls together; any refusal or unwind aborts the whole prefix.

use super::{
    Authority, CapabilityToken, CommitCx, Database, Error, Execution, TxnCx, Vfs, Workspace,
    WriteTxnError, chain,
};
use crate::write_txn::CompletionCheckpoint;
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertStats};
use fgdb_gql::{
    GlaLimitDimension, GlaLimitExceeded, GqlParameters, GqlQueryExecution, GqlQueryPolicy,
    GraphSavepointOperation, GraphWriteProgramStats, PreparedGraphSet,
    PreparedGraphTransactionControl, PreparedGraphWriteChain,
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
/// does not hold a database borrow or lock while idle. Each statement selects
/// the original retained basis plus earlier staged effects, even when another
/// writer has committed. COMMIT validates every intervening change through the
/// existing strict conflict validator; no statement is refreshed or replayed.
///
/// An older basis reconstructs resident writer/version state and copies snapshot
/// metadata. That reconstruction polls cancellation but is not charged as visible
/// graph work: hidden history cannot spend the signed allowance. It is not a
/// memory/spill bound or retention lease; unavailable history still refuses.
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
    executed_write: bool,
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
                executed_write: false,
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
    ) -> Result<bool, WriteTxnError> {
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
        database.snapshot.check_frontier(self.basis)?;
        Ok(live != self.basis)
    }

    /// Keep the owner terminal while admitting the definition and selecting
    /// semantic state. Setup can retain the LIVE allocator before historical
    /// writer selection, but cannot issue output. The typed scope adapters immediately consume the restored state.
    /// No publication-capable database borrow escapes the preparation scope.
    fn at_basis<V: Vfs + Clone, T, Setup>(
        &mut self,
        database: &mut Database<V>,
        query_cx: &QueryCx,
        setup: impl FnOnce(
            &mut Database<V>,
            &PlannerPredicates,
            &mut Execution<'_, '_, Clock>,
        ) -> Result<Setup, chain::Fault>,
        execute: impl FnOnce(
            &mut crate::prepared_write::PreparationBasis<'_, V>,
            &mut Self,
            Setup,
        ) -> Result<T, chain::Fault>,
    ) -> Result<T, chain::Fault> {
        let idle = self
            .state
            .take()
            .ok_or_else(|| chain::source(WriteTxnError::Finished))?;
        let commit_cx = self.cx.clone();
        let mut state = idle.enter(&commit_cx);
        commit_cx.with_restriction(|| {
            let advanced = self
                .admit_statement(&mut state, database, query_cx)
                .map_err(chain::source)?;
            let setup = setup(database, &self.scope, &mut state.execution)?;
            let selected = database.preparation_basis_controlled(self.basis, || {
                query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                state.execution.poll()
            });
            // History replay is private physical work. Check live authority
            // after it, once, rather than charging or sampling the clock once
            // per hidden row. The current-basis path keeps its existing bill.
            if advanced {
                query_cx
                    .checkpoint()
                    .map_err(WriteTxnError::Interrupted)
                    .map_err(chain::source)?;
                state.execution.checkpoint().map_err(chain::source)?;
            }
            let mut basis = selected.map_err(|error| {
                chain::source(match error {
                    error @ WriteTxnError::BasisRebuild(_) => super::redacted(error),
                    error => error,
                })
            })?;
            self.state = Some(IdleState::leave(state));
            execute(&mut basis, self, setup)
        })
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
        self.at_basis(
            database,
            query_cx,
            |database, scope, execution| {
                chain::admit_definition(statement, None, query_cx, scope, execution)?;
                database.engine_allocator(query_cx).map_err(chain::source)
            },
            |basis, owner, mut allocator| {
                basis.stage_authorized_chain(owner, query_cx, statement, &mut allocator, admit)
            },
        )
    }

    /// Synchronous entry used only by the admitted original-basis scope.
    pub(crate) fn stage_chain_selected<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        query_cx: &QueryCx,
        statement: &PreparedGraphWriteChain,
        allocator: chain::Allocator<'_>,
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
        let state = idle.enter(&self.cx);
        let policy = self.statement_policy();
        let (state, result) = self.cx.with_restriction(|| {
            let (mut state, result) = database.stage_authorized_chains(
                query_cx,
                core::slice::from_ref(statement),
                None,
                policy,
                &self.scope,
                allocator,
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
        self.executed_write = true;
        self.state = Some(IdleState::leave(state));
        Ok(result)
    }

    /// Apply a typed savepoint control to this same unpublished workspace.
    ///
    /// Only effects and the live savepoint stack rewind. Consumed native and
    /// signed allowances, statement ordinals, identity reservations and every
    /// read/conflict witness remain cumulative. Controls require the retained
    /// Write authority but do not require Read authority or create a write.
    ///
    /// The checked output is empty and provisional. An unknown name, missing
    /// retained basis, output refusal, expired authority or cancellation aborts the
    /// entire owner, exactly as for a failed graph statement.
    pub fn control<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        query_cx: &QueryCx,
        control: &PreparedGraphTransactionControl,
        admit: impl FnOnce(
            &[GraphValueRow],
            &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
            &mut dyn crate::AuthorizedWriteOutput,
        ) -> Result<(), WriteTxnError>,
    ) -> Result<(), chain::Fault> {
        self.at_basis(
            database,
            query_cx,
            |_, _, _| Ok(()),
            |basis, owner, ()| basis.control_authorized(owner, query_cx, control, admit),
        )
    }

    /// Synchronous entry used only by the admitted original-basis scope.
    pub(crate) fn control_selected<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        query_cx: &QueryCx,
        control: &PreparedGraphTransactionControl,
        admit: impl FnOnce(
            &[GraphValueRow],
            &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
            &mut dyn crate::AuthorizedWriteOutput,
        ) -> Result<(), WriteTxnError>,
    ) -> Result<(), chain::Fault> {
        let idle = self
            .state
            .take()
            .ok_or_else(|| chain::source(WriteTxnError::Finished))?;
        let mut state = idle.enter(&self.cx);
        let policy = self.statement_policy();
        self.cx.with_restriction(|| {
            let transaction = state.workspace.transaction();
            // Charge the bounded name/stack traversal before touching it.
            // Rollback also visits the discarded staging and retirement
            // journals. Neither this work nor retained scratch is refunded.
            let mut units = control.name().len() as u128 + 1;
            if control.operation() != GraphSavepointOperation::Save {
                units += transaction.savepoints.len() as u128 * 128;
            }
            if control.operation() == GraphSavepointOperation::RollbackTo {
                units += transaction.staged.len() as u128
                    + transaction
                        .chain_retirements
                        .as_ref()
                        .map_or(0, |retired| retired.journal.len()) as u128;
            }
            let work = u128::from(state.used.evaluator.work_units) + units;
            let scratch = u128::from(state.used.evaluator.scratch_entries)
                + u128::from(control.operation() == GraphSavepointOperation::Save);
            for (observed, limit, dimension) in [
                (
                    work,
                    policy.query.evaluator.max_work_units,
                    GlaLimitDimension::WorkUnits,
                ),
                (
                    scratch,
                    policy.query.evaluator.max_scratch_entries,
                    GlaLimitDimension::ScratchEntries,
                ),
            ] {
                if observed > u128::from(limit) {
                    return Err(chain::source(WriteTxnError::OutputLimit(
                        GlaLimitExceeded {
                            dimension,
                            limit,
                            observed,
                        },
                    )));
                }
            }
            state.execution.work(units as u64).map_err(chain::source)?;
            state.used.evaluator.work_units = work as u64;
            state.used.evaluator.scratch_entries = scratch as u64;
            match control.operation() {
                GraphSavepointOperation::Save => transaction.savepoint(database, control.name()),
                GraphSavepointOperation::RollbackTo => {
                    transaction.rollback_to_savepoint(database, control.name())
                }
                GraphSavepointOperation::Release => {
                    transaction.release_savepoint(database, control.name())
                }
            }
            .map_err(chain::source)?;
            let mut result = GqlQueryExecution {
                value: Vec::<GraphValueRow>::new(),
                rows: state.used.selection,
                evaluator: state.used.evaluator,
            };
            super::output::admit_output(
                transaction,
                database,
                query_cx,
                &self.scope,
                &mut state.execution,
                policy.query,
                &mut result,
                admit,
            )
            .map_err(chain::source)?;
            state.used.evaluator = result.evaluator;
            Self::finish_statement(&mut state, query_cx).map_err(chain::source)
        })?;
        self.completed_statements += 1;
        self.state = Some(IdleState::leave(state));
        Ok(())
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
        self.at_basis(
            database,
            query_cx,
            |_, scope, execution| {
                chain::admit_read_definition(statement, query_cx, scope, execution)
            },
            |basis, owner, ()| basis.read_authorized(owner, query_cx, statement, admit),
        )
    }

    /// Synchronous entry used only by the admitted original-basis scope.
    pub(crate) fn read_selected<V: Vfs + Clone>(
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
        let state = idle.enter(&self.cx);
        let policy = self.statement_policy();
        let (state, result) = self.cx.with_restriction(|| {
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
        self.at_basis(
            database,
            query_cx,
            |_, scope, execution| {
                chain::admit_read_definition(statement.input(), query_cx, scope, execution)
            },
            |basis, owner, ()| basis.read_authorized_aggregate(owner, query_cx, statement, admit),
        )
    }

    /// Synchronous entry used only by the admitted original-basis scope.
    pub(crate) fn read_aggregate_selected<V: Vfs + Clone>(
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
        let state = idle.enter(&self.cx);
        let policy = self.statement_policy();
        let (state, result) = self.cx.with_restriction(|| {
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
    /// transaction returns ReadClosed. Savepoint controls alone are read-only;
    /// rolling back successful graph writes does not erase their classification.
    /// Live authority and cancellation are checked by the native completion
    /// checkpoints before publication. Semantic conflict/unknown outcomes keep
    /// their native meaning; this method never retries or replays statements.
    /// Intervening history polls host cancellation without charging signed work
    /// or sampling the clock per hidden record. The original mutation footprint
    /// stays charged, and live authority is checked again after validation,
    /// before either its result or a protected error can escape.
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
                if self.executed_write {
                    state
                        .workspace
                        .transaction()
                        .retain_write_classification(database)?;
                }
                state.execution.bind_decision(state.workspace.transaction());
                state
                    .workspace
                    .transaction()
                    .complete_with_validation_controlled(
                        database,
                        &self.cx,
                        None,
                        false,
                        None,
                        |phase| match phase {
                            CompletionCheckpoint::Logical
                            | CompletionCheckpoint::ValidationComplete => {
                                state.execution.checkpoint()
                            }
                            CompletionCheckpoint::PrivateValidation => state.execution.poll(),
                        },
                    )
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
