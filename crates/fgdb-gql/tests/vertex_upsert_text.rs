//! Native vertex MERGE branch-action frontend laws.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind,
    GraphVertexUpsertBranch, GraphVertexUpsertTextErrorKind, PreparedGraphVertexMergeText,
    PreparedGraphVertexUpsertText,
};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const SEEN: LabelId = LabelId(2);
const CREATED: LabelId = LabelId(3);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Label, "Seen") => Some(GraphSymbol::Label(SEEN)),
        (GraphSymbolKind::Label, "Created") => Some(GraphSymbol::Label(CREATED)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

#[test]
fn one_parameter_table_spans_merge_key_and_both_action_branches() {
    let text = "MERGE (n:Person {p:$p}) ON MATCH SET n.q=$matched,n:Seen ON CREATE SET n.q=$created,n:Created";
    let calls = Cell::new(0);
    let template = PreparedGraphVertexUpsertText::prepare(text, R, |kind, name| {
        calls.set(calls.get() + 1); symbols(kind, name)
    }).unwrap();
    let resolved = calls.get();
    assert_eq!(template.statement(), text);
    assert_eq!(template.parameter_schema().len(), 3);
    let args = GqlParameters::new().with_int64("p", 7).unwrap()
        .with_int64("matched", 100).unwrap().with_int64("created", 200).unwrap();
    let bound = template.bind_parameters(&args).unwrap();
    assert_eq!(calls.get(), resolved, "binding must not re-enter catalog");
    assert_eq!(bound.on_match().len(), 2);
    assert_eq!(bound.on_create().len(), 2);
    let frozen = bound.canonical_bytes();
    assert_eq!(template.bind_parameters(&args).unwrap().canonical_bytes(), frozen);
    assert!(!format!("{template:?} {bound:?}").contains("matched"));

    let reversed = PreparedGraphVertexUpsertText::prepare(
        "MERGE (x:Person {p:$p}) ON CREATE SET x.q=$created,x:Created ON MATCH SET x.q=$matched,x:Seen",
        R, symbols,
    ).unwrap().bind_parameters(&args).unwrap();
    // Branch order in source does not change branch semantics.
    assert_eq!(bound.canonical_bytes(), reversed.canonical_bytes());
}

#[test]
fn duplicate_branch_wrong_target_and_expression_assignment_refuse() {
    for (text, expected) in [
        (
            "MERGE (n:Person {p:1}) ON MATCH SET n.q=1 ON MATCH SET n:Seen",
            Some(GraphVertexUpsertTextErrorKind::DuplicateBranch),
        ),
        (
            "MERGE (n:Person {p:1}) ON CREATE SET other.q=1",
            None,
        ),
        (
            "MERGE (n:Person {p:1}) ON CREATE SET n.q=n.q+1",
            None,
        ),
        (
            "MERGE (n:Person {p:1}) ON DELETE SET n.q=1",
            None,
        ),
    ] {
        let failed = PreparedGraphVertexUpsertText::prepare(text, R, symbols).unwrap_err();
        if let Some(expected) = expected { assert_eq!(failed.kind, expected); }
    }
    assert!(PreparedGraphVertexMergeText::prepare(
        "MERGE (n:Person {p:1}) ON CREATE SET n.q=1", R, symbols,
    ).is_err(), "plain MERGE entrypoint must not silently discard branch clauses");
}

#[test]
fn branch_duplicate_fields_are_rejected_by_the_typed_upsert_definition() {
    let failed = PreparedGraphVertexUpsertText::prepare(
        "MERGE (n:Person {p:1}) ON MATCH SET n.q=1,n.q=2", R, symbols,
    ).unwrap().bind_parameters(&GqlParameters::new()).unwrap_err();
    assert!(matches!(failed.kind,
        GraphVertexUpsertTextErrorKind::UpsertBuild(
            fgdb_gql::GraphVertexUpsertBuildError::DuplicateProperty {
                branch: GraphVertexUpsertBranch::Match
            }
        )));
}

#[test]
fn missing_and_wrong_action_arguments_retain_original_offsets() {
    let text = "MERGE (n:Person {p:$p}) ON MATCH SET n.q=$value";
    let template = PreparedGraphVertexUpsertText::prepare(text, R, symbols).unwrap();
    let missing = template.bind_parameters(&GqlParameters::new().with_int64("p", 1).unwrap()).unwrap_err();
    assert_eq!(missing.offset, text.find("$value").unwrap());
    assert!(matches!(missing.kind,
        GraphVertexUpsertTextErrorKind::Query(GraphPatternTextErrorKind::MissingParameter)));
    let wrong = GqlParameters::new().with_int64("p", 1).unwrap().with_uint64("value", 2).unwrap();
    assert!(matches!(template.bind_parameters(&wrong).unwrap_err().kind,
        GraphVertexUpsertTextErrorKind::Query(GraphPatternTextErrorKind::ParameterTypeMismatch { .. })));
}
