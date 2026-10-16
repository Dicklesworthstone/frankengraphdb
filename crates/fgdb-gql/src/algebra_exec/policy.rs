//! Composition of admission, output, evaluator limits and interruption.
//! All sealed row shapes share admission, terminal collection and policy counters.

mod topology;

use super::{GlaExecutionEvent, GlaExecutionLimits, GlaExecutionStats, GlaLimitExceeded};
use crate::GovernedCheckpoint;
use crate::algebra::{GlaIdentityOutput, GlaOutput, GlaPlan, VertexPredicate};
use crate::{
    BudgetedGqlError, BudgetedGqlExecution, GqlBudgetDimension, GqlBudgetExceeded,
    GqlExecutionBudget, GqlExecutionStats,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, EId, VId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlQueryPolicy {
    pub rows: GqlExecutionBudget,
    pub evaluator: GlaExecutionLimits,
}

impl GqlQueryPolicy {
    #[must_use]
    pub const fn new(
        snapshot_records: u64,
        result_rows: u64,
        work_units: u64,
        scratch_entries: u64,
    ) -> Self {
        Self {
            rows: GqlExecutionBudget::new(snapshot_records, result_rows),
            evaluator: GlaExecutionLimits::new(work_units, scratch_entries),
        }
    }
}

/// Counters describe this one execution. Row limits count complete projected
/// rows, not cells; tuple cells additionally consume evaluator scratch/work.
#[derive(Clone, PartialEq, Eq)]
pub struct GqlQueryExecution<Row = VId> {
    pub value: Vec<Row>,
    pub rows: GqlExecutionStats,
    pub evaluator: GlaExecutionStats,
}

impl<Row> core::fmt::Debug for GqlQueryExecution<Row> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GqlQueryExecution")
            .field("value", &"[REDACTED]")
            .field("rows", &self.rows)
            .field("evaluator", &self.evaluator)
            .finish()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum GqlQueryError<E, C> {
    Source(E),
    Rows(GqlBudgetExceeded),
    Evaluator(GlaLimitExceeded),
    Interrupted(C),
    IdentifiedEdgesRequired,
    /// A predicate's scalar expression raised an arithmetic data exception
    /// (GQL class 22: division by zero, numeric overflow; see
    /// `GraphIntegerErrorKind::is_arithmetic_exception`). It never reads as
    /// UNKNOWN or a filtered row.
    Data(crate::GraphIntegerError),
}

impl<E, C> From<crate::GraphIntegerError> for GqlQueryError<E, C> {
    fn from(error: crate::GraphIntegerError) -> Self {
        Self::Data(error)
    }
}

impl<E, C> GqlQueryError<E, C> {
    pub fn map_source<T>(self, map: impl FnOnce(E) -> T) -> GqlQueryError<T, C> {
        match self {
            Self::Source(error) => GqlQueryError::Source(map(error)),
            Self::Rows(error) => GqlQueryError::Rows(error),
            Self::Evaluator(error) => GqlQueryError::Evaluator(error),
            Self::Interrupted(error) => GqlQueryError::Interrupted(error),
            Self::IdentifiedEdgesRequired => GqlQueryError::IdentifiedEdgesRequired,
            Self::Data(error) => GqlQueryError::Data(error),
        }
    }
}
impl<E: core::fmt::Display, C: core::fmt::Display> core::fmt::Display for GqlQueryError<E, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Source(error) => core::fmt::Display::fmt(error, f),
            Self::Rows(error) => core::fmt::Display::fmt(error, f),
            Self::Evaluator(error) => core::fmt::Display::fmt(error, f),
            Self::Interrupted(error) => write!(f, "query interrupted: {error}"),
            Self::IdentifiedEdgesRequired => f.write_str("captured paths require identified edges"),
            Self::Data(error) => write!(f, "data exception: {error}"),
        }
    }
}
impl<E: core::error::Error + 'static, C: core::error::Error + 'static> core::error::Error
    for GqlQueryError<E, C>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Rows(error) => Some(error),
            Self::Evaluator(error) => Some(error),
            Self::Interrupted(error) => Some(error),
            Self::IdentifiedEdgesRequired => None,
            Self::Data(error) => Some(error),
        }
    }
}

