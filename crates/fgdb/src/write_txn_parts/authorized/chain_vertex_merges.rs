//! Vertex MERGE steps use the native row binder, all-target collector and
//! shared ON/SET evaluator. This host supplies masked reads and authorized
//! original-intent staging inside the chain's single private workspace.

use super::super::{Database, Execution, Vfs, WriteTxn, WriteTxnError, selection};
use super::{Fault, admit_pattern, stage_batch};
use crate::WriteBatch;
use crate::write_txn::{
    FrozenCreations, FrozenValueClass, VertexMergeTargets, collect_vertex_merge_output,
    frozen_merge_predicates, point_scalar_payload_admission, vertex_merge_row_actions_controlled,
};
use fgdb_delta_types::{ElementId, PropertyKeyId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, VertexPredicate};
use fgdb_gql::insertion::{GraphInsertIntent, GraphInsertPolicy, GraphInsertStats};
use fgdb_gql::{
    GlaExecutionEvent, GlaExecutionStats, GlaLimitDimension, GlaLimitExceeded, GqlExecutionStats,
    GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphMutationError, GraphMutationQueryError,
    GraphVertexMergeError, GraphVertexMergePolicy, GraphVertexMergeRowAction,
    GraphVertexMergeRowsBinding, GraphVertexMergeRowsQueryError, GraphVertexMergeStats,
    GraphVertexUpsertAction, GraphWriteChainError, GraphWriteChainStep,
    PreparedGraphVertexMergeRows,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, QueryCx, VId};
use fgdb_warden::{Error, PlannerPredicates};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

type MergeFault = GqlQueryError<GraphVertexMergeError<WriteTxnError, WriteTxnError>, WriteTxnError>;

fn source(error: WriteTxnError) -> MergeFault {
    GqlQueryError::Source(GraphVertexMergeError::Source(error))
}

fn chained(error: MergeFault) -> Fault {
    error.map_source(|error| {
        GraphWriteChainError::VertexMerge(GraphVertexMergeRowsQueryError::Merge(error))
    })
}

fn invalid() -> MergeFault {
    GqlQueryError::Source(GraphVertexMergeError::InvalidSourceStatistics)
}

fn definition(step: &GraphWriteChainStep) -> Option<&PreparedGraphVertexMergeRows> {
    match step {
        GraphWriteChainStep::VertexMerge(merge) => Some(merge),
        GraphWriteChainStep::VertexMergeQuery(query) => Some(query.merge()),
        _ => None,
    }
}

/// Whole-chain capability-only admission, before any engine identity.
pub(super) fn preflight(
    step: &GraphWriteChainStep,
    scope: &PlannerPredicates,
) -> Result<bool, Fault> {
    let Some(merge) = definition(step) else {
        return Ok(false);
    };
    let denied = || chained(source(WriteTxnError::Authorization(Error::ScopeDenied)));
    if !scope.rights().can_read() {
        return Err(chained(source(WriteTxnError::Authorization(
            Error::PermissionDenied,
        ))));
    }
    for key in merge.property_keys() {
        if !scope.allows_property(key) {
            return Err(denied());
        }
    }
    // The compiler gives every row the same labels and predicate shape.
    // Inspect that shape without evaluating an input or consulting storage.
    let probe: Vec<_> = merge
        .property_keys()
        .map(|_| CanonicalScalar::Int(0))
        .collect();
    let single = merge.merge_for(&probe).map_err(|_| chained(invalid()))?;
    let predicates = frozen_merge_predicates(&single).ok_or_else(|| chained(invalid()))?;
    for predicate in predicates {
        if let VertexPredicate::HasLabel(label) = predicate
            && !scope.allows_label(label)
        {
            return Err(denied());
        }
    }
    if let GraphWriteChainStep::VertexMergeQuery(query) = step {
        for binding in query.bindings() {
            if let GraphVertexMergeRowsBinding::Property(key)
            | GraphVertexMergeRowsBinding::InputProperty { key, .. } = binding
                && !scope.allows_property(*key)
            {
                return Err(denied());
            }
        }
    }
    Ok(true)
}

