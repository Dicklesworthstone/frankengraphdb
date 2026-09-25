//! Real scoped row pulls, not an iterator over an eagerly collected result.
use super::*;
use crate::{DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GraphSymbol, GraphSymbolKind};
use fgdb_types::{CommitCx, DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Error as Denied, Grant, LimitDimension, QueryLimits, Scope};
use std::cell::Cell;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x87; 32]);
const P: PropertyKeyId = PropertyKeyId(1);
const H: PropertyKeyId = PropertyKeyId(2);
const ROWS: &str = "MATCH (a)-[e:R]->(b) RETURN e AS edge,a AS start,b AS target,e.p AS weight,e.hidden AS masked";
const VISIBLE: [(u128, u128, u128, Option<i64>); 5] = [
    (10, 0, 1, Some(2)),
    (11, 0, 1, Some(4)),
    (12, 1, 3, Some(8)),
    (13, 3, 3, None),
    (u128::MAX, 3, u128::MAX, Some(16)),
];
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(8701), NS, "graph", SchemaEpoch(0), 1).unwrap()
}
fn grant() -> Grant {
    let mut grant = Grant::read_only("main", 1000, QueryLimits {
        max_nodes: 100_000, max_work: 1_000_000, max_rows: 1000,
    });
    grant.labels = Scope::only([LabelId(1)]);
    grant.relations = Scope::only([RelationId(1)]);
    grant.properties = Scope::only([P]);
    grant
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 1000, 1_000_000, 1_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "hidden") => Some(GraphSymbol::Property(H)),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "H") => Some(GraphSymbol::Label(LabelId(99))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        _ => None,
    }
}
async fn database(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x17; 32], NS, [0x27; 32]))
        .await.unwrap();
    let mut seed = WriteBatch::new(RelationId(1));
    for (id, value) in [(0, 7), (1, 3), (3, 7), (u128::MAX, 0)] {
        let mut properties = vec![(P, CanonicalScalar::Int(value))];
        let mut labels = vec![LabelId(1)];
        if hidden {
            labels.push(LabelId(99));
            properties.push((H, CanonicalScalar::ucs_basic_text("hidden vertex").unwrap()));
        }
        seed.create_vertex(VId(id), labels, properties);
    }
    for (id, from, to, value) in VISIBLE {
        let mut properties: Vec<_> = value.into_iter().map(|v| (P, CanonicalScalar::Int(v))).collect();
        if hidden {
            properties.push((H, CanonicalScalar::ucs_basic_text("hidden edge").unwrap()));
        }
        seed.add_edge(EId(id), VId(from), VId(to), properties);
    }
    if hidden {
        seed.create_vertex(VId(2), vec![LabelId(99)], vec![(P, CanonicalScalar::Int(999))]);
        seed.add_edge(EId(14), VId(0), VId(2), vec![]);
        seed.add_edge(EId(15), VId(2), VId(u128::MAX), vec![]);
    }
    db.write(cx, seed).await.unwrap();
    if hidden {
        let mut denied = WriteBatch::new(RelationId(2));
        denied.add_edge(EId(9), VId(0), VId(3), vec![(P, CanonicalScalar::Int(64))]);
        db.write(cx, denied).await.unwrap();
    }
    db
}
fn value(value: Option<i64>) -> GraphValue {
    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
}
fn row(id: u128, from: u128, to: u128, weight: Option<i64>) -> Vec<GraphValue> {
    vec![GraphValue::Edge(EId(id)), GraphValue::Vertex(VId(from)),
        GraphValue::Vertex(VId(to)), value(weight), value(None)]
}
fn cells(cursor: &mut AuthorizedRowCursor<'_>) -> Vec<Vec<GraphValue>> {
    cursor.by_ref().map(|row| row.unwrap().values().to_vec()).collect()
}
fn native(columns: &[String], rows: &[Vec<GraphValue>]) -> QueryResult {
    QueryResult::Rows {
        columns: columns.to_vec(),
        rows: rows.iter().map(|row| row.iter().cloned().map(QueryValue::Value).collect()).collect(),
    }
}
macro_rules! context {
    ($runtime:ident, $cx:ident, $commit:ident) => {
        let $runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
        let root = $runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let $cx = contexts.query();
        let $commit = contexts.commit();
    };
}

