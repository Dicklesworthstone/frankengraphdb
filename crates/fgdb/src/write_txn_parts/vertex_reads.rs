/// Mutable field indexes borrow scalar payloads from the pinned generation or
/// the transaction's prepared template. Neither source can change during a
/// query. Every new logical entry is admitted before allocation.
struct BorrowedQueryVertex<'a> {
    labels: std::collections::BTreeSet<LabelId>,
    props: std::collections::BTreeMap<PropertyKeyId, &'a CanonicalScalar>,
}

impl<'a> BorrowedQueryVertex<'a> {
    fn new<E>(
        labels: &[LabelId],
        props: &'a [(PropertyKeyId, CanonicalScalar)],
        control: &mut impl FnMut(crate::gql_exec::source::SourceEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        use crate::gql_exec::source::SourceEvent;
        let mut row = Self {
            labels: std::collections::BTreeSet::new(),
            props: std::collections::BTreeMap::new(),
        };
        for label in labels {
            control(SourceEvent::ScratchEntry)?;
            row.labels.insert(*label);
        }
        for (key, value) in props {
            control(SourceEvent::ScratchEntry)?;
            row.props.insert(*key, value);
        }
        Ok(row)
    }

    fn matches(&self, predicates: &[fgdb_gql::algebra::VertexPredicate]) -> bool {
        use fgdb_gql::algebra::VertexPredicate;
        predicates.iter().all(|predicate| {
            // Index only the requested field; the shared GLA predicate owns
            // missing/type/comparison semantics for both owned and borrowed rows.
            let (label, property) = match predicate {
                VertexPredicate::HasLabel(label) => (self.labels.get(label).copied(), None),
                VertexPredicate::IntegerProperty { key, .. } => (
                    None,
                    self.props.get_key_value(key).map(|(key, value)| (*key, *value)),
                ),
            };
            predicate.matches_borrowed(label, property)
        })
    }
}

impl WriteTxn {
    /// Caller has checked owner, health, frontier and initial cancellation.
    /// Scanning begins with its absence witness; partial observations survive
    /// every later refusal. Only final overlay members consume the row budget.
    fn governed_node_rows<'a, V: Vfs + Clone, E>(
        &'a self,
        database: &'a Database<V>,
        label: Option<LabelId>,
        control: &mut impl FnMut(crate::gql_exec::source::SourceEvent) -> Result<(), E>,
    ) -> Result<std::collections::BTreeMap<VId, BorrowedQueryVertex<'a>>, E> {
        use crate::gql_exec::source::{SourceEvent, scan_vertices};
        use fgdb_delta_types::DeltaRow;

        if let Some(label) = label {
            control(if self.scanned_vertex_labels.borrow().contains(&label) {
                SourceEvent::Work
            } else {
                SourceEvent::ScratchEntry
            })?;
            self.scanned_vertex_labels.borrow_mut().insert(label);
        } else {
            control(SourceEvent::Work)?;
            self.scanned_vertices.set(true);
        }
        let basis = scan_vertices(&database.snapshot.patches, self.basis, &mut |event| {
            // The durable basis can exceed the final overlay after deletions.
            // Still meter its scan and retained references, but defer row admission.
            control(if event == SourceEvent::SnapshotRecord { SourceEvent::Work } else { event })
        })?;
        let mut rows = std::collections::BTreeMap::new();
        for row in basis {
            control(SourceEvent::Work)?;
            self.observe_governed_node(row.vid, control)?;
            let fields = BorrowedQueryVertex::new(&row.labels, &row.props, control)?;
            control(SourceEvent::ScratchEntry)?;
            rows.insert(row.vid, fields);
        }

