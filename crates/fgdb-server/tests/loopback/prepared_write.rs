//! Prepared writes through the actual FGP connection and authorized engine.

use super::*;

#[test]
fn fgp_prepared_writes_rebind_atomically_and_enforce_handle_class_and_ownership() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "prepared-write-lifecycle").await;
        let mut scope = grant(Rights::Write);
        scope.limits.max_rows = 0;
        let credential = token(&scope);
        let mut writer = Client::connect(cx, addr, credential.clone()).await.unwrap();
        writer.select(cx, "social").await.unwrap();
        let mut reader = Client::connect(cx, addr, token(&grant(Rights::Read)))
            .await
            .unwrap();
        reader.select(cx, "social").await.unwrap();
        let arguments = |name: &str, age| {
            vec![
                ("name".into(), text(name)),
                ("age".into(), WireValue::Int(age)),
            ]
        };
        let handle = writer
            .prepare_write(
                cx,
                "CREATE (:Person {name:$name,age:$age})",
                arguments("representative only", 99),
            )
            .await
            .unwrap();
        let query = "MATCH (n:Person) RETURN n.name AS name, n.age AS age ORDER BY age";
        let read_handle = reader.prepare_read(cx, query, vec![]).await.unwrap();
        let before = reader
            .execute_prepared(cx, read_handle, vec![])
            .await
            .unwrap();
        assert!(
            before.rows.is_empty(),
            "preparation cannot allocate or publish"
        );
        assert_eq!(before.outcome, Outcome::Rows { seq: 0 });
        for mode in [
            PreparedReadMode::Buffered,
            PreparedReadMode::SourceStream,
            PreparedReadMode::OrderedStream,
        ] {
            assert_eq!(
                server_code(
                    writer
                        .execute_prepared_streaming(
                            cx,
                            mode,
                            handle,
                            arguments("must not write", 1),
                            |_| Ok(()),
                            |_| Ok(()),
                        )
                        .await
                        .unwrap_err()
                ),
                ErrorCode::Statement,
                "a read mode cannot execute a write handle"
            );
        }
        assert_eq!(
            server_code(
                reader
                    .execute_prepared_write(cx, read_handle, vec![])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement,
            "a write mode cannot execute a read handle"
        );
        let mut foreign = Client::connect(cx, addr, credential).await.unwrap();
        foreign.select(cx, "social").await.unwrap();
        assert_eq!(
            server_code(
                foreign
                    .execute_prepared_write(cx, handle, arguments("foreign", 1))
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement
        );
        foreign.release_prepared(cx, handle).await.unwrap();
        foreign.close(cx).await.unwrap();
        let literal = "'); MATCH (n) DETACH DELETE n; //";
        for (seq, name, age) in [(1, "Ann", 30), (2, literal, 7)] {
            assert_eq!(
                writer
                    .execute_prepared_write(cx, handle, arguments(name, age))
                    .await
                    .unwrap(),
                Outcome::WriteCommitted { seq, statements: 1 },
                "write-only max_rows=0 grants need no identity receipts"
            );
        }
        assert_eq!(
            server_code(
                writer
                    .execute_prepared_write(cx, handle, vec![])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement,
            "representative operands are never defaults"
        );
        let atomic = writer.prepare_write(
            cx,
            "CREATE (:Person {name:$name,age:40}); CREATE (:Person {name:'last',age:100/$divisor})",
            vec![("name".into(), text("sample")), ("divisor".into(), WireValue::Int(2))],
        ).await.unwrap();
        let failed = writer
            .execute_prepared_write(
                cx,
                atomic,
                vec![
                    ("name".into(), text("must roll back")),
                    ("divisor".into(), WireValue::Int(0)),
                ],
            )
            .await
            .unwrap_err();
        assert!(matches!(
            server_code(failed),
            ErrorCode::Statement | ErrorCode::Execution
        ));
        let unchanged = reader
            .execute_prepared(cx, read_handle, vec![])
            .await
            .unwrap();
        assert_eq!(
            unchanged.rows,
            [
                vec![text(literal), WireValue::Int(7)],
                vec![text("Ann"), WireValue::Int(30)]
            ]
        );
        assert_eq!(unchanged.outcome, Outcome::Rows { seq: 2 });
        assert_eq!(
            writer
                .execute_prepared_write(
                    cx,
                    atomic,
                    vec![
                        ("name".into(), text("new")),
                        ("divisor".into(), WireValue::Int(2))
                    ],
                )
                .await
                .unwrap(),
            Outcome::WriteCommitted {
                seq: 3,
                statements: 2
            },
            "the entire script has one completion after a fresh rebind"
        );
        writer.release_prepared(cx, atomic).await.unwrap();
        writer.release_prepared(cx, atomic).await.unwrap();
        assert_eq!(
            server_code(
                writer
                    .execute_prepared_write(cx, atomic, vec![])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement
        );
        scope.limits.max_work = 100_000;
        writer.refresh_authority(cx, token(&scope)).await.unwrap();
        assert_eq!(
            server_code(
                writer
                    .execute_prepared_write(cx, handle, arguments("stale", 1))
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement,
            "AUTH_REFRESH discards every write template"
        );
        assert_eq!(
            server_code(
                reader
                    .prepare_write(cx, "malformed syntax", vec![])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::PermissionDenied,
            "Write rights precede syntax inspection"
        );
        let final_rows = reader
            .execute_prepared(cx, read_handle, vec![])
            .await
            .unwrap();
        assert_eq!(final_rows.rows.len(), 4);
        assert_eq!(final_rows.outcome, Outcome::Rows { seq: 3 });
        writer.close(cx).await.unwrap();
        reader.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn fgp_prepared_merge_uses_current_state_and_preparation_preserves_authority_budgets() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "prepared-write-admission").await;
        let mut scope = grant(Rights::ReadWrite);
        scope.limits.max_rows = 0;
        let mut writer = Client::connect(cx, addr, token(&scope)).await.unwrap();
        writer.select(cx, "social").await.unwrap();
        let statement =
            "MERGE (n:Person {name:$name}) ON CREATE SET n.age=$age ON MATCH SET n.age=n.age+$age";
        let args = |age| {
            vec![
                ("name".into(), text("Ann")),
                ("age".into(), WireValue::Int(age)),
            ]
        };
        let handle = writer.prepare_write(cx, statement, args(99)).await.unwrap();
        for (seq, age) in [(1, 2), (2, 3)] {
            assert_eq!(
                writer
                    .execute_prepared_write(cx, handle, args(age))
                    .await
                    .unwrap(),
                Outcome::WriteCommitted { seq, statements: 1 }
            );
        }
        for text in [
            "MATCH (n) RETURN n",
            "CREATE (n:Person) RETURN n",
            "CREATE (n:Person) RETURN 1/0 LIMIT 0",
        ] {
            assert_eq!(
                server_code(writer.prepare_write(cx, text, vec![]).await.unwrap_err()),
                ErrorCode::Statement
            );
        }
        let mut only_write = Client::connect(cx, addr, token(&grant(Rights::Write)))
            .await
            .unwrap();
        only_write.select(cx, "social").await.unwrap();
        assert_eq!(
            server_code(
                only_write
                    .prepare_write(cx, statement, args(1))
                    .await
                    .unwrap_err()
            ),
            ErrorCode::PermissionDenied
        );
        let mut tiny = grant(Rights::Write);
        tiny.limits.max_work = 1;
        let mut limited = Client::connect(cx, addr, token(&tiny)).await.unwrap();
        limited.select(cx, "social").await.unwrap();
        assert_eq!(
            server_code(
                limited
                    .prepare_write(cx, "CREATE (:Person)", vec![])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Budget
        );
        let mut reader = Client::connect(cx, addr, token(&grant(Rights::Read)))
            .await
            .unwrap();
        reader.select(cx, "social").await.unwrap();
        let answer = reader
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (n:Person) RETURN n.name AS name, n.age AS age",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(answer.rows, [vec![text("Ann"), WireValue::Int(5)]]);
        assert_eq!(answer.outcome, Outcome::Rows { seq: 2 });
        reader.close(cx).await.unwrap();
        limited.close(cx).await.unwrap();
        only_write.close(cx).await.unwrap();
        writer.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}
