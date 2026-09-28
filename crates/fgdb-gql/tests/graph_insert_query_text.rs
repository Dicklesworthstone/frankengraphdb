//! CREATE RETURN binds exact native occurrences, including duplicate inputs,
//! and shares preparation, scalar expressions and output ordering with reads.

use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::insertion::{GraphInsertIntent, GraphInsertPolicy, GraphInsertRequest};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphInsertQueryBatch, GraphInsertQueryError, GraphInsertTextErrorKind,
    GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind, PreparedGraphInsertQuery,
    PreparedGraphInsertQueryText, PreparedGraphInsertText,
};
use fgdb_types::{CanonicalScalar, EId, VId};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const COPY: LabelId = LabelId(9);
type ResultOf = Result<GraphInsertQueryBatch, GqlQueryError<GraphInsertQueryError<(), ()>, ()>>;

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
    GraphInsertPolicy::new(GqlQueryPolicy::new(0, 1_000, 2_000_000, 1_000_000), 1_000, 1_000)
}
fn identity(request: GraphInsertRequest) -> Result<ElementId, ()> {
    Ok(match request {
        GraphInsertRequest::Vertex { row, vertex } => ElementId::Vertex(VId(100 + row as u128 * 16 + vertex as u128)),
        GraphInsertRequest::Edge { row, edge } => ElementId::Edge(EId(1_000 + row as u128 * 16 + edge as u128)),
    })
}
fn prepare(text: &str) -> PreparedGraphInsertQuery {
    PreparedGraphInsertQueryText::prepare(text, R, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn run(query: &PreparedGraphInsertQuery) -> ResultOf {
    query.execute_governed(policy(), |_, _| -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), ()>> {
        panic!("standalone/UNWIND CREATE RETURN must never scan the graph")
    }, identity, || Ok(()))
}
fn int(value: i64) -> GraphValue { GraphValue::Scalar(CanonicalScalar::Int(value)) }
fn values(batch: &GraphInsertQueryBatch) -> Vec<Vec<GraphValue>> {
    batch.returning().value.iter().map(|row| row.values().to_vec()).collect()
}

#[test]
fn standalone_return_reads_exact_created_vertex_edge_and_frozen_properties() {
    let query = prepare("CREATE (a:Copy {p:3})-[e:R {p:9}]->(b {p:8}) \
        RETURN a,e,b,a.p AS value,e.p AS weight,a.q AS missing, \
        [a.p,b.p] AS pair,CASE WHEN a.p = 3 THEN b.p ELSE 1/0 END AS chosen");
    assert_eq!(query.columns(), &["a", "e", "b", "value", "weight", "missing", "pair", "chosen"]);
    let batch = run(&query).unwrap();
    assert_eq!(values(&batch), vec![vec![
        GraphValue::Vertex(VId(100)), GraphValue::Edge(EId(1_000)), GraphValue::Vertex(VId(101)),
        int(3), int(9), GraphValue::Scalar(CanonicalScalar::Null),
        GraphValue::List(vec![int(3), int(8)].into_boxed_slice()), int(8),
    ]]);
    assert_eq!(batch.insertion().intents(), &[
        GraphInsertIntent::Vertex { vertex: VId(100), labels: vec![COPY], properties: vec![(P, CanonicalScalar::Int(3))] },
        GraphInsertIntent::Vertex { vertex: VId(101), labels: vec![], properties: vec![(P, CanonicalScalar::Int(8))] },
        GraphInsertIntent::Edge { edge: EId(1_000), relation: R, source: VId(100), destination: VId(101), properties: vec![(P, CanonicalScalar::Int(9))] },
    ]);
    assert_eq!(batch.returning().rows.snapshot_records, 0);
    assert_eq!(batch.returning().rows.result_rows, 1);
    assert_eq!(prepare("CREATE (n {p:1}) RETURN n.p").columns(), &["p"]);
}

#[test]
fn duplicate_unwind_occurrences_retain_their_own_identities_before_output_pages() {
    let query = prepare("UNWIND [2,1,1] AS x CREATE (n:Copy {p:x}) \
        RETURN n,x,n.p AS p ORDER BY x,n");
    let batch = run(&query).unwrap();
    assert_eq!(values(&batch), vec![
        vec![GraphValue::Vertex(VId(116)), int(1), int(1)],
        vec![GraphValue::Vertex(VId(132)), int(1), int(1)],
        vec![GraphValue::Vertex(VId(100)), int(2), int(2)],
    ]);
    assert_eq!(batch.insertion().stats().created_vertices, 3);
    let paged = run(&prepare("UNWIND [2,1,1] AS x CREATE (n {p:x}) \
        RETURN DISTINCT n.p AS p ORDER BY p DESC SKIP 1 LIMIT 1")).unwrap();
    assert_eq!(values(&paged), vec![vec![int(1)]]);
    assert_eq!(paged.insertion().stats().created_vertices, 3);
    let nested = run(&prepare("UNWIND [[2],[3]] AS items UNWIND items AS x \
        CREATE (n {p:x+1}) RETURN items,x,n.p AS p ORDER BY x")).unwrap();
    assert_eq!(values(&nested), vec![
        vec![GraphValue::List(vec![int(2)].into_boxed_slice()), int(2), int(3)],
        vec![GraphValue::List(vec![int(3)].into_boxed_slice()), int(3), int(4)],
    ]);
}

#[test]
fn query_parameter_table_is_shared_once_across_creation_return_and_paging() {
    let calls = Cell::new(0);
    let statement = "UNWIND $items AS x CREATE (n:Copy {p:x+$step}) \
        RETURN n.p+$step AS p,$tail AS tail ORDER BY p DESC SKIP $skip LIMIT $cap; ";
    let template = PreparedGraphInsertQueryText::prepare_with_parameter_types(
        statement, R, &[("tail", GqlParameterType::List)], |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        },
    ).unwrap();
    assert_eq!(template.statement(), statement);
    assert_eq!(calls.get(), 2, "p resolves once across CREATE and RETURN");
    assert_eq!(template.parameter_schema().iter().find(|spec| spec.name == "step").unwrap().occurrences, 2);
    let args = GqlParameters::new().with_list("items", vec![int(1), int(3)]).unwrap()
        .with_int64("step", 10).unwrap().with_list("tail", vec![int(7)]).unwrap()
        .with_uint64("skip", 0).unwrap().with_uint64("cap", 1).unwrap();
    let query = template.bind_parameters(&args).unwrap();
    assert_eq!(query.canonical_bytes(), template.bind_parameters(&args).unwrap().canonical_bytes());
    assert_eq!(calls.get(), 2, "binding must not parse or resolve again");
    let batch = run(&query).unwrap();
    assert_eq!(values(&batch), vec![vec![int(23), GraphValue::List(vec![int(7)].into_boxed_slice())]]);
    assert_eq!(batch.insertion().stats().created_vertices, 2);
    let error = template.bind_parameters(&GqlParameters::new()).unwrap_err();
    assert!(matches!(error.kind, GraphInsertTextErrorKind::Query(GraphPatternTextErrorKind::MissingParameter)));
    let extra = args.clone().with_int64("unused", 1).unwrap();
    assert!(matches!(template.bind_parameters(&extra).unwrap_err().kind,
        GraphInsertTextErrorKind::Query(GraphPatternTextErrorKind::UnexpectedArguments)));
    assert_eq!(calls.get(), 2);
}

