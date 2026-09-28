// Native scripts and text select the stable-basis program lane explicitly.
// Binding, record locations and identity reservations retain their native owners.

impl WriteTxn {
    /// Initialize reservations at the actual live frontier BEFORE selecting a
    /// historical source. Return the ordinary per-step receipt, not a commit.
    /// Failed programs never recycle issued vertex or edge identities.
    pub fn execute_graph_write_program_at_basis_engine_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        program: &fgdb_gql::PreparedGraphWriteProgram,
        policy: fgdb_gql::GraphWriteProgramPolicy,
    ) -> Result<
        fgdb_gql::GraphWriteProgramReceipt,
        fgdb_gql::GraphWriteProgramError<
            WriteTxnError,
            WriteTxnError,
            Box<asupersync::error::Error>,
        >,
    > {
        let preflight = |error| {
            fgdb_gql::GraphWriteProgramError::Program(
                fgdb_gql::GraphMutationProgramError::Preflight(error),
            )
        };
        self.ensure_database(database).map_err(preflight)?;
        let mut allocate = database.engine_allocator(cx).map_err(preflight)?;
        self.execute_graph_write_program_returning_at_basis_governed(
            database,
            cx,
            program,
            policy,
            |request| allocate(request.request),
        )
    }

    /// Bind ALL arguments before reconstruction or execution. Every statement
    /// then sees this transaction's basis plus prior staged effects, with one
    /// cumulative policy and rollback guard. A failure returns no prefix receipt.
    #[allow(clippy::result_large_err)] // once-per-script location and full error
    pub fn execute_graph_write_script_at_basis_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        script: &fgdb_gql::PreparedGraphWriteScript,
        arguments: &fgdb_gql::GqlParameters,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        allocate: impl FnMut(fgdb_gql::GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<
        fgdb_gql::GraphWriteProgramReceipt,
        fgdb_gql::GraphWriteScriptExecutionError<WriteTxnError, A, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::GraphWriteScriptExecutionError as Error;
        self.ensure_database(database).map_err(|error| {
            Error::Program(fgdb_gql::GraphWriteProgramError::Program(
                fgdb_gql::GraphMutationProgramError::Preflight(error),
            ))
        })?;
        let program = script.bind_parameters(arguments).map_err(Error::Binding)?;
        self.execute_graph_write_program_returning_at_basis_governed(
            database, cx, &program, policy, allocate,
        )
        .map_err(Error::Program)
    }

    /// Execute one prebound ingestion batch at the unchanged transaction basis.
    /// Records share one reconstruction, program allowance and rollback guard;
    /// neither per-record publication nor a fresh per-record quota is possible.
    /// Refusals retain the original argument-set/statement error coordinates.
    #[allow(clippy::result_large_err)] // once-per-batch location and full error
    pub fn execute_bound_graph_write_script_batch_at_basis_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        batch: &fgdb_gql::BoundGraphWriteScriptBatch,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        allocate: impl FnMut(fgdb_gql::GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<
        fgdb_gql::GraphWriteProgramReceipt,
        fgdb_gql::GraphWriteScriptExecutionError<WriteTxnError, A, Box<asupersync::error::Error>>,
    > {
        self.execute_graph_write_program_returning_at_basis_governed(
            database,
            cx,
            batch.program(),
            policy,
            allocate,
        )
        .map_err(|source| batch.execution_error(source))
    }

    /// Text front door for stable-basis writes. The existing native compiler
    /// prepares and binds the script; execution consumes only its bound program.
    /// The result is transaction-local with completion == None. This neither
    /// refreshes the basis nor changes query_write's current-frontier contract.
    /// Historical reconstruction has the program-at-basis API's cost boundaries.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::result_large_err)] // once-per-statement report
    pub fn query_write_at_basis<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        text: &str,
        params: &fgdb_gql::GqlParameters,
        resolver: impl FnMut(fgdb_gql::GraphSymbolKind, &str) -> Option<fgdb_gql::GraphSymbol>,
        relation: RelationId,
        budget: fgdb_gql::GraphWriteProgramPolicy,
        allocate: impl FnMut(fgdb_gql::GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<crate::QueryResult, crate::QueryWriteError<A>> {
        self.ensure_database(database).map_err(|error| {
            crate::QueryWriteError::Execute(fgdb_gql::GraphWriteScriptExecutionError::Program(
                fgdb_gql::GraphWriteProgramError::Program(
                    fgdb_gql::GraphMutationProgramError::Preflight(error),
                ),
            ))
        })?;
        let declarations: Vec<(&str, fgdb_gql::GqlParameterType)> = params
            .parameter_types()
            .filter(|(_, kind)| matches!(kind, fgdb_gql::GqlParameterType::Scalar(_)))
            .collect();
        let script = fgdb_gql::PreparedGraphWriteScript::prepare_with_parameter_types(
            text,
            relation,
            &declarations,
            resolver,
        )
        .map_err(crate::QueryWriteError::Prepare)?;
        let receipt = self
            .execute_graph_write_script_at_basis_governed(
                database, cx, &script, params, budget, allocate,
            )
            .map_err(crate::QueryWriteError::Execute)?;
        Ok(crate::QueryResult::Write {
            receipt,
            completion: None,
        })
    }
}

#[cfg(test)]
mod historical_script_tests {
    use super::*;
    use crate::{DatabaseKeys, MemVfs};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_gql::{
        GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
        PreparedGraphWriteScript,
    };
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    const P: PropertyKeyId = PropertyKeyId(1);

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            _ => None,
        }
    }

    fn script(text: &str) -> PreparedGraphWriteScript {
        PreparedGraphWriteScript::prepare_with_parameter_types(text, RelationId(1), &[], symbols)
            .unwrap()
    }

    fn policy() -> GraphWriteProgramPolicy {
        GraphWriteProgramPolicy::new(
            GqlQueryPolicy::new(10_000, 1_000, 100_000, 100_000),
            100,
            100,
            100,
        )
    }

    async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
        let keys = DatabaseKeys::new(
            [0x41; 32],
            DatabaseSecurityNamespaceId([0x42; 32]),
            [0x43; 32],
        );
        let mut db = Database::open_memory(cx, keys).await.unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(VId(1), vec![LabelId(1)], vec![(P, CanonicalScalar::Int(0))]);
        db.write(cx, batch).await.unwrap();
        db
    }

    async fn winner(db: &mut Database<MemVfs>, cx: &CommitCx) {
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(VId(99), vec![], vec![]);
        db.write(cx, batch).await.unwrap();
    }

    #[test]
    fn historical_text_writes_accumulate_at_one_basis_without_publishing_a_prefix() {
        let ((), report) = run_async_under_lab(0x6873_0001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let qcx = contexts.query();
            let mut db = seeded(&cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            let basis = txn.basis();
            winner(&mut db, &cx).await;
            let head = db.frontier().unwrap();
            for expected in [1, 2] {
                let result = txn
                    .query_write_at_basis(
                        &mut db,
                        &qcx,
                        "MATCH (n:L) SET n.p = n.p + $step",
                        &GqlParameters::new().with_int64("step", 1).unwrap(),
                        symbols,
                        RelationId(1),
                        policy(),
                        |_| -> Result<ElementId, &'static str> { panic!("mutation allocated") },
                    )
                    .unwrap();
                assert!(matches!(
                    result,
                    crate::QueryResult::Write {
                        completion: None,
                        ..
                    }
                ));
                assert_eq!(txn.basis(), basis);
                assert_eq!(
                    txn.vertex(&db, VId(1)).unwrap().unwrap().props,
                    vec![(P, CanonicalScalar::Int(expected))]
                );
                assert_eq!(db.frontier().unwrap(), head);
                assert_eq!(
                    db.vertex(VId(1)).unwrap().unwrap().props,
                    vec![(P, CanonicalScalar::Int(0))]
                );
            }
            txn.commit(&mut db, &cx).await.unwrap();
            assert_eq!(db.delta_since(head).unwrap().count(), 1);
            assert_eq!(
                db.vertex(VId(1)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(2))]
            );
            assert!(db.vertex(VId(99)).unwrap().is_some());
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn historical_engine_ids_respect_live_writes_and_failed_reservations_then_reopen() {
        let ((), report) = run_async_under_lab(0x6873_0002, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let qcx = contexts.query();
            let mut db = seeded(&cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            winner(&mut db, &cx).await;
            let read = db.read_session().unwrap();
            let head = db.frontier().unwrap();
            let mut small = policy();
            small.max_created_vertices = 1;
            let two = script("CREATE (n {p: 5}); CREATE (n {p: 6})")
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            assert!(matches!(
                txn.execute_graph_write_program_at_basis_engine_governed(
                    &mut db, &qcx, &two, small,
                ),
                Err(fgdb_gql::GraphWriteProgramError::CreationBudget {
                    statement: 1,
                    limit: 1,
                    observed: 2,
                    ..
                })
            ));
            assert!(txn.prepared.is_none());
            assert!(read.shares_decoded_state_with(&db.read_session().unwrap()));
            let one = script("CREATE (n {p: 5})")
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            let receipt = txn
                .execute_graph_write_program_at_basis_engine_governed(&mut db, &qcx, &one, policy())
                .unwrap();
            let created = receipt.steps()[0].created_vertices().unwrap();
            assert_eq!(created.len(), 1);
            let vid = created[0];
            assert!(
                vid.0 > 100,
                "live V99 and the failed reservation are both spent"
            );
            assert!(db.vertex(vid).unwrap().is_none());
            assert_eq!(
                txn.vertex(&db, vid).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(5))]
            );
            txn.commit(&mut db, &cx).await.unwrap();
            assert_eq!(db.delta_since(head).unwrap().count(), 1);
            let vfs = db.vfs.clone();
            let keys = db.keys.clone();
            let path = db.path().to_path_buf();
            drop(db);
            let reopened = Database::open_with_vfs(&cx, vfs, path, keys).await.unwrap();
            assert_eq!(
                reopened.vertex(vid).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(5))]
            );
            assert!(reopened.vertex(VId(99)).unwrap().is_some());
            assert_eq!(reopened.vertices().unwrap().len(), 3);
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn historical_script_batches_share_quota_and_retain_record_locations() {
        let ((), report) = run_async_under_lab(0x6873_0003, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let txcx = contexts.txn();
            let qcx = contexts.query();
            let mut db = seeded(&cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            winner(&mut db, &cx).await;
            let head = db.frontier().unwrap();
            let batch = script("CREATE (n {p: $value})")
                .bind_parameter_sets_with_limit(
                    &[
                        GqlParameters::new().with_int64("value", 5).unwrap(),
                        GqlParameters::new().with_int64("value", 6).unwrap(),
                    ],
                    2,
                )
                .unwrap();
            let mut allowance = policy();
            allowance.max_created_vertices = 1;
            let mut issued = 0;
            let error = txn.execute_bound_graph_write_script_batch_at_basis_governed(
                &mut db,
                &qcx,
                &batch,
                allowance,
                |_| {
                    issued += 1;
                    Ok::<_, &'static str>(ElementId::Vertex(VId(200 + issued)))
                },
            );
            assert!(matches!(error,
                Err(fgdb_gql::GraphWriteScriptExecutionError::BatchProgram {
                    location: Some(location),
                    source: fgdb_gql::GraphWriteProgramError::CreationBudget {
                        statement: 1, limit: 1, observed: 2, ..
                    },
                }) if location.argument_set == 1 && location.statement == 0
            ));
            assert!(txn.prepared.is_none());
            assert_eq!(issued, 1);
            assert_eq!(db.frontier().unwrap(), head);
            allowance.max_created_vertices = 2;
            let receipt = txn
                .execute_bound_graph_write_script_batch_at_basis_governed(
                    &mut db,
                    &qcx,
                    &batch,
                    allowance,
                    |_| {
                        issued += 1;
                        Ok::<_, &'static str>(ElementId::Vertex(VId(200 + issued)))
                    },
                )
                .unwrap();
            assert_eq!(receipt.steps().len(), 2);
            assert_eq!(receipt.stats().created_vertices, 2);
            txn.commit(&mut db, &cx).await.unwrap();
            assert!(db.vertex(VId(201)).unwrap().is_none());
            assert_eq!(
                db.vertex(VId(202)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(5))]
            );
            assert_eq!(
                db.vertex(VId(203)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(6))]
            );
            assert_eq!(db.delta_since(head).unwrap().count(), 1);
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
