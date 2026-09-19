//! Database-owned recursive topology maintenance, composed from the canonical
//! committed-input, reachability and Z-set prepared guards. No graph store,
//! delta decoder, recursion algorithm or publication authority lives here.

use super::*;
use fgdb_delta_types::zset::committed::EdgeInputError;
use fgdb_delta_types::zset::committed::snapshot::{EdgeSnapshotBuilder, SnapshotInputError};
use fgdb_delta_types::zset::reachability::ReachabilityError;
use fgdb_delta_types::zset::reachability::committed::{
    CommittedReachability, CommittedReachabilityError, CommittedReachabilityUpdate,
};
use fgdb_delta_types::{LimbLimit, SchemaEpoch};
use crate::gql_exec::source::{self, SourceEvent};

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

fn snapshot_error(error: SnapshotInputError<StandingQueryFailure>) -> StandingQueryFailure {
    match error {
        SnapshotInputError::Input(EdgeInputError::Delta(error)) => zset_error(error),
        _ => StandingQueryFailure::InvalidDelta,
    }
}

/// Account the borrowed logical rows before input preparation. Payloads that
/// are irrelevant to pure topology are neither cloned nor charged as retained
/// data. Schema and topology consistency checks remain in CommittedEdgeInput.
fn observe_batch(
    batch: &LogicalDeltaBatch,
    meter: &mut Meter<'_>,
) -> Result<(), StandingQueryFailure> {
    for coordinate in batch.coordinate_entries() {
        meter.charge(ZSetEvent::Work)?;
        for _ in &coordinate.rows {
            meter.charge(ZSetEvent::Work)?;
            meter.stats.delta_rows = meter.stats.delta_rows.checked_add(1)
                .ok_or(StandingQueryFailure::WorkBudget)?;
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
    result_bound(count, meter.policy)?;
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
        observe_batch(batch, meter)?;
        let pending = self.input.prepare_committed_successor(cx, batch, LIMBS,
            &mut |event| meter.charge(event)).map_err(input_error)?;
        finish(&mut self.rows, pending, meter)
    }
}

impl State {
    /// Build privately from ONE already admitted immutable database generation.
    /// The borrowed GQL source owns statement visibility and tombstone precedence;
    /// this adapter never interprets raw blocks or clones graph properties.
    fn from_snapshot(
        snapshot: &crate::Snapshot,
        relation: RelationId,
        meter: &mut Meter<'_>,
    ) -> Result<Self, StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        if snapshot.frontier != snapshot.delta_index.frontier() {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        // Bound the PHYSICAL topology records before the source can allocate its
        // winner map. History retained in Strata still costs work and scratch;
        // unrelated vertex/property delta history is not replayed or scanned.
        let mut records = 0_u64;
        for block in &snapshot.blocks {
            meter.charge(ZSetEvent::Work)?;
            records = records.checked_add(u64::try_from(block.len())
                .map_err(|_| StandingQueryFailure::SnapshotBudget)?)
                .ok_or(StandingQueryFailure::SnapshotBudget)?;
            if meter.policy.rows.max_snapshot_records().is_some_and(|limit| records > limit) {
                return Err(StandingQueryFailure::SnapshotBudget);
            }
        }
        // The shared builder streams directly from the borrowed source. It
        // poisons on any swallowed insertion refusal; no temporary full edge
        // vector, reconstructed log or fabricated insertion commit is needed.
        meter.charge(ZSetEvent::ScratchEntry)?;
        let mut builder = EdgeSnapshotBuilder::from_index(
            crate::GRAPH, crate::BRANCH, &snapshot.delta_index,
            &mut |event| meter.charge(event),
        ).map_err(snapshot_error)?;
        // The embedded write template fixes coordinates to SchemaEpoch(0).
        // Record the selected relation even when empty. A schema-capable spine
        // must supply authenticated current catalog epochs instead; the shared
        // builder already supports them, including known empty relations.
        builder.record_epoch(relation, SchemaEpoch(0), &mut |event| meter.charge(event))
            .map_err(snapshot_error)?;
        source::visit_edges(&snapshot.blocks, snapshot.frontier,
            &mut |event| match event {
                SourceEvent::Work | SourceEvent::SnapshotRecord => meter.charge(ZSetEvent::Work),
                SourceEvent::ScratchEntry => meter.charge(ZSetEvent::ScratchEntry),
            },
            |entry, control| builder.insert(
                entry.eid, (entry.relation, entry.src, entry.dst), SchemaEpoch(0), LIMBS,
                &mut |event| control(match event {
                    ZSetEvent::Work => SourceEvent::Work,
                    ZSetEvent::ScratchEntry => SourceEvent::ScratchEntry,
                }),
            ).map_err(snapshot_error),
        )?;
        let baseline = builder.finish(&mut |event| meter.charge(event)).map_err(snapshot_error)?;
        let input = CommittedReachability::from_snapshot(
            baseline, relation, LIMBS, &mut |event| meter.charge(event),
        ).map_err(input_error)?;
        let rows = input.snapshot(LIMBS, &mut |event| meter.charge(event)).map_err(input_error)?;
        result_bound(rows.len() as u128, meter.policy)?;
        (meter.checkpoint)()?;
        Ok(Self {
            input, rows, policy: meter.policy, frontier: snapshot.frontier,
            stats: meter.stats, failure: None,
        })
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
            let mut checkpoint = || cx.checkpoint().map_err(|_| StandingQueryFailure::Interrupted);
            let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            // Under &self the topology and the delta anchor are one immutable
            // generation. No new commit authority, historical log copy or cursor
            // injection is needed. A failed build never changes a registered view.
            State::from_snapshot(&self.snapshot, relation, &mut meter)
                .map_err(StandingQueryError::Maintenance)
        })
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "recursive/snapshot_tests.rs"]
mod snapshot_tests;
