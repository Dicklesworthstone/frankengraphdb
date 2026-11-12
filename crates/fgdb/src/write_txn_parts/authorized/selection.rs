//! One masked graph source for prepared authorized writes. The caller owns the
//! already verified write permit; selection never issues a second allowance.

use super::{Database, Error, Execution, Vfs, WriteTxn, WriteTxnError};
use crate::gql_exec::{AdmissionUsage, source::SourceEvent};
use crate::query::QueryError;
use crate::write_txn::overlay_scan::OverlayRows;
use crate::write_txn::point_effect_selector::{EffectKey, select as select_effects};
use crate::write_txn::{
    PointReadField, ProjectionReadAttempt, point_find, point_payload_admission,
};
use fgdb_delta_types::{ElementId, PropertyKeyId};
use fgdb_gql::algebra::{
    GlaOutput, GraphValue, GraphValueRow, PreparedGraphPattern, VertexScanDomain,
};
use fgdb_gql::{
    GlaExecutionEvent, GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryExecution,
    GqlQueryPolicy, GraphPatternFactorExecution, PreparedGraphPatternFactors,
    PreparedProcedureCall,
};
use fgdb_prism::{
    FnxMeter, FnxReadError, ParallelEdgePolicy, ProjectionBuildError, ProjectionEdge,
    ProjectionError, SelfLoopPolicy, SnapshotBinding, SnapshotGraphView,
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
            let rows = &coordinate.rows;
            for effect in select_effects(rows, EffectKey::CreateEdge(edge), &mut || {
                control(crate::gql_exec::source::SourceEvent::Work)
            })? {
                control(crate::gql_exec::source::SourceEvent::Work)?;
                if let DeltaRow::CreateEdge {
                    src, relation, dst, ..
                } = effect
                {
                    topology = Some((*src, *relation, *dst));
                }
            }
            // Every prepared cascade is an admitted incidence image. Probe
            // both physical endpoints in canonical vertex order, keeping the
            // exact retired-edge membership check and the existing witness.
            if let Some((source, _, target)) = topology {
                let (first, last) = (source.min(target), source.max(target));
                for vertex in [Some(first), (last != first).then_some(last)]
                    .into_iter()
                    .flatten()
                {
                    for effect in
                        select_effects(rows, EffectKey::DeleteVertex(vertex), &mut || {
                            control(crate::gql_exec::source::SourceEvent::Work)
                        })?
                    {
                        control(crate::gql_exec::source::SourceEvent::Work)?;
                        if let DeltaRow::DeleteVertex {
                            sorted_retired_incident_edges,
                            ..
                        } = effect
                            && point_find(
                                sorted_retired_incident_edges,
                                edge,
                                |id| *id,
                                &mut control,
                            )?
                            .is_some()
                        {
                            topology = None;
                        }
                    }
                }
            }
            for _ in select_effects(rows, EffectKey::DeleteEdge(edge), &mut || {
                control(crate::gql_exec::source::SourceEvent::Work)
            })? {
                control(crate::gql_exec::source::SourceEvent::Work)?;
                topology = None;
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
/// The mutation entry point has already required Read authority.
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
    mutation_element_property_controlled(
        transaction,
        database,
        cx,
        element,
        key,
        scope,
        execution,
        &mut |_| {
            cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
            execution.borrow_mut().checkpoint()
        },
        core::convert::identity,
    )
}

/// As the ordinary mutation reader, with the caller's live native/signed
/// evaluator admitting selected payload and witness ownership before copying.
/// Visibility and physical hidden searches preserve their original transcript.
#[allow(clippy::too_many_arguments)]
pub(super) fn mutation_element_property_controlled<V: Vfs + Clone, Clock: FnMut() -> u64, E>(
    transaction: &WriteTxn,
    database: &Database<V>,
    cx: &QueryCx,
    element: ElementId,
    key: PropertyKeyId,
    scope: &PlannerPredicates,
    execution: &RefCell<&mut Execution<'_, '_, Clock>>,
    control: &mut dyn FnMut(GlaExecutionEvent) -> Result<(), E>,
    source_error: impl Fn(WriteTxnError) -> E,
) -> Result<Option<CanonicalPropertyValue>, E> {
    if !scope.allows_property(key) {
        return Ok(None);
    }
    transaction.check_chain_element_read(
        element,
        &mut |_| control(GlaExecutionEvent::Work),
        &source_error,
    )?;
    if mutation_element_visible(transaction, database, cx, element, scope, execution)
        .map_err(&source_error)?
        .is_none()
    {
        return Ok(None);
    }
    property(
        transaction,
        database,
        element,
        key,
        scope,
        || {
            cx.checkpoint()
                .map_err(WriteTxnError::Interrupted)
                .map_err(&source_error)?;
            execution.borrow_mut().poll().map_err(&source_error)
        },
        control,
        &source_error,
    )
    .map(Some)
}

fn mutation_element_visible<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &WriteTxn,
    database: &Database<V>,
    cx: &QueryCx,
    element: ElementId,
    scope: &PlannerPredicates,
    execution: &RefCell<&mut Execution<'_, '_, Clock>>,
) -> Result<Option<u64>, WriteTxnError> {
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
            Ok(Some(1))
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
            Ok(Some(if source == target { 1 } else { 2 }))
        }
    }
}

