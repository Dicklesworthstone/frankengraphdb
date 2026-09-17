//! Public relational projection through the original governed GLA source.

use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::algebra::{GraphValueOrder, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GqlScalarParameter,
    GraphIntegerBinary, GraphIntegerErrorKind, GraphIntegerExpression, GraphIntegerOp,
    GraphIntegerUnary, GraphSetBuildError, GraphSetExecutionError, GraphSetOperation,
    GraphSetProjection, GraphSetProjectionError, GraphSetQuantifier, GraphSetValue, GraphSymbol,
    GraphSymbolKind, MAX_GRAPH_SET_DEPTH, PreparedGraphSet, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, VId};
use std::cell::Cell;

type QueryResult<C = ()> =
    Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<()>, C>>;
fn input() -> PreparedGraphSet {
    PreparedGraphText::prepare("MATCH (n) RETURN n,n.p AS p", |kind, name| {
        (kind == GraphSymbolKind::Property && name == "p")
            .then_some(GraphSymbol::Property(PropertyKeyId(1)))
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
    .into()
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000)
}
fn expression(ops: &[GraphIntegerOp]) -> GraphSetValue {
    GraphSetValue::Integer(GraphIntegerExpression::prepare(ops).unwrap())
}
fn absolute(quantifier: GraphSetQuantifier) -> PreparedGraphSet {
    input()
        .project(
            vec![GraphSetProjection::new(
                "value",
                expression(&[
                    GraphIntegerOp::Column(1),
                    GraphIntegerOp::Unary(GraphIntegerUnary::Abs),
                ]),
            )],
            quantifier,
        )
        .unwrap()
}
fn run(
    query: &PreparedGraphSet,
    values: &[Option<CanonicalScalar>],
    budget: GqlQueryPolicy,
) -> QueryResult {
    query.execute_governed(
        budget,
        |pattern, remaining| {
            pattern.plan().execute_governed_with_properties(
                values.len() as u64,
                (0..values.len()).map(|i| VId(i as u128 + 1)),
                [],
                |_, _| Ok::<_, ()>(true),
                |vid, _| Ok(values[vid.0 as usize - 1].as_ref()),
                remaining,
                || Ok::<_, ()>(()),
            )
        },
        || Ok::<_, ()>(()),
    )
}
fn integers(rows: &[GraphValueRow]) -> Vec<Option<i64>> {
    rows.iter()
        .map(|row| match row.values()[0].as_scalar().unwrap() {
            CanonicalScalar::Null => None,
            CanonicalScalar::Int(value) => Some(*value),
            _ => panic!("noninteger fixture output"),
        })
        .collect()
}
fn fixture() -> Vec<Option<CanonicalScalar>> {
    vec![
        Some(CanonicalScalar::Int(-2)),
        Some(CanonicalScalar::Int(2)),
        Some(CanonicalScalar::Int(-3)),
        Some(CanonicalScalar::Int(3)),
        Some(CanonicalScalar::Null),
        None,
    ]
}

#[test]
fn distinct_and_ordering_use_computed_values_not_hidden_inputs() {
    let values = fixture();
    assert_eq!(
        integers(
            &run(&absolute(GraphSetQuantifier::All), &values, policy())
                .unwrap()
                .value
        ),
        vec![None, None, Some(2), Some(2), Some(3), Some(3)]
    );
    let query = absolute(GraphSetQuantifier::Distinct);
    assert_eq!(
        integers(&run(&query, &values, policy()).unwrap().value),
        vec![None, Some(2), Some(3)]
    );
    let page = query
        .with_order_by(&[GraphValueOrder::descending(0)])
        .unwrap()
        .with_page(1, Some(1));
    assert_eq!(
        integers(&run(&page, &values, policy()).unwrap().value),
        vec![Some(2)]
    );
    let reordered = page
        .project(
            vec![GraphSetProjection::new(
                "negated",
                expression(&[
                    GraphIntegerOp::Column(0),
                    GraphIntegerOp::Unary(GraphIntegerUnary::Negate),
                ]),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap();
    assert_eq!(
        integers(&run(&reordered, &values, policy()).unwrap().value),
        vec![Some(-2)]
    );
}

#[test]
fn projected_relations_compose_with_all_six_set_variants() {
    let values = fixture();
    let right = input()
        .with_page(0, Some(1))
        .project(
            vec![GraphSetProjection::new(
                "other",
                GraphSetValue::Literal(GqlScalarParameter::new(CanonicalScalar::Int(2)).unwrap()),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap();
    for (operation, quantifier, expected) in [
        (
            GraphSetOperation::Union,
            GraphSetQuantifier::All,
            vec![None, None, Some(2), Some(2), Some(2), Some(3), Some(3)],
        ),
        (
            GraphSetOperation::Union,
            GraphSetQuantifier::Distinct,
            vec![None, Some(2), Some(3)],
        ),
        (
            GraphSetOperation::Intersect,
            GraphSetQuantifier::All,
            vec![Some(2)],
        ),
        (
            GraphSetOperation::Intersect,
            GraphSetQuantifier::Distinct,
            vec![Some(2)],
        ),
        (
            GraphSetOperation::Except,
            GraphSetQuantifier::All,
            vec![None, None, Some(2), Some(3), Some(3)],
        ),
        (
            GraphSetOperation::Except,
            GraphSetQuantifier::Distinct,
            vec![None, Some(3)],
        ),
    ] {
        let query = absolute(GraphSetQuantifier::All)
            .combine(operation, quantifier, right.clone())
            .unwrap();
        assert_eq!(
            integers(&run(&query, &values, policy()).unwrap().value),
            expected
        );
    }
}

#[test]
fn definitions_validate_lazy_references_names_and_total_depth() {
    assert!(matches!(
        input().project(vec![], GraphSetQuantifier::All),
        Err(GraphSetProjectionError::Empty)
    ));
    for value in [
        GraphSetValue::Column(999),
        expression(&[GraphIntegerOp::Column(999)]),
    ] {
        assert!(matches!(
            input().project(
                vec![GraphSetProjection::new("x", value)],
                GraphSetQuantifier::All
            ),
            Err(GraphSetProjectionError::UnknownInput { .. })
        ));
    }
    let lazy_vertex = expression(&[
        GraphIntegerOp::Literal(Some(1)),
        GraphIntegerOp::Column(0),
        GraphIntegerOp::Coalesce,
    ]);
    assert!(matches!(
        input().project(
            vec![GraphSetProjection::new("x", lazy_vertex)],
            GraphSetQuantifier::All
        ),
        Err(GraphSetProjectionError::IntegerInput { .. })
    ));
    let mut query = input();
    for _ in 1..MAX_GRAPH_SET_DEPTH {
        query = query
            .project(
                vec![GraphSetProjection::new("n", GraphSetValue::Column(0))],
                GraphSetQuantifier::All,
            )
            .unwrap();
    }
    assert!(matches!(
        query.project(
            vec![GraphSetProjection::new("n", GraphSetValue::Column(0))],
            GraphSetQuantifier::All
        ),
        Err(GraphSetProjectionError::SetBuild(
            GraphSetBuildError::TooDeep { .. }
        ))
    ));
}

#[test]
fn projection_errors_are_not_hidden_by_distinct_or_zero_output() {
    let query = input()
        .project(
            vec![GraphSetProjection::new(
                "private_name",
                expression(&[
                    GraphIntegerOp::Column(1),
                    GraphIntegerOp::Literal(Some(1)),
                    GraphIntegerOp::Binary(GraphIntegerBinary::Add),
                ]),
            )],
            GraphSetQuantifier::Distinct,
        )
        .unwrap()
        .with_page(0, Some(0));
    for (bad, expected) in [
        (
            CanonicalScalar::Int(i64::MAX),
            GraphIntegerErrorKind::Overflow,
        ),
        (
            CanonicalScalar::Bool(true),
            GraphIntegerErrorKind::NonInteger,
        ),
    ] {
        let result = run(
            &query,
            &[Some(CanonicalScalar::Int(0)), Some(bad)],
            policy(),
        );
        assert!(
            matches!(&result, Err(GqlQueryError::Source(GraphSetExecutionError::Projection { row: 1, column: 0, error }))
            if error.kind == expected)
        );
        assert!(!format!("{query:?} {result:?}").contains("private_name"));
    }
    assert!(run(&query, &[], policy()).unwrap().value.is_empty());
}

#[test]
fn exact_limits_and_every_control_boundary_include_projection_and_payloads() {
    let query = absolute(GraphSetQuantifier::Distinct);
    let values = fixture();
    let measured = run(&query, &values, policy()).unwrap();
    let exact = GqlQueryPolicy::new(
        measured.rows.snapshot_records,
        measured.rows.result_rows,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    );
    assert_eq!(run(&query, &values, exact).unwrap(), measured);
    for cap in [
        GqlQueryPolicy::new(5, 100, 1_000_000, 1_000_000),
        GqlQueryPolicy::new(6, 2, 1_000_000, 1_000_000),
        GqlQueryPolicy::new(6, 3, measured.evaluator.work_units - 1, 1_000_000),
        GqlQueryPolicy::new(6, 3, 1_000_000, measured.evaluator.scratch_entries - 1),
    ] {
        assert!(run(&query, &values, cap).is_err());
    }
    let execute = |stop: usize| {
        let calls = Cell::new(0);
        let checkpoint = || {
            let at = calls.get() + 1;
            calls.set(at);
            if at == stop { Err(stop) } else { Ok(()) }
        };
        let result: QueryResult<usize> = query.execute_governed(
            policy(),
            |pattern, remaining| {
                pattern.plan().execute_governed_with_properties(
                    6,
                    (1..=6).map(VId),
                    [],
                    |_, _| Ok::<_, ()>(true),
                    |vid, _| Ok(values[vid.0 as usize - 1].as_ref()),
                    remaining,
                    checkpoint,
                )
            },
            checkpoint,
        );
        (result, calls.get())
    };
    let (result, total) = execute(0);
    result.unwrap();
    for stop in 1..=total {
        let (result, calls) = execute(stop);
        assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
        assert_eq!(calls, stop);
    }
}
