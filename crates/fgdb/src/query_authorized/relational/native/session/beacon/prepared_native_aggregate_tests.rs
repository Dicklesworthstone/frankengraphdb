//! Actual prepared-session execution: retrieval remains a private source and
//! the native reducer, historical property reader and final permit own results.
use super::*;
use crate::{Database, DatabaseKeys, MemVfs, QueryValue, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_beacon::{DistanceMetric, HnswConfig, TextMatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{GraphExactAverage, GraphSymbol, GraphSymbolKind};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{CanonicalF64, CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x91; 32]);
const L: LabelId = LabelId(1);
const BODY: PropertyKeyId = PropertyKeyId(1);
const X: PropertyKeyId = PropertyKeyId(2);
const SECRET: PropertyKeyId = PropertyKeyId(3);
const NUMBER: PropertyKeyId = PropertyKeyId(4);
const HIGH: VId = VId(u128::MAX);
const TRANSIT: VId = VId(4);
const NOW: u64 = 100;
const EXPIRES: u64 = 100_000;

fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}
fn catalog(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Node") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "body") => Some(GraphSymbol::Property(BODY)),
        (GraphSymbolKind::Property, "x") => Some(GraphSymbol::Property(X)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        (GraphSymbolKind::Property, "number") => Some(GraphSymbol::Property(NUMBER)),
        _ => None,
    }
}
fn options() -> Options {
    let mut options = Options::text(BODY);
    options.vertex_label = Some(L);
    options.projection.vector = vec![X];
    options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
    options
}
fn host() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100, 16, 10_000_000, 100_000)
}
fn grant() -> Grant {
    let mut grant = Grant::read_only("main", EXPIRES, QueryLimits {
        max_nodes: 100, max_work: 100_000_000, max_rows: 16,
    });
    grant.labels = Scope::only([L]);
    grant.properties = Scope::only([BODY, X, NUMBER]);
    grant.relations = Scope::only([RelationId(1)]);
    grant
}
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(0x91), NS, "graph", SchemaEpoch(0), 1).unwrap()
}
fn parameters(ids: &[VId]) -> GqlParameters {
    GqlParameters::new().with_list(
        "eligible", ids.iter().copied().map(GraphValue::Vertex).collect(),
    ).unwrap()
}
fn call(lane: &str) -> String {
    let (text, vector, graph) = match lane {
        "text" => (true, false, false),
        "vector" | "ann" => (false, true, false),
        "graph" => (false, false, true),
        _ => (true, true, true),
    };
    format!(
        "CALL hybrid.search({}{}{}{}label => 'Node', eligible => $eligible, \
         candidates => 2, k => 2) YIELD node, graph_hops ",
        if text { "text => 'red', text_property => 'body', " } else { "" },
        if vector { "vector => [0], vector_properties => ['x'], " } else { "" },
        if lane == "ann" { "ann => 16, " } else { "" },
        if graph { "seeds => [1], relation => 'R', max_hops => 2, graph_candidates => 2, " } else { "" },
    )
}
fn summary(lane: &str) -> String {
    format!("{}WITH node.x AS value RETURN COUNT(*) AS n, SUM(value) AS total, \
        AVG(value) AS mean, MIN(value) AS low, MAX(value) AS high", call(lane))
}
fn rows(result: QueryResult) -> Vec<Vec<QueryValue>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        QueryResult::Write { .. } => panic!("read-only aggregation wrote data"),
    }
}
fn expected() -> Vec<Vec<QueryValue>> {
    vec![vec![
        QueryValue::Count(2), QueryValue::Integer(9),
        QueryValue::Average(GraphExactAverage::new(9, 2).unwrap()),
        QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(2))),
        QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(7))),
    ]]
}
async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x92; 32], NS, [0x93; 32]))
        .await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, x, number) in [(VId(1), 0, 1e16), (VId(2), 2, 1.0), (HIGH, 7, -1e16)] {
        batch.create_vertex(id, vec![L], vec![
            (BODY, text("red")), (X, CanonicalScalar::Int(x)),
            (SECRET, text("never an aggregate input")),
            (NUMBER, CanonicalScalar::Float(CanonicalF64::new(number))),
        ]);
    }
    batch.create_vertex(TRANSIT, vec![L], vec![]);
    batch.create_vertex(VId(90), vec![LabelId(99)], vec![(BODY, CanonicalScalar::Bool(true))]);
    for (eid, from, to) in [(1, VId(1), TRANSIT), (2, TRANSIT, VId(2)), (3, TRANSIT, HIGH),
        (4, VId(1), VId(90)), (5, VId(90), HIGH)] {
        batch.add_edge(EId(eid), from, to, vec![]);
    }
    db.write(cx, batch).await.unwrap();
    db
}

