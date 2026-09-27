// Re-evaluate a deliberately narrow, decision-free append program. The native
// mutation evaluator remains the only producer of effects and dependencies.

#[derive(Default)]
struct AppendRebaseFootprint {
    creations: std::collections::BTreeSet<ElementId>,
    endpoints: std::collections::BTreeSet<VId>,
}

impl AppendRebaseFootprint {
    fn record(&mut self, row: &PendingRow) -> Result<(), WriteTxnError> {
        match row {
            PendingRow::Vertex {
                vid, ensure: false, ..
            } => {
                self.creations.insert(ElementId::Vertex(*vid));
            }
            PendingRow::Edge {
                eid,
                src,
                dst,
                ensure: false,
                ..
            } => {
                self.creations.insert(ElementId::Edge(*eid));
                self.endpoints.insert(*src);
                self.endpoints.insert(*dst);
            }
            _ => return Err(WriteTxnError::AppendRebaseIneligible),
        }
        Ok(())
    }

    fn conflicts(
        &self,
        row: &fgdb_delta_types::DeltaRow,
        checkpoint: &mut impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<bool, WriteTxnError> {
        use fgdb_delta_types::DeltaRow;
        checkpoint()?;
        match row {
            DeltaRow::CreateVertex { .. }
            | DeltaRow::CreateEdge { .. }
            | DeltaRow::DeleteVertex { .. }
            | DeltaRow::DeleteEdge { .. }
            | DeltaRow::LabelMembership { .. }
            | DeltaRow::Property { .. } => {}
            // Includes schema/constraints. New families need an explicit
            // independence law before any append policy may cross them.
            _ => return Err(WriteTxnError::AppendRebaseIneligible),
        }
        let mut touched = std::collections::BTreeSet::new();
        validation_touches(row, &mut touched, checkpoint)?;
        let mut collision = false;
        for element in &touched {
            checkpoint()?;
            collision |= self.creations.contains(element);
        }
        let retired_endpoint = matches!(
            row, DeltaRow::DeleteVertex { vid, .. } if self.endpoints.contains(vid)
        );
        Ok(collision || retired_endpoint)
    }
}

impl WriteTxn {
    /// Commit vertex/edge creations after read-validated append rebase.
    ///
    /// Ordinary commit conservatively conflicts on shared endpoint vertices.
    /// This opt-in finalization can admit independent appends to the same
    /// neighborhood: it re-evaluates the original typed creation instructions
    /// against the current healthy writer and then uses ordinary FCW validation
    /// and ONE Chronicle publication. No source text is parsed or retried.
    ///
    /// Eligibility requires only unconditional creates, no savepoints and no
    /// active mixed-program scope. Recorded reads are allowed only after the
    /// ordinary conservative point/negative/scan/expansion validator proves
    /// their entire original-basis interval unchanged. Reads are not repeated
    /// or dropped, and a changed read refuses under FG-LAW-FCW-READ-01. Full
    /// vertex observations still conflict with changed incidence or fields;
    /// this does not infer narrower predicates from returned values.
    /// ENSURE, CAS, updates and deletes are refused even when their net effect
    /// was a no-op. The complete
    /// retained history is required. Schema/constraint or unknown delta families,
    /// writes to proposed identities and retirement of any endpoint all refuse.
    /// Reusing an identity that was concurrently created then deleted is refused
    /// even when the current writer no longer contains it.
    /// If the basis must advance, a proposed creation must not have been read:
    /// its staged created_at is a basis placeholder already visible to callers.
    /// The current witness cannot separate those reads from earlier negative
    /// reads of the same identity, so both refuse conservatively. Observations
    /// of existing unrelated elements remain eligible after read validation.
    ///
    /// Re-evaluation must produce the EXACT original canonical template. This
    /// preserves IDs, payloads, creation ordering and already-issued identities;
    /// it never converts a create into an ensure or silently renumbers births.
    /// Only the validated basis and freshly captured dependencies are replaced.
    /// A prefix whose atomic-group ordering cannot be reproduced is refused.
    ///
    /// max_expanded_rows bounds the complete relation-expanded evaluator input
    /// before cloning staged payloads. It is not a byte-memory, history-work or
    /// I/O budget. History/eligibility traversals checkpoint; the existing
    /// synchronous evaluator and template comparison remain non-preemptible.
    ///
    /// This is a TERMINAL commit attempt, not refresh_snapshot: once polled and
    /// owner-admitted, any refusal, cancellation or unwind releases the workspace
    /// and its pin. Wrong-owner calls and unpolled futures preserve it. After
    /// publication starts, the ordinary unknown/recovery contract applies. No
    /// extra cancellation check follows publication. Raw embedded authority is
    /// unchanged; this is not an authorized-token API, full SSI, automatic retry
    /// or the general context-derived semantic merge ladder.
    pub async fn commit_append_only_rebased<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
        max_expanded_rows: u64,
    ) -> Result<CommitSeq, WriteTxnError> {
        let completion = cx
            .with_restriction_async(self.complete_with_basis_controlled(
                database,
                cx,
                None,
                true,
                Some(max_expanded_rows),
                || cx.checkpoint().map_err(WriteTxnError::Interrupted),
            ))
            .await?;
        match completion {
            EmbeddedTxnCompletion::WriteCommitted { commit_seq } => Ok(commit_seq),
            EmbeddedTxnCompletion::ReadClosed { .. } => {
                unreachable!("append finalization requires a prepared write")
            }
        }
    }

