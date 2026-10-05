//! Conditional relationship writes over the existing authorized MERGE. The
//! native branch lowerer and original image checks remain the only writers.

use super::{
    Authority, CapabilityToken, Database, Error, Execution, Vfs, Workspace, WriteTxnError,
};
use crate::write_txn::authorized::{edge_merge, stage};
use crate::write_txn::{WriteTxn, edge_upsert_actions, edge_upsert_property};
use fgdb_gql::{
    GqlQueryError, GraphEdgeMergeOutcome, GraphEdgeUpsertError, GraphEdgeUpsertPolicy,
    GraphEdgeUpsertStats, PreparedGraphEdgeUpsert,
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

fn source(error: WriteTxnError) -> Fault {
    GqlQueryError::Source(GraphEdgeUpsertError::Staging(error))
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
                edge_upsert_property(state.0, state.1, edge, key, control)
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
