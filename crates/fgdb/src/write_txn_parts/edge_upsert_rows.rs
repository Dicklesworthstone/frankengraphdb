// Relationship ON/SET actions over a chained write's carried rows. Hosts share
// the native occurrence collector and this controlled action evaluator; they
// own only admitted property reads, original-intent staging and final output.

/// One occurrence's complete target set, frozen before its actions. Preserve
/// the edge's own orientation for undirected matches and final projections.
pub(crate) struct EdgeUpsertRowTargets(
    pub(crate) Vec<(fgdb_gql::GraphEdgeMergeOutcome, (VId, VId))>,
);

impl EdgeMergeOutput for EdgeUpsertRowTargets {
    const PARALLEL: bool = true;

    fn for_pairs(pairs: usize) -> Option<Self> {
        (pairs <= 1).then(|| Self(Vec::new()))
    }

    fn record<C>(
        &mut self,
        outcome: fgdb_gql::GraphEdgeMergeOutcome,
        resolved: ResolvedEdge,
        control: &mut impl FnMut(Option<fgdb_gql::GlaExecutionEvent>) -> Result<(), C>,
    ) -> Result<(), C> {
        if outcome.edge().is_some() {
            control(Some(fgdb_gql::GlaExecutionEvent::ScratchEntry))?;
            self.0.push((outcome, resolved.endpoints));
        }
        Ok(())
    }

    fn result_rows(&self) -> u64 {
        self.0.len() as u64
    }
}

/// Evaluate the selected ON clause and trailing SET for one resolved edge.
/// The caller admits the cumulative effect count first. Every RHS of one
/// clause completes before its original assignments reach the staging host.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub(crate) fn edge_merge_row_actions_controlled<E, A, C>(
    (relation, edge): (RelationId, EId),
    (at, row): (usize, &fgdb_gql::algebra::GraphValueRow),
    clauses: [&[fgdb_gql::GraphEdgeMergeRowAction]; 2],
    policy: fgdb_gql::GqlQueryPolicy,
    evaluator: &mut fgdb_gql::GlaExecutionStats,
    checkpoint: impl FnMut() -> Result<(), C>,
    mut read: impl FnMut(
        fgdb_delta_types::PropertyKeyId,
        &mut dyn FnMut(fgdb_gql::GlaExecutionEvent) -> EdgeUpsertActionResult<(), E, A, C>,
    ) -> EdgeUpsertActionResult<fgdb_gql::algebra::GraphValue, E, A, C>,
    mut stage: impl FnMut(WriteBatch) -> EdgeUpsertActionResult<(), E, A, C>,
) -> EdgeUpsertActionResult<(), E, A, C> {
    use crate::gql_exec::source::SourceEvent;
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::{
        GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GqlQueryError, GraphEdgeMergeError,
        GraphEdgeMergeRowAction, GraphEdgeMergeRowInput, GraphEdgeUpsertError,
        GraphEdgeUpsertValue, GraphIntegerEvaluationError,
    };
    let checkpoint = std::cell::RefCell::new(checkpoint);
    let mut control = |event| -> EdgeUpsertActionResult<(), E, A, C> {
        checkpoint.borrow_mut()().map_err(GqlQueryError::Interrupted)?;
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
    };
    let payload = |event| match event {
        SourceEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
        _ => GlaExecutionEvent::Work,
    };
    for (clause, actions) in clauses.into_iter().enumerate() {
        if actions.is_empty() {
            continue;
        }
        let mut batch = WriteBatch::new(relation);
        for (action, definition) in actions.iter().enumerate() {
            control(GlaExecutionEvent::Work)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            let expression = |source| {
                GqlQueryError::Source(GraphEdgeUpsertError::Expression {
                    clause,
                    action,
                    source,
                })
            };
            // The vertex a row-vertex assignment writes (fgdb-at06q). A NULL
            // one, from OPTIONAL MATCH, takes no assignment, as SET on NULL.
            let vertex = match definition.vertex_column() {
                None => None,
                Some(column) => match row.values().get(column) {
                    Some(GraphValue::Vertex(vertex)) => Some(*vertex),
                    Some(GraphValue::Scalar(CanonicalScalar::Null)) => continue,
                    _ => {
                        return Err(GqlQueryError::Source(GraphEdgeUpsertError::Merge(
                            GraphEdgeMergeError::InputSchema { row: at, column },
                        )));
                    }
                },
            };
            let (key, written) = match definition {
                GraphEdgeMergeRowAction::Edge(definition) => {
                    let written = match &definition.value {
                        GraphEdgeUpsertValue::Literal(value) => {
                            point_scalar_payload_admission(value.value(), &mut |event| {
                                control(payload(event))
                            })?;
                            CanonicalPropertyValue::Scalar(value.value().clone())
                        }
                        GraphEdgeUpsertValue::Stored(value) => {
                            point_payload_admission(value, &mut |event| control(payload(event)))?;
                            value.clone()
                        }
                        GraphEdgeUpsertValue::Expression { .. }
                        | GraphEdgeUpsertValue::Composite { .. } => {
                            return Err(GqlQueryError::Source(GraphEdgeUpsertError::Merge(
                                GraphEdgeMergeError::InputSchema {
                                    row: at,
                                    column: action,
                                },
                            )));
                        }
                    };
                    (definition.key, written)
                }
                GraphEdgeMergeRowAction::Expression { key, inputs, value } => {
                    let mut frame = Vec::new();
                    for input in inputs {
                        control(GlaExecutionEvent::ScratchEntry)?;
                        frame.push(match *input {
                            GraphEdgeMergeRowInput::Property(key) => {
                                control(GlaExecutionEvent::Work)?;
                                read(key, &mut control)?
                            }
                            GraphEdgeMergeRowInput::Column(column) => row
                                .values()
                                .get(column)
                                .ok_or(GqlQueryError::Source(GraphEdgeUpsertError::Merge(
                                    GraphEdgeMergeError::InputSchema { row: at, column },
                                )))?
                                .copy_with_control(&mut control)?,
                        });
                    }
                    let value = value
                        .evaluate_scalar_with_control(&frame, &mut control)
                        .map_err(|error| match error {
                            GraphIntegerEvaluationError::Control(error) => error,
                            GraphIntegerEvaluationError::Value(error) => expression(error),
                        })?;
                    (*key, CanonicalPropertyValue::Scalar(value))
                }
                GraphEdgeMergeRowAction::Value { key, value }
                | GraphEdgeMergeRowAction::RowVertex { key, value, .. } => {
                    let written = value
                        .written(row, &mut control)
                        .map_err(|error| match error {
                            GraphIntegerEvaluationError::Control(error) => error,
                            GraphIntegerEvaluationError::Value(error) => expression(error),
                        })?
                        .ok_or(GqlQueryError::Source(
                            GraphEdgeUpsertError::NonScalarValue { clause, action },
                        ))?;
                    point_payload_admission(&written, &mut |event| control(payload(event)))?;
                    (*key, written)
                }
            };
            match vertex {
                Some(vertex) => batch.set_vertex_property_value(vertex, key, Some(written)),
                None => batch.set_edge_property_value(edge, key, Some(written)),
            };
        }
        checkpoint.borrow_mut()().map_err(GqlQueryError::Interrupted)?;
        stage(batch)?;
    }
    control(GlaExecutionEvent::Work)
}

