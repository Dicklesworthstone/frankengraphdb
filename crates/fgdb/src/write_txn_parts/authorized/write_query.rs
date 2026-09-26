//! A final masked result query is part of the private write's acceptance, not a
//! separate post-commit read. The program owner still performs the only commit.

use super::super::super::super::selection;
use super::{
    Authority, CapabilityToken, Database, Error, Execution, GraphWriteProgramPolicy,
    GraphWriteProgramStats, PlannerPredicates, PreparedGraphWriteProgram, Vfs,
    WriteTxn, WriteTxnError, preflight,
};
use fgdb_gql::algebra::{GlaOutput, PreparedGraphPattern};
use fgdb_gql::{
    GlaExecutionLimits, GlaLimitDimension, GqlBudgetDimension, GqlExecutionBudget,
    GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphWriteQueryError,
};
use fgdb_types::{CommitCx, EmbeddedTxnCompletion, QueryCx, TxnCx};
use std::cell::RefCell;

type Fault = GraphWriteQueryError<WriteTxnError, WriteTxnError, WriteTxnError>;
type QueryFault = GqlQueryError<WriteTxnError, WriteTxnError>;
type ResultValue<Row> = (GraphWriteProgramStats, GqlQueryExecution<Row>);
type Receipt<Row> = (GraphWriteProgramStats, GqlQueryExecution<Row>, EmbeddedTxnCompletion);

fn query_source(error: WriteTxnError) -> Fault {
    Fault::Query(GqlQueryError::Source(error))
}

// Subtract the exact successful program's cumulative native totals. These are
// query/proposal units; signed binding/staging/completion charges live in the
// separate, shared permit and must NOT be substituted for these counters.
fn remaining(policy: GqlQueryPolicy, used: GraphWriteProgramStats) -> Result<GqlQueryPolicy, Fault> {
    let invalid = || query_source(WriteTxnError::AuthorizedMutationRefused);
    let subtract = |limit: Option<u64>, used: u64| -> Result<Option<u64>, Fault> {
        limit.map(|limit| limit.checked_sub(used).ok_or_else(&invalid)).transpose()
    };
    let records = subtract(policy.rows.max_snapshot_records(), used.selection.snapshot_records)?;
    let rows = subtract(policy.rows.max_result_rows(), used.selection.result_rows)?;
    let rows = match (records, rows) {
        (None, None) => GqlExecutionBudget::UNLIMITED,
        (Some(records), None) => GqlExecutionBudget::snapshot_records(records),
        (None, Some(rows)) => GqlExecutionBudget::result_rows(rows),
        (Some(records), Some(rows)) => GqlExecutionBudget::new(records, rows),
    };
    Ok(GqlQueryPolicy {
        rows,
        evaluator: GlaExecutionLimits::new(
            policy.evaluator.max_work_units.checked_sub(used.evaluator.work_units)
                .ok_or_else(&invalid)?,
            policy.evaluator.max_scratch_entries.checked_sub(used.evaluator.scratch_entries)
                .ok_or_else(invalid)?,
        ),
    })
}

// A failure from the residual allowance names the original WHOLE-operation
// ceiling and total observation, not the smaller final-query residual.
fn cumulative_error(mut error: QueryFault, policy: GqlQueryPolicy, used: GraphWriteProgramStats) -> Fault {
    match &mut error {
        GqlQueryError::Rows(exceeded) => {
            let (limit, before) = match exceeded.dimension {
                GqlBudgetDimension::SnapshotRecords =>
                    (policy.rows.max_snapshot_records(), used.selection.snapshot_records),
                GqlBudgetDimension::ResultRows =>
                    (policy.rows.max_result_rows(), used.selection.result_rows),
            };
            exceeded.limit = limit.unwrap_or(u64::MAX);
            let Some(observed) = exceeded.observed.checked_add(before) else {
                return query_source(WriteTxnError::Authorization(Error::TooLarge));
            };
            exceeded.observed = observed;
        }
        GqlQueryError::Evaluator(exceeded) => match exceeded.dimension {
            GlaLimitDimension::WorkUnits => {
                exceeded.limit = policy.evaluator.max_work_units;
                exceeded.observed += u128::from(used.evaluator.work_units);
            }
            GlaLimitDimension::ScratchEntries => {
                exceeded.limit = policy.evaluator.max_scratch_entries;
                exceeded.observed += u128::from(used.evaluator.scratch_entries);
            }
        },
        _ => {}
    }
    Fault::Query(error)
}