    // Called only under the terminal completion guard. A failed observation
    // cannot escape into a still-active old-basis workspace and affect a retry.
    fn prepare_append_rebase<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        max_expanded_rows: u64,
        checkpoint: &mut impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<(), WriteTxnError> {
        let frontier = database.frontier()?;
        let previous = self
            .prepared
            .as_ref()
            .ok_or(WriteTxnError::NoPreparedWrite)?;
        if !std::sync::Arc::ptr_eq(&self.handle_owner, &previous.handle_owner) {
            return Err(WriteTxnError::WrongDatabase);
        }
        if previous.basis != self.basis {
            return Err(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live: previous.basis,
            });
        }
        // The common completion guard has validated every recorded read at
        // the OLD basis. Keep those witnesses; only mutation conflicts rebase.
        if !self.savepoints.is_empty() || self.program_multi_relation {
            return Err(WriteTxnError::AppendRebaseIneligible);
        }
        // Reject conditional/raw decisions BEFORE looking at the new writer.
        // Normalization must not turn an ENSURE or CAS into an eligible append.
        for batch in &self.staged {
            checkpoint()?;
            for row in &batch.rows {
                checkpoint()?;
                if !matches!(
                    row,
                    PendingRow::Vertex { ensure: false, .. }
                        | PendingRow::Edge { ensure: false, .. }
                ) {
                    return Err(WriteTxnError::AppendRebaseIneligible);
                }
            }
        }
        database.admit_ordered_write_rows(self.staged.iter(), max_expanded_rows)?;
        let mut footprint = AppendRebaseFootprint::default();
        for row in self.staged.iter().flat_map(|batch| &batch.rows) {
            checkpoint()?;
            footprint.record(row)?;
        }
        if frontier != self.basis {
            self.validate_unobserved_creations(&footprint.creations, checkpoint)?;
        }
        // A current-state lookup alone misses create/delete identity races and
        // retired endpoints. Admission needs the COMPLETE original-basis tail.
        for batch in database.delta_since(self.basis)? {
            checkpoint()?;
            for coordinate in batch.coordinate_entries() {
                checkpoint()?;
                for row in &coordinate.rows {
                    if footprint.conflicts(row, checkpoint)? {
                        return Err(WriteError::FirstCommitterWins {
                            law: "FG-LAW-FCW-01",
                            detail:
                                "append rebase crossed an identity write or endpoint retirement"
                                    .to_owned(),
                        }
                        .into());
                    }
                }
            }
        }
        checkpoint()?;
        if frontier == self.basis {
            return Ok(());
        }
        // This reconstructs both the canonical effects and CURRENT dependency
        // set with the ordinary evaluator, rather than blessing a stale draft.
        let prepared =
            database.prepare_ordered_writes_bounded(self.staged.clone(), max_expanded_rows)?;
        checkpoint()?;
        if prepared.template != previous.template {
            return Err(WriteTxnError::AppendRebaseIneligible);
        }
        debug_assert_eq!(prepared.basis, frontier);
        self.prepared = Some(prepared);
        self.basis = frontier;
        Ok(())
    }

    // Chronicle history cannot validate an observation of a staged creation:
    // it is not in that history yet. Preserve the original-basis read contract
    // rather than changing escaped metadata while retaining identical deltas.
    fn validate_unobserved_creations(
        &self,
        creations: &std::collections::BTreeSet<ElementId>,
        checkpoint: &mut impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<(), WriteTxnError> {
        let observations = self.read_set.borrow();
        for element in creations {
            checkpoint()?;
            if observations.contains(element) {
                return Err(WriteTxnError::AppendRebaseIneligible);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod append_rebase_tests {
    include!("append_rebase_tests.rs");
}

#[cfg(test)]
mod observed_creation_tests {
    use super::*;
    use crate::{DatabaseKeys, MemVfs};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    const R: RelationId = RelationId(1);
    const P: PropertyKeyId = PropertyKeyId(1);
    const NEW: u128 = u128::MAX;

    fn keys() -> DatabaseKeys {
        DatabaseKeys::new(
            [0x81; 32],
            DatabaseSecurityNamespaceId([0x82; 32]),
            [0x83; 32],
        )
    }

    async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) {
        let mut batch = WriteBatch::new(R);
        for id in 1..=3 {
            batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(0))]);
        }
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(cx, batch).await.unwrap();
    }

    fn append() -> WriteBatch {
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(NEW), vec![], vec![]);
        batch.add_edge(EId(NEW), VId(1), VId(NEW), vec![]);
        batch
    }

    async fn advance(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
        let mut batch = WriteBatch::new(R);
        batch.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(1)));
        db.write(cx, batch).await.unwrap()
    }

    #[test]
    fn staged_vertex_and_edge_metadata_reads_refuse_a_changed_basis() {
        let ((), report) = run_async_under_lab(0x81a0_0001, |root| async move {
            let purposes = PurposeContexts::narrow_runtime_root(&root);
            let cx = purposes.commit();
            let txcx = purposes.txn();
            for edge in [false, true] {
                let mut db = Database::open_memory(&cx, keys()).await.unwrap();
                seed(&mut db, &cx).await;
                let mut tx = db.begin(&txcx).unwrap();
                tx.write(&mut db, append()).unwrap();
                let observed = if edge {
                    tx.edge(&db, EId(NEW)).unwrap().unwrap().entry.created_at
                } else {
                    tx.vertex(&db, VId(NEW)).unwrap().unwrap().created_at
                };
                assert_eq!(observed, tx.basis());
                let frontier = advance(&mut db, &cx).await;
                assert!(matches!(
                    tx.commit_append_only_rebased(&mut db, &cx, 2).await,
                    Err(WriteTxnError::AppendRebaseIneligible)
                ));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertex(VId(NEW)).unwrap().is_none());
                assert!(db.edge(EId(NEW)).unwrap().is_none());
                assert_eq!(tx.state(), EmbeddedTxnState::Aborted);
                assert!(tx.prepared.is_none());
                assert!(tx.staged.is_empty());
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn negative_creation_reads_refuse_but_no_drift_keeps_read_your_writes() {
        let ((), report) = run_async_under_lab(0x81a0_0002, |root| async move {
            let purposes = PurposeContexts::narrow_runtime_root(&root);
            let cx = purposes.commit();
            let txcx = purposes.txn();
            for drift in [false, true] {
                let mut db = Database::open_memory(&cx, keys()).await.unwrap();
                seed(&mut db, &cx).await;
                let mut tx = db.begin(&txcx).unwrap();
                assert!(tx.vertex(&db, VId(NEW)).unwrap().is_none());
                assert!(tx.edge(&db, EId(NEW)).unwrap().is_none());
                tx.write(&mut db, append()).unwrap();
                assert!(tx.vertex(&db, VId(NEW)).unwrap().is_some());
                assert!(tx.edge(&db, EId(NEW)).unwrap().is_some());
                if drift {
                    advance(&mut db, &cx).await;
                }
                let frontier = db.frontier().unwrap();
                let result = tx.commit_append_only_rebased(&mut db, &cx, 2).await;
                if drift {
                    assert!(matches!(result, Err(WriteTxnError::AppendRebaseIneligible)));
                    assert_eq!(db.frontier().unwrap(), frontier);
                } else {
                    assert_eq!(result.unwrap(), CommitSeq(frontier.0 + 1));
                    assert!(db.vertex(VId(NEW)).unwrap().is_some());
                    assert!(db.edge(EId(NEW)).unwrap().is_some());
                }
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn existing_observations_and_unobserved_appends_still_rebase_and_reopen() {
        let ((), report) = run_async_under_lab(0x81a0_0003, |root| async move {
            let purposes = PurposeContexts::narrow_runtime_root(&root);
            let cx = purposes.commit();
            let txcx = purposes.txn();
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            seed(&mut db, &cx).await;
            let mut tx = db.begin(&txcx).unwrap();
            assert!(tx.vertex(&db, VId(1)).unwrap().is_some());
            assert!(tx.edge(&db, EId(10)).unwrap().is_some());
            tx.write(&mut db, append()).unwrap();
            let original = tx.prepared.as_ref().unwrap().template.clone();
            let frontier = advance(&mut db, &cx).await;
            let seq = tx
                .commit_append_only_rebased(&mut db, &cx, 2)
                .await
                .unwrap();
            assert_eq!(seq, CommitSeq(frontier.0 + 1));
            let tail = db.delta_since(frontier).unwrap().collect::<Vec<_>>();
            assert_eq!(tail.len(), 1);
            assert_eq!(tail[0].coordinate_entries(), original.coordinate_entries());
            drop(db);
            let db = Database::open_with_vfs(&cx, vfs, &path, keys())
                .await
                .unwrap();
            assert!(db.vertex(VId(NEW)).unwrap().is_some());
            assert_eq!(db.edge(EId(NEW)).unwrap().unwrap().entry.dst, VId(NEW));
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn creation_observation_walk_is_cancellable_and_keeps_identity_domains() {
        let ((), report) = run_async_under_lab(0x81a0_0004, |root| async move {
            let purposes = PurposeContexts::narrow_runtime_root(&root);
            let cx = purposes.commit();
            let txcx = purposes.txn();
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut db, &cx).await;
            let tx = db.begin(&txcx).unwrap();
            tx.read_set.borrow_mut().insert(ElementId::Edge(EId(NEW)));
            let creations = (1..=64)
                .map(|id| ElementId::Vertex(VId(id)))
                .chain([ElementId::Vertex(VId(NEW))])
                .collect();
            for stop in 1..=65 {
                let mut seen = 0;
                let result = tx.validate_unobserved_creations(&creations, &mut || {
                    seen += 1;
                    if seen == stop {
                        Err(WriteTxnError::NoPreparedWrite)
                    } else {
                        Ok(())
                    }
                });
                assert!(matches!(result, Err(WriteTxnError::NoPreparedWrite)));
                assert_eq!(seen, stop);
            }
            assert!(
                tx.validate_unobserved_creations(&creations, &mut || Ok(()))
                    .is_ok()
            );
            tx.abort();
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
