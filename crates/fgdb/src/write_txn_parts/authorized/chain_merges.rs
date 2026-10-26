//! Relationship MERGE steps inside the authorized chain's existing workspace.
//! Native selection and pair reduction own bag multiplicity and creation.
//! This adapter supplies scoped reads and controlled private continuation rows.

use super::super::{Database, Execution, Vfs, WriteTxn, WriteTxnError, edge_merge, selection};
use super::{Fault, admit_pattern};
use crate::write_txn::{EdgeUpsertRowTargets, edge_merge_row_actions_controlled};
use fgdb_delta_types::ElementId;
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertStats};
use fgdb_gql::{
    GlaExecutionEvent, GlaExecutionStats, GlaLimitDimension, GlaLimitExceeded, GqlExecutionStats,
    GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphEdgeMergeBinding, GraphEdgeMergeError,
    GraphEdgeMergeOutcome, GraphEdgeMergePolicy, GraphEdgeMergeQueryError, GraphEdgeMergeRowAction,
    GraphEdgeUpsertBranch, GraphEdgeUpsertError, GraphEdgeUpsertQueryError, GraphMutationError,
    GraphMutationQueryError, GraphWriteChainError, GraphWriteChainStep, PreparedGraphEdgeMerge,
    PreparedGraphEdgeMergeQuery,
};
use fgdb_types::{CanonicalScalar, EId, QueryCx, VId};
use fgdb_warden::{Error, PlannerPredicates};
use std::cell::RefCell;

fn source(error: WriteTxnError) -> Fault {
    GqlQueryError::Source(GraphWriteChainError::EdgeMerge(
        GraphEdgeMergeQueryError::Merge(GraphEdgeMergeError::Source(error)),
    ))
}

fn invalid() -> Fault {
    GqlQueryError::Source(GraphWriteChainError::EdgeMerge(
        GraphEdgeMergeQueryError::Merge(GraphEdgeMergeError::InvalidSourceStatistics),
    ))
}

type ActionFault = GqlQueryError<GraphEdgeUpsertError<WriteTxnError, WriteTxnError>, WriteTxnError>;

fn action_source(error: WriteTxnError) -> ActionFault {
    GqlQueryError::Source(GraphEdgeUpsertError::Staging(error))
}

fn action_error(error: ActionFault) -> Fault {
    error.map_source(|error| {
        GraphWriteChainError::EdgeUpsert(GraphEdgeUpsertQueryError::Upsert(error))
    })
}

fn action_context(error: Fault) -> Fault {
    error.map_source(|error| match error {
        GraphWriteChainError::EdgeMerge(GraphEdgeMergeQueryError::Merge(error)) => {
            GraphWriteChainError::EdgeUpsert(GraphEdgeUpsertQueryError::Upsert(
                GraphEdgeUpsertError::Merge(error),
            ))
        }
        GraphWriteChainError::EdgeMerge(GraphEdgeMergeQueryError::Returning(error)) => {
            GraphWriteChainError::EdgeUpsert(GraphEdgeUpsertQueryError::Returning(error))
        }
        error => error,
    })
}

fn admit_actions(
    clauses: [&[GraphEdgeMergeRowAction]; 2],
    scope: &PlannerPredicates,
) -> Result<(), ActionFault> {
    // Only the selected branch is authority-relevant. Check every target key
    // before evaluating any RHS, including clauses whose value is unchanged.
    for action in clauses.into_iter().flatten() {
        if !scope.allows_property(action.key()) {
            return Err(action_source(WriteTxnError::Authorization(
                Error::ScopeDenied,
            )));
        }
    }
    Ok(())
}

fn definition(step: &GraphWriteChainStep) -> Option<&PreparedGraphEdgeMerge> {
    match step {
        GraphWriteChainStep::EdgeMerge(merge)
        | GraphWriteChainStep::EdgeMergeRows { merge, .. } => Some(merge),
        GraphWriteChainStep::EdgeMergeQuery(query) => Some(query.merge()),
        GraphWriteChainStep::EdgeUpsert(upsert)
        | GraphWriteChainStep::EdgeUpsertRows { upsert, .. }
        | GraphWriteChainStep::EdgeUpsertQuery { upsert, .. } => Some(upsert.merge()),
        _ => None,
    }
}