/// A computed MERGE output may name any identity, including a parameter's.
/// Require Read before applying the existing masked point-read contract;
/// carrying an identity through a write never confers observation authority.
#[allow(clippy::too_many_arguments)]
pub(super) fn returning_element_property<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &WriteTxn,
    database: &Database<V>,
    cx: &QueryCx,
    element: ElementId,
    key: PropertyKeyId,
    scope: &PlannerPredicates,
    execution: &RefCell<&mut Execution<'_, '_, Clock>>,
) -> Result<Option<CanonicalPropertyValue>, WriteTxnError> {
    if !scope.rights().can_read() {
        return Err(WriteTxnError::Authorization(Error::PermissionDenied));
    }
    mutation_element_property(transaction, database, cx, element, key, scope, execution)
}

/// Complete metadata of a carried identity uses the same original-object
/// admission as scalar reads, including both endpoints. Only after admission
/// may the collector name visible fields or retain their insertion-gap mask.
#[allow(clippy::too_many_arguments)]
pub(super) fn returning_element_metadata<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &WriteTxn,
    database: &Database<V>,
    cx: &QueryCx,
    element: ElementId,
    kind: fgdb_gql::GraphElementMetadata,
    catalog: &fgdb_gql::ReverseSymbolCatalog,
    scope: &PlannerPredicates,
    execution: &RefCell<&mut Execution<'_, '_, Clock>>,
    control: &mut dyn FnMut(GlaExecutionEvent) -> Result<(), ()>,
) -> Result<Option<fgdb_gql::algebra::GraphValue>, fgdb_gql::GraphElementMetadataError<WriteTxnError>>
{
    use crate::gql_exec::source::SourceEvent;
    use fgdb_gql::GraphElementMetadataError;
    if !scope.rights().can_read() {
        return Err(GraphElementMetadataError::Source(
            WriteTxnError::Authorization(Error::PermissionDenied),
        ));
    }
    transaction.check_chain_element_read(
        element,
        &mut |_| {
            cx.checkpoint()
                .map_err(WriteTxnError::Interrupted)
                .map_err(GraphElementMetadataError::Source)?;
            execution
                .borrow_mut()
                .checkpoint()
                .map_err(GraphElementMetadataError::Source)?;
            control(GlaExecutionEvent::Work).map_err(|()| GraphElementMetadataError::Interrupted)
        },
        &GraphElementMetadataError::Source,
    )?;
    let Some(nodes) =
        mutation_element_visible(transaction, database, cx, element, scope, execution)
            .map_err(GraphElementMetadataError::Source)?
    else {
        return Ok(None);
    };
    // Match read-only metadata admission: every visible vertex or unique
    // relationship endpoint spends one node, even when its witnesses are warm.
    // Both endpoint predicates precede all node charges, so a hidden endpoint
    // remains indistinguishable from an absent relationship.
    for _ in 0..nodes {
        cx.checkpoint()
            .map_err(WriteTxnError::Interrupted)
            .map_err(GraphElementMetadataError::Source)?;
        let mut borrowed = execution.borrow_mut();
        let execution = &mut **borrowed;
        execution
            .checkpoint()
            .map_err(GraphElementMetadataError::Source)?;
        let now = (execution.clock)();
        execution
            .permit
            .charge_nodes_at(now, 1)
            .map_err(WriteTxnError::Authorization)
            .map_err(GraphElementMetadataError::Source)?;
    }
    transaction.admitted_element_metadata_with_control(
        database,
        element,
        kind,
        catalog,
        Some(scope),
        &mut || {
            cx.checkpoint()
                .map_err(WriteTxnError::Interrupted)
                .map_err(GraphElementMetadataError::Source)?;
            execution
                .borrow_mut()
                .poll()
                .map_err(GraphElementMetadataError::Source)
        },
        &mut |event| {
            cx.checkpoint()
                .map_err(WriteTxnError::Interrupted)
                .map_err(GraphElementMetadataError::Source)?;
            execution
                .borrow_mut()
                .checkpoint()
                .map_err(GraphElementMetadataError::Source)?;
            control(match event {
                SourceEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
                SourceEvent::Work | SourceEvent::SnapshotRecord => GlaExecutionEvent::Work,
            })
            .map_err(|()| GraphElementMetadataError::Interrupted)
        },
        &GraphElementMetadataError::Source,
    )
}

