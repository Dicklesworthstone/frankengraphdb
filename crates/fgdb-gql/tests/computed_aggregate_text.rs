//! Native aggregate text must share the checked expression and group engines.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphAggregate, GraphAggregateError, GraphAggregateRow, GraphAggregateTextSlot,
    GraphIntegerErrorKind, GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind,
    MAX_GRAPH_INTEGER_INSTRUCTIONS, PreparedGraphAggregate, PreparedGraphAggregateText,
    PreparedGraphText,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, CanonicalScalarKind, VId};
use std::cell::Cell;
use std::collections::BTreeMap;

const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
type Props = BTreeMap<(VId, PropertyKeyId), CanonicalPropertyValue>;
type Triple = (VId, RelationId, VId);
type ResultOf = Result<
    GqlQueryExecution<GraphAggregateRow>,
    GqlQueryError<GraphAggregateError<&'static str>, ()>,
>;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000, 1_000, 5_000_000, 2_000_000)
}
fn run(
    plan: &PreparedGraphAggregate,
    vertices: &[VId],
    edges: &[Triple],
    props: &Props,
) -> ResultOf {
    plan.execute_governed(
        (vertices.len() + edges.len()) as u64,
        vertices.iter().copied(),
        edges.iter().copied(),
        |_, _| Ok(true),
        |vid, key| Ok(props.get(&(vid, key))),
        policy(),
        || Ok(()),
    )
}
fn properties() -> Props {
    let mut props = Props::new();
    for (id, p, q) in [(1, -2, -2), (2, 2, 2), (3, 2, 3), (4, 5, 1)] {
        props.insert((VId(id), P), CanonicalScalar::Int(p).into());
        props.insert((VId(id), Q), CanonicalScalar::Int(q).into());
    }
    props.insert((VId(5), Q), CanonicalScalar::Null.into());
    props
}

fn run_collection_relation(
    plan: &PreparedGraphAggregate,
    vertices: &[VId],
    edges: &[Triple],
    props: &Props,
) -> Vec<GraphAggregateRow> {
    let normalized = plan.incremental_collection_relation().unwrap();
    assert_eq!(normalized.key_columns(), plan.key_columns());
    assert_eq!(normalized.aggregate_columns(), plan.aggregate_columns());
    assert_eq!(normalized.input().operand_count(), 1);
    assert_eq!(
        normalized.input().incremental_result_order(),
        Some((&[][..], None))
    );
    normalized
        .execute_governed(
            policy(),
            |source, policy| {
                source.plan().execute_governed_with_properties(
                    (vertices.len() + edges.len()) as u64,
                    vertices.iter().copied(),
                    edges.iter().copied(),
                    |_, _| Ok::<_, &'static str>(true),
                    |id, key| Ok(props.get(&(id, key))),
                    policy,
                    || Ok::<_, ()>(()),
                )
            },
            || Ok::<_, ()>(()),
        )
        .unwrap()
        .value
}

#[test]
fn maintained_collection_normalization_preserves_native_and_computed_source_order() {
    use fgdb_gql::algebra::GraphValue;

    let vertices = [VId(1), VId(2), VId(3), VId(4)];
    let mut props = Props::new();
    for (id, p, q) in [(1, 7, 1), (2, 2, 1), (3, 7, 2), (4, -4, 2)] {
        props.insert((VId(id), P), CanonicalScalar::Int(p).into());
        props.insert((VId(id), Q), CanonicalScalar::Int(q).into());
    }
    for (text, expected) in [
        ("MATCH (n) RETURN collect(n.p) AS values", vec![7, 2, 7, -4]),
        (
            "MATCH (n) RETURN collect(DISTINCT n.p) AS values",
            vec![7, 2, -4],
        ),
        (
            "MATCH (n) RETURN collect(-n.p) AS values",
            vec![4, -2, -7, -7],
        ),
        (
            "MATCH (n) RETURN collect(n.p+n.q) AS values",
            vec![-2, 3, 8, 9],
        ),
        (
            "MATCH (n) RETURN [x IN collect(n.p) | x*2] AS values",
            vec![14, 4, 14, -8],
        ),
    ] {
        let plan = prepare(text);
        let original = plan.canonical_bytes();
        let actual = run_collection_relation(&plan, &vertices, &[], &props);
        assert_eq!(
            actual,
            run(&plan, &vertices, &[], &props).unwrap().value,
            "{text}"
        );
        assert_eq!(actual.len(), 1);
        assert_eq!(
            actual[0].values()[0].as_value(),
            Some(&GraphValue::List(
                expected
                    .into_iter()
                    .map(|value| GraphValue::Scalar(CanonicalScalar::Int(value)))
                    .collect()
            )),
            "{text}"
        );
        assert_eq!(plan.canonical_bytes(), original);
    }
    props.insert((VId(u128::MAX), P), CanonicalScalar::Null.into());
    props.insert((VId(u128::MAX), Q), CanonicalScalar::Int(3).into());
    let vertices = [VId(1), VId(2), VId(3), VId(4), VId(u128::MAX)];
    for text in [
        "MATCH (n) RETURN collect(n) AS nodes,collect(n.p) AS values,COUNT(*) AS count,SUM(n.p) AS total,AVG(n.p) AS average",
        "MATCH (n) RETURN n.q AS bucket,collect(DISTINCT n.p) AS values GROUP BY n.q ORDER BY n.q DESC",
        "MATCH (n) RETURN DISTINCT collect(n.p) AS values GROUP BY n.q HAVING COUNT(n.p)>0 ORDER BY SUM(n.p) DESC LIMIT 1",
        "MATCH (n) RETURN n.q AS bucket,[x IN collect(n.p) | x+n.q] AS values GROUP BY n.q ORDER BY n.q",
        "MATCH (n) RETURN reduce(total=0,x IN collect(n.p) | total+x) AS total,any(x IN collect(n.p) WHERE x<0) AS negative",
        "MATCH (n) RETURN collect([n.p,n.q]) AS values,COUNT(*) AS count",
        "MATCH (n) RETURN collect(1) AS `_fgdb_collect_order_0`,collect(n.p) AS `_fgdb_collect_order_1`",
        "MATCH (n) RETURN collect(n.p) AS values LIMIT 0",
    ] {
        let plan = prepare(text);
        for vertices in [&vertices[..], &[][..]] {
            assert_eq!(
                run_collection_relation(&plan, vertices, &[], &props),
                run(&plan, vertices, &[], &props).unwrap().value,
                "{text}"
            );
        }
    }
}

