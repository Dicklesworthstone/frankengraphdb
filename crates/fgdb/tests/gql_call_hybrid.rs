//! `CALL hybrid.search(...)` inside a native GQL read. The oracle is the
//! library's own `beacon_search_graph` over an explicitly spelled projection
//! and query, so every lane combination must return exactly its hits, ranks
//! and fused scores, in its order. Composition, refusals and the capability
//! path (hidden vertices change nothing) follow.

use asupersync::security::key::AuthKey;
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{
    Database, DatabaseKeys, GqlError, HybridCallError, MemVfs, ProcedureError, QueryError,
    QueryResult, QueryValue, WriteBatch,
};
use fgdb_beacon::expansion::{ExpansionDirection, ExpansionLimits, ExpansionSpec};
use fgdb_beacon::read::{Projection, ReadOptions, ReadPolicy};
use fgdb_beacon::{
    DistanceMetric, ExactHybridQuery, ExactRrfProfile, GraphHybridHit, GraphHybridQuery,
    HnswConfig, IndexConfig, TextMatch, VectorSearch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSetExecutionError, GraphSymbol,
    GraphSymbolKind,
};
use fgdb_types::{
    CanonicalF64, CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x4b; 32]);
const CITES: RelationId = RelationId(1);
const SHORTCUT: RelationId = RelationId(2);
const DOC: LabelId = LabelId(1);
const SECRET: LabelId = LabelId(2);
const TITLE: PropertyKeyId = PropertyKeyId(1);
const BODY: PropertyKeyId = PropertyKeyId(2);
const E0: PropertyKeyId = PropertyKeyId(3);
const E1: PropertyKeyId = PropertyKeyId(4);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x4a; 32], NS, [0x4c; 32])
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "CITES") => Some(GraphSymbol::Relation(CITES)),
        (GraphSymbolKind::Label, "Doc") => Some(GraphSymbol::Label(DOC)),
        (GraphSymbolKind::Property, "title") => Some(GraphSymbol::Property(TITLE)),
        (GraphSymbolKind::Property, "body") => Some(GraphSymbol::Property(BODY)),
        (GraphSymbolKind::Property, "e0") => Some(GraphSymbol::Property(E0)),
        (GraphSymbolKind::Property, "e1") => Some(GraphSymbol::Property(E1)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 100_000, 100_000_000, 10_000_000)
}

fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}

fn float(value: f64) -> CanonicalScalar {
    CanonicalScalar::Float(CanonicalF64::new(value))
}

/// Six documents citing in a chain 1->2->3->4->5, with exact-f32 embeddings.
/// `hidden` adds a Secret vertex that would win every lane and bridge 1 to 5.
fn corpus(hidden: bool) -> Vec<WriteBatch> {
    let mut batch = WriteBatch::new(CITES);
    let docs: [(u128, &str, &str, f64, f64); 6] = [
        (1, "Ada", "analytical engine programs", 1.0, 0.0),
        (
            2,
            "Babbage",
            "difference engine and analytical engine",
            0.75,
            0.25,
        ),
        (
            3,
            "Turing",
            "computable numbers universal machine",
            0.0,
            1.0,
        ),
        (4, "Lovelace", "notes on the analytical engine", 0.5, 0.5),
        (
            5,
            "Hopper",
            "compilers for the engine of commerce",
            0.25,
            0.75,
        ),
        (
            6,
            "Shannon",
            "a mathematical theory of communication",
            0.125,
            0.875,
        ),
    ];
    for (id, title, body, e0, e1) in docs {
        batch.create_vertex(
            VId(id),
            vec![DOC],
            vec![
                (TITLE, text(title)),
                (BODY, text(body)),
                (E0, float(e0)),
                (E1, float(e1)),
            ],
        );
    }
    for edge in 1..=4_u128 {
        batch.add_edge(EId(edge), VId(edge), VId(edge + 1), vec![]);
    }
    let mut batches = Vec::new();
    if hidden {
        batch.create_vertex(
            VId(90),
            vec![SECRET],
            vec![
                (TITLE, text("Secret")),
                (BODY, text("engine engine analytical engine")),
                (E0, float(1.0)),
                (E1, float(0.0)),
            ],
        );
        batch.add_edge(EId(90), VId(1), VId(90), vec![]);
        batch.add_edge(EId(91), VId(90), VId(5), vec![]);
        batches.push(batch);
        let mut shortcut = WriteBatch::new(SHORTCUT);
        shortcut.add_edge(EId(92), VId(1), VId(6), vec![]);
        batches.push(shortcut);
    } else {
        batches.push(batch);
    }
    batches
}

