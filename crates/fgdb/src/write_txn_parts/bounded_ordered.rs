impl WriteTxn {
    /// Stage an ordered suffix only when the complete program fits its limit.
    ///
    /// Counts include all retained batches and their replication across relation
    /// slices, not just the incoming suffix or its normalized effects. Borrowed
    /// admission precedes cloning the staged prefix and the ordinary staging
    /// path's ensure scans. The same limit can be applied to each successive
    /// call; it is per-call policy, not a sticky transaction mode.
    ///
    /// Refusal leaves staged effects, prepared births, savepoints and the pin
    /// unchanged. Conservative observations from a refused routing attempt are
    /// retained for conflict validation, including after savepoint rollback.
    /// An admitted call delegates to `write_ordered`, preserving its guards,
    /// prefix birth ordinals, read-your-writes and single-publication contract.
    ///
    /// Like `Database::prepare_ordered_writes_bounded`, this limits expanded
    /// evaluator-input rows, not bytes, storage work, cancellation or spill.
    pub fn write_ordered_bounded<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        batches: Vec<WriteBatch>,
        max_expanded_rows: u64,
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
        if let Err(error) = database
            .admit_ordered_write_rows(self.staged.iter().chain(batches.iter()), max_expanded_rows)
        {
            // Routing may have observed an existing edge's actual relation.
            // Do not let discarding the attempted suffix discard that read.
            // This intentionally retains conservative dependencies even when
            // the earlier raw-input admission already refused the request.
            let mut observations = self.read_set.borrow_mut();
            for batch in &batches {
                crate::prepared_write::PreparedDependencies::capture(&database.writer, batch)
                    .retain_observations(&mut observations);
            }
            return Err(error);
        }
        self.write_ordered(database, batches)
    }
}

#[cfg(test)]
mod bounded_ordered_tests {
    use super::*;
    use crate::{DatabaseKeys, MemVfs, WriteMismatchPolicy};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    const P: PropertyKeyId = PropertyKeyId(1);

