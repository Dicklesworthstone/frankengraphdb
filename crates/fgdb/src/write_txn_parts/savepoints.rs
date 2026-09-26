// Savepoints retain the SAME prepared overlay as ordinary transaction staging.
// They neither publish Chronicle records nor create independently owned pins.

const MAX_EMBEDDED_SAVEPOINTS: usize = 64;

struct EmbeddedSavepoint {
    name: String,
    staged_len: usize,
    prepared: Option<PreparedWrite>,
}

impl WriteTxn {
    /// Save the exact staged prefix without committing it or changing the basis.
    /// Names are case-sensitive and local to this transaction. Reusing a name
    /// shadows its older savepoint until the newer one is released.
    ///
    /// This bounded embedded API permits 64 live savepoints. Each owns a clone
    /// of the current prepared template, not another snapshot pin. The count
    /// bound is not byte-level memory/spill governance. Completion or abort
    /// discards every savepoint; none is durable or usable by another transaction.
    pub fn savepoint<V: Vfs + Clone>(
        &mut self,
        database: &Database<V>,
        name: &str,
    ) -> Result<(), WriteTxnError> {
        self.ensure_database(database)?;
        database.frontier()?;
        if self.savepoints.len() >= MAX_EMBEDDED_SAVEPOINTS {
            return Err(WriteTxnError::SavepointLimit {
                limit: MAX_EMBEDDED_SAVEPOINTS,
            });
        }
        let saved = EmbeddedSavepoint {
            name: name.to_owned(),
            staged_len: self.staged.len(),
            prepared: self.prepared.clone(),
        };
        self.savepoints.push(saved);
        Ok(())
    }

    /// Restore the most recent savepoint with this name and discard younger
    /// savepoints. The target remains live and can be rolled back to again.
    /// No preparation, ID allocation, I/O or durable publication occurs here.
    ///
    /// Only effects rewind. Point reads, negative reads, scans, expansions and
    /// the before-image dependencies of discarded preparations remain conflict
    /// witnesses, including conditional operations normalized into no-ops.
    /// Database-owned identity reservations are never reclaimed.
    ///
    /// A healthy owner's frontier may have advanced: restore the exact cached
    /// prepared write at the ORIGINAL basis rather than rebasing it. Ordinary
    /// finish/commit must still validate the complete intervening history.
    /// Wrong-owner, terminal, unhealthy and unknown-name refusals change nothing.
    pub fn rollback_to_savepoint<V: Vfs + Clone>(
        &mut self,
        database: &Database<V>,
        name: &str,
    ) -> Result<(), WriteTxnError> {
        self.ensure_database(database)?;
        database.frontier()?;
        let index = self
            .savepoints
            .iter()
            .rposition(|saved| saved.name == name)
            .ok_or(WriteTxnError::UnknownSavepoint)?;
        let staged_len = self.savepoints[index].staged_len;
        // Clone before changing live workspace state. Retain the target's
        // original prepared value so repeat rollback cannot change its meaning.
        let restored = self.savepoints[index].prepared.clone();
        let appended = self.staged.len() > staged_len;
        let discarded = core::mem::replace(&mut self.prepared, restored);
        self.staged.truncate(staged_len);
        self.savepoints.truncate(index + 1);
        if appended && let Some(prepared) = discarded {
            // The latest successful preparation includes the whole raw prefix,
            // even observations erased from its canonical net effects. Failed
            // preparations already preserve their own observations in write().
            prepared
                .dependencies
                .retain_observations(self.read_set.get_mut());
        }
        Ok(())
    }

    /// Release the most recent named savepoint and all younger savepoints.
    /// Staged effects and every conflict witness remain unchanged. An older
    /// same-name savepoint becomes visible again. Release never commits.
    pub fn release_savepoint<V: Vfs + Clone>(
        &mut self,
        database: &Database<V>,
        name: &str,
    ) -> Result<(), WriteTxnError> {
        self.ensure_database(database)?;
        database.frontier()?;
        let index = self
            .savepoints
            .iter()
            .rposition(|saved| saved.name == name)
            .ok_or(WriteTxnError::UnknownSavepoint)?;
        self.savepoints.truncate(index);
        Ok(())
    }
}

