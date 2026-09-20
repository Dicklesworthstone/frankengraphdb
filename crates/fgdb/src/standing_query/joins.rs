//! Dependency-ordered equijoins of maintained row, set and join bags.
//!
//! The native row/schema adapter owns exact join arithmetic and its prepared
//! sink. This module owns only registry dependencies, source/budget admission,
//! current-frontier publication and rebuild. No base-graph rescan per tick.

use super::*;
use fgdb_delta_types::{LimbLimit, ZWeight};
use fgdb_gql::row_join::{IncrementalRowJoin, RowJoinBuildError, RowJoinError, RowJoinSpec};
use fgdb_gql::algebra::MAX_PATTERN_VERTICES;

const LIMBS: LimbLimit = LimbLimit::new(4);

pub(crate) struct State {
    pub(super) inputs: [usize; 2],
    columns: Vec<String>,
    operator: IncrementalRowJoin,
    last_delta: Option<ZSet<GraphValueRow>>,
    pub(super) policy: GqlQueryPolicy,
    pub(super) frontier: CommitSeq,
    pub(super) stats: StandingQueryStats,
    pub(super) failure: Option<StandingQueryFailure>,
}
fn join_error(error: RowJoinError<StandingQueryFailure>) -> StandingQueryFailure {
    match error {
        RowJoinError::Delta(error) => zset_error(error),
        RowJoinError::ResultBudget { .. } => StandingQueryFailure::ResultBudget,
        RowJoinError::InputSchema { .. } | RowJoinError::NegativeMultiplicity { .. }
        | RowJoinError::InvalidResult => StandingQueryFailure::InvalidDelta,
    }
}
impl State {
    pub(super) fn spec(&self) -> &RowJoinSpec { self.operator.spec() }
    pub(super) fn columns(&self) -> &[String] { &self.columns }
    pub(super) fn rows(&self) -> &ZSet<GraphValueRow> { self.operator.rows() }
    pub(super) fn delta(&self) -> Option<&ZSet<GraphValueRow>> { self.last_delta.as_ref() }

