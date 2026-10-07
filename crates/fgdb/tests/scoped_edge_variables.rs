//! Named relationships declared by OPTIONAL MATCH and by a later required
//! MATCH, through `Database::query` and the write-program path (fgdb-o4uen).
//! An OPTIONAL relationship is NULL on a row its clause did not match, like
//! the clause's vertices. Every expected row is enumerated by hand from
//! `graph()`.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, QueryError, QueryResult, QueryValue, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
    PreparedGraphWriteScript,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, TxnCx,
    VId,
};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const NAME: PropertyKeyId = PropertyKeyId(1);
const W: PropertyKeyId = PropertyKeyId(2);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x6a; 32],
        DatabaseSecurityNamespaceId([0x6b; 32]),
        [0x6c; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(NAME)),
        (GraphSymbolKind::Property, "w") => Some(GraphSymbol::Property(W)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 100_000, 100_000_000, 10_000_000)
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn text(value: &str) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap())
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}
fn edge(id: u128) -> GraphValue {
    GraphValue::Edge(EId(id))
}

/// Person a..d (vertices 1..4). R edges: 11 a->b (w 5), 14 a->b (w 6, a
/// parallel edge), 12 a->c (w 7), 13 b->c (w 9). c and d have no R edge.
fn graph() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for (id, name) in [(1_u128, "a"), (2, "b"), (3, "c"), (4, "d")] {
        batch.create_vertex(
            VId(id),
            vec![PERSON],
            vec![(NAME, CanonicalScalar::ucs_basic_text(name).unwrap())],
        );
    }
    for (id, source, destination, w) in [(11, 1, 2, 5), (14, 1, 2, 6), (12, 1, 3, 7), (13, 2, 3, 9)]
    {
        batch.add_edge(
            EId(id),
            VId(source),
            VId(destination),
            vec![(W, CanonicalScalar::Int(w))],
        );
    }
    batch
}

fn run<T>(test: impl AsyncFnOnce(&CommitCx, &QueryCx, &TxnCx) -> T) -> T {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let cx = contexts.query();
    let txn = contexts.txn();
    runtime.block_on(test(&commit, &cx, &txn))
}

fn cells(result: QueryResult) -> Vec<Vec<GraphValue>> {
    let QueryResult::Rows { rows, .. } = result else {
        return vec![vec![text("<not a row result>")]];
    };
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| match cell {
                    QueryValue::Value(value) => value,
                    QueryValue::Count(count) => {
                        i64::try_from(count).map_or_else(|_| text("<count overflow>"), int)
                    }
                    _ => text("<not a plain value>"),
                })
                .collect()
        })
        .collect()
}

/// A bag: the laws compare row multisets, so no ordering policy is assumed.
fn bag(mut rows: Vec<Vec<GraphValue>>) -> Vec<Vec<GraphValue>> {
    rows.sort();
    rows
}