/// Events one evaluator run defers.
const EVALUATOR_RUN: u64 = 1024;

/// A governed execution's charges: the host checkpoint, row and evaluator
/// counters, and the open evaluator run. Pattern and aggregate executions
/// share it. For a host that takes runs (`GovernedCheckpoint::RUNS`), while
/// the evaluator's allowances cover two whole runs, events are counted and
/// their host units, work and scratch charged together (the host's in one
/// call). The Work and scratch fast path is all `event` holds, so it inlines
/// into the hottest loops; everything else is `slow`. Any other host is
/// charged per event: host, rows, then evaluator.
pub(crate) struct Charges<K> {
    checkpoint: K,
    policy: GqlQueryPolicy,
    pub(crate) rows: GqlExecutionStats,
    pub(crate) evaluator: GlaExecutionStats,
    /// Events the open run may still take; `opened` is its size, so it has
    /// taken `opened - left`, each owing its host unit and evaluator work.
    left: u64,
    opened: u64,
    /// Scratch entries the open run has taken, owing their scratch charge.
    scratch: u64,
}

impl<K> Charges<K> {
    /// Charges after an admitted input of `snapshot_records`, before any event.
    pub(crate) fn new(checkpoint: K, policy: GqlQueryPolicy, snapshot_records: u64) -> Self {
        Self {
            checkpoint,
            policy,
            rows: GqlExecutionStats {
                snapshot_records,
                result_rows: 0,
            },
            evaluator: GlaExecutionStats::default(),
            left: 0,
            opened: 0,
            scratch: 0,
        }
    }

    /// The host's own charge outside any run, as before and after execution.
    /// No run is open there, so nothing deferred is passed.
    fn unit<E, C>(&mut self) -> Result<(), GqlQueryError<E, C>>
    where
        K: GovernedCheckpoint<C>,
    {
        debug_assert_eq!(self.left, 0);
        self.checkpoint.unit().map_err(GqlQueryError::Interrupted)
    }

    /// A Work event or scratch entry inside a run is only counted: the
    /// closure's whole body, so it inlines into the hottest loops.
    #[inline(always)]
    pub(crate) fn event<E, C>(
        &mut self,
        event: GlaExecutionEvent,
    ) -> Result<(), GqlQueryError<E, C>>
    where
        K: GovernedCheckpoint<C>,
    {
        if self.left > 0 {
            match event {
                GlaExecutionEvent::Work => {
                    self.left -= 1;
                    return Ok(());
                }
                GlaExecutionEvent::ScratchEntry => {
                    self.left -= 1;
                    self.scratch += 1;
                    return Ok(());
                }
                GlaExecutionEvent::ResultRow => {}
            }
        }
        self.slow(event)
    }

    /// Every event the fast path does not take. A spent run settles; the
    /// next opens while the host takes runs and the evaluator's work and
    /// scratch allowances each cover two whole runs, so no evaluator refusal
    /// can fall inside one. An event outside a run is charged on the spot:
    /// host, rows, then evaluator, the per-event order.
    #[inline(never)]
    fn slow<E, C>(&mut self, event: GlaExecutionEvent) -> Result<(), GqlQueryError<E, C>>
    where
        K: GovernedCheckpoint<C>,
    {
        if self.left == 0 {
            self.settle()?;
            let limits = self.policy.evaluator;
            let covered = |limit: u64, used: u64| limit.saturating_sub(used) >= 2 * EVALUATOR_RUN;
            if !K::RUNS
                || !covered(limits.max_work_units, self.evaluator.work_units)
                || !covered(limits.max_scratch_entries, self.evaluator.scratch_entries)
            {
                self.checkpoint.unit().map_err(GqlQueryError::Interrupted)?;
                return self.judge(event);
            }
            self.left = EVALUATOR_RUN;
            self.opened = EVALUATOR_RUN;
            if event != GlaExecutionEvent::ResultRow {
                return self.event(event);
            }
        }
        // Inside a run, a result row's limit is judged now while its host
        // unit and evaluator work wait. Per event, the host was charged for
        // it and for every event before it first, so a host refusal among
        // them wins.
        self.left -= 1;
        let next = self.rows.result_rows + 1;
        if let Err(error) = self.policy.rows.check(GqlBudgetDimension::ResultRows, next) {
            self.settle()?;
            return Err(GqlQueryError::Rows(error));
        }
        self.rows.result_rows = next;
        Ok(())
    }

