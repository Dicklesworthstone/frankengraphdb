//! Exercise transaction scripts through the actual FGP client and fragmented
//! transport. Provisional delivery and publication are separate observations.
use super::super::transaction::TransactionScript;
use super::*;
use fgdb_protocol::body::{
    ExecuteTransaction, TransactionBegun, TransactionCompletion, TransactionHandle,
    TransactionResultEnd,
};

const HANDLE: TransactionHandle = TransactionHandle([0x71; 16]);
const BASIS: u64 = 7;
const WRITE_QUERY: &str = "CREATE (:Person {name:$name}) RETURN $name AS name";

fn admitted(input: &str) -> TransactionScript {
    TransactionScript::parse(input, || Ok(()))
        .unwrap_or_else(|error| panic!("{}", error.message))
}

fn mixed() -> TransactionScript {
    admitted(
        r#"[
            {"mode":"write","statement":"CREATE (:Person {name:$name}) RETURN $name AS name","parameters":{"name":"text:Ada"}},
            {"mode":"read","statement":"RETURN $limit AS cap, true AS active","parameters":{"limit":"uint:18446744073709551615"}},
            {"mode":"write","statement":"CREATE (:Person {name:$name}) RETURN $name AS name","parameters":{"name":"text:Bob"}}
        ]"#,
    )
}

fn single(write: bool) -> TransactionScript {
    admitted(if write {
        r#"[{"mode":"write","statement":"CREATE (:Person {name:$name}) RETURN $name AS name","parameters":{"name":"text:Ada"}}]"#
    } else {
        r#"[{"mode":"read","statement":"RETURN $name AS name","parameters":{"name":"text:Ada"}}]"#
    })
}

fn begun() -> Frame {
    frame(
        FrameKind::TransactionBegun,
        4,
        StreamId::CONTROL,
        Binding::Ready(ready().binding(session())),
        &TransactionBegun {
            handle: HANDLE,
            basis: BASIS,
        },
    )
}

fn provisional_chunk(
    request: u64,
    columns: Option<Vec<String>>,
    rows: Vec<Vec<WireValue>>,
) -> Frame {
    result(
        FrameKind::TransactionResultChunk,
        request,
        &ResultChunk { columns, rows },
    )
}

fn statement_end(request: u64, statement: u64, rows: u64) -> Frame {
    result(
        FrameKind::TransactionResultEnd,
        request,
        &TransactionResultEnd {
            handle: HANDLE,
            statement,
            rows,
        },
    )
}

fn completed(request: u64, outcome: Outcome) -> Frame {
    frame(
        FrameKind::TransactionCommitted,
        request,
        StreamId::CONTROL,
        Binding::Ready(ready().binding(session())),
        &TransactionCompletion {
            handle: HANDLE,
            outcome,
        },
    )
}

fn assert_submission(frames: &[Frame], statements: usize, commits: usize) {
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame.header().kind() == FrameKind::BeginTransaction)
            .count(),
        1
    );
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame.header().kind() == FrameKind::ExecuteTransaction)
            .count(),
        statements
    );
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame.header().kind() == FrameKind::CommitTransaction)
            .count(),
        commits
    );
    assert!(!frames.iter().any(|frame| matches!(
        frame.header().kind(),
        FrameKind::Execute
            | FrameKind::ExecuteBatch
            | FrameKind::ExecuteReadBatch
            | FrameKind::ExecuteReadStatements
            | FrameKind::Prepare
            | FrameKind::ExecutePrepared
            | FrameKind::ExecutePreparedReadBatch
    )));
}

