// One atomic write followed by a correlated native read of its staged overlay.
// The read is a prepared relation, not a text rewrite or another interpreter.

type WriteChainReadResult<A> = Result<
    (
        fgdb_gql::GraphWriteProgramReceipt,
        fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
    ),
    TxnGqlError<fgdb_gql::GraphWriteChainError<WriteTxnError, A>>,
>;

fn write_read_source_error<A>(
    error: WriteTxnError,
) -> TxnGqlError<fgdb_gql::GraphWriteChainError<WriteTxnError, A>> {
    fgdb_gql::GqlQueryError::Source(fgdb_gql::GraphWriteChainError::Read(
        fgdb_gql::GraphSetExecutionError::Source(error),
    ))
}

fn admit_write_read<A>(
    chain: &fgdb_gql::PreparedGraphWriteChain,
    continuation: &fgdb_gql::PreparedGraphSet,
) -> Result<(), TxnGqlError<fgdb_gql::GraphWriteChainError<WriteTxnError, A>>> {
    if chain.accepts_read_continuation(continuation) {
        Ok(())
    } else {
        Err(fgdb_gql::GqlQueryError::Source(
            fgdb_gql::GraphWriteChainError::Read(fgdb_gql::GraphSetExecutionError::InputSchema {
                operand: 0,
            }),
        ))
    }
}

impl WriteTxn {
    /// Execute a returning native write chain and then a correlated read in
    /// ONE statement workspace. Build the read from `chain.read_input()` with
    /// the existing relational operators. An Inner join is a following MATCH;
    /// a Left join supplies OPTIONAL MATCH null extension. Every read sees the
    /// complete staged write plus the transaction's earlier accepted prefix.
    ///
    /// A late source, expression, budget or cancellation refusal restores that
    /// earlier prefix, retaining read dependencies and spent allocator IDs.
    /// There is no nested transaction or intermediate commit. Success remains
    /// transaction-local until the caller finishes the outer transaction.
    ///
    /// The receipt describes ONLY the write prefix and owns its original IDs.
    /// The returned execution includes write AND read snapshot/work/scratch
    /// counters and only the read's final delivered-row count. Do not add the
    /// receipt's counters to it. The write's explicit RETURN grouping/page is
    /// preserved, but its private output does not spend the final row limit.
    /// This is an owner-level typed API, not read-after-write text compilation.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_graph_write_chain_then_read_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        chain: &fgdb_gql::PreparedGraphWriteChain,
        continuation: &fgdb_gql::PreparedGraphSet,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> WriteChainReadResult<A> {
        use fgdb_gql::{GqlExecutionBudget, GqlQueryError, WithElementReads};
        self.ensure_database(database)
            .map_err(write_read_source_error)?;
        admit_write_read(chain, continuation)?;
        cx.with_restriction(|| {
            cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
            let workspace = MutationProgramWorkspace::new(self);
            let mut write_policy = policy;
            write_policy.query.rows = match policy.query.rows.max_snapshot_records() {
                Some(limit) => GqlExecutionBudget::snapshot_records(limit),
                None => GqlExecutionBudget::UNLIMITED,
            };
            let (_, head, receipt) = workspace.txn.execute_graph_write_chain_prefix_governed(
                database,
                cx,
                chain,
                write_policy,
                allocate,
            )?;
            // Both pattern and point reads use this same staged overlay. The
            // owned row bag is moved through the native source wrapper once.
            let source = WithElementReads {
                source: InsertGraphSource {
                    transaction: workspace.txn,
                    database,
                    cx,
                },
                elements: |element, key| {
                    workspace.txn.merge_return_property(database, element, key)
                },
            };
            let returning =
                chain.finish_read_continuation(continuation, head, policy.query, source, || {
                    cx.checkpoint()
                })?;
            cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
            workspace.accept();
            Ok((receipt, returning))
        })
    }

    /// The same composition with identities reserved by the owning database.
    /// Definition/ownership refusals precede allocator acquisition.
    pub fn execute_graph_write_chain_then_read_engine_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        chain: &fgdb_gql::PreparedGraphWriteChain,
        continuation: &fgdb_gql::PreparedGraphSet,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
    ) -> WriteChainReadResult<WriteTxnError> {
        self.ensure_database(database)
            .map_err(write_read_source_error)?;
        admit_write_read(chain, continuation)?;
        let allocate = database
            .engine_allocator(cx)
            .map_err(write_read_source_error)?;
        self.execute_graph_write_chain_then_read_governed(
            database,
            cx,
            chain,
            continuation,
            policy,
            allocate,
        )
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Autocommit read-after-write composition. No receipt or result rows
    /// escape before ordinary transaction completion. Completion errors retain
    /// their original unknown/durable-needs-recovery cause, never claim abort.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub async fn execute_graph_write_chain_then_read_autocommit_governed<A>(
        &mut self,
        txcx: &TxnCx,
        cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        chain: &fgdb_gql::PreparedGraphWriteChain,
        continuation: &fgdb_gql::PreparedGraphSet,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> Result<
        (
            fgdb_gql::GraphWriteProgramReceipt,
            fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
            EmbeddedTxnCompletion,
        ),
        TxnGqlError<fgdb_gql::GraphWriteChainError<WriteTxnError, A>>,
    > {
        admit_write_read(chain, continuation)?;
        let mut transaction = self
            .begin(txcx)
            .map_err(|error| write_read_source_error(WriteTxnError::Write(error)))?;
        let (receipt, rows) = match transaction.execute_graph_write_chain_then_read_governed(
            self,
            cx,
            chain,
            continuation,
            policy,
            allocate,
        ) {
            Ok(result) => result,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction
            .finish(self, commit_cx)
            .await
            .map_err(write_read_source_error)?;
        Ok((receipt, rows, completion))
    }

    /// Autocommit counterpart using the database's identity allocator.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub async fn execute_graph_write_chain_then_read_autocommit_engine_governed(
        &mut self,
        txcx: &TxnCx,
        cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        chain: &fgdb_gql::PreparedGraphWriteChain,
        continuation: &fgdb_gql::PreparedGraphSet,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
    ) -> Result<
        (
            fgdb_gql::GraphWriteProgramReceipt,
            fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
            EmbeddedTxnCompletion,
        ),
        TxnGqlError<fgdb_gql::GraphWriteChainError<WriteTxnError, WriteTxnError>>,
    > {
        admit_write_read(chain, continuation)?;
        let mut transaction = self
            .begin(txcx)
            .map_err(|error| write_read_source_error(WriteTxnError::Write(error)))?;
        let (receipt, rows) = match transaction.execute_graph_write_chain_then_read_engine_governed(
            self,
            cx,
            chain,
            continuation,
            policy,
        ) {
            Ok(result) => result,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction
            .finish(self, commit_cx)
            .await
            .map_err(write_read_source_error)?;
        Ok((receipt, rows, completion))
    }
}
