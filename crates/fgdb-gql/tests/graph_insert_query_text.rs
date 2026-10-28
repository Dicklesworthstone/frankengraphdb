//! CREATE RETURN binds exact native occurrences, including duplicate inputs,
//! and shares preparation, scalar expressions and output ordering with reads.

use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphPath, GraphValue, GraphValueRow};
use fgdb_gql::insertion::{GraphInsertIntent, GraphInsertPolicy, GraphInsertRequest};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphInsertQueryBatch, GraphInsertQueryError, GraphInsertTextErrorKind,
    GraphPatternTextErrorKind, GraphSetColumnType, GraphSymbol, GraphSymbolKind,
    PreparedGraphInsertQuery, PreparedGraphInsertQueryText, PreparedGraphInsertText,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, EId, VId};
use std::cell::Cell;
use std::collections::BTreeMap;

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
fn run(query: &PreparedGraphInsertQuery) -> ResultOf {
    query.execute_governed(
        policy(),
        |_, _| -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), ()>> {
            panic!("standalone/UNWIND CREATE RETURN must never scan the graph")
        },
        identity,
        || Ok(()),
    )
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
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
fn standalone_return_reads_exact_created_vertex_edge_and_frozen_properties() {
    let query = prepare(
        "CREATE (a:Copy {p:3})-[e:R {p:9}]->(b {p:8}) \
        RETURN a,e,b,a.p AS value,e.p AS weight,a.q AS missing, \
        [a.p,b.p] AS pair,CASE WHEN a.p = 3 THEN b.p ELSE 1/0 END AS chosen",
    );
    assert_eq!(
        query.columns(),
        &[
            "a", "e", "b", "value", "weight", "missing", "pair", "chosen"
        ]
    );
    let batch = run(&query).unwrap();
    assert_eq!(
        values(&batch),
        vec![vec![
            GraphValue::Vertex(VId(100)),
            GraphValue::Edge(EId(1_000)),
            GraphValue::Vertex(VId(101)),
            int(3),
            int(9),
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::List(vec![int(3), int(8)].into_boxed_slice()),
            int(8),
        ]]
    );
    assert_eq!(
        batch.insertion().intents(),
        &[
            GraphInsertIntent::Vertex {
                vertex: VId(100),
                labels: vec![COPY],
                properties: vec![(P, CanonicalScalar::Int(3).into())]
            },
            GraphInsertIntent::Vertex {
                vertex: VId(101),
                labels: vec![],
                properties: vec![(P, CanonicalScalar::Int(8).into())]
            },
            GraphInsertIntent::Edge {
                edge: EId(1_000),
                relation: R,
                source: VId(100),
                destination: VId(101),
                properties: vec![(P, CanonicalScalar::Int(9).into())]
            },
        ]
    );
    assert_eq!(batch.returning().rows.snapshot_records, 0);
    assert_eq!(batch.returning().rows.result_rows, 1);
    assert_eq!(prepare("CREATE (n {p:1}) RETURN n.p").columns(), &["n.p"]);
}

#[test]
fn named_created_paths_return_exact_traversals_and_metadata() {
    let text = "CREATE p=(a:Copy {p:3})-[r:R]->(b)<-[:R]-(c) \
        RETURN p,length(p) AS hops,nodes(p) AS ns,relationships(p) AS es";
    let query = prepare(text);
    assert_eq!(query.columns(), &["p", "hops", "ns", "es"]);
    assert_eq!(
        query.column_types(),
        &[
            GraphSetColumnType::Path,
            GraphSetColumnType::Scalar,
            GraphSetColumnType::Vertices,
            GraphSetColumnType::Edges,
        ]
    );
    let batch = run(&query).unwrap();
    assert_eq!(
        values(&batch),
        vec![vec![
            GraphValue::Path(GraphPath::new(
                VId(100),
                vec![(EId(1_000), VId(101)), (EId(1_001), VId(102)),].into_boxed_slice()
            )),
            int(2),
            GraphValue::Vertices(vec![VId(100), VId(101), VId(102)].into_boxed_slice()),
            GraphValue::Edges(vec![EId(1_000), EId(1_001)].into_boxed_slice()),
        ]]
    );
    assert!(matches!(
        batch.insertion().intents()[4],
        GraphInsertIntent::Edge {
            source: VId(102),
            destination: VId(101),
            ..
        }
    ));
    assert_eq!(
        query.canonical_bytes(),
        prepare(&text.replacen("CREATE", "INSERT", 1)).canonical_bytes()
    );
    assert_eq!(
        query.canonical_bytes(),
        prepare(&text.replace("length(p)", "path_length(p)")).canonical_bytes()
    );

    let zero = run(&prepare(
        "CREATE p=(a) RETURN p,length(p) AS hops,nodes(p) AS ns,relationships(p) AS es",
    ))
    .unwrap();
    assert_eq!(
        values(&zero),
        vec![vec![
            GraphValue::Path(GraphPath::new(VId(100), Box::new([]))),
            int(0),
            GraphValue::Vertices(vec![VId(100)].into_boxed_slice()),
            GraphValue::Edges(Box::new([])),
        ]]
    );
    assert_eq!(zero.insertion().stats().created_vertices, 1);
    assert_eq!(zero.insertion().stats().created_edges, 0);
}

#[test]
fn named_created_paths_survive_with_aliases_and_star_without_scope_leaks() {
    let path = GraphValue::Path(GraphPath::new(
        VId(100),
        vec![(EId(1_000), VId(101))].into_boxed_slice(),
    ));
    let expected = vec![vec![
        path.clone(),
        int(1),
        GraphValue::Vertices(vec![VId(100), VId(101)].into_boxed_slice()),
        GraphValue::Edges(vec![EId(1_000)].into_boxed_slice()),
    ]];
    for tail in [
        "WITH p RETURN p,length(p) AS hops,nodes(p) AS ns,relationships(p) AS es",
        "WITH * RETURN p,length(p) AS hops,nodes(p) AS ns,relationships(p) AS es",
        "WITH p AS q RETURN q,length(q) AS hops,nodes(q) AS ns,relationships(q) AS es",
    ] {
        let query = prepare(&format!("CREATE p=(a)<-[r:R]-(b) {tail}"));
        assert_eq!(values(&run(&query).unwrap()), expected, "{tail}");
    }
    let star = prepare("CREATE p=(a)<-[r:R]-(b) RETURN *");
    assert_eq!(star.columns(), &["a", "b", "p", "r"]);
    assert_eq!(
        values(&run(&star).unwrap()),
        vec![vec![
            GraphValue::Vertex(VId(100)),
            GraphValue::Vertex(VId(101)),
            path,
            GraphValue::Edge(EId(1_000)),
        ]]
    );
    for text in [
        "CREATE p=(a)-[:R]->(b) WITH a RETURN p",
        "CREATE p=(a)-[:R]->(b) WITH p AS q RETURN p",
        "CREATE p=(a)-[:R]->(b) WITH 7 AS x RETURN length(p)",
    ] {
        assert!(
            PreparedGraphInsertQueryText::prepare(text, R, symbols).is_err(),
            "{text}"
        );
    }
}

