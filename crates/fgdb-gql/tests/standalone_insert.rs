//! The relational unit is not a graph scan, sentinel vertex or empty MATCH.
//! Both input kinds enter the same insertion collector and identity contract.

use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::insertion::*;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GqlScalarParameter,
    GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp, GraphMutationValue,
    GraphSymbolKind, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, EId, VId};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
type ResultOf =
    Result<GraphInsertBatch, GqlQueryError<GraphInsertError<&'static str, &'static str>, usize>>;

fn declarations() -> (Vec<GraphInsertVertex>, Vec<GraphInsertEdge>) {
    let value =
        GraphMutationValue::Literal(GqlScalarParameter::new(CanonicalScalar::Int(7)).unwrap());
    (
        vec![
            GraphInsertVertex {
                labels: vec![LabelId(2)],
                properties: vec![(P, value)],
            },
            GraphInsertVertex {
                labels: vec![],
                properties: vec![],
            },
        ],
        vec![
            GraphInsertEdge {
                source: GraphInsertEndpoint::CreatedVertex(0),
                destination: GraphInsertEndpoint::CreatedVertex(1),
                relation: R,
                properties: vec![],
            },
            GraphInsertEdge {
                source: GraphInsertEndpoint::CreatedVertex(1),
                destination: GraphInsertEndpoint::CreatedVertex(1),
                relation: RelationId(2),
                properties: vec![],
            },
        ],
    )
}
fn unit() -> PreparedGraphInsert {
    let (vertices, edges) = declarations();
    PreparedGraphInsert::prepare_standalone(R, vertices, edges).unwrap()
}
fn policy() -> GraphInsertPolicy {
    GraphInsertPolicy::new(GqlQueryPolicy::new(0, 1, 100_000, 100_000), 2, 2)
}
fn no_source(
    _: &PreparedGraphPattern<GraphValueRow>,
    _: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>> {
    panic!("standalone creation must not enter a graph-source callback")
}
fn identity(request: GraphInsertRequest) -> Result<ElementId, &'static str> {
    Ok(match request {
        GraphInsertRequest::Vertex { row, vertex } => {
            ElementId::Vertex(VId(100 + row as u128 * 10 + vertex as u128))
        }
        GraphInsertRequest::Edge { row, edge } => {
            ElementId::Edge(EId(200 + row as u128 * 10 + edge as u128))
        }
    })
}
fn run(plan: &PreparedGraphInsert, policy: GraphInsertPolicy) -> ResultOf {
    plan.execute_governed(policy, no_source, identity, || Ok(()))
}

#[test]
fn unit_creates_exactly_one_structure_without_observing_any_graph_source() {
    let plan = unit();
    assert!(plan.selection().is_none());
    let batch = run(&plan, policy()).unwrap();
    assert_eq!(
        (
            batch.stats().selection.snapshot_records,
            batch.stats().selection.result_rows
        ),
        (0, 1)
    );
    assert_eq!(
        (batch.stats().created_vertices, batch.stats().created_edges),
        (2, 2)
    );
    assert_eq!(
        batch.intents(),
        &[
            GraphInsertIntent::Vertex {
                vertex: VId(100),
                labels: vec![LabelId(2)],
                properties: vec![(P, CanonicalScalar::Int(7))]
            },
            GraphInsertIntent::Vertex {
                vertex: VId(101),
                labels: vec![],
                properties: vec![]
            },
            GraphInsertIntent::Edge {
                edge: EId(200),
                relation: R,
                source: VId(100),
                destination: VId(101),
                properties: vec![]
            },
            GraphInsertIntent::Edge {
                edge: EId(201),
                relation: RelationId(2),
                source: VId(101),
                destination: VId(101),
                properties: vec![]
            },
        ]
    );
    assert_eq!(run(&plan, policy()).unwrap(), batch);
}

