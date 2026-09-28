// Explicit stable-basis GQL execution. The preparation scope selects state;
// the ordinary program executor still owns matching, quotas and rollback.

impl WriteTxn {
    /// Stage a bound mixed GQL program at the transaction's unchanged basis.
    ///
    /// Unlike the current-frontier program entry, another writer's commit does
    /// not itself prevent staging. Every statement sees the pinned snapshot and
    /// earlier staged effects. The existing program guard, cumulative quotas,
    /// failed-statement observations and ordinary FCW completion remain in use.
    /// Neither a refresh nor a rebase occurs, and no intermediate write commits.
    ///
    /// Owner/health admission precedes replay and allocator invocation. Replay
    /// checks cancellation between native operations; one apply/seal/clone can
    /// still run to completion. The program policy does not meter reconstruction
    /// or retained snapshot copying. This is not full SSI, spill or a lease.
    pub fn execute_graph_write_program_at_basis_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        program: &fgdb_gql::PreparedGraphWriteProgram,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        allocate: impl FnMut(fgdb_gql::GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<
        fgdb_gql::GraphWriteProgramStats,
        fgdb_gql::GraphWriteProgramError<WriteTxnError, A, Box<asupersync::error::Error>>,
    > {
        let preflight = |error| {
            fgdb_gql::GraphWriteProgramError::Program(
                fgdb_gql::GraphMutationProgramError::Preflight(error),
            )
        };
        self.ensure_database(database).map_err(preflight)?;
        cx.with_restriction(|| {
            let mut basis = database
                .preparation_basis_controlled(self.basis, || {
                    cx.checkpoint().map_err(WriteTxnError::Interrupted)
                })
                .map_err(preflight)?;
            basis.stage_graph_write_program(self, cx, program, policy, allocate)
        })
    }

    /// The same stable-basis execution with the ordinary per-statement receipt.
    /// A receipt is released only when the whole program is accepted. Any error
    /// or unwind restores the previous workspace and the actual live database;
    /// learned read dependencies remain available to ordinary finish/commit.
    /// Issued external identities are never reclaimed, including on rollback.
    pub fn execute_graph_write_program_returning_at_basis_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        program: &fgdb_gql::PreparedGraphWriteProgram,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        allocate: impl FnMut(fgdb_gql::GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<
        fgdb_gql::GraphWriteProgramReceipt,
        fgdb_gql::GraphWriteProgramError<WriteTxnError, A, Box<asupersync::error::Error>>,
    > {
        let preflight = |error| {
            fgdb_gql::GraphWriteProgramError::Program(
                fgdb_gql::GraphMutationProgramError::Preflight(error),
            )
        };
        self.ensure_database(database).map_err(preflight)?;
        cx.with_restriction(|| {
            let mut basis = database
                .preparation_basis_controlled(self.basis, || {
                    cx.checkpoint().map_err(WriteTxnError::Interrupted)
                })
                .map_err(preflight)?;
            basis.stage_graph_write_program_returning(self, cx, program, policy, allocate)
        })
    }
}

#[cfg(test)]
mod historical_program_tests {
    use super::*;
    use crate::{DatabaseKeys, MemVfs};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_gql::{
        GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
        PreparedGraphWriteProgram, PreparedGraphWriteScript,
    };
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    const P: PropertyKeyId = PropertyKeyId(1);
    const Q: PropertyKeyId = PropertyKeyId(2);

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            _ => None,
        }
    }

    fn program(text: &str) -> PreparedGraphWriteProgram {
        PreparedGraphWriteScript::prepare_with_parameter_types(text, RelationId(1), &[], symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap()
    }

    fn policy() -> GraphWriteProgramPolicy {
        GraphWriteProgramPolicy::new(GqlQueryPolicy::new(10_000, 1_000, 100_000, 100_000), 100, 100, 100)
    }

    async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
        let keys = DatabaseKeys::new(
            [0x31; 32],
            DatabaseSecurityNamespaceId([0x32; 32]),
            [0x33; 32],
        );
        let mut db = Database::open_memory(cx, keys).await.unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(VId(1), vec![LabelId(1)], vec![(P, CanonicalScalar::Int(0))]);
        batch.create_vertex(VId(2), vec![], vec![]);
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(cx, batch).await.unwrap();
        db
    }

    async fn disjoint_winner(db: &mut Database<MemVfs>, cx: &CommitCx) {
        let mut winner = WriteBatch::new(RelationId(1));
        winner.create_vertex(VId(99), vec![], vec![]);
        db.write(cx, winner).await.unwrap();
    }

