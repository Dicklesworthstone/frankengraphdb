// Autocommit is composition of the existing begin -> stage -> finish lifecycle.
// It introduces no writer, commit protocol, rollback mechanism or error domain.

impl<V: Vfs + Clone> Database<V> {
    /// Execute one prepared mutation as an embedded autocommit operation. A
    /// successful staging step is immediately validated and completed through
    /// `WriteTxn::finish`; a zero-match statement closes read-only without
    /// inventing a marker. Execution refusal explicitly aborts the private
    /// transaction so its snapshot obligation is released before return.
    pub async fn execute_graph_mutation_autocommit_governed(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        mutation: &fgdb_gql::PreparedGraphMutation,
        policy: fgdb_gql::GraphMutationPolicy,
    ) -> Result<
        (fgdb_gql::GraphMutationStats, EmbeddedTxnCompletion),
        fgdb_gql::GqlQueryError<fgdb_gql::GraphMutationError<WriteTxnError>, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::{GqlQueryError, GraphMutationError};
        let infrastructure = |error| GqlQueryError::Source(GraphMutationError::Source(error));
        let mut transaction = self.begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let stats = match transaction.execute_graph_mutation_governed(
            self, query_cx, mutation, policy,
        ) {
            Ok(stats) => stats,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction.finish(self, commit_cx).await.map_err(infrastructure)?;
        Ok((stats, completion))
    }

    /// Autocommit mutation plus the same staged target receipt as the explicit
    /// transaction API. The receipt escapes only after finish succeeds; on a
    /// read-close it is necessarily empty. A WriteCommitted outcome is the
    /// durability acknowledgment, not the receipt itself.
    pub async fn execute_graph_mutation_returning_autocommit_governed(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        mutation: &fgdb_gql::PreparedGraphMutation,
        policy: fgdb_gql::GraphMutationPolicy,
    ) -> Result<
        (fgdb_gql::GraphMutationStats, Vec<VId>, EmbeddedTxnCompletion),
        fgdb_gql::GqlQueryError<fgdb_gql::GraphMutationError<WriteTxnError>, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::{GqlQueryError, GraphMutationError};
        let infrastructure = |error| GqlQueryError::Source(GraphMutationError::Source(error));
        let mut transaction = self.begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let (stats, targets) = match transaction.execute_graph_mutation_returning_governed(
            self, query_cx, mutation, policy,
        ) {
            Ok(result) => result,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction.finish(self, commit_cx).await.map_err(infrastructure)?;
        Ok((stats, targets, completion))
    }

