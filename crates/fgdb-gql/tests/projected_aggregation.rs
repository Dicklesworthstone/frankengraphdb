//! Computed inputs must reach the SAME aggregate laws as real stored columns.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GqlScalarParameter,
    GraphAggregate, GraphAggregateBuildError, GraphAggregateColumn, GraphAggregateError,
    GraphAggregateFilter, GraphAggregateOrder, GraphAggregateRow, GraphAggregateTest,
    GraphIntegerBinary, GraphIntegerErrorKind, GraphIntegerExpression, GraphIntegerOp,
    GraphIntegerUnary, GraphSetProjection, GraphSetProjectionError, GraphSetValue, GraphSymbol,
    GraphSymbolKind, PreparedGraphAggregate, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, VId};
use std::cell::Cell;
use std::collections::BTreeMap;

const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const PRODUCT: PropertyKeyId = PropertyKeyId(3);
const BUCKET: PropertyKeyId = PropertyKeyId(4);
const R: RelationId = RelationId(1);
type Props = BTreeMap<(VId, PropertyKeyId), CanonicalScalar>;
type Edge = (VId, RelationId, VId);
type ResultOf<C = ()> = Result<
    GqlQueryExecution<GraphAggregateRow>,
    GqlQueryError<GraphAggregateError<&'static str>, C>,
