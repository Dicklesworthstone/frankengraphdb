//! Calendar authority belongs to the host; binding owns each observation.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{
    Database, DatabaseKeys, PreparedNativeRead, QueryError, QueryResult, QueryValue,
    QueryWriteError, StatementClock, StatementTimeError,
};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
};
use fgdb_types::{
    CanonicalScalar, CanonicalTimestamp, DatabaseSecurityNamespaceId, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Rights, Scope};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x47; 32]);
const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x46; 32], NS, [0x48; 32])
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000)
}

fn write_policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(policy(), 100, 100, 100)
}

fn instant(milliseconds: usize) -> CanonicalTimestamp {
    CanonicalTimestamp::offset_only(milliseconds as i128 * 1_000_000, 0).unwrap()
}

fn advancing_clock() -> (StatementClock, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let clock =
        StatementClock::new(move |_| Some(instant(observed.fetch_add(1, Ordering::SeqCst) + 1)));
    (clock, calls)
}

fn integers(columns: &[&str], rows: &[&[i64]]) -> QueryResult {
    QueryResult::Rows {
        columns: columns.iter().map(|name| (*name).into()).collect(),
        rows: rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|value| {
                        QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(*value)))
                    })
                    .collect()
            })
            .collect(),
    }
}

fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(0x481), NS, "graph", SchemaEpoch(1), 1).unwrap()
}

fn grant() -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([L]),
        relations: Scope::only([R]),
        properties: Scope::only([P, Q]),
        rights: Rights::ReadWrite,
        limits: QueryLimits {
            max_nodes: 10_000,
            max_work: 1_000_000,
            max_rows: 10_000,
        },
        expires_at_ms: 10_000,
    }
}

fn lab<F>(seed: u64, body: impl FnOnce(PurposeContexts) -> F + Send + 'static)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let ((), report) = run_async_under_lab(seed, |root| async move {
        body(PurposeContexts::narrow_runtime_root(&root)).await
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn fixed_statement_authority_reproduces_literal_results_across_lab_runs() {
    let mut executions = Vec::new();
    for seed in [0x48a5, 0x48a6] {
        let (execution, report) = run_async_under_lab(seed, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            // 2015-07-21T21:40:32.142+01:00. Both independent lab roots
            // receive the same explicit calendar authority and empty graph.
            let fixed = CanonicalTimestamp::offset_only(1_437_511_232_142_000_000, 3600).unwrap();
            db.set_statement_clock(Some(StatementClock::new(move |_| Some(fixed.clone()))));
            let result = db
                .query(
                    &cx,
                    "UNWIND [2,1] AS n RETURN n, toString(date()) AS d, \
                     toString(time()) AS t, toString(localtime()) AS lt, \
                     toString(datetime()) AS dt, toString(localdatetime()) AS ld, \
                     timestamp() AS ms, datetime() = datetime() AS same ORDER BY n",
                    &GqlParameters::new(),
                    symbols,
                    policy(),
                )
                .unwrap();
            let expected = QueryResult::Rows {
                columns: ["n", "d", "t", "lt", "dt", "ld", "ms", "same"]
                    .map(String::from)
                    .to_vec(),
                rows: [1, 2]
                    .into_iter()
                    .map(|n| {
                        let mut row = vec![QueryValue::Value(GraphValue::Scalar(
                            CanonicalScalar::Int(n),
                        ))];
                        for value in [
                            "2015-07-21",
                            "21:40:32.142+01:00",
                            "21:40:32.142",
                            "2015-07-21T21:40:32.142+01:00",
                            "2015-07-21T21:40:32.142",
                        ] {
                            row.push(QueryValue::Value(GraphValue::Scalar(
                                CanonicalScalar::ucs_basic_text(value).unwrap(),
                            )));
                        }
                        row.extend([
                            QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(
                                1_437_511_232_142,
                            ))),
                            QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Bool(true))),
                        ]);
                        row
                    })
                    .collect(),
            };
            assert_eq!(result, expected);
            let QueryResult::Rows { columns, rows } = result else {
                panic!("the current-time statement must return rows");
            };
            let canonical_rows = rows
                .into_iter()
                .map(|row| {
                    fgdb_gql::algebra::GraphValueRow::from_owned_values(
                        row.into_iter()
                            .map(|cell| {
                                let QueryValue::Value(value) = cell else {
                                    panic!("the literal oracle contains only scalar values");
                                };
                                value
                            })
                            .collect(),
                    )
                    .canonical_bytes()
                    .unwrap()
                })
                .collect::<Vec<_>>();
            (columns, canonical_rows)
        });
        assert!(report.lab_test_passed(), "{report:?}");
        executions.push(execution);
    }
    assert_eq!(executions[0], executions[1]);
}

