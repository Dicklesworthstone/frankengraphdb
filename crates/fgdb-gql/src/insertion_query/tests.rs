use super::*;
use crate::algebra::{GraphValue, GraphValueOrder};
use crate::insertion::{
    GraphInsertEdge, GraphInsertEndpoint, GraphInsertIntent, GraphInsertLimitDimension,
    GraphInsertVertex,
};
use crate::{
    GlaLimitDimension, GqlBudgetDimension, GqlParameters, GqlScalarParameter, GraphIntegerBinary,
    GraphIntegerError, GraphIntegerErrorKind, GraphIntegerExpression, GraphIntegerOp,
    GraphMutationValue, GraphSetValue, PreparedGraphSet, PreparedGraphText,
};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::{CanonicalScalar, EId, VId};
use std::cell::Cell;

const R: RelationId = RelationId(9);
const P: PropertyKeyId = PropertyKeyId(4);
type ResultOf = Result<
    GraphInsertQueryBatch,
    GqlQueryError<GraphInsertQueryError<&'static str, &'static str>, usize>,
>;

fn policy() -> GraphInsertPolicy {
    GraphInsertPolicy::new(GqlQueryPolicy::new(100, 100, 100_000, 100_000), 100, 100)
}
fn scalar(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}
fn unwind(values: Vec<GraphValue>) -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .unwind(
            "value".into(),
            GraphSetValue::Value(GraphValue::List(values.into())),
        )
        .unwrap()
}
fn vertex(value: GraphMutationValue) -> GraphInsertVertex {
    GraphInsertVertex {
        labels: vec![LabelId(3)],
        properties: vec![(P, value)],
    }
}
fn insertion(values: Vec<GraphValue>) -> PreparedGraphInsert {
    PreparedGraphInsert::prepare_relation(
        unwind(values),
        R,
        vec![vertex(GraphMutationValue::Column(0))],
        vec![GraphInsertEdge {
            source: GraphInsertEndpoint::CreatedVertex(0),
            destination: GraphInsertEndpoint::CreatedVertex(0),
            relation: R,
            properties: vec![(P, GraphMutationValue::Column(0))],
        }],
    )
    .unwrap()
}
fn projection(columns: usize) -> Vec<GraphSetProjection> {
    (0..columns)
        .map(|column| {
            GraphSetProjection::new(format!("column_{column}"), GraphSetValue::Column(column))
        })
        .collect()
}
fn query(values: Vec<GraphValue>) -> PreparedGraphInsertQuery {
    PreparedGraphInsertQuery::prepare(
        insertion(values),
        vec![
            GraphInsertBinding::Input(0),
            GraphInsertBinding::CreatedVertex(0),
            GraphInsertBinding::CreatedEdge(0),
            GraphInsertBinding::VertexProperty { vertex: 0, key: P },
            GraphInsertBinding::EdgeProperty { edge: 0, key: P },
            GraphInsertBinding::VertexProperty {
                vertex: 0,
                key: PropertyKeyId(99),
            },
        ],
        projection(6),
        GraphSetQuantifier::All,
    )
    .unwrap()
}
fn no_source(
    _: &PreparedGraphPattern<GraphValueRow>,
    _: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>> {
    panic!("source-free insertion RETURN must not scan a graph")
}
fn identity(request: GraphInsertRequest) -> Result<ElementId, &'static str> {
    Ok(match request {
        GraphInsertRequest::Vertex { row, vertex } => {
            ElementId::Vertex(VId(100 + row as u128 * 256 + vertex as u128))
        }
        GraphInsertRequest::Edge { row, edge } => {
            ElementId::Edge(EId(200 + row as u128 * 256 + edge as u128))
        }
    })
}
fn run(query: &PreparedGraphInsertQuery, policy: GraphInsertPolicy) -> ResultOf {
    query.execute_governed(policy, no_source, identity, || Ok(()))
}
fn values(result: &GraphInsertQueryBatch) -> Vec<Vec<GraphValue>> {
    result
        .returning()
        .value
        .iter()
        .map(|row| row.values().to_vec())
        .collect()
}
fn divide_by_input() -> GraphSetValue {
    GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Literal(Some(12)),
            GraphIntegerOp::Column(0),
            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
        ])
        .unwrap(),
    )
}