>;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Property, "product") => Some(GraphSymbol::Property(PRODUCT)),
        (GraphSymbolKind::Property, "bucket") => Some(GraphSymbol::Property(BUCKET)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn pattern(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn input() -> PreparedGraphPattern<GraphValueRow> {
    pattern("MATCH (n) RETURN n.p AS p,n.q AS q,n")
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 5_000_000, 5_000_000)
}
fn expression(ops: &[GraphIntegerOp]) -> GraphSetValue {
    GraphSetValue::Integer(GraphIntegerExpression::prepare(ops).unwrap())
}
fn product_projection() -> Vec<GraphSetProjection> {
    vec![
        GraphSetProjection::new(
            "bucket",
            expression(&[
                GraphIntegerOp::Column(0),
                GraphIntegerOp::Unary(GraphIntegerUnary::Abs),
            ]),
        ),
        GraphSetProjection::new(
            "product",
            expression(&[
                GraphIntegerOp::Column(0),
                GraphIntegerOp::Column(1),
                GraphIntegerOp::Binary(GraphIntegerBinary::Multiply),
            ]),
        ),
    ]
}
fn summaries() -> [GraphAggregate<'static>; 9] {
    [
        GraphAggregate::count_rows("rows"),
        GraphAggregate::count("present", 1),
        GraphAggregate::count_distinct("unique", 1),
        GraphAggregate::sum_int("total", 1),
        GraphAggregate::sum_int_distinct("distinct_total", 1),
        GraphAggregate::average_int("mean", 1),
        GraphAggregate::average_int_distinct("distinct_mean", 1),
        GraphAggregate::min("minimum", 1),
        GraphAggregate::max("maximum", 1),
    ]
}
fn run(
    plan: &PreparedGraphAggregate,
    vertices: &[VId],
    edges: &[Edge],
    props: &Props,
    budget: GqlQueryPolicy,
) -> ResultOf {
    plan.execute_governed(
        (vertices.len() + edges.len()) as u64,
        vertices.iter().copied(),
        edges.iter().copied(),
        |_, _| Ok::<_, &'static str>(true),
        |vid, key| Ok(props.get(&(vid, key))),
        budget,
        || Ok::<_, ()>(()),
    )
}

#[test]
fn nullable_products_and_computed_groups_match_the_independent_stored_column_oracle() {
    let computed = PreparedGraphAggregate::prepare_projected(
        input(),
        product_projection(),
        &[0],
        &summaries(),
        0,
        None,
    )
    .unwrap();
    let plain = PreparedGraphAggregate::prepare(
        pattern("MATCH (n) RETURN n.bucket AS bucket,n.product AS product"),
        &[0],
        &summaries(),
        0,
        None,
    )
    .unwrap();
    let domain = [None, Some(-2_i64), Some(-1), Some(0), Some(2)];
    for code in 0..625_usize {
        let mut digits = code;
        let mut properties = Props::new();
        for vid in [VId(1), VId(2)] {
            let p = domain[digits % domain.len()];
            digits /= domain.len();
            let q = domain[digits % domain.len()];
            digits /= domain.len();
            properties.insert(
                (vid, P),
                p.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
            );
            properties.insert(
                (vid, Q),
                q.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
            );
            // The oracle is ordinary wide arithmetic, not the expression VM.
            let product = p
                .zip(q)
                .map(|(p, q)| i64::try_from(i128::from(p) * i128::from(q)).unwrap());
            let bucket = p.map(i64::abs);
            properties.insert(
                (vid, PRODUCT),
                product.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
            );
            properties.insert(
                (vid, BUCKET),
                bucket.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
            );
        }
        let actual = run(&computed, &[VId(1), VId(2)], &[], &properties, policy()).unwrap();
        let expected = run(&plain, &[VId(1), VId(2)], &[], &properties, policy()).unwrap();
        assert_eq!(actual.value, expected.value, "case {code}");
        assert_eq!(
            actual.rows, expected.rows,
            "admitted source and final rows, case {code}"
        );
    }
}

#[test]
fn argument_distinct_operates_on_computed_values_and_averages_remain_exact() {
    let plan = PreparedGraphAggregate::prepare_projected(
        input(),
        product_projection(),
        &[],
        &summaries(),
        0,
        None,
    )
    .unwrap();
    let mut properties = Props::new();
    for (id, p, q) in [(1, -2, -2), (2, 2, 2), (3, 2, 2), (4, 5, 1)] {
        properties.insert((VId(id), P), CanonicalScalar::Int(p));
        properties.insert((VId(id), Q), CanonicalScalar::Int(q));
    }
    let result = run(
        &plan,
        &[VId(1), VId(2), VId(3), VId(4)],
        &[],
        &properties,
        GqlQueryPolicy::new(4, 1, 1_000_000, 1_000_000),
    )
    .unwrap();
    let values = result.value[0].values();
    assert_eq!(values[0].as_count(), Some(4));
    assert_eq!(values[1].as_count(), Some(4));
    assert_eq!(values[2].as_count(), Some(2));
    assert_eq!(values[3].as_integer(), Some(17));
    assert_eq!(values[4].as_integer(), Some(9));
    let mean = values[5].as_average().unwrap();
    assert_eq!((mean.numerator(), mean.denominator()), (17, 4));
    let mean = values[6].as_average().unwrap();
    assert_eq!((mean.numerator(), mean.denominator()), (9, 2));
    assert_eq!(
        values[7].as_value().unwrap().as_scalar(),
        Some(&CanonicalScalar::Int(4))
    );
    assert_eq!(
        values[8].as_value().unwrap().as_scalar(),
        Some(&CanonicalScalar::Int(5))
    );
}

#[test]
fn constant_inputs_preserve_walk_occurrences_isolates_and_empty_group_laws() {
    let source = pattern("MATCH WALK (a)-[:R*0..2]->(b) RETURN b");
    let projection = vec![GraphSetProjection::new(
        "value",
        GraphSetValue::Literal(GqlScalarParameter::new(CanonicalScalar::Int(7)).unwrap()),
    )];
    let functions = [
        GraphAggregate::count_rows("rows"),
        GraphAggregate::sum_int("total", 0),
        GraphAggregate::count_distinct("unique", 0),
    ];
    let plan = PreparedGraphAggregate::prepare_projected(
        source.clone(),
        projection.clone(),
        &[],
        &functions,
        0,
        None,
    )
    .unwrap();
    let edges = [
        (VId(1), R, VId(1)),
        (VId(1), R, VId(1)),
        (VId(1), R, VId(2)),
    ];
    let result = run(&plan, &[VId(1), VId(2)], &edges, &Props::new(), policy()).unwrap();
    assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(11));
    assert_eq!(result.value[0].get(1).unwrap().as_integer(), Some(77));
    assert_eq!(result.value[0].get(2).unwrap().as_count(), Some(1));
    let isolated = run(&plan, &[VId(9)], &[], &Props::new(), policy()).unwrap();
    assert_eq!(isolated.value[0].get(0).unwrap().as_count(), Some(1));
    let empty = run(&plan, &[], &[], &Props::new(), policy()).unwrap();
    assert_eq!(empty.value.len(), 1);
    assert_eq!(empty.value[0].get(0).unwrap().as_count(), Some(0));
    assert!(empty.value[0].get(1).unwrap().is_null());
    let keyed =
        PreparedGraphAggregate::prepare_projected(source, projection, &[0], &functions, 0, None)
            .unwrap();
    assert!(
        run(&keyed, &[], &[], &Props::new(), policy())
            .unwrap()
            .value
            .is_empty()
    );
}