    /// Create graph structures and complete the private transaction in one call.
    /// Identity allocation remains externally owned: an allocator refusal aborts
    /// the transaction, while already-issued identities are never reclaimed or
    /// licensed for reuse. A zero-row MATCH may finish as ReadClosed.
    pub async fn execute_graph_insert_autocommit_governed<A>(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        insertion: &fgdb_gql::insertion::PreparedGraphInsert,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> Result<
        (fgdb_gql::insertion::GraphInsertStats, EmbeddedTxnCompletion),
        fgdb_gql::GqlQueryError<
            fgdb_gql::insertion::GraphInsertError<WriteTxnError, A>,
            Box<asupersync::error::Error>,
        >,
    > {
        use fgdb_gql::GqlQueryError;
        use fgdb_gql::insertion::GraphInsertError;
        let infrastructure = |error| GqlQueryError::Source(GraphInsertError::Source(error));
        let mut transaction = self.begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let stats = match transaction.execute_graph_insert_governed(
            self, query_cx, insertion, policy, allocate,
        ) {
            Ok(stats) => stats,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction.finish(self, commit_cx).await.map_err(infrastructure)?;
        Ok((stats, completion))
    }

    /// Autocommit CREATE plus the exact created identities. Returning happens
    /// only after completion succeeds, so WriteCommitted means those IDs are now
    /// durable on this handle; ReadClosed can only accompany empty identity bags.
    pub async fn execute_graph_insert_returning_autocommit_governed<A>(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        insertion: &fgdb_gql::insertion::PreparedGraphInsert,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> Result<
        (fgdb_gql::insertion::GraphInsertStats, Vec<VId>, Vec<EId>, EmbeddedTxnCompletion),
        fgdb_gql::GqlQueryError<
            fgdb_gql::insertion::GraphInsertError<WriteTxnError, A>,
            Box<asupersync::error::Error>,
        >,
    > {
        use fgdb_gql::GqlQueryError;
        use fgdb_gql::insertion::GraphInsertError;
        let infrastructure = |error| GqlQueryError::Source(GraphInsertError::Source(error));
        let mut transaction = self.begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let (stats, vertices, edges) = match transaction.execute_graph_insert_returning_governed(
            self, query_cx, insertion, policy, allocate,
        ) {
            Ok(result) => result,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction.finish(self, commit_cx).await.map_err(infrastructure)?;
        Ok((stats, vertices, edges, completion))
    }

    /// Execute an ordered CREATE/update/delete program and publish it through one
    /// transaction completion. Every program step still observes predecessors in
    /// the canonical overlay and the existing whole-program rollback guard owns
    /// execution failure. The autocommit wrapper only owns the outer lifecycle.
    pub async fn execute_graph_write_program_autocommit_governed<A>(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        program: &fgdb_gql::PreparedGraphWriteProgram,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        allocate: impl FnMut(fgdb_gql::GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<
        (fgdb_gql::GraphWriteProgramStats, EmbeddedTxnCompletion),
        fgdb_gql::GraphWriteProgramError<WriteTxnError, A, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::{GraphMutationProgramError, GraphWriteProgramError};
        let infrastructure = |error| {
            GraphWriteProgramError::Program(GraphMutationProgramError::Preflight(error))
        };
        let mut transaction = self.begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let stats = match transaction.execute_graph_write_program_governed(
            self, query_cx, program, policy, allocate,
        ) {
            Ok(stats) => stats,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction.finish(self, commit_cx).await.map_err(infrastructure)?;
        Ok((stats, completion))
    }

    /// Mixed autocommit plus the per-step receipt. The receipt is constructed by
    /// the program adapter only after its final acceptance boundary, then this
    /// method withholds it again until outer transaction completion succeeds.
    pub async fn execute_graph_write_program_returning_autocommit_governed<A>(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        program: &fgdb_gql::PreparedGraphWriteProgram,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        allocate: impl FnMut(fgdb_gql::GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<
        (fgdb_gql::GraphWriteProgramReceipt, EmbeddedTxnCompletion),
        fgdb_gql::GraphWriteProgramError<WriteTxnError, A, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::{GraphMutationProgramError, GraphWriteProgramError};
        let infrastructure = |error| {
            GraphWriteProgramError::Program(GraphMutationProgramError::Preflight(error))
        };
        let mut transaction = self.begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let receipt = match transaction.execute_graph_write_program_returning_governed(
            self, query_cx, program, policy, allocate,
        ) {
            Ok(receipt) => receipt,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction.finish(self, commit_cx).await.map_err(infrastructure)?;
        Ok((receipt, completion))
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Reserve an identity above every committed creation of its kind.
    /// Reservations are shared by staged transactions on this opened handle.
    /// Reopen reconstructs the floor from Chronicle, including deleted IDs;
    /// never-committed reservations are not durable leases.
    pub fn allocate_identity(
        &mut self,
        cx: &fgdb_types::QueryCx,
        request: fgdb_gql::insertion::GraphInsertRequest,
    ) -> Result<ElementId, WriteTxnError> {
        self.engine_allocator(cx)?(request)
    }

    fn engine_allocator<'a>(
        &mut self,
        cx: &'a fgdb_types::QueryCx,
    ) -> Result<impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, WriteTxnError> + 'a, WriteTxnError> {
        cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
        let index = self.delta_index()?;
        let mut state = self.identity_allocation.lock().map_err(|_| WriteTxnError::IdentityExhausted)?;
        for batch in index.since(state.frontier).map_err(crate::read_error_from_index)? {
            for coordinate in batch.coordinate_entries() {
                for row in &coordinate.rows {
                    match row {
                        fgdb_delta_types::DeltaRow::CreateVertex { vid, .. } => state.vertex = state.vertex.max(vid.0),
                        fgdb_delta_types::DeltaRow::CreateEdge { eid, .. } => state.edge = state.edge.max(eid.0),
                        _ => {}
                    }
                }
            }
        }
        state.frontier = index.frontier();
        drop(state);
        let allocation = self.identity_allocation.clone();
        Ok(move |request| {
            cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
            let mut state = allocation.lock().map_err(|_| WriteTxnError::IdentityExhausted)?;
            let vertex = matches!(request, fgdb_gql::insertion::GraphInsertRequest::Vertex { .. });
            let high = if vertex { &mut state.vertex } else { &mut state.edge };
            *high = high.checked_add(1).ok_or(WriteTxnError::IdentityExhausted)?;
            Ok(if vertex { ElementId::Vertex(VId(*high)) } else { ElementId::Edge(EId(*high)) })
        })
    }
}

impl WriteTxn {
    /// Reserve from this transaction's owning database without a caller allocator.
    pub fn allocate_identity<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        request: fgdb_gql::insertion::GraphInsertRequest,
    ) -> Result<ElementId, WriteTxnError> {
        self.ensure_database(database)?;
        let live = database.frontier()?;
        if live != self.basis {
            return Err(WriteTxnError::SnapshotAdvanced { pinned: self.basis, live });
        }
        database.allocate_identity(cx, request)
    }
}
