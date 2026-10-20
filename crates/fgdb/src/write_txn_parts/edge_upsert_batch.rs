// Occurrence-preserving relationship upserts over the existing native owners.
// The MATCH input is frozen once; only native staged state changes between
// occurrences. No matcher, expression interpreter or commit path is duplicated.

type EdgeUpsertBatchResult<A> = Result<
    (
        fgdb_gql::GraphEdgeUpsertBatchStats,
        Vec<fgdb_gql::GraphEdgeMergeOutcome>,
    ),
    TxnGqlError<fgdb_gql::GraphEdgeUpsertError<WriteTxnError, A>>,
>;
type EdgeUpsertBatchQueryResult<A> = Result<
    (
        fgdb_gql::GraphEdgeUpsertBatchStats,
        fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
    ),
    TxnGqlError<fgdb_gql::GraphEdgeUpsertQueryError<WriteTxnError, A>>,
>;

// One input occurrence can resolve to every existing parallel relationship.
// The shared collector still owns orientation, property equality, identity
// order, creation admission and all of its ordinary source/absence witnesses.
struct EdgeUpsertOccurrences(Vec<fgdb_gql::GraphEdgeMergeOutcome>);
impl EdgeMergeOutput for EdgeUpsertOccurrences {
    const PARALLEL: bool = true;

    fn for_pairs(pairs: usize) -> Option<Self> {
        (pairs <= 1).then(|| Self(Vec::new()))
    }

    fn record<C>(
        &mut self,
        outcome: fgdb_gql::GraphEdgeMergeOutcome,
        _resolved: ResolvedEdge,
        control: &mut impl FnMut(Option<fgdb_gql::GlaExecutionEvent>) -> Result<(), C>,
    ) -> Result<(), C> {
        if outcome.edge().is_some() {
            control(Some(fgdb_gql::GlaExecutionEvent::ScratchEntry))?;
            self.0.push(outcome);
        }
        Ok(())
    }

    fn result_rows(&self) -> u64 {
        self.0.len() as u64
    }
}

fn edge_upsert_batch_invalid<A>() -> TxnGqlError<fgdb_gql::GraphEdgeUpsertError<WriteTxnError, A>> {
    fgdb_gql::GqlQueryError::Source(fgdb_gql::GraphEdgeUpsertError::Merge(
        fgdb_gql::GraphEdgeMergeError::InvalidSourceStatistics,
    ))
}

