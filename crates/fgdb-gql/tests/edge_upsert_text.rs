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
    let template = PreparedGraphEdgeUpsertText::prepare(TEXT, R, |kind, name| {
        calls.set(calls.get() + 1);
        symbols(kind, name)
    })
    .unwrap();
    let resolved = calls.get();
    assert_eq!(template.statement(), TEXT);
    assert_eq!(template.parameter_schema().len(), 4);
    let args = arguments();
    let bound = template.bind_parameters(&args).unwrap();
    assert_eq!(
        bound.on_create()[0].value.value(),
        &CanonicalScalar::Int(200)
    );
    assert_eq!(
        bound.on_match()[0].value.value(),
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
        bound.on_create()[0].value.value(),
        &CanonicalScalar::ucs_basic_text(payload).unwrap()
    );
    assert!(!format!("{template:?} {bound:?}").contains(payload));
}
