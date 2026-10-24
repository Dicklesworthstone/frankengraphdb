//! One masked graph source for prepared authorized writes. The caller owns the
//! already verified write permit; selection never issues a second allowance.

use super::{Database, Error, Execution, Vfs, WriteTxn, WriteTxnError};
use crate::query::QueryError;
use crate::write_txn::overlay_scan::OverlayRows;
use crate::write_txn::{
    PointReadField, ProjectionReadAttempt, point_find, point_payload_admission,
};
use fgdb_delta_types::{ElementId, PropertyKeyId};
use fgdb_gql::algebra::{GlaOutput, PreparedGraphPattern};
use fgdb_gql::{
    GlaExecutionEvent, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphPatternFactorExecution, PreparedGraphPatternFactors,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, EId, QueryCx, VId};
use fgdb_warden::PlannerPredicates;
use std::cell::RefCell;

type Selected<Row> = Result<GqlQueryExecution<Row>, GqlQueryError<WriteTxnError, WriteTxnError>>;

/// Read one field of an ALREADY admitted target in the private write workspace.
/// The caller owns target/endpoint authorization and supplies its SAME live
/// signed/native evaluator control. This helper grants no identity authority.
///
/// History, directory and field-search comparisons depend on hidden records and
/// fields. They poll the host's cancellation context, never the holder's work
/// allowance. Only the logical lookup, selected payload, witness and returned
/// scalar are charged. In particular an unrelated hidden key must not move the
/// refusal threshold of an allowed-key binary search (FG-INV-20).
///
/// The native point projector still selects the pinned historical winner and
/// canonical staged effects. No masked row copy or alternate overlay is built.
/// Hidden keys become NULL before lookup or observation; admitted keys retain
/// the original point-read/refused-read witnesses, including absence. Physical
/// scan cost remains the host's responsibility, not a timing or byte-memory
/// isolation claim. Privileged point-read accounting is deliberately unchanged.
#[allow(clippy::too_many_arguments)]
pub(super) fn property<V: Vfs + Clone, E>(
    transaction: &WriteTxn,
    database: &Database<V>,
    element: ElementId,
    key: PropertyKeyId,
    scope: &PlannerPredicates,
    mut poll: impl FnMut() -> Result<(), E>,
    control: &mut dyn FnMut(GlaExecutionEvent) -> Result<(), E>,
    source_error: impl Fn(WriteTxnError) -> E,
) -> Result<CanonicalPropertyValue, E> {
    use crate::gql_exec::source::SourceEvent;

    if !scope.allows_property(key) {
        control(GlaExecutionEvent::ScratchEntry)?;
        return Ok(CanonicalScalar::Null.into());
    }
    control(GlaExecutionEvent::Work)?;
    let field = PointReadField::Property(key);
    let mut attempt = ProjectionReadAttempt {
        reads: &transaction.point_reads,
        element,
        accepted: false,
    };
    let projected = transaction.point_projection_with_control(
        database,
        element,
        field,
        &mut |_| poll(),
        &source_error,
    )?;
    if let Some(value) = projected.property {
        point_payload_admission(value, &mut |event| {
            control(match event {
                SourceEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
                SourceEvent::Work | SourceEvent::SnapshotRecord => GlaExecutionEvent::Work,
            })
        })?;
    }
    // Reserve on EVERY lookup, even when this exact witness is already warm.
    // These live checks also revalidate the permit after physical traversal.
    control(GlaExecutionEvent::ScratchEntry)?;
    control(GlaExecutionEvent::ScratchEntry)?;
    transaction.point_reads.borrow_mut().record(element, field);
    control(GlaExecutionEvent::ScratchEntry)?;
    let value = projected
        .property
        .cloned()
        .unwrap_or_else(|| CanonicalScalar::Null.into());
    attempt.accepted = true;
    Ok(value)
}

// Parameter lists may carry arbitrary identities. Resolve only the metadata
// needed for authorization, through the same canonical overlay as field reads.
// Visibility retains its signed label operands and lifetime witnesses, without
// turning unrelated hidden fields into dependencies or charging hidden searches.
fn mutation_vertex_visible<V: Vfs + Clone>(
    transaction: &WriteTxn,
    database: &Database<V>,
    vertex: VId,
    scope: &PlannerPredicates,
    poll: &mut impl FnMut() -> Result<(), WriteTxnError>,
) -> Result<bool, WriteTxnError> {
    let element = ElementId::Vertex(vertex);
    let mut attempt = ProjectionReadAttempt {
        reads: &transaction.point_reads,
        element,
        accepted: false,
    };
    let mut control = |_| poll();
    let field = PointReadField::VertexExistence;
    let exists = transaction
        .point_projection_with_control(
            database,
            element,
            field,
            &mut control,
            &core::convert::identity,
        )?
        .exists;
    transaction.point_reads.borrow_mut().record(element, field);
    let mut present = Vec::new();
    for label in scope.vertex_label_dependencies() {
        let field = PointReadField::Label(label);
        let projected = transaction.point_projection_with_control(
            database,
            element,
            field,
            &mut control,
            &core::convert::identity,
        )?;
        transaction.point_reads.borrow_mut().record(element, field);
        if projected.label {
            present.push(label);
        }
    }
    attempt.accepted = true;
    Ok(exists && scope.allows_vertex(&present))
}

