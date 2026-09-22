use super::*;
use crate::algebra::{GraphColumn, GraphPatternBuilder, IntegerComparison};
use crate::{
    GraphAggregate, GraphAggregateColumn, GraphAggregateOrder, GraphExactAverage, GraphHavingOp,
    GraphHavingOperand, GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp,
    GraphSetOperation, GraphSetProjection, GraphSetQuantifier, GraphSetValue,
    PreparedGraphAggregate, PreparedGraphSet, PreparedGraphSetAggregate,
};
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{CanonicalScalar, VId};

fn int(n: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(n))
}
fn input() -> PreparedGraphSet {
    let mut source = GraphPatternBuilder::new();
    source.vertex("n").unwrap();
    source
        .prepare_values(
            &[
                GraphColumn::vertex("id", "n"),
                GraphColumn::property("value", "n", PropertyKeyId(1)),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates()
        .into()
}
fn relation() -> PreparedGraphSet {
    input()
        .combine(
            GraphSetOperation::Except,
            GraphSetQuantifier::All,
            input().with_page(0, Some(2)),
        )
        .unwrap()
        .project(
            vec![
                GraphSetProjection::new("amount", GraphSetValue::Column(1)),
                GraphSetProjection::new("owner", GraphSetValue::Column(0)),
            ],
            GraphSetQuantifier::Distinct,
        )
        .unwrap()
}
fn definition() -> PreparedGraphSetAggregate {
    PreparedGraphSetAggregate::prepare(
        relation(),
        &[1],
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::count("nonnull", 0),
            GraphAggregate::count_distinct("distinct", 0),
            GraphAggregate::sum_int("sum", 0),
            GraphAggregate::sum_int_distinct("unique_sum", 0),
            GraphAggregate::average_int("avg", 0),
            GraphAggregate::average_int_distinct("unique_avg", 0),
            GraphAggregate::min("minimum", 0),
            GraphAggregate::max("maximum", 1),
        ],
        0,
        None,
    )
    .unwrap()
}
fn values() -> Vec<GraphAggregateValue> {
    vec![
        GraphAggregateValue::Count(3),
        GraphAggregateValue::Count(2),
        GraphAggregateValue::Count(2),
        GraphAggregateValue::Integer(7),
        GraphAggregateValue::Integer(7),
        GraphAggregateValue::Average(GraphExactAverage::new(7, 2).unwrap()),
        GraphAggregateValue::Average(GraphExactAverage::new(7, 2).unwrap()),
        GraphAggregateValue::Value(int(3)),
        GraphAggregateValue::Value(GraphValue::Vertex(VId(u128::MAX))),
    ]
}

#[test]
fn complete_group_schema_uses_final_relation_positions_and_preserves_all_operators() {
    let query = definition();
    let original = query.canonical_bytes();
    assert_eq!(
        query.incremental_input_column_type(0),
        Some(GraphSetColumnType::Scalar)
    );
    assert_eq!(
        query.incremental_input_column_type(1),
        Some(GraphSetColumnType::Vertex)
    );
    assert_eq!(query.incremental_input_column_type(2), None);
    assert_eq!(query.aggregate_specs().len(), 9);
    let row = query
        .materialize_incremental_row(vec![GraphValue::Vertex(VId(u128::MAX))], values())
        .unwrap();
    assert_eq!(row.values(), values());
    assert!(
        query
            .materialize_incremental_row(vec![int(3)], values())
            .is_none()
    );
    let mut invalid = values();
    invalid[8] = GraphAggregateValue::Value(int(1));
    assert!(
        query
            .materialize_incremental_row(vec![GraphValue::Vertex(VId(1))], invalid)
            .is_none()
    );
    let transformed = query
        .clone()
        .with_key_output_columns(&[])
        .unwrap()
        .with_aggregate_output_prefix(1)
        .unwrap()
        .with_distinct_output(true)
        .with_result_clauses(
            &[],
            &[GraphAggregateOrder::descending(
                GraphAggregateColumn::Aggregate(3),
            )],
        )
        .unwrap();
    let complete = transformed.complete_groups().unwrap();
    assert!(!complete.has_incremental_output_transform());
    assert_eq!(
        complete.input().canonical_bytes(),
        query.input().canonical_bytes()
    );
    assert_eq!(complete.canonical_bytes(), original);
    assert_eq!(query.canonical_bytes(), original);
    let output = transformed
        .project_incremental_output(&row, &mut |_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap();
    assert!(output.keys().is_empty());
    assert_eq!(output.values(), &[GraphAggregateValue::Count(3)]);
}

#[test]
fn single_source_binding_entrypoint_cannot_strip_a_relational_stage() {
    let query = PreparedGraphAggregate::prepare_relation(
        input(),
        &[0],
        &[GraphAggregate::sum_int("total", 1)],
        0,
        None,
    )
    .unwrap();
    assert!(query.supports_incremental_input());
    assert!(
        query
            .evaluate_incremental_input(vec![GraphValue::Vertex(VId(1)), int(7)], &mut |_| Ok::<
                _,
                (),
            >(
                ()
            ))
            .unwrap()
            .is_none()
    );
    let transcript = query.canonical_bytes();
    let compound = PreparedGraphSetAggregate::from_relation(query).unwrap();
    assert_eq!(compound.canonical_bytes(), transcript);
    assert!(compound.supports_incremental_maintenance_with_having());
    let graph = input().incremental_pattern().unwrap().clone();
    let plain =
        PreparedGraphAggregate::prepare(graph, &[], &[GraphAggregate::count_rows("n")], 0, None)
            .unwrap();
    assert!(PreparedGraphSetAggregate::from_relation(plain).is_none());
}

#[test]
fn having_and_output_expressions_share_exact_domains_and_every_control_refusal() {
    let having = GraphHavingExpression::prepare(&[GraphHavingOp::Compare {
        left: GraphHavingOperand::Column(GraphAggregateColumn::Aggregate(5)),
        comparison: IntegerComparison::Greater,
        right: GraphHavingOperand::Integer(3),
    }])
    .unwrap();
    let query = definition().with_having_expression(&having).unwrap();
    let row = query
        .materialize_incremental_row(vec![GraphValue::Vertex(VId(1))], values())
        .unwrap();
    let mut calls = 0;
    assert_eq!(
        query
            .evaluate_incremental_having(&row, &mut |_| {
                calls += 1;
                Ok::<_, usize>(())
            })
            .unwrap(),
        Some(true)
    );
    for stop in 1..=calls {
        let mut seen = 0;
        assert!(matches!(query.evaluate_incremental_having(&row, &mut |_| {
            seen += 1; if seen == stop {Err(stop)} else {Ok(())}
        }), Err(GqlQueryError::Interrupted(n)) if n == stop));
        assert_eq!(seen, stop);
    }
    let expr = GraphIntegerExpression::prepare(&[
        GraphIntegerOp::Column(4),
        GraphIntegerOp::Literal(Some(2)),
        GraphIntegerOp::Binary(GraphIntegerBinary::Multiply),
    ])
    .unwrap();
    let transformed = query
        .with_output_projection(vec![
            GraphSetProjection::new("twice_sum", GraphSetValue::Integer(expr)),
            GraphSetProjection::new("mean", GraphSetValue::Column(6)),
        ])
        .unwrap();
    let mut calls = 0;
    let output = transformed
        .project_incremental_output(&row, &mut |_| {
            calls += 1;
            Ok::<_, usize>(())
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        output.values(),
        &[
            GraphAggregateValue::Integer(14),
            GraphAggregateValue::Average(GraphExactAverage::new(7, 2).unwrap())
        ]
    );
    for stop in 1..=calls {
        let mut seen = 0;
        assert!(
            matches!(transformed.project_incremental_output(&row, &mut |_| {
            seen += 1; if seen == stop {Err(stop)} else {Ok(())}
        }), Err(GqlQueryError::Interrupted(n)) if n == stop)
        );
        assert_eq!(seen, stop);
    }
    assert_eq!(
        transformed.complete_groups().unwrap().having_expression(),
        Some(&having)
    );
}

#[test]
fn unsupported_schema_never_becomes_an_admitted_group_even_when_hidden() {
    let graph = input().incremental_pattern().unwrap().clone();
    let query = PreparedGraphAggregate::prepare_projected(
        graph,
        vec![GraphSetProjection::new(
            "hidden",
            GraphSetValue::List(vec![]),
        )],
        &[],
        &[GraphAggregate::count_rows("n")],
        0,
        Some(0),
    )
    .unwrap();
    assert!(query.complete_groups().is_none());
    let unsupported = PreparedGraphSetAggregate::prepare(
        input(),
        &[],
        &[GraphAggregate::collect("list", 1)],
        0,
        None,
    )
    .unwrap();
    assert!(
        unsupported
            .materialize_incremental_row(
                vec![],
                vec![GraphAggregateValue::Value(GraphValue::List(Box::new([])))]
            )
            .is_none()
    );
}
