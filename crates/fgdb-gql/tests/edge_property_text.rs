//! Fixed-length edge property binding and identity-domain separation.
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_gql::algebra::GraphValue;
use fgdb_types::{CanonicalScalar, EId, VId};

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}
#[test]
fn fixed_edge_properties_use_eid_and_preserve_missing_null() {
    let text = PreparedGraphText::prepare("MATCH (a)-[r:R]->(b) RETURN r.p AS p ORDER BY p", symbols).unwrap();
    let prepared = text.bind_parameters(&GqlParameters::new()).unwrap();
    let value = CanonicalScalar::Int(2003);
    let wrong_domain = CanonicalScalar::Int(99);
    let result = prepared.plan().execute_governed_with_element_properties(
        2, [VId(1), VId(2), VId(3)],
        [(EId(1), VId(1), RelationId(1), VId(2)), (EId(2), VId(1), RelationId(1), VId(3))],
        |_, _| Ok::<_, ()>(true), |_, _| Ok(Some(&wrong_domain)),
        |eid, _| Ok((eid == EId(1)).then_some(&value)),
        GqlQueryPolicy::new(2, 10, 10000, 10000), || Ok::<_, ()>(()),
    ).unwrap();
    assert_eq!(result.value.iter().map(|row| row.values().to_vec()).collect::<Vec<_>>(),
        vec![vec![GraphValue::Scalar(value)], vec![GraphValue::Scalar(CanonicalScalar::Null)]]);
}
#[test]
fn edge_property_source_errors_are_not_null() {
    let text = PreparedGraphText::prepare("MATCH (a)-[r:R]->(b) RETURN r.p AS p LIMIT 0", symbols).unwrap();
    let prepared = text.bind_parameters(&GqlParameters::new()).unwrap();
    let result = prepared.plan().execute_governed_with_element_properties(
        1, [VId(1), VId(2)], [(EId(1), VId(1), RelationId(1), VId(2))],
        |_, _| Ok::<_, &'static str>(true), |_, _| Ok(None), |_, _| Err("unreadable edge payload"),
        GqlQueryPolicy::new(1, 10, 10000, 10000), || Ok::<_, ()>(()),
    );
    assert!(matches!(result, Err(GqlQueryError::Source("unreadable edge payload"))));
}
