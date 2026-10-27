//! Frozen UNWIND field updates retain the authorized source and original
//! per-field staging checks while separating selection from ordered effects.

use super::{
    Database, Execution, PlannerPredicates, QueryCx, RefCell, Vfs, WriteTxn, WriteTxnError,
    select_proposal, source, stage_proposal,
};
use fgdb_gql::{
    GraphMutationIntent, GraphMutationProgramError, GraphWriteProgramError,
    GraphWriteProgramPolicy, GraphWriteProgramStats, GraphWriteStatement, GraphWriteStepError,
    GraphWriteStepReceipt, GraphWriteStepStats, PreparedGraphWriteProgram,
};
use std::collections::BTreeMap;

type Fault = GraphWriteProgramError<WriteTxnError, WriteTxnError, WriteTxnError>;

// The caller owns one private workspace, permit and completion. No field is
// staged until all native MATCH/RHS proposals have read that entry workspace.
// Staging still authorizes every original intent, even a forbidden no-op that
// a later row overwrites or a label action that leaves the capability's scope.
// Statements with disjoint element targets can authorize against independent
// native row workspaces, then compose their checked intents once. Two statements
// touching the same element retain sequential before/after checks. Mixed
// vertex/edge programs also retain those checks: updating an edge's endpoint
// may change its authorization image even when the direct targets differ.
#[allow(clippy::too_many_arguments)]
pub(in super::super) fn apply_frozen_program<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    program: &PreparedGraphWriteProgram,
    policy: GraphWriteProgramPolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    returning: bool,
) -> Result<(GraphWriteProgramStats, Vec<GraphWriteStepReceipt>), Fault> {
    let mut proposals = Vec::new();
    // The public authorized program starts with an empty workspace. Keep any
    // future caller with a retained prefix on the existing staging path; only
    // the empty-prefix, single-relation case has the row-count cost law below.
    let mut independent = transaction.staged.is_empty() && !program.freezes_mixed_update_input();
    let mut relation = None;
    let mut target_owners = BTreeMap::new();
    let stats = cx.with_restriction(|| {
        let controls = RefCell::new(&mut *execution);
        program.execute_governed::<WriteTxnError, WriteTxnError, WriteTxnError>(
            policy,
            |statement, input, remaining| {
                let GraphWriteStatement::Mutation(mutation) = input else {
                    return Err(GraphWriteStepError::Mutation(source(
                        WriteTxnError::UnsupportedAtomicMutation,
                    )));
                };
                let proposal = select_proposal(
                    transaction,
                    database,
                    cx,
                    mutation,
                    remaining.mutations,
                    scope,
                    &mut **controls.borrow_mut(),
                )
                .map_err(GraphWriteStepError::Mutation)?;
                if independent {
                    if relation.is_some_and(|first| first != mutation.relation()) {
                        independent = false;
                    } else {
                        relation = Some(mutation.relation());
                        // This is eligibility metadata over already-admitted
                        // visible intents, not another graph read. Poll without
                        // changing the sequential fallback's signed charges.
                        for intent in proposal.intents() {
                            cx.checkpoint()
                                .map_err(WriteTxnError::Interrupted)
                                .and_then(|()| controls.borrow_mut().poll())
                                .map_err(source)
                                .map_err(GraphWriteStepError::Mutation)?;
                            let target = match intent {
                                GraphMutationIntent::Property { vertex, .. }
                                | GraphMutationIntent::Label { vertex, .. }
                                | GraphMutationIntent::StoredProperty {
                                    target: fgdb_delta_types::ElementId::Vertex(vertex),
                                    ..
                                } => fgdb_delta_types::ElementId::Vertex(*vertex),
                                GraphMutationIntent::EdgeProperty { edge, .. }
                                | GraphMutationIntent::StoredProperty {
                                    target: fgdb_delta_types::ElementId::Edge(edge),
                                    ..
                                } if program.freezes_edge_update_input() => {
                                    fgdb_delta_types::ElementId::Edge(*edge)
                                }
                                _ => {
                                    independent = false;
                                    break;
                                }
                            };
                            let owner = target_owners.entry(target).or_insert(statement);
                            if *owner != statement {
                                independent = false;
                                break;
                            }
                        }
                    }
                }
                let stats = proposal.stats();
                proposals.push((statement, proposal, remaining.mutations));
                Ok(GraphWriteStepStats::Mutation(stats))
            },
            || {
                cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                controls.borrow_mut().checkpoint()
            },
        )
    })?;
    drop(target_owners);

    let mut receipts = Vec::new();
    let mut batches = Vec::new();
    for (statement, proposal, remaining) in proposals {
        let GraphWriteStatement::Mutation(mutation) = &program.statements()[statement] else {
            return Err(GraphMutationProgramError::InvalidStatistics { statement }.into());
        };
        let stage = |transaction: &mut WriteTxn| {
            stage_proposal(
                transaction,
                database,
                cx,
                mutation,
                proposal,
                remaining,
                scope,
                execution,
                returning,
            )
        };
        let staged = if independent {
            let row = crate::write_txn::MutationProgramWorkspace::new(transaction);
            let result = stage(row.txn);
            if result.is_ok() {
                for batch in &row.txn.staged[row.staged_len..] {
                    cx.checkpoint()
                        .map_err(WriteTxnError::Interrupted)
                        .and_then(|()| execution.poll())
                        .map_err(|error| GraphMutationProgramError::Statement {
                            statement,
                            source: source(error),
                        })?;
                    batches.push(batch.clone());
                }
            }
            // Restores effects, retaining every read and preparation witness,
            // the authorization digest and the single cumulative signed permit.
            drop(row);
            result
        } else {
            stage(transaction)
        };
        let (_, targets, edges) =
            staged.map_err(|source| GraphMutationProgramError::Statement { statement, source })?;
        if returning {
            receipts.push(GraphWriteStepReceipt::Mutation { targets, edges });
        }
    }
    if independent && !batches.is_empty() {
        // Each statement's original field sequence already passed the real
        // native evaluator and before/after authorization. Disjoint elements
        // cannot alter one another's images. A single ordinary preparation of
        // those exact raw intents preserves canonical effects and still runs
        // all native checks; nothing publishes before the caller's completion.
        let compose = || -> Result<(), WriteTxnError> {
            let rows = batches
                .iter()
                .try_fold(0_u64, |total, batch: &crate::WriteBatch| {
                    cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                    u64::try_from(batch.rows.len())
                        .ok()
                        .and_then(|count| total.checked_add(count))
                        .ok_or(WriteTxnError::Authorization(fgdb_warden::Error::TooLarge))
                })?;
            // Additional preparation work, counted before evaluating it. This
            // path has one relation and no entry prefix, so each input is read
            // exactly once by the final ordered evaluator.
            execution.work(rows)?;
            transaction
                .write_ordered(database, batches)
                .map_err(crate::write_txn::authorized::redacted)
        };
        compose().map_err(|source| GraphMutationProgramError::Interrupted {
            completed_statements: stats.completed_statements,
            source,
        })?;
    }
    // The program meter has already accounted for every original proposal.
    // Recheck the same live permit and QueryCx after the final field is staged;
    // any refusal still drops the caller's unpublished workspace.
    cx.checkpoint()
        .map_err(WriteTxnError::Interrupted)
        .and_then(|()| execution.checkpoint())
        .map_err(|source| GraphMutationProgramError::Interrupted {
            completed_statements: stats.completed_statements,
            source,
        })?;
    Ok((stats, receipts))
}
