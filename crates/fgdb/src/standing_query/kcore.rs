//! Exact standing core numbers over the existing committed topology lifecycle.
//! Uses the component input/bootstrap and the component-owned k-core kernel;
//! no second graph representation, delta decoder or publication authority.

mod rows;

use super::*;
use fgdb_delta_types::zset::committed::CommittedEdgeInput;
use fgdb_delta_types::zset::components::kcore::{CoreError, IncrementalCoreNumbers};
use fgdb_delta_types::zset::set::SetOperation;
use fgdb_delta_types::{LimbLimit, ZWeight};

const LIMBS: LimbLimit = LimbLimit::new(4);

#[derive(Debug, PartialEq, Eq)]
enum Input {
    Committed(CommittedEdgeInput),
    Selected(components::relation::VertexSupport),
}

pub(crate) struct State {
    input: Input,
    cores: IncrementalCoreNumbers<VId>,
    rows: ZSet<(VId, u64)>,
    relational: Option<rows::State>,
    // None retains the whole-graph selector, including future relation types.
    pub(super) relation: Option<RelationId>,
    pub(super) selected: Option<components::relation::Definition>,
    pub(super) policy: GqlQueryPolicy,
    pub(super) frontier: CommitSeq,
    pub(super) stats: StandingQueryStats,
    pub(super) failure: Option<StandingQueryFailure>,
}
fn core_error(error: CoreError<StandingQueryFailure>) -> StandingQueryFailure {
    match error {
        CoreError::Topology(error) => components::component_error(error),
        CoreError::Delta(error) => zset_error(error),
        CoreError::DegreeOverflow => StandingQueryFailure::Arithmetic,
        CoreError::InconsistentTopology => StandingQueryFailure::InvalidDelta,
    }
}
impl State {
    pub(super) fn has_value_rows(&self) -> bool {
        self.relational.is_some()
    }

    pub(super) fn columns(&self) -> Option<&[String]> {
        self.relational.as_ref().map(rows::State::columns)
    }

    pub(super) fn value_rows(&self) -> Option<&ZSet<GraphValueRow>> {
        self.relational.as_ref().map(rows::State::rows)
    }

    pub(super) fn value_delta(&self) -> Option<&ZSet<GraphValueRow>> {
        self.relational.as_ref().and_then(rows::State::delta)
    }

