//! Restricted eager aggregation must have the native visitor's semantics.
//! The independent database contains only the explicitly admitted graph; it
//! neither calls the authorization adapter nor reconstructs results afterward.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryError, QueryResult, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregate, GraphAggregateError,
    GraphAggregateRow, GraphAggregateTextSlot, GraphAggregateValue, GraphSetProjection,
    GraphSetValue, GraphSymbol, GraphSymbolKind, PreparedGraphAggregate,
    PreparedGraphAggregateText, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use fgdb_warden::{Authority, Error as Denied, Grant, LimitDimension, QueryLimits, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x85; 32]);
const P: PropertyKeyId = PropertyKeyId(1);
const H: PropertyKeyId = PropertyKeyId(2);
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 1000, 1_000_000, 1_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "hidden") => Some(GraphSymbol::Property(H)),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        _ => None,
    }
}
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(8501), NS, "graph", SchemaEpoch(0), 1).unwrap()
}
fn grant() -> Grant {
    let mut grant = Grant::read_only(
        "main",
        1000,
        QueryLimits { max_nodes: 1000, max_work: 1_000_000, max_rows: 1000 },
    );
    grant.labels = Scope::only([LabelId(1)]);
    grant.relations = Scope::only([RelationId(1)]);
    grant.properties = Scope::only([P]);
    grant
}
async fn database(cx: &CommitCx, visible_only: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x25; 32], NS, [0x36; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, value) in [(0, Some(7)), (1, Some(3)), (2, Some(999)), (3, Some(7)), (u128::MAX, None)] {
        if visible_only && id == 2 {
            continue;
        }
        let labels = if id == 2 {
            vec![LabelId(99)]
        } else if visible_only {
            vec![LabelId(1)]
        } else {
            vec![LabelId(1), LabelId(99)]
        };
        let mut properties = Vec::new();
        if let Some(value) = value {
            properties.push((P, CanonicalScalar::Int(value)));
        }
        if !visible_only {
            properties.push((H, CanonicalScalar::Int(if id == 0 { 55 } else { 66 })));
        }
        batch.create_vertex(VId(id), labels, properties);
    }
    for (id, from, to) in [
        (10, 0, 1), (11, 1, 3), (12, 0, 2), (13, 2, u128::MAX), (14, 0, 1), (15, 3, 3),
    ] {
        if !visible_only || (from != 2 && to != 2) {
            batch.add_edge(EId(id), VId(from), VId(to), vec![]);
        }
    }
    db.write(cx, batch).await.unwrap();
    db
}
fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn list(values: &[i64]) -> GraphAggregateValue {
    GraphAggregateValue::Value(GraphValue::List(
        values.iter().map(|value| GraphValue::Scalar(CanonicalScalar::Int(*value))).collect(),
    ))
}
fn materialize(columns: Vec<String>, slots: &[GraphAggregateTextSlot], rows: Vec<GraphAggregateRow>) -> QueryResult {
    QueryResult::Rows {
        columns,
        rows: rows.into_iter().map(|row| {
            slots.iter().map(|slot| match *slot {
                GraphAggregateTextSlot::GroupKey(at) => GraphAggregateValue::Value(row.keys()[at].clone()),
                GraphAggregateTextSlot::Aggregate(at) => row.values()[at].clone(),
            }).collect()
        }).collect(),
    }
}

