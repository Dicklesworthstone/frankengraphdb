//! Query-selected connectivity over a complete maintained row relation.
//!
//! Selection, joins and bag semantics belong to the parent circuit. This
//! adapter derives endpoint presence and directed edge support, then uses the
//! SAME weak/strong kernels and pair/native sinks as graph-owned components.
//! No graph scan, intent evaluation or second topology algorithm lives here.

use super::*;
use fgdb_delta_types::zset::set::{IncrementalSet, SetError, SetOperation};
use fgdb_gql::GraphSetColumnType;
use fgdb_gql::algebra::GraphValue;

pub(super) type VertexSupport = IncrementalSet<VId>;

/// Fixed source identity and schema, retained across rebuild. Only registration
/// constructs this definition, from an already admitted, earlier registry entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::standing_query) struct Definition {
    input: usize,
    endpoints: [usize; 2],
    width: usize,
}

impl Definition {
    fn matches_schema(self, source: &StandingQuery) -> bool {
        sets::columns(source).is_some_and(|names| names.len() == self.width)
            && self.endpoints.iter().all(|&column| {
                column < self.width
                    && sets::column_type(source, column) == Some(GraphSetColumnType::Vertex)
            })
    }
}

fn support_error(error: SetError<StandingQueryFailure>) -> StandingQueryFailure {
    match error {
        SetError::Delta(error) => zset_error(error),
        SetError::NegativeMultiplicity { .. } => StandingQueryFailure::InvalidDelta,
    }
}

struct Projection {
    vertices: ZSet<VId>,
    edges: ZSet<Pair>,
}

fn endpoint(value: &GraphValue) -> Result<Option<VId>, StandingQueryFailure> {
    match value {
        GraphValue::Vertex(vertex) => Ok(Some(*vertex)),
        value if value.is_null() => Ok(None),
        _ => Err(StandingQueryFailure::InvalidDelta),
    }
}

