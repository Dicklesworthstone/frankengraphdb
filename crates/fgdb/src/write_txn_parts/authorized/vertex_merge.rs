//! Authorized unique-vertex MERGE and its selected ON MATCH/ON CREATE actions.
//! Reuse the native reducer, point projection and checked scalar-action collector.

use crate::write_txn::authorized::{
    Authority, CapabilityToken, Database, Error, Execution, Vfs, Workspace, WriteBatch, WriteTxn,
    WriteTxnError, selection, stage,
};
use crate::write_txn::{
    collect_vertex_merge, vertex_replacement_keys, vertex_upsert_actions,
    vertex_upsert_known_effects, vertex_upsert_labels, vertex_upsert_properties,
    vertex_upsert_query_selection_policy, vertex_upsert_returning,
};
use fgdb_delta_types::ElementId;
use fgdb_gql::insertion::GraphInsertIntent;
use fgdb_gql::{
    GqlQueryError, GraphVertexMergeError, GraphVertexMergeOutcome, GraphVertexMergePolicy,
    GraphVertexMergeStats, GraphVertexUpsertError, GraphVertexUpsertPolicy, GraphVertexUpsertStats,
    PreparedGraphVertexMerge, PreparedGraphVertexUpsert, PreparedGraphVertexUpsertQuery,
};
use fgdb_types::{CommitCx, EmbeddedTxnCompletion, QueryCx, TxnCx};
use fgdb_warden::PlannerPredicates;
use std::cell::RefCell;

type MergeFault = GqlQueryError<GraphVertexMergeError<WriteTxnError, WriteTxnError>, WriteTxnError>;
type UpsertFault =
    GqlQueryError<GraphVertexUpsertError<WriteTxnError, WriteTxnError>, WriteTxnError>;

impl<V: Vfs + Clone> Database<V> {
    /// MERGE, its ON MATCH/ON CREATE and trailing SET clauses, and RETURN
    /// under one ReadWrite permit and one native transaction completion.
    ///
    /// The authorized reducer chooses one visible vertex exactly once. Every
    /// original write passes before/after image authorization; RETURN then
    /// reads the chosen vertex from that same private overlay, after all clauses.
    /// Hidden scalar fields yield NULL before lookup, source admission or
    /// field-witness creation. Whole maps omit hidden fields before naming,
    /// payload inspection or charges, and conservatively witness the selected
    /// vertex including absent keys. labels() omits hidden labels the same
    /// way, under the same witness. No projection rematches the graph.
    ///
    /// The native evaluator allowance spans actions and projection. Only final
    /// projected rows consume the signed result allowance, so LIMIT 0 still
    /// commits effects. Any expression, quota, authority or cancellation failure
    /// discards all private effects. Native committed/unknown/recovery outcomes
    /// remain unchanged; no fallible query or permission check follows completion.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_vertex_upsert_query_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        query: &PreparedGraphVertexUpsertQuery,
        policy: GraphVertexUpsertPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<
        (
            GraphVertexUpsertStats,
            GraphVertexMergeOutcome,
            fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
            EmbeddedTxnCompletion,
        ),
        UpsertFault,
    > {
        self.execute_graph_vertex_upsert_query_authorized_checked(
            txn_cx,
            query_cx,
            commit_cx,
            authority,
            token,
            branch,
            query,
            policy,
            clock,
            |_, _| Ok(()),
        )
        .await
    }

