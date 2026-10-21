//! Native conversion and collector laws, without a replacement storage encoder.

use super::*;
use crate::insertion::{GraphInsertEdge, GraphInsertEndpoint, GraphInsertVertex};
use crate::{GqlParameters, GraphMutationValue, PreparedGraphInsertQueryText};
use std::cell::Cell;

type Fault = GqlQueryError<GraphInsertError<(), ()>, usize>;
const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}

fn specimen() -> GraphValue {
    list(vec![
        int(1),
        list(vec![int(2), GraphValue::Scalar(CanonicalScalar::Null)]),
        list(vec![]),
    ])
}

fn reservations(value: &GraphValue) -> Vec<GlaExecutionEvent> {
    let mut events = Vec::new();
    reserve(value, 7, 3, &mut |event| {
        events.push(event);
        Ok::<_, Fault>(())
    })
    .unwrap();
    events
}

#[test]
fn nested_storage_copy_reserves_each_node_and_preserves_native_bytes() {
    let value = specimen();
    let expected = value.to_written_property().unwrap();
    assert_eq!(
        reservations(&value),
        vec![GlaExecutionEvent::ScratchEntry; 6]
    );
    let mut events = Vec::new();
    let result = written(&value, 7, 3, &mut |event| {
        events.push(event);
        Ok::<_, Fault>(())
    })
    .unwrap();
    assert_eq!(result, expected);
    assert_eq!(events, reservations(&value));
    assert_eq!(GraphValue::from_property_value(&result).unwrap(), value);
}

#[test]
fn text_payloads_and_sort_keys_are_reserved_before_the_storage_copy() {
    let payload = "x".repeat(GRAPH_VALUE_PAYLOAD_UNIT_BYTES + 1);
    let text = CanonicalScalar::ucs_basic_text(&payload).unwrap();
    let CanonicalScalar::Text(ref value) = text else {
        unreachable!();
    };
    let payload = value.len().div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
        + value
            .canonical_sort_key()
            .map_or(0, |key| key.len().div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES));
    let value = list(vec![GraphValue::Scalar(text)]);
    assert_eq!(
        reservations(&value),
        vec![GlaExecutionEvent::ScratchEntry; 2 + payload]
    );
}

#[test]
fn every_storage_reservation_can_interrupt_without_changing_the_input() {
    let value = specimen();
    let before = value.canonical_bytes().unwrap();
    let count = reservations(&value).len();
    for fail_at in 0..count {
        let mut observed = 0;
        let result = written(&value, 7, 3, &mut |_| {
            let at = observed;
            observed += 1;
            if at == fail_at {
                Err::<(), Fault>(GqlQueryError::Interrupted(at))
            } else {
                Ok(())
            }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == fail_at));
        assert_eq!(observed, fail_at + 1);
        assert_eq!(value.canonical_bytes().unwrap(), before);
    }
    assert_eq!(
        written(&value, 7, 3, &mut |_| Ok::<_, Fault>(())).unwrap(),
        value.to_written_property().unwrap()
    );
}

#[test]
fn invalid_domains_and_oversized_public_trees_refuse_at_the_original_field() {
    let mut deep = int(1);
    for _ in 0..=GraphValue::MAX_LIST_DEPTH {
        deep = list(vec![deep]);
    }
    for value in [
        list(vec![GraphValue::Vertex(VId(1))]),
        list(vec![GraphValue::Edge(EId(1))]),
        list(vec![GraphValue::map(vec![("x".into(), int(1))]).unwrap()]),
        GraphValue::Vertices(vec![VId(1)].into_boxed_slice()),
        list(vec![int(1); GraphValue::MAX_LIST_NODES]),
        deep,
    ] {
        let result = written(&value, 7, 3, &mut |_| Ok::<_, Fault>(()));
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphInsertError::InputSchema {
                row: 7,
                column: 3
            }))
        ));
    }
    let exact = list(vec![int(1); GraphValue::MAX_LIST_NODES - 1]);
    assert_eq!(reservations(&exact).len(), GraphValue::MAX_LIST_NODES);
}

