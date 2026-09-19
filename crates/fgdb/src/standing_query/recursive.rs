//! Database-owned recursive topology maintenance, composed from the canonical
//! committed-input, reachability and Z-set prepared guards. No graph store,
//! delta decoder, recursion algorithm or publication authority lives here.

use super::*;
use fgdb_delta_types::zset::committed::EdgeInputError;
use fgdb_delta_types::zset::reachability::ReachabilityError;
use fgdb_delta_types::zset::reachability::committed::{
    CommittedReachability, CommittedReachabilityError, CommittedReachabilityUpdate,
};
use fgdb_delta_types::LimbLimit;

const LIMBS: LimbLimit = LimbLimit::new(4);
type Pair = (VId, VId);

pub(crate) struct State {
    input: CommittedReachability,
    pub(super) rows: ZSet<Pair>,
    pub(super) policy: GqlQueryPolicy,
    pub(super) frontier: CommitSeq,
    pub(super) stats: StandingQueryStats,
    pub(super) failure: Option<StandingQueryFailure>,
}

fn input_error(error: CommittedReachabilityError<StandingQueryFailure>) -> StandingQueryFailure {
    match error {
        CommittedReachabilityError::Input(EdgeInputError::Delta(error))
        | CommittedReachabilityError::Reachability(ReachabilityError::Delta(error)) => zset_error(error),
        _ => StandingQueryFailure::InvalidDelta,
    }
}

/// Account the borrowed logical rows before input preparation. Payloads that
/// are irrelevant to pure topology are neither cloned nor charged as retained
/// data. Schema and topology consistency checks remain in CommittedEdgeInput.
fn observe_batch(
    batch: &LogicalDeltaBatch,
    meter: &mut Meter<'_>,
    snapshot_limit: Option<u64>,
) -> Result<(), StandingQueryFailure> {
    for coordinate in batch.coordinate_entries() {
        meter.charge(ZSetEvent::Work)?;
        for _ in &coordinate.rows {
            meter.charge(ZSetEvent::Work)?;
            meter.stats.delta_rows = meter.stats.delta_rows.checked_add(1)
                .ok_or(StandingQueryFailure::WorkBudget)?;
            if snapshot_limit.is_some_and(|limit| meter.stats.delta_rows > limit) {
                return Err(StandingQueryFailure::SnapshotBudget);
            }
        }
    }
    Ok(())
}

fn result_bound(rows: u128, policy: GqlQueryPolicy) -> Result<(), StandingQueryFailure> {
    if policy.rows.max_result_rows().is_some_and(|limit| rows > u128::from(limit)) {
        return Err(StandingQueryFailure::ResultBudget);
    }
    Ok(())
}

/// No full sink scan: the canonical closure derivative consists only of +/-1
/// membership changes. Check every changed key and the final cardinality before
/// preparing output. Additions may sort before removals; a valid equal-size
/// replacement must not be rejected for a transiently larger prefix.
fn finish(
    rows: &mut ZSet<Pair>,
    pending: CommittedReachabilityUpdate<'_>,
    meter: &mut Meter<'_>,
    bound_result: bool,
) -> Result<(), StandingQueryFailure> {
    let mut count = rows.len() as u128;
    for (pair, weight) in pending.delta().iter() {
        meter.charge(ZSetEvent::Work)?;
        match weight.to_i128() {
            Some(1) if rows.weight(pair).is_none() => {
                count = count.checked_add(1).ok_or(StandingQueryFailure::Arithmetic)?;
            }
            Some(-1) if rows.weight(pair).is_some_and(|old| old.to_i128() == Some(1)) => {
                count = count.checked_sub(1).ok_or(StandingQueryFailure::InvalidDelta)?;
            }
            _ => return Err(StandingQueryFailure::InvalidDelta),
        }
    }
    if bound_result { result_bound(count, meter.policy)?; }
    let sink = rows.prepare_update(pending.delta(), LIMBS, &mut |event| meter.charge(event))
        .map_err(zset_error)?;
    (meter.checkpoint)()?;
    // All arithmetic, checks and cancellation are complete. These are the
    // existing infallible prepared publications, with their stated collection
    // allocation/panic boundary; this is not allocator-level failure atomicity.
    let _ = pending.commit();
    sink.commit();
    Ok(())
}

impl State {
    pub(super) fn relation(&self) -> RelationId { self.input.relation() }

    pub(super) fn maintain(
        &mut self,
        cx: &CommitCx,
        batch: &LogicalDeltaBatch,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        if self.frontier != self.input.frontier() {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        observe_batch(batch, meter, None)?;
        let pending = self.input.prepare_committed_successor(cx, batch, LIMBS,
            &mut |event| meter.charge(event)).map_err(input_error)?;
        finish(&mut self.rows, pending, meter, true)
    }
}

impl<V: Vfs + Clone> Database<V> {
    pub(super) fn prepare_standing_reachability(
        &self,
        cx: &QueryCx,
        relation: RelationId,
        policy: GqlQueryPolicy,
    ) -> Result<State, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        cx.with_restriction(|| {
            let index = self.delta_index().map_err(StandingQueryError::Read)?;
            let batches = index.since(CommitSeq::ORIGIN)
                .map_err(|_| StandingQueryError::Maintenance(StandingQueryFailure::InvalidDelta))?;
            let mut checkpoint = || cx.checkpoint().map_err(|_| StandingQueryFailure::Interrupted);
            let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            // Registry backing, input and sink are still private while replaying.
            meter.charge(ZSetEvent::ScratchEntry).map_err(StandingQueryError::Maintenance)?;
            let mut state = State {
                input: CommittedReachability::new(crate::GRAPH, crate::BRANCH, relation),
                rows: ZSet::new(), policy, frontier: CommitSeq::ORIGIN,
                stats: StandingQueryStats::default(), failure: None,
            };
            for batch in batches {
                // Bound historical input cumulatively BEFORE the next batch can
                // allocate input/closure state. The core still checks its exact
                // anchor, gap-free successor and envelope for every indexed tick.
                observe_batch(batch, &mut meter, policy.rows.max_snapshot_records())
                    .map_err(StandingQueryError::Maintenance)?;
                let pending = state.input.prepare_next(index, LIMBS, &mut |event| meter.charge(event))
                    .map_err(input_error).map_err(StandingQueryError::Maintenance)?
                    .ok_or(StandingQueryError::Maintenance(StandingQueryFailure::InvalidDelta))?;
                if pending.commit_seq() != batch.commit_seq() {
                    return Err(StandingQueryError::Maintenance(StandingQueryFailure::InvalidDelta));
                }
                // Intermediate historical closures are not public results. A
                // deleted past cycle cannot prevent rebuilding a small current
                // view under its valid result limit. Work/scratch remain bounded.
                finish(&mut state.rows, pending, &mut meter, false)
                    .map_err(StandingQueryError::Maintenance)?;
            }
            let caught_up = state.input.prepare_next(index, LIMBS, &mut |event| meter.charge(event))
                .map_err(input_error).map_err(StandingQueryError::Maintenance)?.is_none();
            if !caught_up || state.input.frontier() != self.snapshot.frontier {
                return Err(StandingQueryError::Maintenance(StandingQueryFailure::InvalidDelta));
            }
            result_bound(state.rows.len() as u128, policy).map_err(StandingQueryError::Maintenance)?;
            (meter.checkpoint)().map_err(StandingQueryError::Maintenance)?;
            state.frontier = state.input.frontier();
            state.stats = meter.stats;
            Ok(state)
        })
    }
}

#[cfg(test)]
mod tests;