#[test]
fn invalid_return_scopes_and_unsupported_queries_refuse_before_any_catalog_call() {
    for statement in [
        "MATCH (m) CREATE (n:Copy) RETURN n",
        "UNWIND [1] AS x MATCH (m) CREATE (n:Copy) RETURN n",
        "CREATE (n:Copy) RETURN absent",
        "CREATE (n:Copy) RETURN n+1 AS bad",
        "CREATE (n:Copy) RETURN CASE WHEN TRUE THEN 1 ELSE missing END AS bad",
        "CREATE (n:Copy) RETURN n AS duplicate,n AS duplicate",
        "CREATE (n:Copy) RETURN n AS renamed,renamed AS bad",
        "CREATE (n:Copy) RETURN count(*) AS count",
        "CREATE (n:Copy) RETURN labels(n) AS labels",
        "CREATE (a)-[e:R]->(b) RETURN type(e) AS kind",
        "CREATE (a)-[a:R]->(b) RETURN a",
        "CREATE (a)-[e:R]->(e) RETURN a",
        "CREATE (a)-[e:R]->(b),(e) RETURN a",
        "UNWIND [1] AS x CREATE (a)-[x:R]->(b) RETURN a",
        "CREATE (n:Copy) RETURN n; CREATE (m)",
        "CREATE (n:Copy) RETURN n;;",
        "CREATE (n:Copy) RETURN n ORDER BY absent",
        "CREATE (n:Copy) RETURN n LIMIT -1",
        "CREATE (:Copy) RETURN *",
    ] {
        let calls = Cell::new(0);
        let result = PreparedGraphInsertQueryText::prepare(statement, R, |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        });
        assert!(result.is_err(), "unexpectedly accepted {statement}");
        assert_eq!(calls.get(), 0, "resolved catalog before refusing {statement}");
    }
    // The ordinary insertion facade still refuses RETURN instead of silently
    // discarding a result requested through the wrong prepared-statement type.
    assert!(PreparedGraphInsertText::prepare("CREATE (n) RETURN n", R, symbols).is_err());
}