#[test]
fn scoped_rows_preserve_direction_multiplicity_pages_and_masked_fields() {
    context!(runtime, cx, commit);
    let db = runtime.block_on(database(&commit, true));
    let slim = runtime.block_on(database(&commit, false));
    let issuer = authority();
    let token = issuer.issue_at(&grant(), 100).unwrap();
    let params = GqlParameters::new();
    let calls = Cell::new(0_u64);
    let clock = || { calls.set(calls.get() + 1); 100 };
    let mut session = db.authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), clock).unwrap();
    let mut visible = slim.authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), clock).unwrap();
    for (direction, pattern) in [
        (0, "(a)-[e:R]->(b)"), (1, "(a)<-[e:R]-(b)"), (2, "(a)-[e:R]-(b)"),
    ] {
        let mut expected = Vec::new();
        for (id, a, b, weight) in VISIBLE {
            match direction {
                0 => expected.push(row(id, a, b, weight)),
                1 => expected.push(row(id, b, a, weight)),
                _ => {
                    expected.push(row(id, a.min(b), a.max(b), weight));
                    if a != b { expected.push(row(id, a.max(b), a.min(b), weight)); }
                }
            }
        }
        for quantifier in ["", "DISTINCT "] {
            for (skip, take) in [(0, 20), (1, 3), (3, 0), (20, 2)] {
                let text = format!("MATCH {pattern} RETURN {quantifier}e AS edge,a AS start,b AS target,e.p AS weight,e.hidden AS masked SKIP {skip} LIMIT {take}");
                let prepared = session.prepare(&cx, &text, &params).unwrap();
                let eager = session.execute(&cx, &prepared, &params).unwrap();
                let before = calls.get();
                let mut cursor = session.stream(&cx, &prepared, &params).unwrap();
                let got = cells(&mut cursor);
                let count = calls.get() - before;
                assert_eq!(got, expected.iter().skip(skip).take(take).cloned().collect::<Vec<_>>());
                assert_eq!(native(cursor.columns(), &got), eager);
                assert_eq!(cursor.state(), VertexScanState::Exhausted);
                assert_eq!(cursor.size_hint(), (0, Some(0)));
                assert!(cursor.next().is_none());
                drop(cursor);
                let prepared = visible.prepare(&cx, &text, &params).unwrap();
                let before = calls.get();
                let mut cursor = visible.stream(&cx, &prepared, &params).unwrap();
                assert_eq!(cells(&mut cursor), got);
                assert_eq!(calls.get() - before, count, "hidden population changed live charging: {text}");
            }
        }
    }
}

