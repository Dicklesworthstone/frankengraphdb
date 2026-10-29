//! INSERT must preserve CREATE's native program, receipts and complete graph state.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
    GraphWriteStepReceipt, PreparedGraphInsertText, PreparedGraphWriteProgram,
    PreparedGraphWriteScript,
};
use fgdb_types::{
    CanonicalPropertyValue, CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId,
    EmbeddedTxnCompletion, PurposeContexts, QueryCx, VId,
};
use std::collections::VecDeque;

const R: RelationId = RelationId(1);
/// A second relation type, so a chain writes more than one (Merge5 [4]).
const T: RelationId = RelationId(2);
const PERSON: LabelId = LabelId(1);
const SOURCE: LabelId = LabelId(2);
const COPY: LabelId = LabelId(3);
/// Labels and a type spelled like the COUNT and EXISTS subqueries.
const COUNT: LabelId = LabelId(4);
const EXISTS: LabelId = LabelId(5);
const COUNTED: RelationId = RelationId(3);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const NAME: PropertyKeyId = PropertyKeyId(3);
const BORN: PropertyKeyId = PropertyKeyId(4);
const SINCE: PropertyKeyId = PropertyKeyId(5);
const SEEDS: [u64; 3] = [0x1a5e_0001, 0x1a5e_1027, 0x1a5e_a913];

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x49; 32],
        DatabaseSecurityNamespaceId([0x4a; 32]),
        [0x4b; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R" | "KNOWS") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "T") => Some(GraphSymbol::Relation(T)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Label, "Source") => Some(GraphSymbol::Label(SOURCE)),
        (GraphSymbolKind::Label, "Copy") => Some(GraphSymbol::Label(COPY)),
        (GraphSymbolKind::Label, "Count") => Some(GraphSymbol::Label(COUNT)),
        (GraphSymbolKind::Label, "Exists") => Some(GraphSymbol::Label(EXISTS)),
        (GraphSymbolKind::Relation, "COUNT") => Some(GraphSymbol::Relation(COUNTED)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(NAME)),
        (GraphSymbolKind::Property, "born") => Some(GraphSymbol::Property(BORN)),
        (GraphSymbolKind::Property, "since") => Some(GraphSymbol::Property(SINCE)),
        _ => None,
    }
}

fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000),
        1_000,
        1_000,
        1_000,
    )
}

async fn fresh(cx: &CommitCx, seeded: bool) -> Database<MemVfs> {
    let vfs = MemVfs::new().unwrap();
    let path = vfs.database_dir();
    let mut db = Database::create_with_vfs(cx, vfs, &path, keys())
        .await
        .unwrap();
    if seeded {
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(10), vec![SOURCE], vec![(P, CanonicalScalar::Int(10))]);
        batch.create_vertex(VId(20), vec![SOURCE], vec![(P, CanonicalScalar::Int(20))]);
        batch.add_edge(
            EId(30),
            VId(10),
            VId(20),
            vec![(Q, CanonicalScalar::Int(30))],
        );
        db.write(cx, batch).await.unwrap();
    }
    db
}

// query_write takes an external allocator. Every value supplied here is first
// reserved by the owning Database's production allocator, never synthesized.
fn reserve(
    db: &mut Database<MemVfs>,
    cx: &QueryCx,
    vertices: usize,
    edges: usize,
) -> (VecDeque<ElementId>, VecDeque<ElementId>) {
    let vertices = (0..vertices)
        .map(|vertex| {
            db.allocate_identity(cx, GraphInsertRequest::Vertex { row: 0, vertex })
                .unwrap()
        })
        .collect();
    let edges = (0..edges)
        .map(|edge| {
            db.allocate_identity(cx, GraphInsertRequest::Edge { row: 0, edge })
                .unwrap()
        })
        .collect();
    (vertices, edges)
}

struct Case {
    create: String,
    params: GqlParameters,
    vertices: usize,
    edges: usize,
}

fn corpus(seed: u64) -> Vec<Case> {
    let mut state = seed;
    (0..12)
        .map(|shape| {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let value = (state % 1_000) as i64 + 1;
            let (create, vertices, edges) = match shape {
                0 => (format!("CREATE ({{p:{value}}})"), 1, 0),
                1 => (format!("CREATE (:Person {{p:{value},q:NULL}})"), 1, 0),
                2 => (format!("CREATE (:Person:Copy {{q:-{value},p:{value}}})"), 1, 0),
                3 => (format!("CREATE (a:Copy {{p:{value}}})-[:R {{q:{value}}}]->(b:Person)"), 2, 1),
                4 => (format!("CREATE (a:Copy)<-[:R {{p:{value}}}]-(b:Person {{q:{value}}})"), 2, 1),
                5 => (format!("CREATE (a:Copy {{p:{value}}})-[:R]->(a)"), 1, 1),
                6 => (format!("CREATE (a:Copy {{p:{value}}}),(b:Person),(a)-[:R]->(b),(a)-[:R {{q:NULL}}]->(b)"), 2, 2),
                7 => ("CREATE (:Copy {p:$value,q:$value+1})".to_owned(), 1, 0),
                8 => (format!("MATCH (a:Source) CREATE (b:Copy {{p:a.p+{value}}}),(a)-[:R {{q:a.p}}]->(b)"), 2, 2),
                9 => ("MATCH (a:Source)-[:R]->(b) CREATE (c:Copy {p:a.p+$value,q:b.p}),(b)-[:R]->(c)".to_owned(), 1, 1),
                10 => (format!("MATCH (a:Source) WHERE a.p=10 CREATE (a)-[:R {{p:{value}}}]->(a)"), 0, 1),
                11 => (format!("MATCH (a:Source),(b:Source) WHERE a.p=10 AND b.p=20 CREATE (a)-[:R {{q:{value}}}]->(b)"), 0, 1),
                _ => unreachable!(),
            };
            let params = if matches!(shape, 7 | 9) {
                GqlParameters::new().with_int64("value", value).unwrap()
            } else {
                GqlParameters::new()
            };
            Case { create, params, vertices, edges }
        })
        .collect()
}

fn program(text: &str, params: &GqlParameters) -> PreparedGraphWriteProgram {
    let declarations = params.parameter_types().collect::<Vec<_>>();
    PreparedGraphWriteScript::prepare_with_parameter_types(text, R, &declarations, symbols)
        .unwrap()
        .bind_parameters(params)
        .unwrap()
}

async fn equivalent(contexts: &PurposeContexts, case: &Case, transactional: bool) {
    let commit = contexts.commit();
    let cx = contexts.query();
    let txcx = contexts.txn();
    let insert = case.create.replace("CREATE", "INSERT");
    let create_program = program(&case.create, &case.params);
    let insert_program = program(&insert, &case.params);
    assert_eq!(
        create_program.canonical_bytes(),
        insert_program.canonical_bytes(),
        "{}",
        case.create
    );
    let mut create_db = fresh(&commit, true).await;
    let mut insert_db = fresh(&commit, true).await;
    let mut receipts = Vec::new();
    for (db, text, bound) in [
        (&mut create_db, case.create.as_str(), &create_program),
        (&mut insert_db, insert.as_str(), &insert_program),
    ] {
        let before_vertices = db.vertices().unwrap();
        let before_edges = db.edges().unwrap();
        if transactional {
            let mut tx = db.begin(&txcx).unwrap();
            let receipt = tx
                .execute_graph_write_program_returning_engine_governed(db, &cx, bound, policy())
                .unwrap();
            let staged_vertices = tx.vertices(db).unwrap();
            let staged_edges = tx.edges(db).unwrap();
            assert_eq!(
                staged_vertices.len(),
                before_vertices.len() + case.vertices,
                "{text}"
            );
            assert_eq!(
                staged_edges.len(),
                before_edges.len() + case.edges,
                "{text}"
            );
            assert_eq!(
                db.vertices().unwrap(),
                before_vertices,
                "staging leaked: {text}"
            );
            assert_eq!(db.edges().unwrap(), before_edges, "staging leaked: {text}");
            let completion = tx.finish(db, &commit).await.unwrap();
            receipts.push(QueryResult::Write {
                receipt,
                completion: Some(completion),
            });
        } else {
            let (mut vertices, mut edges) = reserve(db, &cx, case.vertices, case.edges);
            let result = db
                .query_write(
                    &txcx,
                    &cx,
                    &commit,
                    text,
                    &case.params,
                    symbols,
                    R,
                    policy(),
                    |request| {
                        match request.request {
                            GraphInsertRequest::Vertex { .. } => vertices.pop_front(),
                            GraphInsertRequest::Edge { .. } => edges.pop_front(),
                        }
                        .ok_or("query requested an unreserved identity")
                    },
                )
                .await
                .unwrap();
            assert!(
                vertices.is_empty() && edges.is_empty(),
                "unused reservations: {text}"
            );
            receipts.push(result);
        }
        assert_eq!(
            db.vertices().unwrap().len(),
            before_vertices.len() + case.vertices,
            "{text}"
        );
        assert_eq!(
            db.edges().unwrap().len(),
            before_edges.len() + case.edges,
            "{text}"
        );
    }
    assert_eq!(receipts[0], receipts[1], "{}", case.create);
    // These rows include identity, labels, relation/endpoints, all properties,
    // birth ordinals and lifetimes, not a projection or graph-size surrogate.
    assert_eq!(
        create_db.vertices().unwrap(),
        insert_db.vertices().unwrap(),
        "{}",
        case.create
    );
    assert_eq!(
        create_db.edges().unwrap(),
        insert_db.edges().unwrap(),
        "{}",
        case.create
    );
    assert_eq!(create_db.frontier().unwrap(), insert_db.frontier().unwrap());
    assert_eq!(txcx.outstanding_obligations(), 0);
}

#[test]
fn generated_insert_matches_create_programs_receipts_and_complete_database_state() {
    for seed in SEEDS {
        let ((), report) = run_async_under_lab(seed, move |root| {
            let seed = seed;
            async move {
                let contexts = PurposeContexts::narrow_runtime_root(&root);
                for case in corpus(seed) {
                    let insert = case.create.replace("CREATE", "INSERT");
                    let create = PreparedGraphInsertText::prepare(&case.create, R, symbols)
                        .unwrap()
                        .bind_parameters(&case.params)
                        .unwrap();
                    let insert = PreparedGraphInsertText::prepare(&insert, R, symbols)
                        .unwrap()
                        .bind_parameters(&case.params)
                        .unwrap();
                    assert_eq!(
                        create.canonical_bytes(),
                        insert.canonical_bytes(),
                        "{}",
                        case.create
                    );
                    for transactional in [false, true] {
                        equivalent(&contexts, &case, transactional).await;
                    }
                }
            }
        });
        assert!(report.lab_test_passed(), "seed {seed}: {report:?}");
    }
}

#[test]
fn insert_scripts_preserve_dependent_overlay_reads_and_one_completion() {
    for seed in SEEDS {
        let ((), report) = run_async_under_lab(seed, move |root| {
            let seed = seed;
            async move {
                let contexts = PurposeContexts::narrow_runtime_root(&root);
                let case = Case {
                    create: "CREATE (a:Copy {p:$value})-[:R]->(b:Person {q:$value}); \
                    MATCH (a:Copy) CREATE (c:Person {p:a.p+1}),(a)-[:R {q:a.p}]->(c); \
                    MATCH (n:Person) SET n.q=$value+2;"
                        .to_owned(),
                    params: GqlParameters::new()
                        .with_int64("value", (seed % 1_000) as i64)
                        .unwrap(),
                    vertices: 3,
                    edges: 2,
                };
                for transactional in [false, true] {
                    equivalent(&contexts, &case, transactional).await;
                }
            }
        });
        assert!(report.lab_test_passed(), "seed {seed}: {report:?}");
    }
}

