//! Scalar UNWIND operands remain typed native arguments, including aliases
//! shorter than a parameter token and values containing query-shaped text.

use super::*;
use crate::{GqlParameterType, GraphPatternTextErrorKind, GraphWriteScriptErrorKind};
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_types::CanonicalScalarKind;

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}
fn list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}
fn object(fields: Vec<(&str, GraphValue)>) -> GraphValue {
    GraphValue::map(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
    .unwrap()
}
fn rows(values: Vec<GraphValue>) -> GqlParameters {
    GqlParameters::new().with_list("rows", values).unwrap()
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Label, "Entity") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "Other") => Some(GraphSymbol::Label(LabelId(2))),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(PropertyKeyId(3))),
        _ => None,
    }
}

#[test]
fn multirow_vertex_updates_freeze_selection_feedback_across_fields_aliases_and_scopes() {
    let input = rows(vec![
        object(vec![("id", int(1)), ("p", int(2))]),
        object(vec![("id", int(2)), ("p", int(3))]),
    ]);
    let single = rows(vec![object(vec![("id", int(1)), ("p", int(2))])]);
    for tail in [
        "MATCH (n:Entity {id:row.id}) SET n.id=row.p",
        "MATCH (n:Entity) WHERE n.id=row.id SET n.id=row.p+1",
        "MATCH (n:Entity) WHERE n.id IS NULL SET n.id=row.p",
        "MATCH (n:Entity {id:row.id}) REMOVE n.id",
        "MATCH (n:Entity) REMOVE n:Entity",
        "MATCH (n:Entity) SET n:Entity",
        "MATCH (a:Entity), (b:Entity {id:row.id}) SET a.id=row.p",
        "MATCH (a:Entity) OPTIONAL MATCH (a)-[:R]->(b {id:row.id}) SET a.id=row.p",
        "MATCH (a:Entity) WHERE EXISTS { MATCH (a)-[:R]->(b {id:row.id}) } SET a.id=row.p",
        "MATCH (n:Entity) WHERE n.id=row.id OR n.p=0 SET n.id=row.p",
        "MATCH (n:Entity) WHERE NOT (n.id=row.id) REMOVE n.id",
        "MATCH (n:Entity) WHERE abs(n.id)=row.id SET n.id=row.p",
        "MATCH (n:Entity) WHERE coalesce(0,n.id)=row.id SET n.id=row.p",
        "MATCH (n:Entity) WHERE upper(n.name)='X' SET n.name='changed'",
        "MATCH (n:Entity) WHERE n.id IN [row.id,NULL] SET n += {id:row.p}",
        "MATCH (a:Entity)-[e:R]->(b) WHERE e.id=a.p SET a.p=row.p",
        "MATCH (a:Entity) OPTIONAL MATCH (a)-[:R]->(b) WHERE coalesce(b.id,0)=row.id SET a.id=row.p",
        "MATCH (a:Entity) WHERE EXISTS { MATCH (a)-[:R]->(b) WHERE b.id=row.id OR b.p=0 } SET a.id=row.p",
    ] {
        let definition =
            GraphUnwindWriteText::parse(&format!("UNWIND $rows AS row {tail}")).unwrap();
        let bound = definition
            .bind(&input, RelationId(1), symbols)
            .unwrap_or_else(|error| panic!("{tail}: {error:?}"));
        assert!(bound.program().freezes_vertex_update_input(), "{tail}");
        assert_eq!(bound.argument_sets(), 2);
        let singleton = definition.bind(&single, RelationId(1), symbols).unwrap();
        assert_eq!(singleton.argument_sets(), 1);
        assert!(!singleton.program().freezes_vertex_update_input());
    }
    for tail in [
        "MATCH (a:Entity)-[e:R]->(b) WHERE e.id=row.id SET e.id=row.p",
        "MATCH (a:Entity)-[e:R {id:row.id}]->(b) SET e += {id:row.p}",
        "MATCH (a:Entity)-[e:R]->(b) WHERE e.id IS NULL SET e.id=row.p",
        "MATCH (a:Entity)-[e:R]->(b) WHERE abs(e.id)=row.id SET e.id=row.p",
        "MATCH (a:Entity)-[e:R]->(b) WHERE e.id=a.p REMOVE e.id",
        "MATCH (n:Entity {id:row.id})-[:R]->(m) DETACH DELETE n",
        "MATCH (n:Entity {id:row.id}), (anchor:Entity {id:1}) DETACH DELETE n",
        "MATCH (n:Entity {id:row.id}), (anchor:Entity {id:1}) DELETE n",
    ] {
        let definition =
            GraphUnwindWriteText::parse(&format!("UNWIND $rows AS row {tail}")).unwrap();
        let result = definition.bind(&input, RelationId(1), symbols);
        assert!(
            matches!(
                result,
                Err(GraphUnwindWriteError::RowFeedsLaterMatch { statement: 0 })
            ),
            "{tail}: {result:?}"
        );
        assert_eq!(
            definition
                .bind(&single, RelationId(1), symbols)
                .unwrap()
                .argument_sets(),
            1,
            "a single input row has no later MATCH: {tail}"
        );
    }
}

