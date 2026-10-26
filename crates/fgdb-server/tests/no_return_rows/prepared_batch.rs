//! Reusing a connection-owned script must preserve the native batch boundary:
//! one cumulative allowance, no failed prefix, and one durable completion.

use super::*;

fn arguments(name: &str, tail: &str, divisor: i64, age: i64) -> Vec<(String, WireValue)> {
    vec![
        ("name".into(), WireValue::Text(name.into())),
        ("tail".into(), WireValue::Text(tail.into())),
        ("divisor".into(), WireValue::Int(divisor)),
        ("age".into(), WireValue::Int(age)),
    ]
}

fn refusal(error: ClientError) -> ErrorCode {
    match error {
        ClientError::Server { code, .. } => code,
        other => panic!("expected a server refusal, got {other:?}"),
    }
}

#[test]
fn prepared_batches_rebind_once_per_record_and_reopen_without_failed_prefixes() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "prepared-batch-atomic", |config| {
            config.max_statements = 4;
        })
        .await;
        let mut writer = fixture.client(cx, &grant(Rights::Write, 0)).await;
        let mut observer = fixture.client(cx, &grant(Rights::Read, 100)).await;
        let script = "CREATE (:Person {name:$name,age:100/$divisor}); \
                      CREATE (:Person {name:$tail,age:$age})";
        let handle = writer
            .prepare_write(cx, script, arguments("sample", "sample-tail", 2, 99))
            .await
            .unwrap();
        let before = state(cx, &mut observer).await;
        assert!(before.0.rows.is_empty() && before.1.rows.is_empty());
        assert_eq!(before.0.outcome, Outcome::Rows { seq: 0 });

        assert_eq!(
            writer
                .execute_prepared_batch(
                    cx,
                    handle,
                    vec![
                        arguments("Ann", "Ann-tail", 2, 30),
                        arguments("Bob", "Bob-tail", 4, 20)
                    ],
                )
                .await
                .unwrap(),
            Outcome::WriteCommitted {
                seq: 1,
                statements: 4
            },
            "four statements share one commit and need no result-row grant"
        );
        let committed = state(cx, &mut observer).await;
        assert_eq!(committed.0.rows.len(), 4);
        assert!(committed.1.rows.is_empty());
        assert_eq!(committed.0.outcome, Outcome::Rows { seq: 1 });

        let mut missing = arguments("missing", "missing-tail", 2, 1);
        missing.retain(|(name, _)| name != "age");
        assert_eq!(
            refusal(
                writer
                    .execute_prepared_batch(
                        cx,
                        handle,
                        vec![arguments("prefix", "prefix-tail", 2, 1), missing],
                    )
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement,
            "a later binding failure must not publish an earlier record"
        );

        let mut foreign_identity = arguments("foreign", "foreign-tail", 2, 1);
        foreign_identity.push(("injected".into(), WireValue::Vertex(77)));
        assert_eq!(
            refusal(
                writer
                    .execute_prepared_batch(
                        cx,
                        handle,
                        vec![arguments("prefix", "prefix-tail", 2, 1), foreign_identity],
                    )
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement,
            "wire identities are not authority to refer to graph elements"
        );

        assert!(matches!(
            refusal(
                writer
                    .execute_prepared_batch(
                        cx,
                        handle,
                        vec![
                            arguments("prefix", "prefix-tail", 2, 1),
                            arguments("bad", "bad-tail", 0, 1)
                        ],
                    )
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement | ErrorCode::Execution
        ));
        assert_eq!(
            refusal(
                writer
                    .execute_prepared_batch(
                        cx,
                        handle,
                        vec![arguments("over-limit", "over-limit-tail", 2, 1); 3],
                    )
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Budget,
            "the host limit counts all six statements, not each two-statement record"
        );
        assert!(
            writer
                .execute_prepared_batch(cx, handle, vec![])
                .await
                .is_err()
        );
        let unchanged = state(cx, &mut observer).await;
        assert_eq!(unchanged.0.rows, committed.0.rows);
        assert_eq!(unchanged.1.rows, committed.1.rows);
        assert_eq!(unchanged.0.outcome, Outcome::Rows { seq: 1 });

        assert_eq!(
            writer
                .execute_prepared_batch(cx, handle, vec![arguments("Cy", "Cy-tail", 5, 10)])
                .await
                .unwrap(),
            Outcome::WriteCommitted {
                seq: 2,
                statements: 2
            },
            "the retained script and connection remain usable after refused batches"
        );
        let final_state = state(cx, &mut observer).await;
        assert_eq!(
            final_state
                .0
                .rows
                .iter()
                .map(|row| row[1..].to_vec())
                .collect::<Vec<_>>(),
            [
                ("Ann", 50),
                ("Ann-tail", 30),
                ("Bob", 25),
                ("Bob-tail", 20),
                ("Cy", 20),
                ("Cy-tail", 10),
            ]
            .map(|(name, age)| vec![WireValue::Text(name.into()), WireValue::Int(age)])
        );
        observer.close(cx).await.unwrap();
        writer.close(cx).await.unwrap();
        fixture.stop(cx, 2, 6, 0).await;
    });
}

#[test]
fn prepared_merge_batches_observe_staged_records_and_each_new_committed_snapshot() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "prepared-batch-merge", |_| {}).await;
        let mut writer = fixture.client(cx, &grant(Rights::ReadWrite, 0)).await;
        let mut observer = fixture.client(cx, &grant(Rights::Read, 100)).await;
        let args = |name: &str, age| {
            vec![
                ("name".into(), WireValue::Text(name.into())),
                ("age".into(), WireValue::Int(age)),
            ]
        };
        let handle = writer
            .prepare_write(
                cx,
                "MERGE (n:Person {name:$name}) ON CREATE SET n.age=$age ON MATCH SET n.age=n.age+$age",
                args("representative", 99),
            )
            .await
            .unwrap();
        assert_eq!(
            writer
                .execute_prepared_batch(
                    cx,
                    handle,
                    vec![args("Ann", 2), args("Ann", 3), args("Bob", 7)]
                )
                .await
                .unwrap(),
            Outcome::WriteCommitted {
                seq: 1,
                statements: 3
            }
        );
        assert_eq!(
            writer
                .execute_prepared_batch(cx, handle, vec![args("Ann", 4), args("Bob", 1)])
                .await
                .unwrap(),
            Outcome::WriteCommitted {
                seq: 2,
                statements: 2
            }
        );
        let final_state = state(cx, &mut observer).await;
        assert_eq!(
            final_state
                .0
                .rows
                .iter()
                .map(|row| row[1..].to_vec())
                .collect::<Vec<_>>(),
            [
                vec![WireValue::Text("Ann".into()), WireValue::Int(9)],
                vec![WireValue::Text("Bob".into()), WireValue::Int(8)],
            ]
        );
        let no_effect = writer
            .prepare_write(
                cx,
                "MATCH (n:Person {name:$name}) SET n.age=$age",
                args("missing", 1),
            )
            .await
            .unwrap();
        assert_eq!(
            writer
                .execute_prepared_batch(
                    cx,
                    no_effect,
                    vec![args("missing", 2), args("still-missing", 3)]
                )
                .await
                .unwrap(),
            Outcome::ReadClosed {
                seq: 2,
                statements: 2
            },
            "an all-no-effect batch does not synthesize a write commit"
        );
        observer.close(cx).await.unwrap();
        writer.close(cx).await.unwrap();
        fixture.stop(cx, 2, 2, 0).await;
    });
}