async fn open(commit: &fgdb_types::CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::<MemVfs>::open_memory(commit, keys())
        .await
        .unwrap();
    for batch in corpus(hidden) {
        db.write(commit, batch).await.unwrap();
    }
    db
}

fn run<T>(test: impl AsyncFnOnce(&fgdb_types::CommitCx, &QueryCx) -> T) -> T {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let cx = contexts.query();
    runtime.block_on(test(&commit, &cx))
}

fn rows(result: QueryResult) -> (Vec<String>, Vec<Vec<GraphValue>>) {
    let QueryResult::Rows { columns, rows } = result else {
        return (vec!["<not a row result>".to_owned()], Vec::new());
    };
    let rows = rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| match cell {
                    QueryValue::Value(value) => Some(value),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .expect("every cell is a plain value")
        })
        .collect();
    (columns, rows)
}

/// A native read without ORDER BY returns its rows in canonical order, not
/// the procedure's: put them in vertex order so two answers compare as sets
/// of exact rows. Rank order itself is checked through ORDER BY score.
fn ordered(
    (columns, mut rows): (Vec<String>, Vec<Vec<GraphValue>>),
) -> (Vec<String>, Vec<Vec<GraphValue>>) {
    let vertex = |row: &Vec<GraphValue>| match row.first() {
        Some(GraphValue::Vertex(id)) => id.0,
        _ => u128::MAX,
    };
    rows.sort_by_key(vertex);
    (columns, rows)
}

/// The oracle's hits as the YIELD node, score, vector_rank, text_rank,
/// graph_rank, graph_hops rows the procedure must produce.
fn expected(hits: Vec<GraphHybridHit>) -> Vec<Vec<GraphValue>> {
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    let int = |value: Option<u32>| {
        value.map_or(null.clone(), |value| {
            GraphValue::Scalar(CanonicalScalar::Int(i64::from(value)))
        })
    };
    hits.into_iter()
        .map(|hit| {
            vec![
                GraphValue::Vertex(hit.id),
                GraphValue::Scalar(CanonicalScalar::Decimal(hit.decimal_score)),
                int(hit.vector_rank.map(|rank| rank.get())),
                int(hit.text_rank.map(|rank| rank.get())),
                int(hit.graph_rank.map(|rank| rank.get())),
                int(hit.graph_hops),
            ]
        })
        .collect()
}

const OUTPUTS: &str = "YIELD node, score, vector_rank, text_rank, graph_rank, graph_hops \
     RETURN node, score, vector_rank, text_rank, graph_rank, graph_hops";

fn options(text: bool, vector: bool) -> ReadOptions<PropertyKeyId, LabelId> {
    ReadOptions {
        as_of: None,
        vertex_label: None,
        projection: Projection {
            text: text.then_some(BODY),
            vector: if vector { vec![E0, E1] } else { Vec::new() },
        },
        index: IndexConfig {
            vector: vector.then(|| HnswConfig::new(2, DistanceMetric::Cosine)),
            ..IndexConfig::default()
        },
        policy: ReadPolicy::default(),
    }
}

fn query<'a>(
    text: &'a str,
    vector: &'a [f32],
    k: usize,
    candidates: u32,
    graph_candidates: u32,
) -> GraphHybridQuery<'a> {
    let weight = |enabled: bool| u16::from(enabled);
    GraphHybridQuery {
        retrieval: ExactHybridQuery {
            vector,
            text,
            k,
            vector_candidates: if vector.is_empty() { 0 } else { candidates },
            text_candidates: if text.is_empty() { 0 } else { candidates },
            vector_mode: VectorSearch::Exact,
            text_mode: TextMatch::Any,
            profile: ExactRrfProfile::new(60, weight(!vector.is_empty()), weight(!text.is_empty()))
                .unwrap(),
        },
        graph_candidates,
        graph_weight: weight(graph_candidates != 0),
    }
}