    /// The same atomic upsert with a trusted host's final result admission.
    ///
    /// `admit` runs exactly once, including for zero rows, after native result
    /// and signed row admission but BEFORE completion. It may build a private
    /// wire representation or refuse one that the host cannot deliver. An error
    /// discards the workspace. The callback must not publish rows or perform
    /// external effects: even accepted results are tentative until completion.
    /// It has no database/transaction access and cannot replace the native rows.
    ///
    /// The supplied meter polls cancellation and charges additional work to the
    /// SAME signed write permit, including expiry and issuer retirement. Hosts
    /// must meter their bounded traversal/encoding work before doing it; no new
    /// allowance is issued. The ordinary method uses a no-op admission and keeps
    /// its existing charges. No callback runs after publication.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_vertex_upsert_query_authorized_checked(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        query: &PreparedGraphVertexUpsertQuery,
        policy: GraphVertexUpsertPolicy,
        mut clock: impl FnMut() -> u64,
        admit: impl FnOnce(
            &[fgdb_gql::algebra::GraphValueRow],
            &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
        ) -> Result<(), WriteTxnError>,
    ) -> Result<
        (
            GraphVertexUpsertStats,
            GraphVertexMergeOutcome,
            fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
            EmbeddedTxnCompletion,
        ),
        UpsertFault,
    > {
        let source = |error| GqlQueryError::Source(GraphVertexUpsertError::Staging(error));
        if authority.namespace() != self.keys.namespace {
            return Err(source(WriteTxnError::Authorization(Error::WrongAuthority)));
        }
        let now = clock();
        let verified = authority
            .verify_at(token, branch, now)
            .map_err(|error| source(WriteTxnError::Authorization(error)))?;
        let permit = verified
            .begin_write_at(branch, now)
            .map_err(|error| source(WriteTxnError::Authorization(error)))?;
        if !verified.predicates().rights().can_read() {
            return Err(source(WriteTxnError::Authorization(
                Error::PermissionDenied,
            )));
        }
        commit_cx
            .with_restriction_async(async {
                let mut execution = Execution {
                    cx: commit_cx,
                    permit,
                    clock,
                };
                execution.checkpoint().map_err(source)?;
                let mut workspace = Workspace(Some(
                    self.begin(txn_cx)
                        .map_err(|error| source(WriteTxnError::Write(error)))?,
                ));
                let (stats, outcome) = upsert(
                    workspace.transaction(),
                    self,
                    query_cx,
                    query.upsert(),
                    vertex_upsert_query_selection_policy(policy),
                    verified.predicates(),
                    &mut execution,
                    false,
                )?;
                let result = query_cx.with_restriction(|| {
                    let controls = RefCell::new(&mut execution);
                    let transaction = &*workspace.transaction();
                    vertex_upsert_returning(
                        query,
                        stats,
                        outcome,
                        policy,
                        || {
                            query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                            controls.borrow_mut().checkpoint()
                        },
                        |key, control| {
                            if !verified.predicates().allows_property(key) {
                                control(fgdb_gql::GlaExecutionEvent::ScratchEntry)?;
                                return Ok(fgdb_gql::algebra::GraphValue::Scalar(
                                    fgdb_types::CanonicalScalar::Null,
                                ));
                            }
                            selection::property(
                                transaction,
                                self,
                                ElementId::Vertex(outcome.vertex()),
                                key,
                                verified.predicates(),
                                || {
                                    query_cx
                                        .checkpoint()
                                        .map_err(WriteTxnError::Interrupted)
                                        .map_err(GqlQueryError::Interrupted)?;
                                    controls
                                        .borrow_mut()
                                        .poll()
                                        .map_err(GqlQueryError::Interrupted)
                                },
                                control,
                                source,
                            )
                            .and_then(|value| {
                                // A stored List or Map returns as itself (fgdb-k3nub).
                                fgdb_gql::algebra::GraphValue::from_property_value(&value).ok_or(
                                    GqlQueryError::Data(fgdb_gql::GraphIntegerError {
                                        instruction: 0,
                                        kind: fgdb_gql::GraphIntegerErrorKind::StoredValueBounds,
                                    }),
                                )
                            })
                        },
                        |control| {
                            vertex_upsert_properties(
                                transaction,
                                self,
                                outcome.vertex(),
                                query.property_catalog(),
                                |key| verified.predicates().allows_property(key),
                                || {
                                    query_cx
                                        .checkpoint()
                                        .map_err(WriteTxnError::Interrupted)
                                        .map_err(GqlQueryError::Interrupted)?;
                                    controls
                                        .borrow_mut()
                                        .poll()
                                        .map_err(GqlQueryError::Interrupted)
                                },
                                control,
                            )
                        },
                        |control| {
                            vertex_upsert_labels(
                                transaction,
                                self,
                                outcome.vertex(),
                                query.property_catalog(),
                                |label| verified.predicates().allows_label(label),
                                || {
                                    query_cx
                                        .checkpoint()
                                        .map_err(WriteTxnError::Interrupted)
                                        .map_err(GqlQueryError::Interrupted)?;
                                    controls
                                        .borrow_mut()
                                        .poll()
                                        .map_err(GqlQueryError::Interrupted)
                                },
                                control,
                            )
                        },
                        selection::returning_element_reader(
                            transaction,
                            self,
                            query_cx,
                            verified.predicates(),
                            &controls,
                        ),
                    )
                })?;
                let rows = u64::try_from(result.value.len())
                    .map_err(|_| source(WriteTxnError::Authorization(Error::TooLarge)))?;
                execution
                    .permit
                    .charge_rows_at((execution.clock)(), rows)
                    .map_err(|error| source(WriteTxnError::Authorization(error)))?;
                query_cx
                    .with_restriction(|| {
                        admit(&result.value, &mut |work| {
                            query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                            execution.work(work)
                        })
                    })
                    .map_err(source)?;
                execution.bind_decision(workspace.transaction());
                let completion = workspace
                    .transaction()
                    .complete_controlled(self, commit_cx, None, false, || {
                        query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        execution.checkpoint()
                    })
                    .await
                    .map_err(source)?;
                Ok((stats, outcome, result, completion))
            })
            .await
    }
}

