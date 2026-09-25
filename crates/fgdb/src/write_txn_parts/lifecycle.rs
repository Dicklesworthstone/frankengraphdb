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
            savepoints: Vec::new(),
            program_multi_relation: false,
            read_set: std::cell::RefCell::new(std::collections::BTreeSet::new()),
            match_expansions: std::cell::RefCell::new(std::collections::BTreeSet::new()),
            scanned_vertex_labels: std::cell::RefCell::new(std::collections::BTreeSet::new()),
            scanned_vertices: std::cell::Cell::new(false),
            scanned_edges: std::cell::Cell::new(false),
            state: EmbeddedTxnState::Active,
            pin: Some(pin),
        })
    }

    /// The snapshot frontier retained for this transaction.
    #[must_use]
    pub const fn basis(&self) -> CommitSeq {
        self.basis
    }

    /// Inspect the retained local outcome, including after a completion future
    /// was dropped. This is not a durable outcome lookup or recovery authority.
    #[must_use]
    pub const fn state(&self) -> EmbeddedTxnState {
        self.state
    }

    /// Validate lifecycle and ownership before observing another handle or
    /// changing staged/conflict state. A sequence is meaningful only within
    /// the opened writer lifetime that supplied this transaction's basis.
    fn ensure_database<V: Vfs>(&self, database: &Database<V>) -> Result<(), WriteTxnError> {
        if self.pin.is_none() || self.state.is_terminal() {
            return Err(WriteTxnError::Finished);
        }
        if !std::sync::Arc::ptr_eq(&self.handle_owner, &database.handle_owner) {
            return Err(WriteTxnError::WrongDatabase);
        }
        Ok(())
    }

    /// Stage a batch against this transaction's pinned snapshot.
    /// Same-relation prefixes retain the original preparation path. Once an
    /// explicit composition call has staged multiple relations, subsequent
    /// writes preserve source order through `prepare_ordered_writes`.
    /// Use `write_ordered` to explicitly introduce the first dependent relation;
    /// `write_atomic` continues to check independent groups on every call.
    /// Mixed write programs may introduce another relation under their
    /// whole-program rollback guard.
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
            && (self
                .staged
                .iter()
                .any(|staged| staged.relation != first.relation)
                || (self.program_multi_relation && batch.relation != first.relation))
        {
            // Mixed programs promise that each step sees its predecessor.
            // Independent-group preparation would reject a later SET/CAS on
            // an earlier creation or silently prevent a dependent relation.
            // The program guard still owns all-or-nothing workspace acceptance.
            return self.write_ordered(database, vec![batch]);
        }
        if let Some(expected) = self.staged.first().map(|staged| staged.relation)
            && batch.relation != expected
        {
            return Err(WriteTxnError::RelationMismatch {
                expected,
                found: batch.relation,
            });
        }
        if batch
            .rows
            .iter()
            .any(|row| matches!(row, PendingRow::Edge { ensure: true, .. }))
        {
            // Keep actual ensure aliases and insertion witnesses even if
            // subsequent preparation fails or normalizes the ensure to no-op.
            drop(self.edges(database)?);
        }
        let previous_len = self.staged.len();
        self.staged.push(batch);
        let combined = Self::combined_batch(&self.staged)
            .expect("a batch was staged immediately before combination");
        let prepared = match database.prepare_write(combined) {
            Ok(prepared) => prepared,
            Err(source) => {
                self.retain_failed_preparation_observations(database, previous_len);
                let _ = self.staged.pop();
                return Err(WriteTxnError::Write(source));
            }
        };
        debug_assert_eq!(prepared.basis(), self.basis);
        self.prepared = Some(prepared);
        Ok(())
    }

    /// Atomically stage relation groups, including all prior staged batches.
    ///
    /// Independent groups share the pinned basis. A leading vertex-create or
    /// vertex-ensure prefix may additionally supply new endpoints to multiple
    /// relations, under `prepare_atomic_writes`' exact-prefix verification.
    /// The prefix can come from an earlier `write` call; it must precede every
    /// other intent in the complete staged input. Suffix groups remain
    /// independent and may not change the shared initialized vertex contents.
    /// This is not arbitrary ordered cross-relation statement execution.
    /// A refusal preserves the prior staged effects and prepared write; any
    /// observations already made still participate in conflict validation.
    /// No capsule or marker is published until the ordinary `commit` method.
    pub fn write_atomic<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        batches: Vec<WriteBatch>,
    ) -> Result<(), WriteTxnError> {
        self.stage_composed_writes(database, batches, false)
    }

    /// Stage source-ordered, dependent writes spanning edge relations.
    ///
    /// The complete staged prefix is prepared by `Database::prepare_ordered_writes`:
    /// later batches can use earlier endpoints, properties and identity-addressed
    /// edge mutations without publishing an intermediate commit. The existing
    /// overlay readers immediately see the newly prepared net effects.
    ///
    /// A refusal preserves the previous overlay and all observations; savepoints
    /// retain the exact prepared prefix without introducing another mode or pin.
    /// Ordinary `write` calls continue ordered composition while the retained
    /// prefix spans multiple relations. After rollback to a single relation,
    /// introduce a different relation explicitly with this method again.
    /// `write_atomic` remains the stricter independent-group API.
    ///
    /// Completion uses the existing FCW validator and single Chronicle publication.
    /// This inherits the ordered evaluator's bounded decomposition restrictions;
    /// it does not add cross-graph transactions, rebasing or full SSI.
    pub fn write_ordered<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        batches: Vec<WriteBatch>,
    ) -> Result<(), WriteTxnError> {
        self.stage_composed_writes(database, batches, true)
    }

    fn stage_composed_writes<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        batches: Vec<WriteBatch>,
        ordered: bool,
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
        if batches
            .iter()
            .flat_map(|batch| &batch.rows)
            .any(|row| matches!(row, PendingRow::Edge { ensure: true, .. }))
        {
            drop(self.edges(database)?);
        }
        let previous_len = self.staged.len();
        self.staged.extend(batches);
        let result = if ordered {
            // An ordered suffix must not renumber births the prefix already
            // exposed. Atomic groups stay canonical instead: relation order
            // assigns births on every call, so a new group may move them.
            database
                .prepare_ordered_writes(self.staged.clone())
                .and_then(|prepared| prepared.retain_birth_ordinals(self.prepared.as_ref()))
        } else {
            database.prepare_atomic_writes(self.staged.clone())
        };
        let prepared = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                self.retain_failed_preparation_observations(database, previous_len);
                self.staged.truncate(previous_len);
                return Err(error);
            }
        };
        debug_assert_eq!(prepared.basis(), self.basis);
        self.prepared = Some(prepared);
        Ok(())
    }

    fn retain_failed_preparation_observations<V: Vfs>(
        &self,
        database: &Database<V>,
        previous_len: usize,
    ) {
        // Preparation is side-effect-free on the live writer, so it still
        // supplies the exact basis that the refused attempt observed. Keep
        // the same conservative capture used by successful prepared writes;
        // returned errors (notably CAS.actual) can expose those observations.
        let mut observations = self.read_set.borrow_mut();
        for batch in &self.staged[previous_len..] {
            crate::prepared_write::PreparedDependencies::capture(&database.writer, batch)
                .retain_observations(&mut observations);
        }
    }
}