    /// The row and evaluator charges of one event.
    fn judge<E, C>(&mut self, event: GlaExecutionEvent) -> Result<(), GqlQueryError<E, C>> {
        if event == GlaExecutionEvent::ResultRow {
            let next = self.rows.result_rows + 1;
            self.policy
                .rows
                .check(GqlBudgetDimension::ResultRows, next)
                .map_err(GqlQueryError::Rows)?;
            self.rows.result_rows = next;
        }
        self.evaluator
            .charge_event(self.policy.evaluator, event)
            .map_err(GqlQueryError::Evaluator)
    }

    /// Charge the run: its host units in one checkpoint call, in event order,
    /// then its events' evaluator work and its scratch entries. Nothing is
    /// left pending, so a host that refused here is never called again. A
    /// run opens only while the evaluator's allowances cover two whole runs,
    /// so only the host can refuse here, at its own per-unit ordinal.
    pub(crate) fn settle<E, C>(&mut self) -> Result<(), GqlQueryError<E, C>>
    where
        K: GovernedCheckpoint<C>,
    {
        let taken = self.opened - self.left;
        let scratch = std::mem::take(&mut self.scratch);
        self.left = 0;
        self.opened = 0;
        if taken > 0 {
            self.checkpoint
                .units(taken)
                .map_err(GqlQueryError::Interrupted)?;
            self.evaluator
                .charge_work_units(self.policy.evaluator, taken)
                .map_err(GqlQueryError::Evaluator)?;
        }
        if scratch > 0 {
            self.evaluator
                .charge_scratch_entries(self.policy.evaluator, scratch)
                .map_err(GqlQueryError::Evaluator)?;
        }
        Ok(())
    }
}

// One admission/completion contract for predicate-only and property-aware
// executions. Neither path gets a second meter or converts a source error to
// null. The accessors and private terminal collector are the only differences.
fn governed<Row, E, C, K: GovernedCheckpoint<C>>(
    snapshot_records: u64,
    policy: GqlQueryPolicy,
    mut checkpoint: K,
    execute: impl FnOnce(&mut Charges<K>) -> Result<Vec<Row>, GqlQueryError<E, C>>,
) -> Result<GqlQueryExecution<Row>, GqlQueryError<E, C>> {
    checkpoint.unit().map_err(GqlQueryError::Interrupted)?;
    policy
        .rows
        .check(GqlBudgetDimension::SnapshotRecords, snapshot_records)
        .map_err(GqlQueryError::Rows)?;
    let mut meter = Charges::new(checkpoint, policy, snapshot_records);
    let value = execute(&mut meter);
    // A run's units are charged before the outcome is judged; a host refusal
    // among them is the one per-event charging met first. A settled or
    // refused run leaves none, so a host that has refused is not called
    // again.
    meter.settle()?;
    let value = value?;
    meter.unit()?;
    debug_assert_eq!(
        u64::try_from(value.len()).ok(),
        Some(meter.rows.result_rows)
    );
    Ok(GqlQueryExecution {
        value,
        rows: meter.rows,
        evaluator: meter.evaluator,
    })
}

impl<Row: GlaIdentityOutput> GlaPlan<Row> {
    /// Admission precedes index construction. Each final row is checked before
    /// release; refusal never returns a partially constructed result vector.
    pub fn execute_budgeted<E>(
        &self,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
        mut test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        budget: GqlExecutionBudget,
    ) -> Result<BudgetedGqlExecution<Vec<Row>>, BudgetedGqlError<E>> {
        budget
            .check(GqlBudgetDimension::SnapshotRecords, snapshot_records)
            .map_err(BudgetedGqlError::Budget)?;
        let mut stats = GqlExecutionStats {
            snapshot_records,
            result_rows: 0,
        };
        let value = {
            let mut control = |event| {
                charge_result_row(&mut stats, budget, event).map_err(BudgetedGqlError::Budget)
            };
            if let Some(physical) = topology::compile(self.operators(), &mut control)? {
                physical.execute(self, edges, &mut control)?
            } else {
                self.execute_with_control(
                    vertices,
                    edges,
                    |vid, predicates| {
                        test_vertex(vid, predicates).map_err(BudgetedGqlError::Execution)
                    },
                    &mut control,
                )?
            }
        };
        debug_assert_eq!(u64::try_from(value.len()).ok(), Some(stats.result_rows));
        Ok(BudgetedGqlExecution { value, stats })
    }