fn mutation_edge_topology<V: Vfs + Clone>(
    transaction: &WriteTxn,
    database: &Database<V>,
    edge: EId,
    poll: &mut impl FnMut() -> Result<(), WriteTxnError>,
) -> Result<Option<(VId, fgdb_delta_types::RelationId, VId)>, WriteTxnError> {
    use fgdb_delta_types::DeltaRow;
    let snapshot = transaction.query_snapshot(database)?;
    let element = ElementId::Edge(edge);
    let mut attempt = ProjectionReadAttempt {
        reads: &transaction.point_reads,
        element,
        accepted: false,
    };
    let mut control = |_| poll();
    let coordinate = snapshot.adjacency_index().statement_at_controlled(
        &snapshot.blocks,
        edge,
        transaction.basis,
        &mut control,
    )?;
    let mut topology = coordinate.map(|(block, row)| {
        let edge = &snapshot.blocks[block][row];
        (edge.src, edge.relation, edge.dst)
    });
    if let Some(prepared) = &transaction.prepared {
        for coordinate in prepared.template.coordinate_entries() {
            control(crate::gql_exec::source::SourceEvent::Work)?;
            for effect in &coordinate.rows {
                control(crate::gql_exec::source::SourceEvent::Work)?;
                match effect {
                    DeltaRow::CreateEdge {
                        eid,
                        src,
                        relation,
                        dst,
                        ..
                    } if *eid == edge => {
                        topology = Some((*src, *relation, *dst));
                    }
                    DeltaRow::DeleteEdge { eid, .. } if *eid == edge => topology = None,
                    DeltaRow::DeleteVertex {
                        sorted_retired_incident_edges,
                        ..
                    } if point_find(sorted_retired_incident_edges, edge, |id| *id, &mut control)?
                        .is_some() =>
                    {
                        topology = None;
                    }
                    _ => {}
                }
            }
        }
    }
    transaction
        .point_reads
        .borrow_mut()
        .record(element, PointReadField::EdgeTopology);
    attempt.accepted = true;
    Ok(topology)
}

/// Admit an arbitrary carried identity before reading its field: keys first,
/// then vertex visibility, or relationship type AND both endpoint visibilities.
/// Absent and hidden inputs read as NULL. The shared expression evaluator meters
/// its logical copy; physical searches only poll, and the same signed permit
/// admits selected payload before cloning it. All reads use the frozen overlay.
#[allow(clippy::too_many_arguments)]
pub(super) fn mutation_element_property<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &WriteTxn,
    database: &Database<V>,
    cx: &QueryCx,
    element: ElementId,
    key: PropertyKeyId,
    scope: &PlannerPredicates,
    execution: &RefCell<&mut Execution<'_, '_, Clock>>,
) -> Result<Option<CanonicalPropertyValue>, WriteTxnError> {
    if !scope.allows_property(key) {
        return Ok(None);
    }
    // Reserve the visibility witnesses and temporary label slots before any
    // hidden metadata is examined. This worst-case bound depends only on the
    // signed predicate and caller-supplied value kind, including missing and
    // hidden identities. Physical probes consume no further signed allowance.
    let labels = u64::try_from(scope.vertex_label_dependencies().count())
        .map_err(|_| WriteTxnError::Authorization(Error::TooLarge))?;
    let vertex_slots = labels
        .checked_mul(2)
        .and_then(|slots| slots.checked_add(2))
        .ok_or(WriteTxnError::Authorization(Error::TooLarge))?;
    let slots = match element {
        ElementId::Vertex(_) => vertex_slots,
        ElementId::Edge(_) => vertex_slots
            .checked_mul(2)
            .and_then(|slots| slots.checked_add(2))
            .ok_or(WriteTxnError::Authorization(Error::TooLarge))?,
    };
    cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
    execution.borrow_mut().work(slots)?;
    let mut poll = || {
        cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
        execution.borrow_mut().poll()
    };
    poll()?;
    match element {
        ElementId::Vertex(vertex) => {
            if !mutation_vertex_visible(transaction, database, vertex, scope, &mut poll)? {
                return Ok(None);
            }
        }
        ElementId::Edge(edge) => {
            let Some((source, relation, target)) =
                mutation_edge_topology(transaction, database, edge, &mut poll)?
            else {
                return Ok(None);
            };
            if !scope.allows_relation(relation) {
                return Ok(None);
            }
            for vertex in [source, target] {
                if !mutation_vertex_visible(transaction, database, vertex, scope, &mut poll)? {
                    return Ok(None);
                }
            }
        }
    }
    property(
        transaction,
        database,
        element,
        key,
        scope,
        || {
            cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
            execution.borrow_mut().poll()
        },
        &mut |_| {
            cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
            execution.borrow_mut().checkpoint()
        },
        core::convert::identity,
    )
    .map(Some)
}

