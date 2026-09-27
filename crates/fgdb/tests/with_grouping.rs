//! Native WITH grouping through the durable embedded database and Warden.
//! These bounded resident-source tests make no full-SSI or spill claim.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{
    Database, DatabaseKeys, MemVfs, NativeReadClass, PreparedNativeRead, QueryResult, QueryValue,
    WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x5d; 32]);
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1000, 2_000_000, 2_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn value(n: i64) -> QueryValue {
    QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(n)))
}
fn rows(columns: &[&str], rows: Vec<Vec<QueryValue>>) -> QueryResult {
    QueryResult::Rows {
        columns: columns.iter().map(|name| (*name).into()).collect(),
        rows,
    }
}
async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x34; 32], NS, [0x78; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, label, p) in [(1, 1, 10), (2, 1, 10), (3, 1, 20), (4, 99, 99)] {
        batch.create_vertex(
            VId(id),
            vec![LabelId(label)],
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(p)),
                (PropertyKeyId(2), CanonicalScalar::Int(id as i64 * 100)),
            ],
        );
    }
    for (eid, a, b) in [(10, 1, 2), (11, 1, 2), (12, 2, 3), (13, 1, 4)] {
        batch.add_edge(EId(eid), VId(a), VId(b), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    db
}

#[test]
fn native_query_classifies_group_then_rows_and_group_then_exact_summary_separately() {
    let ((), report) = run_async_under_lab(0x5d_6701, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit()).await;
        let params = GqlParameters::new();
        let text = "MATCH (n:L) WITH n.p AS p, count(*) AS c WHERE c > 1 RETURN p, c";
        let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
        assert_eq!(prepared.facade_class(), NativeReadClass::Set);
        assert_eq!(
            db.query(&c.query(), text, &params, symbols, policy())
                .unwrap(),
            rows(&["p", "c"], vec![vec![value(10), value(2)]])
        );
        let text = "MATCH (n:L) WITH n.p AS p, count(*) AS c RETURN sum(c) AS total";
        let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
        assert_eq!(prepared.facade_class(), NativeReadClass::PipelineAggregate);
        assert_eq!(
            db.query(&c.query(), text, &params, symbols, policy())
                .unwrap(),
            rows(&["total"], vec![vec![QueryValue::Integer(3)]])
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_grouping_outputs_feed_correlated_match_and_repeated_aggregation() {
    let ((), report) = run_async_under_lab(0x5d_6702, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit()).await;
        let params = GqlParameters::new();
        assert_eq!(
            db.query(
                &c.query(),
                "MATCH (n:L) WITH collect(n) AS xs UNWIND xs AS n MATCH (n)-[:R]->(m:L) \
             WITH n, count(m) AS c WITH sum(c) AS total RETURN total",
                &params,
                symbols,
                policy()
            )
            .unwrap(),
            rows(&["total"], vec![vec![value(3)]])
        );
        assert_eq!(
            db.query(
                &c.query(),
                "MATCH (n:L) WHERE n.p = 404 WITH count(*) AS c, collect(n) AS xs RETURN c, xs",
                &params,
                symbols,
                policy()
            )
            .unwrap(),
            rows(
                &["c", "xs"],
                vec![vec![
                    value(0),
                    QueryValue::Value(GraphValue::List(Box::new([])))
                ]]
            )
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_group_keys_arguments_and_continuations_see_only_the_scoped_source() {
    let ((), report) = run_async_under_lab(0x5d_6703, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit()).await;
        let issuer = Authority::new(
            AuthKey::from_seed(6713),
            NS,
            "host-graph",
            SchemaEpoch(0),
            1,
        )
        .unwrap();
        let mut grant = Grant::read_only(
            "main",
            1000,
            QueryLimits {
                max_nodes: 1000,
                max_rows: 1000,
                max_work: 2_000_000,
            },
        );
        grant.labels = Scope::only([LabelId(1)]);
        grant.relations = Scope::only([RelationId(1)]);
        grant.properties = Scope::only([PropertyKeyId(1)]);
        let token = issuer.issue_at(&grant, 100).unwrap();
        let params = GqlParameters::new();
        for (text, expected) in [
            (
                "MATCH (n) WITH n.secret AS hidden, count(*) AS c, count(n.secret) AS seen RETURN c, seen",
                rows(&["c", "seen"], vec![vec![value(3), value(0)]]),
            ),
            (
                "MATCH (n) WITH collect(n) AS xs UNWIND xs AS n MATCH (n)-[:R]->(m) \
              WITH count(*) AS c RETURN c",
                rows(&["c"], vec![vec![value(3)]]),
            ),
        ] {
            let actual = db
                .query_authorized(
                    &c.query(),
                    &issuer,
                    &token,
                    "main",
                    text,
                    &params,
                    symbols,
                    policy(),
                    || 100,
                )
                .unwrap();
            assert_eq!(actual, expected, "{text}");
            assert_ne!(
                db.query(&c.query(), text, &params, symbols, policy())
                    .unwrap(),
                expected,
                "unscoped control must expose the difference: {text}"
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
