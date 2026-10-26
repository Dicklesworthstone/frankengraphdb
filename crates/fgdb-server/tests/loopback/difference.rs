//! Compare complete historical query bags through real FGP/TCP. Small sort
//! runs and flow windows force the external and transport boundaries.

use super::ordered_stream::{empty_scratch, start_ordered};
use super::*;
use fgdb_protocol::body::{DiffMode, DiffResultEnd};

fn args(floor: i64) -> Vec<(String, WireValue)> {
    vec![("floor".into(), WireValue::Int(floor))]
}

async fn connect(cx: &Cx, address: SocketAddr, scope: &Grant) -> Client {
    let mut client = Client::connect(cx, address, token(scope)).await.unwrap();
    client.select(cx, "social").await.unwrap();
    client
}

async fn insert(cx: &Cx, owner: &mut Client, ages: &[i64]) {
    owner
        .execute_batch(
            cx,
            "CREATE (:Person {name:'initial',age:$age})",
            ages.iter()
                .map(|age| vec![("age".into(), WireValue::Int(*age))])
                .collect(),
        )
        .await
        .unwrap();
}

#[test]
fn historical_differences_preserve_bags_endpoints_and_prepared_rebinding() {
    run(async |cx| {
        let (address, shutdown, mut task, directory) =
            start_ordered(cx, "historical-difference-bags", 100).await;
        let mut owner = connect(cx, address, &grant(Rights::ReadWrite)).await;
        let mut reader = connect(cx, address, &grant(Rights::Read)).await;
        insert(cx, &mut owner, &[1, 1, 2]).await;
        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (n:Person) WHERE n.age=1 SET n.age=3",
                vec![],
            )
            .await
            .unwrap();
        let query = "MATCH (n:Person) WHERE n.age >= $floor RETURN n.age AS age";
        let prepared = reader.prepare_read(cx, query, args(99)).await.unwrap();
        let staged_query = "MATCH (n:Person) WHERE n.age >= $floor WITH n AS item \
                            RETURN [item][0].age AS age";
        let staged = reader
            .prepare_read(cx, staged_query, args(99))
            .await
            .unwrap();
        // The requested upper endpoint is now older than the current frontier.
        insert(cx, &mut owner, &[77]).await;
        for mode in [DiffMode::Buffered, DiffMode::OrderedStream] {
            let answer = reader
                .execute_diff(cx, mode, 1, 2, query, args(0))
                .await
                .unwrap();
            assert_eq!(answer.columns, ["age"]);
            assert_eq!(
                answer.entries,
                [(-2, vec![WireValue::Int(1)]), (2, vec![WireValue::Int(3)]),]
            );
            assert_eq!(
                answer.end,
                DiffResultEnd {
                    before: 1,
                    after: 2,
                    rows: 2
                }
            );
            assert_eq!(
                reader
                    .execute_prepared_diff(cx, prepared, mode, 1, 2, args(0))
                    .await
                    .unwrap(),
                answer,
            );
            assert_eq!(
                reader
                    .execute_diff(cx, mode, 1, 2, staged_query, args(0))
                    .await
                    .unwrap(),
                answer,
            );
            assert_eq!(
                reader
                    .execute_prepared_diff(cx, staged, mode, 1, 2, args(0))
                    .await
                    .unwrap(),
                answer,
            );
            let rebound = reader
                .execute_prepared_diff(cx, prepared, mode, 1, 2, args(2))
                .await
                .unwrap();
            assert_eq!(rebound.entries, [(2, vec![WireValue::Int(3)])]);
            assert_eq!(rebound.end.rows, 1);
            assert_eq!(
                reader
                    .execute_prepared_diff(cx, staged, mode, 1, 2, args(2))
                    .await
                    .unwrap(),
                rebound,
            );
            let reverse = reader
                .execute_diff(cx, mode, 2, 1, query, args(0))
                .await
                .unwrap();
            assert_eq!(
                reverse.entries,
                [(2, vec![WireValue::Int(1)]), (-2, vec![WireValue::Int(3)]),]
            );
            assert_eq!(
                reverse.end,
                DiffResultEnd {
                    before: 2,
                    after: 1,
                    rows: 2
                }
            );
            let equal = reader
                .execute_diff(cx, mode, 2, 2, query, args(0))
                .await
                .unwrap();
            assert!(equal.entries.is_empty());
            assert_eq!(
                equal.end,
                DiffResultEnd {
                    before: 2,
                    after: 2,
                    rows: 0
                }
            );
            let first = reader
                .execute_diff(cx, mode, 0, 1, query, args(0))
                .await
                .unwrap();
            assert_eq!(
                first.entries,
                [(2, vec![WireValue::Int(1)]), (1, vec![WireValue::Int(2)]),]
            );
            // Each endpoint's ORDER/LIMIT selects a complete bag first.
            let limited = reader
                .execute_diff(
                    cx,
                    mode,
                    1,
                    2,
                    "MATCH (n:Person) RETURN n.age AS age ORDER BY age DESC LIMIT 1",
                    vec![],
                )
                .await
                .unwrap();
            assert_eq!(
                limited.entries,
                [(-1, vec![WireValue::Int(2)]), (1, vec![WireValue::Int(3)]),]
            );
            let staged_page = reader
                .execute_diff(
                    cx,
                    mode,
                    1,
                    2,
                    "MATCH (n:Person) WITH n.age AS age ORDER BY age DESC LIMIT 2 \
                     UNWIND [age, age + 1] AS value \
                     RETURN DISTINCT value ORDER BY value DESC SKIP 1 LIMIT 2",
                    vec![],
                )
                .await
                .unwrap();
            assert_eq!(staged_page.columns, ["value"]);
            assert_eq!(
                staged_page.entries,
                [
                    (-1, vec![WireValue::Int(1)]),
                    (-1, vec![WireValue::Int(2)]),
                    (1, vec![WireValue::Int(3)]),
                ]
            );
            assert_eq!(staged_page.end.rows, 3);
            let mut delivered = 0;
            let error = reader
                .execute_diff_streaming(
                    cx,
                    mode,
                    1,
                    2,
                    "MATCH (n:Person) WITH n AS item \
                     RETURN 10 / ([item][0].age - 3) AS value LIMIT 0",
                    vec![],
                    |_| Ok(()),
                    |_, _| {
                        delivered += 1;
                        Ok(())
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(server_code(error), ErrorCode::Execution);
            assert_eq!(delivered, 0, "both endpoints must complete before delivery");
            // Processing the next request also fences the failed execution's
            // scratch cleanup; its ERROR frame can arrive just before Drop.
            assert_eq!(
                reader
                    .execute(cx, ExecuteMode::Read, "RETURN 1 AS alive", vec![])
                    .await
                    .unwrap()
                    .rows,
                [vec![WireValue::Int(1)]],
            );
            empty_scratch(&directory).await;
        }
        let count = reader
            .execute_diff(
                cx,
                DiffMode::Buffered,
                0,
                2,
                "MATCH (n:Person) RETURN count(*) AS n",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(count.columns, ["n"]);
        assert_eq!(
            count.entries,
            [
                (-1, vec![WireValue::Count(0)]),
                (1, vec![WireValue::Count(3)]),
            ]
        );
        reader.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn historical_differences_mask_both_cuts_and_charge_only_changed_tuples() {
    run(async |cx| {
        let (address, shutdown, mut task, directory) =
            start_ordered(cx, "historical-difference-authority", 100).await;
        let mut owner = connect(cx, address, &grant(Rights::ReadWrite)).await;
        insert(cx, &mut owner, &[7, 7, 7, 7]).await;
        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (n:Person) SET n.name='hidden change'",
                vec![],
            )
            .await
            .unwrap();
        let mut masked_scope = grant(Rights::Read);
        masked_scope.labels = Scope::only([LabelId(1)]);
        masked_scope.properties = Scope::only([PropertyKeyId(2)]);
        masked_scope.limits.max_rows = 0;
        let mut masked = connect(cx, address, &masked_scope).await;
        for mode in [DiffMode::Buffered, DiffMode::OrderedStream] {
            for query in [
                "MATCH (n:Person) RETURN n.name AS hidden",
                "MATCH (n:Person) WITH n AS item RETURN [item][0].name AS hidden",
            ] {
                let answer = masked
                    .execute_diff(cx, mode, 1, 2, query, vec![])
                    .await
                    .unwrap();
                assert!(
                    answer.entries.is_empty(),
                    "hidden property changes must remain invisible"
                );
                assert_eq!(
                    answer.end.rows, 0,
                    "endpoint rows do not consume delivery quota"
                );
            }
        }
        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (n:Person) SET n.age=8",
                vec![],
            )
            .await
            .unwrap();
        let mut scope = grant(Rights::Read);
        scope.limits.max_rows = 1;
        let mut limited = connect(cx, address, &scope).await;
        let query = "MATCH (n:Person) RETURN n.age AS age";
        for mode in [DiffMode::Buffered, DiffMode::OrderedStream] {
            let mut prefix = 0;
            let error = limited
                .execute_diff_streaming(
                    cx,
                    mode,
                    1,
                    3,
                    query,
                    vec![],
                    |_| Ok(()),
                    |_, _| {
                        prefix += 1;
                        Ok(())
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(server_code(error), ErrorCode::Budget);
            assert!(
                prefix <= 1,
                "no second changed tuple may exceed the signed allowance"
            );
            if mode == DiffMode::Buffered {
                assert_eq!(
                    prefix, 0,
                    "buffered comparison fails before any changed row escapes"
                );
            }
            // A magnitude of four is still ONE changed tuple.
            let first = limited
                .execute_diff(cx, mode, 0, 1, query, vec![])
                .await
                .unwrap();
            assert_eq!(first.entries, [(4, vec![WireValue::Int(7)])]);
            assert_eq!(first.end.rows, 1);
            assert_eq!(
                server_code(
                    limited
                        .execute_diff(cx, mode, 99, 99, query, vec![])
                        .await
                        .unwrap_err()
                ),
                ErrorCode::Execution,
                "even equal endpoints must be admitted against retained history"
            );
            empty_scratch(&directory).await;
        }
        let mut writer = connect(cx, address, &grant(Rights::Write)).await;
        let write_handle = writer
            .prepare_write(cx, "CREATE (:Person {age:20})", vec![])
            .await
            .unwrap();
        for mode in [DiffMode::Buffered, DiffMode::OrderedStream] {
            assert_eq!(
                server_code(
                    writer
                        .execute_diff(
                            cx,
                            mode,
                            0,
                            99,
                            "malformed syntax",
                            vec![("bad".into(), WireValue::Vertex(7))],
                        )
                        .await
                        .unwrap_err()
                ),
                ErrorCode::PermissionDenied,
                "read authority precedes syntax, values, history, and scratch admission"
            );
            assert_eq!(
                server_code(
                    writer
                        .execute_prepared_diff(cx, write_handle, mode, 0, 99, vec![])
                        .await
                        .unwrap_err()
                ),
                ErrorCode::PermissionDenied,
                "read authority also precedes prepared-handle class inspection"
            );
        }
        writer.close(cx).await.unwrap();
        masked.close(cx).await.unwrap();
        limited.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn historical_difference_cancellation_drains_and_prepared_ownership_remains_exact() {
    run(async |cx| {
        let (address, shutdown, mut task, directory) =
            start_ordered(cx, "historical-difference-lifecycle", 100).await;
        let mut owner = connect(cx, address, &grant(Rights::ReadWrite)).await;
        let ages = (0..12).collect::<Vec<_>>();
        insert(cx, &mut owner, &ages).await;
        let scope = grant(Rights::Read);
        let mut reader = connect(cx, address, &scope).await;
        let query = "MATCH (n:Person) RETURN n.age AS age";
        let handle = reader.prepare_read(cx, query, vec![]).await.unwrap();
        for mode in [DiffMode::Buffered, DiffMode::OrderedStream] {
            let mut seen = 0;
            let error = reader
                .execute_prepared_diff_streaming(
                    cx,
                    handle,
                    mode,
                    0,
                    1,
                    vec![],
                    |_| Ok(()),
                    |_, _| {
                        seen += 1;
                        Err(ClientError::Protocol("consumer stopped"))
                    },
                )
                .await
                .unwrap_err();
            assert!(matches!(error, ClientError::Protocol("consumer stopped")));
            assert_eq!(seen, 1, "cancel suppresses subsequent callbacks");
            let answer = reader
                .execute(
                    cx,
                    ExecuteMode::Read,
                    "MATCH (n:Person) RETURN count(*) AS n",
                    vec![],
                )
                .await
                .unwrap();
            assert_eq!(answer.rows, [vec![WireValue::Count(12)]]);
            empty_scratch(&directory).await;
        }
        let mut foreign = connect(cx, address, &scope).await;
        assert_eq!(
            server_code(
                foreign
                    .execute_prepared_diff(cx, handle, DiffMode::Buffered, 0, 1, vec![],)
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement,
        );
        foreign.release_prepared(cx, handle).await.unwrap();
        let write_handle = owner
            .prepare_write(cx, "CREATE (:Person)", vec![])
            .await
            .unwrap();
        assert_eq!(
            server_code(
                owner
                    .execute_prepared_diff(cx, write_handle, DiffMode::Buffered, 0, 1, vec![],)
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement,
        );
        reader.refresh_authority(cx, token(&scope)).await.unwrap();
        assert_eq!(
            server_code(
                reader
                    .execute_prepared_diff(cx, handle, DiffMode::Buffered, 0, 1, vec![],)
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement,
        );
        foreign.close(cx).await.unwrap();
        reader.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn http_historical_differences_share_native_scope_cuts_and_exact_weights() {
    run(async |cx| {
        let server = Arc::new(served(cx, "historical-difference-http").await);
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
        let path = "/v1/databases/social/query-diff";
        let (status, body) = http(
            address,
            "POST",
            "/v1/databases/social/write-batch",
            host,
            Some(&owner),
            r#"{"statement":"CREATE (:Person {name:'same',age:$age})","argument_sets":[{"age":4},{"age":4},{"age":7}]}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let (status, body) = http(
            address,
            "POST",
            "/v1/databases/social/write",
            host,
            Some(&owner),
            r#"{"statement":"MATCH (n:Person) WHERE n.age=4 SET n.age=9"}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let (status, body) = http(
            address,
            "POST",
            path,
            host,
            Some(&reader),
            r#"{"statement":"MATCH (n:Person) WHERE n.age >= $floor RETURN n.age AS age","parameters":{"floor":0},"before":1,"after":"2"}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            r#"{"v":1,"columns":["age"],"changes":[{"weight":"-2","row":[{"type":"int","value":"4"}]},{"weight":"2","row":[{"type":"int","value":"9"}]}],"before":"1","after":"2","changed_rows":"2"}"#,
        );
        let (status, body) = http(
            address,
            "POST",
            path,
            host,
            Some(&reader),
            r#"{"statement":"MATCH (n:Person) WHERE n.age >= $floor RETURN n.age AS age","parameters":{"floor":5},"before":"2","after":"1"}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            r#"{"v":1,"columns":["age"],"changes":[{"weight":"-2","row":[{"type":"int","value":"9"}]}],"before":"2","after":"1","changed_rows":"1"}"#,
        );
        let masked = token(&Grant {
            properties: Scope::only([PropertyKeyId(1)]),
            limits: QueryLimits {
                max_rows: 0,
                ..grant(Rights::Read).limits
            },
            ..grant(Rights::Read)
        });
        let (status, body) = http(
            address,
            "POST",
            path,
            host,
            Some(&masked),
            r#"{"statement":"MATCH (n:Person) RETURN n.age AS hidden","before":"1","after":"2"}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            r#"{"v":1,"columns":["hidden"],"changes":[],"before":"1","after":"2","changed_rows":"0"}"#,
        );
        for body in [
            r#"{"statement":"RETURN 1","before":"18446744073709551616","after":"1"}"#,
            r#"{"statement":"RETURN 1","before":1.0,"after":1}"#,
            r#"{"statement":"RETURN 1","before":"-1","after":"1"}"#,
            r#"{"statement":"RETURN 1","before":"1"}"#,
            r#"{"statement":"RETURN 1","before":"1","after":"2","stream":true}"#,
        ] {
            let (status, response) = http(address, "POST", path, host, Some(&reader), body).await;
            assert_eq!(status, 400, "{response}");
            assert!(response.contains("\"protocol\""), "{response}");
        }
        // The full u64 endpoint is parsed exactly and reaches history
        // admission; it must not wrap or round to an existing revision.
        let (status, body) = http(
            address,
            "POST",
            path,
            host,
            Some(&reader),
            r#"{"statement":"RETURN 1","before":"18446744073709551615","after":"18446744073709551615"}"#,
        )
        .await;
        assert_eq!(status, 500, "{body}");
        assert!(body.contains("\"execution\""), "{body}");
        let writer = token(&grant(Rights::Write));
        let (status, body) = http(address, "POST", path, host, Some(&writer), "invalid json").await;
        assert_eq!(status, 403, "{body}");
        assert!(body.contains("\"permission_denied\""), "{body}");
        let (status, body) = http(
            address,
            "POST",
            "/v1/databases/missing/query-diff",
            host,
            Some(&reader),
            "invalid json",
        )
        .await;
        assert_eq!(status, 404, "{body}");
        let (status, body) = http(address, "GET", path, host, Some(&reader), "").await;
        assert_eq!(status, 405, "{body}");
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}
