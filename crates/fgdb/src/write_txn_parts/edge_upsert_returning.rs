// RETURN is part of the edge upsert's acceptance, not a post-commit query.
// Reuse the exact chosen EId, canonical staged properties and rollback owner.

type EdgeUpsertQueryResult<A> = Result<
    (
        fgdb_gql::GraphEdgeUpsertStats,
        fgdb_gql::GraphEdgeMergeOutcome,
        fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
    ),
    TxnGqlError<fgdb_gql::GraphEdgeUpsertQueryError<WriteTxnError, A>>,
>;

fn edge_upsert_query_selection_policy(
    mut policy: fgdb_gql::GraphEdgeUpsertPolicy,
) -> fgdb_gql::GraphEdgeUpsertPolicy {
    // MATCH occurrences and the internal MERGE receipt are private inputs.
    // Only RETURN's final page consumes the caller's result-row allowance.
    policy.merge.query.rows = policy.merge.query.rows.max_snapshot_records().map_or(
        fgdb_gql::GqlExecutionBudget::UNLIMITED,
        fgdb_gql::GqlExecutionBudget::snapshot_records,
    );
    policy
}

// Shared by the ordinary and capability-masked hosts. A host must supply
// point reads under its original authority; this helper never owns a graph.
fn edge_upsert_returning<E, A, C>(
    query: &fgdb_gql::PreparedGraphEdgeUpsertQuery,
    stats: fgdb_gql::GraphEdgeUpsertStats,
    outcome: fgdb_gql::GraphEdgeMergeOutcome,
    policy: fgdb_gql::GraphEdgeUpsertPolicy,
    mut checkpoint: impl FnMut() -> Result<(), C>,
    mut property: impl FnMut(
        EId,
        fgdb_delta_types::PropertyKeyId,
        &mut dyn FnMut(fgdb_gql::GlaExecutionEvent) -> EdgeUpsertActionResult<(), E, A, C>,
    ) -> EdgeUpsertActionResult<CanonicalScalar, E, A, C>,
    elements: impl FnMut(
        ElementId,
        fgdb_delta_types::PropertyKeyId,
    ) -> Result<Option<fgdb_types::CanonicalPropertyValue>, E>,
) -> Result<
    fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
    fgdb_gql::GqlQueryError<fgdb_gql::GraphEdgeUpsertQueryError<E, A>, C>,
> {
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::{
        GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GqlQueryError,
        GraphEdgeReturnBinding, GraphEdgeUpsertQueryError,
    };
    // Edge-upsert stats have their own cumulative evaluator. merge.evaluator
    // alone would forget every ON/SET expression and reset the work budget.
    let mut evaluator = stats.evaluator;
    let row = (|| -> EdgeUpsertActionResult<Option<Vec<GraphValue>>, E, A, C> {
        let mut control = |event| {
            checkpoint().map_err(GqlQueryError::Interrupted)?;
            let work = u128::from(evaluator.work_units) + 1;
            let scratch = u128::from(evaluator.scratch_entries)
                + u128::from(event == GlaExecutionEvent::ScratchEntry);
            for (observed, limit, dimension) in [
                (
                    work,
                    policy.merge.query.evaluator.max_work_units,
                    GlaLimitDimension::WorkUnits,
                ),
                (
                    scratch,
                    policy.merge.query.evaluator.max_scratch_entries,
                    GlaLimitDimension::ScratchEntries,
                ),
            ] {
                if observed > u128::from(limit) {
                    return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                        dimension,
                        limit,
                        observed,
                    }));
                }
            }
            evaluator.work_units = work as u64;
            evaluator.scratch_entries = scratch as u64;
            Ok(())
        };
        control(GlaExecutionEvent::Work)?;
        let Some(edge) = outcome.edge() else {
            return Ok(None);
        };
        // Admit the complete private input before its allocation, even when
        // LIMIT 0 will discard every output row.
        for _ in query.bindings() {
            control(GlaExecutionEvent::ScratchEntry)?;
        }
        let mut row = Vec::with_capacity(query.bindings().len());
        for binding in query.bindings() {
            control(GlaExecutionEvent::Work)?;
            row.push(match binding {
                GraphEdgeReturnBinding::Edge => GraphValue::Edge(edge),
                GraphEdgeReturnBinding::Property(key) => {
                    GraphValue::Scalar(property(edge, *key, &mut control)?)
                }
            });
        }
        Ok(Some(row))
    })()
    .map_err(|error| error.map_source(GraphEdgeUpsertQueryError::Upsert))?;
    query
        .project_governed_reading(
            row,
            stats.merge.match_selection,
            evaluator,
            policy.merge.query,
            elements,
            checkpoint,
        )
        .map_err(|error| error.map_source(GraphEdgeUpsertQueryError::Returning))
}