#[test]
fn maintained_collection_source_admission_keeps_topology_and_complete_schema_bounds() {
    use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, MAX_PATTERN_VERTICES};

    for text in [
        "MATCH (n) RETURN COUNT(*) AS count",
        "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN collect(c.p) AS values",
        "MATCH (a)-[r:R]->(b) RETURN collect(r.p) AS values",
        "MATCH (a),(b) RETURN collect(a.p) AS values",
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) RETURN collect(b.p) AS values",
    ] {
        assert!(
            prepare(text).incremental_collection_relation().is_none(),
            "{text}"
        );
    }
    let mut graph = GraphPatternBuilder::new();
    graph.vertex("n").unwrap();
    let names: Vec<_> = (0..MAX_PATTERN_VERTICES - 1)
        .map(|column| format!("c{column}"))
        .collect();
    let mut columns: Vec<_> = names
        .iter()
        .map(|name| GraphColumn::property(name, "n", P))
        .collect();
    let prepare_columns = |columns: &[GraphColumn<'_>]| {
        PreparedGraphAggregate::prepare(
            graph
                .prepare_values(columns, 0, None)
                .unwrap()
                .with_duplicates(),
            &[],
            &[GraphAggregate::collect("values", 0)],
            0,
            None,
        )
        .unwrap()
    };
    // The hidden identity cannot take the tuple producer's private COUNT cell.
    assert!(
        prepare_columns(&columns)
            .incremental_collection_relation()
            .is_none()
    );
    // An already projected identity reuses its source column at the same bound.
    columns[1] = GraphColumn::vertex(&names[1], "n");
    assert!(
        prepare_columns(&columns)
            .incremental_collection_relation()
            .is_some()
    );

    let mut edge = GraphPatternBuilder::new();
    edge.vertex("a").unwrap();
    edge.vertex("b").unwrap();
    edge.edge("a", R, fgdb_gql::algebra::GlaDirection::Forward, "b")
        .unwrap();
    let mut columns: Vec<_> = names
        .iter()
        .map(|name| GraphColumn::property(name, "b", P))
        .collect();
    let prepare_edge = |columns: &[GraphColumn<'_>]| {
        PreparedGraphAggregate::prepare(
            edge.prepare_values(columns, 0, None)
                .unwrap()
                .with_duplicates(),
            &[],
            &[GraphAggregate::collect("values", 0)],
            0,
            None,
        )
        .unwrap()
    };
    assert!(
        prepare_edge(&columns)
            .incremental_collection_relation()
            .is_none()
    );
    assert!(
        prepare_edge(&columns[..columns.len() - 2])
            .incremental_collection_relation()
            .is_some()
    );
    columns[0] = GraphColumn::vertex(&names[0], "a");
    assert!(
        prepare_edge(&columns)
            .incremental_collection_relation()
            .is_none()
    );
    columns[1] = GraphColumn::vertex(&names[1], "b");
    assert!(
        prepare_edge(&columns)
            .incremental_collection_relation()
            .is_some()
    );
}

#[test]
fn maintained_edge_collections_preserve_endpoint_and_computed_row_order() {
    use fgdb_gql::algebra::GraphValue;

    let vertices = [VId(1), VId(2), VId(3), VId(4)];
    let mut props = Props::new();
    for (id, p, q) in [(1, 7, 1), (2, -4, 1), (3, 2, 2), (4, 11, 2)] {
        props.insert((VId(id), P), CanonicalScalar::Int(p).into());
        props.insert((VId(id), Q), CanonicalScalar::Int(q).into());
    }
    // Shuffled input and parallel edges must not become the collection order.
    let edges = [
        (VId(3), R, VId(3)),
        (VId(1), R, VId(4)),
        (VId(1), R, VId(2)),
        (VId(3), R, VId(1)),
        (VId(2), R, VId(3)),
        (VId(1), R, VId(2)),
    ];
    for (text, expected) in [
        (
            "MATCH (a)-[:R]->(b) RETURN collect(b.p) AS values",
            vec![-4, -4, 11, 2, 7, 2],
        ),
        (
            "MATCH (a)-[:R]->(b) RETURN collect(DISTINCT b.p) AS values",
            vec![-4, 11, 2, 7],
        ),
        (
            "MATCH (a)<-[:R]-(b) RETURN collect(b.p) AS values",
            vec![2, 7, 7, -4, 2, 7],
        ),
        (
            "MATCH (a)-[:R]-(b) RETURN collect(b.p) AS values",
            vec![-4, -4, 2, 11, 7, 7, 2, 7, -4, 2, 7],
        ),
        (
            "MATCH (a)-[:R]->(b) RETURN collect(-b.p) AS values",
            vec![4, 4, -2, -2, -7, -11],
        ),
        (
            "MATCH (a)-[:R]->(b) RETURN [x IN collect(b.p) | x*2] AS values",
            vec![-8, -8, 22, 4, 14, 4],
        ),
    ] {
        let plan = prepare(text);
        let bytes = plan.canonical_bytes();
        let actual = run_collection_relation(&plan, &vertices, &edges, &props);
        assert_eq!(
            actual,
            run(&plan, &vertices, &edges, &props).unwrap().value,
            "{text}"
        );
        assert_eq!(actual.len(), 1);
        assert_eq!(
            actual[0].values()[0].as_value(),
            Some(&GraphValue::List(
                expected
                    .into_iter()
                    .map(|value| GraphValue::Scalar(CanonicalScalar::Int(value)))
                    .collect()
            )),
            "{text}"
        );
        assert_eq!(plan.canonical_bytes(), bytes);
    }
    for text in [
        "MATCH (a)-[:R]->(b) RETURN collect(a) AS sources,collect(b) AS destinations,collect(b.p) AS values,COUNT(*) AS count,SUM(b.p) AS total,AVG(b.p) AS average",
        "MATCH (a)-[:R]->(b) RETURN a.q AS bucket,collect(b.p) AS values GROUP BY a.q ORDER BY a.q DESC",
        "MATCH (a)-[:R]->(b) WHERE a<>b AND (a.q=b.q OR b.p>0) RETURN collect(b.p) AS values",
        "MATCH (a)-[:R]->(b) RETURN DISTINCT collect(b.p) AS values GROUP BY a.q HAVING COUNT(b.p)>0 ORDER BY SUM(b.p) DESC LIMIT 1",
        "MATCH (a)-[:R]->(b) RETURN collect([a.p,b.p]) AS pairs,collect(a.p+b.p) AS values",
        "MATCH (a)-[:R]->(b) RETURN collect(1) AS `_fgdb_collect_order_0`,collect(b.p) AS `_fgdb_collect_order_1`",
        "MATCH (a)-[:R]->(b) RETURN collect(b.p) AS values LIMIT 0",
    ] {
        let plan = prepare(text);
        for edges in [&edges[..], &[][..]] {
            assert_eq!(
                run_collection_relation(&plan, &vertices, edges, &props),
                run(&plan, &vertices, edges, &props).unwrap().value,
                "{text}"
            );
        }
    }
}

