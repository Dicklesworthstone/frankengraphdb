//! Selection over maintained bags. The existing native RowPredicate owns
//! Boolean evaluation; this adapter owns one registry dependency and lifecycle.

use super::*;
use fgdb_delta_types::{LimbLimit, ZWeight};
use fgdb_gql::GraphSetPredicateOp;
use fgdb_gql::row_filter::{IncrementalRowFilter, RowFilterError, RowFilterSpec};

const LIMBS: LimbLimit = LimbLimit::new(4);

pub(crate) struct State {
    pub(super) input: usize,
    columns: Vec<String>,
    operator: IncrementalRowFilter,
    last_delta: Option<ZSet<GraphValueRow>>,
    pub(super) policy: GqlQueryPolicy,
    pub(super) frontier: CommitSeq,
    pub(super) stats: StandingQueryStats,
    pub(super) failure: Option<StandingQueryFailure>,
}
fn filter_error(error: RowFilterError<StandingQueryFailure>) -> StandingQueryFailure {
    match error {
        RowFilterError::Delta(error) => zset_error(error),
        RowFilterError::ResultBudget { .. } => StandingQueryFailure::ResultBudget,
        _ => StandingQueryFailure::InvalidDelta,
    }
}
impl State {
    pub(super) fn spec(&self) -> &RowFilterSpec { self.operator.spec() }
    pub(super) fn columns(&self) -> &[String] { &self.columns }
    pub(super) fn rows(&self) -> &ZSet<GraphValueRow> { self.operator.rows() }
    pub(super) fn delta(&self) -> Option<&ZSet<GraphValueRow>> { self.last_delta.as_ref() }

