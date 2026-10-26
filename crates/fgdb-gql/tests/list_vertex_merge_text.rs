//! List-valued vertex MERGE key admission and immutable parameter binding.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameterType, GqlParameters, GraphSymbol, GraphSymbolKind, GraphWriteChainStep,
    PreparedGraphEdgeMergeText, PreparedGraphVertexMergeText, PreparedGraphWriteChainText,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn arguments(values: Vec<GraphValue>) -> GqlParameters {
    GqlParameters::new().with_list("key", values).unwrap()
}

#[test]
fn list_parameters_bind_freshly_without_reentering_the_catalog() {
    let calls = Cell::new(0);
    let template = PreparedGraphVertexMergeText::prepare_with_parameter_types(
        "MERGE (n:Item {p:$key})",
        R,
        &[("key", GqlParameterType::List)],
        |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        },
    )
    .unwrap();
    let resolved = calls.get();
    let first = template
        .bind_parameters(&arguments(vec![int(1), int(2)]))
        .unwrap();
    let bytes = first.canonical_bytes();
    let second = template.bind_parameters(&arguments(vec![int(3)])).unwrap();
    assert_ne!(bytes, second.canonical_bytes());
    assert_eq!(
        template
            .bind_parameters(&arguments(vec![int(1), int(2)]))
            .unwrap()
            .canonical_bytes(),
        bytes,
    );
    assert_eq!(calls.get(), resolved);
    let literal =
        PreparedGraphVertexMergeText::prepare("MERGE (renamed:Item {p:[1,2]})", R, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
    assert_eq!(literal.canonical_bytes(), bytes);
    assert_eq!(first.creation().vertices_per_row(), 1);
    assert_eq!(first.creation().edges_per_row(), 0);
}

#[test]
fn null_nested_maps_and_oversize_key_arguments_refuse_before_execution() {
    let template = PreparedGraphVertexMergeText::prepare_with_parameter_types(
        "MERGE (n:Item {p:$key})",
        R,
        &[("key", GqlParameterType::List)],
        symbols,
    )
    .unwrap();
    let null = GqlParameters::new()
        .with_scalar("key", CanonicalScalar::Null)
        .unwrap();
    assert!(template.bind_parameters(&null).is_err());
    let map = GraphValue::map(vec![("x".into(), int(1))]).unwrap();
    assert!(template.bind_parameters(&arguments(vec![map])).is_err());
    let large = GraphValue::Scalar(CanonicalScalar::ucs_basic_text(&"x".repeat(65_536)).unwrap());
    assert!(template.bind_parameters(&arguments(vec![large])).is_err());
    let nested =
        PreparedGraphVertexMergeText::prepare("MERGE (n:Item {p:[{x:1}]})", R, symbols).unwrap();
    assert!(nested.bind_parameters(&GqlParameters::new()).is_err());
    assert!(
        PreparedGraphVertexMergeText::prepare("MERGE (n:Item {p:{x:1}})", R, symbols,).is_err()
    );
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());
}

#[test]
fn stored_tuple_binding_requires_every_key_and_preserves_scalar_api_bytes() {
    let chain = PreparedGraphWriteChainText::prepare(
        "UNWIND [[1,2]] AS key MERGE (a:Item {p:key}) MERGE (b:Item {p:0})",
        R,
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let GraphWriteChainStep::VertexMerge(merge) = &chain.steps()[0] else {
        panic!("the first step must be a vertex MERGE");
    };
    let list = GraphValue::List(vec![int(1), int(2)].into_boxed_slice())
        .to_written_property()
        .unwrap();
    assert!(merge.merge_for_stored(&[]).is_err());
    assert!(
        merge
            .merge_for_stored(&[list.clone(), list.clone()])
            .is_err()
    );
    assert!(
        merge
            .merge_for_stored(&[CanonicalPropertyValue::Scalar(CanonicalScalar::Null)])
            .is_err()
    );
    assert!(merge.merge_for_stored(&[list]).is_ok());
    let old = merge.merge_for(&[CanonicalScalar::Int(9)]).unwrap();
    let stored = merge
        .merge_for_stored(&[CanonicalPropertyValue::Scalar(CanonicalScalar::Int(9))])
        .unwrap();
    assert_eq!(old.canonical_bytes(), stored.canonical_bytes());
}

#[test]
fn relationship_merge_keys_keep_the_existing_scalar_boundary() {
    assert!(
        PreparedGraphEdgeMergeText::prepare(
            "MATCH (a:Item), (b:Item) MERGE (a)-[:R {p:[1,2]}]->(b)",
            R,
            symbols,
        )
        .is_err()
    );
}