// Both public program entry points verify ReadWrite rights before opening the
// private workspace. No internal proposal, match or permit escapes these steps.
#[allow(clippy::too_many_arguments)]
pub(super) fn merge<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    merge: &PreparedGraphVertexMerge,
    policy: GraphVertexMergePolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    returning: bool,
) -> Result<(GraphVertexMergeStats, GraphVertexMergeOutcome), MergeFault> {
    let proposal = cx.with_restriction(|| {
        // Selection and allocation share the native owner sequentially. The
        // common collector produces an internal proposal, never a receipt or
        // an independently staged/committed creation.
        let database = RefCell::new(&mut *database);
        let controls = RefCell::new(&mut *execution);
        collect_vertex_merge(
            merge,
            policy,
            |pattern, allowance| {
                selection::select_overlay(
                    transaction,
                    &database.borrow(),
                    cx,
                    pattern,
                    scope,
                    allowance,
                    &controls,
                )
            },
            |request| database.borrow_mut().allocate_identity(cx, request),
            || {
                cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                controls.borrow_mut().checkpoint()
            },
        )
    })?;
    if let Some(creation) = proposal.creation {
        for intent in creation.into_intents() {
            cx.checkpoint()
                .map_err(WriteTxnError::Interrupted)
                .map_err(|error| GqlQueryError::Source(GraphVertexMergeError::Source(error)))?;
            execution
                .checkpoint()
                .map_err(|error| GqlQueryError::Source(GraphVertexMergeError::Source(error)))?;
            let GraphInsertIntent::Vertex {
                vertex,
                labels,
                properties,
            } = intent
            else {
                unreachable!("validated vertex MERGE contains no edge creation")
            };
            let mut batch = WriteBatch::new(merge.relation());
            batch.create_vertex_values(vertex, labels, properties);
            for row in batch.rows {
                stage(transaction, database, batch.relation, row, execution)
                    .map_err(|error| GqlQueryError::Source(GraphVertexMergeError::Source(error)))?;
            }
        }
    }
    if returning {
        // The fixed-size private creation identity was NOT a delivered row.
        // Both Created and Matched now pay exactly one final receipt unit.
        deliver(execution)
            .map_err(|error| GqlQueryError::Source(GraphVertexMergeError::Source(error)))?;
    }
    Ok((proposal.stats, proposal.outcome))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn upsert<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    upsert: &PreparedGraphVertexUpsert,
    policy: GraphVertexUpsertPolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    returning: bool,
) -> Result<(GraphVertexUpsertStats, GraphVertexMergeOutcome), UpsertFault> {
    let (merge_stats, outcome) = merge(
        transaction,
        database,
        cx,
        upsert.merge(),
        policy.merge,
        scope,
        execution,
        false,
    )
    .map_err(|error| error.map_source(GraphVertexUpsertError::Merge))?;
    let stats = cx.with_restriction(|| {
        let state = RefCell::new((&mut *transaction, &mut *database));
        let controls = RefCell::new(&mut *execution);
        vertex_upsert_actions(
            upsert,
            policy,
            merge_stats,
            outcome,
            (0, 0),
            || {
                cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                controls.borrow_mut().checkpoint()
            },
            |key, control| {
                // Mask BEFORE lookup, not after reading a protected payload.
                // A hidden field always supplies NULL, with no data-dependent
                // source work, size admission, error or conflict observation.
                if !scope.allows_property(key) {
                    control(fgdb_gql::GlaExecutionEvent::ScratchEntry)?;
                    return Ok(fgdb_types::CanonicalScalar::Null);
                }
                let state = state.borrow();
                selection::property(
                    state.0,
                    state.1,
                    ElementId::Vertex(outcome.vertex()),
                    key,
                    scope,
                    || {
                        cx.checkpoint()
                            .map_err(WriteTxnError::Interrupted)
                            .map_err(GqlQueryError::Interrupted)?;
                        controls
                            .borrow_mut()
                            .poll()
                            .map_err(GqlQueryError::Interrupted)
                    },
                    control,
                    |error| GqlQueryError::Source(GraphVertexUpsertError::Staging(error)),
                )
                .and_then(selection::scalar)
            },
            |control| {
                let state = state.borrow();
                vertex_replacement_keys(
                    state.0,
                    state.1,
                    outcome.vertex(),
                    Some(scope),
                    || {
                        cx.checkpoint()
                            .map_err(WriteTxnError::Interrupted)
                            .map_err(GqlQueryError::Interrupted)?;
                        controls
                            .borrow_mut()
                            .poll()
                            .map_err(GqlQueryError::Interrupted)
                    },
                    control,
                    |error| GqlQueryError::Source(GraphVertexUpsertError::Staging(error)),
                )
            },
            |batch| {
                let mut state = state.borrow_mut();
                let (transaction, database) = &mut *state;
                let mut execution = controls.borrow_mut();
                for row in batch.rows {
                    cx.checkpoint()
                        .map_err(WriteTxnError::Interrupted)
                        .map_err(|error| {
                            GqlQueryError::Source(GraphVertexUpsertError::Staging(error))
                        })?;
                    execution.checkpoint().map_err(|error| {
                        GqlQueryError::Source(GraphVertexUpsertError::Staging(error))
                    })?;
                    // Authorize EVERY original field in EVERY clause, even a
                    // no-op or overwritten value. A scope escape refuses before
                    // a later clause can read that vertex or erase the attempt.
                    stage(transaction, database, batch.relation, row, &mut execution).map_err(
                        |error| GqlQueryError::Source(GraphVertexUpsertError::Staging(error)),
                    )?;
                }
                Ok(())
            },
        )
    })?;
    if returning {
        deliver(execution)
            .map_err(|error| GqlQueryError::Source(GraphVertexUpsertError::Staging(error)))?;
    }
    Ok((stats, outcome))
}

