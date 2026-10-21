//! Native proposal/conversion laws. The supplied source rows isolate the
//! collector; embedded tests separately exercise real graph and storage reads.

use super::*;
use crate::{
    GqlParameters, GraphSetProjection, GraphSetQuantifier, GraphSetValue,
    GraphSymbol, GraphSymbolKind, PreparedGraphText,
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const BASE: u128 = 1_u128 << 100;
type Fault = GqlQueryError<GraphMutationError<()>, usize>;

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}

fn sample() -> GraphValue {
    list(vec![int(1), list(vec![int(2), GraphValue::Scalar(CanonicalScalar::Null)]), list(vec![])])
}

fn policy() -> GraphMutationPolicy {
    GraphMutationPolicy::new(GqlQueryPolicy::new(100, 100, 1_000_000, 1_000_000), 100)
}

fn mutation(edge: bool, value: GraphMutationValue) -> PreparedGraphMutation {
    let text = if edge {
        "MATCH (a)-[r:R]->(b) RETURN r,r.p AS p"
    } else {
        "MATCH (n) RETURN n,n.p AS p"
    };
    let source = PreparedGraphText::prepare(text, |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    PreparedGraphMutation::prepare(source, R, vec![GraphMutationAction::SetProperty {
        target: 0,
        key: Q,
        value,
    }])
    .unwrap()
}

fn input(rows: Vec<(GraphValue, GraphValue)>) -> GqlQueryExecution<GraphValueRow> {
    let count = rows.len() as u64;
    GqlQueryExecution {
        value: rows.into_iter().map(|(target, value)| {
            GraphValueRow::from_owned_values(vec![target, value])
        }).collect(),
        rows: GqlExecutionStats { snapshot_records: count, result_rows: count },
        evaluator: GlaExecutionStats { work_units: 7, scratch_entries: 3 },
    }
}

fn run(
    mutation: &PreparedGraphMutation,
    rows: Vec<(GraphValue, GraphValue)>,
    policy: GraphMutationPolicy,
    checkpoint: impl FnMut() -> Result<(), usize>,
) -> Result<GraphMutationBatch, Fault> {
    mutation.execute_governed(
        policy,
        |_, _| Ok::<_, GqlQueryError<(), usize>>(input(rows)),
        checkpoint,
    )
}

#[test]
fn copied_and_computed_lists_produce_native_vertex_and_edge_intents() {
    for edge in [false, true] {
        let target = if edge { GraphValue::Edge(EId(BASE + 1)) } else { GraphValue::Vertex(VId(BASE + 1)) };
        let identity = if edge { ElementId::Edge(EId(BASE + 1)) } else { ElementId::Vertex(VId(BASE + 1)) };
        for value in [sample(), list(vec![])] {
            for expression in [
                GraphMutationValue::Column(1),
                GraphMutationValue::Composite(GraphSetValue::Value(value.clone())),
            ] {
                let query = mutation(edge, expression);
                let batch = run(&query, vec![(target.clone(), value.clone())], policy(), || Ok(())).unwrap();
                assert_eq!(batch.intents(), &[GraphMutationIntent::StoredProperty {
                    target: identity,
                    key: Q,
                    value: value.to_written_property().unwrap(),
                }]);
                assert_eq!(batch.stats().effects, 1);
                assert_eq!(batch.stats().target_vertices, u64::from(!edge));
                assert_eq!(batch.stats().target_edges, u64::from(edge));
            }
        }
    }
}

#[test]
fn repeated_nested_assignments_collapse_but_deep_disagreement_refuses_both_orders() {
    let query = mutation(false, GraphMutationValue::Column(1));
    let target = GraphValue::Vertex(VId(BASE + 1));
    let first = sample();
    let second = list(vec![int(1), list(vec![int(3), GraphValue::Scalar(CanonicalScalar::Null)]), list(vec![])]);
    let batch = run(&query, vec![(target.clone(), first.clone()); 2], policy(), || Ok(())).unwrap();
    assert_eq!(batch.stats().selection.result_rows, 2);
    assert_eq!(batch.stats().effects, 1);
    for (a, b) in [(first.clone(), second.clone()), (second, first.clone()), (first, int(1))] {
        let result = run(&query, vec![(target.clone(), a), (target.clone(), b)], policy(), || Ok(()));
        assert!(matches!(result, Err(GqlQueryError::Source(
            GraphMutationError::ConflictingAssignment { first_row: 0, row: 1, .. }
        ))));
    }
}

#[test]
fn final_property_and_property_map_bindings_copy_the_list_proposal() {
    let mut catalog = crate::ReverseSymbolCatalog::new();
    catalog.insert_property(P, "p");
    catalog.insert_property(Q, "q");
    let target = ElementId::Vertex(VId(BASE + 1));
    let value = sample();
    let mut proposals = BTreeMap::new();
    proposals.insert((target, Field::Property(P)), Proposal {
        value: Value::Stored(value.to_written_property().unwrap()), row: 0, action: 0,
    });
    let old = GraphValue::map(vec![("p".into(), int(99)), ("q".into(), int(7))]).unwrap();
    let row = GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(BASE + 1)), int(99), old]);
    let mut meter = Meter { policy: policy(), evaluator: GlaExecutionStats::default(), checkpoint: || Ok::<_, usize>(()) };
    let output = returned_row::<(), usize, _>(
        &[GraphMutationBinding::Property { target: 0, key: P, current: 1 },
          GraphMutationBinding::Properties { target: 0, current: 2 }],
        &row, 0, &proposals, Some(&catalog), &mut meter,
    ).unwrap();
    assert_eq!(output.values(), &[
        value.clone(), GraphValue::map(vec![("p".into(), value), ("q".into(), int(7))]).unwrap(),
    ]);
    assert!(meter.evaluator.scratch_entries > 0);
}

