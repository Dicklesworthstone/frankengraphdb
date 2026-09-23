//! Product reads through signed capabilities and complete native definitions.
//! These resident-source laws do not establish physical side-channel isolation.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryError, ReadError, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValueRow, IntegerComparison, PreparedGraphPattern};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregate, GraphAggregateColumn,
    GraphAggregateFilter, GraphAggregateOrder, GraphAggregateTest, GraphSetProjection,
    GraphSetValue, GraphSymbol, GraphSymbolKind, PreparedGraphAggregate,
    PreparedGraphAggregateText, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use fgdb_warden::{Authority, Error, Grant, LimitDimension, QueryLimits, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x6d; 32]);
const BRANCH: &str = "host-branch";
fn authority(seed: u64) -> Authority {
    Authority::new(AuthKey::from_seed(seed), NS, "host-graph", SchemaEpoch(0), 1).unwrap()
}
fn grant() -> Grant {
    let mut grant = Grant::read_only(BRANCH, 1000, QueryLimits {
        max_nodes: 1000, max_work: 1_000_000, max_rows: 1000,
    });
    grant.labels = Scope::only([LabelId(1)]);
    grant.relations = Scope::only([RelationId(1)]);
    grant.properties = Scope::only([PropertyKeyId(1)]);
    grant
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "H") => Some(GraphSymbol::Label(LabelId(99))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "hidden") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn pattern(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap().with_duplicates()
}
fn aggregate(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x32; 32], NS, [0x67; 32])).await.unwrap();
    let mut initial = WriteBatch::new(RelationId(1));
    for (vid, labels, p) in [(1, vec![LabelId(1), LabelId(99)], 10), (2, vec![LabelId(99)], 20), (3, vec![LabelId(1)], 30)] {
        initial.create_vertex(VId(vid), labels, vec![
            (PropertyKeyId(1), CanonicalScalar::Int(p)),
            (PropertyKeyId(2), CanonicalScalar::Int(p * 10)),
        ]);
    }
    for (eid, src, dst) in [(10, 1, 2), (11, 2, 3), (12, 1, 3), (13, 1, 3), (14, 3, 1)] {
        initial.add_edge(EId(eid), VId(src), VId(dst), vec![
            (PropertyKeyId(1), CanonicalScalar::Int(5)),
            (PropertyKeyId(2), CanonicalScalar::Int(999)),
        ]);
    }
    db.write(cx, initial).await.unwrap();
    let mut other = WriteBatch::new(RelationId(2));
    other.add_edge(EId(20), VId(1), VId(3), vec![]);
    db.write(cx, other).await.unwrap();
    db
}

