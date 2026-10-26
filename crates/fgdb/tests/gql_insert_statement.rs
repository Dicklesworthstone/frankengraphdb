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
        let frontier = db.frontier().unwrap();
        for refused in [
            "MATCH (n:Source) WITH n.p AS p CREATE (p)-[:R]->(:Copy)",
            "MATCH (n:Source) WITH n AS a CREATE (a:Person)",
            "MATCH (n:Source) WITH n AS a CREATE (:Copy {p: n.p})",
            "MATCH (n:Source) WITH n AS a CREATE (:Copy) RETURN n",
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
        // A carried element's metadata after the clauses, an anonymous MERGE
        // with clauses, and a SET of a row element refuse and publish nothing.
        let frontier = db.frontier().unwrap();
        for refused in [
            "MATCH (person:Person {born: 5}) MERGE (c:Copy {p: 4}) SET c.q = 9 \
             RETURN labels(person) AS l",
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
        // A head no write follows and a WHERE after it refuse and publish
        // nothing.
        let frontier = db.frontier().unwrap();
        for refused in [
            "MERGE (a:Person {p: 60}) WITH a",
            "MERGE (a:Person {p: 60}) WITH a WHERE a.p > 1 CREATE (b)",
        ] {
            assert!(write(&mut db, refused).await.is_err(), "{refused}");
        }
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
