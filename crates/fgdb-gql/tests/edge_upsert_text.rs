//! Native relationship upserts bind selection and branch arguments once.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GraphEdgeUpsertBranch, GraphEdgeUpsertBuildError,
    GraphEdgeUpsertTextErrorKind, GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind,
    PreparedGraphEdgeUpsertText,
};
use fgdb_types::{CanonicalScalar, CanonicalScalarKind};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const W: PropertyKeyId = PropertyKeyId(2);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "w") => Some(GraphSymbol::Property(W)),
        _ => None,
    }
}
fn arguments() -> GqlParameters {
    GqlParameters::new()
        .with_int64("left", 1)
        .unwrap()
        .with_int64("right", 2)
        .unwrap()
        .with_int64("fresh", 200)
        .unwrap()
        .with_int64("seen", 100)
        .unwrap()
}
const TEXT: &str = "MATCH (a),(b) WHERE a.p=$left AND b.p=$right MERGE (a)-[e:R]->(b) ON CREATE SET e.w=$fresh ON MATCH SET e.w=$seen";

#[test]
fn native_selection_and_branches_share_one_frozen_parameter_contract() {
    let calls = Cell::new(0);
    let template = PreparedGraphEdgeUpsertText::prepare(TEXT, RelationId(9), |kind, name| {
        calls.set(calls.get() + 1);
        symbols(kind, name)
    })
    .unwrap();
    let resolved = calls.get();
    assert_eq!(template.statement(), TEXT);
    assert_eq!(template.parameter_schema().len(), 4);
    let args = arguments();
    let bound = template.bind_parameters(&args).unwrap();
    assert_eq!(bound.merge().relation(), R);
    assert_eq!(
        bound.on_create()[0].value.literal().unwrap().value(),
        &CanonicalScalar::Int(200)
    );
    assert_eq!(
        bound.on_match()[0].value.literal().unwrap().value(),
        &CanonicalScalar::Int(100)
    );
    assert_eq!(template.bind_parameters(&args).unwrap(), bound);
    assert_eq!(calls.get(), resolved, "rebind may not enter the catalog");

    let reverse = PreparedGraphEdgeUpsertText::prepare(
        "MATCH (a),(b) WHERE a.p=$left AND b.p=$right MERGE (b)<-[e:R]-(a) ON MATCH SET e.w=$seen ON CREATE SET e.w=$fresh",
        R, symbols,
    ).unwrap().bind_parameters(&args).unwrap();
    assert_eq!(bound.canonical_bytes(), reverse.canonical_bytes());
    assert!(!format!("{template:?} {bound:?}").contains("$fresh"));
}

#[test]
fn malformed_branch_direction_and_target_never_reach_catalog_resolution() {
    for text in [
        "MATCH (a),(b) MERGE (a)-[e:R]->(b) ON MATCH SET e.w=1 ON MATCH SET e.p=2",
        "MATCH (a),(b) MERGE (a)-[e:R]-(b) ON MATCH SET e.w=1",
        "MATCH (a),(b) MERGE (a)<-[e:R]->(b) ON MATCH SET e.w=1",
        "MATCH (a),(b) MERGE (a)-[a:R]->(b) ON MATCH SET a.w=1",
        "MATCH (a),(b) MERGE (a)-[e:R]->(b) ON MATCH SET a.w=1",
        "MATCH (a),(b) MERGE (a)-[e:R]->(b) ON CREATE SET e.w=a.p",
        "MATCH (a),(b) MERGE (a)-[e:R {w:1}]->(b) ON MATCH SET e.w=1",
        "MATCH (a),(b) MERGE (a)-[e:R]->(b)",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphEdgeUpsertText::prepare(text, R, |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "{text}");
    }
}

