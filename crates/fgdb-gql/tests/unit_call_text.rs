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
fn scoped_write_imports_survive_every_native_with_owner() {
    for text in [
        "WITH 7 AS x CALL(x) { WITH 1 AS y CREATE (:F {p:x+y}) } RETURN x",
        "WITH 7 AS x CALL(x) { CREATE (:F) WITH 1 AS y CREATE (:F {p:x+y}) } RETURN x",
        "WITH 7 AS x CALL(x) { CREATE (:F) WITH count(*) AS c \
         CREATE (:F {p:x+c}) RETURN x+c AS value } RETURN value",
        "WITH 7 AS x CALL(x) { CREATE (:F) WITH DISTINCT 1 AS y ORDER BY x+y \
         CREATE (:F {p:x+y}) } RETURN x",
        "WITH 7 AS x CALL(x) { WITH (x) AS x CREATE (:F {p:x}) } RETURN x",
        "MATCH (n:F) CALL(n) { SET n.p=1 WITH 0 AS ignored SET n.q=n.p } RETURN n",
        "WITH 7 AS x CALL(x) { MERGE (:F {p:x}) WITH 1 AS y \
         CREATE (:F {p:x+y}) RETURN x AS value } RETURN value",
        "WITH 7 AS x CALL(x) { MERGE (:F {p:x}) WITH count(*) AS c \
         CREATE (:F {p:x+c}) RETURN x AS value } RETURN value",
        "MATCH (a:F),(b:F) WITH a,b,7 AS x CALL(a,b,x) { \
         MERGE (a)-[:R]->(b) WITH 1 AS y CREATE (:F {p:x+y}) } RETURN x",
        "MATCH (a:F),(b:F) WITH a,b,7 AS x CALL(a,b,x) { \
         MERGE (a)-[r:R]->(b) ON CREATE SET r.p=x \
         WITH count(*) AS c CREATE (:F {p:x+c}) } RETURN x",
        "WITH 7 AS x CALL(x) { WITH 0 AS ignored \
         FOREACH (y IN [1,2] | CREATE (:F {p:x+y})) } RETURN x",
    ] {
        prepare(text);
    }
}

#[test]
fn scoped_write_import_names_cannot_be_redeclared_or_inherited_by_empty_calls() {
    for text in [
        "UNWIND [] AS x CALL(x) { WITH 1 AS x CREATE (:F) }",
        "WITH 7 AS x CALL(x) { CREATE (:F) WITH 1 AS x CREATE (:F) }",
        "WITH 7 AS x CALL(x) { CREATE (:F) WITH count(*) AS x CREATE (:F) }",
        "WITH 7 AS x CALL(x) { MERGE (:F) WITH 1 AS x CREATE (:F) }",
        "MATCH (n:F) CALL(n) { SET n.p=1 WITH 1 AS n CREATE (:F) }",
        "WITH 7 AS x CALL(x) { WITH 1 AS y UNWIND [2] AS x CREATE (:F) }",
        "WITH 7 AS x CALL(x) { WITH 1 AS y CALL() { CREATE (:F {p:x}) } }",
        "WITH 7 AS x CALL { WITH x WITH 1 AS y CREATE (:F {p:x}) }",
        "WITH 7 AS x CALL { WITH x CREATE (:F) WITH 1 AS y CREATE (:F {p:x}) }",
    ] {
        assert!(
            PreparedGraphWriteChainText::prepare(text, RelationId(1), symbols).is_err(),
            "unexpectedly admitted {text}",
        );
    }
}

#[test]
fn post_write_continuations_need_their_owned_row_without_an_ambient_call_frame() {
    let chain = prepare("CALL() { CREATE (:F) RETURN 1 AS n } RETURN n");
    let owned = chain.read_input().unwrap();
    let arguments = fgdb_gql::PreparedGraphSet::arguments(
        owned.columns().to_vec(),
        owned.column_types().to_vec(),
    )
    .unwrap();
    assert!(chain.accepts_read_continuation(&owned));
    assert!(!chain.accepts_read_continuation(&owned.clone().cross_join(arguments).unwrap()));
    assert!(!chain.accepts_read_continuation(&owned.clone().cross_join(owned).unwrap()));
}

