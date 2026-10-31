//! Native bounded vertex MERGE frontend laws.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::GraphInsertBuildError;
use fgdb_gql::{
    GqlParameterType, GqlParameters, GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind,
    GraphVertexMergeTextErrorKind, PreparedGraphVertexMergeText,
};
use fgdb_types::{CanonicalScalar, CanonicalScalarKind};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const NAME: PropertyKeyId = PropertyKeyId(2);
const ACTIVE: PropertyKeyId = PropertyKeyId(3);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(NAME)),
        (GraphSymbolKind::Property, "active") => Some(GraphSymbol::Property(ACTIVE)),
        _ => None,
    }
}

#[test]
fn integer_scalar_and_literal_values_share_match_and_creation_binding() {
    let text = "MERGE (n:Person {p:$p,name:$name,active:TRUE})";
    let text_kind = CanonicalScalarKind::of(&CanonicalScalar::ucs_basic_text("x").unwrap());
    let calls = Cell::new(0);
    let template = PreparedGraphVertexMergeText::prepare_with_parameter_types(
        text,
        R,
        &[("name", GqlParameterType::Scalar(text_kind))],
        |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        },
    )
    .unwrap();
    let resolved = calls.get();
    assert_eq!(template.statement(), text);
    assert_eq!(template.parameter_schema().len(), 2);
    let args = GqlParameters::new()
        .with_int64("p", 7)
        .unwrap()
        .with_text("name", "Ada")
        .unwrap();
    let first = template.bind_parameters(&args).unwrap();
    let frozen = first.canonical_bytes();
    assert_eq!(
        calls.get(),
        resolved,
        "binding must not re-enter the catalog"
    );
    assert_eq!(first.creation().vertices_per_row(), 1);
    assert_eq!(first.creation().edges_per_row(), 0);
    assert_eq!(first.target_column(), 0);
    assert_eq!(
        template.bind_parameters(&args).unwrap().canonical_bytes(),
        frozen
    );
    assert!(!format!("{template:?} {first:?}").contains("Ada"));

    let renamed = PreparedGraphVertexMergeText::prepare_with_parameter_types(
        "MERGE (renamed:Person {p:$p,name:$name,active:TRUE})",
        R,
        &[("name", GqlParameterType::Scalar(text_kind))],
        symbols,
    )
    .unwrap()
    .bind_parameters(&args)
    .unwrap();
    assert_eq!(
        first.canonical_bytes(),
        renamed.canonical_bytes(),
        "variable spelling is not semantic"
    );
}

#[test]
fn null_keys_wrong_types_and_extra_arguments_refuse_at_original_parameter() {
    let text = "MERGE (n:Person {name:$name})";
    let text_kind = CanonicalScalarKind::of(&CanonicalScalar::ucs_basic_text("x").unwrap());
    let template = PreparedGraphVertexMergeText::prepare_with_parameter_types(
        text,
        R,
        &[("name", GqlParameterType::Scalar(text_kind))],
        symbols,
    )
    .unwrap();
    let missing = template.bind_parameters(&GqlParameters::new()).unwrap_err();
    assert_eq!(missing.offset, text.find("$name").unwrap());
    assert!(matches!(
        missing.kind,
        GraphVertexMergeTextErrorKind::Query(GraphPatternTextErrorKind::MissingParameter)
    ));
    let wrong = GqlParameters::new().with_int64("name", 1).unwrap();
    assert!(matches!(
        template.bind_parameters(&wrong).unwrap_err().kind,
        GraphVertexMergeTextErrorKind::Query(
            GraphPatternTextErrorKind::ParameterTypeMismatch { .. }
        )
    ));
    let null = GqlParameters::new()
        .with_scalar("name", CanonicalScalar::Null)
        .unwrap();
    assert!(matches!(
        template.bind_parameters(&null).unwrap_err().kind,
        GraphVertexMergeTextErrorKind::Query(GraphPatternTextErrorKind::Expected(
            "non-null MERGE property value"
        ))
    ));
    let extra = GqlParameters::new()
        .with_text("name", "Ada")
        .unwrap()
        .with_int64("extra", 1)
        .unwrap();
    assert!(template.bind_parameters(&extra).is_err());
}

#[test]
fn malformed_duplicate_and_relationship_merge_syntax_refuse_before_catalog_access() {
    for text in [
        "MERGE",
        "MERGE ()",
        "MERGE (n:Person:Person)",
        "MERGE (n:Person {p:1,p:2})",
        "MERGE (n:Person {})",
        "MERGE (n:Person {p:NULL})",
        "MERGE (a)-[:R]->(b)",
        "MERGE (n:Person {p:1}) ON CREATE SET n.p=2",
        "MATCH (n) MERGE (n)",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphVertexMergeText::prepare(text, R, |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "malformed MERGE reached catalog: {text}");
    }
    let duplicate = PreparedGraphVertexMergeText::prepare("MERGE (n:Person {p:1,p:2})", R, symbols)
        .unwrap_err();
    assert!(matches!(
        duplicate.kind,
        GraphVertexMergeTextErrorKind::InsertBuild(GraphInsertBuildError::DuplicateProperty {
            declaration: 0
        })
    ));
}

#[test]
fn row_merge_target_reuse_requires_every_branch_to_preserve_keys_and_visibility() {
    use fgdb_gql::{
        GraphSymbolResolver, GraphWriteChainStep, PreparedGraphWriteChainText, ReverseSymbolCatalog,
    };

    struct Catalog;
    impl GraphSymbolResolver for Catalog {
        fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
            symbols(kind, name)
        }

        fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
            let mut catalog = ReverseSymbolCatalog::new();
            catalog.insert_property(P, "p");
            catalog.insert_property(NAME, "name");
            catalog.insert_property(ACTIVE, "active");
            Some(catalog)
        }
    }

    for (pattern, actions, expected) in [
        ("p:k", "", true),
        (
            "p:k",
            "ON CREATE SET n.active=0 ON MATCH SET n.active=n.active+1",
            true,
        ),
        ("p:k", "ON CREATE SET n.name=[1,2] SET n.active=k+1", true),
        ("p:k,active:7", "SET n.name='payload'", true),
        ("p:k", "ON CREATE SET n.p=2", false),
        ("p:k", "ON MATCH SET n.p=n.p+1", false),
        ("p:k", "ON CREATE SET n.active=0 SET n.p=k", false),
        ("p:k,active:7", "ON MATCH SET n.active=8", false),
        ("p:k", "ON CREATE SET n:Person", false),
        ("p:k", "ON MATCH SET n:Person", false),
        ("p:k", "SET n={active:k}", false),
        ("p:k", "ON MATCH SET n += {active:k}", false),
    ] {
        let text = format!("UNWIND [1,1] AS k MERGE (n:Person {{{pattern}}}) {actions} RETURN n");
        let chain = PreparedGraphWriteChainText::prepare(&text, R, Catalog)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .bind_parameters(&GqlParameters::new())
            .unwrap_or_else(|error| panic!("{text}: {error:?}"));
        let [GraphWriteChainStep::VertexMergeQuery(query)] = chain.steps() else {
            panic!("expected one row MERGE query: {text}");
        };
        assert_eq!(query.merge().preserves_match_targets(), expected, "{text}");
    }
}
