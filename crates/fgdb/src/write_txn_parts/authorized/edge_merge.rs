//! Directed relationship get-or-create on the same native MERGE collector.
//! Selection is masked; existence candidates are confined to its admitted pair.

use super::super::{
    EdgeMergeOutput, EdgeMergeRows, collect_edge_merge_output, edge_merge_query_rows,
};
use super::{
    Authority, CapabilityToken, Database, Error, Execution, Vfs, Workspace, WriteTxn,
    WriteTxnError, redacted, selection, stage,
};
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{
    GqlQueryError, GqlQueryExecution, GraphEdgeMergeError, GraphEdgeMergeOutcome,
    GraphEdgeMergePolicy, GraphEdgeMergeQueryError, GraphEdgeMergeStats, PreparedGraphEdgeMerge,
    PreparedGraphEdgeMergeQuery,
};
use fgdb_types::{CommitCx, EmbeddedTxnCompletion, QueryCx, TxnCx};
use fgdb_warden::PlannerPredicates;
use std::cell::RefCell;

type Fault = GqlQueryError<GraphEdgeMergeError<WriteTxnError, WriteTxnError>, WriteTxnError>;
type Receipt = (
    GraphEdgeMergeStats,
    GraphEdgeMergeOutcome,
    EmbeddedTxnCompletion,
);
type QueryFault =
    GqlQueryError<GraphEdgeMergeQueryError<WriteTxnError, WriteTxnError>, WriteTxnError>;
type QueryReceipt = (
    GraphEdgeMergeStats,
    GqlQueryExecution<GraphValueRow>,
    EmbeddedTxnCompletion,
);

