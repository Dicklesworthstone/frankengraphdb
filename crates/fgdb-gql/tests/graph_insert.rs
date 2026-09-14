//! Creation uses public GLA rows and the shared scalar VM, with an independent
//! occurrence-to-identity oracle. No storage or identity allocator is mocked as
//! durable here; the database suite exercises the actual staging boundary.

use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::insertion::*;
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp, GraphMutationValue,
    GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, EId, VId};
use std::cell::Cell;
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
type ResultOf = Result<GraphInsertBatch, GqlQueryError<GraphInsertError<&'static str, &'static str>, usize>>;
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn select(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn plan(selection: PreparedGraphPattern<GraphValueRow>) -> PreparedGraphInsert {
    let increment = GraphIntegerExpression::prepare(&[
        GraphIntegerOp::Column(2), GraphIntegerOp::Literal(Some(1)),
        GraphIntegerOp::Binary(GraphIntegerBinary::Add),
    ]).unwrap();
    PreparedGraphInsert::prepare(selection, R,
        vec![GraphInsertVertex { labels: vec![LabelId(9)], properties: vec![
            (Q, GraphMutationValue::Expression(increment)), (P, GraphMutationValue::Column(2)),
        ] }],
        vec![
            GraphInsertEdge { source: GraphInsertEndpoint::Column(0), destination: GraphInsertEndpoint::CreatedVertex(0), properties: vec![] },
            GraphInsertEdge { source: GraphInsertEndpoint::CreatedVertex(0), destination: GraphInsertEndpoint::Column(1),
                properties: vec![(P, GraphMutationValue::Column(2))] },
        ],
    ).unwrap()
}
fn standard() -> PreparedGraphInsert {
    plan(select("MATCH (n)-[:R]->(m) RETURN n,m,n.p AS p"))
}
fn policy() -> GraphInsertPolicy {
    GraphInsertPolicy::new(GqlQueryPolicy::new(100, 100, 1_000_000, 1_000_000), 100, 100)
}
fn identity(request: GraphInsertRequest) -> Result<ElementId, &'static str> {
    Ok(match request {
        GraphInsertRequest::Vertex { row, vertex } => ElementId::Vertex(VId((1_u128 << 100) + row as u128 * 256 + vertex as u128)),
        GraphInsertRequest::Edge { row, edge } => ElementId::Edge(EId((1_u128 << 110) + row as u128 * 256 + edge as u128)),
    })
}
fn source(plan: &PreparedGraphPattern<GraphValueRow>, allowance: GqlQueryPolicy,
    edges: &[(VId, RelationId, VId)], values: &BTreeMap<VId, CanonicalScalar>)
    -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>> {
    plan.plan().execute_governed_with_properties((3 + edges.len()) as u64,
        [VId(1), VId(2), VId(3)], edges.iter().copied(), |_, _| Ok(true),
        |vid, _| Ok(values.get(&vid)), allowance, || Ok(()))
}
fn edges() -> Vec<(VId, RelationId, VId)> {
    vec![(VId(1), R, VId(2)), (VId(1), R, VId(2)), (VId(1), R, VId(3)), (VId(2), R, VId(3))]
}
fn values() -> BTreeMap<VId, CanonicalScalar> {
    BTreeMap::from([(VId(1), CanonicalScalar::Int(10)), (VId(2), CanonicalScalar::Int(20))])
}
fn run(plan: &PreparedGraphInsert, policy: GraphInsertPolicy) -> ResultOf {
    plan.execute_governed(policy, |plan, allowance| source(plan, allowance, &edges(), &values()), identity, || Ok(()))
}