#[test]
fn occurrence_rows_keep_duplicate_values_distinct_identities_and_frozen_properties() {
    let input = vec![scalar(3), scalar(1), scalar(3), null()];
    let query = query(input.clone());
    let result = run(&query, policy()).unwrap();
    let expected: Vec<_> = input
        .iter()
        .enumerate()
        .map(|(row, value)| {
            vec![
                value.clone(),
                GraphValue::Vertex(VId(100 + row as u128 * 256)),
                GraphValue::Edge(EId(200 + row as u128 * 256)),
                value.clone(),
                value.clone(),
                null(),
            ]
        })
        .collect();
    assert_eq!(values(&result), expected);
    assert_eq!(result.insertion().stats().created_vertices, 4);
    assert_eq!(result.insertion().stats().created_edges, 4);
    assert_eq!(result.returning().rows.result_rows, 4);
    for (row, intents) in result
        .insertion()
        .intents()
        .as_chunks::<2>()
        .0
        .iter()
        .enumerate()
    {
        let vertex = VId(100 + row as u128 * 256);
        assert!(
            matches!(&intents[0], GraphInsertIntent::Vertex { vertex: id, properties, .. }
            if *id == vertex && properties[0].1 == *input[row].as_scalar().unwrap())
        );
        assert!(
            matches!(&intents[1], GraphInsertIntent::Edge { edge, source, destination, .. }
            if *edge == EId(200 + row as u128 * 256) && *source == vertex && *destination == vertex)
        );
    }
}

#[test]
fn standalone_return_maps_each_declaration_and_missing_edge_property_to_null() {
    let insertion = PreparedGraphInsert::prepare_standalone(
        R,
        vec![
            vertex(GraphMutationValue::Literal(
                GqlScalarParameter::new(CanonicalScalar::Int(7)).unwrap(),
            )),
            vertex(GraphMutationValue::Literal(
                GqlScalarParameter::new(CanonicalScalar::Int(8)).unwrap(),
            )),
        ],
        vec![GraphInsertEdge {
            source: GraphInsertEndpoint::CreatedVertex(1),
            destination: GraphInsertEndpoint::CreatedVertex(0),
            relation: R,
            properties: vec![],
        }],
    )
    .unwrap();
    let query = PreparedGraphInsertQuery::prepare(
        insertion,
        vec![
            GraphInsertBinding::CreatedVertex(1),
            GraphInsertBinding::CreatedEdge(0),
            GraphInsertBinding::CreatedVertex(0),
            GraphInsertBinding::VertexProperty { vertex: 1, key: P },
            GraphInsertBinding::EdgeProperty { edge: 0, key: P },
        ],
        projection(5),
        GraphSetQuantifier::All,
    )
    .unwrap();
    let result = run(&query, policy()).unwrap();
    assert_eq!(
        values(&result),
        vec![vec![
            GraphValue::Vertex(VId(101)),
            GraphValue::Edge(EId(200)),
            GraphValue::Vertex(VId(100)),
            scalar(8),
            null(),
        ]]
    );
    assert!(matches!(
        result.insertion().intents()[2],
        GraphInsertIntent::Edge {
            source: VId(101),
            destination: VId(100),
            ..
        }
    ));
}