#[allow(clippy::too_many_arguments)]
fn result_query<V: Vfs + Clone, Row: GlaOutput, Clock: FnMut() -> u64>(
    transaction: &WriteTxn,
    database: &Database<V>,
    cx: &QueryCx,
    query: &PreparedGraphPattern<Row>,
    policy: GqlQueryPolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    stats: GraphWriteProgramStats,
) -> Result<ResultValue<Row>, Fault> {
    cx.with_restriction(|| {
        cx.checkpoint().map_err(WriteTxnError::Interrupted).map_err(query_source)?;
        execution.checkpoint().map_err(query_source)?;
        let budget = remaining(policy, stats)?;
        let result = {
            // End all sequential callback borrows before receipt admission or
            // native completion. This is the SAME permit as the preceding writes.
            let controls = RefCell::new(&mut *execution);
            selection::select_overlay(transaction, database, cx, query, scope, budget, &controls)
                .map_err(|error| cumulative_error(error, policy, stats))?
        };
        let rows = u64::try_from(result.value.len())
            .map_err(|_| query_source(WriteTxnError::Authorization(Error::TooLarge)))?;
        // Reserve only the actual final rows, not intermediate selected rows or
        // internal identity receipts. Even an empty result rechecks live authority.
        execution.permit.charge_rows_at((execution.clock)(), rows)
            .map_err(|error| query_source(WriteTxnError::Authorization(error)))?;
        Ok((stats, result))
    })
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute an authorized write program and a final result query atomically.
    ///
    /// The query reads the program's complete canonical staged graph through
    /// the SAME masked source and GLA evaluator as other authorized selections.
    /// It observes new IDs, changed fields and deletions, not an earlier basis
    /// or a fresh post-commit snapshot. Both vertex and edge fields are masked
    /// before predicates, expressions and projection. No mutation-ID receipts
    /// are delivered by this API; the supplied query determines the result rows.
    ///
    /// ReadWrite is mandatory before any graph access or identity reservation,
    /// including standalone creations followed by an empty result query. One
    /// live permit covers writes, result selection, final row admission and
    /// completion. Native record/selected-row/work/scratch ceilings in policy
    /// are cumulative across the program AND query; the final query receives
    /// only the remainder. Returned program and query statistics are separate
    /// phase totals (add them for native usage), not independently reset budgets.
    ///
    /// The complete result and its signed row charge are accepted BEFORE the
    /// sole native commit. A query error, quota refusal, expiry, cancellation or
    /// unwind discards the unpublished write prefix and all private results.
    /// No fallible authorization or query execution follows publication. Native
    /// committed/unknown/recovery failures retain their ordinary meaning in the
    /// Program error arm. An all-read execution completes ReadClosed; an empty
    /// final result does NOT cancel otherwise valid writes.
    ///
    /// This is a typed final-pattern query, not new RETURN syntax, a multi-request
    /// transaction or a streaming result lease. Resident overlay materialization,
    /// sequential-ID metadata and trusted issuer/branch/clock boundaries remain.
    /// It does not complete mandatory secure-view, durable audit/revocation, or SSI.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_write_program_then_query_authorized<Row: GlaOutput>(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        program: &PreparedGraphWriteProgram,
        query: &PreparedGraphPattern<Row>,
        policy: GraphWriteProgramPolicy,
        mut clock: impl FnMut() -> u64,
    ) -> Result<Receipt<Row>, Fault> {
        let refusal = |error| Fault::Program(preflight(WriteTxnError::Authorization(error)));
        if authority.namespace() != self.keys.namespace {
            return Err(refusal(Error::WrongAuthority));
        }
        let now = clock();
        let verified = authority.verify_at(token, branch, now).map_err(refusal)?;
        let permit = verified.begin_write_at(branch, now).map_err(refusal)?;
        if !verified.predicates().rights().can_read() {
            return Err(refusal(Error::PermissionDenied));
        }
        let mut execution = Execution { cx: commit_cx, permit, clock };
        self.complete_authorized_program_with_output(
            txn_cx, query_cx, commit_cx, program, policy, verified.predicates(),
            &mut execution, false,
            |transaction, database, execution, stats, _| {
                result_query(transaction, database, query_cx, query, policy.mutations.query,
                    verified.predicates(), execution, stats)
            },
            Fault::Program,
        ).await.map(|((stats, result), completion)| (stats, result, completion))
    }
}

#[cfg(test)]
#[path = "write_query_tests.rs"]
mod tests;