#[test]
fn duplicate_fields_fail_in_the_selected_typed_branch_definition() {
    let template = PreparedGraphEdgeUpsertText::prepare(
        "MATCH (a),(b) MERGE (a)-[e:R]->(b) ON CREATE SET e.w=1,e.w=2",
        R,
        symbols,
    )
    .unwrap();
    let error = template.bind_parameters(&GqlParameters::new()).unwrap_err();
    assert!(matches!(
        error.kind,
        GraphEdgeUpsertTextErrorKind::UpsertBuild(GraphEdgeUpsertBuildError::DuplicateProperty {
            branch: GraphEdgeUpsertBranch::Create,
        })
    ));
}

#[test]
fn missing_and_wrong_branch_arguments_keep_original_byte_offsets() {
    let text = "\u{2003}MATCH (a),(b) MERGE (a)-[e:R]->(b) ON CREATE SET e.w=$value";
    let template = PreparedGraphEdgeUpsertText::prepare(text, R, symbols).unwrap();
    let missing = template.bind_parameters(&GqlParameters::new()).unwrap_err();
    assert_eq!(missing.offset, text.find('$').unwrap());
    assert!(matches!(
        missing.kind,
        GraphEdgeUpsertTextErrorKind::Query(GraphPatternTextErrorKind::MissingParameter)
    ));
    let wrong = template
        .bind_parameters(&GqlParameters::new().with_uint64("value", 1).unwrap())
        .unwrap_err();
    assert_eq!(wrong.offset, text.find('$').unwrap());
    assert!(matches!(
        wrong.kind,
        GraphEdgeUpsertTextErrorKind::Query(
            GraphPatternTextErrorKind::ParameterTypeMismatch { .. }
        )
    ));
}

#[test]
fn quoted_parameter_payload_is_a_value_not_additional_write_syntax() {
    let payload = "private '; MATCH (x) DETACH DELETE x; --";
    let kind = CanonicalScalarKind::of(&CanonicalScalar::ucs_basic_text(payload).unwrap());
    let template = PreparedGraphEdgeUpsertText::prepare_with_parameter_types(
        "MATCH (a),(b) MERGE (a)-[e:R]->(b) ON CREATE SET e.w=$payload",
        R,
        &[("payload", GqlParameterType::Scalar(kind))],
        symbols,
    )
    .unwrap();
    let bound = template
        .bind_parameters(&GqlParameters::new().with_text("payload", payload).unwrap())
        .unwrap();
    assert_eq!(bound.on_create().len(), 1);
    assert_eq!(
        bound.on_create()[0].value.literal().unwrap().value(),
        &CanonicalScalar::ucs_basic_text(payload).unwrap()
    );
    assert!(!format!("{template:?} {bound:?}").contains(payload));
}

// Clauses can produce the same final literal value without being the same
// executable definition: overwritten actions still cost work and need authority.
#[test]
fn trailing_set_retains_its_own_clause_and_overwritten_on_actions() {
    let args = arguments();
    let bind = |text: &str| {
        PreparedGraphEdgeUpsertText::prepare(text, R, symbols)
            .unwrap()
            .bind_parameters(&args)
            .unwrap()
    };
    let head = "MATCH (a),(b) WHERE a.p=$left AND b.p=$right MERGE (a)-[e:R]->(b)";
    let plain = bind(&format!("{head} SET e.w=$seen, e.p=$fresh"));
    assert!(plain.on_match().is_empty());
    assert!(plain.on_create().is_empty());
    assert_eq!(plain.after().len(), 2);
    for branch in [GraphEdgeUpsertBranch::Match, GraphEdgeUpsertBranch::Create] {
        assert_eq!(plain.action_count(branch), 2);
    }
    assert_eq!(plain.action_count(GraphEdgeUpsertBranch::NoInput), 0);
    let sequential = bind(&format!(
        "{head} ON CREATE SET e.w=$fresh SET e.w=$seen, e.p=$fresh"
    ));
    assert_eq!(sequential.on_create().len(), 1);
    assert_eq!(sequential.after().len(), 2);
    assert_eq!(sequential.action_count(GraphEdgeUpsertBranch::Create), 3);
    assert_eq!(sequential.action_count(GraphEdgeUpsertBranch::Match), 2);
    assert_eq!(sequential.on_create()[0].value.literal().unwrap().value(),
        &CanonicalScalar::Int(200));
    assert_ne!(plain.canonical_bytes(), sequential.canonical_bytes());
    let flattened = bind(&format!(
        "{head} ON CREATE SET e.w=$seen, e.p=$fresh ON MATCH SET e.w=$seen, e.p=$fresh"
    ));
    assert_ne!(plain.canonical_bytes(), flattened.canonical_bytes());
}

