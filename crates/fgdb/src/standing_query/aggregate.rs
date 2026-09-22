//! Session-local vertex, fixed-hop and scoped aggregates maintained from deltas.
//! COUNT/SUM/AVG, their DISTINCT forms, and scalar/vertex MIN/MAX share atomic
//! tick publication. HAVING filters completed groups, not retained support.
//! This is not a durable subscription or delivery protocol.

#[path = "boolean.rs"]
mod boolean;
#[path = "edge.rs"]
mod edge;
#[path = "grouped.rs"]
mod grouped;
use grouped::{AggregateKey, contributions};

use super::{Meter, StandingQueryError, StandingQueryFailure, StandingQueryStats, zset_error};
use crate::{Database, VertexRow};
use asupersync::fs::Vfs;
use fgdb_delta_types::zset::aggregate::IncrementalAggregate;
use fgdb_delta_types::{
    DeltaRow, ElementId, LabelId, LimbLimit, LogicalDeltaBatch, PropertyKeyId, ZSet, ZSetEvent,
};
use fgdb_gql::algebra::{GlaOperator, ValueProjection, VertexPredicate};
use fgdb_gql::{GqlQueryPolicy, GraphAggregateFunction, GraphAggregateRow, PreparedGraphAggregate};
use fgdb_types::{CanonicalScalar, CommitSeq, QueryCx, VId};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

// Only fields needed by the immutable definition are retained. Nonmatching
// vertices remain present so a later property/label transition can admit them.
#[derive(Debug, Default, PartialEq)]
struct VertexState {
    labels: BTreeSet<LabelId>,
    props: BTreeMap<PropertyKeyId, CanonicalScalar>,
}

pub(crate) struct StandingQuery {
    pub(super) definition: PreparedGraphAggregate,
    pub(super) policy: GqlQueryPolicy,
    vertices: BTreeMap<VId, VertexState>,
    edges: Option<edge::State>,
    aggregate: IncrementalAggregate<AggregateKey>,
    pub(super) rows: ZSet<GraphAggregateRow>,
    // Final public aggregate output, after HAVING/projection/DISTINCT/page.
    // Row sinks own a different row domain and retain their derivative there.
    pub(super) last_delta: Option<ZSet<GraphAggregateRow>>,
    pub(super) frontier: CommitSeq,
    pub(super) stats: StandingQueryStats,
    pub(super) failure: Option<StandingQueryFailure>,
}

impl core::fmt::Debug for StandingQuery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StandingQuery")
            .field("frontier", &self.frontier)
            .field("result_groups", &self.rows.len())
            .field("failure", &self.failure)
            .field("data", &"[REDACTED]")
            .finish()
    }
}

