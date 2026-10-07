//! Scalar UNWIND operands remain typed native arguments, including aliases
//! shorter than a parameter token and values containing query-shaped text.

use super::*;
use crate::{GqlParameterType, GraphPatternTextErrorKind, GraphWriteScriptErrorKind};
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_types::CanonicalScalarKind;

fn int(value: i64) -> GraphValue { GraphValue::Scalar(CanonicalScalar::Int(value)) }
fn null() -> GraphValue { GraphValue::Scalar(CanonicalScalar::Null) }
fn list(values: Vec<GraphValue>) -> GraphValue { GraphValue::List(values.into_boxed_slice()) }
fn object(fields: Vec<(&str, GraphValue)>) -> GraphValue {
    GraphValue::map(fields.into_iter().map(|(key, value)| (key.into(), value)).collect()).unwrap()
}
fn rows(values: Vec<GraphValue>) -> GqlParameters {
    GqlParameters::new().with_list("rows", values).unwrap()
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Entity") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(PropertyKeyId(3))),
        _ => None,
    }
}

#[test]
fn direct_scalar_aliases_lower_to_the_same_native_statements_without_parameter_capture() {
    for alias in ["x", "id", "value", "`x`"] {
        let text = format!("UNWIND $rows AS {alias} MATCH (n:Entity {{id:{alias}}}) \
            SET n.p={alias}+$aa");
        let plan = GraphUnwindWriteText::parse(&text).unwrap();
        assert_eq!(plan.fields.len(), 1, "repeated bare aliases share one operand");
        assert!(plan.fields[0].path.is_empty());
        assert_ne!(plan.fields[0].parameter, "aa");
        let args = rows(vec![int(-9), int(0), int(17)]).with_int64("aa", 2).unwrap();
        let frozen = args.canonical_bytes();
        let actual = plan.bind(&args, RelationId(1), symbols).unwrap();
        let reference = PreparedGraphWriteScript::prepare_with_parameter_types(
            "MATCH (n:Entity {id:$value}) SET n.p=$value+$aa", RelationId(1),
            &[("value", GqlParameterType::Scalar(CanonicalScalarKind::Int))], symbols,
        ).unwrap();
        for (at, value) in [-9, 0, 17].into_iter().enumerate() {
            let args = GqlParameters::new().with_scalar("value", CanonicalScalar::Int(value)).unwrap()
                .with_int64("aa", 2).unwrap();
            let expected = reference.bind_parameters(&args).unwrap();
            assert_eq!(&actual.program().statements()[at], &expected.statements()[0]);
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
    let values = vec![list(vec![int(4), null(), int(4)]), list(vec![]), null(), list(vec![int(9)])];
    let expanded = expansion::expand(&plan, &values, 4, &mut |_| Ok::<_, ()>(())).unwrap();
    assert_eq!(expanded.len(), 4);
    let expected = [Some(CanonicalScalar::Int(4)), Some(CanonicalScalar::Null),
        Some(CanonicalScalar::Int(4)), Some(CanonicalScalar::Int(9))];
    for (at, expected) in expected.iter().enumerate() {
        let input = expanded.at(at, 1);
        let scalar = scalar_field(input, &plan.fields[0], at, &mut |_| Ok::<_, ()>(())).unwrap();
        assert_eq!(scalar, expected.as_ref());
        if let (GraphValue::Scalar(original), Some(selected)) = (input, scalar) {
            assert!(core::ptr::eq(original, selected), "selection must borrow, not clone a container");
        }
    }
    let args = rows(values);
    assert_eq!(plan.bind_with_limit(&args, RelationId(1), 4, symbols).unwrap().argument_sets(), 4);
    let parent = GraphUnwindWriteText::parse("UNWIND $rows AS p UNWIND p.tags AS t \
        MERGE (n:Entity {id:p.id}) SET n.name=t").unwrap();
    let payload = CanonicalScalar::ucs_basic_text("quote'; DELETE n; // é\n").unwrap();
    let args = rows(vec![object(vec![("id", int(1)), ("tags", list(vec![null(),
        GraphValue::Scalar(payload), GraphValue::Scalar(CanonicalScalar::ucs_basic_text("end").unwrap())]))])]);
    assert_eq!(parent.bind(&args, RelationId(1), symbols).unwrap().argument_sets(), 3);
    // Array items can also be selected directly, without another UNWIND.
    let indexed = GraphUnwindWriteText::parse("UNWIND $rows AS row \
        MATCH (n:Entity) SET n.p=row[-1]").unwrap();
    assert_eq!(indexed.bind(&rows(vec![list(vec![int(1), int(7)]), list(vec![])]),
        RelationId(1), symbols).unwrap().argument_sets(), 2);
}

#[test]
fn scalar_types_containers_and_each_expansion_boundary_are_admitted_before_catalog() {
    let plan = GraphUnwindWriteText::parse("UNWIND $rows AS x MATCH (n:Entity) SET n.p=x").unwrap();
    for (values, kind) in [
        (vec![int(1), GraphValue::Scalar(CanonicalScalar::Bool(true))], GraphUnwindRowError::IncompatibleFieldTypes),
        (vec![int(1), object(vec![("p", int(2))])], GraphUnwindRowError::ExpectedScalarField),
        (vec![int(1), list(vec![int(2)])], GraphUnwindRowError::ExpectedScalarField),
    ] {
        let args = rows(values);
        let frozen = args.canonical_bytes();
        assert!(matches!(plan.bind(&args, RelationId(1), |_, _| panic!("catalog")),
            Err(GraphUnwindWriteError::Row { row: 1, kind: found, .. }) if found == kind));
        assert_eq!(args.canonical_bytes(), frozen);
    }
    let nested = GraphUnwindWriteText::parse("UNWIND $rows AS m UNWIND m AS x \
        MATCH (n:Entity) SET n.p=x").unwrap();
    assert!(matches!(nested.bind(&rows(vec![list(vec![int(1)]), int(2)]),
        RelationId(1), |_, _| panic!("catalog")),
        Err(GraphUnwindWriteError::Expansion { row: 1, clause: 1,
            kind: GraphUnwindRowError::ExpectedListField, .. })));
    let intermediate = GraphUnwindWriteText::parse("UNWIND $rows AS m UNWIND m AS x \
        UNWIND x AS y MATCH (n:Entity) SET n.p=y").unwrap();
    let args = rows(vec![list(vec![list(vec![]), list(vec![]), list(vec![])])]);
    assert!(matches!(intermediate.bind_with_limit(&args, RelationId(1), 2, |_, _| panic!("catalog")),
        Err(GraphUnwindWriteError::TooManyRows { limit: 2, observed: 3 })));
    assert!(matches!(intermediate.bind_with_limit(&args, RelationId(1), 3, |_, _| panic!("catalog")),
        Err(GraphUnwindWriteError::Empty)));
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
        for at in edit.generated.clone() { assert_eq!(plan.original_offset(at), edit.original.start); }
        generated = edit.generated.end;
        original = edit.original.end;
    }
    while generated <= plan.lowered.len() {
        assert_eq!(plan.original_offset(generated), original);
        generated += 1;
        original += 1;
    }
    assert!(plan.lowered.contains("'x; $aa é'"), "literal bytes are not rewritten");
    let error = plan.bind(&rows(vec![int(1)]), RelationId(1), symbols).unwrap_err();
    let GraphUnwindWriteError::Definition(error) = error else { panic!("{error}") };
    assert_eq!(error.offset, text.find("missing").unwrap());

    // Restore diagnostics inside a generated token, including its first byte.
    for edit in &plan.source_offsets {
        for generated in edit.generated.clone() {
            let mut error = GraphWriteScriptError { statement: Some(0), offset: generated,
                kind: GraphWriteScriptErrorKind::Syntax(GraphPatternTextErrorKind::Expected("operand")) };
            plan.restore_script_offsets(&mut error);
            assert_eq!(error.offset, edit.original.start);
        }
    }
    let semicolon = "UNWIND $rows AS x MERGE (n:Entity {id:x}) SET n.p=x; // é";
    let plan = GraphUnwindWriteText::parse(semicolon).unwrap();
    let batch = plan.bind(&rows(vec![int(3)]), RelationId(1), symbols).unwrap();
    assert_eq!(batch.location(0).unwrap().span, 0..semicolon.find(';').unwrap());
}

#[test]
fn every_scalar_lookup_binding_boundary_and_exact_work_allowance_can_refuse() {
    let plan = GraphUnwindWriteText::parse("UNWIND $rows AS m UNWIND m AS x \
        MATCH (n:Entity {id:x}) SET n.p=x").unwrap();
    let args = rows(vec![list(vec![int(2), int(5), int(2)])]);
    let frozen = args.canonical_bytes();
    let (mut events, mut total) = (0usize, 0u64);
    let expected = plan.bind_with_limit_controlled(&args, RelationId(1), 3, symbols, |event| {
        events += 1;
        if let GraphUnwindBindEvent::Work(units) = event { total += units; }
        Ok::<_, usize>(())
    }).unwrap();
    for stop in 0..events {
        let mut at = 0;
        let result = plan.bind_with_limit_controlled(&args, RelationId(1), 3, symbols, |_| {
            let current = at; at += 1;
            if current == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GraphUnwindBindError::Interrupted(found)) if found == stop));
        assert_eq!(args.canonical_bytes(), frozen);
    }
    for allowance in [total, total - 1] {
        let mut spent = 0;
        let result = plan.bind_with_limit_controlled(&args, RelationId(1), 3, symbols, |event| {
            if let GraphUnwindBindEvent::Work(units) = event { spent += units; }
            if spent > allowance { Err(()) } else { Ok(()) }
        });
        assert_eq!(result.is_ok(), allowance == total);
    }
    let retried = plan.bind_with_limit(&args, RelationId(1), 3, symbols).unwrap();
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
        let refused = GraphUnwindWriteText::parse(text).and_then(|plan|
            plan.bind(&rows(vec![int(3)]), RelationId(1), |_, _| panic!("catalog")));
        assert!(refused.is_err(), "{text}");
    }
    for text in [
        "UNWIND $rows AS x CREATE (n {id:x})",
        "UNWIND $rows AS x UNWIND x AS y CREATE (n {id:y}) RETURN n",
        "UNWIND $rows AS SET MATCH (n) WHERE n.p=SET CREATE (m {id:SET})",
        "UNWIND $rows AS MERGE MATCH (n) WHERE n.p=MERGE INSERT (m {id:MERGE})",
    ] {
        assert!(GraphUnwindWriteText::parse_if_supported(text).unwrap().is_none(), "{text}");
    }
}