impl WriteTxn {
    /// Execute unique relationship MERGE, ON/SET clauses and RETURN atomically.
    /// RETURN reads the selected relationship's final staged fields, not a new
    /// MATCH. A missing property is NULL; NoInput produces no occurrence.
    ///
    /// Every source, creation, action, work and scratch ceiling still applies.
    /// The result-row ceiling governs only the final page, so LIMIT 0 executes
    /// effects and validates fields. Errors and unwinding restore the prior
    /// workspace while retaining read/absence witnesses and allocated IDs.
    /// The caller owns completion; this API never publishes effects itself.
    pub fn execute_graph_edge_upsert_query_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGraphEdgeUpsertQuery,
        policy: fgdb_gql::GraphEdgeUpsertPolicy,
        allocate: impl FnMut(fgdb_gql::GraphEdgeMergeRequest) -> Result<ElementId, A>,
    ) -> EdgeUpsertQueryResult<A> {
        use fgdb_gql::{GqlQueryError, GraphEdgeUpsertQueryError};
        let workspace = MutationProgramWorkspace::new(self);
        let (stats, outcome) = workspace
            .txn
            .execute_graph_edge_upsert_governed(
                database,
                cx,
                query.upsert(),
                edge_upsert_query_selection_policy(policy),
                allocate,
            )
            .map_err(|error| error.map_source(GraphEdgeUpsertQueryError::Upsert))?;
        let rows = cx.with_restriction(|| {
            let failure = std::cell::RefCell::new(None);
            let result = edge_upsert_returning(
                query,
                stats,
                outcome,
                policy,
                || match failure.borrow_mut().take() {
                    Some(error) => Err(error),
                    None => cx.checkpoint(),
                },
                |edge, key, control| {
                    edge_upsert_property(&*workspace.txn, &*database, edge, key, control)
                },
                |element, key| {
                    workspace
                        .txn
                        .element_property_with_checkpoint(database, cx, &failure, element, key)
                },
            );
            match failure.into_inner() {
                Some(error) => Err(GqlQueryError::Interrupted(error)),
                None => result,
            }
        })?;
        workspace.accept();
        Ok((stats, outcome, rows))
    }

    /// As above, reserving the relationship identity through the database.
    pub fn execute_graph_edge_upsert_query_engine_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGraphEdgeUpsertQuery,
        policy: fgdb_gql::GraphEdgeUpsertPolicy,
    ) -> EdgeUpsertQueryResult<WriteTxnError> {
        use fgdb_gql::{GqlQueryError, GraphEdgeUpsertError, GraphEdgeUpsertQueryError};
        let source = |error| {
            GqlQueryError::Source(GraphEdgeUpsertQueryError::Upsert(
                GraphEdgeUpsertError::Staging(error),
            ))
        };
        self.ensure_database(database).map_err(source)?;
        let mut allocate = database.engine_allocator(cx).map_err(source)?;
        self.execute_graph_edge_upsert_query_governed(database, cx, query, policy, move |_| {
            allocate(fgdb_gql::insertion::GraphInsertRequest::Edge { row: 0, edge: 0 })
        })
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Accept all RETURN rows before the sole commit. No fallible result read
    /// runs after publication; commit uncertainty retains its native error.
    pub async fn execute_graph_edge_upsert_query_autocommit_governed<A>(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        query: &fgdb_gql::PreparedGraphEdgeUpsertQuery,
        policy: fgdb_gql::GraphEdgeUpsertPolicy,
        allocate: impl FnMut(fgdb_gql::GraphEdgeMergeRequest) -> Result<ElementId, A>,
    ) -> Result<
        (
            fgdb_gql::GraphEdgeUpsertStats,
            fgdb_gql::GraphEdgeMergeOutcome,
            fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
            EmbeddedTxnCompletion,
        ),
        TxnGqlError<fgdb_gql::GraphEdgeUpsertQueryError<WriteTxnError, A>>,
    > {
        use fgdb_gql::{GqlQueryError, GraphEdgeUpsertError, GraphEdgeUpsertQueryError};
        let source = |error| {
            GqlQueryError::Source(GraphEdgeUpsertQueryError::Upsert(
                GraphEdgeUpsertError::Staging(error),
            ))
        };
        let mut transaction = self
            .begin(txn_cx)
            .map_err(|error| source(WriteTxnError::Write(error)))?;
        let (stats, outcome, rows) = match transaction
            .execute_graph_edge_upsert_query_governed(self, query_cx, query, policy, allocate)
        {
            Ok(result) => result,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction.finish(self, commit_cx).await.map_err(source)?;
        Ok((stats, outcome, rows, completion))
    }
}