#[test]
fn duplicate_inputs_create_distinct_paths_and_limits_only_page_results() {
    let query = prepare("UNWIND [1,1] AS x CREATE p=(a)-[:R]->(b) RETURN p");
    let batch = run(&query).unwrap();
    assert_eq!(
        values(&batch),
        vec![
            vec![GraphValue::Path(GraphPath::new(
                VId(100),
                vec![(EId(1_000), VId(101))].into_boxed_slice()
            ))],
            vec![GraphValue::Path(GraphPath::new(
                VId(116),
                vec![(EId(1_016), VId(117))].into_boxed_slice()
            ))],
        ]
    );
    let empty = run(&query.with_page(0, Some(0))).unwrap();
    assert!(empty.returning().value.is_empty());
    assert_eq!(empty.insertion().intents(), batch.insertion().intents());
}

#[test]
fn named_path_namespace_conflicts_refuse_before_catalog_resolution() {
    for text in [
        "CREATE p=(p:Copy) RETURN p",
        "CREATE p=(a)-[p:R]->(b) RETURN p",
        "CREATE p=(a:Copy),p=(b) RETURN p",
        "CREATE (a:Copy),a=(b) RETURN a",
        "CREATE p=(a:Copy),(p) RETURN p",
        "CREATE p=(a)-[:R]->(b),(c)-[p:R]->(d) RETURN p",
        "MATCH (p:Copy) CREATE p=(a) RETURN p",
        "UNWIND [1] AS p CREATE p=(a:Copy) RETURN p",
        "MATCH (a) CREATE p=(a) RETURN p",
        "CREATE p=(a:Copy) RETURN properties(p)",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphInsertQueryText::prepare(text, R, |kind, name: &str| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "catalog consulted before refusing {text}");
    }
}

#[test]
fn path_return_limit_zero_keeps_source_and_property_errors_before_allocation() {
    let query = prepare("UNWIND [1,0] AS x CREATE p=(a {p:10/x})-[:R]->(b) RETURN p LIMIT 0");
    let result: ResultOf = query.execute_governed(
        policy(),
        |_, _| panic!("UNWIND does not scan a graph"),
        |_| panic!("every property row must validate before allocation"),
        || Ok(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphInsertQueryError::Insertion(_)))
    ));

    let query = prepare("MATCH (a) CREATE p=(a)-[:R]->(b) RETURN p LIMIT 0");
    let result: ResultOf = query.execute_governed(
        policy(),
        |_, _| Err(GqlQueryError::Source(())),
        |_| panic!("source failure must precede allocation"),
        || Ok(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphInsertQueryError::Insertion(_)))
    ));
}

#[test]
fn duplicate_unwind_occurrences_retain_their_own_identities_before_output_pages() {
    let query = prepare(
        "UNWIND [2,1,1] AS x CREATE (n:Copy {p:x}) \
        RETURN n,x,n.p AS p ORDER BY x,n",
    );
    let batch = run(&query).unwrap();
    assert_eq!(
        values(&batch),
        vec![
            vec![GraphValue::Vertex(VId(116)), int(1), int(1)],
            vec![GraphValue::Vertex(VId(132)), int(1), int(1)],
            vec![GraphValue::Vertex(VId(100)), int(2), int(2)],
        ]
    );
    assert_eq!(batch.insertion().stats().created_vertices, 3);
    let paged = run(&prepare(
        "UNWIND [2,1,1] AS x CREATE (n {p:x}) \
        RETURN DISTINCT n.p AS p ORDER BY p DESC SKIP 1 LIMIT 1",
    ))
    .unwrap();
    assert_eq!(values(&paged), vec![vec![int(1)]]);
    assert_eq!(paged.insertion().stats().created_vertices, 3);
    let nested = run(&prepare(
        "UNWIND [[2],[3]] AS items UNWIND items AS x \
        CREATE (n {p:x+1}) RETURN items,x,n.p AS p ORDER BY x",
    ))
    .unwrap();
    assert_eq!(
        values(&nested),
        vec![
            vec![
                GraphValue::List(vec![int(2)].into_boxed_slice()),
                int(2),
                int(3)
            ],
            vec![
                GraphValue::List(vec![int(3)].into_boxed_slice()),
                int(3),
                int(4)
            ],
        ]
    );
}

#[test]
fn query_parameter_table_is_shared_once_across_creation_return_and_paging() {
    let calls = Cell::new(0);
    let statement = "UNWIND $items AS x CREATE (n:Copy {p:x+$step}) \
        RETURN n.p+$step AS p,$tail AS tail ORDER BY p DESC SKIP $skip LIMIT $cap; ";
    let template = PreparedGraphInsertQueryText::prepare_with_parameter_types(
        statement,
        R,
        &[("tail", GqlParameterType::List)],
        |kind, name: &str| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        },
    )
    .unwrap();
    assert_eq!(template.statement(), statement);
    assert_eq!(calls.get(), 2, "p resolves once across CREATE and RETURN");
    assert_eq!(
        template
            .parameter_schema()
            .iter()
            .find(|spec| spec.name == "step")
            .unwrap()
            .occurrences,
        2
    );
    let args = GqlParameters::new()
        .with_list("items", vec![int(1), int(3)])
        .unwrap()
        .with_int64("step", 10)
        .unwrap()
        .with_list("tail", vec![int(7)])
        .unwrap()
        .with_uint64("skip", 0)
        .unwrap()
        .with_uint64("cap", 1)
        .unwrap();
    let query = template.bind_parameters(&args).unwrap();
    assert_eq!(
        query.canonical_bytes(),
        template.bind_parameters(&args).unwrap().canonical_bytes()
    );
    assert_eq!(calls.get(), 2, "binding must not parse or resolve again");
    let batch = run(&query).unwrap();
    assert_eq!(
        values(&batch),
        vec![vec![
            int(23),
            GraphValue::List(vec![int(7)].into_boxed_slice())
        ]]
    );
    assert_eq!(batch.insertion().stats().created_vertices, 2);
    let error = template.bind_parameters(&GqlParameters::new()).unwrap_err();
    assert!(matches!(
        error.kind,
        GraphInsertTextErrorKind::Query(GraphPatternTextErrorKind::MissingParameter)
    ));
    let extra = args.clone().with_int64("unused", 1).unwrap();
    assert!(matches!(
        template.bind_parameters(&extra).unwrap_err().kind,
        GraphInsertTextErrorKind::Query(GraphPatternTextErrorKind::UnexpectedArguments)
    ));
    assert_eq!(calls.get(), 2);
}

