//! Plain DELETE under one live capability. The GQL collector freezes typed
//! targets; native pinned reads prove that the vertex cascade set is empty.

use super::{Authority, CapabilityToken, Database, Error, Execution, Vfs};
use super::{Workspace, WriteBatch, WriteTxn, WriteTxnError, redacted, selection, stage};
use crate::gql_exec::source::SourceEvent;
use fgdb_delta_types::RelationId;
use fgdb_gql::{
    GlaLimitDimension, GlaLimitExceeded, GqlBudgetDimension, GqlQueryError, GraphDeleteError,
    GraphDeletePolicy, GraphDeleteStats, PreparedGraphDelete,
};
use fgdb_types::{CommitCx, EId, EmbeddedTxnCompletion, QueryCx, TxnCx, VId};
use fgdb_warden::PlannerPredicates;
use std::cell::RefCell;

type Fault = GqlQueryError<GraphDeleteError<WriteTxnError>, WriteTxnError>;
type Receipt = (GraphDeleteStats, Vec<VId>, Vec<EId>, EmbeddedTxnCompletion);

fn source(error: WriteTxnError) -> Fault {
    GqlQueryError::Source(GraphDeleteError::Source(error))
}

// Extend, never reset, the collector's native work/scratch allowance while the
// same live Warden permit pays every logical proof and staging-input event.
fn event<Clock: FnMut() -> u64>(
    stats: &mut GraphDeleteStats,
    policy: GraphDeletePolicy,
    cx: &QueryCx,
    execution: &mut Execution<'_, '_, Clock>,
    kind: SourceEvent,
) -> Result<(), Fault> {
    cx.checkpoint()
        .map_err(WriteTxnError::Interrupted)
        .map_err(source)?;
    execution.checkpoint().map_err(source)?;
    let mut next = *stats;
    if kind == SourceEvent::SnapshotRecord {
        next.selection.snapshot_records =
            next.selection
                .snapshot_records
                .checked_add(1)
                .ok_or(GqlQueryError::Source(
                    GraphDeleteError::InvalidSourceStatistics,
                ))?;
        policy
            .query
            .rows
            .check(
                GqlBudgetDimension::SnapshotRecords,
                next.selection.snapshot_records,
            )
            .map_err(GqlQueryError::Rows)?;
    }
    let work = u128::from(next.evaluator.work_units) + 1;
    let scratch =
        u128::from(next.evaluator.scratch_entries) + u128::from(kind == SourceEvent::ScratchEntry);
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
            return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                dimension,
                limit,
                observed,
            }));
        }
    }
    next.evaluator.work_units = work as u64;
    next.evaluator.scratch_entries = scratch as u64;
    *stats = next;
    Ok(())
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute non-detaching MATCH ... DELETE under one ReadWrite capability.
    ///
    /// Matching uses the same masked GLA source as authorized reads and writes.
    /// The ordinary DELETE collector freezes and deduplicates typed targets,
    /// ignoring null OPTIONAL targets. Only the explicitly selected EIds are
    /// removed. A vertex may be deleted only when every incident edge is among
    /// those selected identities; unselected parallel, incoming, self-loop and
    /// cross-relation edges refuse the entire statement, never silently cascade.
    /// Explicit edge deletions are staged before vertices regardless of the
    /// target declaration order. Every original image still passes native
    /// authorized staging, including actual edge relation and both endpoints.
    ///
    /// Whole-edge deletion requires unrestricted property visibility. Any
    /// vertex deletion requires visibility of all fields and all incidence.
    /// These decisions precede incidence inspection, so a restricted holder
    /// cannot use a plain DELETE refusal as an oracle for hidden edges/fields.
    /// Selection, proof, staging and final validation share one signed permit.
    /// No proposed target, private workspace or result prefix escapes on error.
    /// An empty selection closes read-only without publishing a new sequence.
    /// Stats are control receipts and need no signed result-row allowance.
    ///
    /// Incidence is read through the native index and pinned overlay, retaining
    /// its target and point dependencies. The proof reads only target incidence,
    /// without cloning unrelated edges or property payloads. Its logical
    /// records/work and delete proposals
    /// extend the selection's native policy counters. As on ordinary DELETE,
    /// overlay materialization and native preparation remain resident-storage
    /// operations, not bounded-memory or preemptible query execution.
    ///
    /// The trusted host supplies issuer, exact branch routing and monotone
    /// issuer-epoch clock. No fallible authorization check follows publication;
    /// native unknown/recovery outcomes remain authoritative. This is not a
    /// mandatory secure facade, durable audit/revocation protocol or full SSI.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_delete_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        deletion: &PreparedGraphDelete,
        policy: GraphDeletePolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<(GraphDeleteStats, EmbeddedTxnCompletion), Fault> {
        self.delete_authorized_inner(
            txn_cx, query_cx, commit_cx, authority, token, branch, deletion, policy, clock, false,
        )
        .await
        .map(|(stats, _, _, completion)| (stats, completion))
    }

    /// Return independently sorted, distinct vertex and edge targets after
    /// successful native completion. One signed row is charged per identity
    /// before staging/publication. Parallel EIds remain distinct; repeated
    /// MATCH occurrences and target aliases do not duplicate identity receipts.
    /// Receipt storage is the already bounded native target proposal, not a
    /// second clone of graph records or an unbounded result surface.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_delete_returning_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        deletion: &PreparedGraphDelete,
        policy: GraphDeletePolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<Receipt, Fault> {
        self.delete_authorized_inner(
            txn_cx, query_cx, commit_cx, authority, token, branch, deletion, policy, clock, true,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn delete_authorized_inner(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        deletion: &PreparedGraphDelete,
        policy: GraphDeletePolicy,
        mut clock: impl FnMut() -> u64,
        returning: bool,
    ) -> Result<Receipt, Fault> {
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
                let (stats, vertices, edges) = apply(
                    workspace.transaction(),
                    self,
                    query_cx,
                    deletion,
                    policy,
                    verified.predicates(),
                    &mut execution,
                    returning,
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
                Ok((stats, vertices, edges, completion))
            })
            .await
    }
}

