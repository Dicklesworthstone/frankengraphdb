//! openCypher map values through `Database::query` (fgdb-2jw3z): map
//! literals, `m.key`, `keys(m)`, maps over aggregate outputs, the map's place
//! in value order, and map rows driving CREATE (`UNWIND $rows AS row CREATE
//! (:Person {name: row.name})`). Every expectation is written out by hand.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, QueryError, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    GraphWriteProgramPolicy, PreparedGraphWriteScript,
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
        (GraphSymbolKind::Label, "Listed") => Some(GraphSymbol::Label(LabelId(3))),
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

fn map_parameters(entries: &[(&str, GraphValue)]) -> GqlParameters {
    GqlParameters::new()
        .with_map(
            "payload",
            entries
                .iter()
                .map(|(key, value)| ((*key).into(), value.clone()))
                .collect(),
        )
        .unwrap()
}

#[test]
fn top_level_map_parameters_compose_with_graph_rows_unwind_and_grouped_outputs() {
    run(async |commit, cx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let fields = [
            ("title", text("a quote ' and $syntax are data")),
            ("nested", map(&[("answer", int(42))])),
            (
                "rows",
                list([map(&[("value", int(7))]), map(&[("value", int(9))])]),
            ),
        ];
        let params = map_parameters(&fields);
        for (statement, expected) in [
            ("RETURN $payload AS value", vec![vec![map(&fields)]]),
            (
                "RETURN $payload.nested.answer AS value, $payload.missing AS absent",
                vec![vec![int(42), null()]],
            ),
            (
                "WITH $payload AS m RETURN m.title AS title, keys(m) AS keys",
                vec![vec![
                    text("a quote ' and $syntax are data"),
                    list([text("nested"), text("rows"), text("title")]),
                ]],
            ),
            (
                "UNWIND $payload.rows AS row RETURN row.value AS value",
                vec![vec![int(7)], vec![int(9)]],
            ),
            (
                "MATCH (n:Person) RETURN n.name AS name, $payload.nested.answer AS answer ORDER BY name",
                vec![
                    vec![text("a"), int(42)],
                    vec![text("b"), int(42)],
                    vec![text("c"), int(42)],
                ],
            ),
            (
                "MATCH (n:Person) WITH count(*) AS n RETURN $payload.nested.answer AS answer",
                vec![vec![int(42)]],
            ),
        ] {
            assert_eq!(
                cells(
                    db.query(cx, statement, &params, symbols, policy())
                        .expect(statement)
                ),
                expected,
                "{statement}"
            );
        }
        // Map admission grants no scalar property coercion or numeric role.
        for statement in [
            "MATCH (n:Person) WHERE n.p = $payload RETURN n",
            "RETURN $payload + 1 AS n",
            "RETURN 1 AS n LIMIT $payload",
        ] {
            assert!(
                db.query(cx, statement, &params, symbols, policy()).is_err(),
                "{statement}"
            );
        }
    });
}

#[test]
fn map_parameter_certificate_replays_canonical_order_and_rejects_value_changes() {
    run(async |commit, cx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let args = map_parameters(&[("z", int(5)), ("a", int(2))]);
        let query = "MATCH (n:Person) RETURN n.name AS name, $payload AS payload ORDER BY name";
        let (result, certificate) = db
            .execute_certified(cx, query, &args, symbols, policy())
            .unwrap();
        let expected = cells(result);
        let reordered = map_parameters(&[("a", int(2)), ("z", int(5))]);
        assert_eq!(args.canonical_bytes(), reordered.canonical_bytes());
        let mut extra = WriteBatch::new(R);
        extra.create_vertex(
            VId(4),
            vec![PERSON],
            vec![(NAME, CanonicalScalar::ucs_basic_text("later").unwrap())],
        );
        db.write(commit, extra).await.unwrap();
        assert_eq!(
            cells(
                db.replay(cx, &certificate, &reordered, symbols, policy())
                    .unwrap()
            ),
            expected
        );
        let changed = map_parameters(&[("a", int(3)), ("z", int(5))]);
        assert!(matches!(
            db.replay(cx, &certificate, &changed, symbols, policy()),
            Err(fgdb::ReplayRefusal::ParameterValuesMismatch)
        ));
    });
}