#[test]
fn scoped_write_arguments_outlive_empty_bags_and_restore_parent_frames() {
    use fgdb_delta_types::ElementId;
    use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
    use fgdb_gql::insertion::{GraphInsertIntent, GraphInsertPolicy};
    use fgdb_gql::{
        GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphElementReader, GraphIntegerError,
        GraphSetSource, GraphWriteChainCursor, GraphWriteChainTask,
    };
    use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, VId};
    use std::sync::Arc;

    struct Lexical(Option<Arc<GraphValueRow>>);

    impl GraphSetSource<(), ()> for Lexical {
        fn call_arguments(&self) -> Option<Arc<GraphValueRow>> {
            self.0.clone()
        }

        fn pattern(
            &mut self,
            _: &PreparedGraphPattern<GraphValueRow>,
            _: GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), ()>> {
            panic!("lexical import restoration must not query a graph source")
        }
    }

    impl GraphElementReader<()> for Lexical {
        fn call_arguments(&self) -> Option<Arc<GraphValueRow>> {
            self.0.clone()
        }

        fn element_property(
            &mut self,
            _: ElementId,
            _: PropertyKeyId,
        ) -> Result<Option<CanonicalPropertyValue>, ()> {
            panic!("source-free lexical restoration must not read an element")
        }
    }

    let query = GqlQueryPolicy::new(100, 100, 1_000_000, 1_000_000);
    for (text, expected_rows, expected_properties) in [
        (
            "WITH 7 AS x CALL(x) { WITH 1 AS y CREATE (:F {p:x+y}) \
             WITH 2 AS z CREATE (:F {p:x+z}) RETURN x+z AS value } RETURN value",
            vec![9],
            vec![8, 9],
        ),
        (
            "WITH 7 AS x CALL(x) { UNWIND [] AS ignored CREATE (:F) \
             WITH count(*) AS c CREATE (:F {p:x+c}) \
             RETURN x+c AS value } RETURN value",
            vec![7],
            vec![7],
        ),
        (
            "WITH 7 AS x CALL(x) { WITH 0 AS ignored \
             CALL() { WITH 100 AS x CREATE (:F {p:x}) } \
             WITH 2 AS y CREATE (:F {p:x+y}) RETURN x AS value } RETURN value",
            vec![7],
            vec![9, 100],
        ),
        (
            "WITH 7 AS x CALL(x) { CREATE (:F {p:x}) WITH 2 AS y \
             RETURN x+y AS value UNION ALL WITH 3 AS y RETURN x+y AS value } RETURN value",
            vec![9, 10],
            vec![7],
        ),
        (
            "WITH 3 AS y,7 AS x CALL(x,y) { CREATE (:F {p:x-y}) RETURN x-y AS value \
             UNION ALL WITH 2 AS n RETURN x-y+n AS value } RETURN value",
            vec![4, 6],
            vec![4],
        ),
        (
            "WITH 7 AS x CALL(x) { WITH 0 AS ignored \
             FOREACH (y IN [1,2] | CREATE (:F {p:x+y})) } RETURN x",
            vec![7],
            vec![8, 9],
        ),
        (
            "UNWIND [2,2] AS x CALL(x) { WITH 1 AS y CREATE (:F {p:x+y}) \
             WITH count(*) AS c RETURN x+c AS value } RETURN value",
            vec![3, 3],
            vec![3, 3],
        ),
    ] {
        let chain = prepare(text);
        let mut cursor = GraphWriteChainCursor::new(&chain, true);
        let mut rows = None;
        let mut next_id = 0_u128;
        let mut properties = Vec::new();
        let mut retain = |batch: fgdb_gql::insertion::GraphInsertBatch| {
            for intent in batch.into_intents() {
                let GraphInsertIntent::Vertex {
                    properties: fields, ..
                } = intent
                else {
                    panic!("only a native vertex creation is expected");
                };
                assert_eq!(fields.len(), 1, "{text}");
                assert_eq!(fields[0].0, PropertyKeyId(1), "{text}");
                properties.push(fields[0].1.clone());
            }
        };
        while let Some(task) = cursor
            .next(&mut rows, &mut |_| Ok::<_, GraphIntegerError>(()))
            .unwrap()
        {
            let arguments = cursor.call_arguments();
            match task {
                GraphWriteChainTask::Read(read) => {
                    rows = Some(
                        read.execute_governed(rows.take(), query, Lexical(arguments), || Ok(()))
                            .unwrap_or_else(|error| panic!("read {text}: {error:?}"))
                            .value,
                    );
                }
                GraphWriteChainTask::Write {
                    step: GraphWriteChainStep::Query(insert),
                    delivers: false,
                    ..
                } => {
                    let proposal = insert
                        .collect_governed_over(
                            rows.take(),
                            GraphInsertPolicy::new(query, 100, 0),
                            Lexical(arguments.clone()),
                            |_| {
                                next_id += 1;
                                Ok::<_, ()>(ElementId::Vertex(VId(next_id)))
                            },
                            || Ok(()),
                        )
                        .unwrap_or_else(|error| panic!("collect {text}: {error:?}"));
                    let (batch, output) = proposal.into_parts();
                    retain(batch);
                    rows = Some(
                        output
                            .finish_governed_reading(Lexical(arguments), || Ok::<(), ()>(()))
                            .unwrap_or_else(|error| panic!("output {text}: {error:?}"))
                            .value,
                    );
                }
                GraphWriteChainTask::Write {
                    step: GraphWriteChainStep::Insert(insert),
                    delivers: false,
                    ..
                } => {
                    let batch = insert
                        .execute_governed_over(
                            rows.take(),
                            GraphInsertPolicy::new(query, 100, 0),
                            Lexical(arguments),
                            |_| {
                                next_id += 1;
                                Ok::<_, ()>(ElementId::Vertex(VId(next_id)))
                            },
                            || Ok(()),
                        )
                        .unwrap_or_else(|error| panic!("insert {text}: {error:?}"));
                    retain(batch);
                    rows = Some(Vec::new());
                }
                other => panic!("unexpected native task for {text}: {other:?}"),
            }
        }
        assert!(cursor.call_arguments().is_none(), "{text}");
        let mut actual: Vec<_> = rows
            .unwrap()
            .into_iter()
            .map(|row| {
                assert_eq!(row.len(), 1, "{text}");
                row.values()[0].clone()
            })
            .collect();
        actual.sort();
        assert_eq!(
            actual,
            expected_rows
                .into_iter()
                .map(|value| GraphValue::Scalar(CanonicalScalar::Int(value)))
                .collect::<Vec<_>>(),
            "{text}",
        );
        properties.sort();
        assert_eq!(
            properties,
            expected_properties
                .into_iter()
                .map(|value| CanonicalPropertyValue::from(CanonicalScalar::Int(value)))
                .collect::<Vec<_>>(),
            "{text}",
        );
        assert_eq!(next_id as usize, properties.len(), "{text}");
    }
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
fn malformed_write_bodies_refuse_before_catalog_resolution() {
    for text in [
        "CALL() { CREATE (:F)",
        "CALL() { CREATE (:F]; }",
        "CALL() { CREATE (:F); CREATE (:F) }",
        "CALL() { CREATE (:F) UNION }",
        "CALL() { UNION CREATE (:F) }",
        "CALL() { CREATE (:F) UNION ALL UNION ALL CREATE (:F) }",
        "CALL() { CREATE (:F) UNION ALL CREATE (:F) UNION CREATE (:F) }",
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
fn call_union_structural_errors_keep_the_native_diagnostic_and_utf8_byte_offset() {
    for (text, expected) in [
        (
            "WITH '雪' AS note CALL() { UNION CREATE (:F) }",
            "a nonempty UNION arm",
        ),
        (
            "CALL() { CREATE (:F {p:'雪'}) UNION ALL UNION ALL CREATE (:F) }",
            "a nonempty UNION arm",
        ),
        (
            "CALL() { CREATE (:F {p:'雪'}) UNION ALL CREATE (:F) UNION CREATE (:F) }",
            "one quantifier across CALL UNION arms",
        ),
        (
            "CALL() { CREATE (:F {p:'雪'}) UNION DISTINCT CREATE (:F) UNION ALL CREATE (:F) }",
            "one quantifier across CALL UNION arms",
        ),
    ] {
        let lookups = Cell::new(0);
        let error =
            PreparedGraphWriteChainText::prepare(text, RelationId(1), |kind, name: &str| {
                lookups.set(lookups.get() + 1);
                symbols(kind, name)
            })
            .unwrap_err();
        let fgdb_gql::GraphWriteChainTextError::Insert(error) = error else {
            panic!("CALL framing keeps its native query diagnostic: {text}");
        };
        let fgdb_gql::GraphInsertTextErrorKind::Query(
            fgdb_gql::GraphPatternTextErrorKind::Expected(actual),
        ) = error.kind
        else {
            panic!("structural UNION refusal keeps its expected-token class: {text}");
        };
        assert_eq!(actual, expected, "{text}");
        assert_eq!(error.offset, text.rfind("UNION").unwrap(), "{text}");
        assert_eq!(lookups.get(), 0, "{text}");
    }
}

#[test]
fn call_union_framing_preserves_identifier_operands_aliases_and_native_read_classification() {
    for text in [
        "WITH 3 AS union CALL(union) { CREATE (:F {p:union}) \
         RETURN union AS value } RETURN value",
        "WITH 3 AS union CALL(union) { CREATE (:F) RETURN 1+union AS value \
         UNION ALL RETURN union AS value } RETURN value",
        "WITH 3 AS union CALL(union) { CREATE (:F) RETURN 2*union AS value \
         UNION ALL RETURN union AS value } RETURN value",
        "WITH 3 AS union CALL(union) { CREATE (:F) \
         RETURN CASE WHEN true THEN union ELSE 2 END AS value \
         UNION ALL RETURN union AS value } RETURN value",
        "WITH 3 AS union CALL(union) { WITH union AS local CREATE (:F {p:local}) \
         RETURN local AS value UNION ALL RETURN union AS value } RETURN value",
        "WITH [1,2] AS union CALL(union) { UNWIND union AS local CREATE (:F {p:local}) \
         RETURN local AS value UNION ALL RETURN 0 AS value } RETURN value",
        "WITH 3 AS union CALL(union) { CREATE (:F) RETURN union AS value ORDER BY union \
         UNION ALL RETURN 0 AS value } RETURN value",
        "CALL() { CREATE (:F) RETURN 1 AS union \
         UNION ALL RETURN 2 AS union } RETURN union",
        "CALL() { WITH 3 AS delete CREATE (:F) RETURN delete \
         UNION ALL RETURN 2 AS delete } RETURN delete",
        "CALL() { WITH 3 AS `local value` CREATE (:F) RETURN `local value` AS value \
         UNION ALL RETURN 2 AS value } RETURN value",
        "CALL() { WITH 3 AS on RETURN on \
         UNION ALL CREATE (:F) RETURN 2 AS on } RETURN on",
        "CALL() { WITH 3 AS starts RETURN starts \
         UNION ALL CREATE (:F) RETURN 2 AS starts } RETURN starts",
        "CALL() { WITH 3 AS ends RETURN ends \
         UNION ALL CREATE (:F) RETURN 2 AS ends } RETURN ends",
        "WITH true AS union CALL(union) { CREATE (:F) RETURN NOT union AS value \
         UNION ALL RETURN false AS value } RETURN value",
        "WITH {union:3} AS item CALL(item) { CREATE (:F) RETURN item.union AS value \
         UNION ALL RETURN 0 AS value } RETURN value",
        "CALL() { CREATE (:F) WITH 1 AS value RETURN * \
         UNION ALL RETURN 2 AS value } RETURN value",
    ] {
        prepare(text);
    }
    // The framer must see the next arm after a delimited operand. The
    // variable grammar separately refuses a delimited grammar keyword.
    let text = "CALL() { WITH 3 AS `not` CREATE (:F) RETURN `not` \
                UNION ALL RETURN 2 AS `not` } RETURN `not`";
    assert!(PreparedGraphWriteChainText::is_chain(text).unwrap());
    assert!(
        PreparedGraphWriteChainText::is_chain(
            "CALL() { RETURN `not` UNION ALL CREATE (:F) RETURN 2 AS value }",
        )
        .unwrap()
    );
    let fgdb_gql::GraphWriteChainTextError::Insert(error) =
        PreparedGraphWriteChainText::prepare(text, RelationId(1), symbols).unwrap_err()
    else {
        panic!("the write head must preserve the variable parser's refusal");
    };
    assert!(matches!(
        error.kind,
        fgdb_gql::GraphInsertTextErrorKind::Query(fgdb_gql::GraphPatternTextErrorKind::Expected(
            "a delimited identifier that is not a keyword",
        )),
    ));
    assert_eq!(error.offset, text.find("`not`").unwrap());
    for text in [
        "CALL { WITH 1 AS union,2 AS delete RETURN union+delete AS value } RETURN value",
        "CALL { WITH 1 AS union,2 AS delete RETURN 2*union+delete AS value } RETURN value",
        "CALL { WITH 1 AS union,2 AS delete \
         RETURN CASE WHEN true THEN union ELSE delete END AS value } RETURN value",
        "CALL { WITH true AS union RETURN NOT union AS value } RETURN value",
    ] {
        assert!(
            !PreparedGraphWriteChainText::is_chain(text).unwrap(),
            "identifier operands must not reopen a completed read branch: {text}",
        );
        assert!(
            PreparedGraphSetText::prepare(text, symbols).is_ok(),
            "{text}"
        );
    }
}

#[test]
fn call_union_arms_keep_native_read_write_owners_and_one_checked_export_schema() {
    use fgdb_gql::GraphSetColumnType::{Any, Scalar};

    for (text, read_arms, kind) in [
        (
            "CALL() { CREATE (:F) RETURN 1 AS value UNION RETURN 2 AS value }",
            vec![false, true],
            Scalar,
        ),
        (
            "CALL() { RETURN 1 AS value UNION ALL CREATE (:F) RETURN 2 AS value }",
            vec![true, false],
            Scalar,
        ),
        (
            "CALL() { CREATE (n:F {p:1}) RETURN n.p AS value \
             UNION RETURN [1] AS value UNION RETURN {p:1} AS value }",
            vec![false, true, true],
            Any,
        ),
    ] {
        let chain = prepare(text);
        let GraphWriteChainStep::ReturningCall(call) = &chain.steps()[0] else {
            panic!("UNION must retain the native returning CALL owner: {text}");
        };
        assert_eq!(call.export_columns(), &["value"], "{text}");
        assert_eq!(call.export_column_types(), &[kind], "{text}");
        assert_eq!(
            call.arms()
                .iter()
                .map(|arm| arm.read_body().is_some())
                .collect::<Vec<_>>(),
            read_arms,
            "a read arm must never be represented by an empty write: {text}",
        );
        for arm in call.arms() {
            if let Some(body) = arm.write_body() {
                assert!(!body.steps().is_empty());
            }
        }
    }
    let unit = prepare("CALL() { CREATE (:F) UNION CREATE (:F) }");
    let GraphWriteChainStep::UnitCall(call) = &unit.steps()[0] else {
        panic!("unit UNION preserves its outer occurrence without exporting rows");
    };
    assert_eq!(call.arms().len(), 2);
    assert!(call.arms().iter().all(|arm| arm.write_body().is_some()));
    assert!(!unit.returns_rows());
}

#[test]
fn call_creation_metadata_keeps_static_fields_and_lazy_carried_element_read_demand() {
    let constant =
        prepare("CALL() { CREATE (a:F {p:1}),(b),(a)-[:R {q:2}]->(b) UNION CREATE (:F) }");
    let GraphWriteChainStep::UnitCall(call) = &constant.steps()[0] else {
        panic!("unit CALL owner");
    };
    let GraphWriteChainStep::Insert(insertion) = &call.arms()[0].write_body().unwrap().steps()[0]
    else {
        panic!("native constant creation owner");
    };
    assert!(!insertion.requires_read());
    assert_eq!(
        insertion.created_vertex_label_sets().collect::<Vec<_>>(),
        vec![&[LabelId(1)][..], &[][..]],
    );
    assert_eq!(
        insertion.created_property_keys().collect::<Vec<_>>(),
        vec![PropertyKeyId(1), PropertyKeyId(2)],
    );
    assert_eq!(
        insertion.created_edge_relations().collect::<Vec<_>>(),
        vec![RelationId(1)],
    );

    let carried = prepare(
        "MATCH (n:F) CALL(n) { UNWIND [] AS unused CREATE (:F {p:n.p}) \
         UNION CREATE (:F) }",
    );
    let GraphWriteChainStep::UnitCall(call) = &carried.steps()[0] else {
        panic!("correlated unit CALL owner");
    };
    let GraphWriteChainStep::Insert(insertion) = &call.arms()[0].write_body().unwrap().steps()[0]
    else {
        panic!("native carried-element creation owner");
    };
    assert!(
        insertion.requires_read(),
        "a zero-row source cannot erase the declared property read from n",
    );
}

#[test]
fn call_union_arms_refuse_scope_schema_and_parameter_contract_drift() {
    for text in [
        "CALL() { CREATE (:F) UNION RETURN 1 AS value }",
        "CALL() { CREATE (:F) RETURN 1 AS value UNION CREATE (:F) }",
        "CALL() { CREATE (:F) RETURN 1 AS value UNION RETURN 2 AS other }",
        "CALL() { CREATE (:F) RETURN 1 AS value UNION RETURN 2 AS value,3 AS extra }",
        "CALL() { CREATE (n:F) RETURN n AS value UNION RETURN 2 AS value }",
        "CALL() { CREATE (:F) RETURN 1 AS value UNION RETURN [1] AS value }",
        "WITH 1 AS x CALL { WITH x CREATE (:F {p:x}) RETURN 1 AS value \
         UNION RETURN x AS value } RETURN value",
        "WITH 1 AS x,2 AS y CALL { WITH x CREATE (:F {p:x}) RETURN 1 AS value \
         UNION WITH y RETURN x AS value } RETURN value",
        "CALL() { CREATE (local:F) RETURN 1 AS value UNION RETURN local AS value }",
        "CALL() { CREATE (:F {p:$arg}) RETURN 1 AS value \
         UNION UNWIND $arg AS member RETURN member AS value }",
    ] {
        assert!(
            PreparedGraphWriteChainText::prepare(text, RelationId(1), symbols).is_err(),
            "unexpectedly admitted {text}",
        );
    }

    let template = PreparedGraphWriteChainText::prepare_with_parameter_types(
        "CALL() { CREATE (:F {p:$value}) RETURN $value AS value \
         UNION ALL RETURN $value+1 AS value } RETURN value",
        RelationId(1),
        &[("value", GqlParameterType::Int64)],
        symbols,
    )
    .unwrap();
    let schema = template.parameter_schema();
    assert_eq!(schema.len(), 1);
    assert_eq!(schema[0].name, "value");
    assert_eq!(schema[0].parameter_type, GqlParameterType::Int64);
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());
    let first = template
        .bind_parameters(&GqlParameters::new().with_int64("value", 7).unwrap())
        .unwrap();
    let retained = first.clone();
    let second = template
        .bind_parameters(&GqlParameters::new().with_int64("value", 11).unwrap())
        .unwrap();
    assert_ne!(first, second);
    assert_eq!(first, retained);
}

#[test]
fn call_union_list_parameters_retain_their_domain_across_unconstrained_uses() {
    let inferred = "CALL() { UNWIND $arg AS x RETURN $arg AS value \
                    UNION ALL CREATE (:F) RETURN $arg AS value } RETURN value";
    let declared = "CALL() { CREATE (:F {p:$arg}) RETURN 1 AS value \
                    UNION ALL UNWIND $arg AS x RETURN 1 AS value } RETURN value";
    for (text, declarations) in [
        (inferred, &[][..]),
        (declared, &[("arg", GqlParameterType::List)][..]),
    ] {
        let template = PreparedGraphWriteChainText::prepare_with_parameter_types(
            text,
            RelationId(1),
            declarations,
            symbols,
        )
        .unwrap_or_else(|error| panic!("{text}: {error:?}"));
        let schema = template.parameter_schema();
        assert_eq!(schema.len(), 1);
        assert_eq!(schema[0].parameter_type, GqlParameterType::List);
        assert!(
            template
                .bind_parameters(&GqlParameters::new().with_int64("arg", 1).unwrap())
                .is_err()
        );
        template
            .bind_parameters(&GqlParameters::new().with_list("arg", vec![]).unwrap())
            .unwrap();
    }
}

#[test]
fn call_union_rejects_incompatible_list_and_page_parameters_in_both_orders() {
    use fgdb_gql::{
        GraphInsertTextErrorKind, GraphPatternTextErrorKind::ConflictingParameterTypes,
        GraphSetTextErrorKind, GraphWriteChainTextError,
    };
    let write = "CREATE (:F) RETURN 1 AS value LIMIT $arg";
    let read = "UNWIND $arg AS member RETURN 1 AS value";
    for (first, second) in [(write, read), (read, write)] {
        let text = format!("CALL() {{ {first} UNION ALL {second} }} RETURN value");
        let error =
            PreparedGraphWriteChainText::prepare(&text, RelationId(1), symbols).unwrap_err();
        let offset = match error {
            GraphWriteChainTextError::Insert(error)
                if matches!(
                    error.kind,
                    GraphInsertTextErrorKind::Query(ConflictingParameterTypes)
                ) =>
            {
                error.offset
            }
            GraphWriteChainTextError::Read(error)
                if matches!(
                    error.kind,
                    GraphSetTextErrorKind::Pattern(ConflictingParameterTypes)
                ) =>
            {
                error.offset
            }
            other => panic!("expected a parameter conflict for {text}: {other:?}"),
        };
        assert_eq!(offset, text.rfind("$arg").unwrap());
    }
}

#[test]
fn call_union_arm_limits_precede_catalog_work_and_errors_keep_original_byte_offsets() {
    let body = std::iter::repeat_n("CREATE (:F)", fgdb_gql::MAX_GRAPH_SET_DEPTH + 1)
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let lookups = Cell::new(0);
    let text = format!("WITH '雪' AS note CALL() {{ {body} }}");
    let fgdb_gql::GraphWriteChainTextError::Insert(error) =
        PreparedGraphWriteChainText::prepare(&text, RelationId(1), |kind, name: &str| {
            lookups.set(lookups.get() + 1);
            symbols(kind, name)
        })
        .unwrap_err()
    else {
        panic!("CALL arm-count refusal keeps its native query diagnostic");
    };
    assert!(matches!(
        error.kind,
        fgdb_gql::GraphInsertTextErrorKind::Query(fgdb_gql::GraphPatternTextErrorKind::Expected(
            "bounded CALL UNION arm count"
        ))
    ));
    assert_eq!(error.offset, text.rfind("UNION").unwrap());
    assert_eq!(lookups.get(), 0);

    for separator in [" UNION ", " UNION DISTINCT ", " UNION ALL "] {
        let body = std::iter::repeat_n("CREATE (:F)", fgdb_gql::MAX_GRAPH_SET_DEPTH)
            .collect::<Vec<_>>()
            .join(separator);
        let chain = prepare(&format!("CALL() {{ {body} }}"));
        let GraphWriteChainStep::UnitCall(call) = &chain.steps()[0] else {
            panic!("a valid boundary-sized UNION keeps its native unit CALL owner");
        };
        assert_eq!(call.arms().len(), fgdb_gql::MAX_GRAPH_SET_DEPTH);
    }

    let text = "CALL() { RETURN '雪' AS value \
                UNION ALL CREATE (:Missing) RETURN 'x' AS value }";
    let fgdb_gql::GraphWriteChainTextError::Insert(error) =
        PreparedGraphWriteChainText::prepare(text, RelationId(1), symbols).unwrap_err()
    else {
        panic!("the second arm keeps its native insertion diagnostic");
    };
    assert_eq!(error.offset, text.find("Missing").unwrap());
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
    assert_eq!(call.arms()[0].write_body().unwrap().steps().len(), 2);
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
        (
            "UNWIND [1,1] AS x CALL(x) { CREATE (:F {p:x}) \
             UNION ALL CREATE (:F {p:x+10}) } RETURN x",
            vec![int(1), int(1)],
            vec![1, 1, 11, 11],
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
        let body = call.arms()[0].write_body().unwrap();
        assert_eq!(body.columns(), &["value"], "{text}");
        assert_eq!(body.column_types(), &[kind], "{text}");
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
        "CALL { CREATE (n:F {p:1}) RETURN n } SET n.q=2 RETURN n{.p,.q} AS value",
        "CALL { CREATE (n:F {p:1}) RETURN n } SET n.q=2 RETURN properties(n) AS value",
        "CALL { CREATE (n:F {p:1}) RETURN n } SET n.q=2 RETURN keys(n) AS value",
        "CALL { CREATE (n:F {p:1}) RETURN n } SET n.q=2 RETURN labels(n) AS value",
        "CALL { CREATE (n:F {p:1}) RETURN n } CREATE (:F) RETURN n{.p} AS value",
        "CALL { CREATE ()-[r:R {p:3}]->() RETURN r } SET r.q=4 RETURN r{.p,.q} AS value",
        "CALL { CREATE ()-[r:R {p:3}]->() RETURN r } SET r.q=4 RETURN properties(r) AS value",
        "CALL { CREATE (n:F) RETURN n } MATCH (m:F) RETURN n,m",
        "CREATE (a:F) WITH a CALL(a) { CREATE (b:F) RETURN b } CREATE (copy:F) RETURN copy",
        "CREATE (a) WITH a CALL (a) { CREATE (b) RETURN b } CREATE (copy) RETURN copy",
        "CALL { CALL { CREATE (:F) RETURN 1 AS value } } RETURN 7 AS kept",
    ] {
        prepare(text);
    }
}

#[test]
fn optional_write_calls_keep_export_domains_imports_and_native_continuations() {
    use fgdb_gql::GraphSetColumnType::{Edge, Scalar, Vertex};

    for (text, kind) in [
        ("OPTIONAL CALL { CREATE (n:F) RETURN n AS value }", Vertex),
        ("OPTIONAL CALL() { CREATE (:F) RETURN 1 AS value }", Scalar),
        (
            "MATCH (n:F) OPTIONAL CALL(n) { SET n.p=1 RETURN n AS value } RETURN value",
            Vertex,
        ),
        (
            "OPTIONAL CALL { CREATE ()-[r:R]->() RETURN r AS value } RETURN value",
            Edge,
        ),
        (
            "WITH 7 AS x OPTIONAL CALL { WITH x CREATE (:F {p:x}) RETURN x AS value } RETURN value",
            Scalar,
        ),
        (
            "WITH 7 AS x,8 AS y OPTIONAL CALL(*) { CREATE (:F {p:x,q:y}) RETURN x+y AS value } RETURN value",
            Scalar,
        ),
        (
            "OPTIONAL CALL { CREATE (:F) RETURN 1 AS value UNION ALL RETURN 2 AS value } RETURN value",
            Scalar,
        ),
    ] {
        let chain = prepare(text);
        let GraphWriteChainStep::ReturningCall(call) = &chain.steps()[0] else {
            panic!("OPTIONAL must retain the native returning write owner: {text}");
        };
        assert!(call.is_optional(), "{text}");
        assert_eq!(call.export_column_types(), &[kind], "{text}");
        assert_eq!(chain.column_types(), &[kind], "{text}");
        assert_eq!(chain.columns(), &["value"], "{text}");
    }
    for text in [
        "CREATE (n:F) OPTIONAL CALL(n) { SET n.p=1 RETURN n AS updated } RETURN updated",
        "CALL { CREATE (:F) RETURN 1 AS x } OPTIONAL CALL(x) { CREATE (:F {p:x}) RETURN x+1 AS y } RETURN x,y",
        "WITH 1 AS x OPTIONAL CALL(x) { OPTIONAL CALL(x) { CREATE (:F {p:x}) RETURN x AS y } RETURN y } RETURN y",
        "OPTIONAL CALL { CREATE (n:F) RETURN n } SET n.q=2 RETURN n.p,n.q",
        "OPTIONAL CALL { CREATE (n:F) RETURN n } MATCH (m:F) CREATE (n)-[:R]->(m) RETURN n",
        "CREATE (n:F) OPTIONAL CALL(n) { MATCH (n)-[:R]->(m) RETURN m } RETURN n,m",
    ] {
        prepare(text);
    }
}

#[test]
fn optional_unit_calls_are_legal_and_returning_calls_retain_scope_refusals() {
    for text in [
        "OPTIONAL CALL { CREATE (:F) }",
        "UNWIND [1,1,2] AS x OPTIONAL CALL(x) { CREATE (:F {p:x}) } RETURN x",
        "OPTIONAL CALL { CREATE (:F) UNION ALL CREATE (:F) } RETURN 1 AS value",
    ] {
        let chain = prepare(text);
        assert!(
            matches!(chain.steps()[0], GraphWriteChainStep::UnitCall(_)),
            "{text}"
        );
    }
    for text in [
        "WITH 7 AS x OPTIONAL CALL(x) { CREATE (:F) RETURN x } RETURN x",
        "WITH 7 AS x OPTIONAL CALL() { CREATE (:F {p:x}) RETURN 1 AS value } RETURN value",
        "OPTIONAL CALL { CREATE (local:F) RETURN 1 AS value } RETURN local",
        "OPTIONAL CALL { CREATE (:F) RETURN 1 AS value UNION CREATE (:F) }",
        "OPTIONAL CALL { CREATE (n:F) RETURN n AS value UNION RETURN 1 AS value }",
        "OPTIONAL CALL { CREATE (:F) RETURN 1 AS value CREATE (:F) }",
    ] {
        assert!(
            PreparedGraphWriteChainText::prepare(text, RelationId(1), symbols).is_err(),
            "OPTIONAL must not bypass scope or body admission: {text}",
        );
    }
    for text in [
        "OPTIONAL CALL { RETURN 1 AS value } RETURN value",
        "OPTIONAL CALL() { MATCH (n:F) RETURN n } RETURN n",
    ] {
        assert!(
            !PreparedGraphWriteChainText::is_chain(text).unwrap(),
            "{text}"
        );
        assert!(
            PreparedGraphSetText::prepare(text, symbols).is_ok(),
            "{text}"
        );
    }
    assert!(
        PreparedGraphSetText::prepare(
            "OPTIONAL CALL { CREATE (:F) RETURN 1 AS value } RETURN value",
            symbols,
        )
        .is_err()
    );
}

#[test]
fn captured_return_maps_keep_the_existing_nesting_boundary() {
    // Keep ordinary supported inputs and the exact diagnostic of the retained
    // native CLI. Deep selector lookahead must yield to this existing guard.
    let resolve = |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(LabelId(1))),
        _ => symbols(kind, name),
    };
    let statement = |opening: &str, closing: &str, depth| {
        format!(
            "CALL {{ CREATE (n:Visible {{p:1}}) RETURN n }} \
             WITH n,n.p AS before SET n.q=2 RETURN {}1{} AS value",
            opening.repeat(depth),
            closing.repeat(depth),
        )
    };
    for (opening, closing, error_at) in [("n{x:", "}", 341), ("{x:", "}", 276), ("[", "]", 146)] {
        for depth in [1, 8, 16, 32, 64] {
            let text = statement(opening, closing, depth);
            let chain = PreparedGraphWriteChainText::prepare(&text, RelationId(1), resolve)
                .unwrap_or_else(|error| panic!("{opening} depth {depth}: {error:?}"))
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            assert_eq!(chain.columns(), &["value"], "{opening} depth {depth}");
        }
        for depth in [128, 512, 2_048] {
            let text = statement(opening, closing, depth);
            let error = PreparedGraphWriteChainText::prepare(&text, RelationId(1), resolve)
                .expect_err("the existing expression nesting bound must reject before execution");
            assert_eq!(
                error.to_string(),
                format!("byte {error_at}: integer expression nests deeper than 64 levels"),
                "{opening} depth {depth}",
            );
        }
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
        (
            "UNWIND [1,1,2] AS x CALL(x) { UNWIND [x,x] AS y \
             CREATE (:F {p:y}) RETURN y AS value UNION RETURN x AS value } RETURN x,value",
            vec![vec![1, 1], vec![1, 1], vec![2, 2]],
            6,
            3,
        ),
        (
            "WITH 1 AS x,2 AS y CALL { WITH x CREATE (:F {p:x}) \
             RETURN x AS lhs,10 AS rhs UNION ALL WITH y RETURN 20 AS rhs,y AS lhs \
             } RETURN x,y,lhs,rhs",
            vec![vec![1, 2, 1, 10], vec![1, 2, 2, 20]],
            1,
            1,
        ),
        (
            "CALL() { RETURN 7 AS value UNION ALL UNWIND [] AS x CREATE (:F) \
             RETURN x AS value UNION ALL CREATE (:F) RETURN 8 AS value } RETURN value",
            vec![vec![7], vec![8]],
            1,
            1,
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