// The existing pattern callback keeps its original physical capabilities.
// Procedure refusal stays optional, so an unsupported host is not advertised.
struct ScopedInputSource<S, P> {
    patterns: S,
    procedures: P,
}

impl<S, P> fgdb_gql::GraphSetSource<WriteTxnError, WriteTxnError> for ScopedInputSource<S, P>
where
    S: FnMut(&PreparedGraphPattern<GraphValueRow>, GqlQueryPolicy) -> Selected<GraphValueRow>,
    P: FnMut(
        &PreparedProcedureCall,
        &[GraphValue],
        GqlQueryPolicy,
    ) -> Option<Selected<GraphValueRow>>,
{
    fn pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Selected<GraphValueRow> {
        (self.patterns)(pattern, policy)
    }

    fn procedure(
        &mut self,
        call: &PreparedProcedureCall,
        arguments: &[GraphValue],
        policy: GqlQueryPolicy,
    ) -> Option<Selected<GraphValueRow>> {
        (self.procedures)(call, arguments, policy)
    }
}

/// A MERGE input relation's whole source: `patterns` serves its pattern
/// leaves, and a carried identity it does not rematch (`a.p` of a WITH
/// vertex) is read as a MATCH projection reads it. A named hidden field keeps
/// the explicit refusal; an imported identity confers no authority, so Read
/// and the same signed permit are charged before each point lookup.
pub(super) fn scoped_input<'a, V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &'a WriteTxn,
    database: &'a Database<V>,
    cx: &'a QueryCx,
    scope: &'a PlannerPredicates,
    execution: &'a RefCell<&mut Execution<'_, '_, Clock>>,
    patterns: impl FnMut(
        &PreparedGraphPattern<fgdb_gql::algebra::GraphValueRow>,
        GqlQueryPolicy,
    ) -> Selected<fgdb_gql::algebra::GraphValueRow>
    + 'a,
) -> impl fgdb_gql::GraphSetSource<WriteTxnError, WriteTxnError> + 'a {
    fgdb_gql::with_element_metadata(
        fgdb_gql::WithElementReads {
            source: ScopedInputSource {
                patterns,
                procedures: move |call: &PreparedProcedureCall,
                                  arguments: &[GraphValue],
                                  policy| {
                    procedure(
                        transaction,
                        database,
                        cx,
                        call,
                        arguments,
                        scope,
                        policy,
                        execution,
                    )
                },
            },
            elements: move |element, key| {
                if !scope.allows_property(key) {
                    return Err(WriteTxnError::Authorization(Error::ScopeDenied));
                }
                returning_element_property(
                    transaction,
                    database,
                    cx,
                    element,
                    key,
                    scope,
                    execution,
                )
            },
        },
        move |element, kind, catalog, control| {
            returning_element_metadata(
                transaction,
                database,
                cx,
                element,
                kind,
                catalog,
                scope,
                execution,
                control,
            )
        },
    )
}

