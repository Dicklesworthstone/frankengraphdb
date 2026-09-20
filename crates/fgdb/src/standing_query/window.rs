//! Ranked finite pages over existing maintained native row bags.
//!
//! The native window kernel owns DISTINCT, comparison, weighted offset and
//! deletion/refill. This adapter owns one registry dependency and a canonical
//! bag sink for downstream consumers. Ordered output stays compressed; it is
//! never expanded into one retained Arc per occurrence.

use super::*;
use fgdb_delta_types::{LimbLimit, ZWeight};
use fgdb_gql::algebra::GraphValueOrder;
use fgdb_gql::row_window::{IncrementalRowWindow, RowWindowError, RowWindowSpec};
use fgdb_gql::GraphSetQuantifier;

const LIMBS: LimbLimit = LimbLimit::new(4);

pub(crate) struct State {
    pub(super) input: usize,
    columns: Vec<String>,
    operator: IncrementalRowWindow,
    rows: ZSet<GraphValueRow>,
    last_delta: Option<ZSet<GraphValueRow>>,
    pub(super) policy: GqlQueryPolicy,
    pub(super) frontier: CommitSeq,
    pub(super) stats: StandingQueryStats,
    pub(super) failure: Option<StandingQueryFailure>,
}

fn window_error(error: RowWindowError<StandingQueryFailure>) -> StandingQueryFailure {
    match error {
        RowWindowError::Delta(error) => zset_error(error),
        RowWindowError::ResultBudget { .. } => StandingQueryFailure::ResultBudget,
        RowWindowError::InputSchema | RowWindowError::NegativeMultiplicity => {
            StandingQueryFailure::InvalidDelta
        }
    }
}

impl State {
    pub(super) fn spec(&self) -> &RowWindowSpec { self.operator.spec() }
    pub(super) fn columns(&self) -> &[String] { &self.columns }
    pub(super) fn rows(&self) -> &ZSet<GraphValueRow> { &self.rows }
    pub(super) fn delta(&self) -> Option<&ZSet<GraphValueRow>> { self.last_delta.as_ref() }
    pub(super) fn ordered(&self)
        -> impl DoubleEndedIterator<Item = (&GraphValueRow, &ZWeight)> + ExactSizeIterator {
        self.operator.rows()
    }