#[test]
fn invalid_return_scopes_and_unsupported_queries_refuse_before_any_catalog_call() {
    for statement in [
        "MATCH (m) CREATE (n:Copy) SET n.p=1 RETURN n",
        "MATCH (a)-[r:R]->(b) CREATE (r:Copy) RETURN r",
        "MATCH (a) CREATE (c)-[a:R]->(d) RETURN a",
        "MATCH (a) CREATE (a:Copy) RETURN a",
        "UNWIND [1] AS x MATCH (m) WITH m CREATE (n:Copy) RETURN n",
        "CREATE (n:Copy) RETURN absent",
        "CREATE (n:Copy) RETURN n+1 AS bad",
        "CREATE (n:Copy) RETURN CASE WHEN TRUE THEN 1 ELSE missing END AS bad",
        "CREATE (n:Copy) RETURN n AS duplicate,n AS duplicate",
        "CREATE (n:Copy) RETURN n AS renamed,renamed AS bad",
        "CREATE (n:Copy) RETURN count(DISTINCT *) AS count",
        // `RETURN labels(n)` of a created vertex is supported since
        // fgdb-iufb7 (GraphInsertBinding::VertexLabels); its law is in
        // insertion_property_maps.rs.
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
        let result = PreparedGraphInsertQueryText::prepare(statement, R, |kind, name: &str| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        });
        assert!(result.is_err(), "unexpectedly accepted {statement}");
        assert_eq!(
            calls.get(),
            0,
            "resolved catalog before refusing {statement}"
        );
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
    let error = PreparedGraphInsertQueryText::prepare(&statement, R, |kind, name: &str| {
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
        "MATCH (n) CREATE (copy {p:n.p}) RETURN n,copy",
        "MATCH (n) WHERE n.RETURN=1 CREATE (copy) RETURN copy",
    ] {
        assert!(
            PreparedGraphInsertQueryText::has_return_clause(statement).unwrap(),
            "{statement}"
        );
    }
    for statement in [
        "CREATE (n {p:'RETURN'})",
        "CREATE (n {p:'CREATE; RETURN'}); CREATE (m)",
        "UNWIND ['RETURN'] AS x CREATE (n {p:x})",
        "UNWIND [1] AS RETURN CREATE (n)",
        "CREATE (RETURN)",
        "CREATE (n {RETURN:1})",
        "MATCH (n) RETURN n",
        "MATCH (n) WHERE n.CREATE=1 RETURN n",
        "MATCH (n) CREATE (copy {p:'RETURN'})",
        "UNWIND [1] AS x RETURN x",
    ] {
        assert!(
            !PreparedGraphInsertQueryText::has_return_clause(statement).unwrap(),
            "{statement}"
        );
    }
}

#[test]
fn empty_sources_never_allocate_and_return_star_has_only_visible_bindings() {
    for statement in [
        "UNWIND [] AS x CREATE (n {p:1/0}) RETURN 1/0 AS bad",
        "UNWIND NULL AS x CREATE (n {p:1/0}) RETURN n",
        "UNWIND [1,2] AS x UNWIND [] AS y CREATE (n) RETURN x,n",
    ] {
        let result: ResultOf = prepare(statement).execute_governed(
            policy(),
            |_, _| panic!("empty source has no graph reads"),
            |_| panic!("empty source has no identities"),
            || Ok(()),
        );
        let batch = result.unwrap();
        assert!(batch.insertion().intents().is_empty());
        assert!(batch.returning().value.is_empty());
    }
    // RETURN *'s columns are in name order (fgdb-7jja3).
    let query = prepare("UNWIND [2] AS x CREATE (a)-[e:R]->(b),() RETURN *");
    assert_eq!(query.columns(), &["a", "b", "e", "x"]);
    let batch = run(&query).unwrap();
    assert_eq!(
        values(&batch),
        vec![vec![
            GraphValue::Vertex(VId(100)),
            GraphValue::Vertex(VId(101)),
            GraphValue::Edge(EId(1_000)),
            int(2),
        ]]
    );
    assert_eq!(
        batch.insertion().stats().created_vertices,
        3,
        "anonymous nodes create effects without exposing synthetic names"
    );
}

type Props = BTreeMap<(VId, PropertyKeyId), CanonicalPropertyValue>;
type Triple = (VId, RelationId, VId);

fn run_matched(
    query: &PreparedGraphInsertQuery,
    vertices: &[VId],
    edges: &[Triple],
    props: &Props,
) -> ResultOf {
    let mut policy = policy();
    policy.query = GqlQueryPolicy::new(1_000, 1_000, 2_000_000, 1_000_000);
    query.execute_governed(
        policy,
        |selection, allowance| {
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
        },
        identity,
        || Ok(()),
    )
}

struct MultipartInsertSource<'a> {
    vertices: &'a [VId],
    edges: &'a [Triple],
    properties: &'a Props,
}

impl fgdb_gql::GraphSetSource<(), ()> for MultipartInsertSource<'_> {
    fn pattern(
        &mut self,
        selection: &fgdb_gql::algebra::PreparedGraphPattern<GraphValueRow>,
        allowance: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), ()>> {
        selection.plan().execute_governed_with_properties(
            (self.vertices.len() + self.edges.len()) as u64,
            self.vertices.iter().copied(),
            self.edges.iter().copied(),
            |vid, predicates| {
                Ok(predicates.iter().all(|predicate| {
                    predicate.matches_borrowed(
                        [],
                        self.properties.iter().filter_map(|(&(owner, key), value)| {
                            (owner == vid).then_some((key, value))
                        }),
                    )
                }))
            },
            |vid, key| Ok(self.properties.get(&(vid, key))),
            allowance,
            || Ok(()),
        )
    }

    fn element_property(
        &mut self,
        element: ElementId,
        key: PropertyKeyId,
    ) -> Option<Result<Option<CanonicalPropertyValue>, ()>> {
        Some(Ok(match element {
            ElementId::Vertex(vertex) => self.properties.get(&(vertex, key)).cloned(),
            ElementId::Edge(_) => None,
        }))
    }
}