// Evaluate the actual checked bytecode, not an imitation of expression syntax.
fn evaluate(
    value: &fgdb_gql::GraphEdgeUpsertValue,
    stored: &[(PropertyKeyId, CanonicalScalar)],
) -> CanonicalScalar {
    use fgdb_gql::GraphEdgeUpsertValue;
    use fgdb_gql::algebra::GraphValue;
    match value {
        GraphEdgeUpsertValue::Literal(value) => value.value().clone(),
        GraphEdgeUpsertValue::Expression { properties, value } => {
            let input = properties.iter().map(|key| {
                GraphValue::Scalar(stored.iter().find(|(k, _)| k == key)
                    .map_or(CanonicalScalar::Null, |(_, value)| value.clone()))
            }).collect::<Vec<_>>();
            value.evaluate_scalar_with_control(&input, &mut |_| Ok::<(), ()>(())).unwrap()
        }
    }
}

#[test]
fn computed_expressions_rebind_without_catalog_reentry_and_keep_property_ordinals() {
    let text = "MATCH (a),(b) MERGE (a)-[e:R]->(b) SET e.w=e.w+e.p*$step+e.w";
    let calls = Cell::new(0);
    let prepared = PreparedGraphEdgeUpsertText::prepare(text, R, |kind, name| {
        calls.set(calls.get() + 1);
        symbols(kind, name)
    }).unwrap();
    assert_eq!(calls.get(), 3, "one resolution per R, w, p");
    let original = prepared.bind_parameters(&GqlParameters::new().with_int64("step", 2).unwrap()).unwrap();
    let changed = prepared.bind_parameters(&GqlParameters::new().with_int64("step", 3).unwrap()).unwrap();
    assert_eq!(calls.get(), 3);
    let fgdb_gql::GraphEdgeUpsertValue::Expression { properties, .. } = &original.after()[0].value else {
        panic!("computed SET must retain its bytecode")
    };
    assert_eq!(properties, &[W, P], "first-reference order; repeated w shares its slot");
    let stored = [(P, CanonicalScalar::Int(5)), (W, CanonicalScalar::Int(7))];
    assert_eq!(evaluate(&original.after()[0].value, &stored), CanonicalScalar::Int(24));
    assert_eq!(evaluate(&changed.after()[0].value, &stored), CanonicalScalar::Int(29));
    assert_ne!(original.canonical_bytes(), changed.canonical_bytes());
    assert_eq!(prepared.statement(), text);
    assert!(!format!("{prepared:?} {original:?}").contains("$step"));
}

#[test]
fn shared_scalar_bytecode_handles_copy_null_lazy_case_text_and_boolean_results() {
    let head = "MATCH (a),(b) MERGE (a)-[e:R]->(b) SET e.w=";
    for (rhs, input, expected) in [
        ("e.p", vec![(P, CanonicalScalar::Int(9))], CanonicalScalar::Int(9)),
        ("e.p", vec![], CanonicalScalar::Null),
        ("COALESCE(e.p, 1/0)", vec![(P, CanonicalScalar::Int(9))], CanonicalScalar::Int(9)),
        ("CASE WHEN e.p IS NULL THEN 4 ELSE 1/0 END", vec![], CanonicalScalar::Int(4)),
        ("e.p > 3 AND e.p < 10", vec![(P, CanonicalScalar::Int(9))], CanonicalScalar::Bool(true)),
        ("UPPER(e.p)", vec![(P, CanonicalScalar::ucs_basic_text("Ada").unwrap())],
            CanonicalScalar::ucs_basic_text("ADA").unwrap()),
    ] {
        let bound = PreparedGraphEdgeUpsertText::prepare(&format!("{head}{rhs}"), R, symbols)
            .unwrap().bind_parameters(&GqlParameters::new()).unwrap();
        assert_eq!(evaluate(&bound.after()[0].value, &input), expected, "{rhs}");
    }
}