/// Called during whole-chain admission, before the workspace or any identity.
/// A false result leaves the step to another owner. All relationship MERGEs
/// require ReadWrite, including NoInput.
pub(super) fn preflight(
    step: &GraphWriteChainStep,
    scope: &PlannerPredicates,
) -> Result<bool, Fault> {
    let Some(merge) = definition(step) else {
        return Ok(false);
    };
    // The public prepared step must not pair actions with a different RETURN
    // definition. Refuse the entire chain before observations or allocation.
    if let GraphWriteChainStep::EdgeUpsertQuery { upsert, query } = step
        && query.merge() != upsert.merge()
    {
        return Err(action_context(invalid()));
    }
    if !scope.rights().can_read() {
        return Err(source(WriteTxnError::Authorization(
            Error::PermissionDenied,
        )));
    }
    if !scope.allows_relation(merge.relation())
        || merge.property_keys().any(|key| !scope.allows_property(key))
    {
        return Err(source(WriteTxnError::Authorization(Error::ScopeDenied)));
    }
    if let Some(pattern) = merge.selection() {
        admit_pattern(pattern, scope).map_err(source)?;
    }
    if let GraphWriteChainStep::EdgeMergeQuery(query)
    | GraphWriteChainStep::EdgeUpsertQuery { query, .. } = step
    {
        for binding in query.bindings() {
            if let GraphEdgeMergeBinding::Property(key)
            | GraphEdgeMergeBinding::EndpointProperty { key, .. }
            | GraphEdgeMergeBinding::InputProperty { key, .. } = binding
                && !scope.allows_property(*key)
            {
                return Err(source(WriteTxnError::Authorization(Error::ScopeDenied)));
            }
        }
    }
    Ok(true)
}

fn event<E, Clock: FnMut() -> u64>(
    cx: &QueryCx,
    execution: &RefCell<&mut Execution<'_, '_, Clock>>,
    policy: GqlQueryPolicy,
    stats: &mut GlaExecutionStats,
    kind: GlaExecutionEvent,
) -> Result<(), GqlQueryError<E, WriteTxnError>> {
    cx.checkpoint()
        .map_err(WriteTxnError::Interrupted)
        .map_err(GqlQueryError::Interrupted)?;
    execution
        .borrow_mut()
        .checkpoint()
        .map_err(GqlQueryError::Interrupted)?;
    let work = u128::from(stats.work_units) + 1;
    let scratch =
        u128::from(stats.scratch_entries) + u128::from(kind == GlaExecutionEvent::ScratchEntry);
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
    stats.work_units = work as u64;
    stats.scratch_entries = scratch as u64;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn property<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &WriteTxn,
    database: &Database<V>,
    cx: &QueryCx,
    element: ElementId,
    key: fgdb_delta_types::PropertyKeyId,
    scope: &PlannerPredicates,
    policy: GqlQueryPolicy,
    evaluator: &mut GlaExecutionStats,
    execution: &RefCell<&mut Execution<'_, '_, Clock>>,
) -> Result<GraphValue, Fault> {
    let mut control = |kind| event(cx, execution, policy, evaluator, kind);
    let value = selection::property(
        transaction,
        database,
        element,
        key,
        scope,
        || {
            cx.checkpoint()
                .map_err(WriteTxnError::Interrupted)
                .map_err(GqlQueryError::Interrupted)?;
            execution
                .borrow_mut()
                .poll()
                .map_err(GqlQueryError::Interrupted)
        },
        &mut control,
        source,
    )?;
    GraphValue::admit_property_value(&value, &mut control)
}

