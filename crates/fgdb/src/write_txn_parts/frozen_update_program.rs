// The UNWIND compiler admits two profiles here: vertex field updates, and
// vertex upserts with pairwise distinct MERGE keys (fgdb-2l3pt). Every
// selection and RHS uses the real transaction overlay at entry; one ordered
// preparation then stages all proposals, without rewalking a growing prefix.

impl WriteTxn {
    #[allow(clippy::type_complexity)]
    fn execute_frozen_vertex_updates_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        program: &fgdb_gql::PreparedGraphWriteProgram,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        returning: bool,
    ) -> Result<
        (
            fgdb_gql::GraphWriteProgramStats,
            Vec<fgdb_gql::GraphWriteStepReceipt>,
        ),
        fgdb_gql::GraphWriteProgramError<WriteTxnError, A, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::{
            GqlQueryError, GraphMutationError, GraphMutationIntent, GraphMutationProgramError,
            GraphWriteProgramError, GraphWriteStatement, GraphWriteStepError,
            GraphWriteStepReceipt, GraphWriteStepStats,
        };
        debug_assert!(program.freezes_vertex_update_input());
        cx.with_restriction(|| {
            let workspace = MutationProgramWorkspace::new(self);
            workspace.txn.program_multi_relation = true;
            let mut batches = Vec::new();
            let mut receipts = Vec::new();
            let mut deferred_error_at = None;
            let source = |error| {
                GraphWriteStepError::Mutation(GqlQueryError::Source(GraphMutationError::Source(
                    error,
                )))
            };
            let result = program.execute_governed(
                policy,
                |statement, input, remaining| {
                    let GraphWriteStatement::Mutation(input) = input else {
                        return Err(source(WriteTxnError::UnsupportedAtomicMutation));
                    };
                    workspace
                        .txn
                        .admit_mutation(database, input.relation())
                        .map_err(source)?;
                    let proposal = input
                        .execute_governed(
                            remaining.mutations,
                            |pattern, allowance| {
                                workspace.txn.execute_graph_pattern_governed(
                                    database, cx, pattern, allowance,
                                )
                            },
                            || cx.checkpoint(),
                        )
                        .map_err(GraphWriteStepError::Mutation)?;
                    let stats = proposal.stats();
                    let mut batch = WriteBatch::new(input.relation());
                    let mut targets = std::collections::BTreeSet::new();
                    for intent in proposal.into_intents() {
                        cx.checkpoint()
                            .map_err(GqlQueryError::Interrupted)
                            .map_err(GraphWriteStepError::Mutation)?;
                        let vertex = match intent {
                            GraphMutationIntent::Property { vertex, key, value } => {
                                batch.set_vertex_property_value(vertex, key, value);
                                vertex
                            }
                            GraphMutationIntent::StoredProperty {
                                target: ElementId::Vertex(vertex),
                                key,
                                value,
                            } => {
                                batch.set_vertex_property_value(vertex, key, Some(value));
                                vertex
                            }
                            GraphMutationIntent::Label {
                                vertex,
                                label,
                                present,
                            } => {
                                batch.set_vertex_label(vertex, label, present);
                                vertex
                            }
                            _ => return Err(source(WriteTxnError::UnsupportedAtomicMutation)),
                        };
                        if returning {
                            targets.insert(vertex);
                        }
                    }
                    if returning {
                        receipts.push(GraphWriteStepReceipt::Mutation {
                            targets: targets.into_iter().collect(),
                            edges: Vec::new(),
                        });
                    }
                    if !batch.is_empty() {
                        batches.push((statement, batch));
                    }
                    if statement + 1 == program.statements().len() && !batches.is_empty() {
                        // Admission must inspect every original input, including
                        // an invalid value that a later assignment overwrites.
                        // Keep that input's location instead of blaming the final
                        // row where the one ordered preparation is performed.
                        for (origin, batch) in &batches {
                            cx.checkpoint()
                                .map_err(GqlQueryError::Interrupted)
                                .map_err(GraphWriteStepError::Mutation)?;
                            if let Err(error) = database.admit_write_values(batch) {
                                deferred_error_at = Some(*origin);
                                return Err(source(WriteTxnError::Write(error)));
                            }
                        }
                        workspace
                            .txn
                            .write_ordered(
                                database,
                                core::mem::take(&mut batches)
                                    .into_iter()
                                    .map(|(_, batch)| batch)
                                    .collect(),
                            )
                            .map_err(source)?;
                    }
                    Ok(GraphWriteStepStats::Mutation(stats))
                },
                || cx.checkpoint(),
            );
            let stats = result.map_err(|error| match (deferred_error_at, error) {
                (
                    Some(statement),
                    GraphWriteProgramError::Program(GraphMutationProgramError::Statement {
                        source,
                        ..
                    }),
                ) => GraphMutationProgramError::Statement { statement, source }.into(),
                (_, error) => error,
            })?;
            // The program's actual statistics and final checkpoint are checked
            // AFTER staging. A late refusal restores the caller's exact prefix
            // and retains the selection and preparation conflict witnesses.
            workspace.accept();
            Ok((stats, receipts))
        })
    }
}