#[test]
fn every_lane_combination_equals_the_library_search() {
    run(async |commit, cx| {
        let db = open(commit, false).await;
        let embedding = [0.5_f32, 0.5];
        let parameters = GqlParameters::new()
            .with_list(
                "q",
                embedding
                    .iter()
                    .map(|x| GraphValue::Scalar(float(f64::from(*x))))
                    .collect(),
            )
            .unwrap();
        let none = ExpansionSpec {
            seeds: &[],
            relation: Some(CITES),
            direction: ExpansionDirection::Outgoing,
            max_hops: 0,
            include_seeds: false,
            limits: ExpansionLimits::default(),
        };
        let seeds = [VId(1)];
        let chain = ExpansionSpec {
            seeds: &seeds,
            max_hops: 2,
            ..none
        };
        let cases = [
            (
                "text => 'analytical engine', text_property => 'body', k => 3",
                options(true, false),
                query("analytical engine", &[], 3, 3, 0),
                none,
            ),
            (
                "vector => $q, vector_properties => ['e0', 'e1'], metric => 'cosine', k => 3",
                options(false, true),
                query("", &embedding, 3, 3, 0),
                none,
            ),
            (
                "text => 'engine', text_property => 'body', vector => $q, \
                 vector_properties => ['e0', 'e1'], metric => 'cosine', candidates => 4, \
                 k => 5, fusion => 'RRF'",
                options(true, true),
                query("engine", &embedding, 5, 4, 0),
                none,
            ),
            (
                "text => 'analytical', text_property => 'body', seeds => [1], \
                 relation => 'CITES', max_hops => 2, k => 4",
                options(true, false),
                query("analytical", &[], 4, 4, 4),
                chain,
            ),
        ];
        for (arguments, options, oracle, expansion) in cases {
            let hits = db
                .beacon_search_graph(cx, &options, oracle, expansion)
                .unwrap();
            assert!(hits.len() >= 3, "{arguments}: a thin oracle proves little");
            let best: Vec<GraphValue> = hits.iter().map(|hit| GraphValue::Vertex(hit.id)).collect();
            let want = ordered((Vec::new(), expected(hits))).1;
            let parameters = if arguments.contains("$q") {
                parameters.clone()
            } else {
                GqlParameters::new()
            };
            let text = format!("CALL hybrid.search({arguments}) {OUTPUTS}");
            let (columns, actual) = ordered(rows(
                db.query(cx, &text, &parameters, symbols, policy()).unwrap(),
            ));
            assert_eq!(
                columns,
                [
                    "node",
                    "score",
                    "vector_rank",
                    "text_rank",
                    "graph_rank",
                    "graph_hops"
                ]
            );
            assert_eq!(actual, want, "{arguments}");
            // Ordered by the fused score, the rows come best first, ties by
            // ascending vertex: the oracle's own order.
            let ranked = format!(
                "CALL hybrid.search({arguments}) YIELD node, score \
                 RETURN node, score ORDER BY score DESC, node"
            );
            let (_, ranked) = rows(
                db.query(cx, &ranked, &parameters, symbols, policy())
                    .unwrap(),
            );
            let order: Vec<GraphValue> = ranked.into_iter().map(|row| row[0].clone()).collect();
            assert_eq!(order, best, "{arguments}");
        }
    });
}

