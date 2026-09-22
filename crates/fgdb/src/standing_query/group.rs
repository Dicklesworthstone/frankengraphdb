//! Exact native groups downstream of a complete maintained row circuit.
//!
//! The row circuit is the only input authority. This adapter neither reopens
//! the graph nor interprets a first leaf as the whole input. The existing group
//! kernel, native HAVING/output evaluator and deletion-safe ranked sink own all
//! algebra; this module owns the registry dependency and publication boundary.

use super::*;
use fgdb_delta_types::LimbLimit;
use fgdb_gql::row_aggregate::definition::GroupDefinition;
use fgdb_gql::row_aggregate::definition::operator::{
    GroupBuildError, GroupError, IncrementalGroupAggregate,
};
use fgdb_gql::{GraphAggregateTextSlot, PreparedGraphSetAggregate};

const LIMBS: LimbLimit = LimbLimit::new(4);

pub(crate) struct State {
    pub(super) input: usize,
    operator: IncrementalGroupAggregate<PreparedGraphSetAggregate>,
    output: output::State<PreparedGraphSetAggregate>,
    last_delta: Option<ZSet<GraphAggregateRow>>,
    pub(super) policy: GqlQueryPolicy,
    pub(super) frontier: CommitSeq,
    pub(super) stats: StandingQueryStats,
    pub(super) failure: Option<StandingQueryFailure>,
}

/// A grouped query's private input is not its public result. Keep every source,
/// work and scratch allowance, but do not charge one final GROUP row allowance
/// against each pre-group occurrence. Explicit input DISTINCT/pages stay in
/// their definitions. Work/scratch remain per registry node, not circuit-wide.
pub(super) fn input_policy(policy: GqlQueryPolicy) -> GqlQueryPolicy {
    GqlQueryPolicy {
        rows: fgdb_gql::GqlExecutionBudget::snapshot_records(
            policy.rows.max_snapshot_records().unwrap_or(u64::MAX),
        ),
        evaluator: policy.evaluator,
    }
}

fn group_error(error: GroupError<StandingQueryFailure>) -> StandingQueryFailure {
    match error {
        GroupError::Delta(error) => zset_error(error),
        GroupError::NonInteger { column } => StandingQueryFailure::NonIntegerAggregate { column },
        GroupError::NonIntegerHaving => StandingQueryFailure::NonIntegerHaving,
        GroupError::Arithmetic => StandingQueryFailure::Arithmetic,
        GroupError::ResultBudget { .. } => StandingQueryFailure::ResultBudget,
        GroupError::InputSchema | GroupError::NegativeMultiplicity | GroupError::InvalidResult => {
            StandingQueryFailure::InvalidDelta
        }
    }
}

