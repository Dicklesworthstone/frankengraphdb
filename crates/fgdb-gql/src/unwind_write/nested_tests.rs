//! Nested documents feed the existing atomic write-program binder. These tests
//! distinguish path selection, ingress admission, and native statement binding.
use super::*;
use fgdb_delta_types::{LabelId, PropertyKeyId};

fn object(entries: Vec<(&str, GraphValue)>) -> GraphValue {
    GraphValue::map(entries.into_iter().map(|(key, value)| (key.into(), value)).collect()).unwrap()
}
fn list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn text(value: &str) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap())
}
fn document(id: i64, name: GraphValue) -> GraphValue {
    object(vec![("identity", object(vec![("id", int(id))])),
        ("versions", list(vec![object(vec![("name", text("old"))]),
            object(vec![("name", name)])]))])
}
fn args(values: Vec<GraphValue>) -> GqlParameters {
    GqlParameters::new().with_list("rows", values).unwrap()
}
fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Entity") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        (GraphSymbolKind::Property, "other") => Some(GraphSymbol::Property(PropertyKeyId(3))),
        _ => None,
    }
}
const QUERY: &str = "UNWIND $rows AS row MERGE (n:Entity {id:row.identity.id}) \
    SET n.name=row.versions[-1].name";

fn lookup<'a>(path: &str, row: &'a GraphValue)
    -> Result<Option<&'a CanonicalScalar>, GraphUnwindBindError<()>> {
    let query = format!("UNWIND $rows AS row MATCH (n) SET n.name={path}");
    let parsed = GraphUnwindWriteText::parse(&query).unwrap();
    assert_eq!(parsed.fields.len(), 1);
    scalar_field(row, &parsed.fields[0], 0, &mut |_| Ok(()))
}

#[test]
fn nested_paths_select_borrowed_leaves_and_bind_one_complete_program() {
    let row = document(7, text("Ada\n'quoted'; $aa"));
    let selected = lookup("row.versions[-1].name", &row).unwrap().unwrap();
    let (keys, values) = row.as_map().unwrap();
    let versions = keys.binary_search_by(|key| key.as_ref().cmp("versions")).unwrap();
    let GraphValue::List(versions) = &values[versions] else { panic!("list") };
    let (_, fields) = versions[1].as_map().unwrap();
    let GraphValue::Scalar(original) = &fields[0] else { panic!("scalar") };
    assert!(core::ptr::eq(selected, original), "lookup must not clone the document or payload");
    assert_eq!(lookup("row.identity.id", &row).unwrap(), Some(&CanonicalScalar::Int(7)));
    assert_eq!(lookup("row.versions[+0].name", &row).unwrap(),
        Some(&CanonicalScalar::ucs_basic_text("old").unwrap()));

    let parsed = GraphUnwindWriteText::parse(QUERY).unwrap();
    let arguments = args(vec![row, document(8, text("Bob")), document(7, text("updated"))]);
    let frozen = arguments.canonical_bytes();
    let batch = parsed.bind(&arguments, RelationId(1), resolve).unwrap();
    assert_eq!(batch.argument_sets(), 3);
    assert_eq!(batch.program().statements().len(), 3);
    for at in 0..3 {
        assert_eq!(batch.location(at).unwrap().argument_set, at);
        assert_eq!(batch.location(at).unwrap().span, 0..QUERY.len());
    }
    assert_eq!(arguments.canonical_bytes(), frozen);
}

#[test]
fn absent_null_and_out_of_range_paths_propagate_null_without_index_overflow() {
    let row = document(3, GraphValue::Scalar(CanonicalScalar::Null));
    for path in ["row.absent.field[0].more", "row.versions[2].name", "row.versions[-3].name",
        "row.versions[9223372036854775807].name", "row.versions[-9223372036854775808].name",
        "row.versions[-1].name.more[0]"] {
        assert!(lookup(path, &row).unwrap().is_none(), "{path}");
    }
    assert!(lookup("row.any.path[0]", &GraphValue::Scalar(CanonicalScalar::Null))
        .unwrap().is_none());
    let arguments = args(vec![document(1, GraphValue::Scalar(CanonicalScalar::Null)),
        document(2, text("typed later")), object(vec![("identity", object(vec![("id", int(3))]))])]);
    assert_eq!(GraphUnwindWriteText::parse(QUERY).unwrap()
        .bind(&arguments, RelationId(1), resolve).unwrap().argument_sets(), 3);
}

#[test]
fn invalid_nested_containers_or_leaf_types_refuse_before_any_catalog_callback() {
    let cases = [
        (object(vec![("identity", int(9))]), GraphUnwindRowError::ExpectedMapField),
        (object(vec![("identity", object(vec![("id", int(9))])),
            ("versions", object(vec![]))]), GraphUnwindRowError::ExpectedListField),
        (document(9, list(vec![int(1)])), GraphUnwindRowError::ExpectedScalarField),
        (document(9, int(3)), GraphUnwindRowError::IncompatibleFieldTypes),
    ];
    let parsed = GraphUnwindWriteText::parse(QUERY).unwrap();
    for (bad, expected) in cases {
        let arguments = args(vec![document(1, text("valid prefix")), bad]);
        let frozen = arguments.canonical_bytes();
        let error = parsed.bind(&arguments, RelationId(1), |_, _| panic!("catalog before input admission"))
            .unwrap_err();
        assert!(matches!(error, GraphUnwindWriteError::Row { row: 1, kind, .. } if kind == expected));
        assert_eq!(arguments.canonical_bytes(), frozen);
    }
}

