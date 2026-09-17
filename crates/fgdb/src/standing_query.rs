//! Session-local global equality aggregates maintained from committed logical deltas.
//! This is not a durable subscription or a resumable delivery protocol.

use crate::{Database, ReadError, VertexRow};
use asupersync::fs::Vfs;
use fgdb_delta_types::zset::aggregate::{AggregateError, IncrementalAggregate};
use fgdb_delta_types::{
    DeltaRow, ElementId, LabelId, LimbLimit, LogicalDeltaBatch, PropertyKeyId, ZSet, ZSetError,
    ZSetEvent, ZWeight,
};
use fgdb_gql::algebra::{
    GlaOperator, GraphValue, IntegerComparison, ValueProjection, VertexPredicate,
};
use fgdb_gql::{
    GqlQueryPolicy, GraphAggregateFunction, GraphAggregateRow, GraphAggregateValue,
    PreparedGraphAggregate,
};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, QueryCx, VId};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct StandingQueryHandle {
    owner: Arc<()>,
    index: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StandingQueryStats {
    pub delta_rows: u64,
    pub affected_vertices: u64,
    pub work_units: u64,
    pub scratch_entries: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StandingQueryFailure {
    WorkBudget,
    ScratchBudget,
    SnapshotBudget,
    ResultBudget,
    Interrupted,
    Arithmetic,
    NonIntegerSum,
    InvalidDelta,
}

#[derive(Debug)]
pub enum StandingQueryError {
    ForeignHandle,
    UnknownHandle,
    Unsupported,
    Unavailable {
        frontier: CommitSeq,
        reason: StandingQueryFailure,
    },
    Read(ReadError),
    Interrupted(Box<asupersync::error::Error>),
    Maintenance(StandingQueryFailure),
}
impl core::fmt::Display for StandingQueryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ForeignHandle => f.write_str("standing query belongs to another opened database"),
            Self::UnknownHandle => f.write_str("unknown standing query"),
            Self::Unsupported => {
                f.write_str("standing query is outside the global equality COUNT/SUM profile")
            }
            Self::Unavailable { frontier, reason } => write!(
                f,
                "standing query unavailable after {frontier:?}: {reason:?}"
            ),
            Self::Read(error) => error.fmt(f),
            Self::Interrupted(error) => error.fmt(f),
            Self::Maintenance(reason) => {
                write!(f, "standing query initialization refused: {reason:?}")
            }
        }
    }
}
impl core::error::Error for StandingQueryError {}

#[derive(Debug)]
pub struct StandingQueryView<'a> {
    query: &'a StandingQuery,
}
impl StandingQueryView<'_> {
    pub fn frontier(&self) -> CommitSeq {
        self.query.frontier
    }
    pub fn rows(&self) -> &ZSet<GraphAggregateRow> {
        &self.query.rows
    }
    pub fn last_maintenance(&self) -> &StandingQueryStats {
        &self.query.stats
    }
}

// Only fields needed by the immutable definition are retained. Nonmatching
// vertices remain present so a later property/label transition can admit them.
#[derive(Debug, Default)]
struct VertexState {
    labels: BTreeSet<LabelId>,
    props: BTreeMap<PropertyKeyId, CanonicalScalar>,
}

#[derive(Debug)]
pub(crate) struct StandingQuery {
    definition: PreparedGraphAggregate,
    policy: GqlQueryPolicy,
    vertices: BTreeMap<VId, VertexState>,
    aggregate: IncrementalAggregate<usize>,
    rows: ZSet<GraphAggregateRow>,
    frontier: CommitSeq,
    stats: StandingQueryStats,
    failure: Option<StandingQueryFailure>,
}

