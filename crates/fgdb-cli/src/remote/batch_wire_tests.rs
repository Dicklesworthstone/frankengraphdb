//! Reuse the existing scripted transport and the actual FGP client. These
//! assertions inspect emitted wire frames, not a second batch executor.
use super::super::batch::{CsvSchema, PreparedBatch, ReadStatements};
use super::*;
use fgdb_protocol::body::{
    ExecuteBatch, ExecutePrepared, ExecutePreparedReadBatch, ExecuteReadStatements, Prepare,
    Prepared, PreparedHandle,
    PreparedReadMode, ReadBatchResultChunk, ReadBatchResultEnd,
};

const BATCH_QUERY: &str = "CREATE (:Person {name:$name})";
const BATCH_ROWS: &str = r#"[{"name":"text:Ada"},{"name":"text:Bob"}]"#;

fn mixed_reads() -> ReadStatements {
    ReadStatements::parse(r#"[
        {"statement":"RETURN $n AS name","parameters":{"n":"text:Ada"}},
        {"statement":"MATCH (n) FOR SYSTEM_TIME AS OF 3 RETURN n.id AS id"},
        {"statement":"RETURN $n AS count, true AS active","parameters":{"n":"uint:18446744073709551615"}}
    ]"#, || Ok(())).map_err(|error| error.message).expect("valid mixed read fixture")
}

