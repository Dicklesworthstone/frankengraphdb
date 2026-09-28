//! MATCH-selected creation keeps the native input, resource and failure laws.
//! Failures never return a successful prefix of private creation proposals.

use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::insertion::{
    GraphInsertError, GraphInsertIntent, GraphInsertPolicy, GraphInsertRequest,
};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphInsertQueryBatch,
    GraphInsertQueryError, GraphSetColumnType, GraphSymbol, GraphSymbolKind,
    PreparedGraphInsertQuery, PreparedGraphInsertQueryText,
};
use fgdb_types::{CanonicalScalar, EId, VId};
use std::cell::Cell;
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const COPY: LabelId = LabelId(9);
type ResultOf = Result<GraphInsertQueryBatch, GqlQueryError<GraphInsertQueryError<(), ()>, ()>>;
type Props = BTreeMap<(VId, PropertyKeyId), CanonicalScalar>;
type Triple = (VId, RelationId, VId);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Copy") => Some(GraphSymbol::Label(COPY)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn policy() -> GraphInsertPolicy {
    GraphInsertPolicy::new(
        GqlQueryPolicy::new(0, 1_000, 2_000_000, 1_000_000),
        1_000,
        1_000,
    )
}
fn identity(request: GraphInsertRequest) -> Result<ElementId, ()> {
    Ok(match request {
        GraphInsertRequest::Vertex { row, vertex } => {
            ElementId::Vertex(VId(100 + row as u128 * 16 + vertex as u128))
        }
        GraphInsertRequest::Edge { row, edge } => {
            ElementId::Edge(EId(1_000 + row as u128 * 16 + edge as u128))
        }
    })
}
fn prepare(text: &str) -> PreparedGraphInsertQuery {
    PreparedGraphInsertQueryText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn matched_policy() -> GraphInsertPolicy {
    GraphInsertPolicy::new(
        GqlQueryPolicy::new(1_000, 1_000, 2_000_000, 1_000_000),
        1_000,
        1_000,
    )
}
fn match_rows(
    selection: &PreparedGraphPattern<GraphValueRow>,
    allowance: GqlQueryPolicy,
    vertices: &[VId],
    edges: &[Triple],
    props: &Props,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), ()>> {
    selection.plan().execute_governed_with_properties(
        (vertices.len() + edges.len()) as u64,
        vertices.iter().copied(),
        edges.iter().copied(),
        |vid, predicates| {
            Ok(predicates.iter().all(|predicate| {
                predicate.matches_borrowed(
                    [],
                    props.iter().filter_map(|(&(owner, key), value)| {
                        (owner == vid).then_some((key, value))
                    }),
                )
            }))
        },
        |vid, key| Ok(props.get(&(vid, key))),
        allowance,
        || Ok(()),
    )
}
fn values(batch: &GraphInsertQueryBatch) -> Vec<Vec<GraphValue>> {
    batch
        .returning()
        .value
        .iter()
        .map(|row| row.values().to_vec())
        .collect()
}

#[test]
fn null_matched_endpoints_refuse_before_any_identity_is_allocated() {
    let query = prepare(
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) CREATE (b)-[e:R]->(n) RETURN n,e",
    );
    let result: ResultOf = query.execute_governed(
        matched_policy(),
        |selection, allowance| {
            match_rows(selection, allowance, &[VId(1)], &[], &Props::new())
        },
        |_| panic!("the null endpoint must be checked before identity allocation"),
        || Ok(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphInsertQueryError::Insertion(
            GraphInsertError::NullEndpoint { .. }
        )))
    ));
}

#[test]
fn matched_late_return_failure_and_cancellation_never_return_a_partial_proposal() {
    let vertices = [VId(1), VId(2)];
    let props = Props::from([
        ((VId(1), P), CanonicalScalar::Int(2)),
        ((VId(2), P), CanonicalScalar::Int(1)),
    ]);
    let query = prepare("MATCH (a) CREATE (n) RETURN 1/(a.p-1) AS value LIMIT 0");
    let allocations = Cell::new(0);
    let result: ResultOf = query.execute_governed(
        matched_policy(),
        |selection, allowance| match_rows(selection, allowance, &vertices, &[], &props),
        |request| {
            allocations.set(allocations.get() + 1);
            identity(request)
        },
        || Ok(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphInsertQueryError::Returning(_)))
    ));
    assert_eq!(allocations.get(), 2, "RETURN fails only after complete creation");

    allocations.set(0);
    let result: ResultOf = prepare("MATCH (a) CREATE (n) RETURN a,n").execute_governed(
        matched_policy(),
        |selection, allowance| match_rows(selection, allowance, &vertices, &[], &props),
        |request| {
            allocations.set(allocations.get() + 1);
            identity(request)
        },
        || if allocations.get() == 0 { Ok(()) } else { Err(()) },
    );
    assert!(matches!(result, Err(GqlQueryError::Interrupted(()))));
    assert!(allocations.get() > 0, "exercise cancellation after allocation starts");
}