#[test]
fn repeated_matches_create_distinct_row_local_structures_with_frozen_properties() {
    let result = run(&standard(), policy()).unwrap();
    assert_eq!((result.stats().selection.result_rows, result.stats().created_vertices, result.stats().created_edges), (4, 4, 8));
    let mut expected = Vec::new();
    for (row, (source, _, destination)) in edges().into_iter().enumerate() {
        let ElementId::Vertex(vertex) = identity(GraphInsertRequest::Vertex { row, vertex: 0 }).unwrap() else { unreachable!() };
        let value = if source == VId(1) { 10 } else { 20 };
        expected.push(GraphInsertIntent::Vertex { vertex, labels: vec![LabelId(9)],
            properties: vec![(P, CanonicalScalar::Int(value)), (Q, CanonicalScalar::Int(value + 1))] });
        for (edge, from, to, properties) in [
            (0, source, vertex, vec![]), (1, vertex, destination, vec![(P, CanonicalScalar::Int(value))]),
        ] {
            let ElementId::Edge(id) = identity(GraphInsertRequest::Edge { row, edge }).unwrap() else { unreachable!() };
            expected.push(GraphInsertIntent::Edge { edge: id, source: from, destination: to, properties });
        }
    }
    assert_eq!(result.intents(), expected);
    assert_eq!(run(&standard(), policy()).unwrap(), result);
    assert!(!format!("{result:?}").contains("1267650600228229401496703205376"));
}

#[test]
fn declarations_validate_endpoints_and_all_expression_columns_before_execution() {
    let input = select("MATCH (n) RETURN n,n.p AS p");
    let vertex = GraphInsertVertex { labels: vec![], properties: vec![] };
    for endpoint in [GraphInsertEndpoint::Column(1), GraphInsertEndpoint::Column(9), GraphInsertEndpoint::CreatedVertex(1)] {
        assert!(PreparedGraphInsert::prepare(input.clone(), R, vec![vertex.clone()], vec![GraphInsertEdge {
            source: GraphInsertEndpoint::Column(0), destination: endpoint, properties: vec![],
        }]).is_err());
    }
    let conditional = GraphIntegerExpression::prepare(&[
        GraphIntegerOp::Literal(Some(7)), GraphIntegerOp::Column(0), GraphIntegerOp::Coalesce,
    ]).unwrap();
    assert!(matches!(PreparedGraphInsert::prepare(input.clone(), R,
        vec![GraphInsertVertex { labels: vec![], properties: vec![(P, GraphMutationValue::Expression(conditional))] }], vec![]),
        Err(GraphInsertBuildError::ValueColumn { column: 0, .. })));
    assert!(matches!(PreparedGraphInsert::prepare(input.clone(), R,
        vec![GraphInsertVertex { labels: vec![LabelId(1), LabelId(1)], properties: vec![] }], vec![]),
        Err(GraphInsertBuildError::DuplicateLabel { .. })));
    assert!(matches!(PreparedGraphInsert::prepare(input.clone(), R,
        vec![GraphInsertVertex { labels: vec![], properties: vec![(P, GraphMutationValue::Column(1)), (P, GraphMutationValue::Column(1))] }], vec![]),
        Err(GraphInsertBuildError::DuplicateProperty { .. })));
    assert!(matches!(PreparedGraphInsert::prepare(input.clone(), R, vec![], vec![]), Err(GraphInsertBuildError::Empty)));
    assert!(matches!(PreparedGraphInsert::prepare(input, R, vec![vertex; MAX_GRAPH_INSERT_DECLARATIONS + 1], vec![]),
        Err(GraphInsertBuildError::TooManyDeclarations { .. })));
}

#[test]
fn null_endpoints_and_late_bad_values_refuse_before_any_identity_request() {
    let calls = Cell::new(0);
    let optional = plan(select("MATCH (n) OPTIONAL MATCH (n)-[:R]->(m) RETURN n,m,n.p AS p"));
    let result = optional.execute_governed(policy(), |plan, allowance| source(plan, allowance, &edges(), &values()),
        |request| { calls.set(calls.get() + 1); identity(request) }, || Ok(()));
    assert!(matches!(result, Err(GqlQueryError::Source(GraphInsertError::NullEndpoint { .. }))));
    assert_eq!(calls.get(), 0);
    for bad in [CanonicalScalar::Int(i64::MAX), CanonicalScalar::Bool(true)] {
        let mut inputs = values(); inputs.insert(VId(2), bad);
        let result = standard().execute_governed(policy(), |plan, allowance| source(plan, allowance, &edges(), &inputs),
            |request| { calls.set(calls.get() + 1); identity(request) }, || Ok(()));
        assert!(matches!(result, Err(GqlQueryError::Source(GraphInsertError::Arithmetic { row: 3, .. }))));
        assert_eq!(calls.get(), 0);
    }
}

