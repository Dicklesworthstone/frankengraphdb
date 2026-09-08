impl WriteTxn {
    /// Read one vertex from the pinned durable basis plus this transaction's
    /// staged row-order overlay. This performs no preparation or publication.
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

    /// Reuse the same ordered staged evaluator for point and table reads.
    fn vertex_over_basis(&self, vid: VId, mut overlay: Option<VertexRow>) -> Option<VertexRow> {
        let mut intent_ordinal = 0u64;
        for pending in self.staged.iter().flat_map(|batch| &batch.rows) {
            intent_ordinal = intent_ordinal
                .checked_add(1)
                .expect("a transaction cannot stage 2^64 rows");
            match pending {
                PendingRow::Vertex {
                    vid: row_vid,
                    labels,
                    props,
                    ensure: _,
                } if *row_vid == vid && overlay.is_none() => {
                    let mut labels = labels.clone();
                    let mut props = props.clone();
                    crate::sort_write_labels_and_props(&mut labels, &mut props);
                    overlay = Some(VertexRow {
                        vid,
                        birth_ordinal: intent_ordinal,
                        created_at: self.basis,
                        retired_at: None,
                        labels,
                        props,
                    });
                }
                PendingRow::DeleteVertex { vid: row_vid, .. } if *row_vid == vid => {
                    overlay = None;
                }
                PendingRow::SetLabel {
                    vid: row_vid,
                    label,
                    member,
                } if *row_vid == vid => {
                    if let Some(row) = overlay.as_mut() {
                        match row.labels.binary_search(label) {
                            Ok(at) if !member => {
                                row.labels.remove(at);
                            }
                            Err(at) if *member => row.labels.insert(at, *label),
                            Ok(_) | Err(_) => {}
                        }
                    }
                }
                PendingRow::SetProperty {
                    vid: row_vid,
                    key,
                    value,
                } if *row_vid == vid => {
                    if let Some(row) = overlay.as_mut() {
                        Self::overlay_property(&mut row.props, *key, value.as_ref());
                    }
                }
                PendingRow::CompareAndSet {
                    elem: ElementId::Vertex(row_vid),
                    key,
                    expected,
                    value,
                    ..
                } if *row_vid == vid => {
                    if let Some(row) = overlay.as_mut() {
                        let actual = row
                            .props
                            .binary_search_by_key(key, |(property, _)| *property)
                            .ok()
                            .map(|at| &row.props[at].1);
                        if actual == expected.as_deref() {
                            Self::overlay_property(&mut row.props, *key, Some(value.as_ref()));
                        }
                    }
                }
                PendingRow::Vertex { .. }
                | PendingRow::Edge { .. }
                | PendingRow::DeleteEdge { .. }
                | PendingRow::DeleteVertex { .. }
                | PendingRow::SetLabel { .. }
                | PendingRow::SetEdgeProperty { .. }
                | PendingRow::SetProperty { .. }
                | PendingRow::CompareAndSet { .. } => {}
            }
        }
        self.read_set.borrow_mut().insert(ElementId::Vertex(vid));
        overlay
    }

    /// Read every vertex from the pinned basis through this transaction's
    /// staged row-order overlay, sorted by vertex identity.
    pub fn vertices<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
    ) -> Result<Vec<VertexRow>, WriteTxnError> {
        self.ensure_database(database)?;

        // Keep the admitted rows rather than discarding them and resolving the
        // same immutable patch history once more for every vertex identity.
        let mut basis: std::collections::BTreeMap<VId, VertexRow> = database
            .vertices_at(self.basis)?
            .into_iter()
            .map(|row| (row.vid, row))
            .collect();
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
