// Explicit semantic replay of idempotent existence instructions. Unlike the
// exact-effect append/field policies, an ENSURE may change its no-op decision.
// The native ordered evaluator alone derives the new effects and dependencies.

#[derive(Default)]
struct IdempotentRebaseFootprint {
    proposals: std::collections::BTreeSet<ElementId>,
    protected: MixedRebaseFootprint,
}

impl IdempotentRebaseFootprint {
    fn record(&mut self, row: &PendingRow) -> Result<(), WriteTxnError> {
        let proposal = match row {
            PendingRow::Vertex { vid, ensure: true, .. } => ElementId::Vertex(*vid),
            PendingRow::Edge { eid, ensure: true, .. } => ElementId::Edge(*eid),
            // Reuse, do not weaken or duplicate, the existing raw-instruction
            // independence laws for every non-idempotent instruction.
            _ => return self.protected.record(row),
        };
        self.proposals.insert(proposal);
        Ok(())
    }
}

impl WriteTxn {
    /// Finalize an ordered program containing ENSURE at the healthy frontier.
    ///
    /// `ensure_vertex` retains an already-live vertex, including its winner's
    /// labels/properties. `ensure_edge_by_triple` retains a live relationship
    /// with the requested (source, relation, destination), even under another
    /// edge identity. Concurrent existence creation can therefore converge
    /// rather than losing ordinary first-committer-wins validation. A vanished
    /// matching triple may instead require creation. Neither decision is
    /// guessed here: the COMPLETE original ordered program is evaluated once
    /// by the existing mutation evaluator, including spent-ID, endpoint,
    /// scalar and storage admission. No application callback is retried.
    ///
    /// This policy intentionally does NOT promise the original staged-effect
    /// digest. A newly satisfied ENSURE can disappear from the final template;
    /// its proposed identity is not substituted into an existing relationship.
    /// Identities are never reassigned, refunded or resurrected. The final
    /// freshly prepared template alone goes through ordinary FCW and Chronicle.
    /// An all-no-op prepared write keeps the existing WriteCommitted contract.
    ///
    /// Every point, negative, scan and expansion observation validates over the
    /// ORIGINAL basis interval before replay. A changed observation refuses
    /// with FG-LAW-FCW-READ-01. If the basis advances, an observed proposed
    /// identity is conservatively ineligible even when its net effect vanished.
    /// This includes projected property/label/topology reads, not just complete
    /// rows: triple deduplication can remove a proposed edge whose fields were
    /// observed, without any concurrent write naming that proposed edge ID.
    /// Unrelated unchanged reads are allowed. Savepoints and active mixed-
    /// program scopes refuse.
    ///
    /// ENSURE may be interleaved with unconditional creations, property/label
    /// edits, CAS and edge/vertex retirement across relation coordinates. Each
    /// non-ENSURE instruction retains the existing mixed-rebase independence
    /// law: no identity collision, changed raw field/guard, lifetime change or
    /// cascade-incidence phantom. A write-and-restoration still conflicts. In
    /// particular, ENSURE followed by SET/DELETE cannot overwrite or retire a
    /// concurrently created winner merely by reevaluating its existence test.
    /// An ENSURE alone does not protect a triple from changes: rechecking that
    /// predicate is this explicit policy's purpose. Reading that neighborhood
    /// separately still records an ordinary, non-replayable observation.
    ///
    /// At least one ENSURE is required. Unknown instructions, schema/constraint
    /// rows, or coordinate schema/binding drift refuse with MixedRebaseIneligible.
    /// Other finalization policies retain their unchanged eligibility and exact-
    /// effect contracts. One native re-evaluation covers the ENTIRE program;
    /// there are no per-relation commits, conditional retries or partial receipts.
    ///
    /// `max_expanded_rows` bounds the entire relation-expanded replay input
    /// before payload cloning. It is not a byte-memory or history-work budget.
    /// Eligibility/history traversals checkpoint; native synchronous preparation
    /// is not internally preemptible. The owner-admitted, polled attempt is
    /// terminal on refusal, interruption, unwind or dropped commit future.
    /// Wrong-owner and unpolled calls preserve the workspace. There is only one
    /// publication, with the ordinary unknown/recovery outcome and no post-
    /// publication cancellation check. This raw embedded operation grants no
    /// token authority and does not claim full SSI or automatic query retry.
    pub async fn commit_idempotent_rebased<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
        max_expanded_rows: u64,
    ) -> Result<CommitSeq, WriteTxnError> {
        cx.with_restriction_async(self.commit_idempotent_rebased_controlled(
            database,
            cx,
            max_expanded_rows,
            None,
            || cx.checkpoint().map_err(WriteTxnError::Interrupted),
        ))
        .await
    }

    // The outer guard owns early replay failures; ordinary complete_controlled
    // owns validation/publication once entered. Both are the SAME guard type,
    // with idempotent workspace cleanup and a single commit invocation. On
    // cancellation Rust drops the awaited completion future before this guard,
    // so an inner unknown/committed terminal outcome cannot become an abort.
    async fn commit_idempotent_rebased_controlled<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
        max_expanded_rows: u64,
        crash_at: Option<fgdb_chronicle::commit::CrashPoint>,
        mut checkpoint: impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<CommitSeq, WriteTxnError> {
        self.ensure_database(database)?;
        let mut attempt = TxnCompletionGuard::new(self, database);
        attempt.database.frontier()?;
        checkpoint()?;
        if let Some((law, _, _)) = attempt.transaction.transaction_conflict_in(
            attempt.database,
            ConflictScope::Reads,
            &mut checkpoint,
        )? {
            return Err(WriteError::FirstCommitterWins {
                law,
                detail: "idempotent rebase crossed an original read observation".to_owned(),
            }
            .into());
        }
        attempt.transaction.prepare_idempotent_rebase(
            attempt.database,
            max_expanded_rows,
            &mut checkpoint,
        )?;
        attempt.entered_commit = true;
        let completion = attempt
            .transaction
            .complete_controlled(attempt.database, cx, crash_at, true, &mut checkpoint)
            .await?;
        match completion {
            EmbeddedTxnCompletion::WriteCommitted { commit_seq } => Ok(commit_seq),
            EmbeddedTxnCompletion::ReadClosed { .. } => {
                unreachable!("idempotent finalization requires a prepared write")
            }
        }
    }

    fn prepare_idempotent_rebase<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        max_expanded_rows: u64,
        checkpoint: &mut impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<(), WriteTxnError> {
        let frontier = database.frontier()?;
        let previous = self.prepared.as_ref().ok_or(WriteTxnError::NoPreparedWrite)?;
        if !std::sync::Arc::ptr_eq(&self.handle_owner, &previous.handle_owner) {
            return Err(WriteTxnError::WrongDatabase);
        }
        if previous.basis != self.basis {
            return Err(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live: previous.basis,
            });
        }
        if !self.savepoints.is_empty() || self.program_multi_relation {
            return Err(WriteTxnError::MixedRebaseIneligible);
        }
        database.admit_ordered_write_rows(self.staged.iter(), max_expanded_rows)?;
        let mut footprint = IdempotentRebaseFootprint::default();
        for batch in &self.staged {
            checkpoint()?;
            for row in &batch.rows {
                checkpoint()?;
                footprint.record(row)?;
            }
        }
        if footprint.proposals.is_empty() {
            return Err(WriteTxnError::MixedRebaseIneligible);
        }
        footprint.protected.protect_vertex_cascades(&previous.template, checkpoint)?;
        if frontier != self.basis {
            self.validate_unobserved_creations(&footprint.proposals, checkpoint)
                .map_err(mixed_rebase_error)?;
            // Exact-effect rebase need only protect escaped birth metadata.
            // ENSURE can instead remove an entire proposed creation, so its
            // projected observations cannot be validated by committed history
            // alone: a matching winner can have a DIFFERENT edge identity.
            let projected = self.point_reads.borrow();
            for element in &footprint.proposals {
                checkpoint()?;
                if projected.0.contains_key(element) {
                    return Err(WriteTxnError::MixedRebaseIneligible);
                }
            }
            self.validate_unobserved_creations(&footprint.protected.append.creations, checkpoint)
                .map_err(mixed_rebase_error)?;
        }
        // Even without reads, require the complete suffix. Metadata is part of
        // the boundary too: a row-free schema transition cannot evade the row
        // family's fail-closed check. Current ordinary templates have one
        // common graph/branch/schema binding across their relation coordinates.
        let binding = previous.template.coordinate_entries().first()
            .ok_or(WriteTxnError::MixedRebaseIneligible)?;
        for batch in database.delta_since(self.basis)? {
            checkpoint()?;
            for coordinate in batch.coordinate_entries() {
                checkpoint()?;
                if coordinate.graph != binding.graph
                    || coordinate.branch != binding.branch
                    || coordinate.schema_epoch != binding.schema_epoch
                    || coordinate.schema_transition != binding.schema_transition
                {
                    return Err(WriteTxnError::MixedRebaseIneligible);
                }
                for row in &coordinate.rows {
                    if footprint.protected.conflicts(row, checkpoint)? {
                        return Err(WriteError::FirstCommitterWins {
                            law: "FG-LAW-FCW-01",
                            detail: "idempotent rebase crossed a protected identity, field or lifetime"
                                .to_owned(),
                        }.into());
                    }
                }
            }
        }
        checkpoint()?;
        if frontier == self.basis {
            return Ok(());
        }
        let prepared =
            database.prepare_ordered_writes_bounded(self.staged.clone(), max_expanded_rows)?;
        checkpoint()?;
        debug_assert_eq!(prepared.basis, frontier);
        self.prepared = Some(prepared);
        self.basis = frontier;
        Ok(())
    }
}