#[test]
fn map_parameters_drive_atomic_create_and_return_from_nested_bulk_input() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let (commit, cx, txn) = (contexts.commit(), contexts.query(), contexts.txn());
    runtime.block_on(async {
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let arguments = map_parameters(&[
            ("tag", text("batch")),
            ("rows", list([map(&[("name", text("a")), ("p", int(1))]), map(&[("name", text("b"))])])),
        ]);
        let statement = "UNWIND $payload.rows AS row CREATE (n:Person {name: row.name, p: row.p, first: $payload.tag}) RETURN n.name AS name, n.p AS p, $payload.tag AS tag ORDER BY name";
        let write_policy = GraphWriteProgramPolicy::new(policy(), 100_000, 100_000, 100_000);
        let result = db.query_write_engine(&txn, &cx, &commit, statement, &arguments, symbols, R, write_policy).await.unwrap();
        assert_eq!(cells(result), vec![vec![text("a"), int(1), text("batch")], vec![text("b"), null(), text("batch")]]);
        let before = db.frontier().unwrap();
        let invalid = map_parameters(&[
            ("tag", text("refused")),
            ("rows", list([map(&[("name", text("must not commit"))]), map(&[("name", map(&[("nested", int(1))]))])])),
        ]);
        assert!(db.query_write_engine(&txn, &cx, &commit, statement, &invalid, symbols, R,
            GraphWriteProgramPolicy::new(policy(), 100_000, 100_000, 100_000)).await.is_err());
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(cells(db.query(&cx, "MATCH (n:Person) RETURN n.name AS name ORDER BY name", &GqlParameters::new(), symbols, policy()).unwrap()), vec![vec![text("a")], vec![text("b")]]);
        // The no-RETURN facade must propagate Map declarations as well.
        db.query_write_engine(&txn, &cx, &commit, "CREATE (:Person {name:$payload.tag})", &arguments, symbols, R,
            GraphWriteProgramPolicy::new(policy(), 100_000, 100_000, 100_000)).await.unwrap();
    });
}

#[test]
fn map_parameter_fields_drive_scalar_graph_predicates_arithmetic_and_aggregates() {
    run(async |commit, cx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let params = map_parameters(&[
            ("threshold", map(&[("value", int(20))])),
            ("step", int(5)),
            ("name", text("a")),
            ("metadata", map(&[("title", text("kept as a map"))])),
        ]);
        for (statement, expected) in [
            (
                "MATCH (n:Person) WHERE n.p >= $payload.threshold.value RETURN n.name AS name, n.p + $payload.step AS next ORDER BY name",
                vec![vec![text("b"), int(25)], vec![text("c"), int(35)]],
            ),
            (
                "MATCH (n:Person) WHERE n.name = $payload.name OR n.p >= $payload.threshold.value AND n.p < 30 RETURN n.name AS name ORDER BY name",
                vec![vec![text("a")], vec![text("b")]],
            ),
            (
                "MATCH (n:Person) WHERE n.p = $payload.missing RETURN n.name AS name",
                Vec::new(),
            ),
            (
                "MATCH (n:Person) WHERE $payload.missing IS NULL RETURN n.name AS name ORDER BY name",
                vec![vec![text("a")], vec![text("b")], vec![text("c")]],
            ),
            (
                "RETURN $payload.metadata AS metadata, upper($payload.metadata.title) AS title",
                vec![vec![
                    map(&[("title", text("kept as a map"))]),
                    text("KEPT AS A MAP"),
                ]],
            ),
        ] {
            assert_eq!(
                cells(
                    db.query(cx, statement, &params, symbols, policy())
                        .expect(statement)
                ),
                expected,
                "{statement}"
            );
        }
        let aggregate = db.query(cx, "MATCH (n:Person) RETURN sum(n.p + $payload.step) AS a, sum(n.p + $payload.threshold.value) AS b", &params, symbols, policy()).unwrap();
        let QueryResult::Rows { rows, .. } = aggregate else {
            panic!("aggregate returns rows");
        };
        assert!(
            matches!(rows.as_slice(), [row] if matches!(row.as_slice(), [QueryValue::Integer(75), QueryValue::Integer(120)])),
            "{rows:?}"
        );
    });
}

#[test]
fn map_field_set_and_merge_rhs_use_native_atomic_write_programs() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let (commit, cx, txn) = (contexts.commit(), contexts.query(), contexts.txn());
    runtime.block_on(async {
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, graph()).await.unwrap();
        let params = map_parameters(&[
            ("selected", text("b")), ("increment", int(5)), ("next_name", text("beta")),
            ("seed", int(100)), ("tag", text("bound map field")),
        ]);
        let writes = || GraphWriteProgramPolicy::new(policy(), 100_000, 100_000, 100_000);
        db.query_write_engine(&txn, &cx, &commit,
            "MATCH (n:Person) WHERE n.name=$payload.selected SET n.p=n.p+$payload.increment, n.name=upper($payload.next_name), n.first=$payload.absent",
            &params, symbols, R, writes()).await.unwrap();
        assert_eq!(cells(db.query(&cx, "MATCH (n:Person) WHERE n.name='BETA' RETURN n.p AS p, n.first AS first", &GqlParameters::new(), symbols, policy()).unwrap()), vec![vec![int(25), null()]]);
        let merge = "MERGE (n:Person {name:'new'}) ON CREATE SET n.p=$payload.seed ON MATCH SET n.p=n.p+$payload.increment SET n.first=$payload.tag";
        for _ in 0..2 {
            db.query_write_engine(&txn, &cx, &commit, merge, &params, symbols, R, writes()).await.unwrap();
        }
        assert_eq!(cells(db.query(&cx, "MATCH (n:Person) WHERE n.name='new' RETURN n.p AS p, n.first AS first", &GqlParameters::new(), symbols, policy()).unwrap()), vec![vec![int(105), text("bound map field")]]);
        let before = db.frontier().unwrap();
        for bad in [
            map_parameters(&[("value", map(&[("private", int(3))]))]),
            map_parameters(&[("value", GraphValue::Scalar(CanonicalScalar::Bool(true)))]),
        ] {
            // The first statement must never escape a later binding failure.
            assert!(db.query_write_engine(&txn, &cx, &commit,
                "MATCH (n:Person) WHERE n.name='a' SET n.p=999; MATCH (n:Person) WHERE n.name='new' SET n.p=n.p+$payload.value",
                &bad, symbols, R, writes()).await.is_err());
            assert_eq!(db.frontier().unwrap(), before);
        }
        assert_eq!(cells(db.query(&cx, "MATCH (n:Person) WHERE n.name='a' RETURN n.p AS p", &GqlParameters::new(), symbols, policy()).unwrap()), vec![vec![int(10)]]);
    });
}