#[test]
fn projected_groups_reuse_having_hidden_keys_distinct_ordering_and_pagination() {
    let projection = vec![
        GraphSetProjection::new("group", GraphSetValue::Column(0)),
        GraphSetProjection::new(
            "value",
            expression(&[
                GraphIntegerOp::Column(1),
                GraphIntegerOp::Literal(Some(0)),
                GraphIntegerOp::Coalesce,
            ]),
        ),
    ];
    let functions = [
        GraphAggregate::count_rows("rows"),
        GraphAggregate::sum_int("total", 1),
    ];
    let base =
        PreparedGraphAggregate::prepare_projected(input(), projection, &[0], &functions, 0, None)
            .unwrap();
    let properties = BTreeMap::from([
        ((VId(1), P), CanonicalScalar::Int(1)),
        ((VId(1), Q), CanonicalScalar::Int(5)),
        ((VId(2), P), CanonicalScalar::Int(2)),
        ((VId(2), Q), CanonicalScalar::Int(5)),
    ]);
    let plan = base
        .with_result_clauses(
            &[GraphAggregateFilter {
                column: GraphAggregateColumn::Aggregate(1),
                test: GraphAggregateTest::IsNotNull,
            }],
            &[GraphAggregateOrder::descending(
                GraphAggregateColumn::Aggregate(1),
            )],
        )
        .unwrap()
        .with_key_output_columns(&[])
        .unwrap()
        .with_aggregate_output_prefix(1)
        .unwrap();
    let result = run(&plan, &[VId(1), VId(2)], &[], &properties, policy()).unwrap();
    assert_eq!(result.value.len(), 2);
    assert!(
        result
            .value
            .iter()
            .all(|row| row.keys().is_empty() && row.values().len() == 1)
    );
    let distinct = plan.with_distinct_output(true);
    assert_eq!(
        run(&distinct, &[VId(1), VId(2)], &[], &properties, policy())
            .unwrap()
            .value
            .len(),
        1
    );
}