#[test]
fn disjoint_updates_single_vertex_deletion_and_merges_keep_their_native_programs() {
    let input = rows(vec![int(1), int(1), int(2)]);
    // A constant update cannot change what a later row reads, so it stages
    // once (fgdb-2l3pt); a right-hand side reading the row stays sequential.
    for (tail, frozen) in [
        ("MATCH (n:Entity {id:x}) SET n.p=x", true),
        ("MATCH (n:Entity {id:x}) SET n.p=n.p+1", false),
        ("MATCH (n:Entity {id:x}) SET n:Other", true),
        ("MATCH (n:Entity {id:x}) REMOVE n.name", true),
        ("MATCH (n:Entity {id:x}) DETACH DELETE n", false),
        ("MATCH (n:Entity {id:x}) DELETE n", false),
        (
            "MERGE (n:Entity {id:x}) ON CREATE SET n.p=1 ON MATCH SET n.p=n.p+1",
            false,
        ),
    ] {
        let definition = GraphUnwindWriteText::parse(&format!("UNWIND $rows AS x {tail}")).unwrap();
        let actual = definition.bind(&input, RelationId(1), symbols).unwrap();
        assert_eq!(actual.argument_sets(), 3, "{tail}");
        assert_eq!(actual.program().statements().len(), 3, "{tail}");
        assert_eq!(
            actual.program().freezes_vertex_update_input(),
            frozen,
            "{tail}"
        );
    }
}

#[test]
fn compound_membership_preserves_disjoint_keys_domains_and_native_statements() {
    let input = rows(vec![
        object(vec![("id", int(1)), ("p", int(7))]),
        object(vec![("id", int(2)), ("p", int(8))]),
    ]);
    let frozen = input.canonical_bytes();
    for tail in [
        "MATCH (n:Entity) WHERE n.id=row.id OR n.p=0 SET n.name='changed'",
        "MATCH (n:Entity) WHERE NOT (n.id=row.id) SET n.p=row.p",
        "MATCH (n:Entity) WHERE abs(n.id)=row.id SET n.p=n.p+row.p",
        "MATCH (n:Entity) WHERE coalesce(n.id,0)=row.id SET n += {p:row.p}",
        "MATCH (n:Entity) WHERE n.id IN [row.id,NULL] SET n.name='changed'",
        "MATCH (n:Entity) WHERE n.id=row.id OR n.id IS NULL SET n:Other",
        "MATCH (n:Entity) WHERE n.id=row.id OR n.id IS NULL REMOVE n.p",
        "MATCH (n:Entity) WHERE id(n)=row.id SET n.id=row.p",
        "MATCH (a:Entity)-[e:R]->(b) WHERE id(e)=row.id SET e.id=row.p",
        "MATCH (a:Entity)-[e:R]->(b) WHERE e.id=row.id SET a.id=row.p",
        "MATCH (a:Entity)-[e:R {id:row.id}]->(b) SET a.id=row.p",
        "MATCH (a:Entity)-[e:R]->(b) WHERE e.id IS NULL SET a.id=row.p",
        "MATCH (a:Entity {id:row.id})-[e:R]->(b) SET e.id=row.p",
        "MATCH (a:Entity {id:row.id})-[e:R]->(b) SET e += {id:row.p}",
        "MATCH (a:Entity) OPTIONAL MATCH (a)-[e:R]->(b) WHERE coalesce(e.id,0)=row.id SET a.id=row.p",
        "MATCH (a:Entity) WHERE EXISTS { MATCH (a)-[:R]->(b) WHERE b.id=row.id OR b.p=0 } SET a.name='changed'",
        "MATCH (a:Entity) WHERE NOT EXISTS { MATCH (a)-[e:R]->(b) WHERE e.id=row.id } SET a.id=row.p",
    ] {
        let text = format!("UNWIND $rows AS row {tail}");
        let definition = GraphUnwindWriteText::parse(&text).unwrap();
        let actual = definition
            .bind(&input, RelationId(1), symbols)
            .unwrap_or_else(|error| panic!("{tail}: {error:?}"));
        assert_eq!(actual.argument_sets(), 2, "{tail}");

        // The accepted adapter must retain the ordinary native statements,
        // including each row's values, rather than weakening its predicates.
        let native_text = tail.replace("row.id", "$id").replace("row.p", "$p");
        let declarations: Vec<_> = [("id", "row.id"), ("p", "row.p")]
            .into_iter()
            .filter(|(_, field)| tail.contains(*field))
            .map(|(name, _)| (name, GqlParameterType::Scalar(CanonicalScalarKind::Int)))
            .collect();
        let native = PreparedGraphWriteScript::prepare_with_parameter_types(
            &native_text,
            RelationId(1),
            &declarations,
            symbols,
        )
        .unwrap();
        for (row, (id, p)) in [(1, 7), (2, 8)].into_iter().enumerate() {
            let mut arguments = GqlParameters::new();
            for (name, value) in [("id", id), ("p", p)] {
                if declarations.iter().any(|(declared, _)| *declared == name) {
                    arguments = arguments
                        .with_scalar(name, CanonicalScalar::Int(value))
                        .unwrap();
                }
            }
            let expected = native.bind_parameters(&arguments).unwrap();
            assert_eq!(
                &actual.program().statements()[row],
                &expected.statements()[0],
                "{tail}, row {row}"
            );
        }
        assert_eq!(input.canonical_bytes(), frozen);
    }
}

