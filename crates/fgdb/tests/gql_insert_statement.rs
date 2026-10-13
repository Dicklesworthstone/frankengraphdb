//! INSERT must preserve CREATE's native program, receipts and complete graph state.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
    GraphWriteStepReceipt, PreparedGraphInsertText, PreparedGraphWriteProgram,
    PreparedGraphWriteScript,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    PurposeContexts, QueryCx, VId,
};
use std::collections::VecDeque;

const R: RelationId = RelationId(1);
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
