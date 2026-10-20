//! Frozen UNWIND vertex updates retain the authorized source and original
//! per-field staging checks while separating selection from ordered effects.

use super::{
    Database, Execution, PlannerPredicates, QueryCx, RefCell, Vfs, WriteTxn, WriteTxnError,
    select_proposal, source, stage_proposal,
};
use fgdb_gql::{
    GraphMutationProgramError, GraphWriteProgramError, GraphWriteProgramPolicy,
    GraphWriteProgramStats, GraphWriteStatement, GraphWriteStepError, GraphWriteStepReceipt,
    GraphWriteStepStats, PreparedGraphWriteProgram,
};

type Fault = GraphWriteProgramError<WriteTxnError, WriteTxnError, WriteTxnError>;

// The caller owns one private workspace, permit and completion. No field is
// staged until all native MATCH/RHS proposals have read that entry workspace.
// Staging still authorizes every original intent, even a forbidden no-op that
// a later row overwrites or a label action that leaves the capability's scope.
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

    let mut receipts = Vec::new();
    for (statement, proposal, remaining) in proposals {
        let GraphWriteStatement::Mutation(mutation) = &program.statements()[statement] else {
            return Err(GraphMutationProgramError::InvalidStatistics { statement }.into());
        };
        let (_, targets, edges) = stage_proposal(
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
        .map_err(|source| GraphMutationProgramError::Statement { statement, source })?;
        if returning {
            receipts.push(GraphWriteStepReceipt::Mutation { targets, edges });
        }
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