/// fgdb-sl4ym (TCK Create3 [5], [6]): `MATCH ... WITH ... CREATE` creates
/// once per WITH row. A vertex the row carries is a bound endpoint, an
/// aggregate or filtered row feeds properties, RETURN reads the row, and a
/// name the WITH dropped declares a new vertex, as openCypher scopes it.
#[test]
fn create_after_with_reads_the_with_row() {
    let ((), report) = run_async_under_lab(0x1a5e_a17b, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = fresh(&commit, true).await;
        let mut next = 100u128;
        let mut write = async |db: &mut Database<MemVfs>, text: &str| {
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| {
                    next += 1;
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                    })
                },
            )
            .await
            .map_err(|error| format!("{text}: {error}"))
        };
        let returned = write(
            &mut db,
            "MATCH (n:Source) WITH n AS a CREATE (a)-[:R]->(:Copy {p: 1}) RETURN a",
        )
        .await
        .unwrap();
        let QueryResult::Rows { columns, rows } = returned else {
            unreachable!("CREATE ... RETURN answers rows")
        };
        assert_eq!(columns, ["a"]);
        assert_eq!(rows.len(), 2);
        let copies = |db: &Database<MemVfs>| {
            db.vertices()
                .unwrap()
                .into_iter()
                .filter(|vertex| vertex.labels == vec![COPY])
                .count()
        };
        assert_eq!(copies(&db), 2);
        let from_sources = |db: &Database<MemVfs>| {
            db.edges()
                .unwrap()
                .into_iter()
                .filter(|edge| [VId(10), VId(20)].contains(&edge.entry.src))
                .count()
        };
        // The seeded 10 -> 20 edge, plus one from each source.
        assert_eq!(from_sources(&db), 3);
        write(
            &mut db,
            "MATCH (n:Source) WITH count(n) AS c CREATE (:Copy {p: c})",
        )
        .await
        .unwrap();
        let counted = db
            .vertices()
            .unwrap()
            .into_iter()
            .filter(|vertex| {
                vertex.labels == vec![COPY]
                    && vertex.props == vec![(P, CanonicalScalar::Int(2).into())]
            })
            .count();
        assert_eq!(counted, 1);
        write(
            &mut db,
            "MATCH (n:Source) WITH n, n.p AS p WHERE p > 15 \
             CREATE (n)-[:R {since: p}]->(:Person)",
        )
        .await
        .unwrap();
        let since = db
            .edges()
            .unwrap()
            .into_iter()
            .filter(|edge| edge.props == vec![(SINCE, CanonicalScalar::Int(20).into())])
            .map(|edge| edge.entry.src)
            .collect::<Vec<_>>();
        assert_eq!(since, [VId(20)]);
        // A carried vertex's property feeds a created one and RETURN reads it.
        let carried = write(
            &mut db,
            "MATCH (n:Source) WITH n AS a CREATE (:Person {p: a.p + 1}) RETURN a.p AS p",
        )
        .await
        .unwrap();
        let QueryResult::Rows { rows, .. } = carried else {
            unreachable!("CREATE ... RETURN answers rows")
        };
        assert_eq!(rows.len(), 2);
        let written = db
            .vertices()
            .unwrap()
            .into_iter()
            .filter(|vertex| vertex.labels == vec![PERSON])
            .flat_map(|vertex| vertex.props)
            .filter(|(key, _)| *key == P)
            .map(|(_, value)| value)
            .collect::<Vec<_>>();
        assert_eq!(written.len(), 2);
        for value in [11, 21] {
            assert!(
                written.contains(&CanonicalScalar::Int(value).into()),
                "{written:?}"
            );
        }
        // RETURN * is the WITH row and the named creations, never a dropped
        // MATCH binding.
        let starred = write(
            &mut db,
            "MATCH (n:Source) WITH n AS a LIMIT 1 CREATE (b:Copy) RETURN *",
        )
        .await
        .unwrap();
        let QueryResult::Rows { columns, rows } = starred else {
            unreachable!("CREATE ... RETURN * answers rows")
        };
        assert_eq!(columns, ["a", "b"]);
        assert_eq!(rows.len(), 1);
        // `m` left scope at the WITH: CREATE declares a new vertex m.
        let vertices = db.vertices().unwrap().len();
        write(
            &mut db,
            "MATCH (n:Source), (m:Source) WITH n AS a LIMIT 1 CREATE (m)-[:R]->(a)",
        )
        .await
        .unwrap();
        assert_eq!(db.vertices().unwrap().len(), vertices + 1);
        // A dynamically typed binding is an endpoint when its row holds a
        // vertex there (TCK Match4 [4]): each row is checked when it runs.
        let before = db.edges().unwrap().len();
        write(
            &mut db,
            "MATCH (n:Source) WITH collect(n) AS ns WITH ns[0] AS s, ns[1] AS t \
             CREATE (s)-[:R]->(t)",
        )
        .await
        .unwrap();
        let edges = db.edges().unwrap();
        assert_eq!(edges.len(), before + 1);
        assert_eq!(
            edges
                .iter()
                .filter(|edge| {
                    let ends = [edge.entry.src, edge.entry.dst];
                    ends.contains(&VId(10)) && ends.contains(&VId(20))
                })
                .count(),
            2,
            "the seeded edge and the new one join the two sources"
        );
        let frontier = db.frontier().unwrap();
        for refused in [
            "MATCH (n:Source) WITH n.p AS p CREATE (p)-[:R]->(:Copy)",
            "MATCH (n:Source) WITH n AS a CREATE (a:Person)",
            "MATCH (n:Source) WITH n AS a CREATE (:Copy {p: n.p})",
            "MATCH (n:Source) WITH n AS a CREATE (:Copy) RETURN n",
            // An element that is not a vertex, or NULL, is no endpoint.
            "MATCH (n:Source) WITH n, [n.p] AS ps WITH n, ps[0] AS s CREATE (n)-[:R]->(s)",
            "MATCH (n:Source) WITH n, [NULL] AS ps WITH n, ps[0] AS s CREATE (n)-[:R]->(s)",
        ] {
            assert!(write(&mut db, refused).await.is_err(), "{refused}");
        }
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-cpny9 (TCK Create3 [2], [7]-[10]): in one statement, a CREATE after
/// a CREATE's WITH runs once per row that WITH returned and sees the earlier
/// write. All writes publish together or not at all; each step binds exactly
/// the parameters it names.
#[test]
fn chained_creates_read_the_previous_writes_rows() {
    let ((), report) = run_async_under_lab(0x1a5e_c4a1, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = fresh(&commit, true).await;
        let mut next = 100u128;
        let mut write = async |db: &mut Database<MemVfs>, text: &str, params: GqlParameters| {
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &params,
                symbols,
                R,
                policy(),
                |request| {
                    next += 1;
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                    })
                },
            )
            .await
            .map_err(|error| format!("{text}: {error}"))
        };
        let labelled = |db: &Database<MemVfs>, label: LabelId| {
            db.vertices()
                .unwrap()
                .into_iter()
                .filter(|vertex| vertex.labels == vec![label])
                .collect::<Vec<_>>()
        };
        // Two WITH rows: each runs both CREATEs, and the second reads the
        // copy the first made for that row.
        let edges = db.edges().unwrap().len();
        let returned = write(
            &mut db,
            "MATCH (n:Source) WITH n AS a CREATE (a)-[:R]->(b:Copy) \
             WITH a AS x, b AS y CREATE (x)-[:R]->(y) RETURN x, y",
            GqlParameters::new(),
        )
        .await
        .unwrap();
        let QueryResult::Rows { columns, rows } = returned else {
            unreachable!("a chain with RETURN answers rows")
        };
        assert_eq!(columns, ["x", "y"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(labelled(&db, COPY).len(), 2);
        assert_eq!(db.edges().unwrap().len(), edges + 4);
        // Every copy now has two edges from its own source.
        for copy in labelled(&db, COPY) {
            let into = db
                .edges()
                .unwrap()
                .into_iter()
                .filter(|edge| edge.entry.dst == copy.vid)
                .map(|edge| edge.entry.src)
                .collect::<Vec<_>>();
            assert_eq!(into.len(), 2, "{into:?}");
            assert_eq!(into[0], into[1]);
            assert!([VId(10), VId(20)].contains(&into[0]));
        }
        // A carried vertex's property, as the earlier CREATE wrote it, feeds
        // the next one; without RETURN the chain answers a write receipt.
        let person = write(
            &mut db,
            "CREATE (a:Person {p: 7}) WITH a CREATE (b:Person {p: a.p + 1}) \
             CREATE (a)<-[:R]-(b)",
            GqlParameters::new(),
        )
        .await
        .unwrap();
        let QueryResult::Write { receipt, .. } = person else {
            unreachable!("a chain without RETURN answers a write receipt")
        };
        assert_eq!(receipt.stats().created_vertices, 2);
        assert_eq!(receipt.stats().created_edges, 1);
        assert_eq!(receipt.steps().len(), 2);
        let written = labelled(&db, PERSON)
            .into_iter()
            .flat_map(|vertex| vertex.props)
            .map(|(_, value)| value)
            .collect::<Vec<_>>();
        assert_eq!(written.len(), 2, "{written:?}");
        for value in [7, 8] {
            assert!(
                written.contains(&CanonicalScalar::Int(value).into()),
                "{written:?}"
            );
        }
        // UNWIND between the writes multiplies the rows the next one reads.
        let counted = write(
            &mut db,
            "CREATE (a:Person) WITH a UNWIND [1, 2, 3] AS i \
             CREATE (:Copy {p: i})-[:R]->(a) RETURN count(*) AS c",
            GqlParameters::new(),
        )
        .await
        .unwrap();
        let QueryResult::Rows { rows, .. } = counted else {
            unreachable!("a chain with RETURN answers rows")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(labelled(&db, COPY).len(), 5);
        // Each step binds exactly the parameters it names.
        let params = GqlParameters::new()
            .with_int64("first", 40)
            .unwrap()
            .with_int64("second", 41)
            .unwrap();
        write(
            &mut db,
            "CREATE (a:Person {p: $first}) WITH a CREATE (:Copy {p: $second})",
            params.clone(),
        )
        .await
        .unwrap();
        let frontier = db.frontier().unwrap();
        let extra = params.clone().with_int64("unused", 0).unwrap();
        assert!(
            write(
                &mut db,
                "CREATE (a:Person {p: $first}) WITH a CREATE (:Copy {p: $second})",
                extra,
            )
            .await
            .is_err()
        );
        // A failing later step publishes none of the earlier ones.
        assert!(
            write(
                &mut db,
                "CREATE (a:Person {p: 0}) WITH a CREATE (:Copy {p: 1 / a.p})",
                GqlParameters::new(),
            )
            .await
            .is_err()
        );
        assert_eq!(db.frontier().unwrap(), frontier);
        // TCK Create3 [3]: the second MATCH () is its own pattern, not the
        // first's anonymous vertex. Each of the v first rows creates one
        // vertex, then matches all 2v vertices and creates one per match.
        let v = db.vertices().unwrap().len();
        write(
            &mut db,
            "MATCH () CREATE () WITH * MATCH () CREATE ()",
            GqlParameters::new(),
        )
        .await
        .unwrap();
        assert_eq!(db.vertices().unwrap().len(), v + v + 2 * v * v);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-cpny9 (TCK Merge5-shaped `CREATE (a), (b) MERGE (a)-[:R]->(b)`): a
/// relationship MERGE after a CREATE, with or without WITH between, resolves
/// every row the CREATE produced against the graph that CREATE left. A
/// missing relationship is created once per distinct endpoint pair, an
/// existing one (even one the same statement created) is matched, and a
/// failure anywhere publishes nothing.
#[test]
fn a_relationship_merge_after_create_resolves_the_created_rows() {
    let ((), report) = run_async_under_lab(0x1a5e_c4a2, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = fresh(&commit, false).await;
        let mut next = 100u128;
        let mut write = async |db: &mut Database<MemVfs>, text: &str| {
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| {
                    next += 1;
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                    })
                },
            )
            .await
            .map_err(|error| format!("{text}: {error}"))
        };
        // The source of every edge between a Person and a Copy, by the
        // Person's `p`.
        let sources = |db: &Database<MemVfs>| {
            let vertices = db.vertices().unwrap();
            db.edges()
                .unwrap()
                .into_iter()
                .map(|edge| {
                    let source = vertices
                        .iter()
                        .find(|vertex| vertex.vid == edge.entry.src)
                        .expect("an edge's source is stored");
                    (source.labels.clone(), source.props.clone())
                })
                .collect::<Vec<_>>()
        };
        // RETURN reads the relationship the MERGE created.
        let returned = write(
            &mut db,
            "CREATE (a:Person), (b:Copy) MERGE (a)-[r:R]->(b) RETURN count(r) AS c",
        )
        .await
        .unwrap();
        let QueryResult::Rows { columns, rows } = returned else {
            unreachable!("a chain with RETURN answers rows")
        };
        assert_eq!(columns, ["c"]);
        // A write RETURN's count is an Int value, as main's edge upsert
        // RETURN laws pin it.
        let int = |value| {
            QueryValue::Value(fgdb_gql::algebra::GraphValue::Scalar(CanonicalScalar::Int(
                value,
            )))
        };
        assert_eq!(rows, [vec![int(1)]]);
        assert_eq!(db.edges().unwrap().len(), 1);
        // Without RETURN, after a WITH and written right to left: a receipt
        // with one step per write, and the edge runs from `a`.
        let written = write(
            &mut db,
            "CREATE (a:Person {p: 1}), (b:Copy {p: 2}) WITH a, b MERGE (b)<-[:R]-(a)",
        )
        .await
        .unwrap();
        let QueryResult::Write { receipt, .. } = written else {
            unreachable!("a chain without RETURN answers a write receipt")
        };
        assert_eq!(receipt.stats().created_vertices, 2);
        assert_eq!(receipt.stats().created_edges, 1);
        assert_eq!(receipt.steps().len(), 2);
        // An undirected MERGE creates its edge left to right.
        write(
            &mut db,
            "CREATE (a:Person {p: 3}), (b:Copy {p: 4}) MERGE (a)-[:R]-(b)",
        )
        .await
        .unwrap();
        let from = sources(&db);
        assert_eq!(from.len(), 3);
        for p in [1, 3] {
            assert!(
                from.contains(&(vec![PERSON], vec![(P, CanonicalScalar::Int(p).into())])),
                "{from:?}"
            );
        }
        // Three rows of one pair: one creation, and every row resolves to it.
        let counted = write(
            &mut db,
            "CREATE (a:Person), (b:Copy) WITH a, b UNWIND [1, 2, 3] AS i \
             MERGE (a)-[r:R]->(b) RETURN count(*) AS c",
        )
        .await
        .unwrap();
        let QueryResult::Rows { rows, .. } = counted else {
            unreachable!("a chain with RETURN answers rows")
        };
        assert_eq!(rows, [vec![int(3)]]);
        assert_eq!(db.edges().unwrap().len(), 4);
        // The MERGE sees the CREATE's own relationship and matches it.
        write(
            &mut db,
            "CREATE (a:Person)-[:R]->(b:Copy) MERGE (a)-[:R]->(b)",
        )
        .await
        .unwrap();
        assert_eq!(db.edges().unwrap().len(), 5);
        // A refusal after the MERGE staged publishes neither write.
        let frontier = db.frontier().unwrap();
        let vertices = db.vertices().unwrap().len();
        assert!(
            write(
                &mut db,
                "CREATE (a:Person {p: 0}), (b:Copy) MERGE (a)-[:R]->(b) RETURN 1 / a.p AS bad",
            )
            .await
            .is_err()
        );
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(db.vertices().unwrap().len(), vertices);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-cpny9 (TCK Merge5 [9], Create3 [11], [13], Merge1 [7], [12]): a
/// vertex MERGE is a chain step. As the first write it runs once; after a
/// write it runs once per row that write produced, and rows with equal
/// property values share one MERGE. It binds every vertex it selects (one
/// row each) or creates exactly one, and the next write reads the merged
/// vertex beside the row.
#[test]
fn vertex_merge_steps_bind_or_create_per_row() {
    let ((), report) = run_async_under_lab(0x1a5e_c4a3, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = fresh(&commit, false).await;
        let mut next = 100u128;
        let mut write = async |db: &mut Database<MemVfs>, text: &str| {
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| {
                    next += 1;
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                    })
                },
            )
            .await
            .map_err(|error| format!("{text}: {error}"))
        };
        let count = |db: &Database<MemVfs>, label: LabelId| {
            db.vertices()
                .unwrap()
                .into_iter()
                .filter(|vertex| vertex.labels == vec![label])
                .count()
        };
        // Merge5 [9]: two MERGEs then a relationship MERGE between them.
        let first = write(
            &mut db,
            "MERGE (a:Person) MERGE (b:Copy) MERGE (a)-[:R]->(b)",
        )
        .await
        .unwrap();
        let QueryResult::Write { receipt, .. } = first else {
            unreachable!("a chain without RETURN answers a write receipt")
        };
        assert_eq!(receipt.steps().len(), 3);
        assert_eq!(receipt.stats().created_vertices, 2);
        assert_eq!(receipt.stats().created_edges, 1);
        // Run again: everything matches and nothing is created.
        let again = write(
            &mut db,
            "MERGE (a:Person) MERGE (b:Copy) MERGE (a)-[:R]->(b)",
        )
        .await
        .unwrap();
        let QueryResult::Write { receipt, .. } = again else {
            unreachable!("a chain without RETURN answers a write receipt")
        };
        assert_eq!(receipt.stats().created_vertices, 0);
        assert_eq!(receipt.stats().created_edges, 0);
        assert_eq!((count(&db, PERSON), count(&db, COPY)), (1, 1));
        assert_eq!(db.edges().unwrap().len(), 1);
        // Create3 [13]: a merged vertex is an endpoint of a later CREATE.
        write(
            &mut db,
            "MERGE (t:Source {p: 42}) CREATE (f:Copy) CREATE (t)-[:R]->(f)",
        )
        .await
        .unwrap();
        write(
            &mut db,
            "MERGE (t:Source {p: 42}) CREATE (f:Copy) CREATE (t)-[:R]->(f)",
        )
        .await
        .unwrap();
        assert_eq!(count(&db, SOURCE), 1);
        assert_eq!(count(&db, COPY), 3);
        assert_eq!(db.edges().unwrap().len(), 3);
        let source = db
            .vertices()
            .unwrap()
            .into_iter()
            .find(|vertex| vertex.labels == vec![SOURCE])
            .expect("the merged Source")
            .vid;
        let from_source = db
            .edges()
            .unwrap()
            .into_iter()
            .filter(|edge| edge.entry.src == source)
            .count();
        assert_eq!(from_source, 2);
        // Merge1 [7]: a MERGE after two CREATEs binds both, creating none.
        let bound = write(&mut db, "CREATE (:Person) CREATE (:Person) MERGE (:Person)")
            .await
            .unwrap();
        let QueryResult::Write { receipt, .. } = bound else {
            unreachable!("a chain without RETURN answers a write receipt")
        };
        assert_eq!(receipt.stats().created_vertices, 2);
        assert_eq!(count(&db, PERSON), 3);
        // A row value and a carried element's property feed the MERGE map
        // (Merge1 [12]); equal values share one vertex.
        write(
            &mut db,
            "CREATE (a:Person {p: 7}) WITH a, a.p AS q UNWIND [1, 2] AS i \
             MERGE (b:Copy {p: q}) CREATE (a)-[:R]->(b)",
        )
        .await
        .unwrap();
        let sevens = db
            .vertices()
            .unwrap()
            .into_iter()
            .filter(|vertex| {
                vertex.labels == vec![COPY]
                    && vertex.props == vec![(P, CanonicalScalar::Int(7).into())]
            })
            .count();
        assert_eq!(sevens, 1);
        write(&mut db, "CREATE (a:Person {p: 8}) MERGE (:Copy {p: a.p})")
            .await
            .unwrap();
        let eights = db
            .vertices()
            .unwrap()
            .into_iter()
            .filter(|vertex| vertex.props == vec![(P, CanonicalScalar::Int(8).into())])
            .count();
        assert_eq!(eights, 2);
        // An ON/SET target other than the MERGE vertex, a NULL map value and
        // a MERGE of a row vertex refuse and publish nothing. (RETURN after
        // chained MERGEs is vertex_merge_steps_run_over_a_leading_unwind_and_return_rows;
        // ON/SET on them is vertex_merge_steps_apply_on_and_set_clauses_per_row.)
        let frontier = db.frontier().unwrap();
        for refused in [
            "CREATE (a) MERGE (b:Copy) ON CREATE SET a.p = 1",
            "CREATE (a:Person) MERGE (b:Copy {p: a.q}) CREATE (a)-[:R]->(b)",
            "CREATE (a:Person) WITH a MERGE (a) CREATE (b)",
        ] {
            assert!(write(&mut db, refused).await.is_err(), "{refused}");
        }
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-cpny9 (TCK Merge9 [1], [2]): a vertex MERGE runs over a leading
/// UNWIND of a non-parameter list, alone or heading a chain, and the last
/// MERGE of a statement may RETURN. RETURN sees one row per input row and
/// vertex it merged: the row's columns, the vertex and its properties.
#[test]
fn vertex_merge_steps_run_over_a_leading_unwind_and_return_rows() {
    let ((), report) = run_async_under_lab(0x1a5e_c4a4, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = fresh(&commit, false).await;
        let mut next = 100u128;
        let mut write = async |db: &mut Database<MemVfs>, text: &str| {
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| {
                    next += 1;
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                    })
                },
            )
            .await
            .map_err(|error| format!("{text}: {error}"))
        };
        let rows = |result: QueryResult| {
            let QueryResult::Rows { rows, .. } = result else {
                unreachable!("a MERGE with RETURN answers rows")
            };
            rows
        };
        let int = |value| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)));
        let count = |db: &Database<MemVfs>, label: LabelId| {
            db.vertices()
                .unwrap()
                .into_iter()
                .filter(|vertex| vertex.labels == vec![label])
                .count()
        };
        // Merge9 [1]: four values, four vertices; a second run matches them.
        for _ in 0..2 {
            let counted = write(
                &mut db,
                "UNWIND [1, 2, 3, 4] AS int MERGE (n:Person {p: int}) RETURN count(*) AS c",
            )
            .await
            .unwrap();
            assert_eq!(rows(counted), [vec![int(4)]]);
            assert_eq!(count(&db, PERSON), 4);
        }
        // Equal values share one vertex; RETURN reads the row and the vertex.
        let shared = write(
            &mut db,
            "UNWIND [5, 5, 6] AS v MERGE (n:Copy {p: v}) RETURN v, n.p AS p ORDER BY v",
        )
        .await
        .unwrap();
        assert_eq!(
            rows(shared),
            [
                vec![int(5), int(5)],
                vec![int(5), int(5)],
                vec![int(6), int(6)]
            ]
        );
        assert_eq!(count(&db, COPY), 2);
        // Merge9 [2]: one shared MERGE, one per value, and a relationship each.
        write(
            &mut db,
            "UNWIND ['a', 'b', 'c'] AS who MERGE (m:Source {name: 'M'}) \
             MERGE (p:Person {name: who}) MERGE (p)-[:R]->(m)",
        )
        .await
        .unwrap();
        assert_eq!(count(&db, SOURCE), 1);
        assert_eq!(count(&db, PERSON), 7);
        assert_eq!(db.edges().unwrap().len(), 3);
        // Source-free WITH stages before the MERGE (Merge1 [8], Merge9 [4]):
        // a WITH alone, and after an UNWIND with WHERE and DISTINCT.
        write(&mut db, "WITH 42 AS var MERGE (c:Copy {p: var})")
            .await
            .unwrap();
        assert_eq!(count(&db, COPY), 3);
        let filtered = write(
            &mut db,
            "UNWIND [42, 42, 7] AS props WITH props WHERE props > 32 \
             WITH DISTINCT props AS q MERGE (a:Copy {p: q}) RETURN a.p AS prop",
        )
        .await
        .unwrap();
        assert_eq!(rows(filtered), [vec![int(42)]]);
        assert_eq!(count(&db, COPY), 3);
        // RETURN after chained MERGEs and over a carried row column.
        let pair = write(
            &mut db,
            "MERGE (a:Copy {p: 9}) MERGE (b:Copy {p: 10}) RETURN a.p AS x, b.p AS y",
        )
        .await
        .unwrap();
        assert_eq!(rows(pair), [vec![int(9), int(10)]]);
        let carried = write(
            &mut db,
            "CREATE (a:Person {p: 11}) WITH a, a.p AS q MERGE (b:Copy {p: q}) RETURN q, b.p AS p",
        )
        .await
        .unwrap();
        assert_eq!(rows(carried), [vec![int(11), int(11)]]);
        // RETURN * returns the row's binding and the merged vertex, ordered
        // by name (n, then x).
        let starred = write(&mut db, "UNWIND [1] AS x MERGE (n:Person {p: x}) RETURN *")
            .await
            .unwrap();
        let QueryResult::Rows { columns, rows } = starred else {
            unreachable!("a MERGE with RETURN answers rows")
        };
        assert_eq!(columns, ["n", "x"]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1], int(1));
        // A NULL UNWIND value refuses and publishes nothing.
        let frontier = db.frontier().unwrap();
        assert!(
            write(&mut db, "UNWIND [null] AS x MERGE (n:Person {p: x})")
                .await
                .is_err()
        );
        assert_eq!(db.frontier().unwrap(), frontier);
        // A List key merges as a stored List (fgdb-fc7cx): a second MERGE
        // binds the vertex the first created.
        let list = CanonicalPropertyValue::List(
            fgdb_types::CanonicalList::try_new(vec![CanonicalScalar::Int(1).into()]).unwrap(),
        );
        for _ in 0..2 {
            write(&mut db, "UNWIND [[1]] AS x MERGE (n:Person {p: x})")
                .await
                .unwrap();
        }
        assert_eq!(
            db.vertices()
                .unwrap()
                .iter()
                .filter(|vertex| vertex.props == vec![(P, list.clone())])
                .count(),
            1
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-cpny9 (TCK Merge1 [11]): a vertex MERGE runs over a leading MATCH,
/// alone or heading a chain. Directly after the MATCH its row is every MATCH
/// binding, as `WITH *` carries them; after a WITH (or UNWIND) it is that
/// stage's row. Each distinct value tuple merges once, and RETURN reads the
/// row and the vertex.
#[test]
fn vertex_merge_steps_run_over_a_leading_match() {
    let ((), report) = run_async_under_lab(0x1a5e_c4a5, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = fresh(&commit, false).await;
        let mut next = 100u128;
        let mut write = async |db: &mut Database<MemVfs>, text: &str| {
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| {
                    next += 1;
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                    })
                },
            )
            .await
            .map_err(|error| format!("{text}: {error}"))
        };
        let rows = |result: QueryResult| {
            let QueryResult::Rows { rows, .. } = result else {
                unreachable!("a MERGE with RETURN answers rows")
            };
            rows
        };
        let int = |value| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)));
        let count = |db: &Database<MemVfs>, label: LabelId| {
            db.vertices()
                .unwrap()
                .into_iter()
                .filter(|vertex| vertex.labels == vec![label])
                .count()
        };
        // Six people born in three places.
        write(
            &mut db,
            "CREATE (:Person {born: 1}), (:Person {born: 2}), (:Person {born: 3}), \
             (:Person {born: 1}), (:Person {born: 2}), (:Person {born: 3})",
        )
        .await
        .unwrap();
        // Merge1 [11]: three distinct values, three vertices; a rerun
        // matches them.
        for _ in 0..2 {
            write(
                &mut db,
                "MATCH (person:Person) MERGE (city:Copy {p: person.born})",
            )
            .await
            .unwrap();
            assert_eq!(count(&db, COPY), 3);
            assert_eq!(count(&db, PERSON), 6);
        }
        // RETURN groups over the MATCH row and the merged vertex.
        let grouped = write(
            &mut db,
            "MATCH (person:Person) MERGE (city:Copy {p: person.born}) \
             RETURN city.p AS p, count(*) AS n ORDER BY p",
        )
        .await
        .unwrap();
        assert_eq!(
            rows(grouped),
            [
                vec![int(1), int(2)],
                vec![int(2), int(2)],
                vec![int(3), int(2)]
            ]
        );
        assert_eq!(count(&db, COPY), 3);
        // After a WITH, the WITH row is the scope.
        let projected = write(
            &mut db,
            "MATCH (person:Person) WITH person.born AS b MERGE (s:Source {p: b}) \
             RETURN b, s.p AS p ORDER BY b",
        )
        .await
        .unwrap();
        assert_eq!(
            rows(projected),
            [
                vec![int(1), int(1)],
                vec![int(1), int(1)],
                vec![int(2), int(2)],
                vec![int(2), int(2)],
                vec![int(3), int(3)],
                vec![int(3), int(3)]
            ]
        );
        assert_eq!(count(&db, SOURCE), 3);
        // An UNWIND after the MATCH: twelve rows, two distinct values.
        write(
            &mut db,
            "MATCH (person:Person) UNWIND [10, 11] AS i MERGE (:Source {q: i})",
        )
        .await
        .unwrap();
        assert_eq!(count(&db, SOURCE), 5);
        // Heading a chain: the next write reads each MATCH row and its vertex.
        write(
            &mut db,
            "MATCH (person:Person) MERGE (city:Copy {p: person.born}) \
             CREATE (person)-[:R]->(city)",
        )
        .await
        .unwrap();
        assert_eq!(count(&db, COPY), 3);
        let vertices = db.vertices().unwrap();
        let born = |vid: VId, key: PropertyKeyId| {
            vertices
                .iter()
                .find(|vertex| vertex.vid == vid)
                .and_then(|vertex| vertex.props.iter().find(|(k, _)| *k == key))
                .map(|(_, value)| value.clone())
        };
        let edges = db.edges().unwrap();
        assert_eq!(edges.len(), 6);
        for edge in &edges {
            assert!(born(edge.entry.src, BORN).is_some());
            assert_eq!(born(edge.entry.src, BORN), born(edge.entry.dst, P));
        }
        // A MERGE of a MATCH vertex, a NULL map value and a SET of a MATCH
        // vertex after the MERGE refuse and publish nothing.
        let frontier = db.frontier().unwrap();
        for refused in [
            "MATCH (person:Person) MERGE (person)",
            "MATCH (person:Person) MERGE (c:Copy {p: person.q})",
            "MATCH (person:Person) MERGE (c:Copy {p: 1}) ON CREATE SET person.q = 1",
        ] {
            assert!(write(&mut db, refused).await.is_err(), "{refused}");
        }
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-cpny9 (TCK Merge4 [1], [2]): ON MATCH SET, ON CREATE SET and SET on
/// a MERGE step run per row. Each row runs its own MERGE against the state
/// earlier rows left, so a vertex row 1 created is MATCHED by row 2 and gets
/// the ON MATCH clause. Right-hand sides read the vertex's properties before
/// the clause and the row.
#[test]
fn vertex_merge_steps_apply_on_and_set_clauses_per_row() {
    let ((), report) = run_async_under_lab(0x1a5e_c4a6, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = fresh(&commit, false).await;
        let mut next = 100u128;
        let mut write = async |db: &mut Database<MemVfs>, text: &str| {
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| {
                    next += 1;
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                    })
                },
            )
            .await
            .map_err(|error| format!("{text}: {error}"))
        };
        let rows = |result: QueryResult| {
            let QueryResult::Rows { rows, .. } = result else {
                unreachable!("a MERGE with RETURN answers rows")
            };
            rows
        };
        let int = |value| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)));
        let stored = |value| CanonicalPropertyValue::from(CanonicalScalar::Int(value));
        // The one vertex whose `p` is `value`: its labels and properties.
        let merged = |db: &Database<MemVfs>, value: i64| {
            let matching: Vec<_> = db
                .vertices()
                .unwrap()
                .into_iter()
                .filter(|vertex| {
                    vertex
                        .props
                        .iter()
                        .any(|(k, v)| *k == P && *v == stored(value))
                })
                .collect();
            assert_eq!(matching.len(), 1, "one vertex with p = {value}");
            let vertex = matching.into_iter().next().expect("one vertex");
            (vertex.labels, vertex.props)
        };
        write(
            &mut db,
            "CREATE (:Person {born: 1}), (:Person {born: 2}), (:Person {born: 5}), \
             (:Person {born: 5})",
        )
        .await
        .unwrap();
        // Merge4 [1]: row 1 creates (ON CREATE), row 2 matches (ON MATCH).
        write(
            &mut db,
            "MATCH (person:Person {born: 5}) MERGE (a:Copy {p: 2}) \
             ON MATCH SET a:Source ON CREATE SET a.q = 7",
        )
        .await
        .unwrap();
        assert_eq!(
            merged(&db, 2),
            (vec![SOURCE, COPY], vec![(P, stored(2)), (Q, stored(7))])
        );
        // Merge4 [2]: both clauses read the row; RETURN reads it too.
        let returned = write(
            &mut db,
            "MATCH (person:Person) WHERE person.born < 3 MERGE (c:Copy {p: 3}) \
             ON MATCH SET c.name = person.born ON CREATE SET c.name = person.born \
             RETURN person.born AS b ORDER BY b",
        )
        .await
        .unwrap();
        assert_eq!(rows(returned), [vec![int(1)], vec![int(2)]]);
        let (_, props) = merged(&db, 3);
        assert!(
            props == vec![(P, stored(3)), (NAME, stored(1))]
                || props == vec![(P, stored(3)), (NAME, stored(2))],
            "{props:?}"
        );
        // An expression over the vertex's own property and the row, then a
        // trailing SET every row runs.
        write(
            &mut db,
            "MATCH (person:Person {born: 5}) MERGE (c:Copy {p: 4}) \
             ON CREATE SET c.q = 0 ON MATCH SET c.q = c.q + person.born SET c.name = 1",
        )
        .await
        .unwrap();
        assert_eq!(
            merged(&db, 4),
            (
                vec![COPY],
                vec![(P, stored(4)), (Q, stored(5)), (NAME, stored(1))]
            )
        );
        // Over a leading UNWIND: equal values match the earlier row's vertex.
        write(
            &mut db,
            "UNWIND [11, 11, 12] AS x MERGE (n:Source {p: x}) \
             ON CREATE SET n.q = 0 SET n.q = n.q + 1",
        )
        .await
        .unwrap();
        assert_eq!(merged(&db, 11).1, vec![(P, stored(11)), (Q, stored(2))]);
        assert_eq!(merged(&db, 12).1, vec![(P, stored(12)), (Q, stored(1))]);
        // A first MERGE with ON CREATE heads a chain; a rerun matches all.
        for _ in 0..2 {
            write(
                &mut db,
                "MERGE (a:Source {p: 20}) ON CREATE SET a.q = 1 \
                 MERGE (b:Source {p: 21}) ON CREATE SET b.q = 2 MERGE (a)-[:R]->(b)",
            )
            .await
            .unwrap();
            assert_eq!(merged(&db, 20).1, vec![(P, stored(20)), (Q, stored(1))]);
            assert_eq!(merged(&db, 21).1, vec![(P, stored(21)), (Q, stored(2))]);
            assert_eq!(db.edges().unwrap().len(), 1);
        }
        // The MERGE merges the very vertex the row carries: RETURN reads the
        // carried element's property after the SET.
        let same = write(
            &mut db,
            "MATCH (person:Person {born: 1}) MERGE (same:Person {born: 1}) SET same.q = 3 \
             RETURN person.q AS q",
        )
        .await
        .unwrap();
        assert_eq!(rows(same), [vec![int(3)]]);
        // A carried element's labels after the clauses read its final state
        // (fgdb-a1bx3): one row per matched person, the two born 5.
        let labelled = write(
            &mut db,
            "MATCH (person:Person {born: 5}) MERGE (c:Copy {p: 4}) SET c.q = 9 \
             RETURN labels(person) AS l",
        )
        .await
        .unwrap();
        let person = QueryValue::Value(GraphValue::List(
            vec![GraphValue::Scalar(
                CanonicalScalar::ucs_basic_text("Person").unwrap(),
            )]
            .into_boxed_slice(),
        ));
        assert_eq!(rows(labelled), [vec![person.clone()], vec![person]]);
        // An anonymous MERGE with clauses and a SET of a row element refuse
        // and publish nothing.
        let frontier = db.frontier().unwrap();
        for refused in [
            "MATCH (person:Person) MERGE (:Copy {p: 4}) ON CREATE SET person.q = 1",
            "MATCH (person:Person) MERGE (c:Copy {p: 4}) SET person.q = 1",
        ] {
            assert!(write(&mut db, refused).await.is_err(), "{refused}");
        }
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-cpny9 (TCK Unwind1 [14], Merge1 [9]): a MERGE step's map value and a
/// right-hand side that reads the row and not the MERGE vertex are computed
/// per row by the shared value compiler (`m.p`, `y + 1`). Rows with equal
/// computed tuples share one vertex. A NULL or non-scalar computed key, an
/// arithmetic exception and a Map assignment refuse and publish nothing.
#[test]
fn vertex_merge_steps_compute_values_from_the_row() {
    let ((), report) = run_async_under_lab(0x1a5e_c4a7, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = fresh(&commit, false).await;
        let mut next = 100u128;
        let mut write = async |db: &mut Database<MemVfs>, text: &str| {
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| {
                    next += 1;
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                    })
                },
            )
            .await
            .map_err(|error| format!("{text}: {error}"))
        };
        let rows = |result: QueryResult| {
            let QueryResult::Rows { rows, .. } = result else {
                unreachable!("a MERGE with RETURN answers rows")
            };
            rows
        };
        let int = |value| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)));
        let stored = |value| CanonicalPropertyValue::from(CanonicalScalar::Int(value));
        // The one vertex whose `p` is `value`: its labels and properties.
        let merged = |db: &Database<MemVfs>, value: i64| {
            let matching: Vec<_> = db
                .vertices()
                .unwrap()
                .into_iter()
                .filter(|vertex| {
                    vertex
                        .props
                        .iter()
                        .any(|(k, v)| *k == P && *v == stored(value))
                })
                .collect();
            assert_eq!(matching.len(), 1, "one vertex with p = {value}");
            let vertex = matching.into_iter().next().expect("one vertex");
            (vertex.labels, vertex.props)
        };
        // Unwind1 [14]: a map field keys the MERGE, and the SET reads
        // another. Row 2 matches row 1's vertex, so its SET wins.
        let unwound = write(
            &mut db,
            "UNWIND [{p: 30, q: 1}, {p: 30, q: 2}, {p: 31, q: 3}] AS m \
             MERGE (n:Source {p: m.p}) SET n.q = m.q RETURN n.p AS p, n.q AS q",
        )
        .await
        .unwrap();
        assert_eq!(
            rows(unwound),
            [
                vec![int(30), int(2)],
                vec![int(30), int(2)],
                vec![int(31), int(3)],
            ]
        );
        assert_eq!(
            merged(&db, 30),
            (vec![SOURCE], vec![(P, stored(30)), (Q, stored(2))])
        );
        assert_eq!(
            merged(&db, 31),
            (vec![SOURCE], vec![(P, stored(31)), (Q, stored(3))])
        );
        // Merge1 [9]: arithmetic over a WITH row, in a chain's first step and
        // in a later step over the previous write's rows.
        let chained = write(
            &mut db,
            "MATCH (s:Source {p: 31}) WITH s.p AS x, s.q AS y \
             MERGE (:Copy {p: x + 10, q: y + 1}) MERGE (:Copy {p: x + 11, q: y}) RETURN x, y",
        )
        .await
        .unwrap();
        assert_eq!(rows(chained), [vec![int(31), int(3)]]);
        assert_eq!(
            merged(&db, 41),
            (vec![COPY], vec![(P, stored(41)), (Q, stored(4))])
        );
        assert_eq!(
            merged(&db, 42),
            (vec![COPY], vec![(P, stored(42)), (Q, stored(3))])
        );
        // A rerun computes the same tuples and matches both vertices.
        write(
            &mut db,
            "MATCH (s:Source {p: 31}) WITH s.p AS x, s.q AS y \
             MERGE (:Copy {p: x + 10, q: y + 1}) MERGE (:Copy {p: x + 11, q: y}) RETURN x, y",
        )
        .await
        .unwrap();
        assert_eq!(merged(&db, 41).1, vec![(P, stored(41)), (Q, stored(4))]);
        assert_eq!(merged(&db, 42).1, vec![(P, stored(42)), (Q, stored(3))]);
        let frontier = db.frontier().unwrap();
        for refused in [
            "UNWIND [{q: 1}] AS m MERGE (n:Source {p: m.p})",
            "WITH 9223372036854775807 AS y MERGE (n:Source {p: y + 1})",
            "UNWIND [{p: 50}] AS m MERGE (n:Source {p: m.p}) SET n.q = m",
        ] {
            assert!(write(&mut db, refused).await.is_err(), "{refused}");
        }
        assert_eq!(db.frontier().unwrap(), frontier);
        // A List field merges as a stored List (fgdb-fc7cx): the second run
        // binds the vertex the first created.
        let list = CanonicalPropertyValue::List(
            fgdb_types::CanonicalList::try_new(vec![CanonicalScalar::Int(1).into()]).unwrap(),
        );
        for _ in 0..2 {
            write(&mut db, "UNWIND [{p: [1]}] AS m MERGE (n:Source {p: m.p})")
                .await
                .unwrap();
        }
        assert_eq!(
            db.vertices()
                .unwrap()
                .iter()
                .filter(|vertex| vertex.props == vec![(P, list.clone())])
                .count(),
            1
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-cpny9: a MERGE step another write follows may have a WITH head. The
/// next write reads the head's output row: projected items, an aggregate,
/// DISTINCT rows, and the merged vertex's or relationship's properties
/// after its clauses.
#[test]
fn merge_steps_take_a_with_head() {
    let ((), report) = run_async_under_lab(0x1a5e_c4a7, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = fresh(&commit, false).await;
        let mut next = 100u128;
        let mut write = async |db: &mut Database<MemVfs>, text: &str| {
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| {
                    next += 1;
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                    })
                },
            )
            .await
            .map_err(|error| format!("{text}: {error}"))
        };
        let stored = |value| CanonicalPropertyValue::from(CanonicalScalar::Int(value));
        let with_p = |db: &Database<MemVfs>, label: LabelId, value: i64| {
            db.vertices()
                .unwrap()
                .into_iter()
                .filter(|vertex| {
                    vertex.labels == vec![label]
                        && vertex
                            .props
                            .iter()
                            .any(|(k, v)| *k == P && *v == stored(value))
                })
                .count()
        };
        // A projected item and the merged vertex's property after ON CREATE.
        write(
            &mut db,
            "MERGE (a:Person {p: 1}) ON CREATE SET a.q = 7 WITH a, a.q AS q \
             CREATE (a)-[:R]->(:Copy {p: q})",
        )
        .await
        .unwrap();
        assert_eq!(with_p(&db, PERSON, 1), 1);
        assert_eq!(with_p(&db, COPY, 7), 1);
        assert_eq!(db.edges().unwrap().len(), 1);
        // An aggregate head: one row for three merged rows.
        write(
            &mut db,
            "UNWIND [1, 2, 2] AS v MERGE (n:Source {p: v}) WITH count(*) AS c \
             CREATE (:Copy {p: c})",
        )
        .await
        .unwrap();
        assert_eq!(with_p(&db, COPY, 3), 1);
        // DISTINCT: two vertices, two creations.
        write(
            &mut db,
            "UNWIND [1, 1, 2] AS v MERGE (n:Source {p: v}) WITH DISTINCT n \
             CREATE (n)-[:R]->(:Copy {p: 9})",
        )
        .await
        .unwrap();
        assert_eq!(with_p(&db, COPY, 9), 2);
        assert_eq!(db.edges().unwrap().len(), 3);
        // After a relationship MERGE step.
        write(
            &mut db,
            "CREATE (a:Person {p: 50}), (b:Person {p: 51}) MERGE (a)-[r:R]->(b) \
             WITH b CREATE (b)-[:R]->(:Copy {p: 52})",
        )
        .await
        .unwrap();
        assert_eq!(with_p(&db, COPY, 52), 1);
        assert_eq!(db.edges().unwrap().len(), 5);
        // RETURN after a relationship MERGE step reads the carried endpoints'
        // properties after the statement.
        let carried = write(
            &mut db,
            "CREATE (a:Person {p: 70}), (b:Person {p: 71}) MERGE (a)-[:R]->(b) \
             RETURN a.p AS x, b.p AS y",
        )
        .await
        .unwrap();
        let QueryResult::Rows { rows, .. } = carried else {
            unreachable!("a MERGE with RETURN answers rows")
        };
        let int = |value| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)));
        assert_eq!(rows, [vec![int(70), int(71)]]);
        assert_eq!(db.edges().unwrap().len(), 6);
        // WITH * carries every binding to the next write (TCK Match8 [2]).
        let edges = db.edges().unwrap().len();
        write(
            &mut db,
            "MERGE (a:Person {p: 60}) WITH * CREATE (a)-[:R]->(:Copy)",
        )
        .await
        .unwrap();
        assert_eq!(db.edges().unwrap().len(), edges + 1);
        // A WHERE after the head filters the merged rows before the next
        // write (fgdb-vy1rf, 60d60df1): it holds once, then never.
        let vertices = db.vertices().unwrap().len();
        write(
            &mut db,
            "MERGE (a:Person {p: 60}) WITH a WHERE a.p > 1 CREATE (b)",
        )
        .await
        .unwrap();
        assert_eq!(db.vertices().unwrap().len(), vertices + 1);
        write(
            &mut db,
            "MERGE (a:Person {p: 60}) WITH a WHERE a.p > 100 CREATE (b)",
        )
        .await
        .unwrap();
        assert_eq!(db.vertices().unwrap().len(), vertices + 1);
        // A head no write follows refuses and publishes nothing.
        let frontier = db.frontier().unwrap();
        assert!(
            write(&mut db, "MERGE (a:Person {p: 60}) WITH a")
                .await
                .is_err()
        );
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// TCK Merge5 [4], Merge9 [3], Create3 [12]: a chain may write relationships
/// of several types in one statement, though the write names one relation:
/// each step sees its predecessor's writes, whatever their type.
#[test]
fn chains_write_relationships_of_several_types() {
    let ((), report) = run_async_under_lab(0x1a5e_c4a9, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = fresh(&commit, false).await;
        let mut next = 100u128;
        let rows = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                "CREATE (a:Person {p: 1}), (b:Person {p: 2}) MERGE (a)-[:T]->(b) \
                 CREATE (b)-[:R]->(:Copy) RETURN count(*) AS c",
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| {
                    next += 1;
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                    })
                },
            )
            .await
            .unwrap();
        let QueryResult::Rows { rows, .. } = rows else {
            unreachable!("a chain with RETURN answers rows")
        };
        assert_eq!(
            rows,
            [vec![QueryValue::Value(GraphValue::Scalar(
                CanonicalScalar::Int(1)
            ))]]
        );
        let mut relations: Vec<_> = db
            .edges()
            .unwrap()
            .iter()
            .map(|edge| edge.entry.relation)
            .collect();
        relations.sort();
        assert_eq!(relations, [R, T]);
        assert_eq!(db.vertices().unwrap().len(), 3);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-cpny9 (TCK Merge5 [18]): a relationship MERGE over a leading MATCH
