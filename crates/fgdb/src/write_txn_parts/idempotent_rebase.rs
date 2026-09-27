// Explicit semantic replay of idempotent existence instructions. Unlike the
// exact-effect append/field policies, an ENSURE may change its no-op decision.
// The native ordered evaluator alone derives the new effects and dependencies.

impl WriteTxn {
    /// Finalize an ENSURE program at the current healthy writer frontier.
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
    /// identity is conservatively ineligible even when its net effect vanished:
    /// staged metadata or values may already have escaped. Unrelated unchanged
    /// reads are allowed. Savepoints and active mixed-program scopes refuse.
    /// Non-ENSURE instructions and unknown/schema/constraint history families
    /// refuse with MixedRebaseIneligible; existing finalization policies retain
    /// their own, unchanged eligibility and exact-effect contracts.
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
        let mut proposals = std::collections::BTreeSet::new();
        for batch in &self.staged {
            checkpoint()?;
            for row in &batch.rows {
                checkpoint()?;
                let proposal = match row {
                    PendingRow::Vertex { vid, ensure: true, .. } => ElementId::Vertex(*vid),
                    PendingRow::Edge { eid, ensure: true, .. } => ElementId::Edge(*eid),
                    _ => return Err(WriteTxnError::MixedRebaseIneligible),
                };
                proposals.insert(proposal);
            }
        }
        if frontier != self.basis {
            self.validate_unobserved_creations(&proposals, checkpoint)
                .map_err(mixed_rebase_error)?;
        }
        // Even without reads, demand a complete retained suffix. An empty
        // append footprint supplies the existing closed history-family law;
        // it imposes no artificial conflict on idempotent existence decisions.
        let history = AppendRebaseFootprint::default();
        for batch in database.delta_since(self.basis)? {
            checkpoint()?;
            for coordinate in batch.coordinate_entries() {
                checkpoint()?;
                for row in &coordinate.rows {
                    if history.conflicts(row, checkpoint).map_err(mixed_rebase_error)? {
                        return Err(WriteTxnError::MixedRebaseIneligible);
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
}
