//! Session-local vertex, fixed-hop and scoped aggregates maintained from deltas.
//! COUNT/SUM/AVG, their DISTINCT forms, and scalar/vertex MIN/MAX share atomic
//! tick publication. HAVING filters completed groups, not retained support.
//! This is not a durable subscription or delivery protocol.

mod boolean;
mod grouped;
mod edge;
use grouped::{AggregateKey, contributions};

use crate::{Database, ReadError, VertexRow};
use asupersync::fs::Vfs;
use fgdb_delta_types::zset::aggregate::IncrementalAggregate;
use fgdb_delta_types::{
    DeltaRow, ElementId, LabelId, LimbLimit, LogicalDeltaBatch, PropertyKeyId, ZSet, ZSetError,
    ZSetEvent,
};
use fgdb_gql::algebra::{
    GlaOperator, ValueProjection, VertexPredicate,
};
use fgdb_gql::{
    GqlQueryPolicy, GraphAggregateFunction, GraphAggregateRow,
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
    /// Distinct retained/new edge identities examined for a one-hop tick.
    /// Parallel edges count separately; a self-loop counts once.
    pub affected_edges: u64,
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
    NonIntegerHaving,
    /// Computed input column and value-independent scalar failure. A source
    /// binding identity or payload is never included in the diagnostic.
    InputExpression { column: usize, error: fgdb_gql::GraphIntegerError },
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
                f.write_str("standing query is outside the admitted COUNT/SUM/AVG/DISTINCT/MIN/MAX profile")
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
#[derive(Debug, Default, PartialEq)]
struct VertexState {
    labels: BTreeSet<LabelId>,
    props: BTreeMap<PropertyKeyId, CanonicalScalar>,
}

pub(crate) struct StandingQuery {
    definition: PreparedGraphAggregate,
    policy: GqlQueryPolicy,
    vertices: BTreeMap<VId, VertexState>,
    edges: Option<edge::State>,
    aggregate: IncrementalAggregate<AggregateKey>,
    rows: ZSet<GraphAggregateRow>,
    frontier: CommitSeq,
    stats: StandingQueryStats,
    failure: Option<StandingQueryFailure>,
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
    query.supports_incremental_maintenance_with_having()
        && aggregate_functions_eligible(query)
        && (eligible_flat_input(query) || edge::supports_scoped(query))
}
fn aggregate_functions_eligible(query: &PreparedGraphAggregate) -> bool {
    query.aggregates().iter().all(|aggregate| match aggregate.function() {
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
                query.incremental_input_column_type(column) == Some(fgdb_gql::GraphSetColumnType::Scalar)
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
            GlaOperator::ScanVertices | GlaOperator::ScanEdges { .. } if position == 0 => scans += 1,
            // Predicates, including Boolean/scalar programs, reuse GLA. Shape
            // admission still rejects pages and unsupported source operators.
            GlaOperator::Select { slot, predicates } if slot.ordinal() < width => {
                if !predicates.iter().all(|predicate| matches!(predicate,
                    VertexPredicate::HasLabel(_)
                    | VertexPredicate::IntegerProperty { .. }
                    | VertexPredicate::ScalarProperty { .. }
                    | VertexPredicate::PropertyNull { .. })) {
                    return false;
                }
            }
            GlaOperator::VertexIdentity { left, right, .. }
                if left.ordinal() < width && right.ordinal() < width => {}
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
    query.input_pattern().value_columns().iter().any(|column| matches!(column, ValueProjection::Property { key: actual, .. } if *actual == key))
        || query.input_pattern().plan().operators().iter().any(|op| match op {
            GlaOperator::Select { predicates, .. } => predicates.iter().any(|p| p.property_key() == Some(key)),
            // Operands need not be returned or appear in a unary predicate.
            // Retain and invalidate on BOTH sides of a binding-dependent test.
            GlaOperator::CompareProperties { left_key, right_key, .. } => *left_key == key || *right_key == key,
            GlaOperator::SelectBoolean { expression } => expression
                .referenced_vertex_properties().any(|(_, actual)| actual == key),
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
    fn maintain(
        &mut self,
        batch: &LogicalDeltaBatch,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        if batch.commit_seq() != self.frontier.checked_successor()
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
                meter.stats.delta_rows = meter.stats.delta_rows.checked_add(1)
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
                    let target = staged.get_mut(vid).ok_or(StandingQueryFailure::InvalidDelta)?;
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
                    vid, label, before, after,
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
                &self.definition, batch, &self.vertices, &staged, &mut updates, meter,
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
        self.integrate(updates, meter)?;
        // Aggregate and result publication succeeded; only owned map patches
        // remain. No fallible callback or arithmetic follows this boundary.
        for (vid, state) in staged {
            match state {
                Some(state) => { self.vertices.insert(vid, state); }
                None => { self.vertices.remove(&vid); }
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
        DeltaRow::CreateVertex { vid, .. }
        | DeltaRow::DeleteVertex { vid, .. } => Some(*vid),
        DeltaRow::LabelMembership { vid, label, .. }
            if needs_label(query, *label) => Some(*vid),
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
    /// Register a session-local maintained result against the current committed
    /// snapshot. Initialization and later explicit rebuilds share one admitted
    /// source path; ordinary commit maintenance never scans that source again.
    /// COUNT/DISTINCT and MIN/MAX admit scalar or vertex arguments; integer
    /// SUM/AVG and their DISTINCT forms admit scalar input columns, including
    /// computed ones. Each computed row uses the shared GQL projection evaluator
    /// before grouping, with checked arithmetic and ordinary lazy scalar branches.
    /// These functions reuse the admitted vertex, fixed-hop and optional/probe shapes.
    /// Boolean WHERE and its scalar programs use ordinary GLA semantics, with
    /// every hidden vertex-property dependency retained for invalidation.
    /// HAVING evaluates only completed changed groups; filtered-out groups keep
    /// their full support so later insertions/retractions can re-admit them.
    /// The result-row budget counts visible groups after HAVING, while work and
    /// scratch budgets still govern all maintenance, including rejected groups.
    pub fn register_standing_query(
        &mut self,
        cx: &QueryCx,
        definition: PreparedGraphAggregate,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        let query = self.prepare_standing_query(cx, definition, policy)?;
        let index = self.standing_queries.len();
        self.standing_queries.push(query);
        Ok(StandingQueryHandle {
            owner: Arc::clone(&self.handle_owner),
            index,
        })
    }

    /// Rebuild an existing standing query from the authoritative current
    /// snapshot, retaining the same handle and immutable prepared definition.
    /// A maintenance refusal does not roll back the already durable write;
    /// this method repairs the derived view after the cause is corrected or
    /// a larger policy is supplied. It is also valid for a healthy view.
    ///
    /// Preparation is private: any read, cancellation, budget or arithmetic
    /// refusal preserves the prior result, policy, failure and frontier. On
    /// success all are replaced together, then later commits resume ordinary
    /// incremental maintenance. This deliberately rebuilds the full admitted
    /// snapshot rather than skipping deltas or trusting a partial old state.
    /// No handle transfer across reopened databases, durable registration,
    /// delivery replay or automatic retry is implied.
    pub fn rebuild_standing_query(
        &mut self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
        policy: GqlQueryPolicy,
    ) -> Result<CommitSeq, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        if !Arc::ptr_eq(&self.handle_owner, &handle.owner) {
            return Err(StandingQueryError::ForeignHandle);
        }
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        let definition = self.standing_queries
            .get(handle.index)
            .ok_or(StandingQueryError::UnknownHandle)?
            .definition.clone();
        let replacement = self.prepare_standing_query(cx, definition, policy)?;
        let frontier = replacement.frontier;
        // No await, source mutation or fallible callback can interleave the
        // completed preparation and this one replacement under &mut self.
        self.standing_queries[handle.index] = replacement;
        Ok(frontier)
    }

    fn prepare_standing_query(
        &self,
        cx: &QueryCx,
        definition: PreparedGraphAggregate,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQuery, StandingQueryError> {
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
                frontier: self.snapshot.frontier,
                stats: StandingQueryStats::default(),
                failure: None,
            };
            let mut updates = Vec::new();
            for row in self.vertices().map_err(StandingQueryError::Read)? {
                let state = state_from(&query.definition, &row, &mut meter)
                    .map_err(StandingQueryError::Maintenance)?;
                if query.edges.is_none() {
                    contributions(&query.definition, row.vid, &state, 1, &mut updates, &mut meter)
                        .map_err(StandingQueryError::Maintenance)?;
                }
                query.vertices.insert(row.vid, state);
            }
            if let Some(edges) = &mut query.edges {
                for row in self.edges().map_err(StandingQueryError::Read)? {
                    edges.seed(
                        &query.definition, &row.entry, &query.vertices, &mut updates, &mut meter,
                    ).map_err(StandingQueryError::Maintenance)?;
                }
            }
            if let Some(edges) = &query.edges {
                edges.finish_seed(&query.definition, &query.vertices, &mut updates, &mut meter)
                    .map_err(StandingQueryError::Maintenance)?;
            }
            query
                .integrate(updates, &mut meter)
                .map_err(StandingQueryError::Maintenance)?;
            query.stats = meter.stats;
            // The built query is still private, including its aggregate and
            // result sink. Refuse cancellation before the public owner swaps.
            (meter.checkpoint)().map_err(StandingQueryError::Maintenance)?;
            Ok(query)
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