#[test]
fn aggregate_collections_bind_nested_element_scopes_and_preserve_occurrences() {
    use fgdb_gql::GraphAggregateValue;
    use fgdb_gql::algebra::GraphValue;

    let integer = |value| GraphValue::Scalar(CanonicalScalar::Int(value));
    let list = |values: &[i64]| GraphValue::List(values.iter().copied().map(integer).collect());
    let vertices = [VId(1), VId(2), VId(3), VId(4)];
    let props = properties();
    for (text, expected) in [
        (
            "MATCH (n) RETURN [x IN collect(n.p) WHERE x > 0 | x * 2] AS value",
            list(&[4, 4, 10]),
        ),
        (
            "MATCH (n) RETURN [n IN collect(n.p) | n * 2] AS value",
            list(&[-4, 4, 4, 10]),
        ),
        (
            "MATCH (n) RETURN [x IN collect(n.p) | x + count(*)] AS value",
            list(&[2, 6, 6, 9]),
        ),
        (
            "MATCH (n) RETURN [x IN collect(n.p) | x + count(n)] AS value",
            list(&[2, 6, 6, 9]),
        ),
        (
            "MATCH (n) RETURN [x IN collect(n.p) | size(collect([y IN [1, 2] | y]))] AS value",
            list(&[4, 4, 4, 4]),
        ),
        (
            "MATCH (n) RETURN [x IN collect(DISTINCT n.p) | x] AS value",
            list(&[-2, 2, 5]),
        ),
        (
            "MATCH (n) RETURN [x IN collect(n.p) | [y IN [10, 20] | x + y]] AS value",
            GraphValue::List(
                [
                    list(&[8, 18]),
                    list(&[12, 22]),
                    list(&[12, 22]),
                    list(&[15, 25]),
                ]
                .into(),
            ),
        ),
        (
            "MATCH (n) RETURN [x IN collect(n.p) | reduce(total=x, y IN [1, 2] | total + y)] AS value",
            list(&[1, 5, 5, 8]),
        ),
        (
            "MATCH (n) RETURN reduce(total=0, x IN collect(n.p) | total * 10 + x) AS value",
            integer(-1775),
        ),
        (
            "MATCH (n) RETURN [x IN collect(n) | x] AS value",
            GraphValue::List(vertices.iter().copied().map(GraphValue::Vertex).collect()),
        ),
        (
            "MATCH (n) RETURN [x IN collect(n.p) WHERE null | 1/0] AS value",
            list(&[]),
        ),
    ] {
        let rows = run(&prepare(text), &vertices, &[], &props).unwrap().value;
        assert_eq!(rows.len(), 1, "{text}");
        assert_eq!(
            rows[0].values(),
            &[GraphAggregateValue::Value(expected)],
            "{text}"
        );
    }
    // A scoped expression can read the enclosing group's key; the local
    // never becomes a new key and duplicate input values keep their weight.
    let text = "MATCH (n) RETURN n.p AS p, [x IN collect(n.q) | x + n.p] AS values GROUP BY n.p ORDER BY n.p";
    let rows = run(&prepare(text), &vertices, &[], &props).unwrap().value;
    for (row, (key, values)) in rows
        .iter()
        .zip([(-2, vec![-4]), (2, vec![4, 5]), (5, vec![6])])
    {
        assert_eq!(
            row.values(),
            &[
                GraphAggregateValue::Value(integer(key)),
                GraphAggregateValue::Value(list(&values)),
            ]
        );
    }
    assert_eq!(rows.len(), 3);

    let text = "MATCH (n) RETURN n AS node, [x IN collect(n.p) | id(n) + x] AS values GROUP BY n ORDER BY n";
    let rows = run(&prepare(text), &vertices, &[], &props).unwrap().value;
    for (row, (vertex, expected)) in rows.iter().zip(vertices.into_iter().zip([-1, 4, 5, 9])) {
        assert_eq!(
            row.values(),
            &[
                GraphAggregateValue::Value(GraphValue::Vertex(vertex)),
                GraphAggregateValue::Value(list(&[expected])),
            ]
        );
    }
    assert_eq!(rows.len(), 4);
}

#[test]
fn aggregate_arguments_cannot_depend_on_output_element_scopes() {
    for text in [
        "MATCH (n) RETURN [x IN collect(n.p) | sum(x+1)] AS values",
        "MATCH (n) RETURN [x IN collect(n.p) | collect([x])] AS values",
        "MATCH (n) RETURN [x IN collect(n.p) | collect([y IN [1, 2] | x+y])] AS values",
        "MATCH (n) RETURN reduce(total=0, x IN collect(n.p) | sum(total+1)) AS value",
    ] {
        assert!(
            PreparedGraphAggregateText::prepare(text, symbols)
                .and_then(|template| template.bind_parameters(&GqlParameters::new()))
                .is_err(),
            "{text}"
        );
    }
}

#[test]
fn aggregate_list_predicates_keep_three_valued_empty_and_null_laws() {
    use fgdb_gql::GraphAggregateValue;
    use fgdb_gql::algebra::GraphValue;

    let vertices = [VId(1), VId(2), VId(3), VId(4)];
    let props = properties();
    for (expression, nonempty, empty) in [
        (
            "any(x IN collect(n.p) WHERE x > 4)",
            Some(true),
            Some(false),
        ),
        (
            "all(x IN collect(n.p) WHERE x > 0)",
            Some(false),
            Some(true),
        ),
        (
            "none(x IN collect(n.p) WHERE x = 3)",
            Some(true),
            Some(true),
        ),
        (
            "single(x IN collect(n.p) WHERE x = 2)",
            Some(false),
            Some(false),
        ),
        (
            "single(x IN collect(n.p) WHERE x = 5)",
            Some(true),
            Some(false),
        ),
        ("any(x IN [count(*), null] WHERE x > 0)", Some(true), None),
        ("all(x IN [count(*), null] WHERE x > 0)", None, Some(false)),
        ("none(x IN [count(*), null] WHERE x > 0)", Some(false), None),
        ("single(x IN [count(*), null] WHERE x > 0)", None, None),
    ] {
        let text = format!("MATCH (n) RETURN {expression} AS value");
        let plan = prepare(&text);
        for (vertices, truth) in [(&vertices[..], nonempty), (&[][..], empty)] {
            let rows = run(&plan, vertices, &[], &props).unwrap().value;
            assert_eq!(rows.len(), 1, "{text}");
            assert_eq!(
                rows[0].values(),
                &[GraphAggregateValue::Value(GraphValue::Scalar(
                    truth.map_or(CanonicalScalar::Null, CanonicalScalar::Bool),
                ))],
                "{text}"
            );
        }
    }
    for expression in [
        "[x IN null | count(*)]",
        "reduce(total=count(*), x IN null | total + x)",
        "any(x IN null WHERE x = count(*))",
    ] {
        let text = format!("MATCH (n) RETURN {expression} AS value");
        let rows = run(&prepare(&text), &vertices, &[], &props).unwrap().value;
        assert_eq!(
            rows[0].values(),
            &[GraphAggregateValue::Value(GraphValue::Scalar(
                CanonicalScalar::Null
            ))],
            "{text}"
        );
    }
    let plan = prepare(
        "MATCH (n) RETURN [x IN collect(n.p) | x] AS list, reduce(total=count(*), x IN collect(n.p) | total+x) AS total",
    );
    let rows = run(&plan, &[], &[], &props).unwrap().value;
    assert_eq!(
        rows[0].values(),
        &[
            GraphAggregateValue::Value(GraphValue::List(Box::new([]))),
            GraphAggregateValue::Count(0),
        ]
    );
}

#[test]
fn aggregate_reduction_keeps_wide_sums_and_exact_average_accumulators() {
    use fgdb_gql::{GraphAggregateValue, GraphExactAverage};

    let vertices = [VId(1), VId(2)];
    let props: Props = BTreeMap::from([
        ((VId(1), P), CanonicalScalar::Int(i64::MAX).into()),
        ((VId(2), P), CanonicalScalar::Int(i64::MAX).into()),
        ((VId(1), Q), CanonicalScalar::Int(1).into()),
        ((VId(2), Q), CanonicalScalar::Int(2).into()),
    ]);
    for (text, expected) in [
        (
            "MATCH (n) RETURN reduce(total=sum(n.p), x IN collect(n.q) | total + x) AS value",
            GraphAggregateValue::Integer(i128::from(i64::MAX) * 2 + 3),
        ),
        (
            "MATCH (n) RETURN reduce(total=sum(n.p), x IN [] | total + x) AS value",
            GraphAggregateValue::Integer(i128::from(i64::MAX) * 2),
        ),
        (
            "MATCH (n) RETURN reduce(total=avg(n.q), x IN collect(n.p) | total) AS value",
            GraphAggregateValue::Average(GraphExactAverage::new(3, 2).unwrap()),
        ),
        (
            "MATCH (n) RETURN reduce(total=avg(n.q), x IN collect(n.p) | COALESCE(total, x)) AS value",
            GraphAggregateValue::Average(GraphExactAverage::new(3, 2).unwrap()),
        ),
    ] {
        let rows = run(&prepare(text), &vertices, &[], &props).unwrap().value;
        assert_eq!(rows[0].values(), &[expected], "{text}");
    }
    // A maintainer can hold a count larger than any scalar integer. A local
    // accumulator must retain that domain rather than truncate the input row.
    let plan = prepare("MATCH (n) RETURN reduce(total=count(*), x IN [1, 2] | total-x) AS value");
    let complete = GraphAggregateRow::from_group_values(
        Vec::new(),
        vec![GraphAggregateValue::Count(u64::MAX)],
    );
    let projected = plan
        .project_incremental_output(&complete, &mut |_| Ok::<_, ()>(()))
        .unwrap()
        .expect("a complete native count group");
    assert_eq!(
        projected.values(),
        &[GraphAggregateValue::Integer(i128::from(u64::MAX) - 3)]
    );
}