// Only the selected ON branch attempts effects. Admit every original target
// field before evaluating that branch, even when a later assignment would
// overwrite it or the stored value is equal.
fn admit_actions(
    actions: &[GraphVertexMergeRowAction],
    scope: &PlannerPredicates,
) -> Result<(), MergeFault> {
    let denied = || source(WriteTxnError::Authorization(Error::ScopeDenied));
    for action in actions {
        match action {
            GraphVertexMergeRowAction::Vertex(GraphVertexUpsertAction::SetLabel {
                label, ..
            }) => {
                if !scope.allows_label(*label) {
                    return Err(denied());
                }
            }
            GraphVertexMergeRowAction::Vertex(GraphVertexUpsertAction::SetProperty {
                key, ..
            })
            | GraphVertexMergeRowAction::Vertex(GraphVertexUpsertAction::SetStored {
                key, ..
            })
            | GraphVertexMergeRowAction::Value { key, .. } => {
                if !scope.allows_property(*key) {
                    return Err(denied());
                }
            }
            GraphVertexMergeRowAction::Expression { key, .. } => {
                if !scope.allows_property(*key) {
                    return Err(denied());
                }
            }
            GraphVertexMergeRowAction::Vertex(GraphVertexUpsertAction::SetExpression {
                ..
            }) => {
                return Err(invalid());
            }
        }
    }
    Ok(())
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

fn remaining(
    policy: GraphVertexMergePolicy,
    used: &GraphVertexMergeStats,
) -> GraphVertexMergePolicy {
    GraphVertexMergePolicy::new(GqlQueryPolicy::new(
        policy
            .query
            .rows
            .max_snapshot_records()
            .unwrap_or(u64::MAX)
            .saturating_sub(used.match_selection.snapshot_records),
        u64::MAX,
        policy
            .query
            .evaluator
            .max_work_units
            .saturating_sub(used.evaluator.work_units),
        policy
            .query
            .evaluator
            .max_scratch_entries
            .saturating_sub(used.evaluator.scratch_entries),
    ))
    .with_creation_limit(
        policy
            .max_created_vertices
            .saturating_sub(used.created_vertices),
    )
}

fn add(used: &mut GraphVertexMergeStats, next: GraphVertexMergeStats) -> Result<(), MergeFault> {
    let plus = |left: u64, right: u64| left.checked_add(right).ok_or_else(invalid);
    used.match_selection.snapshot_records = plus(
        used.match_selection.snapshot_records,
        next.match_selection.snapshot_records,
    )?;
    used.match_selection.result_rows = plus(
        used.match_selection.result_rows,
        next.match_selection.result_rows,
    )?;
    used.evaluator.work_units = plus(used.evaluator.work_units, next.evaluator.work_units)?;
    used.evaluator.scratch_entries = plus(
        used.evaluator.scratch_entries,
        next.evaluator.scratch_entries,
    )?;
    used.created_vertices = plus(used.created_vertices, next.created_vertices)?;
    Ok(())
}

/// Candidate buckets are advisory; the native predicates decide every match.
/// Admit candidate retention and observe cancellation throughout the lookup.
#[allow(clippy::too_many_arguments)]
fn held_matches<Clock: FnMut() -> u64>(
    held: &FrozenCreations,
    key: Option<(PropertyKeyId, &CanonicalScalar)>,
    predicates: &[VertexPredicate],
    scope: &PlannerPredicates,
    cx: &QueryCx,
    execution: &RefCell<&mut Execution<'_, '_, Clock>>,
    policy: GqlQueryPolicy,
    stats: &mut GlaExecutionStats,
) -> Result<Vec<VId>, MergeFault> {
    let key = if let Some((key, value)) = key {
        point_scalar_payload_admission(value, &mut |kind| {
            use crate::gql_exec::source::SourceEvent;
            event(
                cx,
                execution,
                policy,
                stats,
                match kind {
                    SourceEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
                    _ => GlaExecutionEvent::Work,
                },
            )
        })?;
        Some((key, FrozenValueClass::of(value)))
    } else {
        None
    };
    let mut candidates = BTreeSet::new();
    {
        let mut candidate = |at: usize| -> Result<(), MergeFault> {
            cx.checkpoint()
                .map_err(WriteTxnError::Interrupted)
                .map_err(GqlQueryError::Interrupted)?;
            execution
                .borrow_mut()
                .poll()
                .map_err(GqlQueryError::Interrupted)?;
            if !scope.allows_vertex(&held.vertices[at].1) {
                return Err(source(WriteTxnError::Authorization(Error::ScopeDenied)));
            }
            event(cx, execution, policy, stats, GlaExecutionEvent::Work)?;
            if !candidates.contains(&at) {
                event(
                    cx,
                    execution,
                    policy,
                    stats,
                    GlaExecutionEvent::ScratchEntry,
                )?;
                candidates.insert(at);
            }
            Ok(())
        };
        if let Some((key, class)) = key {
            let bucket = |class| {
                held.buckets
                    .get(&(key, class))
                    .map_or(&[][..], Vec::as_slice)
            };
            match class {
                class @ FrozenValueClass::Number(_) => {
                    for &at in bucket(class)
                        .iter()
                        .chain(bucket(FrozenValueClass::Decimal))
                    {
                        candidate(at)?;
                    }
                }
                FrozenValueClass::Decimal => {
                    for (_, bucket) in held.buckets.range(
                        (key, FrozenValueClass::Number(0))..=(key, FrozenValueClass::Decimal),
                    ) {
                        for &at in bucket {
                            candidate(at)?;
                        }
                    }
                }
                class => {
                    for &at in bucket(class) {
                        candidate(at)?;
                    }
                }
            }
        } else {
            for at in 0..held.vertices.len() {
                candidate(at)?;
            }
        }
    }
    let mut matched = Vec::new();
    for at in candidates {
        // A held visible candidate consumes the same signed node visit as a
        // visible overlay candidate, including a later predicate rejection.
        {
            let mut execution = execution.borrow_mut();
            let now = (execution.clock)();
            execution
                .permit
                .charge_nodes_at(now, 1)
                .map_err(WriteTxnError::Authorization)
                .map_err(GqlQueryError::Interrupted)?;
        }
        let (vertex, labels, properties) = &held.vertices[at];
        let mut accepts = true;
        for predicate in predicates {
            event(cx, execution, policy, stats, GlaExecutionEvent::Work)?;
            if !predicate.matches(labels, properties) {
                accepts = false;
                break;
            }
        }
        if accepts {
            event(
                cx,
                execution,
                policy,
                stats,
                GlaExecutionEvent::ScratchEntry,
            )?;
            matched.push(*vertex);
        }
    }
    Ok(matched)
}

/// Held creations have already passed the same native before/after authority
/// owner. Mask before even looking at their property slots; ordinary targets
/// retain the normal point projector and its absence/read witnesses.
#[allow(clippy::too_many_arguments)]
fn action_property<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &WriteTxn,
    database: &Database<V>,
    cx: &QueryCx,
    vertex: VId,
    key: PropertyKeyId,
    frozen: Option<&FrozenCreations>,
    scope: &PlannerPredicates,
    execution: &RefCell<&mut Execution<'_, '_, Clock>>,
    control: &mut dyn FnMut(GlaExecutionEvent) -> Result<(), MergeFault>,
) -> Result<GraphValue, MergeFault> {
    if !scope.allows_property(key) {
        control(GlaExecutionEvent::ScratchEntry)?;
        return Ok(GraphValue::Scalar(CanonicalScalar::Null));
    }
    if let Some(held) = frozen
        && let Some(&at) = held.index.get(&vertex)
    {
        let (_, labels, properties) = &held.vertices[at];
        if !scope.allows_vertex(labels) {
            return Err(source(WriteTxnError::Authorization(Error::ScopeDenied)));
        }
        control(GlaExecutionEvent::Work)?;
        return match properties.iter().find(|(present, _)| *present == key) {
            Some((_, value)) => GraphValue::admit_property_value(value, &mut |kind| control(kind)),
            None => Ok(GraphValue::Scalar(CanonicalScalar::Null)),
        };
    }
    let value = selection::property(
        transaction,
        database,
        ElementId::Vertex(vertex),
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
        control,
        source,
    )?;
    GraphValue::admit_property_value(&value, &mut |kind| control(kind))
}