pub(super) fn returning_element_reader<'a, V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &'a WriteTxn,
    database: &'a Database<V>,
    cx: &'a QueryCx,
    scope: &'a PlannerPredicates,
    execution: &'a RefCell<&mut Execution<'_, '_, Clock>>,
) -> impl fgdb_gql::GraphElementReader<WriteTxnError> + 'a {
    fgdb_gql::with_element_metadata(
        move |element, key| {
            returning_element_property(transaction, database, cx, element, key, scope, execution)
        },
        move |element, kind, catalog, control| {
            returning_element_metadata(
                transaction,
                database,
                cx,
                element,
                kind,
                catalog,
                scope,
                execution,
                control,
            )
        },
    )
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
        false,
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
        false,
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

/// A joined MATCH can admit only its relational roots when the capability
/// hides no field or incidence. Unsupported definitions and restricted scopes
/// decline before source work; accepted attempts never retry on a fresh meter.
/// Each retained winner still spends the existing signed node allowance.
#[allow(clippy::too_many_arguments)]
pub(super) fn select_seeded_overlay<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &WriteTxn,
    database: &Database<V>,
    cx: &QueryCx,
    pattern: &PreparedGraphPattern<fgdb_gql::algebra::GraphValueRow>,
    seeds: fgdb_gql::GraphSeeds<'_>,
    scope: &PlannerPredicates,
    policy: GqlQueryPolicy,
    execution: &RefCell<&mut Execution<'_, '_, Clock>>,
) -> Option<Selected<fgdb_gql::algebra::GraphValueRow>> {
    if !scope.sees_all_incidence()
        || !scope.sees_all_fields()
        || !WriteTxn::supports_seeded_query(pattern.plan())
    {
        return None;
    }
    Some(with_overlay(
        transaction,
        database,
        cx,
        pattern.plan().reads_edges(),
        true,
        execution,
        |overlay| {
            let overlay = overlay.expect("seeded admission retains even an empty overlay");
            controlled(cx, execution, |node, poll, checkpoint| {
                database.select_for_authorized_overlay_with_seeds(
                    cx,
                    overlay,
                    pattern,
                    Some(seeds),
                    scope,
                    policy,
                    node,
                    poll,
                    checkpoint,
                )
            })
        },
    ))
}

type ProcedureFault = GqlQueryError<WriteTxnError, WriteTxnError>;

