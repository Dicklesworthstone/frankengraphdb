//! Nested parameter expansion is checked independently of graph execution.
//! Native binding below still compiles every resulting mutation before return.
use super::*;
use fgdb_delta_types::{LabelId, PropertyKeyId};

fn object(entries: Vec<(&str, GraphValue)>) -> GraphValue {
    GraphValue::map(entries.into_iter().map(|(name, value)| (name.into(), value)).collect()).unwrap()
}
fn list(values: Vec<GraphValue>) -> GraphValue { GraphValue::List(values.into_boxed_slice()) }
fn int(value: i64) -> GraphValue { GraphValue::Scalar(CanonicalScalar::Int(value)) }
fn null() -> GraphValue { GraphValue::Scalar(CanonicalScalar::Null) }
fn item(id: i64) -> GraphValue { object(vec![("id", int(id))]) }
fn root(id: i64, children: Vec<GraphValue>, other: Vec<GraphValue>) -> GraphValue {
    object(vec![("id", int(id)), ("children", list(children)), ("other", list(other))])
}
fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Entity") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "parent") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        (GraphSymbolKind::Property, "other") => Some(GraphSymbol::Property(PropertyKeyId(3))),
        _ => None,
    }
}
const QUERY: &str = "UNWIND $rows AS parent UNWIND parent.children AS child \
    MERGE (n:Entity {id:child.id}) SET n.parent=parent.id";
const PRODUCT: &str = "UNWIND $rows AS parent UNWIND parent.children AS child \
    UNWIND parent.other AS sibling MERGE (n:Entity {id:child.id}) \
    SET n.parent=parent.id,n.other=sibling.id";
fn selected(input: &InputRows<'_>, row: usize, field: &UnwindField) -> i64 {
    match scalar_field(input.at(row, field.source), field, row, &mut |_| Ok::<_, ()>(())).unwrap() {
        Some(CanonicalScalar::Int(value)) => *value,
        value => panic!("expected integer, got {value:?}"),
    }
}

#[test]
fn correlated_products_match_independent_nested_loops_and_borrow_the_input() {
    let plan = GraphUnwindWriteText::parse(PRODUCT).unwrap();
    assert_eq!(plan.sources.len(), 2);
    assert_eq!(plan.sources[0].source, 0);
    assert_eq!(plan.sources[1].source, 0, "a source can use a non-immediate ancestor");
    assert_eq!(plan.fields.iter().map(|field| field.source).collect::<Vec<_>>(), [1, 0, 2]);
    for a in 0..=3 {
        for b in 0..=3 {
            for c in 0..=3 {
                for d in 0..=3 {
                    let roots = vec![root(10, (0..a).map(item).collect(), (20..20+b).map(item).collect()),
                        root(30, (40..40+c).map(item).collect(), (60..60+d).map(item).collect())];
                    let mut expected = Vec::new();
                    for (parent, left, right) in [(10, 0..a, 20..20+b), (30, 40..40+c, 60..60+d)] {
                        for child in left {
                            for sibling in right.clone() { expected.push(vec![child, parent, sibling]); }
                        }
                    }
                    let expanded = expand(&plan, &roots, 64, &mut |_| Ok::<_, ()>(()));
                    if expected.is_empty() {
                        assert!(matches!(expanded, Err(GraphUnwindBindError::Binding(GraphUnwindWriteError::Empty))));
                        continue;
                    }
                    let input = expanded.unwrap();
                    let actual: Vec<_> = (0..input.len()).map(|row| plan.fields.iter()
                        .map(|field| selected(&input, row, field)).collect::<Vec<_>>()).collect();
                    assert_eq!(actual, expected);
                    for at in 0..input.len() {
                        assert!(roots.iter().any(|root| core::ptr::eq(root, input.at(at, 0))));
                    }
                    let args = GqlParameters::new().with_list("rows", roots).unwrap();
                    let batch = plan.bind(&args, RelationId(1), resolve).unwrap();
                    assert_eq!(batch.argument_sets(), expected.len());
                    assert_eq!(batch.program().statements().len(), expected.len());
                    assert_eq!(batch.location(expected.len()-1).unwrap().span, 0..PRODUCT.len());
                }
            }
        }
    }
}

