//! Historical maintained results from a current native bag and its exact suffix.
//! No graph reader, query evaluator, second history store or ACK participates.

use super::*;
use fgdb_delta_types::{LimbLimit, ZWeight};
use fgdb_gql::GqlExecutionBudget;

const LIMBS: LimbLimit = LimbLimit::new(4);
type Bag = ZSet<Vec<QueryValue>>;

impl<V: Vfs + Clone> Database<V> {
    /// Reconstruct the complete native result BAG at one retained replay cut.
    /// The current maintained result and every accepted successor through its
    /// frontier share this immutable database borrow. Copy the current bag via
    /// the ordinary native mapper, then undo whole ticks newest-first. No graph
    /// scan, expression evaluation or endpoint-query fallback is performed.
    ///
    /// `at` must lie in this healthy sink's retained window. Both the source
    /// and sink must be current and healthy, even at the live frontier or for
    /// an empty result. Validate the COMPLETE suffix before copying its current
    /// baseline. Missing/evicted/future cuts return ReplayGap, never a partial
    /// snapshot; malformed links or negative reconstructed counts refuse too.
    /// Registration/rebuild establishes the earliest available baseline.
    ///
    /// Return slots and exact aggregate domains match standing_native_bag.
    /// Multiplicities remain compressed checked integers, including promoted
    /// weights. DISTINCT, HAVING, ordering and pages were already applied by
    /// the maintained source. The result is a bag, not its historical sequence;
    /// pure rank changes have no bag derivative and are not reconstructed.
    ///
    /// One cumulative work/scratch allowance covers suffix admission, baseline
    /// projection, row copies and exact arithmetic. ResultRows bounds ONLY the
    /// final historical support, not current or intermediate support. No budget
    /// is refunded when a row disappears. SnapshotRecords is unused (no graph
    /// reads). State remains resident and these are logical event/payload units,
    /// not an allocator-byte bound or a spill implementation.
    ///
    /// Any refusal drops the tentative result; source state, retained frames
    /// and consumer acknowledgements remain unchanged. The returned bag owns
    /// its payloads and can outlive the writer. This privileged embedded API is
    /// session-local derived history, not arbitrary Chronicle history, a durable
    /// subscription/resume token, cross-branch comparison or live authorization.
    pub fn standing_replay_snapshot(
        &self,
        cx: &QueryCx,
        replay: &StandingQueryHandle,
        at: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<ZSet<Vec<QueryValue>>, StandingQueryError> {
        let StandingQuery::Replay(state) = self.admitted_standing_query(cx, replay)? else {
            return Err(StandingQueryError::Unsupported);
        };
        let source = self.admitted_standing_query(cx, &state.source)?;
        if state.history.frontier() != state.frontier || source.status().1 != state.frontier {
            return Err(StandingQueryError::Delivery(
                StandingQueryFailure::InvalidDelta,
            ));
        }
        let width = self.standing_native_columns(cx, &state.source)?.len();
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
            state.history.snapshot_with(at, width, &mut meter, |meter| {
                self.native_bag_metered(cx, &state.source, None, meter)
            })
        })
    }
}

