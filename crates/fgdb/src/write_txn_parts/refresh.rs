// Explicit snapshot advancement is validation, not a second intent evaluator.
// A successful refresh carries the exact cached net effects and savepoints;
// it never silently repeats a query or accepts a changed before-image.

impl WriteTxn {
    /// Advance this workspace to the owner's current healthy snapshot after
    /// proving that its observations and prepared dependencies are unchanged.
    /// This lets a transaction continue staging after unrelated commits without
    /// discarding its read/modify/write work or publishing an intermediate write.
    ///
    /// The complete retained suffix is required, even for an empty workspace.
    /// Point/negative reads, scans, expansions, raw mutation footprints, and the
    /// before-images of every current/saved preparation must survive it. Schema
    /// or constraint changes refuse rather than silently changing bindings.
    /// Conflicting writes are NOT merged: this is the existing conservative FCW
    /// read contract, not SSI, intent replay, or the merge ladder's replay rung.
    ///
    /// Success returns the new basis. Canonical templates, birth ordinals,
    /// staged source order, savepoint names, observations and the existing pin
    /// are retained. The saved preparations advance with the workspace, so a
    /// later rollback cannot restore an obsolete basis. Old basis-bound query
    /// evidence no longer describes this workspace. Already-issued immutable
    /// database read views are unaffected.
    ///
    /// Wrong-owner, health, missing-history, conflict and cancellation refusals
    /// preserve the ENTIRE workspace, including its old basis and active pin.
    /// Scalable validation traversals checkpoint through TxnCx; the final
    /// acceptance is bounded by the existing 64-savepoint limit and contains no
    /// allocation, callback, await, durable I/O, or cancellation checkpoint.
    /// Ordinary reads and staging never invoke this operation implicitly.
    pub fn refresh_snapshot<V: Vfs + Clone>(
        &mut self,
        database: &Database<V>,
        cx: &TxnCx,
    ) -> Result<CommitSeq, WriteTxnError> {
        cx.with_restriction(|| {
            self.refresh_snapshot_controlled(database, &mut || {
                cx.checkpoint().map_err(WriteTxnError::Interrupted)
            })
        })
    }