#[cfg(test)]
mod savepoint_tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    fn keys() -> crate::DatabaseKeys {
        crate::DatabaseKeys::new(
            [0x81; 32],
            DatabaseSecurityNamespaceId([0x82; 32]),
            [0x83; 32],
        )
    }

    fn creation(vid: u128) -> WriteBatch {
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(VId(vid), vec![], vec![]);
        batch
    }

    fn seeded_vertex() -> WriteBatch {
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(
            VId(1),
            vec![],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(10))],
        );
        batch
    }

    #[test]
    fn rollback_restores_exact_overlay_and_commits_only_the_saved_prefix() {
        let ((), report) = run_async_under_lab(0xb10c_1001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            db.write(&commit, seeded_vertex()).await.unwrap();
            let basis = db.frontier().unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            txn.write(&mut db, creation(777)).unwrap();
            let saved_template = txn.prepared.as_ref().unwrap().template.clone();
            txn.savepoint(&db, "edit").unwrap();
            let mut edit = creation(2);
            edit.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(11)));
            txn.write(&mut db, edit).unwrap();
            assert_eq!(
                txn.vertex(&db, VId(1)).unwrap().unwrap().props[0].1,
                CanonicalScalar::Int(11)
            );
            assert!(txn.vertex(&db, VId(2)).unwrap().is_some());

            txn.rollback_to_savepoint(&db, "edit").unwrap();
            assert_eq!(txn.prepared.as_ref().unwrap().template, saved_template);
            assert_eq!(
                txn.vertex(&db, VId(1)).unwrap().unwrap().props[0].1,
                CanonicalScalar::Int(10)
            );
            assert!(txn.vertex(&db, VId(2)).unwrap().is_none());
            assert!(txn.vertex(&db, VId(777)).unwrap().is_some());
            assert_eq!(db.frontier().unwrap(), basis);
            txn.rollback_to_savepoint(&db, "edit").unwrap();
            let committed = txn.commit(&mut db, &commit).await.unwrap();
            assert_eq!(committed, CommitSeq(basis.0 + 1));
            assert!(db.vertex(VId(777)).unwrap().is_some());
            assert!(db.vertex(VId(2)).unwrap().is_none());
            assert_eq!(
                db.vertex(VId(1)).unwrap().unwrap().props[0].1,
                CanonicalScalar::Int(10)
            );
            assert!(txn.savepoints.is_empty());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn duplicate_names_shadow_and_release_keeps_effects() {
        let ((), report) = run_async_under_lab(0xb10c_1002, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let basis = db.frontier().unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            txn.savepoint(&db, "step").unwrap();
            txn.write(&mut db, creation(1)).unwrap();
            txn.savepoint(&db, "step").unwrap();
            txn.write(&mut db, creation(2)).unwrap();
            txn.savepoint(&db, "inner").unwrap();
            txn.write(&mut db, creation(3)).unwrap();

            txn.rollback_to_savepoint(&db, "step").unwrap();
            assert_eq!(txn.savepoints.len(), 2);
            assert!(txn.vertex(&db, VId(1)).unwrap().is_some());
            assert!(txn.vertex(&db, VId(2)).unwrap().is_none());
            assert!(txn.vertex(&db, VId(3)).unwrap().is_none());
            assert!(matches!(
                txn.rollback_to_savepoint(&db, "inner"),
                Err(WriteTxnError::UnknownSavepoint)
            ));
            txn.write(&mut db, creation(4)).unwrap();
            txn.release_savepoint(&db, "step").unwrap();
            assert_eq!(txn.savepoints.len(), 1);
            assert!(txn.vertex(&db, VId(4)).unwrap().is_some());
            assert!(matches!(
                txn.rollback_to_savepoint(&db, "Step"),
                Err(WriteTxnError::UnknownSavepoint)
            ));
            txn.rollback_to_savepoint(&db, "step").unwrap();
            assert!(txn.prepared.is_none());
            assert!(txn.staged.is_empty());
            assert!(txn.vertex(&db, VId(1)).unwrap().is_none());
            assert!(txn.vertex(&db, VId(4)).unwrap().is_none());
            assert!(matches!(txn.finish(&mut db, &commit).await.unwrap(),
                EmbeddedTxnCompletion::ReadClosed { snapshot_seq, validated_through }
                if snapshot_seq == basis && validated_through == basis));
            assert_eq!(db.frontier().unwrap(), basis);
            assert!(txn.savepoints.is_empty());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn rolled_back_noop_preparation_keeps_its_before_image_dependency() {
        let ((), report) = run_async_under_lab(0xb10c_1003, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            db.write(&commit, seeded_vertex()).await.unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            txn.write(&mut db, creation(777)).unwrap();
            txn.savepoint(&db, "before").unwrap();
            let mut noop = WriteBatch::new(RelationId(1));
            noop.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(10)));
            txn.write(&mut db, noop).unwrap();
            txn.rollback_to_savepoint(&db, "before").unwrap();
            let mut winner = WriteBatch::new(RelationId(1));
            winner.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(11)));
            db.write(&commit, winner).await.unwrap();
            // No intervening query may repair a lost preparation witness.
            assert!(matches!(
                txn.commit(&mut db, &commit).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01",
                    ..
                }))
            ));
            assert!(db.vertex(VId(777)).unwrap().is_none());
            assert!(txn.savepoints.is_empty());
            assert!(txn.pin.is_none());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn rollback_does_not_erase_an_empty_scan_phantom_witness() {
        let ((), report) = run_async_under_lab(0xb10c_1004, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            txn.savepoint(&db, "scan").unwrap();
            assert!(txn.vertices(&db).unwrap().is_empty());
            txn.rollback_to_savepoint(&db, "scan").unwrap();
            db.write(&commit, creation(1)).await.unwrap();
            let frontier = db.frontier().unwrap();
            assert!(matches!(
                txn.finish(&mut db, &commit).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01",
                    ..
                }))
            ));
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(txn.savepoints.is_empty());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn savepoint_operations_reject_wrong_owners_and_terminal_transactions() {
        let ((), report) = run_async_under_lab(0xb10c_1005, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let other = Database::open_memory(&commit, keys()).await.unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            txn.savepoint(&db, "owner").unwrap();
            txn.write(&mut db, creation(1)).unwrap();
            let before = txn.prepared.as_ref().unwrap().template.clone();
            assert!(matches!(
                txn.savepoint(&other, "foreign"),
                Err(WriteTxnError::WrongDatabase)
            ));
            assert!(matches!(
                txn.rollback_to_savepoint(&other, "owner"),
                Err(WriteTxnError::WrongDatabase)
            ));
            assert!(matches!(
                txn.release_savepoint(&other, "owner"),
                Err(WriteTxnError::WrongDatabase)
            ));
            assert!(matches!(
                txn.release_savepoint(&db, "absent"),
                Err(WriteTxnError::UnknownSavepoint)
            ));
            assert_eq!(txn.savepoints.len(), 1);
            assert_eq!(txn.staged.len(), 1);
            assert_eq!(txn.prepared.as_ref().unwrap().template, before);
            txn.rollback_to_savepoint(&db, "owner").unwrap();
            txn.finish(&mut db, &commit).await.unwrap();
            assert!(txn.savepoints.is_empty());
            assert!(matches!(
                txn.savepoint(&db, "new"),
                Err(WriteTxnError::Finished)
            ));
            assert!(matches!(
                txn.rollback_to_savepoint(&db, "owner"),
                Err(WriteTxnError::Finished)
            ));
            assert!(matches!(
                txn.release_savepoint(&db, "owner"),
                Err(WriteTxnError::Finished)
            ));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn rollback_after_frontier_advance_preserves_the_original_prepared_basis() {
        let ((), report) = run_async_under_lab(0xb10c_1006, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            let basis = txn.basis();
            txn.write(&mut db, creation(1)).unwrap();
            txn.savepoint(&db, "safe").unwrap();
            let saved_template = txn.prepared.as_ref().unwrap().template.clone();
            txn.write(&mut db, creation(2)).unwrap();
            db.write(&commit, creation(99)).await.unwrap();
            let advanced = db.frontier().unwrap();
            txn.rollback_to_savepoint(&db, "safe").unwrap();
            assert_eq!(txn.basis(), basis);
            assert_eq!(txn.prepared.as_ref().unwrap().basis(), basis);
            assert_eq!(txn.prepared.as_ref().unwrap().template, saved_template);
            let seq = txn.commit(&mut db, &commit).await.unwrap();
            assert_eq!(seq, CommitSeq(advanced.0 + 1));
            assert!(db.vertex(VId(1)).unwrap().is_some());
            assert!(db.vertex(VId(2)).unwrap().is_none());
            assert!(db.vertex(VId(99)).unwrap().is_some());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn savepoint_count_limit_is_atomic_and_release_returns_capacity() {
        let ((), report) = run_async_under_lab(0xb10c_1007, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let basis = db.frontier().unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            for _ in 0..MAX_EMBEDDED_SAVEPOINTS {
                txn.savepoint(&db, "bounded").unwrap();
            }
            assert!(matches!(txn.savepoint(&db, "refused"),
                Err(WriteTxnError::SavepointLimit { limit }) if limit == MAX_EMBEDDED_SAVEPOINTS));
            assert_eq!(txn.savepoints.len(), MAX_EMBEDDED_SAVEPOINTS);
            assert!(txn.prepared.is_none());
            txn.release_savepoint(&db, "bounded").unwrap();
            assert_eq!(txn.savepoints.len(), MAX_EMBEDDED_SAVEPOINTS - 1);
            txn.savepoint(&db, "replacement").unwrap();
            assert_eq!(txn.savepoints.len(), MAX_EMBEDDED_SAVEPOINTS);
            txn.finish(&mut db, &commit).await.unwrap();
            assert!(txn.savepoints.is_empty());
            assert!(txn.pin.is_none());
            assert_eq!(db.frontier().unwrap(), basis);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