#[test]
fn distinct_order_and_page_limit_only_returned_rows_never_creation() {
    let insertion = insertion(vec![scalar(3), scalar(1), scalar(3), scalar(2)]);
    let query = PreparedGraphInsertQuery::prepare(
        insertion,
        vec![GraphInsertBinding::VertexProperty { vertex: 0, key: P }],
        projection(1),
        GraphSetQuantifier::Distinct,
    )
    .unwrap()
    .with_order_by(&[GraphValueOrder::descending(0)])
    .unwrap()
    .with_page(1, Some(1));
    let mut allowance = policy();
    allowance.query.rows = GqlQueryPolicy::new(0, 1, 0, 0).rows;
    let result = run(&query, allowance).unwrap();
    assert_eq!(values(&result), vec![vec![scalar(2)]]);
    assert_eq!(result.insertion().stats().selection.result_rows, 4);
    assert_eq!(result.insertion().stats().created_vertices, 4);
    assert_eq!(result.insertion().stats().created_edges, 4);
    assert_eq!(result.returning().rows.result_rows, 1);
    let empty = query.with_page(0, Some(0));
    allowance.query.rows = GqlQueryPolicy::new(0, 0, 0, 0).rows;
    let empty = run(&empty, allowance).unwrap();
    assert!(empty.returning().value.is_empty());
    assert_eq!(empty.insertion().intents(), result.insertion().intents());
}

#[test]
fn late_return_failure_spends_ids_but_exposes_no_proposal_even_under_limit_zero() {
    let query = PreparedGraphInsertQuery::prepare(
        insertion(vec![scalar(3), scalar(0)]),
        vec![GraphInsertBinding::Input(0)],
        vec![GraphSetProjection::new("quotient", divide_by_input())],
        GraphSetQuantifier::All,
    )
    .unwrap();
    for query in [query.clone(), query.with_page(0, Some(0))] {
        let allocated = Cell::new(0);
        let result: ResultOf = query.execute_governed(
            policy(),
            no_source,
            |request| {
                allocated.set(allocated.get() + 1);
                identity(request)
            },
            || Ok(()),
        );
        assert_eq!(allocated.get(), 4);
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphInsertQueryError::Returning(
                GraphSetExecutionError::Projection {
                    row: 1,
                    column: 0,
                    error: GraphIntegerError {
                        kind: GraphIntegerErrorKind::DivisionByZero,
                        ..
                    },
                }
            )))
        ));
    }
}

#[test]
fn all_property_rows_validate_before_first_identity_even_when_return_only_uses_ids() {
    let query = PreparedGraphInsertQuery::prepare(
        insertion(vec![scalar(7), GraphValue::List(vec![scalar(8)].into())]),
        vec![GraphInsertBinding::CreatedVertex(0)],
        projection(1),
        GraphSetQuantifier::All,
    )
    .unwrap();
    let result: ResultOf = query.execute_governed(
        policy(),
        no_source,
        |_| panic!("invalid properties must precede every identity request"),
        || Ok(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphInsertQueryError::Insertion(
            GraphInsertError::InputSchema { row: 1, column: 0 }
        )))
    ));
}

#[test]
fn empty_relations_return_nothing_and_allocate_nothing() {
    let query = query(vec![]);
    let result: ResultOf = query.execute_governed(
        GraphInsertPolicy::new(GqlQueryPolicy::new(0, 0, 1000, 1000), 0, 0),
        no_source,
        |_| panic!("empty input has no creation occurrences"),
        || Ok(()),
    );
    let result = result.unwrap();
    assert!(result.returning().value.is_empty());
    assert!(result.insertion().intents().is_empty());
    assert_eq!(result.insertion().stats().created_vertices, 0);
}

#[test]
fn all_return_stages_share_creation_work_and_scratch_counters_without_reset() {
    let query = query(vec![scalar(7), scalar(8)])
        .with_order_by(&[GraphValueOrder::descending(0)])
        .unwrap()
        .with_page(0, Some(1));
    let baseline = run(&query, policy()).unwrap();
    let prefix = baseline.insertion().stats().evaluator;
    let whole = baseline.returning().evaluator;
    assert!(whole.work_units > prefix.work_units);
    assert!(whole.scratch_entries > prefix.scratch_entries);
    let exact = GraphInsertPolicy::new(
        GqlQueryPolicy::new(0, 1, whole.work_units, whole.scratch_entries),
        2,
        2,
    );
    assert_eq!(run(&query, exact).unwrap(), baseline);
    for dimension in [
        GlaLimitDimension::WorkUnits,
        GlaLimitDimension::ScratchEntries,
    ] {
        let mut tight = exact;
        let limit = match dimension {
            GlaLimitDimension::WorkUnits => {
                tight.query.evaluator.max_work_units = prefix.work_units;
                prefix.work_units
            }
            GlaLimitDimension::ScratchEntries => {
                tight.query.evaluator.max_scratch_entries = prefix.scratch_entries;
                prefix.scratch_entries
            }
        };
        let allocated = Cell::new(0);
        let result: ResultOf = query.execute_governed(
            tight,
            no_source,
            |request| {
                allocated.set(allocated.get() + 1);
                identity(request)
            },
            || Ok(()),
        );
        assert_eq!(allocated.get(), 4);
        assert!(matches!(result, Err(GqlQueryError::Evaluator(error))
            if error.dimension == dimension && error.limit == limit && error.observed > u128::from(limit)));
    }
}

