//! A relationship pattern with newly introduced vertex bindings is one MERGE
//! unit. Ordinary positive GLA matching resolves the complete pattern first;
//! the native insertion collector creates every new endpoint only when that
//! complete relation is empty. A matching isolated vertex is never an upsert.

use super::{
    GraphEdgeMergeBuildError, GraphEdgeMergeError, GraphEdgeMergeOutcome, PreparedGraphEdgeMerge,
};
use crate::algebra::{
    GlaDirection, GraphColumn, GraphPathFunction, GraphPatternBuilder, GraphValue, GraphValueRow,
    IntegerComparison, VertexPredicate,
};
use crate::insertion::{
    GraphInsertBatch, GraphInsertEdge, GraphInsertEndpoint, GraphInsertError, GraphInsertIntent,
    GraphInsertLimitDimension, GraphInsertPolicy, GraphInsertRequest, GraphInsertStats,
    GraphInsertVertex, PreparedGraphInsert,
};
use crate::vertex_merge::GraphVertexMergeRowValue;
use crate::{
    GlaExecutionEvent, GlaExecutionStats, GlaLimitDimension, GlaLimitExceeded, GqlBudgetDimension,
    GqlBudgetExceeded, GqlExecutionStats, GqlQueryError, GqlQueryPolicy, GraphElementKey,
    GraphMutationValue, GraphSetOperand, GraphSetOperation, GraphSetPredicateOp,
    GraphSetProjection, GraphSetQuantifier, GraphSetSource, GraphSetValue, PreparedGraphSet,
};
use fgdb_delta_types::{ElementId, PropertyKeyId};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, VId};

type Fault<E, A, C> = GqlQueryError<GraphEdgeMergeError<E, A>, C>;
type Fields = Vec<(PropertyKeyId, CanonicalPropertyValue)>;

/// One complete matched or created relationship pattern. `endpoints` are the
/// relationship's stored orientation; the input slots retain pattern orientation.
pub struct GraphEdgeWholeMergeRow {
    pub input: GraphValueRow,
    pub outcome: GraphEdgeMergeOutcome,
    pub endpoints: (VId, VId),
}

/// Private native staging proposal. Nothing has been written or published.
pub struct GraphEdgeWholeMergeProposal {
    pub stats: GraphInsertStats,
    pub rows: Vec<GraphEdgeWholeMergeRow>,
    pub creation: Option<GraphInsertBatch>,
}

impl core::fmt::Debug for GraphEdgeWholeMergeRow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphEdgeWholeMergeRow([REDACTED])")
    }
}
impl core::fmt::Debug for GraphEdgeWholeMergeProposal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphEdgeWholeMergeProposal")
            .field("stats", &self.stats)
            .field("rows", &self.rows.len())
            .field("creation", &self.creation.is_some())
            .finish()
    }
}

struct Meter<F> {
    policy: GraphInsertPolicy,
    stats: GraphInsertStats,
    checkpoint: F,
}

fn invalid<E, A, C>() -> Fault<E, A, C> {
    GqlQueryError::Source(GraphEdgeMergeError::InvalidSourceStatistics)
}