#[test]
fn optional_relationships_project_null_where_the_clause_has_no_witness() {
    run(async |commit, cx, _| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let params = GqlParameters::new();
        let query = |text: &str| {
            bag(cells(
                db.query(cx, text, &params, symbols, policy()).expect(text),
            ))
        };
        // The relationship, its property, its type and its far endpoint.
        assert_eq!(
            query(
                "MATCH (a:Person) OPTIONAL MATCH (a)-[r:R]->(b) \
                 RETURN a.name AS a, r AS r, r.w AS w, type(r) AS t, b.name AS b"
            ),
            bag(vec![
                vec![text("a"), edge(11), int(5), text("R"), text("b")],
                vec![text("a"), edge(14), int(6), text("R"), text("b")],
                vec![text("a"), edge(12), int(7), text("R"), text("c")],
                vec![text("b"), edge(13), int(9), text("R"), text("c")],
                vec![text("c"), null(), null(), null(), null()],
                vec![text("d"), null(), null(), null(), null()],
            ])
        );
        // The clause's WHERE selects its witness; a row without one is kept.
        assert_eq!(
            query(
                "MATCH (a:Person) OPTIONAL MATCH (a)-[r:R]->(b) WHERE r.w > 6 \
                 RETURN a.name AS a, r AS r"
            ),
            bag(vec![
                vec![text("a"), edge(12)],
                vec![text("b"), edge(13)],
                vec![text("c"), null()],
                vec![text("d"), null()],
            ])
        );
        // Two OPTIONAL relationships in one row stay distinct.
        assert_eq!(
            query(
                "MATCH (a:Person) OPTIONAL MATCH (a)-[r:R]->(b) OPTIONAL MATCH (b)-[s:R]->(c) \
                 RETURN a.name AS a, r AS r, s AS s"
            ),
            bag(vec![
                vec![text("a"), edge(11), edge(13)],
                vec![text("a"), edge(14), edge(13)],
                vec![text("a"), edge(12), null()],
                vec![text("b"), edge(13), null()],
                vec![text("c"), null(), null()],
                vec![text("d"), null(), null()],
            ])
        );
        // A later required MATCH exports its relationship and never nulls.
        assert_eq!(
            query("MATCH (a:Person) MATCH (a)-[r:R]->(b) RETURN a.name AS a, r.w AS w"),
            bag(vec![
                vec![text("a"), int(5)],
                vec![text("a"), int(6)],
                vec![text("a"), int(7)],
                vec![text("b"), int(9)],
            ])
        );
        // An EXISTS relationship stays private while an OPTIONAL one exports.
        assert_eq!(
            query(
                "MATCH (a:Person) WHERE EXISTS { MATCH (a)-[x:R]->(q) WHERE x.w > 8 } \
                 OPTIONAL MATCH (a)-[r:R]->(b) RETURN a.name AS a, r AS r"
            ),
            bag(vec![vec![text("b"), edge(13)]])
        );
    });
}

#[test]
fn optional_relationships_aggregate_and_cross_with() {
    run(async |commit, cx, _| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let params = GqlParameters::new();
        let query = |text: &str| {
            bag(cells(
                db.query(cx, text, &params, symbols, policy()).expect(text),
            ))
        };
        // count(r) counts relationships; count(*) counts rows, including the
        // NULL-extended ones.
        assert_eq!(
            query(
                "MATCH (a:Person) OPTIONAL MATCH (a)-[r:R]->(b) \
                 RETURN a.name AS a, count(*) AS rows, count(r) AS edges"
            ),
            bag(vec![
                vec![text("a"), int(3), int(3)],
                vec![text("b"), int(1), int(1)],
                vec![text("c"), int(1), int(0)],
                vec![text("d"), int(1), int(0)],
            ])
        );
        // The relationship crosses WITH as a value, NULL included.
        assert_eq!(
            query(
                "MATCH (a:Person) OPTIONAL MATCH (a)-[r:R]->(b) WITH a, r \
                 RETURN a.name AS a, r AS r"
            ),
            bag(vec![
                vec![text("a"), edge(11)],
                vec![text("a"), edge(14)],
                vec![text("a"), edge(12)],
                vec![text("b"), edge(13)],
                vec![text("c"), null()],
                vec![text("d"), null()],
            ])
        );
        assert_eq!(
            query(
                "MATCH (a:Person) OPTIONAL MATCH (a)-[r:R]->(b) WITH a, r WHERE r IS NULL \
                 RETURN a.name AS a"
            ),
            bag(vec![vec![text("c")], vec![text("d")]])
        );
        assert_eq!(
            query(
                "MATCH (a:Person) OPTIONAL MATCH (a)-[r:R]->(b) WITH a.name AS a, r.w AS w \
                 WHERE w > 6 OR w IS NULL RETURN a, w"
            ),
            bag(vec![
                vec![text("a"), int(7)],
                vec![text("b"), int(9)],
                vec![text("c"), null()],
                vec![text("d"), null()],
            ])
        );
    });
}