    async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
        let keys = DatabaseKeys::new(
            [0xc1; 32],
            DatabaseSecurityNamespaceId([0xc2; 32]),
            [0xc3; 32],
        );
        let mut db = Database::open_memory(cx, keys).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in 1..=2 {
            seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(0))]);
        }
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(cx, seed).await.unwrap();
        db
    }

    fn prefix() -> WriteBatch {
        let mut batch = WriteBatch::new(RelationId(9));
        batch.create_vertex(VId(5), vec![], vec![]);
        batch.add_edge(EId(50), VId(1), VId(5), vec![]);
        batch
    }

    fn suffix() -> WriteBatch {
        let mut batch = WriteBatch::new(RelationId(2));
        batch.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(7)));
        batch.add_edge(EId(60), VId(5), VId(2), vec![]);
        batch
    }

    #[test]
    fn complete_prefix_is_admitted_and_refusal_preserves_savepoint_and_births() {
        let ((), report) = run_async_under_lab(0x6f62_1001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut db = seeded(&cx).await;
            let basis = db.frontier().unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            txn.write_ordered_bounded(&mut db, vec![prefix()], 2)
                .unwrap();
            txn.savepoint(&db, "prefix").unwrap();
            let retained = txn.prepared.as_ref().unwrap().template.clone();
            let birth = txn.vertex(&db, VId(5)).unwrap().unwrap().birth_ordinal;
            for (limit, required) in [(2, 4), (4, 6), (5, 6)] {
                assert!(matches!(
                    txn.write_ordered_bounded(&mut db, vec![suffix()], limit),
                    Err(WriteTxnError::OrderedWriteBudgetExceeded {
                        limit: actual_limit,
                        required: actual_required,
                    }) if actual_limit == limit && actual_required == required
                ));
                assert_eq!(txn.staged.len(), 1);
                assert_eq!(txn.prepared.as_ref().unwrap().template, retained);
                assert_eq!(txn.savepoints.len(), 1);
                assert!(txn.pin.is_some());
                assert!(txn.vertex(&db, VId(5)).unwrap().unwrap().props.is_empty());
                assert_eq!(db.frontier().unwrap(), basis);
                assert!(db.vertex(VId(5)).unwrap().is_none());
            }
            txn.write_ordered_bounded(&mut db, vec![suffix()], 6)
                .unwrap();
            assert_eq!(
                txn.vertex(&db, VId(5)).unwrap().unwrap().birth_ordinal,
                birth
            );
            txn.rollback_to_savepoint(&db, "prefix").unwrap();
            assert_eq!(txn.prepared.as_ref().unwrap().template, retained);
            assert_eq!(txn.staged.len(), 1);
            txn.write_ordered_bounded(&mut db, vec![suffix()], 6)
                .unwrap();
            let seq = txn.commit(&mut db, &cx).await.unwrap();
            assert_eq!(seq, CommitSeq(basis.0 + 1));
            assert_eq!(db.delta_since(basis).unwrap().count(), 1);
            assert_eq!(
                db.vertex(VId(5)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(7))]
            );
            assert_eq!(db.vertex(VId(5)).unwrap().unwrap().birth_ordinal, birth);
            assert!(db.edge(EId(60)).unwrap().is_some());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn refused_routing_observation_still_conflicts_after_savepoint_rollback() {
        let ((), report) = run_async_under_lab(0x6f62_1002, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut db = seeded(&cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            txn.write_ordered_bounded(&mut db, vec![prefix()], 2)
                .unwrap();
            txn.savepoint(&db, "prefix").unwrap();
            let mut attempted = WriteBatch::new(RelationId(2));
            attempted.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(9)));
            assert!(matches!(
                txn.write_ordered_bounded(&mut db, vec![attempted], 4),
                Err(WriteTxnError::OrderedWriteBudgetExceeded {
                    limit: 4,
                    required: 5,
                })
            ));
            assert!(txn.read_set.borrow().contains(&ElementId::Edge(EId(10))));
            txn.rollback_to_savepoint(&db, "prefix").unwrap();
            let mut concurrent = WriteBatch::new(RelationId(1));
            concurrent.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(6)));
            db.write(&cx, concurrent).await.unwrap();
            assert!(matches!(
                txn.commit(&mut db, &cx).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01",
                    ..
                }))
            ));
            assert!(db.vertex(VId(5)).unwrap().is_none());
            assert!(txn.pin.is_none());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn admitted_guard_failure_does_not_accept_the_suffix() {
        let ((), report) = run_async_under_lab(0x6f62_1003, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut db = seeded(&cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            txn.write_ordered_bounded(&mut db, vec![prefix()], 2)
                .unwrap();
            let retained = txn.prepared.as_ref().unwrap().template.clone();
            let mut bad = WriteBatch::new(RelationId(2));
            bad.compare_and_set_vertex_property(
                VId(1),
                P,
                Some(CanonicalScalar::Int(999)),
                CanonicalScalar::Int(8),
                WriteMismatchPolicy::AbortWrite,
            );
            // Two vertex instructions replicated twice, plus one edge row.
            assert!(matches!(
                txn.write_ordered_bounded(&mut db, vec![bad], 5),
                Err(WriteTxnError::Write(_))
            ));
            assert_eq!(txn.staged.len(), 1);
            assert_eq!(txn.prepared.as_ref().unwrap().template, retained);
            assert!(txn.read_set.borrow().contains(&ElementId::Vertex(VId(1))));
            txn.commit(&mut db, &cx).await.unwrap();
            assert!(db.vertex(VId(5)).unwrap().is_some());
            assert_eq!(
                db.vertex(VId(1)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(0))]
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn owner_and_terminal_state_precede_budget_and_input_admission() {
        let ((), report) = run_async_under_lab(0x6f62_1004, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut db = seeded(&cx).await;
            let mut foreign = seeded(&cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            assert!(matches!(
                txn.write_ordered_bounded(&mut foreign, vec![], 0),
                Err(WriteTxnError::WrongDatabase)
            ));
            assert!(txn.pin.is_some());
            assert!(txn.read_set.borrow().is_empty());
            assert!(matches!(
                txn.write_ordered_bounded(&mut db, vec![], 0),
                Err(WriteTxnError::Write(WriteError::EmptyBatch))
            ));
            txn.finish(&mut db, &cx).await.unwrap();
            assert!(matches!(
                txn.write_ordered_bounded(&mut db, vec![prefix()], 0),
                Err(WriteTxnError::Finished)
            ));
            assert!(txn.pin.is_none());
            assert!(txn.staged.is_empty());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