#[test]
fn lowering_keeps_offsets_and_distinguishes_complete_paths_without_capture() {
    let query = "UNWIND /* é */ $rows AS row\nMERGE (n:Entity {id:row.identity.id}) \
        ON CREATE SET n.name=row.versions /* x */ [-1] . name \
        ON MATCH SET n.name=row.versions[-1].name SET n.other=$aa";
    let parsed = GraphUnwindWriteText::parse(query).unwrap();
    assert_eq!(parsed.fields.len(), 2, "the same complete path shares one parameter");
    assert!(parsed.fields.iter().all(|field| field.parameter != "aa"));
    assert_eq!(parsed.lowered.len(), query.len());
    for (at, byte) in query.bytes().enumerate().filter(|(_, byte)| matches!(byte, b'\n' | b'\r')) {
        assert_eq!(parsed.lowered.as_bytes()[at], byte);
    }
    let set_at = query.rfind("SET").unwrap();
    assert_eq!(&parsed.lowered[set_at..], &query[set_at..]);
    let arguments = args(vec![document(1, text("row.versions[-1].name;\n'quote'"))])
        .with_text("aa", "global").unwrap();
    parsed.bind(&arguments, RelationId(1), resolve).unwrap();

    let query = "UNWIND $rows AS row MERGE (n:Entity {id:row.identity.id}) \
        ON CREATE SET n.name=row.versions[0].name ON MATCH SET n.name=row.versions[-1].name";
    let parsed = GraphUnwindWriteText::parse(query).unwrap();
    assert_eq!(parsed.fields.len(), 3, "shared prefix is not a shared full path");
    let quoted = "UNWIND $rows AS row MERGE (n:Entity {id:row.identity.id}) \
        SET n.name='row.versions[0].name; $aa é'";
    let parsed = GraphUnwindWriteText::parse(quoted).unwrap();
    let at = quoted.find("'row").unwrap();
    assert_eq!(&parsed.lowered[at..], &quoted[at..]);
}

#[test]
fn static_index_and_definition_bounds_refuse_without_erasing_bad_suffixes() {
    for path in ["row.data[$i]", "row.data[row.index]", "row.data[1 + 1]", "row.data[1..2]",
        "row.data[1.0]", "row.data['key']", "row.data[]", "row.data[-]",
        "row.data[9223372036854775808]", "row.data[-9223372036854775809]",
        "row.data[18446744073709551616]", "row.data."] {
        let query = format!("UNWIND $rows AS row MERGE (n:Entity {{id:{path}}})");
        assert!(matches!(GraphUnwindWriteText::parse(&query), Err(GraphUnwindWriteError::Syntax(_))), "{path}");
    }
    let path = format!("row{}", ".x".repeat(MAX_UNWIND_FIELD_STEPS));
    let query = format!("UNWIND $rows AS row MERGE (n:Entity {{id:{path}}})");
    assert_eq!(GraphUnwindWriteText::parse(&query).unwrap().fields[0].path.len(), MAX_UNWIND_FIELD_STEPS);
    let query = format!("UNWIND $rows AS row MERGE (n:Entity {{id:{path}.x}})");
    assert!(matches!(GraphUnwindWriteText::parse(&query), Err(GraphUnwindWriteError::Syntax(_))));
}

#[test]
fn every_nested_lookup_and_binder_checkpoint_can_refuse_then_retry_cleanly() {
    let parsed = GraphUnwindWriteText::parse(QUERY).unwrap();
    let arguments = args(vec![document(1, text("one")), document(2, text("two"))]);
    let frozen = arguments.canonical_bytes();
    let mut events = 0;
    let mut units = 0_u64;
    parsed.bind_with_limit_controlled(&arguments, RelationId(1), 64, resolve, |event| {
        events += 1;
        if let GraphUnwindBindEvent::Work(work) = event { units += work; }
        Ok::<_, usize>(())
    }).unwrap();
    for boundary in 0..events {
        let mut at = 0;
        let error = parsed.bind_with_limit_controlled(&arguments, RelationId(1), 64, resolve, |_| {
            let current = at;
            at += 1;
            if current == boundary { Err(boundary) } else { Ok(()) }
        }).unwrap_err();
        assert!(matches!(error, GraphUnwindBindError::Interrupted(at) if at == boundary));
        assert_eq!(arguments.canonical_bytes(), frozen);
    }
    for allowance in [units, units - 1] {
        let mut used = 0_u64;
        let result = parsed.bind_with_limit_controlled(&arguments, RelationId(1), 64, resolve, |event| {
            if let GraphUnwindBindEvent::Work(work) = event { used += work; }
            if used > allowance { Err(()) } else { Ok(()) }
        });
        assert_eq!(result.is_ok(), allowance == units);
    }
    assert_eq!(parsed.bind(&arguments, RelationId(1), resolve).unwrap().argument_sets(), 2);

    let row = document(1, text("leaf"));
    let field = &parsed.fields[1]; // map -> list -> map -> scalar
    let mut lookups = 0;
    scalar_field(&row, field, 0, &mut |_| { lookups += 1; Ok::<_, ()>(()) }).unwrap();
    assert_eq!(lookups, field.path.len());
}
