impl WriteTxn {
    /// Stage against the original transaction basis even after other commits.
    ///
    /// This is the explicit stable-snapshot alternative to [`Self::write`],
    /// whose current-frontier admission contract is unchanged. It neither
    /// refreshes the basis nor rebases successful effects onto newer values.
    /// Guards, ENSURE aliases, cascades and read-your-writes all use the pinned
    /// basis plus the retained staged prefix. Completion still validates every
    /// intervening commit through the existing observation and FCW checks.
    ///
    /// Ordinary staging owns relation admission, normalization and rollback;
    /// this adapter only selects its preparation basis. Refused statements keep
    /// their original-basis observations, including diagnostics and cascades.
    /// No durable effect or publication occurs here. Older bases require a
    /// retained delta prefix and native in-memory reconstruction; this is not
    /// full SSI, automatic query refresh, a retention lease or a spill path.
    pub fn write_at_basis<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        batch: WriteBatch,
    ) -> Result<(), WriteTxnError> {
        // Do not inspect/reconstruct a foreign database before owner admission.
        self.ensure_database(database)?;
        database
            .preparation_basis(self.basis)?
            .stage_write(self, batch)
    }

    /// Stage independent relation groups at the unchanged transaction basis.
    ///
    /// Shares [`Self::write_atomic`]'s complete-prefix independence and shared
    /// vertex-initialization rules, including canonical relation-order births.
    /// Other writers need not stop, but conflicting commits still cause normal
    /// finalization to refuse. This never silently chooses ordered composition.
    pub fn write_atomic_at_basis<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        batches: Vec<WriteBatch>,
    ) -> Result<(), WriteTxnError> {
        self.ensure_database(database)?;
        database
            .preparation_basis(self.basis)?
            .stage_atomic_writes(self, batches)
    }

    /// Stage dependent relation groups without moving the transaction basis.
    ///
    /// Shares [`Self::write_ordered`]'s source-order and observed-birth retention
    /// rules. Later instructions see the earlier staged prefix, not another
    /// writer's successor state. Savepoint rollback and commit remain unchanged.
    pub fn write_ordered_at_basis<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        batches: Vec<WriteBatch>,
    ) -> Result<(), WriteTxnError> {
        self.ensure_database(database)?;
        database
            .preparation_basis(self.basis)?
            .stage_ordered_writes(self, batches)
    }

    /// Admit the complete ordered program's expanded rows at its pinned basis.
    ///
    /// Routing and refused-routing observations use historical edge ownership.
    /// The limit has exactly [`Self::write_ordered_bounded`]'s scope: expanded
    /// evaluator-input rows, not historical reconstruction work or bytes. One
    /// reconstruction serves both borrowed admission and successful staging.
    pub fn write_ordered_at_basis_bounded<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        batches: Vec<WriteBatch>,
        max_expanded_rows: u64,
    ) -> Result<(), WriteTxnError> {
        self.ensure_database(database)?;
        database
            .preparation_basis(self.basis)?
            .stage_ordered_writes_bounded(self, batches, max_expanded_rows)
    }
}

#[cfg(test)]
mod historical_staging_tests {
    use super::*;
    use crate::{DatabaseKeys, MemVfs, WriteMismatchPolicy};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    const P: PropertyKeyId = PropertyKeyId(1);

