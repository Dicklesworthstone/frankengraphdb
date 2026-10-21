// The UNWIND compiler admits only vertex field updates to this profile. Every
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
                                batch.set_vertex_property(vertex, key, value);
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
