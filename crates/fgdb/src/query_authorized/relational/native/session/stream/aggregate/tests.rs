//! Real committed generations, host-owned capabilities and native aggregate pulls.
use super::*;
use crate::{DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GraphExactAverage, GraphSymbol, GraphSymbolKind};
use fgdb_types::{CommitCx, DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Error as Denied, Grant, LimitDimension, QueryLimits, Scope};
use std::cell::Cell;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x84; 32]);
const P: PropertyKeyId = PropertyKeyId(1);
const H: PropertyKeyId = PropertyKeyId(2);
fn authority() -> Authority {
    Authority::new(
        AuthKey::from_seed(8401),
        NS,
        "host-graph",
        SchemaEpoch(0),
        1,
    )
    .unwrap()
}
fn grant() -> Grant {
    let mut g = Grant::read_only(
        "host-main",
        1000,
        QueryLimits {
            max_nodes: 100_000,
            max_work: 1_000_000,
            max_rows: 1000,
        },
    );
    g.labels = Scope::only([LabelId(1)]);
    g.relations = Scope::only([RelationId(1)]);
    g.properties = Scope::only([P]);
    g
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000)
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
async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x18; 32], NS, [0x29; 32]))
        .await
        .unwrap();
    let mut seed = WriteBatch::new(RelationId(1));
    for (id, value, visible) in [
        (0, Some(7), true),
        (1, Some(3), true),
        (2, Some(999), false),
        (3, Some(7), true),
        (u128::MAX, None, true),
    ] {
        let mut props = Vec::new();
        if let Some(value) = value {
            props.push((P, CanonicalScalar::Int(value)));
        }
        props.push((
            H,
            CanonicalScalar::ucs_basic_text("hidden-nonnumeric").unwrap(),
        ));
        seed.create_vertex(
            VId(id),
            if visible {
                vec![LabelId(1), LabelId(99)]
            } else {
                vec![LabelId(99)]
            },
            props,
        );
    }
    for (id, a, b) in [(10, 0, 1), (11, 1, 3), (12, 0, 2), (13, 2, u128::MAX)] {
        seed.add_edge(EId(id), VId(a), VId(b), vec![]);
    }
    db.write(cx, seed).await.unwrap();
    db
}
fn scalar(value: i64) -> QueryValue {
    QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)))
}
fn null() -> QueryValue {
    QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Null))
}
fn native(cursor: &mut AuthorizedAggregateCursor<'_>) -> QueryResult {
    let slots = cursor.output_slots().to_vec();
    let columns = cursor.columns().to_vec();
    let rows = cursor
        .by_ref()
        .map(|row| {
            let row = row.unwrap();
            slots
                .iter()
                .map(|slot| match *slot {
                    GraphAggregateTextSlot::GroupKey(at) => {
                        QueryValue::Value(row.keys()[at].clone())
                    }
                    GraphAggregateTextSlot::Aggregate(at) => row.values()[at].clone(),
                })
                .collect()
        })
        .collect();
    QueryResult::Rows { columns, rows }
}
fn result(names: &[&str], rows: Vec<Vec<QueryValue>>) -> QueryResult {
    QueryResult::Rows {
        columns: names.iter().map(|s| (*s).to_owned()).collect(),
        rows,
    }
}