#[test]
fn map_scalar_field_certificates_replay_field_values_and_pinned_graph_state() {
    run(async |commit, cx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let params = map_parameters(&[("floor", int(20)), ("delta", int(1))]);
        let (result, certificate) = db.execute_certified(cx,
            "MATCH (n:Person) WHERE n.p >= $payload.floor RETURN n.p + $payload.delta AS p ORDER BY p",
            &params, symbols, policy()).unwrap();
        assert_eq!(cells(result), vec![vec![int(21)], vec![int(31)]]);
        let reordered = map_parameters(&[("delta", int(1)), ("floor", int(20))]);
        let mut change = WriteBatch::new(R);
        change.create_vertex(VId(4), vec![PERSON], vec![(P, CanonicalScalar::Int(40))]);
        db.write(commit, change).await.unwrap();
        assert_eq!(
            cells(
                db.replay(cx, &certificate, &reordered, symbols, policy())
                    .unwrap()
            ),
            vec![vec![int(21)], vec![int(31)]]
        );
        let wrong = map_parameters(&[("delta", int(2)), ("floor", int(20))]);
        assert!(matches!(
            db.replay(cx, &certificate, &wrong, symbols, policy()),
            Err(fgdb::ReplayRefusal::ParameterValuesMismatch)
        ));
    });
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
            // Entries are operands of scalar operators (fgdb-o5o9t).
            ("WITH {a: 1, b: true} AS m RETURN m.a + 1 AS v", int(2)),
            (
                "WITH {a: 3, b: true} AS m RETURN CASE WHEN m.b = true THEN m.a * 10 ELSE 0 END AS v",
                int(30),
            ),
            ("WITH {a: 1} AS m RETURN m.missing + 1 AS v", null()),
            // A list element shadows a same-named row column.
            (
                "WITH {a: 1} AS m RETURN [m IN [{a: 5}] | m.a + 1] AS v",
                list([int(6)]),
            ),
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
        // A map entry is not a property value, and a non-map row has no
        // entries: each refuses typed, and the whole program commits nothing,
        // including its earlier rows. (A list entry and keys() are stored
        // Lists since fgdb-k3nub, below.)
        for (statement, phase) in [
            // Per row, at execution: the value is known only then.
            (
                "UNWIND [{name: 'ok'}, {name: {a: 1}}] AS row CREATE (:Person {name: row.name})",
                "execute:",
            ),
            (
                "UNWIND [{name: 'ok'}, {name: [{a: 1}]}] AS row CREATE (:Person {name: row.name})",
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
            // At preparation: a map literal is never a written value.
            ("CREATE (:Person {name: {a: 1}})", "prepare:"),
        ] {
            let result = write(&mut db, cxs, statement, &empty).await;
            assert!(
                result.as_ref().is_err_and(|error| error.starts_with(phase)),
                "{statement}: {result:?}"
            );
        }
        assert_eq!(query(&db, people), expected);
        // fgdb-k3nub: a list-valued entry and keys() of a map row are written
        // as stored Lists and read back as the same lists.
        for statement in [
            "UNWIND [{name: ['a', 'b']}] AS row CREATE (:Listed {name: row.name})",
            "UNWIND [{name: 'ok'}] AS row CREATE (:Listed {name: keys(row)})",
        ] {
            write(&mut db, cxs, statement, &empty).await.unwrap();
        }
        let mut listed = query(&db, "MATCH (n:Listed) RETURN n.name AS name");
        listed.sort();
        let mut wanted = vec![
            vec![list([text("a"), text("b")])],
            vec![list([text("name")])],
        ];
        wanted.sort();
        assert_eq!(listed, wanted);
        // fgdb-kr8j1: a set operand's property column carries the stored
        // Lists, and UNION deduplicates them by canonical value. Paired with
        // a list literal column it widens, and keeps every stored List.
        let mut union = query(
            &db,
            "MATCH (n:Listed) RETURN n.name AS name UNION MATCH (m:Listed) RETURN m.name AS name",
        );
        union.sort();
        assert_eq!(union, wanted);
        let mut widened = query(
            &db,
            "MATCH (n:Listed) RETURN n.name AS name UNION MATCH (m:Listed) RETURN ['z'] AS name",
        );
        widened.sort();
        let mut wanted_widened = wanted.clone();
        wanted_widened.push(vec![list([text("z")])]);
        wanted_widened.sort();
        assert_eq!(widened, wanted_widened);
        // fgdb-bqri3: a pattern WHERE whose scalar program meets a stored List
        // raises. It never reads the row as UNKNOWN, which silently dropped
        // every listed vertex (count 0 for size(n.name) > 1). In RETURN and
        // after WITH the value language reads the list.
        for statement in [
            "MATCH (n:Listed) WHERE size(n.name) > 1 RETURN count(*) AS c",
            "MATCH (n:Listed) WHERE size(n.name) > 1 RETURN n.name AS name",
        ] {
            assert!(
                db.query(&cx, statement, &empty, symbols, policy()).is_err(),
                "{statement}"
            );
        }
        let boolean = |value| GraphValue::Scalar(CanonicalScalar::Bool(value));
        let mut longer = query(&db, "MATCH (n:Listed) RETURN size(n.name) > 1 AS longer");
        longer.sort();
        let mut wanted_longer = vec![vec![boolean(true)], vec![boolean(false)]];
        wanted_longer.sort();
        assert_eq!(longer, wanted_longer);
        assert_eq!(
            query(
                &db,
                "MATCH (n:Listed) WITH n WHERE size(n.name) > 1 RETURN n.name AS name"
            ),
            vec![vec![list([text("a"), text("b")])]]
        );
        // SET from a property column that holds a stored List copies the List
        // (it panicked the process, "complete input schema checked"). Here two
        // listed rows assign different Lists to one tag: a conflicting
        // assignment at execution, so nothing commits.
        let copied = "MATCH (n:Listed), (m:Tag) SET m.first = n.name";
        let result = write(&mut db, cxs, copied, &empty).await;
        assert!(
            result
                .as_ref()
                .is_err_and(|error| error.starts_with("execute:")),
            "{copied}: {result:?}"
        );
        assert_eq!(
            query(&db, "MATCH (m:Tag) RETURN m.first AS first"),
            vec![vec![int(42)]]
        );
    });
}

