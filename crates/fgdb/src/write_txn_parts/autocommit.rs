// Autocommit is composition of the existing begin -> stage -> finish lifecycle.
// It introduces no writer, commit protocol, rollback mechanism or error domain.

impl<V: Vfs + Clone> Database<V> {
    /// Execute CREATE/INSERT RETURN and publish its writes through one ordinary
    /// transaction completion. The complete RETURN result is evaluated and
    /// admitted before staging or commit, and escapes only after finish succeeds.
    /// An empty result page can accompany WriteCommitted: RETURN pagination does
    /// not change the creation effects. Empty creation input closes read-only.
    pub async fn execute_graph_insert_query_autocommit_governed<A>(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        query: &fgdb_gql::PreparedGraphInsertQuery,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> Result<
        (
            fgdb_gql::insertion::GraphInsertStats,
            fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
            EmbeddedTxnCompletion,
        ),
        TxnGqlError<fgdb_gql::GraphInsertQueryError<WriteTxnError, A>>,
    > {
        use fgdb_gql::insertion::GraphInsertError;
        use fgdb_gql::{GqlQueryError, GraphInsertQueryError};
        let infrastructure = |error| {
            GqlQueryError::Source(GraphInsertQueryError::Insertion(GraphInsertError::Source(
                error,
            )))
        };
        let mut transaction = self
            .begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let (stats, returning) = match transaction
            .execute_graph_insert_query_governed(self, query_cx, query, policy, allocate)
        {
            Ok(result) => result,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction
            .finish(self, commit_cx)
            .await
            .map_err(infrastructure)?;
        Ok((stats, returning, completion))
    }

    /// CREATE/INSERT RETURN autocommit using the owning database's identity
    /// reservations. Returned rows are released only after the ordinary commit
    /// or read-close completion; a failed RETURN cannot publish any creations.
    pub async fn execute_graph_insert_query_autocommit_engine_governed(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        query: &fgdb_gql::PreparedGraphInsertQuery,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
    ) -> Result<
        (
            fgdb_gql::insertion::GraphInsertStats,
            fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
            EmbeddedTxnCompletion,
        ),
        TxnGqlError<fgdb_gql::GraphInsertQueryError<WriteTxnError, WriteTxnError>>,
    > {
        use fgdb_gql::insertion::GraphInsertError;
        use fgdb_gql::{GqlQueryError, GraphInsertQueryError};
        let infrastructure = |error| {
            GqlQueryError::Source(GraphInsertQueryError::Insertion(GraphInsertError::Source(
                error,
            )))
        };
        let mut transaction = self
            .begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let (stats, returning) = match transaction
            .execute_graph_insert_query_engine_governed(self, query_cx, query, policy)
        {
            Ok(result) => result,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction
            .finish(self, commit_cx)
            .await
            .map_err(infrastructure)?;
        Ok((stats, returning, completion))
    }

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
        fgdb_gql::GqlQueryError<
            fgdb_gql::GraphMutationError<WriteTxnError>,
            Box<asupersync::error::Error>,
        >,
    > {
        use fgdb_gql::{GqlQueryError, GraphMutationError};
        let infrastructure = |error| GqlQueryError::Source(GraphMutationError::Source(error));
        let mut transaction = self
            .begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let stats =
            match transaction.execute_graph_mutation_governed(self, query_cx, mutation, policy) {
                Ok(stats) => stats,
                Err(error) => {
                    transaction.abort();
                    return Err(error);
                }
            };
        let completion = transaction
            .finish(self, commit_cx)
            .await
            .map_err(infrastructure)?;
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
        (
            fgdb_gql::GraphMutationStats,
            Vec<VId>,
            Vec<EId>,
            EmbeddedTxnCompletion,
        ),
        fgdb_gql::GqlQueryError<
            fgdb_gql::GraphMutationError<WriteTxnError>,
            Box<asupersync::error::Error>,
        >,
    > {
        use fgdb_gql::{GqlQueryError, GraphMutationError};
        let infrastructure = |error| GqlQueryError::Source(GraphMutationError::Source(error));
        let mut transaction = self
            .begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let (stats, targets, edges) = match transaction
            .execute_graph_mutation_returning_governed(self, query_cx, mutation, policy)
        {
            Ok(result) => result,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction
            .finish(self, commit_cx)
            .await
            .map_err(infrastructure)?;
        Ok((stats, targets, edges, completion))
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
        let mut transaction = self
            .begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let stats = match transaction
            .execute_graph_insert_governed(self, query_cx, insertion, policy, allocate)
        {
            Ok(stats) => stats,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction
            .finish(self, commit_cx)
            .await
            .map_err(infrastructure)?;
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
        (
            fgdb_gql::insertion::GraphInsertStats,
            Vec<VId>,
            Vec<EId>,
            EmbeddedTxnCompletion,
        ),
        fgdb_gql::GqlQueryError<
            fgdb_gql::insertion::GraphInsertError<WriteTxnError, A>,
            Box<asupersync::error::Error>,
        >,
    > {
        use fgdb_gql::GqlQueryError;
        use fgdb_gql::insertion::GraphInsertError;
        let infrastructure = |error| GqlQueryError::Source(GraphInsertError::Source(error));
        let mut transaction = self
            .begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let (stats, vertices, edges) = match transaction
            .execute_graph_insert_returning_governed(self, query_cx, insertion, policy, allocate)
        {
            Ok(result) => result,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction
            .finish(self, commit_cx)
            .await
            .map_err(infrastructure)?;
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
        let infrastructure =
            |error| GraphWriteProgramError::Program(GraphMutationProgramError::Preflight(error));
        let mut transaction = self
            .begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let stats = match transaction
            .execute_graph_write_program_governed(self, query_cx, program, policy, allocate)
        {
            Ok(stats) => stats,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction
            .finish(self, commit_cx)
            .await
            .map_err(infrastructure)?;
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
        let infrastructure =
            |error| GraphWriteProgramError::Program(GraphMutationProgramError::Preflight(error));
        let mut transaction = self
            .begin(txcx)
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
        let completion = transaction
            .finish(self, commit_cx)
            .await
            .map_err(infrastructure)?;
        Ok((receipt, completion))
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Reserve a fresh engine identity: the keyed permutation of the next
    /// counter of its kind ([`IdentityPermutation`], fgdb-hxgm1 channel 2),
    /// skipping any identity the writer already holds. Reservations are shared
    /// by staged transactions on this opened handle. Every engine commit
    /// records the counters in its marker, so after reopen no identity this
    /// handle issued before that commit is reissued, deleted or not.
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
    ) -> Result<
        impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, WriteTxnError> + 'a,
        WriteTxnError,
    > {
        cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
        self.ensure_readable()?;
        // Weak handles on the committed identities: the existence check that
        // keeps an engine identity off one a client chose explicitly. Weak,
        // not Arc, because an autocommit commits while its allocator is still
        // in scope: the fold then mutates the sets in place instead of copying
        // them. A handle that outlived that commit no longer upgrades and
        // skips the check; the commit-time spent check still refuses any
        // collision (AlreadyLive / IdentitySpent), so nothing is lost but
        // the skip.
        let spent_vertices = std::sync::Arc::downgrade(&self.writer.spent_vertices());
        let spent_edges = std::sync::Arc::downgrade(&self.writer.spent_edges());
        let allocation = self.identity_allocation.clone();
        Ok(move |request| {
            cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
            let mut state = allocation
                .lock()
                .map_err(|_| WriteTxnError::IdentityExhausted)?;
            Ok(match request {
                fgdb_gql::insertion::GraphInsertRequest::Vertex { .. } => {
                    ElementId::Vertex(VId(state.issue(true, |id| {
                        spent_vertices
                            .upgrade()
                            .is_some_and(|spent| spent.contains(&VId(id)))
                    })?))
                }
                fgdb_gql::insertion::GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(state
                    .issue(false, |id| {
                        spent_edges
                            .upgrade()
                            .is_some_and(|spent| spent.contains(&EId(id)))
                    })?)),
            })
        })
    }

    /// Create graph structures with engine-owned identities and complete the
    /// private transaction in one call; no caller allocator is accepted. The
    /// identity reservation shares the explicit-transaction allocator: an
    /// aborted autocommit never reissues or reclaims its issued identities on
    /// this handle, and reopen resumes from the counters the last engine
    /// commit recorded (fgdb-hxgm1 channel 2).
    pub async fn execute_graph_insert_autocommit_engine_governed(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        insertion: &fgdb_gql::insertion::PreparedGraphInsert,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
    ) -> Result<
        (fgdb_gql::insertion::GraphInsertStats, EmbeddedTxnCompletion),
        fgdb_gql::GqlQueryError<
            fgdb_gql::insertion::GraphInsertError<WriteTxnError, WriteTxnError>,
            Box<asupersync::error::Error>,
        >,
    > {
        use fgdb_gql::{GqlQueryError, insertion::GraphInsertError};
        let infrastructure = |error| GqlQueryError::Source(GraphInsertError::Source(error));
        let mut transaction = self
            .begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let stats = match transaction
            .execute_graph_insert_engine_governed(self, query_cx, insertion, policy)
        {
            Ok(stats) => stats,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction
            .finish(self, commit_cx)
            .await
            .map_err(infrastructure)?;
        Ok((stats, completion))
    }

    /// Autocommit CREATE with engine-issued identities returned after the
    /// completion future resolves; no caller allocator closure is involved.
    pub async fn execute_graph_insert_returning_autocommit_engine_governed(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        insertion: &fgdb_gql::insertion::PreparedGraphInsert,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
    ) -> Result<
        (
            fgdb_gql::insertion::GraphInsertStats,
            Vec<VId>,
            Vec<EId>,
            EmbeddedTxnCompletion,
        ),
        fgdb_gql::GqlQueryError<
            fgdb_gql::insertion::GraphInsertError<WriteTxnError, WriteTxnError>,
            Box<asupersync::error::Error>,
        >,
    > {
        use fgdb_gql::{GqlQueryError, insertion::GraphInsertError};
        let infrastructure = |error| GqlQueryError::Source(GraphInsertError::Source(error));
        let mut transaction = self
            .begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let (stats, vertices, edges) = match transaction
            .execute_graph_insert_returning_engine_governed(self, query_cx, insertion, policy)
        {
            Ok(result) => result,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction
            .finish(self, commit_cx)
            .await
            .map_err(infrastructure)?;
        Ok((stats, vertices, edges, completion))
    }

    /// Mixed autocommit program with database-owned identity reservations.
    pub async fn execute_graph_write_program_autocommit_engine_governed(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        program: &fgdb_gql::PreparedGraphWriteProgram,
        policy: fgdb_gql::GraphWriteProgramPolicy,
    ) -> Result<
        (fgdb_gql::GraphWriteProgramStats, EmbeddedTxnCompletion),
        fgdb_gql::GraphWriteProgramError<
            WriteTxnError,
            WriteTxnError,
            Box<asupersync::error::Error>,
        >,
    > {
        use fgdb_gql::{GraphMutationProgramError, GraphWriteProgramError};
        let infrastructure =
            |error| GraphWriteProgramError::Program(GraphMutationProgramError::Preflight(error));
        let mut transaction = self
            .begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let stats = match transaction
            .execute_graph_write_program_engine_governed(self, query_cx, program, policy)
        {
            Ok(stats) => stats,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction
            .finish(self, commit_cx)
            .await
            .map_err(infrastructure)?;
        Ok((stats, completion))
    }

    /// Mixed autocommit program receipt with engine-issued identities; the
    /// receipt escapes only after the outer completion succeeds.
    pub async fn execute_graph_write_program_returning_autocommit_engine_governed(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        program: &fgdb_gql::PreparedGraphWriteProgram,
        policy: fgdb_gql::GraphWriteProgramPolicy,
    ) -> Result<
        (fgdb_gql::GraphWriteProgramReceipt, EmbeddedTxnCompletion),
        fgdb_gql::GraphWriteProgramError<
            WriteTxnError,
            WriteTxnError,
            Box<asupersync::error::Error>,
        >,
    > {
        use fgdb_gql::{GraphMutationProgramError, GraphWriteProgramError};
        let infrastructure =
            |error| GraphWriteProgramError::Program(GraphMutationProgramError::Preflight(error));
        let mut transaction = self
            .begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let receipt = match transaction
            .execute_graph_write_program_returning_engine_governed(self, query_cx, program, policy)
        {
            Ok(receipt) => receipt,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction
            .finish(self, commit_cx)
            .await
            .map_err(infrastructure)?;
        Ok((receipt, completion))
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
            return Err(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live,
            });
        }
        database.allocate_identity(cx, request)
    }
}
