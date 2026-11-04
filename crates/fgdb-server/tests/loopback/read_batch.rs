//! HTTP and FGP batches retain exact historical cuts, one signed budget and
//! complete admission over the same native read session as individual queries.

use super::*;

#[test]
fn fgp_consistent_read_batches_preserve_masking_history_and_cumulative_limits() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "consistent-read-batch-fgp").await;
        let mut writer = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        writer.select(cx, "social").await.unwrap();
        for statement in [
            "CREATE (:Person {name:'Ann',age:10}), (:Person {name:'Bob',age:20}), (:Company {name:'Hidden',age:999})",
            "MATCH (n:Person {name:'Ann'}) SET n.age=30",
        ] {
            writer
                .execute(cx, ExecuteMode::Write, statement, vec![])
                .await
                .unwrap();
        }
        let mut permitted = grant(Rights::Read);
        permitted.labels = Scope::only([LabelId(1)]);
        permitted.limits.max_rows = 4;
        permitted.limits.max_nodes = 4;
        let statement = "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $cut RETURN n.name AS name,n.age AS age ORDER BY name";
        let arguments = || {
            [1, 2]
                .into_iter()
                .map(|cut| vec![("cut".into(), WireValue::Count(cut))])
                .collect()
        };
        let mut reader = Client::connect(cx, addr, token(&permitted)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let answer = reader
            .execute_read_batch(cx, statement, arguments())
            .await
            .unwrap();
        assert_eq!((answer.end.results, answer.end.rows), (2, 4));
        assert_eq!(answer.results.len(), 2);
        for (result, seq, age) in [(&answer.results[0], 1, 10), (&answer.results[1], 2, 30)] {
            assert_eq!(result.seq, seq);
            assert_eq!(result.columns, ["name", "age"]);
            assert_eq!(
                result.rows,
                vec![
                    vec![text("Ann"), WireValue::Int(age)],
                    vec![text("Bob"), WireValue::Int(20)],
                ]
            );
        }
        let current = reader
            .execute_read_batch(
                cx,
                "MATCH (n) RETURN n.age AS age ORDER BY age",
                vec![vec![], vec![]],
            )
            .await
            .unwrap();
        assert_eq!(current.results[0], current.results[1]);
        assert_eq!(current.results[0].seq, 2);
        for dimension in ["rows", "nodes"] {
            let mut limited = permitted.clone();
            if dimension == "rows" {
                limited.limits.max_rows = 3;
            } else {
                limited.limits.max_nodes = 3;
            }
            let mut client = Client::connect(cx, addr, token(&limited)).await.unwrap();
            client.select(cx, "social").await.unwrap();
            let seen = std::sync::atomic::AtomicUsize::new(0);
            let failure = client
                .execute_read_batch_streaming(
                    cx,
                    statement,
                    arguments(),
                    |_, _, _| {
                        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(())
                    },
                    |_, _| {
                        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(())
                    },
                    |_, _, _| {
                        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(())
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(server_code(failure), ErrorCode::Budget, "{dimension}");
            assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 0);
            // Each read fits by itself; the failed batch does not poison the
            // connection or manufacture a fresh allowance for every result.
            let one = client
                .execute_read_batch(
                    cx,
                    statement,
                    vec![vec![("cut".into(), WireValue::Count(2))]],
                )
                .await
                .unwrap();
            assert_eq!(one.end.rows, 2);
            client.close(cx).await.unwrap();
        }
        for count in [32, 33] {
            let result = writer
                .execute_read_batch(cx, "RETURN 1 AS value", vec![vec![]; count])
                .await;
            if count == 32 {
                let result = result.unwrap();
                assert_eq!((result.end.results, result.end.rows), (32, 32));
                assert!(result.results.iter().all(|result| result.seq == 2));
            } else {
                assert_eq!(server_code(result.unwrap_err()), ErrorCode::Statement);
            }
        }
        reader.close(cx).await.unwrap();
        writer.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn fgp_late_native_and_wire_refusals_release_no_batch_prefix_or_write_effects() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "consistent-read-batch-refusals").await;
        let mut client = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        client.select(cx, "social").await.unwrap();
        for (statement, arguments) in [
            (
                "RETURN 12/$divisor AS value",
                vec![
                    vec![("divisor".into(), WireValue::Int(3))],
                    vec![("divisor".into(), WireValue::Int(0))],
                ],
            ),
            (
                "RETURN $value AS value",
                vec![vec![("value".into(), WireValue::Int(7))], vec![]],
            ),
            ("CREATE (:Person)", vec![vec![]]),
            // The request fits the negotiated frame. Only the later result
            // contains an oversized single cell; the first result must not
            // escape before the complete batch's wire preflight rejects it.
            (
                "RETURN range(1,$end) AS values",
                vec![
                    vec![("end".into(), WireValue::Int(1))],
                    vec![("end".into(), WireValue::Int(2000))],
                ],
            ),
        ] {
            let seen = std::sync::atomic::AtomicUsize::new(0);
            let result = client
                .execute_read_batch_streaming(
                    cx,
                    statement,
                    arguments,
                    |_, _, _| {
                        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(())
                    },
                    |_, _| {
                        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(())
                    },
                    |_, _, _| {
                        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(())
                    },
                )
                .await;
            assert!(
                matches!(result, Err(ClientError::Server { .. })),
                "{statement}: {result:?}"
            );
            if statement == "RETURN range(1,$end) AS values" {
                let Err(ClientError::Server { code, message }) = &result else {
                    unreachable!()
                };
                assert_eq!(*code, ErrorCode::Execution);
                assert!(message.contains("negotiated frame limit"), "{message}");
            }
            assert_eq!(
                seen.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "{statement}"
            );
            let unchanged = client
                .execute(cx, ExecuteMode::Read, "MATCH (n) RETURN n", vec![])
                .await
                .unwrap();
            assert!(unchanged.rows.is_empty());
            assert_eq!(unchanged.outcome, Outcome::Rows { seq: 0 });
        }
        client.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn fgp_consistent_read_batch_callback_cancellation_drains_and_reuses_the_connection() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "consistent-read-batch-cancellation").await;
        let mut client = Client::connect(cx, addr, token(&grant(Rights::Read)))
            .await
            .unwrap();
        client.select(cx, "social").await.unwrap();
        for stage in 0..3 {
            let result = client
                .execute_read_batch_streaming(
                    cx,
                    "UNWIND range(1,20) AS value RETURN value",
                    vec![vec![], vec![]],
                    |_, _, _| {
                        if stage == 0 {
                            Err(ClientError::Protocol("sink stopped"))
                        } else {
                            Ok(())
                        }
                    },
                    |_, _| {
                        if stage == 1 {
                            Err(ClientError::Protocol("sink stopped"))
                        } else {
                            Ok(())
                        }
                    },
                    |_, _, _| Err(ClientError::Protocol("sink stopped")),
                )
                .await;
            assert!(matches!(result, Err(ClientError::Protocol("sink stopped"))));
            let next = client
                .execute_read_batch(cx, "RETURN 7 AS value", vec![vec![]])
                .await
                .unwrap();
            assert_eq!((next.end.results, next.end.rows), (1, 1));
            assert_eq!(next.results[0].rows, vec![vec![WireValue::Int(7)]]);
        }
        client.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn heterogeneous_http_batches_share_one_budget_and_keep_independent_schemas() {
    run(async |cx| {
        let server = Arc::new(served(cx, "heterogeneous-read-batch-http").await);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = server.shutdown();
        let mut task = cx
            .spawn(move |child| async move {
                server
                    .serve_http(&child, listener, vec!["127.0.0.1".into()])
                    .await
                    .unwrap();
            })
            .unwrap();
        let host = "127.0.0.1";
        let path = "/v1/databases/social/query-batch";
        let writer = token(&grant(Rights::ReadWrite));
        for statement in [
            r#"{"statement":"CREATE (:Person {name:'Ann',age:10}), (:Person {name:'Bob',age:21}), (:Company {name:'Hidden',age:999})"}"#,
            r#"{"statement":"MATCH (n:Person {name:'Ann'}) SET n.age=30"}"#,
        ] {
            let (status, body) = http(
                addr,
                "POST",
                "/v1/databases/social/write",
                host,
                Some(&writer),
                statement,
            )
            .await;
            assert_eq!(status, 200, "{body}");
        }
        let mut permitted = grant(Rights::Read);
        permitted.labels = Scope::only([LabelId(1)]);
        permitted.limits.max_rows = 4;
        permitted.limits.max_nodes = 4;
        let reader = token(&permitted);
        let request = r#"{"statements":[
            {"statement":"MATCH (n) FOR SYSTEM_TIME AS OF SEQ $cut RETURN n.name AS name,n.age AS age ORDER BY name","parameters":{"cut":{"$uint64":"1"}}},
            {"statement":"MATCH (n) RETURN count(*) AS n,sum(n.age) AS total,avg(n.age) AS mean"},
            {"statement":"RETURN $payload AS data","parameters":{"payload":{"items":[1,"x",null]}}}
        ]}"#;
        let (status, body) = http(addr, "POST", path, host, Some(&reader), request).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            concat!(
                r#"{"v":1,"results":["#,
                r#"{"columns":["name","age"],"rows":[[{"type":"text","value":"Ann"},{"type":"int","value":"10"}],[{"type":"text","value":"Bob"},{"type":"int","value":"21"}]],"seq":1},"#,
                r#"{"columns":["n","total","mean"],"rows":[[{"type":"count","value":"2"},{"type":"wideint","value":"51"},{"type":"average","value":"51/2"}]],"seq":2},"#,
                r#"{"columns":["data"],"rows":[[{"type":"map","value":{"items":{"type":"list","value":[{"type":"int","value":"1"},{"type":"text","value":"x"},{"type":"null"}]}}}]],"seq":2}]}"#,
            )
        );
        for dimension in ["rows", "nodes"] {
            let mut limited = permitted.clone();
            if dimension == "rows" {
                limited.limits.max_rows = 3;
            } else {
                limited.limits.max_nodes = 3;
            }
            let credential = token(&limited);
            let (status, body) = http(addr, "POST", path, host, Some(&credential), request).await;
            assert_eq!(status, 422, "{dimension}: {body}");
            assert!(!body.contains("\"results\""), "{body}");
        }
        for failing in ["RETURN $missing AS absent", "CREATE (:Person)"] {
            let body = format!(
                r#"{{"statements":[{{"statement":"RETURN 'prefix' AS marker"}},{{"statement":{}}}]}}"#,
                fgdb_protocol::json::quote(failing),
            );
            let (status, body) = http(addr, "POST", path, host, Some(&writer), &body).await;
            assert_ne!(status, 200, "{body}");
            assert!(!body.contains("\"results\""), "{body}");
            assert!(!body.contains("prefix"), "{body}");
        }
        for body in [
            r#"{"statements":[]}"#,
            r#"{"statements":["RETURN 1"]}"#,
            r#"{"statements":[{"statement":"RETURN 1","mode":"write"}]}"#,
            r#"{"statements":[{"statement":"RETURN 1"}],"statement":"RETURN 2","argument_sets":[{}]}"#,
            r#"{"statements":null,"statement":"RETURN 2","argument_sets":[{}]}"#,
        ] {
            let (status, body) = http(addr, "POST", path, host, Some(&reader), body).await;
            assert_eq!(status, 400, "{body}");
            assert!(!body.contains("\"results\""), "{body}");
        }
        for count in [32, 33] {
            let statements = vec![r#"{"statement":"RETURN 1 AS one"}"#; count].join(",");
            let body = format!(r#"{{"statements":[{statements}]}}"#);
            let (status, body) = http(addr, "POST", path, host, Some(&writer), &body).await;
            assert_eq!(status, if count == 32 { 200 } else { 400 }, "{body}");
            if count == 32 {
                assert_eq!(body.matches("\"seq\":2").count(), count, "{body}");
            }
        }
        let (status, body) = http(
            addr,
            "POST",
            "/v1/databases/social/query",
            host,
            Some(&writer),
            r#"{"statement":"MATCH (n) RETURN count(*) AS n"}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"value\":\"3\""), "{body}");
        assert!(body.ends_with("\"seq\":2}"), "{body}");
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn http_read_batches_share_authority_and_preserve_each_selected_cut() {
    run(async |cx| {
        let server = Arc::new(served(cx, "read-batch-http").await);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = server.shutdown();
        let mut task = cx
            .spawn(move |child| async move {
                server
                    .serve_http(&child, listener, vec!["127.0.0.1".into()])
                    .await
                    .unwrap();
            })
            .unwrap();
        let host = "127.0.0.1";
        let path = "/v1/databases/social/query-batch";
        let writer = token(&grant(Rights::ReadWrite));
        for statement in [
            r#"{"statement":"CREATE (:Person {name:'Ann',age:10}), (:Person {name:'Bob',age:20}), (:Company {name:'Hidden',age:999})"}"#,
            r#"{"statement":"MATCH (n:Person {name:'Ann'}) SET n.age=30"}"#,
        ] {
            let (status, body) = http(
                addr,
                "POST",
                "/v1/databases/social/write",
                host,
                Some(&writer),
                statement,
            )
            .await;
            assert_eq!(status, 200, "{body}");
        }
        let mut permitted = grant(Rights::Read);
        permitted.labels = Scope::only([LabelId(1)]);
        permitted.limits.max_rows = 4;
        permitted.limits.max_nodes = 4;
        let reader = token(&permitted);
        let request = r#"{"statement":"MATCH (n) FOR SYSTEM_TIME AS OF SEQ $cut RETURN n.name AS name,n.age AS age ORDER BY name","argument_sets":[{"cut":{"$uint64":"1"}},{"cut":{"$uint64":"2"}}]}"#;
        let (status, body) = http(addr, "POST", path, host, Some(&reader), request).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            r#"{"v":1,"results":[{"columns":["name","age"],"rows":[[{"type":"text","value":"Ann"},{"type":"int","value":"10"}],[{"type":"text","value":"Bob"},{"type":"int","value":"20"}]],"seq":1},{"columns":["name","age"],"rows":[[{"type":"text","value":"Ann"},{"type":"int","value":"30"}],[{"type":"text","value":"Bob"},{"type":"int","value":"20"}]],"seq":2}]}"#
        );

        // Each query fits alone. A batch must use one signed allowance,
        // rather than minting a fresh permit per argument set.
        for dimension in ["rows", "nodes"] {
            let mut limited = permitted.clone();
            if dimension == "rows" {
                limited.limits.max_rows = 3;
            } else {
                limited.limits.max_nodes = 3;
            }
            let credential = token(&limited);
            let (status, body) = http(addr, "POST", path, host, Some(&credential), request).await;
            assert_eq!(status, 422, "{dimension}: {body}");
            assert!(body.contains("\"budget\""), "{body}");
            assert!(!body.contains("\"results\""), "{body}");
            for cut in [1, 2] {
                let one = format!(
                    r#"{{"statement":"MATCH (n) FOR SYSTEM_TIME AS OF SEQ $cut RETURN n.name AS name,n.age AS age ORDER BY name","argument_sets":[{{"cut":{{"$uint64":"{cut}"}}}}]}}"#
                );
                let (status, body) = http(addr, "POST", path, host, Some(&credential), &one).await;
                assert_eq!(status, 200, "{dimension}, cut {cut}: {body}");
            }
        }

        // A late expression failure, a missing operand or a write cannot
        // expose the earlier result or advance the database.
        for request in [
            r#"{"statement":"RETURN 12/$divisor AS value","argument_sets":[{"divisor":3},{"divisor":0}]}"#,
            r#"{"statement":"RETURN $value AS value","argument_sets":[{"value":7},{}]}"#,
            r#"{"statement":"CREATE (:Person)","argument_sets":[{}]}"#,
            r#"{"statement":"RETURN 1 AS value","argument_sets":[]}"#,
            r#"{"statement":"RETURN 1 AS value","argument_sets":[{},null]}"#,
            r#"{"statement":"RETURN 1 AS value","argument_sets":[{}],"parameters":{}}"#,
        ] {
            let (status, body) = http(addr, "POST", path, host, Some(&writer), request).await;
            assert_ne!(status, 200, "{body}");
            assert!(body.contains("\"error\""), "{body}");
            assert!(!body.contains("\"results\""), "{body}");
        }
        let oversized = format!(
            r#"{{"statement":"RETURN 1 AS value","argument_sets":[{}]}}"#,
            vec!["{}"; 33].join(",")
        );
        let (status, body) = http(addr, "POST", path, host, Some(&writer), &oversized).await;
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("32"), "{body}");
        assert!(!body.contains("\"results\""), "{body}");

        // Rights are checked before syntax, including an otherwise malformed
        // body. A write capability cannot acquire read access through batching.
        let write_only = token(&grant(Rights::Write));
        let (status, body) = http(addr, "POST", path, host, Some(&write_only), "not JSON").await;
        assert_eq!(status, 403, "{body}");
        let (status, _) = http(addr, "GET", path, host, Some(&reader), "").await;
        assert_eq!(status, 405);
        let (status, _) = http(addr, "POST", path, host, None, request).await;
        assert_eq!(status, 401);
        let (status, _) = http(
            addr,
            "POST",
            "/v1/databases/missing/query-batch",
            host,
            Some(&reader),
            request,
        )
        .await;
        assert_eq!(status, 404);

        let (status, body) = http(
            addr,
            "POST",
            "/v1/databases/social/query",
            host,
            Some(&writer),
            r#"{"statement":"MATCH (n) RETURN count(*) AS n"}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"value\":\"3\""), "{body}");
        assert!(body.ends_with("\"seq\":2}"), "{body}");
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}