fn properties(expression: GraphSetValue) -> Properties {
    Properties {
        keys: vec![P],
        projection: vec![GraphSetProjection::new("p", expression)],
    }
}

#[test]
fn column_and_bound_lists_keep_the_same_charges_without_changing_scalar_paths() {
    let value = specimen();
    for expression in [
        GraphSetValue::Column(0),
        GraphSetValue::Value(value.clone()),
    ] {
        let row = GraphValueRow::from_owned_values(vec![value.clone()]);
        let mut events = Vec::new();
        let result = fields(&properties(expression), &row, 7, 0, &mut |event| {
            events.push(event);
            Ok::<_, Fault>(())
        })
        .unwrap();
        let mut expected = vec![GlaExecutionEvent::Work];
        expected.extend(reservations(&value));
        assert_eq!(events, expected);
        assert_eq!(result, vec![(P, value.to_written_property().unwrap())]);
    }
    let mut events = Vec::new();
    let result = fields(
        &properties(GraphSetValue::Column(0)),
        &GraphValueRow::from_owned_values(vec![int(7)]),
        0,
        0,
        &mut |event| {
            events.push(event);
            Ok::<_, Fault>(())
        },
    )
    .unwrap();
    assert_eq!(result, vec![(P, CanonicalScalar::Int(7).into())]);
    assert_eq!(
        events,
        vec![GlaExecutionEvent::Work, GlaExecutionEvent::ScratchEntry]
    );
}

#[test]
fn computed_lists_charge_the_storage_copy_in_addition_to_the_expression_result() {
    let expression = GraphSetValue::List(vec![
        GraphSetValue::Value(int(1)),
        GraphSetValue::Value(specimen()),
    ]);
    let row = GraphValueRow::from_owned_values(Vec::new());
    let mut evaluation = Vec::new();
    let value = crate::set_ops::evaluate_value(&expression, &row, 0, &mut |event| {
        evaluation.push(event);
        Ok::<_, Fault>(())
    })
    .unwrap();
    let mut observed = Vec::new();
    let result = fields(&properties(expression), &row, 7, 0, &mut |event| {
        observed.push(event);
        Ok::<_, Fault>(())
    })
    .unwrap();
    let mut expected = vec![GlaExecutionEvent::Work];
    expected.extend(evaluation);
    expected.extend(reservations(&value));
    expected.push(GlaExecutionEvent::ScratchEntry);
    assert_eq!(observed, expected);
    assert_eq!(result, vec![(P, value.to_written_property().unwrap())]);
}

#[test]
fn collection_copies_spend_the_existing_meter_not_a_refreshed_allowance() {
    let value = specimen();
    let prefix = GlaExecutionStats {
        work_units: 17,
        scratch_entries: 11,
    };
    for (work, scratch, success) in [(23, 17, true), (22, 17, false), (23, 16, false)] {
        let mut meter = Meter {
            policy: GqlQueryPolicy::new(0, 1, work, scratch),
            evaluator: prefix,
            checkpoint: || Ok::<_, usize>(()),
        };
        let result = written::<(), (), usize>(&value, 7, 3, &mut |event| meter.event(event));
        if success {
            assert_eq!(result.unwrap(), value.to_written_property().unwrap());
            assert_eq!(meter.evaluator.work_units, 23);
            assert_eq!(meter.evaluator.scratch_entries, 17);
        } else {
            assert!(matches!(result, Err(GqlQueryError::Evaluator(_))));
        }
    }
}

fn allowance(work: u64, scratch: u64) -> GraphInsertPolicy {
    GraphInsertPolicy::new(GqlQueryPolicy::new(0, 1, work, scratch), 1, 1)
}

fn native_run(
    plan: &PreparedGraphInsert,
    policy: GraphInsertPolicy,
    allocations: &Cell<usize>,
    checkpoint: impl FnMut() -> Result<(), usize>,
) -> ResultOf<GraphInsertBatch, (), (), usize> {
    plan.execute_governed(
        policy,
        |_, _| -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), usize>> {
            panic!("standalone collection creation must not select the graph")
        },
        |request| {
            allocations.set(allocations.get() + 1);
            Ok(match request {
                GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(1_u128 << 100)),
                GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(1_u128 << 101)),
            })
        },
        checkpoint,
    )
}