#[test]
fn nested_sources_use_their_declared_ancestor_and_null_items_are_not_empty_lists() {
    let text = "UNWIND $rows AS p UNWIND p.groups AS g UNWIND g.children AS c \
        MERGE (n:Entity {id:c.id}) SET n.parent=p.id,n.other=g.id";
    let plan = GraphUnwindWriteText::parse(text).unwrap();
    assert_eq!(plan.sources[1].source, 1);
    let roots = vec![object(vec![("id", int(7)), ("groups", list(vec![
        root(10, vec![item(1), item(2)], vec![]), null(),
        root(20, vec![item(3)], vec![]), object(vec![("id", int(99))]),
    ]))])];
    let input = expand(&plan, &roots, 64, &mut |_| Ok::<_, ()>(())).unwrap();
    let actual: Vec<_> = (0..input.len()).map(|row| plan.fields.iter()
        .map(|field| selected(&input, row, field)).collect::<Vec<_>>()).collect();
    assert_eq!(actual, [vec![1,7,10], vec![2,7,10], vec![3,7,20]]);

    let plan = GraphUnwindWriteText::parse(QUERY).unwrap();
    let roots = vec![root(1, vec![], vec![]), null(), object(vec![("id", int(2))]),
        root(3, vec![null(), item(8)], vec![])];
    let input = expand(&plan, &roots, 64, &mut |_| Ok::<_, ()>(())).unwrap();
    assert_eq!(input.len(), 2);
    assert!(input.at(0, 1).is_null(), "[null] contributes one child occurrence");
    assert!(core::ptr::eq(input.at(0, 0), &roots[3]));
    let args = GqlParameters::new().with_list("rows", roots).unwrap();
    // NULL is a SET operand here, not a MERGE key: key admissibility remains
    // the ordinary mutation compiler's contract, independent of expansion.
    let plan = GraphUnwindWriteText::parse("UNWIND $rows AS parent \
        UNWIND parent.children AS child MERGE (n:Entity {id:parent.id}) SET n.other=child.id").unwrap();
    assert_eq!(plan.bind(&args, RelationId(1), resolve).unwrap().argument_sets(), 2);
}

#[test]
fn final_and_intermediate_expansion_limits_refuse_before_native_resolution() {
    let plan = GraphUnwindWriteText::parse(PRODUCT).unwrap();
    let args = GqlParameters::new().with_list("rows",
        vec![root(1, vec![item(1),item(2)], vec![item(3),item(4),item(5)])]).unwrap();
    assert_eq!(plan.bind_with_limit(&args, RelationId(1), 6, resolve).unwrap().argument_sets(), 6);
    assert!(matches!(plan.bind_with_limit(&args, RelationId(1), 5, |_, _| panic!("catalog")),
        Err(GraphUnwindWriteError::TooManyRows { limit: 5, observed: 6 })));
    let text = "UNWIND $rows AS p UNWIND p.groups AS g UNWIND g.children AS c \
        MERGE (n:Entity {id:c.id})";
    let plan = GraphUnwindWriteText::parse(text).unwrap();
    let args = GqlParameters::new().with_list("rows", vec![object(vec![("groups", list(
        (0..4).map(|id| root(id, vec![], vec![])).collect()))])]).unwrap();
    assert!(matches!(plan.bind_with_limit(&args, RelationId(1), 3, |_, _| panic!("catalog")),
        Err(GraphUnwindWriteError::TooManyRows { limit: 3, observed: 4 })));
    assert!(matches!(plan.bind_with_limit(&args, RelationId(1), 4, |_, _| panic!("catalog")),
        Err(GraphUnwindWriteError::Empty)));
}

#[test]
fn malformed_late_sources_and_incompatible_leaves_cannot_publish_a_prefix() {
    let plan = GraphUnwindWriteText::parse(QUERY).unwrap();
    for bad in [object(vec![("children", int(42))])] {
        let args = GqlParameters::new().with_list("rows", vec![root(1,vec![item(3)],vec![]), bad]).unwrap();
        let frozen = args.canonical_bytes();
        assert!(matches!(plan.bind(&args, RelationId(1), |_, _| panic!("catalog before admission")),
            Err(GraphUnwindWriteError::Expansion { row: 1, clause: 1,
                kind: GraphUnwindRowError::ExpectedListField, .. })));
        assert_eq!(args.canonical_bytes(), frozen);
    }
    // A scalar list item is now a valid binding. Selecting its `.id` still
    // refuses, at the exact flattened operand coordinate, before the catalog.
    let args = GqlParameters::new().with_list("rows", vec![root(1,vec![item(3)],vec![]),
        root(2, vec![item(4), int(42)], vec![])]).unwrap();
    assert!(matches!(plan.bind(&args, RelationId(1), |_, _| panic!("catalog before admission")),
        Err(GraphUnwindWriteError::Row { row: 2,
            kind: GraphUnwindRowError::ExpectedMapField, .. })));
    let args = GqlParameters::new().with_list("rows", vec![root(1, vec![item(1),
        object(vec![("id", GraphValue::Scalar(CanonicalScalar::Bool(true)))])], vec![])]).unwrap();
    assert!(matches!(plan.bind(&args, RelationId(1), |_, _| panic!("catalog")),
        Err(GraphUnwindWriteError::Row { row: 1, kind: GraphUnwindRowError::IncompatibleFieldTypes, .. })));
}