#[cfg(test)]
mod ordered_staging_tests {
    use super::*;
    use crate::{DatabaseKeys, MemVfs, WriteMismatchPolicy};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    const P: PropertyKeyId = PropertyKeyId(1);

    fn keys() -> DatabaseKeys {
        DatabaseKeys::new(
            [0xd4; 32],
            DatabaseSecurityNamespaceId([0xd5; 32]),
            [0xd6; 32],
        )
    }

    async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
        let mut db = Database::open_memory(cx, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in 1..=2 {
            seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(0))]);
        }
        db.write(cx, seed).await.unwrap();
        db
    }

    fn dependent_program() -> [WriteBatch; 3] {
        let mut first = WriteBatch::new(RelationId(9));
        first.create_vertex(VId(5), vec![], vec![(P, CanonicalScalar::Int(1))]);
        first.add_edge(EId(50), VId(1), VId(5), vec![(P, CanonicalScalar::Int(10))]);
        let mut second = WriteBatch::new(RelationId(2));
        second.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(2)));
        second.create_vertex(VId(6), vec![], vec![]);
        second.add_edge(EId(60), VId(5), VId(6), vec![]);
        let mut last = WriteBatch::new(RelationId(1));
        last.compare_and_set_vertex_property(
            VId(5),
            P,
            Some(CanonicalScalar::Int(2)),
            CanonicalScalar::Int(3),
            WriteMismatchPolicy::AbortWrite,
        );
        last.compare_and_set_edge_property(
            EId(50),
            P,
            Some(CanonicalScalar::Int(10)),
            CanonicalScalar::Int(11),
            WriteMismatchPolicy::AbortWrite,
        );
        last.add_edge(EId(70), VId(6), VId(2), vec![]);
        [first, second, last]
    }

    #[test]
    fn dependent_relations_stage_read_their_writes_and_publish_once() {
        let ((), report) = run_async_under_lab(0x6f74_0001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut db = seeded(&cx).await;
            let basis = db.frontier().unwrap();
            let pinned = db.read_session().unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            let [first, second, last] = dependent_program();
            txn.write(&mut db, first).unwrap();
            let prefix = txn.prepared.as_ref().unwrap().template.clone();
            assert!(txn.write_atomic(&mut db, vec![second.clone()]).is_err());
            assert_eq!(txn.prepared.as_ref().unwrap().template, prefix);
            txn.write_ordered(&mut db, vec![second]).unwrap();
            // A normal continuation must not switch back to independent groups.
            txn.write(&mut db, last).unwrap();
            assert_eq!(
                txn.vertex(&db, VId(5)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(3))]
            );
            assert!(txn.vertex(&db, VId(6)).unwrap().is_some());
            assert!(db.vertex(VId(5)).unwrap().is_none());
            assert_eq!(db.frontier().unwrap(), basis);
            let seq = txn.commit(&mut db, &cx).await.unwrap();
            assert_eq!(seq, CommitSeq(basis.0 + 1));
            assert_eq!(db.delta_since(basis).unwrap().count(), 1);
            assert_eq!(
                db.vertex(VId(5)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(3))]
            );
            assert_eq!(db.vertex(VId(6)).unwrap().unwrap().birth_ordinal, 4);
            assert_eq!(
                db.edge(EId(50)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(11))]
            );
            for (eid, relation) in [(50, 9), (60, 2), (70, 1)] {
                let edge = db.edge(EId(eid)).unwrap().unwrap();
                assert_eq!(edge.entry.relation, RelationId(relation));
                assert_eq!(edge.entry.created_at, seq);
            }
            assert!(pinned.vertex(VId(5)).unwrap().is_none());
            assert!(db.vertex_at(VId(5), basis).unwrap().is_none());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn ordered_refusal_and_savepoint_rollback_preserve_the_exact_prefix() {
        let ((), report) = run_async_under_lab(0x6f74_0002, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut db = seeded(&cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            let [first, second, last] = dependent_program();
            txn.write_ordered(&mut db, vec![first, second]).unwrap();
            txn.savepoint(&db, "prefix").unwrap();
            let prefix = txn.prepared.as_ref().unwrap().template.clone();
            let mut bad = WriteBatch::new(RelationId(3));
            bad.compare_and_set_vertex_property(
                VId(5),
                P,
                Some(CanonicalScalar::Int(999)),
                CanonicalScalar::Int(4),
                WriteMismatchPolicy::AbortWrite,
            );
            assert!(txn.write_ordered(&mut db, vec![bad]).is_err());
            assert_eq!(txn.staged.len(), 2);
            assert_eq!(txn.prepared.as_ref().unwrap().template, prefix);
            txn.write(&mut db, last.clone()).unwrap();
            txn.rollback_to_savepoint(&db, "prefix").unwrap();
            assert_eq!(txn.staged.len(), 2);
            assert_eq!(txn.prepared.as_ref().unwrap().template, prefix);
            assert_eq!(
                txn.vertex(&db, VId(5)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(2))]
            );
            txn.write(&mut db, last).unwrap();
            txn.commit(&mut db, &cx).await.unwrap();
            assert_eq!(
                db.vertex(VId(5)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(3))]
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn rolled_back_ordered_noop_keeps_its_conflict_observation() {
        let ((), report) = run_async_under_lab(0x6f74_0003, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut db = seeded(&cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            let mut first = WriteBatch::new(RelationId(9));
            first.create_vertex(VId(5), vec![], vec![]);
            let mut second = WriteBatch::new(RelationId(2));
            second.create_vertex(VId(6), vec![], vec![]);
            txn.write_ordered(&mut db, vec![first, second]).unwrap();
            txn.savepoint(&db, "before_guard").unwrap();
            let mut noop = WriteBatch::new(RelationId(3));
            noop.compare_and_set_vertex_property(
                VId(1),
                P,
                Some(CanonicalScalar::Int(0)),
                CanonicalScalar::Int(0),
                WriteMismatchPolicy::AbortWrite,
            );
            txn.write_ordered(&mut db, vec![noop]).unwrap();
            txn.rollback_to_savepoint(&db, "before_guard").unwrap();
            let mut winner = WriteBatch::new(RelationId(1));
            winner.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(1)));
            db.write(&cx, winner).await.unwrap();
            let frontier = db.frontier().unwrap();
            assert!(matches!(
                txn.commit(&mut db, &cx).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
            ));
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(db.vertex(VId(5)).unwrap().is_none());
            assert!(db.vertex(VId(6)).unwrap().is_none());
            assert!(txn.pin.is_none());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn ordered_staging_refuses_empty_foreign_and_advanced_inputs_without_losing_work() {
        let ((), report) = run_async_under_lab(0x6f74_0004, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut db = seeded(&cx).await;
            let mut other = seeded(&cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            let [first, second, _] = dependent_program();
            txn.write(&mut db, first).unwrap();
            let prefix = txn.prepared.as_ref().unwrap().template.clone();
            assert!(matches!(
                txn.write_ordered(&mut other, vec![second.clone()]),
                Err(WriteTxnError::WrongDatabase)
            ));
            assert!(matches!(
                txn.write_ordered(&mut db, vec![]),
                Err(WriteTxnError::Write(WriteError::EmptyBatch))
            ));
            assert!(matches!(
                txn.write_ordered(&mut db, vec![WriteBatch::new(RelationId(2))]),
                Err(WriteTxnError::Write(WriteError::EmptyBatch))
            ));
            let mut winner = WriteBatch::new(RelationId(1));
            winner.create_vertex(VId(999), vec![], vec![]);
            db.write(&cx, winner).await.unwrap();
            assert!(matches!(
                txn.write_ordered(&mut db, vec![second]),
                Err(WriteTxnError::SnapshotAdvanced { .. })
            ));
            assert_eq!(txn.staged.len(), 1);
            assert_eq!(txn.prepared.as_ref().unwrap().template, prefix);
            txn.commit(&mut db, &cx).await.unwrap();
            assert!(db.vertex(VId(5)).unwrap().is_some());
            assert!(db.vertex(VId(6)).unwrap().is_none());
            assert!(matches!(
                txn.write_ordered(&mut db, vec![]),
                Err(WriteTxnError::Finished)
            ));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn mixed_workspace_admits_dependent_relations_and_does_not_leak_permission() {
        let ((), report) = run_async_under_lab(0x6f74_0005, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut db = seeded(&cx).await;
            let basis = db.frontier().unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            let [first, second, last] = dependent_program();
            txn.write(&mut db, first).unwrap();
            {
                // This is the same acceptance guard and ordinary write seam
                // used by both public mixed-program execution adapters.
                let workspace = MutationProgramWorkspace::new(&mut txn);
                workspace.txn.program_multi_relation = true;
                workspace.txn.write(&mut db, second).unwrap();
                workspace.txn.write(&mut db, last).unwrap();
                workspace.accept();
            }
            assert!(!txn.program_multi_relation);
            assert_eq!(
                txn.vertex(&db, VId(5)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(3))]
            );
            assert_eq!(db.frontier().unwrap(), basis);
            let seq = txn.commit(&mut db, &cx).await.unwrap();
            assert_eq!(seq, CommitSeq(basis.0 + 1));
            assert_eq!(
                db.edge(EId(50)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(11))]
            );
            assert_eq!(db.delta_since(basis).unwrap().count(), 1);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn mixed_ordered_errors_and_unwinds_restore_the_original_workspace() {
        let ((), report) = run_async_under_lab(0x6f74_0006, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            for unwind in [false, true] {
                let mut db = seeded(&cx).await;
                let mut txn = db.begin(&txcx).unwrap();
                let [first, second, _] = dependent_program();
                txn.write(&mut db, first).unwrap();
                let prefix = txn.prepared.as_ref().unwrap().template.clone();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let workspace = MutationProgramWorkspace::new(&mut txn);
                    workspace.txn.program_multi_relation = true;
                    workspace.txn.write(&mut db, second.clone())?;
                    if unwind {
                        panic!("injected unwind after dependent ordered staging");
                    }
                    let mut bad = WriteBatch::new(RelationId(3));
                    bad.compare_and_set_vertex_property(
                        VId(5),
                        P,
                        Some(CanonicalScalar::Int(999)),
                        CanonicalScalar::Int(4),
                        WriteMismatchPolicy::AbortWrite,
                    );
                    workspace.txn.write(&mut db, bad)?;
                    workspace.accept();
                    Ok::<(), WriteTxnError>(())
                }));
                if unwind {
                    assert!(result.is_err());
                } else {
                    assert!(result.unwrap().is_err());
                }
                assert!(!txn.program_multi_relation);
                assert_eq!(txn.staged.len(), 1);
                assert_eq!(txn.prepared.as_ref().unwrap().template, prefix);
                assert!(txn.vertex(&db, VId(6)).unwrap().is_none());
                assert_eq!(
                    txn.vertex(&db, VId(5)).unwrap().unwrap().props,
                    vec![(P, CanonicalScalar::Int(1))]
                );
                // No remembered execution mode may outlive a rolled-back program.
                assert!(matches!(
                    txn.write(&mut db, second),
                    Err(WriteTxnError::RelationMismatch { .. })
                ));
                txn.commit(&mut db, &cx).await.unwrap();
                assert!(db.vertex(VId(6)).unwrap().is_none());
                assert_eq!(
                    db.edge(EId(50)).unwrap().unwrap().props,
                    vec![(P, CanonicalScalar::Int(10))]
                );
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn nested_mixed_workspace_rollback_retains_the_outer_scope_and_prefix() {
        let ((), report) = run_async_under_lab(0x6f74_0007, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut db = seeded(&cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            let [first, second, last] = dependent_program();
            {
                let outer = MutationProgramWorkspace::new(&mut txn);
                outer.txn.program_multi_relation = true;
                outer.txn.write(&mut db, first).unwrap();
                let prefix = outer.txn.prepared.as_ref().unwrap().template.clone();
                {
                    let inner = MutationProgramWorkspace::new(outer.txn);
                    inner.txn.write(&mut db, second.clone()).unwrap();
                    // Dropping an unaccepted inner operation restores only it.
                }
                assert!(outer.txn.program_multi_relation);
                assert_eq!(outer.txn.prepared.as_ref().unwrap().template, prefix);
                outer.txn.write(&mut db, second).unwrap();
                outer.txn.write(&mut db, last).unwrap();
                outer.accept();
            }
            assert!(!txn.program_multi_relation);
            txn.commit(&mut db, &cx).await.unwrap();
            assert_eq!(
                db.vertex(VId(5)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(3))]
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    /// Ported from ebabc3ae's `switching_from_independent_groups_never_renumbers_observed_births`
    /// (lost in d8ce9e90, fgdb-write-ordered-silent-revert-2d80i). Ordinals are now
    /// database-global, so the observed values are captured, not hard-coded.
    #[test]
    fn switching_from_independent_groups_never_renumbers_observed_births() {
        let ((), report) = run_async_under_lab(0x6f74_0008, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut db = seeded(&cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            let mut first = WriteBatch::new(RelationId(9));
            first.create_vertex(VId(5), vec![], vec![(P, CanonicalScalar::Int(1))]);
            let mut second = WriteBatch::new(RelationId(2));
            second.create_vertex(VId(6), vec![], vec![(P, CanonicalScalar::Int(2))]);
            txn.write_atomic(&mut db, vec![first, second]).unwrap();
            let birth = |txn: &WriteTxn, db: &Database<MemVfs>, id| {
                txn.vertex(db, VId(id)).unwrap().unwrap().birth_ordinal
            };
            let births = [birth(&txn, &db, 5), birth(&txn, &db, 6)];
            assert!(
                births[1] < births[0],
                "independent groups stage in relation order: {births:?}"
            );
            let saved = txn.staged_effect_digest().unwrap();
            txn.savepoint(&db, "independent").unwrap();
            let mut suffix = WriteBatch::new(RelationId(1));
            suffix.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(3)));
            suffix.add_edge(EId(50), VId(5), VId(6), vec![]);
            txn.write_ordered(&mut db, vec![suffix.clone()]).unwrap();
            assert_eq!([birth(&txn, &db, 5), birth(&txn, &db, 6)], births);
            txn.rollback_to_savepoint(&db, "independent").unwrap();
            assert_eq!(txn.staged_effect_digest().unwrap(), saved);
            assert_eq!([birth(&txn, &db, 5), birth(&txn, &db, 6)], births);
            txn.write_ordered(&mut db, vec![suffix]).unwrap();
            let mut later = WriteBatch::new(RelationId(9));
            later.create_vertex(VId(7), vec![], vec![]);
            txn.write(&mut db, later).unwrap();
            let seventh = birth(&txn, &db, 7);
            assert!(
                seventh > births[0].max(births[1]),
                "a fresh birth is never reused"
            );
            assert_eq!([birth(&txn, &db, 5), birth(&txn, &db, 6)], births);
            txn.commit(&mut db, &cx).await.unwrap();
            for (id, expected) in [(5, births[0]), (6, births[1]), (7, seventh)] {
                assert_eq!(db.vertex(VId(id)).unwrap().unwrap().birth_ordinal, expected);
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