#[test]
fn vertex_and_edge_collections_keep_atomic_preallocation_and_exact_limits() {
    let expression = GraphMutationValue::Composite(GraphSetValue::List(vec![
        GraphSetValue::Value(int(1)),
        GraphSetValue::Value(specimen()),
    ]));
    let plan = PreparedGraphInsert::prepare_standalone(
        R,
        vec![GraphInsertVertex {
            labels: Vec::new(),
            properties: vec![(P, expression.clone())],
        }],
        vec![GraphInsertEdge {
            source: GraphInsertEndpoint::CreatedVertex(0),
            destination: GraphInsertEndpoint::CreatedVertex(0),
            relation: R,
            properties: vec![(P, expression)],
        }],
    )
    .unwrap();
    let allocations = Cell::new(0);
    let mut checkpoints = 0;
    let batch = native_run(
        &plan,
        allowance(1_000_000, 1_000_000),
        &allocations,
        || {
            checkpoints += 1;
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(allocations.get(), 2);
    assert_eq!(batch.stats().created_vertices, 1);
    assert_eq!(batch.stats().created_edges, 1);
    let expected = list(vec![int(1), specimen()])
        .to_written_property()
        .unwrap();
    for intent in batch.intents() {
        let properties = match intent {
            GraphInsertIntent::Vertex { properties, .. }
            | GraphInsertIntent::Edge { properties, .. } => properties,
        };
        assert_eq!(properties, &vec![(P, expected.clone())]);
    }
    let stats = batch.stats().evaluator;
    let exact = native_run(
        &plan,
        allowance(stats.work_units, stats.scratch_entries),
        &Cell::new(0),
        || Ok(()),
    )
    .unwrap();
    assert_eq!(batch, exact);
    for (work, scratch) in [
        (stats.work_units - 1, stats.scratch_entries),
        (stats.work_units, stats.scratch_entries - 1),
    ] {
        assert!(matches!(
            native_run(&plan, allowance(work, scratch), &Cell::new(0), || Ok(())),
            Err(GqlQueryError::Evaluator(_))
        ));
    }
    let refused_allocations = Cell::new(0);
    assert!(matches!(
        native_run(&plan, allowance(1_000_000, 2), &refused_allocations, || {
            Ok(())
        }),
        Err(GqlQueryError::Evaluator(_))
    ));
    assert_eq!(refused_allocations.get(), 0);
    for fail_at in 0..checkpoints {
        let mut at = 0;
        let result = native_run(
            &plan,
            allowance(1_000_000, 1_000_000),
            &Cell::new(0),
            || {
                let here = at;
                at += 1;
                if here == fail_at {
                    Err(here)
                } else {
                    Ok(())
                }
            },
        );
        assert!(matches!(result, Err(GqlQueryError::Interrupted(here)) if here == fail_at));
    }
}

#[test]
fn textual_collection_return_uses_the_same_frozen_values_without_a_second_source() {
    let plan = PreparedGraphInsertQueryText::prepare(
        "CREATE (n {p:[1,[2,3]]}) RETURN n.p AS p",
        R,
        |kind, name: &str| match (kind, name) {
            (crate::GraphSymbolKind::Property, "p") => Some(crate::GraphSymbol::Property(P)),
            _ => None,
        },
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let result = plan.execute_governed(
        allowance(1_000_000, 1_000_000),
        |_, _| -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), usize>> {
            panic!("source-free RETURN must not invent a MATCH")
        },
        |_| Ok::<_, ()>(ElementId::Vertex(VId(1_u128 << 100))),
        || Ok::<_, usize>(()),
    )
    .unwrap();
    assert_eq!(
        result.returning().value,
        vec![GraphValueRow::from_owned_values(vec![list(vec![
            int(1),
            list(vec![int(2), int(3)]),
        ])])]
    );
    assert_eq!(result.insertion().stats().created_vertices, 1);
}
