//! HTTP batches retain exact historical cuts, one signed budget and atomic
//! delivery over the same native read session as individual queries.

use super::*;

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
