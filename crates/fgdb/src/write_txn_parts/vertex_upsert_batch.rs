// All-target vertex MERGE uses the existing matcher, insertion collector,
// point readers, SET bytecode and statement rollback workspace.

struct VertexMergeTargets {
    matched: std::collections::BTreeSet<VId>,
    created: Option<VId>,
}
impl VertexMergeOutput for VertexMergeTargets {
    const UNIQUE: bool = false;

    fn matched(matched: std::collections::BTreeSet<VId>) -> Self {
        Self {
            matched,
            created: None,
        }
    }

    fn created(vertex: VId) -> Self {
        Self {
            matched: std::collections::BTreeSet::new(),
            created: Some(vertex),
        }
    }
}
impl VertexMergeTargets {
    fn len(&self) -> usize {
        self.matched.len() + usize::from(self.created.is_some())
    }

    fn outcomes(&self) -> impl Iterator<Item = fgdb_gql::GraphVertexMergeOutcome> + '_ {
        self.matched
            .iter()
            .copied()
            .map(fgdb_gql::GraphVertexMergeOutcome::Matched)
            .chain(self.created.map(fgdb_gql::GraphVertexMergeOutcome::Created))
    }

    // Transfer the frozen target set to a non-authoritative receipt in full
    // identity order. Ownership and cancellation are charged before acceptance.
    fn into_outcomes<E>(
        self,
        mut control: impl FnMut(fgdb_gql::GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Vec<fgdb_gql::GraphVertexMergeOutcome>, E> {
        use fgdb_gql::{GlaExecutionEvent, GraphVertexMergeOutcome};
        let mut outcomes = Vec::new();
        control(GlaExecutionEvent::Work)?;
        for vertex in self.matched {
            control(GlaExecutionEvent::ScratchEntry)?;
            outcomes.push(GraphVertexMergeOutcome::Matched(vertex));
        }
        if let Some(vertex) = self.created {
            control(GlaExecutionEvent::ScratchEntry)?;
            outcomes.push(GraphVertexMergeOutcome::Created(vertex));
        }
        control(GlaExecutionEvent::Work)?;
        Ok(outcomes)
    }
}

type VertexMergeBatchResult<A> = Result<
    (
        fgdb_gql::GraphVertexMergeStats,
        Vec<fgdb_gql::GraphVertexMergeOutcome>,
    ),
    TxnGqlError<fgdb_gql::GraphVertexMergeError<WriteTxnError, A>>,
>;

type VertexUpsertBatchResult<A> = Result<
    (
        fgdb_gql::GraphVertexUpsertStats,
        Vec<fgdb_gql::GraphVertexMergeOutcome>,
    ),
    TxnGqlError<fgdb_gql::GraphVertexUpsertError<WriteTxnError, A>>,
>;

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
    vertex_targets_event(cx, policy, evaluator, event)
}

