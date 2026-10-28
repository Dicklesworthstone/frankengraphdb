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
        "MATCH (n:Entity) WHERE n.id=row.id SET n.id=n.id+row.p",
        "MATCH (n:Entity) WHERE n.id=row.id SET n.id=row.p, n.p=n.p+1",
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
fn duplicate_vertex_and_relationship_feedback_cannot_freeze_current_property_reads() {
    let input = rows(vec![
        object(vec![("id", int(1)), ("p", int(2))]),
        object(vec![("id", int(1)), ("p", int(3))]),
    ]);
    // Neo4j's EagerPipe buffers CypherRow identities; SetPipe evaluates each
    // row's SetOperation against the current QueryState. Freezing RHS reads
    // would lose an increment when these rows bind the same graph identity.
    for tail in [
        "MATCH (n:Entity {id:row.id}) SET n.id=row.id, n.p=n.p+row.p",
        "MATCH (a)-[e:R {id:row.id}]->(b) SET e.id=row.id, e.p=e.p+row.p",
        "MATCH (a)-[e:R {id:row.id}]->(b) SET e.id=row.id, a.p=a.p+row.p",
        "MATCH (a:Entity {id:row.id})-[e:R]->(b) SET a.id=row.p, e.p=e.p+1",
    ] {
        let definition =
            GraphUnwindWriteText::parse(&format!("UNWIND $rows AS row {tail}")).unwrap();
        assert!(
            matches!(
                definition.bind(&input, RelationId(1), symbols),
                Err(GraphUnwindWriteError::RowFeedsLaterMatch { statement: 0 })
            ),
            "{tail}"
        );
    }
}

#[test]
fn frozen_updates_admit_closed_collection_expressions_but_keep_graph_reads_live() {
    let input = rows(vec![
        object(vec![("id", int(1)), ("p", int(2))]),
        object(vec![("id", int(2)), ("p", int(3))]),
    ]);
    for expression in [
        "[row.id, row.p]",
        "reverse([row.id, row.p])",
        "[x IN [row.id, row.p] WHERE x > 0 | x + 1]",
        "[x IN [row.id] | [y IN [row.p] | x + y]]",
        "[reduce(s = 0, x IN [row.id, row.p] | s + x)]",
        "[{a: row.id, b: row.p}.a, row.p]",
        "CASE WHEN row.id > 0 THEN [row.p] ELSE [1 / 0] END",
        "range(row.id, row.p)",
        "[row.id, row.p][0..1]",
    ] {
        for (pattern, actions, vertex, edge) in [
            ("(n:Entity {id:row.id})", "n.id=row.p, n.p", true, false),
            ("(a)-[e:R {id:row.id}]->(b)", "e.id=row.p, e.p", false, true),
            (
                "(a:Entity {id:row.id})-[e:R]->(b)",
                "a.id=row.p, e.p",
                false,
                false,
            ),
        ] {
            let text = format!("UNWIND $rows AS row MATCH {pattern} SET {actions}={expression}");
            let bound = GraphUnwindWriteText::parse(&text)
                .unwrap()
                .bind(&input, RelationId(1), symbols)
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            assert!(bound.program().freezes_field_update_input(), "{text}");
            assert_eq!(
                bound.program().freezes_vertex_update_input(),
                vertex,
                "{text}"
            );
            assert_eq!(bound.program().freezes_edge_update_input(), edge, "{text}");
            assert_eq!(
                bound.program().freezes_mixed_update_input(),
                !vertex && !edge,
                "{text}"
            );
        }
    }

    for expression in [
        "[n.p, row.p]",
        "[x IN [row.id] | n.p + x]",
        "[x IN [n] | x.p]",
        "[reduce(s = 0, x IN [row.id] | s + n.p)]",
        "CASE WHEN row.id > 0 THEN [row.p] ELSE [n.p] END",
        "[rand()]",
    ] {
        let feedback = format!(
            "UNWIND $rows AS row MATCH (n:Entity {{id:row.id}}) SET n.id=row.p, n.p={expression}"
        );
        let result =
            GraphUnwindWriteText::parse(&feedback)
                .unwrap()
                .bind(&input, RelationId(1), symbols);
        assert!(
            matches!(
                result,
                Err(GraphUnwindWriteError::RowFeedsLaterMatch { statement: 0 })
            ),
            "{feedback}: {result:?}"
        );
    }

    // The same collections are batchable without feedback; a collection that
    // reads the current target must stay on the ordinary sequential path.
    for (expression, frozen) in [("[row.p, row.id]", true), ("[n.p, row.p]", false)] {
        let text =
            format!("UNWIND $rows AS row MATCH (n:Entity {{id:row.id}}) SET n.p={expression}");
        let bound = GraphUnwindWriteText::parse(&text)
            .unwrap()
            .bind(&input, RelationId(1), symbols)
            .unwrap();
        assert_eq!(
            bound.program().freezes_vertex_update_input(),
            frozen,
            "{text}"
        );
    }
}

