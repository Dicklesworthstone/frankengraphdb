//! Governed composition, not a second graph interpreter or transaction manager.

use super::{GraphQueryWriteBindError, PreparedGraphQueryWrite};
use crate::algebra::GraphValueRow;
use crate::{
    BoundGraphWriteScriptBatch, GlaExecutionStats, GlaLimitDimension, GlaLimitExceeded,
    GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphSetExecutionError, GraphWriteProgramPolicy, GraphWriteProgramReceipt,
    GraphWriteProgramStats, GraphWriteScriptExecutionError, PreparedGraphSet,
};
use core::convert::Infallible;

/// Phase-specific failures retain the source and input-record coordinates.
/// Program quota failures describe the residual allowance passed to that phase.
/// A completion error is never evidence that a published transaction rolled back.
#[derive(Debug)]
pub enum GraphQueryWriteExecutionError<E, A, C> {
    Preflight(E),
    Source(GqlQueryError<GraphSetExecutionError<E>, C>),
    Binding(GraphQueryWriteBindError<GqlQueryError<Infallible, C>>),
    Program(GraphWriteScriptExecutionError<E, A, C>),
    Control(GqlQueryError<Infallible, C>),
    InvalidSourceStatistics,
    InvalidProgramStatistics,
    Completion(E),
}

impl<E: core::fmt::Display, A: core::fmt::Display, C: core::fmt::Display> core::fmt::Display
    for GraphQueryWriteExecutionError<E, A, C>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Preflight(error) => write!(f, "query-fed write preflight: {error}"),
            Self::Source(error) => write!(f, "query-fed write source: {error}"),
            Self::Binding(error) => error.fmt(f),
            Self::Program(error) => error.fmt(f),
            Self::Control(error) => error.fmt(f),
            Self::InvalidSourceStatistics => f.write_str("invalid query-fed source statistics"),
            Self::InvalidProgramStatistics => f.write_str("invalid query-fed program statistics"),
            Self::Completion(error) => write!(f, "query-fed write completion: {error}"),
        }
    }
}
impl<E, A, C> core::error::Error for GraphQueryWriteExecutionError<E, A, C>
where
    E: core::error::Error + 'static,
    A: core::error::Error + 'static,
    C: core::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Preflight(error) | Self::Completion(error) => Some(error),
            Self::Source(error) => Some(error),
            Self::Binding(error) => Some(error),
            Self::Program(error) => Some(error),
            Self::Control(error) => Some(error),
            Self::InvalidSourceStatistics | Self::InvalidProgramStatistics => None,
        }
    }
}

/// Accepted transaction-local results. Only the host's ordinary finish commits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphQueryWriteExecution {
    /// Frozen source occurrences, including duplicates.
    pub input_rows: u64,
    /// Whole-operation counters: source plus binding plus sequential program.
    /// Binding spends work/scratch, not fabricated selected rows or statements.
    pub stats: GraphWriteProgramStats,
    /// None denotes an empty source. A present receipt retains the native
    /// program's own counters and ordered steps, not a durability claim.
    pub receipt: Option<GraphWriteProgramReceipt>,
}

fn within(policy: GqlQueryPolicy, rows: GqlExecutionStats, evaluator: GlaExecutionStats) -> bool {
    rows.snapshot_records <= policy.rows.max_snapshot_records().unwrap_or(u64::MAX)
        && rows.result_rows <= policy.rows.max_result_rows().unwrap_or(u64::MAX)
        && evaluator.work_units <= policy.evaluator.max_work_units
        && evaluator.scratch_entries <= policy.evaluator.max_scratch_entries
}

fn charge<C>(
    policy: GqlQueryPolicy,
    used: &mut GlaExecutionStats,
    entries: u64,
) -> Result<(), GqlQueryError<Infallible, C>> {
    let work = u128::from(used.work_units) + u128::from(entries);
    let scratch = u128::from(used.scratch_entries) + u128::from(entries);
    for (observed, limit, dimension) in [
        (work, policy.evaluator.max_work_units, GlaLimitDimension::WorkUnits),
        (scratch, policy.evaluator.max_scratch_entries, GlaLimitDimension::ScratchEntries),
    ] {
        if observed > u128::from(limit) {
            return Err(GqlQueryError::Evaluator(GlaLimitExceeded { dimension, limit, observed }));
        }
    }
    // Install neither successor until both bounds hold, including at u64::MAX.
    *used = GlaExecutionStats { work_units: work as u64, scratch_entries: scratch as u64 };
    Ok(())
}