#[test]
fn multipart_read_bag_is_frozen_before_first_creation_and_rebinds_without_catalog_calls() {
    let text = "MATCH (a) WHERE a.p < $ceiling WITH a,a.p AS key \
        UNWIND [0,1] AS occurrence MATCH (a)-[:R]->(b) \
        WITH a,key,occurrence,b.p AS target \
        CREATE (copy:Copy {p:key+occurrence+$step,q:target}),(a)-[:R]->(copy) \
        RETURN copy.p AS copied,copy.q AS target ORDER BY copied,target";
    let calls = Cell::new(0);
    let template = PreparedGraphInsertQueryText::prepare(text, R, |kind, name: &str| {
        calls.set(calls.get() + 1);
        symbols(kind, name)
    })
    .unwrap();
    let resolved = calls.get();
    let vertices = [VId(1), VId(2), VId(3)];
    let edges = [
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
        (VId(2), R, VId(3)),
    ];
    let properties = Props::from([
        ((VId(1), P), CanonicalScalar::Int(10).into()),
        ((VId(2), P), CanonicalScalar::Int(20).into()),
        ((VId(3), P), CanonicalScalar::Int(30).into()),
    ]);
    for (ceiling, step, expected) in [
        (
            30,
            1,
            vec![(11, 20), (11, 20), (12, 20), (12, 20), (21, 30), (22, 30)],
        ),
        (20, 4, vec![(14, 20), (14, 20), (15, 20), (15, 20)]),
        (0, 9, vec![]),
    ] {
        let arguments = GqlParameters::new()
            .with_int64("ceiling", ceiling)
            .unwrap()
            .with_int64("step", step)
            .unwrap();
        let query = template.bind_parameters(&arguments).unwrap();
        assert_eq!(calls.get(), resolved);
        assert_eq!(
            query.canonical_bytes(),
            template
                .bind_parameters(&arguments)
                .unwrap()
                .canonical_bytes()
        );
        let mut allowance = policy();
        allowance.query = GqlQueryPolicy::new(1_000, 1_000, 2_000_000, 1_000_000);
        let batch = query
            .execute_governed_with_source(
                allowance,
                MultipartInsertSource {
                    vertices: &vertices,
                    edges: &edges,
                    properties: &properties,
                },
                identity,
                || Ok(()),
            )
            .unwrap();
        assert_eq!(
            values(&batch),
            expected
                .iter()
                .map(|&(a, b)| vec![int(a), int(b)])
                .collect::<Vec<_>>()
        );
        assert_eq!(
            batch.insertion().stats().created_vertices as usize,
            expected.len()
        );
        assert_eq!(
            batch.insertion().stats().created_edges as usize,
            expected.len()
        );
        assert_eq!(batch.insertion().intents().len(), expected.len() * 2);
    }
}

#[test]
fn optional_and_read_call_parts_feed_first_create_with_their_own_cardinality() {
    let vertices = [VId(1), VId(2), VId(3)];
    let edges = [
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
        (VId(2), R, VId(3)),
    ];
    let properties = Props::from([
        ((VId(1), P), CanonicalScalar::Int(10).into()),
        ((VId(2), P), CanonicalScalar::Int(20).into()),
        ((VId(3), P), CanonicalScalar::Int(30).into()),
    ]);
    for (text, expected) in [
        (
            "MATCH (a) WITH a OPTIONAL MATCH (a)-[:R]->(b) \
          WITH coalesce(b.p,-1) AS value CREATE (copy:Copy {p:value}) RETURN copy.p ORDER BY copy.p",
            vec![-1, 20, 20, 30],
        ),
        (
            "MATCH (a) WITH a OPTIONAL MATCH (a)-[:R]->(b) WHERE b.p>=30 \
          WITH coalesce(b.p,-1) AS value CREATE (copy:Copy {p:value}) RETURN copy.p ORDER BY copy.p",
            vec![-1, -1, 30],
        ),
        (
            "MATCH (a) WITH a,30 AS floor OPTIONAL MATCH (a)-[:R]->(b) WHERE b.p+1>floor \
          CREATE (copy:Copy {p:coalesce(b.p,-1)}) RETURN copy.p ORDER BY copy.p",
            vec![-1, -1, 30],
        ),
        (
            "MATCH (a) WITH a CALL { WITH a MATCH (a)-[:R]->(b) RETURN count(*) AS degree } \
          CREATE (copy:Copy {p:degree}) RETURN copy.p ORDER BY copy.p",
            vec![0, 1, 2],
        ),
        (
            "UNWIND [{key:20},{key:10}] AS row MATCH (a {p:row.key}) \
          CREATE (copy:Copy {p:a.p}) RETURN copy.p ORDER BY copy.p",
            vec![10, 20],
        ),
        (
            "UNWIND [{key:20},{key:10}] AS row MATCH (a {p:row.key}) WITH a \
          MATCH (a)-[:R]->(b) CREATE (copy:Copy {p:a.p+b.p}) RETURN copy.p ORDER BY copy.p",
            vec![30, 30, 50],
        ),
        (
            "CALL { RETURN 7 AS value } INSERT (copy:Copy {p:value}) RETURN copy.p",
            vec![7],
        ),
    ] {
        assert!(
            PreparedGraphInsertQueryText::has_return_clause(text).unwrap(),
            "{text}"
        );
        let query = prepare(text);
        let mut allowance = policy();
        allowance.query = GqlQueryPolicy::new(1_000, 1_000, 2_000_000, 1_000_000);
        let batch = query
            .execute_governed_with_source(
                allowance,
                MultipartInsertSource {
                    vertices: &vertices,
                    edges: &edges,
                    properties: &properties,
                },
                identity,
                || Ok(()),
            )
            .expect(text);
        assert_eq!(
            values(&batch),
            expected
                .into_iter()
                .map(|value| vec![int(value)])
                .collect::<Vec<_>>(),
            "{text}"
        );
    }
}

#[test]
fn multipart_source_and_late_field_errors_precede_allocation_even_with_limit_zero() {
    let query = prepare(
        "MATCH (a) WITH a OPTIONAL MATCH (a)-[:R]->(b) \
        WITH a.p AS value CREATE (copy:Copy {p:1/(20-value)}) RETURN copy.p LIMIT 0",
    );
    let vertices = [VId(1), VId(2)];
    let properties = Props::from([
        ((VId(1), P), CanonicalScalar::Int(10).into()),
        ((VId(2), P), CanonicalScalar::Int(20).into()),
    ]);
    let allocated = Cell::new(0);
    let mut allowance = policy();
    allowance.query = GqlQueryPolicy::new(1_000, 1_000, 2_000_000, 1_000_000);
    let result = query.execute_governed_with_source(
        allowance,
        MultipartInsertSource {
            vertices: &vertices,
            edges: &[],
            properties: &properties,
        },
        |request| {
            allocated.set(allocated.get() + 1);
            identity(request)
        },
        || Ok(()),
    );
    assert!(result.is_err());
    assert_eq!(allocated.get(), 0);
    let result = query.execute_governed(
        allowance,
        |_, _| Err(GqlQueryError::<(), ()>::Source(())),
        |request| {
            allocated.set(allocated.get() + 1);
            identity(request)
        },
        || Ok(()),
    );
    assert!(result.is_err());
    assert_eq!(allocated.get(), 0);
}