#[test]
fn mixed_field_updates_freeze_feedback_and_preserve_their_own_provenance() {
    let input = rows(vec![
        object(vec![("id", int(1)), ("p", int(2))]),
        object(vec![("id", int(2)), ("p", int(3))]),
    ]);
    let single = rows(vec![object(vec![("id", int(1)), ("p", int(2))])]);
    for tail in [
        "MATCH (a:Entity)-[e:R]->(b) WHERE e.id=row.id SET e.id=row.p, a.id=row.p",
        "MATCH (a:Entity)-[e:R]->(b) WHERE e.id=row.id SET e.id=row.p, a:Other",
        "MATCH (a:Entity {id:row.id})-[e:R]->(b) SET a.id=row.p, e.p=row.p+1",
        "MATCH (a:Entity {id:row.id}) OPTIONAL MATCH (a)-[e:R]->(b) SET a.id=row.p, e.p=1/0",
        "MATCH (a:Entity)-[e:R {id:row.id}]->(b) REMOVE a:Entity,e.id",
        "MATCH (a:Entity {id:row.id})-[e:R]->(b) REMOVE a.id,e.p",
    ] {
        let definition =
            GraphUnwindWriteText::parse(&format!("UNWIND $rows AS row {tail}")).unwrap();
        let bound = definition
            .bind(&input, RelationId(1), symbols)
            .unwrap_or_else(|error| panic!("{tail}: {error:?}"));
        assert!(bound.program().freezes_mixed_update_input(), "{tail}");
        assert!(bound.program().freezes_field_update_input(), "{tail}");
        assert!(!bound.program().freezes_vertex_update_input(), "{tail}");
        assert!(!bound.program().freezes_edge_update_input(), "{tail}");
        let ordinary =
            crate::PreparedGraphWriteProgram::prepare(bound.program().statements().to_vec())
                .unwrap();
        assert!(!ordinary.freezes_field_update_input());
        assert_eq!(ordinary.statements(), bound.program().statements());
        assert_ne!(
            ordinary.canonical_bytes(),
            bound.program().canonical_bytes()
        );
        assert!(
            bound
                .program()
                .canonical_bytes()
                .starts_with(b"fgdb:mixed-write-program:frozen-unwind-mixed-updates:v1\0")
        );
        assert!(bound.clone().into_program().freezes_mixed_update_input());
        assert!(
            bound
                .location(1)
                .is_some_and(|location| location.argument_set == 1)
        );
        assert!(
            !definition
                .bind(&single, RelationId(1), symbols)
                .unwrap()
                .program()
                .freezes_field_update_input()
        );
    }
}