#[test]
fn nullable_first_rows_never_hide_later_boolean_property_dependencies() {
    let input = rows(vec![
        object(vec![("id", null()), ("p", int(2))]),
        object(vec![("id", int(2)), ("p", int(3))]),
    ]);
    let frozen = input.canonical_bytes();
    for tail in [
        "MATCH (n:Entity) WHERE n.id=row.id OR n.p=0 SET n.id=row.p",
        "MATCH (a:Entity) WHERE EXISTS { MATCH (a)-[:R]->(b) WHERE coalesce(b.id,0)=row.id } SET a.id=row.p",
    ] {
        let definition =
            GraphUnwindWriteText::parse(&format!("UNWIND $rows AS row {tail}")).unwrap();
        assert!(
            definition
                .bind(&input, RelationId(1), symbols)
                .unwrap()
                .program()
                .freezes_vertex_update_input()
        );
    }
    let edge = GraphUnwindWriteText::parse(
        "UNWIND $rows AS row MATCH (a:Entity)-[e:R {id:row.id}]->(b) SET e.id=row.p",
    )
    .unwrap();
    assert!(matches!(
        edge.bind(&input, RelationId(1), symbols),
        Err(GraphUnwindWriteError::RowFeedsLaterMatch { statement: 0 })
    ));
    let disjoint = GraphUnwindWriteText::parse(
        "UNWIND $rows AS row MATCH (n:Entity) WHERE n.id=row.id OR n.id IS NULL SET n.p=row.p",
    )
    .unwrap();
    assert_eq!(
        disjoint
            .bind(&input, RelationId(1), symbols)
            .unwrap()
            .argument_sets(),
        2
    );
    for tail in [
        "MATCH (n:Entity {id:row.id}) SET n += row.p",
        "MATCH (n:Entity {id:row.id}) SET n = row.p",
    ] {
        if let Ok(definition) = GraphUnwindWriteText::parse(&format!("UNWIND $rows AS row {tail}"))
        {
            assert!(definition.bind(&input, RelationId(1), symbols).is_err());
        }
    }
    assert_eq!(input.canonical_bytes(), frozen);
}

#[test]
fn selection_feedback_uses_final_expansion_count_and_preserves_definition_control() {
    let definition = GraphUnwindWriteText::parse(
        "UNWIND $rows AS row UNWIND row.ids AS x MATCH (n:Entity {id:x}) SET n.id=row.p",
    )
    .unwrap();
    let input = rows(vec![object(vec![
        ("ids", list(vec![int(1), int(2)])),
        ("p", int(3)),
    ])]);
    let expanded = definition.bind(&input, RelationId(1), symbols).unwrap();
    assert_eq!(expanded.argument_sets(), 2);
    assert!(expanded.program().freezes_vertex_update_input());
    let refused = definition.bind_with_limit_controlled(
        &input,
        RelationId(1),
        3,
        symbols,
        |event| match event {
            GraphUnwindBindEvent::Definition(_) => Err("definition refused"),
            GraphUnwindBindEvent::Work(_) => Ok(()),
        },
    );
    assert!(matches!(
        refused,
        Err(GraphUnwindBindError::Interrupted("definition refused"))
    ));
    let contracted = rows(vec![
        object(vec![("ids", list(vec![int(1)])), ("p", int(3))]),
        object(vec![("ids", list(vec![])), ("p", int(4))]),
    ]);
    let contracted = definition
        .bind(&contracted, RelationId(1), symbols)
        .unwrap();
    assert_eq!(contracted.argument_sets(), 1);
    assert!(!contracted.program().freezes_vertex_update_input());
}

