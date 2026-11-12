//! Real transport owners bind one trusted instant, without keeping PREPARE's
//! representative values or consulting a calendar while a result is drained.

use super::*;
use crate::recovery::tests::{fail_after_marker, served, settled, vertex};
use asupersync::lab::run_async_under_lab;
use fgdb::StatementClock;
use fgdb_gql::{GqlParameters, GraphSymbolKind};
use fgdb_protocol::body::{
    ExecuteConditionalWrite, ExecuteMode, Prepare, PrepareMode, PreparedExecutionMode,
    WireTimestamp,
};
use fgdb_types::{CanonicalTimestamp, CommitSeq};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const EPOCH_NANOS: i128 = 1_609_459_200_000_000_000;

pub(crate) fn counted_clock() -> (StatementClock, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let observations = Arc::clone(&calls);
    let clock = StatementClock::new(move |_| {
        let observation = observations.fetch_add(1, Ordering::SeqCst);
        CanonicalTimestamp::offset_only(EPOCH_NANOS + observation as i128 * 1_000_000_000, 0).ok()
    });
    (clock, calls)
}

pub(crate) async fn install_clock(cx: &Cx, server: &mut crate::Server, clock: StatementClock) {
    let db = Arc::get_mut(server.databases.get_mut("test").unwrap()).unwrap();
    db.statement_clock = clock.clone();
    db.db
        .write(cx)
        .await
        .unwrap()
        .set_statement_clock(Some(clock));
}

fn instant(observation: usize) -> WireValue {
    WireValue::Timestamp(WireTimestamp {
        instant_utc_nanos: EPOCH_NANOS + observation as i128 * 1_000_000_000,
        utc_offset_seconds: 0,
        zone: None,
    })
}

#[cfg(test)]
fn accepted<T>(result: Result<T, Refusal>) -> T {
    result.unwrap_or_else(|error| panic!("{}: {}", error.code.name(), error.message))
}

fn request(mode: ExecuteMode, statement: &str) -> Execute {
    Execute {
        mode,
        statement: statement.into(),
        parameters: Vec::new(),
    }
}

async fn state(cx: &Cx, db: &Served) -> (CommitSeq, usize) {
    let guard = db.db.read(cx).await.unwrap();
    (guard.frontier().unwrap(), guard.vertices().unwrap().len())
}

