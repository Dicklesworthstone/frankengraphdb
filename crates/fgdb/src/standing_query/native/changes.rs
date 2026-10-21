//! Compressed native baseline and one-tick delivery from published maintained
//! bags. No graph source, expression evaluator or result re-execution occurs.

mod statement;
mod subscription;
pub use statement::{NativeSubscriptionSetup, SubscribeError};
pub use subscription::{
    NativeSubscription, SubscriptionBatch, SubscriptionError, SubscriptionReceipt,
};

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
    /// Projection consolidates immediately: beyond the maintained source,
    /// retain current nonzero native support plus one projected tuple, not a
    /// second vector of all source keys. Cumulative scratch is not refunded
    /// when a key cancels. This does not make the maintained state spillable.
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
        let mut checkpoint = || {
            cx.checkpoint()
                .map_err(|_| StandingQueryFailure::Interrupted)
        };
        let mut meter = Meter {
            policy,
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        self.native_bag_metered(cx, handle, from, &mut meter)
    }

    // Replay snapshot reconstruction must copy its current baseline through
    // this SAME mapper and spend the SAME allowance as subsequent rewinding.
    // This seam grants no source/handle exception and resets no counters.
    pub(in crate::standing_query) fn native_bag_metered(
        &self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
        from: Option<CommitSeq>,
        meter: &mut Meter<'_>,
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
            let width = layout.columns().len();
            let rows = match layout {
                Layout::Rows { .. } | Layout::Circuit { .. } => {
                    let rows = match from {
                        Some(from) => sets::delta(query).ok_or_else(|| unavailable(from))?,
                        None => sets::rows(query).ok_or(StandingQueryError::Unsupported)?,
                    };
                    collect_bag(rows, from.is_some(), width, meter, row_cells)
                }
                Layout::Aggregate { slots, .. } | Layout::GroupCircuit { slots, .. } => {
                    let view = match from {
                        Some(from) => self
                            .standing_query_delta(cx, handle)?
                            .ok_or_else(|| unavailable(from))?,
                        None => self.standing_query(cx, handle)?,
                    };
                    collect_bag(view.rows(), from.is_some(), width, meter, |row, meter| {
                        aggregate_cells(row, slots, meter)
                    })
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
    // RETURN slots may merge many private keys into one public tuple. Keep
    // only the current native support, in the same source-key addition order
    // as from_updates, without retaining every projected copy until the end.
    let mut rows = ZSet::new();
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
        rows.accumulate(cells, weight, LIMBS, &mut |event| meter.charge(event))
            .map_err(zset_error)?;
    }
    // RETURN slots may permute, repeat or omit cells. Consolidate the delivered
    // identity before quota admission: opposite images must cancel even with a
    // zero changed-row allowance. Negative tuples are never thresholded away.
    // The relation is private until all projections, exact additions, final
    // quota admission and the completion checkpoint succeed. A refusal drops
    // it without changing the source, subscription receipt or replay history.
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

#[cfg(test)]
mod online_tests {
    use super::*;

    type Bag = ZSet<Vec<QueryValue>>;

    fn source(count: u32, weight: impl Fn(u32) -> ZWeight) -> ZSet<u32> {
        ZSet::from_updates((0..count).map(|key| (key, weight(key))), LIMBS, &mut |_| {
            Ok::<_, ()>(())
        })
        .unwrap()
    }

    fn wide(rows: u64) -> GqlQueryPolicy {
        GqlQueryPolicy::new(0, rows, u64::MAX, u64::MAX)
    }

    fn run(
        source: &ZSet<u32>,
        signed: bool,
        policy: GqlQueryPolicy,
        mut checkpoint: impl FnMut() -> Result<(), StandingQueryFailure>,
        project: impl FnMut(&u32, &mut Meter<'_>) -> Result<Vec<QueryValue>, StandingQueryFailure>,
    ) -> (Result<Bag, StandingQueryFailure>, StandingQueryStats) {
        let mut meter = Meter {
            policy,
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        let result = collect_bag(source, signed, 1, &mut meter, project);
        (result, meter.stats)
    }

    fn one(_: &u32, _: &mut Meter<'_>) -> Result<Vec<QueryValue>, StandingQueryFailure> {
        // Deliberately unmetered fixture projection to isolate the collector's
        // own staging cost. Real row/group projection controls remain unchanged.
        Ok(vec![QueryValue::Integer(7)])
    }

    #[test]
    fn exhausted_consolidation_work_stops_before_the_next_projection() {
        let source = source(4, |_| ZWeight::ONE);
        let mut projected = Vec::new();
        let (result, stats) = run(
            &source,
            false,
            GqlQueryPolicy::new(0, 1, 6, u64::MAX),
            || Ok(()),
            |key, meter| {
                projected.push(*key);
                one(key, meter)
            },
        );
        assert_eq!(result, Err(StandingQueryFailure::WorkBudget));
        assert_eq!(stats.work_units, 7);
        assert_eq!(projected, [0, 1]);
        // The deferred implementation projected key 2 before any consolidation
        // consumed its work, despite not being able to finish key 1.
    }

    #[test]
    fn collapsing_private_keys_spend_no_occurrence_vector_reservations() {
        let n = 16_384_u32;
        let source = source(n, |_| ZWeight::ONE);
        // Work is 3 for the first key and 4 for each colliding key. Scratch
        // reserves each projected tuple and exactly one retained output key.
        let work = 4 * u64::from(n) - 1;
        let scratch = u64::from(n) + 1;
        let exact = GqlQueryPolicy::new(0, 1, work, scratch);
        let (result, stats) = run(&source, false, exact, || Ok(()), one);
        let result = result.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result.iter().next().unwrap().1.to_i128(),
            Some(i128::from(n))
        );
        assert_eq!(stats.work_units, work);
        assert_eq!(stats.scratch_entries, scratch);
        for (policy, error) in [
            (
                GqlQueryPolicy::new(0, 1, work - 1, scratch),
                StandingQueryFailure::WorkBudget,
            ),
            (
                GqlQueryPolicy::new(0, 1, work, scratch - 1),
                StandingQueryFailure::ScratchBudget,
            ),
            (
                GqlQueryPolicy::new(0, 0, work, scratch),
                StandingQueryFailure::ResultBudget,
            ),
        ] {
            assert_eq!(run(&source, false, policy, || Ok(()), one).0, Err(error));
        }
    }

    #[test]
    fn cancellation_and_reappearance_keep_cumulative_scratch_and_final_only_row_quota() {
        let counts = [3, -3, 1, -1];
        let source = source(4, |key| ZWeight::from_i128(counts[key as usize]));
        let (result, stats) = run(&source, true, wide(0), || Ok(()), one);
        assert!(result.unwrap().is_empty());
        assert_eq!(stats.work_units, 14);
        assert_eq!(stats.scratch_entries, 6); // four tuple copies, two new keys
        assert_eq!(
            run(
                &source,
                true,
                GqlQueryPolicy::new(0, 0, 14, 5),
                || Ok(()),
                one
            )
            .0,
            Err(StandingQueryFailure::ScratchBudget)
        );
    }

    #[test]
    fn exact_promoted_weights_survive_collisions_and_full_cancellation() {
        let positive = source(2, |_| ZWeight::from_i128(i128::MAX));
        let result = run(&positive, false, wide(1), || Ok(()), one).0.unwrap();
        let expected = ZWeight::from_i128(i128::MAX)
            .checked_add(&ZWeight::from_i128(i128::MAX), LIMBS)
            .unwrap();
        assert!(expected.to_i128().is_none());
        assert_eq!(result.iter().next().unwrap().1, &expected);
        let signed = source(4, |key| {
            ZWeight::from_i128(if key < 2 { i128::MAX } else { -i128::MAX })
        });
        assert!(
            run(&signed, true, wide(0), || Ok(()), one)
                .0
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn late_shape_and_negative_baseline_refusals_never_release_a_partial_bag() {
        let source = source(4, |_| ZWeight::ONE);
        let (result, _) = run(
            &source,
            false,
            wide(1),
            || Ok(()),
            |key, meter| {
                if *key == 3 {
                    Ok(Vec::new())
                } else {
                    one(key, meter)
                }
            },
        );
        assert_eq!(result, Err(StandingQueryFailure::InvalidDelta));
        assert_eq!(
            run(&source, false, wide(1), || Ok(()), one)
                .0
                .unwrap()
                .len(),
            1
        );

        let negative = self::source(2, |key| ZWeight::from_i128(if key == 0 { 1 } else { -1 }));
        let mut projected = Vec::new();
        let (result, _) = run(
            &negative,
            false,
            wide(0),
            || Ok(()),
            |key, meter| {
                projected.push(*key);
                one(key, meter)
            },
        );
        assert_eq!(result, Err(StandingQueryFailure::InvalidDelta));
        assert_eq!(projected, [0]); // invalid input is not laundered by cancellation
    }

    #[test]
    fn every_collision_checkpoint_and_unwind_preserves_source_and_retry() {
        let counts = [2, -1, -1, 3];
        let source = source(4, |key| ZWeight::from_i128(counts[key as usize]));
        let original = source
            .checked_clone(LIMBS, &mut |_| Ok::<_, ()>(()))
            .unwrap();
        let mut calls = 0;
        let expected = run(
            &source,
            true,
            wide(1),
            || {
                calls += 1;
                Ok(())
            },
            one,
        )
        .0;
        assert_eq!(
            expected
                .as_ref()
                .unwrap()
                .iter()
                .next()
                .unwrap()
                .1
                .to_i128(),
            Some(3)
        );
        for stop in 1..=calls {
            let mut seen = 0;
            let result = run(
                &source,
                true,
                wide(1),
                || {
                    seen += 1;
                    if seen == stop {
                        Err(StandingQueryFailure::Interrupted)
                    } else {
                        Ok(())
                    }
                },
                one,
            )
            .0;
            assert_eq!(result, Err(StandingQueryFailure::Interrupted));
            assert_eq!(seen, stop);
            let mut seen = 0;
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _ = run(
                        &source,
                        true,
                        wide(1),
                        || {
                            seen += 1;
                            assert_ne!(seen, stop, "injected collector unwind");
                            Ok(())
                        },
                        one,
                    );
                }))
                .is_err()
            );
            assert_eq!(seen, stop);
            assert_eq!(source, original);
            assert_eq!(run(&source, true, wide(1), || Ok(()), one).0, expected);
        }
    }
}