impl State {
    pub(super) fn definition(&self) -> &PreparedGraphSetAggregate {
        self.output.definition()
    }
    pub(super) fn rows(&self) -> &ZSet<GraphAggregateRow> {
        &self.output.rows
    }
    pub(super) fn ordered_rows(&self) -> Option<&[Arc<GraphAggregateRow>]> {
        self.output.ordered_rows()
    }
    fn apply(
        &mut self,
        delta: &ZSet<GraphValueRow>,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        // No public row limit on private complete groups: HAVING, projected
        // collisions, DISTINCT and the final window determine that limit.
        let groups = self
            .operator
            .prepare(delta, LIMBS, None, &mut |event| meter.charge(event))
            .map_err(group_error)?;
        let output = self.output.prepare(groups.delta(), meter)?;
        (meter.checkpoint)()?;
        // Raw inputs, group support, output classes/page and derivative remain
        // tentative until here. Nothing recoverably fallible follows.
        let _ = groups.commit();
        self.last_delta = Some(output.commit());
        Ok(())
    }
    pub(super) fn maintain(
        &mut self,
        batch: &LogicalDeltaBatch,
        sources: &[StandingQuery],
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
        let source = sets::input_at(sources, self.input, at)?;
        let delta = sets::delta(source).ok_or(StandingQueryFailure::DependencyUnavailable)?;
        meter.stats.delta_rows =
            u64::try_from(delta.len()).map_err(|_| StandingQueryFailure::WorkBudget)?;
        self.apply(delta, meter)
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Maintain a complete bound relational aggregate in one owned circuit.
    /// All input set/product/projection/filter/window semantics execute BEFORE
    /// grouping. COUNT, COUNT DISTINCT, SUM, SUM DISTINCT, AVG, AVG DISTINCT,
    /// MIN and MAX preserve native exact domains, including collection keys
    /// and extrema. UNWIND/list-index Any inputs keep runtime Int64/NULL checks
    /// for numeric functions; arbitrary payloads are never silently coerced.
    /// NULL group keys coalesce, while NULL arguments do not contribute to
    /// nonnull statistics. Empty global input has one zero/null group; empty
    /// grouped input has none. HAVING runs before output expressions, output
    /// DISTINCT and group ORDER BY/OFFSET/LIMIT, through the existing engines.
    ///
    /// Read through standing_query or standing_native_query. Native column
    /// order here is visible keys followed by visible aggregate/output columns.
    /// standing_group_delta exposes the latest accepted FINAL-output derivative,
    /// not a backlog. Rebuild repairs the complete owned circuit atomically.
    /// No parameters, resolver or source tree is needed again after admission.
    ///
    /// max_result_rows bounds final occurrences, not private input or hidden
    /// groups. Work/scratch limits are per node; compressed-input initialization
    /// and graph sources still obey max_snapshot_records. Ranking retains its
    /// candidate index and can walk a large offset. No circuit-wide byte limit,
    /// spill, durable subscription, COLLECT or scalar narrowing is introduced.
    pub fn register_standing_relation_aggregate(
        &mut self,
        cx: &QueryCx,
        definition: &PreparedGraphSetAggregate,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        cx.with_restriction(|| {
            let columns = definition
                .key_columns()
                .iter()
                .chain(definition.aggregate_columns())
                .cloned()
                .collect::<Vec<_>>();
            let slots = (0..definition.key_columns().len())
                .map(GraphAggregateTextSlot::GroupKey)
                .chain(
                    (0..definition.aggregate_columns().len())
                        .map(GraphAggregateTextSlot::Aggregate),
                )
                .collect::<Vec<_>>();
            native::set::register_group(self, cx, definition, &columns, &slots, policy)
        })
    }

    pub(super) fn prepare_standing_group(
        &self,
        cx: &QueryCx,
        input: usize,
        definition: PreparedGraphSetAggregate,
        policy: GqlQueryPolicy,
        before: usize,
    ) -> Result<State, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        let sources = self
            .standing_queries
            .get(..before)
            .ok_or(StandingQueryError::UnknownHandle)?;
        let at = self.snapshot.frontier;
        cx.with_restriction(|| {
            let parent =
                sets::input_at(sources, input, at).map_err(StandingQueryError::Maintenance)?;
            let names = sets::columns(parent).ok_or(StandingQueryError::Unsupported)?;
            let schema = definition.input().column_types();
            if names.len() != schema.len() {
                return Err(StandingQueryError::GroupSchema(GroupBuildError::InputWidth));
            }
            let mut checkpoint = || {
                cx.checkpoint()
                    .map_err(|_| StandingQueryFailure::Interrupted)
            };
            let mut meter = Meter {
                policy,
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            // Never infer a schema from present data: empty parents still have
            // full column domains, and keys need not follow the first leaf.
            for (column, kind) in schema.iter().enumerate() {
                meter
                    .charge(ZSetEvent::Work)
                    .map_err(StandingQueryError::Maintenance)?;
                if sets::column_type(parent, column) != Some(*kind) {
                    return Err(StandingQueryError::GroupSchema(
                        GroupBuildError::InputSchema { column },
                    ));
                }
            }
            let complete = definition
                .complete_groups()
                .ok_or(StandingQueryError::Unsupported)?;
            let operator = IncrementalGroupAggregate::new(complete, schema)
                .map_err(StandingQueryError::GroupSchema)?;
            let rows = sets::rows(parent).ok_or(StandingQueryError::Unsupported)?;
            if policy
                .rows
                .max_snapshot_records()
                .is_some_and(|limit| rows.len() as u128 > u128::from(limit))
            {
                return Err(StandingQueryError::Maintenance(
                    StandingQueryFailure::SnapshotBudget,
                ));
            }
            meter
                .charge(ZSetEvent::ScratchEntry)
                .map_err(StandingQueryError::Maintenance)?;
            let mut state = State {
                input,
                operator,
                output: output::State::new(definition),
                last_delta: None,
                policy,
                frontier: at,
                stats: StandingQueryStats::default(),
                failure: None,
            };
            state
                .apply(rows, &mut meter)
                .map_err(StandingQueryError::Maintenance)?;
            state.last_delta = None; // A baseline is not a committed successor.
            state.stats = meter.stats;
            Ok(state)
        })
    }

    /// Latest exact FINAL output change of a relational group circuit. None
    /// denotes registration/rebuild, Some(empty) an accepted unchanged commit.
    /// The derivative is a bag, not an ordering trace or a retained delivery log.
    pub fn standing_group_delta<'a>(
        &'a self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
    ) -> Result<Option<StandingQueryView<'a>>, StandingQueryError> {
        let StandingQuery::Group(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.last_delta.as_ref().map(|rows| StandingQueryView {
            rows,
            ordered: None,
            frontier: query.frontier,
            stats: &query.stats,
        }))
    }

    /// Frozen source-aware definition; never an executable first-leaf surrogate.
    pub fn standing_group_definition<'a>(
        &'a self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
    ) -> Result<&'a PreparedGraphSetAggregate, StandingQueryError> {
        let StandingQuery::Group(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.definition())
    }
}

#[cfg(test)]
mod tests;