/// fgdb-k3nub, the write half: SET writes a List value as the stored List it
/// is. A literal, a concatenation, a copy of another element's stored List
/// and a `+=` entry all read back exactly. A map at any depth refuses typed,
/// as CREATE's does, and two rows assigning different Lists to one field
/// conflict. Each refusal commits nothing.
#[test]
fn set_writes_list_values_as_stored_lists() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let (commit, cx, txn) = (contexts.commit(), contexts.query(), contexts.txn());
    runtime.block_on(async {
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, graph()).await.unwrap();
        let cxs = (&commit, &cx, &txn);
        let empty = GqlParameters::new();
        let first = |db: &Database<fgdb::MemVfs>| {
            let text = "MATCH (n:Person) RETURN n.name AS name, n.first AS first";
            let mut rows = cells(db.query(&cx, text, &empty, symbols, policy()).expect(text));
            rows.sort();
            rows
        };
        for statement in [
            "MATCH (n:Person {name: 'a'}) SET n.first = [1, 2, 3]",
            "MATCH (n:Person {name: 'a'}) SET n.first = n.first + [4]",
            "MATCH (a:Person {name: 'a'}), (b:Person {name: 'b'}) SET b.first = a.first",
            "MATCH (n:Person {name: 'c'}) SET n += {first: ['x', 'y']}",
        ] {
            write(&mut db, cxs, statement, &empty)
                .await
                .expect(statement);
        }
        let numbers = list([int(1), int(2), int(3), int(4)]);
        let expected = vec![
            vec![text("a"), numbers.clone()],
            vec![text("b"), numbers],
            vec![text("c"), list([text("x"), text("y")])],
        ];
        assert_eq!(first(&db), expected);
        for statement in [
            "MATCH (n:Person {name: 'a'}) SET n.first = [{k: 1}]",
            "MATCH (n:Person {name: 'a'}) SET n += {first: [{k: 1}]}",
            "MATCH (a:Person), (b:Person {name: 'c'}) SET b.first = [a.p]",
        ] {
            let result = write(&mut db, cxs, statement, &empty).await;
            assert!(
                result
                    .as_ref()
                    .is_err_and(|error| error.starts_with("execute:")),
                "{statement}: {result:?}"
            );
        }
        assert_eq!(first(&db), expected);
        // A parameter the statement declares as a List writes the same way:
        // `SET n.p = $tags`, the parameterized form of a list write.
        let tags = GqlParameters::new()
            .with_list("tags", vec![text("p"), text("q")])
            .unwrap();
        let program = PreparedGraphWriteScript::prepare_with_parameter_types(
            "MATCH (n:Person {name: 'b'}) SET n.first = $tags",
            R,
            &[("tags", GqlParameterType::List)],
            symbols,
        )
        .unwrap()
        .bind_parameters(&tags)
        .unwrap();
        db.execute_graph_write_program_returning_autocommit_engine_governed(
            &txn,
            &cx,
            &commit,
            &program,
            GraphWriteProgramPolicy::new(policy(), 100_000, 100_000, 100_000),
        )
        .await
        .unwrap();
        assert_eq!(first(&db)[1], vec![text("b"), list([text("p"), text("q")])]);
        // MERGE branches take a constant List: a literal ON CREATE, a
        // declared List parameter ON MATCH. A map inside refuses when the
        // statement binds and commits nothing.
        write(
            &mut db,
            cxs,
            "MERGE (n:Person {name: 'd'}) ON CREATE SET n.first = ['m', 'n']",
            &empty,
        )
        .await
        .expect("MERGE ON CREATE list");
        let program = PreparedGraphWriteScript::prepare_with_parameter_types(
            "MERGE (n:Person {name: 'd'}) ON MATCH SET n.first = $tags",
            R,
            &[("tags", GqlParameterType::List)],
            symbols,
        )
        .unwrap()
        .bind_parameters(&tags)
        .unwrap();
        db.execute_graph_write_program_returning_autocommit_engine_governed(
            &txn,
            &cx,
            &commit,
            &program,
            GraphWriteProgramPolicy::new(policy(), 100_000, 100_000, 100_000),
        )
        .await
        .unwrap();
        let refused = write(
            &mut db,
            cxs,
            "MERGE (n:Person {name: 'e'}) ON CREATE SET n.first = [{k: 1}]",
            &empty,
        )
        .await;
        assert!(refused.is_err(), "{refused:?}");
        let people = first(&db);
        assert_eq!(people.len(), 4, "the refused MERGE created nothing");
        assert_eq!(people[3], vec![text("d"), list([text("p"), text("q")])]);
        // A relationship MERGE branch takes a constant List too.
        write(
            &mut db,
            cxs,
            "MATCH (a:Person {name: 'a'}), (b:Person {name: 'b'}) \
             MERGE (a)-[r:R]->(b) ON CREATE SET r.first = [1, 2]",
            &empty,
        )
        .await
        .expect("relationship MERGE ON CREATE list");
        let text_query = "MATCH (a:Person)-[r:R]->(b:Person) RETURN r.first AS first";
        assert_eq!(
            cells(
                db.query(&cx, text_query, &empty, symbols, policy())
                    .expect(text_query)
            ),
            vec![vec![list([int(1), int(2)])]]
        );
        // A `+` chain with a List operand reads whole whichever operand
        // leads: SET with a literal head, CREATE with a literal or a stored
        // List head, and a constant MERGE branch.
        for statement in [
            "MATCH (n:Person {name: 'c'}) SET n.first = ['w'] + n.first",
            "CREATE (:Person {name: 'f', first: [1] + [2]})",
            "MATCH (n:Person {name: 'c'}) CREATE (:Person {name: 'g', first: n.first + ['z']})",
            "MERGE (n:Person {name: 'h'}) ON CREATE SET n.first = ['m'] + ['n']",
        ] {
            write(&mut db, cxs, statement, &empty)
                .await
                .expect(statement);
        }
        let people = first(&db);
        assert_eq!(people.len(), 7);
        let chain = list([text("w"), text("x"), text("y")]);
        assert_eq!(people[2], vec![text("c"), chain]);
        assert_eq!(people[4], vec![text("f"), list([int(1), int(2)])]);
        assert_eq!(
            people[5],
            vec![
                text("g"),
                list([text("w"), text("x"), text("y"), text("z")])
            ]
        );
        assert_eq!(people[6], vec![text("h"), list([text("m"), text("n")])]);
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
            // A wildcard still requires valid syntax for later selectors.
            "MATCH (n:Person) RETURN n{.*, .} AS m",
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

/// fgdb-v6ku6, owner ruling (a): `=` is FALSE, `<>` TRUE and IN FALSE
/// between values of different domains, as in openCypher, wherever a query
/// compares them: a stored List against a scalar under WHERE, NOT and OR,
/// scalar properties of unrelated kinds, two properties, a WITH filter, a
/// grouped filter, a projection and the bare RETURN. NULL stays UNKNOWN, and
/// an ordering across domains stays NULL. Without the ruling, `NOT n.first =
/// 'a'` silently dropped every vertex whose `first` is a List.
#[test]
fn values_of_different_domains_are_unequal_through_database_queries() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let (commit, cx, txn) = (contexts.commit(), contexts.query(), contexts.txn());
    runtime.block_on(async {
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, graph()).await.unwrap();
        let empty = GqlParameters::new();
        // a: p 10, first ['math', 'poetry']; b: p 20, first ['a']; c: p 30;
        // d: first 'a' and no p; e: p 'x' and no first.
        for statement in [
            "MATCH (n:Person {name: 'a'}) SET n.first = ['math', 'poetry']",
            "MATCH (n:Person {name: 'b'}) SET n.first = ['a']",
            "CREATE (:Person {name: 'd', first: 'a'})",
            "CREATE (:Person {name: 'e', p: 'x'})",
        ] {
            write(&mut db, (&commit, &cx, &txn), statement, &empty)
                .await
                .expect(statement);
        }
        let column = |statement: &str| {
            let mut values: Vec<_> = cells(
                db.query(&cx, statement, &empty, symbols, policy())
                    .expect(statement),
            )
            .into_iter()
            .map(|row| row[0].clone())
            .collect();
            values.sort();
            values
        };
        let names = |list: &[&str]| list.iter().map(|name| text(name)).collect::<Vec<_>>();
        for (statement, expected) in [
            // A stored List against a scalar.
            (
                "MATCH (n:Person) WHERE NOT n.first = 'a' RETURN n.name AS name",
                names(&["a", "b"]),
            ),
            (
                "MATCH (n:Person) WHERE n.first <> 'a' RETURN n.name AS name",
                names(&["a", "b"]),
            ),
            (
                "MATCH (n:Person) WHERE n.first = 'a' RETURN n.name AS name",
                names(&["d"]),
            ),
            (
                "MATCH (n:Person) WHERE n.first <> 'a' OR n.p = 30 RETURN n.name AS name",
                names(&["a", "b", "c"]),
            ),
            // Scalar properties of unrelated kinds.
            (
                "MATCH (n:Person) WHERE n.p <> 10 RETURN n.name AS name",
                names(&["b", "c", "e"]),
            ),
            (
                "MATCH (n:Person) WHERE NOT n.p = 'x' RETURN n.name AS name",
                names(&["a", "b", "c"]),
            ),
            (
                "MATCH (n:Person) WHERE n.p = 10 OR n.name <> 1 RETURN n.name AS name",
                names(&["a", "b", "c", "d", "e"]),
            ),
            (
                "MATCH (n:Person) WHERE n.p IN [10, 'x'] RETURN n.name AS name",
                names(&["a", "e"]),
            ),
            // An ordering across domains stays UNKNOWN, under NOT too.
            (
                "MATCH (n:Person) WHERE n.p < 'z' RETURN n.name AS name",
                names(&["e"]),
            ),
            (
                "MATCH (n:Person) WHERE NOT n.p < 'z' RETURN n.name AS name",
                names(&[]),
            ),
            // Two properties.
            (
                "MATCH (a:Person {name: 'a'}), (b:Person) WHERE NOT a.first = b.first \
                 RETURN b.name AS name",
                names(&["b", "d"]),
            ),
            (
                "MATCH (a:Person {name: 'a'}), (b:Person) WHERE a.first <> b.name \
                 RETURN b.name AS name",
                names(&["a", "b", "c", "d", "e"]),
            ),
            // A WITH filter and a grouped filter.
            (
                "MATCH (n:Person) WITH n.name AS name, n.first AS first \
                 WHERE first <> 'a' RETURN name",
                names(&["a", "b"]),
            ),
            (
                "MATCH (n:Person) WITH n.name AS name, n.p AS p WHERE NOT p = 'x' RETURN name",
                names(&["a", "b", "c"]),
            ),
            (
                "MATCH (n:Person) WITH n.p AS p, count(*) AS c WHERE p <> 10 RETURN p",
                vec![int(20), int(30), text("x")],
            ),
        ] {
            assert_eq!(column(statement), expected, "{statement}");
        }
        // A projection of scalar comparisons.
        let statement = "MATCH (n:Person) RETURN n.name AS name, n.p <> 'x' AS a, \
                         n.p = 10 AS b, n.name <> 1 AS c, n.p IN ['x', 20] AS d";
        let mut rows = cells(
            db.query(&cx, statement, &empty, symbols, policy())
                .expect(statement),
        );
        rows.sort();
        let truth = |value: bool| GraphValue::Scalar(CanonicalScalar::Bool(value));
        let (t, f) = (truth(true), truth(false));
        assert_eq!(
            rows,
            vec![
                vec![text("a"), t.clone(), t.clone(), t.clone(), f.clone()],
                vec![text("b"), t.clone(), f.clone(), t.clone(), t.clone()],
                vec![text("c"), t.clone(), f.clone(), t.clone(), f.clone()],
                vec![text("d"), null(), null(), t.clone(), null()],
                vec![text("e"), f.clone(), f, t.clone(), t],
            ],
            "{statement}"
        );
        // The bare RETURN, with the null rules and numeric equality that stay.
        let statement = "RETURN 1 = 'a' AS a, 1 <> 'a' AS b, 1 IN ['a', 2] AS c, \
                         [1] = ['a'] AS d, 1 IN [1, null] AS e, 2 IN [1, null] AS f, \
                         null = null AS g, 1 = 1.0 AS h";
        assert_eq!(
            cells(
                db.query(&cx, statement, &empty, symbols, policy())
                    .expect(statement)
            ),
            vec![vec![
                truth(false),
                truth(true),
                truth(false),
                truth(false),
                truth(true),
                null(),
                null(),
                truth(true),
            ]],
            "{statement}"
        );
    });
}

