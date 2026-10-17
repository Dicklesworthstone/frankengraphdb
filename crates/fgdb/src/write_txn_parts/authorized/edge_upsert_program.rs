//! Occurrence-preserving authorized relationship writes. The native MERGE
//! collector and action bytecode remain the only semantic owners; the masked
//! source, endpoint proof and original-image staging remain the only readers
//! and writers. No occurrence obtains its own transaction or signed permit.

use super::{Database, Execution, Vfs, WriteTxn, WriteTxnError};
use crate::write_txn::authorized::{edge_merge, selection, stage};
use crate::write_txn::edge_upsert_actions;
use fgdb_delta_types::ElementId;
use fgdb_gql::{
    GlaExecutionEvent, GlaExecutionStats, GlaLimitDimension, GlaLimitExceeded, GqlExecutionStats,
    GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphEdgeMergeError, GraphEdgeMergeOutcome,
    GraphEdgeUpsertBatchStats, GraphEdgeUpsertBranch, GraphEdgeUpsertError, GraphEdgeUpsertPolicy,
    GraphWriteStepReceipt, GraphWriteStepStats, PreparedGraphEdgeUpsert,
};
use fgdb_types::QueryCx;
use fgdb_warden::{Error, PlannerPredicates};
use std::cell::RefCell;
use std::collections::BTreeSet;

type Fault = GqlQueryError<GraphEdgeUpsertError<WriteTxnError, WriteTxnError>, WriteTxnError>;

fn staging(error: WriteTxnError) -> Fault {
    GqlQueryError::Source(GraphEdgeUpsertError::Staging(error))
}

fn invalid() -> Fault {
    GqlQueryError::Source(GraphEdgeUpsertError::Merge(
        GraphEdgeMergeError::InvalidSourceStatistics,
    ))
}

fn event<Clock: FnMut() -> u64>(
    cx: &QueryCx,
    execution: &mut Execution<'_, '_, Clock>,
    policy: GqlQueryPolicy,
    stats: &mut GlaExecutionStats,
    kind: GlaExecutionEvent,
) -> Result<(), Fault> {
    cx.checkpoint()
        .map_err(WriteTxnError::Interrupted)
        .map_err(GqlQueryError::Interrupted)?;
    execution.checkpoint().map_err(GqlQueryError::Interrupted)?;
    let work = u128::from(stats.work_units) + 1;
    let scratch =
        u128::from(stats.scratch_entries) + u128::from(kind == GlaExecutionEvent::ScratchEntry);
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

struct Batch {
    stats: GraphEdgeUpsertBatchStats,
    target_edges: u64,
    // Retained only for an identity receipt or a final RETURN, never stats-only.
    outcomes: Vec<GraphEdgeMergeOutcome>,
}

#[allow(clippy::too_many_arguments)]
fn apply_all<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    input: &PreparedGraphEdgeUpsert,
    policy: GraphEdgeUpsertPolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    retain_outcomes: bool,
) -> Result<Batch, Fault> {
    // Check before the initial MATCH, including NoInput. A hidden MERGE key
    // must never control existence, creation, errors or an observable limit.
    if !scope.rights().can_read() {
        return Err(staging(WriteTxnError::Authorization(
            Error::PermissionDenied,
        )));
    }
    if !scope.allows_relation(input.merge().relation())
        || input
            .merge()
            .properties()
            .iter()
            .any(|(key, _)| !scope.allows_property(*key))
    {
        return Err(staging(WriteTxnError::Authorization(Error::ScopeDenied)));
    }
    cx.with_restriction(|| {
        let selected = {
            let controls = RefCell::new(&mut *execution);
            selection::select_overlay(
                transaction,
                database,
                cx,
                input.merge().selection(),
                scope,
                policy.merge.query,
                &controls,
            )
            .map_err(|error| {
                error.map_source(|error| {
                    GraphEdgeUpsertError::Merge(GraphEdgeMergeError::Source(error))
                })
            })?
        };
        if u64::try_from(selected.value.len()).ok() != Some(selected.rows.result_rows) {
            return Err(invalid());
        }
        let mut stats = GraphEdgeUpsertBatchStats {
            match_selection: selected.rows,
            overlay_edges: 0,
            created_edges: 0,
            matched_occurrences: 0,
            action_effects: 0,
            evaluator: selected.evaluator,
        };
        let mut targets = BTreeSet::new();
        let mut outcomes = Vec::new();
        event(
            cx,
            execution,
            policy.merge.query,
            &mut stats.evaluator,
            GlaExecutionEvent::Work,
        )?;
        for (row_at, row) in selected.value.into_iter().enumerate() {
            event(
                cx,
                execution,
                policy.merge.query,
                &mut stats.evaluator,
                GlaExecutionEvent::ScratchEntry,
            )?;
            let records = stats
                .match_selection
                .snapshot_records
                .checked_add(stats.overlay_edges)
                .ok_or_else(invalid)?;
            let mut remaining = policy.merge;
            remaining.max_created_edges = policy
                .merge
                .max_created_edges
                .checked_sub(stats.created_edges)
                .ok_or_else(invalid)?;
            // This tuple already passed the one masked selection. Transfer it
            // and the cumulative prefix to the ordinary collector, not a fake
            // pattern, a text rewrite, or an unchecked privileged graph read.
            let (merge_stats, resolved) = edge_merge::apply_occurrence(
                transaction,
                database,
                cx,
                input.merge(),
                remaining,
                scope,
                execution,
                GqlQueryExecution {
                    value: vec![row],
                    rows: GqlExecutionStats {
                        snapshot_records: records,
                        result_rows: 1,
                    },
                    evaluator: stats.evaluator,
                },
                row_at,
            )
            .map_err(|error| {
                error.map_source(|error| {
                    GraphEdgeUpsertError::Merge(match error {
                        GraphEdgeMergeError::CreationLimit { observed, .. } => {
                            GraphEdgeMergeError::CreationLimit {
                                limit: policy.merge.max_created_edges,
                                observed: u128::from(stats.created_edges) + observed,
                            }
                        }
                        error => error,
                    })
                })
            })?;
            stats.evaluator = merge_stats.evaluator;
            stats.overlay_edges = stats
                .overlay_edges
                .checked_add(merge_stats.overlay_edges)
                .ok_or_else(invalid)?;
            stats.created_edges = stats
                .created_edges
                .checked_add(merge_stats.created_edges)
                .ok_or_else(invalid)?;
            // Freeze all parallel matches for THIS occurrence before changing
            // any key. The next occurrence must resolve against the new state.
            for outcome in resolved.0 {
                let (edge, branch) = match outcome {
                    GraphEdgeMergeOutcome::Matched(edge) => (edge, GraphEdgeUpsertBranch::Match),
                    GraphEdgeMergeOutcome::Created(edge) => (edge, GraphEdgeUpsertBranch::Create),
                    GraphEdgeMergeOutcome::NoInput => return Err(invalid()),
                };
                let actions = u128::from(stats.action_effects) + input.action_count(branch) as u128;
                if actions > u128::from(policy.max_actions) {
                    return Err(GqlQueryError::Source(GraphEdgeUpsertError::ActionLimit {
                        limit: policy.max_actions,
                        observed: actions,
                    }));
                }
                let state = RefCell::new((&mut *transaction, &mut *database));
                let controls = RefCell::new(&mut *execution);
                let mut prefix = merge_stats;
                prefix.evaluator = stats.evaluator;
                let completed = edge_upsert_actions::<WriteTxnError, WriteTxnError, _>(
                    input,
                    policy,
                    prefix,
                    outcome,
                    || {
                        cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        controls.borrow_mut().checkpoint()
                    },
                    |edge, key, control| {
                        if !scope.allows_property(key) {
                            control(GlaExecutionEvent::Work)?;
                            control(GlaExecutionEvent::ScratchEntry)?;
                            return Ok(fgdb_types::CanonicalScalar::Null);
                        }
                        let state = state.borrow();
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
                            stage(transaction, database, batch.relation, row, &mut execution)
                                .map_err(staging)?;
                        }
                        Ok(())
                    },
                )?;
                stats.evaluator = completed.evaluator;
                stats.action_effects = actions as u64;
                if matches!(outcome, GraphEdgeMergeOutcome::Matched(_)) {
                    stats.matched_occurrences = stats
                        .matched_occurrences
                        .checked_add(1)
                        .ok_or_else(invalid)?;
                }
                event(
                    cx,
                    execution,
                    policy.merge.query,
                    &mut stats.evaluator,
                    GlaExecutionEvent::Work,
                )?;
                if input.action_count(branch) != 0 && !targets.contains(&edge) {
                    event(
                        cx,
                        execution,
                        policy.merge.query,
                        &mut stats.evaluator,
                        GlaExecutionEvent::ScratchEntry,
                    )?;
                    targets.insert(edge);
                }
                if retain_outcomes {
                    event(
                        cx,
                        execution,
                        policy.merge.query,
                        &mut stats.evaluator,
                        GlaExecutionEvent::ScratchEntry,
                    )?;
                    outcomes.push(outcome);
                }
            }
        }
        event(
            cx,
            execution,
            policy.merge.query,
            &mut stats.evaluator,
            GlaExecutionEvent::Work,
        )?;
        Ok(Batch {
            stats,
            target_edges: u64::try_from(targets.len()).map_err(|_| invalid())?,
            outcomes,
        })
    })
}

