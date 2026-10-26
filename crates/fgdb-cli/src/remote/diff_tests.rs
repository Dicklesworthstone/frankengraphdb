//! Finite diff CLI parsing and delivery through the real framed client.

use super::super::diff;
use super::*;
use fgdb_protocol::body::{DiffMode, DiffResultChunk, DiffResultEnd, ExecuteDiff};

fn cuts(mode: DiffMode) -> diff::Options {
    diff::Options {
        before: u64::MAX,
        after: 0,
        mode,
    }
}

fn diff_chunk(columns: bool, entries: Vec<(i128, Vec<WireValue>)>) -> Frame {
    result(
        FrameKind::DiffResultChunk,
        4,
        &DiffResultChunk {
            columns: columns.then(|| vec!["value\n".into()]),
            entries,
        },
    )
}

fn diff_end(before: u64, after: u64, rows: u64) -> Frame {
    result(
        FrameKind::DiffResultEnd,
        4,
        &DiffResultEnd {
            before,
            after,
            rows,
        },
    )
}

#[test]
fn remote_diff_requires_exact_explicit_endpoints_and_its_own_stream_mode() {
    let parsed = options(
        "diff",
        &[
            "--before",
            "18446744073709551615",
            "--after",
            "0",
            "--param",
            "p=uint:18446744073709551615",
        ],
    )
    .unwrap_or_else(|error| panic!("{}", error.message));
    assert_eq!(parsed.difference, Some(cuts(DiffMode::Buffered)));
    assert_eq!(
        parsed.parameters,
        [("p".into(), WireValue::Count(u64::MAX))]
    );
    let parsed = options("diff", &["--before", "0", "--after", "0", "--stream"])
        .unwrap_or_else(|error| panic!("{}", error.message));
    assert_eq!(
        parsed.difference,
        Some(diff::Options {
            before: 0,
            after: 0,
            mode: DiffMode::OrderedStream
        })
    );
    for flags in [
        vec![],
        vec!["--before", "0"],
        vec!["--after", "0"],
        vec!["--before", "+1", "--after", "0"],
        vec!["--before", "-1", "--after", "0"],
        vec!["--before", "1.0", "--after", "0"],
        vec!["--before", "18446744073709551616", "--after", "0"],
        vec!["--before", "0", "--before", "1", "--after", "0"],
        vec!["--before", "0", "--after", "0", "--stream", "--stream"],
    ] {
        assert_eq!(
            options("diff", &flags)
                .err()
                .expect("invalid cuts refuse before credentials")
                .code,
            2
        );
    }
    for extra in [
        vec!["--read-mode", "ordered"],
        vec!["--rows-file", "rows.json"],
        vec!["--prepared-jsonl", "rows.jsonl"],
        vec!["--write-mode", "returning"],
        vec!["--max-batches", "1"],
        vec!["--csv-types", "p=int"],
    ] {
        let mut flags = vec!["--before", "0", "--after", "0"];
        flags.extend(extra);
        assert_eq!(options("diff", &flags).err().unwrap().code, 2);
    }
    for verb in ["query", "write", "subscribe"] {
        for flags in [
            vec!["--before", "0"],
            vec!["--after", "0"],
            vec!["--stream"],
        ] {
            assert_eq!(options(verb, &flags).err().unwrap().code, 2);
        }
    }
    let encrypted = options(
        "diff",
        &[
            "--before",
            "1",
            "--after",
            "2",
            "--tls-server-name",
            "db.example.test",
            "--tls-ca-file",
            "ca.pem",
        ],
    )
    .unwrap_or_else(|error| panic!("{}", error.message));
    assert!(encrypted.tls.is_some());
}

