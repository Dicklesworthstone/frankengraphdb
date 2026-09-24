//! Real Warden sessions drive the existing indexed probe cursor over committed
//! MemVfs generations. These tests do not establish physical noninterference.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryError, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::stream::VertexScanState;
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use fgdb_warden::{Authority, Error, Grant, LimitDimension, QueryLimits, Scope};
use std::cell::Cell;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x73; 32]);
const BRANCH: &str = "host-main";
fn issuer() -> Authority {
    Authority::new(AuthKey::from_seed(7301), NS, "host-graph", SchemaEpoch(0), 1).unwrap()
}
fn grant() -> Grant {
    let mut value = Grant::read_only(BRANCH, 1000, QueryLimits {
        max_nodes: 1_000_000, max_work: 10_000_000, max_rows: 1000,
    });
    value.labels = Scope::only([LabelId(1)]);
    value.relations = Scope::only([RelationId(1)]);
    value.properties = Scope::only([PropertyKeyId(1)]);
    value
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(1_000_000, 1000, 10_000_000, 10_000_000) }
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
async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x18; 32], NS, [0x29; 32])).await.unwrap();
    let mut seed = WriteBatch::new(RelationId(1));
    for (id, labels) in [
        (1, vec![LabelId(1), LabelId(99)]), (2, vec![LabelId(99)]),
        (3, vec![LabelId(1)]), (4, vec![LabelId(1)]),
    ] {
        seed.create_vertex(VId(id), labels, vec![
            (PropertyKeyId(1), CanonicalScalar::Int(id as i64 * 10)),
            (PropertyKeyId(2), CanonicalScalar::Int(999)),
        ]);
    }
    for (eid, a, b) in [(10, 1, 2), (11, 2, 4), (12, 1, 3), (13, 1, 3), (14, 3, 1), (15, 3, 3)] {
        seed.add_edge(EId(eid), VId(a), VId(b), vec![(PropertyKeyId(2), CanonicalScalar::Int(999))]);
    }
    db.write(cx, seed).await.unwrap();
    let mut forbidden = WriteBatch::new(RelationId(2));
    forbidden.add_edge(EId(20), VId(1), VId(4), vec![]);
    db.write(cx, forbidden).await.unwrap();
    db
}
fn expected(ids: &[u128]) -> Vec<GraphValueRow> {
    ids.iter().map(|&id| GraphValueRow::from_owned_values(vec![
        GraphValue::Vertex(VId(id)), GraphValue::Scalar(CanonicalScalar::Int(id as i64 * 10)),
    ])).collect()
}
fn native(columns: Vec<String>, rows: &[GraphValueRow]) -> QueryResult {
    QueryResult::Rows {
        columns,
        rows: rows.iter().map(|row| row.values().iter().cloned().map(QueryValue::Value).collect()).collect(),
    }
}

