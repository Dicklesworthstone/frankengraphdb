impl WriteTxn {
    pub(crate) fn begin(
        basis: CommitSeq,
        basis_counts: crate::gql_exec::source::exact_counts::ExactCounts,
        txn: &TxnCx,
        obligation_id: ObligationId,
        handle_owner: std::sync::Arc<()>,
    ) -> Result<Self, ObligationAcquireError> {
        let pin = txn.pin_snapshot(obligation_id)?;
        Ok(Self {
            handle_owner,
            basis,
            basis_counts,
            staged: Vec::new(),
            observed_staged_vertex_batches: std::cell::Cell::new(0),
            prepared: None,
            prepared_source_ordered: true,
            chain_retirements: None,
            savepoints: Vec::new(),
            program_multi_relation: false,
            read_set: std::cell::RefCell::new(std::collections::BTreeSet::new()),
            point_reads: std::cell::RefCell::new(PointReads::default()),
            count_reads: std::cell::RefCell::new(CountReads::default()),
            match_expansions: std::cell::RefCell::new(std::collections::BTreeSet::new()),
            scanned_vertex_labels: std::cell::RefCell::new(std::collections::BTreeSet::new()),
            scanned_vertices: std::cell::Cell::new(false),
            scanned_edges: std::cell::Cell::new(false),
            scanned_edge_relations: std::cell::RefCell::new(std::collections::BTreeSet::new()),
            authorization_decision_digest: None,
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

    /// Retain a successfully executed, compiler-classified write even when
    /// its source produced no intents. Classification is independent of net
    /// effects: only a transaction containing exclusively reads may read-close.
    /// This private carrier uses the ordinary canonical empty delta, without
    /// inventing an element, coordinate or raw intent. Public empty WriteBatch
    /// admission remains unchanged, and all original read witnesses remain on
    /// this transaction for the ordinary completion validator.
    fn retain_write_classification<V: Vfs>(
        &mut self,
        database: &Database<V>,
    ) -> Result<(), WriteTxnError> {
        self.ensure_database(database)?;
        if self.prepared.is_some() {
            return Ok(());
        }
        debug_assert!(self.staged.is_empty());
        let template = crate::LogicalDeltaTemplate::build(
            crate::intent_semantics_oid(),
            [0_u8; 32],
            Vec::new(),
        )
        .map_err(WriteError::Canonical)?;
        self.prepared = Some(std::sync::Arc::new(crate::PreparedWrite {
            template,
            basis: self.basis,
            handle_owner: std::sync::Arc::clone(&self.handle_owner),
            dependencies: crate::prepared_write::PreparedDependencies::default(),
            replay_row_allowance: None,
            raw_input: None,
            raw_ordered: None,
            authorization_decision_digest: None,
        }));
        Ok(())
    }

    /// Stage a batch against this transaction's pinned snapshot.
    /// Ordered prefixes reuse already admitted effects where the native
    /// evaluator can reconstruct only the suffix's required state. Unsupported
    /// intent families retain full preparation. Once an
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
        let mixed_prefix = self
            .prepared
            .as_ref()
            .and_then(|prepared| prepared.raw_ordered.as_ref())
            .is_some_and(|summary| summary.batches == self.staged.len());
        if let Some(first) = self.staged.first()
            && (mixed_prefix
                || (!self
                    .prepared
                    .as_ref()
                    .and_then(|prepared| prepared.raw_input)
                    .is_some_and(|summary| summary.relation == first.relation)
                    && self
                        .staged
                        .iter()
                        .any(|staged| staged.relation != first.relation))
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
        // Preparation retains ENSURE aliases and insertion dependencies in
        // PreparedDependencies. They are mutation inputs, not escaped reads:
        // calling edges() here would manufacture a public full-graph scan and
        // prevent idempotent finalization from reevaluating existence. Failed
        // or rolled-back preparations separately retain their observations.
        let previous_len = self.staged.len();
        self.staged.push(batch);
        let incremental = match self.prepared.as_deref() {
            Some(previous) if self.prepared_source_ordered => database.try_extend_prepared_write(
                previous,
                &self.staged[..previous_len],
                self.staged.last().expect("a batch was just staged"),
            ),
            _ => Ok(None),
        };
        let prepared = match incremental.and_then(|incremental| {
            if let Some(prepared) = incremental {
                return Ok(prepared);
            }
            let combined = Self::combined_batch(&self.staged)
                .expect("a batch was staged immediately before combination");
            database
                .prepare_write(combined)
                .map_err(WriteTxnError::Write)
        }) {
            Ok(prepared) => prepared,
            Err(source) => {
                self.retain_failed_preparation_observations(database, previous_len);
                self.truncate_staged(previous_len);
                return Err(source);
            }
        };
        debug_assert_eq!(prepared.basis(), self.basis);
        self.prepared = Some(std::sync::Arc::new(prepared));
        self.prepared_source_ordered = true;
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
        // A mixed write program or chain promises that each step sees its
        // predecessor, as `write` does: a later step's vertices follow an
        // earlier step's edge group, outside any shared vertex prefix, so
        // independent groups would refuse its new edge's endpoint
        // (`CREATE (a)-[:T]->(b); CREATE (c)-[:T]->(d)`, TCK Create3 [8]).
        self.stage_composed_writes(database, batches, self.program_multi_relation)
    }

    /// Stage source-ordered, dependent writes spanning edge relations.
    ///
    /// Later batches use earlier endpoints, properties and identity-addressed
    /// edge mutations without publishing an intermediate commit. Supported
    /// prefixes reuse already admitted effects through the same native relation
    /// slices as `Database::prepare_ordered_writes`; other intent families retain
    /// full preparation. Overlay readers immediately see the new net effects.
    /// A prefix staged by `write_atomic` is first re-evaluated in source order,
    /// which can change a conditional outcome or refuse. Birth ordinals already
    /// exposed by that prefix are retained.
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
        // As in write(), successful ENSURE dependencies belong to the
        // prepared mutation. Public scans must only come from actual readers.
        let previous_len = self.staged.len();
        self.staged.extend(batches);
        let result = if ordered {
            let incremental = match self.prepared.as_deref() {
                Some(previous) if self.prepared_source_ordered => database
                    .try_extend_prepared_ordered_writes(
                        previous,
                        &self.staged[..previous_len],
                        &self.staged[previous_len..],
                    ),
                // An atomic prefix's normalized effects do not prove the
                // result of replaying its interleaved raw groups. Establish
                // that result once through the ordinary ordered evaluator.
                _ => Ok(None),
            };
            incremental.and_then(|prepared| match prepared {
                // Incremental preparation restores selected births from the
                // retained prefix and copies untouched births verbatim. Do
                // not collect every old birth and clone the complete new
                // template again merely to restore the same ordinals.
                Some(prepared) => Ok(prepared),
                // Full source-order preparation can renumber births exposed
                // by independent groups. Only this fallback needs the repair.
                None => database
                    .prepare_ordered_writes(self.staged.clone())
                    .and_then(|prepared| prepared.retain_birth_ordinals(self.prepared.as_deref())),
            })
        } else {
            database.prepare_atomic_writes(self.staged.clone())
        };
        let prepared = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                self.retain_failed_preparation_observations(database, previous_len);
                self.truncate_staged(previous_len);
                return Err(error);
            }
        };
        debug_assert_eq!(prepared.basis(), self.basis);
        self.prepared = Some(std::sync::Arc::new(prepared));
        // One atomic relation uses the ordinary source-ordered evaluator too.
        // Mixed atomic groups need a fresh proof even if their net is empty.
        self.prepared_source_ordered = ordered
            || self.staged.first().is_some_and(|first| {
                self.staged
                    .iter()
                    .all(|batch| batch.relation == first.relation)
            });
        Ok(())
    }

    fn truncate_staged(&mut self, len: usize) {
        self.staged.truncate(len);
        self.observed_staged_vertex_batches
            .set(self.observed_staged_vertex_batches.get().min(len));
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

    fn atomic_cancelled_cas_prefix(abort: bool) -> Vec<WriteBatch> {
        let int = CanonicalScalar::Int;
        let mut first = WriteBatch::new(RelationId(9));
        first.set_vertex_property(VId(1), P, Some(int(1)));
        let mut middle = WriteBatch::new(RelationId(2));
        middle.compare_and_set_vertex_property(
            VId(1),
            P,
            Some(int(if abort { 0 } else { 1 })),
            int(if abort { 0 } else { 2 }),
            if abort {
                WriteMismatchPolicy::AbortWrite
            } else {
                WriteMismatchPolicy::NoOp
            },
        );
        let mut last = WriteBatch::new(RelationId(9));
        last.compare_and_set_vertex_property(
            VId(1),
            P,
            Some(int(1)),
            int(0),
            WriteMismatchPolicy::NoOp,
        );
        vec![first, middle, last]
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
                vec![(P, CanonicalScalar::Int(3).into())]
            );
            assert!(txn.vertex(&db, VId(6)).unwrap().is_some());
            assert!(db.vertex(VId(5)).unwrap().is_none());
            assert_eq!(db.frontier().unwrap(), basis);
            let seq = txn.commit(&mut db, &cx).await.unwrap();
            assert_eq!(seq, CommitSeq(basis.0 + 1));
            assert_eq!(db.delta_since(basis).unwrap().count(), 1);
            assert_eq!(
                db.vertex(VId(5)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(3).into())]
            );
            assert_eq!(db.vertex(VId(6)).unwrap().unwrap().birth_ordinal, 4);
            assert_eq!(
                db.edge(EId(50)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(11).into())]
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
                vec![(P, CanonicalScalar::Int(2).into())]
            );
            txn.write(&mut db, last).unwrap();
            txn.commit(&mut db, &cx).await.unwrap();
            assert_eq!(
                db.vertex(VId(5)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(3).into())]
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn late_ordered_failure_and_savepoint_restore_the_exact_prepared_prefix() {
        let ((), report) = run_async_under_lab(0x6972_0107, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let mut db = seeded(&cx).await;
            let basis = db.frontier().unwrap();
            let int = CanonicalScalar::Int;
            let mut first = WriteBatch::new(RelationId(9));
            first.ensure_vertex(VId(1), vec![], vec![(P, int(999))]);
            first.create_vertex(VId(5), vec![], vec![(P, int(5))]);
            let mut same_relation = WriteBatch::new(RelationId(9));
            same_relation.add_edge(EId(50), VId(1), VId(5), vec![(P, int(50))]);
            same_relation.set_vertex_property(VId(2), P, Some(int(7)));
            let mut second = WriteBatch::new(RelationId(2));
            second.create_vertex(VId(6), vec![], vec![(P, int(6))]);
            second.add_edge(EId(60), VId(5), VId(6), vec![(P, int(60))]);
            second.set_edge_property(EId(50), P, Some(int(51)));
            second.set_vertex_label(VId(5), fgdb_delta_types::LabelId(2), true);
            let prefix = vec![first, same_relation, second];
            let mut txn = db.begin(&contexts.txn()).unwrap();
            for batch in &prefix {
                txn.write_ordered(&mut db, vec![batch.clone()]).unwrap();
            }
            txn.savepoint(&db, "prefix").unwrap();
            let previous = std::sync::Arc::clone(txn.prepared.as_ref().unwrap());
            let mut first = WriteBatch::new(RelationId(1));
            first.create_vertex(VId(500), vec![], vec![]);
            let mut last = WriteBatch::new(RelationId(3));
            last.compare_and_set_edge_property(
                EId(60),
                P,
                Some(int(999)),
                int(61),
                WriteMismatchPolicy::AbortWrite,
            );
            assert!(
                txn.write_ordered(&mut db, vec![first.clone(), last])
                    .is_err()
            );
            assert!(std::sync::Arc::ptr_eq(
                txn.prepared.as_ref().unwrap(),
                &previous
            ));
            assert_eq!(txn.staged.len(), prefix.len());
            assert!(txn.vertex(&db, VId(500)).unwrap().is_none());
            let mut last = WriteBatch::new(RelationId(3));
            last.compare_and_set_edge_property(
                EId(60),
                P,
                Some(int(60)),
                int(61),
                WriteMismatchPolicy::AbortWrite,
            );
            let suffix = vec![first, last];
            txn.write_ordered(&mut db, suffix.clone()).unwrap();
            let full = db
                .prepare_ordered_writes(prefix.into_iter().chain(suffix.clone()).collect())
                .unwrap();
            let actual = txn.prepared.as_ref().unwrap();
            assert_eq!(
                actual.template.canonical_bytes(),
                full.template.canonical_bytes()
            );
            assert_eq!(
                actual.dependencies.observations().collect::<Vec<_>>(),
                full.dependencies.observations().collect::<Vec<_>>(),
            );
            assert_eq!(actual.replay_row_allowance, full.replay_row_allowance);
            assert_eq!(actual.basis(), full.basis());
            txn.rollback_to_savepoint(&db, "prefix").unwrap();
            assert!(std::sync::Arc::ptr_eq(
                txn.prepared.as_ref().unwrap(),
                &previous
            ));
            assert!(txn.vertex(&db, VId(500)).unwrap().is_none());
            txn.write_ordered(&mut db, suffix).unwrap();
            assert_eq!(db.frontier().unwrap(), basis);
            txn.commit(&mut db, &cx).await.unwrap();
            assert_eq!(db.delta_since(basis).unwrap().count(), 1);
            assert!(db.vertex(VId(500)).unwrap().is_some());
            assert_eq!(
                db.edge(EId(60)).unwrap().unwrap().props,
                vec![(P, int(61).into())]
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
                vec![(P, CanonicalScalar::Int(3).into())]
            );
            assert_eq!(db.frontier().unwrap(), basis);
            let seq = txn.commit(&mut db, &cx).await.unwrap();
            assert_eq!(seq, CommitSeq(basis.0 + 1));
            assert_eq!(
                db.edge(EId(50)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(11).into())]
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
                    vec![(P, CanonicalScalar::Int(1).into())]
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
                    vec![(P, CanonicalScalar::Int(10).into())]
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
                vec![(P, CanonicalScalar::Int(3).into())]
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn atomic_cancelled_cas_prefix_matches_full_ordered_continuation() {
        let ((), report) = run_async_under_lab(0x6972_0201, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            for explicit_ordered in [false, true] {
                let mut db = seeded(&cx).await;
                let basis = db.frontier().unwrap();
                let prefix = atomic_cancelled_cas_prefix(false);
                let mut txn = db.begin(&contexts.txn()).unwrap();
                txn.write_atomic(&mut db, prefix.clone()).unwrap();
                assert!(!txn.prepared_source_ordered);
                assert!(
                    txn.prepared
                        .as_ref()
                        .unwrap()
                        .template
                        .rows()
                        .next()
                        .is_none()
                );
                assert_eq!(
                    txn.vertex(&db, VId(1)).unwrap().unwrap().props,
                    vec![(P, CanonicalScalar::Int(0).into())]
                );
                let mut suffix = WriteBatch::new(RelationId(3));
                suffix.create_vertex(VId(5), vec![], vec![]);
                let mut raw = prefix;
                raw.push(suffix.clone());
                let full = db.prepare_ordered_writes(raw.clone()).unwrap();
                if explicit_ordered {
                    txn.write_ordered(&mut db, vec![suffix]).unwrap();
                } else {
                    // Ordinary write auto-routes an existing mixed prefix.
                    txn.write(&mut db, suffix).unwrap();
                }
                assert!(txn.prepared_source_ordered);
                let actual = txn.prepared.as_ref().unwrap();
                assert_eq!(
                    actual.template.canonical_bytes(),
                    full.template.canonical_bytes()
                );
                assert_eq!(
                    actual.dependencies.observations().collect::<Vec<_>>(),
                    full.dependencies.observations().collect::<Vec<_>>()
                );
                assert!(actual.template.rows().any(|row| row
                    == &fgdb_delta_types::DeltaRow::Property {
                        elem: ElementId::Vertex(VId(1)),
                        property: P,
                        before: Some(CanonicalScalar::Int(0).into()),
                        after: Some(CanonicalScalar::Int(2).into()),
                    }));
                assert_eq!(
                    txn.vertex(&db, VId(1)).unwrap().unwrap().props,
                    vec![(P, CanonicalScalar::Int(2).into())]
                );
                // Ordered provenance is independent of replay authority. A
                // later supported suffix reuses this proved prefix, retaining
                // a deliberately withheld allowance rather than re-granting it.
                std::sync::Arc::make_mut(txn.prepared.as_mut().unwrap()).replay_row_allowance =
                    None;
                let mut next = WriteBatch::new(RelationId(4));
                next.compare_and_set_vertex_property(
                    VId(1),
                    P,
                    Some(CanonicalScalar::Int(2)),
                    CanonicalScalar::Int(3),
                    WriteMismatchPolicy::AbortWrite,
                );
                raw.push(next.clone());
                txn.write(&mut db, next).unwrap();
                let full = db.prepare_ordered_writes(raw).unwrap();
                assert_eq!(
                    txn.prepared.as_ref().unwrap().template.canonical_bytes(),
                    full.template.canonical_bytes()
                );
                assert_eq!(txn.prepared.as_ref().unwrap().replay_row_allowance, None);
                assert_eq!(db.frontier().unwrap(), basis);
                assert!(db.vertex(VId(5)).unwrap().is_none());
                assert_eq!(
                    txn.commit(&mut db, &cx).await.unwrap(),
                    CommitSeq(basis.0 + 1)
                );
                assert_eq!(
                    db.vertex(VId(1)).unwrap().unwrap().props,
                    vec![(P, CanonicalScalar::Int(3).into())]
                );
                assert!(db.vertex(VId(5)).unwrap().is_some());
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn atomic_cancelled_cas_refusal_preserves_the_original_prepared_prefix() {
        let ((), report) = run_async_under_lab(0x6972_0202, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            for explicit_ordered in [false, true] {
                let mut db = seeded(&cx).await;
                let basis = db.frontier().unwrap();
                let prefix = atomic_cancelled_cas_prefix(true);
                let mut txn = db.begin(&contexts.txn()).unwrap();
                txn.write_atomic(&mut db, prefix.clone()).unwrap();
                let previous = std::sync::Arc::clone(txn.prepared.as_ref().unwrap());
                assert!(previous.template.rows().next().is_none());
                let mut suffix = WriteBatch::new(RelationId(3));
                suffix.create_vertex(VId(5), vec![], vec![]);
                let expected = db
                    .prepare_ordered_writes(prefix.into_iter().chain([suffix.clone()]).collect())
                    .unwrap_err();
                let WriteTxnError::Write(WriteError::CompareAndSetMismatch(expected)) = expected
                else {
                    panic!("ordered replay must refuse its CAS guard");
                };
                let actual = if explicit_ordered {
                    txn.write_ordered(&mut db, vec![suffix])
                } else {
                    txn.write(&mut db, suffix)
                }
                .unwrap_err();
                let WriteTxnError::Write(WriteError::CompareAndSetMismatch(actual)) = actual else {
                    panic!("ordered continuation must refuse the same CAS guard");
                };
                // Debug redacts property values; compare the typed error and
                // independently require the exact source-order preimages.
                assert_eq!(actual, expected);
                assert_eq!(actual.elem, ElementId::Vertex(VId(1)));
                assert_eq!(actual.name, P);
                assert_eq!(actual.expected, Some(CanonicalScalar::Int(0).into()));
                assert_eq!(actual.actual, Some(CanonicalScalar::Int(1).into()));
                assert!(std::sync::Arc::ptr_eq(
                    txn.prepared.as_ref().unwrap(),
                    &previous
                ));
                assert!(!txn.prepared_source_ordered);
                assert_eq!(txn.staged.len(), 3);
                assert_eq!(
                    txn.vertex(&db, VId(1)).unwrap().unwrap().props,
                    vec![(P, CanonicalScalar::Int(0).into())]
                );
                assert!(txn.vertex(&db, VId(5)).unwrap().is_none());
                assert_eq!(db.frontier().unwrap(), basis);
                txn.abort();
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn atomic_ordered_provenance_restores_with_savepoints_and_workspaces() {
        let ((), report) = run_async_under_lab(0x6972_0203, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let mut db = seeded(&cx).await;
            let mut txn = db.begin(&contexts.txn()).unwrap();
            txn.write_atomic(&mut db, atomic_cancelled_cas_prefix(false))
                .unwrap();
            txn.savepoint(&db, "atomic").unwrap();
            let previous = std::sync::Arc::clone(txn.prepared.as_ref().unwrap());
            let mut suffix = WriteBatch::new(RelationId(3));
            suffix.create_vertex(VId(5), vec![], vec![]);
            let mut atomic_suffix = WriteBatch::new(RelationId(4));
            atomic_suffix.create_vertex(VId(6), vec![], vec![]);
            for unwind in [false, true] {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let outer = MutationProgramWorkspace::new(&mut txn);
                    outer
                        .txn
                        .write_ordered(&mut db, vec![suffix.clone()])
                        .unwrap();
                    assert!(outer.txn.prepared_source_ordered);
                    {
                        let inner = MutationProgramWorkspace::new(outer.txn);
                        inner
                            .txn
                            .write_atomic(&mut db, vec![atomic_suffix.clone()])
                            .unwrap();
                        assert!(!inner.txn.prepared_source_ordered);
                    }
                    assert!(outer.txn.prepared_source_ordered);
                    assert_eq!(
                        outer.txn.vertex(&db, VId(1)).unwrap().unwrap().props,
                        vec![(P, CanonicalScalar::Int(2).into())]
                    );
                    if unwind {
                        std::panic::resume_unwind(Box::new("atomic-to-ordered workspace unwind"));
                    }
                    // Unaccepted normal return and unwinding restore the same proof.
                }));
                assert_eq!(outcome.is_err(), unwind);
                assert!(!txn.prepared_source_ordered);
                assert!(std::sync::Arc::ptr_eq(
                    txn.prepared.as_ref().unwrap(),
                    &previous
                ));
                assert!(txn.vertex(&db, VId(5)).unwrap().is_none());
            }
            txn.write_ordered(&mut db, vec![suffix.clone()]).unwrap();
            txn.savepoint(&db, "ordered").unwrap();
            txn.write_atomic(&mut db, vec![atomic_suffix]).unwrap();
            assert!(!txn.prepared_source_ordered);
            txn.rollback_to_savepoint(&db, "ordered").unwrap();
            assert!(txn.prepared_source_ordered);
            assert_eq!(
                txn.vertex(&db, VId(1)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(2).into())]
            );
            assert!(txn.vertex(&db, VId(6)).unwrap().is_none());
            txn.rollback_to_savepoint(&db, "atomic").unwrap();
            assert!(!txn.prepared_source_ordered);
            assert!(std::sync::Arc::ptr_eq(
                txn.prepared.as_ref().unwrap(),
                &previous
            ));
            txn.write(&mut db, suffix).unwrap();
            assert!(txn.prepared_source_ordered);
            assert_eq!(
                txn.vertex(&db, VId(1)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(2).into())]
            );
            txn.commit(&mut db, &cx).await.unwrap();
            assert!(db.vertex(VId(5)).unwrap().is_some());
            assert!(db.vertex(VId(6)).unwrap().is_none());
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
            assert!(!txn.prepared_source_ordered);
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
            assert!(txn.prepared_source_ordered);
            assert_eq!([birth(&txn, &db, 5), birth(&txn, &db, 6)], births);
            txn.rollback_to_savepoint(&db, "independent").unwrap();
            assert!(!txn.prepared_source_ordered);
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

    #[test]
    fn incremental_births_after_atomic_reordering_survive_failure_and_rollback() {
        let ((), report) = run_async_under_lab(0x6f74_0009, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut db = seeded(&cx).await;
            let basis = db.frontier().unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            let mut high = WriteBatch::new(RelationId(9));
            high.create_vertex(VId(5), vec![], vec![(P, CanonicalScalar::Int(1))]);
            let mut low = WriteBatch::new(RelationId(2));
            low.create_vertex(VId(6), vec![], vec![(P, CanonicalScalar::Int(2))]);
            txn.write_atomic(&mut db, vec![high, low]).unwrap();
            let birth = |txn: &WriteTxn, db: &Database<MemVfs>, id| {
                txn.vertex(db, VId(id)).unwrap().unwrap().birth_ordinal
            };
            let retained_births = [birth(&txn, &db, 5), birth(&txn, &db, 6)];
            assert!(retained_births[1] < retained_births[0]);

            // This first continuation needs full source-order preparation and
            // restoration of the already observed relation-order births.
            let mut transition = WriteBatch::new(RelationId(4));
            transition.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(3)));
            transition.add_edge(EId(50), VId(5), VId(6), vec![]);
            txn.write_ordered(&mut db, vec![transition]).unwrap();
            assert!(txn.prepared_source_ordered);
            assert_eq!([birth(&txn, &db, 5), birth(&txn, &db, 6)], retained_births);
            let saved = std::sync::Arc::clone(txn.prepared.as_ref().unwrap());
            txn.savepoint(&db, "ordered").unwrap();

            // The first suffix seeds an old edge and its reversed-birth
            // endpoints. The second moves the shared vertex backbone to a
            // lower coordinate. Neither may reinterpret those old ordinals.
            let mut stable = WriteBatch::new(RelationId(2));
            stable.set_edge_property(EId(50), P, Some(CanonicalScalar::Int(7)));
            stable.create_vertex(VId(7), vec![], vec![]);
            let mut moved = WriteBatch::new(RelationId(1));
            moved.set_vertex_property(VId(6), P, Some(CanonicalScalar::Int(4)));
            moved.create_vertex(VId(8), vec![], vec![]);
            moved.add_edge(EId(80), VId(7), VId(8), vec![]);
            let suffixes = [stable, moved];
            let mut complete = None;
            for round in 0..2 {
                for suffix in &suffixes {
                    let previous = std::sync::Arc::clone(txn.prepared.as_ref().unwrap());
                    let mut raw = txn.staged.clone();
                    raw.push(suffix.clone());
                    let expected = db
                        .prepare_ordered_writes(raw)
                        .unwrap()
                        .retain_birth_ordinals(Some(&previous))
                        .unwrap();
                    txn.write_ordered(&mut db, vec![suffix.clone()]).unwrap();
                    let actual = txn.prepared.as_ref().unwrap();
                    assert_eq!(
                        actual.template.canonical_bytes(),
                        expected.template.canonical_bytes()
                    );
                    assert_eq!(
                        actual.dependencies.observations().collect::<Vec<_>>(),
                        expected.dependencies.observations().collect::<Vec<_>>()
                    );
                    assert_eq!(actual.replay_row_allowance, expected.replay_row_allowance);
                    assert_eq!([birth(&txn, &db, 5), birth(&txn, &db, 6)], retained_births);
                }
                let actual = std::sync::Arc::clone(txn.prepared.as_ref().unwrap());
                if let Some(expected) = &complete {
                    assert_eq!(&actual.template.canonical_bytes(), expected);
                } else {
                    complete = Some(actual.template.canonical_bytes());
                }
                let mut refused = WriteBatch::new(RelationId(1));
                refused.compare_and_set_vertex_property(
                    VId(5),
                    P,
                    Some(CanonicalScalar::Int(999)),
                    CanonicalScalar::Int(9),
                    WriteMismatchPolicy::AbortWrite,
                );
                assert!(matches!(
                    txn.write_ordered(&mut db, vec![refused]),
                    Err(WriteTxnError::Write(WriteError::CompareAndSetMismatch(_)))
                ));
                assert!(std::sync::Arc::ptr_eq(
                    txn.prepared.as_ref().unwrap(),
                    &actual
                ));
                assert_eq!(db.frontier().unwrap(), basis);
                if round == 0 {
                    txn.rollback_to_savepoint(&db, "ordered").unwrap();
                    assert!(std::sync::Arc::ptr_eq(
                        txn.prepared.as_ref().unwrap(),
                        &saved
                    ));
                    assert!(txn.prepared_source_ordered);
                    assert!(txn.vertex(&db, VId(7)).unwrap().is_none());
                    assert!(txn.vertex(&db, VId(8)).unwrap().is_none());
                }
            }
            let births = [5, 6, 7, 8].map(|id| (id, birth(&txn, &db, id)));
            assert!(births[2].1 > retained_births[0].max(retained_births[1]));
            assert!(births[3].1 > births[2].1);
            assert_eq!(
                txn.commit(&mut db, &cx).await.unwrap(),
                CommitSeq(basis.0 + 1)
            );
            for (id, expected) in births {
                assert_eq!(db.vertex(VId(id)).unwrap().unwrap().birth_ordinal, expected);
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