/// A stored List grows in every write row and sizes inside arithmetic
/// (fgdb-c4i0d, fgdb-k3nub). SET over a WITH row or an UNWIND row appends
/// with `+` whichever operand leads, reading row values inside the list, as
/// MATCH ... SET already did; coalesce with a list default starts an absent
/// List; size()
/// of a stored List counts its elements inside arithmetic, as size() of a
/// text counts its characters. Expected values are enumerated by hand.
#[test]
fn stored_lists_grow_in_write_rows_and_size_in_arithmetic() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let (commit, cx, txn) = (contexts.commit(), contexts.query(), contexts.txn());
    runtime.block_on(async {
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, graph()).await.unwrap();
        let cxs = (&commit, &cx, &txn);
        let empty = GqlParameters::new();
        let query = |db: &Database<fgdb::MemVfs>, text: &str| {
            let mut rows = cells(db.query(&cx, text, &empty, symbols, policy()).expect(text));
            rows.sort();
            rows
        };
        let first = "MATCH (n:Person) RETURN n.name AS name, n.first AS first";
        for statement in [
            "MATCH (n:Person) WITH n SET n.first = coalesce(n.first, []) + [n.p]",
            "UNWIND [10, 20] AS v MATCH (n:Person {p: v}) SET n.first = n.first + [v * 2]",
            "MATCH (n:Person {name: 'c'}) WITH n SET n.first = [0] + n.first",
            "CREATE (n:Person {name: 'd', first: [1]})",
            "MATCH (n:Person {name: 'd'}) WITH n, 3 AS last SET n.first = n.first + [2, last]",
        ] {
            write(&mut db, cxs, statement, &empty)
                .await
                .expect(statement);
        }
        assert_eq!(
            query(&db, first),
            vec![
                vec![text("a"), list([int(10), int(20)])],
                vec![text("b"), list([int(20), int(40)])],
                vec![text("c"), list([int(0), int(30)])],
                vec![text("d"), list([int(1), int(2), int(3)])],
            ]
        );
        assert_eq!(
            query(
                &db,
                "MATCH (n:Person) \
                 RETURN n.name AS name, size(n.first) + 1 AS s, size(n.name) * 2 AS c"
            ),
            vec![
                vec![text("a"), int(3), int(2)],
                vec![text("b"), int(3), int(2)],
                vec![text("c"), int(3), int(2)],
                vec![text("d"), int(4), int(2)],
            ]
        );
        write(
            &mut db,
            cxs,
            "MATCH (n:Person) WITH n SET n.p = size(n.first) * 100",
            &empty,
        )
        .await
        .expect("size() of a stored List inside a row SET");
        assert_eq!(
            query(&db, "MATCH (n:Person) RETURN n.name AS name, n.p AS p"),
            vec![
                vec![text("a"), int(200)],
                vec![text("b"), int(200)],
                vec![text("c"), int(200)],
                vec![text("d"), int(300)],
            ]
        );
    });
}

