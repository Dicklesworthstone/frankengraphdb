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
                    (NAME, CanonicalScalar::ucs_basic_text(name).unwrap()),
                    (BORN, CanonicalScalar::Int(born)),
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
        assert_eq!(edge.props, vec![(SINCE, CanonicalScalar::Int(1833))]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