impl WriteTxn {
    /// The frozen upsert profile (PreparedGraphWriteProgram::
    /// freezes_vertex_upsert_input). Each row's MERGE matches against the
    /// entry overlay and allocates its creation exactly as the sequential
    /// path does; its creation and ON/SET actions append to that row's batch
    /// instead of re-preparing every earlier row. One ordered preparation
    /// stages all rows after the last statement. Distinct keys and key-free
    /// constant actions make this the sequential program's result; receipts
    /// and per-statement statistics keep their ordinary shape.
    #[allow(clippy::type_complexity)]
    fn execute_frozen_vertex_upserts_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        program: &fgdb_gql::PreparedGraphWriteProgram,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        mut allocate: impl FnMut(fgdb_gql::GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<
        (
            fgdb_gql::GraphWriteProgramStats,
            Vec<fgdb_gql::GraphWriteStepReceipt>,
        ),
        fgdb_gql::GraphWriteProgramError<WriteTxnError, A, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::insertion::GraphInsertIntent;
        use fgdb_gql::{
            GlaExecutionEvent, GqlQueryError, GraphMutationProgramError, GraphVertexMergeError,
            GraphVertexUpsertBranch, GraphVertexUpsertError, GraphVertexUpsertStats,
            GraphWriteIdentityRequest, GraphWriteProgramError, GraphWriteStatement,
            GraphWriteStepError,
        };
        debug_assert!(program.freezes_vertex_upsert_input());
        cx.with_restriction(|| {
            let workspace = MutationProgramWorkspace::new(self);
            workspace.txn.program_multi_relation = true;
            let mut batches = Vec::new();
            let mut receipts = Vec::new();
            let mut deferred_error_at = None;
            let staging = |error| GqlQueryError::Source(GraphVertexUpsertError::Staging(error));
            let result = program.execute_governed(
                policy,
                |statement, input, remaining| {
                    let GraphWriteStatement::VertexUpsert(upsert) = input else {
                        return Err(GraphWriteStepError::VertexUpsert(staging(
                            WriteTxnError::UnsupportedAtomicMutation,
                        )));
                    };
                    let policy = remaining.vertex_upsert_policy();
                    let mut batch = WriteBatch::new(upsert.merge().relation());
                    let (stats, receipt) = (|| {
                        let proposal =
                            collect_vertex_merge_output::<VertexMergeTargets, WriteTxnError, A, _>(
                                upsert.merge(),
                                policy.merge,
                                |pattern, allowance| {
                                    workspace.txn.execute_graph_pattern_governed(
                                        database, cx, pattern, allowance,
                                    )
                                },
                                |request| {
                                    allocate(GraphWriteIdentityRequest { statement, request })
                                },
                                || cx.checkpoint(),
                            )
                            .map_err(|error| error.map_source(GraphVertexUpsertError::Merge))?;
                        let targets = proposal.outcome;
                        let (branch, actions) = if targets.created.is_some() {
                            (GraphVertexUpsertBranch::Create, upsert.on_create())
                        } else {
                            (GraphVertexUpsertBranch::Match, upsert.on_match())
                        };
                        let observed = (targets.len() as u128)
                            .checked_mul(actions.len() as u128 + upsert.after().len() as u128)
                            .ok_or_else(|| {
                                GqlQueryError::Source(GraphVertexUpsertError::Merge(
                                    GraphVertexMergeError::AccountingOverflow,
                                ))
                            })?;
                        if observed > u128::from(policy.max_actions) {
                            return Err(GqlQueryError::Source(
                                GraphVertexUpsertError::ActionLimit {
                                    limit: policy.max_actions,
                                    observed,
                                },
                            ));
                        }
                        if let Some(creation) = proposal.creation {
                            for intent in creation.into_intents() {
                                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                                let GraphInsertIntent::Vertex {
                                    vertex,
                                    labels,
                                    properties,
                                } = intent
                                else {
                                    unreachable!("validated vertex MERGE contains no edge creation")
                                };
                                batch.create_vertex_values(vertex, labels, properties);
                            }
                        }
                        let mut stats = GraphVertexUpsertStats {
                            merge: proposal.stats,
                            branch,
                            action_effects: observed as u64,
                        };
                        for outcome in targets.outcomes() {
                            vertex_upsert_batch_event::<A>(
                                cx,
                                policy.merge.query,
                                &mut stats.merge.evaluator,
                                GlaExecutionEvent::Work,
                            )?;
                            let action_stats = vertex_upsert_actions::<WriteTxnError, A, _>(
                                upsert,
                                policy,
                                stats.merge,
                                outcome,
                                || cx.checkpoint(),
                                // The profile admits no action that reads its
                                // target; a read would see the entry overlay.
                                |key, control| {
                                    vertex_upsert_property(
                                        &*workspace.txn,
                                        &*database,
                                        outcome.vertex(),
                                        key,
                                        control,
                                    )
                                },
                                |staged| {
                                    batch.rows.extend(staged.rows);
                                    Ok(())
                                },
                            )?;
                            stats.merge.evaluator = action_stats.merge.evaluator;
                        }
                        let outcomes = targets.into_outcomes(|event| {
                            vertex_upsert_batch_event::<A>(
                                cx,
                                policy.merge.query,
                                &mut stats.merge.evaluator,
                                event,
                            )
                        })?;
                        vertex_upsert_program_result::<A>((stats, outcomes))
                    })()
                    .map_err(GraphWriteStepError::VertexUpsert)?;
                    receipts.push(receipt);
                    if !batch.is_empty() {
                        batches.push((statement, batch));
                    }
                    if statement + 1 == program.statements().len() && !batches.is_empty() {
                        // Admit every row's values first, so a refusal names
                        // its own row, not the last one where staging happens.
                        for (origin, batch) in &batches {
                            cx.checkpoint()
                                .map_err(GqlQueryError::Interrupted)
                                .map_err(GraphWriteStepError::VertexUpsert)?;
                            if let Err(error) = database.admit_write_values(batch) {
                                deferred_error_at = Some(*origin);
                                return Err(GraphWriteStepError::VertexUpsert(staging(
                                    WriteTxnError::Write(error),
                                )));
                            }
                        }
                        workspace
                            .txn
                            .write_ordered(
                                database,
                                core::mem::take(&mut batches)
                                    .into_iter()
                                    .map(|(_, batch)| batch)
                                    .collect(),
                            )
                            .map_err(|error| GraphWriteStepError::VertexUpsert(staging(error)))?;
                    }
                    Ok(stats)
                },
                || cx.checkpoint(),
            );
            let stats = result.map_err(|error| match (deferred_error_at, error) {
                (
                    Some(statement),
                    GraphWriteProgramError::Program(GraphMutationProgramError::Statement {
                        source,
                        ..
                    }),
                ) => GraphMutationProgramError::Statement { statement, source }.into(),
                (_, error) => error,
            })?;
            workspace.accept();
            Ok((stats, receipts))
        })
    }
}
