//! The fast creation owner may hold a prefix only when no later expression
//! or imported endpoint needs that prefix. Exercise the compiler's actual
//! FOREACH/CREATE plans and the typed stored-value seam separately.

use super::*;
use crate::algebra::GraphValue;
use crate::insertion::{GraphInsertEndpoint, GraphInsertVertex};
use crate::{
    GqlParameters, GraphElementKey, GraphMutationValue, GraphSetColumnType, GraphSetProjection,
    GraphSetQuantifier, GraphSetValue, GraphSymbol, GraphSymbolKind, PreparedGraphSet,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::CanonicalScalar;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);

fn prepare(text: &str) -> PreparedGraphWriteChain {
    PreparedGraphWriteChainText::prepare(text, R, |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Label, "N") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "Other") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    })
    .expect(text)
    .bind_parameters(&GqlParameters::new())
    .expect(text)
}

#[test]
fn scalar_creation_loops_and_complete_fresh_subgraphs_are_independent() {
    for text in [
        "FOREACH (x IN [1,2,2] | CREATE (:N {p:x}))",
        "FOREACH (x IN [] | CREATE (:N {p:x}))",
        "FOREACH (x IN [1,2] | CREATE (a:N {p:x})-[:R {p:x}]->(b:N))",
        "FOREACH (x IN [1,2] | CREATE (a:N {p:x}) CREATE (b:N {p:x}))",
        "FOREACH (x IN [1,2] | FOREACH (y IN [x,x+1] | CREATE (:N {p:y})))",
        "UNWIND [1,1,2] AS outer FOREACH (x IN [outer] | CREATE (:N {p:x})) RETURN outer",
    ] {
        let chain = prepare(text);
        assert_eq!(chain.independent_create_relation(), Some(R), "{text}");
    }
}

#[test]
fn observing_or_dependent_loops_keep_the_visible_prefix() {
    for text in [
        "MATCH (n:N) FOREACH (x IN [1,2] | CREATE (:N {p:x}))",
        "FOREACH (x IN [1,2] | CREATE (a:N) CREATE (a)-[:R]->(:N))",
        "FOREACH (x IN [1,2] | CREATE (a:N) SET a.p=x)",
        "FOREACH (x IN [1,2] | MERGE (:N {p:x}))",
        "FOREACH (x IN [1,2] | CREATE (:N)-[:Other]->(:N))",
        "CREATE (n:N {p:1}) FOREACH (x IN [1,2] | CREATE (:N {p:[n][0].p}))",
        "CREATE (n:N {p:1}) FOREACH (x IN [[n][0].p] | CREATE (:N {p:x}))",
        "CREATE (n:N {p:1}) FOREACH (x IN [] | CREATE (:N {p:x})) RETURN [n][0].p",
        "FOREACH (x IN [1,2] | CREATE (:N {p:x})) RETURN count(*)",
    ] {
        let chain = prepare(text);
        assert_eq!(chain.independent_create_relation(), None, "{text}");
    }
}

fn stored(value: GraphSetValue) -> PreparedGraphInsert {
    PreparedGraphInsert::prepare_relation(
        PreparedGraphSet::owned(vec!["v".into()], vec![GraphSetColumnType::Vertex]),
        R,
        vec![GraphInsertVertex {
            labels: vec![],
            properties: vec![(P, GraphMutationValue::Composite(value))],
        }],
        vec![],
    )
    .unwrap()
}

fn property() -> GraphSetValue {
    GraphSetValue::MapGet {
        map: Box::new(GraphSetValue::Column(0)),
        key: "p".into(),
        property: GraphElementKey::Property(P),
    }
}

#[test]
fn stored_properties_need_their_own_effect_proof_even_in_unselected_lazy_arms() {
    let input_only = stored(GraphSetValue::Value(GraphValue::Scalar(
        CanonicalScalar::Int(7),
    )));
    assert!(!input_only.requires_read());
    assert_eq!(input_only.independent_owned_create_relation(), Some(R));
    for value in [
        property(),
        GraphSetValue::Case {
            arms: vec![(
                GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Bool(false))),
                property(),
            )],
            otherwise: Some(Box::new(GraphSetValue::Value(GraphValue::Scalar(
                CanonicalScalar::Int(7),
            )))),
        },
    ] {
        let insertion = stored(value);
        assert!(
            !insertion.requires_read(),
            "the legacy check covers only graph operands"
        );
        assert_eq!(insertion.independent_owned_create_relation(), None);
    }
}

#[test]
fn created_bindings_are_safe_but_computed_return_reads_are_not() {
    let insertion = PreparedGraphInsert::prepare_standalone(
        R,
        vec![GraphInsertVertex {
            labels: vec![],
            properties: vec![],
        }],
        vec![crate::insertion::GraphInsertEdge {
            source: GraphInsertEndpoint::CreatedVertex(0),
            destination: GraphInsertEndpoint::CreatedVertex(0),
            relation: R,
            properties: vec![],
        }],
    )
    .unwrap();
    for (projection, expected) in [(GraphSetValue::Column(0), Some(R)), (property(), None)] {
        let query = PreparedGraphInsertQuery::prepare(
            insertion.clone(),
            vec![crate::GraphInsertBinding::CreatedVertex(0)],
            vec![GraphSetProjection::new("v", projection)],
            GraphSetQuantifier::All,
        )
        .unwrap();
        assert_eq!(query.independent_create_relation(), expected);
    }
}
