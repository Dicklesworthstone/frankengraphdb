// Explicit projections and local adjacency domains (plan 7.3), never a
// refinement of an already recorded full-row getter or query witness.
// Canonical staged effects still own read-your-writes.

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PointReadField {
    Property(fgdb_delta_types::PropertyKeyId),
    Label(LabelId),
    /// All current AND absent edges of this type and direction at the vertex.
    /// The full domain is retained even when no neighbour was returned.
    Adjacency {
        relation: RelationId,
        incoming: bool,
    },
    /// Immutable incidence and existence, not the edge's property payload.
    EdgeTopology,
}

#[derive(Default)]
struct PointReads(
    std::collections::BTreeMap<ElementId, std::collections::BTreeSet<PointReadField>>,
    // A refused governed projection may expose a data-dependent failure before
    // its precise domain fits. This allocation-free superset survives rollback.
    Option<ElementId>,
    // Each accepted scan retains its predicate, including an empty result.
    Vec<VertexScanRead>,
);

impl PointReads {
    fn record(&mut self, element: ElementId, field: PointReadField) {
        self.0.entry(element).or_default().insert(field);
    }

    #[cfg(test)]
    fn record_adjacency(
        &mut self,
        vertex: VId,
        relation: RelationId,
        incoming: bool,
        edges: impl IntoIterator<Item = EId>,
    ) {
        let result =
            self.record_adjacency_controlled(vertex, relation, incoming, edges, &mut || {
                Ok::<(), core::convert::Infallible>(())
            });
        match result {
            Ok(()) => {}
            Err(never) => match never {},
        }
    }

    fn record_adjacency_controlled<E>(
        &mut self,
        vertex: VId,
        relation: RelationId,
        incoming: bool,
        edges: impl IntoIterator<Item = EId>,
        control: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<(), E> {
        // The anchor covers the insertion gap and its own lifetime; the EIds
        // cover retirement, including vertex-delete cascades. Keep all matching
        // basis and staged edges, not just one edge per distinct neighbour.
        // Union with earlier observations; never remove a broad read or slot.
        control()?;
        self.record(
            ElementId::Vertex(vertex),
            PointReadField::Adjacency { relation, incoming },
        );
        for eid in edges {
            control()?;
            self.record(ElementId::Edge(eid), PointReadField::EdgeTopology);
        }
        Ok(())
    }

    fn retain_refused_projection(&mut self, element: ElementId) {
        self.1 = Some(self.1.map_or(element, |previous| previous.min(element)));
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty() && self.1.is_none() && self.2.is_empty()
    }

    fn clear(&mut self) {
        self.0.clear();
        self.1 = None;
        self.2.clear();
    }

    fn contains(&self, element: ElementId, field: PointReadField) -> bool {
        self.0
            .get(&element)
            .is_some_and(|fields| fields.contains(&field))
    }

    /// Slots and adjacency gaps share one original-basis validation law.
    /// Current-value equality cannot erase an intervening write. Unknown
    /// families conservatively affect every nonempty projected-read set.
    fn conflict(
        &self,
        row: &fgdb_delta_types::DeltaRow,
        checkpoint: &mut impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<Option<ElementId>, WriteTxnError> {
        use fgdb_delta_types::DeltaRow;
        if self.is_empty() {
            // Existing callers without point reads retain their control trace.
            return Ok(None);
        }
        checkpoint()?;
        if let Some(element) = self.1 {
            return Ok(Some(element));
        }
        let target = match row {
            DeltaRow::Property { elem, property, .. } => {
                return Ok(self
                    .contains(*elem, PointReadField::Property(*property))
                    .then_some(*elem));
            }
            DeltaRow::LabelMembership { vid, label, .. } => {
                let element = ElementId::Vertex(*vid);
                return Ok(self
                    .contains(element, PointReadField::Label(*label))
                    .then_some(element));
            }
            DeltaRow::CreateVertex { vid, .. } => ElementId::Vertex(*vid),
            DeltaRow::CreateEdge {
                eid,
                src,
                relation,
                dst,
                ..
            } => {
                // Test the logical range, not just the EIds returned earlier.
                // Self-loops satisfy both orientations; the tests are a union.
                for (anchor, incoming) in [(*src, false), (*dst, true)] {
                    let element = ElementId::Vertex(anchor);
                    if self.contains(
                        element,
                        PointReadField::Adjacency {
                            relation: *relation,
                            incoming,
                        },
                    ) {
                        return Ok(Some(element));
                    }
                }
                ElementId::Edge(*eid)
            }
            DeltaRow::DeleteEdge { eid, .. } => ElementId::Edge(*eid),
            DeltaRow::DeleteVertex {
                vid,
                sorted_retired_incident_edges,
                ..
            } => {
                let vertex = ElementId::Vertex(*vid);
                if self.0.contains_key(&vertex) {
                    return Ok(Some(vertex));
                }
                // Edge retirement can be carried only by its vertex's cascade.
                for eid in sorted_retired_incident_edges {
                    checkpoint()?;
                    let edge = ElementId::Edge(*eid);
                    if self.0.contains_key(&edge) {
                        return Ok(Some(edge));
                    }
                }
                return Ok(None);
            }
            // Includes schema, constraints and not-yet-supported field algebras.
            // Do not infer independence from an unrecognized effect's shape.
            _ => return Ok(self.0.keys().next().copied()),
        };
        Ok(self.0.contains_key(&target).then_some(target))
    }
}

include!("vertex_scan_reads.rs");
include!("point_projection.rs");
include!("governed_point_reads.rs");

impl WriteTxn {
    /// Read only one vertex property at the pinned basis plus canonical effects.
    ///
    /// None means an absent vertex or an absent property; a stored scalar Null
    /// remains Some(Null). Both absence cases retain the requested property and
    /// element-lifetime witness before returning. Unrelated properties, labels
    /// and incidence do not become reads merely because the resident row holds
    /// them. Calling vertex(), vertices() or a query still retains its ORIGINAL
    /// broader observations; this method never removes or narrows a witness.
    ///
    /// Finish, explicit refresh and both rebase policies validate these domains
    /// against complete retained history, including change-and-restoration and
    /// retirement. Savepoint rollback retains observations. Ordinary writes
    /// keep their conservative write footprint; finer reads do not opt into a
    /// rebase or change its eligibility and exact-effect requirements.
    ///
    /// This is a raw embedded read, not capability masking or full SSI. It
    /// borrows the admitted historical row and canonical effects, copying only
    /// the selected scalar. It does not copy unrelated properties or labels.
    /// History remains resident; this is not a byte-memory quota or spill path.
    pub fn vertex_property<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        vid: VId,
        property: fgdb_delta_types::PropertyKeyId,
    ) -> Result<Option<CanonicalScalar>, WriteTxnError> {
        let field = PointReadField::Property(property);
        let row = self.point_projection(database, ElementId::Vertex(vid), field)?;
        self.point_reads
            .borrow_mut()
            .record(ElementId::Vertex(vid), PointReadField::Property(property));
        Ok(row.property.cloned())
    }

    /// The edge-property sibling of vertex_property, with identical absence,
    /// ownership, snapshot and witness semantics. Vertex-deletion cascades also
    /// invalidate the observed edge lifetime, even without a DeleteEdge row.
    pub fn edge_property<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        eid: EId,
        property: fgdb_delta_types::PropertyKeyId,
    ) -> Result<Option<CanonicalScalar>, WriteTxnError> {
        let field = PointReadField::Property(property);
        let row = self.point_projection(database, ElementId::Edge(eid), field)?;
        self.point_reads
            .borrow_mut()
            .record(ElementId::Edge(eid), PointReadField::Property(property));
        Ok(row.property.cloned())
    }

