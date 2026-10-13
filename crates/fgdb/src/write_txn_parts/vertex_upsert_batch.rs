// All-target vertex MERGE uses the existing matcher, insertion collector,
// point readers, SET bytecode and statement rollback workspace.

struct VertexMergeTargets {
    matched: std::collections::BTreeSet<VId>,
    created: Option<VId>,
}
impl VertexMergeOutput for VertexMergeTargets {
    const UNIQUE: bool = false;

    fn matched(matched: std::collections::BTreeSet<VId>) -> Self {
        Self { matched, created: None }
    }

    fn created(vertex: VId) -> Self {
        Self { matched: std::collections::BTreeSet::new(), created: Some(vertex) }
    }
}
impl VertexMergeTargets {
    fn len(&self) -> usize {
        self.matched.len() + usize::from(self.created.is_some())
    }

    fn outcomes(&self) -> impl Iterator<Item = fgdb_gql::GraphVertexMergeOutcome> + '_ {
        self.matched.iter().copied().map(fgdb_gql::GraphVertexMergeOutcome::Matched)
            .chain(self.created.map(fgdb_gql::GraphVertexMergeOutcome::Created))
    }
}

type VertexUpsertBatchQueryResult<A> = Result<
    (
        fgdb_gql::GraphVertexUpsertStats,
        fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
    ),
    TxnGqlError<fgdb_gql::GraphVertexUpsertError<WriteTxnError, A>>,
>;

