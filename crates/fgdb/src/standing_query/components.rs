//! Exact database-owned weak components, including every live isolated vertex.
//! CommittedEdgeInput validates EId lifetimes, cascades and source succession;
//! the shared component kernel owns connectivity. No second log or graph store.

use super::*;
use crate::gql_exec::source::{self, SourceEvent};
use fgdb_delta_types::{DeltaRow, LimbLimit, ZWeight};
use fgdb_delta_types::zset::committed::{CommittedEdgeInput, EdgeInputError, EdgeTuple};
use fgdb_delta_types::zset::components::{ComponentError, IncrementalComponents};

const LIMBS: LimbLimit = LimbLimit::new(4);
type Pair = (VId, VId);

pub(crate) struct State {
    input: CommittedEdgeInput,
    components: IncrementalComponents<VId>,
    rows: ZSet<Pair>,
    pub(super) relation: RelationId,
    pub(super) policy: GqlQueryPolicy,
    pub(super) frontier: CommitSeq,
    pub(super) stats: StandingQueryStats,
    pub(super) failure: Option<StandingQueryFailure>,
}
fn input_error(error: EdgeInputError<StandingQueryFailure>) -> StandingQueryFailure {
    match error { EdgeInputError::Delta(error) => zset_error(error),
        _ => StandingQueryFailure::InvalidDelta }
}
fn component_error(error: ComponentError<StandingQueryFailure>) -> StandingQueryFailure {
    match error { ComponentError::Delta(error) => zset_error(error),
        ComponentError::CardinalityOverflow => StandingQueryFailure::Arithmetic,
        _ => StandingQueryFailure::InvalidDelta }
}
fn result_bound(count: usize, policy: GqlQueryPolicy) -> Result<(), StandingQueryFailure> {
    if policy.rows.max_result_rows().is_some_and(|limit| count as u128 > u128::from(limit)) {
        return Err(StandingQueryFailure::ResultBudget);
    }
    Ok(())
}
fn project(input: &ZSet<EdgeTuple>, relation: RelationId, meter: &mut Meter<'_>)
    -> Result<ZSet<Pair>, StandingQueryFailure> {
    input.filter(|(r, _, _)| Ok(*r == relation), LIMBS, &mut |event| meter.charge(event))
        .map_err(zset_error)?
        .map(|(_, a, b)| Ok((*a, *b)), LIMBS, &mut |event| meter.charge(event))
        .map_err(zset_error)
}
fn vertex_delta(batch: &LogicalDeltaBatch, meter: &mut Meter<'_>)
    -> Result<ZSet<VId>, StandingQueryFailure> {
    let mut updates = Vec::new();
    for coordinate in batch.coordinate_entries() {
        meter.charge(ZSetEvent::Work)?;
        if coordinate.graph != crate::GRAPH || coordinate.branch != crate::BRANCH { continue; }
        // Vertex lifetime is graph-wide, not scoped to the selected edge type.
        for row in &coordinate.rows {
            meter.charge(ZSetEvent::Work)?;
            let update = match row {
                DeltaRow::CreateVertex { vid, .. } => Some((*vid, ZWeight::ONE)),
                DeltaRow::DeleteVertex { vid, .. } => Some((*vid, ZWeight::from_i128(-1))),
                _ => None,
            };
            if let Some(update) = update {
                meter.charge(ZSetEvent::ScratchEntry)?;
                updates.push(update);
            }
        }
    }
    ZSet::from_updates(updates, LIMBS, &mut |event| meter.charge(event)).map_err(zset_error)
}

impl State {
    pub(super) fn maintain(&mut self, cx: &CommitCx, batch: &LogicalDeltaBatch,
        meter: &mut Meter<'_>) -> Result<(), StandingQueryFailure> {
        if self.frontier != self.input.frontier() { return Err(StandingQueryFailure::InvalidDelta); }
        recursive::observe_batch(batch, meter)?;
        // Validate the whole batch BEFORE filtering its topology or vertices.
        let input = self.input.prepare_committed_successor(cx, batch, LIMBS,
            &mut |event| meter.charge(event)).map_err(input_error)?;
        let vertices = vertex_delta(batch, meter)?;
        let edges = project(input.delta(), self.relation, meter)?;
        let pending = self.components.prepare(&vertices, &edges, LIMBS,
            &mut |event| meter.charge(event)).map_err(component_error)?;
        meter.stats.affected_vertices = u64::try_from(pending.affected_vertices())
            .map_err(|_| StandingQueryFailure::Arithmetic)?;
        // Final membership size, not the transient retraction/insertion prefix.
        result_bound(pending.vertex_count(), meter.policy)?;
        let sink = self.rows.prepare_update(pending.delta(), LIMBS,
            &mut |event| meter.charge(event)).map_err(zset_error)?;
        for (pair, _) in pending.delta().iter() {
            meter.charge(ZSetEvent::Work)?;
            if sink.weight(pair).is_some_and(|w| w != &ZWeight::ONE) {
                return Err(StandingQueryFailure::InvalidDelta);
            }
        }
        (meter.checkpoint)()?;
        // All recoverable work is done. No callbacks separate these publications.
        let _ = pending.commit();
        sink.commit();
        let _ = input.commit();
        Ok(())
    }

    fn from_snapshot(snapshot: &crate::Snapshot, relation: RelationId, meter: &mut Meter<'_>)
        -> Result<Self, StandingQueryFailure> {
        // Admit physical vertex AND edge history before either borrowed scan.
        // Compacted versions/tombstones cost source admission too. The shared
        // topology bootstrap's own edge admission remains unchanged.
        let mut records = 0_u128;
        for count in snapshot.blocks.iter().map(|block| block.len())
            .chain(snapshot.patches.iter().map(|patch| patch.len())) {
            meter.charge(ZSetEvent::Work)?;
            records = records.checked_add(count as u128).ok_or(StandingQueryFailure::SnapshotBudget)?;
            if meter.policy.rows.max_snapshot_records().is_some_and(|limit| records > u128::from(limit)) {
                return Err(StandingQueryFailure::SnapshotBudget);
            }
        }
        let baseline = recursive::topology_snapshot(snapshot, relation, meter)?;
        let edges = project(baseline.rows(), relation, meter)?;
        let mut vertices = Vec::new();
        source::visit_vertices(&snapshot.patches, snapshot.frontier, &mut |event| {
            meter.charge(match event { SourceEvent::ScratchEntry => ZSetEvent::ScratchEntry,
                SourceEvent::Work | SourceEvent::SnapshotRecord => ZSetEvent::Work })
        }, |row, control| {
            control(SourceEvent::Work)?;
            control(SourceEvent::ScratchEntry)?;
            vertices.push((row.vid, ZWeight::ONE));
            Ok(())
        })?;
        let vertices = ZSet::from_updates(vertices, LIMBS, &mut |event| meter.charge(event)).map_err(zset_error)?;
        let mut components = IncrementalComponents::new();
        let pending = components.prepare(&vertices, &edges, LIMBS,
            &mut |event| meter.charge(event)).map_err(component_error)?;
        result_bound(pending.vertex_count(), meter.policy)?;
        meter.stats.affected_vertices = u64::try_from(pending.affected_vertices())
            .map_err(|_| StandingQueryFailure::Arithmetic)?;
        (meter.checkpoint)()?;
        // From empty state the exact derivative IS the initial membership set.
        let rows = pending.commit();
        Ok(Self { input: baseline.into_input(), components, rows, relation,
            policy: meter.policy, frontier: snapshot.frontier, stats: meter.stats, failure: None })
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Register exact weak components using one relation's undirected support.
    /// Every live graph vertex participates, including vertices without any
    /// selected edge. Parallel/opposite edges preserve connectivity until the
    /// last supporting edge is retired; self-loops do not merge components.
    /// Each row is (vertex, minimum vertex in its component), with weight one.
    ///
    /// Committed writes maintain the result or fence it unavailable. Failures
    /// never undo a durable write or stop healthy sibling views. Explicit
    /// rebuild_standing_query repairs from current authenticated state, not by
    /// replaying historical components. Property-only ticks do not traverse
    /// connectivity. Changes can rederive a whole affected component, but never
    /// scan an unrelated one. This is not a union-find complexity guarantee.
    ///
    /// Result limits count vertices. Initialization admits combined physical
    /// vertex/edge records; work/scratch govern source, kernel and sink together.
    /// All state remains session-local and in-memory: no durable registration,
    /// spill, byte-memory bound, strong components, label/property filtering or
    /// valid-time filtering is implied by this topology-specific API.
    pub fn register_standing_components(&mut self, cx: &QueryCx, relation: RelationId,
        policy: GqlQueryPolicy) -> Result<StandingQueryHandle, StandingQueryError> {
        let state = self.prepare_standing_components(cx, relation, policy)?;
        Ok(self.store_standing_query(StandingQuery::Components(Box::new(state))))
    }

    pub(super) fn prepare_standing_components(&self, cx: &QueryCx, relation: RelationId,
        policy: GqlQueryPolicy) -> Result<State, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        cx.with_restriction(|| {
            let mut checkpoint = || cx.checkpoint().map_err(|_| StandingQueryFailure::Interrupted);
            let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            State::from_snapshot(&self.snapshot, relation, &mut meter).map_err(StandingQueryError::Maintenance)
        })
    }

    /// Borrow current membership. Rows are in canonical (vertex, representative)
    /// order; ordered_rows() is None because no separate rank stage is needed.
    pub fn standing_components<'a>(&'a self, cx: &QueryCx, handle: &StandingQueryHandle)
        -> Result<StandingQueryView<'a, (VId, VId)>, StandingQueryError> {
        let StandingQuery::Components(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(StandingQueryView { rows: &query.rows, ordered: None,
            frontier: query.frontier, stats: &query.stats })
    }

    /// Current component count without scanning membership rows.
    pub fn standing_component_count(&self, cx: &QueryCx, handle: &StandingQueryHandle)
        -> Result<usize, StandingQueryError> {
        let StandingQuery::Components(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.components.component_count())
    }

    /// Current representative, or None for a non-live vertex. A missing or
    /// unavailable view is a typed refusal, never confused with missing data.
    pub fn standing_component(&self, cx: &QueryCx, handle: &StandingQueryHandle, vertex: VId)
        -> Result<Option<VId>, StandingQueryError> {
        let StandingQuery::Components(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.components.representative(&vertex).copied())
    }
}

#[cfg(test)]
mod tests;