#[test]
fn direct_scalar_aliases_lower_to_the_same_native_statements_without_parameter_capture() {
    for alias in ["x", "id", "value", "`x`"] {
        let text = format!(
            "UNWIND $rows AS {alias} MATCH (n:Entity {{id:{alias}}}) \
            SET n.p={alias}+$aa"
        );
        let plan = GraphUnwindWriteText::parse(&text).unwrap();
        assert_eq!(
            plan.fields.len(),
            1,
            "repeated bare aliases share one operand"
        );
        assert!(plan.fields[0].path.is_empty());
        assert_ne!(plan.fields[0].parameter, "aa");
        let args = rows(vec![int(-9), int(0), int(17)])
            .with_int64("aa", 2)
            .unwrap();
        let frozen = args.canonical_bytes();
        let actual = plan.bind(&args, RelationId(1), symbols).unwrap();
        let reference = PreparedGraphWriteScript::prepare_with_parameter_types(
            "MATCH (n:Entity {id:$value}) SET n.p=$value+$aa",
            RelationId(1),
            &[("value", GqlParameterType::Scalar(CanonicalScalarKind::Int))],
            symbols,
        )
        .unwrap();
        for (at, value) in [-9, 0, 17].into_iter().enumerate() {
            let args = GqlParameters::new()
                .with_scalar("value", CanonicalScalar::Int(value))
                .unwrap()
                .with_int64("aa", 2)
                .unwrap();
            let expected = reference.bind_parameters(&args).unwrap();
            assert_eq!(
                &actual.program().statements()[at],
                &expected.statements()[0]
            );
            let location = actual.location(at).unwrap();
            assert_eq!(location.argument_set, at);
            assert_eq!(location.statement, 0);
            assert_eq!(location.span, 0..text.len());
        }
        assert_eq!(args.canonical_bytes(), frozen);
    }
}

#[test]
fn nested_scalar_arrays_and_list_aliases_flatten_by_reference_with_nulls_and_duplicates() {
    let text = "UNWIND $rows AS m UNWIND m AS x MATCH (n:Entity) SET n.p=x";
    let plan = GraphUnwindWriteText::parse(text).unwrap();
    assert!(plan.sources[0].path.is_empty());
    let values = vec![
        list(vec![int(4), null(), int(4)]),
        list(vec![]),
        null(),
        list(vec![int(9)]),
    ];
    let expanded = expansion::expand(&plan, &values, 4, &mut |_| Ok::<_, ()>(())).unwrap();
    assert_eq!(expanded.len(), 4);
    let expected = [
        Some(CanonicalScalar::Int(4)),
        Some(CanonicalScalar::Null),
        Some(CanonicalScalar::Int(4)),
        Some(CanonicalScalar::Int(9)),
    ];
    for (at, expected) in expected.iter().enumerate() {
        let input = expanded.at(at, 1);
        let scalar = scalar_field(input, &plan.fields[0], at, &mut |_| Ok::<_, ()>(())).unwrap();
        assert_eq!(scalar, expected.as_ref());
        if let (GraphValue::Scalar(original), Some(selected)) = (input, scalar) {
            assert!(
                core::ptr::eq(original, selected),
                "selection must borrow, not clone a container"
            );
        }
    }
    let args = rows(values);
    assert_eq!(
        plan.bind_with_limit(&args, RelationId(1), 4, symbols)
            .unwrap()
            .argument_sets(),
        4
    );
    let parent = GraphUnwindWriteText::parse(
        "UNWIND $rows AS p UNWIND p.tags AS t \
        MERGE (n:Entity {id:p.id}) SET n.name=t",
    )
    .unwrap();
    let payload = CanonicalScalar::ucs_basic_text("quote'; DELETE n; // é\n").unwrap();
    let args = rows(vec![object(vec![
        ("id", int(1)),
        (
            "tags",
            list(vec![
                null(),
                GraphValue::Scalar(payload),
                GraphValue::Scalar(CanonicalScalar::ucs_basic_text("end").unwrap()),
            ]),
        ),
    ])]);
    assert_eq!(
        parent
            .bind(&args, RelationId(1), symbols)
            .unwrap()
            .argument_sets(),
        3
    );
    // Array items can also be selected directly, without another UNWIND.
    let indexed = GraphUnwindWriteText::parse(
        "UNWIND $rows AS row \
        MATCH (n:Entity) SET n.p=row[-1]",
    )
    .unwrap();
    assert_eq!(
        indexed
            .bind(
                &rows(vec![list(vec![int(1), int(7)]), list(vec![])]),
                RelationId(1),
                symbols
            )
            .unwrap()
            .argument_sets(),
        2
    );
}

