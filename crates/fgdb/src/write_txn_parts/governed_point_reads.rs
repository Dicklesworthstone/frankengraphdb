// Govern existing point projections BEFORE owning a scalar, witness or result.
// The ordinary projection and the ordinary AdmissionUsage remain authoritative.

struct ProjectionReadAttempt<'a> {
    reads: &'a std::cell::RefCell<PointReads>,
    element: ElementId,
    accepted: bool,
}

impl Drop for ProjectionReadAttempt<'_> {
    fn drop(&mut self) {
        if !self.accepted {
            // No callback runs while the witness RefCell is borrowed. This
            // allocation-free superset preserves information conveyed by a
            // data-dependent refusal, even when precise witnesses did not fit.
            self.reads
                .borrow_mut()
                .retain_refused_projection(self.element);
        }
    }
}

/// Admit a stored value before it is copied: a scalar's payload units, and for
/// a List or Map one scratch entry per element plus every key's and nested
/// value's payload (fgdb-k3nub).
fn point_payload_admission<E>(
    value: &CanonicalPropertyValue,
    control: &mut impl FnMut(crate::gql_exec::source::SourceEvent) -> Result<(), E>,
) -> Result<(), E> {
    use crate::gql_exec::source::SourceEvent::ScratchEntry;
    use fgdb_gql::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES;
    match value {
        CanonicalPropertyValue::Scalar(scalar) => point_scalar_payload_admission(scalar, control),
        CanonicalPropertyValue::List(list) => list.values().iter().try_for_each(|value| {
            control(ScratchEntry)?;
            point_payload_admission(value, &mut *control)
        }),
        CanonicalPropertyValue::Map(map) => map.entries().iter().try_for_each(|entry| {
            control(ScratchEntry)?;
            for _ in 0..entry
                .key()
                .as_str()
                .len()
                .div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
            {
                control(ScratchEntry)?;
            }
            point_payload_admission(entry.value(), &mut *control)
        }),
    }
}

fn point_scalar_payload_admission<E>(
    value: &CanonicalScalar,
    control: &mut impl FnMut(crate::gql_exec::source::SourceEvent) -> Result<(), E>,
) -> Result<(), E> {
    use crate::gql_exec::source::SourceEvent::ScratchEntry;
    use fgdb_gql::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES;
    // Same scalar payload unit as GLA. Text spelling and collation key are
    // separate lengths, so adding them cannot wrap. Never encode/clone merely
    // to find a length. Fixed-size scalar storage is part of the result entry.
    let sizes = match value {
        CanonicalScalar::Text(value) => [
            value.len(),
            value.canonical_sort_key().map_or(0, <[u8]>::len),
        ],
        CanonicalScalar::Bytes(value) => [value.as_slice().len(), 0],
        CanonicalScalar::Timestamp(value) => {
            [value.zone().map_or(0, |zone| zone.identifier().len()), 0]
        }
        CanonicalScalar::Null
        | CanonicalScalar::Bool(_)
        | CanonicalScalar::Int(_)
        | CanonicalScalar::Decimal(_)
        | CanonicalScalar::Float(_)
        | CanonicalScalar::Date(_)
        | CanonicalScalar::LocalTime(_)
        | CanonicalScalar::Time(_)
        | CanonicalScalar::LocalDateTime(_)
        | CanonicalScalar::Duration(_) => [0, 0],
    };
    for bytes in sizes {
        for _ in 0..bytes.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
            // Each owned payload unit consumes BOTH scratch and work, before
            // any part of the scalar is cloned. Refusal releases no value.
            control(ScratchEntry)?;
        }
    }
    Ok(())
}