/// Resolve every row against the preceding row's state. Only action-free
/// exact tuples share a resolution; all matching vertices retain identity
/// order and every input occurrence continues independently.
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
    mut frozen: Option<&mut FrozenCreations>,
    continues: bool,
) -> Result<(GraphInsertStats, GqlQueryExecution<GraphValueRow>, u64), Fault> {
    if !preflight(step, scope)? {
        return Err(chained(invalid()));
    }
    let merge = definition(step).ok_or_else(|| chained(invalid()))?;
    // Intermediate matches and input occurrences never spend final row limits.
    let private = GraphVertexMergePolicy::new(GqlQueryPolicy::new(
        policy.query.rows.max_snapshot_records().unwrap_or(u64::MAX),
        u64::MAX,
        policy.query.evaluator.max_work_units,
        policy.query.evaluator.max_scratch_entries,
    ))
    .with_creation_limit(policy.max_vertices);
    cx.with_restriction(|| {
        let selected = {
            let controls = RefCell::new(&mut *execution);
            merge
                .select_governed::<WriteTxnError, WriteTxnError, WriteTxnError>(
                    owned,
                    private.query,
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
                .map_err(chained)?
        };
        let rows = selected.value;
        let mut used = GraphVertexMergeStats {
            match_selection: GqlExecutionStats {
                snapshot_records: selected.rows.snapshot_records,
                result_rows: 0,
            },
            evaluator: selected.evaluator,
            created_vertices: 0,
        };
        let mut resolved: BTreeMap<Vec<CanonicalPropertyValue>, Vec<VId>> = BTreeMap::new();
        let mut pairs = Vec::new();
        let mut effects = 0_u64;
        let memoized = merge.actions().is_none();
        let retains_rows = continues || matches!(step, GraphWriteChainStep::VertexMergeQuery(_));
        for (at, row) in rows.iter().enumerate() {
            let tuple = {
                let controls = RefCell::new(&mut *execution);
                merge
                    .stored_tuple(at, row, &mut |kind| {
                        event(cx, &controls, private.query, &mut used.evaluator, kind)
                    })
                    .map_err(chained)?
            };
            let collections = tuple.iter().any(CanonicalPropertyValue::is_collection);
            if collections && let Some(held) = frozen.take() {
                // The held prefix has already passed original-intent
                // admission. Use its existing finish owner so flushing it
                // neither skips nor repeats authority/effect accounting.
                super::finish_frozen(transaction, database, cx, core::mem::take(held), execution)
                    .map_err(source)
                    .map_err(chained)?;
            }
            // A list containing NULL is not equal to itself. Keep every
            // collection occurrence on the governed, masked native matcher.
            let memoized = memoized && !collections;
            let mut created_here = false;
            let vertices: &[VId] = if !memoized || !resolved.contains_key(&tuple) {
                {
                    let controls = RefCell::new(&mut *execution);
                    event(
                        cx,
                        &controls,
                        private.query,
                        &mut used.evaluator,
                        GlaExecutionEvent::ScratchEntry,
                    )
                    .map_err(chained)?;
                }
                let single = merge.merge_for_stored(&tuple).map_err(|error| {
                    chained(GqlQueryError::Source(GraphVertexMergeError::RowMerge {
                        row: at,
                        source: Box::new(error),
                    }))
                })?;
                let held = match frozen.as_deref() {
                    Some(held) => {
                        let predicates =
                            frozen_merge_predicates(&single).ok_or_else(|| chained(invalid()))?;
                        let controls = RefCell::new(&mut *execution);
                        held_matches(
                            held,
                            merge
                                .property_keys()
                                .next()
                                .zip(tuple.first().and_then(CanonicalPropertyValue::as_scalar)),
                            &predicates,
                            scope,
                            cx,
                            &controls,
                            private.query,
                            &mut used.evaluator,
                        )
                        .map_err(chained)?
                    }
                    None => Vec::new(),
                };
                let allowance = remaining(private, &used);
                let proposal = {
                    let controls = RefCell::new(&mut *execution);
                    let database = RefCell::new(&mut *database);
                    collect_vertex_merge_output::<
                        VertexMergeTargets,
                        WriteTxnError,
                        WriteTxnError,
                        WriteTxnError,
                    >(
                        &single,
                        allowance,
                        |pattern, allowance| {
                            let mut selected = selection::select_overlay(
                                transaction,
                                &database.borrow(),
                                cx,
                                pattern,
                                scope,
                                allowance,
                                &controls,
                            )?;
                            for vertex in held {
                                event(
                                    cx,
                                    &controls,
                                    allowance,
                                    &mut selected.evaluator,
                                    GlaExecutionEvent::ScratchEntry,
                                )?;
                                selected.value.push(GraphValueRow::from_owned_values(vec![
                                    GraphValue::Vertex(vertex),
                                ]));
                                selected.rows.result_rows =
                                    selected.rows.result_rows.checked_add(1).ok_or(
                                        GqlQueryError::Source(WriteTxnError::Authorization(
                                            Error::TooLarge,
                                        )),
                                    )?;
                            }
                            Ok(selected)
                        },
                        |request| database.borrow_mut().allocate_identity(cx, request),
                        || {
                            cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                            controls.borrow_mut().checkpoint()
                        },
                    )
                    .map_err(chained)?
                };
                add(&mut used, proposal.stats).map_err(chained)?;
                created_here = proposal.outcome.created.is_some();
                let mut vertices = Vec::new();
                {
                    let controls = RefCell::new(&mut *execution);
                    for outcome in proposal.outcome.outcomes() {
                        event(
                            cx,
                            &controls,
                            private.query,
                            &mut used.evaluator,
                            GlaExecutionEvent::ScratchEntry,
                        )
                        .map_err(chained)?;
                        vertices.push(outcome.vertex());
                    }
                }
                if let Some(creation) = proposal.creation {
                    let mut batch = WriteBatch::new(merge.relation());
                    for intent in creation.into_intents() {
                        let GraphInsertIntent::Vertex {
                            vertex,
                            labels,
                            properties,
                        } = intent
                        else {
                            return Err(chained(invalid()));
                        };
                        batch.create_vertex_values(vertex, labels, properties);
                    }
                    stage_batch(
                        transaction,
                        database,
                        cx,
                        batch,
                        frozen.as_deref_mut(),
                        execution,
                    )
                    .map_err(source)
                    .map_err(chained)?;
                }
                match resolved.entry(tuple) {
                    std::collections::btree_map::Entry::Occupied(mut entry) => {
                        entry.insert(vertices);
                        entry.into_mut()
                    }
                    std::collections::btree_map::Entry::Vacant(entry) => entry.insert(vertices),
                }
            } else {
                &resolved[&tuple]
            };
            if let Some(actions) = merge.actions() {
                let on = if created_here {
                    &actions.on_create
                } else {
                    &actions.on_match
                };
                admit_actions(on, scope).map_err(chained)?;
                admit_actions(&actions.after, scope).map_err(chained)?;
                let observed = (on.len() as u128)
                    .checked_add(actions.after.len() as u128)
                    .and_then(|actions| actions.checked_mul(vertices.len() as u128))
                    .and_then(|actions| u128::from(effects).checked_add(actions))
                    .ok_or_else(|| {
                        chained(GqlQueryError::Source(
                            GraphVertexMergeError::AccountingOverflow,
                        ))
                    })?;
                if observed > u128::from(policy.max_mutation_effects) {
                    return Err(GqlQueryError::Source(GraphWriteChainError::Mutation(
                        GraphMutationQueryError::Mutation(GraphMutationError::EffectLimit {
                            limit: policy.max_mutation_effects,
                            observed,
                        }),
                    )));
                }
                effects = observed as u64;
                for &vertex in vertices {
                    let controls = RefCell::new(&mut *execution);
                    let state =
                        RefCell::new((&mut *transaction, &mut *database, frozen.as_deref_mut()));
                    vertex_merge_row_actions_controlled(
                        (merge.relation(), vertex),
                        (at, row),
                        [on, &actions.after],
                        private,
                        &mut used,
                        || {
                            cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                            controls.borrow_mut().checkpoint()
                        },
                        |key, control| {
                            let state = state.borrow();
                            action_property(
                                state.0,
                                state.1,
                                cx,
                                vertex,
                                key,
                                state.2.as_deref(),
                                scope,
                                &controls,
                                control,
                            )
                        },
                        |batch| {
                            let mut state = state.borrow_mut();
                            let (transaction, database, frozen) = &mut *state;
                            stage_batch(
                                transaction,
                                database,
                                cx,
                                batch,
                                frozen.as_deref_mut(),
                                &mut controls.borrow_mut(),
                            )
                            .map_err(source)
                        },
                    )
                    .map_err(chained)?;
                }
            }
            // A terminal no-RETURN MERGE has no consumer for its private
            // occurrences. Do not allocate or expose a continuation bag.
            if retains_rows {
                let controls = RefCell::new(&mut *execution);
                for &vertex in vertices {
                    event(
                        cx,
                        &controls,
                        private.query,
                        &mut used.evaluator,
                        GlaExecutionEvent::ScratchEntry,
                    )
                    .map_err(chained)?;
                    pairs.push((at, vertex));
                }
            }
        }
        let controls = RefCell::new(&mut *execution);
        let evaluator = RefCell::new(used.evaluator);
        let mut continued = Vec::new();
        let mut returning = Vec::new();
        for (at, vertex) in pairs {
            let mut control = |kind| {
                event(
                    cx,
                    &controls,
                    policy.query,
                    &mut evaluator.borrow_mut(),
                    kind,
                )
            };
            match step {
                GraphWriteChainStep::VertexMerge(_) => {
                    continued.push(step.continuation_row_controlled(
                        &rows[at],
                        ElementId::Vertex(vertex),
                        &mut control,
                    )?)
                }
                GraphWriteChainStep::VertexMergeQuery(_) => {
                    returning.push(step.vertex_return_row_controlled(
                        &rows[at],
                        vertex,
                        |element, key| {
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
                                    controls
                                        .borrow_mut()
                                        .poll()
                                        .map_err(GqlQueryError::Interrupted)
                                },
                                &mut |kind| {
                                    event(
                                        cx,
                                        &controls,
                                        policy.query,
                                        &mut evaluator.borrow_mut(),
                                        kind,
                                    )
                                },
                                |error| chained(source(error)),
                            )?;
                            Ok(Some(value))
                        },
                        &mut control,
                    )?)
                }
                _ => return Err(chained(invalid())),
            }
        }
        let selection = GqlExecutionStats {
            snapshot_records: used.match_selection.snapshot_records,
            result_rows: 0,
        };
        let evaluator = evaluator.into_inner();
        let result = if let GraphWriteChainStep::VertexMergeQuery(query) = step {
            query
                .finish_governed_reading::<WriteTxnError, WriteTxnError>(
                    returning,
                    selection,
                    evaluator,
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
                        GraphWriteChainError::VertexMerge(
                            GraphVertexMergeRowsQueryError::Returning(error),
                        )
                    })
                })?
        } else {
            GqlQueryExecution {
                value: continued,
                rows: selection,
                evaluator,
            }
        };
        let stats = GraphInsertStats {
            selection: result.rows,
            evaluator: result.evaluator,
            created_vertices: used.created_vertices,
            created_edges: 0,
        };
        Ok((stats, result, effects))
    })
}
