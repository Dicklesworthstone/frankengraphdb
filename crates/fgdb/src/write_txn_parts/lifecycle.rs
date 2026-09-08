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

        if let Some(expected) = self.staged.first().map(|staged| staged.relation)
            && batch.relation != expected
        {
            return Err(WriteTxnError::RelationMismatch {
                expected,
                found: batch.relation,
            });
        }
        self.staged.push(batch);
        let combined = Self::combined_batch(&self.staged)
            .expect("a batch was staged immediately before combination");
        let prepared = match database.prepare_write(combined) {
            Ok(prepared) => prepared,
            Err(source) => {
                self.staged.pop();
                return Err(WriteTxnError::Write(source));
            }
        };
        debug_assert_eq!(prepared.basis(), self.basis);
        self.prepared = Some(prepared);
        Ok(())
    }
}
