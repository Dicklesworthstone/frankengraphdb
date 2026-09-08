impl WriteTxn {
    /// Validate the pinned read/mutation footprints, commit the prepared batch
    /// exactly as derived, then release the pin.
    pub async fn commit<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
    ) -> Result<CommitSeq, WriteTxnError> {
        self.commit_with_crash(database, cx, None).await
    }

    /// Commit through the production crash-point path. Wrong-owner calls leave
    /// the transaction unchanged. An admitted owner's terminal attempt releases
    /// the pin whether validation/publication succeeds or fails.
    pub async fn commit_with_crash<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
        crash_at: Option<fgdb_chronicle::commit::CrashPoint>,
    ) -> Result<CommitSeq, WriteTxnError> {
        self.ensure_database(database)?;
        if self.prepared.is_none() {
            self.release_pin();
            return Err(WriteTxnError::NoPreparedWrite);
        }
        let conflict = match self.transaction_conflict(database) {
            Ok(conflict) => conflict,
            Err(source) => {
                self.release_pin();
                return Err(WriteTxnError::Read(source));
            }
        };
        if let Some((law, element, committed_at)) = conflict {
            self.release_pin();
            return Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                law,
                detail: format!(
                    "transaction dependency {element:?} changed at {committed_at:?} after pinned basis {:?}",
                    self.basis
                ),
            }));
        }
        let prepared = self
            .prepared
            .take()
            .expect("the prepared write was checked immediately above");
        self.staged.clear();

        let result = database
            .commit_prepared_with_crash(cx, prepared, crash_at)
            .await
            .map_err(WriteTxnError::Write);
        self.release_pin();
        result
    }

    /// End the transaction without publishing its prepared batch.
    pub fn abort(mut self) {
        self.staged.clear();
        self.prepared = None;
        self.release_pin();
    }

    fn combined_batch(staged: &[WriteBatch]) -> Option<WriteBatch> {
        let mut batches = staged.iter().cloned();
        let mut combined = batches.next()?;
        for mut batch in batches {
            debug_assert_eq!(batch.relation, combined.relation);
            combined.rows.append(&mut batch.rows);
        }
        Some(combined)
    }

    fn overlay_property(
        props: &mut Vec<(fgdb_delta_types::PropertyKeyId, CanonicalScalar)>,
        key: fgdb_delta_types::PropertyKeyId,
        value: Option<&CanonicalScalar>,
    ) {
        match props.binary_search_by_key(&key, |(property, _)| *property) {
            Ok(at) => match value {
                Some(value) => props[at].1 = value.clone(),
                None => {
                    props.remove(at);
                }
            },
            Err(at) => {
                if let Some(value) = value {
                    props.insert(at, (key, value.clone()));
                }
            }
        }
    }

    /// Preparation observes more than its net writes: conditional no-ops and
    /// ensure-existing operations still depend on their targets, and edge
    /// creation depends on its endpoints. Keep those dependencies even when
    /// canonicalization removes the corresponding mutation from the template.
    fn mutation_footprint(&self) -> std::collections::BTreeSet<ElementId> {
        let mut footprint = std::collections::BTreeSet::new();
        for pending in self.staged.iter().flat_map(|batch| &batch.rows) {
            match pending {
                PendingRow::Vertex { vid, .. }
                | PendingRow::DeleteVertex { vid, .. }
                | PendingRow::SetLabel { vid, .. }
                | PendingRow::SetProperty { vid, .. } => {
                    footprint.insert(ElementId::Vertex(*vid));
                }
                PendingRow::Edge { eid, src, dst, .. } => {
                    footprint.insert(ElementId::Edge(*eid));
                    footprint.insert(ElementId::Vertex(*src));
                    footprint.insert(ElementId::Vertex(*dst));
                }
                PendingRow::DeleteEdge { eid, .. }
                | PendingRow::SetEdgeProperty { eid, .. } => {
                    footprint.insert(ElementId::Edge(*eid));
                }
                PendingRow::CompareAndSet { elem, .. } => {
                    footprint.insert(*elem);
                }
            }
        }
        if let Some(prepared) = &self.prepared {
            for coordinate in prepared.template.coordinate_entries() {
                for row in &coordinate.rows {
                    // Include engine-derived cascade targets, not only the
                    // identifiers the caller happened to name explicitly.
                    crate::touched_elements(row, &mut footprint);
                }
            }
        }
        footprint
    }

    fn transaction_conflict<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
    ) -> Result<Option<(&'static str, ElementId, CommitSeq)>, ReadError> {
        let read_set = self.read_set.borrow();
        let match_expansions = self.match_expansions.borrow();
        let scanned_vertex_labels = self.scanned_vertex_labels.borrow();
        let mutation_footprint = self.mutation_footprint();
        let scanned_vertices = self.scanned_vertices.get();
        let scanned_edges = self.scanned_edges.get();
        if read_set.is_empty()
            && match_expansions.is_empty()
            && scanned_vertex_labels.is_empty()
            && mutation_footprint.is_empty()
            && !scanned_vertices
            && !scanned_edges
        {
            return Ok(None);
        }
        // One complete suffix serves both read and mutation validation. Do not
        // trust the coordinator's resettable in-memory FCW map for an old basis.
        // delta_since refuses a retired prefix instead of silently validating
        // against only the surviving tail. Validation precedes prepared.take().
        for batch in database.delta_since(self.basis)? {
            let seq = batch.commit_seq();
            let mut touched = std::collections::BTreeSet::new();
            let mut endpoints = std::collections::BTreeSet::new();
            for coordinate in batch.coordinate_entries() {
                for row in &coordinate.rows {
                    match row {
                        fgdb_delta_types::DeltaRow::CreateVertex { vid, labels, .. }
                            if scanned_vertices
                                || labels
                                    .iter()
                                    .any(|label| scanned_vertex_labels.contains(label)) =>
                        {
                            return Ok(Some(("FG-LAW-FCW-READ-01", ElementId::Vertex(*vid), seq)));
                        }
                        fgdb_delta_types::DeltaRow::LabelMembership {
                            vid,
                            label,
                            after: true,
                            ..
                        } if scanned_vertex_labels.contains(label) => {
                            return Ok(Some(("FG-LAW-FCW-READ-01", ElementId::Vertex(*vid), seq)));
                        }
                        fgdb_delta_types::DeltaRow::CreateEdge {
                            eid, src, relation, ..
                        } if scanned_edges || match_expansions.contains(&(*src, *relation)) => {
                            return Ok(Some(("FG-LAW-FCW-READ-01", ElementId::Edge(*eid), seq)));
                        }
                        _ => {}
                    }
                    crate::adjacency_endpoints(row, &mut endpoints);
                    crate::touched_elements(row, &mut touched);
                }
            }
            if let Some(element) = endpoints
                .iter()
                .chain(touched.iter())
                .find(|element| read_set.contains(*element))
            {
                return Ok(Some(("FG-LAW-FCW-READ-01", *element, seq)));
            }
            if let Some(element) = endpoints
                .iter()
                .chain(touched.iter())
                .find(|element| mutation_footprint.contains(*element))
            {
                return Ok(Some(("FG-LAW-FCW-01", *element, seq)));
            }
        }
        Ok(None)
    }

    fn release_pin(&mut self) {
        if let Some(pin) = self.pin.take() {
            let _receipt = pin.abort();
        }
    }
}