#[test]
fn aliases_and_paths_are_sealed_before_lowering_with_original_byte_coordinates() {
    let text = "UNWIND /* é */ $rows AS p\nUNWIND p.children AS c\nMERGE (n:Entity {id:c.id}) \
        ON CREATE SET n.parent=p.id ON MATCH SET n.parent=p.id SET n.other=$aa";
    let plan = GraphUnwindWriteText::parse(text).unwrap();
    assert_eq!(plan.fields.len(), 2);
    assert_ne!(plan.fields[0].parameter, plan.fields[1].parameter, "p.id and c.id are different bindings");
    assert!(plan.fields.iter().all(|field| field.parameter != "aa"));
    assert_eq!(plan.lowered.len(), text.len());
    let merge = text.find("MERGE").unwrap();
    assert_eq!(&plan.lowered[merge..merge+5], "MERGE");
    for (at, byte) in text.bytes().enumerate().filter(|(_, byte)| matches!(byte, b'\n' | b'\r')) {
        assert_eq!(plan.lowered.as_bytes()[at], byte);
    }
    for prefix in ["UNWIND p.children AS p", "UNWIND later.children AS c",
        "UNWIND $other[$i] AS c", "UNWIND p.children[$i] AS c", "UNWIND p.children AS"] {
        let text = format!("UNWIND $rows AS p {prefix} MERGE (n:Entity {{id:p.id}})");
        assert!(GraphUnwindWriteText::parse(&text).is_err(), "{text}");
    }
    for target in ["p", "c"] {
        let text = format!("UNWIND $rows AS p UNWIND p.children AS c MATCH ({target}) SET {target}.id=1");
        let args = GqlParameters::new().with_list("rows", vec![root(1, vec![item(2)], vec![])]).unwrap();
        assert!(GraphUnwindWriteText::parse(&text).and_then(|definition|
            definition.bind(&args, RelationId(1), |_, _| panic!("graph alias cannot enter catalog"))
        ).is_err());
    }
    let mut text = "UNWIND $rows AS p".to_owned();
    for at in 1..MAX_UNWIND_SOURCES { text.push_str(&format!(" UNWIND p.children AS c{at}")); }
    GraphUnwindWriteText::parse(&format!("{text} MERGE (n:Entity {{id:p.id}})")).unwrap();
    assert!(GraphUnwindWriteText::parse(&format!("{text} UNWIND p.children AS extra MERGE (n:Entity {{id:p.id}})")).is_err());
    for text in ["UNWIND $rows AS p UNWIND $other AS c CREATE (n {id:c})",
        "UNWIND $rows AS p UNWIND p.children AS SET CREATE (n {id:SET.id})"] {
        assert!(GraphUnwindWriteText::parse_if_supported(text).unwrap().is_none(), "{text}");
    }
}

#[test]
fn every_expansion_and_binding_control_can_refuse_and_the_same_input_retries() {
    let plan = GraphUnwindWriteText::parse(PRODUCT).unwrap();
    let args = GqlParameters::new().with_list("rows",
        vec![root(1, vec![item(1),item(2)], vec![item(3),item(4)])]).unwrap();
    let frozen = args.canonical_bytes();
    let (mut events, mut units) = (0, 0u64);
    plan.bind_with_limit_controlled(&args, RelationId(1), 64, resolve, |event| {
        events += 1;
        if let GraphUnwindBindEvent::Work(work) = event { units += work; }
        Ok::<_, usize>(())
    }).unwrap();
    for stop in 0..events {
        let mut at = 0;
        let error = plan.bind_with_limit_controlled(&args, RelationId(1), 64, resolve, |_| {
            let current = at; at += 1;
            if current == stop { Err(stop) } else { Ok(()) }
        }).unwrap_err();
        assert!(matches!(error, GraphUnwindBindError::Interrupted(at) if at == stop));
        assert_eq!(args.canonical_bytes(), frozen);
    }
    for limit in [units, units - 1] {
        let mut spent = 0;
        let bound = plan.bind_with_limit_controlled(&args, RelationId(1), 64, resolve, |event| {
            if let GraphUnwindBindEvent::Work(work) = event { spent += work; }
            if spent > limit { Err(()) } else { Ok(()) }
        });
        assert_eq!(bound.is_ok(), limit == units);
    }
    assert_eq!(plan.bind(&args, RelationId(1), resolve).unwrap().argument_sets(), 4);
}