fn deliver<Clock: FnMut() -> u64>(
    execution: &mut Execution<'_, '_, Clock>,
) -> Result<(), WriteTxnError> {
    execution.checkpoint()?;
    execution
        .permit
        .charge_rows_at((execution.clock)(), 1)
        .map_err(WriteTxnError::Authorization)
}

// An all-target host for the same native MERGE and action collectors. Keep the
// explicit singular APIs above: callers that require uniqueness still get it.
mod all_targets {
    use super::*;
    use crate::write_txn::{VertexMergeTargets, collect_vertex_merge_output};
    use fgdb_gql::algebra::{GraphValue, GraphValueRow};
    use fgdb_gql::{
        GlaExecutionEvent, GlaExecutionStats, GlaLimitDimension, GlaLimitExceeded,
        GqlQueryExecution, GqlQueryPolicy, GraphVertexReturnBinding, GraphVertexUpsertBranch,
    };

    type Receipt = (
        GraphVertexUpsertStats,
        GqlQueryExecution<GraphValueRow>,
        EmbeddedTxnCompletion,
    );

    fn staging(error: WriteTxnError) -> UpsertFault {
        GqlQueryError::Source(GraphVertexUpsertError::Staging(error))
    }

    // Ownership of each target and final row is work even when there are no
    // SET clauses. Native and signed counters are cumulative across the batch.
    fn event<Clock: FnMut() -> u64>(
        cx: &QueryCx,
        execution: &mut Execution<'_, '_, Clock>,
        policy: GqlQueryPolicy,
        stats: &mut GlaExecutionStats,
        event: GlaExecutionEvent,
    ) -> Result<(), UpsertFault> {
        cx.checkpoint()
            .map_err(WriteTxnError::Interrupted)
            .map_err(GqlQueryError::Interrupted)?;
        execution.checkpoint().map_err(GqlQueryError::Interrupted)?;
        let work = u128::from(stats.work_units) + 1;
        let scratch = u128::from(stats.scratch_entries)
            + u128::from(event == GlaExecutionEvent::ScratchEntry);
        for (observed, limit, dimension) in [
            (
                work,
                policy.evaluator.max_work_units,
                GlaLimitDimension::WorkUnits,
            ),
            (
                scratch,
                policy.evaluator.max_scratch_entries,
                GlaLimitDimension::ScratchEntries,
            ),
        ] {
            if observed > u128::from(limit) {
                return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                    dimension,
                    limit,
                    observed,
                }));
            }
        }
        stats.work_units = work as u64;
        stats.scratch_entries = scratch as u64;
        Ok(())
    }

    impl<V: Vfs + Clone> Database<V> {
        /// Execute every visible vertex matching MERGE under one capability.
        /// Selection runs once, before any ON MATCH/ON CREATE or trailing SET.
        /// Only an empty visible selection creates a vertex. Hidden matches do
        /// not suppress creation or enter the target set or observable budgets.
        ///
        /// Every target uses the existing native action evaluator and original
        /// before/after image authorization. One private workspace covers all
        /// effects and final RETURN/WITH processing. Pagination and filtering
        /// never limit which targets receive actions. Only final projected rows
        /// spend the signed row grant; native work, scratch and action limits
        /// cover the complete statement rather than restarting per target.
        ///
        /// This is the plural counterpart of the explicit unique upsert API,
        /// not a uniqueness constraint, relational MERGE input, or full SSI.
        #[allow(clippy::too_many_arguments)]
        pub async fn execute_graph_vertex_upserts_query_authorized(
            &mut self,
            txn_cx: &TxnCx,
            query_cx: &QueryCx,
            commit_cx: &CommitCx,
            authority: &Authority,
            token: &CapabilityToken,
            branch: &str,
            query: &PreparedGraphVertexUpsertQuery,
            policy: GraphVertexUpsertPolicy,
            clock: impl FnMut() -> u64,
        ) -> Result<Receipt, UpsertFault> {
            self.execute_graph_vertex_upserts_query_authorized_checked(
                txn_cx,
                query_cx,
                commit_cx,
                authority,
                token,
                branch,
                query,
                policy,
                clock,
                |_, _| Ok(()),
            )
            .await
        }

        /// Admit the complete final result before the sole native completion.
        /// The trusted callback runs once, including for LIMIT 0, and must keep
        /// its result private. Its meter uses the same live signed write permit.
        /// Refusal, expiry, retirement or unwind discards all private effects.
        /// No fallible host processing follows committed/unknown completion.
        #[allow(clippy::too_many_arguments)]
        pub async fn execute_graph_vertex_upserts_query_authorized_checked(
            &mut self,
            txn_cx: &TxnCx,
            query_cx: &QueryCx,
            commit_cx: &CommitCx,
            authority: &Authority,
            token: &CapabilityToken,
            branch: &str,
            query: &PreparedGraphVertexUpsertQuery,
            policy: GraphVertexUpsertPolicy,
            mut clock: impl FnMut() -> u64,
            admit: impl FnOnce(
                &[GraphValueRow],
                &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
            ) -> Result<(), WriteTxnError>,
        ) -> Result<Receipt, UpsertFault> {
            let refusal = |error| staging(WriteTxnError::Authorization(error));
            if authority.namespace() != self.keys.namespace {
                return Err(refusal(Error::WrongAuthority));
            }
            let now = clock();
            let verified = authority.verify_at(token, branch, now).map_err(refusal)?;
            let permit = verified.begin_write_at(branch, now).map_err(refusal)?;
            if !verified.predicates().rights().can_read() {
                return Err(refusal(Error::PermissionDenied));
            }
            commit_cx
                .with_restriction_async(async {
                    let mut execution = Execution {
                        cx: commit_cx,
                        permit,
                        clock,
                    };
                    execution.checkpoint().map_err(staging)?;
                    let mut workspace = Workspace(Some(
                        self.begin(txn_cx)
                            .map_err(WriteTxnError::Write)
                            .map_err(staging)?,
                    ));
                    let (stats, result) = query_cx.with_restriction(|| {
                        apply_all(
                            workspace.transaction(),
                            self,
                            query_cx,
                            query,
                            policy,
                            verified.predicates(),
                            &mut execution,
                        )
                    })?;
                    let rows =
                        u64::try_from(result.value.len()).map_err(|_| refusal(Error::TooLarge))?;
                    execution
                        .permit
                        .charge_rows_at((execution.clock)(), rows)
                        .map_err(refusal)?;
                    query_cx
                        .with_restriction(|| {
                            admit(&result.value, &mut |work| {
                                query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                                execution.work(work)
                            })
                        })
                        .map_err(staging)?;
                    execution.bind_decision(workspace.transaction());
                    let completion = workspace
                        .transaction()
                        .complete_controlled(self, commit_cx, None, false, || {
                            query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                            execution.checkpoint()
                        })
                        .await
                        .map_err(staging)?;
                    Ok((stats, result, completion))
                })
                .await
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_all<V: Vfs + Clone, Clock: FnMut() -> u64>(
        transaction: &mut WriteTxn,
        database: &mut Database<V>,
        cx: &QueryCx,
        query: &PreparedGraphVertexUpsertQuery,
        policy: GraphVertexUpsertPolicy,
        scope: &PlannerPredicates,
        execution: &mut Execution<'_, '_, Clock>,
    ) -> Result<(GraphVertexUpsertStats, GqlQueryExecution<GraphValueRow>), UpsertFault> {
        let upsert = query.upsert();
        let proposal = {
            let database = RefCell::new(&mut *database);
            let controls = RefCell::new(&mut *execution);
            collect_vertex_merge_output::<VertexMergeTargets, WriteTxnError, WriteTxnError, _>(
                upsert.merge(),
                vertex_upsert_query_selection_policy(policy).merge,
                |pattern, allowance| {
                    selection::select_overlay(
                        transaction,
                        &database.borrow(),
                        cx,
                        pattern,
                        scope,
                        allowance,
                        &controls,
                    )
                },
                |request| database.borrow_mut().allocate_identity(cx, request),
                || {
                    cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                    controls.borrow_mut().checkpoint()
                },
            )
            .map_err(|error| error.map_source(GraphVertexUpsertError::Merge))?
        };
        let targets = proposal.outcome;
        let (branch, actions) = if targets.created.is_some() {
            (GraphVertexUpsertBranch::Create, upsert.on_create())
        } else {
            (GraphVertexUpsertBranch::Match, upsert.on_match())
        };
        let observed = (targets.len() as u128)
            .checked_mul(
                vertex_upsert_known_effects(actions) as u128
                    + vertex_upsert_known_effects(upsert.after()) as u128,
            )
            .ok_or_else(|| {
                GqlQueryError::Source(GraphVertexUpsertError::Merge(
                    GraphVertexMergeError::AccountingOverflow,
                ))
            })?;
        if observed > u128::from(policy.max_actions) {
            return Err(GqlQueryError::Source(GraphVertexUpsertError::ActionLimit {
                limit: policy.max_actions,
                observed,
            }));
        }
        if let Some(creation) = proposal.creation {
            for intent in creation.into_intents() {
                cx.checkpoint()
                    .map_err(WriteTxnError::Interrupted)
                    .map_err(staging)?;
                execution.checkpoint().map_err(staging)?;
                let GraphInsertIntent::Vertex {
                    vertex,
                    labels,
                    properties,
                } = intent
                else {
                    unreachable!("validated vertex MERGE contains no edge creation")
                };
                let mut batch = WriteBatch::new(upsert.merge().relation());
                batch.create_vertex_values(vertex, labels, properties);
                for row in batch.rows {
                    stage(transaction, database, batch.relation, row, execution)
                        .map_err(staging)?;
                }
            }
        }
        let mut stats = GraphVertexUpsertStats {
            merge: proposal.stats,
            branch,
            property_removals: 0,
            action_effects: 0,
        };
        for outcome in targets.outcomes() {
            event(
                cx,
                execution,
                policy.merge.query,
                &mut stats.merge.evaluator,
                GlaExecutionEvent::Work,
            )?;
            let state = RefCell::new((&mut *transaction, &mut *database));
            let controls = RefCell::new(&mut *execution);
            let completed = vertex_upsert_actions(
                upsert,
                policy,
                stats.merge,
                outcome,
                (stats.action_effects, stats.property_removals),
                || {
                    cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                    controls.borrow_mut().checkpoint()
                },
                |key, control| {
                    if !scope.allows_property(key) {
                        control(GlaExecutionEvent::ScratchEntry)?;
                        return Ok(fgdb_types::CanonicalScalar::Null);
                    }
                    let state = state.borrow();
                    selection::property(
                        state.0,
                        state.1,
                        ElementId::Vertex(outcome.vertex()),
                        key,
                        scope,
                        || {
                            cx.checkpoint()
                                .map_err(WriteTxnError::Interrupted)
                                .map_err(GqlQueryError::Interrupted)?;
                            controls
                                .borrow_mut()
                                .poll()
                                .map_err(GqlQueryError::Interrupted)
                        },
                        control,
                        staging,
                    )
                    .and_then(selection::scalar)
                },
                |control| {
                    let state = state.borrow();
                    vertex_replacement_keys(
                        state.0,
                        state.1,
                        outcome.vertex(),
                        Some(scope),
                        || {
                            cx.checkpoint()
                                .map_err(WriteTxnError::Interrupted)
                                .map_err(GqlQueryError::Interrupted)?;
                            controls
                                .borrow_mut()
                                .poll()
                                .map_err(GqlQueryError::Interrupted)
                        },
                        control,
                        staging,
                    )
                },
                |batch| {
                    let mut state = state.borrow_mut();
                    let (transaction, database) = &mut *state;
                    let mut execution = controls.borrow_mut();
                    for row in batch.rows {
                        cx.checkpoint()
                            .map_err(WriteTxnError::Interrupted)
                            .map_err(staging)?;
                        execution.checkpoint().map_err(staging)?;
                        // Check every original attempted field, even a no-op.
                        stage(transaction, database, batch.relation, row, &mut execution)
                            .map_err(staging)?;
                    }
                    Ok(())
                },
            )?;
            stats.merge.evaluator = completed.merge.evaluator;
            stats.action_effects = completed.action_effects;
            stats.property_removals = completed.property_removals;
        }
        let mut evaluator = stats.merge.evaluator;
        let rows = {
            let controls = RefCell::new(&mut *execution);
            let mut control = |kind| {
                event(
                    cx,
                    &mut **controls.borrow_mut(),
                    policy.merge.query,
                    &mut evaluator,
                    kind,
                )
            };
            let mut poll = || {
                cx.checkpoint()
                    .map_err(WriteTxnError::Interrupted)
                    .map_err(GqlQueryError::Interrupted)?;
                controls
                    .borrow_mut()
                    .poll()
                    .map_err(GqlQueryError::Interrupted)
            };
            control(GlaExecutionEvent::Work)?;
            let mut rows = Vec::new();
            for outcome in targets.outcomes() {
                control(GlaExecutionEvent::ScratchEntry)?;
                let mut row = Vec::new();
                for binding in query.bindings() {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    row.push(match *binding {
                        GraphVertexReturnBinding::Vertex => GraphValue::Vertex(outcome.vertex()),
                        GraphVertexReturnBinding::Property(key) => {
                            if scope.allows_property(key) {
                                let value = selection::property(
                                    transaction,
                                    database,
                                    ElementId::Vertex(outcome.vertex()),
                                    key,
                                    scope,
                                    &mut poll,
                                    &mut control,
                                    staging,
                                )?;
                                // A stored List or Map returns as itself (fgdb-k3nub).
                                GraphValue::from_property_value(&value).ok_or(
                                    GqlQueryError::Data(fgdb_gql::GraphIntegerError {
                                        instruction: 0,
                                        kind: fgdb_gql::GraphIntegerErrorKind::StoredValueBounds,
                                    }),
                                )?
                            } else {
                                // Match the singular masked reader's logical charge.
                                control(GlaExecutionEvent::ScratchEntry)?;
                                GraphValue::Scalar(fgdb_types::CanonicalScalar::Null)
                            }
                        }
                        GraphVertexReturnBinding::Properties => vertex_upsert_properties(
                            transaction,
                            database,
                            outcome.vertex(),
                            query.property_catalog(),
                            |key| scope.allows_property(key),
                            &mut poll,
                            &mut control,
                        )?,
                        GraphVertexReturnBinding::Labels => vertex_upsert_labels(
                            transaction,
                            database,
                            outcome.vertex(),
                            query.property_catalog(),
                            |label| scope.allows_label(label),
                            &mut poll,
                            &mut control,
                        )?,
                    });
                }
                rows.push(row);
            }
            rows
        };
        let controls = RefCell::new(execution);
        let result = query
            .project_rows_governed_reading(
                rows,
                stats.merge.match_selection,
                evaluator,
                policy.merge.query,
                selection::returning_element_reader(transaction, database, cx, scope, &controls),
                || {
                    cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                    controls.borrow_mut().checkpoint()
                },
            )
            .map_err(|error| error.map_source(GraphVertexUpsertError::Returning))?;
        Ok((stats, result))
    }
}
