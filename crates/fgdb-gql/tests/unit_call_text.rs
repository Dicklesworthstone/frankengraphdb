//! Unit write CALL keeps native write owners, explicit import scopes and a
//! private body. Engine integration laws cover staged effects and rollback.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GraphSymbol, GraphSymbolKind, GraphWriteChainStep,
    PreparedGraphSetText, PreparedGraphWriteChain, PreparedGraphWriteChainText,
};
use std::cell::Cell;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "F") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}

fn prepare(text: &str) -> PreparedGraphWriteChain {
    assert!(
        PreparedGraphWriteChainText::is_chain(text).unwrap(),
        "{text}"
    );
    PreparedGraphWriteChainText::prepare(text, RelationId(1), symbols)
        .unwrap_or_else(|error| panic!("prepare {text}: {error:?}"))
        .bind_parameters(&GqlParameters::new())
        .unwrap_or_else(|error| panic!("bind {text}: {error:?}"))
}

#[test]
fn unit_calls_admit_native_read_write_bodies_and_every_import_spelling() {
    for text in [
        "CALL { CREATE (:F) }",
        "CALL() { CREATE (:F) }",
        "WITH 7 AS x CALL(x) { CREATE (:F {p:x}) } RETURN x",
        "WITH 7 AS x,8 AS y CALL(*) { CREATE (:F {p:x,q:y}) } RETURN x,y",
        "WITH 7 AS x CALL { WITH x CREATE (:F {p:x}) } RETURN x",
        "WITH 7 AS x,8 AS y CALL { WITH * CREATE (:F {p:x,q:y}) } RETURN y",
        "CALL { WITH 7 AS local CREATE (:F {p:local}) } RETURN 1 AS value",
        "MATCH (n:F) CALL(n) { SET n.p=n.p+1 } RETURN n.p",
        "MATCH (n:F) CALL(n) { MATCH (n)-[:R]->(m) SET m.q=n.p } RETURN n",
        "CALL() { UNWIND [1,1,2] AS x MERGE (n:F {p:x}) ON CREATE SET n.q=1 ON MATCH SET n.q=n.q+1 }",
        "CALL() { MATCH (n:F) WITH n WHERE n.p>0 DELETE n }",
        "CALL() { CREATE (n:F) WITH n MATCH (m:F) CREATE (n)-[:R]->(m) }",
        "MATCH (n:F) CALL(n) { OPTIONAL MATCH (n)-[:R]->(m) SET m.q=1 } RETURN n",
        "WITH 7 AS x CALL(x) { WITH x AS y CREATE (:F {p:y}) } RETURN x",
    ] {
        let chain = prepare(text);
        assert!(
            matches!(
                chain.steps().first(),
                Some(GraphWriteChainStep::UnitCall(_))
            ),
            "{text}"
        );
    }
}

#[test]
fn unit_calls_compose_with_prior_and_later_writes_reads_and_scoped_bodies() {
    for text in [
        "CREATE (n:F {p:1}) CALL(n) { SET n.p=n.p+1 } RETURN n.p",
        "CALL() { CREATE (:F) } CREATE (:F) RETURN 1 AS value",
        "CALL() { CREATE (:F) } MATCH (n:F) RETURN n.p",
        "CALL() { CREATE (:F) } WITH 7 AS x CREATE (:F {p:x}) RETURN x",
        "WITH 7 AS x CALL(x) { CALL(x) { CREATE (:F {p:x}) } CREATE (:F {q:x}) } RETURN x",
        "CALL() { CALL() { CALL() { CREATE (:F) } } }",
        "CALL() { FOREACH (x IN [1,2] | CREATE (:F {p:x})) } RETURN 1 AS value",
        "CALL() { MATCH (n:F) CALL(n) { SET n.q=1 } }",
        "CALL() { CREATE (n:F) CALL(n) { SET n.q=1 } DELETE n }",
        "CALL() { CALL() { CREATE (:F) } } CALL() { CREATE (:F) }",
        "CALL { CALL { RETURN 7 AS x } CREATE (:F {p:x}) }",
    ] {
        prepare(text);
    }
}