    fn refresh_snapshot_controlled<V: Vfs + Clone>(
        &mut self,
        database: &Database<V>,
        checkpoint: &mut impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<CommitSeq, WriteTxnError> {
        self.ensure_database(database)?;
        let frontier = database.frontier()?;
        // A healthy frontier alone cannot attest a gapped validation interval.
        // Do not inherit transaction_conflict's empty-footprint fast return.
        let history = database.delta_since(self.basis)?;
        checkpoint()?;
        if frontier == self.basis {
            return Ok(frontier);
        }
        if let Some((law, element, committed_at)) =
            self.transaction_conflict(database, checkpoint)?
        {
            return Err(WriteError::FirstCommitterWins {
                law,
                detail: format!(
                    "snapshot refresh dependency {element:?} changed at {committed_at:?} after {:?}",
                    self.basis
                ),
            }
            .into());
        }

        // Net effects alone are insufficient: a normalized no-op, ensure alias
        // or older savepoint can retain a before-image absent from today's net.
        // Borrow each dependency rather than cloning large sets without checks.
        let mut prepared_reads = std::collections::BTreeSet::new();
        for prepared in self.prepared.iter().chain(
            self.savepoints
                .iter()
                .filter_map(|saved| saved.prepared.as_ref()),
        ) {
            checkpoint()?;
            if !std::sync::Arc::ptr_eq(&self.handle_owner, &prepared.handle_owner) {
                return Err(WriteTxnError::WrongDatabase);
            }
            if prepared.basis != self.basis {
                return Err(WriteTxnError::SnapshotAdvanced {
                    pinned: self.basis,
                    live: prepared.basis,
                });
            }
            for element in prepared.dependencies.observations() {
                checkpoint()?;
                prepared_reads.insert(element);
            }
        }
        for batch in history {
            checkpoint()?;
            let committed_at = batch.commit_seq();
            for coordinate in batch.coordinate_entries() {
                checkpoint()?;
                for row in &coordinate.rows {
                    checkpoint()?;
                    if matches!(
                        row,
                        fgdb_delta_types::DeltaRow::Schema { .. }
                            | fgdb_delta_types::DeltaRow::Constraint { .. }
                    ) {
                        return Err(WriteError::FirstCommitterWins {
                            law: "FG-LAW-FCW-READ-01",
                            detail: format!(
                                "snapshot refresh crosses schema or constraint changes at {committed_at:?}"
                            ),
                        }
                        .into());
                    }
                    let mut touched = std::collections::BTreeSet::new();
                    validation_touches(row, &mut touched, checkpoint)?;
                    crate::adjacency_endpoints(row, &mut touched);
                    for element in touched {
                        checkpoint()?;
                        if prepared_reads.contains(&element) {
                            return Err(WriteError::FirstCommitterWins {
                                law: "FG-LAW-FCW-01",
                                detail: format!(
                                    "snapshot refresh preparation {element:?} changed at {committed_at:?}"
                                ),
                            }
                            .into());
                        }
                    }
                }
            }
        }
        drop(prepared_reads);
        checkpoint()?;
        // Sole acceptance point. All fallible work is over and the immutable
        // database borrow prevents publication from moving the validated cut.
        // Templates/dependencies are byte-identical; only their proven cut moves.
        for saved in &mut self.savepoints {
            if let Some(prepared) = &mut saved.prepared {
                prepared.basis = frontier;
            }
        }
        if let Some(prepared) = &mut self.prepared {
            prepared.basis = frontier;
        }
        self.basis = frontier;
        Ok(frontier)
    }
}

#[cfg(test)]
mod snapshot_refresh_tests {
    use super::*;
    use crate::{DatabaseKeys, MemVfs, WriteMismatchPolicy};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    const P: PropertyKeyId = PropertyKeyId(1);

    fn keys() -> DatabaseKeys {
        DatabaseKeys::new([0x51; 32], DatabaseSecurityNamespaceId([0x52; 32]), [0x53; 32])
    }

    async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
        let mut database = Database::open_memory(cx, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in 1..=6 {
            seed.create_vertex(VId(id), vec![LabelId(1)], vec![(P, CanonicalScalar::Int(10))]);
        }
        seed.add_edge(EId(10), VId(1), VId(2), vec![(P, CanonicalScalar::Int(10))]);
        database.write(cx, seed).await.unwrap();
        database
    }

    fn set_vertex(id: u128, value: i64) -> WriteBatch {
        let mut batch = WriteBatch::new(RelationId(1));
        batch.set_vertex_property(VId(id), P, Some(CanonicalScalar::Int(value)));
        batch
    }