    /// Read one label membership without observing the complete vertex.
    ///
    /// Some(true/false) is membership on an existing vertex; None means the
    /// vertex is absent. The selected membership AND vertex lifetime remain
    /// dependencies, including negative answers and staged effects. Other
    /// labels, properties and incidence are not exposed by this accessor.
    /// Property and label IDs inhabit distinct read domains even when their
    /// numeric values are equal. Full getters and scans retain their broader
    /// witnesses; rollback never removes any point observations.
    ///
    /// The same original-basis validator handles finish, explicit refresh and
    /// rebase. A remove/re-add is still a conflict. This inherits the raw
    /// embedded authority, residency and isolation limits of vertex_property.
    pub fn vertex_has_label<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        vid: VId,
        label: LabelId,
    ) -> Result<Option<bool>, WriteTxnError> {
        let row = self.point_projection(
            database,
            ElementId::Vertex(vid),
            PointReadField::Label(label),
        )?;
        self.point_reads
            .borrow_mut()
            .record(ElementId::Vertex(vid), PointReadField::Label(label));
        Ok(row.exists.then_some(row.label))
    }

    // Retain the original full-row resolution as an independent test oracle.
    // Production point reads borrow their requested projection instead.
    #[cfg(test)]
    fn point_vertex<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        vid: VId,
    ) -> Result<Option<VertexRow>, WriteTxnError> {
        self.ensure_database(database)?;
        let mut row = if database.frontier()? == self.basis {
            database.vertex(vid)?
        } else {
            database.vertex_at(vid, self.basis)?
        };
        if let Some(prepared) = &self.prepared {
            for coordinate in prepared.template.coordinate_entries() {
                for effect in &coordinate.rows {
                    self.apply_vertex_effect(vid, &mut row, effect);
                }
            }
        }
        Ok(row)
    }

    #[cfg(test)]
    fn point_edge<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        eid: EId,
    ) -> Result<Option<EdgeRecord>, WriteTxnError> {
        self.ensure_database(database)?;
        let mut row = if database.frontier()? == self.basis {
            database.edge(eid)?
        } else {
            database.edge_at(eid, self.basis)?
        };
        if let Some(prepared) = &self.prepared {
            for coordinate in prepared.template.coordinate_entries() {
                for effect in &coordinate.rows {
                    self.apply_edge_effect(eid, &mut row, effect);
                }
            }
        }
        Ok(row)
    }
}

#[cfg(test)]
mod point_read_tests {
    include!("point_read_tests.rs");
}

#[cfg(test)]
mod point_label_tests {
    include!("point_label_tests.rs");
}

#[cfg(test)]
mod topology_read_tests {
    include!("topology_read_tests.rs");
}
