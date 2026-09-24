//! Plain aggregation must preserve the native binding visitor's occurrence order.
use super::*;
use crate::{DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, SchemaEpoch};
use fgdb_gql::{GqlParameters, GraphAggregateValue, GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText};
use fgdb_types::{CommitCx, DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Grant, QueryLimits, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x85; 32]);
const P: PropertyKeyId = PropertyKeyId(1);
const H: PropertyKeyId = PropertyKeyId(2);
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(8501), NS, "graph", SchemaEpoch(0), 1).unwrap()
}
fn grant() -> Grant {
    let mut grant = Grant::read_only("main", 1000, QueryLimits {
        max_nodes: 1000, max_work: 1_000_000, max_rows: 1000,
    });
    grant.labels = Scope::only([LabelId(1)]);
    grant.properties = Scope::only([P]);
    grant.relations = Scope::All;
    grant
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "hidden") => Some(GraphSymbol::Property(H)),
        _ => None,
    }
}
async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x15; 32], NS, [0x25; 32])).await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, value, visible) in [(0, Some(7), true), (1, Some(3), true),
        (2, Some(999), false), (3, Some(7), true), (u128::MAX, None, true)]
    {
        let mut properties = Vec::new();
        if let Some(value) = value { properties.push((P, CanonicalScalar::Int(value))); }
        properties.push((H, CanonicalScalar::ucs_basic_text("hidden").unwrap()));
        batch.create_vertex(VId(id), vec![LabelId(if visible { 1 } else { 99 })], properties);
    }
    db.write(cx, batch).await.unwrap();
    db
}
fn list(values: &[i64]) -> QueryValue {
    GraphAggregateValue::Value(GraphValue::List(values.iter().map(|value| {
        GraphValue::Scalar(CanonicalScalar::Int(*value))
    }).collect()))
}
fn rows(result: QueryResult) -> Vec<Vec<QueryValue>> {
    match result { QueryResult::Rows { rows, .. } => rows, _ => panic!("read returned a write") }
}

#[test]
fn eager_plain_collect_agrees_with_native_visitation_not_sorted_projection() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.query();
    let db = runtime.block_on(database(&contexts.commit()));
    let issuer = authority();
    let token = issuer.issue_at(&grant(), 100).unwrap();
    let params = GqlParameters::new();
    let text = "MATCH (n) WHERE n.hidden IS NULL RETURN COLLECT(n.p) AS items,COLLECT(DISTINCT n.p) AS unique_items,COUNT(*) AS n";
    let mut session = db.authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100).unwrap();
    let prepared = session.prepare(&cx, text, &params).unwrap();
    let expected = vec![list(&[7, 3, 7]), list(&[7, 3]), GraphAggregateValue::Count(4)];
    assert_eq!(rows(session.execute(&cx, &prepared, &params).unwrap()), vec![expected.clone()]);
    assert_eq!(rows(db.query_authorized(&cx, &issuer, &token, "main", text, &params, symbols, policy(), || 100).unwrap()), vec![expected.clone()]);
    let mut cursor = session.stream_aggregate(&cx, &prepared, &params).unwrap();
    assert_eq!(cursor.next().unwrap().unwrap().values(), expected);
    assert!(cursor.next().is_none());

    // Mutation-sensitive negative control: the replaced adapter asks a row
    // query to sort its output before the receiver observes any occurrence.
    let definition = PreparedGraphAggregateText::prepare(text, symbols).unwrap().bind_parameters(&params).unwrap();
    let sorted = authorized(&db, &cx, &issuer, &token, "main", None, || 100,
        |snapshot, at, scope, execution| {
            definition.execute_with_source_governed(policy(),
                |pattern, remaining| pattern_at(snapshot, at, pattern, scope, execution, remaining),
                || execution.borrow_mut().checkpoint(),
            ).map(|result| result.value).map_err(aggregate_error)
        }).unwrap();
    assert_ne!(sorted[0].values(), expected);
    assert_eq!(sorted[0].values()[0], list(&[3, 7, 7]));
}

#[test]
fn hidden_group_keys_and_history_do_not_change_visitation_or_delivery_admission() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.query();
    let commit = contexts.commit();
    let mut db = runtime.block_on(database(&commit));
    let basis = db.frontier().unwrap();
    let issuer = authority();
    let mut g = grant(); g.limits.max_nodes = 4; g.limits.max_rows = 1;
    let token = issuer.issue_at(&g, 100).unwrap();
    let params = GqlParameters::new();
    let text = "MATCH (n) RETURN n.hidden AS key,COLLECT(n.p) AS items,COUNT(*) AS n GROUP BY n.hidden HAVING COUNT(*) = 4 LIMIT 1";
    let expected = vec![GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null)), list(&[7, 3, 7]), GraphAggregateValue::Count(4)];
    assert_eq!(rows(db.query_authorized(&cx, &issuer, &token, "main", text, &params, symbols, policy(), || 100).unwrap()), vec![expected.clone()]);
    let mut change = WriteBatch::new(RelationId(1));
    change.set_vertex_label(VId(1), LabelId(1), false);
    change.set_vertex_property(VId(0), P, Some(CanonicalScalar::Int(70)));
    runtime.block_on(db.write(&commit, change)).unwrap();
    runtime.block_on(db.compact(&commit)).unwrap();
    let history = format!("MATCH (n) FOR SYSTEM_TIME AS OF SEQ {} RETURN n.hidden AS key,COLLECT(n.p) AS items,COUNT(*) AS n GROUP BY n.hidden", basis.0);
    assert_eq!(rows(db.query_authorized(&cx, &issuer, &token, "main", &history, &params, symbols, policy(), || 100).unwrap()), vec![expected]);
    let mut fewer = g.clone();
    fewer.limits.max_nodes = 3;
    let denied = issuer.issue_at(&fewer, 100).unwrap();
    assert!(matches!(db.query_authorized(&cx, &issuer, &denied, "main", &format!("{history} LIMIT 0"), &params, symbols, policy(), || 100),
        Err(QueryError::Authorization(fgdb_warden::Error::LimitExceeded(fgdb_warden::LimitDimension::Nodes)))));
}

#[test]
fn computed_and_paged_relational_inputs_keep_their_own_complete_row_order() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.query();
    let db = runtime.block_on(database(&contexts.commit()));
    let issuer = authority();
    let token = issuer.issue_at(&grant(), 100).unwrap();
    let params = GqlParameters::new();
    for (text, expected) in [
        ("MATCH (n) WITH n.p AS value ORDER BY value DESC NULLS LAST LIMIT 2 RETURN COLLECT(value) AS items", list(&[7, 7])),
        ("MATCH (n) WITH n.p AS value ORDER BY value ASC NULLS FIRST LIMIT 3 RETURN COLLECT(value) AS items", list(&[3, 7])),
    ] {
        assert_eq!(rows(db.query_authorized(&cx, &issuer, &token, "main", text, &params, symbols, policy(), || 100).unwrap()), vec![vec![expected]], "{text}");
    }
}
