//! Heterogeneous native reads share admission, authority and flow ownership
//! while retaining each statement's schema, parameters and selected cut.

use super::*;
use fgdb_protocol::body::Execute;
use fgdb_protocol::client::ReadBatchResult;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

fn read(statement: &str, parameters: Vec<(String, WireValue)>) -> Execute {
    Execute {
        mode: ExecuteMode::Read,
        statement: statement.into(),
        parameters,
    }
}

async fn seed(cx: &Cx, client: &mut Client) {
    for statement in [
        "CREATE (:Person {name:'Ann',age:10}), (:Person {name:'Bob',age:21}), \
         (:Company {name:'Hidden',age:999})",
        "MATCH (n:Person {name:'Ann'}) SET n.age=30",
    ] {
        client
            .execute(cx, ExecuteMode::Write, statement, vec![])
            .await
            .unwrap();
    }
}

async fn refuse_without_prefix(
    cx: &Cx,
    client: &mut Client,
    statements: Vec<Execute>,
) -> ClientError {
    let callbacks = AtomicUsize::new(0);
    let error = client
        .execute_read_statements_streaming(
            cx,
            statements,
            |_, _, _| {
                callbacks.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            |_, _| {
                callbacks.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            |_, _, _| {
                callbacks.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .unwrap_err();
    assert_eq!(callbacks.load(Ordering::SeqCst), 0, "{error:?}");
    error
}

#[test]
fn fgp_read_statements_keep_independent_schemas_parameters_and_snapshot_cuts() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "read-statements-values").await;
        let mut writer = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        writer.select(cx, "social").await.unwrap();
        seed(cx, &mut writer).await;
        let mut scope = grant(Rights::Read);
        scope.labels = Scope::only([LabelId(1)]);
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let payload = WireValue::Map(vec![(
            "items".into(),
            WireValue::List(vec![WireValue::Int(1), text("x"), WireValue::Null]),
        )]);
        let answer = reader
            .execute_read_statements(
                cx,
                vec![
                    read("MATCH (n) WHERE n.age < 0 RETURN n.name AS absent", vec![]),
                    read(
                        "MATCH (n) RETURN n.name AS name,n.age AS age ORDER BY name",
                        vec![],
                    ),
                    read(
                        "MATCH (n) RETURN count(*) AS n,sum(n.age) AS total,avg(n.age) AS mean",
                        vec![],
                    ),
                    read(
                        "RETURN $value AS data",
                        vec![("value".into(), payload.clone())],
                    ),
                    // Reusing a name with another type must bind this statement's
                    // own value, not retain the previous statement's map.
                    read(
                        "RETURN $value+$increment AS next",
                        vec![
                            ("value".into(), WireValue::Int(7)),
                            ("increment".into(), WireValue::Int(1)),
                        ],
                    ),
                    read(
                        "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $cut \
                         RETURN n.age AS old_age ORDER BY old_age",
                        vec![("cut".into(), WireValue::Count(1))],
                    ),
                ],
            )
            .await
            .unwrap();
        assert_eq!((answer.end.results, answer.end.rows), (6, 7));
        assert_eq!(
            answer.results,
            vec![
                ReadBatchResult {
                    columns: vec!["absent".into()],
                    rows: vec![],
                    seq: 2,
                },
                ReadBatchResult {
                    columns: vec!["name".into(), "age".into()],
                    rows: vec![
                        vec![text("Ann"), WireValue::Int(30)],
                        vec![text("Bob"), WireValue::Int(21)],
                    ],
                    seq: 2,
                },
                ReadBatchResult {
                    columns: vec!["n".into(), "total".into(), "mean".into()],
                    rows: vec![vec![
                        WireValue::Count(2),
                        WireValue::WideInt(51),
                        WireValue::Average {
                            numerator: 51,
                            denominator: 2,
                        },
                    ]],
                    seq: 2,
                },
                ReadBatchResult {
                    columns: vec!["data".into()],
                    rows: vec![vec![payload]],
                    seq: 2,
                },
                ReadBatchResult {
                    columns: vec!["next".into()],
                    rows: vec![vec![WireValue::Int(8)]],
                    seq: 2,
                },
                ReadBatchResult {
                    columns: vec!["old_age".into()],
                    rows: vec![vec![WireValue::Int(10)], vec![WireValue::Int(21)]],
                    seq: 1,
                },
            ]
        );
        // A later request pins a fresh current cut without changing an explicit
        // historical selector or carrying the previous request's parameters.
        writer
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (n:Person {name:'Ann'}) SET n.age=40",
                vec![],
            )
            .await
            .unwrap();
        let refreshed = reader
            .execute_read_statements(
                cx,
                vec![
                    read("MATCH (n) RETURN sum(n.age) AS total", vec![]),
                    read(
                        "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $cut RETURN sum(n.age) AS old",
                        vec![("cut".into(), WireValue::Count(1))],
                    ),
                ],
            )
            .await
            .unwrap();
        assert_eq!(refreshed.results[0].seq, 3);
        assert_eq!(refreshed.results[0].rows, [vec![WireValue::WideInt(61)]]);
        assert_eq!(refreshed.results[1].seq, 1);
        assert_eq!(refreshed.results[1].rows, [vec![WireValue::WideInt(31)]]);
        reader.close(cx).await.unwrap();
        writer.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn fgp_read_statements_share_signed_allowances_and_check_read_authority() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "read-statements-limits").await;
        let mut writer = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        writer.select(cx, "social").await.unwrap();
        seed(cx, &mut writer).await;
        let statements = vec![
            read("MATCH (n) RETURN n.name AS name ORDER BY name", vec![]),
            read("MATCH (n) RETURN n.age AS age ORDER BY age", vec![]),
        ];
        let mut scope = grant(Rights::Read);
        scope.labels = Scope::only([LabelId(1)]);
        scope.limits.max_rows = 4;
        scope.limits.max_nodes = 4;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let full = reader
            .execute_read_statements(cx, statements.clone())
            .await
            .unwrap();
        assert_eq!((full.end.results, full.end.rows), (2, 4));
        assert_eq!(full.results[0].rows, [vec![text("Ann")], vec![text("Bob")]]);
        assert_eq!(
            full.results[1].rows,
            [vec![WireValue::Int(21)], vec![WireValue::Int(30)]]
        );
        reader.close(cx).await.unwrap();
        for dimension in ["rows", "nodes"] {
            let mut limited = scope.clone();
            if dimension == "rows" {
                limited.limits.max_rows = 3;
            } else {
                limited.limits.max_nodes = 3;
            }
            let mut reader = Client::connect(cx, addr, token(&limited)).await.unwrap();
            reader.select(cx, "social").await.unwrap();
            let error = refuse_without_prefix(cx, &mut reader, statements.clone()).await;
            assert_eq!(server_code(error), ErrorCode::Budget, "{dimension}");
            for statement in &statements {
                let one = reader
                    .execute_read_statements(cx, vec![statement.clone()])
                    .await
                    .unwrap();
                assert_eq!((one.end.results, one.end.rows), (1, 2));
                assert_eq!(one.results[0].seq, 2);
            }
            reader.close(cx).await.unwrap();
        }
        let mut write_only = Client::connect(cx, addr, token(&grant(Rights::Write)))
            .await
            .unwrap();
        write_only.select(cx, "social").await.unwrap();
        let error = refuse_without_prefix(
            cx,
            &mut write_only,
            vec![read("RETURN 1 AS prefix", vec![]), read("RETURN (", vec![])],
        )
        .await;
        assert_eq!(server_code(error), ErrorCode::PermissionDenied);
        write_only.close(cx).await.unwrap();
        writer.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn fgp_late_read_statement_refusals_expose_no_prefix_or_write_effects() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "read-statements-refusals").await;
        let mut client = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        client.select(cx, "social").await.unwrap();
        seed(cx, &mut client).await;
        for (statement, parameters) in [
            ("RETURN (", vec![]),
            ("RETURN $value AS missing", vec![]),
            (
                "RETURN 12/$value AS quotient",
                vec![("value".into(), WireValue::Int(0))],
            ),
            ("CREATE (:Person {name:'forbidden'})", vec![]),
            ("MATCH (n:Person) SET n.age=77 RETURN n", vec![]),
            // The request fits the frame; this later result's single cell does
            // not. Wire preflight must precede the first result's schema too.
            ("RETURN range(1,2000) AS oversized", vec![]),
        ] {
            let error = refuse_without_prefix(
                cx,
                &mut client,
                vec![
                    read(
                        "RETURN $value AS prefix",
                        vec![("value".into(), WireValue::Int(7))],
                    ),
                    read(statement, parameters),
                ],
            )
            .await;
            assert!(
                matches!(error, ClientError::Server { .. }),
                "{statement}: {error:?}"
            );
            if statement.starts_with("CREATE") || statement.contains(" SET ") {
                assert_eq!(server_code(error), ErrorCode::Statement);
            } else if statement.contains("oversized") {
                let ClientError::Server { code, message } = error else {
                    unreachable!()
                };
                assert_eq!(code, ErrorCode::Execution);
                assert!(message.contains("negotiated frame limit"), "{message}");
            }
            let unchanged = client
                .execute(
                    cx,
                    ExecuteMode::Read,
                    "MATCH (n) RETURN n.name AS name,n.age AS age ORDER BY name",
                    vec![],
                )
                .await
                .unwrap();
            assert_eq!(unchanged.outcome, Outcome::Rows { seq: 2 });
            assert_eq!(
                unchanged.rows,
                [
                    vec![text("Ann"), WireValue::Int(30)],
                    vec![text("Bob"), WireValue::Int(21)],
                    vec![text("Hidden"), WireValue::Int(999)],
                ]
            );
        }
        client.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn fgp_read_statement_shape_refusals_leave_the_connection_idle() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "read-statements-shape").await;
        let mut client = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        client.select(cx, "social").await.unwrap();
        let statements: Vec<_> = (0..32)
            .map(|value| {
                read(
                    "RETURN $value AS value",
                    vec![("value".into(), WireValue::Int(value))],
                )
            })
            .collect();
        let full = client
            .execute_read_statements(cx, statements.clone())
            .await
            .unwrap();
        assert_eq!((full.end.results, full.end.rows), (32, 32));
        for (index, result) in full.results.iter().enumerate() {
            assert_eq!(result.columns, ["value"]);
            assert_eq!(result.rows, [vec![WireValue::Int(index as i64)]]);
            assert_eq!(result.seq, 0);
        }
        let mut oversized = statements;
        oversized.push(read("RETURN 32 AS value", vec![]));
        let mut invalid = vec![vec![], oversized];
        for mode in [
            ExecuteMode::Write,
            ExecuteMode::Subscribe,
            ExecuteMode::ReadStream,
            ExecuteMode::ReadOrdered,
        ] {
            invalid.push(vec![
                read("RETURN 1 AS prefix", vec![]),
                Execute {
                    mode,
                    statement: "RETURN 2 AS value".into(),
                    parameters: vec![],
                },
            ]);
        }
        invalid.push(vec![read(
            "RETURN $value AS value",
            vec![
                ("value".into(), WireValue::Int(1)),
                ("value".into(), WireValue::Int(2)),
            ],
        )]);
        for statements in invalid {
            let error = refuse_without_prefix(cx, &mut client, statements).await;
            assert!(matches!(error, ClientError::Body(_)), "{error:?}");
            let next = client
                .execute_read_statements(cx, vec![read("MATCH (n) RETURN count(*) AS n", vec![])])
                .await
                .unwrap();
            assert_eq!(next.results[0].rows, [vec![WireValue::Count(0)]]);
            assert_eq!(next.results[0].seq, 0);
        }
        client.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn fgp_read_statement_streams_share_credit_and_drain_each_callback_cancellation() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "read-statements-streaming").await;
        let mut client = Client::connect(cx, addr, token(&grant(Rights::Read)))
            .await
            .unwrap();
        client.select(cx, "social").await.unwrap();
        let statements = vec![
            read(
                "UNWIND range(1,$end) AS value RETURN value",
                vec![("end".into(), WireValue::Int(12))],
            ),
            read("RETURN 'tail' AS marker,99 AS code", vec![]),
            read("UNWIND range(1,8) AS value RETURN value*10 AS tens", vec![]),
        ];
        let mut schemas = Vec::new();
        let mut rows = Vec::new();
        let mut completed = Vec::new();
        let end = client
            .execute_read_statements_streaming(
                cx,
                statements.clone(),
                |index, seq, columns| {
                    schemas.push((index, seq, columns.to_vec()));
                    Ok(())
                },
                |index, row| {
                    rows.push((index, row));
                    Ok(())
                },
                |index, seq, rows| {
                    completed.push((index, seq, rows));
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert_eq!((end.results, end.rows), (3, 21));
        assert_eq!(
            schemas,
            [
                (0, 0, vec!["value".into()]),
                (1, 0, vec!["marker".into(), "code".into()]),
                (2, 0, vec!["tens".into()]),
            ]
        );
        assert_eq!(completed, [(0, 0, 12), (1, 0, 1), (2, 0, 8)]);
        let mut expected: Vec<_> = (1..=12)
            .map(|value| (0, vec![WireValue::Int(value)]))
            .collect();
        expected.push((1, vec![text("tail"), WireValue::Int(99)]));
        expected.extend((1..=8).map(|value| (2, vec![WireValue::Int(value * 10)])));
        assert_eq!(rows, expected);

        // Every cancellation happens at the second result, after several
        // four-row credit windows. No queued later schema, row or end may
        // reach the caller, and the original terminal must still be drained.
        for stage in 0..3 {
            let stopped = AtomicBool::new(false);
            let callbacks = [
                AtomicUsize::new(0),
                AtomicUsize::new(0),
                AtomicUsize::new(0),
            ];
            let error = client
                .execute_read_statements_streaming(
                    cx,
                    statements.clone(),
                    |index, _, _| {
                        assert!(!stopped.load(Ordering::SeqCst));
                        callbacks[0].fetch_add(1, Ordering::SeqCst);
                        if stage == 0 && index == 1 {
                            stopped.store(true, Ordering::SeqCst);
                            Err(ClientError::Protocol("sink stopped"))
                        } else {
                            Ok(())
                        }
                    },
                    |index, _| {
                        assert!(!stopped.load(Ordering::SeqCst));
                        callbacks[1].fetch_add(1, Ordering::SeqCst);
                        if stage == 1 && index == 1 {
                            stopped.store(true, Ordering::SeqCst);
                            Err(ClientError::Protocol("sink stopped"))
                        } else {
                            Ok(())
                        }
                    },
                    |index, _, _| {
                        assert!(!stopped.load(Ordering::SeqCst));
                        callbacks[2].fetch_add(1, Ordering::SeqCst);
                        if stage == 2 && index == 1 {
                            stopped.store(true, Ordering::SeqCst);
                            Err(ClientError::Protocol("sink stopped"))
                        } else {
                            Ok(())
                        }
                    },
                )
                .await
                .unwrap_err();
            assert!(matches!(error, ClientError::Protocol("sink stopped")));
            assert!(stopped.load(Ordering::SeqCst));
            assert_eq!(
                callbacks.map(|count| count.load(Ordering::SeqCst)),
                match stage {
                    0 => [2, 12, 1],
                    1 => [2, 13, 1],
                    _ => [2, 13, 2],
                }
            );
            let recovered = client
                .execute(cx, ExecuteMode::Read, "RETURN 7 AS recovered", vec![])
                .await
                .unwrap();
            assert_eq!(recovered.rows, [vec![WireValue::Int(7)]]);
            assert_eq!(recovered.outcome, Outcome::Rows { seq: 0 });
        }
        client.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}