#[test]
fn allocator_failures_wrong_domains_and_repeated_ids_never_return_partial_batches() {
    let count = Cell::new(0);
    let repeated = standard().execute_governed(policy(), |plan, allowance| source(plan, allowance, &edges(), &values()),
        |request| {
            count.set(count.get() + 1);
            match request {
                GraphInsertRequest::Vertex { .. } => Ok(ElementId::Vertex(VId(100))),
                _ => identity(request),
            }
        }, || Ok(()));
    assert!(matches!(repeated, Err(GqlQueryError::Source(GraphInsertError::DuplicateIdentity {
        request: GraphInsertRequest::Vertex { row: 1, .. }
    }))));
    assert_eq!(count.get(), 4);
    let wrong: ResultOf = standard().execute_governed(policy(), |plan, allowance| source(plan, allowance, &edges(), &values()),
        |_| Ok(ElementId::Edge(EId(100))), || Ok(()));
    assert!(matches!(wrong, Err(GqlQueryError::Source(GraphInsertError::IdentityKind { .. }))));
    let failed = standard().execute_governed(policy(), |plan, allowance| source(plan, allowance, &edges(), &values()),
        |request| if request == (GraphInsertRequest::Edge { row: 3, edge: 1 }) { Err("identity service failed") } else { identity(request) }, || Ok(()));
    assert!(matches!(failed, Err(GqlQueryError::Source(GraphInsertError::IdentitySource("identity service failed")))));
}

#[test]
fn exact_cumulative_limits_and_every_control_checkpoint_are_enforced() {
    let insertion = standard();
    let measured = run(&insertion, policy()).unwrap().stats();
    let exact = GraphInsertPolicy::new(GqlQueryPolicy::new(measured.selection.snapshot_records,
        measured.selection.result_rows, measured.evaluator.work_units, measured.evaluator.scratch_entries), 4, 8);
    assert_eq!(run(&insertion, exact).unwrap().stats(), measured);
    for cap in [
        GraphInsertPolicy::new(GqlQueryPolicy::new(6, 4, u64::MAX, u64::MAX), 4, 8),
        GraphInsertPolicy::new(GqlQueryPolicy::new(7, 3, u64::MAX, u64::MAX), 4, 8),
        GraphInsertPolicy::new(GqlQueryPolicy::new(7, 4, measured.evaluator.work_units - 1, u64::MAX), 4, 8),
        GraphInsertPolicy::new(GqlQueryPolicy::new(7, 4, u64::MAX, measured.evaluator.scratch_entries - 1), 4, 8),
        GraphInsertPolicy::new(policy().query, 3, 8), GraphInsertPolicy::new(policy().query, 4, 7),
    ] { assert!(run(&insertion, cap).is_err()); }
    let events = Cell::new(0);
    insertion.execute_governed(policy(), |plan, allowance| source(plan, allowance, &edges(), &values()), identity,
        || { events.set(events.get() + 1); Ok(()) }).unwrap();
    for stop in 1..=events.get() {
        let seen = Cell::new(0);
        let result = insertion.execute_governed(policy(), |plan, allowance| source(plan, allowance, &edges(), &values()), identity,
            || { seen.set(seen.get() + 1); if seen.get() == stop { Err(stop) } else { Ok(()) } });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(seen.get(), stop);
    }
}

#[test]
fn empty_matches_never_allocate_and_untrusted_source_statistics_cannot_wrap() {
    let calls = Cell::new(0);
    let empty = standard().execute_governed(GraphInsertPolicy::new(policy().query, 0, 0),
        |plan, allowance| source(plan, allowance, &[], &values()),
        |request| { calls.set(calls.get() + 1); identity(request) }, || Ok(())).unwrap();
    assert!(empty.intents().is_empty()); assert_eq!(calls.get(), 0);
    for corrupted in 0..3 {
        let result = standard().execute_governed(GraphInsertPolicy::new(GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX), 100, 100),
            |plan, allowance| {
                let mut result = source(plan, allowance, &edges(), &values())?;
                match corrupted {
                    0 => result.rows.result_rows += 1,
                    1 => result.evaluator.work_units = u64::MAX,
                    _ => { result.evaluator.work_units = 0; result.evaluator.scratch_entries = u64::MAX; }
                }
                Ok(result)
            }, |request| { calls.set(calls.get() + 1); identity(request) }, || Ok(()));
        assert!(result.is_err()); assert_eq!(calls.get(), 0);
    }
}