#[test]
fn optional_relationship_forms_without_a_null_safe_meaning_refuse() {
    run(async |commit, cx, _| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let params = GqlParameters::new();
        for statement in [
            // startNode(r) would read a's bound endpoint on a NULL-r row.
            "MATCH (a:Person) OPTIONAL MATCH (a)-[r:R]->(b) RETURN startNode(r) AS s",
            "MATCH (a:Person) OPTIONAL MATCH (a)-[r:R]->(b) RETURN endNode(r) AS s",
            // Reusing a name would correlate relationships, which is
            // unsupported, or rebind a relationship as a vertex.
            "MATCH (x)-[r:R]->(y) OPTIONAL MATCH (y)-[r:R]->(z) RETURN x AS x",
            "MATCH (a:Person) OPTIONAL MATCH (a)-[r:R]->(b) MATCH (r)-[:R]->(c) RETURN a AS a",
            // A quantified OPTIONAL edge is not a single relationship.
            "MATCH (a:Person) OPTIONAL MATCH (a)-[r:R*1..2]->(b) RETURN r AS r",
            // EXISTS locals never export.
            "MATCH (a:Person) WHERE EXISTS { MATCH (a)-[x:R]->(q) } RETURN x AS x",
        ] {
            let result = db.query(cx, statement, &params, symbols, policy());
            assert!(
                matches!(result, Err(QueryError::Refused { .. })),
                "{statement}: {result:?}"
            );
        }
    });
}

#[test]
fn writes_through_optional_relationships_skip_null_rows() {
    run(async |commit, cx, txn| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let params = GqlParameters::new();
        for statement in [
            "MATCH (a:Person) OPTIONAL MATCH (a)-[r:R]->(b) WHERE r.w > 6 SET r.w = 0",
            "MATCH (a:Person) OPTIONAL MATCH (a)-[r:R]->(b) WHERE r.w = 5 DELETE r",
        ] {
            let program = PreparedGraphWriteScript::prepare(statement, R, symbols)
                .expect(statement)
                .bind_parameters(&params)
                .expect(statement);
            db.execute_graph_write_program_returning_autocommit_engine_governed(
                txn,
                cx,
                commit,
                &program,
                GraphWriteProgramPolicy::new(policy(), 100_000, 100_000, 100_000),
            )
            .await
            .expect(statement);
        }
        // 12 and 13 were set to 0, 11 was deleted, 14 is untouched; the
        // NULL-extended rows of c and d changed nothing.
        let rows = db
            .query(
                cx,
                "MATCH (x)-[e:R]->(y) RETURN e AS e, e.w AS w",
                &params,
                symbols,
                policy(),
            )
            .unwrap();
        assert_eq!(
            bag(cells(rows)),
            bag(vec![
                vec![edge(12), int(0)],
                vec![edge(13), int(0)],
                vec![edge(14), int(6)],
            ])
        );
    });
}

/// `type(r)` and `labels(n)` as grouping keys and WITH values read catalog
/// names, whichever facade answers, even when the statement spells no
/// relation or label. A `type(r)` key used to group by the edge itself (one
/// row per edge), and `WITH type(r) AS t` used to read NULL.
#[test]
fn type_and_label_keys_group_by_catalog_names() {
    run(async |commit, cx, _| {
        const S: RelationId = RelationId(2);
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let mut other = WriteBatch::new(S);
        other.add_edge(EId(20), VId(3), VId(4), vec![]);
        db.write(commit, other).await.unwrap();
        let bind = || {
            RelationBind::new()
                .with_relation("R", R)
                .with_relation("S", S)
                .with_label("Person", PERSON)
                .with_property("name", NAME)
                .with_property("w", W)
        };
        let params = GqlParameters::new();
        let query = |text: &str| {
            bag(cells(
                db.query(cx, text, &params, bind(), policy()).expect(text),
            ))
        };
        let types = bag(vec![vec![text("R"), int(4)], vec![text("S"), int(1)]]);
        for statement in [
            "MATCH (a)-[r]->(b) RETURN type(r) AS t, count(*) AS c",
            "MATCH (a)-[r]->(b) RETURN type(r), count(*)",
            "MATCH (a)-[r]->(b) WITH type(r) AS t RETURN t, count(*) AS c",
        ] {
            assert_eq!(query(statement), types, "{statement}");
        }
        let person = GraphValue::List(vec![text("Person")].into_boxed_slice());
        for statement in [
            "MATCH (n) RETURN labels(n) AS l, count(*) AS c",
            "MATCH (n) WITH labels(n) AS l RETURN l, count(*) AS c",
        ] {
            assert_eq!(
                query(statement),
                vec![vec![person.clone(), int(4)]],
                "{statement}"
            );
        }
        // type(r) beside r is a second key, not a replacement: one group per edge.
        assert_eq!(
            query("MATCH (a)-[r]->(b) RETURN type(r) AS t, r AS r, count(*) AS c").len(),
            5
        );
    });
}
