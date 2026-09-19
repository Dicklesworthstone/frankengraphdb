//! Simultaneous assignment reduction. Borrowed scalars point into the frozen
//! selection/literals; computed scalars move into intent ownership without copies.

use super::*;
use crate::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES;
use crate::algebra_exec::charge_payload;
use crate::{
    GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GqlBudgetDimension,
    GraphIntegerEvaluationError,
};
use fgdb_delta_types::ElementId;
use std::collections::BTreeMap;

type ResultOf<T, E, C> = Result<T, GqlQueryError<GraphMutationError<E>, C>>;

fn property_intent(
    target: ElementId,
    key: PropertyKeyId,
    value: Option<CanonicalScalar>,
) -> GraphMutationIntent {
    match target {
        ElementId::Vertex(vertex) => GraphMutationIntent::Property { vertex, key, value },
        ElementId::Edge(edge) => GraphMutationIntent::EdgeProperty { edge, key, value },
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Field {
    Property(PropertyKeyId),
    Label(LabelId),
    Delete,
}
enum Value<'a> {
    Property(Option<&'a CanonicalScalar>),
    /// Canonical NULL is stored, never a property-removal intention.
    Computed(CanonicalScalar),
    Label(bool),
    Delete,
}
struct Proposal<'a> {
    value: Value<'a>,
    row: usize,
    action: usize,
}

struct Meter<F> {
    policy: GraphMutationPolicy,
    evaluator: GlaExecutionStats,
    checkpoint: F,
}
impl<F> Meter<F> {
    fn event<E, C>(&mut self, event: GlaExecutionEvent) -> ResultOf<(), E, C>
    where
        F: FnMut() -> Result<(), C>,
    {
        (self.checkpoint)().map_err(GqlQueryError::Interrupted)?;
        let work = u128::from(self.evaluator.work_units) + 1;
        let scratch = u128::from(self.evaluator.scratch_entries)
            + u128::from(event == GlaExecutionEvent::ScratchEntry);
        for (observed, limit, dimension) in [
            (
                work,
                self.policy.query.evaluator.max_work_units,
                GlaLimitDimension::WorkUnits,
            ),
            (
                scratch,
                self.policy.query.evaluator.max_scratch_entries,
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
        self.evaluator = GlaExecutionStats {
            work_units: work as u64,
            scratch_entries: scratch as u64,
        };
        Ok(())
    }
}

fn equal<E>(
    left: &Value<'_>,
    right: &Value<'_>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<bool, E> {
    control(GlaExecutionEvent::Work)?;
    match (left, right) {
        (Value::Property(left), Value::Property(right)) => {
            for scalar in [*left, *right].into_iter().flatten() {
                charge_payload(scalar, control)?;
            }
            Ok(left == right)
        }
        (Value::Computed(left), Value::Computed(right)) => {
            charge_payload(left, control)?;
            charge_payload(right, control)?;
            Ok(left == right)
        }
        (Value::Computed(computed), Value::Property(scalar))
        | (Value::Property(scalar), Value::Computed(computed)) => {
            charge_payload(computed, control)?;
            if let Some(scalar) = scalar {
                charge_payload(scalar, control)?;
            }
            Ok(Some(computed) == *scalar)
        }
        (Value::Label(left), Value::Label(right)) => Ok(left == right),
        (Value::Delete, Value::Delete) => Ok(true),
        _ => Ok(false),
    }
}

/// Same logical payload units used by value-row projection. Reserve before
/// the only scalar clone in proposal production, including a text sort key.
fn reserve_copy<E>(
    value: &CanonicalScalar,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    let sizes = match value {
        CanonicalScalar::Text(value) => [
            value.len(),
            value.canonical_sort_key().map_or(0, <[u8]>::len),
        ],
        CanonicalScalar::Bytes(value) => [value.as_slice().len(), 0],
        CanonicalScalar::Timestamp(value) => {
            [value.zone().map_or(0, |zone| zone.identifier().len()), 0]
        }
        _ => [0, 0],
    };
    control(GlaExecutionEvent::ScratchEntry)?;
    for bytes in sizes {
        for _ in 0..bytes.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
            control(GlaExecutionEvent::ScratchEntry)?;
        }
    }
    Ok(())
}

pub(super) fn execute<E, C>(
    mutation: &PreparedGraphMutation,
    policy: GraphMutationPolicy,
    source: impl FnOnce(
        &PreparedGraphPattern<GraphValueRow>,
        GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
    mut checkpoint: impl FnMut() -> Result<(), C>,
) -> ResultOf<GraphMutationBatch, E, C> {
    checkpoint().map_err(GqlQueryError::Interrupted)?;
    let selected = mutation.select_governed(policy.query, source, &mut checkpoint)?;
    if u64::try_from(selected.value.len()).ok() != Some(selected.rows.result_rows) {
        return Err(GqlQueryError::Source(
            GraphMutationError::InvalidSourceStatistics,
        ));
    }
    for (dimension, observed) in [
        (
            GqlBudgetDimension::SnapshotRecords,
            selected.rows.snapshot_records,
        ),
        (GqlBudgetDimension::ResultRows, selected.rows.result_rows),
    ] {
        policy
            .query
            .rows
            .check(dimension, observed)
            .map_err(GqlQueryError::Rows)?;
    }
    for (observed, limit, dimension) in [
        (
            selected.evaluator.work_units,
            policy.query.evaluator.max_work_units,
            GlaLimitDimension::WorkUnits,
        ),
        (
            selected.evaluator.scratch_entries,
            policy.query.evaluator.max_scratch_entries,
            GlaLimitDimension::ScratchEntries,
        ),
    ] {
        if observed > limit {
            return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                dimension,
                limit,
                observed: u128::from(observed),
            }));
        }
    }
    let mut meter = Meter {
        policy,
        evaluator: selected.evaluator,
        checkpoint,
    };
    meter.event(GlaExecutionEvent::Work)?;
    let columns = &mutation.columns;
    let mut proposals = BTreeMap::<(ElementId, Field), Proposal<'_>>::new();
    for (row_at, row) in selected.value.iter().enumerate() {
        meter.event(GlaExecutionEvent::Work)?;
        if row.len() != columns.len() {
            return Err(GqlQueryError::Source(GraphMutationError::InputSchema {
                row: row_at,
                column: row.len().min(columns.len()),
            }));
        }
        for (column, (value, expression)) in row.values().iter().zip(columns).enumerate() {
            meter.event(GlaExecutionEvent::Work)?;
            let valid = expression.accepts(value);
            if !valid {
                return Err(GqlQueryError::Source(GraphMutationError::InputSchema {
                    row: row_at,
                    column,
                }));
            }
        }
        for (action_at, action) in mutation.actions.iter().enumerate() {
            meter.event(GlaExecutionEvent::Work)?;
            let target_value = &row.values()[action.target()];
            let target = if let Some(vertex) = target_value.as_vertex() {
                ElementId::Vertex(vertex)
            } else if let Some(edge) = target_value.as_edge() {
                ElementId::Edge(edge)
            } else {
                // Only canonical null can remain after the schema check. An
                // absent OPTIONAL target does not execute an assignment RHS.
                continue;
            };
            let (field, value) = match action {
                GraphMutationAction::SetProperty { key, value, .. } => {
                    let value = match value {
                        GraphMutationValue::Column(column) => Value::Property(Some(
                            row.values()[*column]
                                .as_scalar()
                                .expect("complete input schema checked"),
                        )),
                        GraphMutationValue::Literal(value) => Value::Property(Some(value.value())),
                        GraphMutationValue::Expression(expression) => {
                            let value = expression
                                .evaluate_scalar_with_control(row.values(), &mut |event| {
                                    meter.event(event)
                                })
                                .map_err(|failure| match failure {
                                    GraphIntegerEvaluationError::Control(error) => error,
                                    GraphIntegerEvaluationError::Value(error) => {
                                        GqlQueryError::Source(GraphMutationError::Arithmetic {
                                            row: row_at,
                                            action: action_at,
                                            error,
                                        })
                                    }
                                })?;
                            Value::Computed(value)
                        }
                    };
                    (Field::Property(*key), value)
                }
                GraphMutationAction::RemoveProperty { key, .. } => {
                    (Field::Property(*key), Value::Property(None))
                }
                GraphMutationAction::SetLabel { label, present, .. } => {
                    (Field::Label(*label), Value::Label(*present))
                }
                GraphMutationAction::DetachDelete { .. } => (Field::Delete, Value::Delete),
            };
            if let Some(previous) = proposals.get(&(target, field)) {
                if !equal(&previous.value, &value, &mut |event| meter.event(event))? {
                    return Err(GqlQueryError::Source(
                        GraphMutationError::ConflictingAssignment {
                            first_row: previous.row,
                            first_action: previous.action,
                            row: row_at,
                            action: action_at,
                        },
                    ));
                }
            } else {
                let observed = proposals.len() as u128 + 1;
                if observed > u128::from(policy.max_effects) {
                    return Err(GqlQueryError::Source(GraphMutationError::EffectLimit {
                        limit: policy.max_effects,
                        observed,
                    }));
                }
                meter.event(GlaExecutionEvent::ScratchEntry)?;
                proposals.insert(
                    (target, field),
                    Proposal {
                        value,
                        row: row_at,
                        action: action_at,
                    },
                );
            }
        }
    }
    let mut intents = Vec::new();
    let mut previous_target = None;
    let mut target_vertices = 0_u64;
    let mut target_edges = 0_u64;
    for ((target, field), proposal) in proposals {
        meter.event(GlaExecutionEvent::Work)?;
        if previous_target != Some(target) {
            match target {
                ElementId::Vertex(_) => target_vertices += 1,
                ElementId::Edge(_) => target_edges += 1,
            }
            previous_target = Some(target);
        }
        meter.event(GlaExecutionEvent::ScratchEntry)?;
        let intent = match (target, field, proposal.value) {
            (target, Field::Property(key), Value::Property(value)) => {
                if let Some(scalar) = value {
                    reserve_copy(scalar, &mut |event| meter.event(event))?;
                }
                property_intent(target, key, value.cloned())
            }
            (target, Field::Property(key), Value::Computed(value)) => {
                meter.event(GlaExecutionEvent::ScratchEntry)?;
                property_intent(target, key, Some(value))
            }
            (ElementId::Vertex(vertex), Field::Label(label), Value::Label(present)) => {
                GraphMutationIntent::Label {
                    vertex,
                    label,
                    present,
                }
            }
            (ElementId::Vertex(vertex), Field::Delete, Value::Delete) => {
                GraphMutationIntent::DetachDelete { vertex }
            }
            _ => unreachable!("field and proposal are constructed together"),
        };
        intents.push(intent);
    }
    (meter.checkpoint)().map_err(GqlQueryError::Interrupted)?;
    let stats = GraphMutationStats {
        selection: selected.rows,
        evaluator: meter.evaluator,
        target_vertices,
        target_edges,
        effects: intents.len() as u64,
    };
    Ok(GraphMutationBatch { intents, stats })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
    fn query() -> PreparedGraphPattern<GraphValueRow> {
        PreparedGraphText::prepare("MATCH (a)-[:R]->(b) RETURN a,b.p AS p", |kind, _| {
            Some(match kind {
                GraphSymbolKind::Relation => GraphSymbol::Relation(RelationId(1)),
                GraphSymbolKind::Property => GraphSymbol::Property(PropertyKeyId(1)),
                GraphSymbolKind::Label => GraphSymbol::Label(LabelId(1)),
            })
        })
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
    }
    fn mutation() -> PreparedGraphMutation {
        PreparedGraphMutation::prepare(
            query(),
            RelationId(1),
            vec![GraphMutationAction::SetProperty {
                target: 0,
                key: PropertyKeyId(2),
                value: GraphMutationValue::Column(1),
            }],
        )
        .unwrap()
    }
    fn run(
        values: &[CanonicalScalar],
        policy: GraphMutationPolicy,
    ) -> ResultOf<GraphMutationBatch, (), ()> {
        mutation().execute_governed(
            policy,
            |plan, policy| {
                plan.plan().execute_governed_with_properties(
                    values.len() as u64,
                    [],
                    (0..values.len()).map(|i| (VId(1), RelationId(1), VId(i as u128 + 10))),
                    |_, _| Ok::<_, ()>(true),
                    |vid, _| Ok(Some(&values[vid.0 as usize - 10])),
                    policy,
                    || Ok::<_, ()>(()),
                )
            },
            || Ok::<_, ()>(()),
        )
    }
    fn policy() -> GraphMutationPolicy {
        GraphMutationPolicy::new(GqlQueryPolicy::new(100, 100, 100_000, 100_000), 100)
    }
    #[test]
    fn repeated_assignments_collapse_but_disagreement_never_selects_a_last_writer() {
        let values = [CanonicalScalar::Int(7), CanonicalScalar::Int(7)];
        let result = run(&values, policy()).unwrap();
        assert_eq!(
            result.intents(),
            &[GraphMutationIntent::Property {
                vertex: VId(1),
                key: PropertyKeyId(2),
                value: Some(CanonicalScalar::Int(7)),
            }]
        );
        assert_eq!(result.stats().selection.result_rows, 2);
        assert_eq!(
            (result.stats().target_vertices, result.stats().effects),
            (1, 1)
        );
        for values in [
            [CanonicalScalar::Int(7), CanonicalScalar::Int(9)],
            [CanonicalScalar::Int(9), CanonicalScalar::Int(7)],
            [CanonicalScalar::Int(7), CanonicalScalar::Null],
        ] {
            assert!(matches!(
                run(&values, policy()),
                Err(GqlQueryError::Source(
                    GraphMutationError::ConflictingAssignment { .. }
                ))
            ));
        }
    }
    #[test]
    fn selection_and_proposals_share_work_scratch_and_exact_effect_limits() {
        let values = [CanonicalScalar::Int(7), CanonicalScalar::Int(7)];
        let measured = run(&values, policy()).unwrap();
        let stats = measured.stats();
        let exact = GraphMutationPolicy::new(
            GqlQueryPolicy::new(
                2,
                2,
                stats.evaluator.work_units,
                stats.evaluator.scratch_entries,
            ),
            1,
        );
        assert_eq!(run(&values, exact).unwrap().stats(), stats);
        for cap in [
            GraphMutationPolicy::new(GqlQueryPolicy::new(1, 2, u64::MAX, u64::MAX), 1),
            GraphMutationPolicy::new(GqlQueryPolicy::new(2, 1, u64::MAX, u64::MAX), 1),
            GraphMutationPolicy::new(
                GqlQueryPolicy::new(2, 2, stats.evaluator.work_units - 1, u64::MAX),
                1,
            ),
            GraphMutationPolicy::new(
                GqlQueryPolicy::new(2, 2, u64::MAX, stats.evaluator.scratch_entries - 1),
                1,
            ),
            GraphMutationPolicy::new(GqlQueryPolicy::new(2, 2, u64::MAX, u64::MAX), 0),
        ] {
            assert!(run(&values, cap).is_err());
        }
    }
    #[test]
    fn schema_is_checked_before_a_source_can_run() {
        for action in [
            GraphMutationAction::DetachDelete { target: 1 },
            GraphMutationAction::RemoveProperty {
                target: usize::MAX,
                key: PropertyKeyId(1),
            },
            GraphMutationAction::SetProperty {
                target: 0,
                key: PropertyKeyId(2),
                value: GraphMutationValue::Column(0),
            },
        ] {
            assert!(PreparedGraphMutation::prepare(query(), RelationId(1), vec![action]).is_err());
        }
        assert!(matches!(
            PreparedGraphMutation::prepare(
                query(),
                RelationId(1),
                vec![
                    GraphMutationAction::DetachDelete { target: 0 },
                    GraphMutationAction::RemoveProperty {
                        target: 0,
                        key: PropertyKeyId(1)
                    },
                ]
            ),
            Err(GraphMutationBuildError::MixedDeletionAndUpdates)
        ));
        for column in [0, 2, usize::MAX] {
            let expression =
                GraphIntegerExpression::prepare(&[crate::GraphIntegerOp::Column(column)]).unwrap();
            assert!(
                matches!(PreparedGraphMutation::prepare(query(), RelationId(1), vec![
                GraphMutationAction::SetProperty { target: 0, key: PropertyKeyId(2), value: GraphMutationValue::Expression(expression) },
            ]), Err(GraphMutationBuildError::ValueColumn { column: found, .. }) if found == column)
            );
        }
    }

    #[test]
    fn late_rhs_source_failure_never_returns_an_earlier_partial_assignment() {
        let value = CanonicalScalar::Int(7);
        let reads = std::cell::Cell::new(0);
        let result: ResultOf<GraphMutationBatch, &str, ()> = mutation().execute_governed(
            policy(),
            |plan, policy| {
                plan.plan().execute_governed_with_properties(
                    2,
                    [],
                    [
                        (VId(1), RelationId(1), VId(10)),
                        (VId(2), RelationId(1), VId(11)),
                    ],
                    |_, _| Ok::<_, &str>(true),
                    |vid, _| {
                        reads.set(reads.get() + 1);
                        if vid == VId(11) {
                            Err("RHS property source failed")
                        } else {
                            Ok(Some(&value))
                        }
                    },
                    policy,
                    || Ok::<_, ()>(()),
                )
            },
            || Ok::<_, ()>(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphMutationError::Source(
                "RHS property source failed"
            )))
        ));
        assert_eq!(
            reads.get(),
            2,
            "a valid earlier match preceded the failing RHS read"
        );
    }

    #[test]
    fn overflow_refusal_leaves_both_mutation_counters_unchanged() {
        let policy = GraphMutationPolicy::new(
            GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX),
            u64::MAX,
        );
        let mut meter = Meter {
            policy,
            checkpoint: || Ok::<_, ()>(()),
            evaluator: GlaExecutionStats {
                work_units: u64::MAX,
                scratch_entries: 0,
            },
        };
        let before = meter.evaluator;
        assert!(matches!(meter.event::<(), ()>(GlaExecutionEvent::Work),
            Err(GqlQueryError::Evaluator(GlaLimitExceeded { dimension: GlaLimitDimension::WorkUnits, observed, .. }))
                if observed == u128::from(u64::MAX) + 1));
        assert_eq!(meter.evaluator, before);
        meter.evaluator = GlaExecutionStats {
            work_units: 0,
            scratch_entries: u64::MAX,
        };
        let before = meter.evaluator;
        assert!(
            matches!(meter.event::<(), ()>(GlaExecutionEvent::ScratchEntry),
            Err(GqlQueryError::Evaluator(GlaLimitExceeded { dimension: GlaLimitDimension::ScratchEntries, observed, .. }))
                if observed == u128::from(u64::MAX) + 1)
        );
        assert_eq!(meter.evaluator, before);
    }

    #[test]
    fn computed_scalars_share_assignment_equality_but_never_equal_removal() {
        for value in [None, Some(i64::MIN), Some(0), Some(i64::MAX)] {
            let scalar = value.map_or(CanonicalScalar::Null, CanonicalScalar::Int);
            for (left, right) in [
                (
                    Value::Computed(scalar.clone()),
                    Value::Property(Some(&scalar)),
                ),
                (
                    Value::Property(Some(&scalar)),
                    Value::Computed(scalar.clone()),
                ),
            ] {
                assert!(equal(&left, &right, &mut |_| Ok::<_, ()>(())).unwrap());
            }
            assert!(
                !equal(
                    &Value::Computed(scalar),
                    &Value::Property(None),
                    &mut |_| Ok::<_, ()>(())
                )
                .unwrap()
            );
        }
        assert!(
            !equal(
                &Value::Computed(CanonicalScalar::Int(1)),
                &Value::Property(Some(&CanonicalScalar::Bool(true))),
                &mut |_| Ok::<_, ()>(())
            )
            .unwrap()
        );
    }
}