struct Meter<'a> {
    policy: GqlQueryPolicy,
    stats: StandingQueryStats,
    checkpoint: &'a mut dyn FnMut() -> Result<(), StandingQueryFailure>,
}
impl Meter<'_> {
    fn charge(&mut self, event: ZSetEvent) -> Result<(), StandingQueryFailure> {
        (self.checkpoint)()?;
        let (counter, limit, error) = match event {
            ZSetEvent::Work => (
                &mut self.stats.work_units,
                self.policy.evaluator.max_work_units,
                StandingQueryFailure::WorkBudget,
            ),
            ZSetEvent::ScratchEntry => (
                &mut self.stats.scratch_entries,
                self.policy.evaluator.max_scratch_entries,
                StandingQueryFailure::ScratchBudget,
            ),
        };
        *counter = counter.checked_add(1).ok_or(error)?;
        if *counter > limit {
            return Err(error);
        }
        Ok(())
    }
    fn units(&mut self, event: ZSetEvent, units: usize) -> Result<(), StandingQueryFailure> {
        for _ in 0..units {
            self.charge(event)?;
        }
        Ok(())
    }
}
fn zset_error(error: ZSetError<StandingQueryFailure>) -> StandingQueryFailure {
    match error {
        ZSetError::Control(error) | ZSetError::Callback(error) => error,
        _ => StandingQueryFailure::Arithmetic,
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
    if !query.supports_incremental_maintenance() || !query.group_key_columns().is_empty() {
        return false;
    }
    let operators = query.input_pattern().plan().operators();
    let mut scans = 0;
    let mut equalities = 0;
    let mut projections = 0;
    for op in operators {
        match op {
            GlaOperator::ScanVertices => scans += 1,
            GlaOperator::Select { slot, predicates } if slot.ordinal() == 0 => {
                for predicate in predicates {
                    match predicate {
                        VertexPredicate::HasLabel(_) => {}
                        VertexPredicate::IntegerProperty {
                            comparison: IntegerComparison::Equal,
                            ..
                        } => equalities += 1,
                        VertexPredicate::ScalarProperty { predicate, .. }
                            if predicate.comparison() == IntegerComparison::Equal =>
                        {
                            equalities += 1
                        }
                        _ => return false,
                    }
                }
            }
            GlaOperator::ProjectValues { columns } => {
                projections += 1;
                if columns.iter().any(|column| !matches!(column,
                    ValueProjection::Property { slot, .. } | ValueProjection::Vertex { slot } if slot.ordinal() == 0)) { return false; }
            }
            GlaOperator::OrderByValues
            | GlaOperator::Limit {
                offset: 0,
                count: None,
            } => {}
            _ => return false,
        }
    }
    scans == 1
        && equalities == 1
        && projections == 1
        && query
            .aggregates()
            .iter()
            .all(|aggregate| match aggregate.function() {
                GraphAggregateFunction::CountRows => aggregate.argument_column().is_none(),
                GraphAggregateFunction::Count => aggregate.argument_column().is_some(),
                GraphAggregateFunction::SumInt => {
                    aggregate.argument_column().is_some_and(|column| {
                        matches!(
                            query.input_pattern().value_columns().get(column),
                            Some(ValueProjection::Property { .. })
                        )
                    })
                }
                _ => false,
            })
}
fn needs_property(query: &PreparedGraphAggregate, key: PropertyKeyId) -> bool {
    query.input_pattern().value_columns().iter().any(|column| matches!(column, ValueProjection::Property { key: actual, .. } if *actual == key))
        || query.input_pattern().plan().operators().iter().any(|op| matches!(op, GlaOperator::Select { predicates, .. } if predicates.iter().any(|p| p.property_key() == Some(key))))
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
fn contributions(
    query: &PreparedGraphAggregate,
    state: &VertexState,
    sign: i128,
    output: &mut Vec<((usize, Option<i128>), ZWeight)>,
    meter: &mut Meter<'_>,
) -> Result<(), StandingQueryFailure> {
    for op in query.input_pattern().plan().operators() {
        if let GlaOperator::Select { predicates, .. } = op {
            for predicate in predicates {
                meter.units(ZSetEvent::Work, 1 + predicate.comparison_work_units())?;
                if !predicate.matches_borrowed(
                    state.labels.iter().copied(),
                    state.props.iter().map(|(k, v)| (*k, v)),
                ) {
                    return Ok(());
                }
            }
        }
    }
    for (index, aggregate) in query.aggregates().iter().enumerate() {
        meter.charge(ZSetEvent::Work)?;
        let value = match aggregate
            .argument_column()
            .map(|column| query.input_pattern().value_columns()[column])
        {
            None | Some(ValueProjection::Vertex { .. }) => Some(0),
            Some(ValueProjection::Property { key, .. }) => match state.props.get(&key) {
                None | Some(CanonicalScalar::Null) => None,
                Some(CanonicalScalar::Int(value))
                    if aggregate.function() == GraphAggregateFunction::SumInt =>
                {
                    Some(i128::from(*value))
                }
                Some(_) if aggregate.function() == GraphAggregateFunction::Count => Some(0),
                _ => return Err(StandingQueryFailure::NonIntegerSum),
            },
            _ => return Err(StandingQueryFailure::InvalidDelta),
        };
        meter.charge(ZSetEvent::ScratchEntry)?;
        output.push(((index, value), ZWeight::from_i128(sign)));
    }
    Ok(())
}
impl StandingQuery {
    fn integrate(
        &mut self,
        updates: Vec<((usize, Option<i128>), ZWeight)>,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        // GQL output is bounded by u64 counts/i128 sums. Four limbs also admit
        // an intermediate exact sum across one signed input tick; promotion is
        // never allowed to become an unaccounted, unbounded allocation.
        let limbs = LimbLimit::new(4);
        let delta = ZSet::from_updates(updates, limbs, &mut |event| meter.charge(event))
            .map_err(zset_error)?;
        self.aggregate
            .apply(&delta, limbs, &mut |event| meter.charge(event))
            .map_err(|error| match error {
                AggregateError::ZSet(error) => zset_error(error),
                AggregateError::NegativeMultiplicity => StandingQueryFailure::InvalidDelta,
            })?;
        if self
            .policy
            .rows
            .max_result_rows()
            .is_some_and(|limit| limit < 1)
        {
            return Err(StandingQueryFailure::ResultBudget);
        }
        let mut values = Vec::new();
        for (index, spec) in self.definition.aggregates().iter().enumerate() {
            meter.charge(ZSetEvent::Work)?;
            meter.charge(ZSetEvent::ScratchEntry)?;
            let summary = self.aggregate.get(&index);
            let value = match spec.function() {
                GraphAggregateFunction::CountRows | GraphAggregateFunction::Count => {
                    let count = match summary {
                        None => 0,
                        Some(summary) => {
                            let weight = if spec.function() == GraphAggregateFunction::CountRows {
                                summary.count_rows()
                            } else {
                                summary.count_values()
                            };
                            u64::try_from(weight.to_i128().ok_or(StandingQueryFailure::Arithmetic)?)
                                .map_err(|_| StandingQueryFailure::Arithmetic)?
                        }
                    };
                    GraphAggregateValue::Count(count)
                }
                GraphAggregateFunction::SumInt => match summary.and_then(|summary| summary.sum()) {
                    None => GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
                    Some(sum) => GraphAggregateValue::Integer(
                        sum.to_i128().ok_or(StandingQueryFailure::Arithmetic)?,
                    ),
                },
                _ => return Err(StandingQueryFailure::InvalidDelta),
            };
            values.push(value);
        }
        let row = self
            .definition
            .incremental_global_row(values)
            .ok_or(StandingQueryFailure::InvalidDelta)?;
        let rows = ZSet::from_updates([(row, ZWeight::ONE)], limbs, &mut |event| {
            meter.charge(event)
        })
        .map_err(zset_error)?;
        (meter.checkpoint)()?;
        self.rows = rows;
        Ok(())
    }

    fn maintain(
        &mut self,
        batch: &LogicalDeltaBatch,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        let mut affected = BTreeSet::new();
        for row in batch
            .coordinate_entries()
            .iter()
            .flat_map(|entry| &entry.rows)
        {
            meter.charge(ZSetEvent::Work)?;
            meter.stats.delta_rows += 1;
            if let Some(vid) = affected_vertex(row) {
                if !affected.contains(&vid) {
                    meter.charge(ZSetEvent::ScratchEntry)?;
                    affected.insert(vid);
                }
            }
        }
        meter.stats.affected_vertices = affected.len() as u64;
        let mut updates = Vec::new();
        for vid in &affected {
            meter.charge(ZSetEvent::Work)?;
            if let Some(state) = self.vertices.get(vid) {
                contributions(&self.definition, state, -1, &mut updates, meter)?;
            }
        }
        for row in batch
            .coordinate_entries()
            .iter()
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
                    if self.vertices.contains_key(vid) {
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
                    self.vertices.insert(*vid, state);
                }
                DeltaRow::DeleteVertex { vid, .. } => {
                    self.vertices
                        .remove(vid)
                        .ok_or(StandingQueryFailure::InvalidDelta)?;
                }
                DeltaRow::LabelMembership {
                    vid, label, after, ..
                } if needs_label(&self.definition, *label) => {
                    let state = self
                        .vertices
                        .get_mut(vid)
                        .ok_or(StandingQueryFailure::InvalidDelta)?;
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
                    after,
                    ..
                } if needs_property(&self.definition, *property) => {
                    let state = self
                        .vertices
                        .get_mut(vid)
                        .ok_or(StandingQueryFailure::InvalidDelta)?;
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
        for vid in &affected {
            meter.charge(ZSetEvent::Work)?;
            if let Some(state) = self.vertices.get(vid) {
                contributions(&self.definition, state, 1, &mut updates, meter)?;
            }
        }
        self.integrate(updates, meter)
    }
}
fn affected_vertex(row: &DeltaRow) -> Option<VId> {
    match row {
        DeltaRow::CreateVertex { vid, .. }
        | DeltaRow::DeleteVertex { vid, .. }
        | DeltaRow::LabelMembership { vid, .. } => Some(*vid),
        DeltaRow::Property {
            elem: ElementId::Vertex(vid),
            ..
        }
        | DeltaRow::ValidTime {
            elem: ElementId::Vertex(vid),
            ..
        }
        | DeltaRow::Counter {
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
    pub fn register_standing_query(
        &mut self,
        cx: &QueryCx,
        definition: PreparedGraphAggregate,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        if !eligible(&definition) {
            return Err(StandingQueryError::Unsupported);
        }
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
            // Registration alone may scan the snapshot. Admit and charge every
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
            let mut query = StandingQuery {
                definition,
                policy,
                vertices: BTreeMap::new(),
                aggregate: IncrementalAggregate::new(),
                rows: ZSet::new(),
                frontier: self.snapshot.frontier,
                stats: StandingQueryStats::default(),
                failure: None,
            };
            let mut updates = Vec::new();
            for row in self.vertices().map_err(StandingQueryError::Read)? {
                let state = state_from(&query.definition, &row, &mut meter)
                    .map_err(StandingQueryError::Maintenance)?;
                contributions(&query.definition, &state, 1, &mut updates, &mut meter)
                    .map_err(StandingQueryError::Maintenance)?;
                query.vertices.insert(row.vid, state);
            }
            query
                .integrate(updates, &mut meter)
                .map_err(StandingQueryError::Maintenance)?;
            query.stats = meter.stats;
            let index = self.standing_queries.len();
            self.standing_queries.push(query);
            Ok(StandingQueryHandle {
                owner: Arc::clone(&self.handle_owner),
                index,
            })
        })
    }

    pub fn standing_query<'a>(
        &'a self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
    ) -> Result<StandingQueryView<'a>, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        if !Arc::ptr_eq(&self.handle_owner, &handle.owner) {
            return Err(StandingQueryError::ForeignHandle);
        }
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        let query = self
            .standing_queries
            .get(handle.index)
            .ok_or(StandingQueryError::UnknownHandle)?;
        if let Some(reason) = query.failure {
            return Err(StandingQueryError::Unavailable {
                frontier: query.frontier,
                reason,
            });
        }
        Ok(StandingQueryView { query })
    }
}

pub(crate) fn publish(queries: &mut [StandingQuery], cx: &CommitCx, batch: &LogicalDeltaBatch) {
    for query in queries {
        if query.failure.is_some() {
            continue;
        }
        let mut checkpoint = || {
            cx.checkpoint()
                .map_err(|_| StandingQueryFailure::Interrupted)
        };
        let mut meter = Meter {
            policy: query.policy,
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        match query.maintain(batch, &mut meter) {
            Ok(()) => query.frontier = batch.commit_seq(),
            Err(reason) => query.failure = Some(reason),
        }
        query.stats = meter.stats;
    }
}
