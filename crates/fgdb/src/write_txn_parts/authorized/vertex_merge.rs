//! Authorized unique-vertex MERGE and its selected ON MATCH/ON CREATE actions.
//! Reuse the native reducer, point projection and checked scalar-action collector.

use crate::write_txn::authorized::{
    Authority, CapabilityToken, Database, Error, Execution, Vfs, Workspace, WriteBatch, WriteTxn,
    WriteTxnError, selection, stage,
};
use crate::write_txn::{
    collect_vertex_merge, vertex_upsert_actions, vertex_upsert_property, vertex_upsert_returning,
};
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
    /// Hidden properties yield NULL before lookup, source admission or witness
    /// creation. No unmasked read or rematch is used to construct the result.
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
        mut clock: impl FnMut() -> u64,
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
                    policy,
                    verified.predicates(),
                    &mut execution,
                    false,
                )?;
                let result = query_cx.with_restriction(|| {
                    let controls = RefCell::new(&mut execution);
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
                                return Ok(fgdb_types::CanonicalScalar::Null);
                            }
                            vertex_upsert_property(
                                workspace.transaction(),
                                self,
                                outcome.vertex(),
                                key,
                                control,
                            )
                        },
                    )
                })?;
                let rows = u64::try_from(result.value.len())
                    .map_err(|_| source(WriteTxnError::Authorization(Error::TooLarge)))?;
                execution
                    .permit
                    .charge_rows_at((execution.clock)(), rows)
                    .map_err(|error| source(WriteTxnError::Authorization(error)))?;
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
            |pattern, budget| {
                selection::select_overlay(
                    transaction,
                    &database.borrow(),
                    cx,
                    pattern,
                    scope,
                    budget,
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
            batch.create_vertex(vertex, labels, properties);
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
            || {
                cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                controls.borrow_mut().checkpoint()
            },
            |key, control| {
                // Mask BEFORE lookup, not after reading a protected payload.
                // A hidden field always supplies NULL, with no data-dependent
                // source work, size admission, error or conflict observation.
                // The MERGE reducer has already admitted this target's scope.
                if !scope.allows_property(key) {
                    control(fgdb_gql::GlaExecutionEvent::ScratchEntry)?;
                    return Ok(fgdb_types::CanonicalScalar::Null);
                }
                let state = state.borrow();
                vertex_upsert_property(state.0, state.1, outcome.vertex(), key, control)
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
