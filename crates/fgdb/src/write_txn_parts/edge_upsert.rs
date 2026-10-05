// Relationship MERGE has one branch decision, evaluator allowance and native
// rollback boundary. Each SET clause freezes inputs before its writes; the
// following clause reads canonical staged effects, never a second graph model.

type EdgeUpsertActionResult<T, E, A, C> =
    Result<T, fgdb_gql::GqlQueryError<fgdb_gql::GraphEdgeUpsertError<E, A>, C>>;

#[allow(clippy::too_many_arguments)]
fn edge_upsert_actions<E, A, C>(
    upsert: &fgdb_gql::PreparedGraphEdgeUpsert,
    policy: fgdb_gql::GraphEdgeUpsertPolicy,
    merge_stats: fgdb_gql::GraphEdgeMergeStats,
    outcome: fgdb_gql::GraphEdgeMergeOutcome,
    mut checkpoint: impl FnMut() -> Result<(), C>,
    mut property: impl FnMut(
        EId,
        fgdb_delta_types::PropertyKeyId,
        &mut dyn FnMut(fgdb_gql::GlaExecutionEvent) -> EdgeUpsertActionResult<(), E, A, C>,
    ) -> EdgeUpsertActionResult<CanonicalScalar, E, A, C>,
    mut stage: impl FnMut(WriteBatch) -> EdgeUpsertActionResult<(), E, A, C>,
) -> EdgeUpsertActionResult<fgdb_gql::GraphEdgeUpsertStats, E, A, C> {
    use crate::gql_exec::source::SourceEvent;
    use fgdb_gql::{
        GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GqlQueryError,
        GraphEdgeMergeOutcome, GraphEdgeUpsertBranch, GraphEdgeUpsertError, GraphEdgeUpsertStats,
        GraphEdgeUpsertValue, GraphIntegerEvaluationError,
    };
    let (branch, selected) = match outcome {
        GraphEdgeMergeOutcome::NoInput => (GraphEdgeUpsertBranch::NoInput, &[][..]),
        GraphEdgeMergeOutcome::Matched(_) => (GraphEdgeUpsertBranch::Match, upsert.on_match()),
        GraphEdgeMergeOutcome::Created(_) => (GraphEdgeUpsertBranch::Create, upsert.on_create()),
    };
    let observed = upsert.action_count(branch) as u128;
    if observed > u128::from(policy.max_actions) {
        return Err(GqlQueryError::Source(GraphEdgeUpsertError::ActionLimit {
            limit: policy.max_actions,
            observed,
        }));
    }
    let mut evaluator = merge_stats.evaluator;
    let mut control = |event| -> EdgeUpsertActionResult<(), E, A, C> {
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
    // NoInput must skip the trailing clause as well as both ON branches.
    if let Some(edge) = outcome.edge() {
        for (clause, actions) in [selected, upsert.after()].into_iter().enumerate() {
            if actions.is_empty() {
                continue;
            }
            let mut batch = WriteBatch::new(upsert.merge().relation());
            for (action, definition) in actions.iter().enumerate() {
                control(GlaExecutionEvent::ScratchEntry)?;
                let value = match &definition.value {
                    GraphEdgeUpsertValue::Literal(value) => {
                        point_payload_admission(value.value(), &mut |event| {
                            control(match event {
                                SourceEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
                                _ => GlaExecutionEvent::Work,
                            })
                        })?;
                        value.value().clone()
                    }
                    GraphEdgeUpsertValue::Expression { properties, value } => {
                        for _ in properties {
                            control(GlaExecutionEvent::ScratchEntry)?;
                        }
                        let mut inputs = Vec::with_capacity(properties.len());
                        for key in properties {
                            inputs.push(fgdb_gql::algebra::GraphValue::Scalar(property(
                                edge,
                                *key,
                                &mut control,
                            )?));
                        }
                        value
                            .evaluate_scalar_with_control(&inputs, &mut control)
                            .map_err(|error| match error {
                                GraphIntegerEvaluationError::Control(error) => error,
                                GraphIntegerEvaluationError::Value(source) => {
                                    GqlQueryError::Source(GraphEdgeUpsertError::Expression {
                                        clause,
                                        action,
                                        source,
                                    })
                                }
                            })?
                    }
                };
                batch.set_edge_property(edge, definition.key, Some(value));
            }
            // All right-hand sides have succeeded before the first write of
            // this clause. The next clause must observe this native staging.
            stage(batch)?;
        }
    }
    // Refusal here still belongs to the outer all-or-nothing workspace.
    control(GlaExecutionEvent::Work)?;
    drop(control);
    Ok(GraphEdgeUpsertStats {
        merge: merge_stats,
        branch,
        action_effects: observed as u64,
        evaluator,
    })
}

// The same history, prepared-net projection, payload reservation and retained
// point witness as ordinary governed reads. Only the already selected EId is
// addressed. A missing field is NULL, not an instruction to remove a property.
fn edge_upsert_property<V: Vfs + Clone, A, C>(
    transaction: &WriteTxn,
    database: &Database<V>,
    edge: EId,
    property: fgdb_delta_types::PropertyKeyId,
    control: &mut dyn FnMut(
        fgdb_gql::GlaExecutionEvent,
    ) -> EdgeUpsertActionResult<(), WriteTxnError, A, C>,
) -> EdgeUpsertActionResult<CanonicalScalar, WriteTxnError, A, C> {
    use crate::gql_exec::source::SourceEvent;
    use fgdb_gql::{GlaExecutionEvent, GqlQueryError, GraphEdgeUpsertError};
    control(GlaExecutionEvent::Work)?;
    let element = ElementId::Edge(edge);
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
        &|error| GqlQueryError::Source(GraphEdgeUpsertError::Staging(error)),
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

impl WriteTxn {
    /// Execute directed MERGE, its selected ON clause and subsequent SET as one
    /// staged operation. Each clause has simultaneous assignment semantics;
    /// clauses share the MERGE's original evaluator and action allowances.
    /// NoInput runs no expression, property callback or branch.
    ///
    /// Failure or unwinding restores the original staged prefix but retains
    /// read/absence witnesses and does not reclaim issued external identities.
    /// This stages only; the caller still owns transaction completion.
    pub fn execute_graph_edge_upsert_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        upsert: &fgdb_gql::PreparedGraphEdgeUpsert,
        policy: fgdb_gql::GraphEdgeUpsertPolicy,
        allocate: impl FnMut(fgdb_gql::GraphEdgeMergeRequest) -> Result<ElementId, A>,
    ) -> Result<
        (
            fgdb_gql::GraphEdgeUpsertStats,
            fgdb_gql::GraphEdgeMergeOutcome,
        ),
        TxnGqlError<fgdb_gql::GraphEdgeUpsertError<WriteTxnError, A>>,
    > {
        use fgdb_gql::{GqlQueryError, GraphEdgeUpsertError};
        self.ensure_database(database)
            .map_err(|error| GqlQueryError::Source(GraphEdgeUpsertError::Staging(error)))?;
        cx.with_restriction(|| {
            let workspace = MutationProgramWorkspace::new(self);
            let (merge_stats, outcome) = workspace
                .txn
                .execute_graph_edge_merge_governed(
                    database,
                    cx,
                    upsert.merge(),
                    policy.merge,
                    allocate,
                )
                .map_err(|error| error.map_source(GraphEdgeUpsertError::Merge))?;
            let state = std::cell::RefCell::new((&mut *workspace.txn, &mut *database));
            let stats = edge_upsert_actions::<WriteTxnError, A, _>(
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
                        .map_err(|error| {
                            GqlQueryError::Source(GraphEdgeUpsertError::Staging(error))
                        })
                },
            )?;
            drop(state);
            workspace.accept();
            Ok((stats, outcome))
        })
    }
}
