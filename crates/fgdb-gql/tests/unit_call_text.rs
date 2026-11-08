//! Write CALL keeps native write owners and explicit import scopes. Unit
//! bodies preserve occurrences; returning bodies join their private output.
//! Engine integration laws cover staged effects and rollback.

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

fn no_graph(
    _: &fgdb_gql::algebra::PreparedGraphPattern<fgdb_gql::algebra::GraphValueRow>,
    _: fgdb_gql::GqlQueryPolicy,
) -> Result<
    fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
    fgdb_gql::GqlQueryError<(), ()>,
> {
    panic!("source-free unit CALL must retain its native owned row source")
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
fn union_or_malformed_write_bodies_refuse_before_catalog_resolution() {
    for text in [
        "CALL() { CREATE (:F) RETURN 1 AS value UNION RETURN 2 AS value }",
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
fn write_keyword_aliases_keep_native_read_only_execution() {
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::{GqlQueryError, GqlQueryPolicy};
    use fgdb_types::CanonicalScalar;

    let mut cases = Vec::new();
    for name in [
        "create", "insert", "merge", "set", "remove", "delete", "detach", "foreach",
    ] {
        cases.push((
            format!("CALL {{ WITH 1 AS {name} RETURN {name} AS value }} RETURN value"),
            vec![1],
        ));
    }
    for (text, expected) in [
        (
            "CALL { WITH 1 AS delete RETURN delete + 1 AS value } RETURN value",
            vec![2],
        ),
        (
            "CALL { WITH 1 AS delete RETURN DISTINCT delete + 1 AS value } RETURN value",
            vec![2],
        ),
        (
            "CALL { WITH [2,3] AS delete RETURN delete[1] AS value } RETURN value",
            vec![3],
        ),
        (
            "CALL { WITH 1 AS delete \
             RETURN CASE delete + 1 WHEN 2 THEN 7 ELSE 0 END AS value } RETURN value",
            vec![7],
        ),
        (
            "CALL { WITH 1 AS delete \
             WITH CASE delete + 1 WHEN 2 THEN 7 ELSE 0 END AS value RETURN value } RETURN value",
            vec![7],
        ),
        (
            "CALL { WITH 1 AS delete WITH DISTINCT delete + 1 AS kept \
             RETURN kept AS value } RETURN value",
            vec![2],
        ),
        (
            "CALL { CALL { WITH 1 AS delete RETURN delete + 1 AS kept } \
             RETURN kept AS value } RETURN value",
            vec![2],
        ),
        (
            "CALL { WITH 1 AS delete RETURN delete + 1 AS value \
             UNION ALL RETURN 3 AS value } RETURN value",
            vec![2, 3],
        ),
        (
            "WITH 1 AS delete CALL(delete) { RETURN delete + 1 AS value } RETURN value",
            vec![2],
        ),
    ] {
        cases.push((text.to_owned(), expected));
    }
    for (text, expected) in cases {
        assert!(
            !PreparedGraphWriteChainText::is_chain(&text).unwrap(),
            "read-only identifier use routed to writes: {text}"
        );
        let plan = PreparedGraphSetText::prepare(&text, symbols)
            .unwrap_or_else(|error| panic!("prepare {text}: {error:?}"))
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let result = plan
            .execute_governed(
                GqlQueryPolicy::new(100, 100, 100_000, 100_000),
                |_, _| -> Result<_, GqlQueryError<(), ()>> {
                    panic!("source-free read CALL must not touch a graph: {text}")
                },
                || Ok(()),
            )
            .unwrap_or_else(|error| panic!("execute {text}: {error:?}"));
        let mut values: Vec<_> = result
            .value
            .into_iter()
            .map(|row| {
                assert_eq!(row.len(), 1, "{text}");
                row.values()[0].clone()
            })
            .collect();
        values.sort();
        assert_eq!(
            values,
            expected
                .into_iter()
                .map(|value| GraphValue::Scalar(CanonicalScalar::Int(value)))
                .collect::<Vec<_>>(),
            "{text}",
        );
    }

    // A child's RETURN ends at its closing brace. UNION begins a new branch;
    // its write must still reach the unit body's explicit refusal rule.
    for text in [
        "CALL { CALL { WITH 1 AS delete RETURN delete + 1 AS value } \
         CREATE (:F {p:value}) }",
        "CALL { CALL { RETURN 1 AS x } CALL { CREATE (:F {p:2}) } }",
        "CALL { WITH {return:7} AS item WITH item.return AS value CREATE (:F {p:value}) }",
        "CALL { WITH {`return`:7} AS item WITH item.`return` AS value CREATE (:F {p:value}) }",
        "CALL { MATCH (n:F) WITH n,1 AS case DELETE n }",
    ] {
        prepare(text);
    }
    for text in [
        "CALL { RETURN 1 AS value UNION CREATE (:F) }",
        "CALL { RETURN 1 AS value UNION ALL CREATE (:F) }",
    ] {
        assert!(
            PreparedGraphWriteChainText::is_chain(text).unwrap(),
            "{text}"
        );
        assert!(
            PreparedGraphWriteChainText::prepare(text, RelationId(1), symbols).is_err(),
            "a returned write branch must still refuse: {text}",
        );
    }
    // RETURN is a reserved identifier in this language, even when delimited;
    // the framing fix must not widen that established name grammar.
    for text in [
        "CALL { WITH 1 AS return WITH return AS value CREATE (:F {p:value}) }",
        "CALL { WITH 1 AS `return` WITH `return` AS value CREATE (:F {p:value}) }",
    ] {
        assert!(PreparedGraphWriteChainText::prepare(text, RelationId(1), symbols).is_err());
    }
}

#[test]
fn unit_call_rebinding_changes_nested_effects_without_mutating_a_bound_plan() {
    use fgdb_delta_types::ElementId;
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::insertion::{GraphInsertIntent, GraphInsertPolicy};
    use fgdb_gql::{GqlQueryPolicy, GraphIntegerError, GraphWriteChainCursor, GraphWriteChainTask};
    use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, VId};

    let text = "WITH $outer AS x CALL(x) { CALL(x) { CREATE (:F {p:x+$delta}) } } \
                RETURN x+$tail AS value";
    let template = PreparedGraphWriteChainText::prepare_with_parameter_types(
        text,
        RelationId(1),
        &[
            ("outer", GqlParameterType::Int64),
            ("delta", GqlParameterType::Int64),
            ("tail", GqlParameterType::Int64),
        ],
        symbols,
    )
    .unwrap();
    assert_eq!(template.parameter_schema().len(), 3);
    let arguments = |outer, delta, tail| {
        GqlParameters::new()
            .with_int64("outer", outer)
            .unwrap()
            .with_int64("delta", delta)
            .unwrap()
            .with_int64("tail", tail)
            .unwrap()
    };
    let first = template.bind_parameters(&arguments(1, 2, 3)).unwrap();
    let retained = first.clone();
    let second = template.bind_parameters(&arguments(9, 20, 30)).unwrap();
    assert_ne!(first, second);
    assert_eq!(first, retained);
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());

    let query = GqlQueryPolicy::new(100, 100, 100_000, 100_000);
    for (chain, expected_property, expected_result) in
        [(&first, 3, 4), (&second, 29, 39), (&first, 3, 4)]
    {
        let mut cursor = GraphWriteChainCursor::new(chain, true);
        let mut rows = None;
        let mut properties = Vec::new();
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
                    continues: false,
                    delivers: false,
                } => {
                    let batch = insert
                        .execute_governed_over(
                            rows.take(),
                            GraphInsertPolicy::new(query, 10, 0),
                            no_graph,
                            |_| Ok::<_, ()>(ElementId::Vertex(VId(1))),
                            || Ok(()),
                        )
                        .unwrap();
                    for intent in batch.into_intents() {
                        let GraphInsertIntent::Vertex {
                            properties: fields, ..
                        } = intent
                        else {
                            panic!("only the native vertex insert may execute");
                        };
                        properties.extend(fields);
                    }
                    rows = Some(Vec::new());
                }
                other => panic!("unexpected task: {other:?}"),
            }
        }
        assert_eq!(
            properties,
            vec![(
                PropertyKeyId(1),
                CanonicalPropertyValue::from(CanonicalScalar::Int(expected_property)),
            )],
        );
        let rows = rows.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].values(),
            &[GraphValue::Scalar(CanonicalScalar::Int(expected_result))],
        );
    }
}