#[test]
fn statement_time_prepared_reads_stream_once_and_recovery_keeps_the_authority() {
    let ((), report) = run_async_under_lab(0xc10c_0001, |root| async move {
        let (mut server, token, _) = served(&root, "statement-clock-read").await;
        let (clock, calls) = counted_clock();
        install_clock(&root, &mut server, clock).await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let query = crate::query_cx(&contexts);
        let mut seed = vertex(1);
        seed.create_vertex(fgdb_types::VId(2), Vec::new(), Vec::new());
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), seed)
            .await
            .unwrap();
        let definition = accepted(
            prepare_read(
                &root,
                db,
                &token,
                &Prepare {
                    mode: PrepareMode::Read,
                    statement:
                        "MATCH (n) RETURN n AS vertex, datetime() AS t, datetime() AS repeated"
                            .into(),
                    parameters: Vec::new(),
                },
            )
            .await,
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "PREPARE has no instant");
        for observation in 0..2 {
            let answer = accepted(read_prepared(&root, db, &token, &definition, &[]).await);
            assert_eq!(
                answer.rows,
                [1, 2].map(|id| vec![
                    WireValue::Vertex(id),
                    instant(observation),
                    instant(observation)
                ])
            );
            assert_eq!(calls.load(Ordering::SeqCst), observation + 1);
        }
        {
            let (mut session, prepared) =
                accepted(read_prepared_session(&root, db, &token, &definition).await);
            let mut cursor =
                accepted(session.stream_prepared(&query, &prepared, &GqlParameters::new()));
            assert_eq!(calls.load(Ordering::SeqCst), 3);
            for id in [1, 2] {
                assert_eq!(
                    accepted(cursor.next().await),
                    Some(vec![WireValue::Vertex(id), instant(2), instant(2)])
                );
            }
            assert!(accepted(cursor.next().await).is_none());
            assert_eq!(calls.load(Ordering::SeqCst), 3, "pulls retain binding time");
        }
        {
            let (mut session, _) = accepted(read_session(&root, db, &token).await);
            let mut cursor = accepted(session.stream(
                &query,
                "MATCH (n) RETURN count(*) AS total, min(datetime()) AS first",
                &GqlParameters::new(),
            ));
            assert!(cursor.is_aggregate());
            assert_eq!(
                accepted(cursor.next().await),
                Some(vec![WireValue::Count(2), instant(3)])
            );
            assert!(accepted(cursor.next().await).is_none());
            assert_eq!(calls.load(Ordering::SeqCst), 4);
        }
        fail_after_marker(&root, db, 99).await;
        assert_eq!(
            settled(db).await,
            crate::DatabaseStatus::Ready { generation: 2 }
        );
        assert_eq!(
            read_prepared(&root, db, &token, &definition, &[])
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::DatabaseRecovering
        );
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        let answer = accepted(
            read(
                &root,
                db,
                &token,
                &request(ExecuteMode::Read, "RETURN datetime() AS t"),
            )
            .await,
        );
        assert_eq!(answer.rows, [vec![instant(4)]]);
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn returning_merge_prepare_defers_time_dependent_list_keys() {
    let ((), report) = run_async_under_lab(0xc10c_0048, |root| async move {
        let (mut server, token, _) = served(&root, "statement-clock-merge-key").await;
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        install_clock(
            &root,
            &mut server,
            StatementClock::new(move |_| {
                let ms = observed.fetch_add(1, Ordering::SeqCst) + 1;
                CanonicalTimestamp::offset_only(ms as i128 * 1_000_000, 0).ok()
            }),
        )
        .await;
        Arc::get_mut(server.databases.get_mut("test").unwrap())
            .unwrap()
            .symbols
            .bind(GraphSymbolKind::Property, "stamp", 1)
            .unwrap();
        let db = &server.databases["test"];
        let definition = accepted(prepare_statement(&root, db, &token, &Prepare {
            mode: PrepareMode::WriteReturning,
            statement: "MERGE (n {stamp:[1/timestamp()]}) RETURN n.stamp AS stored, timestamp() AS ms".into(),
            parameters: Vec::new(),
        }).await);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(state(&root, db).await, (CommitSeq(0), 0));
        for (ms, key) in [(1, 1), (2, 0)] {
            let answer = accepted(
                run_prepared(
                    &root,
                    db,
                    &token,
                    &definition,
                    PreparedExecutionMode::WriteReturning,
                    &[],
                )
                .await,
            );
            assert_eq!(
                answer.rows,
                [vec![
                    WireValue::List(vec![WireValue::Int(key)]),
                    WireValue::Int(ms)
                ]]
            );
            assert_eq!(calls.load(Ordering::SeqCst), ms as usize);
            assert_eq!(state(&root, db).await, (CommitSeq(ms as u64), ms as usize));
        }
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn statement_time_writes_prepare_without_sampling_and_batches_capture_per_record() {
    let ((), report) = run_async_under_lab(0xc10c_0002, |root| async move {
        let (mut server, token, _) = served(&root, "statement-clock-write").await;
        let (clock, calls) = counted_clock();
        install_clock(&root, &mut server, clock).await;
        Arc::get_mut(server.databases.get_mut("test").unwrap())
            .unwrap()
            .symbols
            .bind(GraphSymbolKind::Property, "stamp", 1)
            .unwrap();
        let db = &server.databases["test"];
        let answer = accepted(
            write(
                &root,
                db,
                &token,
                &request(
                    ExecuteMode::Write,
                    "UNWIND [1,2] AS x CREATE (n {stamp:datetime()}) RETURN n.stamp AS stored, datetime() AS repeated",
                ),
            )
            .await,
        );
        assert_eq!(answer.rows, vec![vec![instant(0); 2]; 2]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let returning = accepted(
            prepare_statement(
                &root,
                db,
                &token,
                &Prepare {
                    mode: PrepareMode::WriteReturning,
                    statement: "CREATE (n {stamp:datetime()}) RETURN n.stamp AS stored, datetime() AS repeated".into(),
                    parameters: Vec::new(),
                },
            )
            .await,
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        for observation in 1..3 {
            let answer = accepted(
                run_prepared(
                    &root,
                    db,
                    &token,
                    &returning,
                    PreparedExecutionMode::WriteReturning,
                    &[],
                )
                .await,
            );
            assert_eq!(answer.rows, [vec![instant(observation); 2]]);
            assert_eq!(calls.load(Ordering::SeqCst), observation + 1);
        }
        let script = accepted(
            prepare_statement(
                &root,
                db,
                &token,
                &Prepare {
                    mode: PrepareMode::Write,
                    statement: "CREATE (n {stamp:datetime()})".into(),
                    parameters: Vec::new(),
                },
            )
            .await,
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        let before = state(&root, db).await;
        let answer =
            accepted(run_prepared_batch(&root, db, &token, &script, &[vec![], vec![]]).await);
        assert_eq!(answer.created.vertices, 2);
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        assert_eq!(
            state(&root, db).await,
            (CommitSeq(before.0.0 + 1), before.1 + 2)
        );
        let stored = accepted(
            read(
                &root,
                db,
                &token,
                &request(
                    ExecuteMode::Read,
                    "MATCH (n) RETURN n.stamp AS t ORDER BY t",
                ),
            )
            .await,
        );
        assert_eq!(
            stored.rows,
            [0, 0, 1, 2, 3, 4].map(|observation| vec![instant(observation)])
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            5,
            "ordinary reads need no clock"
        );
        let chain = accepted(
            prepare_statement(
                &root,
                db,
                &token,
                &Prepare {
                    mode: PrepareMode::Write,
                    statement: "CREATE (a {stamp:datetime()}) WITH a CREATE (b {stamp:datetime()})"
                        .into(),
                    parameters: Vec::new(),
                },
            )
            .await,
        );
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        let before = state(&root, db).await;
        let answer =
            accepted(run_prepared_batch(&root, db, &token, &chain, &[vec![], vec![]]).await);
        assert_eq!(answer.created.vertices, 4);
        assert_eq!(calls.load(Ordering::SeqCst), 7);
        assert_eq!(
            state(&root, db).await,
            (CommitSeq(before.0.0 + 1), before.1 + 4)
        );
        let stored = accepted(
            read(
                &root,
                db,
                &token,
                &request(
                    ExecuteMode::Read,
                    "MATCH (n) RETURN n.stamp AS t ORDER BY t",
                ),
            )
            .await,
        );
        assert_eq!(
            stored.rows,
            [0, 0, 1, 2, 3, 4, 5, 5, 6, 6].map(|observation| vec![instant(observation)])
        );
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn statement_time_authority_and_frontier_refusals_precede_capture_and_unavailability_rolls_back() {
    let ((), report) = run_async_under_lab(0xc10c_0003, |root| async move {
        let (mut server, token, _) = served(&root, "statement-clock-refusal").await;
        let (clock, calls) = counted_clock();
        install_clock(&root, &mut server, clock).await;
        {
            let db = &server.databases["test"];
            let read_only = token
                .attenuate(fgdb_warden::Restriction::Rights(fgdb_warden::Rights::Read))
                .unwrap();
            let statement = request(ExecuteMode::Write, "CREATE (n) RETURN datetime() AS t");
            assert_eq!(
                write(&root, db, &read_only, &statement)
                    .await
                    .err()
                    .unwrap()
                    .code,
                ErrorCode::PermissionDenied
            );
            assert_eq!(
                write_if_current(
                    &root,
                    db,
                    &token,
                    &ExecuteConditionalWrite {
                        expected_seq: 99,
                        statement: statement.statement,
                        parameters: Vec::new(),
                    },
                )
                .await
                .err()
                .unwrap()
                .code,
                ErrorCode::Conflict
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(state(&root, db).await, (CommitSeq(0), 0));
        }
        install_clock(&root, &mut server, StatementClock::new(|_| None)).await;
        let db = &server.databases["test"];
        let definition = accepted(
            prepare_statement(
                &root,
                db,
                &token,
                &Prepare {
                    mode: PrepareMode::WriteReturning,
                    statement: "CREATE (n) RETURN datetime() AS t".into(),
                    parameters: Vec::new(),
                },
            )
            .await,
        );
        let error = run_prepared(
            &root,
            db,
            &token,
            &definition,
            PreparedExecutionMode::WriteReturning,
            &[],
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.code, ErrorCode::Execution);
        assert!(
            error
                .message
                .contains("calendar-time authority is unavailable")
        );
        let error = read(
            &root,
            db,
            &token,
            &request(ExecuteMode::Read, "RETURN datetime() AS t"),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.code, ErrorCode::Execution);
        assert_eq!(state(&root, db).await, (CommitSeq(0), 0));
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn statement_time_http_and_subscription_reuse_the_native_binding() {
    let ((), report) = run_async_under_lab(0xc10c_0004, |root| async move {
        let (mut server, token, _) = served(&root, "statement-clock-http-subscribe").await;
        let (clock, calls) = counted_clock();
        install_clock(&root, &mut server, clock).await;
        let db = &server.databases["test"];
        let mut http_request = asupersync::http::h1::types::RequestBuilder::new(
            asupersync::http::h1::types::Method::Post,
            "/v1/databases/test/query",
        )
        .header(
            "Authorization",
            format!("Bearer {}", fgdb_protocol::json::hex(&token.encode())),
        )
        .build();
        http_request.body =
            br#"{"statement":"UNWIND [1,2] AS row RETURN datetime() AS t"}"#.to_vec();
        let response = crate::http::respond(
            &root,
            &server,
            http_request,
            &crate::http::OutputAuthority::new(),
        )
        .await;
        assert_eq!(response.status, 200, "{response:?}");
        let body = String::from_utf8(response.body).unwrap();
        assert_eq!(
            body.matches(&fgdb_protocol::json::cell(&instant(0)))
                .count(),
            2,
            "{body}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), vertex(1))
            .await
            .unwrap();
        let mut subscription = accepted(
            subscribe(
                &root,
                db,
                &token,
                &request(
                    ExecuteMode::Subscribe,
                    "SUBSCRIBE TO MATCH (n) RETURN datetime() AS t",
                ),
            )
            .await,
        );
        let baseline = accepted(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert_eq!(baseline.entries, [(1, vec![instant(1)])]);
        subscription
            .consumer
            .acknowledge(baseline.batch.receipt())
            .unwrap();
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), vertex(2))
            .await
            .unwrap();
        let delta = accepted(poll(&root, db, &token, &mut subscription).await).unwrap();
        assert_eq!(delta.entries, [(1, vec![instant(1)])]);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "ticks keep registration time"
        );
        subscription
            .consumer
            .acknowledge(delta.batch.receipt())
            .unwrap();
        drop(subscription);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn write_original_input_refuses_before_clock_capture_or_first_batch_binding() {
    let ((), report) = run_async_under_lab(0xc10c_0005, |root| async move {
        let (mut server, token, _) = served(&root, "write-original-input-before-clock").await;
        let (clock, calls) = counted_clock();
        install_clock(&root, &mut server, clock).await;
        {
            let db = Arc::get_mut(server.databases.get_mut("test").unwrap()).unwrap();
            db.symbols
                .bind(GraphSymbolKind::Property, "stamp", 1)
                .unwrap();
            db.symbols
                .bind(GraphSymbolKind::Property, "need", 2)
                .unwrap();
        }
        let db = &server.databases["test"];
        let limited = token
            .attenuate(fgdb_warden::Restriction::MaxWork(1024))
            .unwrap();
        let large = vec![("unused".into(), WireValue::Text("雪".repeat(2048)))];
        for (mode, execution, text) in [
            (
                PrepareMode::Write,
                PreparedExecutionMode::Write,
                "CREATE (n {stamp:datetime()})",
            ),
            (
                PrepareMode::Write,
                PreparedExecutionMode::Write,
                "CREATE (a {stamp:datetime()}) WITH a CREATE (b {stamp:datetime()})",
            ),
            (
                PrepareMode::WriteReturning,
                PreparedExecutionMode::WriteReturning,
                "CREATE (n {stamp:datetime()}) RETURN n.stamp AS stored",
            ),
        ] {
            let statement = Execute {
                mode: ExecuteMode::Write,
                statement: text.into(),
                parameters: large.clone(),
            };
            assert_eq!(
                write(&root, db, &limited, &statement)
                    .await
                    .err()
                    .unwrap()
                    .code,
                ErrorCode::Budget
            );
            for (expected_seq, code) in [(0, ErrorCode::Budget), (1, ErrorCode::Conflict)] {
                assert_eq!(
                    write_if_current(
                        &root,
                        db,
                        &limited,
                        &ExecuteConditionalWrite {
                            expected_seq,
                            statement: text.into(),
                            parameters: large.clone(),
                        },
                    )
                    .await
                    .err()
                    .unwrap()
                    .code,
                    code,
                    "frontier comparison still precedes conditional input binding"
                );
            }
            assert_eq!(
                prepare_statement(
                    &root,
                    db,
                    &limited,
                    &Prepare {
                        mode,
                        statement: text.into(),
                        parameters: large.clone(),
                    },
                )
                .await
                .err()
                .unwrap()
                .code,
                ErrorCode::Budget
            );
            let prepared = accepted(
                prepare_statement(
                    &root,
                    db,
                    &token,
                    &Prepare {
                        mode,
                        statement: text.into(),
                        parameters: vec![],
                    },
                )
                .await,
            );
            assert_eq!(
                run_prepared(&root, db, &limited, &prepared, execution, &large)
                    .await
                    .err()
                    .unwrap()
                    .code,
                ErrorCode::Budget
            );
            if mode == PrepareMode::Write {
                assert_eq!(
                    run_prepared_batch(&root, db, &limited, &prepared, &[vec![], large.clone()],)
                        .await
                        .err()
                        .unwrap()
                        .code,
                    ErrorCode::Budget,
                    "all original transcripts precede any record's calendar observation"
                );
            }
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(state(&root, db).await, (CommitSeq(0), 0));
        }
        for text in [
            "CREATE (n {stamp:datetime(), need:$required})",
            "CREATE (a {stamp:datetime(), need:$required}) WITH a CREATE (b)",
        ] {
            let prepared = accepted(
                prepare_statement(
                    &root,
                    db,
                    &token,
                    &Prepare {
                        mode: PrepareMode::Write,
                        statement: text.into(),
                        parameters: vec![("required".into(), WireValue::Int(7))],
                    },
                )
                .await,
            );
            // The first record cannot bind. The second record's original
            // payload must nevertheless be admitted before that error or any
            // clock observation can escape from binding the first record.
            let mut last = large.clone();
            last.push(("required".into(), WireValue::Int(7)));
            assert_eq!(
                run_prepared_batch(&root, db, &limited, &prepared, &[vec![], last])
                    .await
                    .err()
                    .unwrap()
                    .code,
                ErrorCode::Budget
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(state(&root, db).await, (CommitSeq(0), 0));
        }
        let answer = accepted(
            write(
                &root,
                db,
                &token,
                &request(
                    ExecuteMode::Write,
                    "CREATE (n {stamp:datetime()}) RETURN n.stamp AS stored",
                ),
            )
            .await,
        );
        assert_eq!(answer.rows, [vec![instant(0)]]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(state(&root, db).await, (CommitSeq(1), 1));
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