impl<F> Meter<F> {
    fn event<E, A, C>(&mut self, kind: GlaExecutionEvent) -> Result<(), Fault<E, A, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        (self.checkpoint)().map_err(GqlQueryError::Interrupted)?;
        let work = u128::from(self.stats.evaluator.work_units) + 1;
        let scratch = u128::from(self.stats.evaluator.scratch_entries)
            + u128::from(kind == GlaExecutionEvent::ScratchEntry);
        for (dimension, limit, observed) in [
            (
                GlaLimitDimension::WorkUnits,
                self.policy.query.evaluator.max_work_units,
                work,
            ),
            (
                GlaLimitDimension::ScratchEntries,
                self.policy.query.evaluator.max_scratch_entries,
                scratch,
            ),
        ] {
            if observed > u128::from(limit) {
                return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                    dimension,
                    limit,
                    observed,
                }));
            }
        }
        self.stats.evaluator = GlaExecutionStats {
            work_units: work as u64,
            scratch_entries: scratch as u64,
        };
        Ok(())
    }

    fn remaining<E, A, C>(&self) -> Result<GqlQueryPolicy, Fault<E, A, C>> {
        Ok(GqlQueryPolicy::new(
            self.policy
                .query
                .rows
                .max_snapshot_records()
                .unwrap_or(u64::MAX)
                .checked_sub(self.stats.selection.snapshot_records)
                .ok_or_else(invalid)?,
            u64::MAX,
            self.policy
                .query
                .evaluator
                .max_work_units
                .checked_sub(self.stats.evaluator.work_units)
                .ok_or_else(invalid)?,
            self.policy
                .query
                .evaluator
                .max_scratch_entries
                .checked_sub(self.stats.evaluator.scratch_entries)
                .ok_or_else(invalid)?,
        ))
    }

    fn absorb<E, A, C>(
        &mut self,
        rows: GqlExecutionStats,
        evaluator: GlaExecutionStats,
    ) -> Result<(), Fault<E, A, C>> {
        let records = self
            .stats
            .selection
            .snapshot_records
            .checked_add(rows.snapshot_records)
            .ok_or_else(invalid)?;
        self.policy
            .query
            .rows
            .check(GqlBudgetDimension::SnapshotRecords, records)
            .map_err(GqlQueryError::Rows)?;
        for (dimension, used, added, limit) in [
            (
                GlaLimitDimension::WorkUnits,
                self.stats.evaluator.work_units,
                evaluator.work_units,
                self.policy.query.evaluator.max_work_units,
            ),
            (
                GlaLimitDimension::ScratchEntries,
                self.stats.evaluator.scratch_entries,
                evaluator.scratch_entries,
                self.policy.query.evaluator.max_scratch_entries,
            ),
        ] {
            let observed = u128::from(used) + u128::from(added);
            if observed > u128::from(limit) {
                return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                    dimension,
                    limit,
                    observed,
                }));
            }
        }
        self.stats.selection.snapshot_records = records;
        self.stats.evaluator.work_units += evaluator.work_units;
        self.stats.evaluator.scratch_entries += evaluator.scratch_entries;
        Ok(())
    }

    fn rebase<E, A, C>(&self, error: Fault<E, A, C>) -> Fault<E, A, C> {
        match error {
            GqlQueryError::Rows(error)
                if error.dimension == GqlBudgetDimension::SnapshotRecords =>
            {
                match error
                    .observed
                    .checked_add(self.stats.selection.snapshot_records)
                {
                    Some(observed) => GqlQueryError::Rows(GqlBudgetExceeded {
                        dimension: error.dimension,
                        limit: self
                            .policy
                            .query
                            .rows
                            .max_snapshot_records()
                            .unwrap_or(u64::MAX),
                        observed,
                    }),
                    None => invalid(),
                }
            }
            GqlQueryError::Evaluator(error) => {
                let (used, limit) = match error.dimension {
                    GlaLimitDimension::WorkUnits => (
                        self.stats.evaluator.work_units,
                        self.policy.query.evaluator.max_work_units,
                    ),
                    GlaLimitDimension::ScratchEntries => (
                        self.stats.evaluator.scratch_entries,
                        self.policy.query.evaluator.max_scratch_entries,
                    ),
                };
                GqlQueryError::Evaluator(GlaLimitExceeded {
                    dimension: error.dimension,
                    limit,
                    observed: error.observed.saturating_add(u128::from(used)),
                })
            }
            error => error,
        }
    }
}

fn build_fault<E, A, C>(error: GraphEdgeMergeBuildError) -> Fault<E, A, C> {
    GqlQueryError::Source(GraphEdgeMergeError::WholePattern(error))
}