#[test]
fn independent_parameters_form_ordered_borrowed_products_and_native_programs() {
    use crate::GqlParameterType;
    use fgdb_types::CanonicalScalarKind;

    let text = "UNWIND /* é */ $left AS x\nUNWIND $right AS y\n\
        MATCH (n:Entity {id:x}) SET n.parent=y,n.other=$aa";
    let plan = GraphUnwindWriteText::parse(text).unwrap();
    assert_eq!(plan.sources[0].parameter.as_deref(), Some("right"));
    assert!(plan.fields.iter().all(|field| field.parameter != "aa"));
    let args = GqlParameters::new().with_list("left", vec![int(-4), int(9)]).unwrap()
        .with_list("right", vec![int(2), int(2), int(7)]).unwrap()
        .with_int64("aa", 100).unwrap();
    let frozen = args.canonical_bytes();
    let Some(GqlParameterValue::List(left)) = args.get("left") else { panic!("left list") };
    let Some(GqlParameterValue::List(right)) = args.get("right") else { panic!("right list") };
    let input = expand_with_parameters(&plan, left.values(), Some(&args), 64,
        &mut |_| Ok::<_, ()>(())).unwrap();
    let expected = [(-4,2), (-4,2), (-4,7), (9,2), (9,2), (9,7)];
    assert_eq!(input.len(), expected.len());
    for (at, (x, y)) in expected.into_iter().enumerate() {
        assert_eq!(selected(&input, at, &plan.fields[0]), x);
        assert_eq!(selected(&input, at, &plan.fields[1]), y);
        assert!(core::ptr::eq(input.at(at, 0), &left.values()[at / 3]));
        assert!(core::ptr::eq(input.at(at, 1), &right.values()[at % 3]));
    }
    let bound = plan.bind(&args, RelationId(1), resolve).unwrap();
    assert_eq!(bound.argument_sets(), expected.len());
    assert_eq!(bound.program().statements().len(), expected.len());
    let reference = PreparedGraphWriteScript::prepare_with_parameter_types(
        "MATCH (n:Entity {id:$lhs}) SET n.parent=$rhs,n.other=$aa", RelationId(1),
        &[("lhs", GqlParameterType::Scalar(CanonicalScalarKind::Int)),
          ("rhs", GqlParameterType::Scalar(CanonicalScalarKind::Int)),
          ("aa", GqlParameterType::Scalar(CanonicalScalarKind::Int))], resolve,
    ).unwrap();
    for (at, (x, y)) in expected.into_iter().enumerate() {
        let params = GqlParameters::new().with_int64("lhs", x).unwrap()
            .with_int64("rhs", y).unwrap().with_int64("aa", 100).unwrap();
        let statement = reference.bind_parameters(&params).unwrap();
        assert_eq!(&bound.program().statements()[at], &statement.statements()[0]);
        assert_eq!(bound.location(at).unwrap().span, 0..text.len());
        assert_eq!(bound.location(at).unwrap().argument_set, at);
    }
    assert_eq!(args.canonical_bytes(), frozen);
}

#[test]
fn repeated_parameter_sources_are_products_not_zips_or_duplicate_arguments() {
    let text = "UNWIND $rows AS x UNWIND $rows AS y MERGE (n:Entity {id:x}) SET n.parent=y";
    let plan = GraphUnwindWriteText::parse(text).unwrap();
    let args = GqlParameters::new().with_list("rows", vec![int(3), int(5)]).unwrap();
    assert!(plan.external_parameters.is_empty());
    let Some(GqlParameterValue::List(values)) = args.get("rows") else { panic!("list") };
    let input = expand_with_parameters(&plan, values.values(), Some(&args), 4,
        &mut |_| Ok::<_, ()>(())).unwrap();
    let actual: Vec<_> = (0..input.len()).map(|at| plan.fields.iter()
        .map(|field| selected(&input, at, field)).collect::<Vec<_>>()).collect();
    assert_eq!(actual, [vec![3,3], vec![3,5], vec![5,3], vec![5,5]]);
    assert_eq!(plan.bind(&args, RelationId(1), resolve).unwrap().argument_sets(), 4);
}