impl PreparedGraphQueryWrite {
    /// Read ONCE, bind ALL occurrences, then stage ONE sequential native batch.
    /// The source, argument ownership and program share the supplied policy;
    /// neither source rows nor binding work receive a fresh allowance per row.
    /// An expanded-statement cap narrows source output admission, never adds a
    /// LIMIT that would silently drop source occurrences. Intermediate storage
    /// and operator costs remain those of the native governed source/program.
    ///
    /// TRUSTED HOST SEAM: read and stage must use the SAME pinned transaction.
    /// The host must wrap this ENTIRE call in its ordinary rollback guard,
    /// including the final checkpoint AFTER stage succeeds. A stage callback
    /// may not commit or expose receipts. Errors/unwinds retain observations
    /// but restore the prior staged prefix; issued identities stay spent.
    /// Callbacks cannot make an arbitrary external writer atomic.
    #[allow(clippy::type_complexity)] // native phase errors intentionally stay typed
    #[allow(clippy::result_large_err)] // once-per-operation phase/record report
    pub fn execute_governed<E, A, C>(
        &self,
        policy: GraphWriteProgramPolicy,
        max_statements: usize,
        mut read: impl FnMut(
            &PreparedGraphSet,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<E>, C>>,
        mut stage: impl FnMut(
            &BoundGraphWriteScriptBatch,
            GraphWriteProgramPolicy,
        ) -> Result<GraphWriteProgramReceipt, GraphWriteScriptExecutionError<E, A, C>>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphQueryWriteExecution, GraphQueryWriteExecutionError<E, A, C>> {
        use GraphQueryWriteExecutionError as Error;
        checkpoint().map_err(|error| Error::Control(GqlQueryError::Interrupted(error)))?;
        let query = policy.mutations.query;
        let read_policy = GqlQueryPolicy::new(
            query.rows.max_snapshot_records().unwrap_or(u64::MAX),
            query.rows.max_result_rows().unwrap_or(u64::MAX)
                .min(self.input_row_limit(max_statements) as u64),
            query.evaluator.max_work_units,
            query.evaluator.max_scratch_entries,
        );
        let source = read(self.source(), read_policy).map_err(Error::Source)?;
        if u64::try_from(source.value.len()).ok() != Some(source.rows.result_rows)
            || !within(read_policy, source.rows, source.evaluator)
        {
            return Err(Error::InvalidSourceStatistics);
        }
        let selection = source.rows;
        let mut evaluator = source.evaluator;
        let batch = self.bind_rows_controlled(&source.value, max_statements, |event| {
            checkpoint().map_err(GqlQueryError::Interrupted)?;
            charge(query, &mut evaluator, event.entries)
        }).map_err(Error::Binding)?;
        // Frozen operands now belong to the admitted batch. Drop read results
        // before writing, but never refund their cumulative scratch/work cost.
        drop(source);
        let mut stats = GraphWriteProgramStats {
            completed_statements: 0,
            selection,
            evaluator,
            target_vertex_visits: 0,
            target_edge_visits: 0,
            mutation_effects: 0,
            created_vertices: 0,
            created_edges: 0,
        };
        let receipt = if let Some(batch) = batch {
            let remaining = GraphWriteProgramPolicy::new(
                GqlQueryPolicy::new(
                    query.rows.max_snapshot_records().unwrap_or(u64::MAX) - selection.snapshot_records,
                    query.rows.max_result_rows().unwrap_or(u64::MAX) - selection.result_rows,
                    query.evaluator.max_work_units - evaluator.work_units,
                    query.evaluator.max_scratch_entries - evaluator.scratch_entries,
                ),
                policy.mutations.max_effects,
                policy.max_created_vertices,
                policy.max_created_edges,
            );
            let receipt = stage(&batch, remaining).map_err(Error::Program)?;
            let written = receipt.stats();
            if written.completed_statements != batch.program().statements().len()
                || receipt.steps().len() != written.completed_statements
                || !within(remaining.mutations.query, written.selection, written.evaluator)
                || written.mutation_effects > remaining.mutations.max_effects
                || written.created_vertices > remaining.max_created_vertices
                || written.created_edges > remaining.max_created_edges
            {
                return Err(Error::InvalidProgramStatistics);
            }
            // Residual checks above prove each sum fits its original u64 cap.
            stats = written;
            stats.selection.snapshot_records += selection.snapshot_records;
            stats.selection.result_rows += selection.result_rows;
            stats.evaluator.work_units += evaluator.work_units;
            stats.evaluator.scratch_entries += evaluator.scratch_entries;
            Some(receipt)
        } else {
            None
        };
        checkpoint().map_err(|error| Error::Control(GqlQueryError::Interrupted(error)))?;
        Ok(GraphQueryWriteExecution { input_rows: selection.result_rows, stats, receipt })
    }
}

#[cfg(test)]
mod tests;