#[test]
fn multipart_insert_scope_and_read_only_call_errors_precede_catalog_resolution() {
    for text in [
        "MATCH (a) WITH a.p AS value MATCH (b) CREATE (copy {p:a.p}) RETURN copy",
        "MATCH (a) WITH a CALL { WITH a CREATE (b) RETURN b } CREATE (copy) RETURN copy",
        "MATCH (a) WITH a MATCH (b) RETURN b CREATE (copy) RETURN copy",
        "MATCH (a) WITH a MATCH (b) CREATE (copy {p:missing}) RETURN copy",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphInsertQueryText::prepare(text, R, |kind, name: &str| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "{text}");
    }
    for text in [
        "MATCH (a) WITH a MATCH (b) CREATE (a)-[:R]->(b)",
        "MATCH (a) WITH a OPTIONAL MATCH (a)-[:R]->(b) WITH coalesce(b.p,0) AS value CREATE (copy {p:value})",
        "CALL { MATCH (a) RETURN count(*) AS total } CREATE (copy {p:total})",
    ] {
        let script = fgdb_gql::PreparedGraphWriteScript::prepare(text, R, symbols).expect(text);
        assert!(script.requires_read(), "{text}");
        script.bind_parameters(&GqlParameters::new()).expect(text);
    }
}

#[test]
fn matched_creation_returns_original_and_created_bindings_for_every_occurrence() {
    let calls = Cell::new(0);
    let template = PreparedGraphInsertQueryText::prepare(
        "MATCH (a)-[:R]->(b) WHERE a.p >= $floor \
         CREATE (c:Copy {p:a.p+$step}),(a)-[e:R {q:$step}]->(c) \
         RETURN a,b,c,e,a.p AS source,c.p AS copied,b.q AS return_only",
        R,
        |kind, name: &str| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        },
    )
    .unwrap();
    assert_eq!(calls.get(), 4, "one resolution per name and domain");
    let arguments = GqlParameters::new()
        .with_int64("floor", 10)
        .unwrap()
        .with_int64("step", 1)
        .unwrap();
    let query = template.bind_parameters(&arguments).unwrap();
    assert_eq!(calls.get(), 4, "binding does not parse or resolve again");
    let vertices = [VId(1), VId(2), VId(3)];
    let edges = [
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
        (VId(2), R, VId(3)),
    ];
    let props = Props::from([
        ((VId(1), P), CanonicalScalar::Int(10).into()),
        ((VId(2), P), CanonicalScalar::Int(20).into()),
        ((VId(2), Q), CanonicalScalar::Int(7).into()),
        ((VId(3), Q), CanonicalScalar::Int(8).into()),
    ]);
    let batch = run_matched(&query, &vertices, &edges, &props).unwrap();
    assert_eq!(
        values(&batch),
        vec![
            vec![
                GraphValue::Vertex(VId(1)),
                GraphValue::Vertex(VId(2)),
                GraphValue::Vertex(VId(100)),
                GraphValue::Edge(EId(1_000)),
                int(10),
                int(11),
                int(7),
            ],
            vec![
                GraphValue::Vertex(VId(1)),
                GraphValue::Vertex(VId(2)),
                GraphValue::Vertex(VId(116)),
                GraphValue::Edge(EId(1_016)),
                int(10),
                int(11),
                int(7),
            ],
            vec![
                GraphValue::Vertex(VId(2)),
                GraphValue::Vertex(VId(3)),
                GraphValue::Vertex(VId(132)),
                GraphValue::Edge(EId(1_032)),
                int(20),
                int(21),
                int(8),
            ],
        ]
    );
    assert_eq!(batch.insertion().stats().created_vertices, 3);
    assert_eq!(batch.insertion().stats().created_edges, 3);
    for (row, source) in [VId(1), VId(1), VId(2)].into_iter().enumerate() {
        assert!(matches!(
            batch.insertion().intents()[row * 2 + 1],
            GraphInsertIntent::Edge { source: actual, destination, .. }
                if actual == source && destination == VId(100 + row as u128 * 16)
        ));
    }
    assert_eq!(
        query.canonical_bytes(),
        template
            .bind_parameters(&arguments)
            .unwrap()
            .canonical_bytes()
    );
}

#[test]
fn match_return_only_columns_and_star_do_not_use_creation_column_positions() {
    let props = Props::from([((VId(1), Q), CanonicalScalar::Int(11).into())]);
    let batch = run_matched(
        &prepare("MATCH (n) CREATE (c {p:7}) RETURN n.q AS q,n,c ORDER BY n"),
        &[VId(1), VId(2)],
        &[],
        &props,
    )
    .unwrap();
    assert_eq!(
        values(&batch),
        vec![
            vec![
                int(11),
                GraphValue::Vertex(VId(1)),
                GraphValue::Vertex(VId(100))
            ],
            vec![
                GraphValue::Scalar(CanonicalScalar::Null),
                GraphValue::Vertex(VId(2)),
                GraphValue::Vertex(VId(116)),
            ],
        ]
    );
    let query = prepare("MATCH (a)-[:R]->(b) CREATE (c),(a)-[e:R]->(c) RETURN *");
    assert_eq!(query.columns(), &["a", "b", "c", "e"]);
    let batch = run_matched(
        &query,
        &[VId(1), VId(2)],
        &[(VId(1), R, VId(2))],
        &Props::new(),
    )
    .unwrap();
    assert_eq!(
        values(&batch),
        vec![vec![
            GraphValue::Vertex(VId(1)),
            GraphValue::Vertex(VId(2)),
            GraphValue::Vertex(VId(100)),
            GraphValue::Edge(EId(1_000)),
        ]]
    );
    let edge_query = prepare(
        "MATCH (a)-[r:R]->(b) CREATE (c {p:r.p}) \
         RETURN r,r.p AS weight,c.p AS copied",
    );
    assert_eq!(
        edge_query.column_types(),
        &[
            GraphSetColumnType::Edge,
            GraphSetColumnType::Stored,
            GraphSetColumnType::Stored
        ]
    );
}

#[test]
fn match_output_distinct_and_limit_never_suppress_create_effects() {
    let vertices = [VId(1), VId(2), VId(3)];
    let edges = [
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
        (VId(2), R, VId(3)),
    ];
    let batch = run_matched(
        &prepare("MATCH (a)-[:R]->(b) CREATE (c) RETURN DISTINCT a ORDER BY a LIMIT 1"),
        &vertices,
        &edges,
        &Props::new(),
    )
    .unwrap();
    assert_eq!(values(&batch), vec![vec![GraphValue::Vertex(VId(1))]]);
    assert_eq!(batch.insertion().stats().created_vertices, 3);
    let batch = run_matched(
        &prepare("MATCH (n) CREATE (copy) RETURN 1 AS one LIMIT 0"),
        &vertices,
        &[],
        &Props::new(),
    )
    .unwrap();
    assert!(batch.returning().value.is_empty());
    assert_eq!(batch.insertion().stats().created_vertices, 3);
}

