//! openCypher map values through `Database::query` (fgdb-2jw3z): map
//! literals, `m.key`, `keys(m)`, maps over aggregate outputs, the map's place
//! in value order, and map rows driving CREATE (`UNWIND $rows AS row CREATE
//! (:Person {name: row.name})`). Every expectation is written out by hand.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, QueryError, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
    PreparedGraphWriteScript,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts, QueryCx, TxnCx, VId,
};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const NAME: PropertyKeyId = PropertyKeyId(2);
const FIRST: PropertyKeyId = PropertyKeyId(3);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x5b; 32]),
        [0x5c; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Label, "Tag") => Some(GraphSymbol::Label(LabelId(2))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(NAME)),
        (GraphSymbolKind::Property, "first") => Some(GraphSymbol::Property(FIRST)),
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
fn list(values: impl IntoIterator<Item = GraphValue>) -> GraphValue {
    GraphValue::List(values.into_iter().collect())
}
/// Built independently of the engine: keys as written, sorted here.
fn map(entries: &[(&str, GraphValue)]) -> GraphValue {
    GraphValue::map(
        entries
            .iter()
            .map(|(key, value)| ((*key).into(), value.clone()))
            .collect(),
    )
    .unwrap()
}

/// Person vertices 1..=3: p = 10, 20, 30 and name = "a", "b", "c".
fn graph() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for (id, name) in [(1_i64, "a"), (2, "b"), (3, "c")] {
        batch.create_vertex(
            VId(id as u128),
            vec![PERSON],
            vec![
                (P, CanonicalScalar::Int(id * 10)),
                (NAME, CanonicalScalar::ucs_basic_text(name).unwrap()),
            ],
        );
    }
    batch
}

fn run<T>(test: impl AsyncFnOnce(&fgdb_types::CommitCx, &QueryCx) -> T) -> T {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let cx = contexts.query();
    runtime.block_on(test(&commit, &cx))
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
                    _ => text("<not a plain value>"),
                })
                .collect()
        })
        .collect()
}

#[test]
fn map_literals_entries_and_keys_follow_opencypher() {
    run(async |commit, cx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let params = GqlParameters::new();
        let one = |text: &str| {
            cells(db.query(cx, text, &params, symbols, policy()).expect(text))
                .into_iter()
                .next()
                .and_then(|row| row.into_iter().next())
        };
        for (statement, expected) in [
            // Keys are canonical: written order does not matter.
            (
                "RETURN {b: 2, a: 'x'} AS m",
                map(&[("a", text("x")), ("b", int(2))]),
            ),
            ("RETURN {} AS m", map(&[])),
            (
                "RETURN {outer: {inner: [1, null]}} AS m",
                map(&[("outer", map(&[("inner", list([int(1), null()]))]))]),
            ),
            (
                "RETURN [{k: 1}, {k: 2}] AS l",
                list([map(&[("k", int(1))]), map(&[("k", int(2))])]),
            ),
            // m.key reads an entry; an absent key is NULL.
            ("WITH {a: 1, b: 'two'} AS m RETURN m.b AS v", text("two")),
            ("WITH {a: 1} AS m RETURN m.missing AS v", null()),
            (
                "WITH {outer: {inner: 7}} AS m RETURN m.outer.inner AS v",
                int(7),
            ),
            ("WITH {l: [5, 6]} AS m RETURN m.l[1] AS v", int(6)),
            // keys() lists the keys ascending.
            (
                "RETURN keys({zeta: 1, alpha: 2}) AS k",
                list([text("alpha"), text("zeta")]),
            ),
            ("RETURN keys({}) AS k", list([])),
            ("RETURN keys(null) AS k", null()),
            // A NULL map reads NULL. (The column must be dynamically typed:
            // a statically scalar column's `.a` refuses at preparation.)
            ("UNWIND [null] AS m RETURN m.a AS v", null()),
            ("UNWIND [{a: 4}] AS m RETURN m.a AS v", int(4)),
            // Backtick-delimited keys.
            (
                "RETURN {`first name`: 'Ada'} AS m",
                map(&[("first name", text("Ada"))]),
            ),
        ] {
            assert_eq!(one(statement), Some(expected), "{statement}");
        }
    });
}

#[test]
fn maps_shape_graph_rows_and_aggregate_outputs() {
    run(async |commit, cx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let params = GqlParameters::new();
        let query = |text: &str| cells(db.query(cx, text, &params, symbols, policy()).expect(text));
        assert_eq!(
            query("MATCH (n:Person) RETURN {name: n.name, p: n.p} AS person ORDER BY person"),
            [("a", 10), ("b", 20), ("c", 30)]
                .iter()
                .map(|(name, p)| vec![map(&[("name", text(name)), ("p", int(*p))])])
                .collect::<Vec<_>>()
        );
        // Over aggregate outputs.
        assert_eq!(
            query("MATCH (n:Person) RETURN {people: count(*), total: sum(n.p)} AS summary"),
            vec![vec![map(&[
                ("people", GraphValue::Scalar(CanonicalScalar::Int(3))),
                ("total", int(60)),
            ])]]
        );
        // An entry of a map built over aggregate outputs.
        assert_eq!(
            query("MATCH (n:Person) RETURN {total: sum(n.p)}.total AS v"),
            vec![vec![int(60)]]
        );
        // Maps order by their canonical key list, then values: DISTINCT
        // collapses equal maps whatever order their literals wrote.
        assert_eq!(
            query("UNWIND [2, 1, 2] AS x RETURN DISTINCT {k: x} AS m ORDER BY m"),
            vec![vec![map(&[("k", int(1))])], vec![map(&[("k", int(2))])]]
        );
    });
}

