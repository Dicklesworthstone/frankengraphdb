// Branch-action vertex MERGE reuses unique MERGE, controlled point projection,
// checked scalar bytecode and ordinary staging inside ONE rollback boundary.

impl WriteTxn {
    /// Execute unique MERGE and its selected ON clause, then a trailing SET.
    /// Every clause freezes all right-hand sides before staging. The next
    /// clause sees canonical staged effects, not a private mutation interpreter.
    ///
    /// A late read, expression, quota or staging refusal restores the original
    /// workspace prefix. Observations remain conflict witnesses; issued graph
    /// identities are not reclaimed. Completion remains the caller's job.
    pub fn execute_graph_vertex_upsert_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        upsert: &fgdb_gql::PreparedGraphVertexUpsert,
        policy: fgdb_gql::GraphVertexUpsertPolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> Result<
        (
            fgdb_gql::GraphVertexUpsertStats,
            fgdb_gql::GraphVertexMergeOutcome,
        ),
        TxnGqlError<fgdb_gql::GraphVertexUpsertError<WriteTxnError, A>>,
    > {
        use fgdb_gql::{GqlQueryError, GraphVertexUpsertError};

        let workspace = MutationProgramWorkspace::new(self);
        let (merge_stats, outcome) = workspace
            .txn
            .execute_graph_vertex_merge_governed(
                database,
                cx,
                upsert.merge(),
                policy.merge,
                allocate,
            )
            .map_err(|error| error.map_source(GraphVertexUpsertError::Merge))?;
        let stats = cx.with_restriction(|| {
            // The callbacks are sequential. Their borrows end before the next
            // clause or callback; neither a frame nor a borrow crosses await.
            let state = std::cell::RefCell::new((&mut *workspace.txn, &mut *database));
            vertex_upsert_actions(
                upsert,
                policy,
                merge_stats,
                outcome,
                || cx.checkpoint(),
                |key, control| {
                    let state = state.borrow();
                    vertex_upsert_property(state.0, state.1, outcome.vertex(), key, control)
                },
                |batch| {
                    let mut state = state.borrow_mut();
                    let (transaction, database) = &mut *state;
                    transaction
                        .write(database, batch)
                        .map(|_| ())
                        .map_err(|error| {
                            GqlQueryError::Source(GraphVertexUpsertError::Staging(error))
                        })
                },
            )
        })?;
        workspace.accept();
        Ok((stats, outcome))
    }
}

type VertexUpsertActionResult<T, E, A, C> =
    Result<T, fgdb_gql::GqlQueryError<fgdb_gql::GraphVertexUpsertError<E, A>, C>>;