/// The README's GraphRAG shape: retrieval, then the statement reads the hit
/// vertices' properties and orders and cuts by the fused score.
#[test]
fn retrieval_composes_with_property_reads_ordering_and_limits() {
    run(async |commit, cx| {
        let db = open(commit, false).await;
        let oracle = db
            .beacon_search_graph(
                cx,
                &options(true, false),
                query("analytical engine", &[], 4, 4, 0),
                ExpansionSpec {
                    seeds: &[],
                    relation: None,
                    direction: ExpansionDirection::Outgoing,
                    max_hops: 0,
                    include_seeds: false,
                    limits: ExpansionLimits::default(),
                },
            )
            .unwrap();
        let titles = [
            "", "Ada", "Babbage", "Turing", "Lovelace", "Hopper", "Shannon",
        ];
        let want: Vec<Vec<GraphValue>> = oracle[..2]
            .iter()
            .map(|hit| {
                vec![
                    GraphValue::Scalar(text(titles[hit.id.0 as usize])),
                    GraphValue::Scalar(CanonicalScalar::Decimal(hit.decimal_score)),
                ]
            })
            .collect();
        let (columns, actual) = rows(
            db.query(
                cx,
                "CALL hybrid.search(text => 'analytical engine', text_property => 'body', \
                 k => 4) YIELD node, score RETURN node.title AS title, score \
                 ORDER BY score DESC LIMIT 2",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap(),
        );
        assert_eq!(columns, ["title", "score"]);
        assert_eq!(actual, want);
    });
}

fn search_error(error: QueryError) -> Result<HybridCallError, QueryError> {
    match error {
        QueryError::Set(GqlQueryError::Source(GraphSetExecutionError::Source(
            GqlError::Procedure(ProcedureError::Search(error)),
        ))) => Ok(error),
        other => Err(other),
    }
}

#[test]
fn malformed_calls_refuse_with_typed_reasons() {
    run(async |commit, cx| {
        let db = open(commit, false).await;
        let query = |text: &str| db.query(cx, text, &GqlParameters::new(), symbols, policy());
        let refusal = |text: &str| search_error(query(text).unwrap_err()).unwrap();
        assert!(matches!(
            refusal("CALL hybrid.search('engine') YIELD node RETURN node"),
            HybridCallError::Positional
        ));
        assert!(matches!(
            refusal("CALL hybrid.find(text => 'engine') YIELD node RETURN node"),
            HybridCallError::UnknownProcedure(name) if name == "find"
        ));
        assert!(matches!(
            refusal("CALL hybrid.search(text => 'engine', text_property => 'body', top => 3) YIELD node RETURN node"),
            HybridCallError::UnknownArgument(name) if name == "top"
        ));
        assert!(matches!(
            refusal("CALL hybrid.search(text => 'engine') YIELD node RETURN node"),
            HybridCallError::Combination(_)
        ));
        assert!(matches!(
            refusal(
                "CALL hybrid.search(text => 'engine', text_property => 'body', max_hops => 2) YIELD node RETURN node"
            ),
            HybridCallError::Combination(_)
        ));
        assert!(matches!(
            refusal(
                "CALL hybrid.search(text => 'engine', text_property => 'body', k => 0) YIELD node RETURN node"
            ),
            HybridCallError::Argument { name: "k", .. }
        ));
        assert!(matches!(
            refusal("CALL hybrid.search(text => 'engine', text_property => 'body') YIELD rank RETURN rank"),
            HybridCallError::Yield(name) if name == "rank"
        ));
        assert!(matches!(
            refusal(
                "CALL hybrid.search(text => 'engine', text_property => 'body') YIELD score MATCH (score) RETURN score"
            ),
            HybridCallError::NotVertex(name) if name == "score"
        ));
        // A schema name resolves at prepare time: an unbound one never
        // reaches execution, and a value cannot stand in for a name.
        assert!(
            query(
                "CALL hybrid.search(text => 'x', text_property => 'nope') YIELD node RETURN node"
            )
            .is_err()
        );
        assert!(
            query("CALL hybrid.search(text => 'x', text_property => 2) YIELD node RETURN node")
                .is_err()
        );
        // Prism keeps its positional signatures.
        let prism = query("CALL fnx.pagerank(alpha => 0.85) YIELD node, score RETURN node, score")
            .unwrap_err();
        assert!(
            matches!(
                prism,
                QueryError::Set(GqlQueryError::Source(GraphSetExecutionError::Source(
                    GqlError::Procedure(ProcedureError::Bind(_))
                )))
            ),
            "{prism:?}"
        );
    });
}

/// Under a capability that sees only Doc vertices and CITES edges, the hidden
/// Secret vertex (the best text and vector match, and a bridge to Hopper) and
/// the forbidden shortcut change nothing: the answer equals the database that
/// never held them.
#[test]
fn a_capability_search_equals_the_physically_restricted_corpus() {
    run(async |commit, cx| {
        let full = open(commit, true).await;
        let clean = open(commit, false).await;
        let authority =
            Authority::new(AuthKey::from_seed(4401), NS, "main", SchemaEpoch(0), 1).unwrap();
        let mut grant = Grant::read_only(
            "main",
            1000,
            QueryLimits {
                max_nodes: 1000,
                max_work: 100_000_000,
                max_rows: 100,
            },
        );
        grant.labels = Scope::only([DOC]);
        grant.relations = Scope::only([CITES]);
        grant.properties = Scope::only([TITLE, BODY, E0, E1]);
        let token = authority.issue_at(&grant, 100).unwrap();
        let parameters = GqlParameters::new()
            .with_list(
                "q",
                vec![
                    GraphValue::Scalar(float(1.0)),
                    GraphValue::Scalar(float(0.0)),
                ],
            )
            .unwrap();
        let text = format!(
            "CALL hybrid.search(text => 'analytical engine', text_property => 'body', \
             vector => $q, vector_properties => ['e0', 'e1'], seeds => [1], \
             direction => 'both', max_hops => 4, k => 6) {OUTPUTS}"
        );
        let scoped = ordered(rows(
            full.query_authorized(
                cx,
                &authority,
                &token,
                "main",
                &text,
                &parameters,
                symbols,
                policy(),
                || 100,
            )
            .unwrap(),
        ));
        let restricted = ordered(rows(
            clean
                .query(cx, &text, &parameters, symbols, policy())
                .unwrap(),
        ));
        assert_eq!(scoped, restricted);
        let unscoped = ordered(rows(
            full.query(cx, &text, &parameters, symbols, policy())
                .unwrap(),
        ));
        assert_ne!(
            unscoped, restricted,
            "the hidden vertex must change an unscoped search, not be an inert fixture"
        );
        assert_eq!(scoped.1.len(), 6, "every lane contributes under the grant");
        assert!(
            scoped
                .1
                .iter()
                .all(|row| row[0] != GraphValue::Vertex(VId(90)))
        );
    });
}