    #[test]
    fn historical_programs_continue_after_disjoint_commits_and_publish_once() {
        let ((), report) = run_async_under_lab(0x6870_0001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let qcx = contexts.query();
            for returning in [false, true] {
                let mut db = seeded(&cx).await;
                let mut txn = db.begin(&txcx).unwrap();
                let basis = txn.basis();
                disjoint_winner(&mut db, &cx).await;
                let head = db.frontier().unwrap();
                let manifest = db.manifest().unwrap();
                let read = db.read_session().unwrap();
                let program = program("MATCH (n:L) SET n.p = n.p + 1; MATCH (n:L) SET n.q = n.p");
                let never = |_| -> Result<ElementId, &'static str> { panic!("mutation allocated") };
                // The explicit API does not silently change old callers.
                assert!(matches!(
                    txn.execute_graph_write_program_governed(&mut db, &qcx, &program, policy(), never),
                    Err(fgdb_gql::GraphWriteProgramError::Program(
                        fgdb_gql::GraphMutationProgramError::Preflight(WriteTxnError::SnapshotAdvanced { .. })
                    ))
                ));
                if returning {
                    txn.execute_graph_write_program_returning_at_basis_governed(
                        &mut db, &qcx, &program, policy(), never,
                    ).unwrap();
                } else {
                    txn.execute_graph_write_program_at_basis_governed(
                        &mut db, &qcx, &program, policy(), never,
                    ).unwrap();
                }
                assert_eq!(txn.basis(), basis);
                assert_eq!(txn.vertex(&db, VId(1)).unwrap().unwrap().props,
                    vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(1))]);
                assert_eq!(db.frontier().unwrap(), head);
                assert_eq!(db.manifest().unwrap(), manifest);
                assert_eq!(db.vertex(VId(1)).unwrap().unwrap().props, vec![(P, CanonicalScalar::Int(0))]);
                assert!(db.vertex(VId(99)).unwrap().is_some());
                let seq = txn.commit(&mut db, &cx).await.unwrap();
                assert_eq!(seq, CommitSeq(head.0 + 1));
                assert_eq!(db.delta_since(head).unwrap().count(), 1);
                assert_eq!(db.vertex(VId(1)).unwrap().unwrap().props,
                    vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(1))]);
                assert_eq!(read.vertex(VId(1)).unwrap().unwrap().props, vec![(P, CanonicalScalar::Int(0))]);
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn historical_program_observes_old_values_but_cannot_commit_over_a_conflict() {
        let ((), report) = run_async_under_lab(0x6870_0002, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let qcx = contexts.query();
            let mut db = seeded(&cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            let mut winner = WriteBatch::new(RelationId(1));
            winner.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(7)));
            db.write(&cx, winner).await.unwrap();
            let head = db.frontier().unwrap();
            txn.execute_graph_write_program_at_basis_governed(
                &mut db, &qcx, &program("MATCH (n:L) SET n.p = n.p + 1"), policy(),
                |_| -> Result<ElementId, &'static str> { panic!("mutation allocated") },
            ).unwrap();
            assert_eq!(txn.vertex(&db, VId(1)).unwrap().unwrap().props, vec![(P, CanonicalScalar::Int(1))]);
            assert!(matches!(txn.commit(&mut db, &cx).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))));
            assert_eq!(db.frontier().unwrap(), head);
            assert_eq!(db.vertex(VId(1)).unwrap().unwrap().props, vec![(P, CanonicalScalar::Int(7))]);
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn failed_historical_program_restores_prefix_and_keeps_its_reads() {
        let ((), report) = run_async_under_lab(0x6870_0003, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let qcx = contexts.query();
            let mut db = seeded(&cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            let mut prefix = WriteBatch::new(RelationId(1));
            prefix.create_vertex(VId(5), vec![], vec![]);
            txn.write(&mut db, prefix).unwrap();
            txn.savepoint(&db, "prefix").unwrap();
            let saved = txn.prepared.as_ref().unwrap().template.clone();
            disjoint_winner(&mut db, &cx).await;
            let read = db.read_session().unwrap();
            // Step one succeeds; plain DELETE must refuse the incident edge.
            let error = txn.execute_graph_write_program_returning_at_basis_governed(
                &mut db, &qcx, &program("MATCH (n:L) SET n.p = n.p + 1; MATCH (n:L) DELETE n"), policy(),
                |_| -> Result<ElementId, &'static str> { panic!("mutation allocated") },
            );
            assert!(matches!(error, Err(fgdb_gql::GraphWriteProgramError::Delete { statement: 1, .. })));
            assert_eq!(txn.prepared.as_ref().unwrap().template, saved);
            assert_eq!(txn.savepoints.len(), 1);
            assert_eq!(txn.staged.len(), 1);
            assert!(read.shares_decoded_state_with(&db.read_session().unwrap()));
            txn.rollback_to_savepoint(&db, "prefix").unwrap();
            let mut winner = WriteBatch::new(RelationId(1));
            winner.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(9)));
            db.write(&cx, winner).await.unwrap();
            assert!(matches!(txn.commit(&mut db, &cx).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law: "FG-LAW-FCW-READ-01", .. }))));
            assert!(db.vertex(VId(5)).unwrap().is_none());
            assert!(db.edge(EId(10)).unwrap().is_some());
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn every_reconstruction_refusal_or_unwind_leaves_live_state_and_pin_intact() {
        let ((), report) = run_async_under_lab(0x6870_0004, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let mut db = seeded(&cx).await;
            let txn = db.begin(&txcx).unwrap();
            let basis = txn.basis();
            disjoint_winner(&mut db, &cx).await;
            let read = db.read_session().unwrap();
            let mut count = 0;
            drop(db.preparation_basis_controlled(basis, || { count += 1; Ok(()) }).unwrap());
            assert!(count > 5, "must traverse the replay, not only its entry");
            for stop in 1..=count {
                for unwind in [false, true] {
                    let mut seen = 0;
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        db.preparation_basis_controlled(basis, || {
                            seen += 1;
                            if seen == stop {
                                assert!(!unwind, "injected historical replay unwind");
                                return Err(WriteTxnError::NoPreparedWrite);
                            }
                            Ok(())
                        }).map(drop)
                    }));
                    if unwind { assert!(result.is_err()); }
                    else { assert!(matches!(result.unwrap(), Err(WriteTxnError::NoPreparedWrite))); }
                    assert!(read.shares_decoded_state_with(&db.read_session().unwrap()));
                    assert_eq!(db.frontier().unwrap(), read.frontier());
                    assert_eq!(db.vertices().unwrap(), read.vertices().unwrap());
                    assert_eq!(db.edges().unwrap(), read.edges().unwrap());
                    assert_eq!(txn.basis(), basis);
                    assert_eq!(txcx.outstanding_obligations(), 1);
                }
            }
            txn.abort();
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
