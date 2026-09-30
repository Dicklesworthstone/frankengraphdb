//! openCypher map values through `Database::query` (fgdb-2jw3z): map
//! literals, `m.key`, `keys(m)`, maps over aggregate outputs, and the map's
//! place in value order. Every expectation is written out by hand.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, QueryError, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, QueryCx, VId};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const NAME: PropertyKeyId = PropertyKeyId(2);

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
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(NAME)),
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
