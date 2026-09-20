//! Exact grouped statistics downstream of maintained native row circuits.
//!
//! This owns a registry dependency, not another graph source. One complete
//! successor derivative enters the native row aggregate adapter. Initialization
//! and rebuild read the current accepted input bag; a new baseline is never
//! confused with an empty committed delta.

use super::*;
use fgdb_delta_types::LimbLimit;
use fgdb_gql::algebra::MAX_PATTERN_VERTICES;
use fgdb_gql::row_aggregate::{
    IncrementalRowAggregate, RowAggregateBuildError, RowAggregateError,
    RowAggregateRow, RowAggregateSpec,
};

const LIMBS: LimbLimit = LimbLimit::new(4);

pub(crate) struct State {
    pub(super) input: usize,
    group_names: Vec<String>,
    operator: IncrementalRowAggregate,
    last_delta: Option<ZSet<RowAggregateRow>>,
    pub(super) policy: GqlQueryPolicy,
    pub(super) frontier: CommitSeq,
    pub(super) stats: StandingQueryStats,
    pub(super) failure: Option<StandingQueryFailure>,
}
fn reduction_error(error: RowAggregateError<StandingQueryFailure>) -> StandingQueryFailure {
    match error {
        RowAggregateError::Delta(error) => zset_error(error),
        RowAggregateError::ResultBudget { .. } => StandingQueryFailure::ResultBudget,
        RowAggregateError::NonIntegerValue { .. } => StandingQueryFailure::NonIntegerSum,
        RowAggregateError::InputSchema | RowAggregateError::NegativeMultiplicity
        | RowAggregateError::InvalidResult => StandingQueryFailure::InvalidDelta,
    }
}
impl State {
    pub(super) fn spec(&self) -> &RowAggregateSpec { self.operator.spec() }
    fn apply(&mut self, delta: &ZSet<GraphValueRow>, meter: &mut Meter<'_>)
        -> Result<(), StandingQueryFailure> {
        let pending = self.operator.prepare(delta, LIMBS, meter.policy.rows.max_result_rows(),
            &mut |event| meter.charge(event)).map_err(reduction_error)?;
        // The native input, aggregate support and output are all still private.
        (meter.checkpoint)()?;
        self.last_delta = Some(pending.commit());
        Ok(())
    }
    pub(super) fn maintain(&mut self, batch: &LogicalDeltaBatch, sources: &[StandingQuery],
        meter: &mut Meter<'_>) -> Result<(), StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        let at = batch.commit_seq();
        if self.frontier.checked_successor().map_err(|_| StandingQueryFailure::InvalidDelta)? != at
            || batch.frontier() != at || batch.commit_marker_identity().commit_seq != at {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        let input = sets::input_at(sources, self.input, at)?;
        let delta = sets::delta(input).ok_or(StandingQueryFailure::DependencyUnavailable)?;
        meter.stats.delta_rows = u64::try_from(delta.len()).map_err(|_| StandingQueryFailure::WorkBudget)?;
        self.apply(delta, meter)
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Maintain exact grouped statistics over an existing row, set, join or
    /// projection view. Column positions address the COMPLETE selected upstream
    /// result, after its DISTINCT/filter/page semantics. Composite group keys
    /// preserve scalar/vertex domains and group NULLs together. The selected
    /// value column must contain Int64 or NULL; nonnumeric data fences this
    /// reducer without undoing the durable write or accepted parent/siblings.
    ///
    /// Each group exposes exact row, nonnull and distinct counts, sum/distinct
    /// sum, minimum/maximum, and average numerator/denominator. Promoted ZWeight
    /// values are never narrowed into u64/i128 result cells or floats. An empty
    /// group_columns list produces one global zero/null group for empty input;
    /// grouped empty input produces no groups. NULL rows count in count_rows,
    /// but not count_values, sums, distinct values or extrema.
    ///
    /// Registration/rebuild bounds compressed upstream support by
    /// max_snapshot_records. Ordinary maintenance visits only changed input
    /// tuples/groups and invalidated extrema; duplicates are not expanded.
    /// max_result_rows bounds final groups, not input occurrences or transient
    /// insertion-first changes. Work/scratch are per-view logical/payload
    /// allowances, not total-circuit or allocator-byte limits.
    ///
    /// A parent must publish the same immediate successor, including a real
    /// empty derivative. An unavailable parent or fresh baseline is NOT an empty
    /// input. Repair dependencies before rebuilding this handle. Registration
    /// remains session-local: no new GQL grammar, durable subscription, spill,
    /// HAVING/ranking, or conversion of exact summaries into scalar rows.
    pub fn register_standing_reduction(
        &mut self, cx: &QueryCx, input: &StandingQueryHandle, group_columns: &[usize],
        value_column: usize, policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        if !Arc::ptr_eq(&self.handle_owner, &input.owner) {
            return Err(StandingQueryError::ForeignHandle);
        }
        if sets::rows(self.admitted_standing_query(cx, input)?).is_none() {
            return Err(StandingQueryError::Unsupported);
        }
        let query = self.prepare_standing_reduction(cx, input.index, group_columns, value_column,
            policy, self.standing_queries.len())?;
        Ok(self.store_standing_query(StandingQuery::Reduction(Box::new(query))))
    }

    pub(super) fn prepare_standing_reduction(
        &self, cx: &QueryCx, input: usize, group_columns: &[usize], value_column: usize,
        policy: GqlQueryPolicy, before: usize,
    ) -> Result<State, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        let sources = self.standing_queries.get(..before).ok_or(StandingQueryError::UnknownHandle)?;
        let at = self.snapshot.frontier;
        cx.with_restriction(|| {
            let source = sets::input_at(sources, input, at).map_err(StandingQueryError::Maintenance)?;
            let names = sets::columns(source).ok_or(StandingQueryError::Unsupported)?;
            if names.len() > MAX_PATTERN_VERTICES || group_columns.len() > MAX_PATTERN_VERTICES {
                return Err(StandingQueryError::ReductionSchema(RowAggregateBuildError::TooManyColumns));
            }
            let mut checkpoint = || cx.checkpoint().map_err(|_| StandingQueryFailure::Interrupted);
            let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            let mut types = Vec::new();
            for column in 0..names.len() {
                meter.charge(ZSetEvent::Work).map_err(StandingQueryError::Maintenance)?;
                meter.charge(ZSetEvent::ScratchEntry).map_err(StandingQueryError::Maintenance)?;
                types.push(sets::column_type(source, column).ok_or(StandingQueryError::Unsupported)?);
            }
            meter.units(ZSetEvent::ScratchEntry, types.len() + group_columns.len())
                .map_err(StandingQueryError::Maintenance)?;
            let spec = RowAggregateSpec::new(&types, group_columns, value_column)
                .map_err(StandingQueryError::ReductionSchema)?;
            let rows = sets::rows(source).ok_or(StandingQueryError::Unsupported)?;
            if policy.rows.max_snapshot_records().is_some_and(|limit| rows.len() as u128 > u128::from(limit)) {
                return Err(StandingQueryError::Maintenance(StandingQueryFailure::SnapshotBudget));
            }
            let mut group_names = Vec::new();
            for &column in group_columns {
                meter.charge(ZSetEvent::Work).map_err(StandingQueryError::Maintenance)?;
                meter.units(ZSetEvent::ScratchEntry, 1 + names[column].len().div_ceil(64))
                    .map_err(StandingQueryError::Maintenance)?;
                group_names.push(names[column].clone());
            }
            meter.charge(ZSetEvent::ScratchEntry).map_err(StandingQueryError::Maintenance)?;
            let mut query = State { input, group_names, operator: IncrementalRowAggregate::new(spec),
                last_delta: None, policy, frontier: at, stats: StandingQueryStats::default(), failure: None };
            query.apply(rows, &mut meter).map_err(StandingQueryError::Maintenance)?;
            query.last_delta = None;
            query.stats = meter.stats;
            Ok(query)
        })
    }

    /// Borrow complete exact summaries in canonical group-key order.
    pub fn standing_reduction<'a>(&'a self, cx: &QueryCx, handle: &StandingQueryHandle)
        -> Result<StandingQueryView<'a, RowAggregateRow>, StandingQueryError> {
        let StandingQuery::Reduction(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(StandingQueryView { rows: query.operator.rows(), ordered: None,
            frontier: query.frontier, stats: &query.stats })
    }
    /// Exact latest retractions/insertions of complete summaries. None is a
    /// fresh registration/rebuild; Some(empty) is an accepted unchanged tick.
    /// This is not a retained backlog or an ACK/resume protocol.
    pub fn standing_reduction_delta<'a>(&'a self, cx: &QueryCx, handle: &StandingQueryHandle)
        -> Result<Option<StandingQueryView<'a, RowAggregateRow>>, StandingQueryError> {
        let StandingQuery::Reduction(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.last_delta.as_ref().map(|rows| StandingQueryView { rows, ordered: None,
            frontier: query.frontier, stats: &query.stats }))
    }
    pub fn standing_reduction_group_names<'a>(&'a self, cx: &QueryCx, handle: &StandingQueryHandle)
        -> Result<&'a [String], StandingQueryError> {
        let StandingQuery::Reduction(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(&query.group_names)
    }
    pub fn standing_reduction_spec<'a>(&'a self, cx: &QueryCx, handle: &StandingQueryHandle)
        -> Result<&'a RowAggregateSpec, StandingQueryError> {
        let StandingQuery::Reduction(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.spec())
    }
}

#[cfg(test)]
mod tests;
