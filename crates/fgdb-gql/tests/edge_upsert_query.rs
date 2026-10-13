//! Checked native RETURN definitions for the relationship actually merged.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphOrderError, GraphValueOrder};
use fgdb_gql::{
    GqlParameters, GraphEdgeReturnBinding, GraphEdgeUpsertQueryBuildError, GraphSetColumnType,
    GraphSetProjection, GraphSetQuantifier, GraphSetValue, GraphSymbol, GraphSymbolKind,
    PreparedGraphEdgeUpsert, PreparedGraphEdgeUpsertQuery, PreparedGraphEdgeUpsertText,
};

fn upsert() -> PreparedGraphEdgeUpsert {
    PreparedGraphEdgeUpsertText::prepare(
        "MATCH (a),(b) MERGE (a)-[e:R]->(b) ON CREATE SET e.p=817239",
        RelationId(1),
        |kind, name: &str| match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        },
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}

fn query() -> PreparedGraphEdgeUpsertQuery {
    PreparedGraphEdgeUpsertQuery::prepare(
        upsert(),
        vec![
            GraphEdgeReturnBinding::Edge,
            GraphEdgeReturnBinding::Property(PropertyKeyId(1)),
        ],
        vec![
            GraphSetProjection::new("edge", GraphSetValue::Column(0)),
            GraphSetProjection::new("value", GraphSetValue::Column(1)),
        ],
        GraphSetQuantifier::All,
    )
    .unwrap()
}

#[test]
fn return_schema_preserves_edge_identity_and_final_property_slots() {
    let query = query();
    assert_eq!(query.columns(), &["edge", "value"]);
    assert_eq!(
        query.column_types(),
        &[GraphSetColumnType::Edge, GraphSetColumnType::Stored]
    );
    assert_eq!(query.upsert(), &upsert());
    assert_eq!(query.bindings().len(), 2);
    assert!(!format!("{query:?}").contains("817239"));
}

#[test]
fn malformed_projection_is_rejected_before_the_host_can_write() {
    for projection in [
        vec![],
        vec![GraphSetProjection::new("bad", GraphSetValue::Column(1))],
        vec![
            GraphSetProjection::new("duplicate", GraphSetValue::Column(0)),
            GraphSetProjection::new("duplicate", GraphSetValue::Column(0)),
        ],
    ] {
        assert!(matches!(
            PreparedGraphEdgeUpsertQuery::prepare(
                upsert(),
                vec![GraphEdgeReturnBinding::Edge],
                projection,
                GraphSetQuantifier::All,
            ),
            Err(GraphEdgeUpsertQueryBuildError::Projection(_))
        ));
    }
    let limit = fgdb_gql::algebra::MAX_PATTERN_VERTICES;
    assert!(matches!(
        PreparedGraphEdgeUpsertQuery::prepare(
            upsert(),
            vec![GraphEdgeReturnBinding::Edge; limit + 1],
            vec![GraphSetProjection::new("edge", GraphSetValue::Column(0))],
            GraphSetQuantifier::All,
        ),
        Err(GraphEdgeUpsertQueryBuildError::TooManyBindings { .. })
    ));
}

#[test]
fn order_validation_is_over_the_final_return_schema() {
    let key = GraphValueOrder {
        column: 1,
        descending: true,
        nulls_first: false,
    };
    assert!(query().with_order_by(&[key]).is_ok());
    assert!(matches!(
        query().with_order_by(&[]),
        Err(GraphOrderError::EmptyOrder)
    ));
    assert!(matches!(
        query().with_order_by(&[key, key]),
        Err(GraphOrderError::DuplicateColumn { column: 1 })
    ));
    assert!(matches!(
        query().with_order_by(&[GraphValueOrder { column: 2, ..key }]),
        Err(GraphOrderError::UnknownColumn { column: 2 })
    ));
}

#[test]
fn canonical_identity_includes_pagination_order_and_selected_fields() {
    let query = query();
    let original = query.canonical_bytes();
    assert_eq!(original, query.clone().canonical_bytes());
    assert_ne!(
        original,
        query.clone().with_page(0, Some(0)).canonical_bytes()
    );
    assert_ne!(original, query.clone().with_page(1, None).canonical_bytes());
    assert_ne!(
        original,
        query
            .clone()
            .with_order_by(&[GraphValueOrder {
                column: 1,
                descending: true,
                nulls_first: false,
            }])
            .unwrap()
            .canonical_bytes()
    );
    let changed = PreparedGraphEdgeUpsertQuery::prepare(
        upsert(),
        vec![
            GraphEdgeReturnBinding::Edge,
            GraphEdgeReturnBinding::Property(PropertyKeyId(2)),
        ],
        vec![
            GraphSetProjection::new("edge", GraphSetValue::Column(0)),
            GraphSetProjection::new("value", GraphSetValue::Column(1)),
        ],
        GraphSetQuantifier::All,
    )
    .unwrap();
    assert_ne!(original, changed.canonical_bytes());
}