#[test]
fn empty_match_is_a_no_op_and_return_errors_expose_no_partial_proposal() {
    let batch = run_matched(
        &prepare("MATCH (n) CREATE (copy {p:1/0}) RETURN 1/0 AS bad"),
        &[],
        &[],
        &Props::new(),
    )
    .unwrap();
    assert!(batch.insertion().intents().is_empty());
    assert_eq!(batch.insertion().stats().created_vertices, 0);
    assert!(batch.returning().value.is_empty());
    let result = run_matched(
        &prepare("MATCH (n) CREATE (copy {p:1}) RETURN 1/0 AS bad"),
        &[VId(1)],
        &[],
        &Props::new(),
    );
    assert!(
        result.is_err(),
        "a failing RETURN cannot publish a CREATE prefix"
    );
}

// Imported keys are not identity bindings, and key 2 is non-unique. Either
// a missing join or an incorrectly shifted graph slot
// therefore changes an independently spelled expected answer.
fn imported_props() -> Props {
    Props::from([
        ((VId(1), P), CanonicalScalar::Int(1).into()),
        ((VId(2), P), CanonicalScalar::Int(2).into()),
        ((VId(3), P), CanonicalScalar::Int(2).into()),
        ((VId(1), Q), CanonicalScalar::Int(10).into()),
        ((VId(2), Q), CanonicalScalar::Int(20).into()),
        ((VId(3), Q), CanonicalScalar::Int(30).into()),
    ])
}

#[test]
fn imported_keys_create_connected_occurrences_with_frozen_source_and_result_fields() {
    let query = prepare(
        "UNWIND [2,1,2] AS x MATCH (a {p:x}) \
         CREATE (a)-[e:R {q:x+1}]->(c:Copy {p:a.q+x}) \
         RETURN x,a,a.q AS old,c,e,c.p AS value,e.q AS weight ORDER BY c",
    );
    let batch = run_matched(&query, &[VId(1), VId(2), VId(3)], &[], &imported_props()).unwrap();
    let expected: Vec<_> = [(2, 2, 20), (2, 3, 30), (1, 1, 10), (2, 2, 20), (2, 3, 30)]
        .into_iter()
        .enumerate()
        .map(|(row, (key, vertex, old))| {
            vec![
                int(key),
                GraphValue::Vertex(VId(vertex)),
                int(old),
                GraphValue::Vertex(VId(100 + row as u128 * 16)),
                GraphValue::Edge(EId(1_000 + row as u128 * 16)),
                int(old + key),
                int(key + 1),
            ]
        })
        .collect();
    assert_eq!(values(&batch), expected);
    assert_eq!(batch.insertion().stats().created_vertices, 5);
    assert_eq!(batch.insertion().stats().created_edges, 5);
    assert_eq!(batch.returning().rows.snapshot_records, 3);
    for (row, vertex) in [2, 3, 1, 2, 3].into_iter().enumerate() {
        assert!(matches!(
            batch.insertion().intents()[2 * row + 1],
            GraphInsertIntent::Edge { source, destination, .. }
                if source == VId(vertex) && destination == VId(100 + row as u128 * 16)
        ));
    }
}

#[test]
fn imported_correlations_share_where_and_map_lowering_and_all_keys_are_required() {
    let map = prepare(
        "UNWIND [1,2] AS x UNWIND [10,20] AS y MATCH (a {p:x,q:y}) \
         CREATE (c {p:x+y}) RETURN x,y,a,c.p AS sum ORDER BY x,y",
    );
    let predicate = prepare(
        "UNWIND [1,2] AS x UNWIND [10,20] AS y MATCH (a) WHERE a.p=x AND y=a.q \
         CREATE (c {p:x+y}) RETURN x,y,a,c.p AS sum ORDER BY x,y",
    );
    assert_eq!(map.canonical_bytes(), predicate.canonical_bytes());
    let batch = run_matched(&map, &[VId(1), VId(2), VId(3)], &[], &imported_props()).unwrap();
    assert_eq!(
        values(&batch),
        vec![
            vec![int(1), int(10), GraphValue::Vertex(VId(1)), int(11)],
            vec![int(2), int(20), GraphValue::Vertex(VId(2)), int(22)],
        ]
    );
}

#[test]
fn imported_map_endpoints_preserve_duplicate_rows_and_nonunique_matches() {
    let source = "UNWIND [{src:2,dst:1,weight:90},{src:1,dst:2,weight:5},\
                         {src:2,dst:1,weight:91},{src:null,dst:1,weight:0},\
                         {dst:1,weight:0},{src:'2',dst:1,weight:0}] AS row ";
    let tail = "CREATE (a)-[e:R {q:row.weight}]->(b) \
                RETURN row.weight AS weight,a,b,e.q AS stored ORDER BY weight,a,b";
    let map = prepare(&format!(
        "{source} MATCH (a {{p:row.src}}), (b {{p:row.dst}}) {tail}"
    ));
    let predicate = prepare(&format!(
        "{source} MATCH (a), (b) WHERE a.p=row.src AND row.dst=b.p {tail}"
    ));
    assert_eq!(map.canonical_bytes(), predicate.canonical_bytes());
    let batch = run_matched(&map, &[VId(1), VId(2), VId(3)], &[], &imported_props()).unwrap();
    assert_eq!(
        values(&batch),
        [
            (5, 1, 2),
            (5, 1, 3),
            (90, 2, 1),
            (90, 3, 1),
            (91, 2, 1),
            (91, 3, 1)
        ]
        .into_iter()
        .map(|(weight, source, destination)| vec![
            int(weight),
            GraphValue::Vertex(VId(source)),
            GraphValue::Vertex(VId(destination)),
            int(weight),
        ])
        .collect::<Vec<_>>()
    );
    assert_eq!(batch.insertion().stats().created_vertices, 0);
    assert_eq!(batch.insertion().stats().created_edges, 6);
    assert_eq!(batch.returning().rows.snapshot_records, 3);
    // Allocation follows imported occurrence order, before RETURN sorts it.
    // Every nonunique endpoint is retained and both row repetitions create.
    for (row, (source, destination, weight)) in [
        (2, 1, 90),
        (3, 1, 90),
        (1, 2, 5),
        (1, 3, 5),
        (2, 1, 91),
        (3, 1, 91),
    ]
    .into_iter()
    .enumerate()
    {
        let GraphInsertIntent::Edge {
            edge,
            source: actual_source,
            destination: actual_destination,
            properties,
            ..
        } = &batch.insertion().intents()[row]
        else {
            panic!("one edge per imported match")
        };
        assert_eq!(*edge, EId(1_000 + row as u128 * 16));
        assert_eq!(
            (*actual_source, *actual_destination),
            (VId(source), VId(destination))
        );
        assert_eq!(properties, &vec![(Q, CanonicalScalar::Int(weight).into())]);
    }
}

