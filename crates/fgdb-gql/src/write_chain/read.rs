//! Correlated read-after-write composition over the native relational engine.
//! The write's output is one owned leaf, never a replayed query or a new scan.

use super::{GraphWriteChainError, GraphWriteChainStep, PreparedGraphWriteChain};
use crate::algebra::GraphValueRow;
use crate::set_ops::WithOwnedRows;
use crate::{
    GlaExecutionStats, GlaLimitDimension, GlaLimitExceeded, GqlBudgetDimension,
    GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphSetColumnType,
    GraphSetExecutionError, GraphSetSource, PreparedGraphSet,
};

type ReadResult<T, E, A, C> = Result<T, GqlQueryError<GraphWriteChainError<E, A>, C>>;

impl PreparedGraphWriteChain {
    fn read_input_types(&self) -> Option<&[GraphSetColumnType]> {
        match self.steps.last()? {
            GraphWriteChainStep::Query(query) => Some(query.column_types()),
            GraphWriteChainStep::EdgeMergeQuery(query) => Some(query.column_types()),
            GraphWriteChainStep::VertexMergeQuery(query) => Some(query.column_types()),
            _ => None,
        }
    }

    /// The schema-only input of a correlated read after this complete write.
    /// Compose it with the ordinary join, filter, projection, grouping and
    /// page builders. A Left join supplies OPTIONAL MATCH null extension.
    ///
    /// The write must explicitly return the columns to carry. Its RETURN
    /// expressions and page finish before the read, but these intermediate
    /// occurrences do not spend the final delivered-row allowance. No graph
    /// access, identity allocation or authority is performed by this method.
    /// This is a typed composition API, not a new text grammar.
    #[must_use]
    pub fn read_input(&self) -> Option<PreparedGraphSet> {
        Some(PreparedGraphSet::owned(
            self.columns().to_vec(),
            self.read_input_types()?.to_vec(),
        ))
    }

    /// Admit a continuation before any write: it must consume exactly one
    /// owned input with this chain's complete ordered names and types. Cloning
    /// the input into two subtrees or omitting it refuses, even under LIMIT 0.
    /// All other nodes retain their native definition/depth/operand checks.
    #[must_use]
    pub fn accepts_read_continuation(&self, continuation: &PreparedGraphSet) -> bool {
        self.read_input_types().is_some_and(|types| {
            continuation.has_one_owned_input(self.columns(), types)
                && continuation.check_parent_depth().is_ok()
        })
    }

