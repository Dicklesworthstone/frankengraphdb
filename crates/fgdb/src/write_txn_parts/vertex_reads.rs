impl WriteTxn {
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
                        DeltaRow::LabelMembership {
                            vid: row_vid,
                            label,
                            after,
                            ..
                        } if *row_vid == vid => {
                            if let Some(row) = overlay.as_mut() {
                                match row.labels.binary_search(label) {
                                    Ok(at) if !after => {
                                        row.labels.remove(at);
                                    }
                                    Err(at) if *after => row.labels.insert(at, *label),
                                    Ok(_) | Err(_) => {}
                                }
                            }
                        }
                        DeltaRow::Property {
                            elem: ElementId::Vertex(row_vid),
                            property,
                            after,
                            ..
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