#[test]
fn mixed_transaction_files_keep_typed_arguments_private_until_one_completion() {
    let ((), report) = run_async_under_lab(0x79a0_0811, |root| async move {
        for robot in [true, false] {
            let flushes = Arc::new(AtomicUsize::new(0));
            let io = script(
                vec![
                    (begun(), 0),
                    (
                        provisional_chunk(
                            5,
                            Some(vec!["name".into()]),
                            vec![vec![WireValue::Text("Ada".into())]],
                        ),
                        0,
                    ),
                    (statement_end(5, 1, 1), 2),
                    (
                        provisional_chunk(
                            6,
                            Some(vec!["cap".into(), "active".into()]),
                            vec![vec![WireValue::Count(u64::MAX), WireValue::Bool(true)]],
                        ),
                        2,
                    ),
                    (statement_end(6, 2, 1), 4),
                    (
                        provisional_chunk(
                            7,
                            Some(vec!["name".into()]),
                            vec![vec![WireValue::Text("Bob".into())]],
                        ),
                        4,
                    ),
                    (statement_end(7, 3, 1), 6),
                    (
                        completed(
                            8,
                            Outcome::WriteCommitted {
                                seq: 8,
                                statements: 3,
                            },
                        ),
                        6,
                    ),
                ],
                Arc::clone(&flushes),
            );
            let written = Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "social").await.unwrap();
            let mut out = Output {
                bytes: vec![],
                flushes,
                fail_on: None,
            };
            mixed()
                .execute(&root, &mut client, robot, &mut out)
                .await
                .unwrap_or_else(|error| panic!("{}", error.message));
            let output = String::from_utf8(out.bytes).unwrap();
            if robot {
                assert_eq!(
                    output.lines().collect::<Vec<_>>(),
                    [
                        r#"{"v":1,"event":"columns","statement":1,"stream":true,"provisional":true,"basis_seq":7,"columns":["name"]}"#,
                        r#"{"v":1,"event":"row","statement":1,"provisional":true,"cells":[{"type":"text","value":"Ada"}]}"#,
                        r#"{"v":1,"event":"columns","statement":2,"stream":true,"provisional":true,"basis_seq":7,"columns":["cap","active"]}"#,
                        r#"{"v":1,"event":"row","statement":2,"provisional":true,"cells":[{"type":"count","value":"18446744073709551615"},{"type":"bool","value":true}]}"#,
                        r#"{"v":1,"event":"columns","statement":3,"stream":true,"provisional":true,"basis_seq":7,"columns":["name"]}"#,
                        r#"{"v":1,"event":"row","statement":3,"provisional":true,"cells":[{"type":"text","value":"Bob"}]}"#,
                        r#"{"v":1,"event":"result","kind":"transaction","outcome":"write_committed","seq":8,"basis_seq":7,"statements":3,"count":3}"#,
                    ]
                );
            } else {
                assert!(output.contains("Ada"));
                assert!(output.contains("Bob"));
                assert!(output.contains("18446744073709551615"));
                assert!(output.contains("provisional"));
                assert!(output.contains("seq 8"));
                assert!(!output.contains(r#""event":"result""#));
            }
            assert_eq!(out.flushes.load(Ordering::SeqCst), 7);
            let frames = sent_frames(&written.lock().unwrap());
            assert_submission(&frames, 3, 1);
            let requests: Vec<_> = frames
                .iter()
                .filter(|frame| frame.header().kind() == FrameKind::ExecuteTransaction)
                .map(|frame| ExecuteTransaction::decode(frame.payload()).unwrap())
                .collect();
            assert!(requests.iter().all(|request| request.handle == HANDLE));
            assert_eq!(requests[0].statement.mode, ExecuteMode::Write);
            assert_eq!(requests[0].statement.statement, WRITE_QUERY);
            assert_eq!(
                requests[0].statement.parameters,
                [("name".into(), WireValue::Text("Ada".into()))]
            );
            assert_eq!(requests[1].statement.mode, ExecuteMode::Read);
            assert_eq!(
                requests[1].statement.parameters,
                [("limit".into(), WireValue::Count(u64::MAX))]
            );
            assert_eq!(requests[2].statement.mode, ExecuteMode::Write);
            assert_eq!(requests[2].statement.statement, WRITE_QUERY);
            assert_eq!(
                requests[2].statement.parameters,
                [("name".into(), WireValue::Text("Bob".into()))]
            );
            assert!(!frames.iter().any(|frame| matches!(
                frame.header().kind(),
                FrameKind::QueryCancel | FrameKind::RollbackTransaction
            )));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn read_only_transaction_reports_its_basis_and_keeps_empty_statement_schema() {
    let ((), report) = run_async_under_lab(0x79a0_0812, |root| async move {
        let transaction = admitted(
            r#"[
                {"mode":"read","statement":"MATCH (n:Absent) RETURN n.name AS name"},
                {"mode":"read","statement":"RETURN $name AS name","parameters":{"name":"text:Ada"}}
            ]"#,
        );
        let flushes = Arc::new(AtomicUsize::new(0));
        let io = script(
            vec![
                (begun(), 0),
                (provisional_chunk(5, Some(vec!["name".into()]), vec![]), 0),
                (statement_end(5, 1, 0), 1),
                (
                    provisional_chunk(
                        6,
                        Some(vec!["name".into()]),
                        vec![vec![WireValue::Text("Ada".into())]],
                    ),
                    1,
                ),
                (statement_end(6, 2, 1), 3),
                (
                    completed(
                        7,
                        Outcome::ReadClosed {
                            seq: BASIS,
                            statements: 2,
                        },
                    ),
                    3,
                ),
            ],
            Arc::clone(&flushes),
        );
        let written = Arc::clone(&io.written);
        let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
        client.select(&root, "social").await.unwrap();
        let mut out = Output {
            bytes: vec![],
            flushes,
            fail_on: None,
        };
        transaction
            .execute(&root, &mut client, true, &mut out)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(
            String::from_utf8(out.bytes)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            [
                r#"{"v":1,"event":"columns","statement":1,"stream":true,"provisional":true,"basis_seq":7,"columns":["name"]}"#,
                r#"{"v":1,"event":"columns","statement":2,"stream":true,"provisional":true,"basis_seq":7,"columns":["name"]}"#,
                r#"{"v":1,"event":"row","statement":2,"provisional":true,"cells":[{"type":"text","value":"Ada"}]}"#,
                r#"{"v":1,"event":"result","kind":"transaction","outcome":"read_closed","seq":7,"basis_seq":7,"statements":2,"count":1}"#,
            ]
        );
        assert_submission(&sent_frames(&written.lock().unwrap()), 2, 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_statement_failure_never_commits_or_retries_a_provisional_prefix() {
    let ((), report) = run_async_under_lab(0x79a0_0813, |root| async move {
        for terminal in [Some(ErrorCode::Budget), Some(ErrorCode::Conflict), None] {
            let flushes = Arc::new(AtomicUsize::new(0));
            let mut replies = vec![
                (begun(), 0),
                (
                    provisional_chunk(
                        5,
                        Some(vec!["name".into()]),
                        vec![vec![WireValue::Text("Ada".into())]],
                    ),
                    0,
                ),
                (statement_end(5, 1, 1), 2),
                (
                    provisional_chunk(
                        6,
                        Some(vec!["cap".into(), "active".into()]),
                        vec![vec![WireValue::Count(u64::MAX), WireValue::Bool(true)]],
                    ),
                    2,
                ),
            ];
            if let Some(code) = terminal {
                replies.push((refusal(6, code), 4));
            }
            let io = script(replies, Arc::clone(&flushes));
            let written = Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "social").await.unwrap();
            let mut out = Output {
                bytes: vec![],
                flushes,
                fail_on: None,
            };
            let error = mixed()
                .execute(&root, &mut client, true, &mut out)
                .await
                .expect_err("the complete script requires every statement");
            assert_eq!(error.code, if terminal.is_some() { 3 } else { 5 });
            assert!(!error.message.contains("rolled back"));
            let output = String::from_utf8(out.bytes).unwrap();
            assert_eq!(output.lines().count(), 4);
            assert!(output.lines().all(|line| line.contains(r#""provisional":true"#)));
            assert!(!output.contains(r#""event":"result""#));
            assert_submission(&sent_frames(&written.lock().unwrap()), 2, 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn schema_or_row_flush_failure_cancels_even_with_statement_end_in_flight() {
    let ((), report) = run_async_under_lab(0x79a0_0814, |root| async move {
        for fail_on in [1, 2] {
            let flushes = Arc::new(AtomicUsize::new(0));
            let io = script(
                vec![
                    (begun(), 0),
                    (
                        provisional_chunk(
                            5,
                            Some(vec!["name".into()]),
                            vec![vec![WireValue::Text("Ada".into())]],
                        ),
                        0,
                    ),
                    // The END was already produced before the client rejected
                    // the schema or last row; it cannot turn rejection into
                    // permission to publish the transaction.
                    (statement_end(5, 1, 1), fail_on),
                ],
                Arc::clone(&flushes),
            );
            let written = Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "social").await.unwrap();
            let mut out = Output {
                bytes: vec![],
                flushes,
                fail_on: Some(fail_on),
            };
            let error = single(true)
                .execute(&root, &mut client, true, &mut out)
                .await
                .expect_err("a failed output callback cannot authorize COMMIT");
            assert_eq!(error.code, 5);
            assert!(error.message.contains("injected output flush failure"));
            assert_eq!(out.flushes.load(Ordering::SeqCst), fail_on);
            assert!(
                !String::from_utf8(out.bytes)
                    .unwrap()
                    .contains(r#""event":"result""#)
            );
            let frames = sent_frames(&written.lock().unwrap());
            assert_submission(&frames, 1, 0);
            let cancels: Vec<_> = frames
                .iter()
                .filter(|frame| frame.header().kind() == FrameKind::QueryCancel)
                .collect();
            assert_eq!(cancels.len(), 1);
            assert_eq!(cancels[0].header().stream_id(), StreamId([5; 16]));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn missing_or_unknown_commit_keeps_provisional_rows_without_success_or_replay() {
    let ((), report) = run_async_under_lab(0x79a0_0815, |root| async move {
        for unknown in [false, true] {
            let flushes = Arc::new(AtomicUsize::new(0));
            let mut replies = vec![
                (begun(), 0),
                (
                    provisional_chunk(
                        5,
                        Some(vec!["name".into()]),
                        vec![vec![WireValue::Text("Ada".into())]],
                    ),
                    0,
                ),
                (statement_end(5, 1, 1), 2),
            ];
            if unknown {
                replies.push((
                    frame(
                        FrameKind::Error,
                        6,
                        StreamId::CONTROL,
                        Binding::Ready(ready().binding(session())),
                        &ErrorBody {
                            code: ErrorCode::OutcomeUnknown,
                            message: "commit outcome is unknown".into(),
                        },
                    ),
                    2,
                ));
            }
            let io = script(replies, Arc::clone(&flushes));
            let written = Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "social").await.unwrap();
            let mut out = Output {
                bytes: vec![],
                flushes,
                fail_on: None,
            };
            let error = single(true)
                .execute(&root, &mut client, true, &mut out)
                .await
                .expect_err("an unacknowledged commit is not successful delivery");
            assert_eq!(error.code, 5);
            assert!(!error.message.contains("rolled back"));
            let output = String::from_utf8(out.bytes).unwrap();
            assert_eq!(output.lines().count(), 2);
            assert!(output.lines().all(|line| line.contains(r#""provisional":true"#)));
            assert!(!output.contains(r#""event":"result""#));
            let frames = sent_frames(&written.lock().unwrap());
            assert_submission(&frames, 1, 1);
            assert!(
                !frames
                    .iter()
                    .any(|frame| frame.header().kind() == FrameKind::RollbackTransaction)
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn final_output_failure_preserves_known_completion_and_never_replays() {
    let ((), report) = run_async_under_lab(0x79a0_0816, |root| async move {
        for write in [false, true] {
            let outcome = if write {
                Outcome::WriteCommitted {
                    seq: 8,
                    statements: 1,
                }
            } else {
                Outcome::ReadClosed {
                    seq: BASIS,
                    statements: 1,
                }
            };
            let flushes = Arc::new(AtomicUsize::new(0));
            let io = script(
                vec![
                    (begun(), 0),
                    (
                        provisional_chunk(
                            5,
                            Some(vec!["name".into()]),
                            vec![vec![WireValue::Text("Ada".into())]],
                        ),
                        0,
                    ),
                    (statement_end(5, 1, 1), 2),
                    (completed(6, outcome), 2),
                ],
                Arc::clone(&flushes),
            );
            let written = Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "social").await.unwrap();
            let mut out = Output {
                bytes: vec![],
                flushes,
                fail_on: Some(3),
            };
            let error = single(write)
                .execute(&root, &mut client, true, &mut out)
                .await
                .expect_err("terminal output flush must be acknowledged");
            assert_eq!(error.code, 5);
            assert!(
                error
                    .message
                    .contains(if write { "seq 8" } else { "seq 7" })
            );
            assert!(
                error.message.contains("do not retry") || error.message.contains("do not replay")
            );
            assert_eq!(out.flushes.load(Ordering::SeqCst), 3);
            // A write can have reached stdout before its flush fails. The
            // caller receives an error that retains the already-known outcome.
            assert!(
                String::from_utf8(out.bytes)
                    .unwrap()
                    .contains(r#""event":"result""#)
            );
            assert_submission(&sent_frames(&written.lock().unwrap()), 1, 1);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn write_request_len(value: &str) -> usize {
    Frame::new(
        FrameKind::ExecuteTransaction,
        6,
        StreamId::CONTROL,
        Binding::Ready(ready().binding(session())),
        ExecuteTransaction {
            handle: HANDLE,
            statement: Execute {
                mode: ExecuteMode::Write,
                statement: WRITE_QUERY.into(),
                parameters: vec![("name".into(), WireValue::Text(value.into()))],
            },
        }
        .encode()
        .unwrap(),
        FrameLimits::new(8192).unwrap(),
    )
    .unwrap()
    .header()
    .frame_len() as usize
}

#[test]
fn every_statement_is_preflighted_at_the_exact_negotiated_limit_before_begin() {
    let ((), report) = run_async_under_lab(0x79a0_0817, |root| async move {
        let padding = 4096_usize.checked_sub(write_request_len("")).unwrap();
        for excess in [0, 1] {
            let value = "x".repeat(padding + excess);
            assert_eq!(write_request_len(&value), 4096 + excess);
            let input = format!(
                r#"[
                    {{"mode":"read","statement":"RETURN 1"}},
                    {{"mode":"write","statement":"{WRITE_QUERY}","parameters":{{"name":"text:{value}"}}}}
                ]"#,
            );
            let transaction = admitted(&input);
            let flushes = Arc::new(AtomicUsize::new(0));
            let replies = if excess == 0 {
                vec![
                    (begun(), 0),
                    (provisional_chunk(5, Some(vec![]), vec![]), 0),
                    (statement_end(5, 1, 0), 1),
                    (provisional_chunk(6, Some(vec![]), vec![]), 1),
                    (statement_end(6, 2, 0), 2),
                    (
                        completed(
                            7,
                            Outcome::WriteCommitted {
                                seq: 8,
                                statements: 2,
                            },
                        ),
                        2,
                    ),
                ]
            } else {
                vec![]
            };
            let io = script(replies, Arc::clone(&flushes));
            let written = Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "social").await.unwrap();
            let mut out = Output {
                bytes: vec![],
                flushes,
                fail_on: None,
            };
            let result = transaction.execute(&root, &mut client, true, &mut out).await;
            let frames = sent_frames(&written.lock().unwrap());
            if excess == 0 {
                result.unwrap_or_else(|error| panic!("{}", error.message));
                assert_submission(&frames, 2, 1);
                let requests: Vec<_> = frames
                    .iter()
                    .filter(|frame| frame.header().kind() == FrameKind::ExecuteTransaction)
                    .collect();
                assert_eq!(requests[1].header().frame_len(), 4096);
            } else {
                assert_eq!(result.expect_err("the second statement is too large").code, 5);
                assert!(out.bytes.is_empty());
                assert_eq!(out.flushes.load(Ordering::SeqCst), 0);
                assert_eq!(
                    frames
                        .iter()
                        .map(|frame| frame.header().kind())
                        .collect::<Vec<_>>(),
                    [FrameKind::Hello, FrameKind::Auth, FrameKind::SelectDatabase]
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn invalid_transaction_file_is_rejected_before_reading_a_missing_credential() {
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for oversized in [false, true] {
        let path = std::env::temp_dir().join(format!(
            "fgdb-remote-transaction-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .unwrap();
        if oversized {
            file.set_len((1 << 20) + 1).unwrap();
        } else {
            // An otherwise valid prefix cannot be submitted before the late
            // untyped parameter is rejected by local admission.
            file.write_all(
                br#"[
                    {"mode":"write","statement":"CREATE (:Person)"},
                    {"mode":"read","statement":"RETURN $x","parameters":{"x":7}}
                ]"#,
            )
            .unwrap();
        }
        drop(file);
        let token = path.with_extension("missing-token");
        assert!(!token.exists());
        let mut args: Vec<String> = [
            "transaction",
            "--addr",
            "127.0.0.1:1",
            "--database",
            "social",
            "--token-file",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        args.push(token.to_str().unwrap().to_owned());
        args.push("--statements-file".into());
        args.push(path.to_str().unwrap().to_owned());
        let mut output = Vec::new();
        let error = super::super::run(&args, true, &mut output)
            .expect_err("invalid input must precede the absent credential");
        assert_eq!(error.code, 2, "{}", error.message);
        assert!(output.is_empty());
    }
}