#[test]
fn input_schema_refusals_precede_execution_and_transcripts_bind_computation() {
    let functions = [GraphAggregate::count_rows("rows")];
    for projection in [
        vec![],
        vec![GraphSetProjection::new(
            "bad name",
            GraphSetValue::Column(0),
        )],
        vec![GraphSetProjection::new(
            "v",
            GraphSetValue::Column(usize::MAX),
        )],
        vec![GraphSetProjection::new(
            "v",
            expression(&[GraphIntegerOp::Column(2)]),
        )],
        vec![
            GraphSetProjection::new("v", GraphSetValue::Column(0)),
            GraphSetProjection::new("v", GraphSetValue::Column(1)),
        ],
    ] {
        assert!(matches!(
            PreparedGraphAggregate::prepare_projected(
                input(),
                projection,
                &[],
                &functions,
                0,
                None
            ),
            Err(GraphAggregateBuildError::InputProjection(_))
        ));
    }
    let projected = vec![GraphSetProjection::new("v", GraphSetValue::Column(0))];
    assert!(matches!(
        PreparedGraphAggregate::prepare_projected(
            input(),
            projected.clone(),
            &[1],
            &functions,
            0,
            None
        ),
        Err(GraphAggregateBuildError::UnknownColumn { column: 1 })
    ));
    assert!(matches!(
        PreparedGraphAggregate::prepare_projected(
            pattern("MATCH (n) RETURN n LIMIT 1"),
            projected.clone(),
            &[],
            &functions,
            0,
            None
        ),
        Err(GraphAggregateBuildError::RequiresUnpaginatedAll)
    ));
    let ordinary = PreparedGraphAggregate::prepare(input(), &[], &functions, 0, None).unwrap();
    let computed =
        PreparedGraphAggregate::prepare_projected(input(), projected, &[], &functions, 0, None)
            .unwrap();
    assert!(ordinary.input_projection().is_none());
    assert!(computed.input_projection().is_some());
    assert_eq!(ordinary.input_pattern(), computed.input_pattern());
    assert_ne!(ordinary.canonical_bytes(), computed.canonical_bytes());
    let renamed = PreparedGraphAggregate::prepare_projected(
        input(),
        vec![GraphSetProjection::new(
            "other_alias",
            GraphSetValue::Column(0),
        )],
        &[],
        &[GraphAggregate::count_rows("renamed")],
        0,
        None,
    )
    .unwrap();
    assert_eq!(computed.canonical_bytes(), renamed.canonical_bytes());
    let changed = PreparedGraphAggregate::prepare_projected(
        input(),
        vec![GraphSetProjection::new(
            "v",
            expression(&[
                GraphIntegerOp::Column(0),
                GraphIntegerOp::Literal(Some(1)),
                GraphIntegerOp::Binary(GraphIntegerBinary::Add),
            ]),
        )],
        &[],
        &functions,
        0,
        None,
    )
    .unwrap();
    assert_ne!(computed.canonical_bytes(), changed.canonical_bytes());
    assert!(matches!(
        PreparedGraphAggregate::prepare_projected(
            input(),
            vec![GraphSetProjection::new(
                "v",
                expression(&[GraphIntegerOp::Column(2)])
            )],
            &[],
            &functions,
            0,
            None
        ),
        Err(GraphAggregateBuildError::InputProjection(
            GraphSetProjectionError::IntegerInput { .. }
        ))
    ));
}

#[test]
fn hidden_or_zero_limit_arithmetic_and_late_source_errors_cannot_be_discarded() {
    for limit in [None, Some(0)] {
        let plan = PreparedGraphAggregate::prepare_projected(
            input(),
            vec![GraphSetProjection::new(
                "value",
                expression(&[
                    GraphIntegerOp::Literal(Some(10)),
                    GraphIntegerOp::Column(0),
                    GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
                ]),
            )],
            &[],
            &[GraphAggregate::count_rows("rows")],
            0,
            limit,
        )
        .unwrap();
        let properties = BTreeMap::from([
            ((VId(1), P), CanonicalScalar::Int(1)),
            ((VId(2), P), CanonicalScalar::Int(0)),
        ]);
        assert!(
            matches!(run(&plan, &[VId(1), VId(2)], &[], &properties, policy()),
            Err(GqlQueryError::Source(GraphAggregateError::InputExpression { error, .. }))
                if error.kind == GraphIntegerErrorKind::DivisionByZero)
        );
        let result: ResultOf = plan.execute_governed(
            2,
            [VId(1), VId(2)],
            [],
            |_, _| Ok(true),
            |vid, key| {
                if vid == VId(2) {
                    Err("late property failure")
                } else {
                    Ok(properties.get(&(vid, key)))
                }
            },
            policy(),
            || Ok(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphAggregateError::Source(
                "late property failure"
            )))
        ));
    }
    let wrong = BTreeMap::from([((VId(1), P), CanonicalScalar::Bool(true))]);
    let plan = PreparedGraphAggregate::prepare_projected(
        input(),
        product_projection(),
        &[],
        &summaries(),
        0,
        None,
    )
    .unwrap();
    assert!(matches!(run(&plan, &[VId(1)], &[], &wrong, policy()),
        Err(GqlQueryError::Source(GraphAggregateError::InputExpression { error, .. })) if error.kind == GraphIntegerErrorKind::NonInteger));
}