fn project_rows(
    delta: &ZSet<GraphValueRow>,
    definition: Definition,
    meter: &mut Meter<'_>,
) -> Result<Projection, StandingQueryFailure> {
    let mut vertices = ZSet::new();
    let mut edges = ZSet::new();
    for (row, weight) in delta.iter() {
        meter.charge(ZSetEvent::Work)?;
        if row.len() != definition.width {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        let [left, right] = definition.endpoints;
        meter.units(ZSetEvent::Work, 2)?;
        let left = endpoint(row.values().get(left).ok_or(StandingQueryFailure::InvalidDelta)?)?;
        let right = endpoint(row.values().get(right).ok_or(StandingQueryFailure::InvalidDelta)?)?;
        // Both cells were checked before processing NULL. A self-loop supplies
        // one endpoint witness per occurrence, not two accidental lifetimes.
        for vertex in left.into_iter().chain(right.filter(|_| right != left)) {
            meter.charge(ZSetEvent::Work)?;
            let count = weight.checked_clone(LIMBS).map_err(|_| StandingQueryFailure::Arithmetic)?;
            vertices.accumulate(vertex, count, LIMBS, &mut |event| meter.charge(event))
                .map_err(zset_error)?;
        }
        if let (Some(left), Some(right)) = (left, right) {
            meter.charge(ZSetEvent::Work)?;
            let count = weight.checked_clone(LIMBS).map_err(|_| StandingQueryFailure::Arithmetic)?;
            edges.accumulate((left, right), count, LIMBS, &mut |event| meter.charge(event))
                .map_err(zset_error)?;
        }
    }
    Ok(Projection { vertices, edges })
}

impl State {
    fn from_selected(
        source: &ZSet<GraphValueRow>,
        definition: Definition,
        relation: ComponentRelation,
        frontier: CommitSeq,
        meter: &mut Meter<'_>,
    ) -> Result<Self, StandingQueryFailure> {
        let projection = project_rows(source, definition, meter)?;
        let mut support = VertexSupport::new(SetOperation::UnionDistinct);
        let vertices = support.prepare(&projection.vertices, &ZSet::new(), LIMBS,
            &mut |event| meter.charge(event)).map_err(support_error)?;
        let mut components = relation.empty_kernel();
        let rows = components.apply(vertices.delta(), &projection.edges, None, meter)?;
        let relational = rows::State::from_membership(&rows, meter)?;
        // Every object is still private. Only a complete candidate can escape.
        let _ = vertices.commit();
        Ok(Self {
            input: Input::Selected(support),
            components,
            rows,
            relational,
            relation,
            policy: meter.policy,
            frontier,
            stats: meter.stats,
            failure: None,
        })
    }

    fn apply_selected(
        &mut self,
        delta: &ZSet<GraphValueRow>,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        let definition = self.relation.selected().ok_or(StandingQueryFailure::InvalidDelta)?;
        let Input::Selected(support) = &mut self.input else {
            return Err(StandingQueryFailure::InvalidDelta);
        };
        let projection = project_rows(delta, definition, meter)?;
        let vertices = support.prepare(&projection.vertices, &ZSet::new(), LIMBS,
            &mut |event| meter.charge(event)).map_err(support_error)?;
        // DISTINCT is applied to integrated counts, not to signed input deltas.
        // Keep its guard uncommitted until connectivity AND both sinks accept.
        let _ = self.components.apply(vertices.delta(), &projection.edges,
            Some((&mut self.rows, &mut self.relational)), meter)?;
        let _ = vertices.commit();
        Ok(())
    }

    pub(in crate::standing_query) fn maintain_with_sources(
        &mut self,
        cx: &CommitCx,
        batch: &LogicalDeltaBatch,
        sources: &[StandingQuery],
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        let Some(definition) = self.relation.selected() else {
            return self.maintain(cx, batch, meter);
        };
        meter.charge(ZSetEvent::Work)?;
        let at = batch.commit_seq();
        if self.frontier.checked_successor().ok() != Some(at)
            || batch.frontier() != at
            || batch.commit_marker_identity().commit_seq != at
        {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        let source = sets::input_at(sources, definition.input, at)?;
        if !definition.matches_schema(source) {
            return Err(StandingQueryFailure::DependencyUnavailable);
        }
        let delta = sets::delta(source).ok_or(StandingQueryFailure::DependencyUnavailable)?;
        meter.stats.delta_rows = u64::try_from(delta.len()).map_err(|_| StandingQueryFailure::WorkBudget)?;
        self.apply_selected(delta, meter)
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Maintain weak components of a query-selected row relation. `endpoints`
    /// selects two zero-based Vertex columns, checked even on an empty source.
    /// Both non-NULL values supply one directed edge per row occurrence. A
    /// single non-NULL endpoint supplies an isolated vertex; two NULLs supply
    /// nothing. The same slot may be selected twice to represent self-loops.
    ///
    /// Unlike whole-relation components, the vertex universe is EXACTLY the
    /// non-NULL endpoints supported by the parent's selected rows, not every
    /// live database vertex. Duplicate rows and shared endpoints retain counted
    /// support: only losing the last witness retires a vertex or edge. Parents'
    /// filters, joins, set operations and pages execute BEFORE connectivity.
    ///
    /// Uses the existing component membership/count/native-row/delta APIs,
    /// kernels, atomic sinks, cursors and replay delivery. Changes to selected
    /// properties can change topology without creating or deleting base edges.
    /// Initialization/rebuild consumes the current parent bag; each commit
    /// consumes only its complete accepted derivative, never graph storage.
    /// An unavailable parent or new baseline is not an empty tick. Rebuild the
    /// parent first, then this view; source, column selection and mode persist.
    ///
    /// max_snapshot_records bounds compressed source support at initialization;
    /// max_result_rows bounds final distinct vertices. Work/scratch are shared
    /// by projection, presence, kernel and sinks, per node, not allocator bytes.
    /// Connectivity rederivation can visit an entire affected weak region.
    /// State is session-local and memory-resident, not durable registration,
    /// spill, a new GQL grammar, a full dynamic complexity bound, or an
    /// authorization boundary. No base vertex/property state is mutated.
    pub fn register_standing_components_from_rows(
        &mut self,
        cx: &QueryCx,
        source: &StandingQueryHandle,
        endpoints: [usize; 2],
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        self.register_selected_components(cx, source, endpoints, false, policy)
    }

    /// Directed strongly connected components under the SAME query-selected
    /// endpoint/NULL/support contract as register_standing_components_from_rows.
    /// A one-way path does not merge SCCs; a directed cycle does. Full-width
    /// minimum Vertex IDs are representatives. Rebuild never changes the mode.
    pub fn register_standing_strong_components_from_rows(
        &mut self,
        cx: &QueryCx,
        source: &StandingQueryHandle,
        endpoints: [usize; 2],
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        self.register_selected_components(cx, source, endpoints, true, policy)
    }

    fn register_selected_components(
        &mut self,
        cx: &QueryCx,
        source: &StandingQueryHandle,
        endpoints: [usize; 2],
        strong: bool,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        let parent = self.admitted_standing_query(cx, source)?;
        let definition = Definition {
            input: source.index,
            endpoints,
            width: sets::columns(parent).ok_or(StandingQueryError::Unsupported)?.len(),
        };
        if !definition.matches_schema(parent) {
            return Err(StandingQueryError::Unsupported);
        }
        let relation = if strong {
            ComponentRelation::StrongRows(definition)
        } else {
            ComponentRelation::WeakRows(definition)
        };
        let state = self.prepare_standing_components(cx, relation, policy)?;
        self.store_standing_components(cx, state)
    }

    pub(super) fn prepare_selected_components(
        &self,
        definition: Definition,
        relation: ComponentRelation,
        meter: &mut Meter<'_>,
    ) -> Result<State, StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        let parent = sets::input_at(&self.standing_queries, definition.input, self.snapshot.frontier)?;
        if !definition.matches_schema(parent) {
            return Err(StandingQueryFailure::DependencyUnavailable);
        }
        let rows = sets::rows(parent).ok_or(StandingQueryFailure::DependencyUnavailable)?;
        if meter.policy.rows.max_snapshot_records()
            .is_some_and(|limit| rows.len() as u128 > u128::from(limit))
        {
            return Err(StandingQueryFailure::SnapshotBudget);
        }
        State::from_selected(rows, definition, relation, self.snapshot.frontier, meter)
    }
}

#[cfg(test)]
mod tests;