fn vertex_upsert_batch_event<A>(
    cx: &fgdb_types::QueryCx,
    policy: fgdb_gql::GqlQueryPolicy,
    evaluator: &mut fgdb_gql::GlaExecutionStats,
    event: fgdb_gql::GlaExecutionEvent,
) -> VertexUpsertActionResult<(), WriteTxnError, A, Box<asupersync::error::Error>> {
    use fgdb_gql::{GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GqlQueryError};
    cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
    let work = u128::from(evaluator.work_units) + 1;
    let scratch = u128::from(evaluator.scratch_entries)
        + u128::from(event == GlaExecutionEvent::ScratchEntry);
    for (observed, limit, dimension) in [
        (work, policy.evaluator.max_work_units, GlaLimitDimension::WorkUnits),
        (scratch, policy.evaluator.max_scratch_entries, GlaLimitDimension::ScratchEntries),
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
}

impl WriteTxn {
    /// Execute all distinct targets of a vertex MERGE and return their final
    /// state through one native RETURN/WITH pipeline. The MATCH runs once;
    /// all existing matching vertices are updated in full identity order.
    /// Only a completely empty match creates the one definition-owned vertex.
    /// No target is re-matched after an earlier ON clause changes its key.
    ///
    /// The existing singular and capability-scoped entrypoints remain unique.
    /// This method does not introduce MATCH/WITH-fed MERGE or chained writes.
    /// All targets share one source/work/scratch/action/creation allowance;
    /// only the final RETURN rows spend the output allowance. Grouping, pages
    /// and a WITH filter never limit which targets receive actions.
    ///
    /// One workspace covers creation, every ON/SET clause, final property/map/
    /// label reads and RETURN. An error or unwind restores the previous staged
    /// prefix, retaining conflict witnesses and spent identities. Completion
    /// remains the caller's responsibility.
    pub fn execute_graph_vertex_upserts_query_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGraphVertexUpsertQuery,
        policy: fgdb_gql::GraphVertexUpsertPolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> VertexUpsertBatchQueryResult<A> {
        use fgdb_gql::algebra::GraphValue;
        use fgdb_gql::{
            GlaExecutionEvent, GqlQueryError, GraphVertexReturnBinding,
            GraphVertexUpsertBranch, GraphVertexUpsertError, GraphVertexUpsertStats,
        };
        let infrastructure = |error| GqlQueryError::Source(GraphVertexUpsertError::Staging(error));
        self.ensure_database(database).map_err(infrastructure)?;
        let live = database.frontier().map_err(WriteTxnError::from).map_err(infrastructure)?;
        if live != self.basis {
            return Err(infrastructure(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live,
            }));
        }
        let upsert = query.upsert();
        if let Some(first) = self.staged.first()
            && !self.program_multi_relation
            && self.staged.iter().all(|batch| batch.relation == first.relation)
            && upsert.merge().relation() != first.relation
        {
            return Err(infrastructure(WriteTxnError::RelationMismatch {
                expected: first.relation,
                found: upsert.merge().relation(),
            }));
        }
        cx.with_restriction(|| {
            cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
            let workspace = MutationProgramWorkspace::new(self);
            let proposal = collect_vertex_merge_output::<VertexMergeTargets, WriteTxnError, A, _>(
                upsert.merge(),
                vertex_upsert_query_selection_policy(policy).merge,
                |pattern, allowance| {
                    workspace.txn.execute_graph_pattern_governed(database, cx, pattern, allowance)
                },
                allocate,
                || cx.checkpoint(),
            ).map_err(|error| error.map_source(GraphVertexUpsertError::Merge))?;
            let targets = proposal.outcome;
            let (branch, actions) = if targets.created.is_some() {
                (GraphVertexUpsertBranch::Create, upsert.on_create())
            } else {
                (GraphVertexUpsertBranch::Match, upsert.on_match())
            };
            // All targets take the same branch. Admit the WHOLE action count
            // before staging the first one, without refreshing it per target.
            let observed = (targets.len() as u128)
                .checked_mul(actions.len() as u128 + upsert.after().len() as u128)
                .ok_or_else(|| GqlQueryError::Source(GraphVertexUpsertError::Merge(
                    fgdb_gql::GraphVertexMergeError::AccountingOverflow,
                )))?;
            if observed > u128::from(policy.max_actions) {
                return Err(GqlQueryError::Source(GraphVertexUpsertError::ActionLimit {
                    limit: policy.max_actions,
                    observed,
                }));
            }
            if let Some(creation) = proposal.creation {
                stage_vertex_merge_creation::<V, A>(
                    workspace.txn, database, cx, upsert.merge().relation(), creation,
                ).map_err(|error| error.map_source(GraphVertexUpsertError::Merge))?;
            }
            let mut stats = GraphVertexUpsertStats {
                merge: proposal.stats,
                branch,
                action_effects: observed as u64,
            };
            for outcome in targets.outcomes() {
                vertex_upsert_batch_event::<A>(
                    cx, policy.merge.query, &mut stats.merge.evaluator, GlaExecutionEvent::Work,
                )?;
                // No native field borrow or lock survives a clause boundary.
                let state = std::cell::RefCell::new((&mut *workspace.txn, &mut *database));
                let action_stats = vertex_upsert_actions::<WriteTxnError, A, _>(
                    upsert, policy, stats.merge, outcome, || cx.checkpoint(),
                    |key, control| {
                        let state = state.borrow();
                        vertex_upsert_property(state.0, state.1, outcome.vertex(), key, control)
                    },
                    |batch| {
                        let mut state = state.borrow_mut();
                        let (transaction, database) = &mut *state;
                        transaction.write(database, batch).map(|_| ()).map_err(infrastructure)
                    },
                )?;
                stats.merge.evaluator = action_stats.merge.evaluator;
            }
            // Keep action statistics as the prefix. Returned execution counters
            // additionally include all final reads and native row processing.
            let mut evaluator = stats.merge.evaluator;
            let rows = {
                let mut control = |event| {
                    vertex_upsert_batch_event::<A>(cx, policy.merge.query, &mut evaluator, event)
                };
                control(GlaExecutionEvent::Work)?;
                let mut rows = Vec::new();
                for outcome in targets.outcomes() {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    let mut row = Vec::new();
                    for binding in query.bindings() {
                        control(GlaExecutionEvent::ScratchEntry)?;
                        row.push(match *binding {
                            GraphVertexReturnBinding::Vertex => GraphValue::Vertex(outcome.vertex()),
                            GraphVertexReturnBinding::Property(key) => GraphValue::Scalar(
                                vertex_upsert_property(
                                    workspace.txn, database, outcome.vertex(), key, &mut control,
                                )?,
                            ),
                            GraphVertexReturnBinding::Properties => vertex_upsert_properties(
                                workspace.txn, database, outcome.vertex(), query.property_catalog(),
                                |_| true,
                                || cx.checkpoint().map_err(GqlQueryError::Interrupted),
                                &mut control,
                            )?,
                            GraphVertexReturnBinding::Labels => vertex_upsert_labels(
                                workspace.txn, database, outcome.vertex(), query.property_catalog(),
                                |_| true,
                                || cx.checkpoint().map_err(GqlQueryError::Interrupted),
                                &mut control,
                            )?,
                        });
                    }
                    rows.push(row);
                }
                rows
            };
            let returning = query.project_rows_governed(
                rows, stats.merge.match_selection, evaluator, policy.merge.query,
                || cx.checkpoint(),
            ).map_err(|error| error.map_source(GraphVertexUpsertError::Returning))?;
            workspace.accept();
            Ok((stats, returning))
        })
    }

    /// The all-target result path, using the database's identity allocator.
    pub fn execute_graph_vertex_upserts_query_engine_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGraphVertexUpsertQuery,
        policy: fgdb_gql::GraphVertexUpsertPolicy,
    ) -> VertexUpsertBatchQueryResult<WriteTxnError> {
        use fgdb_gql::{GqlQueryError, GraphVertexUpsertError};
        let infrastructure = |error| GqlQueryError::Source(GraphVertexUpsertError::Staging(error));
        self.ensure_database(database).map_err(infrastructure)?;
        let allocate = database.engine_allocator(cx).map_err(infrastructure)?;
        self.execute_graph_vertex_upserts_query_governed(database, cx, query, policy, allocate)
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Accept the complete all-target RETURN before the sole transaction finish.
    /// A post-action failure publishes nothing. A finish failure retains its
    /// original committed/unknown meaning; no result-processing retry occurs.
    pub async fn execute_graph_vertex_upserts_query_autocommit_governed<A>(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        query: &fgdb_gql::PreparedGraphVertexUpsertQuery,
        policy: fgdb_gql::GraphVertexUpsertPolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> Result<
        (
            fgdb_gql::GraphVertexUpsertStats,
            fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
            EmbeddedTxnCompletion,
        ),
        TxnGqlError<fgdb_gql::GraphVertexUpsertError<WriteTxnError, A>>,
    > {
        use fgdb_gql::{GqlQueryError, GraphVertexUpsertError};
        let infrastructure = |error| GqlQueryError::Source(GraphVertexUpsertError::Staging(error));
        let mut transaction = self.begin(txn_cx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let (stats, rows) = match transaction.execute_graph_vertex_upserts_query_governed(
            self, query_cx, query, policy, allocate,
        ) {
            Ok(result) => result,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction.finish(self, commit_cx).await.map_err(infrastructure)?;
        Ok((stats, rows, completion))
    }
}
