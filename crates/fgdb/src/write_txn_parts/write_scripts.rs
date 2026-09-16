// Reusable native scripts enter the SAME prepared-program and completion paths.
// Binding is pure and complete before any program step or allocator can run.

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
        self.execute_graph_write_program_returning_governed(database, cx, &program, policy, allocate)
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
        ).await.map_err(Error::Program)
    }
}
