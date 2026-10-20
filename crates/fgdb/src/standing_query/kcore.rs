//! Exact standing core numbers over the existing committed topology lifecycle.
//! Uses the component input/bootstrap and the component-owned k-core kernel;
//! no second graph representation, delta decoder or publication authority.

mod rows;

use super::*;
use fgdb_delta_types::zset::committed::CommittedEdgeInput;
use fgdb_delta_types::zset::components::kcore::{CoreError, IncrementalCoreNumbers};
use fgdb_delta_types::{LimbLimit, ZWeight};

const LIMBS: LimbLimit = LimbLimit::new(4);

pub(crate) struct State {
    input: CommittedEdgeInput,
    cores: IncrementalCoreNumbers<VId>,
    rows: ZSet<(VId, u64)>,
    relational: Option<rows::State>,
    pub(super) relation: RelationId,
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
        if self.frontier != self.input.frontier() {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        recursive::observe_batch(batch, meter)?;
        let input = self
            .input
            .prepare_committed_successor(cx, batch, LIMBS, &mut |event| meter.charge(event))
            .map_err(components::input_error)?;
        // Global vertex membership is projected only after full batch admission.
        let vertices = components::vertex_delta(batch, meter)?;
        let edges = components::project(input.delta(), self.relation, meter)?;
        let pending = self
            .cores
            .prepare(&vertices, &edges, LIMBS, &mut |event| meter.charge(event))
            .map_err(core_error)?;
        meter.stats.affected_vertices = u64::try_from(pending.affected_vertices())
            .map_err(|_| StandingQueryFailure::Arithmetic)?;
        // A vertex has one final number, irrespective of a changed shell value.
        components::result_bound(pending.vertex_count(), meter.policy)?;
        let sink = self
            .rows
            .prepare_update(pending.delta(), LIMBS, &mut |event| meter.charge(event))
            .map_err(zset_error)?;
        let native = self
            .relational
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
        // No recoverable operation separates the accepted publications.
        let _ = pending.commit();
        sink.commit();
        if let Some(native) = native {
            native.commit();
        }
        let _ = input.commit();
        Ok(())
    }

    fn from_snapshot(
        snapshot: &crate::Snapshot,
        relation: RelationId,
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
            input,
            cores,
            rows,
            relational,
            relation,
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
        let state = self.prepare_standing_core_numbers(cx, relation, policy, false)?;
        Ok(self.store_standing_query(StandingQuery::CoreNumbers(Box::new(state))))
    }

    pub(super) fn prepare_standing_core_numbers(
        &self,
        cx: &QueryCx,
        relation: RelationId,
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