fn scalar_units(value: &CanonicalScalar) -> usize {
    let bytes = match value {
        CanonicalScalar::Bytes(value) => value.as_slice().len(),
        CanonicalScalar::Text(value) => value
            .len()
            .saturating_add(value.canonical_sort_key().map_or(0, <[u8]>::len)),
        CanonicalScalar::Timestamp(value) => value.zone().map_or(0, |zone| zone.identifier().len()),
        _ => 0,
    };
    1 + bytes.div_ceil(64)
}
fn eligible(query: &PreparedGraphAggregate) -> bool {
    // A complete relational schema is NOT permission to maintain only its
    // first graph leaf. Relational definitions need a dependency-owned source.
    query.input_relation().is_none()
        && query.supports_incremental_maintenance_with_having()
        && aggregate_functions_eligible(query)
        && (eligible_flat_input(query) || edge::supports_scoped(query))
}
fn aggregate_functions_eligible(query: &PreparedGraphAggregate) -> bool {
    query
        .aggregates()
        .iter()
        .all(|aggregate| match aggregate.function() {
            GraphAggregateFunction::CountRows => aggregate.argument_column().is_none(),
            GraphAggregateFunction::Count
            | GraphAggregateFunction::CountDistinct
            | GraphAggregateFunction::Min
            | GraphAggregateFunction::Max => aggregate.argument_column().is_some(),
            GraphAggregateFunction::SumInt
            | GraphAggregateFunction::SumIntDistinct
            | GraphAggregateFunction::AverageInt
            | GraphAggregateFunction::AverageIntDistinct => {
                aggregate.argument_column().is_some_and(|column| {
                    query.incremental_input_column_type(column)
                        == Some(fgdb_gql::GraphSetColumnType::Scalar)
                })
            }
            _ => false,
        })
}
fn eligible_flat_input(query: &PreparedGraphAggregate) -> bool {
    let operators = query.input_pattern().plan().operators();
    let width = match operators.first() {
        Some(GlaOperator::ScanVertices) => 1,
        Some(GlaOperator::ScanEdges { .. }) => 2,
        _ => return false,
    };
    let mut scans = 0;
    let mut projections = 0;
    for (position, op) in operators.iter().enumerate() {
        match op {
            GlaOperator::ScanVertices | GlaOperator::ScanEdges { .. } if position == 0 => {
                scans += 1
            }
            // Predicates, including Boolean/scalar programs, reuse GLA. Shape
            // admission still rejects pages and unsupported source operators.
            GlaOperator::Select { slot, predicates } if slot.ordinal() < width => {
                if !predicates.iter().all(|predicate| {
                    matches!(
                        predicate,
                        VertexPredicate::HasLabel(_)
                            | VertexPredicate::IntegerProperty { .. }
                            | VertexPredicate::ScalarProperty { .. }
                            | VertexPredicate::PropertyNull { .. }
                    )
                }) {
                    return false;
                }
            }
            GlaOperator::VertexIdentity {
                left,
                right,
                equal: _,
            } if left.ordinal() < width && right.ordinal() < width => {}
            GlaOperator::CompareProperties { left, right, .. }
                if left.ordinal() < width && right.ordinal() < width => {}
            GlaOperator::SelectBoolean { expression }
                if expression.supports_vertex_bindings(width as usize) => {}
            GlaOperator::ProjectValues { columns } => {
                projections += 1;
                if columns.iter().any(|column| !matches!(column,
                    ValueProjection::Property { slot, .. } | ValueProjection::Vertex { slot } if slot.ordinal() < width)) { return false; }
            }
            GlaOperator::OrderByValues
            | GlaOperator::Limit {
                offset: 0,
                count: None,
            } => {}
            _ => return false,
        }
    }
    scans == 1 && projections == 1
}
fn needs_property(query: &PreparedGraphAggregate, key: PropertyKeyId) -> bool {
    query.input_pattern().value_columns().iter().any(
        |column| matches!(column, ValueProjection::Property { key: actual, .. } if *actual == key),
    ) || query
        .input_pattern()
        .plan()
        .operators()
        .iter()
        .any(|op| match op {
            GlaOperator::Select { predicates, .. } => {
                predicates.iter().any(|p| p.property_key() == Some(key))
            }
            // Operands need not be returned or appear in a unary predicate.
            // Retain and invalidate on BOTH sides of a binding-dependent test.
            GlaOperator::CompareProperties {
                left_key,
                right_key,
                ..
            } => *left_key == key || *right_key == key,
            GlaOperator::SelectBoolean { expression } => expression
                .referenced_vertex_properties()
                .any(|(_, actual)| actual == key),
            _ => false,
        })
}
fn needs_label(query: &PreparedGraphAggregate, label: LabelId) -> bool {
    query.input_pattern().plan().operators().iter().any(|op| matches!(op, GlaOperator::Select { predicates, .. } if predicates.iter().any(|p| matches!(p, VertexPredicate::HasLabel(actual) if *actual == label))))
}
fn state_from(
    query: &PreparedGraphAggregate,
    row: &VertexRow,
    meter: &mut Meter<'_>,
) -> Result<VertexState, StandingQueryFailure> {
    let mut state = VertexState::default();
    meter.charge(ZSetEvent::ScratchEntry)?;
    for label in &row.labels {
        meter.charge(ZSetEvent::Work)?;
        if needs_label(query, *label) {
            meter.charge(ZSetEvent::ScratchEntry)?;
            state.labels.insert(*label);
        }
    }
    for (key, value) in &row.props {
        meter.charge(ZSetEvent::Work)?;
        if needs_property(query, *key) {
            meter.units(ZSetEvent::ScratchEntry, scalar_units(value))?;
            state.props.insert(*key, value.clone());
        }
    }
    Ok(state)
}
impl StandingQuery {
    pub(super) fn maintain(
        &mut self,
        batch: &LogicalDeltaBatch,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        self.maintain_with_output::<super::output::State>(batch, meter, None)
    }