impl History {
    // The only production baseline callback is the existing native mapper.
    // This private seam lets tests observe coverage-before-copy and shared
    // budget boundaries without inventing a second database implementation.
    fn snapshot_with(
        &self,
        at: CommitSeq,
        width: usize,
        meter: &mut Meter<'_>,
        baseline: impl FnOnce(&mut Meter<'_>) -> Result<(CommitSeq, Bag), StandingQueryError>,
    ) -> Result<Bag, StandingQueryError> {
        let mut end = at;
        let mut ticks = 0_usize;
        loop {
            meter
                .charge(ZSetEvent::Work)
                .map_err(StandingQueryError::Delivery)?;
            let Some(frame) = self.next(end)? else {
                break;
            };
            ticks = ticks
                .checked_add(1)
                .filter(|count| *count <= self.frames.len())
                .ok_or(StandingQueryError::Delivery(
                    StandingQueryFailure::InvalidDelta,
                ))?;
            end = frame.frontier;
        }
        let start = usize::try_from(at.0 - self.retained_after().0).map_err(|_| {
            StandingQueryError::Delivery(StandingQueryFailure::InvalidDelta)
        })?;
        if start.checked_add(ticks) != Some(self.frames.len()) {
            return Err(StandingQueryError::Delivery(
                StandingQueryFailure::InvalidDelta,
            ));
        }
        (meter.checkpoint)().map_err(StandingQueryError::Delivery)?;

        // The current bag is private input, not delivered historical output.
        // Remove ONLY its result-row ceiling; preserve and spend the existing
        // source/work/scratch allowance and restore policy even on refusal.
        let original = meter.policy.rows;
        meter.policy.rows = GqlExecutionBudget::snapshot_records(
            original.max_snapshot_records().unwrap_or(u64::MAX),
        );
        let baseline = baseline(meter);
        meter.policy.rows = original;
        let (frontier, mut rows) = baseline?;
        if frontier != end || frontier != self.frontier() {
            return Err(StandingQueryError::Delivery(
                StandingQueryFailure::InvalidDelta,
            ));
        }
        for (row, weight) in rows.iter() {
            meter
                .charge(ZSetEvent::Work)
                .map_err(StandingQueryError::Delivery)?;
            if row.len() != width || weight <= &ZWeight::ZERO {
                return Err(StandingQueryError::Delivery(
                    StandingQueryFailure::InvalidDelta,
                ));
            }
        }
        // Undo in reverse COMMIT order, but never reverse signs by narrowing.
        // Each intermediate bag is an actual earlier accepted source state.
        // This avoids a coalesced inverse whose temporary sums could exceed
        // the checked arithmetic domain even when every real snapshot fits.
        for frame in self.frames.iter().rev().take(ticks) {
            meter
                .charge(ZSetEvent::Work)
                .map_err(StandingQueryError::Delivery)?;
            for (row, weight) in frame.rows.iter() {
                rewind_row(&mut rows, row, weight, width, meter)
                    .map_err(StandingQueryError::Delivery)?;
            }
        }
        if original
            .max_result_rows()
            .is_some_and(|limit| rows.len() as u128 > u128::from(limit))
        {
            return Err(StandingQueryError::Delivery(
                StandingQueryFailure::ResultBudget,
            ));
        }
        (meter.checkpoint)().map_err(StandingQueryError::Delivery)?;
        Ok(rows)
    }
}

fn rewind_row(
    rows: &mut Bag,
    row: &Vec<QueryValue>,
    weight: &ZWeight,
    width: usize,
    meter: &mut Meter<'_>,
) -> Result<(), StandingQueryFailure> {
    meter.charge(ZSetEvent::Work)?;
    if row.len() != width || weight.is_zero() {
        return Err(StandingQueryFailure::InvalidDelta);
    }
    let limbs = weight
        .magnitude_limb_count()
        .checked_add(1)
        .ok_or(StandingQueryFailure::ScratchBudget)?;
    meter.units(ZSetEvent::ScratchEntry, limbs)?;
    let inverse = weight
        .checked_neg(LIMBS)
        .map_err(|_| StandingQueryFailure::Arithmetic)?;
    let zero = ZWeight::ZERO;
    let current = rows.weight(row).unwrap_or(&zero);
    meter.charge(ZSetEvent::Work)?;
    let limbs = current
        .magnitude_limb_count()
        .max(inverse.magnitude_limb_count())
        .checked_add(1)
        .ok_or(StandingQueryFailure::ScratchBudget)?;
    meter.units(ZSetEvent::ScratchEntry, limbs)?;
    let previous = current
        .checked_add(&inverse, LIMBS)
        .map_err(|_| StandingQueryFailure::Arithmetic)?;
    if previous < ZWeight::ZERO {
        return Err(StandingQueryFailure::InvalidDelta);
    }
    // The kernel owns insertion/removal and exact addition. Preflight above
    // proves a valid bag count; its work is additional, never silently free.
    drop(previous);
    let cells = copy_row(row, meter)?;
    rows.accumulate(cells, inverse, LIMBS, &mut |event| meter.charge(event))
        .map_err(zset_error)
}

fn copy_row(
    row: &[QueryValue],
    meter: &mut Meter<'_>,
) -> Result<Vec<QueryValue>, StandingQueryFailure> {
    meter.charge(ZSetEvent::ScratchEntry)?;
    let mut cells = Vec::new();
    for value in row {
        meter.charge(ZSetEvent::Work)?;
        let units = match value {
            QueryValue::Value(value) => value.payload_units(),
            _ => 0,
        }
        .checked_add(1)
        .ok_or(StandingQueryFailure::ScratchBudget)?;
        meter.units(ZSetEvent::ScratchEntry, units)?;
        cells
            .try_reserve(1)
            .map_err(|_| StandingQueryFailure::ScratchBudget)?;
        cells.push(value.clone());
    }
    Ok(cells)
}

#[cfg(test)]
mod tests;
