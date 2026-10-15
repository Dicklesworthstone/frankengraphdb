//! All-target vertex MERGE in authorized programs, without synthetic RETURN.
//!
//! The ordinary collector owns matching, deduplication, absence witnesses and
//! creation admission. This host supplies the same masked overlay and original
//! before/after staging as the authorized singular and RETURN entrypoints.
//! The enclosing program owns rollback, the live permit and sole completion.

use super::{Database, Execution, Vfs, WriteTxn, WriteTxnError};
use crate::WriteBatch;
use crate::write_txn::authorized::{selection, stage};
use crate::write_txn::{VertexMergeProposal, VertexMergeTargets, collect_vertex_merge_output};
use fgdb_gql::insertion::{GraphInsertBatch, GraphInsertIntent};
use fgdb_gql::{
    GlaExecutionEvent, GlaExecutionStats, GlaLimitDimension, GlaLimitExceeded, GqlQueryError,
    GqlQueryPolicy, GraphVertexMergeError, GraphVertexMergePolicy, GraphVertexMergeStats,
    GraphWriteStepReceipt, PreparedGraphVertexMerge,
};
use fgdb_types::QueryCx;
use fgdb_warden::PlannerPredicates;
use std::cell::RefCell;

type MergeFault = GqlQueryError<GraphVertexMergeError<WriteTxnError, WriteTxnError>, WriteTxnError>;

// Source errors and interruption retain the same typed nesting as the singular
// program arm. No privileged execution or graph rematch is a fallback.
fn source(error: WriteTxnError) -> MergeFault {
    GqlQueryError::Source(GraphVertexMergeError::Source(error))
}

fn event<E, Clock: FnMut() -> u64>(
    cx: &QueryCx,
    execution: &mut Execution<'_, '_, Clock>,
    policy: GqlQueryPolicy,
    stats: &mut GlaExecutionStats,
    event: GlaExecutionEvent,
) -> Result<(), GqlQueryError<E, WriteTxnError>> {
    cx.checkpoint()
        .map_err(WriteTxnError::Interrupted)
        .map_err(GqlQueryError::Interrupted)?;
    execution.checkpoint().map_err(GqlQueryError::Interrupted)?;
    let work = u128::from(stats.work_units) + 1;
    let scratch =
        u128::from(stats.scratch_entries) + u128::from(event == GlaExecutionEvent::ScratchEntry);
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

#[allow(clippy::too_many_arguments)]
fn select<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    merge: &PreparedGraphVertexMerge,
    policy: GraphVertexMergePolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
) -> Result<VertexMergeProposal<VertexMergeTargets>, MergeFault> {
    let database = RefCell::new(database);
    let controls = RefCell::new(execution);
    collect_vertex_merge_output(
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
}

fn stage_creation<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    relation: fgdb_delta_types::RelationId,
    creation: Option<GraphInsertBatch>,
    execution: &mut Execution<'_, '_, Clock>,
) -> Result<(), WriteTxnError> {
    if let Some(creation) = creation {
        for intent in creation.into_intents() {
            cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
            execution.checkpoint()?;
            let GraphInsertIntent::Vertex {
                vertex,
                labels,
                properties,
            } = intent
            else {
                unreachable!("validated vertex MERGE contains no edge creation")
            };
            let mut batch = WriteBatch::new(relation);
            batch.create_vertex(vertex, labels, properties);
            for row in batch.rows {
                stage(transaction, database, batch.relation, row, execution)?;
            }
        }
    }
    Ok(())
}

// Stats-only programs discard the already-admitted target owner, not synthetic
// rows. Returning programs pre-admit the WHOLE signed receipt, then transfer
// the ordered set under the remaining native work/scratch allowance.
fn outcomes<E, Clock: FnMut() -> u64>(
    targets: VertexMergeTargets,
    cx: &QueryCx,
    execution: &mut Execution<'_, '_, Clock>,
    policy: GqlQueryPolicy,
    evaluator: &mut GlaExecutionStats,
    returning: bool,
) -> Result<Option<Vec<fgdb_gql::GraphVertexMergeOutcome>>, GqlQueryError<E, WriteTxnError>> {
    if !returning {
        return Ok(None);
    }
    let rows = u64::try_from(targets.len()).map_err(|_| {
        GqlQueryError::Interrupted(WriteTxnError::Authorization(fgdb_warden::Error::TooLarge))
    })?;
    execution.checkpoint().map_err(GqlQueryError::Interrupted)?;
    execution
        .permit
        .charge_rows_at((execution.clock)(), rows)
        .map_err(WriteTxnError::Authorization)
        .map_err(GqlQueryError::Interrupted)?;
    targets
        .into_outcomes(|kind| event(cx, execution, policy, evaluator, kind))
        .map(Some)
}

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
) -> Result<(GraphVertexMergeStats, Option<GraphWriteStepReceipt>), MergeFault> {
    cx.with_restriction(|| {
        let proposal = select(transaction, database, cx, merge, policy, scope, execution)?;
        stage_creation(
            transaction,
            database,
            cx,
            merge.relation(),
            proposal.creation,
            execution,
        )
        .map_err(source)?;
        let mut stats = proposal.stats;
        let receipt = outcomes(
            proposal.outcome,
            cx,
            execution,
            policy.query,
            &mut stats.evaluator,
            returning,
        )?
        .map(|outcomes| match outcomes.as_slice() {
            [outcome] => GraphWriteStepReceipt::VertexMerge { outcome: *outcome },
            _ => GraphWriteStepReceipt::VertexMergeBatch { outcomes },
        });
        Ok((stats, receipt))
    })
}