// The surrounding program alone owns rollback and native completion. A
// stats-only step never allocates a final receipt or spends signed result rows.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_many<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    input: &PreparedGraphEdgeUpsert,
    policy: GraphEdgeUpsertPolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    returning: bool,
) -> Result<(GraphWriteStepStats, Option<GraphWriteStepReceipt>), Fault> {
    let Batch {
        mut stats,
        target_edges,
        outcomes,
    } = apply_all(
        transaction,
        database,
        cx,
        input,
        policy,
        scope,
        execution,
        returning,
    )?;
    let receipt = if returning {
        let rows = u64::try_from(outcomes.len()).map_err(|_| invalid())?;
        execution
            .permit
            .charge_rows_at((execution.clock)(), rows)
            .map_err(|error| staging(WriteTxnError::Authorization(error)))?;
        let receipt = match outcomes.len() {
            0 => GraphWriteStepReceipt::EdgeUpsert {
                outcome: GraphEdgeMergeOutcome::NoInput,
            },
            1 => GraphWriteStepReceipt::EdgeUpsert {
                outcome: outcomes[0],
            },
            _ => {
                let mut created = Vec::new();
                for outcome in &outcomes {
                    event(
                        cx,
                        execution,
                        policy.merge.query,
                        &mut stats.evaluator,
                        GlaExecutionEvent::Work,
                    )?;
                    if let GraphEdgeMergeOutcome::Created(edge) = outcome {
                        event(
                            cx,
                            execution,
                            policy.merge.query,
                            &mut stats.evaluator,
                            GlaExecutionEvent::ScratchEntry,
                        )?;
                        created.push(*edge);
                    }
                }
                GraphWriteStepReceipt::EdgeUpsertBatch { outcomes, created }
            }
        };
        Some(receipt)
    } else {
        None
    };
    Ok((
        GraphWriteStepStats::EdgeUpsertBatch {
            stats,
            target_edges,
        },
        receipt,
    ))
}

#[path = "edge_upsert_program_returning.rs"]
mod returning;