#[test]
fn scalar_types_containers_and_each_expansion_boundary_are_admitted_before_catalog() {
    let plan = GraphUnwindWriteText::parse("UNWIND $rows AS x MATCH (n:Entity) SET n.p=x").unwrap();
    for (values, kind) in [
        (
            vec![int(1), GraphValue::Scalar(CanonicalScalar::Bool(true))],
            GraphUnwindRowError::IncompatibleFieldTypes,
        ),
        (
            vec![int(1), object(vec![("p", int(2))])],
            GraphUnwindRowError::ExpectedScalarField,
        ),
        (
            vec![int(1), list(vec![int(2)])],
            GraphUnwindRowError::ExpectedScalarField,
        ),
    ] {
        let args = rows(values);
        let frozen = args.canonical_bytes();
        assert!(
            matches!(plan.bind(&args, RelationId(1), |_, _| panic!("catalog")),
            Err(GraphUnwindWriteError::Row { row: 1, kind: found, .. }) if found == kind)
        );
        assert_eq!(args.canonical_bytes(), frozen);
    }
    let nested = GraphUnwindWriteText::parse(
        "UNWIND $rows AS m UNWIND m AS x \
        MATCH (n:Entity) SET n.p=x",
    )
    .unwrap();
    assert!(matches!(
        nested.bind(
            &rows(vec![list(vec![int(1)]), int(2)]),
            RelationId(1),
            |_, _| panic!("catalog")
        ),
        Err(GraphUnwindWriteError::Expansion {
            row: 1,
            clause: 1,
            kind: GraphUnwindRowError::ExpectedListField,
            ..
        })
    ));
    let intermediate = GraphUnwindWriteText::parse(
        "UNWIND $rows AS m UNWIND m AS x \
        UNWIND x AS y MATCH (n:Entity) SET n.p=y",
    )
    .unwrap();
    let args = rows(vec![list(vec![list(vec![]), list(vec![]), list(vec![])])]);
    assert!(matches!(
        intermediate.bind_with_limit(&args, RelationId(1), 2, |_, _| panic!("catalog")),
        Err(GraphUnwindWriteError::TooManyRows {
            limit: 2,
            observed: 3
        })
    ));
    assert!(matches!(
        intermediate.bind_with_limit(&args, RelationId(1), 3, |_, _| panic!("catalog")),
        Err(GraphUnwindWriteError::Empty)
    ));
}

#[test]
fn short_alias_source_maps_restore_native_errors_spans_and_every_unchanged_byte() {
    let text = "UNWIND /* é */ $rows AS x\nMATCH (n:Entity {id:x}) \
        SET n.p=x,n.missing=x,n.name='x; $aa é'";
    let plan = GraphUnwindWriteText::parse(text).unwrap();
    assert_eq!(plan.source_offsets.len(), 3);
    assert_eq!(plan.original_offset(plan.lowered.len()), text.len());
    let mut generated = 0;
    let mut original = 0;
    for edit in &plan.source_offsets {
        while generated < edit.generated.start {
            assert_eq!(plan.original_offset(generated), original);
            generated += 1;
            original += 1;
        }
        assert_eq!(original, edit.original.start);
        for at in edit.generated.clone() {
            assert_eq!(plan.original_offset(at), edit.original.start);
        }
        generated = edit.generated.end;
        original = edit.original.end;
    }
    while generated <= plan.lowered.len() {
        assert_eq!(plan.original_offset(generated), original);
        generated += 1;
        original += 1;
    }
    assert!(
        plan.lowered.contains("'x; $aa é'"),
        "literal bytes are not rewritten"
    );
    let error = plan
        .bind(&rows(vec![int(1)]), RelationId(1), symbols)
        .unwrap_err();
    let GraphUnwindWriteError::Definition(error) = error else {
        panic!("{error}")
    };
    assert_eq!(error.offset, text.find("missing").unwrap());

    // Restore diagnostics inside a generated token, including its first byte.
    for edit in &plan.source_offsets {
        for generated in edit.generated.clone() {
            let mut error = GraphWriteScriptError {
                statement: Some(0),
                offset: generated,
                kind: GraphWriteScriptErrorKind::Syntax(GraphPatternTextErrorKind::Expected(
                    "operand",
                )),
            };
            plan.restore_script_offsets(&mut error);
            assert_eq!(error.offset, edit.original.start);
        }
    }
    let semicolon = "UNWIND $rows AS x MERGE (n:Entity {id:x}) SET n.p=x; // é";
    let plan = GraphUnwindWriteText::parse(semicolon).unwrap();
    let batch = plan
        .bind(&rows(vec![int(3)]), RelationId(1), symbols)
        .unwrap();
    assert_eq!(
        batch.location(0).unwrap().span,
        0..semicolon.find(';').unwrap()
    );
}