#[cfg(test)]
mod idempotent_rebase_tests {
    use super::*;
    use crate::{DatabaseKeys, MemVfs};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    const R: RelationId = RelationId(1);
    const P: PropertyKeyId = PropertyKeyId(1);
    const NEW: VId = VId(u128::MAX - 1);
    const PROPOSAL: EId = EId(u128::MAX - 1);
    const WINNER: EId = EId(u128::MAX - 2);

    fn keys() -> DatabaseKeys {
        DatabaseKeys::new([0xa1; 32], DatabaseSecurityNamespaceId([0xa2; 32]), [0xa3; 32])
    }

    async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) {
        let mut batch = WriteBatch::new(R);
        for id in 1..=3 {
            batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(0))]);
        }
        db.write(cx, batch).await.unwrap();
    }

    fn requests() -> WriteBatch {
        let mut batch = WriteBatch::new(R);
        batch.ensure_vertex(NEW, vec![LabelId(7)], vec![(P, CanonicalScalar::Int(7))]);
        batch.ensure_edge_by_triple(PROPOSAL, VId(1), NEW, vec![(P, CanonicalScalar::Int(7))]);
        batch
    }

    fn winner() -> WriteBatch {
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(NEW, vec![LabelId(9)], vec![(P, CanonicalScalar::Int(9))]);
        batch.add_edge(WINNER, VId(1), NEW, vec![(P, CanonicalScalar::Int(9))]);
        batch
    }

    fn assert_read_conflict(result: Result<CommitSeq, WriteTxnError>) {
        assert!(matches!(result,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                law: "FG-LAW-FCW-READ-01", ..
            }))
        ));
    }

    fn stage_ensure(
        transaction: &mut WriteTxn,
        database: &mut Database<MemVfs>,
        entrypoint: usize,
        batch: WriteBatch,
    ) -> Result<(), WriteTxnError> {
        match entrypoint {
            0 => transaction.write(database, batch),
            1 => transaction.write_atomic(database, vec![batch]),
            2 => transaction.write_ordered(database, vec![batch]),
            3 => transaction.write_ordered_bounded(database, vec![batch], 64),
            _ => unreachable!("four native staging entrypoints"),
        }
    }

    #[test]
    fn ensure_decisions_replay_but_explicit_empty_scans_remain_observations() {
        let ((), report) = run_async_under_lab(0xa1de_0019, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            for entrypoint in 0..4 {
                for explicit_scan in [false, true] {
                    let mut db = Database::open_memory(&cx, keys()).await.unwrap();
                    seed(&mut db, &cx).await;
                    let mut replay = db.begin(&txcx).unwrap();
                    let mut ordinary = db.begin(&txcx).unwrap();
                    if explicit_scan {
                        assert!(replay.edges(&db).unwrap().is_empty());
                    }
                    stage_ensure(&mut replay, &mut db, entrypoint, requests()).unwrap();
                    stage_ensure(&mut ordinary, &mut db, entrypoint, requests()).unwrap();
                    let frontier = db.write(&cx, winner()).await.unwrap();
                    assert!(matches!(
                        ordinary.commit(&mut db, &cx).await,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
                    ));
                    let result = replay.commit_idempotent_rebased(&mut db, &cx, 2).await;
                    if explicit_scan {
                        assert_read_conflict(result);
                        assert_eq!(db.frontier().unwrap(), frontier);
                    } else {
                        assert_eq!(result.unwrap(), CommitSeq(frontier.0 + 1));
                    }
                    assert!(db.edge(PROPOSAL).unwrap().is_none());
                    assert_eq!(
                        db.edge(WINNER).unwrap().unwrap().props,
                        vec![(P, CanonicalScalar::Int(9))]
                    );
                    assert_eq!(db.vertex(NEW).unwrap().unwrap().labels, vec![LabelId(9)]);
                    assert_eq!(txcx.outstanding_obligations(), 0);
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn discarded_ensure_preparations_keep_alias_and_absent_triple_observations() {
        let ((), report) = run_async_under_lab(0xa1de_0020, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            for failed_preparation in [false, true] {
                for existing_alias in [false, true] {
                    let mut db = Database::open_memory(&cx, keys()).await.unwrap();
                    seed(&mut db, &cx).await;
                    if existing_alias {
                        db.write(&cx, winner()).await.unwrap();
                    }
                    let mut tx = db.begin(&txcx).unwrap();
                    if failed_preparation {
                        let mut rejected = requests();
                        rejected.compare_and_set_vertex_property(
                            VId(3),
                            P,
                            Some(CanonicalScalar::Int(99)),
                            CanonicalScalar::Int(1),
                            crate::WriteMismatchPolicy::AbortWrite,
                        );
                        assert!(matches!(
                            tx.write(&mut db, rejected),
                            Err(WriteTxnError::Write(WriteError::CompareAndSetMismatch(_)))
                        ));
                    } else {
                        tx.savepoint(&db, "before_ensure").unwrap();
                        tx.write_ordered(&mut db, vec![requests()]).unwrap();
                        tx.rollback_to_savepoint(&db, "before_ensure").unwrap();
                        tx.release_savepoint(&db, "before_ensure").unwrap();
                    }
                    assert!(tx.prepared.is_none());
                    assert!(tx.staged.is_empty());
                    // Retrying a valid ENSURE cannot erase the previous error
                    // or discarded preparation's externally relevant reads.
                    tx.write(&mut db, requests()).unwrap();
                    let concurrent = if existing_alias {
                        let mut batch = WriteBatch::new(R);
                        batch.set_edge_property(WINNER, P, Some(CanonicalScalar::Int(19)));
                        batch
                    } else {
                        winner()
                    };
                    let frontier = db.write(&cx, concurrent).await.unwrap();
                    let before = (db.vertices().unwrap(), db.edges().unwrap());
                    assert_read_conflict(tx.commit_idempotent_rebased(&mut db, &cx, 2).await);
                    assert_eq!(db.frontier().unwrap(), frontier);
                    assert_eq!((db.vertices().unwrap(), db.edges().unwrap()), before);
                    assert!(db.edge(PROPOSAL).unwrap().is_none());
                    assert_eq!(tx.state(), EmbeddedTxnState::Aborted);
                    assert_eq!(txcx.outstanding_obligations(), 0);
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn concurrent_ensure_converges_to_native_serial_winner_and_survives_reopen() {
        let ((), report) = run_async_under_lab(0xa1de_0001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys()).await.unwrap();
            seed(&mut db, &cx).await;
            let pinned = db.read_session().unwrap();
            let mut tx = db.begin(&txcx).unwrap();
            tx.write(&mut db, requests()).unwrap();
            let mut ordinary = db.begin(&txcx).unwrap();
            ordinary.write(&mut db, requests()).unwrap();
            let frontier = db.write(&cx, winner()).await.unwrap();
            assert!(matches!(ordinary.commit(&mut db, &cx).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))));
            let seq = tx.commit_idempotent_rebased(&mut db, &cx, 2).await.unwrap();
            assert_eq!(seq, CommitSeq(frontier.0 + 1));
            assert_eq!(db.delta_since(frontier).unwrap().count(), 1);
            assert_eq!(db.vertex(NEW).unwrap().unwrap().props, vec![(P, CanonicalScalar::Int(9))]);
            assert!(db.edge(PROPOSAL).unwrap().is_none());
            assert!(db.edge(WINNER).unwrap().is_some());
            assert!(pinned.vertex(NEW).unwrap().is_none());
            let mut serial = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut serial, &cx).await;
            serial.write(&cx, winner()).await.unwrap();
            let mut serial_tx = serial.begin(&txcx).unwrap();
            serial_tx.write(&mut serial, requests()).unwrap();
            serial_tx.commit(&mut serial, &cx).await.unwrap();
            assert_eq!(db.vertices().unwrap(), serial.vertices().unwrap());
            assert_eq!(db.edges().unwrap(), serial.edges().unwrap());
            let expected = (db.vertices().unwrap(), db.edges().unwrap());
            drop(db);
            let reopened = Database::open_with_vfs(&cx, vfs, &path, keys()).await.unwrap();
            assert_eq!(reopened.frontier().unwrap(), seq);
            assert_eq!((reopened.vertices().unwrap(), reopened.edges().unwrap()), expected);
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn vanished_triple_is_recreated_by_native_ensure_without_resurrecting_its_alias() {
        let ((), report) = run_async_under_lab(0xa1de_0002, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut db, &cx).await;
            db.write(&cx, winner()).await.unwrap();
            let mut tx = db.begin(&txcx).unwrap();
            tx.write(&mut db, requests()).unwrap();
            let mut deletion = WriteBatch::new(R);
            deletion.delete_edge(WINNER);
            let frontier = db.write(&cx, deletion).await.unwrap();
            let seq = tx.commit_idempotent_rebased(&mut db, &cx, 2).await.unwrap();
            assert_eq!(seq, CommitSeq(frontier.0 + 1));
            assert!(db.edge(WINNER).unwrap().is_none());
            let edge = db.edge(PROPOSAL).unwrap().unwrap();
            assert_eq!((edge.entry.src, edge.entry.dst), (VId(1), NEW));
            assert_eq!(edge.props, vec![(P, CanonicalScalar::Int(7))]);
            assert_eq!(db.vertex(NEW).unwrap().unwrap().props, vec![(P, CanonicalScalar::Int(9))]);
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn old_negative_and_positive_observations_are_never_erased_by_replay() {
        let ((), report) = run_async_under_lab(0xa1de_0003, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            for negative in [false, true] {
                let mut db = Database::open_memory(&cx, keys()).await.unwrap();
                seed(&mut db, &cx).await;
                let mut tx = db.begin(&txcx).unwrap();
                if negative {
                    assert!(tx.vertex(&db, NEW).unwrap().is_none());
                } else {
                    assert!(tx.vertex(&db, VId(3)).unwrap().is_some());
                }
                tx.write(&mut db, requests()).unwrap();
                let mut drift = winner();
                drift.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(1)));
                let frontier = db.write(&cx, drift).await.unwrap();
                assert_read_conflict(tx.commit_idempotent_rebased(&mut db, &cx, 3).await);
                assert_eq!(db.frontier().unwrap(), frontier);
                assert_eq!(tx.state(), EmbeddedTxnState::Aborted);
                assert!(tx.prepared.is_none());
                assert!(tx.staged.is_empty());
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn staged_birth_observations_refuse_even_when_concurrent_writes_are_unrelated() {
        let ((), report) = run_async_under_lab(0xa1de_0004, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut db, &cx).await;
            let mut tx = db.begin(&txcx).unwrap();
            tx.write(&mut db, requests()).unwrap();
            assert_eq!(tx.vertex(&db, NEW).unwrap().unwrap().created_at, tx.basis());
            let mut drift = WriteBatch::new(R);
            drift.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(1)));
            let frontier = db.write(&cx, drift).await.unwrap();
            assert!(matches!(tx.commit_idempotent_rebased(&mut db, &cx, 2).await,
                Err(WriteTxnError::MixedRebaseIneligible)));
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(db.vertex(NEW).unwrap().is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn spent_vertex_and_missing_endpoint_refuse_before_any_publication() {
        let ((), report) = run_async_under_lab(0xa1de_0005, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            for spent in [false, true] {
                let mut db = Database::open_memory(&cx, keys()).await.unwrap();
                seed(&mut db, &cx).await;
                let mut tx = db.begin(&txcx).unwrap();
                tx.write(&mut db, requests()).unwrap();
                if spent {
                    db.write(&cx, winner()).await.unwrap();
                }
                let mut drift = WriteBatch::new(R);
                drift.delete_vertex(if spent { NEW } else { VId(1) });
                let frontier = db.write(&cx, drift).await.unwrap();
                let before = (db.vertices().unwrap(), db.edges().unwrap());
                let error = tx.commit_idempotent_rebased(&mut db, &cx, 2).await.unwrap_err();
                if spent {
                    assert!(matches!(error, WriteTxnError::Write(WriteError::IdentitySpent { .. })));
                } else {
                    assert!(matches!(error, WriteTxnError::Write(WriteError::DanglingEndpoint { .. })));
                }
                assert_eq!(db.frontier().unwrap(), frontier);
                assert_eq!((db.vertices().unwrap(), db.edges().unwrap()), before);
                assert_eq!(tx.state(), EmbeddedTxnState::Aborted);
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn wrong_owner_and_unpolled_attempts_preserve_the_real_owners_workspace() {
        let ((), report) = run_async_under_lab(0xa1de_0006, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            let mut other = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut db, &cx).await;
            seed(&mut other, &cx).await;
            let mut tx = db.begin(&txcx).unwrap();
            tx.write(&mut db, requests()).unwrap();
            let before = tx.prepared.as_ref().unwrap().template.clone();
            let basis = tx.basis();
            drop(tx.commit_idempotent_rebased(&mut db, &cx, 2));
            assert!(matches!(tx.commit_idempotent_rebased(&mut other, &cx, 2).await,
                Err(WriteTxnError::WrongDatabase)));
            assert_eq!(tx.basis(), basis);
            assert_eq!(tx.prepared.as_ref().unwrap().template, before);
            assert_eq!(txcx.outstanding_obligations(), 1);
            tx.commit_idempotent_rebased(&mut db, &cx, 2).await.unwrap();
            assert!(db.vertex(NEW).unwrap().is_some());
            assert!(other.vertex(NEW).unwrap().is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn replay_row_limit_is_inclusive_and_refusal_does_not_publish_a_prefix() {
        let ((), report) = run_async_under_lab(0xa1de_0007, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            for limit in [0, 1, 2] {
                let mut db = Database::open_memory(&cx, keys()).await.unwrap();
                seed(&mut db, &cx).await;
                let mut tx = db.begin(&txcx).unwrap();
                tx.write(&mut db, requests()).unwrap();
                let mut drift = WriteBatch::new(R);
                drift.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(1)));
                let frontier = db.write(&cx, drift).await.unwrap();
                let result = tx.commit_idempotent_rebased(&mut db, &cx, limit).await;
                if limit == 2 {
                    assert_eq!(result.unwrap(), CommitSeq(frontier.0 + 1));
                } else {
                    assert!(matches!(result,
                        Err(WriteTxnError::OrderedWriteBudgetExceeded {
                            limit: actual, required: 2,
                        }) if actual == limit));
                    assert_eq!(db.frontier().unwrap(), frontier);
                    assert!(db.vertex(NEW).unwrap().is_none());
                    assert!(db.edge(PROPOSAL).unwrap().is_none());
                    assert_eq!(tx.state(), EmbeddedTxnState::Aborted);
                }
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    async fn mixed_seed(db: &mut Database<MemVfs>, cx: &CommitCx) {
        seed(db, cx).await;
        let mut batch = WriteBatch::new(R);
        batch.add_edge(EId(10), VId(2), VId(3), vec![(P, CanonicalScalar::Int(0))]);
        db.write(cx, batch).await.unwrap();
    }

    fn mixed_requests() -> Vec<WriteBatch> {
        let mut second = WriteBatch::new(RelationId(2));
        second.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(40)));
        second.create_vertex(VId(50), vec![], vec![(P, CanonicalScalar::Int(5))]);
        second.add_edge(EId(60), VId(2), NEW, vec![]);
        second.add_edge(EId(61), NEW, VId(50), vec![]);
        second.set_vertex_property(VId(50), P, Some(CanonicalScalar::Int(6)));
        let mut third = WriteBatch::new(R);
        third.delete_edge(EId(10));
        vec![requests(), second, third]
    }

    fn mixed_winner() -> WriteBatch {
        let mut batch = winner();
        batch.set_vertex_property(VId(1), PropertyKeyId(2), Some(CanonicalScalar::Int(99)));
        batch
    }

    #[test]
    fn mixed_ensure_creations_edits_and_retirement_across_relations_equal_serial_execution() {
        let ((), report) = run_async_under_lab(0xa1de_0010, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys()).await.unwrap();
            mixed_seed(&mut db, &cx).await;
            let mut tx = db.begin(&txcx).unwrap();
            tx.vertex(&db, VId(3)).unwrap(); // an unrelated observation remains valid
            tx.write_ordered(&mut db, mixed_requests()).unwrap();
            let frontier = db.write(&cx, mixed_winner()).await.unwrap();
            let seq = tx.commit_idempotent_rebased(&mut db, &cx, 64).await.unwrap();
            assert_eq!(seq, CommitSeq(frontier.0 + 1));
            assert_eq!(db.delta_since(frontier).unwrap().count(), 1);
            assert!(db.edge(PROPOSAL).unwrap().is_none());
            assert!(db.edge(WINNER).unwrap().is_some());
            assert!(db.edge(EId(10)).unwrap().is_none());
            assert_eq!(db.edge(EId(60)).unwrap().unwrap().entry.relation, RelationId(2));
            assert_eq!(db.vertex(VId(1)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(40)), (PropertyKeyId(2), CanonicalScalar::Int(99))]);
            let mut serial = Database::open_memory(&cx, keys()).await.unwrap();
            mixed_seed(&mut serial, &cx).await;
            serial.write(&cx, mixed_winner()).await.unwrap();
            let mut serial_tx = serial.begin(&txcx).unwrap();
            serial_tx.write_ordered(&mut serial, mixed_requests()).unwrap();
            serial_tx.commit(&mut serial, &cx).await.unwrap();
            assert_eq!(db.vertices().unwrap(), serial.vertices().unwrap());
            assert_eq!(db.edges().unwrap(), serial.edges().unwrap());
            let expected = (db.vertices().unwrap(), db.edges().unwrap());
            drop(db);
            let reopened = Database::open_with_vfs(&cx, vfs, &path, keys()).await.unwrap();
            assert_eq!(reopened.frontier().unwrap(), seq);
            assert_eq!((reopened.vertices().unwrap(), reopened.edges().unwrap()), expected);
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn mixed_raw_field_guards_retirement_and_identity_aba_are_still_conflicts() {
        let ((), report) = run_async_under_lab(0xa1de_0011, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            for case in 0..6 {
                let mut db = Database::open_memory(&cx, keys()).await.unwrap();
                mixed_seed(&mut db, &cx).await;
                let mut tx = db.begin(&txcx).unwrap();
                let mut pending = WriteBatch::new(R);
                pending.ensure_vertex(NEW, vec![], vec![]);
                match case {
                    0 => {
                        pending.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(7)));
                        pending.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(0)));
                    }
                    1 => { pending.compare_and_set_vertex_property(VId(1), P,
                        Some(CanonicalScalar::Int(99)), CanonicalScalar::Int(7),
                        crate::WriteMismatchPolicy::NoOp); }
                    2 => {
                        pending.set_vertex_label(VId(1), LabelId(99), true);
                        pending.set_vertex_label(VId(1), LabelId(99), false);
                    }
                    3 => { pending.delete_edge(EId(10)); }
                    4 => { pending.delete_vertex(VId(2)); }
                    _ => { pending.create_vertex(VId(50), vec![], vec![]); }
                }
                tx.write(&mut db, pending).unwrap();
                let mut first = winner();
                let mut second = WriteBatch::new(R);
                match case {
                    0 | 1 => {
                        first.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(1)));
                        second.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(0)));
                    }
                    2 => {
                        first.set_vertex_label(VId(1), LabelId(99), true);
                        second.set_vertex_label(VId(1), LabelId(99), false);
                    }
                    3 => { first.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(1))); }
                    4 => { first.add_edge(EId(77), VId(2), VId(3), vec![]); }
                    _ => {
                        first.create_vertex(VId(50), vec![], vec![]);
                        second.delete_vertex(VId(50));
                    }
                }
                db.write(&cx, first).await.unwrap();
                if !second.is_empty() {
                    db.write(&cx, second).await.unwrap();
                }
                let frontier = db.frontier().unwrap();
                let before = (db.vertices().unwrap(), db.edges().unwrap());
                assert!(matches!(tx.commit_idempotent_rebased(&mut db, &cx, 64).await,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-01", ..
                    }))), "case {case}");
                assert_eq!(db.frontier().unwrap(), frontier);
                assert_eq!((db.vertices().unwrap(), db.edges().unwrap()), before);
                assert_eq!(tx.state(), EmbeddedTxnState::Aborted);
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn ensure_then_edit_or_delete_never_overwrites_a_concurrent_winner() {
        let ((), report) = run_async_under_lab(0xa1de_0012, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            for delete in [false, true] {
                let mut db = Database::open_memory(&cx, keys()).await.unwrap();
                seed(&mut db, &cx).await;
                let mut tx = db.begin(&txcx).unwrap();
                let mut pending = requests();
                if delete { pending.delete_vertex(NEW); }
                else { pending.set_vertex_property(NEW, P, Some(CanonicalScalar::Int(42))); }
                tx.write(&mut db, pending).unwrap();
                let frontier = db.write(&cx, winner()).await.unwrap();
                let before = (db.vertices().unwrap(), db.edges().unwrap());
                assert!(matches!(tx.commit_idempotent_rebased(&mut db, &cx, 64).await,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-01", ..
                    }))));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert_eq!((db.vertices().unwrap(), db.edges().unwrap()), before);
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn normalized_away_ensure_births_do_not_erase_escaped_metadata() {
        let ((), report) = run_async_under_lab(0xa1de_0013, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut db, &cx).await;
            let mut tx = db.begin(&txcx).unwrap();
            tx.write(&mut db, requests()).unwrap();
            tx.vertex(&db, NEW).unwrap();
            let mut deletion = WriteBatch::new(R);
            deletion.delete_vertex(NEW);
            tx.write(&mut db, deletion).unwrap();
            let mut drift = WriteBatch::new(R);
            drift.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(1)));
            let frontier = db.write(&cx, drift).await.unwrap();
            assert!(matches!(tx.commit_idempotent_rebased(&mut db, &cx, 64).await,
                Err(WriteTxnError::MixedRebaseIneligible)));
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(db.vertex(NEW).unwrap().is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    async fn drifted(cx: &CommitCx, txcx: &TxnCx) -> (Database<MemVfs>, WriteTxn) {
        let mut db = Database::open_memory(cx, keys()).await.unwrap();
        seed(&mut db, cx).await;
        let mut tx = db.begin(txcx).unwrap();
        tx.write(&mut db, requests()).unwrap();
        db.write(cx, winner()).await.unwrap();
        (db, tx)
    }

    #[test]
    fn every_replay_and_finalization_control_refusal_leaves_no_commit_or_workspace() {
        let ((), report) = run_async_under_lab(0xa1de_0014, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let (mut db, mut tx) = drifted(&cx, &txcx).await;
            let mut count = 0;
            tx.commit_idempotent_rebased_controlled(&mut db, &cx, 2, None, || {
                count += 1;
                Ok(())
            }).await.unwrap();
            assert!(count > 10);
            for stop in 1..=count {
                let (mut db, mut tx) = drifted(&cx, &txcx).await;
                let frontier = db.frontier().unwrap();
                let before = (db.vertices().unwrap(), db.edges().unwrap());
                let mut seen = 0;
                let result = tx.commit_idempotent_rebased_controlled(&mut db, &cx, 2, None, || {
                    seen += 1;
                    if seen == stop { Err(WriteTxnError::NoPreparedWrite) } else { Ok(()) }
                }).await;
                assert!(matches!(result, Err(WriteTxnError::NoPreparedWrite)), "stop {stop}");
                assert_eq!(seen, stop);
                assert_eq!(db.frontier().unwrap(), frontier);
                assert_eq!(db.delta_since(frontier).unwrap().count(), 0);
                assert_eq!((db.vertices().unwrap(), db.edges().unwrap()), before);
                assert_eq!(tx.state(), EmbeddedTxnState::Aborted);
                assert!(tx.pin.is_none());
                assert!(tx.prepared.is_none());
                assert!(tx.staged.is_empty());
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn actual_commit_faults_keep_unknown_distinct_from_prepublication_abort() {
        use fgdb_chronicle::commit::CrashPoint;
        let ((), report) = run_async_under_lab(0xa1de_0015, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            for crash in [CrashPoint::BeforeCapsule, CrashPoint::AfterMarkerBeforeD2] {
                let (mut db, mut tx) = drifted(&cx, &txcx).await;
                let frontier = db.frontier().unwrap();
                let result = tx.commit_idempotent_rebased_controlled(
                    &mut db, &cx, 2, Some(crash), || Ok(()),
                ).await;
                if crash == CrashPoint::BeforeCapsule {
                    assert!(matches!(result, Err(WriteTxnError::Write(WriteError::Commit(_)))));
                    assert_eq!(tx.state(), EmbeddedTxnState::Aborted);
                } else {
                    assert!(matches!(result,
                        Err(WriteTxnError::Write(WriteError::CommitOutcomeUnknown { .. }))));
                    assert_eq!(tx.state(), EmbeddedTxnState::CommitOutcomeUnknown {
                        published_frontier: frontier,
                    });
                }
                assert!(tx.prepared.is_none());
                assert!(tx.pin.is_none());
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn final_prepublication_unwind_cleans_both_replay_and_completion_guards() {
        use std::future::Future;
        use std::task::Poll;
        let ((), report) = run_async_under_lab(0xa1de_0016, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let (mut db, mut tx) = drifted(&cx, &txcx).await;
            let mut count = 0;
            tx.commit_idempotent_rebased_controlled(&mut db, &cx, 2, None, || {
                count += 1;
                Ok(())
            }).await.unwrap();
            let (mut db, mut tx) = drifted(&cx, &txcx).await;
            let frontier = db.frontier().unwrap();
            let mut seen = 0;
            let mut future = Box::pin(tx.commit_idempotent_rebased_controlled(
                &mut db, &cx, 2, None, || {
                    seen += 1;
                    assert_ne!(seen, count, "injected final prepublication unwind");
                    Ok(())
                },
            ));
            let panicked = std::future::poll_fn(|task| {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    future.as_mut().poll(task)
                }));
                Poll::Ready(result.is_err())
            }).await;
            drop(future);
            assert!(panicked);
            assert_eq!(tx.state(), EmbeddedTxnState::Aborted);
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(tx.staged.is_empty());
            assert!(tx.prepared.is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn savepoints_active_program_scopes_and_nonensure_programs_remain_ineligible() {
        let ((), report) = run_async_under_lab(0xa1de_0017, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            for case in 0..3 {
                let mut db = Database::open_memory(&cx, keys()).await.unwrap();
                seed(&mut db, &cx).await;
                let mut tx = db.begin(&txcx).unwrap();
                let mut pending = requests();
                if case == 2 {
                    pending = WriteBatch::new(R);
                    pending.create_vertex(NEW, vec![], vec![]);
                }
                tx.write(&mut db, pending).unwrap();
                if case == 0 { tx.savepoint(&db, "keep").unwrap(); }
                if case == 1 { tx.program_multi_relation = true; }
                let frontier = db.frontier().unwrap();
                assert!(matches!(tx.commit_idempotent_rebased(&mut db, &cx, 64).await,
                    Err(WriteTxnError::MixedRebaseIneligible)));
                assert_eq!(tx.state(), EmbeddedTxnState::Aborted);
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertex(NEW).unwrap().is_none());
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn projected_ensure_values_cannot_escape_a_changed_existence_decision() {
        let ((), report) = run_async_under_lab(0xa1de_0018, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            for case in 0..4 {
                let mut db = Database::open_memory(&cx, keys()).await.unwrap();
                seed(&mut db, &cx).await;
                let mut tx = db.begin(&txcx).unwrap();
                let mut pending = WriteBatch::new(R);
                let mut drift = WriteBatch::new(R);
                if case < 2 {
                    pending.ensure_edge_by_triple(PROPOSAL, VId(1), VId(2),
                        vec![(P, CanonicalScalar::Int(7))]);
                    drift.add_edge(WINNER, VId(1), VId(2),
                        vec![(P, CanonicalScalar::Int(9))]);
                } else {
                    pending.ensure_vertex(NEW, vec![LabelId(7)],
                        vec![(P, CanonicalScalar::Int(7))]);
                    drift.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(1)));
                }
                tx.write(&mut db, pending).unwrap();
                match case {
                    0 => assert_eq!(tx.edge_property(&db, PROPOSAL, P).unwrap(),
                        Some(CanonicalScalar::Int(7))),
                    1 => assert_eq!(tx.edge_property(&db, PROPOSAL, PropertyKeyId(99)).unwrap(), None),
                    2 => assert_eq!(tx.vertex_property(&db, NEW, P).unwrap(),
                        Some(CanonicalScalar::Int(7))),
                    _ => assert_eq!(tx.vertex_has_label(&db, NEW, LabelId(7)).unwrap(), Some(true)),
                }
                assert!(tx.read_set.borrow().is_empty());
                assert!(!tx.point_reads.borrow().is_empty());
                let frontier = db.write(&cx, drift).await.unwrap();
                // A distinct matching edge never writes the proposed EId.
                // Prove this reaches the replay observation check rather than
                // accidentally succeeding because ordinary validation refused.
                assert!(tx.transaction_conflict_in(&db, ConflictScope::Reads,
                    &mut || Ok(())).unwrap().is_none());
                let before = (db.vertices().unwrap(), db.edges().unwrap());
                assert!(matches!(tx.commit_idempotent_rebased(&mut db, &cx, 1).await,
                    Err(WriteTxnError::MixedRebaseIneligible)));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert_eq!((db.vertices().unwrap(), db.edges().unwrap()), before);
                assert!(db.edge(PROPOSAL).unwrap().is_none());
                assert_eq!(tx.state(), EmbeddedTxnState::Aborted);
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