    fn prepare_and_publish(&mut self, delta: &ZSet<GraphValueRow>, meter: &mut Meter<'_>)
        -> Result<(), StandingQueryFailure> {
        let pending = self.operator.prepare(delta, LIMBS, meter.policy.rows.max_result_rows(),
            &mut |event| meter.charge(event)).map_err(filter_error)?;
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
        let parent = sets::input_at(sources, self.input, at)?;
        let delta = sets::delta(parent).ok_or(StandingQueryFailure::DependencyUnavailable)?;
        meter.stats.delta_rows = u64::try_from(delta.len()).map_err(|_| StandingQueryFailure::WorkBudget)?;
        self.prepare_and_publish(delta, meter)
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Filter a maintained row/set/join/projection/filter bag using the same
    /// checked three-valued predicate law as relational WHERE. Only TRUE keeps
    /// an occurrence. NULL and incompatible scalar kinds remain UNKNOWN even
    /// beneath NOT. Project computed operands first, then filter their columns.
    /// Input names/types and exact duplicate counts are preserved, not coerced.
    ///
    /// The parent's selected DISTINCT/order/page is upstream of the predicate.
    /// This API exposes an unranked bag; it introduces no new ORDER BY or page.
    /// Shared and nested dependencies use the existing append-ordered registry.
    /// Each tick uses the complete immediate-successor derivative or fences the
    /// child unavailable; no rescan, eager fallback or complete-result difference.
    /// Rebuild unavailable parents first, then this handle. Durable writes and
    /// healthy siblings are not undone by a derived-view failure.
    ///
    /// Initialization counts compressed parent tuples for snapshot admission.
    /// Final result quotas count selected occurrences, not hidden input counts.
    /// Work/scratch are per-view logical events and payload units, not circuit-
    /// wide or allocator-byte bounds. Session-local; no durable feed or spill.
    pub fn register_standing_filter(&mut self, cx: &QueryCx, input: &StandingQueryHandle,
        code: &[GraphSetPredicateOp], policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        let source = self.admitted_standing_query(cx, input)?;
        let names = sets::columns(source).ok_or(StandingQueryError::Unsupported)?;
        let mut types = Vec::new();
        for column in 0..names.len() {
            cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
            types.push(sets::column_type(source, column).ok_or(StandingQueryError::Unsupported)?);
        }
        let spec = RowFilterSpec::new(types, code).map_err(StandingQueryError::FilterSchema)?;
        let state = self.prepare_standing_filter(cx, input.index, spec, policy, self.standing_queries.len())?;
        Ok(self.store_standing_query(StandingQuery::Filter(Box::new(state))))
    }

    pub(super) fn prepare_standing_filter(&self, cx: &QueryCx, input: usize,
        spec: RowFilterSpec, policy: GqlQueryPolicy, before: usize,
    ) -> Result<State, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        let sources = self.standing_queries.get(..before).ok_or(StandingQueryError::UnknownHandle)?;
        let at = self.snapshot.frontier;
        cx.with_restriction(|| {
            let parent = sets::input_at(sources, input, at).map_err(StandingQueryError::Maintenance)?;
            let names = sets::columns(parent).ok_or(StandingQueryError::Unsupported)?;
            if names.len() != spec.column_types().len() { return Err(StandingQueryError::Unsupported); }
            let mut checkpoint = || cx.checkpoint().map_err(|_| StandingQueryFailure::Interrupted);
            let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            for (column, kind) in spec.column_types().iter().enumerate() {
                meter.charge(ZSetEvent::Work).map_err(StandingQueryError::Maintenance)?;
                if sets::column_type(parent, column) != Some(*kind) { return Err(StandingQueryError::Unsupported); }
            }
            let rows = sets::rows(parent).ok_or(StandingQueryError::Unsupported)?;
            if policy.rows.max_snapshot_records().is_some_and(|limit| rows.len() as u128 > u128::from(limit)) {
                return Err(StandingQueryError::Maintenance(StandingQueryFailure::SnapshotBudget));
            }
            let mut columns = Vec::new();
            for name in names {
                meter.charge(ZSetEvent::Work).map_err(StandingQueryError::Maintenance)?;
                meter.units(ZSetEvent::ScratchEntry, 1 + name.len().div_ceil(64))
                    .map_err(StandingQueryError::Maintenance)?;
                columns.push(name.clone());
            }
            let mut state = State { input, columns, operator: IncrementalRowFilter::new(spec),
                last_delta: None, policy, frontier: at, stats: StandingQueryStats::default(), failure: None };
            state.prepare_and_publish(rows, &mut meter).map_err(StandingQueryError::Maintenance)?;
            state.last_delta = None;
            state.stats = meter.stats;
            Ok(state)
        })
    }

    /// Current selected bag. Canonical key order is not an ORDER BY contract.
    pub fn standing_filter<'a>(&'a self, cx: &QueryCx, handle: &StandingQueryHandle)
        -> Result<StandingQueryView<'a, GraphValueRow>, StandingQueryError> {
        let StandingQuery::Filter(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(StandingQueryView { rows: query.rows(), ordered: None, frontier: query.frontier, stats: &query.stats })
    }
    /// None means a new baseline. Some(empty) means an accepted successor with
    /// no selected-row change. This retains only the latest tick, not a backlog.
    pub fn standing_filter_delta<'a>(&'a self, cx: &QueryCx, handle: &StandingQueryHandle)
        -> Result<Option<StandingQueryView<'a, GraphValueRow>>, StandingQueryError> {
        let StandingQuery::Filter(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.delta().map(|rows| StandingQueryView { rows, ordered: None,
            frontier: query.frontier, stats: &query.stats }))
    }
    pub fn standing_filter_columns<'a>(&'a self, cx: &QueryCx, handle: &StandingQueryHandle)
        -> Result<&'a [String], StandingQueryError> {
        let StandingQuery::Filter(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.columns())
    }
    pub fn standing_filter_total<'a>(&'a self, cx: &QueryCx, handle: &StandingQueryHandle)
        -> Result<&'a ZWeight, StandingQueryError> {
        let StandingQuery::Filter(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.operator.total())
    }
}

#[cfg(test)]
mod tests;
