// Relationship ON/SET actions over a chained write's carried rows. Hosts share
// the native occurrence collector and this controlled action evaluator; they
// own only admitted property reads, original-intent staging and final output.

/// One occurrence's complete target set, frozen before its actions. Preserve
/// the edge's own orientation for undirected matches and final projections.
pub(crate) struct EdgeUpsertRowTargets(
    pub(crate) Vec<(fgdb_gql::GraphEdgeMergeOutcome, (VId, VId))>,
);

impl EdgeMergeOutput for EdgeUpsertRowTargets {
    const PARALLEL: bool = true;

    fn for_pairs(pairs: usize) -> Option<Self> {
        (pairs <= 1).then(|| Self(Vec::new()))
    }

    fn record<C>(
        &mut self,
        outcome: fgdb_gql::GraphEdgeMergeOutcome,
        resolved: ResolvedEdge,
        control: &mut impl FnMut(Option<fgdb_gql::GlaExecutionEvent>) -> Result<(), C>,
    ) -> Result<(), C> {
        if outcome.edge().is_some() {
            control(Some(fgdb_gql::GlaExecutionEvent::ScratchEntry))?;
            self.0.push((outcome, resolved.endpoints));
        }
        Ok(())
    }

    fn result_rows(&self) -> u64 {
        self.0.len() as u64
    }
}

/// A carried pair shares its original row across parallel relationships. A
/// whole-pattern match has a separately resolved endpoint row per match.
/// These adapters move already admitted ownership without copying row payloads.
pub(crate) enum EdgeUpsertResolvedRows {
    Bound(fgdb_gql::algebra::GraphValueRow, EdgeUpsertRowTargets),
    Whole(Vec<fgdb_gql::GraphEdgeWholeMergeRow>),
}

pub(crate) enum EdgeUpsertResolvedTargets {
    Many(Vec<(fgdb_gql::GraphEdgeMergeOutcome, (VId, VId))>),
    One((fgdb_gql::GraphEdgeMergeOutcome, (VId, VId))),
}

impl EdgeUpsertResolvedRows {
    pub(crate) fn rows(
        self,
    ) -> impl Iterator<Item = (fgdb_gql::algebra::GraphValueRow, EdgeUpsertResolvedTargets)> {
        let (bound, whole) = match self {
            Self::Bound(row, targets) => (
                Some((row, EdgeUpsertResolvedTargets::Many(targets.0))),
                None,
            ),
            Self::Whole(rows) => (None, Some(rows)),
        };
        bound
            .into_iter()
            .chain(whole.into_iter().flatten().map(|row| {
                (
                    row.input,
                    EdgeUpsertResolvedTargets::One((row.outcome, row.endpoints)),
                )
            }))
    }
}

impl EdgeUpsertResolvedTargets {
    pub(crate) fn as_slice(&self) -> &[(fgdb_gql::GraphEdgeMergeOutcome, (VId, VId))] {
        match self {
            Self::Many(targets) => targets,
            Self::One(target) => std::slice::from_ref(target),
        }
    }

    pub(crate) fn targets(
        self,
    ) -> impl Iterator<Item = (fgdb_gql::GraphEdgeMergeOutcome, (VId, VId))> {
        let (many, one) = match self {
            Self::Many(targets) => (Some(targets), None),
            Self::One(target) => (None, Some(target)),
        };
        many.into_iter().flatten().chain(one)
    }
}

/// The whole-pattern collector needs only native pattern leaves and controlled
/// point fields. Its source reserves the selected payload before returning an
/// owned value; it does not advertise unrelated procedure/metadata capabilities.
pub(crate) struct WholeEdgeMergeSource<P, R> {
    pub(crate) patterns: P,
    pub(crate) properties: R,
}

