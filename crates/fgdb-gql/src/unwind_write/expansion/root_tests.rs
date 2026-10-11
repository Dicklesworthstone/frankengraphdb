//! Root document selection uses the same borrowed, bounded expansion as later
//! sources. These tests also invoke the real native whole-batch compiler.

use super::*;
use crate::parameters::GqlMapParameter;
use fgdb_delta_types::{LabelId, PropertyKeyId};

const QUERY: &str =
    "UNWIND $payload.groups[-1].members AS id MERGE (n:Entity {id:id})";

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}
fn object(entries: Vec<(&str, GraphValue)>) -> GraphValue {
    GraphValue::map(
        entries
            .into_iter()
            .map(|(name, value)| (name.into(), value))
            .collect(),
    )
    .unwrap()
}
fn document(entries: Vec<(&str, GraphValue)>) -> GqlParameters {
    let mut arguments = GqlParameters::new();
    arguments
        .insert(
            "payload".to_owned(),
            GqlParameterValue::Map(
                GqlMapParameter::new(
                    entries
                        .into_iter()
                        .map(|(name, value)| (name.into(), value))
                        .collect(),
                )
                .unwrap(),
            ),
        )
        .unwrap();
    arguments
}
fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Entity") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "parent") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn selected<'a>(plan: &GraphUnwindWriteText, value: &'a GqlParameterValue) -> &'a [GraphValue] {
    parameter_list(
        value,
        &plan.source_path,
        plan.source_offset,
        0,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}
fn integers(values: &[GraphValue]) -> Vec<i64> {
    values
        .iter()
        .map(|value| match value {
            GraphValue::Scalar(CanonicalScalar::Int(value)) => *value,
            value => panic!("expected an integer, got {value:?}"),
        })
        .collect()
}
fn example() -> GqlParameters {
    document(vec![
        (
            "groups",
            list(vec![
                object(vec![("members", int(999))]),
                object(vec![("members", list(vec![int(7), int(3), int(7)]))]),
            ]),
        ),
        ("unused", list(vec![int(900)])),
    ])
}

#[test]
fn document_root_selects_values_without_copying_and_binds_one_program() {
    let plan = GraphUnwindWriteText::parse(QUERY).unwrap();
    let arguments = example();
    let frozen = arguments.canonical_bytes();
    let owner = arguments.get("payload").unwrap();
    let roots = selected(&plan, &owner);
    assert_eq!(integers(roots), [7, 3, 7], "UNWIND retains occurrence order");

    let GqlParameterValue::Map(document) = &owner else {
        panic!("checked map")
    };
    let groups = document.entries().find(|(key, _)| *key == "groups").unwrap().1;
    let members = &groups.as_list().unwrap()[1];
    let (keys, values) = members.as_map().unwrap();
    let at = keys.iter().position(|key| key.as_ref() == "members").unwrap();
    let expected = values[at].as_list().unwrap();
    assert!(core::ptr::eq(roots.as_ptr(), expected.as_ptr()));

    let batch = plan.bind(&arguments, RelationId(1), resolve).unwrap();
    assert_eq!(batch.argument_sets(), 3);
    assert_eq!(batch.program().statements().len(), 3);
    assert_eq!(batch.location(2).unwrap().span, 0..QUERY.len());
    assert_eq!(arguments.canonical_bytes(), frozen);
}

#[test]
fn root_list_indexes_preserve_signed_selection_and_original_byte_locations() {
    let arguments = GqlParameters::new()
        .with_list(
            "payload",
            vec![object(vec![("AS", list(vec![int(4), int(2)]))])],
        )
        .unwrap();
    for selector in ["[0].AS", "[+0].AS", "[-1].AS"] {
        let text = format!(
            "UNWIND /* é; */ $payload {selector} AS id\nMERGE (n:Entity {{id:id}})"
        );
        let plan = GraphUnwindWriteText::parse(&text).unwrap();
        let owner = arguments.get("payload").unwrap();
        assert_eq!(integers(selected(&plan, &owner)), [4, 2]);
        let batch = plan.bind(&arguments, RelationId(1), resolve).unwrap();
        assert_eq!(batch.argument_sets(), 2);
        assert_eq!(batch.location(1).unwrap().span, 0..text.len());
    }
}

