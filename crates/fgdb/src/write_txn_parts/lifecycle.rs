impl WriteTxn {
    pub(crate) fn begin(
        basis: CommitSeq,
        txn: &TxnCx,
        obligation_id: ObligationId,
        handle_owner: std::sync::Arc<()>,
    ) -> Result<Self, ObligationAcquireError> {
        let pin = txn.pin_snapshot(obligation_id)?;
        Ok(Self {
            handle_owner,
            basis,
            staged: Vec::new(),
            prepared: None,
            read_set: std::cell::RefCell::new(std::collections::BTreeSet::new()),
            match_expansions: std::cell::RefCell::new(std::collections::BTreeSet::new()),
            scanned_vertices: std::cell::Cell::new(false),
            scanned_edges: std::cell::Cell::new(false),
            pin: Some(pin),
        })
    }

    /// The snapshot frontier retained for this transaction.
    #[must_use]
    pub const fn basis(&self) -> CommitSeq {
        self.basis
    }

    /// Validate lifecycle and ownership before observing another handle or
    /// changing staged/conflict state. A sequence is meaningful only within
    /// the opened writer lifetime that supplied this transaction's basis.
    fn ensure_database<V: Vfs>(&self, database: &Database<V>) -> Result<(), WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }
        if !std::sync::Arc::ptr_eq(&self.handle_owner, &database.handle_owner) {
            return Err(WriteTxnError::WrongDatabase);
        }
        Ok(())
    }

    /// Stage a same-relation batch against this transaction's pinned snapshot.
    /// Once `write_atomic` has explicitly staged several relation groups,
    /// subsequent writes are admitted under that same independence contract.
    pub fn write<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        batch: WriteBatch,
    ) -> Result<(), WriteTxnError> {
        self.ensure_database(database)?;

        let live = database.frontier()?;
        if live != self.basis {
            return Err(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live,
            });
        }
        if let Some(first) = self.staged.first()
            && self.staged.iter().any(|staged| staged.relation != first.relation)
        {
            return self.write_atomic(database, vec![batch]);
        }
        if let Some(expected) = self.staged.first().map(|staged| staged.relation)
            && batch.relation != expected
        {
            return Err(WriteTxnError::RelationMismatch {
                expected,
                found: batch.relation,
            });
        }
        if batch.rows.iter().any(|row| matches!(row, PendingRow::Edge { ensure: true, .. })) {
            // Keep actual ensure aliases and insertion witnesses even if
            // subsequent preparation fails or normalizes the ensure to no-op.
            drop(self.edges(database)?);
        }
        self.staged.push(batch);
        let combined = Self::combined_batch(&self.staged)
            .expect("a batch was staged immediately before combination");
        let prepared = match database.prepare_write(combined) {
            Ok(prepared) => prepared,
            Err(source) => {
                let _ = self.staged.pop();
                return Err(WriteTxnError::Write(source));
            }
        };
        debug_assert_eq!(prepared.basis(), self.basis);
        self.prepared = Some(prepared);
        Ok(())
    }

    /// Atomically stage independent relation groups, including prior batches.
    ///
    /// Each relation retains its ordered prefix. Other relations must be
    /// independent at the pinned basis, exactly as `prepare_atomic_writes`
    /// requires. This does not permit one relation to consume another's new
    /// vertex. A refusal preserves the old staged effects and prepared write;
    /// any reads already made still participate in conflict validation.
    /// No capsule or marker is published until the ordinary `commit` method.
    pub fn write_atomic<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        batches: Vec<WriteBatch>,
    ) -> Result<(), WriteTxnError> {
        self.ensure_database(database)?;
        let live = database.frontier()?;
        if live != self.basis {
            return Err(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live,
            });
        }
        if batches.is_empty() || batches.iter().any(WriteBatch::is_empty) {
            return Err(WriteError::EmptyBatch.into());
        }
        if batches.iter().flat_map(|batch| &batch.rows)
            .any(|row| matches!(row, PendingRow::Edge { ensure: true, .. }))
        {
            drop(self.edges(database)?);
        }
        let previous_len = self.staged.len();
        self.staged.extend(batches);
        let prepared = match database.prepare_atomic_writes(self.staged.clone()) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.staged.truncate(previous_len);
                return Err(error);
            }
        };
        debug_assert_eq!(prepared.basis(), self.basis);
        self.prepared = Some(prepared);
        Ok(())
    }
}