    fn apply(&mut self, delta: &ZSet<GraphValueRow>, meter: &mut Meter<'_>)
        -> Result<(), StandingQueryFailure> {
        let pending = self.operator.prepare(delta, LIMBS, meter.policy.rows.max_result_rows(),
            &mut |event| meter.charge(event)).map_err(window_error)?;
        // The kernel owns the ordered page. The canonical sink is a different
        // index over only that page, not a second copy of all source candidates.
        for (row, _) in pending.delta().iter() {
            for _ in 0..2 {
                meter.charge(ZSetEvent::ScratchEntry)?;
                for value in row.values() {
                    meter.charge(ZSetEvent::Work)?;
                    let units = value.payload_units().checked_add(1)
                        .ok_or(StandingQueryFailure::ScratchBudget)?;
                    meter.units(ZSetEvent::ScratchEntry, units)?;
                }
            }
        }
        let sink = self.rows.prepare_update(pending.delta(), LIMBS,
            &mut |event| meter.charge(event)).map_err(zset_error)?;
        (meter.checkpoint)()?;
        // No recoverable work between the ordered page, bag, total and delta.
        let delta = pending.commit();
        sink.commit();
        self.last_delta = Some(delta);
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
        meter.stats.delta_rows = u64::try_from(delta.len())
            .map_err(|_| StandingQueryFailure::WorkBudget)?;
        self.apply(delta, meter)
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Maintain a finite ordered occurrence window over a row/set/join/
    /// projection/filter/window result. The parent's complete selection stays
    /// upstream. This stage applies ALL or whole-row DISTINCT, then its own
    /// order, offset and count. Empty order means canonical whole-row order;
    /// ties use complete rows and NULL placement is independent of direction.
    ///
    /// Retain candidates outside the page so deletions can refill it. OFFSET
    /// skips duplicate runs arithmetically. Neither page storage nor its ordered
    /// accessor expands multiplicities. LIMIT 0 still validates every update
    /// and observes dependency failure. A failed view never undoes a durable
    /// write or blocks healthy siblings. Rebuild parents before their children.
    ///
    /// Source admission counts compressed parent tuples; result admission counts
    /// final selected occurrences. Logical work/scratch govern the existing
    /// in-memory kernel and sink, not allocator bytes or total-circuit memory.
    /// No durable subscription, arbitrary unbounded sorting or spill is added.
    #[allow(clippy::too_many_arguments)]
    pub fn register_standing_window(&mut self, cx: &QueryCx, input: &StandingQueryHandle,
        order: &[GraphValueOrder], quantifier: GraphSetQuantifier, offset: u64, count: u64,
        policy: GqlQueryPolicy) -> Result<StandingQueryHandle, StandingQueryError> {
        let parent = self.admitted_standing_query(cx, input)?;
        let names = sets::columns(parent).ok_or(StandingQueryError::Unsupported)?;
        let mut types = Vec::new();
        for column in 0..names.len() {
            cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
            types.push(sets::column_type(parent, column).ok_or(StandingQueryError::Unsupported)?);
        }
        let spec = RowWindowSpec::new(types, order.to_vec(), quantifier, offset, count)
            .map_err(StandingQueryError::WindowSchema)?;
        let query = self.prepare_standing_window(cx, input.index, spec, policy,
            self.standing_queries.len())?;
        Ok(self.store_standing_query(StandingQuery::Window(Box::new(query))))
    }

    pub(super) fn prepare_standing_window(&self, cx: &QueryCx, input: usize,
        spec: RowWindowSpec, policy: GqlQueryPolicy, before: usize)
        -> Result<State, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        let sources = self.standing_queries.get(..before).ok_or(StandingQueryError::UnknownHandle)?;
        let at = self.snapshot.frontier;
        cx.with_restriction(|| {
            let parent = sets::input_at(sources, input, at).map_err(StandingQueryError::Maintenance)?;
            let names = sets::columns(parent).ok_or(StandingQueryError::Unsupported)?;
            if names.len() != spec.input_types().len() { return Err(StandingQueryError::Unsupported); }
            let mut checkpoint = || cx.checkpoint().map_err(|_| StandingQueryFailure::Interrupted);
            let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            for (column, kind) in spec.input_types().iter().enumerate() {
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
            let mut query = State { input, columns, operator: IncrementalRowWindow::new(spec),
                rows: ZSet::new(), last_delta: None, policy, frontier: at,
                stats: StandingQueryStats::default(), failure: None };
            query.apply(rows, &mut meter).map_err(StandingQueryError::Maintenance)?;
            query.last_delta = None; // A rebuilt baseline is not a successor delta.
            query.stats = meter.stats;
            Ok(query)
        })
    }

    /// The selected bag, in canonical key order rather than ORDER BY order.
    /// ordered_rows() is None: use standing_window_ordered for compressed rank.
    pub fn standing_window<'a>(&'a self, cx: &QueryCx, handle: &StandingQueryHandle)
        -> Result<StandingQueryView<'a, GraphValueRow>, StandingQueryError> {
        let StandingQuery::Window(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(StandingQueryView { rows: query.rows(), ordered: None,
            frontier: query.frontier, stats: &query.stats })
    }

    /// Ordered (tuple, occurrence-count) runs, borrowing the current generation.
    /// The iterator length is DISTINCT TUPLES, not the number of occurrences.
    /// No row copies, occurrence expansion, sorting or source reads occur here.
    pub fn standing_window_ordered<'a>(&'a self, cx: &QueryCx, handle: &StandingQueryHandle)
        -> Result<impl DoubleEndedIterator<Item = (&'a GraphValueRow, &'a ZWeight)>
            + ExactSizeIterator + 'a + use<'a, V>, StandingQueryError> {
        let StandingQuery::Window(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.ordered())
    }

    /// Latest accepted page change; None means a fresh baseline, not no change.
    pub fn standing_window_delta<'a>(&'a self, cx: &QueryCx, handle: &StandingQueryHandle)
        -> Result<Option<StandingQueryView<'a, GraphValueRow>>, StandingQueryError> {
        let StandingQuery::Window(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.delta().map(|rows| StandingQueryView { rows, ordered: None,
            frontier: query.frontier, stats: &query.stats }))
    }

    pub fn standing_window_columns<'a>(&'a self, cx: &QueryCx, handle: &StandingQueryHandle)
        -> Result<&'a [String], StandingQueryError> {
        let StandingQuery::Window(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.columns())
    }

    pub fn standing_window_total<'a>(&'a self, cx: &QueryCx, handle: &StandingQueryHandle)
        -> Result<&'a ZWeight, StandingQueryError> {
        let StandingQuery::Window(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.operator.total())
    }
}

#[cfg(test)]
mod tests;