fn scalar_copy<C>(
    value: &CanonicalScalar,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
) -> Result<CanonicalPropertyValue, C> {
    crate::algebra_exec::charge_payload(value, &mut |_| {
        control(GlaExecutionEvent::Work)?;
        control(GlaExecutionEvent::ScratchEntry)
    })?;
    Ok(CanonicalPropertyValue::Scalar(value.clone()))
}

fn written_value<C>(
    value: &GraphValue,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
) -> Result<Option<CanonicalPropertyValue>, C> {
    match value {
        GraphValue::Scalar(CanonicalScalar::Null) => Ok(None),
        GraphValue::Scalar(value) => scalar_copy(value, control).map(Some),
        _ => crate::vertex_merge::written_merge_value(value, control),
    }
}

fn properties<E, A, C>(
    fields: &[(PropertyKeyId, GraphVertexMergeRowValue)],
    at: usize,
    row: &GraphValueRow,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), Fault<E, A, C>>,
) -> Result<Fields, Fault<E, A, C>> {
    let mut result = Vec::new();
    for (property, (key, value)) in fields.iter().enumerate() {
        control(GlaExecutionEvent::ScratchEntry)?;
        let invalid = || GqlQueryError::Source(GraphEdgeMergeError::RowValue { row: at, property });
        let value = match value {
            GraphVertexMergeRowValue::Bound(value) => Some(scalar_copy(value.value(), control)?),
            GraphVertexMergeRowValue::Column(column) => {
                written_value(row.values().get(*column).ok_or_else(invalid)?, control)?
            }
            GraphVertexMergeRowValue::Computed(value) => {
                let value = crate::set_ops::evaluate_value(value, row, property, control).map_err(
                    |error| match error {
                        crate::set_ops::ProjectionFailure::Control(error) => error,
                        crate::set_ops::ProjectionFailure::Arithmetic { error, .. } => {
                            GqlQueryError::Source(GraphEdgeMergeError::RowExpression {
                                row: at,
                                property,
                                source: error,
                            })
                        }
                    },
                )?;
                written_value(&value, control)?
            }
        }
        .ok_or_else(invalid)?;
        if matches!(value, CanonicalPropertyValue::Scalar(CanonicalScalar::Null)) {
            return Err(invalid());
        }
        result.push((*key, value));
    }
    Ok(result)
}

/// The five identity columns are pattern start/end, relationship, physical
/// source/destination. Keeping physical orientation in the native projection
/// avoids a second ungoverned point lookup for undirected RETURN functions.
fn pattern<E, A, C>(
    merge: &PreparedGraphEdgeMerge,
    reverse: bool,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), Fault<E, A, C>>,
) -> Result<PreparedGraphSet, Fault<E, A, C>> {
    let build = |error| build_fault(GraphEdgeMergeBuildError::Pattern(error));
    let mut pattern = GraphPatternBuilder::new();
    control(GlaExecutionEvent::ScratchEntry)?;
    pattern.vertex("a").map_err(build)?;
    let destination = if merge.source == merge.destination {
        "a"
    } else {
        control(GlaExecutionEvent::ScratchEntry)?;
        pattern.vertex("b").map_err(build)?;
        "b"
    };
    for endpoint in &merge.unbound_endpoints {
        let variable = if endpoint.column == merge.source {
            "a"
        } else {
            destination
        };
        for label in &endpoint.labels {
            control(GlaExecutionEvent::ScratchEntry)?;
            pattern
                .filter(variable, VertexPredicate::HasLabel(*label))
                .map_err(build)?;
        }
    }
    control(GlaExecutionEvent::ScratchEntry)?;
    pattern
        .edge(
            "a",
            merge.relation,
            if reverse {
                GlaDirection::Reverse
            } else {
                GlaDirection::Forward
            },
            destination,
        )
        .map_err(build)?;
    control(GlaExecutionEvent::ScratchEntry)?;
    pattern.capture_edge("r", 0).map_err(build)?;
    let (physical_source, physical_destination) = if reverse {
        (destination, "a")
    } else {
        ("a", destination)
    };
    for _ in 0..5 {
        control(GlaExecutionEvent::ScratchEntry)?;
    }
    Ok(pattern
        .prepare_values(
            &[
                GraphColumn::vertex("a", "a"),
                GraphColumn::vertex("b", destination),
                GraphColumn::path("r", "r", GraphPathFunction::Edge),
                GraphColumn::vertex("src", physical_source),
                GraphColumn::vertex("dst", physical_destination),
            ],
            0,
            None,
        )
        .map_err(build)?
        .into())
}