#[test]
fn prepared_execution_samples_once_and_read_views_retain_the_host_authority() {
    lab(0x48a1, |contexts| async move {
        let cx = contexts.query();
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let (clock, calls) = advancing_clock();
        db.set_statement_clock(Some(clock));
        let text =
            "UNWIND [1,2] AS n RETURN n,timestamp() AS first,timestamp() AS second ORDER BY n";
        let params = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
        db.query(&cx, &format!("EXPLAIN {text}"), &params, symbols, policy())
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "PREPARE and EXPLAIN are clock-free"
        );
        for ms in [1, 2] {
            assert_eq!(
                prepared.execute(&db, &cx, &params, policy()).unwrap(),
                integers(&["n", "first", "second"], &[&[1, ms, ms], &[2, ms, ms]])
            );
        }
        let retained = db.read_session().unwrap().downgrade().upgrade().unwrap();
        db.set_statement_clock(None);
        assert_eq!(
            retained
                .query(&cx, "RETURN timestamp() AS ms", &params, symbols, policy())
                .unwrap(),
            integers(&["ms"], &[&[3]])
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(
            db.query(&cx, "RETURN datetime()", &params, symbols, policy())
                .is_err()
        );
        let explicit = params.with_statement_time(instant(99)).unwrap();
        assert_eq!(
            db.query(
                &cx,
                "RETURN timestamp() AS ms",
                &explicit,
                symbols,
                policy()
            )
            .unwrap(),
            integers(&["ms"], &[&[99]])
        );
    });
}

#[test]
fn authorized_reads_rebind_time_and_cannot_substitute_request_context() {
    lab(0x48a2, |contexts| async move {
        let cx = contexts.query();
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let (clock, calls) = advancing_clock();
        db.set_statement_clock(Some(clock));
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let mut session = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
            .unwrap();
        let forged = GqlParameters::new()
            .with_statement_time(instant(99))
            .unwrap();
        let text = "RETURN timestamp() AS ms";
        let prepared = session.prepare(&cx, text, &forged).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            session.execute(&cx, &prepared, &forged).unwrap(),
            integers(&["ms"], &[&[1]])
        );
        assert_eq!(
            session.query(&cx, text, &forged).unwrap(),
            integers(&["ms"], &[&[2]])
        );
        assert_eq!(
            session
                .execute(&cx, &prepared, &GqlParameters::new())
                .unwrap(),
            integers(&["ms"], &[&[3]])
        );
        let cut = db.frontier().unwrap();
        session
            .execute_diff(&cx, &prepared, &forged, cut, cut)
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "both difference endpoints share one bind"
        );
        session.close();
        db.set_statement_clock(None);
        let mut unclocked = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
            .unwrap();
        assert!(
            unclocked.query(&cx, text, &forged).is_err(),
            "request context cannot invent host authority"
        );
    });
}