fn edge_upsert_batch_event<A>(
    cx: &fgdb_types::QueryCx,
    policy: fgdb_gql::GqlQueryPolicy,
    evaluator: &mut fgdb_gql::GlaExecutionStats,
    event: fgdb_gql::GlaExecutionEvent,
) -> EdgeUpsertActionResult<(), WriteTxnError, A, Box<asupersync::error::Error>> {
    use fgdb_gql::{GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GqlQueryError};
    cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
    let work = u128::from(evaluator.work_units) + 1;
    let scratch = u128::from(evaluator.scratch_entries)
        + u128::from(event == GlaExecutionEvent::ScratchEntry);
    for (observed, limit, dimension) in [
        (
            work,
            policy.evaluator.max_work_units,
            GlaLimitDimension::WorkUnits,
        ),
        (
            scratch,
            policy.evaluator.max_scratch_entries,
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
}

impl WriteTxn {
    /// Execute every selected MATCH occurrence, preserving both duplicate
    /// endpoint pairs and parallel relationship matches. Each occurrence
    /// resolves against the preceding occurrence's canonical staged effects:
    /// an earlier creation can match later, and a changed map key can stop
    /// matching. ON clauses run per resolved relationship, in identity order;
    /// assignments within a clause remain simultaneous, then trailing SET sees
    /// them. NoInput runs no action and returns an empty occurrence receipt.
    ///
    /// One guard covers the whole statement. A later expression, quota, source,
    /// cancellation or unwind failure restores the exact previous staged prefix.
    /// Read witnesses and issued external identities are retained. Allocation
    /// requests carry the original MATCH occurrence index, not a reset pair ID.
    ///
    /// MATCH runs once. All later existence scans, actions and retained receipts
    /// spend one cumulative source/work/scratch/creation/action allowance. This
    /// is resident native execution, not spill or physical-byte accounting.
    /// The existing singular upsert primitive retains its stricter cardinality.
    pub fn execute_graph_edge_upserts_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        upsert: &fgdb_gql::PreparedGraphEdgeUpsert,
        policy: fgdb_gql::GraphEdgeUpsertPolicy,
        mut allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> EdgeUpsertBatchResult<A> {
        use fgdb_gql::{
            GlaExecutionEvent, GqlExecutionStats, GqlQueryError, GqlQueryExecution,
            GraphEdgeMergeError, GraphEdgeMergeOutcome, GraphEdgeUpsertBatchStats,
            GraphEdgeUpsertBranch, GraphEdgeUpsertError,
        };
        let infrastructure = |error| GqlQueryError::Source(GraphEdgeUpsertError::Staging(error));
        self.ensure_database(database).map_err(infrastructure)?;
        let live = database
            .frontier()
            .map_err(WriteTxnError::from)
            .map_err(infrastructure)?;
        if live != self.basis {
            return Err(infrastructure(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live,
            }));
        }
        if let Some(first) = self.staged.first()
            && !self.program_multi_relation
            && self
                .staged
                .iter()
                .all(|batch| batch.relation == first.relation)
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
            let selected = workspace
                .txn
                .execute_graph_pattern_governed(
                    database,
                    cx,
                    upsert.merge().selection(),
                    policy.merge.query,
                )
                .map_err(|error| {
                    error.map_source(|error| {
                        GraphEdgeUpsertError::Merge(GraphEdgeMergeError::Source(error))
                    })
                })?;
            if u64::try_from(selected.value.len()).ok() != Some(selected.rows.result_rows) {
                return Err(edge_upsert_batch_invalid());
            }
            let mut stats = GraphEdgeUpsertBatchStats {
                match_selection: selected.rows,
                overlay_edges: 0,
                created_edges: 0,
                matched_occurrences: 0,
                action_effects: 0,
                evaluator: selected.evaluator,
            };
            edge_upsert_batch_event::<A>(
                cx,
                policy.merge.query,
                &mut stats.evaluator,
                GlaExecutionEvent::Work,
            )?;
            let mut outcomes = Vec::new();
            for (row_at, row) in selected.value.into_iter().enumerate() {
                // Transfer an already selected row, without re-running its
                // pattern or cloning its payload. The source callback supplies
                // that real tuple and the cumulative prefix counters to the
                // ordinary collector; it is not a second graph representation.
                edge_upsert_batch_event::<A>(
                    cx,
                    policy.merge.query,
                    &mut stats.evaluator,
                    GlaExecutionEvent::ScratchEntry,
                )?;
                let records = stats
                    .match_selection
                    .snapshot_records
                    .checked_add(stats.overlay_edges)
                    .ok_or_else(edge_upsert_batch_invalid::<A>)?;
                let mut merge_policy = policy.merge;
                merge_policy.max_created_edges = policy
                    .merge
                    .max_created_edges
                    .checked_sub(stats.created_edges)
                    .ok_or_else(edge_upsert_batch_invalid::<A>)?;
                let proposal =
                    collect_edge_merge_output::<EdgeUpsertOccurrences, WriteTxnError, A, _>(
                        upsert.merge(),
                        merge_policy,
                        |_, _| {
                            Ok(GqlQueryExecution {
                                value: vec![row],
                                rows: GqlExecutionStats {
                                    snapshot_records: records,
                                    result_rows: 1,
                                },
                                evaluator: stats.evaluator,
                            })
                        },
                        |source, destination| {
                            workspace.txn.merge_candidate_edges(
                                database,
                                upsert.merge(),
                                source,
                                destination,
                                || cx.checkpoint(),
                            )
                        },
                        |_| {
                            allocate(fgdb_gql::insertion::GraphInsertRequest::Edge {
                                row: row_at,
                                edge: 0,
                            })
                        },
                        || cx.checkpoint(),
                    )
                    .map_err(|error| {
                        error.map_source(|error| {
                            // Expose the original statement-wide creation cap, not
                            // an apparently fresh allowance for this one occurrence.
                            GraphEdgeUpsertError::Merge(match error {
                                GraphEdgeMergeError::CreationLimit { observed, .. } => {
                                    GraphEdgeMergeError::CreationLimit {
                                        limit: policy.merge.max_created_edges,
                                        observed: u128::from(stats.created_edges) + observed,
                                    }
                                }
                                error => error,
                            })
                        })
                    })?;
                stats.evaluator = proposal.stats.evaluator;
                stats.overlay_edges = stats
                    .overlay_edges
                    .checked_add(proposal.stats.overlay_edges)
                    .ok_or_else(edge_upsert_batch_invalid::<A>)?;
                stats.created_edges = stats
                    .created_edges
                    .checked_add(proposal.stats.created_edges)
                    .ok_or_else(edge_upsert_batch_invalid::<A>)?;
                if let Some(batch) = proposal.creation {
                    workspace
                        .txn
                        .write(database, batch)
                        .map_err(infrastructure)?;
                }
                for outcome in proposal.outcome.0 {
                    let branch = match outcome {
                        GraphEdgeMergeOutcome::NoInput => continue,
                        GraphEdgeMergeOutcome::Matched(_) => GraphEdgeUpsertBranch::Match,
                        GraphEdgeMergeOutcome::Created(_) => GraphEdgeUpsertBranch::Create,
                    };
                    let actions =
                        u128::from(stats.action_effects) + upsert.action_count(branch) as u128;
                    if actions > u128::from(policy.max_actions) {
                        return Err(GqlQueryError::Source(GraphEdgeUpsertError::ActionLimit {
                            limit: policy.max_actions,
                            observed: actions,
                        }));
                    }
                    let mut merge_stats = proposal.stats;
                    merge_stats.evaluator = stats.evaluator;
                    let state = std::cell::RefCell::new((&mut *workspace.txn, &mut *database));
                    let action_stats = edge_upsert_actions::<WriteTxnError, A, _>(
                        upsert,
                        policy,
                        merge_stats,
                        outcome,
                        || cx.checkpoint(),
                        |edge, key, control| {
                            let state = state.borrow();
                            edge_upsert_property(state.0, state.1, edge, key, control)
                        },
                        |batch| {
                            let mut state = state.borrow_mut();
                            let (transaction, database) = &mut *state;
                            transaction
                                .write(database, batch)
                                .map(|_| ())
                                .map_err(infrastructure)
                        },
                    )?;
                    stats.evaluator = action_stats.evaluator;
                    stats.action_effects = actions as u64;
                    if matches!(outcome, GraphEdgeMergeOutcome::Matched(_)) {
                        stats.matched_occurrences = stats
                            .matched_occurrences
                            .checked_add(1)
                            .ok_or_else(edge_upsert_batch_invalid::<A>)?;
                    }
                    edge_upsert_batch_event::<A>(
                        cx,
                        policy.merge.query,
                        &mut stats.evaluator,
                        GlaExecutionEvent::ScratchEntry,
                    )?;
                    outcomes.push(outcome);
                }
            }
            edge_upsert_batch_event::<A>(
                cx,
                policy.merge.query,
                &mut stats.evaluator,
                GlaExecutionEvent::Work,
            )?;
            workspace.accept();
            Ok((stats, outcomes))
        })
    }

    /// Preserve the complete upsert occurrence bag through one native RETURN.
    /// All final-state field reads, grouping, DISTINCT, sorting and pagination
    /// finish inside the same rollback boundary as every ON/SET action.
    /// The result-row allowance applies to the final page, never to the private
    /// MATCH input. Snapshot, work, scratch, creation and action caps remain
    /// unchanged. LIMIT 0 still executes and validates the entire statement.
    pub fn execute_graph_edge_upserts_query_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGraphEdgeUpsertQuery,
        policy: fgdb_gql::GraphEdgeUpsertPolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> EdgeUpsertBatchQueryResult<A> {
        use fgdb_gql::algebra::GraphValue;
        use fgdb_gql::{
            GlaExecutionEvent, GqlExecutionStats, GqlQueryError, GraphEdgeReturnBinding,
            GraphEdgeUpsertError, GraphEdgeUpsertQueryError,
        };
        self.ensure_database(database).map_err(|error| {
            GqlQueryError::Source(GraphEdgeUpsertQueryError::Upsert(
                GraphEdgeUpsertError::Staging(error),
            ))
        })?;
        cx.with_restriction(|| {
            let workspace = MutationProgramWorkspace::new(self);
            let (stats, outcomes) = workspace.txn.execute_graph_edge_upserts_governed(
                database, cx, query.upsert(), edge_upsert_query_selection_policy(policy), allocate,
            ).map_err(|error| error.map_source(GraphEdgeUpsertQueryError::Upsert))?;
            let mut evaluator = stats.evaluator;
            let rows = (|| -> EdgeUpsertActionResult<
                Vec<Vec<GraphValue>>, WriteTxnError, A, Box<asupersync::error::Error>,
            > {
                let mut control = |event| {
                    edge_upsert_batch_event::<A>(cx, policy.merge.query, &mut evaluator, event)
                };
                control(GlaExecutionEvent::Work)?;
                let mut rows = Vec::new();
                for outcome in &outcomes {
                    let Some(edge) = outcome.edge() else { continue };
                    control(GlaExecutionEvent::ScratchEntry)?;
                    let mut cells = Vec::new();
                    for binding in query.bindings() {
                        control(GlaExecutionEvent::ScratchEntry)?;
                        cells.push(match *binding {
                            GraphEdgeReturnBinding::Edge => GraphValue::Edge(edge),
                            GraphEdgeReturnBinding::Property(key) => GraphValue::Scalar(
                                edge_upsert_property(workspace.txn, database, edge, key, &mut control)?,
                            ),
                        });
                    }
                    rows.push(cells);
                }
                Ok(rows)
            })().map_err(|error| error.map_source(GraphEdgeUpsertQueryError::Upsert))?;
            let snapshot_records = stats.match_selection.snapshot_records.checked_add(stats.overlay_edges)
                .ok_or_else(|| edge_upsert_batch_invalid::<A>().map_source(GraphEdgeUpsertQueryError::Upsert))?;
            let result_rows = u64::try_from(rows.len())
                .map_err(|_| edge_upsert_batch_invalid::<A>().map_source(GraphEdgeUpsertQueryError::Upsert))?;
            let returning = query.project_rows_governed(
                rows,
                GqlExecutionStats { snapshot_records, result_rows },
                evaluator,
                policy.merge.query,
                || cx.checkpoint(),
            ).map_err(|error| error.map_source(GraphEdgeUpsertQueryError::Returning))?;
            workspace.accept();
            Ok((stats, returning))
        })
    }

    /// The occurrence-preserving RETURN path with database-owned identities.
    pub fn execute_graph_edge_upserts_query_engine_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGraphEdgeUpsertQuery,
        policy: fgdb_gql::GraphEdgeUpsertPolicy,
    ) -> EdgeUpsertBatchQueryResult<WriteTxnError> {
        use fgdb_gql::{GqlQueryError, GraphEdgeUpsertError, GraphEdgeUpsertQueryError};
        let infrastructure = |error| {
            GqlQueryError::Source(GraphEdgeUpsertQueryError::Upsert(
                GraphEdgeUpsertError::Staging(error),
            ))
        };
        self.ensure_database(database).map_err(infrastructure)?;
        let allocate = database.engine_allocator(cx).map_err(infrastructure)?;
        self.execute_graph_edge_upserts_query_governed(database, cx, query, policy, allocate)
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Publish an occurrence-preserving relationship upsert exactly once, and
    /// only after the complete RETURN succeeds. Completion uncertainty is the
    /// ordinary native finish error; it is never relabeled as a rollback.
    pub async fn execute_graph_edge_upserts_query_autocommit_governed<A>(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        query: &fgdb_gql::PreparedGraphEdgeUpsertQuery,
        policy: fgdb_gql::GraphEdgeUpsertPolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> Result<
        (
            fgdb_gql::GraphEdgeUpsertBatchStats,
            fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
            EmbeddedTxnCompletion,
        ),
        TxnGqlError<fgdb_gql::GraphEdgeUpsertQueryError<WriteTxnError, A>>,
    > {
        use fgdb_gql::{GqlQueryError, GraphEdgeUpsertError, GraphEdgeUpsertQueryError};
        let infrastructure = |error| {
            GqlQueryError::Source(GraphEdgeUpsertQueryError::Upsert(
                GraphEdgeUpsertError::Staging(error),
            ))
        };
        let mut transaction = self
            .begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let (stats, rows) = match transaction
            .execute_graph_edge_upserts_query_governed(self, query_cx, query, policy, allocate)
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
        Ok((stats, rows, completion))
    }
}