#[test]
fn unit_call_imports_and_body_bindings_never_widen_outer_scope() {
    for text in [
        "WITH 7 AS x CALL() { CREATE (:F {p:x}) }",
        "WITH 7 AS x CALL { CREATE (:F {p:x}) }",
        "WITH 7 AS x CALL(y) { CREATE (:F) }",
        "WITH 7 AS x CALL(x,x) { CREATE (:F) }",
        "WITH 7 AS x CALL(x AS y) { CREATE (:F {p:y}) }",
        "CALL() { CREATE (local:F) } RETURN local",
        "CALL() { CREATE (local:F) } SET local.p=1",
        "CALL() { CALL() { CREATE (local:F) } SET local.p=1 }",
        "WITH 7 AS x CALL(x) { CALL() { CREATE (:F {p:x}) } }",
        "FOREACH (x IN [] | CALL(x) { CREATE (:F {p:x}) })",
    ] {
        assert!(
            PreparedGraphWriteChainText::prepare(text, RelationId(1), symbols).is_err(),
            "unexpectedly admitted {text}"
        );
    }
}

#[test]
fn returning_or_malformed_unit_bodies_refuse_before_catalog_resolution() {
    for text in [
        "CALL() { CREATE (:F) RETURN 1 AS value }",
        "CALL() { CREATE (:F) UNION CREATE (:F) }",
        "CALL() { CREATE (:F)",
        "CALL() { CREATE (:F]; }",
        "CALL() { CREATE (:F); CREATE (:F) }",
    ] {
        let lookups = Cell::new(0);
        let result =
            PreparedGraphWriteChainText::prepare(text, RelationId(1), |kind, name: &str| {
                lookups.set(lookups.get() + 1);
                symbols(kind, name)
            });
        assert!(result.is_err(), "{text}");
        assert_eq!(lookups.get(), 0, "{text}");
    }
}

#[test]
fn unit_call_parameters_keep_one_typed_contract_across_body_and_outer_tail() {
    let text =
        "WITH 7 AS x CALL(x) { CREATE (n:F {p:x+$delta}) SET n.q=$delta } RETURN x+$delta AS value";
    let template = PreparedGraphWriteChainText::prepare_with_parameter_types(
        text,
        RelationId(1),
        &[("delta", GqlParameterType::Int64)],
        symbols,
    )
    .unwrap();
    let schema = template.parameter_schema();
    assert_eq!(schema.len(), 1);
    assert_eq!(schema[0].name, "delta");
    assert_eq!(schema[0].parameter_type, GqlParameterType::Int64);
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());
    let bound = template
        .bind_parameters(&GqlParameters::new().with_int64("delta", 3).unwrap())
        .unwrap();
    let GraphWriteChainStep::UnitCall(call) = &bound.steps()[0] else {
        panic!("unit CALL must retain its native owner");
    };
    assert_eq!(call.columns(), &["value"]);
    assert_eq!(call.body().steps().len(), 2);
}

#[test]
fn write_call_framing_keeps_read_only_calls_and_identifier_uses_with_their_owner() {
    for text in [
        "CALL() { RETURN 1 AS value } RETURN value",
        "CALL { MATCH (n:F) RETURN n } RETURN n",
        "CALL db.labels() YIELD label RETURN label",
        "WITH 'CALL() { CREATE (:F) }' AS text RETURN text",
        "WITH {create:1, call:2} AS value RETURN value.call",
        "RETURN 1 AS value; CALL() { CREATE (:F) }",
    ] {
        assert!(
            !PreparedGraphWriteChainText::is_chain(text).unwrap(),
            "{text}"
        );
    }
    assert!(
        PreparedGraphSetText::prepare("CALL() { RETURN 1 AS value } RETURN value", symbols).is_ok()
    );
    assert!(PreparedGraphSetText::prepare("CALL() { CREATE (:F) } RETURN 1", symbols).is_err());
}

