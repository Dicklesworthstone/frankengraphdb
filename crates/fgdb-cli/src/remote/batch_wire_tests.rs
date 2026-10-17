//! Reuse the existing scripted transport and the actual FGP client. These
//! assertions inspect emitted wire frames, not a second batch executor.
use super::super::batch::{CsvSchema, PreparedBatch};
use super::*;
use fgdb_protocol::body::ExecuteBatch;

const BATCH_QUERY: &str = "CREATE (:Person {name:$name})";
const BATCH_ROWS: &str = r#"[{"name":"text:Ada"},{"name":"text:Bob"}]"#;

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

#[test]
fn rows_file_is_write_only_exclusive_and_never_silently_ignored() {
    let parsed = options("write", &["--rows-file", "batch.json"])
        .unwrap_or_else(|error| panic!("{}", error.message));
    assert_eq!(
        parsed.rows_file.as_deref(),
        Some(std::path::Path::new("batch.json"))
    );
    for verb in ["query", "subscribe"] {
        assert_eq!(
            options(verb, &["--rows-file", "batch.json"])
                .err()
                .unwrap()
                .code,
            2
        );
    }
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
