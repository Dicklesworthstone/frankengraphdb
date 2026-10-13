//! Dynamic map keys use the existing native value expression, including inside
//! writes. Expected rows below are independent, not copied from another engine.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, PreparedNativeRead, QueryResult, QueryValue, QueryWriteError};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use std::convert::Infallible;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x41; 32],
        DatabaseSecurityNamespaceId([0x42; 32]),
        [0x43; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        _ => None,
    }
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}
fn map(entries: Vec<(&str, GraphValue)>) -> GraphValue {
    GraphValue::map(
        entries
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
    .unwrap()
}
fn expected(columns: &[&str], rows: Vec<Vec<GraphValue>>) -> QueryResult {
    QueryResult::Rows {
        columns: columns.iter().map(|name| (*name).to_owned()).collect(),
        rows: rows
            .into_iter()
            .map(|row| row.into_iter().map(QueryValue::Value).collect())
            .collect(),
    }
}

#[test]
fn prepared_map_keys_rebind_and_compose_with_nested_access_and_comprehensions() {
    let ((), report) = run_async_under_lab(0x6d61_7001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let text = "WITH {a:10,b:20,inner:{x:100}} AS m RETURN m[$key] AS chosen, \
            m['inner']['x'] AS nested, [k IN ['b','absent','a'] | m[k]] AS selected";
        let params = GqlParameters::new().with_text("key", "a").unwrap();
        let plan = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
        for (key, value) in [("a", int(10)), ("b", int(20)), ("missing", null())] {
            let args = GqlParameters::new().with_text("key", key).unwrap();
            let want = expected(
                &["chosen", "nested", "selected"],
                vec![vec![
                    value,
                    int(100),
                    GraphValue::List(vec![int(20), null(), int(10)].into_boxed_slice()),
                ]],
            );
            assert_eq!(plan.execute(&db, &cx, &args, policy()).unwrap(), want);
        }
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn parameter_documents_select_native_identities_lists_and_maps_without_coercion() {
    let ((), report) = run_async_under_lab(0x6d61_7002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let identity = GraphValue::Vertex(VId(u128::MAX));
        let nested = map(vec![("x", int(7))]);
        let values = vec![
            identity,
            nested,
            GraphValue::List(vec![int(1), null()].into_boxed_slice()),
        ];
        for value in values {
            let params = GqlParameters::new()
                .with_list("rows", vec![map(vec![("selected", value.clone())])])
                .unwrap()
                .with_text("key", "selected")
                .unwrap();
            let frozen = params.canonical_bytes();
            assert_eq!(
                db.query(
                    &cx,
                    "UNWIND $rows AS r RETURN r[$key] AS value",
                    &params,
                    symbols,
                    policy()
                )
                .unwrap(),
                expected(&["value"], vec![vec![value]])
            );
            assert_eq!(params.canonical_bytes(), frozen);
        }
        for text in [
            "RETURN {a:1}[0] AS value",
            "RETURN [1,2]['a'] AS value",
            "RETURN {a:1}[true] AS value",
            "RETURN {a:1}[[]] AS value",
        ] {
            assert!(
                db.query(&cx, text, &GqlParameters::new(), symbols, policy())
                    .is_err(),
                "{text}"
            );
        }
        assert_eq!(
            db.query(
                &cx,
                "RETURN {a:1}[null] AS a, null['a'] AS b",
                &GqlParameters::new(),
                symbols,
                policy()
            )
            .unwrap(),
            expected(&["a", "b"], vec![vec![null(), null()]])
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A parenthesized value that a map read or subscript follows is read whole
/// (TCK Graph6 [4], [8], Map1 [3]), and a function argument is a whole
/// expression, comparisons and Boolean operators included (TypeConversion4
/// [3]). Grouping parentheses before an operator keep their meaning.
#[test]
fn parenthesized_values_take_postfix_reads_and_arguments_take_predicates() {
    let ((), report) = run_async_under_lab(0x6d61_7005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let text =
            |value: &str| GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap());
        let boolean = |value| GraphValue::Scalar(CanonicalScalar::Bool(value));
        for (statement, columns, row) in [
            (
                "WITH [123, {existing: 42, notMissing: null}] AS list \
                 RETURN (list[1]).missing AS a, (list[1]).notMissing AS b, \
                 (list[1]).existing AS c",
                vec!["a", "b", "c"],
                vec![null(), null(), int(42)],
            ),
            ("RETURN ([1, 2])[0] AS x", vec!["x"], vec![int(1)]),
            // NULL is dynamically typed: its map read is NULL (Map1 [2],
            // Null1 [5], Null2 [5]).
            (
                "WITH null AS m RETURN m.missing AS a, m.name IS NULL AS b, \
                 m.name IS NOT NULL AS c",
                vec!["a", "b", "c"],
                vec![null(), boolean(true), boolean(false)],
            ),
            // A graph function of NULL is NULL (TCK Graph4 [3]).
            (
                "RETURN type(null) AS a, nodes(null) AS b",
                vec!["a", "b"],
                vec![null(), null()],
            ),
            ("RETURN (1 + 2) * 3 AS x", vec!["x"], vec![int(9)]),
            (
                "RETURN toString(1 < 0) AS a, coalesce(1 = 2, true) AS b, \
                 toString(true AND false) AS c",
                vec!["a", "b", "c"],
                vec![text("false"), boolean(false), text("false")],
            ),
        ] {
            assert_eq!(
                db.query(&cx, statement, &GqlParameters::new(), symbols, policy())
                    .map_err(|error| format!("{statement}: {error}"))
                    .unwrap(),
                expected(&columns, vec![row]),
                "{statement}"
            );
        }
        // `$from..$to` are slice bounds, not field reads (TCK List2 [10]).
        let bounds = GqlParameters::new()
            .with_int64("from", 1)
            .unwrap()
            .with_int64("to", 3)
            .unwrap();
        assert_eq!(
            db.query(
                &cx,
                "WITH [1, 2, 3] AS list RETURN list[$from..$to] AS r",
                &bounds,
                symbols,
                policy()
            )
            .unwrap(),
            expected(
                &["r"],
                vec![vec![GraphValue::List(
                    vec![int(2), int(3)].into_boxed_slice()
                )]]
            )
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// openCypher dynamic property access (Graph7; the Merge6/Merge7 control
/// queries): `n[key]` of a MATCH vertex or relationship reads the property
/// named by `key`, NULL when absent, alone, in arithmetic and in a
/// comprehension. A path is not subscripted, and a non-text key fails.
#[test]
fn element_subscripts_read_the_property_named_by_a_dynamic_key() {
    let ((), report) = run_async_under_lab(0x6d61_7004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut next = 200u128;
        let write_policy = GraphWriteProgramPolicy::new(policy(), 10_000, 10_000, 10_000);
        db.query_write(
            &contexts.txn(),
            &cx,
            &commit,
            "CREATE (:Item {p: 1, q: 'x'})-[:R {q: 'y'}]->(:Item {p: 3})",
            &GqlParameters::new(),
            symbols,
            RelationId(1),
            write_policy,
            |request| {
                next += 1;
                Ok::<_, Infallible>(match request.request {
                    fgdb_gql::insertion::GraphInsertRequest::Vertex { .. } => {
                        ElementId::Vertex(VId(next))
                    }
                    fgdb_gql::insertion::GraphInsertRequest::Edge { .. } => {
                        ElementId::Edge(fgdb_types::EId(next))
                    }
                })
            },
        )
        .await
        .unwrap();
        let text =
            |value: &str| GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap());
        let read = |statement: &str, params: &GqlParameters| {
            db.query(&cx, statement, params, symbols, policy())
        };
        let key = GqlParameters::new().with_text("key", "q").unwrap();
        assert_eq!(
            read(
                "MATCH (n:Item) RETURN n['p'] AS a, n[$key] AS b, n['absent'] AS c ORDER BY a",
                &key
            )
            .unwrap(),
            expected(
                &["a", "b", "c"],
                vec![
                    vec![int(1), text("x"), null()],
                    vec![int(3), null(), null()]
                ]
            )
        );
        assert_eq!(
            read(
                "MATCH ()-[r:R]->() RETURN r['q'] AS a, \
                 [key IN keys(r) | key + '->' + r[key]] AS kv",
                &GqlParameters::new()
            )
            .unwrap(),
            expected(
                &["a", "kv"],
                vec![vec![
                    text("y"),
                    GraphValue::List(vec![text("q->y")].into_boxed_slice())
                ]]
            )
        );
        assert_eq!(
            read(
                "MATCH (n:Item) RETURN n['p'] + 10 AS a, n[null] AS b ORDER BY a",
                &GqlParameters::new()
            )
            .unwrap(),
            expected(
                &["a", "b"],
                vec![vec![int(11), null()], vec![int(13), null()]]
            )
        );
        for refused in [
            "MATCH p = (a:Item)-[:R]->(b) RETURN p['q'] AS a",
            "MATCH (n:Item) RETURN n[1] AS a",
            "MATCH (n:Item) RETURN m['p'] AS a",
        ] {
            assert!(read(refused, &GqlParameters::new()).is_err(), "{refused}");
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dynamic_input_keys_feed_atomic_creation_and_late_bad_rows_cannot_publish() {
    let ((), report) = run_async_under_lab(0x6d61_7003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let query = "UNWIND $rows AS r CREATE (n:Item {p:r[$key]}) RETURN n.p AS p ORDER BY p";
        let params = GqlParameters::new()
            .with_list(
                "rows",
                vec![map(vec![("chosen", int(9))]), map(vec![("chosen", int(4))])],
            )
            .unwrap()
            .with_text("key", "chosen")
            .unwrap();
        let mut next = 100u128;
        let write_policy = GraphWriteProgramPolicy::new(policy(), 10_000, 10_000, 10_000);
        let rows = db
            .query_write(
                &contexts.txn(),
                &cx,
                &commit,
                query,
                &params,
                symbols,
                RelationId(1),
                write_policy,
                |_| {
                    next += 1;
                    Ok::<_, Infallible>(ElementId::Vertex(VId(next)))
                },
            )
            .await
            .unwrap();
        assert_eq!(rows, expected(&["p"], vec![vec![int(4)], vec![int(9)]]));
        let basis = db.frontier().unwrap();
        let before = db.vertices().unwrap();
        let bad = GqlParameters::new()
            .with_list(
                "rows",
                vec![
                    map(vec![("chosen", int(7))]),
                    GraphValue::List(vec![int(8)].into_boxed_slice()),
                ],
            )
            .unwrap()
            .with_text("key", "chosen")
            .unwrap();
        let failure = db
            .query_write(
                &contexts.txn(),
                &cx,
                &commit,
                "UNWIND $rows AS r CREATE (n:Item {p:r[$key]}) RETURN n LIMIT 0",
                &bad,
                symbols,
                RelationId(1),
                write_policy,
                |_| {
                    next += 1;
                    Ok::<_, Infallible>(ElementId::Vertex(VId(next)))
                },
            )
            .await;
        assert!(
            matches!(failure, Err(QueryWriteError::Insert(_))),
            "bad dynamic access must fail during native execution"
        );
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(db.vertices().unwrap(), before);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