#[test]
fn malformed_computed_or_foreign_inputs_are_refused_before_catalog_access() {
    let head = "MATCH (a),(b) MERGE (a)-[e:R]->(b) SET e.w=";
    for rhs in ["a.p+1", "other.p", "e", "e.p+", "e.p.foo", "e.p[0]", "1+'text'", "CASE WHEN TRUE THEN 1 END +"] {
        let calls = Cell::new(0);
        assert!(PreparedGraphEdgeUpsertText::prepare(&format!("{head}{rhs}"), R, |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        }).is_err(), "{rhs}");
        assert_eq!(calls.get(), 0, "{rhs}");
    }
    let calls = Cell::new(0);
    assert!(PreparedGraphEdgeUpsertText::prepare_with_parameter_types(
        &format!("{head}e.p*$bad"), R,
        &[("bad", GqlParameterType::Scalar(CanonicalScalarKind::Text))],
        |kind, name| { calls.set(calls.get() + 1); symbols(kind, name) },
    ).is_err());
    assert_eq!(calls.get(), 0, "declared non-integer arithmetic refuses before names resolve");
}

#[test]
fn computed_parameter_errors_keep_original_utf8_offsets() {
    let text = "\u{2003}MATCH (a),(b) MERGE (a)-[e:R]->(b) SET e.w=e.p+$step";
    let prepared = PreparedGraphEdgeUpsertText::prepare(text, R, symbols).unwrap();
    for arguments in [GqlParameters::new(), GqlParameters::new().with_uint64("step", 1).unwrap()] {
        let error = prepared.bind_parameters(&arguments).unwrap_err();
        assert_eq!(error.offset, text.find('$').unwrap());
        assert!(matches!(error.kind,
            GraphEdgeUpsertTextErrorKind::Query(GraphPatternTextErrorKind::MissingParameter
                | GraphPatternTextErrorKind::ParameterTypeMismatch { .. })));
    }
}

#[test]
fn combined_clause_cap_and_duplicate_fields_cannot_be_erased_by_overwrites() {
    let head = "MATCH (a),(b) MERGE (a)-[e:R]->(b)";
    let actions = (0..fgdb_gql::MAX_GRAPH_EDGE_UPSERT_ACTIONS)
        .map(|i| format!("e.k{i}=1")).collect::<Vec<_>>().join(",");
    let error = PreparedGraphEdgeUpsertText::prepare(
        &format!("{head} ON CREATE SET {actions} SET e.k0=e.k0+1"), R,
        |_, _| panic!("combined action cap must be admitted before catalog access"),
    ).unwrap_err();
    assert!(matches!(error.kind, GraphEdgeUpsertTextErrorKind::UpsertBuild(
        GraphEdgeUpsertBuildError::TooManyActions { branch: GraphEdgeUpsertBranch::Create, observed: 257, .. }
    )));
    let bound = PreparedGraphEdgeUpsertText::prepare(
        &format!("{head} SET e.w=e.p, e.w=1"), R, symbols,
    ).unwrap().bind_parameters(&GqlParameters::new());
    assert!(matches!(bound, Err(fgdb_gql::GraphEdgeUpsertTextError {
        kind: GraphEdgeUpsertTextErrorKind::UpsertBuild(GraphEdgeUpsertBuildError::DuplicateProperty { .. }), ..
    })));
}