fn vertex_targets_event<E>(
    cx: &fgdb_types::QueryCx,
    policy: fgdb_gql::GqlQueryPolicy,
    evaluator: &mut fgdb_gql::GlaExecutionStats,
    event: fgdb_gql::GlaExecutionEvent,
) -> Result<(), TxnGqlError<E>> {
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
    /// Resolve every distinct vertex of a MERGE, or create its single vertex
    /// only when the complete match is empty. The ordinary matcher and absence
    /// witnesses are unchanged. Source occurrences retain the supplied query
    /// allowance; receipt ownership consumes work/scratch, not fabricated rows.
    /// Creation and receipt completion share one rollback workspace. Explicit
    /// singular and capability-scoped primitives keep their unique contract.
    pub fn execute_graph_vertex_merges_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        merge: &fgdb_gql::PreparedGraphVertexMerge,
        policy: fgdb_gql::GraphVertexMergePolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> VertexMergeBatchResult<A> {
        use fgdb_gql::{GqlQueryError, GraphVertexMergeError};
        let infrastructure = |error| GqlQueryError::Source(GraphVertexMergeError::Source(error));
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
            && merge.relation() != first.relation
        {
            return Err(infrastructure(WriteTxnError::RelationMismatch {
                expected: first.relation,
                found: merge.relation(),
            }));
        }
        cx.with_restriction(|| {
            cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
            let workspace = MutationProgramWorkspace::new(self);
            let proposal = collect_vertex_merge_output::<VertexMergeTargets, WriteTxnError, A, _>(
                merge,
                policy,
                |pattern, allowance| {
                    workspace
                        .txn
                        .execute_graph_pattern_governed(database, cx, pattern, allowance)
                },
                allocate,
                || cx.checkpoint(),
            )?;
            if let Some(creation) = proposal.creation {
                stage_vertex_merge_creation::<V, A>(
                    workspace.txn,
                    database,
                    cx,
                    merge.relation(),
                    creation,
                )?;
            }
            let mut stats = proposal.stats;
            let outcomes = proposal.outcome.into_outcomes(|event| {
                vertex_targets_event::<fgdb_gql::GraphVertexMergeError<WriteTxnError, A>>(
                    cx,
                    policy.query,
                    &mut stats.evaluator,
                    event,
                )
            })?;
            workspace.accept();
            Ok((stats, outcomes))
        })
    }

    /// Apply ON/SET to every frozen vertex target without constructing a RETURN
    /// query. All targets share one action/source/work/scratch/creation budget;
    /// the receipt records matched identities or the one created identity.
    /// A failure at any target or during receipt construction restores the prior
    /// staged prefix. Read witnesses and issued identities are never discarded.
    pub fn execute_graph_vertex_upserts_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        upsert: &fgdb_gql::PreparedGraphVertexUpsert,
        policy: fgdb_gql::GraphVertexUpsertPolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> VertexUpsertBatchResult<A> {
        self.with_vertex_upsert_targets(
            database,
            cx,
            upsert,
            policy,
            allocate,
            |_, _, mut stats, targets| {
                let outcomes = targets.into_outcomes(|event| {
                    vertex_upsert_batch_event::<A>(
                        cx,
                        policy.merge.query,
                        &mut stats.merge.evaluator,
                        event,
                    )
                })?;
                Ok((stats, outcomes))
            },
        )
    }

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
            GlaExecutionEvent, GqlQueryError, GraphVertexReturnBinding, GraphVertexUpsertError,
        };
        self.with_vertex_upsert_targets(
            database,
            cx,
            query.upsert(),
            vertex_upsert_query_selection_policy(policy),
            allocate,
            |transaction, database, stats, targets| {
                // Final reads and row operators remain inside the same guard
                // as every action. Only the final result spends the row grant.
                let mut evaluator = stats.merge.evaluator;
                let rows = {
                    let mut control = |event| {
                        vertex_upsert_batch_event::<A>(
                            cx,
                            policy.merge.query,
                            &mut evaluator,
                            event,
                        )
                    };
                    control(GlaExecutionEvent::Work)?;
                    let mut rows = Vec::new();
                    for outcome in targets.outcomes() {
                        control(GlaExecutionEvent::ScratchEntry)?;
                        let mut row = Vec::new();
                        for binding in query.bindings() {
                            control(GlaExecutionEvent::ScratchEntry)?;
                            row.push(match *binding {
                                GraphVertexReturnBinding::Vertex => {
                                    GraphValue::Vertex(outcome.vertex())
                                }
                                GraphVertexReturnBinding::Property(key) => {
                                    vertex_upsert_returned_property(
                                        transaction,
                                        database,
                                        outcome.vertex(),
                                        key,
                                        &mut control,
                                    )?
                                }
                                GraphVertexReturnBinding::Properties => vertex_upsert_properties(
                                    transaction,
                                    database,
                                    outcome.vertex(),
                                    query.property_catalog(),
                                    |_| true,
                                    || cx.checkpoint().map_err(GqlQueryError::Interrupted),
                                    &mut control,
                                )?,
                                GraphVertexReturnBinding::Labels => vertex_upsert_labels(
                                    transaction,
                                    database,
                                    outcome.vertex(),
                                    query.property_catalog(),
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
                let failure = std::cell::RefCell::new(None);
                let returning = query
                    .project_rows_governed_reading(
                        rows,
                        stats.merge.match_selection,
                        evaluator,
                        policy.merge.query,
                        transaction.element_reader_with_checkpoint(database, cx, &failure),
                        || match failure.borrow_mut().take() {
                            Some(error) => Err(error),
                            None => cx.checkpoint(),
                        },
                    )
                    .map_err(|error| error.map_source(GraphVertexUpsertError::Returning));
                let returning = match failure.into_inner() {
                    Some(error) => Err(GqlQueryError::Interrupted(error)),
                    None => returning,
                }?;
                Ok((stats, returning))
            },
        )
    }

    // One matcher/action owner for returning and rowless all-target upserts.
    // The private finalizer runs before acceptance and cannot outlive or leak
    // the guarded workspace. Query callers widen only the private row limit;
    // rowless programs retain their real selected-occurrence allowance.
    fn with_vertex_upsert_targets<V: Vfs + Clone, A, T>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        upsert: &fgdb_gql::PreparedGraphVertexUpsert,
        policy: fgdb_gql::GraphVertexUpsertPolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
        finish: impl FnOnce(
            &mut WriteTxn,
            &mut Database<V>,
            fgdb_gql::GraphVertexUpsertStats,
            VertexMergeTargets,
        ) -> Result<
            T,
            TxnGqlError<fgdb_gql::GraphVertexUpsertError<WriteTxnError, A>>,
        >,
    ) -> Result<T, TxnGqlError<fgdb_gql::GraphVertexUpsertError<WriteTxnError, A>>> {
        use fgdb_gql::{
            GlaExecutionEvent, GqlQueryError, GraphVertexUpsertBranch, GraphVertexUpsertError,
            GraphVertexUpsertStats,
        };
        let infrastructure = |error| GqlQueryError::Source(GraphVertexUpsertError::Staging(error));
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
            let proposal = collect_vertex_merge_output::<VertexMergeTargets, WriteTxnError, A, _>(
                upsert.merge(),
                policy.merge,
                |pattern, allowance| {
                    workspace
                        .txn
                        .execute_graph_pattern_governed(database, cx, pattern, allowance)
                },
                allocate,
                || cx.checkpoint(),
            )
            .map_err(|error| error.map_source(GraphVertexUpsertError::Merge))?;
            let targets = proposal.outcome;
            let (branch, actions) = if targets.created.is_some() {
                (GraphVertexUpsertBranch::Create, upsert.on_create())
            } else {
                (GraphVertexUpsertBranch::Match, upsert.on_match())
            };
            // All targets take the same branch. Admit the WHOLE action count
            // before staging the first one, without refreshing it per target.
            let observed = (targets.len() as u128)
                .checked_mul(
                    vertex_upsert_known_effects(actions) as u128
                        + vertex_upsert_known_effects(upsert.after()) as u128,
                )
                .ok_or_else(|| {
                    GqlQueryError::Source(GraphVertexUpsertError::Merge(
                        fgdb_gql::GraphVertexMergeError::AccountingOverflow,
                    ))
                })?;
            if observed > u128::from(policy.max_actions) {
                return Err(GqlQueryError::Source(GraphVertexUpsertError::ActionLimit {
                    limit: policy.max_actions,
                    observed,
                }));
            }
            if let Some(creation) = proposal.creation {
                stage_vertex_merge_creation::<V, A>(
                    workspace.txn,
                    database,
                    cx,
                    upsert.merge().relation(),
                    creation,
                )
                .map_err(|error| error.map_source(GraphVertexUpsertError::Merge))?;
            }
            let mut stats = GraphVertexUpsertStats {
                merge: proposal.stats,
                branch,
                property_removals: 0,
                action_effects: 0,
            };
            for outcome in targets.outcomes() {
                vertex_upsert_batch_event::<A>(
                    cx,
                    policy.merge.query,
                    &mut stats.merge.evaluator,
                    GlaExecutionEvent::Work,
                )?;
                // No native field borrow or lock survives a clause boundary.
                let state = std::cell::RefCell::new((&mut *workspace.txn, &mut *database));
                let action_stats = vertex_upsert_actions::<WriteTxnError, A, _>(
                    upsert,
                    policy,
                    stats.merge,
                    outcome,
                    (stats.action_effects, stats.property_removals),
                    || cx.checkpoint(),
                    |key, control| {
                        let state = state.borrow();
                        vertex_upsert_property(state.0, state.1, outcome.vertex(), key, control)
                    },
                    |control| {
                        let state = state.borrow();
                        vertex_replacement_keys(
                            state.0,
                            state.1,
                            outcome.vertex(),
                            None,
                            || cx.checkpoint().map_err(GqlQueryError::Interrupted),
                            control,
                            infrastructure,
                        )
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
                stats.merge.evaluator = action_stats.merge.evaluator;
                stats.action_effects = action_stats.action_effects;
                stats.property_removals = action_stats.property_removals;
            }
            let result = finish(workspace.txn, database, stats, targets)?;
            workspace.accept();
            Ok(result)
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
        let mut transaction = self
            .begin(txn_cx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let (stats, rows) = match transaction
            .execute_graph_vertex_upserts_query_governed(self, query_cx, query, policy, allocate)
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