#[test]
fn prepared_batch_handles_are_write_only_connection_owned_and_invalidated_by_refresh() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "prepared-batch-ownership", |_| {}).await;
        let write_scope = grant(Rights::Write, 0);
        let mut writer = fixture.client(cx, &write_scope).await;
        let mut foreign = fixture.client(cx, &write_scope).await;
        let mut reader = fixture.client(cx, &grant(Rights::Read, 100)).await;
        let handle = writer
            .prepare_write(cx, "CREATE (:Person)", vec![])
            .await
            .unwrap();
        assert_eq!(
            refusal(
                foreign
                    .execute_prepared_batch(cx, handle, vec![vec![]])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement
        );
        foreign.release_prepared(cx, handle).await.unwrap();
        let read_handle = reader
            .prepare_read(cx, "MATCH (n:Person) RETURN n", vec![])
            .await
            .unwrap();
        assert_eq!(
            refusal(
                reader
                    .execute_prepared_batch(cx, read_handle, vec![vec![]])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement
        );
        assert_eq!(
            writer
                .execute_prepared_batch(cx, handle, vec![vec![], vec![]])
                .await
                .unwrap(),
            Outcome::WriteCommitted {
                seq: 1,
                statements: 2
            }
        );
        let authority = issuer(issuer_key(), &keys(), 1).unwrap();
        writer
            .refresh_authority(cx, issue_token(&authority, &write_scope).unwrap())
            .await
            .unwrap();
        assert_eq!(
            refusal(
                writer
                    .execute_prepared_batch(cx, handle, vec![vec![]])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement,
            "AUTH_REFRESH cannot retain executable templates from the old authority generation"
        );
        let released = writer
            .prepare_write(cx, "CREATE (:Person)", vec![])
            .await
            .unwrap();
        writer.release_prepared(cx, released).await.unwrap();
        writer.release_prepared(cx, released).await.unwrap();
        assert_eq!(
            refusal(
                writer
                    .execute_prepared_batch(cx, released, vec![vec![]])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement
        );
        assert_eq!(state(cx, &mut reader).await.0.rows.len(), 2);
        reader.close(cx).await.unwrap();
        foreign.close(cx).await.unwrap();
        writer.close(cx).await.unwrap();
        fixture.stop(cx, 1, 2, 0).await;
    });
}