#[test]
fn every_projected_aggregate_checkpoint_and_exact_budget_boundary_is_enforced() {
    let plan = PreparedGraphAggregate::prepare_projected(
        input(),
        product_projection(),
        &[],
        &summaries(),
        0,
        None,
    )
    .unwrap();
    let properties = BTreeMap::from([
        ((VId(1), P), CanonicalScalar::Int(2)),
        ((VId(1), Q), CanonicalScalar::Int(3)),
        ((VId(2), P), CanonicalScalar::Int(4)),
        ((VId(2), Q), CanonicalScalar::Int(5)),
    ]);
    let vertices = [VId(1), VId(2)];
    let measured = run(&plan, &vertices, &[], &properties, policy()).unwrap();
    let exact = GqlQueryPolicy::new(
        measured.rows.snapshot_records,
        measured.rows.result_rows,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    );
    assert_eq!(
        run(&plan, &vertices, &[], &properties, exact).unwrap(),
        measured
    );
    assert_eq!(
        measured.rows.result_rows, 1,
        "private MATCH rows do not consume public output allowance"
    );
    for budget in [
        GqlQueryPolicy::new(1, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(2, 0, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(2, 1, measured.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(2, 1, u64::MAX, measured.evaluator.scratch_entries - 1),
    ] {
        assert!(run(&plan, &vertices, &[], &properties, budget).is_err());
    }
    let execute = |stop| {
        let calls = Cell::new(0_usize);
        let result: ResultOf<usize> = plan.execute_governed(
            2,
            vertices,
            [],
            |_, _| Ok(true),
            |vid, key| Ok(properties.get(&(vid, key))),
            policy(),
            || {
                let next = calls.get() + 1;
                calls.set(next);
                if next == stop { Err(stop) } else { Ok(()) }
            },
        );
        (result, calls.get())
    };
    let (complete, total) = execute(0);
    assert_eq!(complete.unwrap(), measured);
    for stop in 1..=total {
        let (result, calls) = execute(stop);
        assert!(matches!(result, Err(GqlQueryError::Interrupted(found)) if found == stop));
        assert_eq!(calls, stop);
    }
}

#[test]
fn projected_literals_retain_canonical_kinds_and_do_not_leak_through_debug() {
    let secret = CanonicalScalar::ucs_basic_text("private aggregate payload").unwrap();
    let projection = vec![GraphSetProjection::new(
        "literal",
        GraphSetValue::Literal(GqlScalarParameter::new(secret.clone()).unwrap()),
    )];
    let plan = PreparedGraphAggregate::prepare_projected(
        input(),
        projection,
        &[],
        &[
            GraphAggregate::min("minimum", 0),
            GraphAggregate::max("maximum", 0),
        ],
        0,
        None,
    )
    .unwrap();
    let result = run(&plan, &[VId(1)], &[], &Props::new(), policy()).unwrap();
    assert_eq!(
        result.value[0].get(0).unwrap().as_value(),
        Some(&GraphValue::Scalar(secret))
    );
    assert!(!format!("{plan:?} {result:?}").contains("private aggregate payload"));
}
