//! Public native write dispatch must reach each statement's returning engine.
//! No graph state is recovered by rerunning RETURN as a separate read.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, QueryResult, QueryWriteError, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{
    GqlMapParameter, GqlParameterValue, GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphAggregateValue, GraphMutationQueryError, GraphSymbol, GraphSymbolKind,
    GraphVertexUpsertError, GraphWriteIdentityRequest, GraphWriteProgramPolicy,
};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion,
    PurposeContexts, VId,
};
use std::convert::Infallible;
use std::error::Error;
use std::sync::Arc;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const ID: PropertyKeyId = PropertyKeyId(2);
const L: LabelId = LabelId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x41; 32],
        DatabaseSecurityNamespaceId([0x42; 32]),
        [0x43; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(ID)),
        _ => None,
    }
}
fn policy(rows: u64, effects: u64, vertices: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(10_000, rows, 1_000_000, 1_000_000),
        effects,
        vertices,
        0,
    )
}
fn no_allocate(_: GraphWriteIdentityRequest) -> Result<ElementId, Infallible> {
    panic!("this statement must not allocate an identity")
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn rows(result: QueryResult, names: &[&str]) -> Vec<Vec<GraphValue>> {
    let QueryResult::Rows { columns, rows } = result else {
        panic!("RETURN was incorrectly dispatched to the no-result script engine")
    };
    assert_eq!(
        columns,
        names.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()
    );
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| match cell {
                    GraphAggregateValue::Value(value) => value,
                    _ => panic!("write RETURN cells retain native graph/scalar domains"),
                })
                .collect()
        })
        .collect()
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(
        VId(1),
        vec![L],
        vec![(P, CanonicalScalar::Int(4)), (ID, CanonicalScalar::Int(1))],
    );
    batch.create_vertex(
        VId(2),
        vec![L],
        vec![(P, CanonicalScalar::Int(7)), (ID, CanonicalScalar::Int(2))],
    );
    batch
}