#[test]
fn bulk_map_document_creates_edges_through_one_native_input_above_scalar_literal_limit() {
    let rows = (0..256)
        .map(|weight| {
            GraphValue::map(vec![
                ("src".into(), int(1)),
                ("dst".into(), int(2)),
                ("weight".into(), int(weight)),
                (
                    "payload".into(),
                    GraphValue::Scalar(CanonicalScalar::ucs_basic_text(&"x".repeat(512)).unwrap()),
                ),
            ])
            .unwrap()
        })
        .collect();
    let arguments = GqlParameters::new().with_list("rows", rows).unwrap();
    let fgdb_gql::GqlParameterValue::List(document) = arguments.get("rows").unwrap() else {
        panic!("list document")
    };
    assert!(document.canonical_bytes().len() > 65_536);
    let query = PreparedGraphInsertQueryText::prepare(
        "UNWIND $rows AS row MATCH (a {p:row.src}), (b {p:row.dst}) \
         CREATE (a)-[e:R {q:row.weight}]->(b) RETURN e.q AS weight",
        R,
        symbols,
    )
    .unwrap()
    .bind_parameters(&arguments)
    .unwrap();
    let batch = run_matched(&query, &[VId(1), VId(2), VId(3)], &[], &imported_props()).unwrap();
    assert_eq!(batch.insertion().stats().created_edges, 512);
    assert_eq!(batch.returning().rows.snapshot_records, 3);
    assert_eq!(
        values(&batch),
        (0..256)
            .flat_map(|weight| [vec![int(weight)], vec![int(weight)]])
            .collect::<Vec<_>>()
    );
}

#[test]
fn imported_nested_map_keys_keep_public_slots_and_parameter_templates() {
    let text = "UNWIND $rows AS row MATCH (a {p:row.key.p}) \
                CREATE (c:Copy {p:row.payload+a.q}) \
                RETURN row.payload AS payload,a,c,c.p AS stored ORDER BY c";
    let calls = Cell::new(0);
    let template = PreparedGraphInsertQueryText::prepare(text, R, |kind, name: &str| {
        calls.set(calls.get() + 1);
        symbols(kind, name)
    })
    .unwrap();
    let calls_after_prepare = calls.get();
    let map = |keys: &[&str], values: Vec<GraphValue>| GraphValue::Map {
        keys: keys.iter().map(|key| (*key).into()).collect(),
        values: values.into_boxed_slice(),
    };
    let arguments = GqlParameters::new()
        .with_list(
            "rows",
            vec![
                map(&["key", "payload"], vec![map(&["p"], vec![int(2)]), int(7)]),
                map(&["key", "payload"], vec![map(&["p"], vec![int(1)]), int(8)]),
                map(&["key", "payload"], vec![int(0), int(9)]),
            ],
        )
        .unwrap();
    let invalid = template.bind_parameters(&arguments).unwrap();
    assert!(run_matched(&invalid, &[VId(1), VId(2), VId(3)], &[], &imported_props()).is_err());
    let valid = GqlParameters::new()
        .with_list(
            "rows",
            vec![
                map(&["key", "payload"], vec![map(&["p"], vec![int(2)]), int(7)]),
                map(&["key", "payload"], vec![map(&["p"], vec![int(1)]), int(8)]),
                map(
                    &["key", "payload"],
                    vec![GraphValue::Scalar(CanonicalScalar::Null), int(9)],
                ),
            ],
        )
        .unwrap();
    let query = template.bind_parameters(&valid).unwrap();
    assert_eq!(
        calls.get(),
        calls_after_prepare,
        "binding never resolves names again"
    );
    assert_eq!(query.columns(), &["payload", "a", "c", "stored"]);
    let batch = run_matched(&query, &[VId(1), VId(2), VId(3)], &[], &imported_props()).unwrap();
    assert_eq!(
        values(&batch),
        vec![
            vec![
                int(7),
                GraphValue::Vertex(VId(2)),
                GraphValue::Vertex(VId(100)),
                int(27)
            ],
            vec![
                int(7),
                GraphValue::Vertex(VId(3)),
                GraphValue::Vertex(VId(116)),
                int(37)
            ],
            vec![
                int(8),
                GraphValue::Vertex(VId(1)),
                GraphValue::Vertex(VId(132)),
                int(18)
            ],
        ]
    );
}

#[test]
fn imported_map_return_reads_its_entries_and_no_other_leaf_does() {
    // RETURN reads an imported map's entries as CREATE does (fgdb-0s2pq). A
    // missing key reads NULL; an entry and a created property mix freely.
    let query = prepare(
        "UNWIND [{p:1,t:5},{p:2}] AS row CREATE (c {q:row.p}) \
         RETURN row.p AS p,row.t AS t,row.p+c.q AS sum,c.q AS stored ORDER BY p",
    );
    assert_eq!(query.columns(), &["p", "t", "sum", "stored"]);
    let batch = run(&query).unwrap();
    assert_eq!(
        values(&batch),
        vec![
            vec![int(1), int(5), int(2), int(1)],
            vec![
                int(2),
                GraphValue::Scalar(CanonicalScalar::Null),
                int(4),
                int(2)
            ],
        ]
    );
    // UNWIND types its column Any, so a non-map element fails when read, as
    // openCypher's property access on an integer does.
    assert!(run(&prepare("UNWIND [1] AS row CREATE (c) RETURN row.p AS p")).is_err());
    // Only the imported leaf reads `.key`: a later endpoint in the same RETURN
    // still refuses before the catalog, and a scalar WITH import still refuses.
    for text in [
        "UNWIND [{p:1}] AS row CREATE (a)-[e:R]->(b) RETURN row.p AS p,startNode(e).p",
        "UNWIND [{p:1}] AS row CREATE (a)-[e:R]->(b) RETURN startNode(e).p,row.p AS p",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphInsertQueryText::prepare(text, R, |kind, name: &str| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "{text}");
    }
    assert!(
        PreparedGraphInsertQueryText::prepare("WITH 1 AS x CREATE (c) RETURN x.p", R, symbols)
            .is_err()
    );
}