/// A [`property`] value for a consumer that holds only a scalar: a stored
/// List or Map (fgdb-k3nub) refuses as a non-scalar operand rather than
/// reading as NULL.
pub(super) fn scalar<S, C>(
    value: CanonicalPropertyValue,
) -> Result<CanonicalScalar, GqlQueryError<S, C>> {
    match value {
        CanonicalPropertyValue::Scalar(value) => Ok(value),
        CanonicalPropertyValue::List(_) | CanonicalPropertyValue::Map(_) => {
            Err(GqlQueryError::Data(fgdb_gql::GraphIntegerError {
                instruction: 0,
                kind: fgdb_gql::GraphIntegerErrorKind::NonScalar,
            }))
        }
    }
}

fn query_control(error: WriteTxnError) -> QueryError {
    match error {
        WriteTxnError::Authorization(error) => QueryError::Authorization(error),
        WriteTxnError::Interrupted(error) => QueryError::Pattern(GqlQueryError::Interrupted(error)),
        // Controls only produce the two errors above. Never expose a future
        // native mutation error through the selection's interruption carrier.
        _ => QueryError::Authorization(Error::ScopeDenied),
    }
}

fn selection_error(
    error: GqlQueryError<crate::ReadError, QueryError>,
) -> GqlQueryError<WriteTxnError, WriteTxnError> {
    match error {
        GqlQueryError::Source(error) => GqlQueryError::Source(WriteTxnError::from(error)),
        GqlQueryError::Rows(error) => GqlQueryError::Rows(error),
        GqlQueryError::Evaluator(error) => GqlQueryError::Evaluator(error),
        GqlQueryError::IdentifiedEdgesRequired => GqlQueryError::IdentifiedEdgesRequired,
        GqlQueryError::Data(error) => GqlQueryError::Data(error),
        GqlQueryError::Interrupted(QueryError::Authorization(error)) => {
            GqlQueryError::Interrupted(WriteTxnError::Authorization(error))
        }
        GqlQueryError::Interrupted(QueryError::Pattern(GqlQueryError::Interrupted(error))) => {
            GqlQueryError::Interrupted(WriteTxnError::Interrupted(error))
        }
        GqlQueryError::Interrupted(_) => {
            GqlQueryError::Source(WriteTxnError::AuthorizedMutationRefused)
        }
    }
}

/// Call only before staging in an exclusively borrowed private transaction.
/// Its owner checks ReadWrite rights before any database access. Hidden history
/// polls cancellation without charging a signed or native resource allowance;
/// admitted data uses the same source and GLA as ordinary authorized reads.
pub(super) fn select<V: Vfs + Clone, Row: GlaOutput, Clock: FnMut() -> u64>(
    database: &Database<V>,
    cx: &QueryCx,
    pattern: &PreparedGraphPattern<Row>,
    scope: &PlannerPredicates,
    policy: GqlQueryPolicy,
    execution: &RefCell<&mut Execution<'_, '_, Clock>>,
) -> Selected<Row> {
    controlled(cx, execution, |node, poll, checkpoint| {
        database.select_for_authorized_insert(cx, pattern, scope, policy, node, poll, checkpoint)
    })
}

/// The same pinned and scoped selection, physically returned as two complete
/// endpoint factors. The original plan controls sparse overlay construction.
pub(super) fn select_overlay_factors<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &WriteTxn,
    database: &Database<V>,
    cx: &QueryCx,
    request: &PreparedGraphPatternFactors<'_>,
    scope: &PlannerPredicates,
    policy: GqlQueryPolicy,
    execution: &RefCell<&mut Execution<'_, '_, Clock>>,
) -> Result<GraphPatternFactorExecution, GqlQueryError<WriteTxnError, WriteTxnError>> {
    with_overlay(
        transaction,
        database,
        cx,
        request.original().plan().reads_edges(),
        execution,
        |overlay| {
            controlled(cx, execution, |node, poll, checkpoint| match overlay {
                None => database.select_factors_for_authorized_insert(
                    cx, request, scope, policy, node, poll, checkpoint,
                ),
                Some(overlay) => database.select_factors_for_authorized_overlay(
                    cx, overlay, request, scope, policy, node, poll, checkpoint,
                ),
            })
        },
    )
}