#[test]
fn batch_and_maintained_aggregate_collections_share_scopes_and_late_failures() {
    use fgdb_gql::GlaExecutionEvent;

    let props = properties();
    for text in [
        "MATCH (n) RETURN [x IN collect(n.p) WHERE x > 0 | x * 2] AS value",
        "MATCH (n) RETURN all(x IN collect(n.p) WHERE x > 0) AS value",
        "MATCH (n) RETURN reduce(total=0, x IN collect(n.p) | total * 10 + x) AS value",
    ] {
        let plan = prepare(text);
        let source = plan.incremental_source_definition().expect(text);
        for vertices in [
            &[VId(1), VId(2), VId(3), VId(4)][..],
            &[VId(1), VId(4)][..],
            &[][..],
        ] {
            let expected = run(&plan, vertices, &[], &props).unwrap().value;
            let complete = run(&source, vertices, &[], &props).unwrap().value;
            let projected: Vec<_> = complete
                .iter()
                .map(|row| {
                    plan.project_incremental_output(row, &mut |_| Ok::<_, ()>(()))
                        .unwrap()
                        .expect("same completed group schema")
                })
                .collect();
            assert_eq!(projected, expected, "{text}: {vertices:?}");
        }
        let complete = run(&source, &[VId(1), VId(2), VId(3), VId(4)], &[], &props)
            .unwrap()
            .value;
        let mut work = 0;
        let refused = plan.project_incremental_output(&complete[0], &mut |event| {
            if event == GlaExecutionEvent::Work {
                work += 1;
            }
            if work >= 16 {
                Err("cancel during output")
            } else {
                Ok(())
            }
        });
        assert!(
            matches!(
                refused,
                Err(GqlQueryError::Interrupted("cancel during output"))
            ),
            "{text}: {refused:?}"
        );
        assert!(
            plan.project_incremental_output(&complete[0], &mut |_| Ok::<_, ()>(()))
                .unwrap()
                .is_some()
        );
    }
    for (expression, kind) in [
        (
            "[x IN collect(n.p) | 10/(x-2)]",
            GraphIntegerErrorKind::DivisionByZero,
        ),
        (
            "any(x IN collect(n.p) WHERE x < 0 OR 10/(x-2)>0)",
            GraphIntegerErrorKind::DivisionByZero,
        ),
        (
            "[x IN collect(n.p) WHERE x | x]",
            GraphIntegerErrorKind::NonBoolean,
        ),
        (
            "reduce(total=count(*)/0, x IN null | total)",
            GraphIntegerErrorKind::DivisionByZero,
        ),
    ] {
        let text = format!("MATCH (n) RETURN {expression} AS value LIMIT 0");
        let plan = prepare(&text);
        assert!(
            matches!(run(&plan, &[VId(1), VId(2)], &[], &props),
            Err(GqlQueryError::Source(GraphAggregateError::OutputExpression {error, ..})) if error.kind == kind),
            "{text}"
        );
    }
    let plan = prepare("MATCH (n) RETURN [x IN collect(n.p) | range(0,100)] AS value LIMIT 0");
    let complete = run(&plan, &[VId(1), VId(2)], &[], &props).unwrap();
    let mut bounded = policy();
    bounded.evaluator.max_work_units = complete.evaluator.work_units - 1;
    let refused: ResultOf = plan.execute_governed(
        2,
        [VId(1), VId(2)],
        [],
        |_, _| Ok(true),
        |vid, key| Ok(props.get(&(vid, key))),
        bounded,
        || Ok(()),
    );
    assert!(matches!(refused, Err(GqlQueryError::Evaluator(limit))
        if limit.dimension == fgdb_gql::GlaLimitDimension::WorkUnits));
}

#[test]
fn aggregates_inside_nested_maps_lists_and_scalar_outputs_share_the_native_group_engine() {
    use fgdb_gql::GraphAggregateValue;
    use fgdb_gql::algebra::GraphValue;

    let integer = |value| GraphValue::Scalar(CanonicalScalar::Int(value));
    let map = |entries: Vec<(&str, GraphValue)>| {
        let (keys, values): (Vec<Box<str>>, Vec<_>) = entries
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .unzip();
        GraphValue::Map {
            keys: keys.into_boxed_slice(),
            values: values.into_boxed_slice(),
        }
    };
    let plan = prepare(
        "MATCH (n) RETURN {bucket:n.p,nested:{list:[COUNT(*),SUM(n.q)*2],sample:range(0,COUNT(*)-1)}} AS result GROUP BY n.p ORDER BY n.p",
    );
    let rows = run(&plan, &[VId(1), VId(2), VId(3), VId(4)], &[], &properties())
        .unwrap()
        .value;
    let expected: Vec<_> = [(-2, 1, -4), (2, 2, 10), (5, 1, 2)]
        .into_iter()
        .map(|(bucket, count, sum)| {
            GraphAggregateValue::Value(map(vec![
                ("bucket", integer(bucket)),
                (
                    "nested",
                    map(vec![
                        (
                            "list",
                            GraphValue::List(vec![integer(count), integer(sum)].into_boxed_slice()),
                        ),
                        (
                            "sample",
                            GraphValue::List(
                                (0..count)
                                    .map(integer)
                                    .collect::<Vec<_>>()
                                    .into_boxed_slice(),
                            ),
                        ),
                    ]),
                ),
            ]))
        })
        .collect();
    assert_eq!(rows.len(), expected.len());
    for (row, expected) in rows.iter().zip(expected) {
        assert!(row.keys().is_empty());
        assert_eq!(row.values(), &[expected]);
    }
    // An aggregate after a map's comma must select aggregate lowering even
    // when keywords, comments and quoted commas occur before the call.
    let template = PreparedGraphAggregateText::prepare(
        "MATCH (n) RETURN {`AS`:'x,y',`GROUP`:{label:'COUNT(*)',total:/*x*/ SUM(n.q)}} AS result",
        symbols,
    )
    .unwrap();
    assert_eq!(template.columns(), &["result"]);
    let bound = template.bind_parameters(&GqlParameters::new()).unwrap();
    let rows = run(&bound, &[VId(2)], &[], &properties()).unwrap().value;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].values(),
        &[GraphAggregateValue::Value(map(vec![
            (
                "AS",
                GraphValue::Scalar(CanonicalScalar::ucs_basic_text("x,y").unwrap())
            ),
            (
                "GROUP",
                map(vec![
                    (
                        "label",
                        GraphValue::Scalar(CanonicalScalar::ucs_basic_text("COUNT(*)").unwrap())
                    ),
                    ("total", integer(2)),
                ])
            ),
        ]))]
    );
}