/// fgdb-sl4ym (TCK Delete6 [5]-[7], [12]-[14]): `MATCH ... WITH ... SET /
/// REMOVE / DELETE` writes once per WITH row. Targets are the elements the
/// row carries, values read the row, and RETURN or a WITH tail reads the
/// row's values captured before the write.
#[test]
fn writes_after_with_target_the_row_and_return_its_captured_values() {
    let ((), report) = run_async_under_lab(0xa071_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let write = async |db: &mut Database<_>, text: &str| {
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(10, 10, 0),
                no_allocate,
            )
            .await
        };
        let set = write(
            &mut db,
            "MATCH (n:L) WITH n, n.p AS p SET n.id = p * 10 RETURN p ORDER BY p",
        )
        .await
        .unwrap();
        assert_eq!(rows(set, &["p"]), vec![vec![int(4)], vec![int(7)]]);
        for (vid, id) in [(VId(1), 40), (VId(2), 70)] {
            let vertex = db.vertex(vid).unwrap().unwrap();
            assert!(
                vertex
                    .props
                    .contains(&(ID, CanonicalScalar::Int(id).into())),
                "{vertex:?}"
            );
        }
        // A carried element's property reads its value before the write in
        // SET, and after it in RETURN.
        let carried = write(
            &mut db,
            "MATCH (n:L) WITH n SET n.id = n.p + 100 RETURN n.id AS id, n.p AS p ORDER BY p",
        )
        .await
        .unwrap();
        assert_eq!(
            rows(carried, &["id", "p"]),
            vec![vec![int(104), int(4)], vec![int(107), int(7)]]
        );
        // A map column of the row reads its entry in the write.
        let mapped = write(
            &mut db,
            "MATCH (n:L) WITH n, {k: n.p * 2} AS m SET n.id = m.k RETURN n.id AS id ORDER BY id",
        )
        .await
        .unwrap();
        assert_eq!(rows(mapped, &["id"]), vec![vec![int(8)], vec![int(14)]]);
        // A plain WITH may order and page the rows the write then reads.
        assert!(matches!(
            write(
                &mut db,
                "MATCH (n:L) WITH n ORDER BY n.p LIMIT 1 REMOVE n.id"
            )
            .await
            .unwrap(),
            QueryResult::Write { .. }
        ));
        let first = db.vertex(VId(1)).unwrap().unwrap();
        assert!(first.props.iter().all(|(key, _)| *key != ID), "{first:?}");
        let tail = write(
            &mut db,
            "MATCH (n:L) WITH n, n.p AS p WHERE p > 5 DELETE n WITH p WHERE p % 7 = 0 RETURN p",
        )
        .await
        .unwrap();
        assert_eq!(rows(tail, &["p"]), vec![vec![int(7)]]);
        assert_eq!(db.vertex(VId(2)).unwrap(), None);
        let frontier = db.frontier().unwrap();
        for refused in [
            "MATCH (n:L) WITH n.p AS x SET x.id = 1",
            "MATCH (n:L) WITH n AS a SET n.id = 1",
            "MATCH (n:L) WITH collect(n) AS ns SET ns.id = 1",
        ] {
            assert!(write(&mut db, refused).await.is_err(), "{refused}");
        }
        assert_eq!(db.frontier().unwrap(), frontier);
        let total = write(
            &mut db,
            "MATCH (n:L) WITH n, n.p AS p DELETE n RETURN sum(p) AS total",
        )
        .await
        .unwrap();
        assert_eq!(rows(total, &["total"]), vec![vec![int(4)]]);
        assert!(
            db.vertices()
                .unwrap()
                .iter()
                .all(|row| row.retired_at.is_some())
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// TCK Delete3: `DELETE p` deletes a matched path's relationships and nodes
/// (refused while a node keeps another relationship); `DETACH DELETE p`
/// deletes its nodes with every edge; a NULL OPTIONAL path deletes nothing.
#[test]
fn deleting_a_path_deletes_its_elements() {
    let ((), report) = run_async_under_lab(0xa071_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = seed();
        for id in [3, 4] {
            batch.create_vertex(
                VId(id),
                vec![L],
                vec![(ID, CanonicalScalar::Int(id as i64))],
            );
        }
        batch.add_edge(fgdb_types::EId(10), VId(1), VId(2), vec![]);
        batch.add_edge(fgdb_types::EId(11), VId(2), VId(3), vec![]);
        batch.add_edge(fgdb_types::EId(12), VId(2), VId(4), vec![]);
        db.write(&commit, batch).await.unwrap();
        let write = async |db: &mut Database<_>, text: &str| {
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(10, 10, 0),
                no_allocate,
            )
            .await
        };
        let path = "p = (a:L {id: 1})-[:R]->()-[:R]->(c:L {id: 3})";
        let frontier = db.frontier().unwrap();
        // Node 2 keeps edge 12, so a plain DELETE of the path is refused when
        // it executes, not as a name the statement does not know.
        let refused = write(&mut db, &format!("MATCH {path} DELETE p")).await;
        assert!(
            matches!(refused, Err(QueryWriteError::Execute(_))),
            "{refused:?}"
        );
        assert_eq!(db.frontier().unwrap(), frontier);
        write(
            &mut db,
            "OPTIONAL MATCH p = (:L {id: 9})-[:R]->() DETACH DELETE p",
        )
        .await
        .unwrap();
        assert_eq!(db.frontier().unwrap(), frontier);
        write(&mut db, &format!("MATCH {path} DETACH DELETE p"))
            .await
            .unwrap();
        for (vid, live) in [(1, false), (2, false), (3, false), (4, true)] {
            assert_eq!(db.vertex(VId(vid)).unwrap().is_some(), live, "vertex {vid}");
        }
        assert!(
            db.edges().unwrap().is_empty(),
            "every edge left with node 2"
        );
        // Node 4 lost its only edge with node 2, so a plain DELETE of its
        // one-node path deletes it.
        write(&mut db, "MATCH p = (:L {id: 4}) DELETE p")
            .await
            .unwrap();
        assert_eq!(db.vertex(VId(4)).unwrap(), None);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// TCK Delete5: after WITH, DELETE and DETACH DELETE take a value the row
/// computes - a list element, a map entry, a path in a nested map. Each case
/// runs on its own graph. A value that is not an element refuses when it
/// runs, a scalar row column refuses at preparation, and neither commits.
#[test]
fn deletes_after_with_take_elements_the_row_computes() {
    let ((), report) = run_async_under_lab(0xa071_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let write = async |db: &mut Database<_>, text: &str, parameters: &GqlParameters| {
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                parameters,
                symbols,
                R,
                policy(100, 100, 0),
                no_allocate,
            )
            .await
        };
        let empty = GqlParameters::new();
        // Vertices 1..=n labeled L with ids, and R edges by (eid, from, to).
        let graph = |vertices: u128, edges: &[(u128, u128, u128)]| {
            let mut batch = WriteBatch::new(R);
            for id in 1..=vertices {
                batch.create_vertex(
                    VId(id),
                    vec![L],
                    vec![(ID, CanonicalScalar::Int(id as i64))],
                );
            }
            for &(eid, from, to) in edges {
                batch.add_edge(fgdb_types::EId(eid), VId(from), VId(to), vec![]);
            }
            batch
        };
        let star = [(10, 1, 2), (11, 1, 3), (12, 1, 4)];
        let live = |db: &Database<_>, ids: core::ops::RangeInclusive<u128>| {
            ids.filter(|&id| db.vertex(VId(id)).unwrap().is_some())
                .count()
        };
        // DETACH DELETE of one relationship of a collected list (Delete5 [2]).
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, graph(4, &star)).await.unwrap();
        let one = GqlParameters::new().with_int64("i", 1).unwrap();
        write(
            &mut db,
            "MATCH (:L {id: 1})-[r:R]->() WITH collect(r) AS rs DETACH DELETE rs[$i]",
            &one,
        )
        .await
        .unwrap();
        assert_eq!((live(&db, 1..=4), db.edges().unwrap().len()), (4, 2));
        // DETACH DELETE of one collected node takes its edge (Delete5 [1]).
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, graph(4, &star)).await.unwrap();
        write(
            &mut db,
            "MATCH (:L {id: 1})-[:R]->(n) WITH collect(n) AS ns DETACH DELETE ns[0]",
            &empty,
        )
        .await
        .unwrap();
        assert_eq!((live(&db, 1..=4), db.edges().unwrap().len()), (3, 2));
        // A plain DELETE of isolated nodes held in a map (Delete5 [3]).
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, graph(2, &[])).await.unwrap();
        write(
            &mut db,
            "MATCH (n:L) WITH {key: n} AS m DELETE m.key",
            &empty,
        )
        .await
        .unwrap();
        assert_eq!(live(&db, 1..=2), 0);
        // Paths in a nested map: both paths of a two-cycle, plain (Delete5 [7]).
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, graph(2, &[(10, 1, 2), (11, 2, 1)]))
            .await
            .unwrap();
        let frontier = db.frontier().unwrap();
        let at_run = write(&mut db, "MATCH (n:L) WITH [1, 2] AS l DELETE l[0]", &empty).await;
        assert!(
            matches!(at_run, Err(QueryWriteError::Execute(_))),
            "{at_run:?}"
        );
        let scalar = write(&mut db, "MATCH (n:L) WITH n.id AS x DELETE x", &empty).await;
        assert!(scalar.is_err(), "{scalar:?}");
        assert_eq!(db.frontier().unwrap(), frontier);
        write(
            &mut db,
            "MATCH p = (:L)-[:R]->(:L) WITH {key: {key: collect(p)}} AS m \
             DELETE m.key.key[0], m.key.key[1]",
            &empty,
        )
        .await
        .unwrap();
        assert_eq!((live(&db, 1..=2), db.edges().unwrap().len()), (0, 0));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn public_autocommit_returns_parameterized_mutation_rows_with_collection_types() {
    let ((), report) = run_async_under_lab(0xa071_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let payload = GraphValue::List(
            vec![int(3), GraphValue::Scalar(CanonicalScalar::Null)].into_boxed_slice(),
        );
        let meta = GraphValue::map(vec![("k".into(), int(17))]).unwrap();
        let mut args = GqlParameters::new()
            .with_int64("step", 2)
            .unwrap()
            .with_list(
                "tail",
                vec![int(3), GraphValue::Scalar(CanonicalScalar::Null)],
            )
            .unwrap();
        args.insert(
            "meta",
            GqlParameterValue::Map(GqlMapParameter::new(vec![("k".into(), int(17))]).unwrap()),
        )
        .unwrap();
        let frozen = args.canonical_bytes();
        let result = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                "/* CREATE (x) RETURN x */ MATCH (n:L) SET n.p=n.p+$step \
             RETURN n.id AS id,n.p AS p,$tail AS tail,$meta AS meta ORDER BY id",
                &args,
                symbols,
                R,
                policy(2, 2, 0),
                no_allocate,
            )
            .await
            .unwrap();
        assert_eq!(
            rows(result, &["id", "p", "tail", "meta"]),
            vec![
                vec![int(1), int(6), payload.clone(), meta.clone()],
                vec![int(2), int(9), payload, meta],
            ]
        );
        assert_eq!(args.canonical_bytes(), frozen);
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
        for (id, expected) in [(VId(1), 6), (VId(2), 9)] {
            assert_eq!(
                db.vertex(id).unwrap().unwrap().props[0],
                (P, CanonicalScalar::Int(expected).into())
            );
        }
        let result = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                "MATCH (n:L) REMOVE n.p RETURN DISTINCT n.p AS p",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 2, 0),
                no_allocate,
            )
            .await
            .unwrap();
        assert_eq!(
            rows(result, &["p"]),
            vec![vec![GraphValue::Scalar(CanonicalScalar::Null)]]
        );
        assert_eq!(db.frontier().unwrap(), CommitSeq(3));
        // A write RETURN item is a whole expression: a comparison, IS [NOT]
        // NULL or Boolean connector over post-statement values is its Boolean
        // (TCK Remove1 [1]: `REMOVE n.num RETURN n.num IS NOT NULL`).
        let result = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                "MATCH (n:L) SET n.p = n.id RETURN n.id AS id, n.p IS NOT NULL AS kept, \
                 n.p = 2 OR n.p < 0 AS second ORDER BY id",
                &GqlParameters::new(),
                symbols,
                R,
                policy(2, 2, 0),
                no_allocate,
            )
            .await
            .unwrap();
        let flag = |value| GraphValue::Scalar(CanonicalScalar::Bool(value));
        assert_eq!(
            rows(result, &["id", "kept", "second"]),
            vec![
                vec![int(1), flag(true), flag(false)],
                vec![int(2), flag(true), flag(true)],
            ]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn returning_write_classes_share_one_outer_workspace_and_refusals_preserve_its_prefix() {
    let ((), report) = run_async_under_lab(0xa071_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let vertex = VId(u128::MAX);
        let result = txn
            .query_write(
                &mut db,
                &cx,
                "CREATE (n:L {id:1,p:4}) RETURN n,n.p AS p",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 0, 1),
                |request| {
                    assert_eq!(request.statement, 0);
                    assert!(matches!(
                        request.request,
                        GraphInsertRequest::Vertex { row: 0, vertex: 0 }
                    ));
                    Ok::<_, Infallible>(ElementId::Vertex(vertex))
                },
            )
            .unwrap();
        assert_eq!(
            rows(result, &["n", "p"]),
            vec![vec![GraphValue::Vertex(vertex), int(4)]]
        );
        let result = txn
            .query_write(
                &mut db,
                &cx,
                "MATCH (n:L) SET n.p=n.p+1 RETURN n.p AS p",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 1, 0),
                no_allocate,
            )
            .unwrap();
        assert_eq!(rows(result, &["p"]), vec![vec![int(5)]]);
        let result = txn
            .query_write(
                &mut db,
                &cx,
                "MERGE (n:L {id:1}) ON MATCH SET n.p=8 RETURN n,n.p AS p",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 1, 0),
                no_allocate,
            )
            .unwrap();
        assert_eq!(
            rows(result, &["n", "p"]),
            vec![vec![GraphValue::Vertex(vertex), int(8)]]
        );
        let prefix = txn.staged_effect_digest().unwrap();
        let error = txn
            .query_write(
                &mut db,
                &cx,
                "MATCH (n:L) SET n.p=9 RETURN 1/(n.p-9) AS bad",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 1, 0),
                no_allocate,
            )
            .unwrap_err();
        assert!(matches!(
            &error,
            QueryWriteError::Mutation(
                GqlQueryError::Source(GraphMutationQueryError::Returning(_),)
            )
        ));
        assert!(error.source().is_some());
        assert_eq!(txn.staged_effect_digest().unwrap(), prefix);
        let mut allocations = 0;
        let error = txn
            .query_write(
                &mut db,
                &cx,
                "MERGE (n:L {id:2}) SET n.p=9 RETURN 1/(n.p-9) AS bad",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 1, 1),
                |request| {
                    assert_eq!(request.statement, 0);
                    allocations += 1;
                    Ok::<_, Infallible>(ElementId::Vertex(VId(u128::MAX - 1)))
                },
            )
            .unwrap_err();
        assert!(matches!(
            &error,
            QueryWriteError::VertexUpsert(GqlQueryError::Source(
                GraphVertexUpsertError::Returning(_),
            ))
        ));
        assert!(error.source().is_some());
        assert_eq!(allocations, 1);
        assert_eq!(txn.staged_effect_digest().unwrap(), prefix);
        assert!(txn.vertex(&db, VId(u128::MAX - 1)).unwrap().is_none());
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        assert!(matches!(
            txn.query_write(
                &mut db,
                &cx,
                "MATCH (n:L) SET n.p=11",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 1, 0),
                no_allocate,
            )
            .unwrap(),
            QueryResult::Write {
                completion: None,
                ..
            }
        ));
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(1)
            }
        ));
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(
            db.vertex(vertex).unwrap().unwrap().props[0],
            (P, CanonicalScalar::Int(11).into())
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn public_dispatch_preserves_creation_effect_and_final_result_limits() {
    let ((), report) = run_async_under_lab(0xa071_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let create = "MERGE (n:L {id:1}) SET n.p=4 RETURN n.p AS p";
        assert!(matches!(
            db.query_write(
                &txcx,
                &cx,
                &commit,
                create,
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 1, 0),
                no_allocate,
            )
            .await,
            Err(QueryWriteError::VertexUpsert(_))
        ));
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        let result = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                create,
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 1, 1),
                |request| {
                    assert_eq!(request.statement, 0);
                    Ok::<_, Infallible>(ElementId::Vertex(VId(10)))
                },
            )
            .await
            .unwrap();
        assert_eq!(rows(result, &["p"]), vec![vec![int(4)]]);
        for allowance in [policy(0, 1, 0), policy(1, 0, 0)] {
            let result = db
                .query_write(
                    &txcx,
                    &cx,
                    &commit,
                    "MATCH (n:L) SET n.p=7 RETURN n.p AS p",
                    &GqlParameters::new(),
                    symbols,
                    R,
                    allowance,
                    no_allocate,
                )
                .await;
            assert!(matches!(result, Err(QueryWriteError::Mutation(_))));
            assert_eq!(db.frontier().unwrap(), CommitSeq(1));
            assert_eq!(
                db.vertex(VId(10)).unwrap().unwrap().props[0],
                (P, CanonicalScalar::Int(4).into())
            );
        }
        let result = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                "MATCH (n:L) SET n.p=7 RETURN n.p AS p LIMIT 0",
                &GqlParameters::new(),
                symbols,
                R,
                policy(0, 1, 0),
                no_allocate,
            )
            .await
            .unwrap();
        assert!(rows(result, &["p"]).is_empty());
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
        assert_eq!(
            db.vertex(VId(10)).unwrap().unwrap().props[0],
            (P, CanonicalScalar::Int(7).into())
        );
        let result = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                "MATCH (n:L) DETACH DELETE n RETURN n",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 1, 0),
                no_allocate,
            )
            .await
            .unwrap();
        assert_eq!(
            rows(result, &["n"]),
            vec![vec![GraphValue::Vertex(VId(10))]]
        );
        assert_eq!(db.frontier().unwrap(), CommitSeq(3));
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn public_return_cancellation_and_bad_tail_never_publish_a_successful_prefix() {
    let ((), report) = run_async_under_lab(0xa071_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        for (index, text) in [
            "MATCH (n:L) SET n.p=11 RETURN n",
            "MERGE (n:L {id:3}) SET n.p=11 RETURN n",
        ]
        .into_iter()
        .enumerate()
        {
            let cancelled =
                cx.with_checkpoint_probe(Arc::new(SimulationCheckpointProbe::new(Some(1))));
            let error = db
                .query_write(
                    &txcx,
                    &cancelled,
                    &commit,
                    text,
                    &GqlParameters::new(),
                    symbols,
                    R,
                    policy(10, 10, 10),
                    no_allocate,
                )
                .await
                .unwrap_err();
            assert!(matches!(
                (index, error),
                (0, QueryWriteError::Mutation(GqlQueryError::Interrupted(_)))
                    | (
                        1,
                        QueryWriteError::VertexUpsert(GqlQueryError::Interrupted(_))
                    )
            ));
            assert_eq!(db.frontier().unwrap(), CommitSeq(1));
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
        // A returning write with a bad tail publishes nothing. An aggregate
        // RETURN on a write is no longer refused here: 834e922b implemented it,
        // and its laws (write_return_aggregates.rs, mutation_returning.rs)
        // state the accepted results.
        for text in [
            "MATCH (n:L) SET n.p=11 RETURN n; CREATE (m)",
            "MERGE (n:L {id:3}) RETURN n; CREATE (m)",
        ] {
            assert!(
                db.query_write(
                    &txcx,
                    &cx,
                    &commit,
                    text,
                    &GqlParameters::new(),
                    symbols,
                    R,
                    policy(10, 10, 10),
                    no_allocate,
                )
                .await
                .is_err(),
                "{text}"
            );
            assert_eq!(db.frontier().unwrap(), CommitSeq(1));
            assert_eq!(db.vertices().unwrap().len(), 2);
            assert_eq!(
                db.vertex(VId(1)).unwrap().unwrap().props[0],
                (P, CanonicalScalar::Int(4).into())
            );
            assert_eq!(
                db.vertex(VId(2)).unwrap().unwrap().props[0],
                (P, CanonicalScalar::Int(7).into())
            );
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
        // Quoted keywords and comments cannot manufacture a RETURN clause.
        let result = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                "MATCH (n:L) SET n.p='RETURN MERGE CREATE' /* RETURN n */",
                &GqlParameters::new(),
                symbols,
                R,
                policy(2, 2, 0),
                no_allocate,
            )
            .await
            .unwrap();
        assert!(matches!(
            result,
            QueryResult::Write {
                completion: Some(EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: CommitSeq(2)
                }),
                ..
            }
        ));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
