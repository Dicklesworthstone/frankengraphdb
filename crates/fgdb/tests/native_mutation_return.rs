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
const K: LabelId = LabelId(2);
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
        (GraphSymbolKind::Label, "K") => Some(GraphSymbol::Label(K)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(ID)),
        _ => None,
    }
}

struct PropertyMapCatalog;
impl fgdb_gql::GraphSymbolResolver for PropertyMapCatalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        symbols(kind, name)
    }

    fn reverse_catalog(&self) -> Option<fgdb_gql::ReverseSymbolCatalog> {
        let mut catalog = fgdb_gql::ReverseSymbolCatalog::new();
        catalog.insert_property(P, "p");
        catalog.insert_property(ID, "id");
        Some(catalog)
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

/// A SET or REMOVE after WITH may target a dynamically typed column, the
/// element of an UNWIND over collected elements (`WITH collect(n) AS ns
/// UNWIND ns AS m SET m.p = ...`, the shape of TCK List12 [1], [2]). Each
/// row writes the vertex or relationship it holds and a NULL element writes
/// nothing. Any other value, or a relationship as a label target, refuses
/// the whole statement typed: no effect is published.
#[test]
fn writes_after_with_target_an_unwound_element() {
    let ((), report) = run_async_under_lab(0xa071_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = seed();
        batch.add_edge(fgdb_types::EId(10), VId(1), VId(2), vec![]);
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
        let id = |db: &Database<_>, vid: VId| {
            let vertex = db.vertex(vid).unwrap().unwrap();
            vertex
                .props
                .iter()
                .find(|(key, _)| *key == ID)
                .map(|(_, value)| value.clone())
        };
        // Every collected vertex is written, with a value the row carries.
        let counted = write(
            &mut db,
            "MATCH (n:L) WITH collect(n) AS ns, count(n) AS c \
             UNWIND ns AS m SET m.id = c * 10 RETURN count(*) AS rows",
        )
        .await
        .unwrap();
        assert_eq!(rows(counted, &["rows"]), vec![vec![int(2)]]);
        for vid in [VId(1), VId(2)] {
            assert_eq!(id(&db, vid), Some(CanonicalScalar::Int(20).into()));
        }
        // A NULL element writes nothing; the vertex beside it still is.
        write(
            &mut db,
            "MATCH (n:L) WITH [n, null] AS ns UNWIND ns AS m SET m.id = 3",
        )
        .await
        .unwrap();
        for vid in [VId(1), VId(2)] {
            assert_eq!(id(&db, vid), Some(CanonicalScalar::Int(3).into()));
        }
        // REMOVE and a label take the same targets.
        write(
            &mut db,
            "MATCH (n:L) WITH collect(n) AS ns UNWIND ns AS m REMOVE m.id SET m:K",
        )
        .await
        .unwrap();
        for vid in [VId(1), VId(2)] {
            let vertex = db.vertex(vid).unwrap().unwrap();
            assert_eq!(id(&db, vid), None);
            assert!(vertex.labels.contains(&K), "{vertex:?}");
        }
        // A collected relationship is a property target too.
        write(
            &mut db,
            "MATCH ()-[r:R]->() WITH collect(r) AS rs UNWIND rs AS e SET e.p = 9",
        )
        .await
        .unwrap();
        let edge = db.edge(fgdb_types::EId(10)).unwrap().unwrap();
        assert!(
            edge.props.contains(&(P, CanonicalScalar::Int(9).into())),
            "{edge:?}"
        );
        // A value that is no element, or a relationship labeled, refuses the
        // statement when it runs, after the vertex rows beside it.
        let frontier = db.frontier().unwrap();
        for refused in [
            "MATCH (n:L) WITH [n, 1] AS ns UNWIND ns AS m SET m.id = 5",
            "MATCH (n:L) WITH [n, {id: 1}] AS ns UNWIND ns AS m REMOVE m.id",
            "MATCH ()-[r:R]->() WITH collect(r) AS rs UNWIND rs AS e SET e:K",
        ] {
            let error = write(&mut db, refused).await.unwrap_err();
            let mut chain = error.to_string();
            let mut cause = error.source();
            while let Some(inner) = cause {
                chain = format!("{chain}: {inner}");
                cause = inner.source();
            }
            assert!(
                chain.contains("not a node or relationship"),
                "{refused}: {chain}"
            );
        }
        assert_eq!(db.frontier().unwrap(), frontier);
        for vid in [VId(1), VId(2)] {
            assert_eq!(id(&db, vid), None);
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-122y1: a WITH tail after a write over an unwound element reads that
/// element's properties after the write: a field the statement wrote reads
/// the write, a removed field is absent, any other field reads its stored
/// value. An element the statement deletes refuses the read rather than
/// answering a removed value, and nothing is published.
#[test]
fn a_write_tail_reads_an_unwound_elements_post_write_properties() {
    let ((), report) = run_async_under_lab(0xa071_0008, |root| async move {
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
        let null = GraphValue::Scalar(CanonicalScalar::Null);
        // The seed's p are 4 and 7.
        let written = write(
            &mut db,
            "MATCH (n:L) WITH collect(n) AS ns UNWIND ns AS m SET m.id = m.p * 10 \
             WITH m RETURN m.p AS p, m.id AS id ORDER BY p",
        )
        .await
        .unwrap();
        assert_eq!(
            rows(written, &["p", "id"]),
            vec![vec![int(4), int(40)], vec![int(7), int(70)]]
        );
        let removed = write(
            &mut db,
            "MATCH (n:L) WITH collect(n) AS ns UNWIND ns AS m REMOVE m.id \
             WITH m RETURN m.p AS p, m.id AS id ORDER BY p",
        )
        .await
        .unwrap();
        assert_eq!(
            rows(removed, &["p", "id"]),
            vec![vec![int(4), null.clone()], vec![int(7), null]]
        );
        // Without a WITH, the RETURN reads the same post-write values, also
        // beside a list the row computed before the write (TCK List12 [1]).
        let direct = write(
            &mut db,
            "MATCH (n:L) WITH collect(n) AS ns UNWIND ns AS m SET m.id = m.p + 1 \
             RETURN m.p AS p, m.id AS id ORDER BY p",
        )
        .await
        .unwrap();
        assert_eq!(
            rows(direct, &["p", "id"]),
            vec![vec![int(4), int(5)], vec![int(7), int(8)]]
        );
        let listed = write(
            &mut db,
            "MATCH (n:L) WITH collect(n) AS ns WITH ns, [x IN ns | x.p] AS old \
             UNWIND ns AS m SET m.p = 0 RETURN m.p AS p, size(old) AS k",
        )
        .await
        .unwrap();
        assert_eq!(
            rows(listed, &["p", "k"]),
            vec![vec![int(0), int(2)], vec![int(0), int(2)]]
        );
        let frontier = db.frontier().unwrap();
        assert!(
            write(
                &mut db,
                "MATCH (n:L) WITH collect(n) AS ns UNWIND ns AS m DETACH DELETE m \
                 WITH m RETURN m.p AS p",
            )
            .await
            .is_err()
        );
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A write reads `x.key` of an element it computes (fgdb-694ga), in a
/// right-hand side and in its input relation, at the statement's frozen cut:
/// the transaction overlay before the statement's own effects. So
/// `SET m.p = m.p + 1` over an unwound element reads each element's old p,
/// and a second statement of a transaction reads the first one's staged
/// write.
#[test]
fn writes_read_properties_of_elements_they_compute() {
    let ((), report) = run_async_under_lab(0xa071_0008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = seed();
        batch.add_edge(
            fgdb_types::EId(10),
            VId(1),
            VId(2),
            vec![(P, CanonicalScalar::Int(5))],
        );
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
            .unwrap()
        };
        let read = |db: &Database<_>, vid: VId, key: PropertyKeyId| {
            let vertex = db.vertex(vid).unwrap().unwrap();
            vertex
                .props
                .iter()
                .find(|(found, _)| *found == key)
                .map(|(_, value)| value.clone())
        };
        let int_prop = |n: i64| {
            Some(fgdb_types::CanonicalPropertyValue::from(
                CanonicalScalar::Int(n),
            ))
        };
        // A right-hand side over an unwound element: p is 4 and 7.
        write(
            &mut db,
            "MATCH (n:L) WITH collect(n) AS ns UNWIND ns AS m SET m.id = m.p * 3",
        )
        .await;
        assert_eq!(read(&db, VId(1), ID), int_prop(12));
        assert_eq!(read(&db, VId(2), ID), int_prop(21));
        // Every element reads its value before the statement's effects.
        write(
            &mut db,
            "MATCH (n:L) WITH collect(n) AS ns UNWIND ns AS m SET m.p = m.p + 1",
        )
        .await;
        assert_eq!(read(&db, VId(1), P), int_prop(5));
        assert_eq!(read(&db, VId(2), P), int_prop(8));
        // The input relation reads elements too, and so does an element a
        // list subscript computes.
        write(
            &mut db,
            "MATCH (n:L) WITH collect(n) AS ns WITH ns, [x IN ns | x.p] AS ps \
             UNWIND ns AS m SET m.id = ps[0] + ps[1]",
        )
        .await;
        assert_eq!(read(&db, VId(1), ID), int_prop(13));
        assert_eq!(read(&db, VId(2), ID), int_prop(13));
        write(
            &mut db,
            "MATCH (a:L) WITH a, [a][0] AS b SET a.id = b.p * 10",
        )
        .await;
        assert_eq!(read(&db, VId(1), ID), int_prop(50));
        assert_eq!(read(&db, VId(2), ID), int_prop(80));
        // A collected relationship's own property.
        write(
            &mut db,
            "MATCH ()-[r:R]->() WITH collect(r) AS rs UNWIND rs AS e SET e.p = e.p * 2",
        )
        .await;
        let edge = db.edge(fgdb_types::EId(10)).unwrap().unwrap();
        assert!(
            edge.props.contains(&(P, CanonicalScalar::Int(10).into())),
            "{edge:?}"
        );
        // A second statement of one transaction reads the first one's write.
        let mut txn = db.begin(&txcx).unwrap();
        for text in [
            "MATCH (n:L) SET n.p = 100",
            "MATCH (n:L) WITH collect(n) AS ns UNWIND ns AS m SET m.id = m.p + 1",
        ] {
            txn.query_write(
                &mut db,
                &cx,
                text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(10, 10, 0),
                no_allocate,
            )
            .unwrap();
        }
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(read(&db, VId(1), ID), int_prop(101));
        assert_eq!(read(&db, VId(2), ID), int_prop(101));
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

/// A write's MATCH WHERE that the graph predicate cannot hold (a label test
/// under NOT or OR) reads as it does in a read: as the row filter of an
/// implicit `WITH *`, exactly as `MATCH ... WITH * WHERE ...` would. It used
/// to refuse with "a label predicate only as a top-level AND conjunct".
/// `MATCH (n) WHERE NOT n:Seen SET n:Seen` is the everyday form.
#[test]
fn a_write_match_where_over_label_tests_filters_its_rows() {
    let ((), report) = run_async_under_lab(0xa071_0009, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = seed();
        batch.create_vertex(VId(3), vec![K], vec![(P, CanonicalScalar::Int(9))]);
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
        let id = |db: &Database<fgdb::MemVfs>, vid: u128| {
            db.vertex(VId(vid))
                .unwrap()
                .unwrap()
                .props
                .into_iter()
                .find(|(key, _)| *key == ID)
                .map(|(_, value)| value)
        };
        // NOT: the two L vertices, not the K one.
        write(&mut db, "MATCH (n) WHERE NOT n:K SET n.id = 0")
            .await
            .unwrap();
        assert_eq!(id(&db, 1), Some(CanonicalScalar::Int(0).into()));
        assert_eq!(id(&db, 2), Some(CanonicalScalar::Int(0).into()));
        assert_eq!(id(&db, 3), None);
        // OR with a comparison, a label write and RETURN.
        let labelled = write(
            &mut db,
            "MATCH (n) WHERE n:K OR n.p = 4 SET n:K RETURN n.p AS p ORDER BY p",
        )
        .await
        .unwrap();
        assert_eq!(rows(labelled, &["p"]), vec![vec![int(4)], vec![int(9)]]);
        assert_eq!(db.vertex(VId(1)).unwrap().unwrap().labels, vec![L, K]);
        // DELETE of the vertices still without K: only vertex 2.
        write(&mut db, "MATCH (n) WHERE NOT n:K DELETE n")
            .await
            .unwrap();
        let mut left: Vec<_> = db.vertices().unwrap().into_iter().map(|v| v.vid).collect();
        left.sort();
        assert_eq!(left, [VId(1), VId(3)]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A driver's record fields key a pattern: `MATCH (n:L {id: $m.id})` and
/// `MERGE (n:L {id: $m.id})` read the field as the constant it is. A key the
/// record lacks is NULL and matches nothing.
#[test]
fn a_map_field_keys_a_write_pattern() {
    let ((), report) = run_async_under_lab(0xa071_000c, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let record = |entries: &[(&str, i64)]| {
            let entries = (entries.iter())
                .map(|(key, value)| ((*key).into(), int(*value)))
                .collect();
            GqlParameters::new().with_map("m", entries).unwrap()
        };
        let set = "MATCH (n:L {id: $m.id}) SET n.p = $m.p RETURN n.p AS p, n.id AS id";
        let merge = "MERGE (n:L {id: $m.id}) ON MATCH SET n.p = $m.p RETURN n.p AS p, n.id AS id";
        for (text, arguments, expected) in [
            (
                set,
                record(&[("id", 1), ("p", 40)]),
                vec![vec![int(40), int(1)]],
            ),
            (
                merge,
                record(&[("id", 2), ("p", 70)]),
                vec![vec![int(70), int(2)]],
            ),
            // `$m.id` reads NULL, which equals nothing.
            (set, record(&[("p", 0)]), vec![]),
        ] {
            let result = db
                .query_write(
                    &txcx,
                    &cx,
                    &commit,
                    text,
                    &arguments,
                    symbols,
                    R,
                    policy(10, 10, 0),
                    no_allocate,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            assert_eq!(rows(result, &["p", "id"]), expected, "{text}");
        }
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(40).into()),
                (ID, CanonicalScalar::Int(1).into())
            ]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Neo4j's batch ingest, `UNWIND $rows AS row CREATE (n:L) SET n += row`,
/// creates one vertex per row holding that row's map. A batch whose rows
/// hold different keys refuses as written, with nothing published.
#[test]
fn an_unwind_batch_creates_each_row_as_its_properties() {
    let ((), report) = run_async_under_lab(0xa071_000d, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let row = |entries: &[(&str, i64)]| {
            let entries = (entries.iter())
                .map(|(key, value)| ((*key).into(), int(*value)))
                .collect();
            GraphValue::map(entries).unwrap()
        };
        let batch = |rows: Vec<GraphValue>| GqlParameters::new().with_list("rows", rows).unwrap();
        let text = "UNWIND $rows AS row CREATE (n:L) SET n += row RETURN n.id AS id, n.p AS p";
        let result = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &batch(vec![
                    row(&[("id", 1), ("p", 10)]),
                    row(&[("id", 2), ("p", 20)]),
                ]),
                symbols,
                R,
                policy(10, 10, 10),
                |request| {
                    let GraphInsertRequest::Vertex { row, .. } = request.request else {
                        panic!("the batch creates only vertices");
                    };
                    let row = u128::try_from(row).unwrap();
                    Ok::<_, Infallible>(ElementId::Vertex(VId(100 + row)))
                },
            )
            .await
            .unwrap();
        assert_eq!(
            rows(result, &["id", "p"]),
            vec![vec![int(1), int(10)], vec![int(2), int(20)]]
        );
        assert_eq!(
            db.vertex(VId(101)).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(20).into()),
                (ID, CanonicalScalar::Int(2).into())
            ]
        );
        let before = (db.frontier().unwrap(), db.vertices().unwrap());
        db.query_write(
            &txcx,
            &cx,
            &commit,
            text,
            &batch(vec![row(&[("id", 3)]), row(&[("p", 30)])]),
            symbols,
            R,
            policy(10, 10, 10),
            no_allocate,
        )
        .await
        .unwrap_err();
        assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-0s2pq: through the embedded API a map parameter stands for the
/// property map it holds. `SET n += $props ... RETURN`, autocommit and inside
/// a transaction, assigns the map's keys and keeps every other property, and
/// `SET n = $props` replaces them. A key the catalog does not bind refuses at
/// the `$` of the parameter, with nothing published.
#[test]
fn a_map_parameter_assigns_its_keys_through_query_write() {
    let ((), report) = run_async_under_lab(0xa071_000a, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let props = |entries: &[(&str, i64)]| {
            let entries = (entries.iter())
                .map(|(key, value)| ((*key).into(), int(*value)))
                .collect();
            GqlParameters::new().with_map("props", entries).unwrap()
        };
        let merge = "MATCH (n:L {id: 1}) SET n += $props RETURN n.p AS p, n.id AS id";
        let merged = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                merge,
                &props(&[("p", 40)]),
                symbols,
                R,
                policy(10, 10, 0),
                no_allocate,
            )
            .await
            .unwrap();
        assert_eq!(rows(merged, &["p", "id"]), vec![vec![int(40), int(1)]]);

        let replace = "MATCH (n:L {id: 2}) SET n = $props RETURN n.p AS p, n.id AS id";
        let replacement = props(&[("id", 2)]);
        let before = (db.frontier().unwrap(), db.vertices().unwrap());
        let mut txn = db.begin(&txcx).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        // Replacement enumerates every current property, as a literal SET
        // map does. A forward-only resolver cannot name the removed fields;
        // that refusal must preserve the transaction for a catalog-backed retry.
        let unmapped = txn
            .query_write(
                &mut db,
                &cx,
                replace,
                &replacement,
                symbols,
                R,
                policy(10, 10, 0),
                no_allocate,
            )
            .unwrap_err();
        assert!(
            matches!(
                &unmapped,
                QueryWriteError::Mutation(GqlQueryError::Source(
                    GraphMutationQueryError::Mutation(fgdb_gql::GraphMutationError::Source(
                        fgdb::WriteTxnError::Read(fgdb::ReadError::UnmappedProperty(P))
                    ))
                ))
            ),
            "{unmapped:?}"
        );
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
        let replaced = txn
            .query_write(
                &mut db,
                &cx,
                replace,
                &replacement,
                PropertyMapCatalog,
                R,
                policy(10, 10, 0),
                no_allocate,
            )
            .unwrap();
        assert_eq!(
            rows(replaced, &["p", "id"]),
            vec![vec![GraphValue::Scalar(CanonicalScalar::Null), int(2)]]
        );
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(
            db.vertex(VId(2)).unwrap().unwrap().props,
            vec![(ID, CanonicalScalar::Int(2).into())]
        );

        let before = db.frontier().unwrap();
        let refused = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                merge,
                &props(&[("zz", 1)]),
                symbols,
                R,
                policy(10, 10, 0),
                no_allocate,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&refused, QueryWriteError::MutationText(error)
                if error.offset == merge.find('$').unwrap()),
            "{refused:?}"
        );
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