#[test]
fn reserved_delimited_map_keys_stay_opaque_to_functions_and_composition() {
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::{GraphAggregateValue, PreparedGraphSetText};

    let vertices = [VId(1), VId(2), VId(3), VId(4)];
    let props = properties();
    let integer = |value| GraphValue::Scalar(CanonicalScalar::Int(value));
    let plan =
        prepare("MATCH (n) RETURN {`COUNT`:COUNT(*),`SUM`:SUM(n.p),`UNION`:'kept'} AS value");
    let rows = run(&plan, &vertices, &[], &props).unwrap().value;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].values(),
        &[GraphAggregateValue::Value(
            GraphValue::map(vec![
                ("COUNT".into(), integer(4)),
                ("SUM".into(), integer(7)),
                (
                    "UNION".into(),
                    GraphValue::Scalar(CanonicalScalar::ucs_basic_text("kept").unwrap())
                ),
            ])
            .unwrap()
        )]
    );
    let plan = prepare("MATCH (n) RETURN {`AS`:COUNT(*)}.`AS` AS value");
    let rows = run(&plan, &vertices, &[], &props).unwrap().value;
    assert_eq!(rows[0].values(), &[GraphAggregateValue::Value(integer(4))]);

    // The shared composition tokenizer must keep keys opaque in both a map
    // projection and a literal, including a key equal to its set operator.
    let text = "MATCH (n) RETURN n{`AS`:n.p,`GROUP`:n.q} AS value UNION ALL MATCH (n) RETURN {`UNION`:n.p,`RETURN`:n.q} AS value";
    let prepared = PreparedGraphSetText::prepare(text, symbols).unwrap();
    assert_eq!(prepared.columns(), &["value"]);
    assert!(prepared.bind_parameters(&GqlParameters::new()).is_ok());

    let calls = Cell::new(0);
    assert!(
        PreparedGraphAggregateText::prepare(
            "MATCH (n) RETURN {`COUNT`:COUNT(*),`COUNT`:SUM(n.p)} AS value",
            |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            },
        )
        .is_err()
    );
    assert_eq!(
        calls.get(),
        0,
        "duplicate keys refuse before catalog resolution"
    );
}

/// fgdb-y255t, option C: in the aggregate result projection, arithmetic on
/// an exact average reads the binary64 nearest it, rounded once, and so
/// does a list that holds it. The bare cell stays the exact average and a
/// comparison stays exact: 7/3 is below 2.3333333333333335, its nearest
/// binary64. IEEE operations on exactly representable values are the
/// independent expectations.
#[test]
fn result_arithmetic_reads_an_exact_average_as_its_nearest_binary64() {
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::{GraphAggregateValue, GraphExactAverage};

    let vertices = [VId(1), VId(2), VId(3)];
    let mut props = Props::new();
    for (id, p) in [(1, 1), (2, 3), (3, 3)] {
        props.insert((VId(id), P), CanonicalScalar::Int(p).into());
    }
    let mean = 7.0 / 3.0;
    let binary64 = |value: f64| {
        GraphValue::Scalar(CanonicalScalar::Float(fgdb_types::CanonicalF64::new(value)))
    };
    let float = |value: f64| GraphAggregateValue::Value(binary64(value));
    let age = GqlParameters::new().with_int64("age", 12).unwrap();
    for (text, expected) in [
        (
            "MATCH (n) RETURN avg(n.p) AS value",
            GraphAggregateValue::Average(GraphExactAverage::new(7, 3).unwrap()),
        ),
        ("MATCH (n) RETURN avg(n.p) * 2 AS value", float(mean * 2.0)),
        (
            "MATCH (n) RETURN avg(n.p) + 0.5 AS value",
            float(mean + 0.5),
        ),
        ("MATCH (n) RETURN -avg(n.p) AS value", float(-mean)),
        // TCK With6 [5]'s expression.
        (
            "MATCH (n) RETURN $age + avg(n.p) - 1000 AS value",
            float(12.0 + mean - 1000.0),
        ),
        (
            "MATCH (n) RETURN [avg(n.p)] AS value",
            GraphAggregateValue::Value(GraphValue::List(vec![binary64(mean)].into_boxed_slice())),
        ),
        (
            "MATCH (n) RETURN avg(n.p) < 2.3333333333333335 AS value",
            GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Bool(true))),
        ),
    ] {
        let parameters = if text.contains("$age") {
            age.clone()
        } else {
            GqlParameters::new()
        };
        let plan = PreparedGraphAggregateText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&parameters)
            .unwrap();
        let rows = run(&plan, &vertices, &[], &props).unwrap().value;
        assert_eq!(rows[0].values(), &[expected], "{text}");
    }
}

#[test]
fn floating_aggregate_having_and_case_compare_exact_counts_sums_and_averages() {
    use fgdb_gql::GraphAggregateValue;

    let vertices = [VId(1), VId(2), VId(3), VId(4)];
    let props = properties();
    // Both the compact conjunction and the Boolean HAVING program must use
    // the same numeric domain. The inputs sum to 7 and average to 1.75;
    // adding a quarter per input gives floating SUM=8 and AVG=2.
    for predicate in [
        "SUM(n.p+0.25)>0",
        "(SUM(n.p+0.25)>0 AND AVG(n.p+0.25)=2)",
        "SUM(n.p+0.25)>AVG(n.p)",
        "AVG(n.p)<AVG(n.p+0.25)",
        "COUNT(*)=4.0",
        "SUM(n.p)=7.0",
        "AVG(n.p)=1.75",
        "4.0=COUNT(*)",
        "-0.25<AVG(n.p)",
        "COUNT(*)=4e0",
        "AVG(n.p) IN [0.0,1.75,NULL]",
        "AVG(n.p) BETWEEN 1.5 AND 2e0",
        "COUNT(*) NOT IN [0.0,3.0]",
        "AVG(n.p) NOT BETWEEN -1.0 AND 1.5",
    ] {
        let text = format!("MATCH (n) RETURN COUNT(*) AS count HAVING {predicate}");
        let rows = run(&prepare(&text), &vertices, &[], &props).unwrap().value;
        assert_eq!(rows.len(), 1, "{text}");
        assert_eq!(rows[0].values(), &[GraphAggregateValue::Count(4)], "{text}");
    }
    for predicate in [
        "COUNT(*)<4.0",
        "SUM(n.p)>7.0",
        "AVG(n.p)>1.75",
        "4e0<COUNT(*)",
        "AVG(n.p) IN [1.5,2.0]",
        "AVG(n.p) BETWEEN -2.0 AND -0.5",
        "COUNT(*) NOT IN [4.0,NULL]",
    ] {
        let text = format!("MATCH (n) RETURN COUNT(*) AS count HAVING {predicate}");
        assert!(
            run(&prepare(&text), &vertices, &[], &props)
                .unwrap()
                .value
                .is_empty(),
            "{text}"
        );
    }
    let plan = prepare(
        "MATCH (n) RETURN CASE WHEN AVG(n.p)=1.75 AND COUNT(*)=4.0 AND 8.0>SUM(n.p) THEN SUM(n.p)*2 ELSE 0 END AS value",
    );
    let rows = run(&plan, &vertices, &[], &props).unwrap().value;
    assert_eq!(rows[0].values(), &[GraphAggregateValue::Integer(14)]);

    for predicate in ["COUNT(*)<1e309", "-1e309<COUNT(*)"] {
        let text = format!("MATCH (n) RETURN COUNT(*) AS count HAVING {predicate}");
        let calls = Cell::new(0);
        let error = PreparedGraphAggregateText::prepare(&text, |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        })
        .unwrap_err();
        assert_eq!(
            error.kind,
            GraphPatternTextErrorKind::ScalarLiteral,
            "{text}"
        );
        assert_eq!(
            calls.get(),
            0,
            "non-finite literals refuse before catalog resolution"
        );
    }
}