fn base_projection<E>(
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Vec<GraphSetProjection>, E> {
    let mut projection = Vec::new();
    for (column, name) in ["a", "b", "r", "src", "dst"].into_iter().enumerate() {
        control(GlaExecutionEvent::ScratchEntry)?;
        projection.push(GraphSetProjection::new(name, GraphSetValue::Column(column)));
    }
    Ok(projection)
}

fn equal(left: GraphSetValue, right: GraphSetValue) -> GraphSetValue {
    GraphSetValue::Equal {
        left: Box::new(left),
        right: Box::new(right),
    }
}

fn matching<E, A, C>(
    merge: &PreparedGraphEdgeMerge,
    bound: [Option<VId>; 2],
    endpoints: &[(usize, Fields)],
    edge: &Fields,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), Fault<E, A, C>>,
) -> Result<PreparedGraphSet, Fault<E, A, C>> {
    control(GlaExecutionEvent::ScratchEntry)?;
    let mut selected = pattern(merge, false, control)?;
    if merge.undirected {
        control(GlaExecutionEvent::ScratchEntry)?;
        selected = selected
            .combine(
                GraphSetOperation::Union,
                GraphSetQuantifier::Distinct,
                pattern(merge, true, control)?,
            )
            .map_err(|error| build_fault(GraphEdgeMergeBuildError::RelationalInput(error)))?;
    }
    let mut checks = Vec::new();
    for (column, vertex) in bound.into_iter().enumerate() {
        if let Some(vertex) = vertex {
            for _ in 0..3 {
                control(GlaExecutionEvent::ScratchEntry)?;
            }
            checks.push(equal(
                GraphSetValue::Column(column),
                GraphSetValue::Value(GraphValue::Vertex(vertex)),
            ));
        }
    }
    // Establish the carried identity correlation before reading endpoint or
    // relationship map fields. An unrelated edge's payload is outside this
    // occurrence, even though the ordinary native source admits the relation.
    selected = filter_checks(selected, checks, control)?;
    let mut checks = Vec::new();
    for (column, fields) in endpoints
        .iter()
        .map(|(column, fields)| (if *column == merge.source { 0 } else { 1 }, fields))
        .chain(std::iter::once((2, edge)))
    {
        for (key, value) in fields {
            for _ in 0..4 {
                control(GlaExecutionEvent::ScratchEntry)?;
            }
            let value = GraphValue::admit_property_value(value, control)?;
            checks.push(equal(
                GraphSetValue::MapGet {
                    map: Box::new(GraphSetValue::Column(column)),
                    key: "".into(),
                    property: GraphElementKey::Property(*key),
                },
                GraphSetValue::Value(value),
            ));
        }
    }
    filter_checks(selected, checks, control)
}