    async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
        let keys = DatabaseKeys::new(
            [0x41; 32],
            DatabaseSecurityNamespaceId([0x42; 32]),
            [0x43; 32],
        );
        let mut db = Database::open_memory(cx, keys).await.unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        for id in 1..=3 {
            batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(10))]);
        }
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(cx, batch).await.unwrap();
        db
    }

    fn create(id: u128, relation: u64) -> WriteBatch {
        let mut batch = WriteBatch::new(RelationId(relation));
        batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(1))]);
        batch
    }

    fn change(id: u128, value: i64) -> WriteBatch {
        let mut batch = WriteBatch::new(RelationId(1));
        batch.set_vertex_property(VId(id), P, Some(CanonicalScalar::Int(value)));
        batch
    }

    fn bad_guard(id: u128) -> WriteBatch {
        let mut batch = WriteBatch::new(RelationId(1));
        batch.compare_and_set_vertex_property(
            VId(id),
            P,
            Some(CanonicalScalar::Int(999)),
            CanonicalScalar::Int(20),
            WriteMismatchPolicy::AbortWrite,
        );
        batch
    }

    #[test]
    fn first_and_later_statements_can_stage_after_a_disjoint_commit() {
        let ((), report) = run_async_under_lab(0x6261_1001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut db = seeded(&cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            let basis = txn.basis();
            let pinned = db.pinned_read_view().unwrap();
            let live = db.write(&cx, change(3, 90)).await.unwrap();
            let original = std::sync::Arc::clone(&db.snapshot);
            // Legacy callers still get their explicitly documented admission.
            assert!(matches!(
                txn.write(&mut db, create(5, 1)),
                Err(WriteTxnError::SnapshotAdvanced { .. })
            ));
            txn.write_at_basis(&mut db, create(5, 1)).unwrap();
            let mut suffix = WriteBatch::new(RelationId(1));
            suffix.compare_and_set_vertex_property(
                VId(5),
                P,
                Some(CanonicalScalar::Int(1)),
                CanonicalScalar::Int(2),
                WriteMismatchPolicy::AbortWrite,
            );
            suffix.add_edge(EId(50), VId(1), VId(5), vec![]);
            txn.write_at_basis(&mut db, suffix).unwrap();
            assert_eq!(txn.basis(), basis);
            assert_eq!(txn.prepared.as_ref().unwrap().basis(), basis);
            assert_eq!(
                txn.vertex(&db, VId(5)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(2))]
            );
            assert!(db.vertex(VId(5)).unwrap().is_none());
            assert_eq!(db.frontier().unwrap(), live);
            assert!(std::sync::Arc::ptr_eq(&db.snapshot, &original));
            let committed = txn.commit(&mut db, &cx).await.unwrap();
            assert_eq!(committed, CommitSeq(live.0 + 1));
            assert_eq!(db.delta_since(live).unwrap().count(), 1);
            assert!(db.edge(EId(50)).unwrap().is_some());
            assert!(pinned.vertex(VId(5)).unwrap().is_none());
            assert_eq!(
                db.vertex(VId(3)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(90))]
            );
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn atomic_ordered_and_bounded_paths_reuse_their_original_staging_laws() {
        let ((), report) = run_async_under_lab(0x6261_1002, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            for mode in 0..3 {
                let mut db = seeded(&cx).await;
                let mut txn = db.begin(&txcx).unwrap();
                let basis = txn.basis();
                txn.write_at_basis(&mut db, create(5, 9)).unwrap();
                txn.savepoint(&db, "prefix").unwrap();
                let prefix = txn.prepared.as_ref().unwrap().template.clone();
                let live = db.write(&cx, change(3, 90)).await.unwrap();
                let mut suffix = create(6, 2);
                if mode != 0 {
                    suffix.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(2)));
                    suffix.add_edge(EId(60), VId(5), VId(6), vec![]);
                }
                match mode {
                    0 => txn.write_atomic_at_basis(&mut db, vec![suffix]).unwrap(),
                    1 => txn.write_ordered_at_basis(&mut db, vec![suffix]).unwrap(),
                    _ => {
                        // Three vertex instructions in two relation slices,
                        // plus one edge instruction: exactly seven inputs.
                        assert!(matches!(
                            txn.write_ordered_at_basis_bounded(&mut db, vec![suffix.clone()], 6),
                            Err(WriteTxnError::OrderedWriteBudgetExceeded {
                                limit: 6,
                                required: 7,
                            })
                        ));
                        assert_eq!(txn.prepared.as_ref().unwrap().template, prefix);
                        assert_eq!(txn.savepoints.len(), 1);
                        txn.write_ordered_at_basis_bounded(&mut db, vec![suffix], 7)
                            .unwrap();
                    }
                }
                assert_eq!(txn.basis(), basis);
                assert!(txn.vertex(&db, VId(6)).unwrap().is_some());
                assert!(db.vertex(VId(6)).unwrap().is_none());
                assert_eq!(db.frontier().unwrap(), live);
                txn.commit(&mut db, &cx).await.unwrap();
                assert!(db.vertex(VId(5)).unwrap().is_some());
                assert!(db.vertex(VId(6)).unwrap().is_some());
                assert_eq!(db.delta_since(live).unwrap().count(), 1);
                if mode != 0 {
                    assert!(db.edge(EId(60)).unwrap().is_some());
                }
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn refused_historical_guard_keeps_old_actual_and_conflicts_after_rollback() {
        let ((), report) = run_async_under_lab(0x6261_1003, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut db = seeded(&cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            txn.write_at_basis(&mut db, create(5, 1)).unwrap();
            txn.savepoint(&db, "prefix").unwrap();
            let prefix = txn.prepared.as_ref().unwrap().template.clone();
            let WriteError::CompareAndSetMismatch(expected) =
                db.prepare_write(bad_guard(1)).unwrap_err()
            else {
                panic!("the basis guard must fail");
            };
            let live = db.write(&cx, change(1, 90)).await.unwrap();
            let Err(WriteTxnError::Write(WriteError::CompareAndSetMismatch(actual))) =
                txn.write_at_basis(&mut db, bad_guard(1))
            else {
                panic!("the historical guard must fail at its original basis");
            };
            assert_eq!(actual.actual, expected.actual);
            assert_eq!(txn.prepared.as_ref().unwrap().template, prefix);
            assert_eq!(txn.staged.len(), 1);
            txn.rollback_to_savepoint(&db, "prefix").unwrap();
            assert!(txn.read_set.borrow().contains(&ElementId::Vertex(VId(1))));
            assert!(matches!(
                txn.commit(&mut db, &cx).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01",
                    ..
                }))
            ));
            assert_eq!(db.frontier().unwrap(), live);
            assert!(db.vertex(VId(5)).unwrap().is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn refused_historical_cascade_retains_edges_absent_from_the_live_writer() {
        let ((), report) = run_async_under_lab(0x6261_1004, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut db = seeded(&cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            txn.write_at_basis(&mut db, create(5, 1)).unwrap();
            txn.savepoint(&db, "prefix").unwrap();
            let mut winner = WriteBatch::new(RelationId(1));
            winner.delete_edge(EId(10));
            let live = db.write(&cx, winner).await.unwrap();
            let mut attempted = WriteBatch::new(RelationId(1));
            attempted.delete_vertex(VId(1));
            attempted.extend(bad_guard(2)).unwrap();
            assert!(matches!(
                txn.write_at_basis(&mut db, attempted),
                Err(WriteTxnError::Write(WriteError::CompareAndSetMismatch(_)))
            ));
            assert!(txn.read_set.borrow().contains(&ElementId::Edge(EId(10))));
            txn.rollback_to_savepoint(&db, "prefix").unwrap();
            assert!(matches!(
                txn.commit(&mut db, &cx).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01",
                    ..
                }))
            ));
            assert_eq!(db.frontier().unwrap(), live);
            assert!(db.vertex(VId(1)).unwrap().is_some());
            assert!(db.vertex(VId(5)).unwrap().is_none());
            assert!(db.edge(EId(10)).unwrap().is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn ownership_and_terminal_refusals_precede_historical_observation() {
        let ((), report) = run_async_under_lab(0x6261_1005, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut db = seeded(&cx).await;
            let mut foreign = seeded(&cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            db.write(&cx, change(3, 90)).await.unwrap();
            assert!(matches!(
                txn.write_at_basis(&mut foreign, create(5, 1)),
                Err(WriteTxnError::WrongDatabase)
            ));
            assert!(matches!(
                txn.write_atomic_at_basis(&mut foreign, vec![]),
                Err(WriteTxnError::WrongDatabase)
            ));
            assert!(matches!(
                txn.write_ordered_at_basis(&mut foreign, vec![]),
                Err(WriteTxnError::WrongDatabase)
            ));
            assert!(matches!(
                txn.write_ordered_at_basis_bounded(&mut foreign, vec![], 0),
                Err(WriteTxnError::WrongDatabase)
            ));
            assert!(txn.staged.is_empty());
            assert!(txn.read_set.borrow().is_empty());
            assert!(txn.pin.is_some());
            assert!(matches!(
                txn.write_ordered_at_basis(&mut db, vec![]),
                Err(WriteTxnError::Write(WriteError::EmptyBatch))
            ));
            txn.finish(&mut db, &cx).await.unwrap();
            assert!(matches!(
                txn.write_at_basis(&mut db, create(5, 1)),
                Err(WriteTxnError::Finished)
            ));
            assert!(txn.pin.is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn retired_reconstruction_prefix_refuses_without_losing_already_prepared_work() {
        let ((), report) = run_async_under_lab(0x6261_1006, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut db = seeded(&cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            let basis = txn.basis();
            txn.write_at_basis(&mut db, create(5, 1)).unwrap();
            let prefix = txn.prepared.as_ref().unwrap().template.clone();
            let live = db.write(&cx, change(3, 90)).await.unwrap();
            std::sync::Arc::make_mut(&mut db.snapshot)
                .delta_index
                .retire_prefix(basis)
                .unwrap();
            let original = std::sync::Arc::clone(&db.snapshot);
            assert!(matches!(
                txn.write_at_basis(&mut db, create(6, 1)),
                Err(WriteTxnError::Write(WriteError::PreparedHistory(
                    fgdb_delta_types::IndexError::CursorRetired { .. }
                )))
            ));
            assert!(std::sync::Arc::ptr_eq(&db.snapshot, &original));
            assert_eq!(db.frontier().unwrap(), live);
            assert_eq!(txn.prepared.as_ref().unwrap().template, prefix);
            assert_eq!(txn.staged.len(), 1);
            assert!(txn.read_set.borrow().is_empty());
            // Validation only needs the suffix after the already prepared
            // basis, which is still retained; reconstruction needs more.
            txn.commit(&mut db, &cx).await.unwrap();
            assert!(db.vertex(VId(5)).unwrap().is_some());
            assert!(db.vertex(VId(6)).unwrap().is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