#[allow(clippy::too_many_arguments)]
fn returning_row<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &WriteTxn,
    database: &Database<V>,
    cx: &QueryCx,
    query: &PreparedGraphEdgeMergeQuery,
    input: &GraphValueRow,
    edge: EId,
    endpoints: (VId, VId),
    scope: &PlannerPredicates,
    policy: GqlQueryPolicy,
    evaluator: &mut GlaExecutionStats,
    execution: &RefCell<&mut Execution<'_, '_, Clock>>,
) -> Result<Vec<GraphValue>, Fault> {
    event(
        cx,
        execution,
        policy,
        evaluator,
        GlaExecutionEvent::ScratchEntry,
    )?;
    let mut cells = Vec::new();
    for binding in query.bindings() {
        event(
            cx,
            execution,
            policy,
            evaluator,
            GlaExecutionEvent::ScratchEntry,
        )?;
        let endpoint = |start| if start { endpoints.0 } else { endpoints.1 };
        let cell = match *binding {
            GraphEdgeMergeBinding::Input(column) => input
                .values()
                .get(column)
                .ok_or_else(invalid)?
                .copy_with_control(&mut |kind| event(cx, execution, policy, evaluator, kind))?,
            GraphEdgeMergeBinding::Edge => GraphValue::Edge(edge),
            GraphEdgeMergeBinding::Endpoint { start } => GraphValue::Vertex(endpoint(start)),
            GraphEdgeMergeBinding::Property(key) => property(
                transaction,
                database,
                cx,
                ElementId::Edge(edge),
                key,
                scope,
                policy,
                evaluator,
                execution,
            )?,
            GraphEdgeMergeBinding::EndpointProperty { start, key } => property(
                transaction,
                database,
                cx,
                ElementId::Vertex(endpoint(start)),
                key,
                scope,
                policy,
                evaluator,
                execution,
            )?,
            GraphEdgeMergeBinding::InputProperty { column, key } => {
                let element = match input.values().get(column) {
                    Some(GraphValue::Vertex(vertex)) => Some(ElementId::Vertex(*vertex)),
                    Some(GraphValue::Edge(edge)) => Some(ElementId::Edge(*edge)),
                    _ => None,
                };
                match element {
                    Some(element) => property(
                        transaction,
                        database,
                        cx,
                        element,
                        key,
                        scope,
                        policy,
                        evaluator,
                        execution,
                    )?,
                    None => GraphValue::Scalar(CanonicalScalar::Null),
                }
            }
        };
        cells.push(cell);
    }
    Ok(cells)
}