#[test]
fn independent_sources_compose_with_correlated_ancestor_and_list_expansion() {
    let text = "UNWIND $rows AS p UNWIND $groups AS g UNWIND g AS x \
        UNWIND p.children AS c MATCH (n:Entity {id:c.id}) SET n.parent=x,n.other=p.id";
    let plan = GraphUnwindWriteText::parse(text).unwrap();
    let roots = vec![root(1, vec![item(4), item(5)], vec![]), root(2, vec![item(6)], vec![])];
    let args = GqlParameters::new().with_list("rows", roots).unwrap()
        .with_list("groups", vec![list(vec![int(10), int(11)]), list(vec![]), null(), list(vec![int(12)])]).unwrap();
    let Some(GqlParameterValue::List(roots)) = args.get("rows") else { panic!("list") };
    let input = expand_with_parameters(&plan, roots.values(), Some(&args), 64,
        &mut |_| Ok::<_, ()>(())).unwrap();
    let mut expected = Vec::new();
    for (parent, children) in [(1, vec![4,5]), (2, vec![6])] {
        for value in [10,11,12] {
            for child in &children { expected.push(vec![*child, value, parent]); }
        }
    }
    let actual: Vec<_> = (0..input.len()).map(|at| plan.fields.iter()
        .map(|field| selected(&input, at, field)).collect::<Vec<_>>()).collect();
    assert_eq!(actual, expected);
    assert_eq!(plan.bind(&args, RelationId(1), resolve).unwrap().argument_sets(), 9);
}

#[test]
fn independent_null_items_survive_but_empty_lists_produce_no_program() {
    let text = "UNWIND $rows AS x UNWIND $other AS y MATCH (n:Entity) SET n.parent=y,n.other=x";
    let plan = GraphUnwindWriteText::parse(text).unwrap();
    let args = GqlParameters::new().with_list("rows", vec![int(1), null()]).unwrap()
        .with_list("other", vec![null(), int(7), int(7)]).unwrap();
    assert_eq!(plan.bind(&args, RelationId(1), resolve).unwrap().argument_sets(), 6);
    let empty = GqlParameters::new().with_list("rows", vec![int(1)]).unwrap()
        .with_list("other", vec![]).unwrap();
    assert!(matches!(plan.bind(&empty, RelationId(1), |_, _| panic!("catalog")),
        Err(GraphUnwindWriteError::Empty)));
}

#[test]
fn independent_source_names_shapes_and_late_bad_columns_precede_the_catalog() {
    let text = "UNWIND /* é */ $rows AS x UNWIND $other AS y MATCH (n:Entity) SET n.parent=y";
    let plan = GraphUnwindWriteText::parse(text).unwrap();
    let missing = GqlParameters::new().with_list("rows", vec![int(1)]).unwrap();
    assert!(matches!(plan.bind(&missing, RelationId(1), |_, _| panic!("catalog")),
        Err(GraphUnwindWriteError::ArgumentNames)));
    let wrong = missing.clone().with_int64("other", 4).unwrap();
    assert!(matches!(plan.bind(&wrong, RelationId(1), |_, _| panic!("catalog")),
        Err(GraphUnwindWriteError::Expansion { row: 0, clause: 1, offset,
            kind: GraphUnwindRowError::ExpectedListField }) if offset == text.find("$other").unwrap()));
    let mixed = missing.clone().with_list("other", vec![int(1), GraphValue::Scalar(CanonicalScalar::Bool(true))]).unwrap();
    assert!(matches!(plan.bind(&mixed, RelationId(1), |_, _| panic!("catalog")),
        Err(GraphUnwindWriteError::Row { row: 1, kind: GraphUnwindRowError::IncompatibleFieldTypes, .. })));
    let text = "UNWIND $rows AS x UNWIND $empty AS e UNWIND $bad AS b MATCH (n:Entity) SET n.parent=b";
    let plan = GraphUnwindWriteText::parse(text).unwrap();
    let wrong = missing.with_list("empty", vec![]).unwrap().with_int64("bad", 42).unwrap();
    assert!(matches!(plan.bind(&wrong, RelationId(1), |_, _| panic!("catalog")),
        Err(GraphUnwindWriteError::Expansion { clause: 2, kind: GraphUnwindRowError::ExpectedListField, .. })));
}