#[test]
fn matched_source_and_creation_limits_fail_before_allocation_but_output_limit_is_final() {
    let vertices = [VId(1), VId(2)];
    let query = prepare("MATCH (a) CREATE (n) RETURN n");
    let mut limited = matched_policy();
    limited.max_vertices = 1;
    let result: ResultOf = query.execute_governed(
        limited,
        |selection, allowance| {
            match_rows(selection, allowance, &vertices, &[], &Props::new())
        },
        |_| panic!("all creation counts must be admitted before allocation"),
        || Ok(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphInsertQueryError::Insertion(
            GraphInsertError::Limit { .. }
        )))
    ));
    let result: ResultOf = query.execute_governed(
        policy(), // The standalone allowance admits zero snapshot records.
        |selection, allowance| {
            match_rows(selection, allowance, &vertices, &[], &Props::new())
        },
        |_| panic!("source admission must fail before allocation"),
        || Ok(()),
    );
    assert!(matches!(result, Err(GqlQueryError::Rows(_))));

    limited = matched_policy();
    limited.query = GqlQueryPolicy::new(1_000, 1, 2_000_000, 1_000_000);
    let allocations = Cell::new(0);
    let result: ResultOf = query.execute_governed(
        limited,
        |selection, allowance| {
            match_rows(selection, allowance, &vertices, &[], &Props::new())
        },
        |request| {
            allocations.set(allocations.get() + 1);
            identity(request)
        },
        || Ok(()),
    );
    assert!(matches!(result, Err(GqlQueryError::Rows(_))));
    assert_eq!(allocations.get(), 2, "the final row limit cannot truncate CREATE");
}

#[test]
fn matched_edge_results_keep_edge_identity_and_property_domains_in_creation_and_return() {
    let query = prepare(
        "MATCH (a)-[r:R]->(b) CREATE (n {p:r.p}) \
        RETURN r,r.p AS original,n,n.p AS copied ORDER BY r",
    );
    assert_eq!(
        query.column_types(),
        &[
            GraphSetColumnType::Edge,
            GraphSetColumnType::Scalar,
            GraphSetColumnType::Vertex,
            GraphSetColumnType::Scalar,
        ]
    );
    let value = CanonicalScalar::Int(2003);
    let wrong_domain = CanonicalScalar::Int(99);
    let batch = query
        .execute_governed(
            matched_policy(),
            |selection, allowance| {
                selection.plan().execute_governed_with_element_properties(
                    5,
                    [VId(1), VId(2), VId(3)],
                    [(EId(1), VId(1), R, VId(2)), (EId(2), VId(1), R, VId(3))],
                    |_, _| Ok::<_, ()>(true),
                    |_, _| Ok(Some(&wrong_domain)),
                    |edge, _| Ok((edge == EId(1)).then_some(&value)),
                    allowance,
                    || Ok::<_, ()>(()),
                )
            },
            identity,
            || Ok(()),
        )
        .unwrap();
    let rows = values(&batch);
    assert_eq!(rows.len(), 2);
    for (at, property) in [value, CanonicalScalar::Null].into_iter().enumerate() {
        assert_eq!(rows[at][0], GraphValue::Edge(EId(at as u128 + 1)));
        assert_eq!(rows[at][1], GraphValue::Scalar(property.clone()));
        assert_eq!(rows[at][3], GraphValue::Scalar(property.clone()));
        let GraphValue::Vertex(created) = &rows[at][2] else {
            panic!("the created vertex must not be confused with the matched edge");
        };
        assert!(batch.insertion().intents().contains(&GraphInsertIntent::Vertex {
            vertex: *created,
            labels: vec![],
            properties: vec![(P, property)],
        }));
    }
    assert_ne!(rows[0][2], rows[1][2]);
    assert_eq!(batch.insertion().stats().created_vertices, 2);
}

#[test]
fn matched_edge_return_requires_identified_input_and_preserves_source_failures() {
    let query = prepare("MATCH (a)-[r:R]->(b) CREATE (n) RETURN r.p AS p");
    let result: ResultOf = query.execute_governed(
        matched_policy(),
        |selection, allowance| {
            match_rows(
                selection,
                allowance,
                &[VId(1), VId(2)],
                &[(VId(1), R, VId(2))],
                &Props::new(),
            )
        },
        |_| panic!("an unidentified edge cannot be guessed from its endpoints"),
        || Ok(()),
    );
    assert!(matches!(result, Err(GqlQueryError::IdentifiedEdgesRequired)));

    let result: ResultOf = query.execute_governed(
        matched_policy(),
        |selection, allowance| {
            selection.plan().execute_governed_with_element_properties(
                3,
                [VId(1), VId(2)],
                [(EId(1), VId(1), R, VId(2))],
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                |_, _| Err(()),
                allowance,
                || Ok::<_, ()>(()),
            )
        },
        |_| panic!("an unreadable source property must fail before allocation"),
        || Ok(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphInsertQueryError::Insertion(
            GraphInsertError::Source(())
        )))
    ));
}