/// One stored field through the native projection/witness owner. Unlike the
/// scalar-only standalone action API, row expressions may inspect a stored
/// list using the shared read-value language; no value is copied before its
/// complete payload has been admitted.
fn edge_upsert_row_property<V: Vfs + Clone, A, C>(
    transaction: &WriteTxn,
    database: &Database<V>,
    element: ElementId,
    key: fgdb_delta_types::PropertyKeyId,
    control: &mut dyn FnMut(
        fgdb_gql::GlaExecutionEvent,
    ) -> EdgeUpsertActionResult<(), WriteTxnError, A, C>,
) -> EdgeUpsertActionResult<fgdb_gql::algebra::GraphValue, WriteTxnError, A, C> {
    use crate::gql_exec::source::SourceEvent;
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::{GlaExecutionEvent, GqlQueryError, GraphEdgeUpsertError};
    control(GlaExecutionEvent::Work)?;
    let field = PointReadField::Property(key);
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
    let value = match projected.property {
        None => GraphValue::Scalar(CanonicalScalar::Null),
        Some(value) => GraphValue::from_property_value(value).ok_or(GqlQueryError::Data(
            fgdb_gql::GraphIntegerError {
                instruction: 0,
                kind: fgdb_gql::GraphIntegerErrorKind::StoredValueBounds,
            },
        ))?,
    };
    attempt.accepted = true;
    Ok(value)
}

type EdgeUpsertChainResult<A> = Result<
    (
        fgdb_gql::insertion::GraphInsertStats,
        fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
        Option<fgdb_gql::GraphWriteStepReceipt>,
        u64,
    ),
    TxnGqlError<fgdb_gql::GraphWriteChainError<WriteTxnError, A>>,
>;

