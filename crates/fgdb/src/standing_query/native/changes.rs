//! Compressed native baseline and one-tick delivery from published maintained
//! bags. No graph source, expression evaluator or result re-execution occurs.

mod statement;
mod subscription;

use super::*;
use super::{copy_group as aggregate_cells, copy_values as row_cells};
use fgdb_delta_types::LimbLimit;

const LIMBS: LimbLimit = LimbLimit::new(4);

impl StandingQuery {
    /// Commit-hook projection shares the ordinary native delivery mapper.
    /// The caller already checked this source's complete successor/failure.
    /// An absent derivative denotes a baseline, never an unchanged tick.
    pub(in crate::standing_query) fn native_delta_for_replay(
        &self,
        layout: &Layout,
        meter: &mut Meter<'_>,
    ) -> Result<ZSet<Vec<QueryValue>>, StandingQueryFailure> {
        let width = layout.columns().len();
        match layout {
            Layout::Rows { .. } | Layout::Circuit { .. } => {
                let rows = sets::delta(self).ok_or(StandingQueryFailure::DependencyUnavailable)?;
                collect_bag(rows, true, width, meter, row_cells)
            }
            Layout::Aggregate { slots, .. } | Layout::GroupCircuit { slots, .. } => {
                let rows = match self {
                    Self::Aggregate(query) | Self::ProjectedAggregate { source: query, .. } => {
                        query.last_delta.as_ref()
                    }
                    Self::Group(query) => query.delta(),
                    _ => None,
                }
                .ok_or(StandingQueryFailure::DependencyUnavailable)?;
                collect_bag(rows, true, width, meter, |row, meter| {
                    aggregate_cells(row, slots, meter)
                })
            }
        }
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Copy the current selected native result as an exact compressed bag.
    /// Cell positions and types match standing_native_query; column names are
    /// available through standing_native_columns. Duplicate occurrences remain
    /// signed-kernel weights instead of being expanded into repeated rows.
    /// The returned frontier and bag share one immutable database borrow.
    ///
    /// This is a BAG baseline, not an ordered sequence. ORDER BY and pages have
    /// already selected the result, but rank-only changes are not represented.
    /// For an ordered presentation use standing_native_query instead.
    ///
    /// Delivery ResultRows counts distinct returned tuples, not occurrences.
    /// Work and scratch are cumulative, including payload copies and exact
    /// consolidation; max_snapshot_records is unused (no graph reads). Weight
    /// magnitude does not cause occurrence expansion or narrowing to u64/i128.
    /// A refusal releases no partial bag and cannot fence the maintained view.
    /// Logical entry/payload events are not byte-accurate allocator bounds;
    /// B-tree key comparisons retain the existing Z-set cost contract.
    pub fn standing_native_bag(
        &self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
        policy: GqlQueryPolicy,
    ) -> Result<(CommitSeq, ZSet<Vec<QueryValue>>), StandingQueryError> {
        self.native_bag_at(cx, handle, None, policy)
    }

    /// Copy the exact latest-tick native BAG derivative from `from` to the
    /// current frontier. Apply it once to a baseline from this SAME handle.
    /// Retractions remain negative; an accepted no-change successor returns
    /// an empty Z-set. Repeated reads before another commit return the same
    /// derivative; reading is not acknowledgement or destructive consumption.
    ///
    /// DeltaUnavailable means registration/rebuild established a new baseline,
    /// or `from` is not the immediate predecessor of the published frontier.
    /// This rejects skipped commits, a repeated already-applied frontier, and
    /// future cuts rather than treating any of them as an empty change. After
    /// a gap, obtain a fresh standing_native_bag baseline. Only one tick is
    /// retained; this is not a durable subscription, cursor, or backlog.
    ///
    /// The compressed delivery policy is the same as standing_native_bag:
    /// ResultRows bounds final changed tuples AFTER native-slot consolidation,
    /// not the sum or absolute magnitude of their signed weights. These are
    /// changes to final results, after HAVING, DISTINCT and selected pages,
    /// never changes to private source support or hidden aggregate groups.
    pub fn standing_native_delta(
        &self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
        from: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<(CommitSeq, ZSet<Vec<QueryValue>>), StandingQueryError> {
        self.native_bag_at(cx, handle, Some(from), policy)
    }

    fn native_bag_at(
        &self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
        from: Option<CommitSeq>,
        policy: GqlQueryPolicy,
    ) -> Result<(CommitSeq, ZSet<Vec<QueryValue>>), StandingQueryError> {
        let query = self.admitted_standing_query(cx, handle)?;
        let layout = handle
            .native
            .as_deref()
            .ok_or(StandingQueryError::Unsupported)?;
        let frontier = query.status().1;
        let unavailable = |from| StandingQueryError::DeltaUnavailable { from, frontier };
        if let Some(from) = from
            && from.checked_successor().ok() != Some(frontier)
        {
            return Err(unavailable(from));
        }
        cx.with_restriction(|| {
            let mut checkpoint = || {
                cx.checkpoint()
                    .map_err(|_| StandingQueryFailure::Interrupted)
            };
            let mut meter = Meter {
                policy,
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            let width = layout.columns().len();
            let rows = match layout {
                Layout::Rows { .. } | Layout::Circuit { .. } => {
                    let rows = match from {
                        Some(from) => sets::delta(query).ok_or_else(|| unavailable(from))?,
                        None => sets::rows(query).ok_or(StandingQueryError::Unsupported)?,
                    };
                    collect_bag(rows, from.is_some(), width, &mut meter, row_cells)
                }
                Layout::Aggregate { slots, .. } | Layout::GroupCircuit { slots, .. } => {
                    let view = match from {
                        Some(from) => self
                            .standing_query_delta(cx, handle)?
                            .ok_or_else(|| unavailable(from))?,
                        None => self.standing_query(cx, handle)?,
                    };
                    collect_bag(
                        view.rows(),
                        from.is_some(),
                        width,
                        &mut meter,
                        |row, meter| aggregate_cells(row, slots, meter),
                    )
                }
            }
            .map_err(StandingQueryError::Delivery)?;
            Ok((frontier, rows))
        })
    }
}

fn collect_bag<Row: Ord>(
    source: &ZSet<Row>,
    signed: bool,
    width: usize,
    meter: &mut Meter<'_>,
    mut project: impl FnMut(&Row, &mut Meter<'_>) -> Result<Vec<QueryValue>, StandingQueryFailure>,
) -> Result<ZSet<Vec<QueryValue>>, StandingQueryFailure> {
    let mut updates = Vec::new();
    for (row, weight) in source.iter() {
        meter.charge(ZSetEvent::Work)?;
        if weight.is_zero() || (!signed && weight < &ZWeight::ZERO) {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        // One native tuple per support key, regardless of signed magnitude.
        meter.charge(ZSetEvent::ScratchEntry)?;
        let cells = project(row, meter)?;
        if cells.len() != width {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        meter.charge(ZSetEvent::Work)?;
        let weight = weight
            .checked_clone(LIMBS)
            .map_err(|_| StandingQueryFailure::Arithmetic)?;
        meter.charge(ZSetEvent::ScratchEntry)?;
        updates.push((cells, weight));
    }
    // RETURN slots may permute, repeat or omit cells. Consolidate the delivered
    // identity before quota admission: opposite images must cancel even with a
    // zero changed-row allowance. Negative tuples are never thresholded away.
    let rows =
        ZSet::from_updates(updates, LIMBS, &mut |event| meter.charge(event)).map_err(zset_error)?;
    if meter
        .policy
        .rows
        .max_result_rows()
        .is_some_and(|limit| rows.len() as u128 > u128::from(limit))
    {
        return Err(StandingQueryFailure::ResultBudget);
    }
    (meter.checkpoint)()?;
    Ok(rows)
}

#[cfg(test)]
mod tests;