#[test]
fn every_retrieval_lane_feeds_exact_aggregates_and_only_final_rows_are_delivered() {
    let ((), report) = run_async_under_lab(0xbeac_6401, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit()).await;
        let issuer = authority();
        let mut allowed = grant();
        allowed.limits.max_rows = 1;
        let token = issuer.issue_at(&allowed, NOW).unwrap();
        let mut session = db.authorized_read_session(
            &c.query(), &issuer, &token, "main", catalog,
            GqlQueryPolicy::new(100, 1, 10_000_000, 100_000), || NOW,
        ).unwrap();
        let index = session.prepare_beacon_index(&c.query(), &options()).unwrap();
        let params = parameters(&[VId(2), HIGH]);
        for lane in ["text", "vector", "ann", "graph", "mixed"] {
            let prepared = session.prepare(&c.query(), &summary(lane), &params).unwrap();
            assert!(matches!(prepared.native.as_ref(), PreparedNativeRead::PipelineAggregate(_)));
            assert_eq!(rows(session.execute_beacon_index(&c.query(), &index, &prepared, &params).unwrap()), expected());
            let page = session.prepare(&c.query(), &format!("{}RETURN node LIMIT 1", call(lane)), &params).unwrap();
            assert_eq!(rows(session.execute_beacon_index(&c.query(), &index, &page, &params).unwrap()).len(), 1);
            let unpaged = session.prepare(&c.query(), &format!("{}RETURN node", call(lane)), &params).unwrap();
            assert!(session.execute_beacon_index(&c.query(), &index, &unpaged, &params).is_err());
        }
        assert!(session.search_beacon_index(&c.query(), &index, Search::Text {
            query: "red", k: 2, mode: TextMatch::Any,
        }, ReadPolicy::default()).is_err(), "direct hit delivery must keep its signed row limit");
        let mut narrow = options();
        narrow.policy.max_result_rows = 1;
        let narrow = session.prepare_beacon_index(&c.query(), &narrow).unwrap();
        let query = session.prepare(&c.query(), &summary("mixed"), &params).unwrap();
        assert!(session.execute_beacon_index(&c.query(), &narrow, &query, &params).is_err(),
            "an aggregate cannot widen the explicitly frozen native source ceiling");
        assert!(!session.is_closed());
        session.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn grouping_distinct_collection_and_float_summaries_keep_native_value_domains() {
    let ((), report) = run_async_under_lab(0xbeac_6402, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db.authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW).unwrap();
        let index = session.prepare_beacon_index(&c.query(), &options()).unwrap();
        let params = parameters(&[VId(2), HIGH]);
        let text = format!("{}UNWIND [0,0] AS occurrence WITH node.x AS x \
            RETURN x, COUNT(*) AS n, SUM(DISTINCT x) AS total, COLLECT(x) AS values \
            HAVING n = 2 ORDER BY x DESC SKIP 1 LIMIT 1", call("mixed"));
        let prepared = session.prepare(&c.query(), &text, &params).unwrap();
        assert_eq!(rows(session.execute_beacon_index(&c.query(), &index, &prepared, &params).unwrap()), vec![vec![
            QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(2))),
            QueryValue::Count(2), QueryValue::Integer(2),
            QueryValue::Value(GraphValue::List([2,2].map(|n| GraphValue::Scalar(CanonicalScalar::Int(n))).into())),
        ]]);
        let text = format!("{}WITH node.number AS n RETURN SUM(n) AS total, AVG(n) AS mean",
            call("text").replace("candidates => 2, k => 2", "candidates => 3, k => 3"));
        let params = parameters(&[VId(1), VId(2), HIGH]);
        let prepared = session.prepare(&c.query(), &text, &params).unwrap();
        let actual = rows(session.execute_beacon_index(&c.query(), &index, &prepared, &params).unwrap());
        for (cell, expected) in actual[0].iter().zip([1.0, 1.0 / 3.0]) {
            assert_eq!(cell, &QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(expected)))));
        }
        session.close();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn aggregates_preserve_the_prepared_graph_and_property_cut_after_compaction_and_drop() {
    let ((), report) = run_async_under_lab(0xbeac_6403, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = database(&c.commit()).await;
        let old = db.frontier().unwrap();
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(HIGH, X, Some(CanonicalScalar::Int(99)));
        edit.delete_edge(EId(3));
        db.write(&c.commit(), edit).await.unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db.authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW).unwrap();
        let mut historical = options();
        historical.as_of = Some(old);
        let index = session.prepare_beacon_index(&c.query(), &historical).unwrap();
        let current = session.prepare_beacon_index(&c.query(), &options()).unwrap();
        let params = parameters(&[VId(2), HIGH]);
        let prepared = session.prepare(&c.query(), &summary("graph"), &params).unwrap();
        assert_ne!(rows(session.execute_beacon_index(&c.query(), &current, &prepared, &params).unwrap()), expected());
        db.compact(&c.commit()).await.unwrap();
        drop(db);
        assert_eq!(rows(session.execute_beacon_index(&c.query(), &index, &prepared, &params).unwrap()), expected());
        let empty = parameters(&[]);
        let result = rows(session.execute_beacon_index(&c.query(), &index, &prepared, &empty).unwrap());
        assert_eq!(result[0][0], QueryValue::Count(0));
        assert!(result[0][1..].iter().all(QueryValue::is_null));
        session.close();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn aggregate_properties_require_retrieved_provenance_and_mask_before_reduction() {
    let ((), report) = run_async_under_lab(0xbeac_6404, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db.authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW).unwrap();
        let index = session.prepare_beacon_index(&c.query(), &options()).unwrap();
        let params = parameters(&[VId(2), HIGH]);
        let masked = session.prepare(&c.query(), &format!("{}WITH node.secret AS s RETURN COUNT(s) AS n, SUM(s) AS total", call("mixed")), &params).unwrap();
        let result = rows(session.execute_beacon_index(&c.query(), &index, &masked, &params).unwrap());
        assert_eq!(result[0][0], QueryValue::Count(0));
        assert!(result[0][1].is_null());
        for outside in [TRANSIT, VId(90)] {
            let params = parameters(&[VId(2), HIGH]).with_list("outside", vec![GraphValue::Vertex(outside)]).unwrap();
            let query = session.prepare(&c.query(), &format!("{}WITH $outside[0] AS n \
                RETURN SUM(n.x) AS total LIMIT 0", call("mixed")), &params).unwrap();
            assert!(session.execute_beacon_index(&c.query(), &index, &query, &params).is_err());
            assert!(!session.is_closed());
        }
        let bad = session.prepare(&c.query(), &format!("{}WITH node.x / 0 AS x RETURN SUM(x) AS total LIMIT 0", call("mixed")), &params).unwrap();
        assert!(session.execute_beacon_index(&c.query(), &index, &bad, &params).is_err());
        let pattern = session.prepare(&c.query(), "MATCH (n:Node) WITH n.x AS x RETURN SUM(x) AS total", &GqlParameters::new()).unwrap();
        assert!(session.execute_beacon_index(&c.query(), &index, &pattern, &GqlParameters::new()).is_err());
        let good = session.prepare(&c.query(), &summary("mixed"), &params).unwrap();
        assert_eq!(rows(session.execute_beacon_index(&c.query(), &index, &good, &params).unwrap()), expected());
        session.close();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn successive_aggregate_call_sources_share_records_and_signed_nodes() {
    let ((), report) = run_async_under_lab(0xbeac_6405, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit()).await;
        let issuer = authority();
        let params = parameters(&[VId(2), HIGH]);
        let single = format!("{}RETURN COUNT(*) AS n", call("graph"));
        let second = call("graph").replace("YIELD node, graph_hops", "YIELD node AS other, graph_hops AS other_hops");
        let both = format!("{}WITH node {}RETURN COUNT(*) AS n", call("graph"), second);
        // Four saved vertices plus three admitted incidences per CALL. The
        // hidden shortcut contributes neither a source record nor a node.
        for (records, nodes, succeeds) in [(14, 8, true), (13, 8, false), (14, 7, false)] {
            let mut allowed = grant();
            allowed.limits.max_nodes = nodes;
            allowed.limits.max_rows = 1;
            let token = issuer.issue_at(&allowed, NOW).unwrap();
            let mut session = db.authorized_read_session(&c.query(), &issuer, &token, "main", catalog,
                GqlQueryPolicy::new(records, 1, 10_000_000, 100_000), || NOW).unwrap();
            let index = session.prepare_beacon_index(&c.query(), &options()).unwrap();
            let one = session.prepare(&c.query(), &single, &params).unwrap();
            let two = session.prepare(&c.query(), &both, &params).unwrap();
            assert_eq!(rows(session.execute_beacon_index(&c.query(), &index, &one, &params).unwrap()), vec![vec![QueryValue::Count(2)]]);
            let result = session.execute_beacon_index(&c.query(), &index, &two, &params);
            if succeeds {
                assert_eq!(rows(result.unwrap()), vec![vec![QueryValue::Count(4)]]);
            } else {
                assert!(result.is_err());
                assert!(!session.is_closed());
            }
            assert!(session.execute_beacon_index(&c.query(), &index, &one, &params).is_ok());
            session.close();
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cancellation_and_final_expiry_cannot_publish_even_an_empty_global_summary() {
    let ((), report) = run_async_under_lab(0xbeac_6406, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db.authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW).unwrap();
        let index = session.prepare_beacon_index(&c.query(), &options()).unwrap();
        let params = parameters(&[VId(2), HIGH]);
        let prepared = session.prepare(&c.query(), &summary("mixed"), &params).unwrap();
        for params in [params, parameters(&[])] {
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let expected = rows(session.execute_beacon_index(&c.query().with_checkpoint_probe(probe.clone()), &index, &prepared, &params).unwrap());
            for cut in 1..=probe.calls() {
                let stopped = Arc::new(SimulationCheckpointProbe::new(Some(cut)));
                assert!(session.execute_beacon_index(&c.query().with_checkpoint_probe(stopped), &index, &prepared, &params).is_err(), "cut={cut}");
                assert!(!session.is_closed());
            }
            assert_eq!(rows(session.execute_beacon_index(&c.query(), &index, &prepared, &params).unwrap()), expected);
        }
        session.close();
        let calls = Cell::new(0usize);
        let stop = Cell::new(usize::MAX);
        let mut session = db.authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || {
            let n = calls.get() + 1;
            calls.set(n);
            if n >= stop.get() { EXPIRES + 1 } else { NOW }
        }).unwrap();
        let index = session.prepare_beacon_index(&c.query(), &options()).unwrap();
        let params = parameters(&[]);
        let prepared = session.prepare(&c.query(), &summary("mixed"), &params).unwrap();
        calls.set(0);
        assert_eq!(rows(session.execute_beacon_index(&c.query(), &index, &prepared, &params).unwrap())[0][0], QueryValue::Count(0));
        stop.set(calls.get());
        calls.set(0);
        assert!(matches!(session.execute_beacon_index(&c.query(), &index, &prepared, &params), Err(QueryError::Authorization(WardenError::Expired))));
        assert!(session.is_closed());
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