/// fgdb-bqri3: `+` joins a stored List whatever the other operand's static
/// type, as openCypher does. A text or number appends or prepends, and two
/// dynamic Lists concatenate, in SET (plain or over an UNWIND row) and in
/// RETURN. A scalar counter still adds. Every other operator over a List
/// still refuses, and a text predicate over one is NULL. Expected values are
/// enumerated by hand.
#[test]
fn plus_joins_a_stored_list_with_any_operand() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let (commit, cx, txn) = (contexts.commit(), contexts.query(), contexts.txn());
    runtime.block_on(async {
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, graph()).await.unwrap();
        let cxs = (&commit, &cx, &txn);
        let empty = GqlParameters::new();
        let query = |db: &Database<fgdb::MemVfs>, text: &str| {
            let mut rows = cells(db.query(&cx, text, &empty, symbols, policy()).expect(text));
            rows.sort();
            rows
        };
        for statement in [
            "MATCH (n:Person) SET n.first = [n.name]",
            // Text appends and prepends (the scalar compiler's Concat).
            "MATCH (n:Person) SET n.first = n.first + 'z'",
            "MATCH (n:Person {name: 'a'}) SET n.first = 'y' + n.first",
            // A map entry that holds a List concatenates (both dynamic).
            "UNWIND [{who: 'b', more: ['m', 'n']}] AS row \
             MATCH (n:Person {name: row.who}) SET n.first = n.first + row.more",
            // A number appends to a numeric List; a scalar counter adds.
            "MATCH (n:Person {name: 'c'}) SET n.p = [n.p]",
            "MATCH (n:Person {name: 'c'}) SET n.p = n.p + 5",
            "MATCH (n:Person {name: 'a'}) SET n.p = n.p + 1",
        ] {
            write(&mut db, cxs, statement, &empty)
                .await
                .expect(statement);
        }
        assert_eq!(
            query(
                &db,
                "MATCH (n:Person) RETURN n.name AS name, n.first AS first, n.p AS p"
            ),
            vec![
                vec![text("a"), list([text("y"), text("a"), text("z")]), int(11)],
                vec![
                    text("b"),
                    list([text("b"), text("z"), text("m"), text("n")]),
                    int(20)
                ],
                vec![
                    text("c"),
                    list([text("c"), text("z")]),
                    list([int(30), int(5)])
                ],
            ]
        );
        // Two dynamic Lists in RETURN (TCK Return2 [7]), a List with a
        // number, and a text predicate over a List, which is NULL.
        assert_eq!(
            query(
                &db,
                "MATCH (n:Person {name: 'c'}) \
                 RETURN n.first + n.first AS twice, n.p + 7 AS more, \
                 n.first STARTS WITH 'c' AS starts"
            ),
            vec![vec![
                list([text("c"), text("z"), text("c"), text("z")]),
                list([int(30), int(5), int(7)]),
                null(),
            ]]
        );
        for statement in [
            "MATCH (n:Person {name: 'c'}) SET n.p = n.p * 2",
            "MATCH (n:Person {name: 'c'}) SET n.p = n.p - 5",
        ] {
            let result = write(&mut db, cxs, statement, &empty).await;
            assert!(
                result
                    .as_ref()
                    .is_err_and(|error| error.starts_with("execute:")),
                "{statement}: {result:?}"
            );
        }
        assert_eq!(
            query(&db, "MATCH (n:Person {name: 'c'}) RETURN n.p AS p"),
            vec![vec![list([int(30), int(5)])]]
        );
        // Only a join answers a List from the scalar program: one an
        // expression merely passes through (coalesce, CASE) still refuses.
        for statement in [
            "MATCH (n:Person {name: 'c'}) RETURN coalesce(n.first, 'x') AS c",
            "MATCH (n:Person {name: 'c'}) RETURN CASE WHEN n.p IS NULL THEN 'x' ELSE n.p END AS c",
        ] {
            assert!(
                db.query(&cx, statement, &empty, symbols, policy()).is_err(),
                "{statement}"
            );
        }
    });
}