#[test]
fn source_conversion_comparison_and_intents_share_one_allowance_and_checkpoint() {
    let query = mutation(false, GraphMutationValue::Column(1));
    let rows = vec![(GraphValue::Vertex(VId(BASE + 1)), sample()); 2];
    let mut checkpoints = 0;
    let complete = run(&query, rows.clone(), policy(), || { checkpoints += 1; Ok(()) }).unwrap();
    let stats = complete.stats();
    assert!(stats.evaluator.work_units > 7);
    assert!(stats.evaluator.scratch_entries > 3);
    let exact = GraphMutationPolicy::new(GqlQueryPolicy::new(2, 2,
        stats.evaluator.work_units, stats.evaluator.scratch_entries), 1);
    assert_eq!(run(&query, rows.clone(), exact, || Ok(())).unwrap().stats(), stats);
    for (work, scratch, effects) in [
        (stats.evaluator.work_units - 1, stats.evaluator.scratch_entries, 1),
        (stats.evaluator.work_units, stats.evaluator.scratch_entries - 1, 1),
        (stats.evaluator.work_units, stats.evaluator.scratch_entries, 0),
    ] {
        assert!(run(&query, rows.clone(), GraphMutationPolicy::new(
            GqlQueryPolicy::new(2, 2, work, scratch), effects), || Ok(())).is_err());
    }
    assert!(checkpoints > 10);
    for fail_at in 0..checkpoints {
        let mut seen = 0;
        let result = run(&query, rows.clone(), policy(), || {
            let at = seen;
            seen += 1;
            if at == fail_at { Err(at) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == fail_at));
    }
    assert_eq!(run(&query, rows, policy(), || Ok(())).unwrap().intents(), complete.intents());
}

#[test]
fn late_map_or_element_values_never_return_a_partial_list_batch() {
    let query = mutation(false, GraphMutationValue::Column(1));
    let target = GraphValue::Vertex(VId(BASE + 1));
    let invalid = GraphValue::map(vec![("not-written".into(), int(1))]).unwrap();
    let result = run(&query, vec![(target.clone(), sample()), (target.clone(), invalid)], policy(), || Ok(()));
    assert!(matches!(result, Err(GqlQueryError::Source(GraphMutationError::NonScalarValue { row: 1, action: 0 }))));
    // An invalid element nested in an otherwise ordinary list is not coerced.
    let expression = GraphMutationValue::Composite(GraphSetValue::List(vec![GraphSetValue::Column(0)]));
    let query = mutation(false, expression);
    assert!(matches!(run(&query, vec![(target, int(0))], policy(), || Ok(())),
        Err(GqlQueryError::Source(GraphMutationError::NonScalarValue { .. }))));
}

#[test]
fn null_targets_skip_the_rhs_but_null_assignments_remain_stored_scalars() {
    let invalid = GraphValue::map(vec![("invalid".into(), int(1))]).unwrap();
    let query = mutation(false, GraphMutationValue::Composite(GraphSetValue::Value(invalid)));
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    assert!(run(&query, vec![(null.clone(), int(0))], policy(), || Ok(())).unwrap().intents().is_empty());
    let query = mutation(false, GraphMutationValue::Column(1));
    let batch = run(&query, vec![(GraphValue::Vertex(VId(BASE + 1)), null)], policy(), || Ok(())).unwrap();
    assert_eq!(batch.intents(), &[GraphMutationIntent::Property {
        vertex: VId(BASE + 1), key: Q, value: Some(CanonicalScalar::Null),
    }]);
}

#[test]
fn bounded_conversion_reserves_payload_before_copy_and_keeps_inputs_immutable() {
    let value = sample();
    let bytes = value.canonical_bytes().unwrap();
    let mut events = Vec::new();
    let expected = written(&value, 7, 3, &mut |event| { events.push(event); Ok::<_, Fault>(()) }).unwrap();
    assert_eq!(events, vec![GlaExecutionEvent::ScratchEntry; 6]);
    for fail_at in 0..events.len() {
        let mut at = 0;
        let result = written(&value, 7, 3, &mut |_| {
            let current = at;
            at += 1;
            if current == fail_at { Err::<(), Fault>(GqlQueryError::Interrupted(current)) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == fail_at));
        assert_eq!(value.canonical_bytes().unwrap(), bytes);
    }
    assert_eq!(returned(&expected, 7, 3, &mut |_| Ok::<_, Fault>(())).unwrap(), value);
    let mut deep = int(1);
    for _ in 0..=GraphValue::MAX_LIST_DEPTH { deep = list(vec![deep]); }
    for invalid in [deep, list(vec![int(0); GraphValue::MAX_LIST_NODES]),
        list(vec![GraphValue::map(vec![]).unwrap()])] {
        assert!(matches!(written(&invalid, 7, 3, &mut |_| Ok::<_, Fault>(())),
            Err(GqlQueryError::Source(GraphMutationError::NonScalarValue { row: 7, action: 3 }))));
    }
}

#[test]
fn return_limit_zero_neither_suppresses_list_updates_nor_late_failures() {
    let mutation = mutation(false, GraphMutationValue::Column(1));
    let query = PreparedGraphMutationQuery::prepare(
        mutation, vec![GraphMutationBinding::Property { target: 0, key: Q, current: 1 }],
        vec![GraphSetProjection::new("q", GraphSetValue::Column(0))], GraphSetQuantifier::All,
    ).unwrap().with_page(0, Some(0));
    let allowance = GraphMutationPolicy::new(GqlQueryPolicy::new(100, 0, 1_000_000, 1_000_000), 100);
    let target = GraphValue::Vertex(VId(BASE + 1));
    let batch = query.execute_governed(allowance,
        |_, _| Ok::<_, GqlQueryError<(), usize>>(input(vec![(target.clone(), sample())])), || Ok(())
    ).unwrap();
    assert!(batch.returning().value.is_empty());
    assert_eq!(batch.mutation().stats().effects, 1);
    let bad = GraphValue::map(vec![]).unwrap();
    let result = query.execute_governed(allowance,
        |_, _| Ok::<_, GqlQueryError<(), usize>>(input(vec![(target.clone(), sample()), (target, bad)])), || Ok(())
    );
    assert!(matches!(result, Err(GqlQueryError::Source(crate::GraphMutationQueryError::Mutation(
        GraphMutationError::NonScalarValue { row: 1, .. }
    )))));
}