/// heads a chain once another write follows, through the MATCH's WITH head
/// or with every MATCH binding as `WITH *` carries them.
#[test]
fn a_match_led_relationship_merge_heads_a_chain() {
    let ((), report) = run_async_under_lab(0x1a5e_c4a8, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = fresh(&commit, false).await;
        let mut next = 100u128;
        let mut write = async |db: &mut Database<MemVfs>, text: &str| {
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| {
                    next += 1;
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                    })
                },
            )
            .await
            .map_err(|error| format!("{text}: {error}"))
        };
        let int = |value| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)));
        write(&mut db, "CREATE (:Person {p: 0})").await.unwrap();
        // Merge5 [18]: one vertex, so a = b and the MERGE makes a self loop;
        // the later MERGEs bind that vertex and match the loop. A rerun
        // matches everything.
        for _ in 0..2 {
            let doubled = write(
                &mut db,
                "MATCH (n:Person) MATCH (m:Person) WITH n AS a, m AS b MERGE (a)-[:R]->(b) \
                 WITH a AS x, b AS y MERGE (a) MERGE (b) MERGE (a)-[:R]->(b) \
                 RETURN x.p AS x, y.p AS y",
            )
            .await
            .unwrap();
            let QueryResult::Rows { rows, .. } = doubled else {
                unreachable!("a chain with RETURN answers rows")
            };
            assert_eq!(rows, [vec![int(0), int(0)]]);
            assert_eq!(db.vertices().unwrap().len(), 1);
            assert_eq!(db.edges().unwrap().len(), 1);
        }
        // Without WITH the next write reads every MATCH binding and the
        // relationship.
        write(
            &mut db,
            "MATCH (a:Person), (b:Person) MERGE (a)-[r:R]->(b) CREATE (b)-[:R]->(:Copy)",
        )
        .await
        .unwrap();
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(db.edges().unwrap().len(), 2);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-cpny9 (TCK Merge9 [3], Create3 [12]): a relationship MERGE another
/// write follows hands that write each row once per relationship its pair
/// resolved to, the relationship beside the row.
#[test]
fn a_relationship_merge_step_hands_its_rows_to_the_next_write() {
    let ((), report) = run_async_under_lab(0x1a5e_c4a5, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut next = 100u128;
        let mut write = async |db: &mut Database<MemVfs>, text: &str| {
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| {
                    next += 1;
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                    })
                },
            )
            .await
            .map_err(|error| format!("{text}: {error}"))
        };
        // Merge9 [3]: CREATE, MERGE between the created pair, then CREATE.
        let mut db = fresh(&commit, false).await;
        let counted = write(
            &mut db,
            "CREATE (a:Person), (b:Copy) MERGE (a)-[:R]->(b) \
             CREATE (b)-[:R]->(c:Source) RETURN count(*) AS c",
        )
        .await
        .unwrap();
        let QueryResult::Rows { rows, .. } = counted else {
            unreachable!("a chain with RETURN answers rows")
        };
        assert_eq!(
            rows,
            [vec![QueryValue::Value(GraphValue::Scalar(
                CanonicalScalar::Int(1)
            ))]]
        );
        assert_eq!(db.vertices().unwrap().len(), 3);
        assert_eq!(db.edges().unwrap().len(), 2);
        // Create3 [12]: unlabeled MERGEs bind the one vertex, the MERGE
        // between them makes a self-loop, and the CREATEs read the row.
        let mut db = fresh(&commit, false).await;
        let written = write(
            &mut db,
            "CREATE (a) WITH a MERGE (x) MERGE (y) MERGE (x)-[:R]->(y) \
             CREATE (b) CREATE (a)<-[:R]-(b)",
        )
        .await
        .unwrap();
        let QueryResult::Write { receipt, .. } = written else {
            unreachable!("a chain without RETURN answers a write receipt")
        };
        assert_eq!(receipt.stats().created_vertices, 2);
        assert_eq!(receipt.stats().created_edges, 2);
        let edges = db.edges().unwrap();
        assert_eq!(edges.len(), 2);
        assert_eq!(
            edges
                .iter()
                .filter(|edge| edge.entry.src == edge.entry.dst)
                .count(),
            1
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn readme_insert_example_runs_through_database_query_write() {
    let ((), report) = run_async_under_lab(0x1a5e_adaa, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = fresh(&commit, false).await;
        let before = db.frontier().unwrap();
        let (mut vertices, mut edges) = reserve(&mut db, &cx, 2, 1);
        let result = db.query_write(
            &txcx, &cx, &commit,
            "INSERT (:Person {name: 'Ada', born: 1815})-[:KNOWS {since: 1833}]->(:Person {name: 'Charles', born: 1791})",
            &GqlParameters::new(), symbols, R, policy(),
            |request| match request.request {
                GraphInsertRequest::Vertex { .. } => vertices.pop_front(),
                GraphInsertRequest::Edge { .. } => edges.pop_front(),
            }.ok_or("query requested an unreserved identity"),
        ).await.unwrap();
        assert!(vertices.is_empty() && edges.is_empty());
        let QueryResult::Write {
            receipt,
            completion,
        } = result
        else {
            panic!("INSERT must return a write receipt");
        };
        assert!(matches!(
            completion,
            Some(EmbeddedTxnCompletion::WriteCommitted { .. })
        ));
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        let [GraphWriteStepReceipt::Insert { vertices, edges }] = receipt.steps() else {
            panic!("README insertion must be one insertion step");
        };
        assert_eq!((vertices.len(), edges.len()), (2, 1));
        let rows = db.vertices().unwrap();
        assert_eq!(rows.len(), 2);
        for (id, name, born) in [(vertices[0], "Ada", 1815), (vertices[1], "Charles", 1791)] {
            let vertex = db.vertex(id).unwrap().unwrap();
            assert_eq!(vertex.labels, vec![PERSON]);
            assert_eq!(
                vertex.props,
                vec![
                    (NAME, CanonicalScalar::ucs_basic_text(name).unwrap().into()),
                    (BORN, CanonicalScalar::Int(born).into()),
                ]
            );
        }
        let rows = db.edges().unwrap();
        assert_eq!(rows.len(), 1);
        let edge = db.edge(edges[0]).unwrap().unwrap();
        assert_eq!(
            (edge.entry.src, edge.entry.dst, edge.entry.relation),
            (vertices[0], vertices[1], R)
        );
        assert_eq!(edge.props, vec![(SINCE, CanonicalScalar::Int(1833).into())]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-cpny9 (TCK Set1 [6], [7], Delete4 [3]): a SET, REMOVE or DELETE after
/// a CREATE writes the elements that CREATE made, row by row, as the chain's
/// last step. Its right-hand sides read what the CREATE wrote, and its RETURN
/// reads the values the statement left.
#[test]
fn a_write_after_create_updates_what_the_create_made() {
    let ((), report) = run_async_under_lab(0x1a5e_c4aa, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = fresh(&commit, true).await;
        let mut next = 100u128;
        let mut write = async |db: &mut Database<MemVfs>, text: &str, params: GqlParameters| {
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &params,
                symbols,
                R,
                policy(),
                |request| {
                    next += 1;
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                    })
                },
            )
            .await
            .map_err(|error| format!("{text}: {error}"))
        };
        let labelled = |db: &Database<MemVfs>, label: LabelId| {
            db.vertices()
                .unwrap()
                .into_iter()
                .filter(|vertex| vertex.labels == vec![label])
                .collect::<Vec<_>>()
        };
        let rows = |result: QueryResult| {
            let QueryResult::Rows { rows, .. } = result else {
                unreachable!("a chain with RETURN answers rows")
            };
            rows
        };
        let int = |value| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)));
        let stored = |value: i64| -> CanonicalPropertyValue { CanonicalScalar::Int(value).into() };

        // SET reads the created value and RETURN the one it set.
        let set = write(
            &mut db,
            "CREATE (n:Person {p: 1}) SET n.p = n.p + 1, n.q = 5 RETURN n.p AS p, n.q AS q",
            GqlParameters::new(),
        )
        .await
        .unwrap();
        assert_eq!(rows(set), [vec![int(2), int(5)]]);
        let people = labelled(&db, PERSON);
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].props, vec![(P, stored(2)), (Q, stored(5))]);

        // Each UNWIND row's CREATE is set by that row; without RETURN the
        // chain answers its receipt, with the SET's effects.
        let unwound = write(
            &mut db,
            "UNWIND [1, 2, 3] AS i CREATE (n:Copy) SET n.p = i * 10",
            GqlParameters::new(),
        )
        .await
        .unwrap();
        let QueryResult::Write { receipt, .. } = unwound else {
            unreachable!("a chain without RETURN answers a write receipt")
        };
        assert_eq!(receipt.stats().created_vertices, 3);
        assert_eq!(receipt.stats().mutation_effects, 3);
        assert_eq!(receipt.steps().len(), 2);
        let mut copies = labelled(&db, COPY)
            .into_iter()
            .map(|vertex| vertex.props)
            .collect::<Vec<_>>();
        copies.sort();
        assert_eq!(
            copies,
            [10, 20, 30].map(|value| vec![(P, stored(value))]).to_vec()
        );

        // A matched element's property feeds the SET of what the CREATE made.
        let matched = write(
            &mut db,
            "MATCH (a:Source) CREATE (b:Person) SET b.q = a.p RETURN b.q AS q ORDER BY q",
            GqlParameters::new(),
        )
        .await
        .unwrap();
        assert_eq!(rows(matched), [vec![int(10)], vec![int(20)]]);
        assert_eq!(labelled(&db, PERSON).len(), 3);

        // A WITH head between the writes, and a parameter.
        let headed = write(
            &mut db,
            "CREATE (n:Person) WITH n SET n.p = $v RETURN n.p AS p",
            GqlParameters::new().with_int64("v", 11).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(rows(headed), [vec![int(11)]]);

        // REMOVE after CREATE: the property never survives the statement.
        let removed = write(
            &mut db,
            "CREATE (n:Copy {p: 3, q: 4}) REMOVE n.p RETURN n.q AS q",
            GqlParameters::new(),
        )
        .await
        .unwrap();
        assert_eq!(rows(removed), [vec![int(4)]]);
        assert!(
            labelled(&db, COPY)
                .iter()
                .any(|vertex| vertex.props == vec![(Q, stored(4))])
        );

        // CREATE then DELETE (TCK Delete4 [3]): the vertex is made and gone.
        let vertices = db.vertices().unwrap().len();
        let deleted = write(
            &mut db,
            "MATCH (a:Source) CREATE (n:Person) DELETE n",
            GqlParameters::new(),
        )
        .await
        .unwrap();
        let QueryResult::Write { receipt, .. } = deleted else {
            unreachable!("a chain without RETURN answers a write receipt")
        };
        assert_eq!(receipt.stats().created_vertices, 2);
        assert_eq!(db.vertices().unwrap().len(), vertices);

        // Another write may follow a SET: it runs over the SET's post-write
        // rows, and the statement commits once.
        let before = db.frontier().unwrap();
        let (people, copies) = (labelled(&db, PERSON).len(), labelled(&db, COPY).len());
        write(
            &mut db,
            "CREATE (n:Person) SET n.p = 1 CREATE (m:Copy)",
            GqlParameters::new(),
        )
        .await
        .unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(labelled(&db, PERSON).len(), people + 1);
        assert_eq!(labelled(&db, COPY).len(), copies + 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A SET, REMOVE or DELETE another write follows hands that write its
/// post-write rows, through its WITH head or, directly before the write,
/// with every binding as `WITH *` carries them (TCK Match5 [26], Merge1 [14],
/// Merge5 [21]). Deleting a relationship and creating its replacement renames
/// a relationship type in one statement.
#[test]
fn a_write_after_a_mutation_reads_its_post_write_rows() {
    let ((), report) = run_async_under_lab(0x1a5e_c4ab, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = fresh(&commit, true).await;
        let mut next = 100u128;
        let mut write = async |db: &mut Database<MemVfs>, text: &str| {
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| {
                    next += 1;
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                    })
                },
            )
            .await
            .map_err(|error| format!("{text}: {error}"))
        };
        let relations = |db: &Database<MemVfs>| {
            db.edges()
                .unwrap()
                .into_iter()
                .map(|edge| (edge.entry.src, edge.entry.relation, edge.entry.dst))
                .collect::<Vec<_>>()
        };
        let labelled = |db: &Database<MemVfs>, label: LabelId| {
            let mut props = db
                .vertices()
                .unwrap()
                .into_iter()
                .filter(|vertex| vertex.labels == vec![label])
                .map(|vertex| vertex.props)
                .collect::<Vec<_>>();
            props.sort();
            props
        };
        let stored = |value: i64| -> CanonicalPropertyValue { CanonicalScalar::Int(value).into() };
        let int = |value| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)));

        // Rename the seeded R relationship to T, then back through a WITH.
        write(
            &mut db,
            "MATCH (a:Source)-[r:R]->(b:Source) DELETE r CREATE (a)-[:T]->(b)",
        )
        .await
        .unwrap();
        assert_eq!(relations(&db), [(VId(10), T, VId(20))]);
        write(
            &mut db,
            "MATCH (a:Source)-[r:T]->(b:Source) WITH a, b, r DELETE r CREATE (a)-[:R]->(b)",
        )
        .await
        .unwrap();
        assert_eq!(relations(&db), [(VId(10), R, VId(20))]);

        // The next write reads the value the SET left, not the matched one.
        write(
            &mut db,
            "MATCH (n:Source) SET n.q = n.p + 1 CREATE (:Copy {p: n.q})",
        )
        .await
        .unwrap();
        assert_eq!(
            labelled(&db, COPY),
            [vec![(P, stored(11))], vec![(P, stored(21))]]
        );
        // A WITH head filters the post-write rows the next write reads.
        let headed = write(
            &mut db,
            "MATCH (n:Source) SET n.q = n.q * 2 WITH n WHERE n.p = 10 \
             CREATE (n)-[:T]->(m:Person {p: n.q}) RETURN m.p AS p",
        )
        .await
        .unwrap();
        let QueryResult::Rows { rows, .. } = headed else {
            unreachable!("a chain with RETURN answers rows")
        };
        assert_eq!(rows, [vec![int(22)]]);
        assert_eq!(labelled(&db, PERSON), [vec![(P, stored(22))]]);
        // A SET between two CREATEs.
        write(
            &mut db,
            "CREATE (n:Person {p: 1}) SET n.p = 2 CREATE (:Copy {p: n.p})",
        )
        .await
        .unwrap();
        assert_eq!(labelled(&db, COPY).len(), 3);

        // Merge1 [14]: a MERGE after a DELETE never matches what the DELETE
        // removed. The first row creates the vertex and the later ones bind it.
        let merged = write(
            &mut db,
            "MATCH (a:Copy) DELETE a MERGE (b:Copy) RETURN b.p AS p",
        )
        .await
        .unwrap();
        let QueryResult::Rows { rows, .. } = merged else {
            unreachable!("a chain with RETURN answers rows")
        };
        let null = QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Null));
        assert_eq!(rows, vec![vec![null.clone()]; 3]);
        assert_eq!(labelled(&db, COPY), [Vec::new()]);

        // A failing later write rolls the mutation back with it, and a
        // relationship to a vertex the statement deleted refuses.
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        for refused in [
            "MATCH (n:Source) SET n.q = 0 CREATE (:Copy {p: 1 / 0})",
            "MATCH (a:Copy) DELETE a CREATE (a)-[:R]->(:Copy)",
        ] {
            assert!(write(&mut db, refused).await.is_err(), "{refused}");
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before,
                "{refused}"
            );
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A CREATE after a write's WITH tail may start with a MATCH: `... WITH p
/// UNWIND $tagIds AS tagId MATCH (t:Tag {id: tagId}) CREATE (p)-[:HAS_TAG]->(t)`,
/// the shape of LDBC SNB Interactive IU1, IU4, IU6 and IU7. The MATCH joins
/// the previous write's rows by its correlations, as a MATCH after a leading
/// UNWIND does, and reads the graph the earlier writes staged. A MATCH that
/// ends at RETURN is still the terminal read.
#[test]
fn a_chained_create_may_match_against_the_previous_writes_rows() {
    let ((), report) = run_async_under_lab(0x1a5e_c4ac, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = fresh(&commit, true).await;
        let mut next = 100u128;
        let mut write = async |db: &mut Database<MemVfs>, text: &str| {
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| {
                    next += 1;
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                    })
                },
            )
            .await
            .map_err(|error| format!("{text}: {error}"))
        };
        // The vertex a CREATE made, by its label and `p`.
        let made = |db: &Database<MemVfs>, label: LabelId, p: i64| {
            let found: Vec<VId> = db
                .vertices()
                .unwrap()
                .into_iter()
                .filter(|vertex| {
                    vertex.labels == vec![label]
                        && vertex.props.contains(&(P, CanonicalScalar::Int(p).into()))
                })
                .map(|vertex| vertex.vid)
                .collect();
            assert_eq!(found.len(), 1, "{found:?}");
            found[0]
        };
        // The (source, relation, destination, q) of every edge from `from`.
        let out = |db: &Database<MemVfs>, from: VId| {
            let mut edges: Vec<_> = db
                .edges()
                .unwrap()
                .into_iter()
                .filter(|edge| edge.entry.src == from)
                .map(|edge| {
                    let q = edge
                        .props
                        .iter()
                        .find(|(key, _)| *key == Q)
                        .map(|(_, value)| value.clone());
                    (edge.entry.relation, edge.entry.dst, q)
                })
                .collect();
            edges.sort();
            edges
        };

        // IU1's shape: a key without a match (99) adds no row.
        write(
            &mut db,
            "MATCH (s:Source {p: 10}) CREATE (a:Person {p: 1})-[:R]->(s) \
             WITH a UNWIND [10, 20, 99] AS k MATCH (t:Source {p: k}) CREATE (a)-[:T]->(t)",
        )
        .await
        .unwrap();
        let person = made(&db, PERSON, 1);
        assert_eq!(
            out(&db, person),
            [(R, VId(10), None), (T, VId(10), None), (T, VId(20), None)]
        );

        // The MATCH sees the vertex the first step just created.
        write(
            &mut db,
            "CREATE (a:Copy {p: 5}) WITH a UNWIND [5] AS k \
             MATCH (c:Copy {p: k}) CREATE (a)-[:T]->(c)",
        )
        .await
        .unwrap();
        let copy = made(&db, COPY, 5);
        assert_eq!(out(&db, copy), [(T, copy, None)]);

        // Uncorrelated, the MATCH multiplies the rows: one edge per Source.
        write(
            &mut db,
            "CREATE (a:Person {p: 2}) WITH a MATCH (s:Source) CREATE (a)-[:T]->(s)",
        )
        .await
        .unwrap();
        let person = made(&db, PERSON, 2);
        assert_eq!(out(&db, person), [(T, VId(10), None), (T, VId(20), None)]);

        // Three steps; a computed key (`w[0]`) correlates, the row feeds a
        // property, and RETURN reads the matched vertex.
        let returned = write(
            &mut db,
            "MATCH (s:Source {p: 20}) CREATE (a:Person {p: 3})-[:R]->(s) \
             WITH a UNWIND [[10, 7]] AS w MATCH (t:Source {p: w[0]}) \
             CREATE (a)-[:T {q: w[1]}]->(t) \
             WITH a UNWIND [10, 20] AS k MATCH (u:Source {p: k}) CREATE (a)-[:R]->(u) \
             RETURN u.p AS p ORDER BY p",
        )
        .await
        .unwrap();
        let QueryResult::Rows { columns, rows } = returned else {
            unreachable!("a chain with RETURN answers rows")
        };
        assert_eq!(columns, ["p"]);
        let int = |value| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)));
        assert_eq!(rows, [vec![int(10)], vec![int(20)]]);
        let person = made(&db, PERSON, 3);
        assert_eq!(
            out(&db, person),
            [
                (R, VId(10), None),
                (R, VId(20), None),
                (R, VId(20), None),
                (T, VId(10), Some(CanonicalScalar::Int(7).into())),
            ]
        );

        // A MATCH that ends at RETURN is the terminal read, not a write's.
        let counted = write(
            &mut db,
            "CREATE (a:Person {p: 4}) WITH a MATCH (s:Source) RETURN count(s) AS c",
        )
        .await
        .unwrap();
        let QueryResult::Rows { rows, .. } = counted else {
            unreachable!("a terminal read answers rows")
        };
        assert_eq!(rows.len(), 1);

        // A failing matched step publishes none of the earlier ones.
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        assert!(
            write(
                &mut db,
                "CREATE (a:Person {p: 5}) WITH a UNWIND [10] AS k \
                 MATCH (t:Source {p: k}) CREATE (a)-[:T {q: 1 / 0}]->(t)",
            )
            .await
            .is_err()
        );
        assert_eq!(
            (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap()
            ),
            before
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A write right after `MATCH ... UNWIND`, with no WITH between, runs over
/// the UNWIND's rows as `MATCH ... WITH * UNWIND ...` would: `MATCH (p)
/// UNWIND p.tags AS t CREATE (:Tag {name: t})`. It used to refuse ("expected
/// a supported graph write statement") although the explicit `WITH p UNWIND`
/// form ran.
#[test]
fn a_write_right_after_match_unwind_reads_its_rows() {
    let ((), report) = run_async_under_lab(0x1a5e_c4ad, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = fresh(&commit, true).await;
        let mut next = 100u128;
        let mut write = async |db: &mut Database<MemVfs>, text: &str| {
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| {
                    next += 1;
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                    })
                },
            )
            .await
            .map_err(|error| format!("{text}: {error}"))
        };
        let property = |db: &Database<MemVfs>, label: LabelId, key: PropertyKeyId| {
            let mut values: Vec<_> = db
                .vertices()
                .unwrap()
                .into_iter()
                .filter(|vertex| vertex.labels == vec![label])
                .filter_map(|vertex| {
                    vertex
                        .props
                        .into_iter()
                        .find(|(at, _)| *at == key)
                        .map(|(_, value)| value)
                })
                .collect();
            values.sort();
            values
        };
        let stored = |value: i64| -> CanonicalPropertyValue { CanonicalScalar::Int(value).into() };

        // CREATE: one vertex per (source, element) row. The UNWIND reads the
        // matched vertex's property.
        write(
            &mut db,
            "MATCH (s:Source) UNWIND [s.p + 1, s.p + 2] AS i CREATE (:Copy {p: i})",
        )
        .await
        .unwrap();
        assert_eq!(
            property(&db, COPY, P),
            [stored(11), stored(12), stored(21), stored(22)]
        );
        // So does the CREATE itself, through the row's hidden read of `s.p`,
        // and so does a RETURN after it.
        write(
            &mut db,
            "MATCH (s:Source) UNWIND [1, 2] AS i CREATE (:Person {p: s.p * 10 + i})",
        )
        .await
        .unwrap();
        assert_eq!(
            property(&db, PERSON, P),
            [stored(101), stored(102), stored(201), stored(202)]
        );
        let returned = write(
            &mut db,
            "MATCH (s:Source) UNWIND [7] AS i CREATE (c:Person {q: s.p + i}) \
             RETURN s.p AS p, c.q AS q ORDER BY p",
        )
        .await
        .unwrap();
        let QueryResult::Rows { rows, .. } = returned else {
            unreachable!("a CREATE with RETURN answers rows")
        };
        let int = |value| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)));
        assert_eq!(rows, [vec![int(10), int(17)], vec![int(20), int(27)]]);
        // SET: the row's value, one row per vertex.
        write(
            &mut db,
            "MATCH (s:Source) UNWIND [s.p * 2] AS v SET s.q = v",
        )
        .await
        .unwrap();
        assert_eq!(property(&db, SOURCE, Q), [stored(20), stored(40)]);
        // A relationship MERGE: two rows, one relationship.
        let before = db.edges().unwrap().len();
        write(
            &mut db,
            "MATCH (a:Source {p: 10}), (b:Source {p: 20}) UNWIND [1, 1] AS i \
             MERGE (a)-[:T]->(b)",
        )
        .await
        .unwrap();
        let edges = db.edges().unwrap();
        assert_eq!(edges.len(), before + 1);
        assert!(edges.iter().any(|edge| {
            (edge.entry.src, edge.entry.relation, edge.entry.dst) == (VId(10), T, VId(20))
        }));
        // An empty UNWIND writes nothing.
        write(
            &mut db,
            "MATCH (s:Source) UNWIND [] AS i CREATE (:Copy {p: i})",
        )
        .await
        .unwrap();
        assert_eq!(property(&db, COPY, P).len(), 4);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A read between writes can feed the relationship MERGE family, preserving