#[test]
fn all_eleven_native_aggregates_mask_before_grouping_and_keep_lossless_return_layout() {
    let ((), report) = run_async_under_lab(0x5ec0_8401, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let args = GqlParameters::new();
        let mut session = db
            .authorized_read_session(&cx, &issuer, &token, "host-main", symbols, policy(), || 100)
            .unwrap();
        let text = "MATCH (n) WHERE n.hidden IS NULL RETURN COUNT(*) AS rows,n.hidden AS key,COUNT(n.p) AS present,SUM(n.p) AS total,AVG(n.p) AS avg,MIN(n.p) AS lo,MAX(n.p) AS hi,COUNT(DISTINCT n.p) AS support,SUM(DISTINCT n.p) AS unique_sum,AVG(DISTINCT n.p) AS unique_avg,COLLECT(n.p) AS items,COLLECT(DISTINCT n.p) AS unique_items,n.hidden AS repeated GROUP BY n.hidden";
        let prepared = session.prepare(&cx, text, &args).unwrap();
        let eager = session.execute(&cx, &prepared, &args).unwrap();
        let mut cursor = session.stream_aggregate(&cx, &prepared, &args).unwrap();
        assert_eq!(cursor.key_columns(), &["key"]);
        assert_eq!(
            cursor.output_slots()[1],
            GraphAggregateTextSlot::GroupKey(0)
        );
        assert_eq!(
            cursor.output_slots()[12],
            GraphAggregateTextSlot::GroupKey(0)
        );
        let got = native(&mut cursor);
        // QueryValue is a type alias; variants come from the aliased enum.
        use fgdb_gql::GraphAggregateValue::{Average, Count, Integer, Value};
        let expected = result(
            &[
                "rows",
                "key",
                "present",
                "total",
                "avg",
                "lo",
                "hi",
                "support",
                "unique_sum",
                "unique_avg",
                "items",
                "unique_items",
                "repeated",
            ],
            vec![vec![
                Count(4),
                null(),
                Count(3),
                Integer(17),
                Average(GraphExactAverage::new(17, 3).unwrap()),
                scalar(3),
                scalar(7),
                Count(2),
                Integer(10),
                Average(GraphExactAverage::new(5, 1).unwrap()),
                Value(GraphValue::List(
                    vec![7, 3, 7]
                        .into_iter()
                        .map(|v| GraphValue::Scalar(CanonicalScalar::Int(v)))
                        .collect(),
                )),
                Value(GraphValue::List(
                    vec![7, 3]
                        .into_iter()
                        .map(|v| GraphValue::Scalar(CanonicalScalar::Int(v)))
                        .collect(),
                )),
                null(),
            ]],
        );
        assert_eq!(got, expected);
        assert_eq!(got, eager);
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        assert!(cursor.next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn grouped_pages_and_parameterized_anonymous_probes_keep_native_semantics() {
    let ((), report) = run_async_under_lab(0x5ec0_8402, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let mut session = db
            .authorized_read_session(&cx, &issuer, &token, "host-main", symbols, policy(), || 100)
            .unwrap();
        let args = GqlParameters::new();
        for (text, expected) in [
            (
                "MATCH (n) RETURN n.p AS key,COUNT(*) AS n GROUP BY n.p HAVING COUNT(*) > 1 ORDER BY key DESC LIMIT 1",
                result(&["key", "n"], vec![vec![scalar(7), QueryValue::Count(2)]]),
            ),
            (
                "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) WHERE m.hidden IS NULL } RETURN COUNT(*) AS n",
                result(&["n"], vec![vec![QueryValue::Count(2)]]),
            ),
            (
                "MATCH (n) WHERE NOT EXISTS { MATCH (n)-[:R]->(m) } RETURN COUNT(*) AS n",
                result(&["n"], vec![vec![QueryValue::Count(2)]]),
            ),
            (
                "MATCH (n) WHERE EXISTS { MATCH (n)-[:R*2..3]->(m) WHERE m.p IS NULL } RETURN COUNT(*) AS n",
                result(&["n"], vec![vec![QueryValue::Count(0)]]),
            ),
            (
                "MATCH (n) WHERE EXISTS { MATCH (n)-[:S*0..0]->(m) } RETURN COUNT(*) AS n",
                result(&["n"], vec![vec![QueryValue::Count(4)]]),
            ),
            (
                "MATCH (n) RETURN COUNT(n.hidden) AS n,SUM(n.hidden) AS total",
                result(&["n", "total"], vec![vec![QueryValue::Count(0), null()]]),
            ),
        ] {
            let prepared = session.prepare(&cx, text, &args).unwrap();
            let eager = session.execute(&cx, &prepared, &args).unwrap();
            let got = native(&mut session.stream_aggregate(&cx, &prepared, &args).unwrap());
            assert_eq!(got, expected, "{text}");
            assert_eq!(got, eager, "{text}");
        }
        let low = GqlParameters::new().with_int64("wanted", 3).unwrap();
        let high = GqlParameters::new().with_int64("wanted", 999).unwrap();
        let prepared = session.prepare(&cx,"MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) WHERE m.p = $wanted } RETURN COUNT(*) AS n",&low).unwrap();
        for (args, count) in [(&low, 1), (&high, 0), (&low, 1)] {
            assert_eq!(
                native(&mut session.stream_aggregate(&cx, &prepared, args).unwrap()),
                result(&["n"], vec![vec![QueryValue::Count(count)]])
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn signed_delivery_counts_groups_not_inputs_and_late_failures_do_not_publish_partial_groups() {
    let ((), report) = run_async_under_lab(0x5ec0_8403, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = database(&commit).await;
        let issuer = authority();
        let args = GqlParameters::new();
        let mut g = grant();
        g.limits.max_nodes = 4;
        g.limits.max_rows = 1;
        let token = issuer.issue_at(&g, 100).unwrap();
        let mut session = db
            .authorized_read_session(&cx, &issuer, &token, "host-main", symbols, policy(), || 100)
            .unwrap();
        let global = session
            .prepare(&cx, "MATCH (n) RETURN COUNT(*) AS n", &args)
            .unwrap();
        assert_eq!(
            native(&mut session.stream_aggregate(&cx, &global, &args).unwrap()),
            result(&["n"], vec![vec![QueryValue::Count(4)]])
        );
        let grouped = session
            .prepare(
                &cx,
                "MATCH (n) RETURN n.p AS key,COUNT(*) AS n GROUP BY n.p",
                &args,
            )
            .unwrap();
        {
            let mut cursor = session.stream_aggregate(&cx, &grouped, &args).unwrap();
            assert!(cursor.next().unwrap().is_ok());
            assert!(matches!(
                cursor.next(),
                Some(Err(QueryError::Authorization(Denied::LimitExceeded(
                    LimitDimension::Rows
                ))))
            ));
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
        }
        assert!(!session.is_closed());
        assert_eq!(
            native(&mut session.stream_aggregate(&cx, &global, &args).unwrap()),
            result(&["n"], vec![vec![QueryValue::Count(4)]])
        );
        let mut too_few = g.clone();
        too_few.limits.max_nodes = 3;
        let limited = issuer.issue_at(&too_few, 100).unwrap();
        let mut limited = db
            .authorized_read_session(
                &cx,
                &issuer,
                &limited,
                "host-main",
                symbols,
                policy(),
                || 100,
            )
            .unwrap();
        let prepared = limited
            .prepare(&cx, "MATCH (n) RETURN COUNT(*) AS n LIMIT 0", &args)
            .unwrap();
        assert!(matches!(
            limited
                .stream_aggregate(&cx, &prepared, &args)
                .unwrap()
                .next(),
            Some(Err(QueryError::Authorization(Denied::LimitExceeded(
                LimitDimension::Nodes
            ))))
        ));
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(u128::MAX), P, Some(CanonicalScalar::Bool(true)));
        db.write(&commit, change).await.unwrap();
        // No cursor is held across this await; earlier session's generation is independent.
        let broad = issuer.issue_at(&grant(), 100).unwrap();
        let mut fresh = db
            .authorized_read_session(&cx, &issuer, &broad, "host-main", symbols, policy(), || 100)
            .unwrap();
        for suffix in ["", " LIMIT 0"] {
            let prepared = fresh
                .prepare(
                    &cx,
                    &format!("MATCH (n) RETURN SUM(n.p) AS total{suffix}"),
                    &args,
                )
                .unwrap();
            let mut cursor = fresh.stream_aggregate(&cx, &prepared, &args).unwrap();
            assert!(matches!(
                cursor.next(),
                Some(Err(QueryError::AggregateStream(GqlQueryError::Source(
                    GraphAggregateError::NonIntegerSum { .. }
                ))))
            ));
            assert!(cursor.next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn opening_is_lazy_but_owner_branch_shape_and_future_cut_admission_are_mandatory() {
    let ((), report) = run_async_under_lab(0x5ec0_8404, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let args = GqlParameters::new();
        let mut first = db
            .authorized_read_session(&cx, &issuer, &token, "host-main", symbols, policy(), || 100)
            .unwrap();
        let mut session = db
            .authorized_read_session(
                &cx,
                &issuer,
                &token,
                "host-main",
                symbols,
                GqlQueryPolicy::new(0, 100, 1_000_000, 1_000_000),
                || 100,
            )
            .unwrap();
        let foreign = first
            .prepare(&cx, "MATCH (n) RETURN COUNT(*) AS n", &args)
            .unwrap();
        assert!(matches!(
            session.stream_aggregate(&cx, &foreign, &args),
            Err(QueryError::Authorization(Denied::WrongAuthority))
        ));
        let prepared = session
            .prepare(&cx, "MATCH (n) RETURN COUNT(*) AS n", &args)
            .unwrap();
        {
            let mut cursor = session.stream_aggregate(&cx, &prepared, &args).unwrap();
            cursor.close();
            assert!(cursor.next().is_none());
        }
        assert!(!session.is_closed());
        assert!(matches!(
            session
                .stream_aggregate(&cx, &prepared, &args)
                .unwrap()
                .next(),
            Some(Err(QueryError::AggregateStream(GqlQueryError::Rows(_))))
        ));
        for text in [
            "MATCH (n) RETURN n AS id",
            "UNWIND [1,2] AS x RETURN COUNT(*) AS n",
        ] {
            let prepared = session.prepare(&cx, text, &args).unwrap();
            assert!(matches!(
                session.stream_aggregate(&cx, &prepared, &args),
                Err(QueryError::AggregateStreamPlan(_))
                    | Err(QueryError::StreamingUnsupported { .. })
            ));
        }
        // Edge aggregates now share the same lazy admission law. Opening
        // reads nothing; the zero candidate allowance refuses on first demand.
        let edge = session
            .prepare(&cx, "MATCH (a)-[e:R]->(b) RETURN COUNT(*) AS n", &args)
            .unwrap();
        let mut edge = session.stream_aggregate(&cx, &edge, &args).unwrap();
        assert!(matches!(
            edge.next(),
            Some(Err(QueryError::EdgeAggregateStream(GqlQueryError::Rows(_))))
        ));
        assert!(edge.next().is_none());
        drop(edge);
        let routed = GqlParameters::new()
            .with_text("route", "host-main")
            .unwrap();
        let prepared = session
            .prepare(
                &cx,
                "AT BRANCH $route MATCH (n) RETURN COUNT(*) AS n LIMIT 0",
                &routed,
            )
            .unwrap();
        let other = GqlParameters::new().with_text("route", "other").unwrap();
        assert!(matches!(
            session.stream_aggregate(&cx, &prepared, &other),
            Err(QueryError::Authorization(Denied::ScopeDenied))
        ));
        let future = db.frontier().unwrap().0 + 1;
        let prepared = session
            .prepare(
                &cx,
                &format!(
                    "MATCH (n) FOR SYSTEM_TIME AS OF SEQ {future} RETURN COUNT(*) AS n LIMIT 0"
                ),
                &args,
            )
            .unwrap();
        assert!(matches!(
            session.stream_aggregate(&cx, &prepared, &args),
            Err(QueryError::Read(_))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_open_accumulate_and_delivery_expiry_cut_fuses_and_releases_the_session() {
    let ((), report) = run_async_under_lab(0x5ec0_8405, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let args = GqlParameters::new();
        let active = Cell::new(false);
        let calls = Cell::new(0);
        let stop = Cell::new(usize::MAX);
        let clock = || {
            if active.get() {
                calls.set(calls.get() + 1);
            }
            if active.get() && calls.get() == stop.get() {
                1000
            } else {
                100
            }
        };
        let text = "MATCH (n) RETURN n.p AS key,COUNT(*) AS n GROUP BY n.p";
        let expected = {
            let mut session = db
                .authorized_read_session(
                    &cx,
                    &issuer,
                    &token,
                    "host-main",
                    symbols,
                    policy(),
                    clock,
                )
                .unwrap();
            let prepared = session.prepare(&cx, text, &args).unwrap();
            active.set(true);
            session
                .stream_aggregate(&cx, &prepared, &args)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        active.set(false);
        let total = calls.get();
        assert!(total > 0);
        for cut in 1..=total {
            calls.set(0);
            stop.set(cut);
            let mut session = db
                .authorized_read_session(
                    &cx,
                    &issuer,
                    &token,
                    "host-main",
                    symbols,
                    policy(),
                    clock,
                )
                .unwrap();
            let prepared = session.prepare(&cx, text, &args).unwrap();
            active.set(true);
            {
                match session.stream_aggregate(&cx, &prepared, &args) {
                    Err(QueryError::Authorization(Denied::Expired)) => {}
                    Ok(mut cursor) => {
                        let mut prefix = Vec::new();
                        loop {
                            match cursor.next() {
                                Some(Ok(row)) => prefix.push(row),
                                Some(Err(QueryError::Authorization(Denied::Expired))) => break,
                                other => panic!("cut {cut}: {other:?}"),
                            }
                        }
                        assert!(expected.starts_with(&prefix));
                        assert_eq!(cursor.state(), VertexScanState::Failed);
                        assert!(cursor.next().is_none());
                    }
                    other => panic!("cut {cut}: {other:?}"),
                };
            }
            assert_eq!(calls.get(), cut);
            active.set(false);
            assert!(session.is_closed());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn paused_aggregate_keeps_its_generation_and_live_authority_after_writer_drop() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let c = PurposeContexts::narrow_runtime_root(&root);
    let cx = c.query();
    let commit = c.commit();
    let mut db = runtime.block_on(database(&commit));
    let issuer = authority();
    let token = issuer.issue_at(&grant(), 100).unwrap();
    let calls = Cell::new(0);
    let now = Cell::new(100);
    let panic_now = Cell::new(false);
    let mut session = db
        .authorized_read_session(&cx, &issuer, &token, "host-main", symbols, policy(), || {
            assert!(!panic_now.get());
            calls.set(calls.get() + 1);
            now.get()
        })
        .unwrap();
    let args = GqlParameters::new();
    let prepared = session
        .prepare(&cx, "MATCH (n) RETURN SUM(n.p) AS total", &args)
        .unwrap();
    let mut cursor = session.stream_aggregate(&cx, &prepared, &args).unwrap();
    let at = cursor.snapshot_seq();
    drop(prepared);
    let mut unwind_session = db
        .authorized_read_session(&cx, &issuer, &token, "host-main", symbols, policy(), || {
            assert!(!panic_now.get(), "aggregate clock unwind");
            100
        })
        .unwrap();
    let unwind_prepared = unwind_session
        .prepare(&cx, "MATCH (n) RETURN COUNT(*) AS n", &args)
        .unwrap();
    let paused = calls.get();
    let mut change = WriteBatch::new(RelationId(1));
    change.set_vertex_property(VId(0), P, Some(CanonicalScalar::Int(70)));
    runtime.block_on(db.write(&commit, change)).unwrap();
    runtime.block_on(db.compact(&commit)).unwrap();
    assert!(db.frontier().unwrap() > at);
    assert_eq!(calls.get(), paused);
    drop(db);
    drop(token);
    assert_eq!(
        native(&mut cursor),
        result(&["total"], vec![vec![QueryValue::Integer(17)]])
    );
    drop(cursor);
    let historical = session
        .prepare(
            &cx,
            &format!(
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ {} RETURN SUM(n.p) AS total",
                at.0
            ),
            &args,
        )
        .unwrap();
    assert_eq!(
        native(&mut session.stream_aggregate(&cx, &historical, &args).unwrap()),
        result(&["total"], vec![vec![QueryValue::Integer(17)]])
    );
    {
        let mut cursor = unwind_session
            .stream_aggregate(&cx, &unwind_prepared, &args)
            .unwrap();
        panic_now.set(true);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cursor.next();
        }));
        assert!(result.is_err());
        panic_now.set(false);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().is_none());
        cursor.close();
    }
    assert!(unwind_session.is_closed());
    let groups = session
        .prepare(
            &cx,
            "MATCH (n) RETURN n.p AS key,COUNT(*) AS n GROUP BY n.p",
            &args,
        )
        .unwrap();
    {
        let mut cursor = session.stream_aggregate(&cx, &groups, &args).unwrap();
        assert!(cursor.next().unwrap().is_ok());
        issuer.retire();
        assert!(matches!(
            cursor.next(),
            Some(Err(QueryError::Authorization(Denied::AuthorityRetired)))
        ));
        assert!(cursor.next().is_none());
    }
    assert!(session.is_closed());
}

#[test]
fn error_translation_preserves_terminal_credentials_and_native_fault_variants() {
    for cause in [
        Denied::Expired,
        Denied::AuthorityRetired,
        Denied::ClockWentBackwards,
        Denied::LimitExceeded(LimitDimension::Work),
    ] {
        let native = GqlQueryError::Source(GraphAggregateError::Source(VertexScanError::Source(
            QueryError::Authorization(cause),
        )));
        // ubs:ignore -- test assertion on an authorization error value, not secret material.
        assert!(matches!(error(native),QueryError::Authorization(actual) if actual==cause));
    }
    assert!(matches!(
        error(GqlQueryError::Source(GraphAggregateError::NonIntegerSum {
            aggregate: 2
        })),
        QueryError::AggregateStream(GqlQueryError::Source(GraphAggregateError::NonIntegerSum {
            aggregate: 2
        }))
    ));
    assert!(matches!(
        error(GqlQueryError::Source(GraphAggregateError::Source(
            VertexScanError::Probe(EdgeScanError::ExpansionUnavailable)
        ))),
        QueryError::Stream(GqlQueryError::Source(VertexScanError::Probe(
            EdgeScanError::ExpansionUnavailable
        )))
    ));
}