#[test]
fn empty_match_and_unit_have_different_cardinality_and_definition_identity() {
    let selection =
        PreparedGraphText::prepare("MATCH (n) RETURN n", |_: GraphSymbolKind, _: &str| None)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
    let (vertices, edges) = declarations();
    let matched = PreparedGraphInsert::prepare(selection, R, vertices, edges).unwrap();
    assert!(matched.selection().is_some());
    assert!(
        matched
            .canonical_bytes()
            .starts_with(b"fgdb:query-graph-insert:v1\0")
    );
    assert!(
        unit()
            .canonical_bytes()
            .starts_with(b"fgdb:standalone-graph-insert:v1\0")
    );
    assert_ne!(matched.canonical_bytes(), unit().canonical_bytes());
    for count in 0..=3_u128 {
        let calls = Cell::new(0);
        let result: ResultOf = matched.execute_governed(
            GraphInsertPolicy::new(GqlQueryPolicy::new(10, 10, 100_000, 100_000), 10, 10),
            |pattern, allowance| {
                calls.set(calls.get() + 1);
                pattern.plan().execute_governed_with_properties(
                    count as u64,
                    (1..=count).map(VId),
                    [],
                    |_, _| Ok(true),
                    |_, _| Ok(None),
                    allowance,
                    || Ok(()),
                )
            },
            identity,
            || Ok(()),
        );
        let batch = result.unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(batch.stats().created_vertices, count as u64 * 2);
        assert_eq!(batch.stats().created_edges, count as u64 * 2);
        if count == 1 {
            assert_eq!(batch.intents(), run(&unit(), policy()).unwrap().intents());
        }
    }
}

#[test]
fn unit_has_no_column_domain_even_in_lazy_branches() {
    let hidden = GraphIntegerExpression::prepare(&[
        GraphIntegerOp::Literal(Some(7)),
        GraphIntegerOp::Column(0),
        GraphIntegerOp::Coalesce,
    ])
    .unwrap();
    for value in [
        GraphMutationValue::Column(0),
        GraphMutationValue::Expression(hidden),
    ] {
        assert!(matches!(
            PreparedGraphInsert::prepare_standalone(
                R,
                vec![GraphInsertVertex {
                    labels: vec![],
                    properties: vec![(P, value)]
                }],
                vec![]
            ),
            Err(GraphInsertBuildError::ValueColumn {
                declaration: 0,
                column: 0
            })
        ));
    }
    let (vertices, mut edges) = declarations();
    edges[0].source = GraphInsertEndpoint::Column(0);
    assert!(matches!(
        PreparedGraphInsert::prepare_standalone(R, vertices, edges),
        Err(GraphInsertBuildError::EndpointColumn { edge: 0, column: 0 })
    ));
    assert!(matches!(
        PreparedGraphInsert::prepare_standalone(R, vec![], vec![]),
        Err(GraphInsertBuildError::Empty)
    ));
}

#[test]
fn standalone_limits_arithmetic_and_cancellation_share_the_original_error_boundaries() {
    let plan = unit();
    let measured = run(&plan, policy()).unwrap().stats();
    let exact = GraphInsertPolicy::new(
        GqlQueryPolicy::new(
            0,
            1,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        ),
        2,
        2,
    );
    assert_eq!(run(&plan, exact).unwrap().stats(), measured);
    for cap in [
        GraphInsertPolicy::new(GqlQueryPolicy::new(0, 0, u64::MAX, u64::MAX), 2, 2),
        GraphInsertPolicy::new(policy().query, 1, 2),
        GraphInsertPolicy::new(policy().query, 2, 1),
    ] {
        let calls = Cell::new(0);
        let result = plan.execute_governed(
            cap,
            no_source,
            |request| {
                calls.set(calls.get() + 1);
                identity(request)
            },
            || Ok(()),
        );
        assert!(result.is_err());
        assert_eq!(calls.get(), 0);
    }
    for cap in [
        GraphInsertPolicy::new(
            GqlQueryPolicy::new(0, 1, measured.evaluator.work_units - 1, u64::MAX),
            2,
            2,
        ),
        GraphInsertPolicy::new(
            GqlQueryPolicy::new(0, 1, u64::MAX, measured.evaluator.scratch_entries - 1),
            2,
            2,
        ),
    ] {
        assert!(run(&plan, cap).is_err());
    }
    let (mut vertices, edges) = declarations();
    vertices[1].properties.push((
        P,
        GraphMutationValue::Expression(
            GraphIntegerExpression::prepare(&[
                GraphIntegerOp::Literal(Some(1)),
                GraphIntegerOp::Literal(Some(0)),
                GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
            ])
            .unwrap(),
        ),
    ));
    let invalid = PreparedGraphInsert::prepare_standalone(R, vertices, edges).unwrap();
    let calls = Cell::new(0);
    let result = invalid.execute_governed(
        policy(),
        no_source,
        |request| {
            calls.set(calls.get() + 1);
            identity(request)
        },
        || Ok(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphInsertError::Arithmetic {
            row: 0,
            declaration: 1,
            ..
        }))
    ));
    assert_eq!(calls.get(), 0);
    let mut events = 0;
    plan.execute_governed(policy(), no_source, identity, || {
        events += 1;
        Ok(())
    })
    .unwrap();
    for stop in 1..=events {
        let mut seen = 0;
        let result = plan.execute_governed(policy(), no_source, identity, || {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(seen, stop);
    }
}
