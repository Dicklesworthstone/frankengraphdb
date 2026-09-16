// Mixed write programs reuse the existing private program guard and ordinary
// staging paths. This file owns orchestration, not another writer or matcher.

impl WriteTxn {
    /// Stage CREATE, SET/REMOVE, DETACH DELETE and vertex MERGE (including
    /// ON MATCH/ON CREATE actions) as one atomic operation inside this transaction.
    /// Each step sees its predecessor's canonical overlay; assignments within a
    /// step remain frozen and simultaneous. No intermediate step commits.
    ///
    /// Any error, cancellation or Rust unwind restores the exact prior staged
    /// workspace, including its already-prepared write. Read observations are
    /// retained even from failed or rolled-back steps. The snapshot pin stays
    /// live so the caller may continue or abort the outer transaction.
    ///
    /// Identity requests carry the program statement index and row-local request.
    /// The caller's allocation policy must also distinguish separate executions.
    /// Issued identities are NOT reclaimed on rollback. No allocator request
    /// precedes owner, health, basis and coordinate preflight. MERGE's create
    /// branch observes the same remaining creation cap as ordinary insertion;
    /// an existing match does not need creation allowance or a fresh identity.
    ///
    /// Quotas sum source visits, selected occurrences, work, scratch, mutation
    /// intents, created vertices and created edges, including later-canceled
    /// effects. They do not cover allocator service work, cloning the initial
    /// prepared workspace, repeated storage preparation, cascades or commit I/O.
    pub fn execute_graph_write_program_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        program: &fgdb_gql::PreparedGraphWriteProgram,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        mut allocate: impl FnMut(fgdb_gql::GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<
        fgdb_gql::GraphWriteProgramStats,
        fgdb_gql::GraphWriteProgramError<WriteTxnError, A, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::{GraphMutationProgramError, GraphWriteIdentityRequest,
            GraphWriteProgramError, GraphWriteStatement, GraphWriteStepError, GraphWriteStepStats};
        let preflight = |error| GraphWriteProgramError::Program(GraphMutationProgramError::Preflight(error));
        self.ensure_database(database).map_err(preflight)?;
        let live = database.frontier().map_err(WriteTxnError::from).map_err(preflight)?;
        if live != self.basis {
            return Err(preflight(WriteTxnError::SnapshotAdvanced { pinned: self.basis, live }));
        }
        if let Some(first) = self.staged.first()
            && self.staged.iter().all(|batch| batch.relation == first.relation)
            && program.relation() != first.relation
        {
            return Err(preflight(WriteTxnError::RelationMismatch {
                expected: first.relation, found: program.relation(),
            }));
        }
        cx.with_restriction(|| {
            let workspace = MutationProgramWorkspace::new(self);
            let stats = program.execute_governed(policy, |statement, input, remaining| {
                match input {
                    GraphWriteStatement::Mutation(input) => workspace.txn.execute_graph_mutation_governed(
                        database, cx, input, remaining.mutations,
                    ).map(GraphWriteStepStats::Mutation).map_err(GraphWriteStepError::Mutation),
                    GraphWriteStatement::Insert(input) => workspace.txn.execute_graph_insert_governed(
                        database, cx, input, remaining.insertion_policy(),
                        |request| allocate(GraphWriteIdentityRequest { statement, request }),
                    ).map(GraphWriteStepStats::Insert).map_err(GraphWriteStepError::Insert),
                    GraphWriteStatement::VertexMerge(input) => workspace.txn.execute_graph_vertex_merge_governed(
                        database, cx, input, remaining.vertex_merge_policy(),
                        |request| allocate(GraphWriteIdentityRequest { statement, request }),
                    ).map(|(stats, _)| GraphWriteStepStats::VertexMerge(stats))
                        .map_err(GraphWriteStepError::VertexMerge),
                    GraphWriteStatement::VertexUpsert(input) => workspace.txn.execute_graph_vertex_upsert_governed(
                        database, cx, input, remaining.vertex_upsert_policy(),
                        |request| allocate(GraphWriteIdentityRequest { statement, request }),
                    ).map(|(stats, _)| GraphWriteStepStats::VertexUpsert(stats))
                        .map_err(GraphWriteStepError::VertexUpsert),
                }
            }, || cx.checkpoint())?;
            // All quota checks and the final checkpoint ran before acceptance.
            // There is no fallible operation after installing the workspace.
            workspace.accept();
            Ok(stats)
        })
    }