/// Resolve one whole relationship-MERGE step, retaining the native pair
/// reducer's canonical order and parallel matches. The chain owns rollback,
/// final signed-row admission, authorization digest and the sole completion.
///
/// The native creation-only profile also supplies held endpoint vertices.
/// Existence reads keep the chain-entry overlay and original incidence gaps;
/// every new relationship is authorized before joining the held effects.
#[allow(clippy::too_many_arguments)]
pub(super) fn execute<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    step: &GraphWriteChainStep,
    policy: GraphInsertPolicy,
    owned: Option<Vec<GraphValueRow>>,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    frozen: Option<&mut crate::write_txn::FrozenCreations>,
) -> Result<(GraphInsertStats, GqlQueryExecution<GraphValueRow>), Fault> {
    if !preflight(step, scope)? {
        return Err(invalid());
    }
    let merge = definition(step).ok_or_else(invalid)?;
    let allowance = GraphEdgeMergePolicy::new(policy.query).with_creation_limit(policy.max_edges);
    let allowance = match step {
        GraphWriteChainStep::EdgeMergeQuery(query) => query.merge_policy(allowance),
        _ => allowance,
    };
    cx.with_restriction(|| {
        let selected = {
            let controls = RefCell::new(&mut *execution);
            merge
                .select_governed::<WriteTxnError, WriteTxnError, WriteTxnError>(
                    owned,
                    allowance.query,
                    |pattern, allowance| {
                        admit_pattern(pattern, scope).map_err(GqlQueryError::Source)?;
                        selection::select_overlay(
                            transaction,
                            database,
                            cx,
                            pattern,
                            scope,
                            allowance,
                            &controls,
                        )
                    },
                    || {
                        cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        controls.borrow_mut().checkpoint()
                    },
                )
                .map_err(|error| {
                    error.map_source(|error| {
                        GraphWriteChainError::EdgeMerge(GraphEdgeMergeQueryError::Merge(error))
                    })
                })?
        };
        let (merged, resolved, selected) = edge_merge::apply_selected(
            transaction,
            database,
            cx,
            merge,
            allowance,
            scope,
            execution,
            selected,
            frozen,
        )
        .map_err(|error| {
            error.map_source(|error| {
                GraphWriteChainError::EdgeMerge(GraphEdgeMergeQueryError::Merge(error))
            })
        })?;
        // Existence candidates already spent the step's snapshot allowance.
        // Preserve them across steps instead of resetting that part of the cap.
        let records = merged
            .match_selection
            .snapshot_records
            .checked_add(merged.overlay_edges)
            .ok_or_else(invalid)?;
        let mut stats = GraphInsertStats {
            selection: GqlExecutionStats {
                snapshot_records: records,
                result_rows: 0,
            },
            evaluator: merged.evaluator,
            created_vertices: 0,
            created_edges: merged.created_edges,
        };
        let controls = RefCell::new(&mut *execution);
        let mut continued = Vec::new();
        let mut returning = Vec::new();
        if !matches!(step, GraphWriteChainStep::EdgeMerge(_)) {
            for (row_at, input) in selected.iter().enumerate() {
                event(
                    cx,
                    &controls,
                    policy.query,
                    &mut stats.evaluator,
                    GlaExecutionEvent::Work,
                )?;
                let src = input
                    .values()
                    .get(merge.source_column())
                    .and_then(GraphValue::as_vertex)
                    .ok_or_else(invalid)?;
                let dst = input
                    .values()
                    .get(merge.destination_column())
                    .and_then(GraphValue::as_vertex)
                    .ok_or_else(invalid)?;
                let edges = resolved.edges_for((src, dst), row_at).ok_or_else(invalid)?;
                for &(edge, endpoints) in edges {
                    match step {
                        GraphWriteChainStep::EdgeMergeRows { .. } => {
                            continued.push(step.continuation_row_controlled(
                                input,
                                ElementId::Edge(edge),
                                &mut |kind| {
                                    event(cx, &controls, policy.query, &mut stats.evaluator, kind)
                                },
                            )?);
                        }
                        GraphWriteChainStep::EdgeMergeQuery(query) => {
                            returning.push(returning_row(
                                transaction,
                                database,
                                cx,
                                query,
                                input,
                                edge,
                                endpoints,
                                scope,
                                policy.query,
                                &mut stats.evaluator,
                                &controls,
                            )?);
                        }
                        _ => return Err(invalid()),
                    }
                }
            }
        }
        let result = if let GraphWriteChainStep::EdgeMergeQuery(query) = step {
            query
                .finish_governed_reading::<WriteTxnError, WriteTxnError>(
                    returning,
                    stats.selection,
                    stats.evaluator,
                    policy.query,
                    |element, key| {
                        selection::returning_element_property(
                            transaction,
                            database,
                            cx,
                            element,
                            key,
                            scope,
                            &controls,
                        )
                    },
                    || {
                        cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        controls.borrow_mut().checkpoint()
                    },
                )
                .map_err(|error| {
                    error.map_source(|error| {
                        GraphWriteChainError::EdgeMerge(GraphEdgeMergeQueryError::Returning(error))
                    })
                })?
        } else {
            GqlQueryExecution {
                value: continued,
                rows: stats.selection,
                evaluator: stats.evaluator,
            }
        };
        stats.selection = result.rows;
        stats.evaluator = result.evaluator;
        Ok((stats, result))
    })
}