#[test]
fn computed_group_keys_products_distinct_and_exact_averages_have_native_syntax() {
    let text = "MATCH (n) RETURN ABS(n.p) AS bucket,SUM(n.p*n.q) AS total, \
        AVG(COALESCE(n.q,0)) AS mean,COUNT(DISTINCT ABS(n.p)) AS unique \
        GROUP BY ABS(n.p) ORDER BY bucket";
    let template = PreparedGraphAggregateText::prepare(text, symbols).unwrap();
    assert_eq!(template.columns(), &["bucket", "total", "mean", "unique"]);
    assert_eq!(
        template.output_slots(),
        &[
            GraphAggregateTextSlot::GroupKey(0),
            GraphAggregateTextSlot::Aggregate(0),
            GraphAggregateTextSlot::Aggregate(1),
            GraphAggregateTextSlot::Aggregate(2)
        ]
    );
    let plan = template.bind_parameters(&GqlParameters::new()).unwrap();
    let result = run(
        &plan,
        &[VId(1), VId(2), VId(3), VId(4), VId(5)],
        &[],
        &properties(),
    )
    .unwrap();
    assert_eq!(result.value.len(), 3);
    let rows = &result.value;
    assert_eq!(
        rows[0].keys()[0].as_scalar(),
        Some(&CanonicalScalar::Int(2))
    );
    assert_eq!(rows[0].get(0).unwrap().as_integer(), Some(14));
    assert_eq!(
        rows[0].get(1).unwrap().as_average().unwrap().to_string(),
        "1"
    );
    assert_eq!(rows[0].get(2).unwrap().as_count(), Some(1));
    assert_eq!(
        rows[1].keys()[0].as_scalar(),
        Some(&CanonicalScalar::Int(5))
    );
    assert_eq!(rows[1].get(0).unwrap().as_integer(), Some(5));
    assert!(rows[2].keys()[0].is_null());
    assert!(rows[2].get(0).unwrap().is_null());
    assert_eq!(
        rows[2].get(1).unwrap().as_average().unwrap().to_string(),
        "0"
    );
    assert_eq!(rows[2].get(2).unwrap().as_count(), Some(0));
    let expression = prepare("MATCH (n) RETURN SUM(n.p+2*n.q) AS x,SUM((n.p+2)*n.q) AS y");
    let rows = run(&expression, &[VId(2)], &[], &properties())
        .unwrap()
        .value;
    assert_eq!(rows[0].get(0).unwrap().as_integer(), Some(6));
    assert_eq!(rows[0].get(1).unwrap().as_integer(), Some(8));
}

#[test]
fn repeated_hidden_expressions_share_state_but_preserve_parameter_occurrences() {
    let text = "MATCH (n) RETURN SUM(n.p*$factor) AS total \
        HAVING SUM(n.p * $factor)>$floor ORDER BY SUM(n.p*$factor)";
    let calls = Cell::new(0);
    let template = PreparedGraphAggregateText::prepare(text, |kind, name| {
        calls.set(calls.get() + 1);
        symbols(kind, name)
    })
    .unwrap();
    assert_eq!(calls.get(), 1);
    assert_eq!(template.parameter_schema()[0].name, "factor");
    assert_eq!(template.parameter_schema()[0].occurrences, 3);
    assert_eq!(template.parameter_schema()[1].name, "floor");
    let args = GqlParameters::new()
        .with_int64("factor", 3)
        .unwrap()
        .with_int64("floor", 0)
        .unwrap();
    let bound = template.bind_parameters(&args).unwrap();
    assert_eq!(bound.evaluation_aggregate_columns().len(), 1);
    assert_eq!(bound.input_projection().unwrap().len(), 1);
    assert_eq!(bound.input_pattern().columns().len(), 1);
    assert_eq!(bound, template.bind_parameters(&args).unwrap());
    assert_eq!(calls.get(), 1);
    let rows = run(&bound, &[VId(2), VId(4)], &[], &properties())
        .unwrap()
        .value;
    assert_eq!(rows[0].get(0).unwrap().as_integer(), Some(21));
    let hidden = prepare(
        "MATCH (n) RETURN COUNT(*) AS c HAVING \
        SUM(n.p*n.q) IN [14,19] ORDER BY SUM(n.p*n.q)",
    );
    assert_eq!(hidden.evaluation_aggregate_columns().len(), 2);
    assert_eq!(hidden.aggregate_columns().len(), 1);
    let rows = run(&hidden, &[VId(1), VId(2), VId(3)], &[], &properties())
        .unwrap()
        .value;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values().len(), 1);
    assert_eq!(rows[0].get(0).unwrap().as_count(), Some(3));
}

#[test]
fn computed_keys_work_as_hidden_grouping_and_direct_clause_references() {
    let vertices = [VId(1), VId(2), VId(3), VId(4), VId(5)];
    let plan = prepare(
        "MATCH (n) RETURN COUNT(*) AS c GROUP BY ABS(n.p) \
        HAVING ABS(n.p) IS NOT NULL ORDER BY ABS(n.p) DESC",
    );
    let rows = run(&plan, &vertices, &[], &properties()).unwrap().value;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.keys().is_empty()));
    assert_eq!(rows[0].get(0).unwrap().as_count(), Some(1));
    assert_eq!(rows[1].get(0).unwrap().as_count(), Some(3));
    let plan = prepare("MATCH (n) RETURN COUNT(*) AS c GROUP BY n.p+1 ORDER BY n.p+1 DESC LIMIT 1");
    let rows = run(&plan, &vertices, &[], &properties()).unwrap().value;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get(0).unwrap().as_count(), Some(1));
    let plan = prepare("MATCH (n) RETURN DISTINCT COUNT(*) AS c GROUP BY ABS(n.p)");
    assert_eq!(
        run(&plan, &vertices, &[], &properties())
            .unwrap()
            .value
            .len(),
        2
    );
}

#[test]
fn row_independent_outputs_run_after_global_aggregation_even_on_empty_input() {
    use fgdb_gql::GraphCountSource;
    use fgdb_gql::algebra::GraphValue;

    let template = PreparedGraphAggregateText::prepare_with_parameter_types(
        "MATCH (n) RETURN COUNT(*) AS count, [x IN $values | x+1] AS values",
        &[("values", GqlParameterType::List)],
        symbols,
    )
    .unwrap();
    assert_eq!(template.parameter_schema()[0].occurrences, 1);
    for numbers in [vec![1, 1, 3], vec![]] {
        let args = GqlParameters::new()
            .with_list(
                "values",
                numbers
                    .iter()
                    .map(|&value| GraphValue::Scalar(CanonicalScalar::Int(value)))
                    .collect(),
            )
            .unwrap();
        let plan = template.bind_parameters(&args).unwrap();
        assert_eq!(
            plan.count_summary_source(),
            Some(GraphCountSource::Vertices)
        );
        for vertices in [&[][..], &[VId(1), VId(2), VId(3)][..]] {
            let rows = run(&plan, vertices, &[], &Props::new()).unwrap().value;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].values()[0].as_count(), Some(vertices.len() as u64));
            assert_eq!(
                rows[0].values()[1].as_value(),
                Some(&GraphValue::List(
                    numbers
                        .iter()
                        .map(|value| GraphValue::Scalar(CanonicalScalar::Int(value + 1)))
                        .collect()
                ))
            );
            let summary = plan
                .prepare_count_summary(
                    GraphCountSource::Vertices,
                    vertices.len() as u64,
                    vertices.len() as u64,
                )
                .unwrap();
            assert_eq!(
                summary
                    .execute_governed::<&'static str, ()>(
                        vertices.len() as u64,
                        policy(),
                        || Ok(())
                    )
                    .unwrap()
                    .value,
                rows
            );
        }
    }
    let grouped = prepare("MATCH (n) RETURN n.q AS key, COUNT(*) AS count, [1,2] AS values");
    assert!(grouped.count_summary_source().is_none());
    let groups = run(&grouped, &[VId(1), VId(2), VId(3)], &[], &properties())
        .unwrap()
        .value;
    assert_eq!(groups.len(), 3);
    assert!(
        run(&grouped, &[], &[], &Props::new())
            .unwrap()
            .value
            .is_empty()
    );
    assert!(
        prepare("MATCH (n) RETURN COUNT(*) AS count, rand() AS draw")
            .count_summary_source()
            .is_none()
    );
    assert!(
        prepare("MATCH (n) RETURN COUNT(*) AS count, [n.p] AS values")
            .count_summary_source()
            .is_none()
    );
}