    /// Identity outputs do not require a property accessor. Value outputs must
    /// use execute_governed_with_properties instead; an omitted source cannot
    /// silently turn real properties into nulls.
    pub fn execute_governed<E, C>(
        &self,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
        mut test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        policy: GqlQueryPolicy,
        checkpoint: impl GovernedCheckpoint<C>,
    ) -> Result<GqlQueryExecution<Row>, GqlQueryError<E, C>> {
        governed(snapshot_records, policy, checkpoint, |meter| {
            if let Some(physical) =
                topology::compile(self.operators(), &mut |event| meter.event(event))?
            {
                return physical.execute(self, edges, &mut |event| meter.event(event));
            }
            self.execute_with_control(
                vertices,
                edges,
                |vid, predicates| test_vertex(vid, predicates).map_err(GqlQueryError::Source),
                |event| meter.event(event),
            )
        })
    }
}

impl<Row: GlaOutput> GlaPlan<Row> {
    /// Project exact borrowed canonical values from the SAME admitted snapshot
    /// as matching. A source error remains Source, never a missing/null value.
    /// Property payloads are cloned only for newly retained complete rows and
    /// only after their logical scratch reservations. Result rows are owned.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_governed_with_properties<'a, E, C>(
        &self,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
        mut test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        mut property: impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        policy: GqlQueryPolicy,
        checkpoint: impl GovernedCheckpoint<C>,
    ) -> Result<GqlQueryExecution<Row>, GqlQueryError<E, C>> {
        governed(snapshot_records, policy, checkpoint, |meter| {
            if self.requires_identified_edges() || self.projects_edge_properties() {
                return Err(GqlQueryError::IdentifiedEdgesRequired);
            }
            if let Some(physical) =
                topology::compile(self.operators(), &mut |event| meter.event(event))?
            {
                return physical.execute(self, edges, &mut |event| meter.event(event));
            }
            self.execute_with_properties_control(
                vertices,
                edges,
                |vid, predicates| test_vertex(vid, predicates).map_err(GqlQueryError::Source),
                |vid, key| property(vid, key).map_err(GqlQueryError::Source),
                |event| meter.event(event),
            )
        })
    }

    /// Read captured relationship payloads and element names under the same meter as vertex payloads.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_governed_with_element_accessors<'a, E, C>(
        &self,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
        test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        property: impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        edge_property: impl FnMut(EId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        vertex_labels: impl FnMut(VId) -> Result<Option<&'a [crate::algebra::GraphValue]>, E>,
        edge_type: impl FnMut(EId) -> Result<Option<&'a CanonicalScalar>, E>,
        policy: GqlQueryPolicy,
        checkpoint: impl GovernedCheckpoint<C>,
    ) -> Result<GqlQueryExecution<Row>, GqlQueryError<E, C>> {
        if self.projects_property_maps() {
            return governed(snapshot_records, policy, checkpoint, |_| {
                Err(crate::GraphIntegerError {
                    instruction: 0,
                    kind: crate::GraphIntegerErrorKind::PropertyMapSourceRequired,
                }
                .into())
            });
        }
        self.execute_governed_with_element_maps(
            snapshot_records,
            vertices,
            edges,
            test_vertex,
            property,
            edge_property,
            vertex_labels,
            edge_type,
            |_| Ok(None),
            |_| Ok(None),
            policy,
            checkpoint,
        )
    }

    /// Project complete visible property maps under the same cumulative meter
    /// as scalar fields, path captures, catalog names and final result rows.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_governed_with_element_maps<'a, E, C>(
        &self,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
        mut test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        mut property: impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        mut edge_property: impl FnMut(EId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        mut vertex_labels: impl FnMut(VId) -> Result<Option<&'a [crate::algebra::GraphValue]>, E>,
        mut edge_type: impl FnMut(EId) -> Result<Option<&'a CanonicalScalar>, E>,
        mut vertex_properties: impl FnMut(VId) -> Result<Option<&'a crate::algebra::GraphValue>, E>,
        mut edge_properties: impl FnMut(EId) -> Result<Option<&'a crate::algebra::GraphValue>, E>,
        policy: GqlQueryPolicy,
        checkpoint: impl GovernedCheckpoint<C>,
    ) -> Result<GqlQueryExecution<Row>, GqlQueryError<E, C>> {
        governed(snapshot_records, policy, checkpoint, |meter| {
            if let Some(physical) =
                topology::compile(self.operators(), &mut |event| meter.event(event))?
            {
                return physical.execute(
                    self,
                    edges
                        .into_iter()
                        .map(|(_, source, relation, destination)| (source, relation, destination)),
                    &mut |event| meter.event(event),
                );
            }
            self.execute_with_element_maps_control(
                vertices,
                edges,
                |vid, predicates| test_vertex(vid, predicates).map_err(GqlQueryError::Source),
                |vid, key| property(vid, key).map_err(GqlQueryError::Source),
                |eid, key| edge_property(eid, key).map_err(GqlQueryError::Source),
                |vid| vertex_labels(vid).map_err(GqlQueryError::Source),
                |eid| edge_type(eid).map_err(GqlQueryError::Source),
                |vid| vertex_properties(vid).map_err(GqlQueryError::Source),
                |eid| edge_properties(eid).map_err(GqlQueryError::Source),
                |event| meter.event(event),
            )
        })
    }

    /// Read captured relationship payloads under the same meter as vertex payloads.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_governed_with_element_properties<'a, E, C>(
        &self,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
        test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        property: impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        edge_property: impl FnMut(EId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        policy: GqlQueryPolicy,
        checkpoint: impl GovernedCheckpoint<C>,
    ) -> Result<GqlQueryExecution<Row>, GqlQueryError<E, C>> {
        self.execute_governed_with_element_accessors(
            snapshot_records,
            vertices,
            edges,
            test_vertex,
            property,
            edge_property,
            |_| Ok(None),
            |_| Ok(None),
            policy,
            checkpoint,
        )
    }
}

