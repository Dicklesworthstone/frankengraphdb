//! Product reads through signed capabilities and complete native definitions.
//! These resident-source laws do not establish physical side-channel isolation.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, NativeReadClass, PreparedNativeRead, QueryError, QueryResult, QueryValue, ReadError, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValueRow, IntegerComparison, PreparedGraphPattern};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregate, GraphAggregateColumn,
    GraphAggregateFilter, GraphAggregateOrder, GraphAggregateTest, GraphSetProjection,
    GraphSetValue, GraphSymbol, GraphSymbolKind, PreparedGraphAggregate,
    PreparedGraphAggregateText, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use fgdb_warden::{Authority, Error, Grant, LimitDimension, QueryLimits, Restriction, Rights, Scope};

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

fn native_rows(columns: &[&str], rows: Vec<Vec<QueryValue>>) -> QueryResult {
    QueryResult::Rows { columns: columns.iter().map(|name| (*name).to_owned()).collect(), rows }
}
fn value(value: i64) -> QueryValue {
    QueryValue::Value(fgdb_gql::algebra::GraphValue::Scalar(CanonicalScalar::Int(value)))
}
fn p_rows() -> QueryResult {
    native_rows(&["p"], vec![vec![value(10)], vec![value(30)]])
}

#[test]
fn all_seven_native_facades_use_scoped_data_and_lossless_output_slots() {
    let ((), report) = run_async_under_lab(0x5ec0_4001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(401); let token = issuer.issue_at(&grant(), 100).unwrap();
        let params = GqlParameters::new();
        let summary = native_rows(&["rows", "total"], vec![vec![QueryValue::Count(2), QueryValue::Integer(40)]]);
        let union = native_rows(&["p"], vec![vec![value(10)], vec![value(10)], vec![value(30)], vec![value(30)]]);
        for (class, text, expected) in [
            (NativeReadClass::Pattern, "MATCH (n) RETURN n.p AS p", p_rows()),
            (NativeReadClass::Aggregate, "MATCH (n) RETURN count(*) AS rows, sum(n.p) AS total", summary.clone()),
            (NativeReadClass::PipelineAggregate, "MATCH (n) WITH n.p AS p RETURN count(*) AS rows, sum(p) AS total", summary.clone()),
            (NativeReadClass::Set, "MATCH (n) RETURN n.p AS p UNION ALL MATCH (n) RETURN n.p AS p", union.clone()),
            (NativeReadClass::TemporalPattern, "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 2 RETURN n.p AS p", p_rows()),
            (NativeReadClass::TemporalAggregate, "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 2 RETURN count(*) AS rows, sum(n.p) AS total", summary),
            (NativeReadClass::TemporalSet, "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 2 RETURN n.p AS p UNION ALL MATCH (n) RETURN n.p AS p", union),
        ] {
            let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
            assert_eq!(prepared.facade_class(), class, "{text}");
            assert_eq!(db.query_authorized(&cx, &issuer, &token, BRANCH, text, &params, symbols, policy(), || 100).unwrap(), expected);
            assert_eq!(prepared.execute_authorized(&db, &cx, &issuer, &token, BRANCH, &params, policy(), || 100).unwrap(), expected);
            // The trusted unscoped control sees the hidden vertex too. Agreement
            // between two incorrectly privileged adapters cannot pass this law.
            assert_ne!(prepared.execute(&db, &cx, &params, policy()).unwrap(), expected);
        }
        let text = "MATCH (n) RETURN sum(n.p) AS total, count(*) AS rows";
        assert_eq!(db.query_authorized(&cx, &issuer, &token, BRANCH, text, &params, symbols, policy(), || 100).unwrap(),
            native_rows(&["total", "rows"], vec![vec![QueryValue::Integer(40), QueryValue::Count(2)]]));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_native_rebinding_never_caches_the_previous_scope_or_argument_values() {
    let ((), report) = run_async_under_lab(0x5ec0_4002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(402); let token = issuer.issue_at(&grant(), 100).unwrap();
        let first = GqlParameters::new().with_int64("minimum", 5).unwrap();
        let second = GqlParameters::new().with_int64("minimum", 20).unwrap();
        let text = "MATCH (n) WHERE n.p >= $minimum RETURN n.p AS p";
        let prepared = PreparedNativeRead::prepare(text, &first, symbols).unwrap();
        let frozen = prepared.statement().to_owned();
        assert_eq!(prepared.execute_authorized(&db, &cx, &issuer, &token, BRANCH, &first, policy(), || 100).unwrap(), p_rows());
        assert_eq!(prepared.execute_authorized(&db, &cx, &issuer, &token, BRANCH, &second, policy(), || 100).unwrap(),
            native_rows(&["p"], vec![vec![value(30)]]));
        let masked = token.attenuate(Restriction::DenyProperties([PropertyKeyId(1)].into_iter().collect())).unwrap();
        assert_eq!(prepared.execute_authorized(&db, &cx, &issuer, &masked, BRANCH, &first, policy(), || 100).unwrap(), native_rows(&["p"], vec![]));
        let no_vertices = token.attenuate(Restriction::Labels(Scope::only([]))).unwrap();
        assert_eq!(prepared.execute_authorized(&db, &cx, &issuer, &no_vertices, BRANCH, &first, policy(), || 100).unwrap(), native_rows(&["p"], vec![]));
        for invalid in [GqlParameters::new(), GqlParameters::new().with_text("minimum", "20").unwrap(),
            first.clone().with_int64("extra", 0).unwrap()] {
            assert!(matches!(prepared.execute_authorized(&db, &cx, &issuer, &token, BRANCH, &invalid, policy(), || 100),
                Err(QueryError::PatternText(_))));
        }
        // Credential admission wins even over a missing required argument.
        assert!(matches!(prepared.execute_authorized(&db, &cx, &issuer, &token, BRANCH, &GqlParameters::new(), policy(), || 1000),
            Err(QueryError::Authorization(Error::Expired))));
        assert_eq!(prepared.statement(), frozen);
        assert_eq!(prepared.execute_authorized(&db, &cx, &issuer, &token, BRANCH, &first, policy(), || 100).unwrap(), p_rows());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authentication_precedes_text_catalog_and_future_cut_admission() {
    let ((), report) = run_async_under_lab(0x5ec0_4003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(403); let token = issuer.issue_at(&grant(), 100).unwrap();
        let wrong_key = authority(404);
        let wrong_namespace = Authority::new(AuthKey::from_seed(403), DatabaseSecurityNamespaceId([0; 32]), "host-graph", SchemaEpoch(0), 1).unwrap();
        let mut no_read = grant(); no_read.rights = Rights::Write;
        let no_read = issuer.issue_at(&no_read, 100).unwrap();
        for (authority, token, now, error) in [
            (&wrong_namespace, &token, 100, Error::WrongAuthority),
            (&wrong_key, &token, 100, Error::Unauthenticated),
            (&issuer, &token, 1000, Error::Expired),
            (&issuer, &no_read, 100, Error::PermissionDenied),
        ] {
            for text in ["MATCH (n:L) RETURN n", "not a query (", "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ 999999 RETURN n"] {
                assert!(matches!(db.query_authorized(&cx, authority, token, BRANCH, text,
                    &GqlParameters::new(), |_, _: &str| -> Option<GraphSymbol> { panic!("unauthenticated catalog access") },
                    policy(), || now), Err(QueryError::Authorization(actual)) if actual == error));
            }
        }
        assert!(matches!(db.query_authorized(&cx, &issuer, &token, "different-host-branch", "MATCH (n:L) RETURN n", &GqlParameters::new(),
            |_, _: &str| -> Option<GraphSymbol> { panic!("wrong-branch catalog access") }, policy(), || 100),
            Err(QueryError::Authorization(Error::ScopeDenied))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn textual_branch_selectors_can_only_confirm_the_already_authorized_host_branch() {
    let ((), report) = run_async_under_lab(0x5ec0_4004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(405); let token = issuer.issue_at(&grant(), 100).unwrap();
        let parameters = GqlParameters::new().with_text("branch", BRANCH).unwrap();
        for (text, args) in [
            ("AT BRANCH 'host-branch' MATCH (n) RETURN n.p AS p", GqlParameters::new()),
            ("MATCH (n) RETURN n.p AS p AT BRANCH 'host-branch'", GqlParameters::new()),
            ("AT BRANCH $branch MATCH (n) RETURN n.p AS p", parameters.clone()),
        ] {
            assert_eq!(db.query_authorized(&cx, &issuer, &token, BRANCH, text, &args, symbols, policy(), || 100).unwrap(), p_rows());
        }
        for selected in ["other", "HOST-BRANCH", "host-branch' MATCH (n) RETURN n"] {
            let args = GqlParameters::new().with_text("branch", selected).unwrap();
            assert!(matches!(db.query_authorized(&cx, &issuer, &token, BRANCH,
                "AT BRANCH $branch MATCH (n:L) RETURN n.p AS p", &args,
                |_, _: &str| -> Option<GraphSymbol> { panic!("selector switched catalog") }, policy(), || 100),
                Err(QueryError::Authorization(Error::ScopeDenied))));
        }
        // The selector consumes ONLY its own exclusive argument; unrelated
        // arguments remain visible to the ordinary native schema validator.
        let extra = parameters.clone().with_int64("unused", 1).unwrap();
        assert!(db.query_authorized(&cx, &issuer, &token, BRANCH,
            "AT BRANCH $branch MATCH (n) RETURN n.p AS p", &extra, symbols, policy(), || 100).is_err());
        // A selector argument also used in the row expression remains bound.
        let result = db.query_authorized(&cx, &issuer, &token, BRANCH,
            "AT BRANCH $branch RETURN $branch AS name", &parameters, symbols, policy(), || 100).unwrap();
        assert_eq!(result, native_rows(&["name"], vec![vec![QueryValue::Value(
            fgdb_gql::algebra::GraphValue::Scalar(CanonicalScalar::ucs_basic_text(BRANCH).unwrap()),
        )]]));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn temporal_native_rebinding_uses_one_exact_cut_but_current_credential_validity() {
    let ((), report) = run_async_under_lab(0x5ec0_4005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query(); let commit = c.commit();
        let mut db = database(&commit).await;
        let issuer = authority(406); let token = issuer.issue_at(&grant(), 100).unwrap();
        let basis = db.frontier().unwrap();
        let args = GqlParameters::new().with_uint64("at", basis.0).unwrap();
        let text = "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN n.p AS p UNION ALL MATCH (n) RETURN n.p AS p";
        let prepared = PreparedNativeRead::prepare(text, &args, symbols).unwrap();
        assert_eq!(prepared.facade_class(), NativeReadClass::TemporalSet);
        let expected = native_rows(&["p"], vec![vec![value(10)], vec![value(10)], vec![value(30)], vec![value(30)]]);
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_label(VId(1), LabelId(1), false);
        let after = db.write(&commit, change).await.unwrap();
        let current = GqlParameters::new().with_uint64("at", after.0).unwrap();
        for compact in [false, true] {
            if compact { db.compact(&commit).await.unwrap(); }
            assert_eq!(prepared.execute_authorized(&db, &cx, &issuer, &token, BRANCH, &args, policy(), || 100).unwrap(), expected);
            assert_eq!(prepared.execute_authorized(&db, &cx, &issuer, &token, BRANCH, &current, policy(), || 100).unwrap(),
                native_rows(&["p"], vec![vec![value(30)], vec![value(30)]]));
            assert_eq!(db.query_authorized(&cx, &issuer, &token, BRANCH, text, &args, symbols, policy(), || 100).unwrap(), expected);
        }
        let future = GqlParameters::new().with_uint64("at", after.0 + 1).unwrap();
        assert!(matches!(prepared.execute_authorized(&db, &cx, &issuer, &token, BRANCH, &future, policy(), || 100),
            Err(QueryError::Read(ReadError::BeyondFrontier { asked, frontier })) if asked.0 == after.0 + 1 && frontier == after));
        assert!(matches!(prepared.execute_authorized(&db, &cx, &issuer, &token, BRANCH, &args, policy(), || 1000),
            Err(QueryError::Authorization(Error::Expired))));
        let zero = GqlParameters::new().with_uint64("at", 0).unwrap();
        assert_eq!(db.query_authorized(&cx, &issuer, &token, BRANCH,
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN count(*) AS rows", &zero, symbols, policy(), || 100).unwrap(),
            native_rows(&["rows"], vec![vec![QueryValue::Count(0)]]));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn compound_native_permits_do_not_reset_and_private_rows_do_not_consume_delivery() {
    let ((), report) = run_async_under_lab(0x5ec0_4006, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(407);
        let mut limited = grant(); limited.limits.max_nodes = 2;
        let token = issuer.issue_at(&limited, 100).unwrap();
        let params = GqlParameters::new();
        let union = "MATCH (n) RETURN n.p AS p UNION ALL MATCH (n) RETURN n.p AS p";
        assert_eq!(db.query_authorized(&cx, &issuer, &token, BRANCH, "MATCH (n) RETURN n.p AS p", &params, symbols, policy(), || 100).unwrap(), p_rows());
        assert!(matches!(db.query_authorized(&cx, &issuer, &token, BRANCH, union, &params, symbols, policy(), || 100),
            Err(QueryError::Authorization(Error::LimitExceeded(LimitDimension::Nodes)))));
        limited.limits.max_nodes = 4; limited.limits.max_rows = 4;
        let token = issuer.issue_at(&limited, 100).unwrap();
        assert_eq!(db.query_authorized(&cx, &issuer, &token, BRANCH, union, &params, symbols, policy(), || 100).unwrap(),
            native_rows(&["p"], vec![vec![value(10)], vec![value(10)], vec![value(30)], vec![value(30)]]));
        limited.limits.max_rows = 3;
        let token = issuer.issue_at(&limited, 100).unwrap();
        assert!(matches!(db.query_authorized(&cx, &issuer, &token, BRANCH, union, &params, symbols, policy(), || 100),
            Err(QueryError::Authorization(Error::LimitExceeded(LimitDimension::Rows)))));
        limited.limits.max_rows = 1; limited.limits.max_nodes = 2;
        let token = issuer.issue_at(&limited, 100).unwrap();
        assert_eq!(db.query_authorized(&cx, &issuer, &token, BRANCH,
            "MATCH (n) WITH n.p AS p RETURN count(*) AS rows, sum(p) AS total", &params, symbols, policy(), || 100).unwrap(),
            native_rows(&["rows", "total"], vec![vec![QueryValue::Count(2), QueryValue::Integer(40)]]));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn source_free_and_empty_pages_authenticate_and_unsupported_text_never_falls_back() {
    let ((), report) = run_async_under_lab(0x5ec0_4007, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(408);
        let mut limited = grant(); limited.limits.max_nodes = 0; limited.limits.max_rows = 1;
        let token = issuer.issue_at(&limited, 100).unwrap();
        let params = GqlParameters::new();
        assert_eq!(db.query_authorized(&cx, &issuer, &token, BRANCH, "RETURN 1 AS one", &params, symbols, policy(), || 100).unwrap(),
            native_rows(&["one"], vec![vec![value(1)]]));
        let text = "UNWIND [1, 2, 2] AS x RETURN count(*) AS rows, sum(x) AS total";
        let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
        assert!(matches!(&prepared, PreparedNativeRead::PipelineAggregate(query) if query.is_source_free()));
        assert_eq!(db.query_authorized(&cx, &issuer, &token, BRANCH, text, &params, symbols, policy(), || 100).unwrap(),
            native_rows(&["rows", "total"], vec![vec![QueryValue::Count(3), QueryValue::Integer(5)]]));
        limited.limits.max_rows = 0;
        let token = issuer.issue_at(&limited, 100).unwrap();
        assert_eq!(db.query_authorized(&cx, &issuer, &token, BRANCH, "RETURN 1 AS one LIMIT 0", &params, symbols, policy(), || 100).unwrap(),
            native_rows(&["one"], vec![]));
        assert!(matches!(db.query_authorized(&cx, &issuer, &token, BRANCH, "RETURN 1 AS one LIMIT 0", &params, symbols, policy(), || 1000),
            Err(QueryError::Authorization(Error::Expired))));
        // This must be an EXECUTION failure, not a parser refusal or a silently
        // skipped expression merely because the final page contains no rows.
        let invalid = "UNWIND [1, 0] AS x RETURN 10 / x AS quotient LIMIT 0";
        let prepared = PreparedNativeRead::prepare(invalid, &params, symbols).unwrap();
        assert!(matches!(prepared.execute_authorized(&db, &cx, &issuer, &token, BRANCH, &params, policy(), || 100),
            Err(QueryError::Set(GqlQueryError::Source(_))) | Err(QueryError::Aggregate(GqlQueryError::Source(_)))));
        let before = db.frontier().unwrap();
        for refused in ["EXPLAIN MATCH (n) RETURN n", "INSERT (n:L)", "CALL fnx.pagerank()", "MATCH ("] {
            assert!(db.query_authorized(&cx, &issuer, &token, BRANCH, refused, &params, symbols, policy(), || 100).is_err());
            assert_eq!(db.frontier().unwrap(), before);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_preparation_and_final_delivery_recheck_expiry_and_resolver_retirement() {
    let ((), report) = run_async_under_lab(0x5ec0_4008, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(409); let token = issuer.issue_at(&grant(), 100).unwrap();
        let params = GqlParameters::new();
        let text = "MATCH (n:L) WITH n.p AS p RETURN count(*) AS rows, sum(p) AS total";
        let mut calls = 0;
        let expected = db.query_authorized(&cx, &issuer, &token, BRANCH, text, &params, symbols, policy(), || { calls += 1; 100 }).unwrap();
        for stop in 1..=calls {
            let mut seen = 0;
            assert!(matches!(db.query_authorized(&cx, &issuer, &token, BRANCH, text, &params, symbols, policy(), || {
                seen += 1; if seen == stop { 1000 } else { 100 }
            }), Err(QueryError::Authorization(Error::Expired))));
            assert_eq!(seen, stop);
        }
        assert_eq!(db.query_authorized(&cx, &issuer, &token, BRANCH, text, &params, symbols, policy(), || 100).unwrap(), expected);
        let mut seen = 0;
        assert!(matches!(db.query_authorized(&cx, &issuer, &token, BRANCH, text, &params, symbols, policy(), || {
            seen += 1; if seen == calls { issuer.retire(); } 100
        }), Err(QueryError::Authorization(Error::AuthorityRetired))));
        for resolve in [true, false] {
            let issuer = authority(410); let token = issuer.issue_at(&grant(), 100).unwrap();
            let mut entered = false;
            assert!(matches!(db.query_authorized(&cx, &issuer, &token, BRANCH, "MATCH (n:L) RETURN n", &params,
                |kind, name: &str| {
                    entered = true;
                    issuer.retire();
                    if resolve { symbols(kind, name) } else { None }
                }, policy(), || 100), Err(QueryError::Authorization(Error::AuthorityRetired))));
            assert!(entered, "the retirement must occur inside real native catalog resolution");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
