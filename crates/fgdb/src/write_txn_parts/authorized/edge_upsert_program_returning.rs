//! Final RETURN over the same authorized occurrence batch as rowless programs.
//! No result rescans MATCH, reconstructs a graph, or obtains a second permit.

use super::{Batch, Database, Execution, Vfs, WriteTxnError, apply_all, event, invalid, staging};
use crate::write_txn::authorized::{Workspace, selection};
use crate::write_txn::edge_upsert_query_selection_policy;
use fgdb_delta_types::ElementId;
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GlaExecutionEvent, GqlExecutionStats, GqlQueryError, GqlQueryExecution, GraphEdgeReturnBinding,
    GraphEdgeUpsertBatchStats, GraphEdgeUpsertPolicy, GraphEdgeUpsertQueryError,
    PreparedGraphEdgeUpsertQuery,
};
use fgdb_types::{CanonicalScalar, CommitCx, EmbeddedTxnCompletion, QueryCx, TxnCx};
use fgdb_warden::{Authority, CapabilityToken, Error};
use std::cell::RefCell;

type Fault = GqlQueryError<GraphEdgeUpsertQueryError<WriteTxnError, WriteTxnError>, WriteTxnError>;
type Receipt = (
    GraphEdgeUpsertBatchStats,
    GqlQueryExecution<GraphValueRow>,
    EmbeddedTxnCompletion,
);

fn source(error: WriteTxnError) -> Fault {
    staging(error).map_source(GraphEdgeUpsertQueryError::Upsert)
}
fn overflow() -> Fault {
    invalid().map_source(GraphEdgeUpsertQueryError::Upsert)
}

impl<V: Vfs + Clone> Database<V> {
    /// Apply every authorized MATCH occurrence and parallel relationship, then
    /// evaluate one native RETURN over the full occurrence bag's final state.
    ///
    /// The rowless program host owns selection, existence, ON/SET execution
    /// and original-field/image admission. Duplicate inputs remain duplicate
    /// results, including repeated identities. A changed match key affects the
    /// next occurrence; it does not erase the first occurrence's result binding.
    /// The explicitly singular upsert query entrypoint remains unchanged.
    ///
    /// ReadWrite rights and the requested relation are checked before graph
    /// access. Hidden RETURN properties are NULL BEFORE native point lookup,
    /// payload accounting or witnesses. No privileged source is a fallback.
    /// The SAME live signed permit and cumulative native source/work/scratch,
    /// creation and action limits cover all phases. Only the final projected
    /// page spends signed result rows: neither MATCH nor intermediate identity
    /// bindings are separately delivered. LIMIT 0 still evaluates all actions
    /// and RETURN expressions, and cannot waive any non-result allowance.
    ///
    /// Projection, grouping, DISTINCT, ordering and pagination finish before
    /// the sole native completion. Any precommit failure discards all private
    /// effects and output. Unknown/recovery completion errors retain their
    /// native meaning. Engine identities are not reclaimed on refusal. This
    /// remains resident execution, not a spillable or durable result lease.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_edge_upserts_query_authorized(
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
    ) -> Result<Receipt, Fault> {
        self.execute_graph_edge_upserts_query_authorized_checked(
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

    /// Admit the complete native result under the same live permit BEFORE
    /// the sole transaction completion. The trusted callback runs exactly
    /// once, including for zero rows, after projection and signed row admission.
    /// It must retain output privately until completion; it cannot replace
    /// rows, obtain the database or publish a successful prefix.
    ///
    /// The meter polls cancellation and charges additional host work to the
    /// SAME signed permit, including expiry and retirement. Encoding refusal,
    /// quota exhaustion or unwind drops all private effects and output. No
    /// fallible host processing follows committed/unknown completion. The
    /// ordinary entrypoint uses a no-op and preserves its original accounting.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_edge_upserts_query_authorized_checked(
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
                execution.checkpoint().map_err(source)?;
                let mut workspace = Workspace(Some(
                    self.begin(txn_cx)
                        .map_err(WriteTxnError::Write)
                        .map_err(source)?,
                ));
                let Batch {
                    stats, outcomes, ..
                } = apply_all(
                    workspace.transaction(),
                    self,
                    query_cx,
                    query.upsert(),
                    edge_upsert_query_selection_policy(policy),
                    verified.predicates(),
                    &mut execution,
                    true,
                )
                .map_err(|error| error.map_source(GraphEdgeUpsertQueryError::Upsert))?;
                let mut evaluator = stats.evaluator;
                let rows = query_cx
                    .with_restriction(|| {
                        let controls = RefCell::new(&mut execution);
                        let mut control = |kind| {
                            event(
                                query_cx,
                                &mut controls.borrow_mut(),
                                policy.merge.query,
                                &mut evaluator,
                                kind,
                            )
                        };
                        control(GlaExecutionEvent::Work)?;
                        let mut rows = Vec::new();
                        for outcome in outcomes {
                            let edge = outcome.edge().ok_or_else(invalid)?;
                            control(GlaExecutionEvent::ScratchEntry)?;
                            let mut cells = Vec::new();
                            for binding in query.bindings() {
                                control(GlaExecutionEvent::ScratchEntry)?;
                                cells.push(match *binding {
                                    GraphEdgeReturnBinding::Edge => GraphValue::Edge(edge),
                                    GraphEdgeReturnBinding::Property(key) => {
                                        if verified.predicates().allows_property(key) {
                                            let value = selection::property(
                                                workspace.transaction(),
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
                                                &mut control,
                                                staging,
                                            )?;
                                            GraphValue::from_property_value(&value).ok_or(
                                                GqlQueryError::Data(fgdb_gql::GraphIntegerError {
                                                    instruction: 0,
                                                    kind: fgdb_gql::GraphIntegerErrorKind::StoredValueBounds,
                                                }),
                                            )?
                                        } else {
                                            control(GlaExecutionEvent::Work)?;
                                            control(GlaExecutionEvent::ScratchEntry)?;
                                            GraphValue::Scalar(CanonicalScalar::Null)
                                        }
                                    }
                                });
                            }
                            rows.push(cells);
                        }
                        Ok(rows)
                    })
                    .map_err(|error: super::Fault| {
                        error.map_source(GraphEdgeUpsertQueryError::Upsert)
                    })?;
                let snapshot_records = stats
                    .match_selection
                    .snapshot_records
                    .checked_add(stats.overlay_edges)
                    .ok_or_else(overflow)?;
                let result_rows = u64::try_from(rows.len()).map_err(|_| overflow())?;
                let result = query_cx.with_restriction(|| {
                    let transaction = &*workspace.transaction();
                    let controls = RefCell::new(&mut execution);
                    query
                        .project_rows_governed_reading(
                            rows,
                            GqlExecutionStats {
                                snapshot_records,
                                result_rows,
                            },
                            evaluator,
                            policy.merge.query,
                            selection::returning_element_reader(transaction, self, query_cx, verified.predicates(), &controls),
                            || {
                                query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                                controls.borrow_mut().checkpoint()
                            },
                        )
                        .map_err(|error| error.map_source(GraphEdgeUpsertQueryError::Returning))
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
                Ok((stats, result, completion))
            })
            .await
    }
}