#[test]
fn one_document_can_supply_both_root_and_independent_product_sources() {
    let text = "UNWIND $payload.left AS p UNWIND $payload.right AS q \
        MERGE (n:Entity {id:p}) SET n.parent=q";
    let plan = GraphUnwindWriteText::parse(text).unwrap();
    let arguments = document(vec![
        ("left", list(vec![int(1), int(2)])),
        ("right", list(vec![int(10), int(20)])),
    ]);
    let owner = arguments.get("payload").unwrap();
    let roots = selected(&plan, &owner);
    let parameters =
        prepare_parameter_sources(&plan, &arguments, &mut |_| Ok::<_, ()>(())).unwrap();
    let rows = expand_with_parameters(&plan, roots, &parameters, 4, &mut |_| {
        Ok::<_, ()>(())
    })
    .unwrap();
    let actual: Vec<_> = (0..rows.len())
        .map(|row| {
            [rows.at(row, 0), rows.at(row, 1)].map(|value| match value {
                GraphValue::Scalar(CanonicalScalar::Int(value)) => *value,
                _ => panic!("integer product"),
            })
        })
        .collect();
    assert_eq!(actual, [[1, 10], [1, 20], [2, 10], [2, 20]]);
    assert_eq!(
        plan.bind_with_limit(&arguments, RelationId(1), 4, resolve)
            .unwrap()
            .argument_sets(),
        4
    );
    assert!(matches!(
        plan.bind_with_limit(&arguments, RelationId(1), 3, |_, _| panic!("catalog")),
        Err(GraphUnwindWriteError::TooManyRows { limit: 3, observed: 4 })
    ));
}

#[test]
fn root_shape_errors_have_zero_clause_coordinates_before_catalog_access() {
    let text = "UNWIND /* é */ $payload.rows AS id MERGE (n:Entity {id:id})";
    let plan = GraphUnwindWriteText::parse(text).unwrap();
    for (arguments, expected) in [
        (
            document(vec![("rows", int(1))]),
            GraphUnwindRowError::ExpectedListField,
        ),
        (
            GqlParameters::new().with_int64("payload", 1).unwrap(),
            GraphUnwindRowError::ExpectedMapField,
        ),
        (
            GqlParameters::new().with_list("payload", vec![int(1)]).unwrap(),
            GraphUnwindRowError::ExpectedMapField,
        ),
    ] {
        let error = plan
            .bind(&arguments, RelationId(1), |_, _| panic!("catalog"))
            .unwrap_err();
        let GraphUnwindWriteError::Expansion { row, clause, offset, kind } = error else {
            panic!("wrong root refusal: {error}")
        };
        assert_eq!((row, clause, offset), (0, 0, text.find("$payload").unwrap()));
        assert_eq!(kind, expected);
    }
}

#[test]
fn missing_null_and_out_of_range_selections_do_not_invent_empty_successes() {
    let plan = GraphUnwindWriteText::parse(
        "UNWIND $payload.rows AS id MERGE (n:Entity {id:id})",
    )
    .unwrap();
    for arguments in [
        document(vec![]),
        document(vec![("rows", GraphValue::Scalar(CanonicalScalar::Null))]),
        document(vec![("rows", list(vec![]))]),
        GqlParameters::new().with_scalar("payload", CanonicalScalar::Null).unwrap(),
    ] {
        assert!(matches!(
            plan.bind(&arguments, RelationId(1), |_, _| panic!("catalog")),
            Err(GraphUnwindWriteError::Empty)
        ));
    }
    assert!(matches!(
        plan.bind(&GqlParameters::new(), RelationId(1), |_, _| panic!("catalog")),
        Err(GraphUnwindWriteError::SourceParameter)
    ));
    for selector in ["[-9223372036854775808]", "[9223372036854775807]"] {
        let text = format!("UNWIND $payload{selector} AS id MERGE (n:Entity {{id:id}})");
        let plan = GraphUnwindWriteText::parse(&text).unwrap();
        let arguments = GqlParameters::new().with_list("payload", vec![int(1)]).unwrap();
        assert!(matches!(
            plan.bind(&arguments, RelationId(1), |_, _| panic!("catalog")),
            Err(GraphUnwindWriteError::Empty)
        ));
    }
}

#[test]
fn selected_root_cardinality_is_bounded_before_empty_deeper_sources() {
    let plan = GraphUnwindWriteText::parse(
        "UNWIND $payload.rows AS p UNWIND p.children AS c MERGE (n:Entity {id:c})",
    )
    .unwrap();
    let arguments = document(vec![(
        "rows",
        list((0..3).map(|_| object(vec![("children", list(vec![]))])).collect()),
    )]);
    assert!(matches!(
        plan.bind_with_limit(&arguments, RelationId(1), 2, |_, _| panic!("catalog")),
        Err(GraphUnwindWriteError::TooManyRows { limit: 2, observed: 3 })
    ));
}