#[test]
fn malformed_map_forms_refuse_typed() {
    run(async |commit, cx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let params = GqlParameters::new();
        // A repeated key is refused at preparation, never resolved silently.
        let result = db.query(cx, "RETURN {a: 1, a: 2} AS m", &params, symbols, policy());
        assert!(
            matches!(result, Err(QueryError::Refused { .. })),
            "{result:?}"
        );
        // Reading an entry of, or listing the keys of, a non-map is a typed
        // error, never a silent NULL.
        // A statically scalar column is not a map: refused at preparation.
        let result = db.query(
            cx,
            "WITH 3 AS m RETURN m.a AS v",
            &params,
            symbols,
            policy(),
        );
        assert!(
            matches!(result, Err(QueryError::Refused { .. })),
            "{result:?}"
        );
        for statement in [
            "UNWIND [3] AS m RETURN m.a AS v",
            "RETURN keys([1, 2]) AS k",
            "MATCH (n:Person) WHERE n.p = 10 RETURN keys(n) AS k",
        ] {
            let result = db.query(cx, statement, &params, symbols, policy());
            assert!(result.is_err(), "{statement}: {result:?}");
        }
    });
}

/// Run one GQL write program through the engine write path, as the CLI does.
async fn write(
    db: &mut Database<fgdb::MemVfs>,
    (commit, cx, txn): (&CommitCx, &QueryCx, &TxnCx),
    statement: &str,
    params: &GqlParameters,
) -> Result<(), String> {
    let program = PreparedGraphWriteScript::prepare(statement, R, symbols)
        .map_err(|error| format!("prepare: {error:?}"))?
        .bind_parameters(params)
        .map_err(|error| format!("bind: {error:?}"))?;
    db.execute_graph_write_program_returning_autocommit_engine_governed(
        txn,
        cx,
        commit,
        &program,
        GraphWriteProgramPolicy::new(policy(), 100_000, 100_000, 100_000),
    )
    .await
    .map(|_| ())
    .map_err(|error| format!("execute: {error:?}"))
}

/// The openCypher bulk idiom: each map row of a list parameter creates one
/// vertex whose properties are entries of that row. An absent entry reads
/// NULL, which stores no property. A list index, size() and IN over a row
/// are scalar property values too. A row whose entry is a list or map is a
/// typed refusal that commits nothing.
#[test]
fn map_rows_drive_create_properties() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let (commit, cx, txn) = (contexts.commit(), contexts.query(), contexts.txn());
    runtime.block_on(async {
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let cxs = (&commit, &cx, &txn);
        let rows = vec![
            map(&[("name", text("x")), ("p", int(1))]),
            map(&[("name", text("y"))]),
            map(&[
                ("name", text("z")),
                ("p", int(3)),
                ("tags", list([text("t")])),
            ]),
        ];
        let params = GqlParameters::new().with_list("rows", rows).unwrap();
        write(
            &mut db,
            cxs,
            "UNWIND $rows AS row CREATE (:Person {name: row.name, p: row.p})",
            &params,
        )
        .await
        .unwrap();
        let empty = GqlParameters::new();
        let query = |db: &Database<fgdb::MemVfs>, text: &str| {
            let mut rows = cells(db.query(&cx, text, &empty, symbols, policy()).expect(text));
            rows.sort();
            rows
        };
        let people = "MATCH (n:Person) RETURN n.name AS name, n.p AS p";
        let mut expected = vec![
            vec![text("x"), int(1)],
            vec![text("y"), null()],
            vec![text("z"), int(3)],
        ];
        expected.sort();
        assert_eq!(query(&db, people), expected);
        // A list index, size() and IN read a row's list entry.
        write(
            &mut db,
            cxs,
            "UNWIND [{name: 'w', tags: ['a', 'b']}, {name: 'v', tags: []}] AS row \
             CREATE (:Person {name: row.name, p: size(row.tags), first: row.tags[0]})",
            &empty,
        )
        .await
        .unwrap();
        let mut expected = vec![
            vec![text("v"), int(0)],
            vec![text("w"), int(2)],
            vec![text("x"), int(1)],
            vec![text("y"), null()],
            vec![text("z"), int(3)],
        ];
        expected.sort();
        assert_eq!(query(&db, people), expected);
        // A parameter inside a composite value binds to the argument row.
        let k = GqlParameters::new().with_int64("k", 42).unwrap();
        write(
            &mut db,
            cxs,
            "UNWIND [{name: 'u'}] AS row CREATE (:Tag {name: row.name, first: [$k, 0][0]})",
            &k,
        )
        .await
        .unwrap();
        // tags[0] of an empty list is NULL, which stores no property.
        assert_eq!(
            query(
                &db,
                "MATCH (n) WHERE n.first IS NOT NULL RETURN n.name AS name, n.first AS first"
            ),
            vec![vec![text("u"), int(42)], vec![text("w"), text("a")]]
        );
        // A list or map entry is not a property value, and a non-map row has
        // no entries: each refuses typed, and the whole program commits
        // nothing, including its earlier rows.
        for (statement, phase) in [
            // Per row, at execution: the value is known only then.
            (
                "UNWIND [{name: 'ok'}, {name: ['a']}] AS row CREATE (:Person {name: row.name})",
                "execute:",
            ),
            (
                "UNWIND [{name: 'ok'}, {name: {a: 1}}] AS row CREATE (:Person {name: row.name})",
                "execute:",
            ),
            (
                "UNWIND [{name: 'ok'}, 3] AS row CREATE (:Person {name: row.name})",
                "execute:",
            ),
            (
                "UNWIND [{name: 'ok'}] AS row CREATE (:Person {name: row})",
                "execute:",
            ),
            // At preparation: a map or a key list is never scalar.
            ("CREATE (:Person {name: {a: 1}})", "prepare:"),
            (
                "UNWIND [{name: 'ok'}] AS row CREATE (:Person {name: keys(row)})",
                "prepare:",
            ),
        ] {
            let result = write(&mut db, cxs, statement, &empty).await;
            assert!(
                result.as_ref().is_err_and(|error| error.starts_with(phase)),
                "{statement}: {result:?}"
            );
        }
        assert_eq!(query(&db, people), expected);
    });
}

