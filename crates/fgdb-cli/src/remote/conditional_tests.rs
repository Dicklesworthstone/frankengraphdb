//! Conditional writes choose a distinct request and retain the existing
//! result/error contract. A conflict never becomes an unconditional retry.

use super::*;
use fgdb_protocol::body::ExecuteConditionalWrite;

#[test]
fn remote_conditional_write_requires_an_exact_revision_and_one_text_write() {
    for (raw, expected) in [("0", 0), ("18446744073709551615", u64::MAX)] {
        let parsed = options("write", &["--if-current", raw, "--param", "value=int:42"])
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(parsed.expected_seq, Some(expected));
        assert_eq!(parsed.mode, ExecuteMode::Write);
        assert_eq!(parsed.parameters, [("value".into(), WireValue::Int(42))]);
    }
    for flags in [
        vec!["--if-current"],
        vec!["--if-current", ""],
        vec!["--if-current", "+1"],
        vec!["--if-current", "-1"],
        vec!["--if-current", "1.0"],
        vec!["--if-current", "1e3"],
        vec!["--if-current", "18446744073709551616"],
        vec!["--if-current", "1", "--if-current", "1"],
        vec!["--if-current", "1", "--rows-file", "rows.json"],
        vec!["--if-current", "1", "--prepared-jsonl", "rows.jsonl"],
        vec!["--if-current", "1", "--write-mode", "returning"],
        vec!["--if-current", "1", "--read-mode", "buffered"],
        vec!["--if-current", "1", "--max-batches", "1"],
    ] {
        assert_eq!(
            options("write", &flags)
                .err()
                .expect("invalid conditional mode refuses before credentials")
                .code,
            2,
        );
    }
    for verb in ["query", "subscribe", "diff"] {
        let mut flags = vec!["--if-current", "1"];
        if verb == "diff" {
            flags.extend(["--before", "0", "--after", "1"]);
        }
        assert_eq!(options(verb, &flags).err().unwrap().code, 2);
    }
    let parsed = options("write", &[]).unwrap_or_else(|error| panic!("{}", error.message));
    assert_eq!(parsed.expected_seq, None);
}

#[test]
fn remote_conditional_write_preserves_return_rows_and_full_width_revision() {
    let ((), report) = run_async_under_lab(0x79a0_0901, |root| async move {
        let expected = (1_u64 << 53) + 1;
        for conditional in [false, true] {
            for committed in [false, true] {
                let outcome = if committed {
                    Outcome::WriteCommitted {
                        seq: expected + 1,
                        statements: 1,
                    }
                } else {
                    Outcome::ReadClosed {
                        seq: expected,
                        statements: 1,
                    }
                };
                let io = script(
                    vec![
                        (
                            chunk(
                                4,
                                Some(vec!["value".into()]),
                                vec![vec![WireValue::Int(42)]],
                            ),
                            0,
                        ),
                        (
                            result(
                                FrameKind::SnapshotResultEnd,
                                4,
                                &ResultEnd { outcome, rows: 1 },
                            ),
                            0,
                        ),
                    ],
                    Arc::default(),
                );
                let written = Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "social").await.unwrap();
                let statement = "MATCH (n) SET n.p=$value RETURN n.p AS value";
                let answer = execute_statement(
                    &root,
                    &mut client,
                    ExecuteMode::Write,
                    conditional.then_some(expected),
                    statement,
                    vec![("value".into(), WireValue::Int(42))],
                )
                .await
                .unwrap();
                assert_eq!(answer.columns, ["value"]);
                assert_eq!(answer.rows, [vec![WireValue::Int(42)]]);
                assert_eq!(answer.outcome, outcome);
                let frames = sent_frames(&written.lock().unwrap());
                let requests = frames
                    .iter()
                    .filter(|frame| {
                        matches!(
                            frame.header().kind(),
                            FrameKind::Execute | FrameKind::ExecuteConditionalWrite
                        )
                    })
                    .collect::<Vec<_>>();
                assert_eq!(requests.len(), 1);
                if conditional {
                    assert_eq!(
                        requests[0].header().kind(),
                        FrameKind::ExecuteConditionalWrite
                    );
                    let request = ExecuteConditionalWrite::decode(requests[0].payload()).unwrap();
                    assert_eq!(request.expected_seq, expected);
                    assert_eq!(request.statement, statement);
                    assert_eq!(request.parameters, [("value".into(), WireValue::Int(42))]);
                } else {
                    assert_eq!(requests[0].header().kind(), FrameKind::Execute);
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn remote_conditional_write_conflicts_and_unknown_outcomes_never_retry() {
    let ((), report) = run_async_under_lab(0x79a0_0902, |root| async move {
        for code in [ErrorCode::Conflict, ErrorCode::OutcomeUnknown] {
            let io = script(
                vec![
                    (refusal(4, code), 0),
                    (
                        chunk(5, Some(vec!["value".into()]), vec![vec![WireValue::Int(8)]]),
                        0,
                    ),
                    (end(5, 1), 0),
                ],
                Arc::default(),
            );
            let written = Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "social").await.unwrap();
            let error = execute_statement(
                &root,
                &mut client,
                ExecuteMode::Write,
                Some(0),
                "CREATE (:Person)",
                vec![],
            )
            .await
            .unwrap_err();
            assert!(matches!(&error, ClientError::Server { code: found, .. } if *found == code));
            let error = failure(error);
            if code == ErrorCode::Conflict {
                assert_eq!(error.code, 3);
                assert_eq!(error.refusal, Code::Conflict);
            } else {
                assert_eq!(error.code, 5);
            }
            let observed = execute_statement(
                &root,
                &mut client,
                ExecuteMode::Read,
                None,
                "MATCH (n) RETURN n.p AS value",
                vec![],
            )
            .await
            .unwrap();
            assert_eq!(observed.rows, [vec![WireValue::Int(8)]]);
            let frames = sent_frames(&written.lock().unwrap());
            let writes = frames
                .iter()
                .filter(|frame| frame.header().kind() == FrameKind::ExecuteConditionalWrite)
                .collect::<Vec<_>>();
            assert_eq!(writes.len(), 1);
            assert_eq!(
                ExecuteConditionalWrite::decode(writes[0].payload())
                    .unwrap()
                    .expected_seq,
                0
            );
            let ordinary = frames
                .iter()
                .filter(|frame| frame.header().kind() == FrameKind::Execute)
                .collect::<Vec<_>>();
            assert_eq!(ordinary.len(), 1);
            assert_eq!(
                Execute::decode(ordinary[0].payload()).unwrap().mode,
                ExecuteMode::Read
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