fn filter_checks<E, A, C>(
    mut selected: PreparedGraphSet,
    checks: Vec<GraphSetValue>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), Fault<E, A, C>>,
) -> Result<PreparedGraphSet, Fault<E, A, C>> {
    // The property cap is per element, while the relation's column cap is 65.
    // Repeated native projections/filtering retain only the five IDs between
    // chunks; every accepted field therefore participates without widening the
    // public row or weakening either limit.
    let mut checks = checks.into_iter().peekable();
    while checks.peek().is_some() {
        let mut projection = base_projection(control)?;
        let mut filter = Vec::new();
        for (at, check) in checks
            .by_ref()
            .take(crate::algebra::MAX_PATTERN_VERTICES - 5)
            .enumerate()
        {
            control(GlaExecutionEvent::ScratchEntry)?;
            projection.push(GraphSetProjection::new(format!("check{at}"), check));
            control(GlaExecutionEvent::ScratchEntry)?;
            filter.push(GraphSetPredicateOp::Compare {
                left: GraphSetOperand::Column(at + 5),
                comparison: IntegerComparison::Equal,
                right: GraphSetOperand::Literal(
                    crate::GqlScalarParameter::new(CanonicalScalar::Bool(true))
                        .expect("Boolean literal is bounded"),
                ),
            });
            if at != 0 {
                control(GlaExecutionEvent::ScratchEntry)?;
                filter.push(GraphSetPredicateOp::And);
            }
        }
        selected = selected
            .project(projection, GraphSetQuantifier::All)
            .map_err(|error| build_fault(GraphEdgeMergeBuildError::Projection(error)))?
            .filter(&filter)
            .map_err(|error| build_fault(GraphEdgeMergeBuildError::Filter(error)))?
            .project(base_projection(control)?, GraphSetQuantifier::All)
            .map_err(|error| build_fault(GraphEdgeMergeBuildError::Projection(error)))?;
    }
    Ok(selected)
}

fn resolved<E, A, C>(
    merge: &PreparedGraphEdgeMerge,
    row: &GraphValueRow,
    endpoints: (VId, VId),
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), Fault<E, A, C>>,
) -> Result<GraphValueRow, Fault<E, A, C>> {
    control(GlaExecutionEvent::ScratchEntry)?;
    let mut cells = Vec::new();
    for (column, value) in row.values().iter().enumerate() {
        control(GlaExecutionEvent::ScratchEntry)?;
        cells.push(
            if merge
                .unbound_endpoints
                .iter()
                .any(|endpoint| endpoint.column == column)
            {
                GraphValue::Vertex(if column == merge.source {
                    endpoints.0
                } else {
                    endpoints.1
                })
            } else {
                value.copy_with_control(control)?
            },
        );
    }
    Ok(GraphValueRow::from_owned_values(cells))
}

fn creation_fields<E, A, C>(
    fields: Fields,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), Fault<E, A, C>>,
) -> Result<Vec<(PropertyKeyId, GraphMutationValue)>, Fault<E, A, C>> {
    let mut result = Vec::new();
    for (key, value) in fields {
        control(GlaExecutionEvent::ScratchEntry)?;
        let value = GraphValue::admit_property_value(&value, control)?;
        result.push((
            key,
            GraphMutationValue::Composite(GraphSetValue::Value(value)),
        ));
    }
    Ok(result)
}

