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
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
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
        QueryLimits {
            max_nodes: 1000,
            max_work: 1_000_000,
            max_rows: 1000,
        },
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
    for (id, value) in [
        (0, Some(7)),
        (1, Some(3)),
        (2, Some(999)),
        (3, Some(7)),
        (u128::MAX, None),
    ] {
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
        (10, 0, 1),
        (11, 1, 3),
        (12, 0, 2),
        (13, 2, u128::MAX),
        (14, 0, 1),
        (15, 3, 3),
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
        values
            .iter()
            .map(|value| GraphValue::Scalar(CanonicalScalar::Int(*value)))
            .collect(),
    ))
}
fn materialize(
    columns: Vec<String>,
    slots: &[GraphAggregateTextSlot],
    rows: Vec<GraphAggregateRow>,
) -> QueryResult {
    QueryResult::Rows {
        columns,
        rows: rows
            .into_iter()
            .map(|row| {
                slots
                    .iter()
                    .map(|slot| match *slot {
                        GraphAggregateTextSlot::GroupKey(at) => {
                            GraphAggregateValue::Value(row.keys()[at].clone())
                        }
                        GraphAggregateTextSlot::Aggregate(at) => row.values()[at].clone(),
                    })
                    .collect()
            })
            .collect(),
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
        let expected = reference
            .execute_graph_aggregate_governed(&cx, &definition, policy())
            .unwrap()
            .value;
        assert_eq!(
            expected[0].values(),
            &[
                list(&[7, 3, 7]),
                list(&[7, 3]),
                GraphAggregateValue::Count(4),
                GraphAggregateValue::Integer(17),
            ]
        );
        assert_ne!(
            expected[0].values()[0],
            list(&[3, 7, 7]),
            "sorting arguments changes COLLECT"
        );
        let actual = db
            .execute_graph_aggregate_authorized(
                &cx,
                &issuer,
                &token,
                "main",
                &definition,
                policy(),
                || 100,
            )
            .unwrap();
        assert_eq!(actual, expected);
        let mut session = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
            .unwrap();
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
            let expected = reference
                .execute_graph_aggregate_governed(&cx, &definition, policy())
                .unwrap()
                .value;
            let got = db
                .execute_graph_aggregate_authorized(
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    &definition,
                    policy(),
                    || 100,
                )
                .unwrap();
            assert_eq!(got, expected, "{text}");
        }
        // Computed input is explicitly AFTER the complete child's row order.
        // Do not apply the plain-visitor law across that existing boundary.
        let input = PreparedGraphText::prepare("MATCH (n) RETURN n.p AS p,n AS id", symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap()
            .with_duplicates();
        let definition = PreparedGraphAggregate::prepare_projected(
            input,
            vec![GraphSetProjection::new("value", GraphSetValue::Column(0))],
            &[],
            &[GraphAggregate::collect("items", 0)],
            0,
            None,
        )
        .unwrap();
        let expected = reference
            .execute_graph_aggregate_governed(&cx, &definition, policy())
            .unwrap()
            .value;
        assert_eq!(expected[0].values(), &[list(&[3, 7, 7])]);
        let got = db
            .execute_graph_aggregate_authorized(
                &cx,
                &issuer,
                &token,
                "main",
                &definition,
                policy(),
                || 100,
            )
            .unwrap();
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
        let got = db
            .execute_graph_aggregate_authorized(
                &cx,
                &issuer,
                &token,
                "main",
                &query,
                policy(),
                || 100,
            )
            .unwrap();
        assert_eq!(
            got[0].values(),
            &[
                GraphAggregateValue::Count(4),
                GraphAggregateValue::Integer(17)
            ]
        );
        g.limits.max_nodes = 3;
        let limited = issuer.issue_at(&g, 100).unwrap();
        assert!(matches!(
            db.execute_graph_aggregate_authorized(
                &cx,
                &issuer,
                &limited,
                "main",
                &query,
                policy(),
                || 100
            ),
            Err(QueryError::Authorization(Denied::LimitExceeded(
                LimitDimension::Nodes
            )))
        ));
        let at = db.frontier().unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(
            VId(3),
            P,
            Some(CanonicalScalar::ucs_basic_text("not-an-integer").unwrap()),
        );
        db.write(&commit, change).await.unwrap();
        assert_eq!(
            db.execute_graph_aggregate_authorized_at(
                &cx,
                &issuer,
                &token,
                "main",
                &query,
                at,
                policy(),
                || 100
            )
            .unwrap(),
            got,
        );
        let zero = prepare("MATCH (n) RETURN SUM(n.p) AS total LIMIT 0");
        assert!(matches!(
            db.execute_graph_aggregate_authorized(
                &cx,
                &issuer,
                &token,
                "main",
                &zero,
                policy(),
                || 100
            ),
            Err(QueryError::Aggregate(GqlQueryError::Source(
                GraphAggregateError::NonIntegerSum { .. }
            )))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

// fgdb-xtsw3: parallel edges, parallel self-loops and a hidden vertex that
// closes one more triangle separate DIFFERENT EDGES counts from walk counts and
// the scoped graph from the unscoped one.
async fn cycles(cx: &CommitCx, visible_only: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x25; 32], NS, [0x36; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, value) in [(0, 0), (1, 10), (2, 20), (3, 30), (4, 40), (9, 90)] {
        if visible_only && id == 9 {
            continue;
        }
        let labels = vec![LabelId(if id == 9 { 99 } else { 1 })];
        let mut properties = vec![(P, CanonicalScalar::Int(value))];
        if !visible_only {
            properties.push((H, CanonicalScalar::Int(value + 1)));
        }
        batch.create_vertex(VId(id), labels, properties);
    }
    for (id, from, to) in [
        (20, 0, 1),
        (21, 0, 1),
        (22, 1, 2),
        (23, 2, 0),
        (24, 2, 0),
        (25, 3, 3),
        (26, 3, 3),
        (27, 4, 4),
        (28, 4, 4),
        (29, 4, 4),
        (30, 1, 9),
        (31, 9, 0),
    ] {
        if !visible_only || (from != 9 && to != 9) {
            batch.add_edge(EId(id), VId(from), VId(to), vec![]);
        }
    }
    db.write(cx, batch).await.unwrap();
    db
}

#[test]
fn identified_edge_counts_and_reach_take_the_native_visitor_with_owner_results() {
    let ((), report) = run_async_under_lab(0x5ec0_8504, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = cycles(&c.commit(), false).await;
        let reference = cycles(&c.commit(), true).await;
        let issuer = authority();
        let scoped = issuer.issue_at(&grant(), 100).unwrap();
        let mut open = grant();
        open.labels = Scope::All;
        open.relations = Scope::All;
        open.properties = Scope::All;
        let open = issuer.issue_at(&open, 100).unwrap();
        // (text, visible count, unscoped count, scratch): triangles are 3
        // rotations of 2*1*2 and 2*1*1 bindings plus 3! over three loops; two
        // loops give 0. The visitor answers within a scratch allowance that
        // materializing every pattern row first exceeds. Measured minimum
        // scratch, visitor vs materialized: triangles 105..131 vs 339..467,
        // reach 50..58 vs 507..725, bound reach 43..51 vs 131..183.
        for (text, visible, unscoped, scratch) in [
            (
                "MATCH (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) RETURN COUNT(*) AS n",
                18,
                24,
                220,
            ),
            (
                "MATCH (a)-[:R]->{1,3}(b) RETURN COUNT(DISTINCT b) AS n,MIN(b.p) AS lo,MAX(b.p) AS hi",
                5,
                6,
                220,
            ),
            (
                "MATCH (a {p: 0})-[:R]->{1,3}(b) RETURN COUNT(DISTINCT b) AS n,MAX(b.p) AS hi",
                3,
                4,
                90,
            ),
        ] {
            let definition = prepare(text);
            assert!(
                definition.answers_identified_edges_from_topology(),
                "{text} must exercise the identified-edge route"
            );
            for (token, source, count) in [(&scoped, &reference, visible), (&open, &db, unscoped)] {
                let expected = source
                    .execute_graph_aggregate_governed(&cx, &definition, policy())
                    .unwrap()
                    .value;
                assert_eq!(
                    expected[0].values()[0],
                    GraphAggregateValue::Count(count),
                    "{text}"
                );
                let got = db
                    .execute_graph_aggregate_authorized(
                        &cx,
                        &issuer,
                        token,
                        "main",
                        &definition,
                        GqlQueryPolicy::new(10_000, 1000, 1_000_000, scratch),
                        || 100,
                    )
                    .unwrap();
                assert_eq!(got, expected, "{text}");
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// More vertices than an admitted table searches (it indexes its sealed rows
/// for the edge phase's endpoint lookups): an edge with a hidden endpoint is
/// still dropped exactly as in a database that never held that vertex.
#[test]
fn large_tables_drop_hidden_endpoint_edges_as_the_visible_only_graph_does() {
    const VERTICES: u128 = 6000;
    const EDGES: u128 = 20_000;
    fn hidden(id: u128) -> bool {
        id % 7 == 3
    }
    fn endpoints(edge: u128) -> (u128, u128) {
        ((edge * 7919) % VERTICES, (edge * 104_729 + 13) % VERTICES)
    }
    let ((), report) = run_async_under_lab(0x5ec0_8502, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut graphs = Vec::new();
        for visible_only in [false, true] {
            let mut db =
                Database::open_memory(&commit, DatabaseKeys::new([0x25; 32], NS, [0x36; 32]))
                    .await
                    .unwrap();
            let mut batch = WriteBatch::new(RelationId(1));
            for id in 0..VERTICES {
                if visible_only && hidden(id) {
                    continue;
                }
                let labels = vec![if hidden(id) { LabelId(99) } else { LabelId(1) }];
                let properties = vec![(P, CanonicalScalar::Int((id % 10) as i64))];
                batch.create_vertex(VId(id), labels, properties);
            }
            for edge in 0..EDGES {
                let (from, to) = endpoints(edge);
                if visible_only && (hidden(from) || hidden(to)) {
                    continue;
                }
                batch.add_edge(EId(100_000 + edge), VId(from), VId(to), vec![]);
            }
            db.write(&commit, batch).await.unwrap();
            graphs.push(db);
        }
        let (db, reference) = (&graphs[0], &graphs[1]);
        let issuer = authority();
        let mut grant = grant();
        grant.limits = QueryLimits {
            max_nodes: 1_000_000,
            max_work: 100_000_000,
            max_rows: 1000,
        };
        let token = issuer.issue_at(&grant, 100).unwrap();
        let policy = GqlQueryPolicy::new(1_000_000, 1000, 100_000_000, 100_000_000);
        let dropped = (0..EDGES)
            .filter(|&edge| {
                let (from, to) = endpoints(edge);
                hidden(from) || hidden(to)
            })
            .count() as u64;
        assert!(dropped > 1000, "{dropped}");
        for text in [
            "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS c",
            "MATCH (a:L)-[:R]->(b:L) RETURN COUNT(*) AS c",
            "MATCH (a)-[:R]->(b) RETURN a.p AS k,COUNT(*) AS c GROUP BY a.p",
            "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN COUNT(*) AS c",
        ] {
            let definition = prepare(text);
            let expected = reference
                .execute_graph_aggregate_governed(&cx, &definition, policy)
                .unwrap()
                .value;
            let actual = db
                .execute_graph_aggregate_authorized(
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    &definition,
                    policy,
                    || 100,
                )
                .unwrap();
            assert_eq!(actual, expected, "{text}");
        }
        let edges = prepare("MATCH (a)-[:R]->(b) RETURN COUNT(*) AS c");
        let all = db
            .execute_graph_aggregate_governed(&cx, &edges, policy)
            .unwrap()
            .value;
        assert_eq!(all[0].values(), &[GraphAggregateValue::Count(EDGES as u64)]);
        let visible = reference
            .execute_graph_aggregate_governed(&cx, &edges, policy)
            .unwrap()
            .value;
        assert_eq!(
            visible[0].values(),
            &[GraphAggregateValue::Count(EDGES as u64 - dropped)]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