    pub(super) fn maintain(
        &mut self,
        cx: &CommitCx,
        batch: &LogicalDeltaBatch,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        let Input::Committed(input) = &mut self.input else {
            return Err(StandingQueryFailure::InvalidDelta);
        };
        if self.selected.is_some() || self.frontier != input.frontier() {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        recursive::observe_batch(batch, meter)?;
        let input = input
            .prepare_committed_successor(cx, batch, LIMBS, &mut |event| meter.charge(event))
            .map_err(components::input_error)?;
        // Global vertex membership is projected only after full batch admission.
        let vertices = components::vertex_delta(batch, meter)?;
        let edges = components::project(input.delta(), self.relation, meter)?;
        Self::apply_topology(
            &mut self.cores,
            &mut self.rows,
            &mut self.relational,
            &vertices,
            &edges,
            meter,
        )?;
        let _ = input.commit();
        Ok(())
    }

    /// Selected inputs consume the accepted parent derivative at this same
    /// commit. A failed parent or a rebuilt baseline is never an empty tick.
    pub(super) fn maintain_with_sources(
        &mut self,
        cx: &CommitCx,
        batch: &LogicalDeltaBatch,
        sources: &[StandingQuery],
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        let Some(definition) = self.selected else {
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
        meter.stats.delta_rows =
            u64::try_from(delta.len()).map_err(|_| StandingQueryFailure::WorkBudget)?;
        self.apply_selected(delta, meter)
    }

    fn apply_selected(
        &mut self,
        delta: &ZSet<GraphValueRow>,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        let definition = self.selected.ok_or(StandingQueryFailure::InvalidDelta)?;
        let Input::Selected(support) = &mut self.input else {
            return Err(StandingQueryFailure::InvalidDelta);
        };
        let projection = components::relation::project_rows(delta, definition, meter)?;
        let vertices = support
            .prepare(&projection.vertices, &ZSet::new(), LIMBS, &mut |event| {
                meter.charge(event)
            })
            .map_err(components::relation::support_error)?;
        // Presence derives from integrated witness counts. Keep its guard
        // pending until the core kernel and both sinks have accepted, so a
        // shared/duplicate endpoint retires only with its final witness.
        Self::apply_topology(
            &mut self.cores,
            &mut self.rows,
            &mut self.relational,
            vertices.delta(),
            &projection.edges,
            meter,
        )?;
        let _ = vertices.commit();
        Ok(())
    }

    fn apply_topology(
        cores: &mut IncrementalCoreNumbers<VId>,
        rows: &mut ZSet<(VId, u64)>,
        relational: &mut Option<rows::State>,
        vertices: &ZSet<VId>,
        edges: &ZSet<(VId, VId)>,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        let pending = cores
            .prepare(vertices, edges, LIMBS, &mut |event| meter.charge(event))
            .map_err(core_error)?;
        meter.stats.affected_vertices = u64::try_from(pending.affected_vertices())
            .map_err(|_| StandingQueryFailure::Arithmetic)?;
        // A vertex has one final number, irrespective of a changed shell value.
        components::result_bound(pending.vertex_count(), meter.policy)?;
        let sink = rows
            .prepare_update(pending.delta(), LIMBS, &mut |event| meter.charge(event))
            .map_err(zset_error)?;
        let native = relational
            .as_mut()
            .map(|rows| rows.prepare(pending.delta(), pending.vertex_count(), meter))
            .transpose()?;
        for (row, _) in pending.delta().iter() {
            meter.charge(ZSetEvent::Work)?;
            if sink
                .weight(row)
                .is_some_and(|weight| weight != &ZWeight::ONE)
            {
                return Err(StandingQueryFailure::InvalidDelta);
            }
        }
        (meter.checkpoint)()?;
        // Input guards remain pending until all fallible work above succeeds.
        // No recoverable operation separates the accepted publications.
        let _ = pending.commit();
        sink.commit();
        if let Some(native) = native {
            native.commit();
        }
        Ok(())
    }

    fn from_selected(
        source: &ZSet<GraphValueRow>,
        definition: components::relation::Definition,
        frontier: CommitSeq,
        native_rows: bool,
        meter: &mut Meter<'_>,
    ) -> Result<Self, StandingQueryFailure> {
        let projection = components::relation::project_rows(source, definition, meter)?;
        let mut support = components::relation::VertexSupport::new(SetOperation::UnionDistinct);
        let vertices = support
            .prepare(&projection.vertices, &ZSet::new(), LIMBS, &mut |event| {
                meter.charge(event)
            })
            .map_err(components::relation::support_error)?;
        let mut cores = IncrementalCoreNumbers::new();
        let pending = cores
            .prepare(vertices.delta(), &projection.edges, LIMBS, &mut |event| {
                meter.charge(event)
            })
            .map_err(core_error)?;
        components::result_bound(pending.vertex_count(), meter.policy)?;
        meter.stats.affected_vertices = u64::try_from(pending.affected_vertices())
            .map_err(|_| StandingQueryFailure::Arithmetic)?;
        (meter.checkpoint)()?;
        let rows = pending.commit();
        let relational = if native_rows {
            Some(rows::State::from_numbers(&rows, meter)?)
        } else {
            None
        };
        // The candidate is private until every native row is also admitted.
        let _ = vertices.commit();
        Ok(Self {
            input: Input::Selected(support),
            cores,
            rows,
            relational,
            relation: None,
            selected: Some(definition),
            policy: meter.policy,
            frontier,
            stats: meter.stats,
            failure: None,
        })
    }

    fn from_snapshot(
        snapshot: &crate::Snapshot,
        relation: Option<RelationId>,
        native_rows: bool,
        meter: &mut Meter<'_>,
    ) -> Result<Self, StandingQueryFailure> {
        let components::TopologyInput {
            input,
            vertices,
            edges,
        } = components::topology_input(snapshot, relation, meter)?;
        let mut cores = IncrementalCoreNumbers::new();
        let pending = cores
            .prepare(&vertices, &edges, LIMBS, &mut |event| meter.charge(event))
            .map_err(core_error)?;
        components::result_bound(pending.vertex_count(), meter.policy)?;
        meter.stats.affected_vertices = u64::try_from(pending.affected_vertices())
            .map_err(|_| StandingQueryFailure::Arithmetic)?;
        (meter.checkpoint)()?;
        let rows = pending.commit();
        let relational = if native_rows {
            Some(rows::State::from_numbers(&rows, meter)?)
        } else {
            None
        };
        Ok(Self {
            input: Input::Committed(input),
            cores,
            rows,
            relational,
            relation,
            selected: None,
            policy: meter.policy,
            frontier: snapshot.frontier,
            stats: meter.stats,
            failure: None,
        })
    }
}
impl<V: Vfs + Clone> Database<V> {
    /// Maintain exact core numbers of one relation's undirected SIMPLE support.
    /// Parallel/opposite edges contribute a single neighbor; self-loops none.
    /// Every live graph vertex has one (VId, u64) row of weight one, including
    /// isolates with number zero. Number >= k is membership in that k-core.
    /// This is not multigraph/directed core semantics or property/valid-time filtering.
    ///
    /// Core peeling rederives affected components under one cumulative input,
    /// work and scratch allowance. Unrelated components are not scanned; a
    /// giant affected component may still require a full traversal. Result
    /// quotas count final live vertices, not transient shell replacements.
    ///
    /// Ordinary durable writes advance the view or fence it unavailable;
    /// failures never undo durable writes or block healthy siblings. Rebuild
    /// uses current authenticated vertex/edge state through the same handle.
    /// Registrations and arrangements are session-local and in-memory, not
    /// durable subscriptions, historical core indexes or a spill/byte bound.
    pub fn register_standing_core_numbers(
        &mut self,
        cx: &QueryCx,
        relation: RelationId,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        let state = self.prepare_standing_core_numbers(cx, Some(relation), policy, false)?;
        Ok(self.store_standing_query(StandingQuery::CoreNumbers(Box::new(state))))
    }

    /// Maintain exact core numbers over an earlier maintained row source.
    /// `endpoints` selects two zero-based Vertex columns, checked even on empty
    /// input. Non-NULL endpoints define the selected vertex population; two
    /// endpoints also supply an undirected edge per row occurrence. A lone
    /// endpoint or self-pair keeps an isolate, and two NULLs contribute nothing.
    /// Parallel/opposite occurrences collapse to simple adjacency; self-loops
    /// contribute no degree. Counted support preserves shared endpoints until
    /// their last witness retracts. Parent filters/joins/pages apply first, so
    /// property-only changes can change core membership without base-edge edits.
    ///
    /// The handle supports the typed `standing_core_numbers`/`standing_core_number`
    /// methods and native `vertex, core` rows, cursors, signed deltas, row-circuit
    /// composition and subscription delivery. A selected isolate has core zero;
    /// an identity without selected support is absent, not a zero-core member.
    ///
    /// Initialization/rebuild consumes the current parent bag; later commits
    /// consume only its accepted derivatives. A failed parent or new baseline is
    /// not an empty tick. Repair the independently owned parent before rebuilding
    /// this view; the source and endpoint selection persist. A failed view never
    /// rolls back a durable write or prevents healthy siblings from advancing.
    ///
    /// `max_snapshot_records` bounds compressed parent rows at initialization;
    /// `max_result_rows` bounds final distinct vertices. Work/scratch cover
    /// projection, support, kernel and sinks cumulatively per node, while the
    /// parent retains its own policy. Core peeling may traverse an entire
    /// affected component. This trusted embedded registration is session-local
    /// and in-memory, not durable state, a spill/byte bound or an authorization
    /// boundary.
    pub fn register_standing_core_numbers_from_rows(
        &mut self,
        cx: &QueryCx,
        source: &StandingQueryHandle,
        endpoints: [usize; 2],
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        let definition = self.admit_standing_topology_source(cx, source, endpoints)?;
        let state = self.prepare_standing_selected_core_numbers(cx, definition, policy, true)?;
        // Native bootstrap admitted the fixed presentation copy. Retain the
        // typed and native sinks of this one owner without a projection node.
        let layout = Arc::new(native::Layout::Rows {
            columns: state
                .columns()
                .ok_or(StandingQueryError::Unsupported)?
                .to_vec(),
        });
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        let mut handle = self.store_standing_query(StandingQuery::CoreNumbers(Box::new(state)));
        handle.native = Some(layout);
        Ok(handle)
    }

    pub(super) fn prepare_standing_core_numbers(
        &self,
        cx: &QueryCx,
        relation: Option<RelationId>,
        policy: GqlQueryPolicy,
        native_rows: bool,
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
            State::from_snapshot(&self.snapshot, relation, native_rows, &mut meter)
                .map_err(StandingQueryError::Maintenance)
        })
    }