#[test]
fn root_selector_work_can_be_interrupted_before_each_lookup() {
    let plan = GraphUnwindWriteText::parse(QUERY).unwrap();
    let arguments = example();
    // Initial argument lookup, source admission, then each of three selectors.
    for stop in 0..5 {
        let mut visited = 0;
        let result = plan.bind_with_limit_controlled(
            &arguments,
            RelationId(1),
            64,
            |_, _| panic!("root selection cannot reach the catalog"),
            |event| {
                assert!(matches!(event, GraphUnwindBindEvent::Work(_)));
                let current = visited;
                visited += 1;
                if current == stop { Err("stop") } else { Ok(()) }
            },
        );
        assert!(matches!(result, Err(GraphUnwindBindError::Interrupted("stop"))));
        assert_eq!(visited, stop + 1);
    }
}

#[test]
fn unselected_document_payload_is_not_replicated_into_argument_transcripts() {
    let text = "UNWIND $payload.rows AS id MERGE (n:Entity {id:id})";
    let plan = GraphUnwindWriteText::parse(text).unwrap();
    let mut bills = Vec::new();
    let mut sizes = Vec::new();
    for bytes in [1, 1024] {
        let arguments = document(vec![
            ("rows", list(vec![int(1), int(2)])),
            (
                "unused",
                GraphValue::Scalar(CanonicalScalar::ucs_basic_text(&"x".repeat(bytes)).unwrap()),
            ),
        ]);
        sizes.push(arguments.canonical_byte_len());
        let mut units = 0;
        let batch = plan
            .bind_with_limit_controlled(&arguments, RelationId(1), 64, resolve, |event| {
                if let GraphUnwindBindEvent::Work(work) = event {
                    units += work;
                }
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_eq!(batch.argument_sets(), 2);
        bills.push(units);
    }
    assert_ne!(sizes[0], sizes[1]);
    assert_eq!(bills[0], bills[1]);
}

#[test]
fn complete_static_paths_are_required_only_for_the_additional_write_surface() {
    for source in ["$payload.rows", "$payload.rows[$index]", "$payload.rows[0..2]"] {
        let text = format!("UNWIND {source} AS id CREATE (n {{id:id}}) RETURN n");
        assert!(GraphUnwindWriteText::parse_if_supported(&text).unwrap().is_none());
    }
    for source in [
        "$payload.rows[$index]",
        "$payload.rows[0..2]",
        "$payload.rows[0+1]",
        "$payload.rows + [1]",
        "$payload.rows[9223372036854775808]",
    ] {
        let text = format!("UNWIND {source} AS id MERGE (n:Entity {{id:id}})");
        assert!(matches!(
            GraphUnwindWriteText::parse_if_supported(&text),
            Err(GraphUnwindWriteError::Syntax(_))
        ), "{text}");
    }
    for steps in [64, 65] {
        let text = format!(
            "UNWIND $payload{} AS id MERGE (n:Entity {{id:id}})",
            ".rows".repeat(steps)
        );
        assert_eq!(GraphUnwindWriteText::parse(&text).is_ok(), steps == 64);
    }
}

#[test]
fn selected_roots_still_require_exact_argument_names_and_one_scalar_kind() {
    let plan = GraphUnwindWriteText::parse(
        "UNWIND $payload.rows AS id MERGE (n:Entity {id:id})",
    )
    .unwrap();
    let arguments = document(vec![("rows", list(vec![int(1), GraphValue::Scalar(
        CanonicalScalar::ucs_basic_text("two").unwrap(),
    )]))]);
    assert!(matches!(
        plan.bind(&arguments, RelationId(1), |_, _| panic!("catalog")),
        Err(GraphUnwindWriteError::Row {
            row: 1,
            kind: GraphUnwindRowError::IncompatibleFieldTypes,
            ..
        })
    ));
    let arguments = document(vec![("rows", list(vec![int(1)]))])
        .with_int64("unused", 1)
        .unwrap();
    assert!(matches!(
        plan.bind(&arguments, RelationId(1), |_, _| panic!("catalog")),
        Err(GraphUnwindWriteError::ArgumentNames)
    ));
}