        // A canonical no-op can erase an attempted create/delete. Its negative
        // identity observation must still survive, just as in vertices_for_scan.
        for batch in &self.staged {
            control(SourceEvent::Work)?;
            for pending in &batch.rows {
                control(SourceEvent::Work)?;
                if let PendingRow::Vertex { vid, .. } | PendingRow::DeleteVertex { vid, .. } = pending {
                    self.observe_governed_node(*vid, control)?;
                }
            }
        }
        if let Some(prepared) = &self.prepared {
            for coordinate in prepared.template.coordinate_entries() {
                control(SourceEvent::Work)?;
                for effect in &coordinate.rows {
                    control(SourceEvent::Work)?;
                    match effect {
                        DeltaRow::CreateVertex { vid, labels, props, .. } => {
                            self.observe_governed_node(*vid, control)?;
                            let fields = BorrowedQueryVertex::new(labels, props, control)?;
                            if !rows.contains_key(vid) {
                                control(SourceEvent::ScratchEntry)?;
                            }
                            rows.insert(*vid, fields);
                        }
                        DeltaRow::DeleteVertex { vid, .. } => {
                            rows.remove(vid);
                        }
                        DeltaRow::LabelMembership { vid, label, after, .. } => {
                            if let Some(row) = rows.get_mut(vid) {
                                if *after {
                                    if !row.labels.contains(label) {
                                        control(SourceEvent::ScratchEntry)?;
                                        row.labels.insert(*label);
                                    }
                                } else {
                                    row.labels.remove(label);
                                }
                            }
                        }
                        DeltaRow::Property { elem: ElementId::Vertex(vid), property, after, .. } => {
                            if let Some(row) = rows.get_mut(vid) {
                                if let Some(value) = after {
                                    if !row.props.contains_key(property) {
                                        control(SourceEvent::ScratchEntry)?;
                                    }
                                    row.props.insert(*property, value);
                                } else {
                                    row.props.remove(property);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        for _ in rows.values() {
            control(SourceEvent::SnapshotRecord)?;
        }
        Ok(rows)
    }

    fn observe_governed_node<E>(
        &self,
        vid: VId,
        control: &mut impl FnMut(crate::gql_exec::source::SourceEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        if !self.read_set.borrow().contains(&ElementId::Vertex(vid)) {
            control(crate::gql_exec::source::SourceEvent::ScratchEntry)?;
            self.read_set.borrow_mut().insert(ElementId::Vertex(vid));
        }
        Ok(())
    }

    /// Read one vertex from the pinned durable basis plus the canonical staged
    /// effects. This performs no preparation or publication.
    pub fn vertex<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        vid: VId,
    ) -> Result<Option<VertexRow>, WriteTxnError> {
        self.ensure_database(database)?;

        let live = database.frontier()?;
        let overlay = if live == self.basis {
            database.vertex(vid)?
        } else {
            database.vertex_at(vid, self.basis)?
        };
        Ok(self.vertex_over_basis(vid, overlay))
    }

    /// Point and bulk reads apply precisely the net effects that commit will
    /// publish, just as edge reads do. Reinterpreting raw intents would invent
    /// transient creations or assign the wrong birth ordinal after grouping
    /// independent relation prefixes. Before commit, created_at is the basis
    /// placeholder; birth ordinals and content already match the prepared net.
    fn vertex_over_basis(&self, vid: VId, mut overlay: Option<VertexRow>) -> Option<VertexRow> {
        use fgdb_delta_types::DeltaRow;
        if let Some(prepared) = &self.prepared {
            for coordinate in prepared.template.coordinate_entries() {
                for effect in &coordinate.rows {
                    match effect {
                        DeltaRow::CreateVertex {
                            vid: row_vid,
                            birth_ordinal,
                            labels,
                            props,
                            ..
                        } if *row_vid == vid => {
                            overlay = Some(VertexRow {
                                vid,
                                birth_ordinal: *birth_ordinal,
                                created_at: self.basis,
                                retired_at: None,
                                labels: labels.clone(),
                                props: props.clone(),
                            });
                        }
                        DeltaRow::DeleteVertex { vid: row_vid, .. } if *row_vid == vid => {
                            overlay = None;
                        }
                        DeltaRow::LabelMembership { vid: row_vid, label, after, .. }
                            if *row_vid == vid =>
                        {
                            if let Some(row) = overlay.as_mut() {
                                match row.labels.binary_search(label) {
                                    Ok(at) if !after => { row.labels.remove(at); }
                                    Err(at) if *after => row.labels.insert(at, *label),
                                    Ok(_) | Err(_) => {}
                                }
                            }
                        }
                        DeltaRow::Property {
                            elem: ElementId::Vertex(row_vid), property, after, ..
                        } if *row_vid == vid => {
                            if let Some(row) = overlay.as_mut() {
                                Self::overlay_property(&mut row.props, *property, after.as_ref());
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        // Negative reads remain observations even when NENF erased a create
        // and its delete, or an ensure emitted no effect.
        self.read_set.borrow_mut().insert(ElementId::Vertex(vid));
        overlay
    }

    /// Read every vertex from the pinned basis through canonical staged effects,
    /// sorted by vertex identity. Empty results retain an insertion witness.
    pub fn vertices<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
    ) -> Result<Vec<VertexRow>, WriteTxnError> {
        self.vertices_for_scan(database, None)
    }

    /// Read the same complete overlay for evaluation and budget accounting,
    /// but scope insertion dependencies to a node plan's required label.
    /// Existing rows remain conservatively observed, including filtered rows.
    fn vertices_for_scan<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        label: Option<LabelId>,
    ) -> Result<Vec<VertexRow>, WriteTxnError> {
        self.ensure_database(database)?;

        let mut basis: std::collections::BTreeMap<VId, VertexRow> = database
            .vertices_at(self.basis)?
            .into_iter()
            .map(|row| (row.vid, row))
            .collect();
        if let Some(label) = label {
            self.scanned_vertex_labels.borrow_mut().insert(label);
        } else {
            self.scanned_vertices.set(true);
        }
        let mut vids: std::collections::BTreeSet<VId> = basis.keys().copied().collect();
        for pending in self.staged.iter().flat_map(|batch| &batch.rows) {
            match pending {
                PendingRow::Vertex { vid, .. } | PendingRow::DeleteVertex { vid, .. } => {
                    vids.insert(*vid);
                }
                PendingRow::Edge { .. }
                | PendingRow::DeleteEdge { .. }
                | PendingRow::SetLabel { .. }
                | PendingRow::SetEdgeProperty { .. }
                | PendingRow::SetProperty { .. }
                | PendingRow::CompareAndSet { .. } => {}
            }
        }

        let mut rows = Vec::new();
        for vid in vids {
            if let Some(row) = self.vertex_over_basis(vid, basis.remove(&vid)) {
                rows.push(row);
            }
        }
        rows.sort_by_key(|row| row.vid);
        self.read_set
            .borrow_mut()
            .extend(rows.iter().map(|row| ElementId::Vertex(row.vid)));
        Ok(rows)
    }
}