#[test]
fn plain_collections_agree_across_typed_native_scoped_text_and_scoped_pull_execution() {
    let ((), report) = run_async_under_lab(0x5ec0_8501, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit(), false).await;
        let reference = database(&c.commit(), true).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let text = "MATCH (n) WHERE n.hidden IS NULL RETURN COLLECT(n.p) AS items,COLLECT(DISTINCT n.p) AS support,COUNT(*) AS n,SUM(n.p) AS total";
        let definition = prepare(text);
        let expected = reference.execute_graph_aggregate_governed(&cx, &definition, policy()).unwrap().value;
        assert_eq!(expected[0].values(), &[
            list(&[7, 3, 7]), list(&[7, 3]), GraphAggregateValue::Count(4), GraphAggregateValue::Integer(17),
        ]);
        assert_ne!(expected[0].values()[0], list(&[3, 7, 7]), "sorting arguments changes COLLECT");
        let actual = db.execute_graph_aggregate_authorized(
            &cx, &issuer, &token, "main", &definition, policy(), || 100,
        ).unwrap();
        assert_eq!(actual, expected);
        let mut session = db.authorized_read_session(
            &cx, &issuer, &token, "main", symbols, policy(), || 100,
        ).unwrap();
        let args = GqlParameters::new();
        let prepared = session.prepare(&cx, text, &args).unwrap();
        let eager = session.execute(&cx, &prepared, &args).unwrap();
        let mut cursor = session.stream_aggregate(&cx, &prepared, &args).unwrap();
        let columns = cursor.columns().to_vec();
        let slots = cursor.output_slots().to_vec();
        let streamed = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(streamed, expected);
        assert_eq!(eager, materialize(columns, &slots, streamed));
        assert!(cursor.next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn graph_matching_grouping_and_computed_inputs_keep_the_native_order_boundaries() {
    let ((), report) = run_async_under_lab(0x5ec0_8502, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit(), false).await;
        let reference = database(&c.commit(), true).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        for text in [
            "MATCH (n) RETURN n.hidden AS key,COLLECT(n.p) AS items,COUNT(*) AS n GROUP BY n.hidden",
            "MATCH (n) RETURN n.p AS key,COLLECT(n.p) AS items GROUP BY n.p HAVING COUNT(*) > 1 ORDER BY key DESC LIMIT 1",
            "MATCH (a)-[:R]->(b) RETURN COLLECT(b.p) AS items,COUNT(*) AS n",
            "MATCH (a)<-[:R]-(b) RETURN COLLECT(b.p) AS items,COUNT(*) AS n",
            "MATCH (n) WHERE NOT EXISTS { MATCH (n)-[:R]->(m) } RETURN COLLECT(n.p) AS items,COUNT(*) AS n",
            "MATCH (n) RETURN COLLECT(n.p) AS items LIMIT 0",
        ] {
            let definition = prepare(text);
            let expected = reference.execute_graph_aggregate_governed(&cx, &definition, policy()).unwrap().value;
            let got = db.execute_graph_aggregate_authorized(
                &cx, &issuer, &token, "main", &definition, policy(), || 100,
            ).unwrap();
            assert_eq!(got, expected, "{text}");
        }
        // Computed input is explicitly AFTER the complete child's row order.
        // Do not apply the plain-visitor law across that existing boundary.
        let input = PreparedGraphText::prepare("MATCH (n) RETURN n.p AS p,n AS id", symbols)
            .unwrap().bind_parameters(&GqlParameters::new()).unwrap().with_duplicates();
        let definition = PreparedGraphAggregate::prepare_projected(
            input,
            vec![GraphSetProjection::new("value", GraphSetValue::Column(0))],
            &[],
            &[GraphAggregate::collect("items", 0)],
            0,
            None,
        ).unwrap();
        let expected = reference.execute_graph_aggregate_governed(&cx, &definition, policy()).unwrap().value;
        assert_eq!(expected[0].values(), &[list(&[3, 7, 7])]);
        let got = db.execute_graph_aggregate_authorized(
            &cx, &issuer, &token, "main", &definition, policy(), || 100,
        ).unwrap();
        assert_eq!(got, expected);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn one_permit_covers_native_aggregate_admission_and_late_failures_under_zero_pages() {
    let ((), report) = run_async_under_lab(0x5ec0_8503, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = database(&commit, false).await;
        let issuer = authority();
        let mut g = grant();
        g.limits.max_nodes = 4;
        g.limits.max_rows = 1;
        let token = issuer.issue_at(&g, 100).unwrap();
        let query = prepare("MATCH (n) RETURN COUNT(*) AS n,SUM(n.p) AS total");
        let got = db.execute_graph_aggregate_authorized(
            &cx, &issuer, &token, "main", &query, policy(), || 100,
        ).unwrap();
        assert_eq!(got[0].values(), &[GraphAggregateValue::Count(4), GraphAggregateValue::Integer(17)]);
        g.limits.max_nodes = 3;
        let limited = issuer.issue_at(&g, 100).unwrap();
        assert!(matches!(
            db.execute_graph_aggregate_authorized(&cx, &issuer, &limited, "main", &query, policy(), || 100),
            Err(QueryError::Authorization(Denied::LimitExceeded(LimitDimension::Nodes)))
        ));
        let at = db.frontier().unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(3), P, Some(CanonicalScalar::ucs_basic_text("not-an-integer").unwrap()));
        db.write(&commit, change).await.unwrap();
        assert_eq!(
            db.execute_graph_aggregate_authorized_at(&cx, &issuer, &token, "main", &query, at, policy(), || 100).unwrap(),
            got,
        );
        let zero = prepare("MATCH (n) RETURN SUM(n.p) AS total LIMIT 0");
        assert!(matches!(
            db.execute_graph_aggregate_authorized(&cx, &issuer, &token, "main", &zero, policy(), || 100),
            Err(QueryError::Aggregate(GqlQueryError::Source(GraphAggregateError::NonIntegerSum { .. })))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
