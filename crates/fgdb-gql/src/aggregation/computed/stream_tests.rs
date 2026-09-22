//! Shared row-local VM admission and failure semantics for both pull sources.
use super::*;
use crate::algebra::{GraphColumn, GraphPatternBuilder};
use crate::{GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp, GraphSetValue};

type Error = GqlQueryError<GraphAggregateError<()>, usize>;

fn input() -> PreparedGraphPattern<GraphValueRow> {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder
        .prepare_values(
            &[GraphColumn::property("x", "n", PropertyKeyId(1))],
            0,
            None,
        )
        .unwrap()
        .with_duplicates()
}
fn expression(binary: GraphIntegerBinary, operand: i64) -> GraphSetValue {
    GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Column(0),
            GraphIntegerOp::Literal(Some(operand)),
            GraphIntegerOp::Binary(binary),
        ])
        .unwrap(),
    )
}
fn projected() -> PreparedGraphAggregate {
    PreparedGraphAggregate::prepare_projected(
        input(),
        vec![GraphSetProjection::new(
            "twice",
            expression(GraphIntegerBinary::Multiply, 2),
        )],
        &[0],
        &[GraphAggregate::count_rows("n")],
        0,
        None,
    )
    .unwrap()
}
fn row(value: i64) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![GraphValue::Scalar(CanonicalScalar::Int(value))])
}

#[test]
fn projection_owns_one_result_and_plain_input_moves_without_remetering_or_payload_copy() {
    let query = projected();
    let mut events = Vec::new();
    let computed = query
        .evaluate_streamed_input(row(7), &mut |event| {
            events.push(event);
            Ok::<_, Error>(())
        })
        .unwrap();
    assert_eq!(computed, row(14));
    assert!(!events.contains(&GlaExecutionEvent::ResultRow));
    let ordinary = PreparedGraphAggregate::prepare(
        input(),
        &[],
        &[GraphAggregate::sum_int("sum", 0)],
        0,
        None,
    )
    .unwrap();
    let source = row(7);
    let original_storage = source.values().as_ptr();
    let returned = ordinary
        .evaluate_streamed_input(source, &mut |_| -> Result<(), Error> {
            panic!("plain input must not be reprojected or remetered")
        })
        .unwrap();
    assert_eq!(returned.values().as_ptr(), original_storage);
    assert_eq!(returned, row(7));
}

#[test]
fn every_expression_checkpoint_refuses_and_the_same_definition_remains_reusable() {
    let query = projected();
    let transcript = query.canonical_bytes();
    let mut total = 0;
    let expected = query
        .evaluate_streamed_input(row(-9), &mut |_| {
            total += 1;
            Ok::<_, Error>(())
        })
        .unwrap();
    assert!(total > 0);
    for stop in 1..=total {
        let mut calls = 0;
        let result = query.evaluate_streamed_input(row(-9), &mut |_| {
            calls += 1;
            if calls == stop {
                Err(Error::Interrupted(stop))
            } else {
                Ok(())
            }
        });
        assert!(matches!(result, Err(Error::Interrupted(at)) if at == stop));
        assert_eq!(calls, stop);
        assert_eq!(query.canonical_bytes(), transcript);
        assert_eq!(
            query
                .evaluate_streamed_input(row(-9), &mut |_| Ok::<_, Error>(()))
                .unwrap(),
            expected
        );
    }
}

#[test]
fn unused_computed_columns_still_execute_and_preserve_local_binding_error_context() {
    let query = PreparedGraphAggregate::prepare_projected(
        input(),
        vec![
            GraphSetProjection::new("used", GraphSetValue::Column(0)),
            GraphSetProjection::new("unused", expression(GraphIntegerBinary::Divide, 0)),
        ],
        &[],
        &[GraphAggregate::sum_int("sum", 0)],
        0,
        None,
    )
    .unwrap();
    assert!(query.supports_row_local_aggregate_stream());
    let error = query
        .evaluate_streamed_input(row(5), &mut |_| Ok::<_, Error>(()))
        .unwrap_err();
    assert!(matches!(
        error,
        Error::Source(GraphAggregateError::InputExpression {
            row: 0,
            column: 1,
            ..
        })
    ));
}

#[test]
fn row_local_input_admission_never_erases_relations_or_result_clauses() {
    let query = projected();
    assert!(query.supports_row_local_aggregate_stream());
    assert!(!query.supports_incremental_maintenance());
    let mut altered = query.clone();
    altered.offset = 1;
    assert!(!altered.supports_row_local_aggregate_stream());
    altered = query.clone();
    altered.count = Some(0);
    assert!(!altered.supports_row_local_aggregate_stream());
    altered = query.clone();
    altered.output_distinct = true;
    assert!(!altered.supports_row_local_aggregate_stream());
    altered = query.clone();
    altered.output_aggregates = 0;
    assert!(!altered.supports_row_local_aggregate_stream());
    altered = query.clone();
    altered.relational_input = Some(PreparedGraphSet::from(input()));
    assert!(!altered.supports_row_local_aggregate_stream());
    assert!(
        !query
            .clone()
            .with_key_output_columns(&[])
            .unwrap()
            .supports_row_local_aggregate_stream()
    );
}