impl PreparedGraphEdgeMerge {
    /// Resolve one input occurrence against the host's current admitted graph.
    /// `used` is the enclosing chain's cumulative statistics, not a fresh
    /// allowance. Hosts stage the optional native batch before running the
    /// selected action clause, and invoke this again for each later occurrence.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve_whole_row<E, A, C>(
        &self,
        row: GraphValueRow,
        at: usize,
        policy: GraphInsertPolicy,
        used: GraphInsertStats,
        source: &mut dyn GraphSetSource<E, C>,
        mut allocate: impl FnMut(GraphInsertRequest) -> Result<ElementId, A>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphEdgeWholeMergeProposal, Fault<E, A, C>> {
        let mut meter = Meter {
            policy,
            stats: used,
            checkpoint,
        };
        meter.remaining::<E, A, C>()?;
        meter.event(GlaExecutionEvent::Work)?;
        let width = self
            .input_relation()
            .map_or(0, |input| input.column_types().len());
        if !self.has_unbound_endpoints()
            || used.selection.result_rows != 0
            || used.created_vertices > policy.max_vertices
            || used.created_edges > policy.max_edges
            || row.len() != width
        {
            return Err(invalid());
        }
        let mut bound = [None; 2];
        for (index, column) in [self.source, self.destination].into_iter().enumerate() {
            meter.event(GlaExecutionEvent::Work)?;
            let value = row.values().get(column).ok_or(GqlQueryError::Source(
                GraphEdgeMergeError::InputSchema { row: at, column },
            ))?;
            if self
                .unbound_endpoints
                .iter()
                .any(|endpoint| endpoint.column == column)
            {
                if !value.is_null() {
                    return Err(GqlQueryError::Source(GraphEdgeMergeError::InputSchema {
                        row: at,
                        column,
                    }));
                }
            } else {
                if value.is_null() {
                    return Err(GqlQueryError::Source(GraphEdgeMergeError::NullEndpoint {
                        row: at,
                        column,
                    }));
                }
                bound[index] = Some(value.as_vertex().ok_or(GqlQueryError::Source(
                    GraphEdgeMergeError::InputSchema { row: at, column },
                ))?);
            }
        }
        let mut endpoint_fields = Vec::new();
        for endpoint in &self.unbound_endpoints {
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            endpoint_fields.push((
                endpoint.column,
                properties(&endpoint.properties, at, &row, &mut |event| {
                    meter.event(event)
                })?,
            ));
        }
        // Whole-pattern occurrences never share pair outcomes. Evaluate their
        // map directly, without constructing the bound-pair cache's equality
        // transcript, and admit every owned scalar/list before copying it.
        let mut edge_fields = properties(&self.row_properties, at, &row, &mut |event| {
            meter.event(event)
        })?;
        for (key, value) in &self.properties {
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            edge_fields.push((
                *key,
                scalar_copy(value.value(), &mut |event| meter.event::<E, A, C>(event))?,
            ));
        }
        for property in self.stored_properties.iter() {
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            GraphValue::charge_property_value(&property.value, &mut |event| {
                meter.event::<E, A, C>(event)
            })?;
            edge_fields.push((property.key, property.value.clone()));
        }
        edge_fields.sort_by_key(|(key, _)| *key);
        let matched = matching(self, bound, &endpoint_fields, &edge_fields, &mut |event| {
            meter.event(event)
        })?;
        let selection = matched
            .execute_with_source(meter.remaining()?, source, &mut meter.checkpoint)
            .map_err(|error| meter.rebase(error.map_source(GraphEdgeMergeError::InputRelation)))?;
        if u64::try_from(selection.value.len()).ok() != Some(selection.rows.result_rows) {
            return Err(invalid());
        }
        meter.absorb(selection.rows, selection.evaluator)?;
        let mut rows = Vec::new();
        if !selection.value.is_empty() {
            for selected in selection.value {
                meter.event(GlaExecutionEvent::ScratchEntry)?;
                let [
                    GraphValue::Vertex(a),
                    GraphValue::Vertex(b),
                    GraphValue::Edge(edge),
                    GraphValue::Vertex(src),
                    GraphValue::Vertex(dst),
                ] = selected.values()
                else {
                    return Err(invalid());
                };
                rows.push(GraphEdgeWholeMergeRow {
                    input: resolved(self, &row, (*a, *b), &mut |event| meter.event(event))?,
                    outcome: GraphEdgeMergeOutcome::Matched(*edge),
                    endpoints: (*src, *dst),
                });
            }
            return Ok(GraphEdgeWholeMergeProposal {
                stats: meter.stats,
                rows,
                creation: None,
            });
        }
        let mut vertices = Vec::new();
        for (endpoint, (_, fields)) in self.unbound_endpoints.iter().zip(endpoint_fields) {
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            let mut labels = Vec::new();
            for label in &endpoint.labels {
                meter.event(GlaExecutionEvent::ScratchEntry)?;
                labels.push(*label);
            }
            vertices.push(GraphInsertVertex {
                labels,
                properties: creation_fields(fields, &mut |event| meter.event(event))?,
            });
        }
        let endpoint = |column| {
            self.unbound_endpoints
                .iter()
                .position(|endpoint| endpoint.column == column)
                .map_or(
                    GraphInsertEndpoint::Column(column),
                    GraphInsertEndpoint::CreatedVertex,
                )
        };
        let edge = GraphInsertEdge {
            source: endpoint(self.source),
            destination: endpoint(self.destination),
            relation: self.relation,
            properties: creation_fields(edge_fields, &mut |event| meter.event(event))?,
        };
        meter.event(GlaExecutionEvent::ScratchEntry)?;
        let mut names = Vec::new();
        for column in 0..width {
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            names.push(format!("input{column}"));
        }
        let types = self.input_column_types();
        let insertion = PreparedGraphInsert::prepare_relation(
            PreparedGraphSet::owned(names, types),
            self.relation,
            vertices,
            vec![edge],
        )
        .map_err(|error| build_fault(GraphEdgeMergeBuildError::Creation(error)))?;
        // The insertion consumes one owned row. Its property templates have
        // already been evaluated; only bound endpoint identities are read.
        let remaining = GraphInsertPolicy::new(
            meter.remaining()?,
            policy
                .max_vertices
                .checked_sub(meter.stats.created_vertices)
                .ok_or_else(invalid)?,
            policy
                .max_edges
                .checked_sub(meter.stats.created_edges)
                .ok_or_else(invalid)?,
        );
        let mut input = Vec::new();
        for value in row.values() {
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            input.push(value.copy_with_control(&mut |event| meter.event::<E, A, C>(event))?);
        }
        meter.event(GlaExecutionEvent::ScratchEntry)?;
        // Ownership copying above also belongs to the parent's remaining quota.
        let remaining = GraphInsertPolicy {
            query: meter.remaining()?,
            ..remaining
        };
        let creation = insertion
            .execute_governed_over::<E, A, C>(
                Some(vec![GraphValueRow::from_owned_values(input)]),
                remaining,
                |_: &crate::algebra::PreparedGraphPattern<GraphValueRow>, _| {
                    unreachable!("whole-pattern creation has only its owned input")
                },
                |request| {
                    allocate(match request {
                        GraphInsertRequest::Vertex { vertex, .. } => {
                            GraphInsertRequest::Vertex { row: at, vertex }
                        }
                        GraphInsertRequest::Edge { edge, .. } => {
                            GraphInsertRequest::Edge { row: at, edge }
                        }
                    })
                },
                &mut meter.checkpoint,
            )
            .map_err(|error| {
                let error = error.map_source(|error| {
                    GraphEdgeMergeError::Creation(Box::new(match error {
                        GraphInsertError::Limit {
                            dimension,
                            observed,
                            ..
                        } => {
                            let (limit, used) = match dimension {
                                GraphInsertLimitDimension::Vertices => {
                                    (policy.max_vertices, meter.stats.created_vertices)
                                }
                                GraphInsertLimitDimension::Edges => {
                                    (policy.max_edges, meter.stats.created_edges)
                                }
                            };
                            GraphInsertError::Limit {
                                dimension,
                                limit,
                                observed: observed.saturating_add(u128::from(used)),
                            }
                        }
                        error => error,
                    }))
                });
                meter.rebase(error)
            })?;
        let inserted = creation.stats();
        meter.absorb(inserted.selection, inserted.evaluator)?;
        meter.stats.created_vertices = meter
            .stats
            .created_vertices
            .checked_add(inserted.created_vertices)
            .ok_or_else(invalid)?;
        meter.stats.created_edges = meter
            .stats
            .created_edges
            .checked_add(inserted.created_edges)
            .ok_or_else(invalid)?;
        let Some(GraphInsertIntent::Edge {
            edge,
            source: src,
            destination: dst,
            ..
        }) = creation.intents().last()
        else {
            return Err(invalid());
        };
        meter.event(GlaExecutionEvent::ScratchEntry)?;
        rows.push(GraphEdgeWholeMergeRow {
            input: resolved(self, &row, (*src, *dst), &mut |event| meter.event(event))?,
            outcome: GraphEdgeMergeOutcome::Created(*edge),
            endpoints: (*src, *dst),
        });
        Ok(GraphEdgeWholeMergeProposal {
            stats: meter.stats,
            rows,
            creation: Some(creation),
        })
    }
}