/// fgdb-bqri3: WHERE compares a stored List with a list or map value, three
/// valued, as RETURN does. The pattern filter compares scalars only, so a
/// MATCH WHERE with a list or map literal, subscript or slice runs as a row
/// filter after the match, as one over a list function already does; there,
/// and after WITH, a composite comparison is one value-language test.
#[test]
fn where_compares_stored_lists_with_list_and_map_values() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let (commit, cx, txn) = (contexts.commit(), contexts.query(), contexts.txn());
    runtime.block_on(async {
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, graph()).await.unwrap();
        let cxs = (&commit, &cx, &txn);
        let empty = GqlParameters::new();
        for statement in [
            "MATCH (n:Person {name: 'a'}) SET n.first = ['x', 'y']",
            "MATCH (n:Person {name: 'b'}) SET n.first = ['x']",
            "MATCH (n:Person {name: 'c'}) SET n.first = ['z']",
        ] {
            write(&mut db, cxs, statement, &empty)
                .await
                .expect(statement);
        }
        let names = |statement: &str| {
            let mut rows = cells(
                db.query(&cx, statement, &empty, symbols, policy())
                    .expect(statement),
            );
            rows.sort();
            rows.into_iter().flatten().collect::<Vec<_>>()
        };
        let named = |names: &[&str]| names.iter().map(|name| text(name)).collect::<Vec<_>>();
        for (statement, expected) in [
            (
                "MATCH (n:Person) WHERE n.first = ['x', 'y'] RETURN n.name AS name",
                named(&["a"]),
            ),
            (
                "MATCH (n:Person) WHERE n.first <> ['x', 'y'] RETURN n.name AS name",
                named(&["b", "c"]),
            ),
            (
                "MATCH (n:Person) WHERE ['x'] = n.first RETURN n.name AS name",
                named(&["b"]),
            ),
            (
                "MATCH (n:Person) WHERE n.first[0] = 'x' RETURN n.name AS name",
                named(&["a", "b"]),
            ),
            (
                "MATCH (n:Person) WHERE n.first < ['y'] RETURN n.name AS name",
                named(&["a", "b"]),
            ),
            // Values of different domains are unequal (fgdb-v6ku6).
            (
                "MATCH (n:Person) WHERE n.first = {k: 1} RETURN n.name AS name",
                named(&[]),
            ),
            (
                "MATCH (n:Person) WHERE n.p = [10] RETURN n.name AS name",
                named(&[]),
            ),
            (
                "MATCH (n:Person) WHERE n.p <> [10] RETURN n.name AS name",
                named(&["a", "b", "c"]),
            ),
            (
                "MATCH (n:Person) WHERE n.first = ['x'] OR n.p = 30 RETURN n.name AS name",
                named(&["b", "c"]),
            ),
            // After WITH, the row filter compares the composite itself, on
            // either side, and a leading literal's membership too.
            (
                "MATCH (n:Person) WITH n WHERE n.first = ['x'] RETURN n.name AS name",
                named(&["b"]),
            ),
            (
                "MATCH (n:Person) WITH n WHERE ['x'] = n.first RETURN n.name AS name",
                named(&["b"]),
            ),
            (
                "MATCH (n:Person) WITH n WHERE ['z'] IN [n.first] RETURN n.name AS name",
                named(&["c"]),
            ),
            (
                "MATCH (n:Person) WITH n.first AS f, n.name AS name \
                 WHERE f = ['x', 'y'] RETURN name",
                named(&["a"]),
            ),
            (
                "MATCH (n:Person) WITH n.first AS f, n.name AS name \
                 WHERE f <> ['x'] RETURN name",
                named(&["a", "c"]),
            ),
        ] {
            assert_eq!(names(statement), expected, "{statement}");
        }
        let boolean = |value| GraphValue::Scalar(CanonicalScalar::Bool(value));
        assert_eq!(
            names("MATCH (n:Person) RETURN n.name AS name, n.first = ['x'] AS hit"),
            vec![
                text("a"),
                boolean(false),
                text("b"),
                boolean(true),
                text("c"),
                boolean(false),
            ]
        );
    });
}
