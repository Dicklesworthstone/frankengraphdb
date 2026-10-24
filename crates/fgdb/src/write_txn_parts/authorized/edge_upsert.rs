//! Conditional relationship writes over the existing authorized MERGE. The
//! native branch lowerer and original image checks remain the only writers.

use super::{
    Authority, CapabilityToken, Database, Error, Execution, Vfs, Workspace, WriteTxnError,
};
use crate::write_txn::authorized::{edge_merge, selection, stage};
use crate::write_txn::{
    WriteTxn, edge_upsert_actions, edge_upsert_query_selection_policy, edge_upsert_returning,
};
use fgdb_delta_types::ElementId;
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::{
    GqlQueryError, GqlQueryExecution, GraphEdgeMergeOutcome, GraphEdgeUpsertError,
    GraphEdgeUpsertPolicy, GraphEdgeUpsertQueryError, GraphEdgeUpsertStats,
    PreparedGraphEdgeUpsert, PreparedGraphEdgeUpsertQuery,
};
use fgdb_types::{CommitCx, EmbeddedTxnCompletion, QueryCx, TxnCx};
use fgdb_warden::PlannerPredicates;
use std::cell::RefCell;

type Fault = GqlQueryError<GraphEdgeUpsertError<WriteTxnError, WriteTxnError>, WriteTxnError>;
type Receipt = (
    GraphEdgeUpsertStats,
    GraphEdgeMergeOutcome,
    EmbeddedTxnCompletion,
);
type QueryFault =
    GqlQueryError<GraphEdgeUpsertQueryError<WriteTxnError, WriteTxnError>, WriteTxnError>;
type QueryReceipt = (
    GraphEdgeUpsertStats,
    GraphEdgeMergeOutcome,
    GqlQueryExecution<GraphValueRow>,
    EmbeddedTxnCompletion,
);

fn source(error: WriteTxnError) -> Fault {
    GqlQueryError::Source(GraphEdgeUpsertError::Staging(error))
}