#[test]
fn constant_and_nullable_arguments_preserve_walk_optional_and_empty_bags() {
    let plan = prepare(
        "MATCH REPEATABLE ELEMENTS WALK (a)-[:R*0..2]->(b) RETURN \
        COUNT(1) AS c,COUNT(NULL) AS absent,SUM(7) AS total,MIN('quoted '' value') AS text",
    );
    let edges = [
        (VId(1), R, VId(1)),
        (VId(1), R, VId(1)),
        (VId(1), R, VId(2)),
    ];
    let rows = run(&plan, &[VId(1), VId(2)], &edges, &Props::new())
        .unwrap()
        .value;
    assert_eq!(rows[0].get(0).unwrap().as_count(), Some(11));
    assert_eq!(rows[0].get(1).unwrap().as_count(), Some(0));
    assert_eq!(rows[0].get(2).unwrap().as_integer(), Some(77));
    assert_eq!(
        rows[0].get(3).unwrap().as_value().unwrap().as_scalar(),
        Some(&CanonicalScalar::ucs_basic_text("quoted ' value").unwrap())
    );
    let empty = run(&plan, &[], &[], &Props::new()).unwrap().value;
    assert_eq!(empty.len(), 1);
    assert_eq!(empty[0].get(0).unwrap().as_count(), Some(0));
    assert!(empty[0].get(2).unwrap().is_null());
    let optional = prepare(
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) RETURN \
        COUNT(1) AS c,AVG(COALESCE(b.p,0)) AS mean",
    );
    let rows = run(
        &optional,
        &[VId(1), VId(2), VId(3)],
        &[(VId(1), R, VId(2)), (VId(1), R, VId(2))],
        &properties(),
    )
    .unwrap()
    .value;
    assert_eq!(rows[0].get(0).unwrap().as_count(), Some(4));
    assert_eq!(
        rows[0].get(1).unwrap().as_average().unwrap().to_string(),
        "1"
    );
}

#[test]
fn plain_column_aggregates_keep_the_original_streaming_definition() {
    let actual = prepare("MATCH (n) RETURN n.p AS p,SUM(n.q) AS total GROUP BY n.p");
    let child = PreparedGraphText::prepare("MATCH (n) RETURN n.p AS p,n.q AS total", symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let expected = PreparedGraphAggregate::prepare(
        child,
        &[0],
        &[GraphAggregate::sum_int("total", 1)],
        0,
        None,
    )
    .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(actual.canonical_bytes(), expected.canonical_bytes());
    assert!(actual.input_projection().is_none());
    let count = prepare("MATCH (n) RETURN COUNT(*) AS c");
    assert!(count.input_projection().is_none());
    let identity = prepare("MATCH (n) RETURN MIN(n) AS first,COUNT(n) AS c");
    assert!(identity.input_projection().is_none());
    let rows = run(&identity, &[VId(2), VId(1)], &[], &Props::new())
        .unwrap()
        .value;
    assert_eq!(
        rows[0].get(0).unwrap().as_value().unwrap().as_vertex(),
        Some(VId(1))
    );
    assert_eq!(rows[0].get(1).unwrap().as_count(), Some(2));
}

#[test]
fn typed_parameters_share_argument_validation_and_original_offsets_without_interpolation() {
    let text = "\u{2003}MATCH (n) RETURN MIN($payload) AS text,SUM(n.p*$step) AS total";
    let calls = Cell::new(0);
    let template = PreparedGraphAggregateText::prepare_with_parameter_types(
        text,
        &[(
            "payload",
            GqlParameterType::Scalar(CanonicalScalarKind::Text),
        )],
        |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        },
    )
    .unwrap();
    let payload = "λ') GROUP BY secret; SET n.p=0";
    let args = GqlParameters::new()
        .with_text("payload", payload)
        .unwrap()
        .with_int64("step", 2)
        .unwrap();
    let plan = template.bind_parameters(&args).unwrap();
    let frozen = plan.canonical_bytes();
    let rows = run(&plan, &[VId(2)], &[], &properties()).unwrap().value;
    assert_eq!(
        rows[0].get(0).unwrap().as_value().unwrap().as_scalar(),
        Some(&CanonicalScalar::ucs_basic_text(payload).unwrap())
    );
    assert_eq!(rows[0].get(1).unwrap().as_integer(), Some(4));
    assert_eq!(calls.get(), 1);
    assert_eq!(
        frozen,
        template.bind_parameters(&args).unwrap().canonical_bytes()
    );
    let missing = template.bind_parameters(&GqlParameters::new()).unwrap_err();
    assert_eq!(missing.offset, text.find("$payload").unwrap());
    assert!(matches!(
        missing.kind,
        GraphPatternTextErrorKind::MissingParameter
    ));
    let wrong = GqlParameters::new()
        .with_bool("payload", true)
        .unwrap()
        .with_int64("step", 2)
        .unwrap();
    assert!(matches!(
        template.bind_parameters(&wrong).unwrap_err().kind,
        GraphPatternTextErrorKind::ParameterTypeMismatch { .. }
    ));
    assert!(
        template
            .bind_parameters(&args.clone().with_int64("extra", 1).unwrap())
            .is_err()
    );
    assert!(!format!("{plan:?} {template:?} {rows:?} {missing:?}").contains(payload));
}

