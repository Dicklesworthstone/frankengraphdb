//! Two-phase creation: freeze checked values, then assign identities. Nothing
//! is submitted to storage until both phases and the final checkpoint succeed.
//! Standalone creation supplies one zero-column occurrence to these same phases;
//! it never fabricates a public GraphValueRow or calls a graph-source adapter.

use super::*;
use crate::algebra::{GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphValue};
use crate::{
    GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GqlBudgetDimension,
    GraphIntegerEvaluationError,
};
use std::collections::BTreeSet;

type ResultOf<T, E, A, C> = Result<T, GqlQueryError<GraphInsertError<E, A>, C>>;
type Fields = Vec<(PropertyKeyId, CanonicalScalar)>;

struct Meter<F> {
    policy: GqlQueryPolicy,
    evaluator: GlaExecutionStats,
    checkpoint: F,
}
impl<F> Meter<F> {
    fn event<E, A, C>(&mut self, event: GlaExecutionEvent) -> ResultOf<(), E, A, C>
    where
        F: FnMut() -> Result<(), C>,
    {
        (self.checkpoint)().map_err(GqlQueryError::Interrupted)?;
        let work = u128::from(self.evaluator.work_units) + 1;
        let scratch = u128::from(self.evaluator.scratch_entries)
            + u128::from(event == GlaExecutionEvent::ScratchEntry);
        for (dimension, limit, observed) in [
            (
                GlaLimitDimension::WorkUnits,
                self.policy.evaluator.max_work_units,
                work,
            ),
            (
                GlaLimitDimension::ScratchEntries,
                self.policy.evaluator.max_scratch_entries,
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
        self.evaluator = GlaExecutionStats {
            work_units: work as u64,
            scratch_entries: scratch as u64,
        };
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Endpoint {
    Existing(VId),
    Created(usize),
}
struct EdgeDraft {
    source: Endpoint,
    destination: Endpoint,
    properties: Fields,
}
struct RowDraft {
    vertices: Vec<Fields>,
    edges: Vec<EdgeDraft>,
}

fn endpoint<E, A, C>(
    endpoint: GraphInsertEndpoint,
    row: &[GraphValue],
    row_at: usize,
    edge: usize,
) -> ResultOf<Endpoint, E, A, C> {
    match endpoint {
        GraphInsertEndpoint::CreatedVertex(vertex) => Ok(Endpoint::Created(vertex)),
        GraphInsertEndpoint::Column(column) => row[column]
            .as_vertex()
            .map(Endpoint::Existing)
            .ok_or(GqlQueryError::Source(GraphInsertError::NullEndpoint {
                row: row_at,
                edge,
                column,
            })),
    }
}

fn fields<E, A, C>(
    properties: &Properties,
    row: &[GraphValue],
    row_at: usize,
    declaration: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> ResultOf<(), E, A, C>,
) -> ResultOf<Fields, E, A, C> {
    let mut result = Vec::new();
    for (property, (key, expression)) in properties
        .keys
        .iter()
        .zip(&properties.projection)
        .enumerate()
    {
        control(GlaExecutionEvent::Work)?;
        // Use the same scalar/CASE VM as mutation, RETURN and aggregation.
        // Computed values already own their charged payload; move, never copy.
        let value = match expression.value() {
            GraphSetValue::Column(column) => row[*column]
                .as_scalar()
                .expect("the complete input schema was validated"),
            GraphSetValue::Literal(value) => value.value(),
            GraphSetValue::Value(value) => match value.as_scalar() {
                Some(scalar) => scalar,
                // Only a scalar property field is expressible here; a vertex,
                // edge, path or list projection is an input-schema refusal,
                // never a silent truncation.
                None => {
                    return Err(GqlQueryError::Source(GraphInsertError::InputSchema {
                        row: row_at,
                        column: property,
                    }));
                }
            },
            // Composite set expressions cannot name one property field.
            GraphSetValue::List(_) | GraphSetValue::Index { .. } | GraphSetValue::Size(_) => {
                return Err(GqlQueryError::Source(GraphInsertError::InputSchema {
                    row: row_at,
                    column: property,
                }));
            }
            GraphSetValue::Integer(expression) => {
                let value = expression
                    .evaluate_scalar_with_control(row, control)
                    .map_err(|error| match error {
                        GraphIntegerEvaluationError::Control(error) => error,
                        GraphIntegerEvaluationError::Value(error) => {
                            GqlQueryError::Source(GraphInsertError::Arithmetic {
                                row: row_at,
                                declaration,
                                property,
                                error,
                            })
                        }
                    })?;
                control(GlaExecutionEvent::ScratchEntry)?;
                result.push((*key, value));
                continue;
            }
        };
        // Reserve the field and its variable payload before its only clone.
        let sizes = match value {
            CanonicalScalar::Text(value) => [
                value.len(),
                value.canonical_sort_key().map_or(0, <[u8]>::len),
            ],
            CanonicalScalar::Bytes(value) => [value.as_slice().len(), 0],
            CanonicalScalar::Timestamp(value) => {
                [value.zone().map_or(0, |zone| zone.identifier().len()), 0]
            }
            _ => [0, 0],
        };
        control(GlaExecutionEvent::ScratchEntry)?;
        for bytes in sizes {
            for _ in 0..bytes.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
                control(GlaExecutionEvent::ScratchEntry)?;
            }
        }
        result.push((*key, value.clone()));
    }
    Ok(result)
}

fn allocate_id<E, A, C>(
    request: GraphInsertRequest,
    allocator: &mut impl FnMut(GraphInsertRequest) -> Result<ElementId, A>,
    seen: &mut BTreeSet<ElementId>,
    control: &mut impl FnMut(GlaExecutionEvent) -> ResultOf<(), E, A, C>,
) -> ResultOf<ElementId, E, A, C> {
    control(GlaExecutionEvent::ScratchEntry)?;
    let id = allocator(request)
        .map_err(|error| GqlQueryError::Source(GraphInsertError::IdentitySource(error)))?;
    if !matches!(
        (request, id),
        (GraphInsertRequest::Vertex { .. }, ElementId::Vertex(_))
            | (GraphInsertRequest::Edge { .. }, ElementId::Edge(_))
    ) {
        return Err(GqlQueryError::Source(GraphInsertError::IdentityKind {
            request,
        }));
    }
    // BTreeSet comparisons use fixed-width identities. Explicitly charge a
    // logarithmic search allowance before insertion, not only its allocation.
    for _ in 0..(usize::BITS - seen.len().leading_zeros()).max(1) {
        control(GlaExecutionEvent::Work)?;
    }
    if !seen.insert(id) {
        return Err(GqlQueryError::Source(GraphInsertError::DuplicateIdentity {
            request,
        }));
    }
    Ok(id)
}

pub(super) fn execute<E, A, C>(
    insertion: &PreparedGraphInsert,
    policy: GraphInsertPolicy,
    source: impl FnOnce(
        &PreparedGraphPattern<GraphValueRow>,
        GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
    mut allocate: impl FnMut(GraphInsertRequest) -> Result<ElementId, A>,
    mut checkpoint: impl FnMut() -> Result<(), C>,
) -> ResultOf<GraphInsertBatch, E, A, C> {
    checkpoint().map_err(GqlQueryError::Interrupted)?;
    let (selected_rows, selection, evaluator) = match &insertion.selection {
        Some(pattern) => {
            let selected = source(pattern, policy.query)
                .map_err(|error| error.map_source(GraphInsertError::Source))?;
            if u64::try_from(selected.value.len()).ok() != Some(selected.rows.result_rows) {
                return Err(GqlQueryError::Source(
                    GraphInsertError::InvalidSourceStatistics,
                ));
            }
            (selected.value, selected.rows, selected.evaluator)
        }
        None => (
            Vec::new(),
            GqlExecutionStats {
                snapshot_records: 0,
                result_rows: 1,
            },
            GlaExecutionStats::default(),
        ),
    };
    for (dimension, observed) in [
        (
            GqlBudgetDimension::SnapshotRecords,
            selection.snapshot_records,
        ),
        (GqlBudgetDimension::ResultRows, selection.result_rows),
    ] {
        policy
            .query
            .rows
            .check(dimension, observed)
            .map_err(GqlQueryError::Rows)?;
    }
    for (dimension, limit, observed) in [
        (
            GlaLimitDimension::WorkUnits,
            policy.query.evaluator.max_work_units,
            evaluator.work_units,
        ),
        (
            GlaLimitDimension::ScratchEntries,
            policy.query.evaluator.max_scratch_entries,
            evaluator.scratch_entries,
        ),
    ] {
        if observed > limit {
            return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                dimension,
                limit,
                observed: u128::from(observed),
            }));
        }
    }
    let created_vertices = u128::from(selection.result_rows) * insertion.vertices.len() as u128;
    let created_edges = u128::from(selection.result_rows) * insertion.edges.len() as u128;
    for (dimension, limit, observed) in [
        (
            GraphInsertLimitDimension::Vertices,
            policy.max_vertices,
            created_vertices,
        ),
        (
            GraphInsertLimitDimension::Edges,
            policy.max_edges,
            created_edges,
        ),
    ] {
        if observed > u128::from(limit) {
            return Err(GqlQueryError::Source(GraphInsertError::Limit {
                dimension,
                limit,
                observed,
            }));
        }
    }
    let mut meter = Meter {
        policy: policy.query,
        evaluator,
        checkpoint,
    };
    meter.event(GlaExecutionEvent::Work)?;
    let columns = insertion
        .selection
        .as_ref()
        .map_or(&[][..], |pattern| pattern.value_columns());
    let mut drafts = Vec::new();
    // A unit occurrence has no values or graph identity. Ordinary rows retain
    // their existing order and are freed as their draft is completed. Neither
    // path duplicates graph matching or property evaluation.
    let unit = insertion.selection.is_none().then_some(None);
    for (row_at, row) in selected_rows.into_iter().map(Some).chain(unit).enumerate() {
        let row = row.as_ref().map_or(&[][..], GraphValueRow::values);
        meter.event(GlaExecutionEvent::Work)?;
        if row.len() != columns.len() {
            return Err(GqlQueryError::Source(GraphInsertError::InputSchema {
                row: row_at,
                column: row.len().min(columns.len()),
            }));
        }
        for (column, (value, expression)) in row.iter().zip(columns).enumerate() {
            meter.event(GlaExecutionEvent::Work)?;
            let valid = crate::GraphSetColumnType::from(expression).accepts(value);
            if !valid {
                return Err(GqlQueryError::Source(GraphInsertError::InputSchema {
                    row: row_at,
                    column,
                }));
            }
        }
        let mut vertices = Vec::new();
        for (declaration, vertex) in insertion.vertices.iter().enumerate() {
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            vertices.push(fields(
                &vertex.properties,
                row,
                row_at,
                declaration,
                &mut |event| meter.event(event),
            )?);
        }
        let mut edges = Vec::new();
        for (edge_at, edge) in insertion.edges.iter().enumerate() {
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            let source = endpoint(edge.source, row, row_at, edge_at)?;
            let destination = endpoint(edge.destination, row, row_at, edge_at)?;
            let properties = fields(
                &edge.properties,
                row,
                row_at,
                insertion.vertices.len() + edge_at,
                &mut |event| meter.event(event),
            )?;
            edges.push(EdgeDraft {
                source,
                destination,
                properties,
            });
        }
        meter.event(GlaExecutionEvent::ScratchEntry)?;
        drafts.push(RowDraft { vertices, edges });
    }
    // Every selected property's value has now been checked without requesting
    // an identity. No write or identity-derived expression can feed matching.
    let mut intents = Vec::new();
    let mut seen = BTreeSet::new();
    for (row, draft) in drafts.into_iter().enumerate() {
        let mut created = Vec::new();
        for (vertex, (properties, declaration)) in draft
            .vertices
            .into_iter()
            .zip(&insertion.vertices)
            .enumerate()
        {
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            let ElementId::Vertex(id) = allocate_id(
                GraphInsertRequest::Vertex { row, vertex },
                &mut allocate,
                &mut seen,
                &mut |event| meter.event(event),
            )?
            else {
                unreachable!("allocator kind was checked")
            };
            created.push(id);
            let mut labels = Vec::new();
            for label in &declaration.labels {
                meter.event(GlaExecutionEvent::ScratchEntry)?;
                labels.push(*label);
            }
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            intents.push(GraphInsertIntent::Vertex {
                vertex: id,
                labels,
                properties,
            });
        }
        for (edge, draft) in draft.edges.into_iter().enumerate() {
            let resolve = |endpoint| match endpoint {
                Endpoint::Existing(vid) => vid,
                Endpoint::Created(vertex) => created[vertex],
            };
            let source = resolve(draft.source);
            let destination = resolve(draft.destination);
            let ElementId::Edge(id) = allocate_id(
                GraphInsertRequest::Edge { row, edge },
                &mut allocate,
                &mut seen,
                &mut |event| meter.event(event),
            )?
            else {
                unreachable!("allocator kind was checked")
            };
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            intents.push(GraphInsertIntent::Edge {
                edge: id,
                source,
                destination,
                properties: draft.properties,
            });
        }
    }
    meter.event(GlaExecutionEvent::Work)?;
    Ok(GraphInsertBatch {
        intents,
        stats: GraphInsertStats {
            selection,
            evaluator: meter.evaluator,
            created_vertices: created_vertices as u64,
            created_edges: created_edges as u64,
        },
    })
}