    pub(super) fn prepare_standing_selected_core_numbers(
        &self,
        cx: &QueryCx,
        definition: components::relation::Definition,
        policy: GqlQueryPolicy,
        native_rows: bool,
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
            let mut prepare = || {
                meter.charge(ZSetEvent::Work)?;
                let parent = sets::input_at(
                    &self.standing_queries,
                    definition.input,
                    self.snapshot.frontier,
                )?;
                if !definition.matches_schema(parent) {
                    return Err(StandingQueryFailure::DependencyUnavailable);
                }
                let rows = sets::rows(parent).ok_or(StandingQueryFailure::DependencyUnavailable)?;
                if meter
                    .policy
                    .rows
                    .max_snapshot_records()
                    .is_some_and(|limit| rows.len() as u128 > u128::from(limit))
                {
                    return Err(StandingQueryFailure::SnapshotBudget);
                }
                State::from_selected(
                    rows,
                    definition,
                    self.snapshot.frontier,
                    native_rows,
                    &mut meter,
                )
            };
            prepare().map_err(StandingQueryError::Maintenance)
        })
    }

    /// Borrow canonical (vertex, core-number) rows from one current generation.
    /// ordered_rows() is None; rows() already uses canonical vertex order.
    pub fn standing_core_numbers<'a>(
        &'a self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
    ) -> Result<StandingQueryView<'a, (VId, u64)>, StandingQueryError> {
        let StandingQuery::CoreNumbers(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(StandingQueryView {
            rows: &query.rows,
            ordered: None,
            frontier: query.frontier,
            stats: &query.stats,
        })
    }

    /// Read one current core number without scanning result rows. Some(0)
    /// names a live isolate; None names a non-live vertex. Unavailable/wrong-kind
    /// and foreign views refuse instead of being confused with either case.
    pub fn standing_core_number(
        &self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
        vertex: VId,
    ) -> Result<Option<u64>, StandingQueryError> {
        let StandingQuery::CoreNumbers(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.cores.core_number(&vertex))
    }
}

#[cfg(test)]
mod tests;