impl WriteTxn {
    /// Materialize an already admitted element at this transaction's basis and
    /// canonical NET effects. The optional scope masks fields and bounds the
    /// complete-field witness; it does not itself authorize an identity.
    #[allow(clippy::too_many_arguments)]
    fn admitted_element_metadata_with_control<V: Vfs + Clone, E>(
        &self,
        database: &Database<V>,
        element: ElementId,
        kind: fgdb_gql::GraphElementMetadata,
        catalog: &fgdb_gql::ReverseSymbolCatalog,
        scope: Option<&fgdb_warden::PlannerPredicates>,
        poll: &mut impl FnMut() -> Result<(), E>,
        control: &mut impl FnMut(crate::gql_exec::source::SourceEvent) -> Result<(), E>,
        source_error: &impl Fn(WriteTxnError) -> E,
    ) -> Result<Option<fgdb_gql::algebra::GraphValue>, E> {
        use crate::gql_exec::{property_maps, source::SourceEvent};
        use fgdb_gql::GraphElementMetadata;
        let _ = self.query_snapshot(database).map_err(source_error)?;
        control(SourceEvent::Work)?;
        if scope.is_none() {
            self.check_chain_element_read(element, control, source_error)?;
        }
        let mut attempt = ProjectionReadAttempt {
            reads: &self.point_reads,
            element,
            accepted: false,
        };
        for _ in 0..2 {
            control(SourceEvent::ScratchEntry)?;
        }
        self.point_reads.borrow_mut().record(
            element,
            match element {
                ElementId::Vertex(_) => PointReadField::VertexExistence,
                ElementId::Edge(_) => PointReadField::EdgeTopology,
            },
        );
        if let Some(domain) = PointMetadataRead::admit(kind, scope, control)? {
            for _ in 0..2 {
                control(SourceEvent::ScratchEntry)?;
            }
            self.point_reads
                .borrow_mut()
                .record_metadata(element, domain);
        }
        let visible_property = |key| scope.is_none_or(|scope| scope.allows_property(key));
        let visible_label = |label| scope.is_none_or(|scope| scope.allows_label(label));
        let read_error = |error| source_error(WriteTxnError::Read(error));
        // Authorized physical searches and hidden effects only poll. Native
        // reads preserve their physical work accounting. Visible sparse edits
        // reserve their borrowed entry before either host retains it.
        let poll = std::cell::RefCell::new(poll);
        let value = match (element, kind) {
            (ElementId::Vertex(vertex), GraphElementMetadata::Labels) => {
                let labels = self.vertex_labels_with_control(
                    database,
                    vertex,
                    &mut |label| visible_label(label),
                    &mut || (poll.borrow_mut())(),
                    &mut |event| match event {
                        SourceEvent::Work | SourceEvent::SnapshotRecord if scope.is_some() => {
                            (poll.borrow_mut())()
                        }
                        _ => control(event),
                    },
                    source_error,
                )?;
                labels
                    .as_ref()
                    .map(|labels| {
                        property_maps::collect_labels(
                            labels.effective_labels_iter(),
                            catalog,
                            visible_label,
                            control,
                            &read_error,
                        )
                    })
                    .transpose()?
            }
            (
                ElementId::Vertex(vertex),
                GraphElementMetadata::Properties | GraphElementMetadata::Keys,
            ) => {
                let properties = self.vertex_properties_with_control(
                    database,
                    vertex,
                    &mut |key| visible_property(key),
                    &mut || (poll.borrow_mut())(),
                    &mut |event| match event {
                        SourceEvent::Work | SourceEvent::SnapshotRecord if scope.is_some() => {
                            (poll.borrow_mut())()
                        }
                        _ => control(event),
                    },
                    source_error,
                )?;
                properties
                    .as_ref()
                    .map(|properties| match kind {
                        GraphElementMetadata::Properties => property_maps::collect(
                            properties.effective_properties(),
                            Some(catalog),
                            visible_property,
                            control,
                            &read_error,
                        ),
                        _ => property_maps::collect_keys(
                            properties.effective_properties().map(|(key, _)| key),
                            catalog,
                            visible_property,
                            control,
                            &read_error,
                        ),
                    })
                    .transpose()?
            }
            (
                ElementId::Edge(edge),
                GraphElementMetadata::Properties
                | GraphElementMetadata::Keys
                | GraphElementMetadata::Type,
            ) => {
                let projected = self.edge_metadata_with_control(
                    database,
                    edge,
                    &mut |key| kind != GraphElementMetadata::Type && visible_property(key),
                    &mut || (poll.borrow_mut())(),
                    &mut |event| match event {
                        SourceEvent::Work | SourceEvent::SnapshotRecord if scope.is_some() => {
                            (poll.borrow_mut())()
                        }
                        _ => control(event),
                    },
                    source_error,
                )?;
                projected
                    .as_ref()
                    .map(|projected| match kind {
                        GraphElementMetadata::Properties => property_maps::collect(
                            projected.properties.effective_properties(),
                            Some(catalog),
                            visible_property,
                            control,
                            &read_error,
                        ),
                        GraphElementMetadata::Keys => property_maps::collect_keys(
                            projected
                                .properties
                                .effective_properties()
                                .map(|(key, _)| key),
                            catalog,
                            visible_property,
                            control,
                            &read_error,
                        ),
                        _ => property_maps::relation_type(
                            projected.relation,
                            catalog,
                            control,
                            &read_error,
                        ),
                    })
                    .transpose()?
            }
            _ => return Err(read_error(crate::ReadError::InvalidPropertyMap)),
        };
        attempt.accepted = true;
        Ok(value)
    }

