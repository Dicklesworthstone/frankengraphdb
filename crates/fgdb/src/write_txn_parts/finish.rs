impl WriteTxn {
    /// Commit the prepared batch exactly as derived, then release the pin.
    pub async fn commit<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
    ) -> Result<CommitSeq, WriteTxnError> {
        self.commit_with_crash(database, cx, None).await
    }

    /// Commit through the production crash-point path, then release the pin
    /// regardless of whether the prepared write committed or was refused.
    pub async fn commit_with_crash<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
        crash_at: Option<fgdb_chronicle::commit::CrashPoint>,
    ) -> Result<CommitSeq, WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }
        if self.prepared.is_none() {
            self.release_pin();
            return Err(WriteTxnError::NoPreparedWrite);
        }
        let conflict = match self.read_conflict(database) {
            Ok(conflict) => conflict,
            Err(source) => {
                self.release_pin();
                return Err(WriteTxnError::Read(source));
            }
        };
        if let Some((element, committed_at)) = conflict {
            self.release_pin();
            return Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                law: "FG-LAW-FCW-READ-01",
                detail: format!(
                    "read-set element {element:?} was written at {committed_at:?} after pinned basis {:?}",
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
        props: &mut Vec<(fgdb_delta_types::PropertyKeyId, fgdb_types::CanonicalScalar)>,
        key: fgdb_delta_types::PropertyKeyId,
        value: Option<&fgdb_types::CanonicalScalar>,
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

    fn read_conflict<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
    ) -> Result<Option<(ElementId, CommitSeq)>, ReadError> {
        let read_set = self.read_set.borrow();
        let match_expansions = self.match_expansions.borrow();
        if read_set.is_empty() && match_expansions.is_empty() {
            return Ok(None);
        }
        for batch in database.delta_since(self.basis)? {
            let mut touched = std::collections::BTreeSet::new();
            let mut endpoints = std::collections::BTreeSet::new();
            for coordinate in batch.coordinate_entries() {
                for row in &coordinate.rows {
                    if let fgdb_delta_types::DeltaRow::CreateEdge {
                        eid, src, relation, ..
                    } = row
                        && match_expansions.contains(&(*src, *relation))
                    {
                        return Ok(Some((ElementId::Edge(*eid), batch.commit_seq())));
                    }
                    crate::adjacency_endpoints(row, &mut endpoints);
                    crate::touched_elements(row, &mut touched);
                }
            }
            if let Some(element) = endpoints
                .into_iter()
                .find(|element| read_set.contains(element))
            {
                return Ok(Some((element, batch.commit_seq())));
            }
            if let Some(element) = touched
                .into_iter()
                .find(|element| read_set.contains(element))
            {
                return Ok(Some((element, batch.commit_seq())));
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