#[test]
fn native_graph_aggregate_arguments_and_hidden_transit_paths_use_scoped_sources() {
    let ((), report) = run_async_under_lab(0x5ec0_3001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(301); let token = issuer.issue_at(&grant(), 100).unwrap();
        for (text, count, sum) in [
            ("MATCH (n) RETURN count(*) AS rows, sum(n.p) AS total, count(n.hidden) AS hidden", 2, 40),
            ("MATCH (a)-[e:R]->(b) RETURN count(*) AS rows, sum(e.p) AS total, count(e.hidden) AS hidden", 3, 15),
        ] {
            let query = aggregate(text);
            let frozen = query.canonical_bytes();
            let rows = db.execute_graph_aggregate_authorized(&cx, &issuer, &token, BRANCH, &query, policy(), || 100).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].get(0).unwrap().as_count(), Some(count));
            assert_eq!(rows[0].get(1).unwrap().as_integer(), Some(sum));
            assert_eq!(rows[0].get(2).unwrap().as_count(), Some(0));
            assert_eq!(query.canonical_bytes(), frozen);
        }
        let query = aggregate("MATCH (a)-[:R]->(x)-[:R]->(b) RETURN count(*) AS rows");
        assert_eq!(db.execute_graph_aggregate_authorized(&cx, &issuer, &token, BRANCH, &query, policy(), || 100).unwrap()[0].get(0).unwrap().as_count(), Some(4));
        assert_ne!(db.execute_graph_aggregate_governed(&cx, &query, policy()).unwrap().value[0].get(0).unwrap().as_count(), Some(4));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_group_keys_having_hidden_columns_and_order_remain_one_definition() {
    let ((), report) = run_async_under_lab(0x5ec0_3002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(302); let token = issuer.issue_at(&grant(), 100).unwrap();
        let query = PreparedGraphAggregate::prepare_projected(
            pattern("MATCH (n) RETURN n.p AS p, n.hidden AS hidden"),
            vec![GraphSetProjection::new("key", GraphSetValue::Column(1)),
                GraphSetProjection::new("value", GraphSetValue::Column(0))],
            &[0], &[GraphAggregate::sum_int("total", 1), GraphAggregate::count_rows("rows")], 0, Some(1),
        ).unwrap().with_result_clauses(&[GraphAggregateFilter {
            column: GraphAggregateColumn::Aggregate(1),
            test: GraphAggregateTest::Integer { comparison: IntegerComparison::GreaterOrEqual, value: 2 },
        }], &[GraphAggregateOrder::descending(GraphAggregateColumn::Aggregate(0))]).unwrap()
            .with_key_output_columns(&[]).unwrap().with_aggregate_output_prefix(1).unwrap();
        let rows = db.execute_graph_aggregate_authorized(&cx, &issuer, &token, BRANCH, &query, policy(), || 100).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].keys().is_empty());
        assert_eq!(rows[0].values().len(), 1);
        assert_eq!(rows[0].get(0).unwrap().as_integer(), Some(40));
        assert!(db.execute_graph_aggregate_governed(&cx, &query, policy()).unwrap().value.is_empty(),
            "grouping on original private values must disagree with masking before grouping");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn historical_aggregate_scope_and_exact_delivery_limits_survive_compaction() {
    let ((), report) = run_async_under_lab(0x5ec0_3003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query(); let commit = c.commit();
        let mut db = database(&commit).await;
        let issuer = authority(303);
        let mut allowed = grant(); allowed.limits.max_rows = 1; allowed.limits.max_nodes = 2;
        let token = issuer.issue_at(&allowed, 100).unwrap();
        let query = aggregate("MATCH (n) RETURN sum(n.p) AS total");
        let basis = db.frontier().unwrap();
        let before = db.execute_graph_aggregate_authorized(&cx, &issuer, &token, BRANCH, &query, policy(), || 100).unwrap();
        assert_eq!(before[0].get(0).unwrap().as_integer(), Some(40));
        for (dimension, limits) in [
            (LimitDimension::Nodes, QueryLimits { max_nodes: 1, ..allowed.limits }),
            (LimitDimension::Rows, QueryLimits { max_rows: 0, ..allowed.limits }),
            (LimitDimension::Work, QueryLimits { max_work: 0, ..allowed.limits }),
        ] {
            let mut denied = allowed.clone(); denied.limits = limits;
            let denied = issuer.issue_at(&denied, 100).unwrap();
            assert!(matches!(db.execute_graph_aggregate_authorized(&cx, &issuer, &denied, BRANCH, &query, policy(), || 100),
                Err(QueryError::Authorization(Error::LimitExceeded(actual))) if actual == dimension));
        }
        assert!(matches!(db.execute_graph_aggregate_authorized(&cx, &issuer, &token, BRANCH, &query,
            GqlQueryPolicy::new(1000, 0, 1_000_000, 1_000_000), || 100), Err(QueryError::Aggregate(GqlQueryError::Rows(_)))));
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_label(VId(1), LabelId(1), false);
        db.write(&commit, change).await.unwrap();
        for compact in [false, true] {
            if compact { db.compact(&commit).await.unwrap(); }
            assert_eq!(db.execute_graph_aggregate_authorized_at(&cx, &issuer, &token, BRANCH, &query, basis, policy(), || 100).unwrap(), before);
            assert_eq!(db.execute_graph_aggregate_authorized(&cx, &issuer, &token, BRANCH, &query, policy(), || 100).unwrap()[0].get(0).unwrap().as_integer(), Some(30));
        }
        assert!(matches!(db.execute_graph_aggregate_authorized_at(&cx, &issuer, &token, BRANCH, &query, CommitSeq(u64::MAX), policy(), || 100),
            Err(QueryError::Read(ReadError::BeyondFrontier { .. }))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_aggregate_expiry_boundary_and_final_retirement_prevent_delivery() {
    let ((), report) = run_async_under_lab(0x5ec0_3004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(304); let token = issuer.issue_at(&grant(), 100).unwrap();
        let query = aggregate("MATCH (n) RETURN sum(n.p) AS total");
        let mut calls = 0;
        let expected = db.execute_graph_aggregate_authorized(&cx, &issuer, &token, BRANCH, &query, policy(), || { calls += 1; 100 }).unwrap();
        for stop in 1..=calls {
            let mut seen = 0;
            assert!(matches!(db.execute_graph_aggregate_authorized(&cx, &issuer, &token, BRANCH, &query, policy(), || {
                seen += 1; if seen == stop { 1000 } else { 100 }
            }), Err(QueryError::Authorization(Error::Expired))));
        }
        assert_eq!(db.execute_graph_aggregate_authorized(&cx, &issuer, &token, BRANCH, &query, policy(), || 100).unwrap(), expected);
        let mut seen = 0;
        assert!(matches!(db.execute_graph_aggregate_authorized(&cx, &issuer, &token, BRANCH, &query, policy(), || {
            seen += 1; if seen == calls { issuer.retire(); } 100
        }), Err(QueryError::Authorization(Error::AuthorityRetired))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
