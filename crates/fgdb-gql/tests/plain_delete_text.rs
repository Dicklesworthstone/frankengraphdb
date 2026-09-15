//! Native plain DELETE must share MATCH parsing/binding without source rewriting.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryPolicy, GraphDeleteBuildError, GraphDeletePolicy,
    GraphDeleteTextErrorKind, GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind,
    PreparedGraphDeleteText, PreparedGraphMutationText,
};
use fgdb_types::{CanonicalScalar, VId};
use std::cell::Cell;
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn policy() -> GraphDeletePolicy {
    GraphDeletePolicy::new(GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000), 100)
}

#[test]
fn shared_match_where_optional_and_parameters_bind_once() {
    let text = "MATCH (a) WHERE a.p >= $min OPTIONAL MATCH (a)-[:R]->(b) DELETE a,b";
    let calls = Cell::new(0);
    let template = PreparedGraphDeleteText::prepare(text, R, |kind, name| {
        calls.set(calls.get() + 1);
        symbols(kind, name)
    }).unwrap();
    let resolved = calls.get();
    assert_eq!(template.parameter_schema().len(), 1);
    assert_eq!(template.parameter_schema()[0].name, "min");
    assert_eq!(template.statement(), text);
    let deletion = template.bind_parameters(
        &GqlParameters::new().with_int64("min", 2).unwrap(),
    ).unwrap();
    assert_eq!(calls.get(), resolved, "binding must not re-enter the catalog");
    assert_eq!(deletion.target_columns(), &[0, 1]);

    let vertices = [VId(1), VId(2), VId(3)];
    let edges = [(VId(2), R, VId(3))];
    let props = BTreeMap::from([
        (VId(1), CanonicalScalar::Int(1)),
        (VId(2), CanonicalScalar::Int(2)),
        (VId(3), CanonicalScalar::Int(3)),
    ]);
    let proposal = deletion.execute_governed(
        policy(),
        |selection, allowance| selection.plan().execute_governed_with_properties(
            (vertices.len() + edges.len()) as u64,
            vertices,
            edges,
            |vid, predicates| Ok::<_, ()>(predicates.iter().all(|predicate| {
                predicate.matches_borrowed([], Some((P, props.get(&vid).unwrap())))
            })),
            |vid, key| Ok((key == P).then(|| props.get(&vid).unwrap())),
            allowance,
            || Ok::<_, ()>(()),
        ),
        || Ok::<_, ()>(()),
    ).unwrap();
    assert_eq!(proposal.targets(), &[VId(2), VId(3)]);
    assert!(!format!("{template:?} {deletion:?} {proposal:?}").contains("min"));
}

#[test]
fn malformed_delete_refuses_before_catalog_and_detach_keeps_its_old_entrypoint() {
    for text in [
        "MATCH (a) DELETE",
        "MATCH (a) DELETE missing",
        "MATCH (a) DELETE a,a",
        "MATCH (a) DELETE a.p",
        "MATCH (a) DETACH DELETE a",
        "MATCH (a) DELETE a RETURN a",
        "MATCH (a) DELETE a,",
    ] {
        let calls = Cell::new(0);
        assert!(PreparedGraphDeleteText::prepare(text, R, |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        }).is_err(), "{text}");
        assert_eq!(calls.get(), 0, "malformed DELETE reached catalog: {text}");
    }
    assert!(PreparedGraphMutationText::prepare("MATCH (a) DETACH DELETE a", R, symbols).is_ok());
    assert!(PreparedGraphMutationText::prepare("MATCH (a) DELETE a", R, symbols).is_err());
}

#[test]
fn parameter_declarations_and_original_offsets_are_preserved() {
    let text = "MATCH (a) WHERE a.p = $value DELETE a";
    let template = PreparedGraphDeleteText::prepare_with_parameter_types(
        text,
        R,
        &[("value", GqlParameterType::Int64)],
        symbols,
    ).unwrap();
    let missing = template.bind_parameters(&GqlParameters::new()).unwrap_err();
    assert_eq!(missing.offset, text.find("$value").unwrap());
    assert!(matches!(missing.kind,
        GraphDeleteTextErrorKind::Query(GraphPatternTextErrorKind::MissingParameter)));
    let wrong = GqlParameters::new().with_uint64("value", 1).unwrap();
    assert!(matches!(template.bind_parameters(&wrong).unwrap_err().kind,
        GraphDeleteTextErrorKind::Query(GraphPatternTextErrorKind::ParameterTypeMismatch { .. })));
    assert!(template.bind_parameters(
        &GqlParameters::new().with_int64("value", 1).unwrap().with_int64("extra", 2).unwrap(),
    ).is_err());
}

#[test]
fn duplicate_target_is_a_typed_build_refusal() {
    let failed = PreparedGraphDeleteText::prepare("MATCH (a) DELETE a,a", R, symbols).unwrap_err();
    assert!(matches!(failed.kind,
        GraphDeleteTextErrorKind::Build(GraphDeleteBuildError::TargetColumn { target: 1, column: 0 })));
}