/// Run row-fed relationship actions in the chain's one authorized workspace.
/// Freeze the input once, then resolve each occurrence against its predecessor's
/// writes. Parallel matches belong to that occurrence even if an action rekeys
/// one of them. Continuations and RETURN observe the final staged state.
#[allow(clippy::too_many_arguments)]
pub(super) fn execute_actions<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    step: &GraphWriteChainStep,
    policy: GraphInsertPolicy,
    owned: Option<Vec<GraphValueRow>>,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
) -> Result<(GraphInsertStats, GqlQueryExecution<GraphValueRow>, u64), Fault> {
    if !preflight(step, scope).map_err(action_context)? {
        return Err(action_context(invalid()));
    }
    let upsert = match step {
        GraphWriteChainStep::EdgeUpsert(upsert)
        | GraphWriteChainStep::EdgeUpsertRows { upsert, .. }
        | GraphWriteChainStep::EdgeUpsertQuery { upsert, .. } => upsert,
        _ => return Err(action_context(invalid())),
    };
    let merge = upsert.merge();
    // The selected input is private. Only the final RETURN spends output rows;
    // source/work/scratch bounds still cover selection and all occurrences.
    let private = GqlQueryPolicy::new(
        policy.query.rows.max_snapshot_records().unwrap_or(u64::MAX),
        u64::MAX,
        policy.query.evaluator.max_work_units,
        policy.query.evaluator.max_scratch_entries,
    );
    cx.with_restriction(|| {
        let selected = {
            let controls = RefCell::new(&mut *execution);
            merge
                .select_governed::<WriteTxnError, WriteTxnError, WriteTxnError>(
                    owned,
                    private,
                    |pattern, allowance| {
                        admit_pattern(pattern, scope).map_err(GqlQueryError::Source)?;
                        selection::select_overlay(
                            transaction,
                            database,
                            cx,
                            pattern,
                            scope,
                            allowance,
                            &controls,
                        )
                    },
                    || {
                        cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        controls.borrow_mut().checkpoint()
                    },
                )
                .map_err(|error| action_error(error.map_source(GraphEdgeUpsertError::Merge)))?
        };
        if u64::try_from(selected.value.len()).ok() != Some(selected.rows.result_rows) {
            return Err(action_context(invalid()));
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
        let mut effects = 0_u64;
        let mut retained = Vec::new();
        for (at, row) in selected.value.into_iter().enumerate() {
            event(
                cx,
                &RefCell::new(&mut *execution),
                private,
                &mut stats.evaluator,
                GlaExecutionEvent::ScratchEntry,
            )
            .map_err(action_context)?;
            let allowance = GraphEdgeMergePolicy::new(private).with_creation_limit(
                policy
                    .max_edges
                    .checked_sub(stats.created_edges)
                    .ok_or_else(|| action_context(invalid()))?,
            );
            // Move the original row into and back out of the native collector;
            // do not duplicate a carried list/map for each relationship probe.
            let (merged, targets, mut rows) =
                edge_merge::apply_occurrence::<EdgeUpsertRowTargets, _, _>(
                    transaction,
                    database,
                    cx,
                    merge,
                    allowance,
                    scope,
                    execution,
                    GqlQueryExecution {
                        value: vec![row],
                        rows: GqlExecutionStats {
                            snapshot_records: stats.selection.snapshot_records,
                            result_rows: 1,
                        },
                        evaluator: stats.evaluator,
                    },
                    at,
                )
                .map_err(|error| {
                    action_error(error.map_source(|error| {
                        GraphEdgeUpsertError::Merge(match error {
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
                        })
                    }))
                })?;
            if rows.len() != 1 {
                return Err(action_context(invalid()));
            }
            let row = rows.pop().ok_or_else(|| action_context(invalid()))?;
            stats.selection.snapshot_records = merged
                .match_selection
                .snapshot_records
                .checked_add(merged.overlay_edges)
                .ok_or_else(|| action_context(invalid()))?;
            stats.evaluator = merged.evaluator;
            stats.created_edges = stats
                .created_edges
                .checked_add(merged.created_edges)
                .ok_or_else(|| action_context(invalid()))?;
            for &(outcome, _) in &targets.0 {
                let (edge, branch) = match outcome {
                    GraphEdgeMergeOutcome::Matched(edge) => (edge, GraphEdgeUpsertBranch::Match),
                    GraphEdgeMergeOutcome::Created(edge) => (edge, GraphEdgeUpsertBranch::Create),
                    GraphEdgeMergeOutcome::NoInput => return Err(action_context(invalid())),
                };
                let actions = upsert.actions();
                let selected = match branch {
                    GraphEdgeUpsertBranch::Match => &actions.on_match,
                    GraphEdgeUpsertBranch::Create => &actions.on_create,
                    GraphEdgeUpsertBranch::NoInput => return Err(action_context(invalid())),
                };
                let clauses = [selected.as_slice(), actions.after.as_slice()];
                admit_actions(clauses, scope).map_err(action_error)?;
                let observed = u128::from(effects) + upsert.action_count(branch) as u128;
                if observed > u128::from(policy.max_mutation_effects) {
                    return Err(GqlQueryError::Source(GraphWriteChainError::Mutation(
                        GraphMutationQueryError::Mutation(GraphMutationError::EffectLimit {
                            limit: policy.max_mutation_effects,
                            observed,
                        }),
                    )));
                }
                let state = RefCell::new((&mut *transaction, &mut *database));
                let controls = RefCell::new(&mut *execution);
                edge_merge_row_actions_controlled::<WriteTxnError, WriteTxnError, _>(
                    (merge.relation(), edge),
                    (at, &row),
                    clauses,
                    private,
                    &mut stats.evaluator,
                    || {
                        cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        controls.borrow_mut().checkpoint()
                    },
                    |key, control| {
                        // Hidden reads have the same NULL transcript as an
                        // absent property, before lookup or witness creation.
                        if !scope.allows_property(key) {
                            control(GlaExecutionEvent::Work)?;
                            control(GlaExecutionEvent::ScratchEntry)?;
                            return Ok(GraphValue::Scalar(CanonicalScalar::Null));
                        }
                        let state = state.borrow();
                        let value = selection::property(
                            state.0,
                            state.1,
                            ElementId::Edge(edge),
                            key,
                            scope,
                            || {
                                cx.checkpoint()
                                    .map_err(WriteTxnError::Interrupted)
                                    .map_err(GqlQueryError::Interrupted)?;
                                controls
                                    .borrow_mut()
                                    .poll()
                                    .map_err(GqlQueryError::Interrupted)
                            },
                            control,
                            action_source,
                        )?;
                        GraphValue::admit_property_value(&value, &mut |event| control(event))
                    },
                    |batch| {
                        let mut state = state.borrow_mut();
                        let (transaction, database) = &mut *state;
                        super::stage_batch(
                            transaction,
                            database,
                            cx,
                            batch,
                            None,
                            &mut controls.borrow_mut(),
                        )
                        .map_err(action_source)
                    },
                )
                .map_err(action_error)?;
                effects = observed as u64;
            }
            if !matches!(step, GraphWriteChainStep::EdgeUpsert(_)) {
                event(
                    cx,
                    &RefCell::new(&mut *execution),
                    private,
                    &mut stats.evaluator,
                    GlaExecutionEvent::ScratchEntry,
                )
                .map_err(action_context)?;
                retained.push((row, targets.0));
            }
        }
        let controls = RefCell::new(&mut *execution);
        let mut continued = Vec::new();
        let mut returning = Vec::new();
        for (input, targets) in retained {
            for (outcome, endpoints) in targets {
                let edge = match outcome {
                    GraphEdgeMergeOutcome::Matched(edge) | GraphEdgeMergeOutcome::Created(edge) => {
                        edge
                    }
                    GraphEdgeMergeOutcome::NoInput => return Err(action_context(invalid())),
                };
                match step {
                    GraphWriteChainStep::EdgeUpsertRows { .. } => {
                        continued.push(
                            step.continuation_row_controlled(
                                &input,
                                ElementId::Edge(edge),
                                &mut |kind| {
                                    event(cx, &controls, private, &mut stats.evaluator, kind)
                                },
                            )
                            .map_err(action_context)?,
                        );
                    }
                    GraphWriteChainStep::EdgeUpsertQuery { query, .. } => {
                        returning.push(
                            returning_row(
                                transaction,
                                database,
                                cx,
                                query,
                                &input,
                                edge,
                                endpoints,
                                scope,
                                private,
                                &mut stats.evaluator,
                                &controls,
                            )
                            .map_err(action_context)?,
                        );
                    }
                    _ => return Err(action_context(invalid())),
                }
            }
        }
        let result = if let GraphWriteChainStep::EdgeUpsertQuery { query, .. } = step {
            query
                .finish_governed_reading::<WriteTxnError, WriteTxnError>(
                    returning,
                    stats.selection,
                    stats.evaluator,
                    policy.query,
                    |element, key| {
                        selection::returning_element_property(
                            transaction,
                            database,
                            cx,
                            element,
                            key,
                            scope,
                            &controls,
                        )
                    },
                    || {
                        cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        controls.borrow_mut().checkpoint()
                    },
                )
                .map_err(|error| {
                    error.map_source(|error| {
                        GraphWriteChainError::EdgeUpsert(GraphEdgeUpsertQueryError::Returning(
                            error,
                        ))
                    })
                })?
        } else {
            GqlQueryExecution {
                value: continued,
                rows: stats.selection,
                evaluator: stats.evaluator,
            }
        };
        stats.selection = result.rows;
        stats.evaluator = result.evaluator;
        Ok((stats, result, effects))
    })
}