#[test]
fn connected_rows_keep_captured_paths_and_rebind_scoped_predicates() {
    context!(runtime, cx, commit);
    let db = runtime.block_on(database(&commit, true));
    let issuer = authority();
    let token = issuer.issue_at(&grant(), 100).unwrap();
    let params = GqlParameters::new();
    let mut session = db.authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100).unwrap();
    let text = "MATCH route=(a)-[e:R]->(b)-[f:R]->(c) RETURN e AS first,a AS start,f AS second,b AS middle,c AS target,f.p AS weight,f.hidden AS masked,route AS route";
    let prepared = session.prepare(&cx, text, &params).unwrap();
    let eager = session.execute(&cx, &prepared, &params).unwrap();
    {
        let mut cursor = session.stream(&cx, &prepared, &params).unwrap();
        let got = cells(&mut cursor);
        let expected = [
            (10, 0, 12, 1, 3, Some(8)), (11, 0, 12, 1, 3, Some(8)),
            (12, 1, 13, 3, 3, None), (12, 1, u128::MAX, 3, u128::MAX, Some(16)),
            (13, 3, 13, 3, 3, None), (13, 3, u128::MAX, 3, u128::MAX, Some(16)),
        ];
        assert_eq!(got.len(), expected.len());
        for (actual, (first, start, second, middle, target, weight)) in got.iter().zip(expected) {
            assert_eq!(&actual[..7], &[
                GraphValue::Edge(EId(first)), GraphValue::Vertex(VId(start)),
                GraphValue::Edge(EId(second)), GraphValue::Vertex(VId(middle)),
                GraphValue::Vertex(VId(target)), value(weight), value(None),
            ]);
            let GraphValue::Path(path) = &actual[7] else { panic!("native path was narrowed") };
            assert_eq!(path.start(), VId(start));
            assert_eq!(path.steps(), &[(EId(first), VId(middle)), (EId(second), VId(target))]);
        }
        assert_eq!(native(cursor.columns(), &got), eager);
    }
    for (predicate, expected) in [
        ("NOT EXISTS { MATCH (b)-[:S]->(x) }", 5),
        ("EXISTS { MATCH (b)-[:R]->(x) WHERE x.hidden IS NULL }", 4),
        ("NOT (e.hidden = 55)", 0),
    ] {
        let text = format!("MATCH (a)-[e:R]->(b) WHERE {predicate} RETURN e AS edge,a AS start");
        let prepared = session.prepare(&cx, &text, &params).unwrap();
        let eager = session.execute(&cx, &prepared, &params).unwrap();
        let mut cursor = session.stream(&cx, &prepared, &params).unwrap();
        let got = cells(&mut cursor);
        assert_eq!(got.len(), expected, "{text}");
        assert_eq!(native(cursor.columns(), &got), eager);
    }
    let low = GqlParameters::new().with_int64("wanted", 4).unwrap();
    let high = GqlParameters::new().with_int64("wanted", 1000).unwrap();
    let prepared = session.prepare(&cx, "MATCH (a)-[e:R]->(b) WHERE e.p > $wanted RETURN e AS edge,a AS start", &low).unwrap();
    for (args, ids) in [(&low, vec![EId(12), EId(u128::MAX)]), (&high, vec![]), (&low, vec![EId(12), EId(u128::MAX)])] {
        let got = cells(&mut session.stream(&cx, &prepared, args).unwrap());
        assert_eq!(got.iter().map(|row| row[0].clone()).collect::<Vec<_>>(), ids.into_iter().map(GraphValue::Edge).collect::<Vec<_>>());
    }
}