impl WriteTxn {
    /// One relationship action step inside the chain's existing rollback
    /// boundary. Select its input once, resolve each occurrence against prior
    /// effects, then read every continuation/RETURN from the final overlay.
    #[allow(clippy::too_many_arguments)]
    fn execute_graph_edge_upsert_chain_step<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        step: &fgdb_gql::GraphWriteChainStep,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
        owned: Option<Vec<fgdb_gql::algebra::GraphValueRow>>,
        mut allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> EdgeUpsertChainResult<A> {
        use fgdb_gql::algebra::{GraphValue, GraphValueRow};
        use fgdb_gql::insertion::{GraphInsertRequest, GraphInsertStats};
        use fgdb_gql::{
            GlaExecutionEvent, GqlExecutionStats, GqlQueryError, GqlQueryExecution,
            GraphEdgeMergeBinding, GraphEdgeMergeError, GraphEdgeMergeOutcome,
            GraphEdgeMergePolicy, GraphEdgeUpsertBranch, GraphEdgeUpsertError,
            GraphEdgeUpsertQueryError, GraphMutationError, GraphMutationQueryError,
            GraphWriteChainError, GraphWriteChainStep, GraphWriteStepReceipt,
        };
        let chained =
            |error| GraphWriteChainError::EdgeUpsert(GraphEdgeUpsertQueryError::Upsert(error));
        let invalid = || {
            GqlQueryError::Source(chained(GraphEdgeUpsertError::Merge(
                GraphEdgeMergeError::InvalidSourceStatistics,
            )))
        };
        let upsert = match step {
            GraphWriteChainStep::EdgeUpsert(upsert)
            | GraphWriteChainStep::EdgeUpsertRows { upsert, .. }
            | GraphWriteChainStep::EdgeUpsertQuery { upsert, .. } => upsert,
            _ => return Err(invalid()),
        };
        let merge = upsert.merge();
        if let GraphWriteChainStep::EdgeUpsertQuery { query, .. } = step
            && query.merge() != merge
        {
            return Err(invalid());
        }
        // Private input/target occurrences are never delivered result rows.
        let mut private = policy.query;
        private.rows = match private.rows.max_snapshot_records() {
            Some(limit) => fgdb_gql::GqlExecutionBudget::snapshot_records(limit),
            None => fgdb_gql::GqlExecutionBudget::UNLIMITED,
        };
        let selected = merge
            .select_governed::<WriteTxnError, A, _>(
                owned,
                private,
                |pattern, allowance| {
                    self.execute_graph_pattern_governed(database, cx, pattern, allowance)
                },
                || cx.checkpoint(),
            )
            .map_err(|error| {
                error.map_source(|error| chained(GraphEdgeUpsertError::Merge(error)))
            })?;
        if u64::try_from(selected.value.len()).ok() != Some(selected.rows.result_rows) {
            return Err(invalid());
        }
        let mut stats = GraphInsertStats {
            selection: GqlExecutionStats {
                snapshot_records: selected.rows.snapshot_records,
                result_rows: 0,
            },
            evaluator: selected.evaluator,
            created_vertices: 0,
            created_edges: 0,
        };
        let retain_input = !matches!(step, GraphWriteChainStep::EdgeUpsert(_));
        let retain_receipt = !matches!(step, GraphWriteChainStep::EdgeUpsertQuery { .. });
        let mut input = Vec::new();
        let mut pairs = Vec::new();
        let mut outcomes = Vec::new();
        let mut created = Vec::new();
        let mut effects = 0_u64;
        for (at, row) in selected.value.into_iter().enumerate() {
            // Transfer the already admitted row through the real collector;
            // do not deep-copy a carried map to construct a singleton source.
            vertex_targets_event::<GraphWriteChainError<WriteTxnError, A>>(
                cx,
                private,
                &mut stats.evaluator,
                GlaExecutionEvent::ScratchEntry,
            )?;
            let remaining = policy
                .max_edges
                .checked_sub(stats.created_edges)
                .ok_or_else(invalid)?;
            let proposal = collect_edge_merge_output::<EdgeUpsertRowTargets, WriteTxnError, A, _>(
                merge,
                GraphEdgeMergePolicy::new(private).with_creation_limit(remaining),
                |_| {
                    Ok(GqlQueryExecution {
                        value: vec![row],
                        rows: GqlExecutionStats {
                            snapshot_records: stats.selection.snapshot_records,
                            result_rows: 1,
                        },
                        evaluator: stats.evaluator,
                    })
                },
                |source, destination| {
                    self.merge_candidate_edges(database, merge, source, destination, || {
                        cx.checkpoint()
                    })
                },
                |_| allocate(GraphInsertRequest::Edge { row: at, edge: 0 }),
                || cx.checkpoint(),
            )
            .map_err(|error| {
                error.map_source(|error| {
                    chained(GraphEdgeUpsertError::Merge(match error {
                        GraphEdgeMergeError::CreationLimit { observed, .. } => {
                            GraphEdgeMergeError::CreationLimit {
                                limit: policy.max_edges,
                                observed: u128::from(stats.created_edges) + observed,
                            }
                        }
                        GraphEdgeMergeError::InputSchema { column, .. } => {
                            GraphEdgeMergeError::InputSchema { row: at, column }
                        }
                        GraphEdgeMergeError::NullEndpoint { column, .. } => {
                            GraphEdgeMergeError::NullEndpoint { row: at, column }
                        }
                        error => error,
                    }))
                })
            })?;
            let EdgeMergeProposal {
                stats: merged,
                outcome,
                creation,
                mut selected,
            } = proposal;
            stats.selection.snapshot_records = merged
                .match_selection
                .snapshot_records
                .checked_add(merged.overlay_edges)
                .ok_or_else(invalid)?;
            stats.created_edges = stats
                .created_edges
                .checked_add(merged.created_edges)
                .ok_or_else(invalid)?;
            stats.evaluator = merged.evaluator;
            if selected.len() != 1 {
                return Err(invalid());
            }
            let row = selected.pop().ok_or_else(invalid)?;
            if let Some(batch) = creation {
                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                self.stage_edge_upsert_row_batch(database, batch)
                    .map_err(|error| {
                        GqlQueryError::Source(chained(GraphEdgeUpsertError::Staging(error)))
                    })?;
            }
            let input_at = input.len();
            // The collector froze all parallel matches for this occurrence.
            // A key rewrite below affects only the next occurrence's lookup.
            for (outcome, endpoints) in outcome.0 {
                let (edge, branch, on) = match outcome {
                    GraphEdgeMergeOutcome::Matched(edge) => (
                        edge,
                        GraphEdgeUpsertBranch::Match,
                        &upsert.actions().on_match,
                    ),
                    GraphEdgeMergeOutcome::Created(edge) => (
                        edge,
                        GraphEdgeUpsertBranch::Create,
                        &upsert.actions().on_create,
                    ),
                    GraphEdgeMergeOutcome::NoInput => return Err(invalid()),
                };
                let observed = u128::from(effects) + upsert.action_count(branch) as u128;
                if observed > u128::from(policy.max_mutation_effects) {
                    return Err(GqlQueryError::Source(GraphWriteChainError::Mutation(
                        GraphMutationQueryError::Mutation(GraphMutationError::EffectLimit {
                            limit: policy.max_mutation_effects,
                            observed,
                        }),
                    )));
                }
                let state = std::cell::RefCell::new((&mut *self, &mut *database));
                edge_merge_row_actions_controlled::<WriteTxnError, A, _>(
                    (merge.relation(), edge),
                    (at, &row),
                    [on, &upsert.actions().after],
                    private,
                    &mut stats.evaluator,
                    || cx.checkpoint(),
                    |key, control| {
                        let state = state.borrow();
                        edge_upsert_row_property(
                            state.0,
                            state.1,
                            ElementId::Edge(edge),
                            key,
                            control,
                        )
                    },
                    |batch| {
                        let mut state = state.borrow_mut();
                        let (transaction, database) = &mut *state;
                        transaction
                            .stage_edge_upsert_row_batch(database, batch)
                            .map_err(|error| {
                                GqlQueryError::Source(GraphEdgeUpsertError::Staging(error))
                            })
                    },
                )
                .map_err(|error| error.map_source(chained))?;
                effects = observed as u64;
                if retain_receipt {
                    vertex_targets_event::<GraphWriteChainError<WriteTxnError, A>>(
                        cx,
                        private,
                        &mut stats.evaluator,
                        GlaExecutionEvent::ScratchEntry,
                    )?;
                    outcomes.push(outcome);
                    if outcome.created() {
                        vertex_targets_event::<GraphWriteChainError<WriteTxnError, A>>(
                            cx,
                            private,
                            &mut stats.evaluator,
                            GlaExecutionEvent::ScratchEntry,
                        )?;
                        created.push(edge);
                    }
                }
                if retain_input {
                    vertex_targets_event::<GraphWriteChainError<WriteTxnError, A>>(
                        cx,
                        private,
                        &mut stats.evaluator,
                        GlaExecutionEvent::ScratchEntry,
                    )?;
                    pairs.push((input_at, edge, endpoints));
                }
            }
            if retain_input {
                vertex_targets_event::<GraphWriteChainError<WriteTxnError, A>>(
                    cx,
                    private,
                    &mut stats.evaluator,
                    GlaExecutionEvent::ScratchEntry,
                )?;
                input.push(row);
            }
        }
        let result = if let GraphWriteChainStep::EdgeUpsertQuery { query, .. } = step {
            let returning = (|| -> EdgeUpsertActionResult<Vec<Vec<GraphValue>>, WriteTxnError, A, Box<asupersync::error::Error>> {
                let mut control = |event| edge_upsert_batch_event::<A>(cx, private, &mut stats.evaluator, event);
                let mut returning = Vec::new();
                for (at, edge, endpoints) in pairs {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    let row: &GraphValueRow = &input[at];
                    let mut cells = Vec::new();
                    for binding in query.bindings() {
                        control(GlaExecutionEvent::ScratchEntry)?;
                        let endpoint = |start| if start { endpoints.0 } else { endpoints.1 };
                        let property = match *binding {
                            GraphEdgeMergeBinding::Input(column) => {
                                cells.push(row.values().get(column).ok_or(GqlQueryError::Source(
                                    GraphEdgeUpsertError::Merge(GraphEdgeMergeError::InputSchema { row: at, column }),
                                ))?.copy_with_control(&mut control)?);
                                continue;
                            }
                            GraphEdgeMergeBinding::Edge => { cells.push(GraphValue::Edge(edge)); continue; }
                            GraphEdgeMergeBinding::Endpoint { start } => { cells.push(GraphValue::Vertex(endpoint(start))); continue; }
                            GraphEdgeMergeBinding::Property(key) => Some((ElementId::Edge(edge), key)),
                            GraphEdgeMergeBinding::EndpointProperty { start, key } => Some((ElementId::Vertex(endpoint(start)), key)),
                            GraphEdgeMergeBinding::InputProperty { column, key } => match row.values().get(column) {
                                Some(GraphValue::Vertex(vertex)) => Some((ElementId::Vertex(*vertex), key)),
                                Some(GraphValue::Edge(edge)) => Some((ElementId::Edge(*edge), key)),
                                _ => None,
                            },
                        };
                        cells.push(match property {
                            Some((element, key)) => edge_upsert_row_property(self, database, element, key, &mut control)?,
                            None => GraphValue::Scalar(CanonicalScalar::Null),
                        });
                    }
                    returning.push(cells);
                }
                Ok(returning)
            })().map_err(|error| error.map_source(chained))?;
            let failure = std::cell::RefCell::new(None);
            let result = query
                .finish_governed_reading(
                    returning,
                    stats.selection,
                    stats.evaluator,
                    policy.query,
                    self.element_reader_with_checkpoint(database, cx, &failure),
                    || match failure.borrow_mut().take() {
                        Some(error) => Err(error),
                        None => cx.checkpoint(),
                    },
                )
                .map_err(|error| {
                    error.map_source(|error| {
                        GraphWriteChainError::EdgeUpsert(GraphEdgeUpsertQueryError::Returning(
                            error,
                        ))
                    })
                });
            match failure.into_inner() {
                Some(error) => Err(GqlQueryError::Interrupted(error)),
                None => result,
            }?
        } else {
            let mut continued = Vec::new();
            for (at, edge, _) in pairs {
                continued.push(step.continuation_row_controlled(
                    &input[at],
                    ElementId::Edge(edge),
                    &mut |event| {
                        vertex_targets_event::<GraphWriteChainError<WriteTxnError, A>>(
                            cx,
                            private,
                            &mut stats.evaluator,
                            event,
                        )
                    },
                )?);
            }
            GqlQueryExecution {
                value: continued,
                rows: stats.selection,
                evaluator: stats.evaluator,
            }
        };
        stats.selection = result.rows;
        stats.evaluator = result.evaluator;
        let receipt = retain_receipt.then_some(match outcomes.as_slice() {
            [] => GraphWriteStepReceipt::EdgeUpsert {
                outcome: GraphEdgeMergeOutcome::NoInput,
            },
            [outcome] => GraphWriteStepReceipt::EdgeUpsert { outcome: *outcome },
            _ => GraphWriteStepReceipt::EdgeUpsertBatch { outcomes, created },
        });
        cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
        Ok((stats, result, receipt, effects))
    }

    fn stage_edge_upsert_row_batch<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        batch: WriteBatch,
    ) -> Result<(), WriteTxnError> {
        if self
            .staged
            .iter()
            .all(|staged| staged.relation == batch.relation)
        {
            self.write(database, batch)
        } else {
            self.write_atomic(database, vec![batch])
        }
    }
}
