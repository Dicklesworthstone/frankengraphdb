// An ordered mutation program has one PRIVATE workspace acceptance boundary.
// The canonical WriteTxn state remains the only overlay used by every step.

/// A non-exported, non-durable guard for one composite operation. Holding the
/// mutable transaction borrow excludes interleaved work. This is not a public
/// savepoint, rollback verb, nested transaction or independently committed step.
struct MutationProgramWorkspace<'txn> {
    txn: &'txn mut WriteTxn,
    staged_len: usize,
    prepared: Option<PreparedWrite>,
    program_multi_relation: bool,
    accepted: bool,
}
impl<'txn> MutationProgramWorkspace<'txn> {
    fn new(txn: &'txn mut WriteTxn) -> Self {
        let staged_len = txn.staged.len();
        // Keep the exact already-validated prefix, not a recipe that could be
        // re-prepared against a different basis on failure. The current copy is
        // still needed by the first statement's canonical overlay selection.
        let prepared = txn.prepared.clone();
        let program_multi_relation = txn.program_multi_relation;
        Self {
            txn,
            staged_len,
            prepared,
            program_multi_relation,
            accepted: false,
        }
    }
    fn accept(mut self) {
        self.accepted = true;
    }
}
impl Drop for MutationProgramWorkspace<'_> {
    fn drop(&mut self) {
        // Scope permission never escapes, including accepted or nested workspaces.
        self.txn.program_multi_relation = self.program_multi_relation;
        if self.accepted {
            return;
        }
        let appended = self.txn.staged.len() > self.staged_len;
        let discarded = core::mem::replace(&mut self.txn.prepared, self.prepared.take());
        self.txn.staged.truncate(self.staged_len);
        // Restore effects FIRST. Do not rewind query/scan observations: even an
        // error, conditional no-op or discarded program can expose its basis.
        // The latest successful preparation captured the complete staged prefix,
        // including raw intentions that normalized away. Preserve that evidence
        // before discarding the prepared effects. Failed ordinary preparations
        // already retain their own observations in WriteTxn::write.
        if appended && let Some(prepared) = discarded {
            prepared
                .dependencies
                .retain_observations(&mut self.txn.read_set.borrow_mut());
        }
    }
}

impl WriteTxn {
    /// Stage a bounded ordered program as one all-or-nothing embedded operation.
    /// Each statement matches the canonical overlay produced by earlier steps;
    /// each statement's assignments remain simultaneous. Only after every step
    /// and the final resource/checkpoint boundary succeed is that workspace kept.
    /// Errors, interruption and Rust unwinding restore the exact starting staged
    /// effects while retaining all already-observed read and negative-scan facts.
    ///
    /// This does NOT commit. The caller explicitly commits or aborts afterward.
    /// Program-local changes never reach Chronicle or another reader until the
    /// ordinary transaction commit. A successful empty program result invents no
    /// empty write or marker. Existing relation-group restrictions still apply.
    /// The policy sums query/proposal costs across steps, including work later
    /// canceled out by another step. It does not price cloning the starting
    /// prepared workspace, repeated storage preparation, cascades or commit I/O.
    pub fn execute_graph_mutation_program_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        program: &fgdb_gql::PreparedGraphMutationProgram,
        policy: fgdb_gql::GraphMutationPolicy,
    ) -> Result<
        fgdb_gql::GraphMutationProgramStats,
        fgdb_gql::GraphMutationProgramError<WriteTxnError, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::GraphMutationProgramError as Error;
        // Lifecycle/owner/health/basis/coordinate validation wins over a zero
        // program allowance, even when every statement would match no rows.
        self.ensure_database(database).map_err(Error::Preflight)?;
        let live = database
            .frontier()
            .map_err(WriteTxnError::from)
            .map_err(Error::Preflight)?;
        if live != self.basis {
            return Err(Error::Preflight(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live,
            }));
        }
        if let Some(first) = self.staged.first()
            && self
                .staged
                .iter()
                .all(|batch| batch.relation == first.relation)
            && program.relation() != first.relation
        {
            return Err(Error::Preflight(WriteTxnError::RelationMismatch {
                expected: first.relation,
                found: program.relation(),
            }));
        }
        cx.with_restriction(|| {
            let workspace = MutationProgramWorkspace::new(self);
            let stats = program.execute_governed(
                policy,
                |statement, remaining| {
                    workspace
                        .txn
                        .execute_graph_mutation_governed(database, cx, statement, remaining)
                },
                || cx.checkpoint(),
            )?;
            // No fallible operation or new cancellation check follows accept.
            workspace.accept();
            Ok(stats)
        })
    }
}

#[cfg(test)]
mod mutation_program_workspace_tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    #[test]
    fn unwinding_restores_the_original_prefix_and_keeps_discarded_preparation_reads() {
        let ((), report) = run_async_under_lab(0xb10c_0001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txcx = contexts.txn();
            for change_observed_vertex in [false, true] {
                let keys = crate::DatabaseKeys::new(
                    [0x81; 32],
                    DatabaseSecurityNamespaceId([0x82; 32]),
                    [0x83; 32],
                );
                let mut db = Database::open_memory(&commit, keys).await.unwrap();
                let mut seed = WriteBatch::new(RelationId(1));
                seed.create_vertex(
                    VId(1),
                    vec![],
                    vec![(fgdb_delta_types::PropertyKeyId(1), CanonicalScalar::Int(10))],
                );
                db.write(&commit, seed).await.unwrap();
                let mut txn = db.begin(&txcx).unwrap();
                let mut prefix = WriteBatch::new(RelationId(1));
                prefix.create_vertex(VId(777), vec![], vec![]);
                txn.write(&mut db, prefix).unwrap();
                let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let workspace = MutationProgramWorkspace::new(&mut txn);
                    let mut step = WriteBatch::new(RelationId(1));
                    // Equal-to-current: canonicalization removes the effect but
                    // cannot erase the preparation's before-image observation.
                    step.set_vertex_property(
                        VId(1),
                        fgdb_delta_types::PropertyKeyId(1),
                        Some(CanonicalScalar::Int(10)),
                    );
                    workspace.txn.write(&mut db, step).unwrap();
                    panic!("injected unwind before program acceptance");
                }));
                assert!(failure.is_err());
                if change_observed_vertex {
                    let mut winner = WriteBatch::new(RelationId(1));
                    winner.set_vertex_property(
                        VId(1),
                        fgdb_delta_types::PropertyKeyId(1),
                        Some(CanonicalScalar::Int(11)),
                    );
                    db.write(&commit, winner).await.unwrap();
                    // Commit immediately: no query may repair the lost witness.
                    assert!(matches!(
                        txn.commit(&mut db, &commit).await,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                            law: "FG-LAW-FCW-READ-01",
                            ..
                        }))
                    ));
                    assert!(db.vertex(VId(777)).unwrap().is_none());
                } else {
                    txn.commit(&mut db, &commit).await.unwrap();
                    assert!(db.vertex(VId(777)).unwrap().is_some());
                    assert_eq!(
                        db.vertex(VId(1)).unwrap().unwrap().props[0].1,
                        CanonicalScalar::Int(10)
                    );
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