type UpsertFault =
    GqlQueryError<fgdb_gql::GraphVertexUpsertError<WriteTxnError, WriteTxnError>, WriteTxnError>;

fn upsert_staging(error: WriteTxnError) -> UpsertFault {
    GqlQueryError::Source(fgdb_gql::GraphVertexUpsertError::Staging(error))
}

// ON/SET shares the very same frozen target owner as plain MERGE. All reads
// and staging still use the established native evaluator and authorized host.
#[allow(clippy::too_many_arguments)]
pub(super) fn upsert<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    upsert: &fgdb_gql::PreparedGraphVertexUpsert,
    policy: fgdb_gql::GraphVertexUpsertPolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    returning: bool,
) -> Result<(fgdb_gql::GraphWriteStepStats, Option<GraphWriteStepReceipt>), UpsertFault> {
    use crate::write_txn::vertex_upsert_actions;
    use fgdb_delta_types::ElementId;
    use fgdb_gql::{GraphVertexUpsertBranch, GraphVertexUpsertError, GraphVertexUpsertStats};
    cx.with_restriction(|| {
        let proposal = select(
            transaction,
            database,
            cx,
            upsert.merge(),
            policy.merge,
            scope,
            execution,
        )
        .map_err(|error| error.map_source(GraphVertexUpsertError::Merge))?;
        let targets = proposal.outcome;
        let target_vertices = u64::try_from(targets.len()).map_err(|_| {
            GqlQueryError::Source(GraphVertexUpsertError::Merge(
                GraphVertexMergeError::AccountingOverflow,
            ))
        })?;
        let (branch, selected) = if targets.created.is_some() {
            (GraphVertexUpsertBranch::Create, upsert.on_create())
        } else {
            (GraphVertexUpsertBranch::Match, upsert.on_match())
        };
        // Pre-admit EVERY original action before any target's effects, even if
        // a later clause overwrites it or it is equal to the stored value.
        let observed = u128::from(target_vertices)
            .checked_mul(selected.len() as u128 + upsert.after().len() as u128)
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
        stage_creation(
            transaction,
            database,
            cx,
            upsert.merge().relation(),
            proposal.creation,
            execution,
        )
        .map_err(upsert_staging)?;
        let mut stats = GraphVertexUpsertStats {
            merge: proposal.stats,
            branch,
            action_effects: observed as u64,
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
            let completed = vertex_upsert_actions::<WriteTxnError, WriteTxnError, WriteTxnError>(
                upsert,
                policy,
                stats.merge,
                outcome,
                || {
                    cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                    controls.borrow_mut().checkpoint()
                },
                |key, control| {
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
                            controls.borrow_mut().poll().map_err(GqlQueryError::Interrupted)
                        },
                        control,
                        upsert_staging,
                    )
                },
                |batch| {
                    let mut state = state.borrow_mut();
                    let (transaction, database) = &mut *state;
                    let mut execution = controls.borrow_mut();
                    for row in batch.rows {
                        cx.checkpoint()
                            .map_err(WriteTxnError::Interrupted)
                            .map_err(upsert_staging)?;
                        execution.checkpoint().map_err(upsert_staging)?;
                        stage(transaction, database, batch.relation, row, &mut execution)
                            .map_err(upsert_staging)?;
                    }
                    Ok(())
                },
            )?;
            // The action evaluator starts with the previous target's usage;
            // never reset native work/scratch or the live signed allowance.
            stats.merge.evaluator = completed.merge.evaluator;
        }
        let receipt = outcomes(
            targets,
            cx,
            execution,
            policy.merge.query,
            &mut stats.merge.evaluator,
            returning,
        )?
        .map(|outcomes| match outcomes.as_slice() {
            [outcome] => GraphWriteStepReceipt::VertexUpsert { outcome: *outcome },
            _ => GraphWriteStepReceipt::VertexUpsertBatch { outcomes },
        });
        // The shared bulk meter validates branch, real selection, complete
        // actions and distinct modified targets before installing any counters.
        Ok((
            fgdb_gql::GraphWriteStepStats::VertexUpsertBatch {
                stats,
                target_vertices,
            },
            receipt,
        ))
    })
}