fn source(error: WriteTxnError) -> Fault {
    GqlQueryError::Source(GraphEdgeMergeError::Source(error))
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute directed relationship MERGE atomically under ReadWrite authority.
    ///
    /// The ordinary native collector reduces duplicate endpoint occurrences,
    /// refuses null or multiple distinct pairs, and distinguishes NoInput,
    /// Matched and Created. It sees only the exact authorized relation/pair's
    /// canonical live EIds. Parallel EIds remain ambiguous, never an arbitrary
    /// first match or an ensure alias. Both endpoints are selected through the
    /// same masked GLA source as other authorized writes and rechecked before
    /// existence inspection. Hidden endpoint data, properties and other
    /// relations cannot alter that inspection's logical resource counts.
    ///
    /// Creation uses an engine-reserved EId and original before/after image
    /// authorization. An existing match preserves all fields, including hidden
    /// properties. The MERGE map both filters existing edges and fills a
    /// created one (fgdb-at06q), so every key it names must be visible; an
    /// undirected MERGE admits either orientation of the pair. The requested
    /// relation must be permitted even when selection would be empty; this is
    /// a capability-only preflight.
    ///
    /// One live permit covers selection, reduction, creation and final native
    /// completion. Each Matched/Created receipt costs one signed row BEFORE
    /// publication; NoInput costs none. Matched and NoInput close read-only.
    /// Native unknown/recovery outcomes are unchanged; no fallible permission
    /// check or receipt construction follows publication.
    ///
    /// The native edge overlay still materializes resident storage and retains
    /// its conservative scan/point dependencies. Its physical traversal polls
    /// cancellation without billing hidden records; this is NOT bounded-memory
    /// execution, descriptor-I/O isolation or timing noninterference. Uniqueness
    /// is local to the selected visible pair, not a global constraint. The host
    /// owns issuer, branch routing and monotone clock; raw APIs remain trusted.
    /// Engine reservations are not durable leases or reclaimed after refusal;
    /// sequential allocation metadata, durable revocation/audit and full SSI
    /// remain outside this surface.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_edge_merge_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        merge: &PreparedGraphEdgeMerge,
        policy: GraphEdgeMergePolicy,
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
        if !verified.predicates().allows_relation(merge.relation()) {
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
                        .map_err(|error| source(WriteTxnError::Write(error)))?,
                ));
                let (stats, outcome) = apply(
                    workspace.transaction(),
                    self,
                    query_cx,
                    merge,
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

    /// Relationship MERGE RETURN under one ReadWrite permit and one native
    /// completion (fgdb-at06q). Every selected pair resolves through the same
    /// masked collector as a MERGE program, keeping each parallel match. RETURN
    /// then reads those edges and their endpoints from the same private
    /// overlay. A hidden property yields NULL before lookup or witness, and
    /// MATCH bindings are the masked selection's own columns. Only final
    /// projected rows consume the signed result allowance, so LIMIT 0 still
    /// commits the effects. Any refusal discards every private effect; no
    /// fallible check follows completion.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_edge_merge_query_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        query: &PreparedGraphEdgeMergeQuery,
        policy: GraphEdgeMergePolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<QueryReceipt, QueryFault> {
        self.execute_graph_edge_merge_query_authorized_checked(
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

    /// Admit a private host representation of the final MERGE rows before
    /// native completion. The callback runs once, including for zero rows,
    /// after projection and signed row admission. It receives no database or
    /// transaction handle and cannot replace the native result.
    ///
    /// Host work is charged through the same live write permit. Refusal,
    /// expiry, retirement or cancellation therefore discards the unpublished
    /// workspace. No fallible host result processing follows completion.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_edge_merge_query_authorized_checked(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        query: &PreparedGraphEdgeMergeQuery,
        policy: GraphEdgeMergePolicy,
        mut clock: impl FnMut() -> u64,
        admit: impl FnOnce(
            &[GraphValueRow],
            &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
        ) -> Result<(), WriteTxnError>,
    ) -> Result<QueryReceipt, QueryFault> {
        let merged = |error: Fault| error.map_source(GraphEdgeMergeQueryError::Merge);
        let refusal = |error| merged(source(WriteTxnError::Authorization(error)));
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
            .allows_relation(query.merge().relation())
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
                execution.checkpoint().map_err(source).map_err(merged)?;
                let mut workspace = Workspace(Some(
                    self.begin(txn_cx)
                        .map_err(|error| merged(source(WriteTxnError::Write(error))))?,
                ));
                let (stats, resolved, selected) = apply_with_output::<EdgeMergeRows, V, _>(
                    workspace.transaction(),
                    self,
                    query_cx,
                    query.merge(),
                    policy,
                    verified.predicates(),
                    &mut execution,
                    false,
                )
                .map_err(merged)?;
                let result = query_cx.with_restriction(|| {
                    let transaction = &*workspace.transaction();
                    let controls = RefCell::new(&mut execution);
                    let mut checkpoint = || {
                        query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        controls.borrow_mut().checkpoint()
                    };
                    let rows = edge_merge_query_rows(
                        query,
                        &selected,
                        &resolved,
                        &mut checkpoint,
                        |element, key| {
                            // Mask BEFORE lookup: a hidden field is NULL with
                            // no source read, admission or witness.
                            if !verified.predicates().allows_property(key) {
                                return Ok(None);
                            }
                            transaction.merge_return_property(self, element, key)
                        },
                    )
                    .map_err(|error| {
                        error.map_source(|error| {
                            GraphEdgeMergeQueryError::Merge(GraphEdgeMergeError::Source(error))
                        })
                    })?;
                    query
                        .finish_governed(
                            rows,
                            stats.match_selection,
                            stats.evaluator,
                            policy.query,
                            checkpoint,
                        )
                        .map_err(|error| error.map_source(GraphEdgeMergeQueryError::Returning))
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
                    .map_err(source)
                    .map_err(merged)?;
                execution.bind_decision(workspace.transaction());
                let completion = workspace
                    .transaction()
                    .complete_controlled(self, commit_cx, None, false, || {
                        query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        execution.checkpoint()
                    })
                    .await
                    .map_err(source)
                    .map_err(merged)?;
                Ok((stats, result, completion))
            })
            .await
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn apply<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    merge: &PreparedGraphEdgeMerge,
    policy: GraphEdgeMergePolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    returning: bool,
) -> Result<(GraphEdgeMergeStats, GraphEdgeMergeOutcome), Fault> {
    apply_with_output(
        transaction,
        database,
        cx,
        merge,
        policy,
        scope,
        execution,
        returning,
    )
    .map(|(stats, outcome, _)| (stats, outcome))
}

/// Native write programs use every distinct selected pair; the public singular
/// primitive and ON actions retain their independently useful one-pair contract.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_many<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    merge: &PreparedGraphEdgeMerge,
    policy: GraphEdgeMergePolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    returning: bool,
) -> Result<(GraphEdgeMergeStats, fgdb_gql::GraphWriteStepReceipt), Fault> {
    apply_with_output(
        transaction,
        database,
        cx,
        merge,
        policy,
        scope,
        execution,
        returning,
    )
    .map(|(stats, receipt, _)| (stats, receipt))
}

#[allow(clippy::too_many_arguments)]
fn apply_with_output<Output: EdgeMergeOutput, V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    merge: &PreparedGraphEdgeMerge,
    policy: GraphEdgeMergePolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    returning: bool,
) -> Result<(GraphEdgeMergeStats, Output, Vec<GraphValueRow>), Fault> {
    apply_with_input(
        transaction,
        database,
        cx,
        merge,
        policy,
        scope,
        execution,
        returning,
        None,
        None,
    )
}