#[test]
fn relationship_updates_freeze_match_feedback_and_keep_distinct_provenance() {
    let input = rows(vec![
        object(vec![("id", int(1)), ("p", int(2))]),
        object(vec![("id", int(2)), ("p", int(3))]),
    ]);
    let single = rows(vec![object(vec![("id", int(1)), ("p", int(2))])]);
    let original = input.canonical_bytes();
    for tail in [
        "MATCH (a:Entity)-[e:R]->(b) WHERE e.id=row.id SET e.id=row.p",
        "MATCH (a:Entity)-[e:R]->(b) WHERE e.id=row.id SET e.id=row.p+1",
        "MATCH (a:Entity)-[e:R]->(b) WHERE e.id=row.id SET e.id=CASE WHEN row.p=0 THEN 0 ELSE 10/row.p END",
        "MATCH (a:Entity)-[e:R {id:row.id}]->(b) SET e += {id:row.p}",
        "MATCH (a:Entity)-[e:R]->(b) WHERE e.id IS NULL SET e.id=row.p",
        "MATCH (a:Entity)-[e:R]->(b) WHERE abs(e.id)=row.id SET e.id=row.p",
        "MATCH (a:Entity)-[e:R]->(b) WHERE e.id=a.p REMOVE e.id",
        "MATCH (a:Entity)-[e:R]->(b) WHERE e.id=row.id OR e.id IS NULL SET e.id=row.p",
        "MATCH (a:Entity)-[e:R]-(b) WHERE e.id=row.id SET e.id=row.p",
        "MATCH (a:Entity)-[e:R]->(b)-[f:R]->(c) WHERE e.id=row.id SET e.id=row.p, f.p=row.p",
    ] {
        let definition =
            GraphUnwindWriteText::parse(&format!("UNWIND $rows AS row {tail}")).unwrap();
        let bound = definition
            .bind(&input, RelationId(1), symbols)
            .unwrap_or_else(|error| panic!("{tail}: {error:?}"));
        assert!(bound.program().freezes_edge_update_input(), "{tail}");
        assert!(!bound.program().freezes_vertex_update_input(), "{tail}");
        assert!(!bound.program().freezes_edge_merge_input(), "{tail}");
        let ordinary =
            crate::PreparedGraphWriteProgram::prepare(bound.program().statements().to_vec())
                .unwrap();
        assert_eq!(ordinary.statements(), bound.program().statements());
        assert_ne!(
            ordinary.canonical_bytes(),
            bound.program().canonical_bytes()
        );
        let singleton = definition.bind(&single, RelationId(1), symbols).unwrap();
        assert!(!singleton.program().freezes_edge_update_input());
    }
    // Eager MATCH selection does not snapshot RHS graph reads across rows.
    // Until those can be evaluated over the current staging image, reject
    // feedback rather than silently turn repeated increments into one write.
    for tail in [
        "MATCH (a:Entity)-[e:R]->(b) WHERE e.id=row.id SET e.id=e.id+row.p",
        "MATCH (a:Entity)-[e:R]->(b) WHERE e.id=row.id SET e.id=row.p, e.p=e.id+10",
        "MATCH (a:Entity)-[e:R]->(b) WHERE e.id=row.id SET e.id=row.p, e.p=coalesce(e.p,0)+1",
    ] {
        let definition =
            GraphUnwindWriteText::parse(&format!("UNWIND $rows AS row {tail}")).unwrap();
        assert!(
            matches!(
                definition.bind(&input, RelationId(1), symbols),
                Err(GraphUnwindWriteError::RowFeedsLaterMatch { statement: 0 })
            ),
            "{tail}"
        );
        let singleton = definition.bind(&single, RelationId(1), symbols).unwrap();
        assert!(!singleton.program().freezes_edge_update_input());
    }
    assert_eq!(input.canonical_bytes(), original);
}

