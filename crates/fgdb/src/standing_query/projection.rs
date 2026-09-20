//! Computed projections over current maintained row/set/join/projection bags.
//! The native row kernel owns evaluation and DISTINCT. This module only wires
//! one dependency, admission, result publication and the existing failure fence.

use super::*;
use fgdb_delta_types::{LimbLimit, ZWeight};
use fgdb_gql::row_projection::{IncrementalRowProjection, RowProjectionError, RowProjectionSpec};
use fgdb_gql::{GraphSetProjection, GraphSetQuantifier};

const LIMBS: LimbLimit = LimbLimit::new(4);

pub(crate) struct State {
    pub(super) input: usize,
    columns: Vec<String>,
    operator: IncrementalRowProjection,
    last_delta: Option<ZSet<GraphValueRow>>,
    pub(super) policy: GqlQueryPolicy,
    pub(super) frontier: CommitSeq,
    pub(super) stats: StandingQueryStats,
    pub(super) failure: Option<StandingQueryFailure>,
}
fn projection_error(error: RowProjectionError<StandingQueryFailure>) -> StandingQueryFailure {
    match error {
        RowProjectionError::Delta(error) => zset_error(error),
        RowProjectionError::Expression { column, error } => {
            StandingQueryFailure::OutputExpression { column, error }
        }
        RowProjectionError::ResultBudget { .. } => StandingQueryFailure::ResultBudget,
        _ => StandingQueryFailure::InvalidDelta,
    }
}
impl State {
    pub(super) fn spec(&self) -> &RowProjectionSpec {
        self.operator.spec()
    }
    pub(super) fn columns(&self) -> &[String] {
        &self.columns
    }
    pub(super) fn rows(&self) -> &ZSet<GraphValueRow> {
        self.operator.rows()
    }
    pub(super) fn delta(&self) -> Option<&ZSet<GraphValueRow>> {
        self.last_delta.as_ref()
    }