// Execute one step only. The caller owns the same private transaction, permit
// and eventual native completion; no per-step commit or fresh allowance exists.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    query_cx: &QueryCx,
    deletion: &PreparedGraphDelete,
    policy: GraphDeletePolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    returning: bool,
) -> Result<(GraphDeleteStats, Vec<VId>, Vec<EId>), Fault> {
    let proposal = query_cx.with_restriction(|| {
        let execution = RefCell::new(&mut *execution);
        deletion.execute_governed(
            policy,
            |pattern, allowance| {
                selection::select_overlay(
                    transaction,
                    database,
                    query_cx,
                    pattern,
                    scope,
                    allowance,
                    &execution,
                )
            },
            || {
                query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                execution.borrow_mut().checkpoint()
            },
        )
    })?;
    let stats = proposal.stats();
    complete(
        transaction,
        database,
        query_cx,
        deletion.relation(),
        stats,
        proposal.into_target_parts(),
        policy,
        scope,
        execution,
        returning,
    )
}

// The one non-detaching completion once a collector has frozen sorted typed
// targets: a plain DELETE statement, or a mutation's plain DELETE actions.
#[allow(clippy::too_many_arguments)]
pub(super) fn complete<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    query_cx: &QueryCx,
    relation: RelationId,
    mut stats: GraphDeleteStats,
    (vertices, edges): (Vec<VId>, Vec<EId>),
    policy: GraphDeletePolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    returning: bool,
) -> Result<(GraphDeleteStats, Vec<VId>, Vec<EId>), Fault> {
    if (!vertices.is_empty() && !(scope.sees_all_incidence() && scope.sees_all_fields()))
        || (!edges.is_empty() && !scope.sees_all_properties())
    {
        return Err(source(WriteTxnError::Authorization(Error::ScopeDenied)));
    }
    if returning {
        let rows = stats
            .target_vertices
            .checked_add(stats.target_edges)
            .ok_or_else(|| source(WriteTxnError::Authorization(Error::TooLarge)))?;
        execution
            .permit
            .charge_rows_at((execution.clock)(), rows)
            .map_err(|error| source(WriteTxnError::Authorization(error)))?;
    }
    if !vertices.is_empty() {
        // The same native proof as owner DELETE. Full visibility above admits
        // every target's incidence before it is observed or charged. Original
        // snapshot edges and canonical staged creations/retirements contribute
        // the proof's ordinary witnesses; MATCH alone never proves isolation.
        transaction
            .prove_delete_incidence(database, &vertices, &edges, &mut |kind| {
                event(&mut stats, policy, query_cx, execution, kind)
            })
            .map_err(|error| {
                error.map_source(|error| match error {
                    GraphDeleteError::Source(error) => {
                        // The event callback retains the public signed-limit,
                        // expiry and interruption failures. Storage/ownership
                        // refusals remain redacted as on the former scan.
                        GraphDeleteError::Source(match error {
                            WriteTxnError::Authorization(_) | WriteTxnError::Interrupted(_) => {
                                error
                            }
                            error => redacted(error),
                        })
                    }
                    error => error,
                })
            })?;
    }
    // Retire ALL explicit edges first. The complete incidence
    // proof above guarantees every subsequent native vertex delete
    // has an empty cascade in this exclusively owned workspace.
    for edge in &edges {
        event(
            &mut stats,
            policy,
            query_cx,
            execution,
            SourceEvent::ScratchEntry,
        )?;
        let mut batch = WriteBatch::new(relation);
        batch.delete_edge(*edge);
        for row in batch.rows {
            stage(transaction, database, batch.relation, row, execution).map_err(source)?;
        }
        transaction.record_chain_retirement(
            fgdb_delta_types::ElementId::Edge(*edge),
            &mut |kind| {
                event(
                    &mut stats,
                    policy,
                    query_cx,
                    execution,
                    match kind {
                        fgdb_gql::GlaExecutionEvent::ScratchEntry => SourceEvent::ScratchEntry,
                        _ => SourceEvent::Work,
                    },
                )
            },
        )?;
    }
    for vertex in &vertices {
        event(
            &mut stats,
            policy,
            query_cx,
            execution,
            SourceEvent::ScratchEntry,
        )?;
        let mut batch = WriteBatch::new(relation);
        batch.delete_vertex(*vertex);
        for row in batch.rows {
            stage(transaction, database, batch.relation, row, execution).map_err(source)?;
        }
        transaction.record_chain_retirement(
            fgdb_delta_types::ElementId::Vertex(*vertex),
            &mut |kind| {
                event(
                    &mut stats,
                    policy,
                    query_cx,
                    execution,
                    match kind {
                        fgdb_gql::GlaExecutionEvent::ScratchEntry => SourceEvent::ScratchEntry,
                        _ => SourceEvent::Work,
                    },
                )
            },
        )?;
    }
    if returning {
        Ok((stats, vertices, edges))
    } else {
        Ok((stats, Vec::new(), Vec::new()))
    }
}

#[cfg(test)]
#[path = "deletion_tests.rs"]
mod tests;