    /// Finish a read against the host's POST-WRITE overlay and the already
    /// admitted output of this chain. The host must hold one outer statement
    /// rollback guard across BOTH phases, and must not publish either prefix.
    /// Execute the write with an uncapped private-row allowance and the same
    /// snapshot/work/scratch limits; pass its complete counters here.
    ///
    /// Every graph operand executes through the supplied native source. The
    /// owned bag is moved once without cloning or deduplicating it. Final-row
    /// admission belongs to the continuation alone. All other costs, errors
    /// and interruption remain cumulative across the write and read phases.
    /// This trusted-host boundary grants no capability and performs no commit.
    pub fn finish_read_continuation<E, A, C>(
        &self,
        continuation: &PreparedGraphSet,
        head: GqlQueryExecution<GraphValueRow>,
        policy: GqlQueryPolicy,
        mut source: impl GraphSetSource<E, C>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> ReadResult<GqlQueryExecution<GraphValueRow>, E, A, C> {
        let invalid = || {
            GqlQueryError::Source(GraphWriteChainError::Read(
                GraphSetExecutionError::InputSchema { operand: 0 },
            ))
        };
        if !self.accepts_read_continuation(continuation) {
            return Err(invalid());
        }
        if u64::try_from(head.value.len()).ok() != Some(head.rows.result_rows) {
            return Err(GqlQueryError::Source(GraphWriteChainError::Read(
                GraphSetExecutionError::InvalidSourceStatistics { operand: 0 },
            )));
        }
        policy
            .rows
            .check(GqlBudgetDimension::SnapshotRecords, head.rows.snapshot_records)
            .map_err(GqlQueryError::Rows)?;
        let mut evaluator = head.evaluator;
        for (used, limit, dimension) in [
            (evaluator.work_units, policy.evaluator.max_work_units, GlaLimitDimension::WorkUnits),
            (evaluator.scratch_entries, policy.evaluator.max_scratch_entries, GlaLimitDimension::ScratchEntries),
        ] {
            if used > limit {
                return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                    dimension, limit, observed: u128::from(used),
                }));
            }
        }
        // Validate the complete carried schema before any continuation source
        // callback. The write already charged ownership; these are inspection
        // events, not another payload copy or another source-record admission.
        let types = self.read_input_types().ok_or_else(invalid)?;
        let mut inspect = || -> ReadResult<(), E, A, C> {
            checkpoint().map_err(GqlQueryError::Interrupted)?;
            let observed = u128::from(evaluator.work_units) + 1;
            let limit = policy.evaluator.max_work_units;
            if observed > u128::from(limit) {
                return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                    dimension: GlaLimitDimension::WorkUnits, limit, observed,
                }));
            }
            evaluator.work_units = observed as u64;
            Ok(())
        };
        inspect()?;
        for row in &head.value {
            inspect()?;
            if row.len() != types.len() {
                return Err(invalid());
            }
            for (value, kind) in row.values().iter().zip(types) {
                inspect()?;
                if !kind.accepts(value) || !value.validate_bounds() {
                    return Err(invalid());
                }
                for _ in 0..value.payload_units() {
                    inspect()?;
                }
            }
        }
        drop(inspect);
        let budget = GqlQueryPolicy::new(
            policy.rows.max_snapshot_records().unwrap_or(u64::MAX) - head.rows.snapshot_records,
            policy.rows.max_result_rows().unwrap_or(u64::MAX),
            policy.evaluator.max_work_units - evaluator.work_units,
            policy.evaluator.max_scratch_entries - evaluator.scratch_entries,
        );
        let output = continuation
            .execute_with_source(
                budget,
                &mut WithOwnedRows { source: &mut source, rows: Some(head.value) },
                &mut checkpoint,
            )
            .map_err(|error| match error {
                GqlQueryError::Evaluator(mut error) => {
                    let (spent, limit) = match error.dimension {
                        GlaLimitDimension::WorkUnits => (evaluator.work_units, policy.evaluator.max_work_units),
                        GlaLimitDimension::ScratchEntries => (evaluator.scratch_entries, policy.evaluator.max_scratch_entries),
                    };
                    error.observed = error.observed.saturating_add(u128::from(spent));
                    error.limit = limit;
                    GqlQueryError::Evaluator(error)
                }
                GqlQueryError::Rows(mut error) if error.dimension == GqlBudgetDimension::SnapshotRecords => {
                    let Some(observed) = error.observed.checked_add(head.rows.snapshot_records) else {
                        return GqlQueryError::Source(GraphWriteChainError::Read(
                            GraphSetExecutionError::AccountingOverflow { dimension: error.dimension },
                        ));
                    };
                    error.observed = observed;
                    error.limit = policy.rows.max_snapshot_records().unwrap_or(u64::MAX);
                    GqlQueryError::Rows(error)
                }
                error => error.map_source(GraphWriteChainError::Read),
            })?;
        // Native execution admitted the output under exactly these remainders;
        // checked sums also fail closed if a future source violates that law.
        let inconsistent = || GqlQueryError::Source(GraphWriteChainError::Read(
            GraphSetExecutionError::InvalidSourceStatistics { operand: 0 },
        ));
        let rows = GqlExecutionStats {
            snapshot_records: head.rows.snapshot_records.checked_add(output.rows.snapshot_records)
                .ok_or_else(inconsistent)?,
            result_rows: output.rows.result_rows,
        };
        let evaluator = GlaExecutionStats {
            work_units: evaluator.work_units.checked_add(output.evaluator.work_units)
                .ok_or_else(inconsistent)?,
            scratch_entries: evaluator.scratch_entries.checked_add(output.evaluator.scratch_entries)
                .ok_or_else(inconsistent)?,
        };
        Ok(GqlQueryExecution { value: output.value, rows, evaluator })
    }
}

#[cfg(test)]
mod tests;