    /// Execute the identical atomic mixed program but retain one ordered receipt
    /// per successfully accepted step. Creation receipts contain exact staged
    /// IDs; mutation receipts contain distinct proposal targets; MERGE receipts
    /// distinguish matched from created identities. No successful-prefix receipt
    /// escapes if ANY statement, quota check or final checkpoint fails.
    ///
    /// Program rollback cannot reclaim external identities. A returned receipt
    /// is still transaction-local: only a later successful finish/commit makes
    /// the staged effects durable.
    pub fn execute_graph_write_program_returning_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        program: &fgdb_gql::PreparedGraphWriteProgram,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        mut allocate: impl FnMut(fgdb_gql::GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<
        fgdb_gql::GraphWriteProgramReceipt,
        fgdb_gql::GraphWriteProgramError<WriteTxnError, A, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::{GraphMutationProgramError, GraphWriteIdentityRequest,
            GraphWriteProgramError, GraphWriteStatement, GraphWriteStepError, GraphWriteStepReceipt,
            GraphWriteStepStats};
        let preflight = |error| GraphWriteProgramError::Program(GraphMutationProgramError::Preflight(error));
        self.ensure_database(database).map_err(preflight)?;
        let live = database.frontier().map_err(WriteTxnError::from).map_err(preflight)?;
        if live != self.basis {
            return Err(preflight(WriteTxnError::SnapshotAdvanced { pinned: self.basis, live }));
        }
        if let Some(first) = self.staged.first()
            && self.staged.iter().all(|batch| batch.relation == first.relation)
            && program.relation() != first.relation
        {
            return Err(preflight(WriteTxnError::RelationMismatch {
                expected: first.relation, found: program.relation(),
            }));
        }
        cx.with_restriction(|| {
            let workspace = MutationProgramWorkspace::new(self);
            let mut receipts = Vec::with_capacity(program.statements().len());
            let stats = program.execute_governed(policy, |statement, input, remaining| {
                match input {
                    GraphWriteStatement::Mutation(input) => workspace.txn
                        .execute_graph_mutation_returning_governed(database, cx, input, remaining.mutations)
                        .map(|(stats, targets)| {
                            receipts.push(GraphWriteStepReceipt::Mutation { targets });
                            GraphWriteStepStats::Mutation(stats)
                        })
                        .map_err(GraphWriteStepError::Mutation),
                    GraphWriteStatement::Insert(input) => workspace.txn
                        .execute_graph_insert_returning_governed(
                            database,
                            cx,
                            input,
                            remaining.insertion_policy(),
                            |request| allocate(GraphWriteIdentityRequest { statement, request }),
                        )
                        .map(|(stats, vertices, edges)| {
                            receipts.push(GraphWriteStepReceipt::Insert { vertices, edges });
                            GraphWriteStepStats::Insert(stats)
                        })
                        .map_err(GraphWriteStepError::Insert),
                    GraphWriteStatement::VertexMerge(input) => workspace.txn
                        .execute_graph_vertex_merge_governed(
                            database, cx, input, remaining.vertex_merge_policy(),
                            |request| allocate(GraphWriteIdentityRequest { statement, request }),
                        )
                        .map(|(stats, outcome)| {
                            receipts.push(GraphWriteStepReceipt::VertexMerge { outcome });
                            GraphWriteStepStats::VertexMerge(stats)
                        })
                        .map_err(GraphWriteStepError::VertexMerge),
                    GraphWriteStatement::VertexUpsert(input) => workspace.txn
                        .execute_graph_vertex_upsert_governed(
                            database, cx, input, remaining.vertex_upsert_policy(),
                            |request| allocate(GraphWriteIdentityRequest { statement, request }),
                        )
                        .map(|(stats, outcome)| {
                            receipts.push(GraphWriteStepReceipt::VertexUpsert { outcome });
                            GraphWriteStepStats::VertexUpsert(stats)
                        })
                        .map_err(GraphWriteStepError::VertexUpsert),
                }
            }, || cx.checkpoint())?;
            debug_assert_eq!(receipts.len(), stats.completed_statements);
            let receipt = fgdb_gql::GraphWriteProgramReceipt::new(stats, receipts);
            // Receipt construction is complete before acceptance. No fallible
            // operation or cancellation point follows this workspace boundary.
            workspace.accept();
            Ok(receipt)
        })
    }
}
