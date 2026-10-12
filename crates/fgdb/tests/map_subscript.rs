//! Dynamic map keys use the existing native value expression, including inside
//! writes. Expected rows below are independent, not copied from another engine.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, PreparedNativeRead, QueryResult, QueryValue, QueryWriteError};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use std::convert::Infallible;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x41; 32], DatabaseSecurityNamespaceId([0x42; 32]), [0x43; 32])
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}
fn int(value: i64) -> GraphValue { GraphValue::Scalar(CanonicalScalar::Int(value)) }
fn null() -> GraphValue { GraphValue::Scalar(CanonicalScalar::Null) }
fn map(entries: Vec<(&str, GraphValue)>) -> GraphValue {
    GraphValue::map(entries.into_iter().map(|(key, value)| (key.into(), value)).collect()).unwrap()
}
fn expected(columns: &[&str], rows: Vec<Vec<GraphValue>>) -> QueryResult {
    QueryResult::Rows {
        columns: columns.iter().map(|name| (*name).to_owned()).collect(),
        rows: rows.into_iter().map(|row| row.into_iter().map(QueryValue::Value).collect()).collect(),
    }
}

#[test]
fn prepared_map_keys_rebind_and_compose_with_nested_access_and_comprehensions() {
    let ((), report) = run_async_under_lab(0x6d61_7001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = Database::open_memory(&contexts.commit(), keys()).await.unwrap();
        let text = "WITH {a:10,b:20,inner:{x:100}} AS m RETURN m[$key] AS chosen, \
            m['inner']['x'] AS nested, [k IN ['b','absent','a'] | m[k]] AS selected";
        let params = GqlParameters::new().with_text("key", "a").unwrap();
        let plan = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
        for (key, value) in [("a", int(10)), ("b", int(20)), ("missing", null())] {
            let args = GqlParameters::new().with_text("key", key).unwrap();
            let want = expected(&["chosen", "nested", "selected"], vec![vec![value, int(100),
                GraphValue::List(vec![int(20), null(), int(10)].into_boxed_slice())]]);
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
        let db = Database::open_memory(&contexts.commit(), keys()).await.unwrap();
        let identity = GraphValue::Vertex(VId(u128::MAX));
        let nested = map(vec![("x", int(7))]);
        let values = vec![identity, nested, GraphValue::List(vec![int(1), null()].into_boxed_slice())];
        for value in values {
            let params = GqlParameters::new().with_list("rows", vec![map(vec![("selected", value.clone())])]).unwrap()
                .with_text("key", "selected").unwrap();
            let frozen = params.canonical_bytes();
            assert_eq!(db.query(&cx, "UNWIND $rows AS r RETURN r[$key] AS value", &params, symbols, policy()).unwrap(),
                expected(&["value"], vec![vec![value]]));
            assert_eq!(params.canonical_bytes(), frozen);
        }
        for text in ["RETURN {a:1}[0] AS value", "RETURN [1,2]['a'] AS value",
            "RETURN {a:1}[true] AS value", "RETURN {a:1}[[]] AS value"] {
            assert!(db.query(&cx, text, &GqlParameters::new(), symbols, policy()).is_err(), "{text}");
        }
        assert_eq!(db.query(&cx, "RETURN {a:1}[null] AS a, null['a'] AS b", &GqlParameters::new(), symbols, policy()).unwrap(),
            expected(&["a", "b"], vec![vec![null(), null()]]));
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
        let params = GqlParameters::new().with_list("rows", vec![map(vec![("chosen", int(9))]),
            map(vec![("chosen", int(4))])]).unwrap().with_text("key", "chosen").unwrap();
        let mut next = 100u128;
        let write_policy = GraphWriteProgramPolicy::new(policy(), 10_000, 10_000, 10_000);
        let rows = db.query_write(&contexts.txn(), &cx, &commit, query, &params, symbols, RelationId(1), write_policy,
            |_| { next += 1; Ok::<_, Infallible>(ElementId::Vertex(VId(next))) }).await.unwrap();
        assert_eq!(rows, expected(&["p"], vec![vec![int(4)], vec![int(9)]]));
        let basis = db.frontier().unwrap();
        let before = db.vertices().unwrap();
        let bad = GqlParameters::new().with_list("rows", vec![map(vec![("chosen", int(7))]),
            GraphValue::List(vec![int(8)].into_boxed_slice())]).unwrap().with_text("key", "chosen").unwrap();
        let failure = db.query_write(&contexts.txn(), &cx, &commit,
            "UNWIND $rows AS r CREATE (n:Item {p:r[$key]}) RETURN n LIMIT 0", &bad, symbols, RelationId(1), write_policy,
            |_| { next += 1; Ok::<_, Infallible>(ElementId::Vertex(VId(next))) }).await;
        assert!(matches!(failure, Err(QueryWriteError::Insert(_))), "bad dynamic access must fail during native execution");
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(db.vertices().unwrap(), before);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