#[test]
fn authorized_write_preparation_is_instant_free_and_batches_capture_per_statement() {
    lab(0x48a3, |contexts| async move {
        let cx = contexts.query();
        let txn = contexts.txn();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let (clock, calls) = advancing_clock();
        db.set_statement_clock(Some(clock));
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let mut session = db
            .authorized_write_session(
                &txn,
                &commit,
                &issuer,
                &token,
                "main",
                symbols,
                R,
                write_policy(),
                100,
                || 100,
            )
            .unwrap();
        let forged = GqlParameters::new()
            .with_statement_time(instant(99))
            .unwrap();
        let prepared = session
            .prepare(&cx, "CREATE (:L {p:timestamp(),q:timestamp()})", &forged)
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        session
            .execute_batch_stats(&cx, &prepared, &[forged.clone(), forged.clone()])
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        session
            .query_stats(
                &cx,
                "UNWIND [1,2,3] AS row CREATE (:L {p:timestamp(),q:timestamp()})",
                &forged,
            )
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "one UNWIND is one statement"
        );
        drop(session);
        assert_eq!(
            db.query(
                &cx,
                "MATCH (n:L) RETURN n.p AS p,n.q AS q ORDER BY p",
                &GqlParameters::new(),
                symbols,
                policy()
            )
            .unwrap(),
            integers(&["p", "q"], &[&[1, 1], &[2, 2], &[3, 3], &[3, 3], &[3, 3]])
        );
    });
}

#[test]
fn prepared_merge_list_keys_evaluate_only_at_each_execution_instant() {
    lab(0x48a7, |contexts| async move {
        let (cx, txn, commit) = (contexts.query(), contexts.txn(), contexts.commit());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let (clock, calls) = advancing_clock();
        db.set_statement_clock(Some(clock));
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let mut session = db
            .authorized_write_session(
                &txn,
                &commit,
                &issuer,
                &token,
                "main",
                symbols,
                R,
                write_policy(),
                100,
                || 100,
            )
            .unwrap();
        let arguments = GqlParameters::new();
        let prepared = session
            .prepare(&cx, "MERGE (n:L {p:[1/timestamp()]})", &arguments)
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "PREPARE never evaluates epoch zero"
        );
        for expected_calls in [1, 2] {
            let (stats, _) = session
                .execute_stats(&cx, &prepared, &arguments)
                .await
                .unwrap();
            assert_eq!(stats.completed_statements, 1);
            assert_eq!(stats.created_vertices, 1);
            assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
        }
        drop(session);
        let vertices = db.vertices().unwrap();
        assert_eq!(vertices.len(), 2);
        let mut values: Vec<_> = vertices.iter().map(|vertex| vertex.props.clone()).collect();
        values.sort();
        assert_eq!(
            values,
            [0, 1].map(|n| vec![(
                P,
                GraphValue::List(vec![GraphValue::Scalar(CanonicalScalar::Int(n))].into())
                    .to_written_property()
                    .unwrap(),
            )])
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn writes_use_one_instant_and_clock_refusal_publishes_nothing() {
    lab(0x48a4, |contexts| async move {
        let cx = contexts.query();
        let txn = contexts.txn();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let (clock, calls) = advancing_clock();
        db.set_statement_clock(Some(clock));
        assert_eq!(
            db.query_write(
                &txn,
                &cx,
                &commit,
                "CREATE (n:L {p:timestamp(),q:timestamp()}) RETURN n.p AS first,n.q AS second",
                &GqlParameters::new(),
                symbols,
                R,
                write_policy(),
                |_| Ok::<_, ()>(ElementId::Vertex(VId(1))),
            )
            .await
            .unwrap(),
            integers(&["first", "second"], &[&[1, 1]])
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let before = (db.frontier().unwrap(), db.vertices().unwrap());
        db.set_statement_clock(Some(StatementClock::new(|_| None)));
        let result = db
            .query_write(
                &txn,
                &cx,
                &commit,
                "CREATE (:L {p:timestamp()})",
                &GqlParameters::new(),
                symbols,
                R,
                write_policy(),
                |_| -> Result<ElementId, ()> {
                    panic!("clock refusal precedes identity allocation")
                },
            )
            .await;
        assert!(matches!(
            result,
            Err(QueryWriteError::StatementTime(
                StatementTimeError::Unavailable
            ))
        ));
        assert!(matches!(
            db.query(
                &cx,
                "RETURN timestamp()",
                &GqlParameters::new(),
                symbols,
                policy()
            ),
            Err(QueryError::StatementTime(StatementTimeError::Unavailable))
        ));
        assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
    });
}