    fn prepare_and_publish(
        &mut self,
        delta: &ZSet<GraphValueRow>,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        let pending = self
            .operator
            .prepare(
                delta,
                LIMBS,
                meter.policy.rows.max_result_rows(),
                &mut |event| meter.charge(event),
            )
            .map_err(projection_error)?;
        (meter.checkpoint)()?;
        // No recoverable work between accepted input/output and derivative.
        self.last_delta = Some(pending.commit());
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
        let parent = sets::input_at(sources, self.input, at)?;
        let delta = sets::delta(parent).ok_or(StandingQueryFailure::DependencyUnavailable)?;
        meter.stats.delta_rows =
            u64::try_from(delta.len()).map_err(|_| StandingQueryFailure::WorkBudget)?;
        self.prepare_and_publish(delta, meter)
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Compute checked native output expressions over a maintained bag. Parent
    /// selection, DISTINCT, ORDER BY and paging happen BEFORE this projection.
    /// ALL sums colliding images; DISTINCT keeps an image until its last input
    /// occurrence disappears. Constants still emit once per input occurrence.
    /// NULL, native identities, lazy scalar expressions and lists retain the
    /// same semantics as GraphSetProjection; there is no alternative evaluator.
    ///
    /// Parent rows/sets/joins/projections can be shared and nested. Each update
    /// consumes the parent's immediate-successor derivative, never its whole
    /// result or an unavailable/new-baseline surrogate. Failure fences only this
    /// child and its dependents, not durable writes or healthy siblings. Rebuild
    /// unavailable parents first, then repair this view through its same handle.
    ///
    /// Snapshot admission counts compressed parent tuples; final result quotas
    /// count occurrences after DISTINCT, without expansion. Work/scratch govern
    /// copies and evaluation under one per-view allowance, not circuit-wide or
    /// byte-memory bounds. Session-local and in-memory; no durable feed or spill.
    pub fn register_standing_projection(
        &mut self,
        cx: &QueryCx,
        input: &StandingQueryHandle,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        let source = self.admitted_standing_query(cx, input)?;
        let columns = sets::columns(source).ok_or(StandingQueryError::Unsupported)?;
        let mut types = Vec::new();
        for column in 0..columns.len() {
            cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
            types.push(sets::column_type(source, column).ok_or(StandingQueryError::Unsupported)?);
        }
        let spec = RowProjectionSpec::new(types, projection, quantifier)
            .map_err(StandingQueryError::ProjectionSchema)?;
        let state = self.prepare_standing_projection(
            cx,
            input.index,
            spec,
            policy,
            self.standing_queries.len(),
        )?;
        Ok(self.store_standing_query(StandingQuery::Projection(Box::new(state))))
    }

    /// Register a frozen selection/projection definition over an existing view.
    /// `RowProjectionSpec::selection` preserves the input bag's columns, while
    /// `RowProjectionSpec::with_filter` selects before evaluating output values
    /// and DISTINCT. FALSE/UNKNOWN skip expressions, but their input counts
    /// remain checked so hidden over-retractions cannot pass unnoticed.
    ///
    /// The declared input types must exactly match the admitted parent's schema,
    /// including for empty inputs. Registration is atomic; subsequent failures
    /// fence this view and its dependents without undoing durable graph writes.
    /// Use the ordinary `standing_projection*` accessors and rebuild API. Parent
    /// paging is upstream; this stage returns an unordered compressed bag.
    /// Session-local, in-memory, and governed by the same per-view allowances as
    /// `register_standing_projection`, not a durable feed or circuit-wide quota.
    pub fn register_standing_projection_spec(
        &mut self,
        cx: &QueryCx,
        input: &StandingQueryHandle,
        spec: RowProjectionSpec,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        self.admitted_standing_query(cx, input)?;
        let state = self.prepare_standing_projection(
            cx,
            input.index,
            spec,
            policy,
            self.standing_queries.len(),
        )?;
        Ok(self.store_standing_query(StandingQuery::Projection(Box::new(state))))
    }

    pub(super) fn prepare_standing_projection(
        &self,
        cx: &QueryCx,
        input: usize,
        spec: RowProjectionSpec,
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
            if names.len() != spec.input_types().len() {
                return Err(StandingQueryError::Unsupported);
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
            for (column, kind) in spec.input_types().iter().enumerate() {
                meter
                    .charge(ZSetEvent::Work)
                    .map_err(StandingQueryError::Maintenance)?;
                if sets::column_type(parent, column) != Some(*kind) {
                    return Err(StandingQueryError::Unsupported);
                }
            }
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
            let mut columns = Vec::new();
            for name in spec.columns() {
                meter
                    .charge(ZSetEvent::Work)
                    .map_err(StandingQueryError::Maintenance)?;
                meter
                    .units(ZSetEvent::ScratchEntry, 1 + name.len().div_ceil(64))
                    .map_err(StandingQueryError::Maintenance)?;
                columns.push(name.to_owned());
            }
            let mut state = State {
                input,
                columns,
                operator: IncrementalRowProjection::new(spec),
                last_delta: None,
                policy,
                frontier: at,
                stats: StandingQueryStats::default(),
                failure: None,
            };
            state
                .prepare_and_publish(rows, &mut meter)
                .map_err(StandingQueryError::Maintenance)?;
            state.last_delta = None; // An initialized/rebuilt baseline is not a successor.
            state.stats = meter.stats;
            Ok(state)
        })
    }

    /// Borrow the current compressed projected bag in native canonical order.
    /// No separate ranked output stage is implied; ordered_rows() is None.
    pub fn standing_projection<'a>(
        &'a self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
    ) -> Result<StandingQueryView<'a, GraphValueRow>, StandingQueryError> {
        let StandingQuery::Projection(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(StandingQueryView {
            rows: query.rows(),
            ordered: None,
            frontier: query.frontier,
            stats: &query.stats,
        })
    }
    /// Latest accepted derivative only, not a backlog. None denotes a new
    /// baseline; Some(empty) denotes an accepted successor with no result change.
    pub fn standing_projection_delta<'a>(
        &'a self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
    ) -> Result<Option<StandingQueryView<'a, GraphValueRow>>, StandingQueryError> {
        let StandingQuery::Projection(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.delta().map(|rows| StandingQueryView {
            rows,
            ordered: None,
            frontier: query.frontier,
            stats: &query.stats,
        }))
    }
    pub fn standing_projection_columns<'a>(
        &'a self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
    ) -> Result<&'a [String], StandingQueryError> {
        let StandingQuery::Projection(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.columns())
    }
    pub fn standing_projection_total<'a>(
        &'a self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
    ) -> Result<&'a ZWeight, StandingQueryError> {
        let StandingQuery::Projection(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.operator.total())
    }
}

#[cfg(test)]
mod tests;
