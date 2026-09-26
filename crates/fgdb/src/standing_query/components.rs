//! Exact database-owned weak/strong components, including isolated vertices.
//! CommittedEdgeInput validates EId lifetimes, cascades and source succession;
//! the shared component kernel owns connectivity. No second log or graph store.

use super::*;
use crate::gql_exec::source::{self, SourceEvent};
use fgdb_delta_types::zset::committed::{CommittedEdgeInput, EdgeInputError, EdgeTuple};
use fgdb_delta_types::zset::components::{ComponentError, ComponentUpdate, IncrementalComponents};
use fgdb_delta_types::zset::components::strong::{
    IncrementalStrongComponents, StrongComponentUpdate,
};
use fgdb_delta_types::{DeltaRow, LimbLimit, ZWeight};

const LIMBS: LimbLimit = LimbLimit::new(4);
type Pair = (VId, VId);

/// The complete component relation definition crosses the registry's rebuild
/// seam. A bare edge type would silently rebuild a strong view as a weak one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ComponentRelation {
    Weak(RelationId),
    Strong(RelationId),
}
impl ComponentRelation {
    fn edge_type(self) -> RelationId {
        match self {
            Self::Weak(relation) | Self::Strong(relation) => relation,
        }
    }
    fn empty_kernel(self) -> Kernel {
        match self {
            Self::Weak(_) => Kernel::Weak(IncrementalComponents::new()),
            Self::Strong(_) => Kernel::Strong(IncrementalStrongComponents::new()),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Kernel {
    Weak(IncrementalComponents<VId>),
    Strong(IncrementalStrongComponents<VId>),
}

// Statically dispatched prepared updates keep one source/sink publication law
// without allocating boxes or duplicating the admission checks for each mode.
trait PreparedMembership {
    fn delta(&self) -> &ZSet<Pair>;
    fn vertex_count(&self) -> usize;
    fn affected_vertices(&self) -> usize;
    fn commit(self) -> ZSet<Pair>;
}
impl PreparedMembership for ComponentUpdate<'_, VId> {
    fn delta(&self) -> &ZSet<Pair> { ComponentUpdate::delta(self) }
    fn vertex_count(&self) -> usize { ComponentUpdate::vertex_count(self) }
    fn affected_vertices(&self) -> usize { ComponentUpdate::affected_vertices(self) }
    fn commit(self) -> ZSet<Pair> { ComponentUpdate::commit(self) }
}
impl PreparedMembership for StrongComponentUpdate<'_, VId> {
    fn delta(&self) -> &ZSet<Pair> { StrongComponentUpdate::delta(self) }
    fn vertex_count(&self) -> usize { StrongComponentUpdate::vertex_count(self) }
    fn affected_vertices(&self) -> usize { StrongComponentUpdate::affected_vertices(self) }
    fn commit(self) -> ZSet<Pair> { StrongComponentUpdate::commit(self) }
}

fn accept_membership(
    pending: impl PreparedMembership,
    rows: Option<&mut ZSet<Pair>>,
    meter: &mut Meter<'_>,
) -> Result<ZSet<Pair>, StandingQueryFailure> {
    meter.stats.affected_vertices = u64::try_from(pending.affected_vertices())
        .map_err(|_| StandingQueryFailure::Arithmetic)?;
    // Final membership size, not the transient retraction/insertion prefix.
    result_bound(pending.vertex_count(), meter.policy)?;
    if let Some(rows) = rows {
        let sink = rows
            .prepare_update(pending.delta(), LIMBS, &mut |event| meter.charge(event))
            .map_err(zset_error)?;
        for (pair, _) in pending.delta().iter() {
            meter.charge(ZSetEvent::Work)?;
            if sink.weight(pair).is_some_and(|w| w != &ZWeight::ONE) {
                return Err(StandingQueryFailure::InvalidDelta);
            }
        }
        (meter.checkpoint)()?;
        let delta = pending.commit();
        sink.commit();
        Ok(delta)
    } else {
        (meter.checkpoint)()?;
        // From empty state the exact derivative IS the initial membership set.
        Ok(pending.commit())
    }
}

impl Kernel {
    fn apply(
        &mut self,
        vertices: &ZSet<VId>,
        edges: &ZSet<Pair>,
        rows: Option<&mut ZSet<Pair>>,
        meter: &mut Meter<'_>,
    ) -> Result<ZSet<Pair>, StandingQueryFailure> {
        match self {
            Self::Weak(kernel) => {
                let pending = kernel.prepare(vertices, edges, LIMBS,
                    &mut |event| meter.charge(event)).map_err(component_error)?;
                accept_membership(pending, rows, meter)
            }
            Self::Strong(kernel) => {
                let pending = kernel.prepare(vertices, edges, LIMBS,
                    &mut |event| meter.charge(event)).map_err(component_error)?;
                accept_membership(pending, rows, meter)
            }
        }
    }
    fn component_count(&self) -> usize {
        match self {
            Self::Weak(kernel) => kernel.component_count(),
            Self::Strong(kernel) => kernel.component_count(),
        }
    }
    fn representative(&self, vertex: &VId) -> Option<&VId> {
        match self {
            Self::Weak(kernel) => kernel.representative(vertex),
            Self::Strong(kernel) => kernel.representative(vertex),
        }
    }
}

pub(crate) struct State {
    input: CommittedEdgeInput,
    components: Kernel,
    rows: ZSet<Pair>,
    pub(super) relation: ComponentRelation,
    pub(super) policy: GqlQueryPolicy,
    pub(super) frontier: CommitSeq,
    pub(super) stats: StandingQueryStats,
    pub(super) failure: Option<StandingQueryFailure>,
}
pub(super) fn input_error(error: EdgeInputError<StandingQueryFailure>) -> StandingQueryFailure {
    match error {
        EdgeInputError::Delta(error) => zset_error(error),
        _ => StandingQueryFailure::InvalidDelta,
    }
}
pub(super) fn component_error(error: ComponentError<StandingQueryFailure>) -> StandingQueryFailure {
    match error {
        ComponentError::Delta(error) => zset_error(error),
        ComponentError::CardinalityOverflow => StandingQueryFailure::Arithmetic,
        _ => StandingQueryFailure::InvalidDelta,
    }
}
pub(super) fn result_bound(
    count: usize,
    policy: GqlQueryPolicy,
) -> Result<(), StandingQueryFailure> {
    if policy
        .rows
        .max_result_rows()
        .is_some_and(|limit| count as u128 > u128::from(limit))
    {
        return Err(StandingQueryFailure::ResultBudget);
    }
    Ok(())
}
pub(super) fn project(
    input: &ZSet<EdgeTuple>,
    relation: RelationId,
    meter: &mut Meter<'_>,
) -> Result<ZSet<Pair>, StandingQueryFailure> {
    input
        .filter(|(r, _, _)| Ok(*r == relation), LIMBS, &mut |event| {
            meter.charge(event)
        })
        .map_err(zset_error)?
        .map(|(_, a, b)| Ok((*a, *b)), LIMBS, &mut |event| {
            meter.charge(event)
        })
        .map_err(zset_error)
}
pub(super) fn vertex_delta(
    batch: &LogicalDeltaBatch,
    meter: &mut Meter<'_>,
) -> Result<ZSet<VId>, StandingQueryFailure> {
    let mut updates = Vec::new();
    for coordinate in batch.coordinate_entries() {
        meter.charge(ZSetEvent::Work)?;
        if coordinate.graph != crate::GRAPH || coordinate.branch != crate::BRANCH {
            continue;
        }
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
    pub(super) fn maintain(
        &mut self,
        cx: &CommitCx,
        batch: &LogicalDeltaBatch,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        if self.frontier != self.input.frontier() {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        recursive::observe_batch(batch, meter)?;
        // Validate the whole batch BEFORE filtering its topology or vertices.
        let input = self
            .input
            .prepare_committed_successor(cx, batch, LIMBS, &mut |event| meter.charge(event))
            .map_err(input_error)?;
        let vertices = vertex_delta(batch, meter)?;
        let edges = project(input.delta(), self.relation.edge_type(), meter)?;
        let _ = self.components.apply(&vertices, &edges, Some(&mut self.rows), meter)?;
        // All recoverable work is done. No callbacks separate these publications.
        let _ = input.commit();
        Ok(())
    }

    fn from_snapshot(
        snapshot: &crate::Snapshot,
        relation: ComponentRelation,
        meter: &mut Meter<'_>,
    ) -> Result<Self, StandingQueryFailure> {
        let TopologyInput {
            input,
            vertices,
            edges,
        } = topology_input(snapshot, relation.edge_type(), meter)?;
        let mut components = relation.empty_kernel();
        let rows = components.apply(&vertices, &edges, None, meter)?;
        Ok(Self {
            input,
            components,
            rows,
            relation,
            policy: meter.policy,
            frontier: snapshot.frontier,
            stats: meter.stats,
            failure: None,
        })
    }
}

/// Shared authenticated, governed vertex/edge bootstrap for topology analytics.
/// This is private preparation, never a new source or cursor authority.
pub(super) struct TopologyInput {
    pub(super) input: CommittedEdgeInput,
    pub(super) vertices: ZSet<VId>,
    pub(super) edges: ZSet<Pair>,
}
pub(super) fn topology_input(
    snapshot: &crate::Snapshot,
    relation: RelationId,
    meter: &mut Meter<'_>,
) -> Result<TopologyInput, StandingQueryFailure> {
    // Admit physical vertex AND edge history before either borrowed scan.
    // Compacted versions/tombstones cost source admission too. The shared
    // topology bootstrap's own edge admission remains unchanged.
    let mut records = 0_u128;
    for count in snapshot
        .blocks
        .iter()
        .map(|block| block.len())
        .chain(snapshot.patches.iter().map(|patch| patch.len()))
    {
        meter.charge(ZSetEvent::Work)?;
        records = records
            .checked_add(count as u128)
            .ok_or(StandingQueryFailure::SnapshotBudget)?;
        if meter
            .policy
            .rows
            .max_snapshot_records()
            .is_some_and(|limit| records > u128::from(limit))
        {
            return Err(StandingQueryFailure::SnapshotBudget);
        }
    }
    let baseline = recursive::topology_snapshot(snapshot, relation, meter)?;
    let edges = project(baseline.rows(), relation, meter)?;
    let mut vertices = Vec::new();
    source::visit_vertices(
        &snapshot.patches,
        snapshot.frontier,
        &mut |event| {
            meter.charge(match event {
                SourceEvent::ScratchEntry => ZSetEvent::ScratchEntry,
                SourceEvent::Work | SourceEvent::SnapshotRecord => ZSetEvent::Work,
            })
        },
        |row, control| {
            control(SourceEvent::Work)?;
            control(SourceEvent::ScratchEntry)?;
            vertices.push((row.vid, ZWeight::ONE));
            Ok(())
        },
    )?;
    let vertices = ZSet::from_updates(vertices, LIMBS, &mut |event| meter.charge(event))
        .map_err(zset_error)?;
    Ok(TopologyInput {
        input: baseline.into_input(),
        vertices,
        edges,
    })
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
    pub fn register_standing_components(
        &mut self,
        cx: &QueryCx,
        relation: RelationId,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        let state = self.prepare_standing_components(cx, ComponentRelation::Weak(relation), policy)?;
        Ok(self.store_standing_query(StandingQuery::Components(Box::new(state))))
    }

    /// Register exact directed strongly connected components for one edge type.
    /// Every live vertex has a weight-one (vertex, minimum SCC member) row,
    /// including isolated vertices and vertices appearing only in other types.
    /// A one-way path does not merge SCCs; directed cycles do. Parallel edges
    /// count independently and a partial retraction retains remaining support.
    ///
    /// Read using standing_components, standing_component_count or
    /// standing_component. Ordinary committed writes maintain the view; staged
    /// transactions remain invisible. Initialization, failure fencing and
    /// explicit rebuild use the SAME owner registry, authenticated source,
    /// cumulative policy and atomic output sink as weak components. Rebuild
    /// retains strong connectivity; a refused rebuild keeps the previous state.
    ///
    /// Directed support changes rederive affected old weakly connected regions
    /// with iterative DFS; unrelated regions are not scanned or copied.
    /// Properties and multiplicity-only changes do not traverse connectivity.
    /// A large affected region can still cost its entire topology. This is not
    /// a fully dynamic SCC complexity guarantee or a quadratic closure cache.
    ///
    /// Result quotas count live vertices; work/scratch include source, kernel
    /// and sink. State remains session-local and memory-resident, not a durable
    /// subscription, spill implementation, byte quota or authorization facade.
    /// Labels, properties and valid time do not filter this topology API.
    pub fn register_standing_strong_components(
        &mut self,
        cx: &QueryCx,
        relation: RelationId,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        let state = self.prepare_standing_components(cx, ComponentRelation::Strong(relation), policy)?;
        Ok(self.store_standing_query(StandingQuery::Components(Box::new(state))))
    }

    pub(super) fn prepare_standing_components(
        &self,
        cx: &QueryCx,
        relation: ComponentRelation,
        policy: GqlQueryPolicy,
    ) -> Result<State, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.ensure_readable().map_err(StandingQueryError::Read)?;
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
            State::from_snapshot(&self.snapshot, relation, &mut meter)
                .map_err(StandingQueryError::Maintenance)
        })
    }

    /// Borrow current membership. Rows are in canonical (vertex, representative)
    /// order; ordered_rows() is None because no separate rank stage is needed.
    /// The handle's registration fixes weak versus strong connectivity.
    pub fn standing_components<'a>(
        &'a self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
    ) -> Result<StandingQueryView<'a, (VId, VId)>, StandingQueryError> {
        let StandingQuery::Components(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(StandingQueryView {
            rows: &query.rows,
            ordered: None,
            frontier: query.frontier,
            stats: &query.stats,
        })
    }

    /// Current component count without scanning membership rows.
    pub fn standing_component_count(
        &self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
    ) -> Result<usize, StandingQueryError> {
        let StandingQuery::Components(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.components.component_count())
    }

    /// Current representative, or None for a non-live vertex. A missing or
    /// unavailable view is a typed refusal, never confused with missing data.
    pub fn standing_component(
        &self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
        vertex: VId,
    ) -> Result<Option<VId>, StandingQueryError> {
        let StandingQuery::Components(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.components.representative(&vertex).copied())
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod strong_tests;
