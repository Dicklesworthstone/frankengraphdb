//! Complete-source admission for the existing materialized aggregate receiver.
//! A host can enforce visibility before every graph leaf without reconstructing
//! the aggregate's computed input, HAVING, hidden columns or result clauses.

use super::*;
use crate::{GlaLimitDimension, GlaLimitExceeded, GraphSetColumnType, GraphSetExecutionError};

type ResultOf<T, E, C> = Result<T, GqlQueryError<GraphAggregateError<E>, C>>;

impl PreparedGraphAggregate {
    /// Execute this complete definition through a trusted graph-source adapter.
    ///
    /// Each callback must execute the supplied pattern exactly once on the SAME
    /// pinned generation and authorization scope, using the supplied remaining
    /// policy. It returns the pattern's complete ordered occurrences, not raw
    /// bindings, deduplicated rows or an independently paginated approximation.
    /// This callback is a host boundary, not a result-provider API for clients.
    ///
    /// Relational inputs use the existing cumulative relational/folded executor.
    /// Plain and computed graph inputs call the source once and feed the existing
    /// materialized group receiver. Paths, labels and edge properties therefore
    /// retain the source's native representations. No graph or definition clone
    /// is needed, and no matching, scalar or aggregate interpreter is duplicated.
    /// Existing iterator entrypoints keep their streaming/factorized kernels.
    ///
    /// Source rows spend work/scratch, not the final group-result allowance.
    /// Statistics, complete row schemas and value bounds are checked before
    /// grouping, even on empty inputs or LIMIT 0. Source, validation, computed
    /// columns, HAVING and final output share one native policy. Source failures
    /// and cancellation remain typed; no group prefix escapes on a late error.
    /// This path retains admitted source rows in memory and does not add spill.
    pub fn execute_with_source_governed<E, C>(
        &self,
        policy: GqlQueryPolicy,
        mut source: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> ResultOf<GqlQueryExecution<GraphAggregateRow>, E, C> {
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        if self.relational_input.is_some() {
            return self.execute_relational_with_source(policy, source, checkpoint);
        }
        let source_policy = GqlQueryPolicy {
            rows: crate::GqlExecutionBudget::snapshot_records(
                policy.rows.max_snapshot_records().unwrap_or(u64::MAX),
            ),
            evaluator: policy.evaluator,
        };
        let mut input = source(&self.input, source_policy)
            .map_err(|error| error.map_source(GraphAggregateError::Source))?;
        admit_input(&self.input, &mut input, policy, &mut checkpoint)?;
        self.finish_materialized_governed(input, policy, checkpoint)
    }
}

fn admit_input<E, C>(
    pattern: &PreparedGraphPattern<GraphValueRow>,
    input: &mut GqlQueryExecution<GraphValueRow>,
    policy: GqlQueryPolicy,
    checkpoint: &mut impl FnMut() -> Result<(), C>,
) -> ResultOf<(), E, C> {
    checkpoint().map_err(GqlQueryError::Interrupted)?;
    if u64::try_from(input.value.len()).ok() != Some(input.rows.result_rows) {
        return Err(GqlQueryError::Source(GraphAggregateError::InputRelation(
            GraphSetExecutionError::InvalidSourceStatistics { operand: 0 },
        )));
    }
    policy
        .rows
        .check(
            GqlBudgetDimension::SnapshotRecords,
            input.rows.snapshot_records,
        )
        .map_err(GqlQueryError::Rows)?;
    // Reject over-reported source usage before any cumulative addition. This
    // also covers empty sources whose group output would otherwise hide it.
    for (used, limit, dimension) in [
        (
            input.evaluator.work_units,
            policy.evaluator.max_work_units,
            GlaLimitDimension::WorkUnits,
        ),
        (
            input.evaluator.scratch_entries,
            policy.evaluator.max_scratch_entries,
            GlaLimitDimension::ScratchEntries,
        ),
    ] {
        if used > limit {
            return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                dimension,
                limit,
                observed: u128::from(used),
            }));
        }
    }
    let mut control = || {
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        input
            .evaluator
            .charge_event(policy.evaluator, GlaExecutionEvent::Work)
            .map_err(GqlQueryError::Evaluator)
    };
    control()?;
    for row in &input.value {
        control()?;
        let invalid = || {
            GqlQueryError::Source(GraphAggregateError::InputRelation(
                GraphSetExecutionError::InputSchema { operand: 0 },
            ))
        };
        if row.len() != pattern.value_columns().len() {
            return Err(invalid());
        }
        for (value, column) in row.values().iter().zip(pattern.value_columns()) {
            control()?;
            if !GraphSetColumnType::from(column).accepts(value) || !value.validate_bounds() {
                return Err(invalid());
            }
            for _ in 0..value.payload_units() {
                control()?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