#[test]
fn every_scalar_lookup_binding_boundary_and_exact_work_allowance_can_refuse() {
    let plan = GraphUnwindWriteText::parse(
        "UNWIND $rows AS m UNWIND m AS x \
        MATCH (n:Entity {id:x}) SET n.p=x",
    )
    .unwrap();
    let args = rows(vec![list(vec![int(2), int(5), int(2)])]);
    let frozen = args.canonical_bytes();
    let (mut events, mut total) = (0usize, 0u64);
    let expected = plan
        .bind_with_limit_controlled(&args, RelationId(1), 3, symbols, |event| {
            events += 1;
            if let GraphUnwindBindEvent::Work(units) = event {
                total += units;
            }
            Ok::<_, usize>(())
        })
        .unwrap();
    for stop in 0..events {
        let mut at = 0;
        let result = plan.bind_with_limit_controlled(&args, RelationId(1), 3, symbols, |_| {
            let current = at;
            at += 1;
            if current == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GraphUnwindBindError::Interrupted(found)) if found == stop));
        assert_eq!(args.canonical_bytes(), frozen);
    }
    for allowance in [total, total - 1] {
        let mut spent = 0;
        let result = plan.bind_with_limit_controlled(&args, RelationId(1), 3, symbols, |event| {
            if let GraphUnwindBindEvent::Work(units) = event {
                spent += units;
            }
            if spent > allowance { Err(()) } else { Ok(()) }
        });
        assert_eq!(result.is_ok(), allowance == total);
    }
    let retried = plan
        .bind_with_limit(&args, RelationId(1), 3, symbols)
        .unwrap();
    assert_eq!(retried.program(), expected.program());
}

#[test]
fn scalar_aliases_cannot_capture_graph_bindings_or_intercept_native_create_pipelines() {
    for text in [
        "UNWIND $rows AS x MATCH (x) SET x.p=1",
        "UNWIND $rows AS x MERGE (x:Entity {id:1})",
        "UNWIND $rows AS x MATCH (n) SET n.p=x[$index]",
        "UNWIND $rows AS x UNWIND x AS x MATCH (n) SET n.p=1",
    ] {
        let refused = GraphUnwindWriteText::parse(text).and_then(|plan| {
            plan.bind(&rows(vec![int(3)]), RelationId(1), |_, _| panic!("catalog"))
        });
        assert!(refused.is_err(), "{text}");
    }
    for text in [
        "UNWIND $rows AS x CREATE (n {id:x})",
        "UNWIND $rows AS x UNWIND x AS y CREATE (n {id:y}) RETURN n",
        "UNWIND $rows AS SET MATCH (n) WHERE n.p=SET CREATE (m {id:SET})",
        "UNWIND $rows AS row MATCH (SET) WHERE SET.id=row.id CREATE (m {id:row.id})",
        "UNWIND $rows AS row MATCH (n) WHERE n.SET={id:row.id} CREATE (m {id:row.id})",
        "UNWIND $rows AS MERGE MATCH (n) WHERE n.p=MERGE INSERT (m {id:MERGE})",
    ] {
        assert!(
            GraphUnwindWriteText::parse_if_supported(text)
                .unwrap()
                .is_none(),
            "{text}"
        );
    }
}