#[test]
fn unit_call_cursor_stops_before_effects_at_every_scheduler_control_boundary() {
    use fgdb_gql::{
        GqlQueryPolicy, GraphIntegerError, GraphIntegerErrorKind, GraphWriteChainCursor,
        GraphWriteChainTask,
    };

    let chain = prepare("UNWIND [1,1,2] AS x CALL(x) { CALL(x) { CREATE (:F {p:x}) } } RETURN x");
    let query = GqlQueryPolicy::new(100, 100, 100_000, 100_000);
    let run = |stop: Option<usize>| {
        let mut cursor = GraphWriteChainCursor::new(&chain, true);
        let mut rows = None;
        let mut trace = Vec::new();
        let mut writes = 0;
        let result = loop {
            let task = cursor.next(&mut rows, &mut |event| {
                let at = trace.len();
                trace.push((event, writes));
                if stop == Some(at) {
                    Err(GraphIntegerError {
                        instruction: at,
                        kind: GraphIntegerErrorKind::IncompatibleOperands,
                    })
                } else {
                    Ok(())
                }
            });
            match task {
                Err(error) => break Err(error),
                Ok(None) => break Ok(rows),
                Ok(Some(GraphWriteChainTask::Read(read))) => {
                    rows = Some(
                        read.execute_governed(rows.take(), query, no_graph, || Ok(()))
                            .unwrap()
                            .value,
                    );
                }
                Ok(Some(GraphWriteChainTask::Write {
                    step: GraphWriteChainStep::Insert(_),
                    continues: false,
                    delivers: false,
                })) => {
                    writes += 1;
                    rows = Some(Vec::new());
                }
                Ok(Some(other)) => panic!("unexpected task: {other:?}"),
            }
        };
        (result, trace, writes)
    };
    let (complete, trace, writes) = run(None);
    assert_eq!(writes, 3);
    assert_eq!(complete.unwrap().unwrap().len(), 3);
    for stop in 0..trace.len() {
        let (refused, observed, writes) = run(Some(stop));
        assert_eq!(refused.unwrap_err().instruction, stop);
        assert_eq!(observed, trace[..=stop]);
        assert_eq!(
            writes, trace[stop].1,
            "effects after refused checkpoint {stop}"
        );
    }
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
                GraphValue::List(Vec::new().into_boxed_slice()),
                GraphValue::List(vec![int(2), int(2)].into_boxed_slice()),
                GraphValue::Scalar(CanonicalScalar::Null),
                GraphValue::List(vec![int(5)].into_boxed_slice()),
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

#[test]
fn returning_calls_retain_typed_exports_from_every_native_write_owner() {
    use fgdb_gql::GraphSetColumnType::{Edge, Scalar, Vertex};

    for (text, kind) in [
        ("CALL { CREATE (n:F) RETURN n AS value }", Vertex),
        ("CALL() { CREATE (:F) RETURN 1 AS value }", Scalar),
        (
            "MATCH (n:F) CALL(n) { SET n.p=1 RETURN n AS value } RETURN value",
            Vertex,
        ),
        (
            "MATCH (n:F) CALL(n) { REMOVE n.p RETURN 7 AS value } RETURN value",
            Scalar,
        ),
        (
            "MATCH (n:F) CALL(n) { DELETE n RETURN 7 AS value } RETURN value",
            Scalar,
        ),
        (
            "CALL { MERGE (n:F {p:1}) RETURN n AS value } RETURN value",
            Vertex,
        ),
        (
            "MATCH (a:F),(b:F) CALL(a,b) { \
             MERGE (a)-[r:R]->(b) RETURN r AS value } RETURN value",
            Edge,
        ),
        (
            "MATCH (a:F),(b:F) CALL(a,b) { MERGE (a)-[r:R]->(b) \
             ON CREATE SET r.p=1 ON MATCH SET r.q=2 RETURN r AS value } RETURN value",
            Edge,
        ),
        (
            "CALL { CREATE (:F) MATCH (m:F) RETURN m AS value } RETURN value",
            Vertex,
        ),
        (
            "CALL { CREATE (:F) RETURN count(*) AS value } RETURN value",
            Scalar,
        ),
    ] {
        let chain = prepare(text);
        let GraphWriteChainStep::ReturningCall(call) = &chain.steps()[0] else {
            panic!("returned writes must retain their scoped owner: {text}");
        };
        assert_eq!(call.export_columns(), &["value"], "{text}");
        assert_eq!(call.export_column_types(), &[kind], "{text}");
        assert_eq!(call.body().columns(), &["value"], "{text}");
        assert_eq!(call.body().column_types(), &[kind], "{text}");
        assert_eq!(chain.columns(), &["value"], "{text}");
        assert_eq!(chain.column_types(), &[kind], "{text}");
        assert!(
            call.output().is_some(),
            "terminal delivery needs a governed output: {text}"
        );
        assert!(chain.returns_rows(), "{text}");
    }

    for text in [
        "WITH 7 AS x CALL { WITH x CREATE (:F {p:x}) RETURN x+1 AS value } RETURN value",
        "WITH 7 AS x,8 AS y CALL(*) { CREATE (:F {p:x,q:y}) RETURN x+y AS value } RETURN value",
        "CALL { CREATE (n:F) RETURN n } SET n.p=2 RETURN n",
        "CALL { CREATE (n:F {p:1}) RETURN n AS created } SET created.q=2 RETURN created.p AS p,created.q AS q",
        "CALL { CREATE (n:F {p:1}) RETURN n AS parent } CREATE (child:F) CREATE (parent)-[:R]->(child) RETURN parent.p AS p",
        "CALL { CREATE (n:F) RETURN n } MATCH (m:F) RETURN n,m",
        "CREATE (a:F) WITH a CALL(a) { CREATE (b:F) RETURN b } CREATE (copy:F) RETURN copy",
        "CREATE (a) WITH a CALL (a) { CREATE (b) RETURN b } CREATE (copy) RETURN copy",
        "CALL { CALL { CREATE (:F) RETURN 1 AS value } } RETURN 7 AS kept",
    ] {
        prepare(text);
    }
}

#[test]
fn returning_call_exports_obey_scope_collision_and_terminal_return_rules() {
    for text in [
        "WITH 7 AS x CALL(x) { CREATE (:F) RETURN x } RETURN x",
        "WITH 7 AS x CALL() { CREATE (:F) RETURN 1 AS x } RETURN x",
        "WITH 7 AS x CALL() { CREATE (:F) RETURN x AS value } RETURN value",
        "WITH 7 AS x CALL(x) { CALL() { CREATE (:F) RETURN x AS value } RETURN value } RETURN value",
        "CALL { CREATE (local:F) RETURN 1 AS value } RETURN local",
        "CALL { CREATE (local:F) RETURN 1 AS value } SET local.p=2",
        "CALL { CREATE (:F) RETURN 1 AS value,2 AS value } RETURN value",
        "CALL { CREATE (:F) RETURN 1 AS value CREATE (:F) } RETURN value",
        "CALL { FOREACH (x IN [] | CREATE (:F) RETURN x AS value) RETURN 1 AS value } RETURN value",
    ] {
        assert!(
            PreparedGraphWriteChainText::prepare(text, RelationId(1), symbols).is_err(),
            "unexpectedly admitted {text}",
        );
    }
}

#[test]
fn returning_scheduler_executes_native_writes_and_joins_each_outer_occurrence() {
    use fgdb_delta_types::ElementId;
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::insertion::GraphInsertPolicy;
    use fgdb_gql::{
        GqlQueryPolicy, GraphIntegerError, GraphWriteChainCursor, GraphWriteChainReadPhase,
        GraphWriteChainTask,
    };
    use fgdb_types::{CanonicalScalar, VId};

    let query = GqlQueryPolicy::new(100, 100, 100_000, 100_000);
    for (text, expected_rows, expected_created, expected_body_reads) in [
        (
            "UNWIND [1,1,2] AS x CALL(x) { UNWIND [x,x+10] AS y \
             CREATE (:F {p:y}) RETURN y AS value } RETURN x,value",
            vec![
                vec![1, 1],
                vec![1, 11],
                vec![1, 1],
                vec![1, 11],
                vec![2, 2],
                vec![2, 12],
            ],
            6,
            0,
        ),
        (
            "UNWIND [[],[2,2],NULL,[5]] AS values CALL(values) { \
             UNWIND values AS x CREATE (:F {p:x}) RETURN x AS value } RETURN value",
            vec![vec![2], vec![2], vec![5]],
            3,
            0,
        ),
        (
            "UNWIND [1,1,2] AS x CALL(x) { \
             CALL(x) { CREATE (:F {p:x}) RETURN x+10 AS v } \
             CALL(v) { CREATE (:F {q:v}) } RETURN v AS value } RETURN x,value",
            vec![vec![1, 11], vec![1, 11], vec![2, 12]],
            6,
            0,
        ),
        (
            "CALL { CREATE (:F) CALL { RETURN 7 AS value } RETURN value } RETURN value",
            vec![vec![7]],
            1,
            1,
        ),
        (
            "CALL { UNWIND [] AS x CREATE (:F) CALL { RETURN 7 AS value } \
             RETURN count(*) AS value } RETURN value",
            vec![vec![0]],
            0,
            1,
        ),
        (
            "CALL { UNWIND [1,1,2] AS x CREATE (:F {p:x}) RETURN x AS value }",
            vec![vec![1], vec![1], vec![2]],
            3,
            0,
        ),
    ] {
        let chain = prepare(text);
        let mut cursor = GraphWriteChainCursor::new(&chain, true);
        let mut rows = None;
        let mut created = 0_u128;
        let mut phases = Vec::new();
        while let Some(task) = cursor
            .next(&mut rows, &mut |_| Ok::<_, GraphIntegerError>(()))
            .unwrap()
        {
            match task {
                GraphWriteChainTask::Read(read) => {
                    phases.push((read.phase(), read.delivers_rows()));
                    rows = Some(
                        read.execute_governed(rows.take(), query, no_graph, || Ok(()))
                            .unwrap_or_else(|error| panic!("read {text}: {error:?}"))
                            .value,
                    );
                }
                GraphWriteChainTask::Write {
                    step: GraphWriteChainStep::Query(insert),
                    delivers: false,
                    ..
                } => {
                    let batch = insert
                        .execute_governed_over(
                            rows.take(),
                            GraphInsertPolicy::new(query, 100, 0),
                            no_graph,
                            |_| {
                                created += 1;
                                Ok::<_, ()>(ElementId::Vertex(VId(created)))
                            },
                            || Ok(()),
                        )
                        .unwrap_or_else(|error| {
                            panic!("native returning CREATE {text}: {error:?}")
                        });
                    rows = Some(batch.into_parts().1.value);
                }
                GraphWriteChainTask::Write {
                    step: GraphWriteChainStep::Insert(insert),
                    continues: false,
                    delivers: false,
                } => {
                    insert
                        .execute_governed_over(
                            rows.take(),
                            GraphInsertPolicy::new(query, 100, 0),
                            no_graph,
                            |_| {
                                created += 1;
                                Ok::<_, ()>(ElementId::Vertex(VId(created)))
                            },
                            || Ok(()),
                        )
                        .unwrap_or_else(|error| panic!("native unit CREATE {text}: {error:?}"));
                    rows = Some(Vec::new());
                }
                other => panic!("unexpected native scheduler task for {text}: {other:?}"),
            }
        }
        let mut actual: Vec<_> = rows
            .unwrap()
            .into_iter()
            .map(|row| row.values().to_vec())
            .collect();
        let mut expected: Vec<Vec<_>> = expected_rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|value| GraphValue::Scalar(CanonicalScalar::Int(value)))
                    .collect()
            })
            .collect();
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected, "{text}");
        assert_eq!(created, expected_created, "{text}");
        assert_eq!(
            phases
                .iter()
                .filter(|(phase, _)| *phase == GraphWriteChainReadPhase::BodyOutput)
                .count(),
            expected_body_reads,
            "{text}",
        );
        assert!(
            phases
                .iter()
                .all(|(phase, delivers)| !*delivers || *phase == GraphWriteChainReadPhase::Output),
            "body work must stay private: {text}",
        );
        assert_eq!(
            phases.iter().filter(|(_, delivers)| *delivers).count(),
            1,
            "{text}"
        );
    }
}