/// openCypher map projection (fgdb-20foe): `n{.a}` reads a property (NULL
/// when absent), `k: e` is any value, `v` is a binding. The source guards
/// the map: a NULL n from OPTIONAL MATCH projects NULL, never `{a: NULL}`.
#[test]
fn map_projection_reads_properties_and_is_null_for_a_null_source() {
    run(async |commit, cx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let mut batch = graph();
        // A fourth person with a name and no p, and one R edge 1 -> 2.
        batch.create_vertex(
            VId(4),
            vec![PERSON],
            vec![(NAME, CanonicalScalar::ucs_basic_text("d").unwrap())],
        );
        batch.add_edge(
            fgdb_types::EId(11),
            VId(1),
            VId(2),
            vec![(P, CanonicalScalar::Int(5))],
        );
        db.write(commit, batch).await.unwrap();
        let params = GqlParameters::new();
        let query = |text: &str| {
            let mut rows = cells(db.query(cx, text, &params, symbols, policy()).expect(text));
            rows.sort();
            rows
        };
        let sorted = |mut rows: Vec<Vec<GraphValue>>| {
            rows.sort();
            rows
        };
        assert_eq!(
            query("MATCH (n:Person) RETURN n{.name, .p} AS m"),
            sorted(vec![
                vec![map(&[("name", text("a")), ("p", int(10))])],
                vec![map(&[("name", text("b")), ("p", int(20))])],
                vec![map(&[("name", text("c")), ("p", int(30))])],
                vec![map(&[("name", text("d")), ("p", null())])],
            ])
        );
        assert_eq!(
            query(
                "MATCH (a:Person) OPTIONAL MATCH (a)-[r:R]->(b) \
                 RETURN a.name AS a, b{.name} AS b, r{.p} AS r"
            ),
            sorted(vec![
                vec![
                    text("a"),
                    map(&[("name", text("b"))]),
                    map(&[("p", int(5))])
                ],
                vec![text("b"), null(), null()],
                vec![text("c"), null(), null()],
                vec![text("d"), null(), null()],
            ])
        );
        assert_eq!(
            query("MATCH (a:Person)-[r:R]->(b) RETURN a{.name, target: b.name, r} AS m"),
            vec![vec![map(&[
                ("name", text("a")),
                ("r", GraphValue::Edge(fgdb_types::EId(11))),
                ("target", text("b")),
            ])]]
        );
        for statement in [
            // `.*` needs the complete property catalog.
            "MATCH (n:Person) RETURN n{.*} AS m",
            // Keys are unique, as in a map literal.
            "MATCH (n:Person) RETURN n{.name, .name} AS m",
            // A shorthand entry names a binding.
            "MATCH (n:Person) RETURN n{nope} AS m",
        ] {
            let result = db.query(cx, statement, &params, symbols, policy());
            assert!(
                matches!(result, Err(QueryError::Refused { .. })),
                "{statement}: {result:?}"
            );
        }
    });
}