#[test]
fn scoped_fixed_and_independent_probes_match_goldens_and_eager_results_before_paging() {
    let ((), report) = run_async_under_lab(0x5ec0_7301, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await;
        let authority = issuer(); let token = authority.issue_at(&grant(), 100).unwrap();
        let mut session = db.authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || 100).unwrap();
        let args = GqlParameters::new();
        for (condition, answer) in [
            ("EXISTS { MATCH (a)-[:R]->(b) }", vec![1, 3]),
            ("NOT EXISTS { MATCH (a)-[:R]->(b) }", vec![4]),
            ("EXISTS { MATCH (a)<-[:R]-(b) }", vec![1, 3]),
            ("EXISTS { MATCH (a)-[:R]-(b) }", vec![1, 3]),
            ("EXISTS { MATCH (a)-[:S]->(b) }", vec![]),
            ("NOT EXISTS { MATCH (a)-[:S]->(b) }", vec![1, 3, 4]),
            ("EXISTS { MATCH (a)-[:R]->(b:H) }", vec![]),
            ("EXISTS { MATCH (a)-[:R]->(b) WHERE b.hidden IS NULL }", vec![1, 3]),
            ("EXISTS { MATCH (a)-[:R]->(b) WHERE b.hidden = 999 }", vec![]),
            ("EXISTS { MATCH (a)-[:R]->(b) WHERE NOT (b.hidden = 999) }", vec![]),
            ("NOT EXISTS { MATCH (a)-[:R]->(b) WHERE b.hidden = 999 }", vec![1, 3, 4]),
            ("EXISTS { MATCH (a)-[:R]->(b) WHERE b.p > a.p }", vec![1]),
            ("EXISTS { MATCH (b) WHERE b.p = 20 }", vec![]),
            ("EXISTS { MATCH (b) WHERE b.p = 40 }", vec![1, 3, 4]),
            ("NOT EXISTS { MATCH (b) WHERE b.hidden = 999 }", vec![1, 3, 4]),
        ] {
            for (skip, take) in [(0, 10), (1, 1), (0, 0)] {
                let text = format!("MATCH (a) WHERE {condition} RETURN a AS id, a.p AS p SKIP {skip} LIMIT {take}");
                let prepared = session.prepare(&cx, &text, &args).unwrap();
                let eager = session.execute(&cx, &prepared, &args).unwrap();
                let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
                let columns = cursor.columns().to_vec();
                let got = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                let wanted = expected(&answer).into_iter().skip(skip).take(take).collect::<Vec<_>>();
                assert_eq!(got, wanted, "{text}");
                assert_eq!(native(columns, &got), eager);
                assert_eq!(cursor.state(), VertexScanState::Exhausted);
                assert!(cursor.next().is_none());
            }
        }
        // Independent golden: a privileged negative control sees the private
        // property and hidden transit vertex that the scoped source removes.
        let mut broad = grant(); broad.labels = Scope::All; broad.relations = Scope::All; broad.properties = Scope::All;
        let broad = authority.issue_at(&broad, 100).unwrap();
        let mut raw = db.authorized_read_session(&cx, &authority, &broad, BRANCH, symbols, policy(), || 100).unwrap();
        let text = "MATCH (a) WHERE EXISTS { MATCH (b) WHERE b.p = 20 } RETURN a AS id, a.p AS p";
        assert_ne!(raw.query(&cx, text, &args).unwrap(), session.query(&cx, text, &args).unwrap());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn bounded_walks_do_not_cross_hidden_transit_vertices_and_zero_hops_need_no_relation() {
    let ((), report) = run_async_under_lab(0x5ec0_7302, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await;
        let authority = issuer(); let token = authority.issue_at(&grant(), 100).unwrap();
        let mut session = db.authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || 100).unwrap();
        let args = GqlParameters::new();
        for (condition, answer) in [
            ("EXISTS { MATCH (a)-[:R]->(x)-[:R]->(b) WHERE b.p = 40 }", vec![]),
            ("EXISTS { MATCH (a)-[:R*2..3]->(b) WHERE b.p = 40 }", vec![]),
            ("NOT EXISTS { MATCH (a)-[:R*2..3]->(b) WHERE b.p = 40 }", vec![1, 3, 4]),
            ("EXISTS { MATCH (a)-[:R*1..3]->(b) WHERE b.hidden IS NULL }", vec![1, 3]),
            ("EXISTS { MATCH (a)-[:S*1..3]->(b) }", vec![]),
            ("EXISTS { MATCH (a)-[:S*0..0]->(b) }", vec![1, 3, 4]),
            ("EXISTS { MATCH (a)-[:S*0..3]->(b) WHERE b = a }", vec![1, 3, 4]),
        ] {
            let text = format!("MATCH (a) WHERE {condition} RETURN a AS id, a.p AS p");
            let prepared = session.prepare(&cx, &text, &args).unwrap();
            let eager = session.execute(&cx, &prepared, &args).unwrap();
            let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
            let columns = cursor.columns().to_vec();
            let got = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(got, expected(&answer), "{text}");
            assert_eq!(native(columns, &got), eager);
        }
        let low = GqlParameters::new().with_int64("wanted", 30).unwrap();
        let high = GqlParameters::new().with_int64("wanted", 40).unwrap();
        let query = session.prepare(&cx,
            "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(b) WHERE b.p = $wanted } RETURN a AS id, a.p AS p", &low).unwrap();
        for (args, answer) in [(&low, vec![1, 3]), (&high, vec![]), (&low, vec![1, 3])] {
            let got = session.stream(&cx, &query, args).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(got, expected(&answer));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_paused_probe_cursor_retains_its_historical_generation_through_writes_compaction_and_drop() {
    // The cursor is deliberately !Send. Keep it on this thread while ordinary
    // production-runtime block_on drives separate writer futures; do not hold
    // it across an await inside the Send-requiring lab runner.
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query(); let commit = c.commit();
    let mut db = runtime.block_on(database(&commit));
    let authority = issuer(); let token = authority.issue_at(&grant(), 100).unwrap();
    let clock_calls = Cell::new(0usize);
    let mut session = db.authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || {
        clock_calls.set(clock_calls.get() + 1); 100
    }).unwrap();
    let args = GqlParameters::new();
    let text = "MATCH (a) WHERE EXISTS { MATCH (a)-[:R*2..3]->(b) WHERE b.p = 40 } RETURN a AS id, a.p AS p";
    let query = session.prepare(&cx, text, &args).unwrap();
    let mut cursor = session.stream(&cx, &query, &args).unwrap();
    let basis = cursor.snapshot_seq();
    drop(query);
    let before = clock_calls.get();
    let mut change = WriteBatch::new(RelationId(1));
    change.set_vertex_label(VId(2), LabelId(1), true);
    runtime.block_on(db.write(&commit, change)).unwrap();
    runtime.block_on(db.compact(&commit)).unwrap();
    assert_eq!(clock_calls.get(), before, "paused cursor performed query work");
    assert!(db.frontier().unwrap() > basis);
    let mut fresh = db.authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || 100).unwrap();
    let prepared = fresh.prepare(&cx, text, &args).unwrap();
    let now = fresh.stream(&cx, &prepared, &args).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(now, expected(&[1, 3]));
    let past = format!("MATCH (a) FOR SYSTEM_TIME AS OF SEQ {} WHERE EXISTS {{ MATCH (a)-[:R*2..3]->(b) WHERE b.p = 40 }} RETURN a AS id, a.p AS p", basis.0);
    let prepared = fresh.prepare(&cx, &past, &args).unwrap();
    assert!(fresh.stream(&cx, &prepared, &args).unwrap().collect::<Result<Vec<_>, _>>().unwrap().is_empty());
    drop(db);
    drop(token);
    assert!(cursor.next().is_none());
    assert_eq!(cursor.state(), VertexScanState::Exhausted);
    drop(cursor);
    assert!(!session.is_closed());
}

#[test]
fn probe_node_record_and_delivery_limits_are_cumulative_and_retryable_after_failure() {
    let ((), report) = run_async_under_lab(0x5ec0_7304, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await; let authority = issuer(); let args = GqlParameters::new();
        for (signed, native_policy, expected_dimension) in [
            (QueryLimits { max_nodes: 1, ..grant().limits }, policy(), Some(LimitDimension::Nodes)),
            (QueryLimits { max_rows: 0, ..grant().limits }, policy(), Some(LimitDimension::Rows)),
            (grant().limits, GqlQueryPolicy::new(1, 1000, 10_000_000, 10_000_000), None),
        ] {
            let mut allowed = grant(); allowed.limits = signed;
            let token = authority.issue_at(&allowed, 100).unwrap();
            let mut session = db.authorized_read_session(&cx, &authority, &token, BRANCH, symbols, native_policy, || 100).unwrap();
            let prepared = session.prepare(&cx, "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(b) } RETURN a AS id, a.p AS p LIMIT 1", &args).unwrap();
            let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
            match (cursor.next(), expected_dimension) {
                (Some(Err(QueryError::Authorization(Error::LimitExceeded(actual)))), Some(wanted)) => assert_eq!(actual, wanted),
                (Some(Err(QueryError::Stream(GqlQueryError::Rows(_)))), None) => {}
                (other, _) => panic!("wrong probe admission failure: {other:?}"),
            }
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
            drop(cursor);
            assert!(!session.is_closed());
            let zero = session.prepare(&cx, "MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->(b) } RETURN a AS id LIMIT 0", &args).unwrap();
            assert!(session.stream(&cx, &zero, &args).unwrap().next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_probe_poll_expiry_cut_retains_only_delivered_prefixes_and_closes_the_session() {
    let ((), report) = run_async_under_lab(0x5ec0_7305, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await; let authority = issuer();
        let token = authority.issue_at(&grant(), 100).unwrap(); let args = GqlParameters::new();
        let calls = Cell::new(0usize); let active = Cell::new(false); let stop = Cell::new(usize::MAX);
        let clock = || { if active.get() { calls.set(calls.get() + 1); }
            if active.get() && calls.get() == stop.get() { 1000 } else { 100 }
        };
        let text = "MATCH (a) WHERE EXISTS { MATCH (a)-[:R*1..2]->(b) WHERE b.hidden IS NULL } RETURN a AS id, a.p AS p";
        let count = {
            let mut session = db.authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), clock).unwrap();
            let prepared = session.prepare(&cx, text, &args).unwrap();
            let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
            active.set(true);
            assert_eq!(cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(), expected(&[1, 3]));
            active.set(false); calls.get()
        };
        assert!(count > 0);
        for cut in 1..=count {
            active.set(false); calls.set(0); stop.set(cut);
            let mut session = db.authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), clock).unwrap();
            let prepared = session.prepare(&cx, text, &args).unwrap();
            let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
            active.set(true); let mut prefix = Vec::new();
            loop {
                match cursor.next() {
                    Some(Ok(row)) => prefix.push(row),
                    Some(Err(QueryError::Authorization(Error::Expired))) => break,
                    other => panic!("expected expiry at {cut}, got {other:?}"),
                }
            }
            assert!(expected(&[1, 3]).starts_with(&prefix));
            assert_eq!(calls.get(), cut);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
            assert_eq!(calls.get(), cut);
            active.set(false); drop(cursor);
            assert!(session.is_closed());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
