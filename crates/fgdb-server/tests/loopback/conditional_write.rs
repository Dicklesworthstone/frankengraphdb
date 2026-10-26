//! Exact-frontier writes over FGP and HTTP use one native autocommit owner.
use super::*;

async fn connected(cx: &Cx, address: SocketAddr, scope: &Grant) -> Client {
    let mut client = Client::connect(cx, address, token(scope)).await.unwrap();
    client.select(cx, "social").await.unwrap();
    client
}

#[test]
fn conditional_writes_refuse_stale_clients_and_preserve_returning_and_no_effects() {
    run(async |cx| {
        let (address, shutdown, mut task) = start(cx, "conditional-write-fgp").await;
        let mut winner = connected(cx, address, &grant(Rights::ReadWrite)).await;
        let mut loser = connected(cx, address, &grant(Rights::ReadWrite)).await;
        let seeded = winner
            .execute_write_if_current(cx, 0, "CREATE (:Person {age:1})", vec![])
            .await
            .unwrap();
        assert_eq!(
            seeded.outcome,
            Outcome::WriteCommitted {
                seq: 1,
                statements: 1
            }
        );
        for client in [&mut winner, &mut loser] {
            let observed = client
                .execute(
                    cx,
                    ExecuteMode::Read,
                    "MATCH (n:Person) RETURN n.age AS age",
                    vec![],
                )
                .await
                .unwrap();
            assert_eq!(observed.outcome, Outcome::Rows { seq: 1 });
            assert_eq!(observed.rows, [vec![WireValue::Int(1)]]);
        }
        let answer = winner
            .execute_write_if_current(cx, 1, "MATCH (n:Person) SET n.age=2", vec![])
            .await
            .unwrap();
        assert_eq!(
            answer.outcome,
            Outcome::WriteCommitted {
                seq: 2,
                statements: 1
            }
        );
        for query in [
            "MATCH (n:Person) SET n.age=3 RETURN n.age AS age",
            "MATCH (n:Person) WHERE n.age=99 SET n.age=3",
            "CREATE (n:Person) RETURN 7 AS value",
        ] {
            let error = loser
                .execute_write_if_current(cx, 1, query, vec![])
                .await
                .unwrap_err();
            match error {
                ClientError::Server { code, message } => {
                    assert_eq!(code, ErrorCode::Conflict);
                    assert_eq!(
                        message,
                        "the expected frontier is no longer current; no write was executed"
                    );
                }
                other => panic!("expected conditional conflict, got {other}"),
            }
        }
        let observed = loser
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (n:Person) RETURN n.age AS age",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(observed.rows, [vec![WireValue::Int(2)]]);
        assert_eq!(observed.outcome, Outcome::Rows { seq: 2 });
        let no_effect = loser
            .execute_write_if_current(cx, 2, "MATCH (n:Person) WHERE n.age=99 SET n.age=3", vec![])
            .await
            .unwrap();
        assert_eq!(
            no_effect.outcome,
            Outcome::ReadClosed {
                seq: 2,
                statements: 1
            }
        );
        let returned = loser
            .execute_write_if_current(
                cx,
                2,
                "CREATE (n:Person {age:$age}) RETURN n.age AS age",
                vec![("age".into(), WireValue::Int(9))],
            )
            .await
            .unwrap();
        assert_eq!(returned.columns, ["age"]);
        assert_eq!(returned.rows, [vec![WireValue::Int(9)]]);
        assert_eq!(
            returned.outcome,
            Outcome::WriteCommitted {
                seq: 3,
                statements: 1
            }
        );

        let mut read_only = connected(cx, address, &grant(Rights::Read)).await;
        assert_eq!(
            server_code(
                read_only
                    .execute_write_if_current(
                        cx,
                        0,
                        "invalid syntax",
                        vec![("bad".into(), WireValue::Vertex(7))],
                    )
                    .await
                    .unwrap_err()
            ),
            ErrorCode::PermissionDenied
        );
        let mut restricted = grant(Rights::Write);
        restricted.properties = Scope::only([PropertyKeyId(1)]);
        let mut scoped = connected(cx, address, &restricted).await;
        assert_eq!(
            server_code(
                scoped
                    .execute_write_if_current(cx, 3, "MATCH (n:Person) SET n.age=100", vec![],)
                    .await
                    .unwrap_err()
            ),
            ErrorCode::PermissionDenied
        );
        let mut stats_grant = grant(Rights::Write);
        stats_grant.limits.max_rows = 0;
        let mut stats = connected(cx, address, &stats_grant).await;
        let answer = stats
            .execute_write_if_current(cx, 3, "MATCH (n:Person) SET n.age=4", vec![])
            .await
            .unwrap();
        assert!(answer.rows.is_empty());
        assert_eq!(
            answer.outcome,
            Outcome::WriteCommitted {
                seq: 4,
                statements: 1
            }
        );
        let observed = winner
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (n:Person) RETURN n.age AS age",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(
            observed.rows,
            [vec![WireValue::Int(4)], vec![WireValue::Int(4)]]
        );
        assert_eq!(observed.outcome, Outcome::Rows { seq: 4 });
        stats.close(cx).await.unwrap();
        scoped.close(cx).await.unwrap();
        read_only.close(cx).await.unwrap();
        loser.close(cx).await.unwrap();
        winner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn http_conditional_writes_parse_exact_sequences_and_authorize_before_body() {
    run(async |cx| {
        let server = Arc::new(served(cx, "conditional-write-http").await);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = server.shutdown();
        let mut task = cx
            .spawn(move |child| async move {
                server
                    .serve_http(&child, listener, vec!["127.0.0.1".into()])
                    .await
                    .unwrap();
            })
            .unwrap();
        let owner = token(&grant(Rights::ReadWrite));
        let reader = token(&grant(Rights::Read));
        let host = "127.0.0.1";
        let path = "/v1/databases/social/write-if-current";
        let (status, body) = http(address, "POST", path, host, Some(&owner),
            r#"{"statement":"CREATE (:Person {age:$age})","parameters":{"age":1},"expected_seq":"0"}"#,
        ).await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"seq\":1"), "{body}");
        assert!(body.contains("\"committed\":true"), "{body}");
        let (status, body) = http(
            address,
            "POST",
            path,
            host,
            Some(&owner),
            r#"{"statement":"MATCH (n:Person) SET n.age=2 RETURN n.age AS age","expected_seq":1}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains(r#""rows":[[{"type":"int","value":"2"}]]"#),
            "{body}"
        );
        assert!(body.contains("\"seq\":2"), "{body}");

        for statement in [
            "MATCH (n:Person) SET n.age=3",
            "MATCH (n:Person) WHERE n.age=99 SET n.age=3",
            "CREATE (n:Person) RETURN 7 AS value",
        ] {
            let body = format!(
                r#"{{"statement":{},"expected_seq":"1"}}"#,
                fgdb_protocol::json::quote(statement)
            );
            let (status, response) = http(address, "POST", path, host, Some(&owner), &body).await;
            assert_eq!(status, 409, "{response}");
            assert!(response.contains("\"conflict\""), "{response}");
            assert!(!response.contains("\"seq\""), "{response}");
        }
        let (status, body) = http(
            address,
            "POST",
            path,
            host,
            Some(&owner),
            r#"{"statement":"MATCH (n:Person) WHERE n.age=99 SET n.age=3","expected_seq":"2"}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"seq\":2"), "{body}");
        assert!(body.contains("\"committed\":false"), "{body}");
        let (status, body) = http(
            address,
            "POST",
            "/v1/databases/social/query",
            host,
            Some(&reader),
            r#"{"statement":"MATCH (n:Person) RETURN n.age AS age"}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains(r#""rows":[[{"type":"int","value":"2"}]]"#),
            "{body}"
        );

        for body in [
            r#"{"statement":"CREATE (n)","expected_seq":"18446744073709551616"}"#,
            r#"{"statement":"CREATE (n)","expected_seq":1.0}"#,
            r#"{"statement":"CREATE (n)","expected_seq":"-1"}"#,
            r#"{"statement":"CREATE (n)"}"#,
            r#"{"statement":"CREATE (n)","expected_seq":"2","stream":true}"#,
        ] {
            let (status, response) = http(address, "POST", path, host, Some(&owner), body).await;
            assert_eq!(status, 400, "{response}");
            assert!(response.contains("\"protocol\""), "{response}");
        }
        for expected in ["18446744073709551615", "\"18446744073709551615\""] {
            let body = format!(r#"{{"statement":"CREATE (n)","expected_seq":{expected}}}"#);
            let (status, response) = http(address, "POST", path, host, Some(&owner), &body).await;
            assert_eq!(status, 409, "{response}");
            assert!(response.contains("\"conflict\""), "{response}");
        }
        let (status, body) = http(address, "POST", path, host, Some(&reader), "invalid json").await;
        assert_eq!(status, 403, "{body}");
        assert!(body.contains("\"permission_denied\""), "{body}");
        let (status, body) = http(address, "GET", path, host, Some(&owner), "").await;
        assert_eq!(status, 405, "{body}");
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}