#[test]
fn malformed_computation_and_grouping_refuse_before_catalog_resolution() {
    for text in [
        "MATCH (n) RETURN SUM(n.p+) AS x",
        "MATCH (n) RETURN SUM(AVG(n.p)) AS x",
        "MATCH (n) RETURN SUM(n+1) AS x",
        "MATCH (n) RETURN SUM(TRUE+1) AS x",
        "MATCH (n) RETURN SUM(COALESCE(n.p)) AS x",
        "MATCH (n) RETURN SUM(other.p*2) AS x",
        "MATCH (n) RETURN SUM(n.p+$x) AS x LIMIT $x",
        "MATCH (n) RETURN SUM(*) AS x",
        "MATCH (n) RETURN ABS(n.p) AS g,COUNT(*) AS c GROUP BY ABS(n.q)",
        "MATCH (n) RETURN COUNT(*) AS c GROUP BY n.p+1,n.p+1",
        "MATCH (n) RETURN COUNT(*) AS c HAVING SUM(n.p+) > 0",
        "MATCH (n) RETURN COUNT(*) AS c ORDER BY SUM(n.p+)",
        "MATCH (n) RETURN [x IN collect(sum(n.p)) | x] AS values",
        "MATCH (n) RETURN [x IN collect(n.p) | x] AS values, x AS leaked",
        "MATCH (n) RETURN [n IN collect(n.p) | id(n)] AS values GROUP BY n",
        "MATCH (n) RETURN [n IN collect(n.p) | elementId(n)] AS values GROUP BY n",
        "MATCH (n) RETURN reduce(n = 0, x IN collect(n.p) | id(n)) AS values GROUP BY n",
        "MATCH (n) RETURN [n IN collect(n.p) | count(n)] AS values",
        "MATCH (n) RETURN [n IN collect(n.p) | sum(id(n))] AS values",
        "MATCH (n) RETURN [n IN collect(n.p) | count(elementId(n))] AS values",
        "MATCH (n) RETURN [n IN collect(n.p) | count(labels(n))] AS values",
        "MATCH (n) RETURN [n IN collect(n.p) | count(n{.p})] AS values",
        "MATCH (a)-[r:R]->(b) RETURN [r IN collect(a.p) | count(r)] AS values",
        "MATCH (a)-[r:R]->(b) RETURN [r IN collect(a.p) | count(type(r))] AS values",
        "MATCH (a)-[r:R]->(b) RETURN [r IN collect(a.p) | count(startNode(r))] AS values",
        "MATCH p=(a)-[:R]->(b) RETURN [p IN collect(a.p) | count(p)] AS values",
        "MATCH p=(a)-[:R]->(b) RETURN [p IN collect(a.p) | count(nodes(p))] AS values",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphAggregateText::prepare(text, |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "{text}");
    }
    // An unaliased computed RETURN value is named by its source text, as in
    // openCypher, and stays the same grouping key as its GROUP BY spelling.
    let named = PreparedGraphAggregateText::prepare(
        "MATCH (n) RETURN ABS(n.p),COUNT(*),COUNT(n.p) GROUP BY ABS(n.p)",
        symbols,
    )
    .unwrap();
    assert_eq!(named.columns(), &["ABS(n.p)", "COUNT(*)", "COUNT(n.p)"]);
    for expression in [
        format!("{}n.p{}", "(".repeat(66), ")".repeat(66)),
        std::iter::repeat_n("n.p", MAX_GRAPH_INTEGER_INSTRUCTIONS)
            .collect::<Vec<_>>()
            .join("+"),
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphAggregateText::prepare(
                &format!("MATCH (n) RETURN SUM({expression}) AS x"),
                |kind, name| {
                    calls.set(calls.get() + 1);
                    symbols(kind, name)
                }
            )
            .is_err()
        );
        assert_eq!(calls.get(), 0);
    }
}

#[test]
fn hidden_arithmetic_and_eager_source_failures_survive_limit_zero_and_lazy_branches() {
    let props: Props = BTreeMap::from([
        ((VId(1), P), CanonicalScalar::Int(1).into()),
        ((VId(2), P), CanonicalScalar::Int(0).into()),
    ]);
    for text in [
        "MATCH (n) RETURN SUM(10/n.p) AS x LIMIT 0",
        "MATCH (n) RETURN COUNT(*) AS c HAVING SUM(10/n.p) IS NOT NULL LIMIT 0",
        "MATCH (n) RETURN COUNT(*) AS c ORDER BY SUM(10/n.p) LIMIT 0",
    ] {
        let plan = prepare(text);
        assert!(matches!(run(&plan, &[VId(1), VId(2)], &[], &props),
            Err(GqlQueryError::Source(GraphAggregateError::InputExpression { error, .. }))
                if error.kind == GraphIntegerErrorKind::DivisionByZero));
    }
    let plan = prepare("MATCH (n) RETURN SUM(COALESCE(1,10/n.p)) AS x LIMIT 0");
    assert!(
        run(&plan, &[VId(1), VId(2)], &[], &props)
            .unwrap()
            .value
            .is_empty()
    );
    let result: ResultOf = plan.execute_governed(
        2,
        [VId(1), VId(2)],
        [],
        |_, _| Ok(true),
        |vid, key| {
            if vid == VId(2) {
                Err("unreadable lazy input")
            } else {
                Ok(props.get(&(vid, key)))
            }
        },
        policy(),
        || Ok(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphAggregateError::Source(
            "unreadable lazy input"
        )))
    ));
    // Preparation validates shape, not data-independent constant evaluation.
    let plan = prepare("MATCH (n) RETURN COUNT(*) AS c,SUM(1/0) AS bad");
    let empty = run(&plan, &[], &[], &Props::new()).unwrap().value;
    assert_eq!(empty[0].get(0).unwrap().as_count(), Some(0));
    assert!(empty[0].get(1).unwrap().is_null());
    assert!(matches!(run(&plan, &[VId(1)], &[], &Props::new()),
        Err(GqlQueryError::Source(GraphAggregateError::InputExpression { error, .. }))
            if error.kind == GraphIntegerErrorKind::DivisionByZero));
}

/// A RETURN item is a whole expression. A comparison over aggregates is the
/// item's Boolean (TCK Return2 [10]: `MATCH (a) RETURN count(a) > 0`). A
/// comparison over a property is a computed key, grouped by its three-valued
/// Boolean as any computed key is grouped by its value, NULL its own group.
#[test]
fn predicate_items_compare_aggregates_and_group_by_their_boolean_value() {
    use fgdb_gql::GraphAggregateValue;
    use fgdb_gql::algebra::GraphValue;

    let flag = |value: Option<bool>| {
        GraphAggregateValue::Value(GraphValue::Scalar(
            value.map_or(CanonicalScalar::Null, CanonicalScalar::Bool),
        ))
    };
    // Keys first, then the aggregate columns, wherever the plan keeps them.
    let cells = |row: &GraphAggregateRow| -> Vec<GraphAggregateValue> {
        (row.keys().iter().cloned().map(GraphAggregateValue::Value))
            .chain(row.values().iter().cloned())
            .collect()
    };
    let props = properties();
    let plan = prepare(
        "MATCH (n) RETURN COUNT(n) > 0 AS any, COUNT(*) = 2 AND SUM(n.p) IS NOT NULL AS pair",
    );
    // p is -2, 2, 2 and 5 on vertices 1 to 4, so vertices 1 and 2 sum to 0,
    // which is not NULL.
    for (vertices, expected) in [
        (&[][..], [false, false]),
        (&[VId(1), VId(2)][..], [true, true]),
        (&[VId(1), VId(2), VId(3), VId(4)][..], [true, false]),
    ] {
        let rows = run(&plan, vertices, &[], &props).unwrap().value;
        assert_eq!(rows.len(), 1, "{vertices:?}");
        assert_eq!(
            cells(&rows[0]),
            expected.map(|value| flag(Some(value))),
            "{vertices:?}"
        );
    }
    // Vertex 5 has no p: its key is NULL, a group apart from false.
    let plan = prepare("MATCH (n) RETURN n.p > 1 AS big, COUNT(*) AS c");
    let vertices = [VId(1), VId(2), VId(3), VId(4), VId(5)];
    let rows = run(&plan, &vertices, &[], &props).unwrap().value;
    assert_eq!(rows.len(), 3);
    for expected in [
        vec![flag(Some(true)), GraphAggregateValue::Count(3)],
        vec![flag(Some(false)), GraphAggregateValue::Count(1)],
        vec![flag(None), GraphAggregateValue::Count(1)],
    ] {
        assert!(
            rows.iter().any(|row| cells(row) == expected),
            "missing group {expected:?}"
        );
    }
}
