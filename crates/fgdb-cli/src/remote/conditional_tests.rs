//! Conditional writes choose a distinct request and retain the existing
//! result/error contract. A conflict never becomes an unconditional retry.

use super::*;
use fgdb_protocol::body::ExecuteConditionalWrite;

#[test]
fn remote_conditional_write_requires_an_exact_revision_and_one_text_write() {
    for (raw, expected) in [("0", 0), ("18446744073709551615", u64::MAX)] {
        let parsed = options("write", &["--if-current", raw, "--param", "value=int:42"])
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(parsed.condition, Some(WriteCondition::Frontier(expected)));
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
    assert_eq!(parsed.condition, None);
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
                    conditional.then_some(WriteCondition::Frontier(expected)),
                    statement,
                    vec![("value".into(), WireValue::Int(42))],
                )
                .await
                .unwrap();
                assert_eq!(answer.columns, ["value"]);
                assert_eq!(answer.rows, [vec![WireValue::Int(42)]]);
                assert_eq!(answer.outcome, outcome);
                for robot in [true, false] {
                    let mut output = Vec::new();
                    render_answer(&answer, robot, &mut output)
                        .unwrap_or_else(|error| panic!("{}", error.message));
                    let text = String::from_utf8(output).unwrap();
                    let receipt = text.lines().last().unwrap();
                    if robot {
                        let tag = if committed {
                            "write_committed"
                        } else {
                            "read_closed"
                        };
                        assert!(receipt.contains(&format!(r#""outcome":"{tag}""#)));
                        assert!(receipt.contains(r#""kind":"written""#));
                        assert!(receipt.contains(r#""count":1"#));
                    } else {
                        let verb = if committed {
                            "committed"
                        } else {
                            "completed without changes"
                        };
                        assert!(receipt.contains(&format!("1 row(s), {verb} at seq ")));
                    }
                    assert!(receipt.contains(&(expected + u64::from(committed)).to_string()));
                }
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
                Some(WriteCondition::Frontier(0)),
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

#[test]
fn head_flag_and_discovery_have_unambiguous_cli_and_wire_contracts() {
    let hash = "ab".repeat(32);
    let raw = format!("{}@{hash}", u64::MAX);
    let head = BranchHead::Recorded {
        seq: core::num::NonZeroU64::MAX,
        chain_hash: [0xab; 32],
    };
    for (raw, expected) in [("empty", BranchHead::Empty), (raw.as_str(), head)] {
        let parsed = options("write", &["--if-head", raw])
            .unwrap_or_else(|failure| panic!("{}", failure.message));
        assert_eq!(parsed.condition, Some(WriteCondition::Head(expected)));
    }
    for flags in [
        vec!["--if-head", "1@bad"],
        vec!["--if-head", "empty", "--if-current", "0"],
        vec!["--if-current", "0", "--if-head", "empty"],
        vec!["--if-head", "empty", "--rows-file", "args.json"],
        vec!["--if-head", "empty", "--prepared-jsonl", "args.jsonl"],
    ] {
        assert_eq!(options("write", &flags).err().unwrap().code, 2);
    }
    assert_eq!(
        options("query", &["--if-head", "empty"])
            .err()
            .unwrap()
            .code,
        2
    );
    let base = [
        "--addr",
        "127.0.0.1:7688",
        "--token-file",
        "token",
        "--database",
        "social",
        "branch-head",
    ]
    .map(str::to_owned);
    assert!(
        parse(&base)
            .unwrap_or_else(|failure| panic!("{}", failure.message))
            .branch_head
    );
    for additional in [
        vec!["statement"],
        vec!["--param", "x=int:1"],
        vec!["--if-head", "empty"],
    ] {
        let mut invalid = base.to_vec();
        invalid.extend(additional.into_iter().map(str::to_owned));
        assert_eq!(parse(&invalid).err().unwrap().code, 2);
    }
    let ((), report) = run_async_under_lab(0x2b10_5501, |root| async move {
        let reply = frame(
            FrameKind::BranchHead,
            4,
            StreamId::CONTROL,
            Binding::Ready(ready().binding(session())),
            &head,
        );
        let io = script(
            vec![(reply, 0), (refusal(5, ErrorCode::Conflict), 0)],
            Arc::default(),
        );
        let written = Arc::clone(&io.written);
        let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
        client.select(&root, "social").await.unwrap();
        let observed = client.current_branch_head(&root).await.unwrap();
        assert_eq!(observed, head);
        let error = execute_statement(
            &root,
            &mut client,
            ExecuteMode::Write,
            Some(WriteCondition::Head(observed)),
            "CREATE ()",
            Vec::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            ClientError::Server {
                code: ErrorCode::Conflict,
                ..
            }
        ));
        let frames = sent_frames(&written.lock().unwrap());
        assert_eq!(frames.len(), 5);
        assert_eq!(frames[3].header().kind(), FrameKind::GetBranchHead);
        assert!(frames[3].payload().is_empty());
        assert_eq!(frames[4].header().kind(), FrameKind::ExecuteHeadWrite);
        let write = fgdb_protocol::body::ExecuteHeadWrite::decode(frames[4].payload()).unwrap();
        assert_eq!(write.expected, head);
        assert_eq!(write.statement, "CREATE ()");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