#[test]
fn combined_scoped_depth_refuses_before_descendant_catalog_work() {
    let unit_depth = fgdb_gql::MAX_GRAPH_SET_DEPTH / 2;
    let foreach_depth = fgdb_gql::MAX_GRAPH_SET_DEPTH - unit_depth + 1;
    let mut text = "CALL() { ".repeat(unit_depth);
    text.push_str(&"FOREACH (x IN [] | ".repeat(foreach_depth));
    text.push_str("CREATE (:F)");
    text.push_str(&")".repeat(foreach_depth));
    text.push_str(&" }".repeat(unit_depth));
    let lookups = Cell::new(0);
    let result = PreparedGraphWriteChainText::prepare(&text, RelationId(1), |kind, name: &str| {
        lookups.set(lookups.get() + 1);
        symbols(kind, name)
    });
    assert!(result.is_err());
    assert_eq!(lookups.get(), 0);
}

#[test]
fn scheduler_executes_native_insert_plans_and_preserves_empty_duplicate_outer_rows() {
    use fgdb_delta_types::ElementId;
    use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
    use fgdb_gql::insertion::{GraphInsertIntent, GraphInsertPolicy};
    use fgdb_gql::{
        GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphIntegerError, GraphWriteChainCursor,
        GraphWriteChainTask,
    };
    use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, VId};

    fn no_graph(
        _: &PreparedGraphPattern<GraphValueRow>,
        _: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), ()>> {
        panic!("source-free unit bodies must retain their native owned row source")
    }

    let int = |value| GraphValue::Scalar(CanonicalScalar::Int(value));
    let query = GqlQueryPolicy::new(100, 100, 100_000, 100_000);
    for (text, mut expected_rows, expected_properties) in [
        (
            "UNWIND [[],[2,2],NULL,[5]] AS values \
             CALL(values) { UNWIND values AS x CREATE (:F {p:x}) } RETURN values",
            vec![
                GraphValue::List(Vec::new()),
                GraphValue::List(vec![int(2), int(2)]),
                GraphValue::Scalar(CanonicalScalar::Null),
                GraphValue::List(vec![int(5)]),
            ],
            vec![2, 2, 5],
        ),
        (
            "UNWIND [1,1,2] AS x CALL(x) { \
                 CALL(x) { UNWIND [x,x+10] AS y CREATE (:F {p:y}) } \
             } RETURN x",
            vec![int(1), int(1), int(2)],
            vec![1, 1, 2, 11, 11, 12],
        ),
    ] {
        let chain = prepare(text);
        let mut cursor = GraphWriteChainCursor::new(&chain, true);
        let mut rows = None;
        let mut properties = Vec::new();
        let mut next_id = 0_u128;
        while let Some(task) = cursor
            .next(&mut rows, &mut |_| Ok::<_, GraphIntegerError>(()))
            .unwrap()
        {
            match task {
                GraphWriteChainTask::Read(read) => {
                    rows = Some(
                        read.execute_governed(rows.take(), query, no_graph, || Ok(()))
                            .unwrap()
                            .value,
                    );
                }
                GraphWriteChainTask::Write {
                    step: GraphWriteChainStep::Insert(insert),
                    delivers: false,
                    continues: false,
                } => {
                    let batch = insert
                        .execute_governed_over(
                            rows.take(),
                            GraphInsertPolicy::new(query, 100, 0),
                            no_graph,
                            |_| {
                                next_id += 1;
                                Ok::<_, ()>(ElementId::Vertex(VId(next_id)))
                            },
                            || Ok(()),
                        )
                        .unwrap();
                    for intent in batch.into_intents() {
                        let GraphInsertIntent::Vertex {
                            properties: fields, ..
                        } = intent
                        else {
                            panic!("only the prepared native vertex insertion may execute");
                        };
                        assert_eq!(fields.len(), 1);
                        assert_eq!(fields[0].0, PropertyKeyId(1));
                        properties.push(fields[0].1.clone());
                    }
                    rows = Some(Vec::new());
                }
                other => panic!("unexpected native scheduler task: {other:?}"),
            }
        }
        let mut actual: Vec<_> = rows
            .unwrap()
            .into_iter()
            .map(|row| {
                assert_eq!(row.len(), 1);
                row.values()[0].clone()
            })
            .collect();
        actual.sort();
        expected_rows.sort();
        properties.sort();
        assert_eq!(actual, expected_rows, "{text}");
        assert_eq!(
            properties,
            expected_properties
                .into_iter()
                .map(|value| CanonicalPropertyValue::from(CanonicalScalar::Int(value)))
                .collect::<Vec<_>>(),
            "{text}",
        );
    }
}