/// Resolve a complete, already-masked chain input through the ordinary native
/// pair reducer. The caller owns selection and its cumulative counters; this
/// seam issues no permit, charges no public rows and never completes.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_selected<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    merge: &PreparedGraphEdgeMerge,
    policy: GraphEdgeMergePolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    selected: GqlQueryExecution<GraphValueRow>,
    frozen: Option<&mut crate::write_txn::FrozenCreations>,
) -> Result<(GraphEdgeMergeStats, EdgeMergeRows, Vec<GraphValueRow>), Fault> {
    apply_with_input(
        transaction,
        database,
        cx,
        merge,
        policy,
        scope,
        execution,
        false,
        Some((selected, None)),
        frozen,
    )
}

/// Resolve one already-masked MATCH occurrence through the SAME endpoint
/// admission, existence scan and original-image staging as ordinary MERGE.
/// The caller owns the frozen input and cumulative source/evaluator prefix.
/// This crate-private seam grants no new permit and never re-runs MATCH.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_occurrence<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    merge: &PreparedGraphEdgeMerge,
    policy: GraphEdgeMergePolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    selected: GqlQueryExecution<GraphValueRow>,
    occurrence: usize,
) -> Result<(GraphEdgeMergeStats, crate::write_txn::EdgeUpsertOccurrences), Fault> {
    apply_with_input(
        transaction,
        database,
        cx,
        merge,
        policy,
        scope,
        execution,
        false,
        Some((selected, Some(occurrence))),
        None,
    )
    .map(|(stats, outcomes, _)| (stats, outcomes))
}