#[test]
fn return_parent_depth_is_admitted_before_catalog_resolution() {
    let mut statement = String::new();
    for index in 0..fgdb_gql::MAX_GRAPH_SET_DEPTH - 2 {
        statement.push_str(&format!("UNWIND [1] AS x{index} "));
    }
    statement.push_str("CREATE (n:Copy)");
    assert!(PreparedGraphInsertText::prepare(&statement, R, symbols).is_ok());
    statement.push_str(" RETURN n");
    let calls = Cell::new(0);
    let error = PreparedGraphInsertQueryText::prepare(&statement, R, |kind, name| {
        calls.set(calls.get() + 1);
        symbols(kind, name)
    })
    .unwrap_err();
    assert!(matches!(
        error.kind,
        GraphInsertTextErrorKind::ReturnBuild(fgdb_gql::GraphInsertQueryBuildError::InputDepth(_))
    ));
    assert_eq!(calls.get(), 0);
}

#[test]
fn native_dispatch_distinguishes_clause_tokens_from_quotes_properties_and_scripts() {
    for statement in [
        "CREATE (n {p:'RETURN'}) RETURN n",
        "UNWIND ['CREATE; RETURN','it''s RETURN'] AS x CREATE (n {p:x}) RETURN n",
        "INSERT (a)-[e:R]->(b) RETURN e;",
        "CREATE (a); CREATE (b) RETURN b",
    ] {
        assert!(PreparedGraphInsertQueryText::has_return_clause(statement).unwrap(), "{statement}");
    }
    for statement in [
        "CREATE (n {p:'RETURN'})",
        "CREATE (n {p:'CREATE; RETURN'}); CREATE (m)",
        "UNWIND ['RETURN'] AS x CREATE (n {p:x})",
        "UNWIND [1] AS RETURN CREATE (n)",
        "CREATE (RETURN)",
        "CREATE (n {RETURN:1})",
        "MATCH (n) RETURN n",
        "UNWIND [1] AS x RETURN x",
    ] {
        assert!(!PreparedGraphInsertQueryText::has_return_clause(statement).unwrap(), "{statement}");
    }
}

#[test]
fn empty_sources_never_allocate_and_return_star_has_only_visible_bindings() {
    for statement in [
        "UNWIND [] AS x CREATE (n {p:1/0}) RETURN 1/0 AS bad",
        "UNWIND NULL AS x CREATE (n {p:1/0}) RETURN n",
        "UNWIND [1,2] AS x UNWIND [] AS y CREATE (n) RETURN x,n",
    ] {
        let result: ResultOf = prepare(statement).execute_governed(policy(),
            |_, _| panic!("empty source has no graph reads"),
            |_| panic!("empty source has no identities"), || Ok(()));
        let batch = result.unwrap();
        assert!(batch.insertion().intents().is_empty());
        assert!(batch.returning().value.is_empty());
    }
    let query = prepare("UNWIND [2] AS x CREATE (a)-[e:R]->(b),() RETURN *");
    assert_eq!(query.columns(), &["x", "a", "b", "e"]);
    let batch = run(&query).unwrap();
    assert_eq!(values(&batch), vec![vec![int(2), GraphValue::Vertex(VId(100)), GraphValue::Vertex(VId(101)), GraphValue::Edge(EId(1_000))]]);
    assert_eq!(batch.insertion().stats().created_vertices, 3, "anonymous nodes create effects without exposing synthetic names");
}