#[test]
fn imported_map_correlations_refuse_scope_and_depth_changes_before_catalog() {
    for text in [
        "UNWIND [{p:1}] AS row MATCH (a) WHERE a.p=row.p OR a.q=1 CREATE (c) RETURN c",
        "UNWIND [{p:1}] AS row MATCH (a) WHERE NOT a.p=row.p CREATE (c) RETURN c",
        "UNWIND [{p:1}] AS row MATCH (a) WHERE a.p=row.p+1 CREATE (c) RETURN c",
        "UNWIND [{p:1}] AS row MATCH (a) OPTIONAL MATCH (b {p:row.p}) CREATE (c) RETURN c",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphInsertQueryText::prepare(text, R, |kind, name: &str| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "{text}");
    }
    let long_key = "p".repeat(fgdb_gql::algebra::MAX_PATTERN_NAME_BYTES + 1);
    for predicate in [format!("a.{long_key}=row.p"), format!("row.p=a.{long_key}")] {
        let text =
            format!("UNWIND [{{p:1}}] AS row MATCH (a) WHERE {predicate} CREATE (c) RETURN c");
        let calls = Cell::new(0);
        assert!(
            PreparedGraphInsertQueryText::prepare(&text, R, |kind, name: &str| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err()
        );
        assert_eq!(
            calls.get(),
            0,
            "join lookahead retains normal graph name admission"
        );
    }
    let mut text = String::from("UNWIND [{p:1}] AS row ");
    for index in 1..fgdb_gql::MAX_GRAPH_SET_DEPTH - 6 {
        text.push_str(&format!("UNWIND [1] AS x{index} "));
    }
    text.push_str("MATCH (a {p:row.p}) CREATE (c) RETURN c");
    let calls = Cell::new(0);
    assert!(
        PreparedGraphInsertQueryText::prepare(&text, R, |kind, name: &str| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        })
        .is_err()
    );
    assert_eq!(
        calls.get(),
        0,
        "computed keys and cleanup count toward native depth"
    );
    // Each side fits independently, but the temporary joined row contains
    // the imported map, 32 computed keys and 32 graph fields: 65 columns.
    let fields = (0..32)
        .map(|key| format!("p{key}:row.p{key}"))
        .collect::<Vec<_>>()
        .join(",");
    let text = format!("UNWIND [{{p0:1}}] AS row MATCH (a {{{fields}}}) CREATE (c) RETURN c");
    let calls = Cell::new(0);
    assert!(
        PreparedGraphInsertQueryText::prepare(&text, R, |kind, name: &str| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        })
        .is_err()
    );
    assert_eq!(
        calls.get(),
        0,
        "transient joined width is a definition bound"
    );
}

#[test]
fn imported_match_preserves_uncorrelated_bags_and_keeps_anonymous_bindings_private() {
    // This was the formerly unsupported UNWIND/MATCH case in the refusal
    // corpus. It is now a positive execution law, not a deleted assertion.
    let query = prepare("UNWIND [1] AS x MATCH (m) CREATE (n:Copy) RETURN n");
    let batch = run_matched(&query, &[VId(1), VId(2)], &[], &Props::new()).unwrap();
    assert_eq!(batch.insertion().stats().created_vertices, 2);
    assert_eq!(
        values(&batch),
        vec![
            vec![GraphValue::Vertex(VId(100))],
            vec![GraphValue::Vertex(VId(116))],
        ]
    );
    let query = prepare("UNWIND [7] AS x MATCH () CREATE (c) RETURN *");
    assert_eq!(query.columns(), &["c", "x"]);
    let batch = run_matched(&query, &[VId(1)], &[], &Props::new()).unwrap();
    assert_eq!(
        values(&batch),
        vec![vec![GraphValue::Vertex(VId(100)), int(7)]]
    );
}

#[test]
fn imported_parameters_are_shared_and_null_or_wrong_kind_keys_never_match() {
    let calls = Cell::new(0);
    let template = PreparedGraphInsertQueryText::prepare(
        "UNWIND $keys AS x MATCH (a {p:x}) WHERE a.q > $floor \
         CREATE (c {q:a.q+$floor}) RETURN x,c.q AS q,$keys AS original",
        R,
        |kind, name: &str| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        },
    )
    .unwrap();
    assert_eq!(calls.get(), 2);
    let keys = vec![
        GraphValue::Scalar(CanonicalScalar::Null),
        GraphValue::Scalar(CanonicalScalar::ucs_basic_text("2").unwrap()),
        int(2),
    ];
    let arguments = GqlParameters::new()
        .with_list("keys", keys.clone())
        .unwrap()
        .with_int64("floor", 25)
        .unwrap();
    let query = template.bind_parameters(&arguments).unwrap();
    assert_eq!(calls.get(), 2);
    let batch = run_matched(&query, &[VId(1), VId(2), VId(3)], &[], &imported_props()).unwrap();
    assert_eq!(
        values(&batch),
        vec![vec![
            int(2),
            int(55),
            GraphValue::List(keys.into_boxed_slice())
        ]]
    );
    assert_eq!(batch.insertion().stats().created_vertices, 1);
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());
    assert!(
        template
            .bind_parameters(&arguments.with_int64("extra", 1).unwrap())
            .is_err()
    );
}

#[test]
fn imported_match_limits_output_without_suppressing_writes_or_expression_errors() {
    for (suffix, count) in [("RETURN DISTINCT x LIMIT 1", 1), ("RETURN x LIMIT 0", 0)] {
        let query = prepare(&format!(
            "UNWIND [2,2] AS x MATCH (a {{p:x}}) CREATE (a)-[:R]->(c) {suffix}"
        ));
        let batch = run_matched(&query, &[VId(1), VId(2), VId(3)], &[], &imported_props()).unwrap();
        assert_eq!(batch.returning().value.len(), count);
        assert_eq!(batch.insertion().stats().created_vertices, 4);
        assert_eq!(batch.insertion().stats().created_edges, 4);
    }
    let bad =
        prepare("UNWIND [1,2] AS x MATCH (a {p:x}) CREATE (c) RETURN 10/(2-x) AS value LIMIT 0");
    assert!(run_matched(&bad, &[VId(1), VId(2)], &[], &imported_props()).is_err());
    for list in ["[]", "NULL", "[99]"] {
        let query = prepare(&format!(
            "UNWIND {list} AS x MATCH (a {{p:x}}) CREATE (c {{p:1/0}}) RETURN 1/0 AS bad"
        ));
        let batch = run_matched(&query, &[VId(1), VId(2)], &[], &imported_props()).unwrap();
        assert!(batch.insertion().intents().is_empty());
        assert!(batch.returning().value.is_empty());
    }
}

#[test]
fn imported_match_rejects_ambiguous_scopes_and_overdeep_joins_before_catalog_calls() {
    for text in [
        "UNWIND [1] AS x MATCH (x) CREATE (c) RETURN c",
        "UNWIND [1] AS x MATCH (a)-[x:R]->(b) CREATE (c) RETURN c",
        "UNWIND [1] AS x MATCH (a) CREATE (x) RETURN x",
        "UNWIND [1] AS x MATCH (a) CREATE (c {p:missing}) RETURN c",
        "UNWIND [1] AS x MATCH (a) WHERE a.p=x OR a.q=1 CREATE (c) RETURN c",
        "UNWIND [1] AS x MATCH (a) OPTIONAL MATCH (b {p:x}) CREATE (c) RETURN c",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphInsertQueryText::prepare(text, R, |kind, name: &str| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "{text}");
    }
    let mut text = String::new();
    for index in 0..fgdb_gql::MAX_GRAPH_SET_DEPTH - 3 {
        text.push_str(&format!("UNWIND [1] AS x{index} "));
    }
    text.push_str("MATCH (a {p:x0}) CREATE (c) RETURN c");
    let calls = Cell::new(0);
    assert!(
        PreparedGraphInsertQueryText::prepare(&text, R, |kind, name: &str| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        })
        .is_err()
    );
    assert_eq!(calls.get(), 0);
}
