//! Immutable relational sources in the ordinary standing-query registry.
//!
//! Only a checked relation with ZERO graph operands may enter. Its complete
//! expression, filter, set, product and positional-page semantics execute once
//! through the existing governed relation executor. No synthetic graph source,
//! new interpreter, or per-commit query evaluation is introduced.

use super::*;
use fgdb_delta_types::{LimbLimit, ZWeight};
use fgdb_gql::{
    GlaLimitDimension, GqlBudgetDimension, GqlQueryError, GraphSetExecutionError, PreparedGraphSet,
};

pub(crate) struct State {
    pub(super) definition: PreparedGraphSet,
    pub(super) rows: ZSet<GraphValueRow>,
    pub(super) ordered: Vec<Arc<GraphValueRow>>,
    pub(super) last_delta: Option<ZSet<GraphValueRow>>,
    pub(super) policy: GqlQueryPolicy,
    pub(super) frontier: CommitSeq,
    pub(super) stats: StandingQueryStats,
    pub(super) failure: Option<StandingQueryFailure>,
}

fn query_error(
    error: GqlQueryError<GraphSetExecutionError<StandingQueryFailure>, StandingQueryFailure>,
) -> StandingQueryFailure {
    match error {
        GqlQueryError::Interrupted(reason)
        | GqlQueryError::Source(GraphSetExecutionError::Source(reason)) => reason,
        GqlQueryError::Source(GraphSetExecutionError::Projection { column, error, .. }) => {
            StandingQueryFailure::InputExpression { column, error }
        }
        GqlQueryError::Evaluator(error) => match error.dimension {
            GlaLimitDimension::WorkUnits => StandingQueryFailure::WorkBudget,
            GlaLimitDimension::ScratchEntries => StandingQueryFailure::ScratchBudget,
        },
        GqlQueryError::Rows(error) => match error.dimension {
            GqlBudgetDimension::SnapshotRecords => StandingQueryFailure::SnapshotBudget,
            GqlBudgetDimension::ResultRows => StandingQueryFailure::ResultBudget,
        },
        GqlQueryError::Source(GraphSetExecutionError::AccountingOverflow { .. }) => {
            StandingQueryFailure::Arithmetic
        }
        _ => StandingQueryFailure::InvalidDelta,
    }
}

impl State {
    fn build(
        definition: PreparedGraphSet,
        at: CommitSeq,
        policy: GqlQueryPolicy,
        checkpoint: &mut dyn FnMut() -> Result<(), StandingQueryFailure>,
    ) -> Result<Self, StandingQueryFailure> {
        checkpoint()?;
        if definition.operand_count() != 0 {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        let execution = definition
            .execute_governed(
                policy,
                // This callback is a refusal, never an empty substitute for a
                // graph leaf. The immutable relation's operand count is exact.
                |_, _| Err(GqlQueryError::Source(StandingQueryFailure::InvalidDelta)),
                &mut *checkpoint,
            )
            .map_err(query_error)?;
        if execution.rows.snapshot_records != 0
            || u64::try_from(execution.value.len()).ok() != Some(execution.rows.result_rows)
        {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        // Continue the SAME allowance through retaining the ordered result and
        // consolidating its exact bag. Execution never resets the copy budget.
        let mut meter = Meter {
            policy,
            stats: StandingQueryStats {
                work_units: execution.evaluator.work_units,
                scratch_entries: execution.evaluator.scratch_entries,
                ..StandingQueryStats::default()
            },
            checkpoint,
        };
        meter.units(ZSetEvent::ScratchEntry, 2)?;
        let mut ordered = Vec::new();
        let mut updates = Vec::new();
        for row in execution.value {
            meter.charge(ZSetEvent::Work)?;
            meter.units(ZSetEvent::ScratchEntry, 3)?;
            for value in row.values() {
                meter.charge(ZSetEvent::Work)?;
                let units = value
                    .payload_units()
                    .checked_add(1)
                    .ok_or(StandingQueryFailure::ScratchBudget)?;
                // Reserve both retained key copies before either is cloned.
                meter.units(ZSetEvent::ScratchEntry, units)?;
                meter.units(ZSetEvent::ScratchEntry, units)?;
            }
            updates.push((row.clone(), ZWeight::ONE));
            ordered.push(Arc::new(row));
        }
        let rows = ZSet::from_updates(updates, LimbLimit::new(4), &mut |e| meter.charge(e))
            .map_err(zset_error)?;
        (meter.checkpoint)()?;
        Ok(Self {
            definition,
            rows,
            ordered,
            last_delta: None,
            policy,
            frontier: at,
            stats: meter.stats,
            failure: None,
        })
    }

    pub(super) fn maintain(
        &mut self,
        batch: &LogicalDeltaBatch,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        let at = batch.commit_seq();
        if self
            .frontier
            .checked_successor()
            .map_err(|_| StandingQueryFailure::InvalidDelta)?
            != at
            || batch.frontier() != at
            || batch.commit_marker_identity().commit_seq != at
        {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        (meter.checkpoint)()?;
        // The usual registry record step publishes the matching frontier.
        // A baseline is None; an accepted unchanged successor is Some(empty).
        // No rows, expressions or source records are visited on this path.
        self.last_delta = Some(ZSet::new());
        Ok(())
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Register a checked source-free relation as immutable session-local input.
    /// Complete projections, filters, UNWIND, sets, products and pages execute
    /// once under one cumulative work/scratch allowance. The selected sequence
    /// and exact bag are retained; no graph is scanned, even in a populated DB.
    /// Any graph operand refuses, including behind an empty result or LIMIT 0.
    ///
    /// Read with standing_rows; use this handle as a normal set/join/projection
    /// input. Each durable successor publishes an empty derivative, without
    /// reevaluating expressions. Rebuild uses the owned definition atomically.
    /// This is bounded in-memory constant evaluation, not graph snapshotting,
    /// a durable subscription, a byte-memory bound, or spill-backed execution.
    pub fn register_standing_constant(
        &mut self,
        cx: &QueryCx,
        definition: PreparedGraphSet,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        let query = self.prepare_standing_constant(cx, definition, policy)?;
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        Ok(self.store_standing_query(StandingQuery::Constant(Box::new(query))))
    }

    pub(super) fn prepare_standing_constant(
        &self,
        cx: &QueryCx,
        definition: PreparedGraphSet,
        policy: GqlQueryPolicy,
    ) -> Result<State, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        if definition.operand_count() != 0 {
            return Err(StandingQueryError::Unsupported);
        }
        cx.with_restriction(|| {
            State::build(definition, self.snapshot.frontier, policy, &mut || {
                cx.checkpoint()
                    .map_err(|_| StandingQueryFailure::Interrupted)
            })
            .map_err(StandingQueryError::Maintenance)
        })
    }
}

#[cfg(test)]
mod tests;