#[test]
fn opening_checks_owner_branch_shape_and_cut_but_does_not_scan_the_unread_graph() {
    context!(runtime, cx, commit);
    let db = runtime.block_on(database(&commit, true));
    let issuer = authority();
    let token = issuer.issue_at(&grant(), 100).unwrap();
    let params = GqlParameters::new();
    let mut owner = db.authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100).unwrap();
    let prepared = owner.prepare(&cx, ROWS, &params).unwrap();
    let mut zero = db.authorized_read_session(&cx, &issuer, &token, "main", symbols,
        GqlQueryPolicy::new(0, 100, 1_000_000, 1_000_000), || 100).unwrap();
    assert!(matches!(zero.stream(&cx, &prepared, &params), Err(QueryError::Authorization(Denied::WrongAuthority))));
    let prepared = zero.prepare(&cx, ROWS, &params).unwrap();
    {
        let mut cursor = zero.stream(&cx, &prepared, &params).unwrap();
        cursor.close(); cursor.close();
        assert_eq!(cursor.state(), VertexScanState::Closed);
        assert!(cursor.next().is_none());
    }
    assert!(!zero.is_closed());
    assert!(matches!(zero.stream(&cx, &prepared, &params).unwrap().next(),
        Some(Err(QueryError::EdgeStream(GqlQueryError::Rows(_))))));
    let empty = zero.prepare(&cx, &format!("{ROWS} LIMIT 0"), &params).unwrap();
    assert!(zero.stream(&cx, &empty, &params).unwrap().next().is_none());
    let denied = zero.prepare(&cx, "MATCH (a)-[e:S]->(b) RETURN e AS edge,a AS start", &params).unwrap();
    assert!(zero.stream(&cx, &denied, &params).unwrap().next().is_none());
    for text in [
        "MATCH (a)-[e:R]->(b) RETURN e AS edge LIMIT 0",
        "MATCH (a)-[e:R]->(b) RETURN a AS start,e AS edge LIMIT 0",
        "MATCH (a)-[e:R]->(b) RETURN e AS edge,a AS start ORDER BY edge DESC LIMIT 0",
    ] {
        let query = zero.prepare(&cx, text, &params).unwrap();
        assert!(matches!(zero.stream(&cx, &query, &params),
            Err(QueryError::EdgeStream(GqlQueryError::Source(EdgeScanError::Plan(_))))));
    }
    let args = GqlParameters::new().with_text("route", "main").unwrap();
    let route = zero.prepare(&cx, &format!("AT BRANCH $route {ROWS} LIMIT 0"), &args).unwrap();
    let other = GqlParameters::new().with_text("route", "other").unwrap();
    assert!(matches!(zero.stream(&cx, &route, &other), Err(QueryError::Authorization(Denied::ScopeDenied))));
    let future = db.frontier().unwrap().0 + 1;
    let query = zero.prepare(&cx, &format!("MATCH (a)-[e:R]->(b) FOR SYSTEM_TIME AS OF SEQ {future} RETURN e AS edge,a AS start LIMIT 0"), &params).unwrap();
    assert!(matches!(zero.stream(&cx, &query, &params), Err(QueryError::Read(_))));

    let mut limited = grant(); limited.limits.max_rows = 1;
    let limited = issuer.issue_at(&limited, 100).unwrap();
    let mut session = db.authorized_read_session(&cx, &issuer, &limited, "main", symbols, policy(), || 100).unwrap();
    let prepared = session.prepare(&cx, ROWS, &params).unwrap();
    {
        let mut cursor = session.stream(&cx, &prepared, &params).unwrap();
        assert_eq!(cursor.next().unwrap().unwrap().values(), row(10, 0, 1, Some(2)));
        assert!(matches!(cursor.next(), Some(Err(QueryError::Authorization(Denied::LimitExceeded(LimitDimension::Rows))))));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().is_none());
    }
    assert!(!session.is_closed());
    let first = session.prepare(&cx, &format!("{ROWS} LIMIT 1"), &params).unwrap();
    assert_eq!(cells(&mut session.stream(&cx, &first, &params).unwrap()), vec![row(10, 0, 1, Some(2))]);
    let mut one = db.authorized_read_session(&cx, &issuer, &token, "main", symbols,
        GqlQueryPolicy::new(1, 1, 1_000_000, 1_000_000), || 100).unwrap();
    let first = one.prepare(&cx, &format!("{ROWS} LIMIT 1"), &params).unwrap();
    assert_eq!(cells(&mut one.stream(&cx, &first, &params).unwrap()), vec![row(10, 0, 1, Some(2))]);
}

#[test]
fn paused_edge_cursor_retains_exact_history_after_writes_compaction_and_writer_drop() {
    context!(runtime, cx, commit);
    let mut db = runtime.block_on(database(&commit, true));
    let issuer = authority();
    let token = issuer.issue_at(&grant(), 100).unwrap();
    let params = GqlParameters::new();
    let calls = Cell::new(0_u64);
    let mut session = db.authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || {
        calls.set(calls.get() + 1); 100
    }).unwrap();
    let prepared = session.prepare(&cx, ROWS, &params).unwrap();
    let mut cursor = session.stream(&cx, &prepared, &params).unwrap();
    let at = cursor.snapshot_seq();
    assert_eq!(cursor.next().unwrap().unwrap().values(), row(10, 0, 1, Some(2)));
    drop(prepared);
    let paused = calls.get();
    let mut change = WriteBatch::new(RelationId(1));
    change.set_vertex_label(VId(1), LabelId(1), false);
    change.set_edge_property(EId(13), P, Some(CanonicalScalar::Int(32)));
    runtime.block_on(db.write(&commit, change)).unwrap();
    runtime.block_on(db.compact(&commit)).unwrap();
    assert!(db.frontier().unwrap() > at);
    assert_eq!(calls.get(), paused, "a paused pull cursor must do no work");
    let mut fresh = db.authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100).unwrap();
    let now = fresh.prepare(&cx, ROWS, &params).unwrap();
    let old = fresh.prepare(&cx, &format!("MATCH (a)-[e:R]->(b) FOR SYSTEM_TIME AS OF SEQ {} RETURN e AS edge,a AS start,b AS target,e.p AS weight,e.hidden AS masked", at.0), &params).unwrap();
    drop(db); drop(token);
    assert_eq!(cells(&mut cursor), VISIBLE[1..].iter().map(|&(id, a, b, w)| row(id, a, b, w)).collect::<Vec<_>>());
    drop(cursor);
    assert!(!session.is_closed());
    assert_eq!(cells(&mut fresh.stream(&cx, &now, &params).unwrap()),
        vec![row(13, 3, 3, Some(32)), row(u128::MAX, 3, u128::MAX, Some(16))]);
    assert_eq!(cells(&mut fresh.stream(&cx, &old, &params).unwrap()),
        VISIBLE.iter().map(|&(id, a, b, w)| row(id, a, b, w)).collect::<Vec<_>>());
}