fn query_source(error: WriteTxnError) -> QueryFault {
    source(error).map_source(GraphEdgeUpsertQueryError::Upsert)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn apply<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    input: &PreparedGraphEdgeUpsert,
    policy: GraphEdgeUpsertPolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    returning: bool,
) -> Result<(GraphEdgeUpsertStats, GraphEdgeMergeOutcome), Fault> {
    // Reuse the already-landed masked selection, original endpoint admission
    // and directed existence probe. Do not issue another permit or deliver a
    // receipt for this internal MERGE; the complete upsert returns one identity.
    let (merge_stats, outcome) = edge_merge::apply(
        transaction,
        database,
        cx,
        input.merge(),
        policy.merge,
        scope,
        execution,
        false,
    )
    .map_err(|error| error.map_source(GraphEdgeUpsertError::Merge))?;
    let stats = cx.with_restriction(|| {
        // Sequential borrows end at each callback, before the next clause.
        let state = RefCell::new((&mut *transaction, &mut *database));
        let controls = RefCell::new(&mut *execution);
        edge_upsert_actions::<WriteTxnError, WriteTxnError, _>(
            input,
            policy,
            merge_stats,
            outcome,
            || {
                cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                controls.borrow_mut().checkpoint()
            },
            |edge, key, control| {
                // Mask BEFORE lookup: neither a hidden value, its byte length,
                // nor its history cost can affect an expression or signed quota.
                if !scope.allows_property(key) {
                    control(fgdb_gql::GlaExecutionEvent::Work)?;
                    control(fgdb_gql::GlaExecutionEvent::ScratchEntry)?;
                    return Ok(fgdb_types::CanonicalScalar::Null);
                }
                let state = state.borrow();
                // The singular host uses the same admitted-field accounting
                // as occurrence-preserving programs. Physical history and
                // hidden-key comparisons poll cancellation without changing
                // either the native or signed work refusal threshold.
                selection::property(
                    state.0,
                    state.1,
                    ElementId::Edge(edge),
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
                    source,
                )
                .and_then(selection::scalar)
            },
            |batch| {
                let mut state = state.borrow_mut();
                let (transaction, database) = &mut *state;
                let mut execution = controls.borrow_mut();
                for row in batch.rows {
                    cx.checkpoint()
                        .map_err(WriteTxnError::Interrupted)
                        .map_err(source)?;
                    execution.checkpoint().map_err(source)?;
                    // Check ORIGINAL images and every field, before the next
                    // clause can overwrite it. Hidden fields stay untouched.
                    stage(transaction, database, batch.relation, row, &mut execution)
                        .map_err(source)?;
                }
                Ok(())
            },
        )
    })?;
    if returning && outcome.edge().is_some() {
        execution
            .permit
            .charge_rows_at((execution.clock)(), 1)
            .map_err(|error| source(WriteTxnError::Authorization(error)))?;
    }
    Ok((stats, outcome))
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute relationship ON/SET actions and their RETURN under one live
    /// ReadWrite capability, private workspace and native completion.
    ///
    /// This is the projected form of the existing singular authorized upsert:
    /// duplicate endpoint occurrences reduce to one pair; distinct pairs and
    /// parallel matching edges refuse. It does not choose an arbitrary edge or
    /// acquire the privileged occurrence-preserving API's wider cardinality.
    /// The original masked selection, existence probe and attempted-field/image
    /// admission remain authoritative. RETURN reads the chosen edge's final
    /// staged properties; hidden fields become NULL BEFORE lookup or witness.
    ///
    /// MATCH and the internal identity receipt are private inputs, not results.
    /// Only the final RETURN page consumes native and signed result-row limits.
    /// LIMIT 0 still executes and validates actions and projection; NoInput runs
    /// no action. Source, work, scratch, creation and action limits are unchanged,
    /// with the same permit covering every phase. Projection, expiry, quota or
    /// cancellation refusal drops all private effects before the sole commit.
    /// Nothing reads the graph or reauthorizes after completion. Ordinary
    /// unknown/recovery outcomes and identity-reservation limitations remain.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_edge_upsert_query_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        query: &PreparedGraphEdgeUpsertQuery,
        policy: GraphEdgeUpsertPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<QueryReceipt, QueryFault> {
        self.execute_graph_edge_upsert_query_authorized_checked(
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

    /// Admit the singular relationship-upsert result before native completion.
    ///
    /// The trusted callback runs exactly once, including for zero rows, after
    /// native projection and signed row admission. It receives only immutable
    /// result rows and a work/cancellation meter tied to the SAME live permit.
    /// Refusal, expiry, issuer retirement, cancellation or unwind drops the
    /// unpublished workspace. The ordinary entrypoint uses a no-op callback and
    /// preserves its existing accounting and result semantics.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_edge_upsert_query_authorized_checked(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        query: &PreparedGraphEdgeUpsertQuery,
        policy: GraphEdgeUpsertPolicy,
        mut clock: impl FnMut() -> u64,
        admit: impl FnOnce(
            &[GraphValueRow],
            &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
        ) -> Result<(), WriteTxnError>,
    ) -> Result<QueryReceipt, QueryFault> {
        let refusal = |error| query_source(WriteTxnError::Authorization(error));
        if authority.namespace() != self.keys.namespace {
            return Err(refusal(Error::WrongAuthority));
        }
        let now = clock();
        let verified = authority.verify_at(token, branch, now).map_err(refusal)?;
        let permit = verified.begin_write_at(branch, now).map_err(refusal)?;
        if !verified.predicates().rights().can_read() {
            return Err(refusal(Error::PermissionDenied));
        }
        if !verified
            .predicates()
            .allows_relation(query.upsert().merge().relation())
        {
            return Err(refusal(Error::ScopeDenied));
        }
        commit_cx
            .with_restriction_async(async {
                let mut execution = Execution {
                    cx: commit_cx,
                    permit,
                    clock,
                };
                execution.checkpoint().map_err(query_source)?;
                let mut workspace = Workspace(Some(
                    self.begin(txn_cx)
                        .map_err(WriteTxnError::Write)
                        .map_err(query_source)?,
                ));
                let (stats, outcome) = apply(
                    workspace.transaction(),
                    self,
                    query_cx,
                    query.upsert(),
                    edge_upsert_query_selection_policy(policy),
                    verified.predicates(),
                    &mut execution,
                    false,
                )
                .map_err(|error| error.map_source(GraphEdgeUpsertQueryError::Upsert))?;
                let result = query_cx.with_restriction(|| {
                    let controls = RefCell::new(&mut execution);
                    let transaction = &*workspace.transaction();
                    edge_upsert_returning::<WriteTxnError, WriteTxnError, _>(
                        query,
                        stats,
                        outcome,
                        policy,
                        || {
                            query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                            controls.borrow_mut().checkpoint()
                        },
                        |edge, key, control| {
                            // Do not fetch a hidden payload and mask it later:
                            // its value, size and history cost must remain private.
                            if !verified.predicates().allows_property(key) {
                                control(fgdb_gql::GlaExecutionEvent::Work)?;
                                control(fgdb_gql::GlaExecutionEvent::ScratchEntry)?;
                                return Ok(fgdb_types::CanonicalScalar::Null);
                            }
                            selection::property(
                                transaction,
                                self,
                                ElementId::Edge(edge),
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
                            .and_then(selection::scalar)
                        },
                        |element, key| {
                            selection::returning_element_property(
                                transaction,
                                self,
                                query_cx,
                                element,
                                key,
                                verified.predicates(),
                                &controls,
                            )
                        },
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
                    .map_err(query_source)?;
                execution.bind_decision(workspace.transaction());
                let completion = workspace
                    .transaction()
                    .complete_controlled(self, commit_cx, None, false, || {
                        query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        execution.checkpoint()
                    })
                    .await
                    .map_err(query_source)?;
                Ok((stats, outcome, result, completion))
            })
            .await
    }

    /// Execute directed MERGE with computed ON and trailing SET clauses.
    /// Expression properties come from the chosen edge's masked native overlay;
    /// each clause freezes its inputs and the next sees preceding checked writes.
    ///
    /// Requires ReadWrite and the requested relation before any database access,
    /// even for NoInput. The existing authorized MERGE chooses one branch from
    /// the masked canonical graph. The native action lowerer then charges ONLY
    /// that branch and trailing SET against the SAME native and signed allowances.
    /// Each selected original property write is authorized, including no-ops;
    /// untouched hidden properties and both endpoints are preserved. NoInput
    /// executes neither branch, allocates no identity and costs no result row.
    ///
    /// One private workspace and one native completion cover the whole call.
    /// A denied action, exhausted allowance, expiry or cancellation discards an
    /// earlier staged creation. Matched/Created returns one identity, admitted
    /// before publication, not an extra receipt for the nested MERGE or actions.
    /// No fallible permission check or receipt construction follows completion.
    /// The host, engine-reservation, resident-overlay, timing, global-constraint
    /// and unknown/recovery boundaries of execute_graph_edge_merge_authorized
    /// apply. This adds neither a second writer nor a long-lived authorized
    /// transaction, durable revocation/audit, mandatory secure facade or full SSI.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_edge_upsert_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        input: &PreparedGraphEdgeUpsert,
        policy: GraphEdgeUpsertPolicy,
        mut clock: impl FnMut() -> u64,
    ) -> Result<Receipt, Fault> {
        let refusal = |error| source(WriteTxnError::Authorization(error));
        if authority.namespace() != self.keys.namespace {
            return Err(refusal(Error::WrongAuthority));
        }
        let now = clock();
        let verified = authority.verify_at(token, branch, now).map_err(refusal)?;
        let permit = verified.begin_write_at(branch, now).map_err(refusal)?;
        if !verified.predicates().rights().can_read() {
            return Err(refusal(Error::PermissionDenied));
        }
        if !verified
            .predicates()
            .allows_relation(input.merge().relation())
        {
            return Err(refusal(Error::ScopeDenied));
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
                        .map_err(WriteTxnError::Write)
                        .map_err(source)?,
                ));
                let (stats, outcome) = apply(
                    workspace.transaction(),
                    self,
                    query_cx,
                    input,
                    policy,
                    verified.predicates(),
                    &mut execution,
                    true,
                )?;
                execution.bind_decision(workspace.transaction());
                let completion = workspace
                    .transaction()
                    .complete_controlled(self, commit_cx, None, false, || {
                        query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        execution.checkpoint()
                    })
                    .await
                    .map_err(source)?;
                Ok((stats, outcome, completion))
            })
            .await
    }
}

#[cfg(test)]
#[path = "edge_upsert_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "edge_upsert_query_tests.rs"]
mod query_tests;