impl<Row: GlaOutput> GlaPlan<Row> {
    /// Preserve source EIds for captured paths under the ordinary query meter.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_governed_with_identified_properties<'a, E, C>(
        &self,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
        mut test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        mut property: impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        policy: GqlQueryPolicy,
        checkpoint: impl GovernedCheckpoint<C>,
    ) -> Result<GqlQueryExecution<Row>, GqlQueryError<E, C>> {
        governed(snapshot_records, policy, checkpoint, |meter| {
            if self.projects_edge_properties() {
                return Err(GqlQueryError::IdentifiedEdgesRequired);
            }
            if let Some(physical) =
                topology::compile(self.operators(), &mut |event| meter.event(event))?
            {
                return physical.execute(
                    self,
                    edges
                        .into_iter()
                        .map(|(_, source, relation, destination)| (source, relation, destination)),
                    &mut |event| meter.event(event),
                );
            }
            self.execute_with_identified_properties_control(
                vertices,
                edges,
                |vid, predicates| test_vertex(vid, predicates).map_err(GqlQueryError::Source),
                |vid, key| property(vid, key).map_err(GqlQueryError::Source),
                |event| meter.event(event),
            )
        })
    }
}

fn charge_result_row(
    rows: &mut GqlExecutionStats,
    budget: GqlExecutionBudget,
    event: GlaExecutionEvent,
) -> Result<(), GqlBudgetExceeded> {
    if event == GlaExecutionEvent::ResultRow {
        let next = rows
            .result_rows
            .checked_add(1)
            .expect("an in-memory result cannot contain 2^64 rows");
        budget.check(GqlBudgetDimension::ResultRows, next)?;
        rows.result_rows = next;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RelationBind;
    use std::cell::Cell;

    fn plan(tail: &str) -> GlaPlan {
        GlaPlan::lower(
            &RelationBind::new()
                .with_relation("R", RelationId(1))
                .bind(&format!("MATCH (a)-[:R]->(b) RETURN b{tail}"))
                .unwrap(),
        )
    }
    fn edges() -> [(VId, RelationId, VId); 3] {
        [
            (VId(1), RelationId(1), VId(3)),
            (VId(1), RelationId(1), VId(2)),
            (VId(1), RelationId(1), VId(2)),
        ]
    }

    #[test]
    fn row_budget_refuses_at_first_excess_output_and_preserves_source_errors() {
        let edges = [2, 3, 4, 5].map(|id| (VId(1), RelationId(1), VId(id)));
        let result = plan("").execute_budgeted(
            4,
            [],
            edges,
            |_, _| Ok::<_, &str>(true),
            GqlExecutionBudget::result_rows(1),
        );
        assert!(matches!(
            result,
            Err(BudgetedGqlError::Budget(GqlBudgetExceeded {
                dimension: GqlBudgetDimension::ResultRows,
                limit: 1,
                observed: 2
            }))
        ));
        let predicates = GlaPlan::lower(
            &RelationBind::new()
                .with_relation("R", RelationId(1))
                .with_label("L", fgdb_delta_types::LabelId(1))
                .bind("MATCH (a)-[:R]->(b:L) RETURN b")
                .unwrap(),
        );
        let result = predicates.execute_budgeted(
            4,
            [],
            edges,
            |vid, _| {
                if vid == VId(5) {
                    Err("last predicate failed")
                } else {
                    Ok(true)
                }
            },
            GqlExecutionBudget::result_rows(0),
        );
        assert!(matches!(
            result,
            Err(BudgetedGqlError::Execution("last predicate failed"))
        ));
    }

    #[test]
    fn row_budget_admission_and_pagination_share_the_canonical_evaluator() {
        let consumed = Cell::new(0);
        let result = plan("").execute_budgeted(
            3,
            [],
            edges()
                .into_iter()
                .inspect(|_| consumed.set(consumed.get() + 1)),
            |_, _| Ok::<_, ()>(true),
            GqlExecutionBudget::snapshot_records(2),
        );
        assert!(matches!(
            result,
            Err(BudgetedGqlError::Budget(GqlBudgetExceeded {
                dimension: GqlBudgetDimension::SnapshotRecords,
                limit: 2,
                observed: 3
            }))
        ));
        assert_eq!(consumed.get(), 0, "admission precedes evaluator input");
        for (tail, expected) in [(" SKIP 1 LIMIT 1", vec![VId(3)]), (" SKIP 2", vec![])] {
            let result = plan(tail)
                .execute_budgeted(
                    3,
                    [],
                    edges(),
                    |_, _| Ok::<_, ()>(true),
                    GqlExecutionBudget::new(3, expected.len() as u64),
                )
                .unwrap();
            assert_eq!(result.value, expected);
            assert_eq!(
                result.stats,
                GqlExecutionStats {
                    snapshot_records: 3,
                    result_rows: expected.len() as u64
                }
            );
        }
        let mut zero = crate::RelationBind::new()
            .with_relation("R", RelationId(1))
            .bind("MATCH (a)-[:R]->(b) RETURN b")
            .unwrap();
        zero.limit = Some(0);
        let result = GlaPlan::lower(&zero)
            .execute_budgeted(
                3,
                [],
                edges(),
                |_, _| Ok::<_, ()>(true),
                GqlExecutionBudget::new(3, 0),
            )
            .unwrap();
        assert!(result.value.is_empty());
        assert_eq!(
            result.stats,
            GqlExecutionStats {
                snapshot_records: 3,
                result_rows: 0
            }
        );
    }

    #[test]
    fn all_four_dimensions_are_enforced_in_one_execution() {
        let plan = plan("");
        let run = |policy| {
            plan.execute_governed(
                3,
                [],
                edges(),
                |_, _| Ok::<_, ()>(true),
                policy,
                || Ok::<_, ()>(()),
            )
        };
        let wide = run(GqlQueryPolicy::new(3, 2, u64::MAX, u64::MAX)).unwrap();
        assert_eq!(wide.value, vec![VId(2), VId(3)]);
        assert_eq!(
            wide.rows,
            GqlExecutionStats {
                snapshot_records: 3,
                result_rows: 2
            }
        );
        let exact = GqlQueryPolicy::new(
            3,
            2,
            wide.evaluator.work_units,
            wide.evaluator.scratch_entries,
        );
        assert_eq!(run(exact).unwrap(), wide);
        for policy in [
            GqlQueryPolicy::new(2, 2, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(3, 1, u64::MAX, u64::MAX),
        ] {
            assert!(matches!(run(policy), Err(GqlQueryError::Rows(_))));
        }
        for policy in [
            GqlQueryPolicy::new(3, 2, exact.evaluator.max_work_units - 1, u64::MAX),
            GqlQueryPolicy::new(3, 2, u64::MAX, exact.evaluator.max_scratch_entries - 1),
        ] {
            assert!(matches!(run(policy), Err(GqlQueryError::Evaluator(_))));
        }
    }

    #[test]
    fn rejected_admission_never_consumes_the_edge_iterator() {
        let consumed = Cell::new(0);
        let input = edges()
            .into_iter()
            .inspect(|_| consumed.set(consumed.get() + 1));
        let error = plan("")
            .execute_governed(
                3,
                [],
                input,
                |_, _| Ok::<_, ()>(true),
                GqlQueryPolicy::new(2, 2, 100, 100),
                || Ok::<_, ()>(()),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            GqlQueryError::Rows(GqlBudgetExceeded {
                dimension: GqlBudgetDimension::SnapshotRecords,
                observed: 3,
                limit: 2
            })
        ));
        assert_eq!(consumed.get(), 0);
    }

    #[test]
    fn result_events_count_final_rows_and_can_refuse_the_copy_tail() {
        let returned = Cell::new(0);
        let error = plan("")
            .execute_with_control(
                [],
                edges(),
                |_, _| Ok(true),
                |event| {
                    if event == GlaExecutionEvent::ResultRow {
                        returned.set(returned.get() + 1);
                        if returned.get() == 2 {
                            return Err("cancel at final row copy");
                        }
                    }
                    Ok(())
                },
            )
            .unwrap_err();
        assert_eq!(error, "cancel at final row copy");
        assert_eq!(returned.get(), 2);
        let paged = plan(" SKIP 1 LIMIT 1")
            .execute_governed(
                3,
                [],
                edges(),
                |_, _| Ok::<_, ()>(true),
                GqlQueryPolicy::new(3, 1, 100, 100),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(paged.value, vec![VId(3)]);
        assert_eq!(paged.rows.result_rows, 1);
    }

    #[test]
    fn interruption_at_every_checkpoint_is_terminal_even_for_empty_results() {
        for tail in ["", " SKIP 99"] {
            let logical = plan(tail);
            let calls = Cell::new(0);
            logical
                .execute_governed(
                    3,
                    [],
                    edges(),
                    |_, _| Ok::<_, ()>(true),
                    GqlQueryPolicy::new(3, 2, 100, 100),
                    || {
                        calls.set(calls.get() + 1);
                        Ok::<_, usize>(())
                    },
                )
                .unwrap();
            for stop in 1..=calls.get() {
                let at = Cell::new(0);
                let result = logical.execute_governed(
                    3,
                    [],
                    edges(),
                    |_, _| Ok::<_, ()>(true),
                    GqlQueryPolicy::new(3, 2, 100, 100),
                    || {
                        at.set(at.get() + 1);
                        if at.get() == stop { Err(stop) } else { Ok(()) }
                    },
                );
                assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
                assert_eq!(at.get(), stop);
            }
        }
    }

    /// **A PATTERN'S EVALUATOR RUNS NEVER MOVE AN OUTCOME.** A two-hop
    /// pattern over a 36-vertex graph takes thousands of events, so runs open
    /// and close many times. A host that takes runs gets the per-event value,
    /// statistics and host total in far fewer calls, and at every sampled
    /// work, scratch, result-row and host limit the execution answers or
    /// refuses exactly as per-event charging does. The host charges a run in
    /// one call, refusing inside it at the exceeding unit as fgdb's signed
    /// meter does, and is never called again after refusing.
    #[test]
    fn pattern_runs_keep_every_value_and_refusal_of_per_event_charging() {
        struct Host<'u> {
            used: &'u Cell<u64>,
            calls: &'u Cell<u64>,
            budget: u64,
            refused: bool,
        }
        impl GovernedCheckpoint<u64> for Host<'_> {
            const RUNS: bool = true;
            fn unit(&mut self) -> Result<(), u64> {
                self.units(1)
            }
            fn units(&mut self, n: u64) -> Result<(), u64> {
                assert!(!self.refused, "called after refusing");
                self.calls.set(self.calls.get() + 1);
                if self.budget - self.used.get() < n {
                    self.used.set(self.budget);
                    self.refused = true;
                    return Err(self.budget + 1);
                }
                self.used.set(self.used.get() + n);
                Ok(())
            }
        }
        let plan: GlaPlan = GlaPlan::lower(
            &RelationBind::new()
                .with_relation("R", RelationId(1))
                .bind("MATCH (a)-[:R]->(b)-[:R]->(c) RETURN c")
                .unwrap(),
        );
        let edges: Vec<_> = (0..36_u128)
            .flat_map(|a| {
                (0..36_u128)
                    .filter(move |b| (a * 7 + b * 3) % 3 == 0)
                    .map(move |b| (VId(a), RelationId(1), VId(b)))
            })
            .collect();
        let runs = |policy: GqlQueryPolicy, budget: u64| {
            let (used, calls) = (Cell::new(0), Cell::new(0));
            let result = plan.execute_governed(
                edges.len() as u64,
                (0..36).map(VId),
                edges.iter().copied(),
                |_, _| Ok::<_, ()>(true),
                policy,
                Host {
                    used: &used,
                    calls: &calls,
                    budget,
                    refused: false,
                },
            );
            (result, used.get(), calls.get())
        };
        let per_event = |policy: GqlQueryPolicy, budget: u64| {
            let used = Cell::new(0);
            let result = plan.execute_governed(
                edges.len() as u64,
                (0..36).map(VId),
                edges.iter().copied(),
                |_, _| Ok::<_, ()>(true),
                policy,
                || {
                    if used.get() == budget {
                        return Err(budget + 1);
                    }
                    used.set(used.get() + 1);
                    Ok(())
                },
            );
            (result, used.get())
        };
        let wide = GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX);
        let (expected, host) = per_event(wide, u64::MAX);
        let expected = expected.unwrap();
        let (bulk, used, calls) = runs(wide, u64::MAX);
        assert_eq!(bulk.as_ref(), Ok(&expected));
        assert_eq!(used, host, "the per-unit host total");
        assert!(calls * 100 < used, "{calls} calls for {used} units");
        let (work, scratch, rows) = (
            expected.evaluator.work_units,
            expected.evaluator.scratch_entries,
            expected.rows.result_rows,
        );
        assert!(work > 4 * 1024, "{work} units must span several runs");
        let sampled = |total: u64| (0..=total).filter(move |at| at % 13 == 0 || total - at <= 40);
        for limit in sampled(work) {
            let policy = GqlQueryPolicy::new(u64::MAX, u64::MAX, limit, u64::MAX);
            assert_eq!(
                runs(policy, u64::MAX).0,
                per_event(policy, u64::MAX).0,
                "work limit {limit}"
            );
        }
        for limit in sampled(scratch) {
            let policy = GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, limit);
            assert_eq!(
                runs(policy, u64::MAX).0,
                per_event(policy, u64::MAX).0,
                "scratch limit {limit}"
            );
        }
        for limit in sampled(rows) {
            let policy = GqlQueryPolicy::new(u64::MAX, limit, u64::MAX, u64::MAX);
            assert_eq!(
                runs(policy, u64::MAX).0,
                per_event(policy, u64::MAX).0,
                "row limit {limit}"
            );
        }
        for budget in sampled(host) {
            let (result, used, _) = runs(wide, budget);
            assert_eq!(
                (result, used),
                per_event(wide, budget),
                "host budget {budget}"
            );
        }
    }
}