#[test]
fn every_expiry_cut_and_host_unwind_fuses_edge_delivery_and_closes_the_session() {
    context!(runtime, cx, commit);
    let db = runtime.block_on(database(&commit, true));
    let issuer = authority();
    let token = issuer.issue_at(&grant(), 100).unwrap();
    let params = GqlParameters::new();
    let active = Cell::new(false);
    let calls = Cell::new(0_usize);
    let stop = Cell::new(usize::MAX);
    let clock = || {
        if active.get() { calls.set(calls.get() + 1); }
        if active.get() && calls.get() == stop.get() { 1000 } else { 100 }
    };
    let expected = {
        let mut session = db.authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), clock).unwrap();
        let prepared = session.prepare(&cx, ROWS, &params).unwrap();
        active.set(true);
        let mut cursor = session.stream(&cx, &prepared, &params).unwrap();
        cells(&mut cursor)
    };
    active.set(false);
    let total = calls.get();
    assert!(total > 0);
    for cut in 1..=total {
        calls.set(0); stop.set(cut);
        let mut session = db.authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), clock).unwrap();
        let prepared = session.prepare(&cx, ROWS, &params).unwrap();
        active.set(true);
        {
            match session.stream(&cx, &prepared, &params) {
                Err(QueryError::Authorization(Denied::Expired)) => {}
                Ok(mut cursor) => {
                    let mut prefix = Vec::new();
                    loop {
                        match cursor.next() {
                            Some(Ok(row)) => prefix.push(row.values().to_vec()),
                            Some(Err(QueryError::Authorization(Denied::Expired))) => break,
                            other => panic!("expiry cut {cut}: {other:?}"),
                        }
                    }
                    assert!(expected.starts_with(&prefix));
                    assert_eq!(cursor.state(), VertexScanState::Failed);
                    assert!(cursor.next().is_none());
                }
                other => panic!("opening cut {cut}: {other:?}"),
            };
        }
        assert_eq!(calls.get(), cut);
        active.set(false);
        assert!(session.is_closed());
    }
    let panic_now = Cell::new(false);
    let mut session = db.authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || {
        assert!(!panic_now.get(), "host callback unwind"); 100
    }).unwrap();
    let prepared = session.prepare(&cx, ROWS, &params).unwrap();
    {
        let mut cursor = session.stream(&cx, &prepared, &params).unwrap();
        panic_now.set(true);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| { cursor.next(); })).is_err());
        panic_now.set(false);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().is_none());
        cursor.close();
    }
    assert!(session.is_closed());
    let mut session = db.authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100).unwrap();
    let prepared = session.prepare(&cx, ROWS, &params).unwrap();
    {
        let mut cursor = session.stream(&cx, &prepared, &params).unwrap();
        assert!(cursor.next().unwrap().is_ok());
        issuer.retire();
        assert!(matches!(cursor.next(), Some(Err(QueryError::Authorization(Denied::AuthorityRetired)))));
        assert!(cursor.next().is_none());
    }
    assert!(session.is_closed());
}
