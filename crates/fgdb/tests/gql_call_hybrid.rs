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
use fgdb_beacon::read::{Projection, ReadOptions, ReadPolicy, VectorEncoding};
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
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalF64, CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};
use std::sync::Arc;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x4b; 32]);
const CITES: RelationId = RelationId(1);
const SHORTCUT: RelationId = RelationId(2);
const DOC: LabelId = LabelId(1);
const SECRET: LabelId = LabelId(2);
const TITLE: PropertyKeyId = PropertyKeyId(1);
const BODY: PropertyKeyId = PropertyKeyId(2);
const E0: PropertyKeyId = PropertyKeyId(3);
const E1: PropertyKeyId = PropertyKeyId(4);
const EMB: PropertyKeyId = PropertyKeyId(5);

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
        (GraphSymbolKind::Property, "emb") => Some(GraphSymbol::Property(EMB)),
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

/// An embedding as one property: little-endian f32 bytes.
fn packed(values: &[f32]) -> CanonicalScalar {
    CanonicalScalar::bytes(values.iter().flat_map(|v| v.to_le_bytes()).collect()).unwrap()
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
                (EMB, packed(&[e0 as f32, e1 as f32])),
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

fn run_transaction<T>(
    test: impl AsyncFnOnce(&fgdb_types::CommitCx, &QueryCx, &fgdb_types::TxnCx) -> T,
) -> T {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(test(&contexts.commit(), &contexts.query(), &contexts.txn()))
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
            encoding: VectorEncoding::Coordinates,
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
    run_transaction(async |commit, cx, txcx| {
        let db = open(commit, false).await;
        let txn = db.begin(txcx).unwrap();
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
            let (_, actual) = ordered(rows(
                txn.query(&db, cx, &text, &parameters, symbols, policy())
                    .unwrap(),
            ));
            assert_eq!(actual, want, "transaction {arguments}");
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
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

/// An embedding stored as ONE packed f32 byte property searches exactly like
/// the same coordinates stored one property each: same hits, ranks, scores.
#[test]
fn a_packed_embedding_property_equals_its_per_coordinate_form() {
    run(async |commit, cx| {
        let db = open(commit, false).await;
        let parameters = GqlParameters::new()
            .with_list(
                "q",
                vec![
                    GraphValue::Scalar(float(0.5)),
                    GraphValue::Scalar(float(0.5)),
                ],
            )
            .unwrap();
        let search = |vector: &str| {
            format!(
                "CALL hybrid.search(text => 'engine', text_property => 'body', vector => $q, \
                 {vector}, metric => 'cosine', candidates => 4, k => 5) {OUTPUTS}"
            )
        };
        let coordinates = ordered(rows(
            db.query(
                cx,
                &search("vector_properties => ['e0', 'e1']"),
                &parameters,
                symbols,
                policy(),
            )
            .unwrap(),
        ));
        let packed = ordered(rows(
            db.query(
                cx,
                &search("vector_property => 'emb'"),
                &parameters,
                symbols,
                policy(),
            )
            .unwrap(),
        ));
        assert_eq!(coordinates.1.len(), 5);
        assert_eq!(packed, coordinates);
        // The two forms are alternatives, and a packed property must hold
        // exactly 4 * dimensions bytes.
        let both = db.query(
            cx,
            &search("vector_properties => ['e0', 'e1'], vector_property => 'emb'"),
            &parameters,
            symbols,
            policy(),
        );
        assert!(matches!(
            search_error(both.unwrap_err()),
            Ok(HybridCallError::Combination(_))
        ));
        let three = GqlParameters::new()
            .with_list(
                "q",
                vec![
                    GraphValue::Scalar(float(0.5)),
                    GraphValue::Scalar(float(0.5)),
                    GraphValue::Scalar(float(0.5)),
                ],
            )
            .unwrap();
        let wrong = db.query(
            cx,
            &search("vector_property => 'emb'"),
            &three,
            symbols,
            policy(),
        );
        assert!(matches!(
            search_error(wrong.unwrap_err()),
            Ok(HybridCallError::Index(_))
        ));
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

/// openCypher's standalone CALL: with no RETURN the read returns exactly the
/// yielded columns, the same rows as an explicit `RETURN` of them.
#[test]
fn a_standalone_call_returns_its_yielded_columns() {
    run(async |commit, cx| {
        let db = open(commit, false).await;
        let query = |text: &str| {
            ordered(rows(
                db.query(cx, text, &GqlParameters::new(), symbols, policy())
                    .unwrap(),
            ))
        };
        let call = "CALL hybrid.search(text => 'engine', text_property => 'body', k => 3) \
                    YIELD node, score AS fused";
        let standalone = query(call);
        assert_eq!(standalone.0, ["node", "fused"]);
        assert_eq!(standalone.1.len(), 3);
        assert_eq!(standalone, query(&format!("{call} RETURN node, fused")));
        let prism = "CALL fnx.pagerank() YIELD node, score";
        assert_eq!(query(prism), query(&format!("{prism} RETURN node, score")));
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

/// The native transaction host must use the same staged corpus and topology
/// as the library search, and later MATCH/WITH stages must read its properties.
#[test]
fn transaction_hybrid_calls_read_staged_corpus_topology_and_properties() {
    run_transaction(async |commit, cx, txcx| {
        let mut db = open(commit, false).await;
        let frontier = db.frontier().unwrap();
        let mut txn = db.begin(txcx).unwrap();
        let mut change = WriteBatch::new(CITES);
        change.delete_vertex(VId(3)); // cascades 2->3 and 3->4
        change.set_vertex_label(VId(5), DOC, false);
        change.set_vertex_property(VId(6), TITLE, Some(text("Updated Shannon")));
        change.set_vertex_property(VId(6), BODY, Some(text("engine engine")));
        change.set_vertex_property(VId(6), E0, Some(float(0.5)));
        change.set_vertex_property(VId(6), E1, Some(float(0.5)));
        change.set_vertex_property(VId(6), EMB, Some(packed(&[0.5, 0.5])));
        change.create_vertex(
            VId(7),
            vec![DOC],
            vec![
                (TITLE, text("New")),
                (BODY, text("analytical engine engine")),
                (E0, float(0.5)),
                (E1, float(0.5)),
                (EMB, packed(&[0.5, 0.5])),
            ],
        );
        let mut shortcut = WriteBatch::new(SHORTCUT);
        shortcut.add_edge(EId(70), VId(1), VId(6), vec![]);
        shortcut.add_edge(EId(71), VId(6), VId(7), vec![]);
        txn.write_ordered(&mut db, vec![change, shortcut]).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let embedding = [0.5_f32, 0.5];
        let parameters = GqlParameters::new()
            .with_list(
                "q",
                embedding
                    .iter()
                    .map(|value| GraphValue::Scalar(float(f64::from(*value))))
                    .collect(),
            )
            .unwrap();
        let mut opts = options(true, true);
        opts.vertex_label = Some(DOC);
        let hits = txn
            .beacon_search_graph(
                &db,
                cx,
                &opts,
                query("engine", &embedding, 10, 10, 10),
                ExpansionSpec {
                    seeds: &[VId(1)],
                    relation: None,
                    direction: ExpansionDirection::Outgoing,
                    max_hops: 2,
                    include_seeds: false,
                    limits: ExpansionLimits::default(),
                },
            )
            .unwrap();
        let mut hops: Vec<_> = hits
            .iter()
            .filter_map(|hit| hit.graph_hops.map(|hops| (hit.id, hops)))
            .collect();
        hops.sort();
        assert_eq!(hops, [(VId(2), 1), (VId(6), 1), (VId(7), 2)]);
        let want = ordered((Vec::new(), expected(hits))).1;
        assert_eq!(
            want.iter().map(|row| row[0].clone()).collect::<Vec<_>>(),
            [1, 2, 4, 6, 7].map(|id| GraphValue::Vertex(VId(id))),
        );
        let call = |vector: &str| {
            format!(
                "CALL hybrid.search(text => 'engine', text_property => 'body', vector => $q, \
                 {vector}, metric => 'cosine', label => 'Doc', seeds => [1], \
                 max_hops => 2, candidates => 10, k => 10)"
            )
        };
        let coordinates = call("vector_properties => ['e0', 'e1']");
        let read = format!("{coordinates} {OUTPUTS}");
        let answer = txn
            .query(&db, cx, &read, &parameters, symbols, policy())
            .unwrap();
        assert_eq!(ordered(rows(answer.clone())).1, want);
        assert_eq!(
            ordered(rows(
                txn.query(
                    &db,
                    cx,
                    &format!("{} {OUTPUTS}", call("vector_property => 'emb'")),
                    &parameters,
                    symbols,
                    policy(),
                )
                .unwrap(),
            )),
            ordered(rows(answer.clone())),
        );
        let composed = format!(
            "{coordinates} YIELD node, score, graph_hops MATCH (node:Doc) \
             WITH node, node.title AS title, score, graph_hops \
             RETURN node, title, score, graph_hops ORDER BY node"
        );
        let want_properties: Vec<_> = want
            .iter()
            .zip(["Ada", "Babbage", "Lovelace", "Updated Shannon", "New"])
            .map(|(row, title)| {
                vec![
                    row[0].clone(),
                    GraphValue::Scalar(text(title)),
                    row[1].clone(),
                    row[5].clone(),
                ]
            })
            .collect();
        assert_eq!(
            rows(
                txn.query(&db, cx, &composed, &parameters, symbols, policy())
                    .unwrap()
            )
            .1,
            want_properties,
        );
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert!(matches!(
            txn.finish(&mut db, commit).await.unwrap(),
            fgdb_types::EmbeddedTxnCompletion::WriteCommitted { .. },
        ));
        assert_eq!(
            db.query(cx, &read, &parameters, symbols, policy()).unwrap(),
            answer,
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

/// Neither LIMIT 0 nor a projection failure followed by savepoint rollback
/// may erase the table observations that ranking and traversal already made.
#[test]
fn transaction_hybrid_calls_retain_hidden_and_failed_read_dependencies() {
    run_transaction(async |commit, cx, txcx| {
        let params = GqlParameters::new();
        let mut db = open(commit, false).await;
        let mut txn = db.begin(txcx).unwrap();
        let hidden = "CALL hybrid.search(text => 'quasar', text_property => 'body', \
                      seeds => [1], max_hops => 3, k => 10) \
                      YIELD node RETURN node LIMIT 0";
        assert!(
            rows(
                txn.query(&db, cx, hidden, &params, symbols, policy())
                    .unwrap()
            )
            .1
            .is_empty()
        );
        let mut winner = WriteBatch::new(SHORTCUT);
        winner.add_edge(EId(70), VId(1), VId(6), vec![]);
        db.write(commit, winner).await.unwrap();
        let frontier = db.frontier().unwrap();
        assert!(matches!(
            txn.finish(&mut db, commit).await,
            Err(fgdb::WriteTxnError::Write(
                fgdb::WriteError::FirstCommitterWins { .. }
            )),
        ));
        assert_eq!(db.frontier().unwrap(), frontier);

        let mut db = open(commit, false).await;
        let mut txn = db.begin(txcx).unwrap();
        txn.savepoint(&db, "before_bad_embedding").unwrap();
        let mut bad = WriteBatch::new(CITES);
        bad.set_vertex_property(
            VId(6),
            EMB,
            Some(CanonicalScalar::bytes(vec![1, 2, 3]).unwrap()),
        );
        txn.write(&mut db, bad).unwrap();
        let error = txn
            .query(
                &db,
                cx,
                "CALL hybrid.search(vector => [0.5, 0.5], vector_property => 'emb', \
                 label => 'Doc', k => 10) YIELD node RETURN node",
                &params,
                symbols,
                policy(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            QueryError::TransactionSet(ref error) if matches!(error.as_ref(),
                GqlQueryError::Source(GraphSetExecutionError::Source(
                    fgdb::WriteTxnError::Gql(GqlError::Procedure(ProcedureError::Search(
                        HybridCallError::Index(fgdb_beacon::BeaconError::InvalidQuery(
                            "packed vector byte length is not 4 * dimensions"
                        ))
                    )))
                )))
        ));
        txn.rollback_to_savepoint(&db, "before_bad_embedding")
            .unwrap();
        // No second search: only the failed CALL observed this label's
        // absent future members. Rolling back the bad write must retain it.
        let mut winner = WriteBatch::new(CITES);
        winner.create_vertex(VId(99), vec![DOC], vec![(EMB, packed(&[0.5, 0.5]))]);
        db.write(commit, winner).await.unwrap();
        let frontier = db.frontier().unwrap();
        assert!(matches!(
            txn.finish(&mut db, commit).await,
            Err(fgdb::WriteTxnError::Write(
                fgdb::WriteError::FirstCommitterWins { .. }
            )),
        ));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn transaction_hybrid_calls_share_allowances_and_preserve_interruption() {
    run_transaction(async |commit, cx, txcx| {
        let db = open(commit, false).await;
        let frontier = db.frontier().unwrap();
        let params = GqlParameters::new();
        let txn = db.begin(txcx).unwrap();
        // The text lane has no matches; the graph lane supplies nodes 2,3,4.
        let call = "CALL hybrid.search(text => 'quasar', text_property => 'body', \
                    seeds => [1], max_hops => 3, k => 10) YIELD node";
        let text = format!("{call} RETURN node");
        let prepare = |text: &str| {
            fgdb_gql::PreparedGraphSetText::prepare(text, symbols)
                .unwrap()
                .bind_parameters(&params)
                .unwrap()
        };
        let single = prepare(&text);
        // Read witnesses allocate only on their first observation. Warm
        // them before comparing exact per-execution work/scratch boundaries.
        let warm = txn
            .execute_graph_set_governed(&db, cx, &single, policy())
            .unwrap();
        assert_eq!(warm.rows.snapshot_records, 10); // six vertices + four edges
        assert_eq!(warm.value.len(), 3);
        let combined = prepare(&format!("{text} UNION ALL {text}"));
        let result = txn
            .execute_graph_set_governed(&db, cx, &combined, policy())
            .unwrap();
        assert_eq!(result.rows.snapshot_records, 20);
        assert_eq!(result.value.len(), 6);
        let exact = GqlQueryPolicy::new(
            result.rows.snapshot_records,
            result.rows.result_rows,
            result.evaluator.work_units,
            result.evaluator.scratch_entries,
        );
        assert_eq!(
            txn.execute_graph_set_governed(&db, cx, &combined, exact)
                .unwrap(),
            result,
        );
        let short_records = GqlQueryPolicy::new(19, 100, 100_000_000, 10_000_000);
        assert!(matches!(
            txn.execute_graph_set_governed(&db, cx, &combined, short_records),
            Err(GqlQueryError::Rows(error))
                if error.dimension == fgdb_gql::GqlBudgetDimension::SnapshotRecords,
        ));
        for work in [false, true] {
            let mut short = exact;
            if work {
                short.evaluator.max_work_units -= 1;
            } else {
                short.evaluator.max_scratch_entries -= 1;
            }
            assert!(
                txn.execute_graph_set_governed(&db, cx, &combined, short)
                    .is_err(),
                "every source, lane and conversion must spend the shared allowance",
            );
        }
        // Three private CALL hit rows must fit a one-row aggregate result.
        assert_eq!(
            txn.query(
                &db,
                cx,
                &format!("{call} RETURN COUNT(*) AS c"),
                &params,
                symbols,
                GqlQueryPolicy::new(1_000_000, 1, 100_000_000, 10_000_000),
            )
            .unwrap(),
            QueryResult::Rows {
                columns: vec!["c".to_owned()],
                rows: vec![vec![QueryValue::Count(3)]],
            },
        );
        let digest = txn.staged_effect_digest().unwrap();
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let expected = txn
            .execute_graph_set_governed(
                &db,
                &cx.with_checkpoint_probe(probe.clone()),
                &single,
                policy(),
            )
            .unwrap();
        let calls = probe.calls();
        assert!(calls > 20);
        for stop in [1, calls / 2, calls] {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let result = txn.execute_graph_set_governed(
                &db,
                &cx.with_checkpoint_probe(probe.clone()),
                &single,
                policy(),
            );
            assert!(
                matches!(result, Err(GqlQueryError::Interrupted(_))),
                "stop={stop}: {result:?}",
            );
            assert_eq!(probe.calls(), stop, "continued after interruption");
        }
        assert_eq!(
            txn.execute_graph_set_governed(&db, cx, &single, policy())
                .unwrap(),
            expected,
        );
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}
