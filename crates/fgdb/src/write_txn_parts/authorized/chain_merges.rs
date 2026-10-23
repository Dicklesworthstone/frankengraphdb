//! Relationship MERGE steps inside the authorized chain's existing workspace.
//! Native selection and pair reduction own bag multiplicity and creation.
//! This adapter supplies scoped reads and controlled private continuation rows.

use super::super::{Database, Execution, Vfs, WriteTxn, WriteTxnError, edge_merge, selection};
use super::{Fault, admit_pattern};
use fgdb_delta_types::ElementId;
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertStats};
use fgdb_gql::{
    GlaExecutionEvent, GlaExecutionStats, GlaLimitDimension, GlaLimitExceeded, GqlExecutionStats,
    GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphEdgeMergeBinding, GraphEdgeMergeError,
    GraphEdgeMergePolicy, GraphEdgeMergeQueryError, GraphWriteChainError, GraphWriteChainStep,
    PreparedGraphEdgeMerge, PreparedGraphEdgeMergeQuery,
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

fn definition(step: &GraphWriteChainStep) -> Option<&PreparedGraphEdgeMerge> {
    match step {
        GraphWriteChainStep::EdgeMerge(merge)
        | GraphWriteChainStep::EdgeMergeRows { merge, .. } => Some(merge),
        GraphWriteChainStep::EdgeMergeQuery(query) => Some(query.merge()),
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
    if !scope.rights().can_read() {
        return Err(source(WriteTxnError::Authorization(
            Error::PermissionDenied,
        )));
    }
    if !scope.allows_relation(merge.relation())
        || merge
            .properties()
            .iter()
            .any(|(key, _)| !scope.allows_property(*key))
    {
        return Err(source(WriteTxnError::Authorization(Error::ScopeDenied)));
    }
    if let Some(pattern) = merge.selection() {
        admit_pattern(pattern, scope).map_err(source)?;
    }
    if let GraphWriteChainStep::EdgeMergeQuery(query) = step {
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

fn event<Clock: FnMut() -> u64>(
    cx: &QueryCx,
    execution: &RefCell<&mut Execution<'_, '_, Clock>>,
    policy: GqlQueryPolicy,
    stats: &mut GlaExecutionStats,
    kind: GlaExecutionEvent,
) -> Result<(), Fault> {
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
            for input in &selected {
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
                let edges = resolved.edges.get(&(src, dst)).ok_or_else(invalid)?;
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
                .finish_governed::<WriteTxnError, WriteTxnError>(
                    returning,
                    stats.selection,
                    stats.evaluator,
                    policy.query,
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