#[test]
fn mixed_read_files_send_one_request_and_deliver_distinct_schemas_and_cuts() {
    let ((), report) = run_async_under_lab(0x79a0_0721, |root| async move {
        for robot in [true, false] {
            let flushes = Arc::new(AtomicUsize::new(0));
            let io = script(
                vec![
                    (
                        consistent_chunk(
                            0,
                            7,
                            true,
                            Some(vec!["name".into()]),
                            vec![vec![WireValue::Text("Ada".into())]],
                        ),
                        0,
                    ),
                    (
                        consistent_chunk(1, 3, true, Some(vec!["id".into()]), vec![]),
                        2,
                    ),
                    (
                        consistent_chunk(
                            2,
                            7,
                            true,
                            Some(vec!["count".into(), "active".into()]),
                            vec![vec![WireValue::Count(u64::MAX), WireValue::Bool(true)]],
                        ),
                        3,
                    ),
                    (consistent_end(3, 2), 5),
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
            mixed_reads()
                .execute(&root, &mut client, robot, &mut out)
                .await
                .unwrap_or_else(|error| panic!("{}", error.message));
            let output = String::from_utf8(out.bytes).unwrap();
            if robot {
                assert_eq!(
                    output.lines().collect::<Vec<_>>(),
                    [
                        r#"{"v":1,"event":"columns","statement":1,"stream":true,"seq":7,"columns":["name"]}"#,
                        r#"{"v":1,"event":"row","statement":1,"cells":[{"type":"text","value":"Ada"}]}"#,
                        r#"{"v":1,"event":"columns","statement":2,"stream":true,"seq":3,"columns":["id"]}"#,
                        r#"{"v":1,"event":"columns","statement":3,"stream":true,"seq":7,"columns":["count","active"]}"#,
                        r#"{"v":1,"event":"row","statement":3,"cells":[{"type":"count","value":"18446744073709551615"},{"type":"bool","value":true}]}"#,
                        r#"{"v":1,"event":"result","kind":"read_batch","statements":3,"count":2}"#,
                    ]
                );
            } else {
                assert!(output.contains("statement 2 at seq 3\nid"));
                assert!(output.contains("completed 3 statement(s), 2 row(s)"));
            }
            assert_eq!(out.flushes.load(Ordering::SeqCst), 6);
            let frames = sent_frames(&written.lock().unwrap());
            let requests: Vec<_> = frames
                .iter()
                .filter(|frame| frame.header().kind() == FrameKind::ExecuteReadStatements)
                .collect();
            assert_eq!(requests.len(), 1);
            let request = ExecuteReadStatements::decode(requests[0].payload()).unwrap();
            assert_eq!(request.statements.len(), 3);
            assert_eq!(request.statements[0].statement, "RETURN $n AS name");
            assert_eq!(
                request.statements[0].parameters,
                [("n".into(), WireValue::Text("Ada".into()))]
            );
            assert_eq!(
                request.statements[2].parameters,
                [("n".into(), WireValue::Count(u64::MAX))]
            );
            assert!(!frames.iter().any(|frame| matches!(
                frame.header().kind(),
                FrameKind::Execute
                    | FrameKind::ExecuteReadBatch
                    | FrameKind::Prepare
                    | FrameKind::ExecutePrepared
            )));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn mixed_read_refusals_and_output_errors_never_retry_or_complete_a_prefix() {
    let ((), report) = run_async_under_lab(0x79a0_0722, |root| async move {
        for code in [ErrorCode::Budget, ErrorCode::Statement, ErrorCode::Protocol] {
            let flushes = Arc::new(AtomicUsize::new(0));
            let io = script(vec![(refusal(4, code), 0)], Arc::clone(&flushes));
            let written = Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "social").await.unwrap();
            let mut out = Output {
                bytes: vec![],
                flushes,
                fail_on: None,
            };
            let error = mixed_reads()
                .execute(&root, &mut client, true, &mut out)
                .await
                .err()
                .expect("batch refused");
            assert!(matches!(error.code, 3 | 5));
            assert!(out.bytes.is_empty());
            let frames = sent_frames(&written.lock().unwrap());
            assert_eq!(
                frames
                    .iter()
                    .filter(|frame| frame.header().kind() == FrameKind::ExecuteReadStatements)
                    .count(),
                1
            );
            assert!(!frames.iter().any(|frame| matches!(
                frame.header().kind(),
                FrameKind::Execute | FrameKind::ExecuteReadBatch | FrameKind::Prepare
            )));
        }
        for fail_on in [1, 2] {
            let flushes = Arc::new(AtomicUsize::new(0));
            let io = script(
                vec![
                    (
                        consistent_chunk(
                            0,
                            7,
                            true,
                            Some(vec!["name".into()]),
                            vec![vec![WireValue::Text("Ada".into())]],
                        ),
                        0,
                    ),
                    (refusal(4, ErrorCode::Cancelled), fail_on),
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
            let error = mixed_reads()
                .execute(&root, &mut client, true, &mut out)
                .await
                .err()
                .expect("output failed");
            assert_eq!(error.code, 5);
            assert!(
                !String::from_utf8(out.bytes)
                    .unwrap()
                    .contains(r#""event":"result""#)
            );
            let frames = sent_frames(&written.lock().unwrap());
            assert_eq!(
                frames
                    .iter()
                    .filter(|frame| frame.header().kind() == FrameKind::QueryCancel)
                    .count(),
                1
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn prepared_batch(csv: bool) -> PreparedBatch {
    let result = if csv {
        let schema =
            CsvSchema::parse("name=text").unwrap_or_else(|error| panic!("{}", error.message));
        PreparedBatch::parse_csv(BATCH_QUERY, "name\nAda\nBob\n", &schema, || Ok(()))
    } else {
        PreparedBatch::parse(BATCH_QUERY, BATCH_ROWS, || Ok(()))
    };
    result.unwrap_or_else(|error| panic!("{}", error.message))
}

const READ_QUERY: &str = "MATCH (n) WHERE n.name = $name RETURN n.name AS name LIMIT $limit";
const READ_HANDLE: PreparedHandle = PreparedHandle([8; 16]);

fn prepared_reads(csv: bool) -> PreparedBatch {
    let result = if csv {
        let schema = CsvSchema::parse("name=text,limit=uint64")
            .unwrap_or_else(|error| panic!("{}", error.message));
        PreparedBatch::parse_csv(
            READ_QUERY,
            "name,limit\nAda,18446744073709551615\nBob,1\n",
            &schema,
            || Ok(()),
        )
    } else {
        PreparedBatch::parse(
            READ_QUERY,
            r#"[{"name":"text:Ada","limit":"uint:18446744073709551615"},{"name":"text:Bob","limit":"uint:1"}]"#,
            || Ok(()),
        )
    };
    result.unwrap_or_else(|error| panic!("{}", error.message))
}

fn consistent_chunk(
    index: u32,
    seq: u64,
    last: bool,
    columns: Option<Vec<String>>,
    rows: Vec<Vec<WireValue>>,
) -> Frame {
    result(
        FrameKind::ReadBatchResultChunk,
        4,
        &ReadBatchResultChunk {
            index,
            seq,
            last,
            chunk: ResultChunk { columns, rows },
        },
    )
}

fn consistent_end(results: u32, rows: u64) -> Frame {
    result(
        FrameKind::ReadBatchResultEnd,
        4,
        &ReadBatchResultEnd { results, rows },
    )
}

#[test]
fn consistent_read_files_deliver_exact_cuts_and_only_one_batch_terminal() {
    let ((), report) = run_async_under_lab(0x79a0_0711, |root| async move {
        for csv in [false, true] {
            let flushes = Arc::new(AtomicUsize::new(0));
            let io = script(
                vec![
                    (
                        consistent_chunk(0, 7, false, Some(vec!["name".into()]), vec![]),
                        0,
                    ),
                    (
                        consistent_chunk(
                            0,
                            7,
                            true,
                            None,
                            vec![vec![WireValue::Text("Ada".into())]],
                        ),
                        1,
                    ),
                    // Explicit historical operands can select a distinct cut.
                    // An empty result still has its own exact schema and cut.
                    (
                        consistent_chunk(1, 3, true, Some(vec!["name".into()]), vec![]),
                        2,
                    ),
                    (consistent_end(2, 1), 3),
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
            prepared_reads(csv)
                .execute_consistent_reads(&root, &mut client, true, &mut out)
                .await
                .unwrap_or_else(|error| panic!("{}", error.message));
            assert_eq!(out.flushes.load(Ordering::SeqCst), 4);
            let output = String::from_utf8(out.bytes).unwrap();
            assert_eq!(
                output.lines().collect::<Vec<_>>(),
                [
                    r#"{"v":1,"event":"columns","statement":1,"stream":true,"seq":7,"columns":["name"]}"#,
                    r#"{"v":1,"event":"row","statement":1,"cells":[{"type":"text","value":"Ada"}]}"#,
                    r#"{"v":1,"event":"columns","statement":2,"stream":true,"seq":3,"columns":["name"]}"#,
                    r#"{"v":1,"event":"result","kind":"read_batch","statements":2,"count":1}"#,
                ]
            );
            let frames = sent_frames(&written.lock().unwrap());
            let requests: Vec<_> = frames
                .iter()
                .filter(|frame| frame.header().kind() == FrameKind::ExecuteReadBatch)
                .collect();
            assert_eq!(requests.len(), 1);
            let request = ExecuteBatch::decode(requests[0].payload()).unwrap();
            assert_eq!(request.statement, READ_QUERY);
            assert_eq!(request.argument_sets.len(), 2);
            assert_eq!(request.argument_sets[0][0].1, WireValue::Count(u64::MAX));
            assert!(!frames.iter().any(|frame| matches!(
                frame.header().kind(),
                FrameKind::Prepare | FrameKind::ExecutePrepared | FrameKind::ExecuteBatch
            )));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn consistent_read_refusal_or_missing_terminal_never_completes_a_prefix() {
    let ((), report) = run_async_under_lab(0x79a0_0712, |root| async move {
        for terminal in [
            Some(refusal(4, ErrorCode::Budget)),
            Some(consistent_end(1, 1)),
            None,
        ] {
            let flushes = Arc::new(AtomicUsize::new(0));
            let mut replies = vec![
                (
                    consistent_chunk(
                        0,
                        7,
                        true,
                        Some(vec!["name".into()]),
                        vec![vec![WireValue::Text("Ada".into())]],
                    ),
                    0,
                ),
                (
                    consistent_chunk(1, 7, true, Some(vec!["name".into()]), vec![]),
                    2,
                ),
            ];
            replies.extend(terminal.map(|frame| (frame, 3)));
            let io = script(replies, Arc::clone(&flushes));
            let written = Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "social").await.unwrap();
            let mut out = Output {
                bytes: vec![],
                flushes,
                fail_on: None,
            };
            let error = prepared_reads(false)
                .execute_consistent_reads(&root, &mut client, true, &mut out)
                .await
                .expect_err("one terminal must confirm all results");
            assert!(matches!(error.code, 3 | 5));
            assert!(
                !String::from_utf8(out.bytes)
                    .unwrap()
                    .contains(r#""event":"result""#)
            );
            assert_eq!(
                sent_frames(&written.lock().unwrap())
                    .iter()
                    .filter(|frame| frame.header().kind() == FrameKind::ExecuteReadBatch)
                    .count(),
                1
            );
        }
        for fail_on in [1, 2] {
            let flushes = Arc::new(AtomicUsize::new(0));
            let io = script(
                vec![
                    (
                        consistent_chunk(
                            0,
                            7,
                            true,
                            Some(vec!["name".into()]),
                            vec![vec![WireValue::Text("Ada".into())]],
                        ),
                        0,
                    ),
                    (refusal(4, ErrorCode::Cancelled), fail_on),
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
            let error = prepared_reads(false)
                .execute_consistent_reads(&root, &mut client, true, &mut out)
                .await
                .expect_err("stdout failure must cancel the batch");
            assert_eq!(error.code, 5);
            assert_eq!(out.flushes.load(Ordering::SeqCst), fail_on);
            assert!(
                !String::from_utf8(out.bytes)
                    .unwrap()
                    .contains(r#""event":"result""#)
            );
            assert_eq!(
                sent_frames(&written.lock().unwrap())
                    .iter()
                    .filter(|frame| frame.header().kind() == FrameKind::QueryCancel)
                    .count(),
                1
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn prepared_reply() -> Frame {
    frame(
        FrameKind::Prepared,
        4,
        StreamId::CONTROL,
        Binding::Ready(ready().binding(session())),
        &Prepared {
            handle: READ_HANDLE,
        },
    )
}

fn read_end(request: u64, seq: u64) -> Frame {
    result(
        FrameKind::SnapshotResultEnd,
        request,
        &ResultEnd {
            outcome: Outcome::Rows { seq },
            rows: 1,
        },
    )
}

fn first_prepared_answer() -> Vec<(Frame, usize)> {
    vec![
        (prepared_reply(), 0),
        (chunk(5, Some(vec!["name".into()]), vec![]), 0),
        (chunk(5, None, vec![vec![WireValue::Text("Ada".into())]]), 1),
        (read_end(5, 7), 2),
    ]
}

#[test]
fn prepared_consistent_reads_send_one_template_and_one_batch_with_fresh_values() {
    let ((), report) = run_async_under_lab(0x79a0_072d, |root| async move {
        for csv in [false, true] {
            for robot in [false, true] {
                let flushes = Arc::new(AtomicUsize::new(0));
                let replies = vec![
                    (prepared_reply(), 0),
                    (
                        result(
                            FrameKind::ReadBatchResultChunk,
                            5,
                            &ReadBatchResultChunk {
                                index: 0,
                                seq: 7,
                                last: true,
                                chunk: ResultChunk {
                                    columns: Some(vec!["name".into()]),
                                    rows: vec![vec![WireValue::Text("Ada".into())]],
                                },
                            },
                        ),
                        0,
                    ),
                    (
                        result(
                            FrameKind::ReadBatchResultChunk,
                            5,
                            &ReadBatchResultChunk {
                                index: 1,
                                seq: 3,
                                last: true,
                                chunk: ResultChunk {
                                    columns: Some(vec!["name".into()]),
                                    rows: vec![vec![WireValue::Text("Bob".into())]],
                                },
                            },
                        ),
                        2,
                    ),
                    (
                        result(
                            FrameKind::ReadBatchResultEnd,
                            5,
                            &ReadBatchResultEnd { results: 2, rows: 2 },
                        ),
                        4,
                    ),
                ];
                let io = script(replies, Arc::clone(&flushes));
                let written = Arc::clone(&io.written);
                let batch = prepared_reads(csv);
                batch
                    .admit_prepared_reads()
                    .unwrap_or_else(|error| panic!("{}", error.message));
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "social").await.unwrap();
                let mut out = Output {
                    bytes: vec![],
                    flushes,
                    fail_on: None,
                };
                batch
                    .execute_prepared_reads(&root, &mut client, robot, &mut out)
                    .await
                    .unwrap_or_else(|error| panic!("{}", error.message));
                assert_eq!(out.flushes.load(Ordering::SeqCst), 5);
                let output = String::from_utf8(out.bytes).unwrap();
                if robot {
                    assert_eq!(
                        output.lines().collect::<Vec<_>>(),
                        [
                            r#"{"v":1,"event":"columns","statement":1,"stream":true,"seq":7,"columns":["name"]}"#,
                            r#"{"v":1,"event":"row","statement":1,"cells":[{"type":"text","value":"Ada"}]}"#,
                            r#"{"v":1,"event":"columns","statement":2,"stream":true,"seq":3,"columns":["name"]}"#,
                            r#"{"v":1,"event":"row","statement":2,"cells":[{"type":"text","value":"Bob"}]}"#,
                            r#"{"v":1,"event":"result","kind":"read_batch","statements":2,"count":2}"#,
                        ]
                    );
                } else {
                    assert!(output.contains("argument set 2 at seq 3"));
                    assert!(output.contains("completed 2 argument set(s), 2 row(s)"));
                }
                let frames = sent_frames(&written.lock().unwrap());
                let prepared: Vec<_> = frames
                    .iter()
                    .filter(|frame| frame.header().kind() == FrameKind::Prepare)
                    .collect();
                assert_eq!(prepared.len(), 1);
                let prepare = Prepare::decode(prepared[0].payload()).unwrap();
                assert_eq!(prepare.statement, READ_QUERY);
                let requests: Vec<_> = frames
                    .iter()
                    .filter(|frame| frame.header().kind() == FrameKind::ExecutePreparedReadBatch)
                    .collect();
                assert_eq!(requests.len(), 1);
                let request = ExecutePreparedReadBatch::decode(requests[0].payload()).unwrap();
                assert_eq!(request.handle, READ_HANDLE);
                assert_eq!(
                    request.argument_sets,
                    [
                        vec![
                            ("limit".into(), WireValue::Count(u64::MAX)),
                            ("name".into(), WireValue::Text("Ada".into())),
                        ],
                        vec![
                            ("limit".into(), WireValue::Count(1)),
                            ("name".into(), WireValue::Text("Bob".into())),
                        ],
                    ]
                );
                assert_eq!(prepare.parameters, request.argument_sets[0]);
                assert!(!frames.iter().any(|frame| matches!(
                    frame.header().kind(),
                    FrameKind::Execute
                        | FrameKind::ExecutePrepared
                        | FrameKind::ExecuteBatch
                        | FrameKind::ExecutePreparedBatch
                        | FrameKind::ExecuteReadBatch
                        | FrameKind::ExecuteReadStatements
                )));
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_consistent_refusals_and_sink_failures_never_retry_or_complete_a_prefix() {
    let ((), report) = run_async_under_lab(0x79a0_072e, |root| async move {
        for (request, code) in [
            (4, ErrorCode::Statement),
            (5, ErrorCode::Budget),
            (5, ErrorCode::Protocol),
        ] {
            let flushes = Arc::new(AtomicUsize::new(0));
            let mut replies = Vec::new();
            if request == 5 {
                replies.push((prepared_reply(), 0));
            }
            replies.push((refusal(request, code), 0));
            let io = script(replies, Arc::clone(&flushes));
            let written = Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "social").await.unwrap();
            let mut out = Output {
                bytes: vec![],
                flushes,
                fail_on: None,
            };
            let error = prepared_reads(false)
                .execute_prepared_reads(&root, &mut client, true, &mut out)
                .await
                .expect_err("prepare or batch refused");
            assert!(matches!(error.code, 3 | 5));
            assert!(out.bytes.is_empty());
            let frames = sent_frames(&written.lock().unwrap());
            assert_eq!(
                frames.iter().filter(|frame| frame.header().kind() == FrameKind::Prepare).count(),
                1
            );
            assert_eq!(
                frames.iter().filter(|frame| frame.header().kind() == FrameKind::ExecutePreparedReadBatch).count(),
                usize::from(request == 5)
            );
            assert!(!frames.iter().any(|frame| matches!(
                frame.header().kind(),
                FrameKind::Execute | FrameKind::ExecuteReadBatch | FrameKind::ExecutePrepared
            )));
        }
        for fail_on in [1, 2] {
            let flushes = Arc::new(AtomicUsize::new(0));
            let io = script(
                vec![
                    (prepared_reply(), 0),
                    (
                        result(
                            FrameKind::ReadBatchResultChunk,
                            5,
                            &ReadBatchResultChunk {
                                index: 0,
                                seq: 7,
                                last: true,
                                chunk: ResultChunk {
                                    columns: Some(vec!["name".into()]),
                                    rows: vec![vec![WireValue::Text("Ada".into())]],
                                },
                            },
                        ),
                        0,
                    ),
                    (refusal(5, ErrorCode::Cancelled), fail_on),
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
            let error = prepared_reads(false)
                .execute_prepared_reads(&root, &mut client, true, &mut out)
                .await
                .expect_err("failed output must cancel and drain");
            assert_eq!(error.code, 5);
            assert!(!String::from_utf8(out.bytes).unwrap().contains(r#""event":"result""#));
            let frames = sent_frames(&written.lock().unwrap());
            assert_eq!(
                frames.iter().filter(|frame| frame.header().kind() == FrameKind::QueryCancel).count(),
                1
            );
            assert_eq!(
                frames.iter().filter(|frame| frame.header().kind() == FrameKind::ExecutePreparedReadBatch).count(),
                1
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_argument_files_reuse_one_template_and_flush_each_answer_at_its_own_cut() {
    let ((), report) = run_async_under_lab(0x79a0_0701, |root| async move {
        for csv in [false, true] {
            for (mode, prepared_mode) in [
                (ExecuteMode::Read, PreparedReadMode::Buffered),
                (ExecuteMode::ReadStream, PreparedReadMode::SourceStream),
                (ExecuteMode::ReadOrdered, PreparedReadMode::OrderedStream),
            ] {
                for robot in [false, true] {
                    let flushes = Arc::new(AtomicUsize::new(0));
                    let mut replies = first_prepared_answer();
                    replies.extend([
                        (chunk(6, Some(vec!["name".into()]), vec![]), 3),
                        (chunk(6, None, vec![vec![WireValue::Text("Bob".into())]]), 4),
                        (read_end(6, 8), 5),
                    ]);
                    let io = script(replies, Arc::clone(&flushes));
                    let written = Arc::clone(&io.written);
                    let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                    client.select(&root, "social").await.unwrap();
                    let mut out = Output {
                        bytes: vec![],
                        flushes,
                        fail_on: None,
                    };
                    prepared_reads(csv)
                        .execute_reads(&root, &mut client, mode, robot, &mut out)
                        .await
                        .unwrap_or_else(|error| panic!("{}", error.message));
                    assert_eq!(out.flushes.load(Ordering::SeqCst), 6);
                    let output = String::from_utf8(out.bytes).unwrap();
                    let lines: Vec<_> = output.lines().collect();
                    assert_eq!(lines.len(), 6);
                    if robot {
                        assert_eq!(lines[0], lines[3]);
                        assert!(!lines[0].contains("seq"));
                        assert_eq!(
                            lines[1],
                            r#"{"v":1,"event":"row","cells":[{"type":"text","value":"Ada"}]}"#
                        );
                        assert_eq!(
                            lines[4],
                            r#"{"v":1,"event":"row","cells":[{"type":"text","value":"Bob"}]}"#
                        );
                        assert_eq!(
                            lines[2],
                            r#"{"v":1,"event":"result","kind":"rows","stream":true,"seq":7,"count":1}"#
                        );
                        assert_eq!(
                            lines[5],
                            r#"{"v":1,"event":"result","kind":"rows","stream":true,"seq":8,"count":1}"#
                        );
                    } else {
                        assert_eq!(
                            lines,
                            [
                                "name",
                                "Ada",
                                "1 row(s) (stream complete at seq 7)",
                                "name",
                                "Bob",
                                "1 row(s) (stream complete at seq 8)"
                            ]
                        );
                    }
                    let frames = sent_frames(&written.lock().unwrap());
                    let prepared: Vec<_> = frames
                        .iter()
                        .filter(|f| f.header().kind() == FrameKind::Prepare)
                        .collect();
                    assert_eq!(prepared.len(), 1);
                    let template = Prepare::decode(prepared[0].payload()).unwrap();
                    assert_eq!(template.statement, READ_QUERY);
                    assert_eq!(
                        template.parameters,
                        vec![
                            ("limit".into(), WireValue::Count(u64::MAX)),
                            ("name".into(), WireValue::Text("Ada".into()))
                        ]
                    );
                    let executions: Vec<_> = frames
                        .iter()
                        .filter(|f| f.header().kind() == FrameKind::ExecutePrepared)
                        .map(|f| ExecutePrepared::decode(f.payload()).unwrap())
                        .collect();
                    assert_eq!(executions.len(), 2);
                    for execution in &executions {
                        assert_eq!(execution.handle, READ_HANDLE);
                        assert_eq!(execution.mode, prepared_mode);
                    }
                    assert_eq!(executions[0].parameters, template.parameters);
                    assert_eq!(
                        executions[1].parameters,
                        vec![
                            ("limit".into(), WireValue::Count(1)),
                            ("name".into(), WireValue::Text("Bob".into()))
                        ]
                    );
                    assert!(!frames.iter().any(|f| matches!(
                        f.header().kind(),
                        FrameKind::Execute | FrameKind::ExecuteBatch
                    )));
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn three_prepared_reads() -> PreparedBatch {
    PreparedBatch::parse(
        READ_QUERY,
        r#"[{"name":"text:Ada","limit":"uint:1"},{"name":"text:Bob","limit":"uint:1"},{"name":"text:Carol","limit":"uint:1"}]"#,
        || Ok(()),
    )
    .unwrap_or_else(|error| panic!("{}", error.message))
}

#[test]
fn a_later_prepared_refusal_preserves_only_completed_answers_and_never_retries() {
    let ((), report) = run_async_under_lab(0x79a0_0702, |root| async move {
        for mode in [
            ExecuteMode::Read,
            ExecuteMode::ReadStream,
            ExecuteMode::ReadOrdered,
        ] {
            for terminal in [
                Some(ErrorCode::Statement),
                Some(ErrorCode::PermissionDenied),
                Some(ErrorCode::Budget),
                None,
            ] {
                let flushes = Arc::new(AtomicUsize::new(0));
                let mut replies = first_prepared_answer();
                replies.extend([
                    (chunk(6, Some(vec!["name".into()]), vec![]), 3),
                    (chunk(6, None, vec![vec![WireValue::Text("Bob".into())]]), 4),
                ]);
                replies.extend(terminal.map(|code| (refusal(6, code), 5)));
                let io = script(replies, Arc::clone(&flushes));
                let written = Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "social").await.unwrap();
                let mut out = Output {
                    bytes: vec![],
                    flushes,
                    fail_on: None,
                };
                let error = three_prepared_reads()
                    .execute_reads(&root, &mut client, mode, true, &mut out)
                    .await
                    .expect_err("the second read is incomplete");
                assert_eq!(error.code, if terminal.is_some() { 3 } else { 5 });
                assert!(
                    error.message.starts_with(
                        "argument set 2: stream incomplete after 1 fully flushed row(s)"
                    ),
                    "{}",
                    error.message
                );
                let output = String::from_utf8(out.bytes).unwrap();
                assert_eq!(output.matches(r#""event":"result""#).count(), 1);
                assert!(output.contains(r#""seq":7"#));
                assert!(!output.contains("Carol"));
                let frames = sent_frames(&written.lock().unwrap());
                assert_eq!(
                    frames
                        .iter()
                        .filter(|f| f.header().kind() == FrameKind::Prepare)
                        .count(),
                    1
                );
                assert_eq!(
                    frames
                        .iter()
                        .filter(|f| f.header().kind() == FrameKind::ExecutePrepared)
                        .count(),
                    2
                );
                assert!(!frames.iter().any(|f| matches!(
                    f.header().kind(),
                    FrameKind::Execute | FrameKind::ExecuteBatch
                )));
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_output_failure_cancels_the_active_read_and_does_not_start_the_next() {
    let ((), report) = run_async_under_lab(0x79a0_0703, |root| async move {
        for mode in [
            ExecuteMode::Read,
            ExecuteMode::ReadStream,
            ExecuteMode::ReadOrdered,
        ] {
            for fail_on in [4, 5] {
                let flushes = Arc::new(AtomicUsize::new(0));
                let mut replies = first_prepared_answer();
                replies.push((chunk(6, Some(vec!["name".into()]), vec![]), 3));
                if fail_on == 5 {
                    replies.push((chunk(6, None, vec![vec![WireValue::Text("Bob".into())]]), 4));
                }
                replies.push((refusal(6, ErrorCode::Cancelled), fail_on));
                let io = script(replies, Arc::clone(&flushes));
                let written = Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "social").await.unwrap();
                let mut out = Output {
                    bytes: vec![],
                    flushes,
                    fail_on: Some(fail_on),
                };
                let error = three_prepared_reads()
                    .execute_reads(&root, &mut client, mode, true, &mut out)
                    .await
                    .expect_err("stdout failure must stop the argument file");
                assert_eq!(error.code, 5);
                assert!(
                    error.message.starts_with(
                        "argument set 2: stream incomplete after 0 fully flushed row(s)"
                    )
                );
                assert_eq!(out.flushes.load(Ordering::SeqCst), fail_on);
                let output = String::from_utf8(out.bytes).unwrap();
                assert_eq!(output.matches(r#""event":"result""#).count(), 1);
                let frames = sent_frames(&written.lock().unwrap());
                assert_eq!(
                    frames
                        .iter()
                        .filter(|f| f.header().kind() == FrameKind::QueryCancel)
                        .count(),
                    1
                );
                assert_eq!(
                    frames
                        .iter()
                        .filter(|f| f.header().kind() == FrameKind::ExecutePrepared)
                        .count(),
                    2
                );
                assert_eq!(
                    frames
                        .iter()
                        .filter(|f| f.header().kind() == FrameKind::Prepare)
                        .count(),
                    1
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn rows_file_is_query_or_write_only_exclusive_and_never_silently_ignored() {
    let parsed = options("write", &["--rows-file", "batch.json"])
        .unwrap_or_else(|error| panic!("{}", error.message));
    assert_eq!(
        parsed.rows_file.as_deref(),
        Some(std::path::Path::new("batch.json"))
    );
    for mode in ["buffered", "source", "ordered"] {
        assert!(
            options("query", &["--rows-file", "batch.json", "--read-mode", mode])
                .unwrap_or_else(|error| panic!("{}", error.message))
                .rows_file
                .is_some()
        );
    }
    assert_eq!(
        options("subscribe", &["--rows-file", "batch.json"])
            .err()
            .unwrap()
            .code,
        2
    );
    for flags in [
        vec!["--rows-file"],
        vec!["--rows-file", "a", "--rows-file", "b"],
        vec!["--rows-file", "a", "--param", "name=text:Ada"],
        vec!["--rows-file", "a", "--read-mode", "buffered"],
        vec!["--rows-file", "a", "--max-batches", "1"],
    ] {
        assert_eq!(options("write", &flags).err().unwrap().code, 2);
    }
}

#[test]
fn remote_batch_submits_one_native_request_and_reports_only_its_terminal() {
    let ((), report) = run_async_under_lab(0x79a0_0601, |root| async move {
        for csv in [false, true] {
            for completion in [
                Outcome::WriteCommitted {
                    seq: 7,
                    statements: 2,
                },
                Outcome::ReadClosed {
                    seq: 7,
                    statements: 2,
                },
            ] {
                for robot in [false, true] {
                    let flushes = Arc::new(AtomicUsize::new(0));
                    let io = script(
                        vec![
                            (chunk(4, Some(vec![]), vec![]), 0),
                            (
                                result(
                                    FrameKind::SnapshotResultEnd,
                                    4,
                                    &ResultEnd {
                                        outcome: completion,
                                        rows: 0,
                                    },
                                ),
                                0,
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
                    prepared_batch(csv)
                        .execute(&root, &mut client, robot, &mut out)
                        .await
                        .unwrap_or_else(|error| panic!("{}", error.message));
                    assert_eq!(out.flushes.load(Ordering::SeqCst), 1);
                    let text = String::from_utf8(out.bytes).unwrap();
                    assert_eq!(text.lines().count(), 1);
                    if robot {
                        assert_eq!(
                            text.trim_end(),
                            r#"{"v":1,"event":"result","kind":"written","seq":7,"statements":2,"records":2}"#
                        );
                    } else {
                        assert_eq!(
                            text.trim_end(),
                            "completed 2 argument set(s), 2 statement(s) at seq 7"
                        );
                    }
                    let frames = sent_frames(&written.lock().unwrap());
                    let batches: Vec<_> = frames
                        .iter()
                        .filter(|frame| frame.header().kind() == FrameKind::ExecuteBatch)
                        .collect();
                    assert_eq!(batches.len(), 1);
                    let sent = ExecuteBatch::decode(batches[0].payload()).unwrap();
                    assert_eq!(sent.statement, BATCH_QUERY);
                    assert_eq!(
                        sent.argument_sets,
                        vec![
                            vec![("name".into(), WireValue::Text("Ada".into()))],
                            vec![("name".into(), WireValue::Text("Bob".into()))],
                        ]
                    );
                    assert!(
                        !frames
                            .iter()
                            .any(|frame| frame.header().kind() == FrameKind::Execute)
                    );
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn batch_refusal_or_lost_terminal_never_emits_success_or_retries() {
    let ((), report) = run_async_under_lab(0x79a0_0602, |root| async move {
        for csv in [false, true] {
            for (reply, exit) in [
                (Some(refusal(4, ErrorCode::Statement)), 3),
                (Some(refusal(4, ErrorCode::PermissionDenied)), 3),
                (Some(refusal(4, ErrorCode::Budget)), 3),
                (Some(refusal(4, ErrorCode::Conflict)), 3),
                (Some(refusal(4, ErrorCode::OutcomeUnknown)), 5),
                (Some(end(4, 0)), 5), // A query terminal is not a write completion.
                (None, 5),            // EOF after columns is not successful completion.
            ] {
                let flushes = Arc::new(AtomicUsize::new(0));
                let mut replies = vec![(chunk(4, Some(vec![]), vec![]), 0)];
                replies.extend(reply.map(|frame| (frame, 0)));
                let io = script(replies, Arc::clone(&flushes));
                let written = Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "social").await.unwrap();
                let mut out = Output {
                    bytes: vec![],
                    flushes,
                    fail_on: None,
                };
                let error = prepared_batch(csv)
                    .execute(&root, &mut client, true, &mut out)
                    .await
                    .expect_err("must not accept a missing or refused completion");
                assert_eq!(error.code, exit);
                assert!(out.bytes.is_empty());
                assert_eq!(out.flushes.load(Ordering::SeqCst), 0);
                let frames = sent_frames(&written.lock().unwrap());
                assert_eq!(
                    frames
                        .iter()
                        .filter(|f| f.header().kind() == FrameKind::ExecuteBatch)
                        .count(),
                    1
                );
                assert!(
                    !frames
                        .iter()
                        .any(|f| f.header().kind() == FrameKind::Execute)
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn output_failure_keeps_known_batch_completion_and_never_replays() {
    let ((), report) = run_async_under_lab(0x79a0_0603, |root| async move {
        for csv in [false, true] {
            let flushes = Arc::new(AtomicUsize::new(0));
            let io = script(
                vec![
                    (chunk(4, Some(vec![]), vec![]), 0),
                    (
                        result(
                            FrameKind::SnapshotResultEnd,
                            4,
                            &ResultEnd {
                                outcome: Outcome::WriteCommitted {
                                    seq: 7,
                                    statements: 2,
                                },
                                rows: 0,
                            },
                        ),
                        0,
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
                fail_on: Some(1),
            };
            let error = prepared_batch(csv)
                .execute(&root, &mut client, true, &mut out)
                .await
                .expect_err("failed stdout flush must fail the command");
            assert_eq!(error.code, 5);
            assert!(error.message.contains("completed at seq 7"));
            assert!(error.message.contains("do not replay"));
            // Bytes may already have reached stdout; only the successful exit
            // and terminal together establish complete delivery, not the line.
            assert!(!out.bytes.is_empty());
            let frames = sent_frames(&written.lock().unwrap());
            assert_eq!(
                frames
                    .iter()
                    .filter(|f| f.header().kind() == FrameKind::ExecuteBatch)
                    .count(),
                1
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_smaller_negotiated_frame_refuses_without_sending_a_batch_prefix() {
    let ((), report) = run_async_under_lab(0x79a0_0604, |root| async move {
        for csv in [false, true] {
            let value = "x".repeat(4096);
            let prepared = if csv {
                let schema = CsvSchema::parse("name=text")
                    .unwrap_or_else(|error| panic!("{}", error.message));
                PreparedBatch::parse_csv(BATCH_QUERY, &format!("name\n{value}\n"), &schema, || {
                    Ok(())
                })
            } else {
                PreparedBatch::parse(
                    BATCH_QUERY,
                    &format!(r#"[{{"name":"text:{value}"}}]"#),
                    || Ok(()),
                )
            }
            .unwrap_or_else(|error| panic!("{}", error.message));
            // Source and body fit local admission, but the real handshake
            // advertises only 4096 inclusive bytes. No fake result is supplied.
            let flushes = Arc::new(AtomicUsize::new(0));
            let io = script(vec![], Arc::clone(&flushes));
            let written = Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "social").await.unwrap();
            let mut out = Output {
                bytes: vec![],
                flushes,
                fail_on: None,
            };
            let error = prepared
                .execute(&root, &mut client, true, &mut out)
                .await
                .expect_err("negotiated limit must refuse");
            assert_eq!(error.code, 5);
            assert!(error.message.contains("negotiated frame limit"));
            assert!(out.bytes.is_empty());
            assert_eq!(out.flushes.load(Ordering::SeqCst), 0);
            let frames = sent_frames(&written.lock().unwrap());
            assert_eq!(
                frames.iter().map(|f| f.header().kind()).collect::<Vec<_>>(),
                [FrameKind::Hello, FrameKind::Auth, FrameKind::SelectDatabase]
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn invalid_rows_file_fails_before_a_missing_credential_is_read() {
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for csv in [false, true] {
        for oversized in [false, true] {
            let path = std::env::temp_dir().join(format!(
                "fgdb-remote-batch-{}-{}",
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
            } else if csv {
                // One valid record followed by an invalid typed field. The
                // native decoder must refuse, before the credential is read.
                file.write_all(b"name\n1\nnot_an_integer\n").unwrap();
            } else {
                file.write_all(br#"[{"name":"text:Ada"},{"name":7}]"#)
                    .unwrap();
            }
            drop(file);
            let mut args: Vec<String> = [
                "write",
                BATCH_QUERY,
                "--addr",
                "127.0.0.1:1",
                "--database",
                "social",
                "--token-file",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect();
            // This absent credential would cause open/4 if read first; the
            // malformed source must instead produce local usage/2.
            let token = path.with_extension("missing-token");
            assert!(!token.exists());
            args.push(token.to_str().unwrap().to_owned());
            args.push("--rows-file".into());
            args.push(path.to_str().unwrap().to_owned());
            if csv {
                args.extend(["--csv-types".into(), "name=int64".into()]);
            }
            let mut output = Vec::new();
            let error = super::super::run(&args, true, &mut output).expect_err("invalid input");
            assert_eq!(error.code, 2, "{}", error.message);
            assert!(output.is_empty());
        }
    }
}