#[test]
fn independent_products_admit_each_prefix_not_only_the_final_count() {
    let text = "UNWIND $rows AS x UNWIND $other AS y MATCH (n:Entity) SET n.parent=x,n.other=y";
    let plan = GraphUnwindWriteText::parse(text).unwrap();
    let args = GqlParameters::new().with_list("rows", vec![int(1), int(2)]).unwrap()
        .with_list("other", vec![int(3), int(4), int(5)]).unwrap();
    assert_eq!(plan.bind_with_limit(&args, RelationId(1), 6, resolve).unwrap().argument_sets(), 6);
    assert!(matches!(plan.bind_with_limit(&args, RelationId(1), 5, |_, _| panic!("catalog")),
        Err(GraphUnwindWriteError::TooManyRows { limit: 5, observed: 6 })));
    let text = "UNWIND $rows AS x UNWIND $other AS y UNWIND $empty AS z MATCH (n:Entity) SET n.parent=x";
    let plan = GraphUnwindWriteText::parse(text).unwrap();
    let args = args.with_list("empty", vec![]).unwrap();
    assert!(matches!(plan.bind_with_limit(&args, RelationId(1), 5, |_, _| panic!("catalog")),
        Err(GraphUnwindWriteError::TooManyRows { limit: 5, observed: 6 })));
    assert!(matches!(plan.bind_with_limit(&args, RelationId(1), 6, |_, _| panic!("catalog")),
        Err(GraphUnwindWriteError::Empty)));
}

#[test]
fn independent_sources_cannot_be_captured_as_scalar_parameters_or_rebound_aliases() {
    for text in [
        "UNWIND $rows AS x UNWIND $other AS y MATCH (n:Entity) SET n.parent=$other",
        "UNWIND $rows AS x UNWIND $rows AS y MATCH (n:Entity) SET n.parent=$rows",
        "UNWIND $rows AS x UNWIND $other AS x MATCH (n:Entity) SET n.parent=x",
        "UNWIND $rows AS x UNWIND $other[$i] AS y MATCH (n:Entity) SET n.parent=y",
    ] {
        assert!(GraphUnwindWriteText::parse(text).is_err(), "{text}");
    }
    assert!(GraphUnwindWriteText::parse_if_supported(
        "UNWIND $rows AS x UNWIND $other AS y CREATE (n:Entity {id:x,parent:y})"
    ).unwrap().is_none());
}

#[test]
fn independent_products_share_one_cancellable_budget_and_can_retry_unchanged() {
    let text = "UNWIND $rows AS x UNWIND $other AS y MATCH (n:Entity) SET n.parent=x,n.other=y";
    let plan = GraphUnwindWriteText::parse(text).unwrap();
    let args = GqlParameters::new().with_list("rows", vec![int(1), int(2)]).unwrap()
        .with_list("other", vec![int(3), int(4)]).unwrap();
    let frozen = args.canonical_bytes();
    let (mut events, mut units) = (0, 0u64);
    plan.bind_with_limit_controlled(&args, RelationId(1), 4, resolve, |event| {
        events += 1;
        if let GraphUnwindBindEvent::Work(work) = event { units += work; }
        Ok::<_, usize>(())
    }).unwrap();
    for stop in 0..events {
        let mut at = 0;
        let bound = plan.bind_with_limit_controlled(&args, RelationId(1), 4, resolve, |_| {
            let current = at;
            at += 1;
            if current == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(bound, Err(GraphUnwindBindError::Interrupted(observed)) if observed == stop));
        assert_eq!(args.canonical_bytes(), frozen);
    }
    for allowance in [units, units - 1] {
        let mut spent = 0;
        let bound = plan.bind_with_limit_controlled(&args, RelationId(1), 4, resolve, |event| {
            if let GraphUnwindBindEvent::Work(work) = event { spent += work; }
            if spent > allowance { Err(()) } else { Ok(()) }
        });
        assert_eq!(bound.is_ok(), allowance == units);
    }
    assert_eq!(plan.bind(&args, RelationId(1), resolve).unwrap().argument_sets(), 4);
}
