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
            self.reads.borrow_mut().retain_refused_projection(self.element);
        }
    }
}

fn point_payload_admission<E>(
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
        CanonicalScalar::Null | CanonicalScalar::Bool(_) | CanonicalScalar::Int(_)
        | CanonicalScalar::Decimal(_) | CanonicalScalar::Float(_) => [0, 0],
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
    ) -> Result<fgdb_gql::GqlQueryExecution<Option<CanonicalScalar>>, TxnGqlError<WriteTxnError>> {
        cx.with_restriction(|| self.point_governed_with_checkpoint(
            database, ElementId::Vertex(vertex), PointReadField::Property(property), policy,
            || cx.checkpoint(), |projection| projection.property.cloned(),
        ))
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
    ) -> Result<fgdb_gql::GqlQueryExecution<Option<CanonicalScalar>>, TxnGqlError<WriteTxnError>> {
        cx.with_restriction(|| self.point_governed_with_checkpoint(
            database, ElementId::Edge(edge), PointReadField::Property(property), policy,
            || cx.checkpoint(), |projection| projection.property.cloned(),
        ))
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
        cx.with_restriction(|| self.point_governed_with_checkpoint(
            database, ElementId::Vertex(vertex), PointReadField::Label(label), policy,
            || cx.checkpoint(), |projection| projection.exists.then_some(projection.label),
        ))
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
        self.ensure_database(database).map_err(GqlQueryError::Source)?;
        database.ensure_readable().map_err(WriteTxnError::from).map_err(GqlQueryError::Source)?;
        database.snapshot.check_frontier(self.basis)
            .map_err(WriteTxnError::from).map_err(GqlQueryError::Source)?;
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
                if event == SourceEvent::SnapshotRecord { snapshot_records += 1; }
                Ok(())
            };
            let projection = self.point_projection_with_control(
                database, element, field, &mut control, &GqlQueryError::Source,
            )?;
            if let Some(value) = projection.property {
                point_payload_admission(value, &mut control)?;
            }
            // Reserve the worst-case map entry and selected-field entry on
            // EVERY call. Warming a witness does not make the policy cheaper.
            control(SourceEvent::ScratchEntry)?;
            control(SourceEvent::ScratchEntry)?;
            self.point_reads.borrow_mut().record(element, field);
            policy.rows.check(GqlBudgetDimension::ResultRows, 1).map_err(GqlQueryError::Rows)?;
            control(SourceEvent::ScratchEntry)?;
            control(SourceEvent::Work)?;
            vec![deliver(projection)]
        };
        attempt.accepted = true;
        usage.finish(policy, Ok(fgdb_gql::GqlQueryExecution {
            value,
            rows: GqlExecutionStats { snapshot_records, result_rows: 1 },
            evaluator: GlaExecutionStats::default(),
        }))
    }
}

#[cfg(test)]
mod governed_point_tests {
    include!("governed_point_tests.rs");
}
