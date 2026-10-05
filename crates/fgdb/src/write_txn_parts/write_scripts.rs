// Reusable native scripts enter the SAME prepared-program and completion paths.
// Binding is pure and complete before any program step or allocator can run.
//
// Each entry point returns once per script or batch. Its Err keeps the failing
// record's location next to the full program error on purpose, so the
// result_large_err allows below are deliberate: the size costs nothing on
// those paths, and boxing would only hide that report behind an indirection.

impl WriteTxn {
    /// Bind every script argument and stage the complete native program with
    /// one shared policy, rollback guard, and returning receipt. This does not
    /// finish the outer transaction. A failed bind leaves its workspace and
    /// observations untouched; an execution failure retains observations but
    /// restores its pre-program staged prefix through the ordinary guard.
    ///
    /// The existing program executor owns database/health/basis preflight,
    /// context restriction, cancellation and identity-allocation ordering.
    /// Binding is bounded definition work, not charged query/operator work.
    #[allow(clippy::result_large_err)] // once-per-script report (file header)
    pub fn execute_graph_write_script_governed<V: Vfs + Clone, A>(
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
        let program = script.bind_parameters(arguments).map_err(Error::Binding)?;
        self.execute_graph_write_program_returning_governed(
            database, cx, &program, policy, allocate,
        )
        .map_err(Error::Program)
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Bind a prepared native script and complete it as ONE autocommit program.
    /// No transaction pin, graph ID, write or partial receipt is issued on a
    /// binding refusal. Successful statements see earlier effects through the
    /// canonical overlay, not through intermediate durable commits.
    ///
    /// Returns the existing exact per-step receipt only after ordinary finish
    /// succeeds, together with WriteCommitted or ReadClosed. Publication errors
    /// keep their existing committed/unknown outcomes; this wrapper never
    /// retries the script or guesses that a failed commit rolled back.
    // Keep all three purpose contexts and the argument map explicit at this
    // application boundary rather than borrowing ambient execution authority.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::result_large_err)] // once-per-script report (file header)
    pub async fn execute_graph_write_script_autocommit_governed<A>(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        script: &fgdb_gql::PreparedGraphWriteScript,
        arguments: &fgdb_gql::GqlParameters,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        allocate: impl FnMut(fgdb_gql::GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<
        (fgdb_gql::GraphWriteProgramReceipt, EmbeddedTxnCompletion),
        fgdb_gql::GraphWriteScriptExecutionError<WriteTxnError, A, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::GraphWriteScriptExecutionError as Error;
        let program = script.bind_parameters(arguments).map_err(Error::Binding)?;
        self.execute_graph_write_program_returning_autocommit_governed(
            txcx, query_cx, commit_cx, &program, policy, allocate,
        )
        .await
        .map_err(Error::Program)
    }
}

impl WriteTxn {
    /// Execute an already-bound ingestion batch through one ordinary returning
    /// write program. Every record sees the canonical overlay of its successful
    /// predecessors; no record or internal chunk obtains a fresh quota.
    /// Failure/unwind restores the outer transaction's original staged prefix
    /// through the existing workspace guard and retains conflict observations.
    ///
    /// The batch may be reused, but external identity allocation must distinguish
    /// invocations. Issued identities are never reclaimed by rollback. Requests
    /// retain flat statement indices; batch.location() maps them to input records.
    /// Receipts are transaction-local until explicit finish/commit succeeds.
    #[allow(clippy::result_large_err)] // once-per-batch report (file header)
    pub fn execute_bound_graph_write_script_batch_governed<V: Vfs + Clone, A>(
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
        self.execute_graph_write_program_returning_governed(
            database,
            cx,
            batch.program(),
            policy,
            allocate,
        )
        .map_err(|source| batch.execution_error(source))
    }

    /// Admit and bind ALL parameter sets before any storage read or allocator
    /// request. max_statements is the expanded batch allowance, not a per-record
    /// limit; the native binder applies its hard ceiling. The execution policy
    /// is independent and shared across the entire batch.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::result_large_err)] // once-per-batch report (file header)
    pub fn execute_graph_write_script_batch_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        script: &fgdb_gql::PreparedGraphWriteScript,
        arguments: &[fgdb_gql::GqlParameters],
        max_statements: usize,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        allocate: impl FnMut(fgdb_gql::GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<
        fgdb_gql::GraphWriteProgramReceipt,
        fgdb_gql::GraphWriteScriptExecutionError<WriteTxnError, A, Box<asupersync::error::Error>>,
    > {
        let batch = script
            .bind_parameter_sets_with_limit(arguments, max_statements)
            .map_err(fgdb_gql::GraphWriteScriptExecutionError::BatchBinding)?;
        self.execute_bound_graph_write_script_batch_governed(database, cx, &batch, policy, allocate)
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Complete one bound batch with the ordinary single-transaction lifecycle.
    /// No per-record commit, retry, or fallback occurs. The complete receipt is
    /// withheld until finish succeeds; finish errors retain their original typed
    /// committed/unknown outcomes and have no fabricated input-record location.
    #[allow(clippy::result_large_err)] // once-per-batch report (file header)
    pub async fn execute_bound_graph_write_script_batch_autocommit_governed<A>(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        batch: &fgdb_gql::BoundGraphWriteScriptBatch,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        allocate: impl FnMut(fgdb_gql::GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<
        (fgdb_gql::GraphWriteProgramReceipt, EmbeddedTxnCompletion),
        fgdb_gql::GraphWriteScriptExecutionError<WriteTxnError, A, Box<asupersync::error::Error>>,
    > {
        self.execute_graph_write_program_returning_autocommit_governed(
            txcx,
            query_cx,
            commit_cx,
            batch.program(),
            policy,
            allocate,
        )
        .await
        .map_err(|source| batch.execution_error(source))
    }

    /// Admit the expanded count and bind every record BEFORE beginning a private
    /// transaction. A late invalid argument creates no transaction pin, graph ID,
    /// database observation, durable marker or partial result. Execution then
    /// uses one shared policy and one ordinary completion for the entire batch.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::result_large_err)] // once-per-batch report (file header)
    pub async fn execute_graph_write_script_batch_autocommit_governed<A>(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        script: &fgdb_gql::PreparedGraphWriteScript,
        arguments: &[fgdb_gql::GqlParameters],
        max_statements: usize,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        allocate: impl FnMut(fgdb_gql::GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<
        (fgdb_gql::GraphWriteProgramReceipt, EmbeddedTxnCompletion),
        fgdb_gql::GraphWriteScriptExecutionError<WriteTxnError, A, Box<asupersync::error::Error>>,
    > {
        let batch = script
            .bind_parameter_sets_with_limit(arguments, max_statements)
            .map_err(fgdb_gql::GraphWriteScriptExecutionError::BatchBinding)?;
        self.execute_bound_graph_write_script_batch_autocommit_governed(
            txcx, query_cx, commit_cx, &batch, policy, allocate,
        )
        .await
    }
}

impl WriteTxn {
    /// Bind a bounded `UNWIND $rows AS row` MERGE or MATCH mutation batch and
    /// stage it in this transaction. Every input binds before the batch enters
    /// the ordinary program executor; no row runs in its own transaction.
    ///
    /// `max_rows` bounds the entire expansion, independently of the shared
    /// execution policy. A binding failure leaves staging and observations
    /// untouched. An execution failure restores the pre-batch staged prefix
    /// while retaining the ordinary conservative conflict observations.
    /// Identity allocations are not reclaimed. The receipt is transaction-local
    /// until the caller finishes the outer transaction.
    ///
    /// The resolver binds symbols, not authority. Context restriction, database
    /// health checks and cancellation remain owned by the program executor.
    #[allow(clippy::too_many_arguments)] // purpose contexts and admission stay explicit
    #[allow(clippy::result_large_err)] // once-per-batch report (file header)
    pub fn execute_graph_unwind_write_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        definition: &fgdb_gql::unwind_write::GraphUnwindWriteText,
        arguments: &fgdb_gql::GqlParameters,
        relation: fgdb_delta_types::RelationId,
        max_rows: usize,
        resolve: impl FnMut(fgdb_gql::GraphSymbolKind, &str) -> Option<fgdb_gql::GraphSymbol>,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        allocate: impl FnMut(fgdb_gql::GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<
        fgdb_gql::GraphWriteProgramReceipt,
        fgdb_gql::unwind_write::GraphUnwindWriteExecutionError<
            WriteTxnError,
            A,
            Box<asupersync::error::Error>,
        >,
    > {
        use fgdb_gql::unwind_write::GraphUnwindWriteExecutionError as Error;
        let batch = definition
            .bind_with_limit(arguments, relation, max_rows, resolve)
            .map_err(Error::Binding)?;
        self.execute_bound_graph_write_script_batch_governed(database, cx, &batch, policy, allocate)
            .map_err(Error::Execution)
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute a bounded UNWIND write batch as one ordinary autocommit program.
    /// Parsing is explicit through `GraphUnwindWriteText`; native UNWIND CREATE
    /// and CREATE RETURN are not intercepted by this entry point.
    ///
    /// All rows bind before a private transaction or graph-identity allocation
    /// begins. Rows see earlier staged effects, so repeated keys can MERGE the
    /// same vertex. Quotas belong to the whole batch, not each row. No partial
    /// receipt is returned on failure, and successful receipts are withheld
    /// until the ordinary finish path returns WriteCommitted or ReadClosed.
    ///
    /// This method never retries or relabels publication failures as rollback.
    /// Execution errors preserve both the input-row coordinates and the original
    /// committed/unknown outcome. The lexical definition and arguments may be
    /// reused; identity allocation must distinguish separate invocations.
    #[allow(clippy::too_many_arguments)] // purpose contexts and admission stay explicit
    #[allow(clippy::result_large_err)] // once-per-batch report (file header)
    pub async fn execute_graph_unwind_write_autocommit_governed<A>(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        definition: &fgdb_gql::unwind_write::GraphUnwindWriteText,
        arguments: &fgdb_gql::GqlParameters,
        relation: fgdb_delta_types::RelationId,
        max_rows: usize,
        resolve: impl FnMut(fgdb_gql::GraphSymbolKind, &str) -> Option<fgdb_gql::GraphSymbol>,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        allocate: impl FnMut(fgdb_gql::GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<
        (fgdb_gql::GraphWriteProgramReceipt, EmbeddedTxnCompletion),
        fgdb_gql::unwind_write::GraphUnwindWriteExecutionError<
            WriteTxnError,
            A,
            Box<asupersync::error::Error>,
        >,
    > {
        use fgdb_gql::unwind_write::GraphUnwindWriteExecutionError as Error;
        let batch = definition
            .bind_with_limit(arguments, relation, max_rows, resolve)
            .map_err(Error::Binding)?;
        self.execute_bound_graph_write_script_batch_autocommit_governed(
            txcx, query_cx, commit_cx, &batch, policy, allocate,
        )
        .await
        .map_err(Error::Execution)
    }
}