    fn apply(&mut self, left: &ZSet<GraphValueRow>, right: &ZSet<GraphValueRow>, meter: &mut Meter<'_>)
        -> Result<(), StandingQueryFailure> {
        let pending = self.operator.prepare(left, right, LIMBS, meter.policy.rows.max_result_rows(),
            &mut |event| meter.charge(event)).map_err(join_error)?;
        // One final registry checkpoint while input and output are tentative.
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
        let left = sets::input_at(sources, self.inputs[0], at)?;
        meter.charge(ZSetEvent::Work)?;
        let right = sets::input_at(sources, self.inputs[1], at)?;
        let left = sets::delta(left).ok_or(StandingQueryFailure::DependencyUnavailable)?;
        let right = sets::delta(right).ok_or(StandingQueryFailure::DependencyUnavailable)?;
        meter.stats.delta_rows = u64::try_from(left.len()).ok()
            .and_then(|left| u64::try_from(right.len()).ok().and_then(|right| left.checked_add(right)))
            .ok_or(StandingQueryFailure::WorkBudget)?;
        self.apply(left, right, meter)
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Maintain an inner equijoin of current row/set/join views. Keys are
    /// zero-based (left column, right column) pairs. All key components must
    /// match canonically and none may be NULL; scalar and vertex domains never
    /// coerce. Output concatenates left then right columns, named `left.<name>`
    /// and `right.<name>`. At most 64 total scalar/vertex columns are admitted.
    /// This is explicit canonical-key equality, not arbitrary WHERE predicates.
    ///
    /// Both complete input derivatives must name the same immediate successor.
    /// Shared/self operands are valid and maintained once; joins and sets may
    /// feed later joins/sets in the registry's acyclic publication order. An
    /// unavailable operand or missing derivative is never an empty bag. A child
    /// failure does not undo the durable write or accepted parents/siblings.
    /// Repair parents before rebuilding the child with rebuild_standing_query.
    ///
    /// ALL multiplicities multiply exactly without expanded duplicate rows.
    /// Initialization/rebuild admits the sum of compressed operand support as
    /// max_snapshot_records (shared operands count twice); final occurrences
    /// obey max_result_rows. Per-view work/scratch cover all input, key, product
    /// and output preparation. No per-tick full-graph or full-result difference
    /// is computed. A large matching group can still produce a quadratic result.
    /// These are session-local logical/payload quotas, not circuit-wide or byte
    /// bounds, spill, a durable feed, new GQL syntax or FreeJoin/WCOJ execution.
    pub fn register_standing_join(
        &mut self, cx: &QueryCx, left: &StandingQueryHandle, right: &StandingQueryHandle,
        keys: &[(usize, usize)], policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        if !Arc::ptr_eq(&self.handle_owner, &left.owner) || !Arc::ptr_eq(&self.handle_owner, &right.owner) {
            return Err(StandingQueryError::ForeignHandle);
        }
        for handle in [left, right] {
            if sets::rows(self.admitted_standing_query(cx, handle)?).is_none() {
                return Err(StandingQueryError::Unsupported);
            }
        }
        let query = self.prepare_standing_join(cx, [left.index, right.index], keys, policy,
            self.standing_queries.len())?;
        Ok(self.store_standing_query(StandingQuery::Join(Box::new(query))))
    }

    pub(super) fn prepare_standing_join(
        &self, cx: &QueryCx, inputs: [usize; 2], keys: &[(usize, usize)],
        policy: GqlQueryPolicy, before: usize,
    ) -> Result<State, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        let sources = self.standing_queries.get(..before).ok_or(StandingQueryError::UnknownHandle)?;
        let at = self.snapshot.frontier;
        cx.with_restriction(|| {
            let left = sets::input_at(sources, inputs[0], at).map_err(StandingQueryError::Maintenance)?;
            let right = sets::input_at(sources, inputs[1], at).map_err(StandingQueryError::Maintenance)?;
            let left_names = sets::columns(left).ok_or(StandingQueryError::Unsupported)?;
            let right_names = sets::columns(right).ok_or(StandingQueryError::Unsupported)?;
            let width = left_names.len().checked_add(right_names.len()).unwrap_or(usize::MAX);
            if width > MAX_PATTERN_VERTICES {
                return Err(StandingQueryError::JoinSchema(RowJoinBuildError::TooManyColumns { observed: width }));
            }
            // Validate bounded metadata even for empty bags, before row copies.
            let mut checkpoint = || cx.checkpoint().map_err(|_| StandingQueryFailure::Interrupted);
            let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            let mut types = [Vec::new(), Vec::new()];
            for (side, (query, names)) in [(left, left_names), (right, right_names)].into_iter().enumerate() {
                for column in 0..names.len() {
                    meter.charge(ZSetEvent::Work).map_err(StandingQueryError::Maintenance)?;
                    meter.charge(ZSetEvent::ScratchEntry).map_err(StandingQueryError::Maintenance)?;
                    types[side].push(sets::column_type(query, column).ok_or(StandingQueryError::Unsupported)?);
                }
            }
            if keys.len() > MAX_PATTERN_VERTICES {
                return Err(StandingQueryError::JoinSchema(RowJoinBuildError::TooManyKeys));
            }
            meter.units(ZSetEvent::ScratchEntry, width + keys.len()).map_err(StandingQueryError::Maintenance)?;
            let spec = RowJoinSpec::new(&types[0], &types[1], keys).map_err(StandingQueryError::JoinSchema)?;
            let left_rows = sets::rows(left).ok_or(StandingQueryError::Unsupported)?;
            let right_rows = sets::rows(right).ok_or(StandingQueryError::Unsupported)?;
            let records = (left_rows.len() as u128) + (right_rows.len() as u128);
            if policy.rows.max_snapshot_records().is_some_and(|limit| records > u128::from(limit)) {
                return Err(StandingQueryError::Maintenance(StandingQueryFailure::SnapshotBudget));
            }
            let mut columns = Vec::new();
            for (prefix, names) in [("left.", left_names), ("right.", right_names)] {
                for name in names {
                    meter.charge(ZSetEvent::Work).map_err(StandingQueryError::Maintenance)?;
                    let bytes = prefix.len().checked_add(name.len())
                        .ok_or(StandingQueryError::Maintenance(StandingQueryFailure::ScratchBudget))?;
                    meter.units(ZSetEvent::ScratchEntry, 1 + bytes.div_ceil(64))
                        .map_err(StandingQueryError::Maintenance)?;
                    columns.push(format!("{prefix}{name}"));
                }
            }
            meter.charge(ZSetEvent::ScratchEntry).map_err(StandingQueryError::Maintenance)?;
            let mut query = State { inputs, columns, operator: IncrementalRowJoin::new(spec),
                last_delta: None, policy, frontier: at, stats: StandingQueryStats::default(), failure: None };
            query.apply(left_rows, right_rows, &mut meter).map_err(StandingQueryError::Maintenance)?;
            query.last_delta = None; // A fresh baseline is not a committed successor.
            query.stats = meter.stats;
            Ok(query)
        })
    }

    /// Borrow the exact current compressed join result in canonical row order.
    pub fn standing_join<'a>(&'a self, cx: &QueryCx, handle: &StandingQueryHandle)
        -> Result<StandingQueryView<'a, GraphValueRow>, StandingQueryError> {
        let StandingQuery::Join(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(StandingQueryView { rows: query.rows(), ordered: None, frontier: query.frontier, stats: &query.stats })
    }
    /// The latest accepted tick only: None after registration/rebuild, Some(empty)
    /// for an accepted no-change tick. This is not a backlog, ACK or resume API.
    pub fn standing_join_delta<'a>(&'a self, cx: &QueryCx, handle: &StandingQueryHandle)
        -> Result<Option<StandingQueryView<'a, GraphValueRow>>, StandingQueryError> {
        let StandingQuery::Join(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.delta().map(|rows| StandingQueryView { rows, ordered: None,
            frontier: query.frontier, stats: &query.stats }))
    }
    pub fn standing_join_columns<'a>(&'a self, cx: &QueryCx, handle: &StandingQueryHandle)
        -> Result<&'a [String], StandingQueryError> {
        let StandingQuery::Join(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.columns())
    }
    /// Exact accepted occurrence total, without scanning or expanding rows.
    pub fn standing_join_total<'a>(&'a self, cx: &QueryCx, handle: &StandingQueryHandle)
        -> Result<&'a ZWeight, StandingQueryError> {
        let StandingQuery::Join(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.operator.total())
    }
}

#[cfg(test)]
mod tests;