/// Every authorized write source shares this dispatch. Catalog calls retain
/// their existing symbol mask; graph algorithms use the canonical staged
/// projection below. Hybrid search has no authorized staged host yet.
#[allow(clippy::too_many_arguments)]
pub(super) fn procedure<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &WriteTxn,
    database: &Database<V>,
    cx: &QueryCx,
    call: &PreparedProcedureCall,
    arguments: &[GraphValue],
    scope: &PlannerPredicates,
    policy: GqlQueryPolicy,
    execution: &RefCell<&mut Execution<'_, '_, Clock>>,
) -> Option<Selected<GraphValueRow>> {
    if crate::query::is_hybrid(call) {
        return None;
    }
    if crate::query::is_catalog(call) {
        return Some(
            crate::query::catalog_procedure(
                call,
                arguments,
                policy,
                |symbol| match symbol {
                    fgdb_gql::GraphSymbol::Label(id) => scope.allows_label(id),
                    fgdb_gql::GraphSymbol::Relation(id) => scope.allows_relation(id),
                    fgdb_gql::GraphSymbol::Property(id) => scope.allows_property(id),
                },
                || {
                    cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                    execution.borrow_mut().poll()
                },
                |units| execution.borrow_mut().work(units),
            )
            .map_err(|error| error.map_source(|_| WriteTxnError::AuthorizedMutationRefused)),
        );
    }
    Some(procedure_overlay(
        transaction,
        database,
        cx,
        call,
        arguments,
        scope,
        policy,
        execution,
    ))
}

fn procedure_failure(error: FnxReadError<crate::ReadError, ProcedureFault>) -> ProcedureFault {
    match error.split_cancel::<WriteTxnError>() {
        Err(error) => error,
        Ok(error) => crate::query::procedure_failure(error).map_source(WriteTxnError::Gql),
    }
}

/// The same live write permit owns source admission, projection construction,
/// arithmetic and private result conversion. Native and signed work interleave
/// one unit at a time, so an earlier caller refusal cannot be hidden by a later
/// signed refusal or kernel error. Physical hidden-row scans use a separate,
/// charge-free cancellation poll.
struct ProcedureMeter<'a, 'run, 'cx, 'permit, Clock> {
    cx: &'a QueryCx,
    execution: &'a RefCell<&'run mut Execution<'cx, 'permit, Clock>>,
    usage: &'a RefCell<AdmissionUsage>,
    policy: GqlQueryPolicy,
}

impl<Clock: FnMut() -> u64> ProcedureMeter<'_, '_, '_, '_, Clock> {
    fn observe(&self, policy: GqlQueryPolicy, event: SourceEvent) -> Result<(), ProcedureFault> {
        self.cx
            .checkpoint()
            .map_err(WriteTxnError::Interrupted)
            .map_err(GqlQueryError::Interrupted)?;
        self.execution
            .borrow_mut()
            .checkpoint()
            .map_err(GqlQueryError::Interrupted)?;
        self.usage.borrow_mut().observe(policy, event)
    }
}

impl<Clock: FnMut() -> u64> FnxMeter<ProcedureFault> for ProcedureMeter<'_, '_, '_, '_, Clock> {
    fn unit(&mut self) -> Result<(), ProcedureFault> {
        self.observe(self.policy, SourceEvent::Work)
    }

    fn poll(&mut self) -> Result<(), ProcedureFault> {
        self.cx
            .checkpoint()
            .map_err(WriteTxnError::Interrupted)
            .map_err(GqlQueryError::Interrupted)?;
        let mut execution = self.execution.borrow_mut();
        execution.poll().map_err(GqlQueryError::Interrupted)?;
        let now = (execution.clock)();
        execution
            .permit
            .checkpoint_at(now)
            .map_err(WriteTxnError::Authorization)
            .map_err(GqlQueryError::Interrupted)
    }
}