#[test]
fn remote_diff_flushes_exact_signed_rows_and_never_invents_snapshot_metadata() {
    let ((), report) = run_async_under_lab(0x79a0_0801, |root| async move {
        for mode in [DiffMode::Buffered, DiffMode::OrderedStream] {
            for robot in [false, true] {
                let flushes = Arc::new(AtomicUsize::new(0));
                let io = script(
                    vec![
                        (diff_chunk(true, vec![]), 0),
                        (
                            diff_chunk(false, vec![(i128::MIN, vec![WireValue::Count(u64::MAX)])]),
                            1,
                        ),
                        (
                            diff_chunk(
                                false,
                                vec![(
                                    i128::MAX,
                                    vec![WireValue::Map(vec![(
                                        "k".into(),
                                        WireValue::Text("v\t".into()),
                                    )])],
                                )],
                            ),
                            2,
                        ),
                        (diff_end(u64::MAX, 0, 2), 3),
                    ],
                    Arc::clone(&flushes),
                );
                let written = Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "social").await.unwrap();
                let mut out = Output {
                    bytes: vec![],
                    flushes: Arc::clone(&flushes),
                    fail_on: None,
                };
                diff::execute(
                    &root,
                    &mut client,
                    cuts(mode),
                    "RETURN $p",
                    vec![("p".into(), WireValue::Count(u64::MAX))],
                    robot,
                    &mut out,
                )
                .await
                .unwrap_or_else(|error| panic!("{}", error.message));
                assert_eq!(flushes.load(Ordering::SeqCst), 4);
                let text = String::from_utf8(out.bytes).unwrap();
                if robot {
                    assert!(text.starts_with(r#"{"v":1,"event":"diff_columns","before":"18446744073709551615","after":"0","semantics":"after_minus_before_bag","columns":["value\n"]}"#));
                    assert!(text.contains(&format!(r#""weight":"{}""#, i128::MIN)));
                    assert!(text.contains(&format!(r#""weight":"{}""#, i128::MAX)));
                    assert!(text.contains(r#""type":"count","value":"18446744073709551615""#));
                    assert!(text.contains(&format!(
                        r#""changed_rows":"2","inserted":"{}","retracted":"{}""#,
                        i128::MAX,
                        i128::MIN.unsigned_abs()
                    )));
                    assert!(!text.contains("snapshot_records"));
                    assert!(!text.contains("work_units"));
                    assert!(!text.contains(r#""seq":"#));
                } else {
                    assert!(text.contains("weight\tvalue\\n"));
                    assert!(text.contains(&format!("{}\t18446744073709551615", i128::MIN)));
                    assert!(text.contains(&format!("+{}\t", i128::MAX)));
                    assert!(text.contains("diff complete 18446744073709551615 -> 0"));
                }
                let frames = sent_frames(&written.lock().unwrap());
                let requests: Vec<_> = frames
                    .iter()
                    .filter(|f| f.header().kind() == FrameKind::ExecuteDiff)
                    .collect();
                assert_eq!(requests.len(), 1);
                let request = ExecuteDiff::decode(requests[0].payload()).unwrap();
                assert_eq!(
                    (request.mode, request.before, request.after),
                    (mode, u64::MAX, 0)
                );
                assert_eq!(
                    request.parameters,
                    [("p".into(), WireValue::Count(u64::MAX))]
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn remote_diff_empty_answer_still_completes_and_late_refusals_leave_only_a_prefix() {
    let ((), report) = run_async_under_lab(0x79a0_0802, |root| async move {
        for late in 0..4 {
            let flushes = Arc::new(AtomicUsize::new(0));
            let mut replies = vec![(diff_chunk(true, vec![]), 0)];
            if late == 0 {
                replies.push((diff_end(u64::MAX, 0, 0), 1));
            } else {
                replies.push((diff_chunk(false, vec![(-1, vec![WireValue::Null])]), 1));
                match late {
                    1 => replies.push((refusal(4, ErrorCode::Budget), 2)),
                    2 => {} // Clean transport EOF still cannot complete a diff.
                    3 => replies.push((diff_end(u64::MAX, 1, 1), 2)),
                    _ => unreachable!(),
                }
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
            let result = diff::execute(
                &root,
                &mut client,
                cuts(DiffMode::OrderedStream),
                "RETURN 1",
                vec![],
                true,
                &mut out,
            )
            .await;
            let text = String::from_utf8(out.bytes).unwrap();
            if late == 0 {
                result.unwrap_or_else(|error| panic!("{}", error.message));
                assert!(text.contains(r#""changed_rows":"0","inserted":"0","retracted":"0""#));
            } else {
                let error = result.expect_err("only matching END completes delivery");
                assert_eq!(error.code, if late == 1 { 3 } else { 5 });
                assert!(
                    error
                        .message
                        .contains("diff incomplete after 1 fully flushed change(s)")
                );
                assert!(!text.contains(r#""event":"result""#));
            }
            let frames = sent_frames(&written.lock().unwrap());
            assert_eq!(
                frames
                    .iter()
                    .filter(|f| f.header().kind() == FrameKind::ExecuteDiff)
                    .count(),
                1
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn remote_diff_output_failure_cancels_and_drains_before_another_query() {
    let ((), report) = run_async_under_lab(0x79a0_0803, |root| async move {
        for fail_on in [1, 2] {
            let flushes = Arc::new(AtomicUsize::new(0));
            let io = script(
                vec![
                    (diff_chunk(true, vec![]), 0),
                    (diff_chunk(false, vec![(1, vec![WireValue::Null])]), 1),
                    (refusal(4, ErrorCode::Cancelled), fail_on),
                    (
                        chunk(6, Some(vec!["value".into()]), vec![vec![WireValue::Int(7)]]),
                        fail_on,
                    ),
                    (end(6, 1), fail_on),
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
            let error = diff::execute(
                &root,
                &mut client,
                cuts(DiffMode::OrderedStream),
                "RETURN 1",
                vec![],
                true,
                &mut out,
            )
            .await
            .expect_err("failed flush is incomplete");
            assert_eq!(error.code, 5);
            assert!(
                error
                    .message
                    .contains("diff incomplete after 0 fully flushed change(s)")
            );
            assert!(
                !String::from_utf8(out.bytes)
                    .unwrap()
                    .contains(r#""event":"result""#)
            );
            let answer = client
                .execute(&root, ExecuteMode::Read, "RETURN 7", vec![])
                .await
                .unwrap();
            assert_eq!(answer.rows, [vec![WireValue::Int(7)]]);
            let frames = sent_frames(&written.lock().unwrap());
            let cancels: Vec<_> = frames
                .iter()
                .filter(|f| f.header().kind() == FrameKind::QueryCancel)
                .collect();
            assert_eq!(cancels.len(), 1);
            assert_eq!(cancels[0].header().stream_id(), StreamId([5; 16]));
            assert!(
                !frames
                    .iter()
                    .any(|f| f.header().kind() == FrameKind::WindowUpdate)
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