/// Resolve the native canonical overlay before masking. Raw staged intentions
/// are never replayed here. The sparse owner uses the existing canonical-effect
/// applicators and borrows unchanged historical winners, so admission can refuse
/// without cloning the entire raw graph. Traversal and sparse construction poll
/// cancellation; only admitted rows consume signed/native query allowances.
/// The resident generation, changed-row payloads and conflict witnesses are not
/// a bounded-byte or spillable storage abstraction.
pub(super) fn select_overlay<V: Vfs + Clone, Row: GlaOutput, Clock: FnMut() -> u64>(
    transaction: &WriteTxn,
    database: &Database<V>,
    cx: &QueryCx,
    pattern: &PreparedGraphPattern<Row>,
    scope: &PlannerPredicates,
    policy: GqlQueryPolicy,
    execution: &RefCell<&mut Execution<'_, '_, Clock>>,
) -> Selected<Row> {
    with_overlay(
        transaction,
        database,
        cx,
        pattern.plan().reads_edges(),
        execution,
        |overlay| match overlay {
            None => select(database, cx, pattern, scope, policy, execution),
            Some(overlay) => controlled(cx, execution, |node, poll, checkpoint| {
                database.select_for_authorized_overlay(
                    cx, overlay, pattern, scope, policy, node, poll, checkpoint,
                )
            }),
        },
    )
}

// One lifetime owner and identical preflight/error ordering for both physical
// forms. No borrowed source escapes the exclusively borrowed statement phase.
fn with_overlay<V: Vfs + Clone, T, Clock: FnMut() -> u64>(
    transaction: &WriteTxn,
    database: &Database<V>,
    cx: &QueryCx,
    reads_edges: bool,
    execution: &RefCell<&mut Execution<'_, '_, Clock>>,
    evaluate: impl FnOnce(
        Option<&OverlayRows<'_, V>>,
    ) -> Result<T, GqlQueryError<WriteTxnError, WriteTxnError>>,
) -> Result<T, GqlQueryError<WriteTxnError, WriteTxnError>> {
    transaction
        .ensure_database(database)
        .map_err(GqlQueryError::Source)?;
    let live = database
        .frontier()
        .map_err(WriteTxnError::from)
        .map_err(GqlQueryError::Source)?;
    if transaction.basis != live {
        return Err(GqlQueryError::Source(WriteTxnError::SnapshotAdvanced {
            pinned: transaction.basis,
            live,
        }));
    }
    if transaction.staged.is_empty() {
        return evaluate(None);
    }
    cx.with_restriction(|| {
        let mut poll = || {
            cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
            execution.borrow_mut().poll()
        };
        poll().map_err(GqlQueryError::Interrupted)?;
        let overlay =
            OverlayRows::new(transaction, database, reads_edges, &mut poll).map_err(|error| {
                match error {
                    error @ (WriteTxnError::Interrupted(_) | WriteTxnError::Authorization(_)) => {
                        GqlQueryError::Interrupted(error)
                    }
                    error => GqlQueryError::Source(super::redacted(error)),
                }
            })?;
        poll().map_err(GqlQueryError::Interrupted)?;
        evaluate(Some(&overlay))
    })
}

type Control<'a> = &'a mut dyn FnMut() -> Result<(), QueryError>;

fn controlled<T, Clock: FnMut() -> u64>(
    cx: &QueryCx,
    execution: &RefCell<&mut Execution<'_, '_, Clock>>,
    evaluate: impl FnOnce(
        Control<'_>,
        Control<'_>,
        Control<'_>,
    ) -> Result<T, GqlQueryError<crate::ReadError, QueryError>>,
) -> Result<T, GqlQueryError<WriteTxnError, WriteTxnError>> {
    evaluate(
        &mut || {
            cx.checkpoint()
                .map_err(|error| query_control(WriteTxnError::Interrupted(error)))?;
            let mut borrowed = execution.borrow_mut();
            let execution = &mut **borrowed;
            execution.checkpoint().map_err(query_control)?;
            let now = (execution.clock)();
            execution
                .permit
                .charge_nodes_at(now, 1)
                .map_err(QueryError::Authorization)
        },
        &mut || {
            cx.checkpoint()
                .map_err(|error| query_control(WriteTxnError::Interrupted(error)))?;
            execution.borrow_mut().poll().map_err(query_control)
        },
        &mut || {
            cx.checkpoint()
                .map_err(|error| query_control(WriteTxnError::Interrupted(error)))?;
            execution.borrow_mut().checkpoint().map_err(query_control)
        },
    )
    .map_err(selection_error)
}

#[cfg(test)]
#[path = "selection_tests.rs"]
mod tests;