    #[test]
    fn read_modify_write_continues_after_unrelated_commits_without_auto_refresh() {
        let ((), report) = run_async_under_lab(0xfa57_0001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut database = seeded(&cx).await;
            let immutable = database.read_session().unwrap();
            let mut txn = database.begin(&txcx).unwrap();
            let original = txn.basis();
            assert_eq!(txn.vertex(&database, VId(1)).unwrap().unwrap().props[0].1,
                CanonicalScalar::Int(10));
            database.write(&cx, set_vertex(6, 60)).await.unwrap();
            let frontier = database.frontier().unwrap();
            assert!(matches!(txn.write(&mut database, set_vertex(1, 11)),
                Err(WriteTxnError::SnapshotAdvanced { .. })));
            assert_eq!(txn.basis(), original);
            assert_eq!(txn.refresh_snapshot(&database, &txcx).unwrap(), frontier);
            assert_eq!(txn.basis(), frontier);
            assert_eq!(database.frontier().unwrap(), frontier, "refresh never commits");
            assert_eq!(txcx.outstanding_obligations(), 1);
            assert_eq!(immutable.vertex(VId(6)).unwrap().unwrap().props[0].1,
                CanonicalScalar::Int(10));
            assert_eq!(txn.vertex(&database, VId(6)).unwrap().unwrap().props[0].1,
                CanonicalScalar::Int(60));
            txn.write(&mut database, set_vertex(1, 11)).unwrap();
            assert_eq!(txn.commit(&mut database, &cx).await.unwrap(), CommitSeq(frontier.0 + 1));
            let recovered = database.recover_authoritatively(&cx).await.unwrap();
            assert_eq!(recovered.vertex(VId(1)).unwrap().unwrap().props[0].1,
                CanonicalScalar::Int(11));
            assert_eq!(recovered.vertex(VId(6)).unwrap().unwrap().props[0].1,
                CanonicalScalar::Int(60));
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn refresh_carries_ordered_writes_and_savepoints_without_rewriting_effects() {
        let ((), report) = run_async_under_lab(0xfa57_0002, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut database = seeded(&cx).await;
            let mut txn = database.begin(&txcx).unwrap();
            let mut first = WriteBatch::new(RelationId(9));
            first.create_vertex(VId(20), vec![], vec![(P, CanonicalScalar::Int(1))]);
            first.add_edge(EId(20), VId(1), VId(20), vec![]);
            let mut second = WriteBatch::new(RelationId(2));
            second.set_vertex_property(VId(20), P, Some(CanonicalScalar::Int(2)));
            second.add_edge(EId(21), VId(20), VId(2), vec![]);
            txn.write_ordered(&mut database, vec![first, second]).unwrap();
            txn.savepoint(&database, "kept").unwrap();
            let saved = txn.prepared.as_ref().unwrap().template.clone();
            let created = txn.vertex(&database, VId(20)).unwrap().unwrap();
            txn.write(&mut database, set_vertex(3, 30)).unwrap();
            let full = txn.prepared.as_ref().unwrap().template.clone();
            let reads = txn.read_set.borrow().clone();
            database.write(&cx, set_vertex(6, 60)).await.unwrap();
            let frontier = database.frontier().unwrap();
            txn.refresh_snapshot(&database, &txcx).unwrap();
            assert_eq!(txn.prepared.as_ref().unwrap().template, full);
            assert_eq!(txn.prepared.as_ref().unwrap().basis(), frontier);
            assert_eq!(txn.savepoints[0].prepared.as_ref().unwrap().template, saved);
            assert_eq!(txn.savepoints[0].prepared.as_ref().unwrap().basis(), frontier);
            assert_eq!(*txn.read_set.borrow(), reads);
            assert_eq!(txn.vertex(&database, VId(20)).unwrap().unwrap().birth_ordinal,
                created.birth_ordinal);
            txn.rollback_to_savepoint(&database, "kept").unwrap();
            assert_eq!(txn.prepared.as_ref().unwrap().template, saved);
            assert_eq!(txn.basis(), frontier);
            assert_eq!(txn.vertex(&database, VId(3)).unwrap().unwrap().props[0].1,
                CanonicalScalar::Int(10));
            let mut suffix = WriteBatch::new(RelationId(4));
            suffix.add_edge(EId(22), VId(20), VId(3), vec![]);
            txn.write_ordered(&mut database, vec![suffix]).unwrap();
            txn.commit(&mut database, &cx).await.unwrap();
            assert_eq!(database.neighbours(VId(20), RelationId(2)).unwrap(), vec![VId(2)]);
            assert_eq!(database.neighbours(VId(20), RelationId(4)).unwrap(), vec![VId(3)]);
            assert_eq!(database.vertex(VId(3)).unwrap().unwrap().props[0].1,
                CanonicalScalar::Int(10));
            assert_eq!(database.vertex(VId(6)).unwrap().unwrap().props[0].1,
                CanonicalScalar::Int(60));
            assert_eq!(database.frontier().unwrap(), CommitSeq(frontier.0 + 1));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn point_negative_scan_adjacency_and_normalized_guards_cannot_be_refreshed_away() {
        let ((), report) = run_async_under_lab(0xfa57_0003, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            for case in 0..6 {
                let mut database = seeded(&cx).await;
                let mut txn = database.begin(&txcx).unwrap();
                let basis = txn.basis();
                let winner = match case {
                    0 => { txn.vertex(&database, VId(1)).unwrap(); set_vertex(1, 11) }
                    1 => {
                        assert!(txn.vertex(&database, VId(99)).unwrap().is_none());
                        let mut batch = WriteBatch::new(RelationId(1));
                        batch.create_vertex(VId(99), vec![], vec![]);
                        batch
                    }
                    2 => {
                        txn.vertices(&database).unwrap();
                        let mut batch = WriteBatch::new(RelationId(1));
                        batch.create_vertex(VId(99), vec![], vec![]);
                        batch
                    }
                    3 => {
                        assert!(txn.in_neighbours(&database, VId(3), RelationId(1)).unwrap().is_empty());
                        let mut batch = WriteBatch::new(RelationId(1));
                        batch.add_edge(EId(99), VId(4), VId(3), vec![]);
                        batch
                    }
                    4 => {
                        let mut batch = WriteBatch::new(RelationId(1));
                        batch.compare_and_set_vertex_property(VId(1), P,
                            Some(CanonicalScalar::Int(999)), CanonicalScalar::Int(11),
                            WriteMismatchPolicy::NoOp);
                        txn.write(&mut database, batch).unwrap();
                        set_vertex(1, 12)
                    }
                    _ => {
                        let mut batch = WriteBatch::new(RelationId(1));
                        batch.ensure_edge_by_triple(EId(99), VId(1), VId(2), vec![]);
                        txn.write(&mut database, batch).unwrap();
                        let mut winner = WriteBatch::new(RelationId(1));
                        winner.delete_edge(EId(10));
                        winner
                    }
                };
                let template = txn.prepared.as_ref().map(|p| p.template.clone());
                let reads = txn.read_set.borrow().clone();
                database.write(&cx, winner).await.unwrap();
                let frontier = database.frontier().unwrap();
                assert!(matches!(txn.refresh_snapshot(&database, &txcx),
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))), "case {case}");
                assert_eq!(txn.basis(), basis);
                assert_eq!(txn.prepared.as_ref().map(|p| p.template.clone()), template);
                assert_eq!(*txn.read_set.borrow(), reads);
                assert_eq!(txn.state(), EmbeddedTxnState::Active);
                assert_eq!(database.frontier().unwrap(), frontier);
                assert_eq!(txcx.outstanding_obligations(), 1);
                txn.abort();
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn refreshed_reads_still_conflict_with_later_writes_and_repeated_refreshes() {
        let ((), report) = run_async_under_lab(0xfa57_0004, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut database = seeded(&cx).await;
            let mut txn = database.begin(&txcx).unwrap();
            txn.vertex(&database, VId(1)).unwrap();
            for value in 11..14 {
                database.write(&cx, set_vertex(6, value)).await.unwrap();
                let frontier = txn.refresh_snapshot(&database, &txcx).unwrap();
                assert_eq!(frontier, database.frontier().unwrap());
                assert_eq!(txn.refresh_snapshot(&database, &txcx).unwrap(), frontier);
                assert_eq!(txcx.outstanding_obligations(), 1);
            }
            let basis = txn.basis();
            txn.write(&mut database, set_vertex(3, 30)).unwrap();
            database.write(&cx, set_vertex(1, 12)).await.unwrap();
            assert!(matches!(txn.refresh_snapshot(&database, &txcx),
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))));
            assert_eq!(txn.basis(), basis);
            assert!(matches!(txn.finish(&mut database, &cx).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))));
            assert_eq!(database.vertex(VId(3)).unwrap().unwrap().props[0].1,
                CanonicalScalar::Int(10));
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    async fn pending_refresh(cx: &CommitCx, txcx: &TxnCx) -> (Database<MemVfs>, WriteTxn) {
        let mut database = seeded(cx).await;
        let mut txn = database.begin(txcx).unwrap();
        txn.vertex(&database, VId(1)).unwrap();
        txn.write(&mut database, set_vertex(3, 30)).unwrap();
        txn.savepoint(&database, "prefix").unwrap();
        txn.write(&mut database, set_vertex(4, 40)).unwrap();
        database.write(cx, set_vertex(6, 60)).await.unwrap();
        (database, txn)
    }

    #[test]
    fn every_refresh_checkpoint_preserves_the_workspace_on_interruption() {
        let ((), report) = run_async_under_lab(0xfa57_0005, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let (database, mut txn) = pending_refresh(&cx, &txcx).await;
            let mut total = 0;
            txn.refresh_snapshot_controlled(&database, &mut || {
                total += 1;
                Ok(())
            }).unwrap();
            assert!(total > 10, "exercise reads, raw writes, history and saved dependencies");
            txn.abort();
            for stop in 1..=total {
                let (database, mut txn) = pending_refresh(&cx, &txcx).await;
                let basis = txn.basis();
                let frontier = database.frontier().unwrap();
                let template = txn.prepared.as_ref().unwrap().template.clone();
                let saved = txn.savepoints[0].prepared.as_ref().unwrap().template.clone();
                let reads = txn.read_set.borrow().clone();
                let mut seen = 0;
                let result = txn.refresh_snapshot_controlled(&database, &mut || {
                    seen += 1;
                    if seen == stop {
                        Err(WriteTxnError::Interrupted(Box::new(
                            asupersync::error::Error::cancelled(
                                &asupersync::types::CancelReason::user("refresh interruption"),
                            ),
                        )))
                    } else {
                        Ok(())
                    }
                });
                assert!(matches!(result, Err(WriteTxnError::Interrupted(_))), "stop={stop}");
                assert_eq!(seen, stop);
                assert_eq!(txn.basis(), basis);
                assert_eq!(txn.prepared.as_ref().unwrap().basis(), basis);
                assert_eq!(txn.prepared.as_ref().unwrap().template, template);
                assert_eq!(txn.savepoints.len(), 1);
                assert_eq!(txn.savepoints[0].name, "prefix");
                assert_eq!(txn.savepoints[0].staged_len, 1);
                assert_eq!(txn.savepoints[0].prepared.as_ref().unwrap().basis(), basis);
                assert_eq!(txn.savepoints[0].prepared.as_ref().unwrap().template, saved);
                assert_eq!(txn.staged.len(), 2);
                assert_eq!(*txn.read_set.borrow(), reads);
                assert_eq!(txn.state(), EmbeddedTxnState::Active);
                assert_eq!(txcx.outstanding_obligations(), 1);
                assert_eq!(database.frontier().unwrap(), frontier);
                assert_eq!(txn.refresh_snapshot(&database, &txcx).unwrap(), frontier);
                txn.abort();
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn missing_history_refuses_even_an_empty_workspace_before_validation_work() {
        let ((), report) = run_async_under_lab(0xfa57_0006, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            for staged in [false, true] {
                let mut database = seeded(&cx).await;
                let mut txn = database.begin(&txcx).unwrap();
                let basis = txn.basis();
                if staged {
                    txn.write(&mut database, set_vertex(3, 30)).unwrap();
                    txn.savepoint(&database, "kept").unwrap();
                }
                let template = txn.prepared.as_ref().map(|p| p.template.clone());
                database.write(&cx, set_vertex(6, 60)).await.unwrap();
                let frontier = database.frontier().unwrap();
                // Exercise the real window retirement primitive as a missing-
                // history injection. This does not authorize GC under a pin.
                let retired = std::sync::Arc::make_mut(&mut database.snapshot)
                    .delta_index.retire_prefix(frontier).unwrap();
                assert!(!retired.is_empty());
                let mut work = 0;
                let result = txn.refresh_snapshot_controlled(&database, &mut || {
                    work += 1;
                    Ok(())
                });
                assert!(matches!(result,
                    Err(WriteTxnError::Read(ReadError::DeltaCursorRetired {
                        asked, retained_after, frontier: observed,
                    })) if asked == basis && retained_after == frontier && observed == frontier));
                assert_eq!(work, 0);
                assert_eq!(txn.basis(), basis);
                assert_eq!(txn.prepared.as_ref().map(|p| p.template.clone()), template);
                assert!(txn.prepared.iter().all(|p| p.basis() == basis));
                assert!(txn.savepoints.iter().filter_map(|s| s.prepared.as_ref())
                    .all(|p| p.basis() == basis));
                assert_eq!(txn.state(), EmbeddedTxnState::Active);
                assert_eq!(txcx.outstanding_obligations(), 1);
                // The exact retained boundary is valid, not a blanket refusal
                // merely because some earlier history has been retired.
                let mut current = database.begin(&txcx).unwrap();
                assert_eq!(current.refresh_snapshot(&database, &txcx).unwrap(), frontier);
                current.abort();
                txn.abort();
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn actual_commit_failure_fences_precede_refresh_even_at_the_same_frontier() {
        use crate::{DatabaseState, DerivedPublicationStage};
        use fgdb_chronicle::commit::CrashPoint;

        let ((), report) = run_async_under_lab(0xfa57_0007, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            for staged in [false, true] {
                for (crash, failure) in [
                    (Some(CrashPoint::AfterMarkerBeforeD2), None),
                    (None, Some(DerivedPublicationStage::FoldCommittedTemplate)),
                ] {
                    let mut database = seeded(&cx).await;
                    let immutable = database.read_session().unwrap();
                    let mut txn = database.begin(&txcx).unwrap();
                    let basis = txn.basis();
                    if staged {
                        txn.write(&mut database, set_vertex(3, 30)).unwrap();
                        txn.savepoint(&database, "kept").unwrap();
                    }
                    let template = txn.prepared.as_ref().map(|p| p.template.clone());
                    let prepared = database.prepare_write(set_vertex(6, 60)).unwrap();
                    assert!(database.commit_template(
                        &cx, prepared.template, crash, failure, None,
                    ).await.is_err());
                    let state = database.state();
                    let mut work = 0;
                    let result = txn.refresh_snapshot_controlled(&database, &mut || {
                        work += 1;
                        Ok(())
                    });
                    match (state, result) {
                        (DatabaseState::CommitOutcomeUnknown { published_frontier },
                            Err(WriteTxnError::Read(ReadError::CommitOutcomeUnknown {
                                published_frontier: observed,
                            }))) => {
                            assert_eq!(published_frontier, basis);
                            assert_eq!(observed, basis);
                            assert!(failure.is_none());
                        }
                        (DatabaseState::NeedsAuthoritativeRecovery(expected),
                            Err(WriteTxnError::Read(ReadError::RecoveryRequired(observed)))) => {
                            assert_eq!(observed, expected);
                            assert_eq!(observed.published_frontier, basis);
                            assert_eq!(observed.durable_frontier, CommitSeq(basis.0 + 1));
                            assert!(failure.is_some());
                        }
                        other => panic!("wrong refresh fence: {other:?}"),
                    }
                    assert_eq!(work, 0, "health precedes same-basis success and validation");
                    assert_eq!(txn.basis(), basis);
                    assert_eq!(txn.prepared.as_ref().map(|p| p.template.clone()), template);
                    assert!(txn.prepared.iter().all(|p| p.basis() == basis));
                    assert_eq!(txn.state(), EmbeddedTxnState::Active);
                    assert_eq!(txcx.outstanding_obligations(), 1);
                    assert_eq!(database.state(), state);
                    assert_eq!(immutable.vertex(VId(6)).unwrap().unwrap().props[0].1,
                        CanonicalScalar::Int(10));
                    txn.abort();
                    let recovered = database.recover_authoritatively(&cx).await.unwrap();
                    assert_eq!(recovered.vertex(VId(3)).unwrap().unwrap().props[0].1,
                        CanonicalScalar::Int(10), "refresh must not publish staged writes");
                    if failure.is_some() {
                        assert_eq!(recovered.vertex(VId(6)).unwrap().unwrap().props[0].1,
                            CanonicalScalar::Int(60));
                    }
                    assert_eq!(txcx.outstanding_obligations(), 0);
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn wrong_owner_terminal_state_and_unwinding_do_not_refresh_the_workspace() {
        let ((), report) = run_async_under_lab(0xfa57_0008, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let (mut database, mut txn) = pending_refresh(&cx, &txcx).await;
            let foreign = seeded(&cx).await;
            let basis = txn.basis();
            let template = txn.prepared.as_ref().unwrap().template.clone();
            let mut work = 0;
            assert!(matches!(txn.refresh_snapshot_controlled(&foreign, &mut || {
                work += 1;
                Ok(())
            }), Err(WriteTxnError::WrongDatabase)));
            assert_eq!(work, 0);
            assert_eq!(txn.basis(), basis);
            assert_eq!(txn.prepared.as_ref().unwrap().template, template);
            let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                txn.refresh_snapshot_controlled(&database, &mut || {
                    panic!("injected refresh unwind")
                })
            }));
            assert!(panicked.is_err());
            assert_eq!(txn.basis(), basis);
            assert_eq!(txn.prepared.as_ref().unwrap().template, template);
            assert_eq!(txn.savepoints[0].prepared.as_ref().unwrap().basis(), basis);
            assert_eq!(txn.state(), EmbeddedTxnState::Active);
            assert_eq!(txcx.outstanding_obligations(), 1);
            txn.refresh_snapshot(&database, &txcx).unwrap();
            txn.abort();
            let mut terminal = database.begin(&txcx).unwrap();
            terminal.finish(&mut database, &cx).await.unwrap();
            assert!(matches!(terminal.refresh_snapshot_controlled(&database, &mut || {
                work += 1;
                Ok(())
            }), Err(WriteTxnError::Finished)));
            assert_eq!(work, 0);
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn rolled_back_and_refused_statement_observations_survive_refresh_attempts() {
        let ((), report) = run_async_under_lab(0xfa57_0009, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            for rollback in [false, true] {
                let mut database = seeded(&cx).await;
                let mut txn = database.begin(&txcx).unwrap();
                let basis = txn.basis();
                if rollback {
                    txn.savepoint(&database, "empty").unwrap();
                    txn.write(&mut database, set_vertex(1, 11)).unwrap();
                    txn.rollback_to_savepoint(&database, "empty").unwrap();
                } else {
                    let mut refused = WriteBatch::new(RelationId(1));
                    refused.compare_and_set_vertex_property(VId(1), P,
                        Some(CanonicalScalar::Int(999)), CanonicalScalar::Int(11),
                        WriteMismatchPolicy::AbortWrite);
                    assert!(matches!(txn.write(&mut database, refused),
                        Err(WriteTxnError::Write(WriteError::CompareAndSetMismatch(_)))));
                }
                assert!(txn.staged.is_empty());
                assert!(txn.prepared.is_none());
                assert!(txn.read_set.borrow().contains(&ElementId::Vertex(VId(1))));
                database.write(&cx, set_vertex(1, 12)).await.unwrap();
                assert!(matches!(txn.refresh_snapshot(&database, &txcx),
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01", ..
                    }))));
                assert_eq!(txn.basis(), basis);
                assert!(txn.read_set.borrow().contains(&ElementId::Vertex(VId(1))));
                assert_eq!(txn.state(), EmbeddedTxnState::Active);
                txn.abort();
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