    pub(super) fn maintain_with_output<D: super::sink::GroupSink>(
        &mut self,
        batch: &LogicalDeltaBatch,
        meter: &mut Meter<'_>,
        downstream: Option<&mut D>,
    ) -> Result<(), StandingQueryFailure> {
        if batch.commit_seq()
            != self
                .frontier
                .checked_successor()
                .map_err(|_| StandingQueryFailure::InvalidDelta)?
            || batch.frontier() != batch.commit_seq()
            || batch.commit_marker_identity().commit_seq != batch.commit_seq()
        {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        let mut affected = BTreeSet::new();
        for entry in batch.coordinate_entries() {
            meter.charge(ZSetEvent::Work)?;
            if entry.graph != crate::GRAPH || entry.branch != crate::BRANCH {
                continue;
            }
            if entry.schema_transition.is_some() {
                return Err(StandingQueryFailure::InvalidDelta);
            }
            for row in &entry.rows {
                meter.charge(ZSetEvent::Work)?;
                meter.stats.delta_rows = meter
                    .stats
                    .delta_rows
                    .checked_add(1)
                    .ok_or(StandingQueryFailure::WorkBudget)?;
                if matches!(row, DeltaRow::Schema { .. } | DeltaRow::Constraint { .. }) {
                    return Err(StandingQueryFailure::InvalidDelta);
                }
                if let Some(vid) = affected_vertex(&self.definition, row) {
                    if !affected.contains(&vid) {
                        meter.charge(ZSetEvent::ScratchEntry)?;
                        affected.insert(vid);
                    }
                }
            }
        }
        meter.stats.affected_vertices = affected.len() as u64;
        let mut updates = Vec::new();
        let mut staged = BTreeMap::new();
        for vid in &affected {
            meter.charge(ZSetEvent::Work)?;
            let next = if let Some(state) = self.vertices.get(vid) {
                if self.edges.is_none() {
                    contributions(&self.definition, *vid, state, -1, &mut updates, meter)?;
                }
                let mut next = VertexState::default();
                meter.charge(ZSetEvent::ScratchEntry)?;
                for label in &state.labels {
                    meter.charge(ZSetEvent::Work)?;
                    meter.charge(ZSetEvent::ScratchEntry)?;
                    next.labels.insert(*label);
                }
                for (key, value) in &state.props {
                    meter.charge(ZSetEvent::Work)?;
                    meter.units(ZSetEvent::ScratchEntry, scalar_units(value))?;
                    next.props.insert(*key, value.clone());
                }
                Some(next)
            } else {
                None
            };
            meter.charge(ZSetEvent::ScratchEntry)?;
            staged.insert(*vid, next);
        }
        for row in batch
            .coordinate_entries()
            .iter()
            .filter(|entry| entry.graph == crate::GRAPH && entry.branch == crate::BRANCH)
            .flat_map(|entry| &entry.rows)
        {
            meter.charge(ZSetEvent::Work)?;
            match row {
                DeltaRow::CreateVertex {
                    vid,
                    birth_ordinal,
                    labels,
                    props,
                    ..
                } => {
                    let target = staged
                        .get_mut(vid)
                        .ok_or(StandingQueryFailure::InvalidDelta)?;
                    if target.is_some() {
                        return Err(StandingQueryFailure::InvalidDelta);
                    }
                    // Borrow the delta fields directly; no full vertex/source clone.
                    let mut state = VertexState::default();
                    meter.charge(ZSetEvent::ScratchEntry)?;
                    let _ = birth_ordinal;
                    for label in labels {
                        meter.charge(ZSetEvent::Work)?;
                        if needs_label(&self.definition, *label) {
                            meter.charge(ZSetEvent::ScratchEntry)?;
                            state.labels.insert(*label);
                        }
                    }
                    for (key, value) in props {
                        meter.charge(ZSetEvent::Work)?;
                        if needs_property(&self.definition, *key) {
                            meter.units(ZSetEvent::ScratchEntry, scalar_units(value))?;
                            state.props.insert(*key, value.clone());
                        }
                    }
                    *target = Some(state);
                }
                DeltaRow::DeleteVertex { vid, .. } => {
                    staged
                        .get_mut(vid)
                        .and_then(Option::take)
                        .ok_or(StandingQueryFailure::InvalidDelta)?;
                }
                DeltaRow::LabelMembership {
                    vid,
                    label,
                    before,
                    after,
                } if needs_label(&self.definition, *label) => {
                    let state = staged
                        .get_mut(vid)
                        .and_then(Option::as_mut)
                        .ok_or(StandingQueryFailure::InvalidDelta)?;
                    if state.labels.contains(label) != *before {
                        return Err(StandingQueryFailure::InvalidDelta);
                    }
                    if *after {
                        meter.charge(ZSetEvent::ScratchEntry)?;
                        state.labels.insert(*label);
                    } else {
                        state.labels.remove(label);
                    }
                }
                DeltaRow::Property {
                    elem: ElementId::Vertex(vid),
                    property,
                    before,
                    after,
                    ..
                } if needs_property(&self.definition, *property) => {
                    let state = staged
                        .get_mut(vid)
                        .and_then(Option::as_mut)
                        .ok_or(StandingQueryFailure::InvalidDelta)?;
                    if state.props.get(property) != before.as_ref() {
                        return Err(StandingQueryFailure::InvalidDelta);
                    }
                    match after {
                        Some(value) => {
                            meter.units(ZSetEvent::ScratchEntry, scalar_units(value))?;
                            state.props.insert(*property, value.clone());
                        }
                        None => {
                            state.props.remove(property);
                        }
                    }
                }
                DeltaRow::Counter {
                    elem: ElementId::Vertex(_),
                    ..
                }
                | DeltaRow::Escrow {
                    subject: ElementId::Vertex(_),
                    ..
                } => return Err(StandingQueryFailure::InvalidDelta),
                _ => {}
            }
        }
        let edge_patch = if let Some(edges) = &self.edges {
            Some(edges.prepare(
                &self.definition,
                batch,
                &self.vertices,
                &staged,
                &mut updates,
                meter,
            )?)
        } else {
            for vid in &affected {
                meter.charge(ZSetEvent::Work)?;
                if let Some(state) = staged.get(vid).and_then(Option::as_ref) {
                    contributions(&self.definition, *vid, state, 1, &mut updates, meter)?;
                }
            }
            None
        };
        match downstream {
            Some(output) => self.integrate_with_output(updates, meter, Some(output))?,
            None => self.integrate(updates, meter)?,
        }
        // Aggregate and all downstream publication succeeded; only owned map
        // patches remain. No fallible callback or arithmetic follows here.
        for (vid, state) in staged {
            match state {
                Some(state) => {
                    self.vertices.insert(vid, state);
                }
                None => {
                    self.vertices.remove(&vid);
                }
            }
        }
        if let (Some(edges), Some(patch)) = (&mut self.edges, edge_patch) {
            edges.publish(patch);
        }
        Ok(())
    }
}
fn affected_vertex(query: &PreparedGraphAggregate, row: &DeltaRow) -> Option<VId> {
    match row {
        DeltaRow::CreateVertex { vid, .. } | DeltaRow::DeleteVertex { vid, .. } => Some(*vid),
        DeltaRow::LabelMembership { vid, label, .. } if needs_label(query, *label) => Some(*vid),
        DeltaRow::Property {
            elem: ElementId::Vertex(vid),
            property,
            ..
        } if needs_property(query, *property) => Some(*vid),
        DeltaRow::Counter {
            elem: ElementId::Vertex(vid),
            ..
        }
        | DeltaRow::Escrow {
            subject: ElementId::Vertex(vid),
            ..
        } => Some(*vid),
        _ => None,
    }
}
impl<V: Vfs + Clone> Database<V> {
    /// Borrow the most recent complete aggregate-output change. This covers
    /// plain and projected graph aggregates as well as relational group views.
    /// The derivative is after HAVING, projection, DISTINCT and result paging,
    /// not a change to private input counts or complete hidden groups.
    ///
    /// Initialization/rebuild returns None; an accepted unchanged successor
    /// returns Some(empty). Only the latest tick is retained, not a backlog.
    /// Integrate once only when its frontier follows the caller's baseline.
    /// Owner, health and freshness checks precede access; a failed view cannot
    /// expose an older successful tick as if it were current.
    pub fn standing_query_delta<'a>(
        &'a self,
        cx: &QueryCx,
        handle: &super::StandingQueryHandle,
    ) -> Result<Option<super::StandingQueryView<'a>>, StandingQueryError> {
        let source = match self.admitted_standing_query(cx, handle)? {
            super::StandingQuery::Aggregate(source)
            | super::StandingQuery::ProjectedAggregate { source, .. } => source,
            super::StandingQuery::Group(_) => return self.standing_group_delta(cx, handle),
            _ => return Err(StandingQueryError::Unsupported),
        };
        Ok(source
            .last_delta
            .as_ref()
            .map(|rows| super::StandingQueryView {
                rows,
                ordered: None,
                frontier: source.frontier,
                stats: &source.stats,
            }))
    }

    pub(super) fn prepare_standing_query(
        &self,
        cx: &QueryCx,
        definition: PreparedGraphAggregate,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQuery, StandingQueryError> {
        self.prepare_standing_query_with_output::<super::output::State>(
            cx, definition, policy, None,
        )
    }

    pub(super) fn prepare_standing_query_with_output<D: super::sink::GroupSink>(
        &self,
        cx: &QueryCx,
        definition: PreparedGraphAggregate,
        policy: GqlQueryPolicy,
        downstream: Option<&mut D>,
    ) -> Result<StandingQuery, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        if !eligible(&definition) {
            return Err(StandingQueryError::Unsupported);
        }
        let boxes = if downstream.is_some() { 2 } else { 1 };
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
            // Registration/rebuild may scan the snapshot. Admit and charge every
            // physical row/payload before merge_all_vertices can allocate it.
            let mut records = 0u64;
            for patch in &self.snapshot.patches {
                for row in patch.iter() {
                    records = records
                        .checked_add(1)
                        .ok_or(StandingQueryError::Maintenance(
                            StandingQueryFailure::SnapshotBudget,
                        ))?;
                    if policy
                        .rows
                        .max_snapshot_records()
                        .is_some_and(|limit| records > limit)
                    {
                        return Err(StandingQueryError::Maintenance(
                            StandingQueryFailure::SnapshotBudget,
                        ));
                    }
                    meter
                        .charge(ZSetEvent::Work)
                        .map_err(StandingQueryError::Maintenance)?;
                    meter
                        .units(ZSetEvent::ScratchEntry, 1 + row.labels.len())
                        .map_err(StandingQueryError::Maintenance)?;
                    for (_, value) in &row.props {
                        meter
                            .units(ZSetEvent::ScratchEntry, scalar_units(value))
                            .map_err(StandingQueryError::Maintenance)?;
                    }
                }
            }
            let edges = edge::State::for_definition(&definition);
            if edges.is_some() {
                edge::admit_snapshot(&self.snapshot, &mut records, &mut meter)
                    .map_err(StandingQueryError::Maintenance)?;
            }
            let mut query = StandingQuery {
                definition,
                policy,
                vertices: BTreeMap::new(),
                edges,
                aggregate: IncrementalAggregate::new(),
                rows: ZSet::new(),
                last_delta: None,
                frontier: self.snapshot.frontier,
                stats: StandingQueryStats::default(),
                failure: None,
            };
            let mut updates = Vec::new();
            for row in self.vertices().map_err(StandingQueryError::Read)? {
                let state = state_from(&query.definition, &row, &mut meter)
                    .map_err(StandingQueryError::Maintenance)?;
                if query.edges.is_none() {
                    contributions(
                        &query.definition,
                        row.vid,
                        &state,
                        1,
                        &mut updates,
                        &mut meter,
                    )
                    .map_err(StandingQueryError::Maintenance)?;
                }
                query.vertices.insert(row.vid, state);
            }
            if let Some(edges) = &mut query.edges {
                for row in self.edges().map_err(StandingQueryError::Read)? {
                    edges
                        .seed(
                            &query.definition,
                            &row.entry,
                            &query.vertices,
                            &mut updates,
                            &mut meter,
                        )
                        .map_err(StandingQueryError::Maintenance)?;
                }
            }
            if let Some(edges) = &mut query.edges {
                edges
                    .finish_seed(&query.definition, &query.vertices, &mut updates, &mut meter)
                    .map_err(StandingQueryError::Maintenance)?;
            }
            match downstream {
                Some(output) => query.integrate_with_output(updates, &mut meter, Some(output)),
                None => query.integrate(updates, &mut meter),
            }
            .map_err(StandingQueryError::Maintenance)?;
            // Both source and output are still private during registration.
            // Bootstrap is a baseline, never an ordinary successor derivative.
            query.last_delta = None;
            meter
                .units(ZSetEvent::ScratchEntry, boxes)
                .map_err(StandingQueryError::Maintenance)?;
            query.stats = meter.stats;
            (meter.checkpoint)().map_err(StandingQueryError::Maintenance)?;
            Ok(query)
        })
    }
}

