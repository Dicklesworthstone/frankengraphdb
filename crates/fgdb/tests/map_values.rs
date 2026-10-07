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