/// the occurrence bag, post-write visibility, final RETURN and one commit.
#[test]
fn matched_relationship_merge_runs_actions_and_following_writes_then_reopens() {
    let ((), report) = run_async_under_lab(0x1a5e_c4ae, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        for id in [10_u128, 20] {
            seed.create_vertex(
                VId(id),
                vec![SOURCE],
                vec![(P, CanonicalScalar::Int(id as i64))],
            );
        }
        db.write(&commit, seed).await.unwrap();
        let write = async |db: &mut Database<MemVfs>, text: &str, params: &GqlParameters| {
            let (mut vertices, mut edges) = reserve(db, &cx, 4, 8);
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                params,
                symbols,
                R,
                policy(),
                |request| {
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => vertices.pop_front().unwrap(),
                        GraphInsertRequest::Edge { .. } => edges.pop_front().unwrap(),
                    })
                },
            )
            .await
            .map_err(|error| format!("{text}: {error}"))
        };
        let int = |value| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)));
        let parameters = GqlParameters::new()
            .with_int64("created", 1)
            .unwrap()
            .with_list(
                "keys",
                [10, 10, 20, 99]
                    .into_iter()
                    .map(|value| GraphValue::Scalar(CanonicalScalar::Int(value)))
                    .collect(),
            )
            .unwrap()
            .with_int64("bump", 1)
            .unwrap();
        let original = parameters.canonical_bytes();
        let text = "MERGE (a:Person {p:$created}) WITH a UNWIND $keys AS k \
            MATCH (b:Source {p:k}) MERGE (a)-[r:T]->(b) \
            ON CREATE SET r.q=b.p ON MATCH SET r.q=r.q+$bump \
            RETURN b.p AS target,r.q AS weight ORDER BY target,weight";
        for (ten, twenty) in [(11, 20), (13, 21)] {
            let before = db.frontier().unwrap();
            let result = write(&mut db, text, &parameters).await.unwrap();
            let QueryResult::Rows { columns, rows } = result else {
                panic!("the final MERGE RETURN must return rows")
            };
            assert_eq!(columns, ["target", "weight"]);
            assert_eq!(
                rows,
                [
                    vec![int(10), int(ten)],
                    vec![int(10), int(ten)],
                    vec![int(20), int(twenty)]
                ],
                "duplicate keys keep both occurrences; RETURN sees all completed actions"
            );
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            assert_eq!(db.vertices().unwrap().len(), 3);
            let edges = db.edges().unwrap();
            assert_eq!(
                edges.len(),
                2,
                "the second occurrence and retry reuse the edges"
            );
            for (destination, weight) in [(VId(10), ten), (VId(20), twenty)] {
                let edge = edges
                    .iter()
                    .find(|edge| edge.entry.dst == destination)
                    .unwrap();
                assert_eq!(edge.entry.relation, T);
                assert_eq!(edge.props, [(Q, CanonicalScalar::Int(weight).into())]);
            }
        }
        assert_eq!(parameters.canonical_bytes(), original);

        // Selection reads a vertex created by an earlier step of this same
        // statement; no external commit is needed before matching it.
        write(
            &mut db,
            "CREATE (a:Copy {p:5}) WITH a MATCH (b:Copy {p:5}) MERGE (a)-[:T]->(b)",
            &GqlParameters::new(),
        )
        .await
        .unwrap();
        let copies: Vec<_> = db
            .vertices()
            .unwrap()
            .into_iter()
            .filter(|v| v.labels == [COPY])
            .collect();
        assert_eq!(copies.len(), 1);
        assert!(db.edges().unwrap().iter().any(|edge| {
            (edge.entry.src, edge.entry.relation, edge.entry.dst)
                == (copies[0].vid, T, copies[0].vid)
        }));

        // Row stages finish before MERGE; its post-action edge property is
        // then available to another write, including a reversed relationship.
        let result = write(
            &mut db,
            "CREATE (a:Person {p:2}) WITH a MATCH (b:Source) \
             WITH a,b ORDER BY b.p DESC LIMIT 1 MERGE (a)<-[r:T]-(b) SET r.q=b.p \
             WITH a,b,r CREATE (a)-[:R {q:r.q}]->(b) \
             RETURN a.p AS source,b.p AS target,r.q AS weight",
            &GqlParameters::new(),
        )
        .await
        .unwrap();
        let QueryResult::Rows { rows, .. } = result else {
            panic!("the following CREATE must retain MERGE's row")
        };
        assert_eq!(rows, [vec![int(2), int(20), int(20)]]);
        let person = db
            .vertices()
            .unwrap()
            .into_iter()
            .find(|v| {
                v.labels == [PERSON] && v.props.contains(&(P, CanonicalScalar::Int(2).into()))
            })
            .unwrap()
            .vid;
        assert!(db.edges().unwrap().iter().any(|edge| {
            (edge.entry.src, edge.entry.relation, edge.entry.dst) == (VId(20), T, person)
        }));
        assert!(db.edges().unwrap().iter().any(|edge| {
            (edge.entry.src, edge.entry.relation, edge.entry.dst) == (person, R, VId(20))
                && edge.props == [(Q, CanonicalScalar::Int(20).into())]
        }));
        assert_eq!(txcx.outstanding_obligations(), 0);
        let expected = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap()
            ),
            expected
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn matched_relationship_merge_late_errors_and_limits_roll_back_the_complete_statement() {
    let ((), report) = run_async_under_lab(0x1a5e_c4af, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = fresh(&commit, true).await;
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        let prefix = "CREATE (a:Person {p:1}) WITH a MATCH (b:Source) MERGE (a)-[r:T]->(b)";
        let cases = [
            (
                format!("{prefix} ON CREATE SET r.q=100/(b.p-20)"),
                policy(),
                "DivisionByZero",
            ),
            (
                prefix.to_owned(),
                GraphWriteProgramPolicy::new(policy().mutations.query, 100, 100, 1),
                "CreationLimit",
            ),
            (
                format!("{prefix} RETURN b.p AS p"),
                GraphWriteProgramPolicy::new(
                    GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000),
                    100,
                    100,
                    100,
                ),
                "ResultRows",
            ),
        ];
        for (text, admitted, expected) in cases {
            let (mut vertices, mut edges) = reserve(&mut db, &cx, 4, 8);
            let error = db
                .query_write(
                    &txcx,
                    &cx,
                    &commit,
                    &text,
                    &GqlParameters::new(),
                    symbols,
                    R,
                    admitted,
                    |request| {
                        Ok::<_, &'static str>(match request.request {
                            GraphInsertRequest::Vertex { .. } => vertices.pop_front().unwrap(),
                            GraphInsertRequest::Edge { .. } => edges.pop_front().unwrap(),
                        })
                    },
                )
                .await
                .unwrap_err();
            assert!(format!("{error:?}").contains(expected), "{text}: {error:?}");
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before
            );
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn created_named_paths_keep_traversal_overlay_reads_and_committed_graph_state() {
    let ((), report) = run_async_under_lab(0x1a5e_ca71, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let vfs = MemVfs::new().unwrap();
        let directory = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &directory, keys())
            .await
            .unwrap();
        let write = async |db: &mut Database<MemVfs>,
                           text: &str,
                           mut vertices: VecDeque<ElementId>,
                           mut edges: VecDeque<ElementId>| {
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| {
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => vertices.pop_front().unwrap(),
                        GraphInsertRequest::Edge { .. } => edges.pop_front().unwrap(),
                    })
                },
            )
            .await
        };
        let (vertices, edges) = reserve(&mut db, &cx, 3, 2);
        let nodes: Vec<_> = vertices
            .iter()
            .map(|value| match value {
                ElementId::Vertex(id) => *id,
                _ => unreachable!("reserved vertex"),
            })
            .collect();
        let relationships: Vec<_> = edges
            .iter()
            .map(|value| match value {
                ElementId::Edge(id) => *id,
                _ => unreachable!("reserved edge"),
            })
            .collect();
        let result = write(
            &mut db,
            "CREATE p=(a:Copy {p:11})<-[incoming:R {q:5}]-(b:Person {p:22}) \
             -[outgoing:T]->(c:Copy {p:33}) \
             RETURN p AS path,length(p) AS hops,nodes(p) AS nodes,relationships(p) AS edges",
            vertices,
            edges,
        )
        .await
        .unwrap();
        let QueryResult::Rows { columns, rows } = result else {
            panic!("named CREATE path returns native values")
        };
        let int = |value| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)));
        assert_eq!(columns, ["path", "hops", "nodes", "edges"]);
        assert_eq!(
            rows,
            [vec![
                QueryValue::Value(GraphValue::Path(fgdb_gql::algebra::GraphPath::new(
                    nodes[0],
                    vec![(relationships[0], nodes[1]), (relationships[1], nodes[2])]
                        .into_boxed_slice(),
                ))),
                int(2),
                QueryValue::Value(GraphValue::Vertices(nodes.clone().into_boxed_slice())),
                QueryValue::Value(GraphValue::Edges(relationships.clone().into_boxed_slice())),
            ]]
        );
        let stored = db.edges().unwrap();
        assert!(stored.iter().any(|edge| {
            (
                edge.entry.eid,
                edge.entry.src,
                edge.entry.relation,
                edge.entry.dst,
            ) == (relationships[0], nodes[1], R, nodes[0])
        }));
        assert!(stored.iter().any(|edge| {
            (
                edge.entry.eid,
                edge.entry.src,
                edge.entry.relation,
                edge.entry.dst,
            ) == (relationships[1], nodes[1], T, nodes[2])
        }));

        // The tail reads newly staged vertex properties through the real host,
        // while a renamed path remains available to each UNWIND occurrence.
        let (vertices, edges) = reserve(&mut db, &cx, 2, 1);
        let result = write(
            &mut db,
            "CREATE p=(a:Person {p:44})-[:R]->(b:Copy {p:55}) \
             WITH p AS carried UNWIND nodes(carried) AS n \
             RETURN n.p AS value,length(carried) AS hops ORDER BY value",
            vertices,
            edges,
        )
        .await
        .unwrap();
        let QueryResult::Rows { columns, rows } = result else {
            panic!("the path's row tail returns its overlay reads")
        };
        assert_eq!(columns, ["value", "hops"]);
        assert_eq!(rows, [vec![int(44), int(1)], vec![int(55), int(1)]]);
        assert_eq!(db.vertices().unwrap().len(), 5);
        assert_eq!(db.edges().unwrap().len(), 3);

        let (vertices, edges) = reserve(&mut db, &cx, 2, 1);
        let expected = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        let failure = write(
            &mut db,
            "CREATE p=(a {p:77})-[:R]->(b {p:88}) \
             WITH p UNWIND nodes(p) AS n \
             RETURN 1 / (n.p - 88) AS value LIMIT (1 - 1)",
            vertices,
            edges,
        )
        .await
        .unwrap_err();
        assert!(format!("{failure:?}").contains("DivisionByZero"));
        assert_eq!(
            (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap()
            ),
            expected,
            "a zero result page does not suppress late errors or publish partial writes"
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &directory, keys())
            .await
            .unwrap();
        assert_eq!(
            (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap()
            ),
            expected,
            "the successful path writes survive reopen"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A CREATE field reads `x.key` of an element its row carries in a value,
/// not a projected column (fgdb-v1rbj): an UNWIND member of a list, a
/// collected list, a path's nodes or relationships, and the relationship of
/// a MERGE path in a chained CREATE. The insertion evaluator had no element
/// reader, so each of these failed at run time with IncompatibleOperands.
#[test]
fn created_fields_read_elements_carried_in_values() {
    let ((), report) = run_async_under_lab(0x1a5e_c4b0, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let stored = |value| CanonicalPropertyValue::from(CanonicalScalar::Int(value));
        for (text, expected) in [
            (
                "MATCH (a:Source) UNWIND [a] AS x CREATE (:Copy {p:x.p})",
                vec![10, 20],
            ),
            (
                "MATCH (a:Source) WITH collect(a) AS xs UNWIND xs AS x CREATE (:Copy {p:x.p})",
                vec![10, 20],
            ),
            (
                "MATCH path=(:Source)-[:R]->(:Source) UNWIND nodes(path) AS n \
                 CREATE (:Copy {p:n.p})",
                vec![10, 20],
            ),
            (
                "MATCH ()-[r:R]->() WITH r UNWIND [r] AS e CREATE (:Copy {p:e.q})",
                vec![30],
            ),
            (
                "MATCH (a:Source) UNWIND [a] AS x CREATE (:Copy {p:x.p+1})",
                vec![11, 21],
            ),
            (
                "MATCH (a:Source {p:10}) UNWIND [a] AS x CREATE (:Copy {p:size(labels(x))})",
                vec![1],
            ),
            (
                "MATCH (a:Source {p:10}),(b:Source {p:20}) MERGE route=(a)<-[r:T]-(b) \
                 ON CREATE SET r.q=7 WITH route AS saved \
                 UNWIND relationships(saved) AS edge CREATE (:Copy {p:edge.q})",
                vec![7],
            ),
        ] {
            let mut db = fresh(&commit, true).await;
            let (mut vertices, mut edges) = reserve(&mut db, &cx, 4, 4);
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| {
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => vertices.pop_front().unwrap(),
                        GraphInsertRequest::Edge { .. } => edges.pop_front().unwrap(),
                    })
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{text}: {error}"));
            let copied: Vec<_> = db
                .vertices()
                .unwrap()
                .into_iter()
                .filter(|vertex| vertex.labels == [COPY])
                .map(|vertex| vertex.props)
                .collect();
            assert_eq!(copied.len(), expected.len(), "{text}: {copied:?}");
            for value in expected {
                assert!(
                    copied.contains(&vec![(P, stored(value))]),
                    "{text}: {value} in {copied:?}"
                );
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        }

        // A chained CREATE with a RETURN reads the carried element's field the
        // same way, after an earlier CREATE of the same statement.
        let mut db = fresh(&commit, true).await;
        let (mut vertices, mut edges) = reserve(&mut db, &cx, 4, 4);
        let returned = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                "CREATE (a:Copy {p:9}) WITH a UNWIND [a] AS x \
                 CREATE (c:Copy {p:x.p}) RETURN c.p AS copied",
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| {
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => vertices.pop_front().unwrap(),
                        GraphInsertRequest::Edge { .. } => edges.pop_front().unwrap(),
                    })
                },
            )
            .await
            .unwrap();
        let QueryResult::Rows { columns, rows } = returned else {
            panic!("a chained CREATE with RETURN returns rows")
        };
        assert_eq!(columns, ["copied"]);
        assert_eq!(
            rows,
            [vec![QueryValue::Value(GraphValue::Scalar(
                CanonicalScalar::Int(9)
            ))]]
        );
        let copies: Vec<_> = db
            .vertices()
            .unwrap()
            .into_iter()
            .filter(|vertex| vertex.labels == [COPY])
            .map(|vertex| vertex.props)
            .collect();
        assert_eq!(copies, [vec![(P, stored(9))], vec![(P, stored(9))]]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A SET or CREATE right side reads an element's identity (`id()`,
/// `elementId()`) or its NULL-ness (`x IS NULL` over an OPTIONAL MATCH
/// element) as a projection does (fgdb-j687q). The write binders refused
/// every element column such an expression named, though only the identity
/// or presence reaches the property. Each stored value equals the same
/// expression's projected value in the statement's own RETURN.
#[test]
fn write_right_sides_read_element_identity_and_presence() {
    let ((), report) = run_async_under_lab(0x1a5e_c4b1, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let write = async |db: &mut Database<MemVfs>, text: &str| {
            let (mut vertices, mut edges) = reserve(db, &cx, 4, 4);
            let result = db
                .query_write(
                    &txcx,
                    &cx,
                    &commit,
                    text,
                    &GqlParameters::new(),
                    symbols,
                    R,
                    policy(),
                    |request| {
                        Ok::<_, &'static str>(match request.request {
                            GraphInsertRequest::Vertex { .. } => vertices.pop_front().unwrap(),
                            GraphInsertRequest::Edge { .. } => edges.pop_front().unwrap(),
                        })
                    },
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error}"));
            match result {
                QueryResult::Rows { rows, .. } => rows,
                QueryResult::Write { .. } => Vec::new(),
            }
        };
        let null = QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Null));
        let boolean = |value| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Bool(value)));

        let mut db = fresh(&commit, true).await;
        for text in [
            "MATCH (a:Source {p:10}) CREATE (c:Copy {p:id(a),name:elementId(a)}) \
             RETURN id(a) AS i,elementId(a) AS e,c.p AS p,c.name AS name",
            "MATCH (a:Source {p:10}),(b:Source {p:20}) SET a.q=id(b),a.name=elementId(b) \
             RETURN id(b) AS i,elementId(b) AS e,a.q AS p,a.name AS name",
            "MATCH ()-[r:R]->(b:Source) SET b.q=id(r),b.name=elementId(r) \
             RETURN id(r) AS i,elementId(r) AS e,b.q AS p,b.name AS name",
        ] {
            let rows = write(&mut db, text).await;
            assert_eq!(rows.len(), 1, "{text}");
            let row = &rows[0];
            assert_ne!(row[0], null, "{text}");
            assert_ne!(row[1], null, "{text}");
            assert_eq!((&row[2], &row[3]), (&row[0], &row[1]), "{text}");
        }

        let rows = write(
            &mut db,
            "MATCH (a:Source {p:10}) OPTIONAL MATCH (a)-[:T]->(x) \
             SET a.q=x IS NULL,a.name=x IS NOT NULL RETURN a.q AS missing,a.name AS found",
        )
        .await;
        assert_eq!(rows, [vec![boolean(true), boolean(false)]]);
        write(
            &mut db,
            "MATCH (a:Source {p:10}) OPTIONAL MATCH (a)-[r:R]->(x) \
             CREATE (:Copy {p:1,q:r IS NOT NULL,name:x IS NULL})",
        )
        .await;
        let flagged = db
            .vertices()
            .unwrap()
            .into_iter()
            .find(|vertex| {
                vertex.labels == [COPY]
                    && vertex
                        .props
                        .contains(&(P, CanonicalPropertyValue::from(CanonicalScalar::Int(1))))
            })
            .unwrap();
        assert!(
            flagged
                .props
                .contains(&(Q, CanonicalPropertyValue::from(CanonicalScalar::Bool(true))))
        );
        assert!(flagged.props.contains(&(
            NAME,
            CanonicalPropertyValue::from(CanonicalScalar::Bool(false))
        )));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A relationship MERGE takes endpoints the WITH row computes, not only