/// Registered Prism reads over the capability-visible staged graph. This
/// borrows the same canonical effect owner as MATCH; it never reconstructs
/// mutations or calls the privileged transaction procedure host. Both full
/// table witnesses survive empty graphs, failed kernels and outer rollback.
///
/// Mask before user selectors, weight resolution and adjacency construction.
/// Signed/native resource counts depend only on admitted records; hidden
/// physical history still consumes host cancellation work. The projection is
/// private statement state, not a retained snapshot certificate or authority.
#[allow(clippy::too_many_arguments)]
fn procedure_overlay<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &WriteTxn,
    database: &Database<V>,
    cx: &QueryCx,
    call: &PreparedProcedureCall,
    arguments: &[GraphValue],
    scope: &PlannerPredicates,
    policy: GqlQueryPolicy,
    execution: &RefCell<&mut Execution<'_, '_, Clock>>,
) -> Selected<GraphValueRow> {
    if !scope.rights().can_read() {
        return Err(GqlQueryError::Source(WriteTxnError::Authorization(
            Error::PermissionDenied,
        )));
    }
    // The native binder rejects incompatible graph laws before source work.
    let spec = crate::query::bind_procedure(call, arguments)
        .map_err(|error| error.map_source(WriteTxnError::Gql))?;
    let options = spec.options_at(transaction.basis);
    with_overlay(
        transaction,
        database,
        cx,
        true,
        true,
        execution,
        |overlay| {
            let overlay = overlay.expect("procedure admission retains even an empty overlay");
            let usage = RefCell::new(AdmissionUsage::default());
            let mut meter = ProcedureMeter {
                cx,
                execution,
                usage: &usage,
                policy,
            };
            let mut source_policy = policy;
            source_policy.evaluator.max_work_units = source_policy
                .evaluator
                .max_work_units
                .min(options.source_limits.max_work_units);
            source_policy.evaluator.max_scratch_entries = source_policy
                .evaluator
                .max_scratch_entries
                .min(options.source_limits.max_scratch_entries);
            let mut poll = |_| {
                cx.checkpoint()
                    .map_err(WriteTxnError::Interrupted)
                    .map_err(GqlQueryError::Interrupted)?;
                execution
                    .borrow_mut()
                    .poll()
                    .map_err(GqlQueryError::Interrupted)
            };
            let mut vertices = Vec::new();
            let mut staging_bytes = 0;
            overlay.visit_vertices(VertexScanDomain::All, &mut poll, |row, _| {
                if !scope.allows_vertex(&row.labels) {
                    return Ok(());
                }
                meter.observe(source_policy, SourceEvent::Work)?;
                for &label in &row.labels {
                    if scope.allows_label(label) {
                        meter.observe(source_policy, SourceEvent::Work)?;
                    }
                }
                {
                    let mut execution = execution.borrow_mut();
                    execution.checkpoint().map_err(GqlQueryError::Interrupted)?;
                    let now = (execution.clock)();
                    execution
                        .permit
                        .charge_nodes_at(now, 1)
                        .map_err(WriteTxnError::Authorization)
                        .map_err(GqlQueryError::Interrupted)?;
                }
                meter.observe(source_policy, SourceEvent::SnapshotRecord)?;
                meter.observe(source_policy, SourceEvent::ScratchEntry)?;
                if options
                    .selection
                    .vertex_labels
                    .members()
                    .is_some_and(|labels| {
                        !labels.iter().any(|label| {
                            scope.allows_label(*label) && row.labels.binary_search(label).is_ok()
                        })
                    })
                {
                    return Ok(());
                }
                crate::query::push_prism_staged(
                    &mut vertices,
                    row.vid,
                    "vertices",
                    options.projection_limits.max_vertices,
                    &mut staging_bytes,
                    options.source_limits.max_staging_bytes,
                )
                .map_err(procedure_failure)
            })?;
            let mut edges = Vec::new();
            overlay
                .visit_edges(None, &mut poll, |entry, properties, _| {
                    if !scope.allows_relation(entry.relation)
                        || !options.selection.relations.contains(entry.relation)
                        || vertices.binary_search(&entry.src).is_err()
                        || vertices.binary_search(&entry.dst).is_err()
                    {
                        return Ok(());
                    }
                    meter.observe(source_policy, SourceEvent::Work)?;
                    meter.observe(source_policy, SourceEvent::SnapshotRecord)?;
                    meter.observe(source_policy, SourceEvent::ScratchEntry)?;
                    if entry.src == entry.dst {
                        match options.projection.self_loops {
                            SelfLoopPolicy::Drop => return Ok(()),
                            SelfLoopPolicy::Reject => {
                                return Err(procedure_failure(FnxReadError::Projection(
                                    ProjectionError::SelfLoop(entry.eid),
                                )));
                            }
                            SelfLoopPolicy::Keep => {}
                        }
                    }
                    let weight =
                        if options.projection.parallel_edges == ParallelEdgePolicy::CollapseUnit {
                            1.0
                        } else {
                            let value = options
                                .selection
                                .weight
                                .property_key()
                                .filter(|&key| scope.allows_property(key))
                                .and_then(|key| {
                                    properties.binary_search_by_key(&key, |(key, _)| *key).ok()
                                })
                                .map(|index| &properties[index].1);
                            options.selection.weight.resolve(value).map_err(|reason| {
                                procedure_failure(FnxReadError::Weight {
                                    edge: entry.eid,
                                    reason,
                                })
                            })?
                        };
                    crate::query::push_prism_staged(
                        &mut edges,
                        ProjectionEdge {
                            eid: entry.eid,
                            source: entry.src,
                            target: entry.dst,
                            weight,
                        },
                        "input edges",
                        options.projection_limits.max_input_edges,
                        &mut staging_bytes,
                        options.source_limits.max_staging_bytes,
                    )
                    .map_err(procedure_failure)
                })
                .expect("procedure overlay retains the edge table")?;
            let graph = SnapshotGraphView::build_owned_with_checkpoint(
                SnapshotBinding {
                    root: database.snapshot.root.0,
                    as_of: transaction.basis,
                },
                vertices,
                edges,
                options.projection,
                options.projection_limits,
                || meter.unit(),
            )
            .map_err(|error| match error {
                ProjectionBuildError::Cancelled(error) => error,
                ProjectionBuildError::Projection(error) => {
                    procedure_failure(FnxReadError::Projection(error))
                }
            })?;
            let remaining = usage.borrow().remaining(policy);
            let mut limits = options.execution_limits;
            limits.max_estimated_work = limits
                .max_estimated_work
                .min(usize::try_from(remaining.evaluator.max_work_units).unwrap_or(usize::MAX));
            // Intermediate CALL rows are private; the enclosing statement owns
            // signed row delivery. Admit their full conversion shape before the
            // kernel reserves its output, independently of the outer final page.
            limits.max_result_rows = limits.max_result_rows.min(
                usize::try_from(remaining.evaluator.max_scratch_entries).unwrap_or(usize::MAX)
                    / (spec.outputs().len() + 1),
            );
            let mut sink = crate::query::ProcedureRowSink::default();
            spec.execute_into(
                &graph,
                limits,
                ProcedureMeter {
                    cx,
                    execution,
                    usage: &usage,
                    policy,
                },
                &mut sink,
            )
            .map_err(|error| procedure_failure(FnxReadError::Execution(error)))?;
            for _ in 0..sink.row_count() {
                meter.observe(policy, SourceEvent::ScratchEntry)?;
                for _ in spec.outputs() {
                    meter.observe(policy, SourceEvent::ScratchEntry)?;
                }
            }
            let value = sink
                .finish()
                .map_err(|error| error.map_source(WriteTxnError::Gql))?;
            meter.poll()?;
            let usage = usage.into_inner();
            usage.finish(
                policy,
                Ok(GqlQueryExecution {
                    rows: GqlExecutionStats {
                        snapshot_records: usage.snapshot_records(),
                        result_rows: value.len() as u64,
                    },
                    evaluator: GlaExecutionStats::default(),
                    value,
                }),
            )
        },
    )
}

// One lifetime owner and identical preflight/error ordering for both physical
// forms. No borrowed source escapes the exclusively borrowed statement phase.
#[allow(clippy::too_many_arguments)]
fn with_overlay<V: Vfs + Clone, T, Clock: FnMut() -> u64>(
    transaction: &WriteTxn,
    database: &Database<V>,
    cx: &QueryCx,
    reads_edges: bool,
    retain_empty: bool,
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
    if transaction.staged.is_empty() && !retain_empty {
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