#[test]
fn constant_relationship_batches_freeze_but_row_reading_and_mixed_updates_do_not() {
    let input = rows(vec![int(1), int(2), int(1)]);
    for (tail, frozen) in [
        ("MATCH (a)-[e:R {id:x}]->(b) SET e.p=x", true),
        ("MATCH (a)-[e:R {id:x}]->(b) REMOVE e.p", true),
        ("MATCH (a)-[e:R {id:x}]->(b) SET e.p=e.p+1", false),
        ("MATCH (a)-[e:R {id:x}]->(b) SET e.p=x, a.p=x", false),
    ] {
        let definition = GraphUnwindWriteText::parse(&format!("UNWIND $rows AS x {tail}")).unwrap();
        let bound = definition
            .bind(&input, RelationId(1), symbols)
            .unwrap_or_else(|error| panic!("{tail}: {error:?}"));
        assert_eq!(
            bound.program().freezes_edge_update_input(),
            frozen,
            "{tail}"
        );
        assert!(!bound.program().freezes_vertex_update_input(), "{tail}");
    }
    let deletion =
        GraphUnwindWriteText::parse("UNWIND $rows AS x MATCH (a)-[e:R {id:x}]->(b) DELETE e")
            .unwrap();
    assert!(matches!(
        deletion.bind(&input, RelationId(1), symbols),
        Err(GraphUnwindWriteError::RowFeedsLaterMatch { statement: 0 })
    ));
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
    assert!(
        edge.bind(&input, RelationId(1), symbols)
            .unwrap()
            .program()
            .freezes_edge_update_input()
    );
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
        // The row map rewrites the key with its own value.
        "MERGE (n:Entity {id: row.id}) SET n += row",
        // A plain MERGE has no actions at all.
        "MERGE (n:Entity {id: row.id})",
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

#[test]
fn distinct_endpoint_edge_merges_freeze_and_every_other_edge_merge_batch_stays_sequential() {
    // fgdb-2l3pt: a directed relationship MERGE between two equality-keyed
    // vertex scans stages once when the endpoint key tuples are pairwise
    // distinct; before, each row re-walked every earlier row's staged edge,
    // and the CLI's work allowance refused a 4000-row batch at row 2196.
    let pairs = |pairs: &[(i64, i64)]| {
        rows(
            pairs
                .iter()
                .map(|(s, d)| object(vec![("s", int(*s)), ("d", int(*d))]))
                .collect(),
        )
    };
    let bind = |tail: &str, input: &GqlParameters| {
        GraphUnwindWriteText::parse(&format!("UNWIND $rows AS row {tail}"))
            .unwrap()
            .bind(input, RelationId(1), symbols)
            .unwrap_or_else(|error| panic!("{tail}: {error:?}"))
    };
    let distinct = pairs(&[(1, 2), (2, 3), (3, 1)]);
    // Directed: a reversed pair is another tuple.
    let swapped = pairs(&[(1, 2), (2, 1)]);
    for tail in [
        "MATCH (a:Entity {id: row.s}), (b:Entity {id: row.d}) MERGE (a)-[:R]->(b)",
        "MATCH (a:Entity {id: row.s}), (b:Entity {id: row.d}) MERGE (a)-[:R {p: row.s}]->(b)",
        "MATCH (a:Entity {id: row.s}), (b:Entity {id: row.d}) MERGE (b)<-[:R]-(a)",
    ] {
        for input in [&distinct, &swapped] {
            let bound = bind(tail, input);
            assert!(bound.program().freezes_edge_merge_input(), "{tail}");
            assert!(!bound.program().freezes_vertex_upsert_input(), "{tail}");
        }
        // A repeated pair must see the first row's relationship.
        assert!(
            !bind(tail, &pairs(&[(1, 2), (1, 2)]))
                .program()
                .freezes_edge_merge_input(),
            "{tail}"
        );
        assert!(
            !bind(tail, &pairs(&[(1, 2)]))
                .program()
                .freezes_edge_merge_input(),
            "{tail}"
        );
    }
    for tail in [
        // Undirected: (1, 2) and (2, 1) name the same relationship.
        "MATCH (a:Entity {id: row.s}), (b:Entity {id: row.d}) MERGE (a)-[:R]-(b)",
        // The endpoint selection reads relationships another row may create.
        "MATCH (a:Entity {id: row.s})-[:R]->(c), (b:Entity {id: row.d}) MERGE (a)-[:R]->(b)",
        // A range is not an equality key.
        "MATCH (a:Entity), (b:Entity {id: row.d}) WHERE a.id > row.s MERGE (a)-[:R]->(b)",
    ] {
        // A shape the adapter refuses stages nothing either.
        let bound = GraphUnwindWriteText::parse(&format!("UNWIND $rows AS row {tail}"))
            .ok()
            .and_then(|definition| definition.bind(&distinct, RelationId(1), symbols).ok());
        assert!(
            bound.is_none_or(|bound| !bound.program().freezes_edge_merge_input()),
            "{tail}"
        );
    }
}