#[cfg(test)]
#[path = "aggregate_delta_tests.rs"]
mod delta_tests;

#[cfg(test)]
mod relational_admission_tests {
    use super::*;
    use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder};
    use fgdb_gql::{GraphAggregate, GraphSetProjection, GraphSetQuantifier, GraphSetValue};

    #[test]
    fn relational_schema_admission_never_authorizes_first_leaf_maintenance() {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        let leaf = builder
            .prepare_values(&[GraphColumn::vertex("id", "n")], 0, None)
            .unwrap()
            .with_duplicates();
        let plain = PreparedGraphAggregate::prepare(
            leaf.clone(),
            &[],
            &[GraphAggregate::count_rows("n")],
            0,
            None,
        )
        .unwrap();
        assert!(eligible(&plain));
        let input = fgdb_gql::PreparedGraphSet::from(leaf)
            .project(
                vec![GraphSetProjection::new("id", GraphSetValue::Column(0))],
                GraphSetQuantifier::Distinct,
            )
            .unwrap();
        let relational = PreparedGraphAggregate::prepare_relation(
            input,
            &[],
            &[GraphAggregate::count_rows("n")],
            0,
            None,
        )
        .unwrap();
        assert!(relational.supports_incremental_maintenance_with_having());
        assert!(eligible_flat_input(&relational));
        assert!(!eligible(&relational));
    }
}
