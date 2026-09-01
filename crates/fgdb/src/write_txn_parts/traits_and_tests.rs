impl core::fmt::Debug for WriteTxn {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("WriteTxn")
            .field("basis", &self.basis)
            .field("staged_batches", &self.staged.len())
            .field("has_prepared_write", &self.prepared.is_some())
            .field("read_set_len", &self.read_set.borrow().len())
            .field(
                "match_expansion_count",
                &self.match_expansions.borrow().len(),
            )
            .field(
                "pin_obligation",
                &self.pin.as_ref().map(PurposeObligation::id),
            )
            .finish()
    }
}

impl Drop for WriteTxn {
    fn drop(&mut self) {
        self.release_pin();
    }
}

#[cfg(test)]
mod tests {
    use super::WriteTxnError;
    use crate::{Database, DatabaseKeys, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::RelationId;
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts, VId};

    #[test]
    fn write_refuses_an_advanced_snapshot_without_preparing() {
        let ((), report) = run_async_under_lab(0x7a_10, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txn_cx = contexts.txn();
            let baseline = txn_cx.outstanding_obligations();
            let directory = std::env::temp_dir().join(format!(
                "fgdb-write-txn-snapshot-advanced-{}",
                std::process::id()
            ));
            let keys = DatabaseKeys::new(
                [0x5a; 32],
                DatabaseSecurityNamespaceId([0x77; 32]),
                [0x3c; 32],
            );
            let mut database = Database::create(&commit, &directory, keys)
                .await
                .expect("database creates");
            let mut transaction = database.begin(&txn_cx).expect("transaction begins");
            let pinned = transaction.basis();

            let mut advancing = WriteBatch::new(RelationId(1));
            advancing.create_vertex(VId(1), Vec::new(), Vec::new());
            let live = database
                .write(&commit, advancing)
                .await
                .expect("autocommit advances the live frontier");

            let mut stale = WriteBatch::new(RelationId(1));
            stale.create_vertex(VId(2), Vec::new(), Vec::new());
            let error = transaction
                .write(&mut database, stale)
                .expect_err("a stale transaction cannot prepare against the live fold");
            assert!(matches!(
                error,
                WriteTxnError::SnapshotAdvanced {
                    pinned: error_pinned,
                    live: error_live,
                } if error_pinned == pinned && error_live == live
            ));
            assert!(
                transaction.staged.is_empty(),
                "snapshot refusal must not retain a staged batch"
            );
            assert!(
                transaction.prepared.is_none(),
                "snapshot refusal must not retain a prepared template"
            );
            assert_eq!(
                txn_cx.outstanding_obligations(),
                baseline + 1,
                "snapshot refusal keeps the pin live until explicit abort"
            );

            transaction.abort();
            assert_eq!(txn_cx.outstanding_obligations(), baseline);
        });

        assert!(report.lab_test_passed(), "lab run failed: {report:?}");
    }
}