#[test]
fn distinct_key_vertex_upserts_freeze_and_every_other_upsert_batch_stays_sequential() {
    // fgdb-2l3pt: a batch whose rows MERGE pairwise distinct keys and write
    // only key-free constants is staged once; anything a later row's MERGE
    // could observe keeps the sequential program.
    let distinct = rows(vec![
        object(vec![("id", int(1)), ("p", int(10))]),
        object(vec![("id", int(2)), ("p", int(20))]),
        object(vec![("id", int(3)), ("p", int(30))]),
    ]);
    let duplicate = rows(vec![
        object(vec![("id", int(1)), ("p", int(10))]),
        object(vec![("id", int(1)), ("p", int(20))]),
    ]);
    let single = rows(vec![object(vec![("id", int(1)), ("p", int(10))])]);
    let bind = |tail: &str, input: &GqlParameters| {
        GraphUnwindWriteText::parse(&format!("UNWIND $rows AS row {tail}"))
            .unwrap()
            .bind(input, RelationId(1), symbols)
            .unwrap_or_else(|error| panic!("{tail}: {error:?}"))
    };
    for tail in [
        "MERGE (n:Entity {id: row.id}) SET n.p = row.p",
        "MERGE (n:Entity {id: row.id}) ON CREATE SET n.p = row.p ON MATCH SET n.name = 'seen'",
        "MERGE (n:Entity {id: row.id}) SET n:Other",
    ] {
        let bound = bind(tail, &distinct);
        assert!(bound.program().freezes_vertex_upsert_input(), "{tail}");
        assert!(!bound.program().freezes_vertex_update_input(), "{tail}");
        // Two rows with one key: the second must see the first's creation.
        assert!(
            !bind(tail, &duplicate)
                .program()
                .freezes_vertex_upsert_input(),
            "{tail}"
        );
        assert!(
            !bind(tail, &single).program().freezes_vertex_upsert_input(),
            "{tail}"
        );
    }
    for tail in [
        // Writes a key property, so a later row's MERGE could match it.
        "MERGE (n:Entity {id: row.id}) SET n.id = row.p",
        // Writes a key label.
        "MERGE (n:Entity {id: row.id}) SET n:Entity",
        // Reads the target's own state.
        "MERGE (n:Entity {id: row.id}) SET n.p = n.p + 1",
    ] {
        assert!(
            !bind(tail, &distinct)
                .program()
                .freezes_vertex_upsert_input(),
            "{tail}"
        );
    }
}

#[test]
fn constant_vertex_updates_freeze_and_row_reading_updates_stay_sequential() {
    // fgdb-2l3pt: an update that writes only bound constants to fields its
    // MATCH does not read stages once. Before, every row re-walked each
    // earlier row's staged update, and the CLI's work allowance refused a
    // 2000-row `MATCH ... SET n.p = row.p` batch at row 1678.
    let distinct = rows(vec![
        object(vec![("id", int(1)), ("p", int(10))]),
        object(vec![("id", int(2)), ("p", int(20))]),
        object(vec![("id", int(3)), ("p", int(30))]),
    ]);
    let duplicate = rows(vec![
        object(vec![("id", int(1)), ("p", int(10))]),
        object(vec![("id", int(1)), ("p", int(20))]),
    ]);
    let single = rows(vec![object(vec![("id", int(1)), ("p", int(10))])]);
    let bind = |tail: &str, input: &GqlParameters| {
        GraphUnwindWriteText::parse(&format!("UNWIND $rows AS row {tail}"))
            .unwrap()
            .bind(input, RelationId(1), symbols)
            .unwrap_or_else(|error| panic!("{tail}: {error:?}"))
    };
    for tail in [
        "MATCH (n:Entity {id: row.id}) SET n.p = row.p",
        "MATCH (n:Entity) WHERE n.id = row.id SET n.p = row.p, n.name = 'seen'",
        "MATCH (n:Entity {id: row.id}) SET n:Other",
        "MATCH (n:Entity {id: row.id}) REMOVE n.name",
    ] {
        // Ordered staging keeps a repeated target's last write, as the
        // sequential program does.
        for input in [&distinct, &duplicate] {
            let bound = bind(tail, input);
            assert!(bound.program().freezes_vertex_update_input(), "{tail}");
            assert!(!bound.program().freezes_vertex_upsert_input(), "{tail}");
        }
        assert!(
            !bind(tail, &single).program().freezes_vertex_update_input(),
            "{tail}"
        );
    }
    for tail in [
        // The right-hand side reads the target, which an earlier row with the
        // same key has changed.
        "MATCH (n:Entity {id: row.id}) SET n.p = n.p + 1",
        "MATCH (n:Entity {id: row.id}) SET n.p = n.name",
        // A relationship update has another staging contract.
        "MATCH (a:Entity)-[e:R {id: row.id}]->(b) SET e.p = row.p",
    ] {
        let bound = bind(tail, &distinct);
        assert!(!bound.program().freezes_vertex_update_input(), "{tail}");
        assert_eq!(bound.program().statements().len(), 3, "{tail}");
    }
}