// Both adapters supply authority-appropriate reads and native staging. This
// collector owns only scalar evaluation and clause boundaries, never a graph.
#[allow(clippy::too_many_arguments)]
fn vertex_upsert_actions<E, A, C>(
    upsert: &fgdb_gql::PreparedGraphVertexUpsert,
    policy: fgdb_gql::GraphVertexUpsertPolicy,
    mut merge_stats: fgdb_gql::GraphVertexMergeStats,
    outcome: fgdb_gql::GraphVertexMergeOutcome,
    mut checkpoint: impl FnMut() -> Result<(), C>,
    mut property: impl FnMut(
        fgdb_delta_types::PropertyKeyId,
        &mut dyn FnMut(fgdb_gql::GlaExecutionEvent) -> VertexUpsertActionResult<(), E, A, C>,
    ) -> VertexUpsertActionResult<CanonicalScalar, E, A, C>,
    mut stage: impl FnMut(WriteBatch) -> VertexUpsertActionResult<(), E, A, C>,
) -> VertexUpsertActionResult<fgdb_gql::GraphVertexUpsertStats, E, A, C> {
    use crate::gql_exec::source::SourceEvent;
    use fgdb_gql::{
        GlaExecutionEvent, GlaExecutionStats, GlaLimitDimension, GlaLimitExceeded, GqlQueryError,
        GraphIntegerEvaluationError, GraphVertexMergeOutcome, GraphVertexUpsertAction,
        GraphVertexUpsertBranch, GraphVertexUpsertError, GraphVertexUpsertStats,
    };
    let (branch, selected) = match outcome {
        GraphVertexMergeOutcome::Matched(_) => (GraphVertexUpsertBranch::Match, upsert.on_match()),
        GraphVertexMergeOutcome::Created(_) => {
            (GraphVertexUpsertBranch::Create, upsert.on_create())
        }
    };
    let observed = selected.len() as u128 + upsert.after().len() as u128;
    if observed > u128::from(policy.max_actions) {
        return Err(GqlQueryError::Source(GraphVertexUpsertError::ActionLimit {
            limit: policy.max_actions,
            observed,
        }));
    }
    // Never refresh evaluator quota per expression, property or SET clause.
    let mut evaluator = merge_stats.evaluator;
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
        evaluator = GlaExecutionStats {
            work_units: work as u64,
            scratch_entries: scratch as u64,
        };
        Ok(())
    };
    for (clause, actions) in [selected, upsert.after()].into_iter().enumerate() {
        if actions.is_empty() {
            continue;
        }
        let mut batch = WriteBatch::new(upsert.merge().relation());
        for (action, definition) in actions.iter().enumerate() {
            control(GlaExecutionEvent::Work)?;
            control(GlaExecutionEvent::ScratchEntry)?; // proposal before allocation
            match definition {
                GraphVertexUpsertAction::SetProperty { key, value } => {
                    point_payload_admission(value.value(), &mut |event| {
                        control(match event {
                            SourceEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
                            _ => GlaExecutionEvent::Work,
                        })
                    })?;
                    batch.set_vertex_property(outcome.vertex(), *key, Some(value.value().clone()));
                }
                GraphVertexUpsertAction::SetExpression {
                    key,
                    properties,
                    value,
                } => {
                    for _ in properties {
                        control(GlaExecutionEvent::ScratchEntry)?;
                    }
                    let mut inputs = Vec::with_capacity(properties.len());
                    for key in properties {
                        let scalar = property(*key, &mut control)?;
                        inputs.push(fgdb_gql::algebra::GraphValue::Scalar(scalar));
                    }
                    let value = value
                        .evaluate_scalar_with_control(&inputs, &mut control)
                        .map_err(|error| match error {
                            GraphIntegerEvaluationError::Control(error) => error,
                            GraphIntegerEvaluationError::Value(source) => {
                                GqlQueryError::Source(GraphVertexUpsertError::Expression {
                                    clause,
                                    action,
                                    source,
                                })
                            }
                        })?;
                    batch.set_vertex_property(outcome.vertex(), *key, Some(value));
                }
                GraphVertexUpsertAction::SetLabel { label, present } => {
                    batch.set_vertex_label(outcome.vertex(), *label, *present);
                }
            }
        }
        // No RHS of this clause observes a sibling assignment. Conversely the
        // next clause MUST see native staging, including original authorization.
        stage(batch)?;
    }
    drop(control);
    merge_stats.evaluator = evaluator;
    Ok(GraphVertexUpsertStats {
        merge: merge_stats,
        branch,
        action_effects: observed as u64,
    })
}

// Read fields of the already admitted MERGE target with the same controlled
// historical projection used by public point reads. No full-row copy, new
// MATCH, source/result row, or second overlay. Directory/history/field/effect
// lookup and scalar/witness ownership are all charged to the action evaluator.
fn vertex_upsert_property<V: Vfs + Clone, A, C>(
    transaction: &WriteTxn,
    database: &Database<V>,
    vertex: VId,
    property: fgdb_delta_types::PropertyKeyId,
    control: &mut dyn FnMut(
        fgdb_gql::GlaExecutionEvent,
    ) -> VertexUpsertActionResult<(), WriteTxnError, A, C>,
) -> VertexUpsertActionResult<CanonicalScalar, WriteTxnError, A, C> {
    use crate::gql_exec::source::SourceEvent;
    use fgdb_gql::{GlaExecutionEvent, GqlQueryError, GraphVertexUpsertError};
    control(GlaExecutionEvent::Work)?;
    let element = ElementId::Vertex(vertex);
    let field = PointReadField::Property(property);
    let mut attempt = ProjectionReadAttempt {
        reads: &transaction.point_reads,
        element,
        accepted: false,
    };
    let mut source_control = |event| {
        control(match event {
            SourceEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
            SourceEvent::Work | SourceEvent::SnapshotRecord => GlaExecutionEvent::Work,
        })
    };
    let projected = transaction.point_projection_with_control(
        database,
        element,
        field,
        &mut source_control,
        &|error| GqlQueryError::Source(GraphVertexUpsertError::Staging(error)),
    )?;
    if let Some(value) = projected.property {
        point_payload_admission(value, &mut source_control)?;
    }
    source_control(SourceEvent::ScratchEntry)?;
    source_control(SourceEvent::ScratchEntry)?;
    transaction.point_reads.borrow_mut().record(element, field);
    source_control(SourceEvent::ScratchEntry)?;
    let value = projected.property.cloned().unwrap_or(CanonicalScalar::Null);
    attempt.accepted = true;
    Ok(value)
}