/// carried MATCH vertices: list subscripts, head()/last() and UNWIND members
/// of collected vertices (the linked-list idiom `UNWIND range(0, size(ns)-2)
/// AS i WITH ns[i] AS x, ns[i+1] AS y MERGE (x)-[:NEXT]->(y)`). It was refused
/// at prepare ("a vertex the WITH row carries"), though CREATE and SET
/// admitted the same open-typed columns. A NULL endpoint refuses as for a
/// carried vertex, and a value that is not a vertex refuses the statement.
#[test]
fn relationship_merge_takes_endpoints_the_row_computes() {
    let ((), report) = run_async_under_lab(0x1a5e_c4b2, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let write = async |db: &mut Database<MemVfs>, text: &str| {
            let (mut vertices, mut edges) = reserve(db, &cx, 4, 4);
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| {
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => vertices.pop_front().unwrap(),
                        GraphInsertRequest::Edge { .. } => edges.pop_front().unwrap(),
                    })
                },
            )
            .await
            .map(|_| ())
            .map_err(|error| format!("{text}: {error}"))
        };
        let merged = |db: &Database<MemVfs>| {
            let mut pairs: Vec<_> = db
                .edges()
                .unwrap()
                .into_iter()
                .filter(|edge| edge.entry.relation == T)
                .map(|edge| (edge.entry.src, edge.entry.dst))
                .collect();
            pairs.sort();
            pairs
        };
        for (text, expected) in [
            (
                "MATCH (n:Source) WITH n ORDER BY n.p WITH collect(n) AS ns \
                 UNWIND range(0, size(ns)-2) AS i WITH ns[i] AS x, ns[i+1] AS y \
                 MERGE (x)-[:T]->(y)",
                vec![(VId(10), VId(20))],
            ),
            (
                "MATCH (n:Source) WITH n ORDER BY n.p WITH collect(n) AS ns \
                 WITH head(ns) AS x, last(ns) AS y MERGE (y)-[:T]->(x)",
                vec![(VId(20), VId(10))],
            ),
            (
                "MATCH (n:Source) WITH collect(n) AS ns UNWIND ns AS x MERGE (x)-[:T]->(x)",
                vec![(VId(10), VId(10)), (VId(20), VId(20))],
            ),
        ] {
            let mut db = fresh(&commit, true).await;
            // A second run matches the first run's relationships.
            for _ in 0..2 {
                write(&mut db, text).await.unwrap();
                assert_eq!(merged(&db), expected, "{text}");
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
        // An out-of-range subscript is a NULL endpoint, and an unwound value
        // that is not a vertex is no endpoint: each refuses with no effect.
        let mut db = fresh(&commit, true).await;
        let before = (db.frontier().unwrap(), db.edges().unwrap());
        for text in [
            "MATCH (n:Source) WITH collect(n) AS ns WITH ns[0] AS x, ns[5] AS y \
             MERGE (x)-[:T]->(y)",
            "MATCH (n:Source {p:10}) WITH [n, 1] AS xs UNWIND xs AS x MERGE (x)-[:T]->(x)",
        ] {
            assert!(write(&mut db, text).await.is_err(), "{text}");
            assert_eq!(
                (db.frontier().unwrap(), db.edges().unwrap()),
                before,
                "{text}"
            );
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A relationship MERGE map value the row computes (fgdb-at06q): a WITH
/// alias, a value over MATCH bindings, a literal or parameter UNWIND member,
/// a list (TCK Merge5 [14]). Each row's values filter existing edges and
/// fill a created one. Rows of one endpoint pair with query-equal values
/// share one edge (1 and 1.0 included), and different values make
/// different edges. A rerun matches every edge. NULL or a map refuses with
/// no effect.
#[test]
fn relationship_merge_maps_read_the_row() {
    fn render(value: &CanonicalPropertyValue) -> String {
        match value {
            CanonicalPropertyValue::Scalar(CanonicalScalar::Int(value)) => format!("{value}"),
            CanonicalPropertyValue::Scalar(CanonicalScalar::Float(value)) => {
                format!("{:?}", value.get())
            }
            CanonicalPropertyValue::Scalar(CanonicalScalar::Text(value)) => {
                format!("'{}'", value.as_str())
            }
            CanonicalPropertyValue::List(list) => format!(
                "[{}]",
                list.values()
                    .iter()
                    .map(render)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            _ => "?".to_owned(),
        }
    }
    let ((), report) = run_async_under_lab(0x1a5e_a706, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let write = async |db: &mut Database<MemVfs>, text: &str, params: &GqlParameters| {
            let (mut vertices, mut edges) = reserve(db, &cx, 4, 4);
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                params,
                symbols,
                R,
                policy(),
                |request| {
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => vertices.pop_front().unwrap(),
                        GraphInsertRequest::Edge { .. } => edges.pop_front().unwrap(),
                    })
                },
            )
            .await
            .map_err(|error| format!("{text}: {error}"))
        };
        // Every T edge: its endpoints and properties, in a stable order.
        let merged = |db: &Database<MemVfs>| {
            let mut edges: Vec<_> = db
                .edges()
                .unwrap()
                .into_iter()
                .filter(|edge| edge.entry.relation == T)
                .map(|edge| {
                    let props = edge
                        .props
                        .iter()
                        .map(|(key, value)| format!("{}={}", key.0, render(value)))
                        .collect::<Vec<_>>()
                        .join(" ");
                    (edge.entry.src, edge.entry.dst, props)
                })
                .collect();
            edges.sort();
            edges
        };
        let none = GqlParameters::new();
        let pair = |props: &str| (VId(10), VId(20), props.to_owned());
        for (text, expected) in [
            (
                "MATCH (a:Source {p: 10}), (b:Source {p: 20}) WITH a, b, 'x' AS s \
                 MERGE (a)-[r:T {name: s}]->(b)",
                vec![pair("3='x'")],
            ),
            // A value over MATCH bindings reads the MATCH rows as WITH * does.
            (
                "MATCH (a:Source {p: 10}), (b:Source {p: 20}) \
                 MERGE (a)-[:T {since: a.p + b.p}]->(b)",
                vec![pair("5=30")],
            ),
            // Rows with query-equal values share one edge, made from the
            // first of them; a different value makes another edge.
            (
                "UNWIND [1, 2, 1.0] AS x MATCH (a:Source {p: 10}), (b:Source {p: 20}) \
                 MERGE (a)-[:T {since: x}]->(b)",
                vec![pair("5=1"), pair("5=2")],
            ),
            // A computed list and a constant beside a row column.
            (
                "MATCH (a:Source {p: 10}), (b:Source {p: 20}) \
                 WITH a, b, split('a,b', ',') AS roles, 7 AS s \
                 MERGE (a)-[:T {name: roles, since: s, q: 'k'}]->(b)",
                vec![pair("2='k' 3=['a','b'] 5=7")],
            ),
        ] {
            let mut db = fresh(&commit, true).await;
            for _ in 0..2 {
                write(&mut db, text, &none).await.unwrap();
                assert_eq!(merged(&db), expected, "{text}");
            }
            assert_eq!(db.edges().unwrap().len(), expected.len() + 1, "{text}");
            assert_eq!(txcx.outstanding_obligations(), 0);
        }

        // An existing edge whose stored value equals the row's matches; the
        // seeded edge 30 carries q = 30 and gains no sibling.
        let mut db = fresh(&commit, true).await;
        write(
            &mut db,
            "MATCH (a:Source {p: 10}), (b:Source {p: 20}) WITH a, b, 30 AS s \
             MERGE (a)-[:R {q: s}]->(b)",
            &none,
        )
        .await
        .unwrap();
        assert_eq!(db.edges().unwrap().len(), 1);

        // RETURN reads the merged relationship's value, over a WITH row and
        // over `UNWIND $rows` (which the batch writer refused at RETURN).
        let returned = write(
            &mut db,
            "MATCH (a:Source {p: 10}), (b:Source {p: 20}) WITH a, b, 'x' AS s \
             MERGE (a)-[r:T {name: s}]->(b) RETURN r.name AS n",
            &none,
        )
        .await
        .unwrap();
        let QueryResult::Rows { columns, rows } = returned else {
            unreachable!("MERGE ... RETURN answers rows")
        };
        assert_eq!(columns, ["n"]);
        assert_eq!(
            rows,
            vec![vec![QueryValue::Value(GraphValue::Scalar(
                CanonicalScalar::Text(fgdb_types::CanonicalText::new_ucs_basic("x").unwrap())
            ))]]
        );
        let row = |a: i64, b: i64, since: i64| {
            let int = |value| GraphValue::Scalar(CanonicalScalar::Int(value));
            GraphValue::map(vec![
                ("a".into(), int(a)),
                ("b".into(), int(b)),
                ("s".into(), int(since)),
            ])
            .unwrap()
        };
        let rows_parameter = GqlParameters::new()
            .with_list(
                "rows",
                vec![row(10, 20, 2020), row(10, 20, 2020), row(20, 10, 1)],
            )
            .unwrap();
        for _ in 0..2 {
            let returned = write(
                &mut db,
                "UNWIND $rows AS row MATCH (a:Source {p: row.a}), (b:Source {p: row.b}) \
                 MERGE (a)-[r:T {since: row.s}]->(b) RETURN r.since AS s",
                &rows_parameter,
            )
            .await
            .unwrap();
            let QueryResult::Rows { rows, .. } = returned else {
                unreachable!("MERGE ... RETURN answers rows")
            };
            let int = |value| {
                vec![QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(
                    value,
                )))]
            };
            assert_eq!(rows, vec![int(2020), int(2020), int(1)]);
        }
        assert_eq!(
            merged(&db),
            vec![
                pair("3='x'"),
                pair("5=2020"),
                (VId(20), VId(10), "5=1".to_owned()),
            ]
        );

        // A MATCH-led map value with ON clauses is the chain's row MERGE: the
        // unique upsert reads only constant map values, and refused it ("expected
        // scalar literal or parameter MERGE property value"). ON CREATE, then
        // ON MATCH, each read the value the row computed.
        let mut db = fresh(&commit, true).await;
        for expected in [(10, 1), (10, 2)] {
            let returned = write(
                &mut db,
                "MATCH (a:Source {p: 10}), (b:Source {p: 20}) \
                 MERGE (a)-[r:T {since: a.p}]->(b) \
                 ON CREATE SET r.q = 1 ON MATCH SET r.q = r.q + 1 RETURN r.since, r.q",
                &none,
            )
            .await
            .unwrap();
            let QueryResult::Rows { rows, .. } = returned else {
                unreachable!("MERGE ... RETURN answers rows")
            };
            let int = |value| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)));
            assert_eq!(rows, vec![vec![int(expected.0), int(expected.1)]]);
        }
        assert_eq!(merged(&db), vec![pair("2=2 5=10")]);

        // NULL (a missing property) and a map can neither match nor be
        // created: each refuses with no effect.
        let before = (db.frontier().unwrap(), db.edges().unwrap());
        for text in [
            "MATCH (a:Source {p: 10}), (b:Source {p: 20}) MERGE (a)-[:T {since: a.name}]->(b)",
            "MATCH (a:Source {p: 10}), (b:Source {p: 20}) WITH a, b, {k: 1} AS m \
             MERGE (a)-[:T {since: m}]->(b)",
        ] {
            assert!(write(&mut db, text, &none).await.is_err(), "{text}");
            assert_eq!(
                (db.frontier().unwrap(), db.edges().unwrap()),
                before,
                "{text}"
            );
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A lone relationship MERGE over a WITH row runs as the statement's one
/// write (TCK Merge5 [16], [17]). is_chain claims every relationship MERGE
/// after a WITH (fgdb-de6mf), and the chain preparer refused it with
/// "expected another write after the first" until it admitted a single
/// relationship MERGE step. A rerun matches every edge the first run made.
#[test]
fn a_lone_relationship_merge_over_with_aliases_is_one_write() {
    let ((), report) = run_async_under_lab(0x1a5e_a717, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let write = async |db: &mut Database<MemVfs>, text: &str| {
            let (mut vertices, mut edges) = reserve(db, &cx, 4, 8);
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| {
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => vertices.pop_front().unwrap(),
                        GraphInsertRequest::Edge { .. } => edges.pop_front().unwrap(),
                    })
                },
            )
            .await
            .map_err(|error| format!("{text}: {error}"))
        };
        let int = |value| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)));
        let pairs = |db: &Database<MemVfs>| {
            let mut pairs: Vec<_> = db
                .edges()
                .unwrap()
                .into_iter()
                .filter(|edge| edge.entry.relation == T)
                .map(|edge| (edge.entry.src, edge.entry.dst))
                .collect();
            pairs.sort();
            pairs
        };
        for (text, mut expected_rows, expected_pairs) in [
            (
                "MATCH (n:Source) MATCH (m:Source) WITH n AS a, m AS b \
                 MERGE (a)-[r:T]->(b) RETURN a.p AS a, b.p AS b",
                vec![
                    vec![int(10), int(10)],
                    vec![int(10), int(20)],
                    vec![int(20), int(10)],
                    vec![int(20), int(20)],
                ],
                vec![
                    (VId(10), VId(10)),
                    (VId(10), VId(20)),
                    (VId(20), VId(10)),
                    (VId(20), VId(20)),
                ],
            ),
            (
                "MATCH (n:Source) WITH n AS a, n AS b MERGE (a)-[r:T]->(b) RETURN a.p AS a",
                vec![vec![int(10)], vec![int(20)]],
                vec![(VId(10), VId(10)), (VId(20), VId(20))],
            ),
        ] {
            expected_rows.sort_by_key(|row| format!("{row:?}"));
            let mut db = fresh(&commit, true).await;
            for _ in 0..2 {
                let QueryResult::Rows { mut rows, .. } = write(&mut db, text).await.unwrap() else {
                    unreachable!("MERGE ... RETURN answers rows")
                };
                rows.sort_by_key(|row| format!("{row:?}"));
                assert_eq!(rows, expected_rows, "{text}");
                assert_eq!(pairs(&db), expected_pairs, "{text}");
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A label or relationship type spelled like the COUNT or EXISTS subquery
/// takes its property map in a write after a WITH. The WITH head's hoist scan
/// read `Count {` there as `COUNT {` and refused the statement ("expected
/// MATCH"), with or without an aggregate. A real subquery still hoists,
/// bare or as a map value.
#[test]
fn labels_spelled_like_subqueries_take_property_maps_after_with() {
    let ((), report) = run_async_under_lab(0x1a5e_c4b3, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = fresh(&commit, true).await;
        for text in [
            "MATCH (n:Source) WITH count(n) AS total CREATE (:Count {p:total})",
            "MATCH (n:Source) WITH sum(n.p) AS total CREATE (:Exists {p:total})",
            "MATCH (n:Source) WITH n CREATE (:Copy:Count {p:n.p})",
            "MATCH (a:Source {p:10}),(b:Source {p:20}) WITH a,b CREATE (a)-[:COUNT {q:1}]->(b)",
            "MATCH (n:Source) WITH n MERGE (:Count {p:7})",
            "MATCH (n:Source) WITH n, COUNT { (n)-->() } AS c CREATE (:Copy {q:c})",
            "MATCH (n:Source) WITH n, {c: COUNT { (n)-->() }} AS m CREATE (:Copy {name:m.c})",
        ] {
            let (mut vertices, mut edges) = reserve(&mut db, &cx, 4, 4);
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| {
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => vertices.pop_front().unwrap(),
                        GraphInsertRequest::Edge { .. } => edges.pop_front().unwrap(),
                    })
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{text}: {error}"));
        }
        let int = |value| CanonicalPropertyValue::from(CanonicalScalar::Int(value));
        let mut labelled: Vec<_> = db
            .vertices()
            .unwrap()
            .into_iter()
            .filter(|vertex| vertex.labels != [SOURCE])
            .map(|vertex| (vertex.labels, vertex.props))
            .collect();
        labelled.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
        let mut expected = vec![
            (vec![COUNT], vec![(P, int(2))]),
            (vec![EXISTS], vec![(P, int(30))]),
            (vec![COPY, COUNT], vec![(P, int(10))]),
            (vec![COPY, COUNT], vec![(P, int(20))]),
            (vec![COUNT], vec![(P, int(7))]),
            // Vertex 10 has two outgoing relationships by then: the seeded R
            // and the COUNT one created above; vertex 20 has none.
            (vec![COPY], vec![(Q, int(2))]),
            (vec![COPY], vec![(Q, int(0))]),
            (vec![COPY], vec![(NAME, int(2))]),
            (vec![COPY], vec![(NAME, int(0))]),
        ];
        expected.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
        assert_eq!(labelled, expected);
        assert!(db.edges().unwrap().iter().any(|edge| {
            (edge.entry.src, edge.entry.relation, edge.entry.dst) == (VId(10), COUNTED, VId(20))
                && edge.props == [(Q, int(1))]
        }));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A relationship MERGE after a multipart read (`MATCH ... WITH ... MATCH
/// ... MERGE (a)-[:R]->(b)`) reads that read's complete relation, as CREATE
/// and SET do there. It refused at the second MATCH ("expected CREATE, SET,
/// REMOVE, DELETE or a relationship MERGE after MATCH ... WITH"). Endpoints
/// may be carried vertices or open columns (an UNWIND member); a NULL from
/// an OPTIONAL MATCH refuses the statement with no effect.
#[test]
fn relationship_merge_reads_a_multipart_read() {
    let ((), report) = run_async_under_lab(0x1a5e_c4b4, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let write = async |db: &mut Database<MemVfs>, text: &str| {
            let (mut vertices, mut edges) = reserve(db, &cx, 4, 4);
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| {
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => vertices.pop_front().unwrap(),
                        GraphInsertRequest::Edge { .. } => edges.pop_front().unwrap(),
                    })
                },
            )
            .await
            .map(|_| ())
            .map_err(|error| format!("{text}: {error}"))
        };
        let merged = |db: &Database<MemVfs>| {
            let mut pairs: Vec<_> = db
                .edges()
                .unwrap()
                .into_iter()
                .filter(|edge| edge.entry.relation == T)
                .map(|edge| (edge.entry.src, edge.entry.dst))
                .collect();
            pairs.sort();
            pairs
        };
        for (text, expected) in [
            (
                "MATCH (n:Source) WITH n MATCH (m:Source {p:20}) MERGE (n)-[:T]->(m)",
                vec![(VId(10), VId(20)), (VId(20), VId(20))],
            ),
            (
                "MATCH (n:Source) WITH collect(n) AS ns UNWIND ns AS x \
                 MATCH (y:Source {p:10}) MERGE (y)-[:T]->(x)",
                vec![(VId(10), VId(10)), (VId(10), VId(20))],
            ),
            (
                "MATCH (y:Source {p:10}) WITH y UNWIND [10, 20] AS k \
                 MATCH (n:Source {p:k}) MERGE (n)<-[:T]-(y)",
                vec![(VId(10), VId(10)), (VId(10), VId(20))],
            ),
        ] {
            let mut db = fresh(&commit, true).await;
            // A second run matches the first run's relationships.
            for _ in 0..2 {
                write(&mut db, text).await.unwrap();
                assert_eq!(merged(&db), expected, "{text}");
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
        let mut db = fresh(&commit, true).await;
        let before = (db.frontier().unwrap(), db.edges().unwrap());
        let text =
            "MATCH (n:Source {p:10}) WITH n OPTIONAL MATCH (n)-[:T]->(m) MERGE (n)-[:T]->(m)";
        assert!(write(&mut db, text).await.is_err(), "{text}");
        assert_eq!((db.frontier().unwrap(), db.edges().unwrap()), before);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A relationship MERGE after a WITH or UNWIND stage, or after a multipart
/// read, runs its ON CREATE / ON MATCH / SET clauses as the chain's row
/// actions. Only actions reading another element reached the chain before;
/// a relationship-only action after a stage was left to the unique upsert,
/// which reads a MATCH only, and refused ("expected CREATE, SET, REMOVE,
/// DELETE or a relationship MERGE after MATCH ... WITH").
#[test]
fn staged_relationship_merges_run_their_on_and_set_clauses() {
    let ((), report) = run_async_under_lab(0x1a5e_c4b5, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let write = async |db: &mut Database<MemVfs>, text: &str| {
            let (mut vertices, mut edges) = reserve(db, &cx, 4, 4);
            let result = db
                .query_write(
                    &txcx,
                    &cx,
                    &commit,
                    text,
                    &GqlParameters::new(),
                    symbols,
                    R,
                    policy(),
                    |request| {
                        Ok::<_, &'static str>(match request.request {
                            GraphInsertRequest::Vertex { .. } => vertices.pop_front().unwrap(),
                            GraphInsertRequest::Edge { .. } => edges.pop_front().unwrap(),
                        })
                    },
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error}"));
            match result {
                QueryResult::Rows { rows, .. } => rows,
                QueryResult::Write { .. } => Vec::new(),
            }
        };
        let int = |value| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)));
        let weights = |db: &Database<MemVfs>| -> Vec<_> {
            db.edges()
                .unwrap()
                .into_iter()
                .filter(|edge| edge.entry.relation == T)
                .map(|edge| (edge.entry.src, edge.entry.dst, edge.props))
                .collect()
        };
        for (text, first, second) in [
            (
                "MATCH (a:Source {p:10}), (b:Source {p:20}) WITH a, b MERGE (a)-[r:T]->(b) \
                 ON CREATE SET r.q = 1 ON MATCH SET r.q = r.q + 1 RETURN r.q AS q",
                1,
                2,
            ),
            (
                "MATCH (a:Source {p:10}) WITH a MATCH (b:Source {p:20}) MERGE (a)-[r:T]->(b) \
                 ON CREATE SET r.q = 5 ON MATCH SET r.q = r.q * 2 RETURN r.q AS q",
                5,
                10,
            ),
            (
                "MATCH (a:Source {p:10}), (b:Source {p:20}) UNWIND [3] AS k \
                 MERGE (a)-[r:T]->(b) ON CREATE SET r.q = k ON MATCH SET r.q = r.q + k \
                 RETURN r.q AS q",
                3,
                6,
            ),
        ] {
            let mut db = fresh(&commit, true).await;
            // The first run creates and takes ON CREATE; the second matches.
            for expected in [first, second] {
                assert_eq!(write(&mut db, text).await, [vec![int(expected)]], "{text}");
            }
            assert_eq!(
                weights(&db),
                [(
                    VId(10),
                    VId(20),
                    vec![(
                        Q,
                        CanonicalPropertyValue::from(CanonicalScalar::Int(second))
                    )]
                )],
                "{text}"
            );
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
        let mut db = fresh(&commit, true).await;
        let text = "MATCH (a:Source {p:10}), (b:Source {p:20}) WITH a, b \
                    MERGE (a)-[r:T]->(b) SET r.q = 7";
        assert!(write(&mut db, text).await.is_empty());
        assert_eq!(
            weights(&db),
            [(
                VId(10),
                VId(20),
                vec![(Q, CanonicalPropertyValue::from(CanonicalScalar::Int(7)))]
            )]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