impl<P, R, E, C> fgdb_gql::GraphSetSource<E, C> for WholeEdgeMergeSource<P, R>
where
    P: FnMut(
        &fgdb_gql::algebra::PreparedGraphPattern<fgdb_gql::algebra::GraphValueRow>,
        fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
        fgdb_gql::GqlQueryError<E, C>,
    >,
    R: FnMut(
        ElementId,
        fgdb_delta_types::PropertyKeyId,
        &mut dyn FnMut(fgdb_gql::GlaExecutionEvent) -> Result<(), ()>,
    ) -> Result<Option<CanonicalPropertyValue>, fgdb_gql::GraphElementPropertyError<E>>,
{
    fn pattern(
        &mut self,
        pattern: &fgdb_gql::algebra::PreparedGraphPattern<fgdb_gql::algebra::GraphValueRow>,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
        fgdb_gql::GqlQueryError<E, C>,
    > {
        (self.patterns)(pattern, policy)
    }

    fn element_property_with_control(
        &mut self,
        element: ElementId,
        key: fgdb_delta_types::PropertyKeyId,
        control: &mut dyn FnMut(fgdb_gql::GlaExecutionEvent) -> Result<(), ()>,
    ) -> Option<Result<Option<CanonicalPropertyValue>, fgdb_gql::GraphElementPropertyError<E>>>
    {
        Some((self.properties)(element, key, control))
    }
}

fn whole_edge_merge_property<V: Vfs + Clone>(
    transaction: &WriteTxn,
    database: &Database<V>,
    element: ElementId,
    key: fgdb_delta_types::PropertyKeyId,
    control: &mut dyn FnMut(fgdb_gql::GlaExecutionEvent) -> Result<(), ()>,
) -> Result<Option<CanonicalPropertyValue>, fgdb_gql::GraphElementPropertyError<WriteTxnError>> {
    use crate::gql_exec::source::SourceEvent;
    use fgdb_gql::{GlaExecutionEvent, GraphElementPropertyError};
    let mut control = |event| {
        control(match event {
            SourceEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
            SourceEvent::Work | SourceEvent::SnapshotRecord => GlaExecutionEvent::Work,
        })
        .map_err(|()| GraphElementPropertyError::Interrupted)
    };
    control(SourceEvent::Work)?;
    let _ = transaction
        .query_snapshot(database)
        .map_err(GraphElementPropertyError::Source)?;
    let mut attempt = ProjectionReadAttempt {
        reads: &transaction.point_reads,
        element,
        accepted: false,
    };
    let field = PointReadField::Property(key);
    let projected = transaction.point_projection_with_control(
        database,
        element,
        field,
        &mut control,
        &GraphElementPropertyError::Source,
    )?;
    if let Some(value) = projected.property {
        point_payload_admission(value, &mut control)?;
    }
    control(SourceEvent::ScratchEntry)?;
    control(SourceEvent::ScratchEntry)?;
    transaction.point_reads.borrow_mut().record(element, field);
    control(SourceEvent::ScratchEntry)?;
    let value = projected.property.cloned();
    attempt.accepted = true;
    Ok(value)
}

struct EdgeMergeRowElementReader<F>(F);

impl<E, F> fgdb_gql::GraphElementReader<E> for EdgeMergeRowElementReader<F>
where
    F: FnMut(
        Option<usize>,
        ElementId,
        fgdb_delta_types::PropertyKeyId,
    ) -> Result<Option<CanonicalPropertyValue>, E>,
{
    fn element_property(
        &mut self,
        element: ElementId,
        key: fgdb_delta_types::PropertyKeyId,
    ) -> Result<Option<CanonicalPropertyValue>, E> {
        (self.0)(None, element, key)
    }

    fn element_property_for_column(
        &mut self,
        column: usize,
        element: ElementId,
        key: fgdb_delta_types::PropertyKeyId,
    ) -> Result<Option<CanonicalPropertyValue>, E> {
        (self.0)(Some(column), element, key)
    }
}

/// A copied map's reader (fgdb-at06q): the row's point reads, and the
/// properties of an element the map copies (`n = a`, `r = a`), at the same
/// cut. Vertex and relationship MERGE use this same reader adapter.
struct MergeRowMapReader<F, M>(F, M);

impl<E, F, M> fgdb_gql::GraphElementReader<E> for MergeRowMapReader<F, M>
where
    F: FnMut(
        Option<usize>,
        ElementId,
        fgdb_delta_types::PropertyKeyId,
    ) -> Result<Option<CanonicalPropertyValue>, E>,
    M: FnMut(
        ElementId,
        fgdb_gql::GraphElementMetadata,
        &fgdb_gql::ReverseSymbolCatalog,
        &mut dyn FnMut(fgdb_gql::GlaExecutionEvent) -> Result<(), ()>,
    ) -> Result<
        Option<fgdb_gql::algebra::GraphValue>,
        fgdb_gql::GraphElementMetadataError<E>,
    >,
{
    fn element_property(
        &mut self,
        element: ElementId,
        key: fgdb_delta_types::PropertyKeyId,
    ) -> Result<Option<CanonicalPropertyValue>, E> {
        (self.0)(None, element, key)
    }

    fn element_property_for_column(
        &mut self,
        column: usize,
        element: ElementId,
        key: fgdb_delta_types::PropertyKeyId,
    ) -> Result<Option<CanonicalPropertyValue>, E> {
        (self.0)(Some(column), element, key)
    }

    fn element_metadata(
        &mut self,
        element: ElementId,
        kind: fgdb_gql::GraphElementMetadata,
        catalog: &fgdb_gql::ReverseSymbolCatalog,
        control: &mut dyn FnMut(fgdb_gql::GlaExecutionEvent) -> Result<(), ()>,
    ) -> Option<Result<Option<fgdb_gql::algebra::GraphValue>, fgdb_gql::GraphElementMetadataError<E>>>
    {
        Some((self.1)(element, kind, catalog, control))
    }
}

/// Admit one copied map completely before staging any of its entries.
/// The host supplies the target's visible keys at this clause's cut; both
/// vertex and relationship actions use the same catalog, payload and
/// cumulative effect accounting. NULL entries retain ordinary unset intent.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn merge_row_property_map<E>(
    map: &fgdb_gql::algebra::GraphValue,
    names: &[(Box<str>, fgdb_delta_types::PropertyKeyId)],
    replace: bool,
    mut keys: impl FnMut(
        &mut dyn FnMut(fgdb_gql::GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Vec<fgdb_delta_types::PropertyKeyId>, E>,
    (max_effects, effects): (u64, &mut u64),
    control: &mut dyn FnMut(fgdb_gql::GlaExecutionEvent) -> Result<(), E>,
    invalid: impl Fn() -> E,
    exceeded: impl Fn(u128) -> E,
) -> Result<
    Vec<(
        fgdb_delta_types::PropertyKeyId,
        Option<CanonicalPropertyValue>,
    )>,
    E,
> {
    use crate::gql_exec::source::SourceEvent;
    use fgdb_gql::GlaExecutionEvent;
    let Some((entries, values)) = map.as_map() else {
        return Err(invalid());
    };
    let mut written = Vec::new();
    for (name, entry) in entries.iter().zip(values) {
        control(GlaExecutionEvent::Work)?;
        let key = names
            .binary_search_by(|(known, _)| (**known).cmp(&**name))
            .map(|at| names[at].1)
            .map_err(|_| invalid())?;
        let value = entry.to_written_property().ok_or_else(&invalid)?;
        point_payload_admission(&value, &mut |event| {
            control(match event {
                SourceEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
                _ => GlaExecutionEvent::Work,
            })
        })?;
        control(GlaExecutionEvent::ScratchEntry)?;
        written.push((key, value));
    }
    // Charge before allocating every removal. Entry assignments also count
    // when their value is NULL or already equals the stored value.
    let mut charge = |control: &mut dyn FnMut(GlaExecutionEvent) -> Result<(), E>| {
        let observed = u128::from(*effects) + 1;
        if observed > u128::from(max_effects) {
            return Err(exceeded(observed));
        }
        control(GlaExecutionEvent::ScratchEntry)?;
        *effects = observed as u64;
        Ok(())
    };
    let mut proposals = Vec::new();
    if replace {
        for key in keys(control)? {
            control(GlaExecutionEvent::Work)?;
            if written.iter().any(|(entry, _)| *entry == key) {
                continue;
            }
            charge(control)?;
            proposals.push((key, None));
        }
    }
    for (key, value) in written {
        charge(control)?;
        proposals.push((key, Some(value)));
    }
    Ok(proposals)
}

/// Evaluate the selected ON clause and trailing SET for one resolved edge.
/// The caller admits the cumulative effect count first. Every RHS of one
/// clause completes before its original assignments reach the staging host.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub(crate) fn edge_merge_row_actions_controlled<E, A, C>(
    (relation, edge): (RelationId, EId),
    (at, row): (usize, &fgdb_gql::algebra::GraphValueRow),
    clauses: [&[fgdb_gql::GraphEdgeMergeRowAction]; 2],
    policy: fgdb_gql::GqlQueryPolicy,
    evaluator: &mut fgdb_gql::GlaExecutionStats,
    (max_effects, effects): (u64, &mut u64),
    checkpoint: impl FnMut() -> Result<(), C>,
    mut read: impl FnMut(
        fgdb_delta_types::PropertyKeyId,
        &mut dyn FnMut(fgdb_gql::GlaExecutionEvent) -> EdgeUpsertActionResult<(), E, A, C>,
    ) -> EdgeUpsertActionResult<fgdb_gql::algebra::GraphValue, E, A, C>,
    mut read_row: impl FnMut(
        Option<usize>,
        ElementId,
        fgdb_delta_types::PropertyKeyId,
        &mut dyn FnMut(fgdb_gql::GlaExecutionEvent) -> EdgeUpsertActionResult<(), E, A, C>,
    ) -> EdgeUpsertActionResult<Option<CanonicalPropertyValue>, E, A, C>,
    mut keys: impl FnMut(
        &mut dyn FnMut(fgdb_gql::GlaExecutionEvent) -> EdgeUpsertActionResult<(), E, A, C>,
    ) -> EdgeUpsertActionResult<Vec<fgdb_delta_types::PropertyKeyId>, E, A, C>,
    mut metadata: impl FnMut(
        ElementId,
        fgdb_gql::GraphElementMetadata,
        &fgdb_gql::ReverseSymbolCatalog,
        &mut dyn FnMut(fgdb_gql::GlaExecutionEvent) -> Result<(), ()>,
    ) -> Result<
        Option<fgdb_gql::algebra::GraphValue>,
        fgdb_gql::GraphElementMetadataError<
            fgdb_gql::GqlQueryError<fgdb_gql::GraphEdgeUpsertError<E, A>, C>,
        >,
    >,
    mut stage: impl FnMut(WriteBatch) -> EdgeUpsertActionResult<(), E, A, C>,
) -> EdgeUpsertActionResult<(), E, A, C> {
    use crate::gql_exec::source::SourceEvent;
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::{
        GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GqlQueryError, GraphEdgeMergeError,
        GraphEdgeMergeRowAction, GraphEdgeMergeRowInput, GraphEdgeUpsertError,
        GraphEdgeUpsertValue, GraphIntegerEvaluationError,
    };
    let checkpoint = std::cell::RefCell::new(checkpoint);
    let mut control = |event| -> EdgeUpsertActionResult<(), E, A, C> {
        checkpoint.borrow_mut()().map_err(GqlQueryError::Interrupted)?;
        let work = u128::from(evaluator.work_units) + 1;
        let scratch = u128::from(evaluator.scratch_entries)
            + u128::from(event == GlaExecutionEvent::ScratchEntry);
        for (observed, limit, dimension) in [
            (
                work,
                policy.evaluator.max_work_units,
                GlaLimitDimension::WorkUnits,
            ),
            (
                scratch,
                policy.evaluator.max_scratch_entries,
                GlaLimitDimension::ScratchEntries,
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
        evaluator.work_units = work as u64;
        evaluator.scratch_entries = scratch as u64;
        Ok(())
    };
    let payload = |event| match event {
        SourceEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
        _ => GlaExecutionEvent::Work,
    };
    for (clause, actions) in clauses.into_iter().enumerate() {
        if actions.is_empty() {
            continue;
        }
        let mut batch = WriteBatch::new(relation);
        for (action, definition) in actions.iter().enumerate() {
            control(GlaExecutionEvent::Work)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            let expression = |source| {
                GqlQueryError::Source(GraphEdgeUpsertError::Expression {
                    clause,
                    action,
                    source,
                })
            };
            // The vertex a row-vertex assignment writes (fgdb-at06q). A NULL
            // one, from OPTIONAL MATCH, takes no assignment, as SET on NULL.
            let vertex = match definition.vertex_column() {
                None => None,
                Some(column) => match row.values().get(column) {
                    Some(GraphValue::Vertex(vertex)) => Some(*vertex),
                    Some(GraphValue::Scalar(CanonicalScalar::Null)) => continue,
                    _ => {
                        return Err(GqlQueryError::Source(GraphEdgeUpsertError::Merge(
                            GraphEdgeMergeError::InputSchema { row: at, column },
                        )));
                    }
                },
            };
            let (key, written) = match definition {
                GraphEdgeMergeRowAction::ClearProperties { keep } => {
                    for key in keys(&mut control)? {
                        control(GlaExecutionEvent::Work)?;
                        if keep.binary_search(&key).is_ok() {
                            continue;
                        }
                        let observed = u128::from(*effects) + 1;
                        if observed > u128::from(max_effects) {
                            return Err(GqlQueryError::Source(GraphEdgeUpsertError::ActionLimit {
                                limit: max_effects,
                                observed,
                            }));
                        }
                        control(GlaExecutionEvent::ScratchEntry)?;
                        *effects = observed as u64;
                        batch.set_edge_property(edge, key, None);
                    }
                    continue;
                }
                GraphEdgeMergeRowAction::Properties {
                    value,
                    replace,
                    names,
                } => {
                    let invalid = || {
                        GqlQueryError::Source(GraphEdgeUpsertError::PropertyMap { clause, action })
                    };
                    // The whole map completes before any of its writes.
                    let map = {
                        let controls = std::cell::RefCell::new(&mut control);
                        value
                            .value_with_elements(
                                row,
                                &mut |event| controls.borrow_mut()(event),
                                &mut MergeRowMapReader(
                                    |column, element, property| {
                                        read_row(column, element, property, &mut |event| {
                                            controls.borrow_mut()(event)
                                        })
                                    },
                                    &mut metadata,
                                ),
                            )
                            .map_err(|error| match error {
                                GraphIntegerEvaluationError::Control(error) => error,
                                GraphIntegerEvaluationError::Value(error) => expression(error),
                            })?
                    };
                    for (key, value) in merge_row_property_map(
                        &map,
                        names,
                        *replace,
                        &mut keys,
                        (max_effects, effects),
                        &mut control,
                        invalid,
                        |observed| {
                            GqlQueryError::Source(GraphEdgeUpsertError::ActionLimit {
                                limit: max_effects,
                                observed,
                            })
                        },
                    )? {
                        batch.set_edge_property_value(edge, key, value);
                    }
                    continue;
                }
                GraphEdgeMergeRowAction::Edge(definition) => {
                    let written = match &definition.value {
                        GraphEdgeUpsertValue::Literal(value) => {
                            point_scalar_payload_admission(value.value(), &mut |event| {
                                control(payload(event))
                            })?;
                            CanonicalPropertyValue::Scalar(value.value().clone())
                        }
                        GraphEdgeUpsertValue::Stored(value) => {
                            point_payload_admission(value, &mut |event| control(payload(event)))?;
                            value.clone()
                        }
                        GraphEdgeUpsertValue::Expression { .. }
                        | GraphEdgeUpsertValue::Composite { .. } => {
                            return Err(GqlQueryError::Source(GraphEdgeUpsertError::Merge(
                                GraphEdgeMergeError::InputSchema {
                                    row: at,
                                    column: action,
                                },
                            )));
                        }
                    };
                    (definition.key, written)
                }
                GraphEdgeMergeRowAction::Expression { key, inputs, value } => {
                    let mut frame = Vec::new();
                    for input in inputs {
                        control(GlaExecutionEvent::ScratchEntry)?;
                        frame.push(match *input {
                            GraphEdgeMergeRowInput::Property(key) => {
                                control(GlaExecutionEvent::Work)?;
                                read(key, &mut control)?
                            }
                            GraphEdgeMergeRowInput::Column(column) => row
                                .values()
                                .get(column)
                                .ok_or(GqlQueryError::Source(GraphEdgeUpsertError::Merge(
                                    GraphEdgeMergeError::InputSchema { row: at, column },
                                )))?
                                .copy_with_control(&mut control)?,
                        });
                    }
                    let value = value
                        .evaluate_scalar_with_control(&frame, &mut control)
                        .map_err(|error| match error {
                            GraphIntegerEvaluationError::Control(error) => error,
                            GraphIntegerEvaluationError::Value(error) => expression(error),
                        })?;
                    (*key, CanonicalPropertyValue::Scalar(value))
                }
                GraphEdgeMergeRowAction::Value { key, value }
                | GraphEdgeMergeRowAction::RowVertex { key, value, .. } => {
                    // Both callbacks spend the same evaluator allowance. The
                    // host's row reader retains the input cut, whereas `read`
                    // above reads the resolved edge at this ON/SET clause.
                    let controls = std::cell::RefCell::new(&mut control);
                    let written = value
                        .written_with_elements(
                            row,
                            &mut |event| controls.borrow_mut()(event),
                            &mut EdgeMergeRowElementReader(|column, element, property| {
                                read_row(column, element, property, &mut |event| {
                                    controls.borrow_mut()(event)
                                })
                            }),
                        )
                        .map_err(|error| match error {
                            GraphIntegerEvaluationError::Control(error) => error,
                            GraphIntegerEvaluationError::Value(error) => expression(error),
                        })?
                        .ok_or(GqlQueryError::Source(
                            GraphEdgeUpsertError::NonScalarValue { clause, action },
                        ))?;
                    point_payload_admission(&written, &mut |event| control(payload(event)))?;
                    (*key, written)
                }
            };
            match vertex {
                Some(vertex) => batch.set_vertex_property_value(vertex, key, Some(written)),
                None => batch.set_edge_property_value(edge, key, Some(written)),
            };
        }
        checkpoint.borrow_mut()().map_err(GqlQueryError::Interrupted)?;
        // A replacement with nothing left to unset, or assignments to a NULL
        // row vertex, write nothing; the clause still read its edge, and an
        // empty batch would refuse as a commit for nothing.
        if !batch.is_empty() {
            stage(batch)?;
        }
    }
    control(GlaExecutionEvent::Work)
}

/// One stored field through the native projection/witness owner. Unlike the
/// scalar-only standalone action API, row expressions may inspect a stored
/// list using the shared read-value language; no value is copied before its
/// complete payload has been admitted.
fn edge_upsert_row_property<V: Vfs + Clone, A, C>(
    transaction: &WriteTxn,
    database: &Database<V>,
    element: ElementId,
    key: fgdb_delta_types::PropertyKeyId,
    control: &mut dyn FnMut(
        fgdb_gql::GlaExecutionEvent,
    ) -> EdgeUpsertActionResult<(), WriteTxnError, A, C>,
) -> EdgeUpsertActionResult<fgdb_gql::algebra::GraphValue, WriteTxnError, A, C> {
    use fgdb_gql::algebra::GraphValue;
    edge_upsert_row_property_with(transaction, database, element, key, control, |value| {
        Ok(match value {
            None => GraphValue::Scalar(CanonicalScalar::Null),
            Some(value) => GraphValue::from_property_value(value).ok_or(
                fgdb_gql::GqlQueryError::Data(fgdb_gql::GraphIntegerError {
                    instruction: 0,
                    kind: fgdb_gql::GraphIntegerErrorKind::StoredValueBounds,
                }),
            )?,
        })
    })
}

/// Admit the selected stored payload and its witness before the caller copies
/// it into its native value domain. The row reader needs an owned stored value;
/// an edge expression needs GraphValue. Neither copies through the other one.
fn edge_upsert_row_property_with<V: Vfs + Clone, A, C, T>(
    transaction: &WriteTxn,
    database: &Database<V>,
    element: ElementId,
    key: fgdb_delta_types::PropertyKeyId,
    control: &mut dyn FnMut(
        fgdb_gql::GlaExecutionEvent,
    ) -> EdgeUpsertActionResult<(), WriteTxnError, A, C>,
    copy: impl FnOnce(Option<&CanonicalPropertyValue>) -> EdgeUpsertActionResult<T, WriteTxnError, A, C>,
) -> EdgeUpsertActionResult<T, WriteTxnError, A, C> {
    merge_row_property_with(
        transaction,
        database,
        element,
        key,
        control,
        |error| fgdb_gql::GqlQueryError::Source(fgdb_gql::GraphEdgeUpsertError::Staging(error)),
        copy,
    )
}

/// One governed row-element field, shared by vertex and relationship MERGE.
fn merge_row_property_with<V: Vfs + Clone, E, T>(
    transaction: &WriteTxn,
    database: &Database<V>,
    element: ElementId,
    key: fgdb_delta_types::PropertyKeyId,
    control: &mut dyn FnMut(fgdb_gql::GlaExecutionEvent) -> Result<(), E>,
    source: impl Fn(WriteTxnError) -> E,
    copy: impl FnOnce(Option<&CanonicalPropertyValue>) -> Result<T, E>,
) -> Result<T, E> {
    use crate::gql_exec::source::SourceEvent;
    use fgdb_gql::GlaExecutionEvent;
    control(GlaExecutionEvent::Work)?;
    let field = PointReadField::Property(key);
    let mut attempt = ProjectionReadAttempt {
        reads: &transaction.point_reads,
        element,
        accepted: false,
    };
    let mut source_control = |event| {
        control(match event {
            SourceEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
            SourceEvent::Work | SourceEvent::SnapshotRecord => GlaExecutionEvent::Work,
        })
    };
    let projected = transaction.point_projection_with_control(
        database,
        element,
        field,
        &mut source_control,
        &source,
    )?;
    if let Some(value) = projected.property {
        point_payload_admission(value, &mut source_control)?;
    }
    source_control(SourceEvent::ScratchEntry)?;
    source_control(SourceEvent::ScratchEntry)?;
    transaction.point_reads.borrow_mut().record(element, field);
    source_control(SourceEvent::ScratchEntry)?;
    let value = copy(projected.property)?;
    attempt.accepted = true;
    Ok(value)
}

/// Borrow a resolved edge's visible keys at the current clause cut. The
/// complete-element witness covers newly inserted keys as well as removals;
/// hidden fields are never copied or charged to an authorized observer.
fn edge_replacement_keys<V: Vfs + Clone, E>(
    transaction: &WriteTxn,
    database: &Database<V>,
    edge: EId,
    scope: Option<&fgdb_warden::PlannerPredicates>,
    poll: impl FnMut() -> Result<(), E>,
    control: &mut dyn FnMut(fgdb_gql::GlaExecutionEvent) -> Result<(), E>,
    source_error: impl Fn(WriteTxnError) -> E,
) -> Result<Vec<fgdb_delta_types::PropertyKeyId>, E> {
    use crate::gql_exec::source::SourceEvent;
    use fgdb_gql::GlaExecutionEvent;
    let poll = std::cell::RefCell::new(poll);
    let mut visible = |key| scope.is_none_or(|scope| scope.allows_property(key));
    let element = ElementId::Edge(edge);
    let mut attempt = ProjectionReadAttempt {
        reads: &transaction.point_reads,
        element,
        accepted: false,
    };
    let projected = {
        let mut source_control = |event| match event {
            SourceEvent::ScratchEntry => control(GlaExecutionEvent::ScratchEntry),
            SourceEvent::Work | SourceEvent::SnapshotRecord if scope.is_some() => {
                poll.borrow_mut()()
            }
            SourceEvent::Work | SourceEvent::SnapshotRecord => control(GlaExecutionEvent::Work),
        };
        let projected = transaction.edge_metadata_with_control(
            database,
            edge,
            &mut visible,
            &mut || poll.borrow_mut()(),
            &mut source_control,
            &source_error,
        )?;
        source_control(SourceEvent::ScratchEntry)?;
        transaction.read_set.borrow_mut().insert(element);
        projected
    };
    let mut keys = Vec::new();
    if let Some(projected) = projected {
        for (key, _) in projected.properties.effective_properties() {
            poll.borrow_mut()()?;
            if !visible(key) {
                continue;
            }
            control(GlaExecutionEvent::Work)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            keys.push(key);
        }
    }
    attempt.accepted = true;
    Ok(keys)
}

type EdgeUpsertChainResult<A> = Result<
    (
        fgdb_gql::insertion::GraphInsertStats,
        fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
        Option<fgdb_gql::GraphWriteStepReceipt>,
        u64,
    ),
    TxnGqlError<fgdb_gql::GraphWriteChainError<WriteTxnError, A>>,
>;

impl WriteTxn {
    /// One relationship action step inside the chain's existing rollback
    /// boundary. Select its input once, resolve each occurrence against prior
    /// effects, then read every continuation/RETURN from the final overlay.
    #[allow(clippy::too_many_arguments)]
    fn execute_graph_edge_upsert_chain_step<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        step: &fgdb_gql::GraphWriteChainStep,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
        owned: Option<Vec<fgdb_gql::algebra::GraphValueRow>>,
        mut allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> EdgeUpsertChainResult<A> {
        use fgdb_gql::algebra::{GraphValue, GraphValueRow};
        use fgdb_gql::insertion::{GraphInsertRequest, GraphInsertStats};
        use fgdb_gql::{
            GlaExecutionEvent, GqlExecutionStats, GqlQueryError, GqlQueryExecution,
            GraphEdgeMergeBinding, GraphEdgeMergeError, GraphEdgeMergeOutcome,
            GraphEdgeMergePolicy, GraphEdgeUpsertBranch, GraphEdgeUpsertError,
            GraphEdgeUpsertQueryError, GraphMutationError, GraphMutationQueryError,
            GraphWriteChainError, GraphWriteChainStep, GraphWriteStepReceipt,
        };
        let chained =
            |error| GraphWriteChainError::EdgeUpsert(GraphEdgeUpsertQueryError::Upsert(error));
        let invalid = || {
            GqlQueryError::Source(chained(GraphEdgeUpsertError::Merge(
                GraphEdgeMergeError::InvalidSourceStatistics,
            )))
        };
        let upsert = match step {
            GraphWriteChainStep::EdgeUpsert(upsert)
            | GraphWriteChainStep::EdgeUpsertRows { upsert, .. }
            | GraphWriteChainStep::EdgeUpsertQuery { upsert, .. } => upsert,
            _ => return Err(invalid()),
        };
        let merge = upsert.merge();
        // Computed row properties keep the selection's immutable preimage,
        // even when an earlier occurrence changes one of those properties.
        let input_prepared = self.prepared.clone();
        if let GraphWriteChainStep::EdgeUpsertQuery { query, .. } = step
            && query.merge() != merge
        {
            return Err(invalid());
        }
        // Private input/target occurrences are never delivered result rows.
        let mut private = policy.query;
        private.rows = match private.rows.max_snapshot_records() {
            Some(limit) => fgdb_gql::GqlExecutionBudget::snapshot_records(limit),
            None => fgdb_gql::GqlExecutionBudget::UNLIMITED,
        };
        // A MATCH input may read a carried element it does not rematch (`a.p`
        // of a WITH vertex), as the vertex MERGE input does.
        let failure = std::cell::RefCell::new(None);
        let selected = {
            let mut source = fgdb_gql::with_element_metadata(
                fgdb_gql::WithElementReads {
                    source: |pattern: &fgdb_gql::algebra::PreparedGraphPattern<
                        fgdb_gql::algebra::GraphValueRow,
                    >,
                             allowance| {
                        self.execute_graph_pattern_governed(database, cx, pattern, allowance)
                    },
                    elements: |element, key| {
                        self.element_property_with_checkpoint(database, cx, &failure, element, key)
                    },
                },
                |element, kind, catalog, control| {
                    self.element_metadata_with_checkpoint(
                        database, cx, &failure, element, kind, catalog, control,
                    )
                },
            );
            merge.select_governed_with_source::<WriteTxnError, A, _>(
                owned,
                private,
                &mut source,
                || match failure.borrow_mut().take() {
                    Some(error) => Err(error),
                    None => cx.checkpoint(),
                },
            )
        };
        let selected = match failure.into_inner() {
            Some(error) => Err(GqlQueryError::Interrupted(error)),
            None => selected,
        }
        .map_err(|error| error.map_source(|error| chained(GraphEdgeUpsertError::Merge(error))))?;
        if u64::try_from(selected.value.len()).ok() != Some(selected.rows.result_rows) {
            return Err(invalid());
        }
        let mut stats = GraphInsertStats {
            selection: GqlExecutionStats {
                snapshot_records: selected.rows.snapshot_records,
                result_rows: 0,
            },
            evaluator: selected.evaluator,
            created_vertices: 0,
            created_edges: 0,
        };
        let retain_input = !matches!(step, GraphWriteChainStep::EdgeUpsert(_));
        let retain_receipt = !matches!(step, GraphWriteChainStep::EdgeUpsertQuery { .. });
        let mut input = Vec::new();
        let mut pairs = Vec::new();
        let mut outcomes = Vec::new();
        let mut created = Vec::new();
        let mut effects = 0_u64;
        for (at, row) in selected.value.into_iter().enumerate() {
            // Transfer the already admitted row through the real collector;
            // do not deep-copy a carried map to construct a singleton source.
            vertex_targets_event::<GraphWriteChainError<WriteTxnError, A>>(
                cx,
                private,
                &mut stats.evaluator,
                GlaExecutionEvent::ScratchEntry,
            )?;
            let resolved = if merge.has_unbound_endpoints() {
                let proposal = {
                    let mut source = WholeEdgeMergeSource {
                        patterns: |pattern: &fgdb_gql::algebra::PreparedGraphPattern<
                            GraphValueRow,
                        >,
                                   allowance| {
                            self.execute_graph_pattern_governed(database, cx, pattern, allowance)
                        },
                        properties: |element,
                                     key,
                                     control: &mut dyn FnMut(
                            fgdb_gql::GlaExecutionEvent,
                        )
                            -> Result<(), ()>| {
                            whole_edge_merge_property(self, database, element, key, control)
                        },
                    };
                    merge.resolve_whole_row(
                        row,
                        at,
                        fgdb_gql::insertion::GraphInsertPolicy {
                            query: private,
                            ..policy
                        },
                        stats,
                        &mut source,
                        &mut allocate,
                        || cx.checkpoint(),
                    )
                }
                .map_err(|error| {
                    error.map_source(|error| chained(GraphEdgeUpsertError::Merge(error)))
                })?;
                stats = proposal.stats;
                if let Some(creation) = proposal.creation {
                    let (_, batches, _, _) = graph_insert_batches_with_control(
                        merge.relation(),
                        creation,
                        false,
                        || cx.checkpoint(),
                    )
                    .map_err(GqlQueryError::Interrupted)?;
                    for batch in batches {
                        self.stage_edge_upsert_row_batch(database, batch)
                            .map_err(|error| {
                                GqlQueryError::Source(chained(GraphEdgeUpsertError::Staging(error)))
                            })?;
                    }
                }
                EdgeUpsertResolvedRows::Whole(proposal.rows)
            } else {
                let remaining = policy
                    .max_edges
                    .checked_sub(stats.created_edges)
                    .ok_or_else(invalid)?;
                let proposal =
                    collect_edge_merge_output::<EdgeUpsertRowTargets, WriteTxnError, A, _>(
                        merge,
                        GraphEdgeMergePolicy::new(private).with_creation_limit(remaining),
                        |_| {
                            Ok(GqlQueryExecution {
                                value: vec![row],
                                rows: GqlExecutionStats {
                                    snapshot_records: stats.selection.snapshot_records,
                                    result_rows: 1,
                                },
                                evaluator: stats.evaluator,
                            })
                        },
                        |source, destination| {
                            self.merge_candidate_edges(database, merge, source, destination, || {
                                cx.checkpoint()
                            })
                        },
                        |_| allocate(GraphInsertRequest::Edge { row: at, edge: 0 }),
                        || cx.checkpoint(),
                    )
                    .map_err(|error| {
                        error.map_source(|error| {
                            chained(GraphEdgeUpsertError::Merge(match error {
                                GraphEdgeMergeError::CreationLimit { observed, .. } => {
                                    GraphEdgeMergeError::CreationLimit {
                                        limit: policy.max_edges,
                                        observed: u128::from(stats.created_edges) + observed,
                                    }
                                }
                                GraphEdgeMergeError::InputSchema { column, .. } => {
                                    GraphEdgeMergeError::InputSchema { row: at, column }
                                }
                                GraphEdgeMergeError::NullEndpoint { column, .. } => {
                                    GraphEdgeMergeError::NullEndpoint { row: at, column }
                                }
                                GraphEdgeMergeError::RowValue { property, .. } => {
                                    GraphEdgeMergeError::RowValue { row: at, property }
                                }
                                GraphEdgeMergeError::RowExpression {
                                    property, source, ..
                                } => GraphEdgeMergeError::RowExpression {
                                    row: at,
                                    property,
                                    source,
                                },
                                error => error,
                            }))
                        })
                    })?;
                let EdgeMergeProposal {
                    stats: merged,
                    outcome,
                    creation,
                    mut selected,
                } = proposal;
                stats.selection.snapshot_records = merged
                    .match_selection
                    .snapshot_records
                    .checked_add(merged.overlay_edges)
                    .ok_or_else(invalid)?;
                stats.created_edges = stats
                    .created_edges
                    .checked_add(merged.created_edges)
                    .ok_or_else(invalid)?;
                stats.evaluator = merged.evaluator;
                if selected.len() != 1 {
                    return Err(invalid());
                }
                let row = selected.pop().ok_or_else(invalid)?;
                if let Some(batch) = creation {
                    cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                    self.stage_edge_upsert_row_batch(database, batch)
                        .map_err(|error| {
                            GqlQueryError::Source(chained(GraphEdgeUpsertError::Staging(error)))
                        })?;
                }
                EdgeUpsertResolvedRows::Bound(row, outcome)
            };
            for (row, targets) in resolved.rows() {
                let input_at = input.len();
                // The collector froze all parallel matches for this occurrence.
                // A key rewrite below affects only the next occurrence's lookup.
                for (outcome, endpoints) in targets.targets() {
                    let (edge, branch, on) = match outcome {
                        GraphEdgeMergeOutcome::Matched(edge) => (
                            edge,
                            GraphEdgeUpsertBranch::Match,
                            &upsert.actions().on_match,
                        ),
                        GraphEdgeMergeOutcome::Created(edge) => (
                            edge,
                            GraphEdgeUpsertBranch::Create,
                            &upsert.actions().on_create,
                        ),
                        GraphEdgeMergeOutcome::NoInput => return Err(invalid()),
                    };
                    let observed = u128::from(effects) + upsert.action_count(branch) as u128;
                    if observed > u128::from(policy.max_mutation_effects) {
                        return Err(GqlQueryError::Source(GraphWriteChainError::Mutation(
                            GraphMutationQueryError::Mutation(GraphMutationError::EffectLimit {
                                limit: policy.max_mutation_effects,
                                observed,
                            }),
                        )));
                    }
                    effects = observed as u64;
                    let state = std::cell::RefCell::new((&mut *self, &mut *database));
                    edge_merge_row_actions_controlled::<WriteTxnError, A, _>(
                        (merge.relation(), edge),
                        (at, &row),
                        [on, &upsert.actions().after],
                        private,
                        &mut stats.evaluator,
                        (policy.max_mutation_effects, &mut effects),
                        || cx.checkpoint(),
                        |key, control| {
                            let state = state.borrow();
                            edge_upsert_row_property(
                                state.0,
                                state.1,
                                ElementId::Edge(edge),
                                key,
                                control,
                            )
                        },
                        |column, element, key, control| {
                            let mut state = state.borrow_mut();
                            let (transaction, database) = &mut *state;
                            if column.is_some_and(|column| {
                                merge.unbound_columns().any(|new| new == column)
                            }) {
                                return edge_upsert_row_property_with(
                                    transaction,
                                    database,
                                    element,
                                    key,
                                    control,
                                    |value| Ok(value.cloned()),
                                );
                            }
                            transaction.with_prepared_point_read(&input_prepared, |transaction| {
                                edge_upsert_row_property_with(
                                    transaction,
                                    database,
                                    element,
                                    key,
                                    control,
                                    |value| Ok(value.cloned()),
                                )
                            })
                        },
                        |control| {
                            let state = state.borrow();
                            edge_replacement_keys(
                                state.0,
                                state.1,
                                edge,
                                None,
                                || cx.checkpoint().map_err(GqlQueryError::Interrupted),
                                control,
                                |error| GqlQueryError::Source(GraphEdgeUpsertError::Staging(error)),
                            )
                        },
                        // A copied element's properties (`r = a`), at the
                        // carried row's frozen input cut, as its `a.key` reads.
                        |element, kind, catalog, control| {
                            let mut state = state.borrow_mut();
                            let (transaction, database) = &mut *state;
                            let failure = std::cell::RefCell::new(None);
                            transaction
                                .with_prepared_point_read(&input_prepared, |transaction| {
                                    transaction.element_metadata_with_checkpoint(
                                        database, cx, &failure, element, kind, catalog, control,
                                    )
                                })
                                .map_err(|error| match error {
                                    fgdb_gql::GraphElementMetadataError::Source(error) => {
                                        fgdb_gql::GraphElementMetadataError::Source(
                                            GqlQueryError::Source(GraphEdgeUpsertError::Staging(
                                                error,
                                            )),
                                        )
                                    }
                                    fgdb_gql::GraphElementMetadataError::Interrupted => {
                                        fgdb_gql::GraphElementMetadataError::Interrupted
                                    }
                                })
                        },
                        |batch| {
                            let mut state = state.borrow_mut();
                            let (transaction, database) = &mut *state;
                            transaction
                                .stage_edge_upsert_row_batch(database, batch)
                                .map_err(|error| {
                                    GqlQueryError::Source(GraphEdgeUpsertError::Staging(error))
                                })
                        },
                    )
                    .map_err(|error| error.map_source(chained))?;
                    if retain_receipt {
                        vertex_targets_event::<GraphWriteChainError<WriteTxnError, A>>(
                            cx,
                            private,
                            &mut stats.evaluator,
                            GlaExecutionEvent::ScratchEntry,
                        )?;
                        outcomes.push(outcome);
                        if outcome.created() {
                            vertex_targets_event::<GraphWriteChainError<WriteTxnError, A>>(
                                cx,
                                private,
                                &mut stats.evaluator,
                                GlaExecutionEvent::ScratchEntry,
                            )?;
                            created.push(edge);
                        }
                    }
                    if retain_input {
                        vertex_targets_event::<GraphWriteChainError<WriteTxnError, A>>(
                            cx,
                            private,
                            &mut stats.evaluator,
                            GlaExecutionEvent::ScratchEntry,
                        )?;
                        pairs.push((input_at, edge, endpoints));
                    }
                }
                if retain_input {
                    vertex_targets_event::<GraphWriteChainError<WriteTxnError, A>>(
                        cx,
                        private,
                        &mut stats.evaluator,
                        GlaExecutionEvent::ScratchEntry,
                    )?;
                    input.push(row);
                }
            }
        }
        let result = if let GraphWriteChainStep::EdgeUpsertQuery { query, .. } = step {
            let returning = (|| -> EdgeUpsertActionResult<Vec<Vec<GraphValue>>, WriteTxnError, A, Box<asupersync::error::Error>> {
                let mut control = |event| edge_upsert_batch_event::<A>(cx, private, &mut stats.evaluator, event);
                let mut returning = Vec::new();
                for (at, edge, endpoints) in pairs {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    let row: &GraphValueRow = &input[at];
                    let mut cells = Vec::new();
                    for binding in query.bindings() {
                        control(GlaExecutionEvent::ScratchEntry)?;
                        let endpoint = |start| if start { endpoints.0 } else { endpoints.1 };
                        let property = match *binding {
                            GraphEdgeMergeBinding::Input(column) => {
                                cells.push(row.values().get(column).ok_or(GqlQueryError::Source(
                                    GraphEdgeUpsertError::Merge(GraphEdgeMergeError::InputSchema { row: at, column }),
                                ))?.copy_with_control(&mut control)?);
                                continue;
                            }
                            GraphEdgeMergeBinding::Edge => { cells.push(GraphValue::Edge(edge)); continue; }
                            GraphEdgeMergeBinding::Path { reverse, function } => {
                                let endpoint = |column| row.values().get(column)
                                    .and_then(GraphValue::as_vertex)
                                    .ok_or(GqlQueryError::Source(GraphEdgeUpsertError::Merge(
                                        GraphEdgeMergeError::InputSchema { row: at, column },
                                    )));
                                let source = endpoint(query.merge().source_column())?;
                                let destination = endpoint(query.merge().destination_column())?;
                                cells.push(edge_merge_path_value(edge, (source, destination), reverse, function, &mut control)?);
                                continue;
                            }
                            GraphEdgeMergeBinding::Endpoint { start } => { cells.push(GraphValue::Vertex(endpoint(start))); continue; }
                            GraphEdgeMergeBinding::Property(key) => Some((ElementId::Edge(edge), key)),
                            GraphEdgeMergeBinding::EndpointProperty { start, key } => Some((ElementId::Vertex(endpoint(start)), key)),
                            GraphEdgeMergeBinding::InputProperty { column, key } => match row.values().get(column) {
                                Some(GraphValue::Vertex(vertex)) => Some((ElementId::Vertex(*vertex), key)),
                                Some(GraphValue::Edge(edge)) => Some((ElementId::Edge(*edge), key)),
                                _ => None,
                            },
                        };
                        cells.push(match property {
                            Some((element, key)) => edge_upsert_row_property(self, database, element, key, &mut control)?,
                            None => GraphValue::Scalar(CanonicalScalar::Null),
                        });
                    }
                    returning.push(cells);
                }
                Ok(returning)
            })().map_err(|error| error.map_source(chained))?;
            let failure = std::cell::RefCell::new(None);
            let result = query
                .finish_governed_reading(
                    returning,
                    stats.selection,
                    stats.evaluator,
                    policy.query,
                    self.element_reader_with_checkpoint(database, cx, &failure),
                    || match failure.borrow_mut().take() {
                        Some(error) => Err(error),
                        None => cx.checkpoint(),
                    },
                )
                .map_err(|error| {
                    error.map_source(|error| {
                        GraphWriteChainError::EdgeUpsert(GraphEdgeUpsertQueryError::Returning(
                            error,
                        ))
                    })
                });
            match failure.into_inner() {
                Some(error) => Err(GqlQueryError::Interrupted(error)),
                None => result,
            }?
        } else {
            let mut continued = Vec::new();
            for (at, edge, _) in pairs {
                continued.push(step.continuation_row_controlled(
                    &input[at],
                    ElementId::Edge(edge),
                    &mut |event| {
                        vertex_targets_event::<GraphWriteChainError<WriteTxnError, A>>(
                            cx,
                            private,
                            &mut stats.evaluator,
                            event,
                        )
                    },
                )?);
            }
            GqlQueryExecution {
                value: continued,
                rows: stats.selection,
                evaluator: stats.evaluator,
            }
        };
        stats.selection = result.rows;
        stats.evaluator = result.evaluator;
        let receipt = retain_receipt.then_some(match outcomes.as_slice() {
            [] => GraphWriteStepReceipt::EdgeUpsert {
                outcome: GraphEdgeMergeOutcome::NoInput,
            },
            [outcome] => GraphWriteStepReceipt::EdgeUpsert { outcome: *outcome },
            _ => GraphWriteStepReceipt::EdgeUpsertBatch { outcomes, created },
        });
        cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
        Ok((stats, result, receipt, effects))
    }

    fn stage_edge_upsert_row_batch<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        batch: WriteBatch,
    ) -> Result<(), WriteTxnError> {
        if self
            .staged
            .iter()
            .all(|staged| staged.relation == batch.relation)
        {
            self.write(database, batch)
        } else {
            self.write_atomic(database, vec![batch])
        }
    }
}