#[test]
fn row_maps_assigned_with_plus_equals_lower_to_one_field_per_key() {
    // fgdb-0s2pq: `SET n += row` assigns each input row's map. Rows sharing a
    // key set lower to `SET n += {k1: $f1, ...}`, one scalar field per key.
    let input = rows(vec![
        object(vec![("id", int(1)), ("p", int(10))]),
        object(vec![("id", int(2)), ("p", int(20))]),
    ]);
    let bind = |text: &str, input: &GqlParameters| {
        GraphUnwindWriteText::parse(text)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .bind(input, RelationId(1), symbols)
    };
    for text in [
        "UNWIND $rows AS row MATCH (n:Entity {id: row.id}) SET n += row",
        "UNWIND $rows AS row MERGE (n:Entity {id: row.id}) SET n += row",
        "UNWIND $rows AS r MATCH (n:Entity {id: r.id}) SET n += r, n:Other",
    ] {
        let bound = bind(text, &input).unwrap_or_else(|error| panic!("{text}: {error:?}"));
        assert_eq!(bound.argument_sets(), 2, "{text}");
        assert_eq!(bound.program().statements().len(), 2, "{text}");
    }
    let nested = rows(vec![object(vec![
        ("id", int(1)),
        ("props", object(vec![("p", int(5)), ("name", int(6))])),
    ])]);
    bind(
        "UNWIND $rows AS row MATCH (n:Entity {id: row.id}) SET n += row.props",
        &nested,
    )
    .unwrap();

    // A second key set would make a missing key assign NULL.
    let differing = rows(vec![
        object(vec![("id", int(1)), ("p", int(10))]),
        object(vec![("id", int(2)), ("name", int(20))]),
    ]);
    let text = "UNWIND $rows AS row MATCH (n:Entity {id: row.id}) SET n += row";
    let refused = bind(text, &differing).unwrap_err();
    assert!(
        matches!(
            refused,
            GraphUnwindWriteError::Row {
                row: 1,
                offset,
                kind: GraphUnwindRowError::MapKeysDiffer,
            } if offset == text.rfind("+= ").unwrap() + 3
        ),
        "{refused:?}"
    );
    let spaced = rows(vec![object(vec![("id", int(1)), ("a b", int(10))])]);
    assert!(matches!(
        bind(text, &spaced),
        Err(GraphUnwindWriteError::Row {
            kind: GraphUnwindRowError::UnsupportedMapKey,
            ..
        })
    ));
    // A map in a scalar position keeps the scalar-field refusal.
    assert!(matches!(
        bind(
            "UNWIND $rows AS row MATCH (n:Entity {id: row.id}) SET n.p = row",
            &input
        ),
        Err(GraphUnwindWriteError::Row {
            kind: GraphUnwindRowError::ExpectedScalarField,
            ..
        })
    ));

    // Diagnostics inside the spliced map point at the original alias, and
    // later bytes keep their own coordinates. `zz` is not in the catalog.
    let unknown = rows(vec![object(vec![("id", int(1)), ("zz", int(10))])]);
    for text in [
        "UNWIND $rows AS row MATCH (n:Entity {id: row.id}) SET n += row",
        "UNWIND $rows AS r MATCH (n:Entity {id: r.id}) SET n += r",
    ] {
        let GraphUnwindWriteError::Definition(error) = bind(text, &unknown).unwrap_err() else {
            panic!("{text}")
        };
        assert_eq!(error.offset, text.rfind("+= ").unwrap() + 3, "{text}");
    }
    let GraphUnwindWriteError::Definition(error) = bind(
        "UNWIND $rows AS r MATCH (n:Entity {id: r.id}) SET n += r, n:Missing",
        &input,
    )
    .unwrap_err() else {
        panic!("an unknown label refuses at preparation")
    };
    assert_eq!(
        error.offset,
        "UNWIND $rows AS r MATCH (n:Entity {id: r.id}) SET n += r, n:Missing"
            .find("Missing")
            .unwrap()
    );
}