#[allow(clippy::too_many_arguments)]
fn apply_with_input<Output: EdgeMergeOutput, V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    merge: &PreparedGraphEdgeMerge,
    policy: GraphEdgeMergePolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    returning: bool,
    selected: Option<(GqlQueryExecution<GraphValueRow>, Option<usize>)>,
    mut frozen: Option<&mut crate::write_txn::FrozenCreations>,
) -> Result<(GraphEdgeMergeStats, Output, Vec<GraphValueRow>), Fault> {
    if !scope.allows_relation(merge.relation()) {
        return Err(source(WriteTxnError::Authorization(Error::ScopeDenied)));
    }
    // The MERGE map is an existence predicate (fgdb-at06q): a key the
    // capability cannot see would let its stored values decide a match.
    // Refuse before selection, never compare a hidden field.
    if merge
        .properties()
        .iter()
        .any(|(key, _)| !scope.allows_property(*key))
    {
        return Err(source(WriteTxnError::Authorization(Error::ScopeDenied)));
    }
    let original_row = selected.as_ref().and_then(|(_, row)| *row);
    let proposal: super::super::EdgeMergeProposal<Output> = cx.with_restriction(|| {
        let database = RefCell::new(&mut *database);
        let controls = RefCell::new(&mut *execution);
        collect_edge_merge_output(
            merge,
            policy,
            |allowance| {
                if let Some((selected, _)) = selected {
                    return Ok(selected);
                }
                merge.select_governed(
                    None,
                    allowance,
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
                    || {
                        cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        controls.borrow_mut().checkpoint()
                    },
                )
            },
            |src, dst| {
                // The selected pair is the entire existence domain. Recheck
                // original endpoints, never a client-selected or masked image.
                for vertex in [src, dst] {
                    cx.checkpoint()
                        .map_err(WriteTxnError::Interrupted)
                        .map_err(GqlQueryError::Interrupted)?;
                    let visible = match frozen
                        .as_deref()
                        .and_then(|held| held.index.get(&vertex).map(|at| &held.vertices[*at]))
                    {
                        Some((_, labels, _)) => {
                            // This is the same logical endpoint lookup as
                            // Execution::vertex. The held image has already
                            // passed ordinary native before/after admission.
                            controls
                                .borrow_mut()
                                .checkpoint()
                                .map_err(GqlQueryError::Source)?;
                            scope.allows_vertex(labels)
                        }
                        None => controls
                            .borrow_mut()
                            .vertex(transaction, &database.borrow(), vertex)
                            .map_err(GqlQueryError::Source)?
                            .is_some(),
                    };
                    if !visible {
                        return Err(GqlQueryError::Source(WriteTxnError::Authorization(
                            Error::ScopeDenied,
                        )));
                    }
                }
                // Retain the native incidence gaps even when no edge exists.
                // Raw records stay private. The exact pair/relation source
                // checks topology before reading payloads; only its admitted
                // edges reach the shared counts and uniqueness reducer.
                let edges = transaction
                    .merge_candidate_edges(&database.borrow(), merge, src, dst, || {
                        cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        controls.borrow_mut().poll()
                    })
                    .map_err(|error| error.map_source(redacted))?;
                let mut admitted = Vec::new();
                for record in edges {
                    cx.checkpoint()
                        .map_err(WriteTxnError::Interrupted)
                        .map_err(GqlQueryError::Interrupted)?;
                    controls
                        .borrow_mut()
                        .poll()
                        .map_err(GqlQueryError::Interrupted)?;
                    if record.entry.relation == merge.relation()
                        && merge
                            .accepts_orientation((src, dst), (record.entry.src, record.entry.dst))
                    {
                        admitted.push(record);
                    }
                }
                Ok(admitted)
            },
            |row| {
                let row = original_row.unwrap_or(row);
                database
                    .borrow_mut()
                    .allocate_identity(cx, GraphInsertRequest::Edge { row, edge: 0 })
            },
            || {
                cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                controls.borrow_mut().checkpoint()
            },
        )
    })?;
    if let Some(batch) = proposal.creation {
        if let Some(held) = frozen.as_deref_mut() {
            super::chain::stage_batch(transaction, database, cx, batch, Some(held), execution)
                .map_err(source)?;
        } else {
            for row in batch.rows {
                cx.checkpoint()
                    .map_err(WriteTxnError::Interrupted)
                    .map_err(source)?;
                execution.checkpoint().map_err(source)?;
                stage(transaction, database, batch.relation, row, execution).map_err(source)?;
            }
        }
    }
    let result_rows = proposal.outcome.result_rows();
    if returning && result_rows != 0 {
        execution
            .permit
            .charge_rows_at((execution.clock)(), result_rows)
            .map_err(|error| source(WriteTxnError::Authorization(error)))?;
    }
    Ok((proposal.stats, proposal.outcome, proposal.selected))
}

#[cfg(test)]
#[path = "edge_merge_tests.rs"]
mod tests;