    // A carried identity reads the same borrowed historical winner and prepared
    // NET effects as explicit point projections. The relational engine already
    // charges the expression's work and copied payload; physical lookup and
    // overlay traversal poll here without inventing another pattern source or
    // resetting the statement's allowance. Only the selected scalar is owned.
    //
    // The point witness includes absent elements/properties and edge retirement
    // through a vertex cascade. If lookup or witness admission is interrupted,
    // the existing guard retains its conservative ALL-CHANGE witness. Successful
    // retry and savepoint rollback cannot erase either kind of observation.
    fn element_property_with_checkpoint<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &fgdb_types::QueryCx,
        failure: &std::cell::RefCell<Option<Box<asupersync::error::Error>>>,
        element: ElementId,
        property: fgdb_delta_types::PropertyKeyId,
    ) -> Result<Option<fgdb_types::CanonicalPropertyValue>, WriteTxnError> {
        if let Some(error) = failure.borrow().as_ref() {
            return Err(WriteTxnError::Interrupted(error.clone()));
        }
        let _ = self.query_snapshot(database)?;
        let mut control = |_: crate::gql_exec::source::SourceEvent| {
            cx.checkpoint().map_err(|error| {
                // The source callback carries a typed copy while the enclosing
                // governed call restores the original control error before
                // returning. No further source or evaluator checkpoint runs.
                let mut failure = failure.borrow_mut();
                WriteTxnError::Interrupted(failure.get_or_insert(error).clone())
            })
        };
        control(crate::gql_exec::source::SourceEvent::Work)?;
        let mut attempt = ProjectionReadAttempt {
            reads: &self.point_reads,
            element,
            accepted: false,
        };
        let field = PointReadField::Property(property);
        let projected =
            self.point_projection_with_control(database, element, field, &mut control, &|error| {
                error
            })?;
        self.point_reads.borrow_mut().record(element, field);
        let value = projected.property.cloned();
        attempt.accepted = true;
        Ok(value)
    }

    /// Project one vertex property under a shared source/work/scratch allowance.
    ///
    /// A successful execution contains exactly ONE optional-value row: None
    /// means absent element/property, while Some(Null) is a stored scalar null.
    /// Negative answers therefore consume one result-row unit too. Snapshot
    /// records count a visible durable target before overlay application (zero
    /// for a missing or entirely staged target). Staged effects are work, not
    /// extra snapshot records. Only the final selected scalar is copied.
    ///
    /// History lookup, field search, every canonical effect/cascade search,
    /// result delivery and witness admission share the existing GQL meter.
    /// Variable-size scalar data reserves GLA payload units before cloning;
    /// unrelated fields are neither copied nor payload-charged. Every call pays
    /// its full logical cost even if the witness was retained by an earlier read.
    ///
    /// Refusal/unwind after initial admission returns no partial row and keeps
    /// an allocation-free ALL-CHANGE witness, as governed adjacency does. This
    /// deliberately may conflict with unrelated writes. Savepoint rollback and
    /// a successful retry cannot erase it. Ownership/health/initial-cancellation
    /// refusals do not install it. Staged effects, savepoints and pins survive.
    ///
    /// Logical entries and payload units are NOT allocator-byte or transaction-
    /// lifetime caps. Resident history remains, but each edge-directory and
    /// version lookup step is controlled. An admitted scalar clone is not
    /// internally preemptible.
    /// This raw embedded API adds no authorization, spill, or full SSI claim.
    pub fn vertex_property_governed<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &fgdb_types::QueryCx,
        vertex: VId,
        property: fgdb_delta_types::PropertyKeyId,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<Option<CanonicalPropertyValue>>,
        TxnGqlError<WriteTxnError>,
    > {
        cx.with_restriction(|| {
            self.point_governed_with_checkpoint(
                database,
                ElementId::Vertex(vertex),
                PointReadField::Property(property),
                policy,
                || cx.checkpoint(),
                |projection| projection.property.cloned(),
            )
        })
    }

    /// Edge-property sibling with the same one-row, absence and admission law.
    /// Its witness preserves the edge identity domain, including retirement
    /// carried by a vertex cascade; an edge ID is never cast to a vertex ID.
    pub fn edge_property_governed<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &fgdb_types::QueryCx,
        edge: EId,
        property: fgdb_delta_types::PropertyKeyId,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<Option<CanonicalPropertyValue>>,
        TxnGqlError<WriteTxnError>,
    > {
        cx.with_restriction(|| {
            self.point_governed_with_checkpoint(
                database,
                ElementId::Edge(edge),
                PointReadField::Property(property),
                policy,
                || cx.checkpoint(),
                |projection| projection.property.cloned(),
            )
        })
    }

    /// One governed label-membership row: None for an absent vertex, Some(false)
    /// for a present vertex without the label, and Some(true) for membership.
    /// The label and vertex lifetime are observed, not unrelated payload fields.
    pub fn vertex_has_label_governed<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &fgdb_types::QueryCx,
        vertex: VId,
        label: LabelId,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<fgdb_gql::GqlQueryExecution<Option<bool>>, TxnGqlError<WriteTxnError>> {
        cx.with_restriction(|| {
            self.point_governed_with_checkpoint(
                database,
                ElementId::Vertex(vertex),
                PointReadField::Label(label),
                policy,
                || cx.checkpoint(),
                |projection| projection.exists.then_some(projection.label),
            )
        })
    }

    // The delivery closure is private and only converts this borrowed result
    // to its owned public shape. It is invoked after ALL quota/cancel checks.
    // Keep the failure guard live across that conversion, including unwinding.
    fn point_governed_with_checkpoint<'a, V: Vfs + Clone, C, Row>(
        &'a self,
        database: &'a Database<V>,
        element: ElementId,
        field: PointReadField,
        policy: fgdb_gql::GqlQueryPolicy,
        mut checkpoint: impl FnMut() -> Result<(), C>,
        deliver: impl FnOnce(PointProjection<'a>) -> Row,
    ) -> Result<fgdb_gql::GqlQueryExecution<Row>, fgdb_gql::GqlQueryError<WriteTxnError, C>> {
        use crate::gql_exec::source::SourceEvent;
        use fgdb_gql::{GlaExecutionStats, GqlBudgetDimension, GqlExecutionStats, GqlQueryError};
        self.ensure_database(database)
            .map_err(GqlQueryError::Source)?;
        database
            .ensure_readable()
            .map_err(WriteTxnError::from)
            .map_err(GqlQueryError::Source)?;
        database
            .snapshot
            .check_frontier(self.basis)
            .map_err(WriteTxnError::from)
            .map_err(GqlQueryError::Source)?;
        let mut usage = crate::gql_exec::AdmissionUsage::default();
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        usage.observe(policy, SourceEvent::Work)?;
        let mut attempt = ProjectionReadAttempt {
            reads: &self.point_reads,
            element,
            accepted: false,
        };
        let mut snapshot_records = 0;
        let value = {
            let mut control = |event| {
                checkpoint().map_err(GqlQueryError::Interrupted)?;
                usage.observe(policy, event)?;
                if event == SourceEvent::SnapshotRecord {
                    snapshot_records += 1;
                }
                Ok(())
            };
            let projection = self.point_projection_with_control(
                database,
                element,
                field,
                &mut control,
                &GqlQueryError::Source,
            )?;
            if let Some(value) = projection.property {
                point_payload_admission(value, &mut control)?;
            }
            // Reserve the worst-case map entry and selected-field entry on
            // EVERY call. Warming a witness does not make the policy cheaper.
            control(SourceEvent::ScratchEntry)?;
            control(SourceEvent::ScratchEntry)?;
            self.point_reads.borrow_mut().record(element, field);
            policy
                .rows
                .check(GqlBudgetDimension::ResultRows, 1)
                .map_err(GqlQueryError::Rows)?;
            control(SourceEvent::ScratchEntry)?;
            control(SourceEvent::Work)?;
            vec![deliver(projection)]
        };
        attempt.accepted = true;
        usage.finish(
            policy,
            Ok(fgdb_gql::GqlQueryExecution {
                value,
                rows: GqlExecutionStats {
                    snapshot_records,
                    result_rows: 1,
                },
                evaluator: GlaExecutionStats::default(),
            }),
        )
    }
}

#[cfg(test)]
mod governed_point_tests {
    include!("governed_point_tests.rs");
}