#[test]
fn final_row_budget_and_creation_limits_refuse_at_their_separate_boundaries() {
    let query = query(vec![scalar(7), scalar(8)]);
    let mut tight = policy();
    tight.query.rows = GqlQueryPolicy::new(0, 1, 0, 0).rows;
    assert!(matches!(run(&query, tight), Err(GqlQueryError::Rows(error))
        if error.dimension == GqlBudgetDimension::ResultRows && error.observed == 2));
    tight = policy();
    tight.max_vertices = 1;
    let result: ResultOf = query.with_page(0, Some(0)).execute_governed(
        tight,
        no_source,
        |_| panic!("creation limits precede allocation regardless of output LIMIT"),
        || Ok(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphInsertQueryError::Insertion(
            GraphInsertError::Limit {
                dimension: GraphInsertLimitDimension::Vertices,
                observed: 2,
                ..
            }
        )))
    ));
}

#[test]
fn every_creation_and_return_checkpoint_can_cancel_without_a_batch() {
    let query = query(vec![scalar(7), scalar(8)])
        .with_order_by(&[GraphValueOrder::descending(0)])
        .unwrap()
        .with_page(1, Some(1));
    let calls = Cell::new(0);
    let baseline: ResultOf = query.execute_governed(policy(), no_source, identity, || {
        calls.set(calls.get() + 1);
        Ok(())
    });
    let baseline = baseline.unwrap();
    assert!(calls.get() > 30);
    for stop in 1..=calls.get() {
        let mut observed = 0;
        let result: ResultOf = query.execute_governed(policy(), no_source, identity, || {
            observed += 1;
            if observed == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
    }
    assert_eq!(run(&query, policy()).unwrap(), baseline);
}

#[test]
fn source_identity_return_and_edge_endpoints_share_the_exact_admitted_match() {
    let pattern = PreparedGraphText::prepare("MATCH (n) RETURN n", |_, _| None)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let insertion = PreparedGraphInsert::prepare(
        pattern,
        R,
        vec![vertex(GraphMutationValue::Literal(
            GqlScalarParameter::new(CanonicalScalar::Int(9)).unwrap(),
        ))],
        vec![GraphInsertEdge {
            source: GraphInsertEndpoint::Column(0),
            destination: GraphInsertEndpoint::CreatedVertex(0),
            relation: R,
            properties: vec![],
        }],
    )
    .unwrap();
    let query = PreparedGraphInsertQuery::prepare(
        insertion,
        vec![
            GraphInsertBinding::Input(0),
            GraphInsertBinding::CreatedVertex(0),
        ],
        projection(2),
        GraphSetQuantifier::All,
    )
    .unwrap();
    let source_calls = Cell::new(0);
    let result: ResultOf = query.execute_governed(
        policy(),
        |pattern, allowance| {
            source_calls.set(source_calls.get() + 1);
            pattern.plan().execute_governed_with_properties(
                2,
                [VId(7), VId(2)],
                [],
                |_, _| Ok(true),
                |_, _| Ok(None),
                allowance,
                || Ok(()),
            )
        },
        identity,
        || Ok(()),
    );
    let result = result.unwrap();
    assert_eq!(source_calls.get(), 1);
    assert_eq!(result.returning().rows.snapshot_records, 2);
    assert_eq!(
        values(&result),
        vec![
            vec![GraphValue::Vertex(VId(2)), GraphValue::Vertex(VId(100))],
            vec![GraphValue::Vertex(VId(7)), GraphValue::Vertex(VId(356))],
        ]
    );
    for (row, source) in [VId(2), VId(7)].into_iter().enumerate() {
        assert!(matches!(result.insertion().intents()[2 * row + 1],
            GraphInsertIntent::Edge { source: actual, destination, .. }
            if actual == source && destination == VId(100 + row as u128 * 256)));
    }
    let failed: ResultOf = query.execute_governed(
        policy(),
        |_, _| Err(GqlQueryError::Source("read failed")),
        |_| panic!("source failure precedes allocation"),
        || Ok(()),
    );
    assert!(matches!(
        failed,
        Err(GqlQueryError::Source(GraphInsertQueryError::Insertion(
            GraphInsertError::Source("read failed")
        )))
    ));
}

#[test]
fn canonical_identity_includes_bindings_property_keys_order_page_and_projection() {
    let query = query(vec![scalar(7)]);
    let bytes = query.canonical_bytes();
    assert_eq!(bytes, query.clone().canonical_bytes());
    assert_ne!(bytes, query.clone().with_page(0, Some(1)).canonical_bytes());
    assert_ne!(
        bytes,
        query
            .clone()
            .with_order_by(&[GraphValueOrder::descending(0)])
            .unwrap()
            .canonical_bytes()
    );
    for binding in [
        GraphInsertBinding::CreatedVertex(0),
        GraphInsertBinding::VertexProperty { vertex: 0, key: P },
        GraphInsertBinding::VertexProperty {
            vertex: 0,
            key: PropertyKeyId(99),
        },
        GraphInsertBinding::CreatedEdge(0),
    ] {
        let changed = PreparedGraphInsertQuery::prepare(
            query.insertion().clone(),
            vec![binding],
            projection(1),
            GraphSetQuantifier::All,
        )
        .unwrap();
        assert_ne!(bytes, changed.canonical_bytes());
    }
    let aliased = PreparedGraphInsertQuery::prepare(
        query.insertion().clone(),
        query.bindings.clone(),
        (0..6)
            .map(|column| {
                GraphSetProjection::new(format!("renamed_{column}"), GraphSetValue::Column(column))
            })
            .collect(),
        GraphSetQuantifier::All,
    )
    .unwrap();
    assert_eq!(bytes, aliased.canonical_bytes());
}

#[test]
fn prepare_rejects_unknown_declarations_and_invalid_projection_before_execution() {
    for binding in [
        GraphInsertBinding::Input(1),
        GraphInsertBinding::CreatedVertex(1),
        GraphInsertBinding::CreatedEdge(1),
        GraphInsertBinding::VertexProperty { vertex: 1, key: P },
        GraphInsertBinding::EdgeProperty { edge: 1, key: P },
    ] {
        assert!(matches!(
            PreparedGraphInsertQuery::prepare(
                insertion(vec![scalar(1)]),
                vec![binding],
                projection(1),
                GraphSetQuantifier::All,
            ),
            Err(GraphInsertQueryBuildError::Binding { binding: 0 })
        ));
    }
    assert!(matches!(
        PreparedGraphInsertQuery::prepare(
            insertion(vec![scalar(1)]),
            vec![],
            projection(1),
            GraphSetQuantifier::All,
        ),
        Err(GraphInsertQueryBuildError::Projection(_))
    ));
    assert!(matches!(
        query(vec![scalar(1)]).with_order_by(&[GraphValueOrder::ascending(6)]),
        Err(GraphOrderError::UnknownColumn { column: 6 })
    ));
}

#[test]
fn constant_return_needs_no_input_cells_and_standalone_still_has_one_occurrence() {
    let insertion = PreparedGraphInsert::prepare_standalone(
        R,
        vec![GraphInsertVertex {
            labels: vec![],
            properties: vec![],
        }],
        vec![],
    )
    .unwrap();
    let query = PreparedGraphInsertQuery::prepare(
        insertion,
        vec![],
        vec![GraphSetProjection::new(
            "constant",
            GraphSetValue::Value(scalar(42)),
        )],
        GraphSetQuantifier::All,
    )
    .unwrap();
    let result = run(&query, policy()).unwrap();
    assert_eq!(values(&result), vec![vec![scalar(42)]]);
    assert_eq!(result.insertion().stats().created_vertices, 1);
    assert_eq!(result.insertion().stats().selection.result_rows, 1);
}

#[test]
fn unused_large_source_lists_are_not_retained_by_the_creation_collector() {
    let mut private_costs = Vec::new();
    let mut retained_costs = Vec::new();
    for width in [1_usize, 200] {
        let payload = vec![scalar(5); width];
        let input = unwind(vec![GraphValue::List(payload.clone().into())])
            .unwind(
                "element".into(),
                GraphSetValue::List(vec![GraphSetValue::Value(scalar(7))]),
            )
            .unwrap();
        let source_stats = input
            .execute_governed(policy().query, no_source, || Ok(()))
            .unwrap();
        let insertion = PreparedGraphInsert::prepare_relation(
            input,
            R,
            vec![vertex(GraphMutationValue::Column(1))],
            vec![],
        )
        .unwrap();
        let identity_only = PreparedGraphInsertQuery::prepare(
            insertion.clone(),
            vec![GraphInsertBinding::CreatedVertex(0)],
            projection(1),
            GraphSetQuantifier::All,
        )
        .unwrap();
        let result = run(&identity_only, policy()).unwrap();
        let collector = result.insertion().stats().evaluator;
        private_costs.push((
            collector.work_units - source_stats.evaluator.work_units,
            collector.scratch_entries - source_stats.evaluator.scratch_entries,
        ));
        let retained = PreparedGraphInsertQuery::prepare(
            insertion,
            vec![GraphInsertBinding::Input(0)],
            projection(1),
            GraphSetQuantifier::All,
        )
        .unwrap();
        let result = run(&retained, policy()).unwrap();
        assert_eq!(
            values(&result),
            vec![vec![GraphValue::List(payload.into())]]
        );
        retained_costs.push(
            result.insertion().stats().evaluator.scratch_entries
                - source_stats.evaluator.scratch_entries,
        );
    }
    assert_eq!(private_costs[0], private_costs[1]);
    assert!(retained_costs[1] > retained_costs[0] + 100);
}

#[test]
fn return_property_binds_the_computed_creation_value_and_native_null_arithmetic() {
    let expression = GraphIntegerExpression::prepare(&[
        GraphIntegerOp::Literal(Some(12)),
        GraphIntegerOp::Column(0),
        GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
    ])
    .unwrap();
    let insertion = PreparedGraphInsert::prepare_relation(
        unwind(vec![scalar(3), scalar(4), null()]),
        R,
        vec![vertex(GraphMutationValue::Expression(expression))],
        vec![],
    )
    .unwrap();
    let query = PreparedGraphInsertQuery::prepare(
        insertion,
        vec![GraphInsertBinding::VertexProperty { vertex: 0, key: P }],
        vec![
            GraphSetProjection::new("property", GraphSetValue::Column(0)),
            GraphSetProjection::new(
                "plus_one",
                GraphSetValue::Integer(
                    GraphIntegerExpression::prepare(&[
                        GraphIntegerOp::Column(0),
                        GraphIntegerOp::Literal(Some(1)),
                        GraphIntegerOp::Binary(GraphIntegerBinary::Add),
                    ])
                    .unwrap(),
                ),
            ),
        ],
        GraphSetQuantifier::All,
    )
    .unwrap();
    let result = run(&query, policy()).unwrap();
    assert_eq!(
        values(&result),
        vec![
            vec![scalar(4), scalar(5)],
            vec![scalar(3), scalar(4)],
            vec![null(), null()],
        ]
    );
    for (intent, expected) in result.insertion().intents().iter().zip([
        CanonicalScalar::Int(4),
        CanonicalScalar::Int(3),
        CanonicalScalar::Null,
    ]) {
        assert!(
            matches!(intent, GraphInsertIntent::Vertex { properties, .. }
            if properties == &[(P, expected)])
        );
    }
}
