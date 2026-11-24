//! Explicit native transactions over real FGP sockets. Provisional statements
//! share one signed allowance and publish only at the separate COMMIT boundary.

use super::*;
use asupersync::io::{AsyncRead, AsyncWrite};
use fgdb_protocol::body::{
    Auth, AuthOk, AuthRefresh, Body, Credential, Empty, ErrorBody, Execute, ExecuteTransaction,
    Hello, Ready, ResultChunk, SelectDatabase, TransactionBegun, TransactionResultEnd,
    TransactionSelector,
};
use fgdb_protocol::transport::{FrameReader, FrameWriter};
use fgdb_protocol::{Binding, Frame, FrameKind, FrameLimits, StreamId};

fn statement(mode: ExecuteMode, text: &str) -> Execute {
    Execute {
        mode,
        statement: text.into(),
        parameters: vec![],
    }
}

async fn connect(cx: &Cx, addr: SocketAddr, scope: &Grant) -> Client {
    let mut client = Client::connect(cx, addr, token(scope)).await.unwrap();
    client.select(cx, "social").await.unwrap();
    client
}

async fn ages(cx: &Cx, client: &mut Client) -> Vec<Vec<WireValue>> {
    client
        .execute(
            cx,
            ExecuteMode::Read,
            "MATCH (n:Person) RETURN n.age AS age ORDER BY age",
            vec![],
        )
        .await
        .unwrap()
        .rows
}

async fn start_http(
    cx: &Cx,
    name: &str,
) -> (
    SocketAddr,
    fgdb_server::Shutdown,
    asupersync::runtime::TaskHandle<()>,
) {
    let server = Arc::new(served(cx, name).await);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = server.shutdown();
    let task = cx
        .spawn(move |child| async move {
            server
                .serve_http(&child, listener, vec!["127.0.0.1".into()])
                .await
                .unwrap();
        })
        .unwrap();
    (address, shutdown, task)
}

async fn http_transaction(address: SocketAddr, scope: &Grant, body: &str) -> (u16, String) {
    http(
        address,
        "POST",
        "/v1/databases/social/transaction",
        "127.0.0.1",
        Some(&token(scope)),
        body,
    )
    .await
}

async fn http_read_transaction(address: SocketAddr, scope: &Grant, body: &str) -> (u16, String) {
    http(
        address,
        "POST",
        "/v1/databases/social/read-transaction",
        "127.0.0.1",
        Some(&token(scope)),
        body,
    )
    .await
}

async fn http_ages(address: SocketAddr) -> String {
    let (status, body) = http(
        address,
        "POST",
        "/v1/databases/social/query",
        "127.0.0.1",
        Some(&token(&grant(Rights::Read))),
        r#"{"statement":"MATCH (n:Person) RETURN n.age AS age ORDER BY age"}"#,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    body
}

#[test]
fn http_read_transactions_need_only_read_authority_and_keep_scope_and_completion() {
    run(async |cx| {
        let (address, shutdown, mut server) = start_http(cx, "http-read-transaction-scope").await;
        let (status, body) = http_transaction(
            address,
            &grant(Rights::ReadWrite),
            r#"{"statements":[{"mode":"write","statement":"CREATE (:Person {name:'Ann',age:21}),(:Company {name:'Hidden',age:99})"}]}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let mut scope = grant(Rights::Read);
        scope.labels = Scope::only([LabelId(1)]);
        scope.properties = Scope::only([PropertyKeyId(2)]);
        let (status, body) = http_read_transaction(
            address,
            &scope,
            r#"{"statements":[
              {"mode":"read","statement":"MATCH (n) RETURN n.name AS name,n.age AS age"},
              {"mode":"read","statement":"MATCH (n:Person) RETURN n.age+$increment AS next","parameters":{"increment":2}}
            ]}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            r#"{"v":1,"basis":1,"results":[{"columns":["name","age"],"rows":[[{"type":"null"},{"type":"int","value":"21"}]]},{"columns":["next"],"rows":[[{"type":"int","value":"23"}]]}],"seq":1,"statements":2,"committed":false}"#
        );
        assert_eq!(
            http_ages(address).await,
            r#"{"v":1,"columns":["age"],"rows":[[{"type":"int","value":"21"}]],"seq":1}"#
        );
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn http_read_transaction_admission_cannot_upgrade_to_a_writer() {
    run(async |cx| {
        let (address, shutdown, mut server) =
            start_http(cx, "http-read-transaction-admission").await;
        let (status, body) =
            http_read_transaction(address, &grant(Rights::Write), "not JSON").await;
        assert_eq!(status, 403, "read rights precede body diagnostics: {body}");
        let (status, body) = http_read_transaction(address, &grant(Rights::Read), "not JSON").await;
        assert_eq!(
            status, 400,
            "an authorized malformed body is diagnosed: {body}"
        );
        for rights in [Rights::Read, Rights::ReadWrite] {
            // This prefix would fail at execution. A later Write envelope
            // must instead refuse during whole-input admission, before BEGIN.
            for control in ["CREATE (:Person {age:9})", "SAVEPOINT held"] {
                let request = format!(
                    r#"{{"statements":[{{"mode":"read","statement":"RETURN 1/0 AS prefix"}},{{"mode":"write","statement":{}}}]}}"#,
                    fgdb_protocol::json::quote(control),
                );
                let (status, body) = http_read_transaction(address, &grant(rights), &request).await;
                assert_eq!(status, 403, "{body}");
                assert!(body.contains("only Read statements"), "{body}");
                assert!(!body.contains("\"results\""), "{body}");
                assert!(!body.contains("\"committed\""), "{body}");
            }
        }
        // Mislabeling graph mutation as a read cannot bypass the native
        // binder's read ceiling, even when the token itself permits writes.
        let (status, body) = http_read_transaction(
            address,
            &grant(Rights::ReadWrite),
            r#"{"statements":[{"mode":"read","statement":"RETURN 1 AS prefix"},{"mode":"read","statement":"CREATE (:Person {age:9})"}]}"#,
        )
        .await;
        assert!(status >= 400, "{body}");
        assert!(!body.contains("\"results\""), "{body}");
        assert!(!body.contains("\"committed\""), "{body}");
        assert_eq!(
            http_ages(address).await,
            r#"{"v":1,"columns":["age"],"rows":[],"seq":0}"#
        );
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn http_read_transactions_refuse_whole_results_on_late_failure_or_shared_limits() {
    run(async |cx| {
        let (address, shutdown, mut server) = start_http(cx, "http-read-transaction-limits").await;
        let mut limited = grant(Rights::Read);
        limited.limits.max_rows = 1;
        let (status, body) = http_read_transaction(
            address,
            &limited,
            r#"{"statements":[{"mode":"read","statement":"RETURN 1 AS first"},{"mode":"read","statement":"RETURN 2 AS second"}]}"#,
        )
        .await;
        assert_eq!(status, 422, "{body}");
        assert!(body.contains("\"budget\""), "{body}");
        assert!(!body.contains("\"results\""), "{body}");
        let (status, body) = http_read_transaction(
            address,
            &grant(Rights::Read),
            r#"{"statements":[{"mode":"read","statement":"RETURN 1 AS prefix"},{"mode":"read","statement":"RETURN $missing AS absent"}]}"#,
        )
        .await;
        assert_eq!(status, 400, "{body}");
        assert!(!body.contains("\"results\""), "{body}");
        for (value, copies) in [("x".repeat(2000), 3), ("\n".repeat(2500), 1)] {
            let statement = format!(
                r#"{{"mode":"read","statement":"RETURN $value AS value","parameters":{{"value":{}}}}}"#,
                fgdb_protocol::json::quote(&value),
            );
            let request = format!(
                r#"{{"statements":[{}]}}"#,
                vec![statement; copies].join(",")
            );
            let (status, body) =
                http_read_transaction(address, &grant(Rights::Read), &request).await;
            assert_eq!(status, 422, "{body}");
            assert!(body.contains("complete transaction response"), "{body}");
            assert!(!body.contains("\"results\""), "{body}");
            assert!(!body.contains("\"committed\""), "{body}");
        }
        // Refusals release the session-local owner and consume no revision.
        let (status, body) = http_read_transaction(
            address,
            &limited,
            r#"{"statements":[{"mode":"read","statement":"RETURN 7 AS value"}]}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            r#"{"v":1,"basis":0,"results":[{"columns":["value"],"rows":[[{"type":"int","value":"7"}]]}],"seq":0,"statements":1,"committed":false}"#
        );
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn http_transactions_share_staged_state_and_publish_one_native_completion() {
    run(async |cx| {
        let (address, shutdown, mut server) = start_http(cx, "http-transaction-atomic").await;
        let scope = grant(Rights::ReadWrite);
        let (status, body) = http_transaction(
            address,
            &scope,
            r#"{"statements":[
              {"mode":"write","statement":"CREATE (n:Person $props) RETURN n.name AS name","parameters":{"props":{"name":"Ann","age":21}}},
              {"mode":"read","statement":"MATCH (n:Person) RETURN n.age AS before"},
              {"mode":"write","statement":"MATCH (n:Person) SET n.age=n.age+$increment RETURN n.age AS after","parameters":{"increment":1}},
              {"mode":"read","statement":"MATCH (n:Person) RETURN count(*) AS n,sum(n.age) AS total"}
            ]}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            r#"{"v":1,"basis":0,"results":[{"columns":["name"],"rows":[[{"type":"text","value":"Ann"}]]},{"columns":["before"],"rows":[[{"type":"int","value":"21"}]]},{"columns":["after"],"rows":[[{"type":"int","value":"22"}]]},{"columns":["n","total"],"rows":[[{"type":"count","value":"1"},{"type":"wideint","value":"22"}]]}],"seq":1,"statements":4,"committed":true}"#
        );
        assert_eq!(
            http_ages(address).await,
            r#"{"v":1,"columns":["age"],"rows":[[{"type":"int","value":"22"}]],"seq":1}"#
        );

        let (status, body) = http_transaction(
            address,
            &scope,
            r#"{"statements":[{"mode":"read","statement":"RETURN 7 AS value"}]}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            r#"{"v":1,"basis":1,"results":[{"columns":["value"],"rows":[[{"type":"int","value":"7"}]]}],"seq":1,"statements":1,"committed":false}"#
        );

        // A successful explicit write keeps its write completion even when
        // no target matches, or when later effects erase its net graph delta.
        let (status, body) = http_transaction(
            address,
            &scope,
            r#"{"statements":[{"mode":"write","statement":"MATCH (n:Person {age:99}) SET n.age=100"}]}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            r#"{"v":1,"basis":1,"results":[{"columns":[],"rows":[]}],"seq":2,"statements":1,"committed":true}"#
        );
        let (status, body) = http_transaction(
            address,
            &scope,
            r#"{"statements":[
              {"mode":"write","statement":"CREATE (:Person {age:99})"},
              {"mode":"write","statement":"MATCH (n:Person {age:99}) DELETE n"}
            ]}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            r#"{"v":1,"basis":2,"results":[{"columns":[],"rows":[]},{"columns":[],"rows":[]}],"seq":3,"statements":2,"committed":true}"#
        );
        assert_eq!(
            http_ages(address).await,
            r#"{"v":1,"columns":["age"],"rows":[[{"type":"int","value":"22"}]],"seq":3}"#
        );
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn http_transactions_abort_late_refusals_and_keep_cumulative_authority() {
    run(async |cx| {
        let (address, shutdown, mut server) = start_http(cx, "http-transaction-refusals").await;
        let scope = grant(Rights::ReadWrite);
        for (mode, statement, expected) in [
            ("read", "RETURN $missing AS absent", 400),
            // Native arithmetic errors remain execution refusals, including
            // a mutation that reads a value staged by the first statement.
            ("write", "CREATE (:Person {age:1/0})", 500),
            ("write", "MATCH (n:Person) SET n.age=n.age/(n.age-10)", 500),
            ("read", "CREATE (:Person {age:99})", 403),
        ] {
            let request = format!(
                r#"{{"statements":[{{"mode":"write","statement":"CREATE (:Person {{age:10}}) RETURN 1 AS prefix"}},{{"mode":{},"statement":{}}}]}}"#,
                fgdb_protocol::json::quote(mode),
                fgdb_protocol::json::quote(statement),
            );
            let (status, body) = http_transaction(address, &scope, &request).await;
            assert_eq!(status, expected, "{statement}: {body}");
            if expected == 500 {
                assert!(body.contains("\"code\":\"execution\""), "{body}");
            }
            assert!(!body.contains("\"results\""), "{body}");
            assert!(!body.contains("\"committed\""), "{body}");
            assert_eq!(
                http_ages(address).await,
                r#"{"v":1,"columns":["age"],"rows":[],"seq":0}"#
            );
        }

        let mut limited = scope.clone();
        limited.limits.max_rows = 1;
        let (status, body) = http_transaction(
            address,
            &limited,
            r#"{"statements":[
              {"mode":"write","statement":"CREATE (:Person {age:10})"},
              {"mode":"read","statement":"RETURN 1 AS first"},
              {"mode":"read","statement":"RETURN 2 AS second"}
            ]}"#,
        )
        .await;
        assert_eq!(status, 422, "{body}");
        assert!(body.contains("\"budget\""), "{body}");
        assert!(!body.contains("\"results\""), "{body}");

        let mut scoped = scope;
        scoped.properties = Scope::only([PropertyKeyId(1)]);
        let (status, body) = http_transaction(
            address,
            &scoped,
            r#"{"statements":[
              {"mode":"write","statement":"CREATE (:Person {name:'prefix'})"},
              {"mode":"write","statement":"CREATE (:Person {age:99})"}
            ]}"#,
        )
        .await;
        assert_eq!(status, 403, "{body}");
        assert!(!body.contains("\"results\""), "{body}");
        assert_eq!(
            http_ages(address).await,
            r#"{"v":1,"columns":["age"],"rows":[],"seq":0}"#
        );

        let (status, body) = http_transaction(address, &grant(Rights::Read), "not JSON").await;
        assert_eq!(status, 403, "write authority precedes parsing: {body}");
        let (status, body) = http_transaction(
            address,
            &grant(Rights::Write),
            r#"{"statements":[
              {"mode":"write","statement":"CREATE (:Person {age:11})"},
              {"mode":"read","statement":"MATCH (n:Person) RETURN n.age"}
            ]}"#,
        )
        .await;
        assert_eq!(status, 403, "a write-only grant cannot read: {body}");
        assert_eq!(
            http_ages(address).await,
            r#"{"v":1,"columns":["age"],"rows":[],"seq":0}"#
        );

        let (status, body) = http_transaction(
            address,
            &grant(Rights::Write),
            r#"{"statements":[{"mode":"write","statement":"CREATE (:Person {age:12})"}]}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            http_ages(address).await,
            r#"{"v":1,"columns":["age"],"rows":[[{"type":"int","value":"12"}]],"seq":1}"#
        );
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn http_transaction_request_admission_rejects_late_invalid_entries_whole() {
    run(async |cx| {
        let (address, shutdown, mut server) = start_http(cx, "http-transaction-input").await;
        let scope = grant(Rights::ReadWrite);
        for invalid in [
            r#"{"statement":"RETURN 1"}"#,
            r#"{"mode":"subscribe","statement":"RETURN 1"}"#,
            r#"{"mode":"read","statement":"   "}"#,
            r#"{"mode":"write","statement":"CREATE (:Person)","unexpected":true}"#,
            r#"{"mode":"read","statement":"RETURN $n","parameters":{"n":{"$uint64":"18446744073709551616"}}}"#,
            r#"{"mode":"read","statement":"RETURN $n","parameters":[]}"#,
            "null",
        ] {
            let request = format!(
                r#"{{"statements":[{{"mode":"write","statement":"CREATE (:Person {{age:1}})"}},{invalid}]}}"#
            );
            let (status, body) = http_transaction(address, &scope, &request).await;
            assert_eq!(status, 400, "{request}: {body}");
            assert!(!body.contains("\"results\""), "{body}");
        }
        for request in [
            r#"{"statements":[]}"#.to_owned(),
            r#"{"statements":{},"statement":"CREATE (:Person)"}"#.to_owned(),
            r#"{"statements":[{"mode":"read","statement":"RETURN 1"}],"extra":1}"#.to_owned(),
            format!(
                r#"{{"statements":[{}]}}"#,
                vec![r#"{"mode":"write","statement":"CREATE (:Person {age:1})"}"#; 33].join(",")
            ),
        ] {
            let (status, body) = http_transaction(address, &scope, &request).await;
            assert_eq!(status, 400, "{body}");
        }
        assert_eq!(
            http_ages(address).await,
            r#"{"v":1,"columns":["age"],"rows":[],"seq":0}"#
        );
        let request = format!(
            r#"{{"statements":[{}]}}"#,
            vec![
                r#"{"mode":"read","statement":"RETURN $value AS value","parameters":{"value":7}}"#;
                32
            ]
            .join(",")
        );
        let (status, body) = http_transaction(address, &scope, &request).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body.matches(r#""columns":["value"]"#).count(), 32);
        assert!(
            body.ends_with(r#""seq":0,"statements":32,"committed":false}"#),
            "{body}"
        );
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn http_transactions_admit_cumulative_and_escaped_json_output_before_commit() {
    run(async |cx| {
        let (address, shutdown, mut server) = start_http(cx, "http-transaction-output").await;
        let scope = grant(Rights::ReadWrite);
        for (value, copies) in [("x".repeat(2000), 3), ("\n".repeat(2500), 1)] {
            let statement = format!(
                r#"{{"mode":"read","statement":"RETURN $value AS value","parameters":{{"value":{}}}}}"#,
                fgdb_protocol::json::quote(&value),
            );
            let request = format!(
                r#"{{"statements":[{{"mode":"write","statement":"CREATE (:Person {{age:50}})"}},{}]}}"#,
                vec![statement; copies].join(","),
            );
            // Every binary row is below 4096 bytes. The HTTP failure is
            // cumulative response retention or JSON escaping, not FGP's cap.
            let (status, body) = http_transaction(address, &scope, &request).await;
            assert_eq!(status, 422, "{body}");
            assert!(body.contains("complete transaction response"), "{body}");
            assert!(!body.contains("\"results\""), "{body}");
            assert!(!body.contains("\"committed\""), "{body}");
            assert_eq!(
                http_ages(address).await,
                r#"{"v":1,"columns":["age"],"rows":[],"seq":0}"#
            );
        }
        // The same first statement fits by itself, and a complete result is
        // available after the earlier output refusals left no committed prefix.
        let request = format!(
            r#"{{"statements":[{{"mode":"write","statement":"CREATE (:Person {{age:60}}) RETURN $value AS value","parameters":{{"value":{}}}}}]}}"#,
            fgdb_protocol::json::quote(&"x".repeat(2000)),
        );
        let (status, body) = http_transaction(address, &scope, &request).await;
        assert_eq!(status, 200, "{body}");
        assert!(body.len() <= 4096);
        assert!(
            body.ends_with(r#""seq":1,"statements":1,"committed":true}"#),
            "{body}"
        );
        assert_eq!(
            http_ages(address).await,
            r#"{"v":1,"columns":["age"],"rows":[[{"type":"int","value":"60"}]],"seq":1}"#
        );
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn read_only_transaction_pins_snapshot_and_scope_while_another_client_writes() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "read-transaction-snapshot").await;
        let mut writer = connect(cx, addr, &grant(Rights::ReadWrite)).await;
        writer
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (:Person {name:'Ann',age:1})",
                vec![],
            )
            .await
            .unwrap();
        let mut scope = grant(Rights::Read);
        scope.labels = Scope::only([LabelId(1)]);
        scope.properties = Scope::only([PropertyKeyId(2)]);
        let mut reader = connect(cx, addr, &scope).await;
        let begun = reader.begin_read_transaction(cx).await.unwrap();
        assert_eq!(begun.basis, 1);
        let read = statement(
            ExecuteMode::Read,
            "MATCH (n:Person) RETURN n.name AS name,n.age AS age",
        );
        let first = reader
            .execute_transaction(cx, begun.handle, &read)
            .await
            .unwrap();
        assert_eq!(first.rows, [vec![WireValue::Null, WireValue::Int(1)]]);
        // A client-side wrong mode sends nothing and preserves its pinned owner.
        assert!(matches!(
            reader
                .execute_transaction(
                    cx,
                    begun.handle,
                    &statement(ExecuteMode::Write, "CREATE (:Person {age:9})"),
                )
                .await,
            Err(ClientError::Protocol(_))
        ));
        writer
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (n:Person) SET n.age=n.age+100",
                vec![],
            )
            .await
            .unwrap();
        let second = reader
            .execute_transaction(cx, begun.handle, &read)
            .await
            .unwrap();
        assert_eq!(second.rows, first.rows);
        assert_eq!(second.end.statement, 2);
        assert_eq!(
            reader.commit_transaction(cx, begun.handle).await.unwrap(),
            Outcome::ReadClosed {
                seq: 1,
                statements: 2
            }
        );
        assert_eq!(ages(cx, &mut reader).await, [vec![WireValue::Int(101)]]);
        // Empty read-only completion also reports its original, now newer cut.
        let empty = reader.begin_read_transaction(cx).await.unwrap();
        assert_eq!(empty.basis, 2);
        assert_eq!(
            reader.commit_transaction(cx, empty.handle).await.unwrap(),
            Outcome::ReadClosed {
                seq: 2,
                statements: 0
            }
        );
        writer.close(cx).await.unwrap();
        reader.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn read_only_transactions_keep_cumulative_rows_and_abort_unrepresentable_output() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "read-transaction-budgets").await;
        let mut scope = grant(Rights::Read);
        scope.limits.max_rows = 1;
        let mut reader = connect(cx, addr, &scope).await;
        let begun = reader.begin_read_transaction(cx).await.unwrap();
        let read = statement(ExecuteMode::Read, "RETURN 1 AS value");
        reader
            .execute_transaction(cx, begun.handle, &read)
            .await
            .unwrap();
        assert_eq!(
            server_code(
                reader
                    .execute_transaction(cx, begun.handle, &read,)
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Budget
        );
        assert!(reader.commit_transaction(cx, begun.handle).await.is_err());
        let next = reader.begin_read_transaction(cx).await.unwrap();
        reader
            .execute_transaction(cx, next.handle, &read)
            .await
            .unwrap();
        assert_eq!(
            reader.commit_transaction(cx, next.handle).await.unwrap(),
            Outcome::ReadClosed {
                seq: 0,
                statements: 1
            }
        );
        reader.close(cx).await.unwrap();

        let mut reader = connect(cx, addr, &grant(Rights::Read)).await;
        let begun = reader.begin_read_transaction(cx).await.unwrap();
        reader
            .execute_transaction(cx, begun.handle, &read)
            .await
            .unwrap();
        // This single list row exceeds one negotiated frame. No schema or row
        // prefix may escape, and the prior successful statement cannot close.
        let mut callbacks = 0;
        let refusal = reader
            .execute_transaction_streaming(
                cx,
                begun.handle,
                &statement(ExecuteMode::Read, "RETURN range(1,700) AS values"),
                |_| {
                    callbacks += 1;
                    Ok(())
                },
                |_| panic!("unrepresentable transaction output must not escape"),
            )
            .await
            .unwrap_err();
        assert_eq!(server_code(refusal), ErrorCode::Budget);
        assert_eq!(callbacks, 0);
        assert!(reader.commit_transaction(cx, begun.handle).await.is_err());
        let empty = reader.begin_read_transaction(cx).await.unwrap();
        reader.rollback_transaction(cx, empty.handle).await.unwrap();
        reader.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn read_transaction_callback_cancellation_releases_the_pin_after_provisional_end() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "read-transaction-cancel").await;
        let mut reader = connect(cx, addr, &grant(Rights::Read)).await;
        for count in [1, 12] {
            let begun = reader.begin_read_transaction(cx).await.unwrap();
            let mut calls = 0;
            let error = reader
                .execute_transaction_streaming(
                    cx,
                    begun.handle,
                    &statement(
                        ExecuteMode::Read,
                        &format!("UNWIND range(1,{count}) AS n RETURN n"),
                    ),
                    |_| Ok(()),
                    |_| {
                        calls += 1;
                        Err(ClientError::Protocol("application rejected read row"))
                    },
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                ClientError::Protocol("application rejected read row")
            ));
            assert_eq!(calls, 1);
            assert!(reader.commit_transaction(cx, begun.handle).await.is_err());
            let next = reader.begin_read_transaction(cx).await.unwrap();
            reader.rollback_transaction(cx, next.handle).await.unwrap();
        }
        reader.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn explicit_statements_share_staged_state_and_publish_one_atomic_commit() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "explicit-transaction-atomic").await;
        let mut writer = connect(cx, addr, &grant(Rights::ReadWrite)).await;
        let mut observer = connect(cx, addr, &grant(Rights::Read)).await;
        let begun = writer.begin_transaction(cx).await.unwrap();
        assert_eq!(begun.basis, 0);
        let inserted = writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(
                    ExecuteMode::Write,
                    "UNWIND range(1,6) AS age CREATE (:Person {age:age})",
                ),
            )
            .await
            .unwrap();
        assert!(inserted.rows.is_empty());
        assert_eq!(inserted.end.handle, begun.handle);
        assert_eq!(inserted.end.statement, 1);
        assert_eq!(inserted.end.rows, 0);
        assert!(ages(cx, &mut observer).await.is_empty());

        // Six rows require more than the fixture's four-row initial credit.
        // The owner reads its own overlay while the second connection sees no
        // graph changes and no extra commit sequence.
        let selected = writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(
                    ExecuteMode::Read,
                    "MATCH (n:Person) RETURN n.age AS age ORDER BY age",
                ),
            )
            .await
            .unwrap();
        assert_eq!(selected.columns, ["age"]);
        assert_eq!(
            selected.rows,
            (1..=6)
                .map(|age| vec![WireValue::Int(age)])
                .collect::<Vec<_>>()
        );
        assert_eq!(selected.end.statement, 2);
        assert_eq!(selected.end.rows, 6);
        let updated = writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(
                    ExecuteMode::Write,
                    "MATCH (n:Person) WHERE n.age=6 SET n.age=60 RETURN n.age AS age",
                ),
            )
            .await
            .unwrap();
        assert_eq!(updated.rows, [vec![WireValue::Int(60)]]);
        assert_eq!(updated.end.statement, 3);
        assert!(ages(cx, &mut observer).await.is_empty());
        assert_eq!(
            writer.commit_transaction(cx, begun.handle).await.unwrap(),
            Outcome::WriteCommitted {
                seq: 1,
                statements: 3,
            }
        );
        let expected: Vec<_> = [1, 2, 3, 4, 5, 60]
            .into_iter()
            .map(|age| vec![WireValue::Int(age)])
            .collect();
        assert_eq!(ages(cx, &mut observer).await, expected);

        // A read-only use of the write-capable owner closes without inventing
        // a write marker or advancing the published graph frontier.
        let begun = writer.begin_transaction(cx).await.unwrap();
        assert_eq!(begun.basis, 1);
        let count = writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(ExecuteMode::Read, "MATCH (n:Person) RETURN count(*) AS n"),
            )
            .await
            .unwrap();
        assert_eq!(count.rows, [vec![WireValue::Count(6)]]);
        assert_eq!(
            writer.commit_transaction(cx, begun.handle).await.unwrap(),
            Outcome::ReadClosed {
                seq: 1,
                statements: 1,
            }
        );

        let begun = writer.begin_transaction(cx).await.unwrap();
        writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(ExecuteMode::Write, "MATCH (n:Person) SET n.age=99"),
            )
            .await
            .unwrap();
        writer.rollback_transaction(cx, begun.handle).await.unwrap();
        assert_eq!(ages(cx, &mut observer).await, expected);
        writer.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn explicit_transactions_continue_on_the_begin_snapshot_across_other_clients_commits() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "explicit-transaction-concurrent-basis").await;
        let mut scope = grant(Rights::ReadWrite);
        scope.labels = Scope::only([LabelId(1)]);
        scope.properties = Scope::only([PropertyKeyId(2)]);
        let mut owner = connect(cx, addr, &scope).await;
        let mut concurrent = connect(cx, addr, &grant(Rights::ReadWrite)).await;

        // The other connection publishes a scope-hidden, disjoint creation.
        // The retained owner must keep its prefix and statement numbering;
        // replaying either CREATE would leave an extra visible vertex.
        let begun = owner.begin_transaction(cx).await.unwrap();
        assert_eq!(begun.basis, 0);
        let first = owner
            .execute_transaction(
                cx,
                begun.handle,
                &statement(
                    ExecuteMode::Write,
                    "CREATE (n:Person {age:10}) RETURN n.age AS age",
                ),
            )
            .await
            .unwrap();
        assert_eq!(first.rows, [vec![WireValue::Int(10)]]);
        assert_eq!(first.end.handle, begun.handle);
        assert_eq!(first.end.statement, 1);
        let advanced = concurrent
            .execute(cx, ExecuteMode::Write, "CREATE (:Company {age:90})", vec![])
            .await
            .unwrap();
        assert_eq!(
            advanced.outcome,
            Outcome::WriteCommitted {
                seq: 1,
                statements: 1,
            }
        );
        assert!(ages(cx, &mut concurrent).await.is_empty());
        let second = owner
            .execute_transaction(
                cx,
                begun.handle,
                &statement(
                    ExecuteMode::Write,
                    "CREATE (n:Person {age:20}) RETURN n.age AS age",
                ),
            )
            .await
            .unwrap();
        assert_eq!(second.rows, [vec![WireValue::Int(20)]]);
        assert_eq!(second.end.handle, begun.handle);
        assert_eq!(second.end.statement, 2);
        assert!(ages(cx, &mut concurrent).await.is_empty());
        assert_eq!(
            owner.commit_transaction(cx, begun.handle).await.unwrap(),
            Outcome::WriteCommitted {
                seq: 2,
                statements: 2,
            }
        );
        let published = concurrent
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (n) RETURN n.age AS age ORDER BY age",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(
            published.rows,
            [
                vec![WireValue::Int(10)],
                vec![WireValue::Int(20)],
                vec![WireValue::Int(90)],
            ]
        );
        assert_eq!(published.outcome, Outcome::Rows { seq: 2 });

        // Overlapping changes also cannot refresh a live owner's basis.
        // Statements continue against BEGIN plus the private writes, and
        // conflict detection remains at the one native COMMIT boundary.
        let begun = owner.begin_transaction(cx).await.unwrap();
        assert_eq!(begun.basis, 2);
        let updated = owner
            .execute_transaction(
                cx,
                begun.handle,
                &statement(
                    ExecuteMode::Write,
                    "MATCH (n:Person) WHERE n.age=10 SET n.age=11 RETURN n.age AS age",
                ),
            )
            .await
            .unwrap();
        assert_eq!(updated.rows, [vec![WireValue::Int(11)]]);
        assert_eq!(updated.end.statement, 1);
        let advanced = concurrent
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (n:Person) WHERE n.age=10 SET n.age=110",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(
            advanced.outcome,
            Outcome::WriteCommitted {
                seq: 3,
                statements: 1,
            }
        );
        let own_rows = owner
            .execute_transaction(
                cx,
                begun.handle,
                &statement(
                    ExecuteMode::Read,
                    "MATCH (n:Person) RETURN n.age AS age ORDER BY age",
                ),
            )
            .await
            .unwrap();
        assert_eq!(
            own_rows.rows,
            [vec![WireValue::Int(11)], vec![WireValue::Int(20)]]
        );
        assert_eq!(own_rows.end.handle, begun.handle);
        assert_eq!(own_rows.end.statement, 2);
        let next = owner
            .execute_transaction(
                cx,
                begun.handle,
                &statement(
                    ExecuteMode::Write,
                    "MATCH (n:Person) WHERE n.age=11 SET n.age=n.age+1 RETURN n.age AS age",
                ),
            )
            .await
            .unwrap();
        assert_eq!(next.rows, [vec![WireValue::Int(12)]]);
        assert_eq!(next.end.statement, 3);
        let aggregate = owner
            .execute_transaction(
                cx,
                begun.handle,
                &statement(
                    ExecuteMode::Read,
                    "MATCH (n:Person) RETURN count(*) AS n,sum(n.age) AS total",
                ),
            )
            .await
            .unwrap();
        assert_eq!(
            aggregate.rows,
            [vec![WireValue::Count(2), WireValue::WideInt(32)]]
        );
        assert_eq!(aggregate.end.handle, begun.handle);
        assert_eq!(aggregate.end.statement, 4);
        assert_eq!(
            server_code(
                owner
                    .commit_transaction(cx, begun.handle)
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Execution
        );
        let published = concurrent
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (n) RETURN n.age AS age ORDER BY age",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(
            published.rows,
            [
                vec![WireValue::Int(20)],
                vec![WireValue::Int(90)],
                vec![WireValue::Int(110)],
            ]
        );
        assert_eq!(published.outcome, Outcome::Rows { seq: 3 });

        // The rejected COMMIT consumed its owner and published no new revision.
        let fresh = owner.begin_transaction(cx).await.unwrap();
        assert_eq!(fresh.basis, 3);
        owner.rollback_transaction(cx, fresh.handle).await.unwrap();
        owner.close(cx).await.unwrap();
        concurrent.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn statement_errors_and_cumulative_delivery_limits_abort_the_whole_prefix() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "explicit-transaction-failures").await;
        let mut writer = connect(cx, addr, &grant(Rights::ReadWrite)).await;
        let mut observer = connect(cx, addr, &grant(Rights::Read)).await;
        let begun = writer.begin_transaction(cx).await.unwrap();
        writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(ExecuteMode::Write, "CREATE (:Person {age:10})"),
            )
            .await
            .unwrap();
        let error = writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(ExecuteMode::Write, "CREATE (:Person {age:1/0})"),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            server_code(error),
            ErrorCode::Statement | ErrorCode::Execution
        ));
        assert!(ages(cx, &mut observer).await.is_empty());
        // Definite statement failure ends the owner; a fresh BEGIN succeeds.
        let next = writer.begin_transaction(cx).await.unwrap();
        assert_eq!(next.basis, 0);
        writer.rollback_transaction(cx, next.handle).await.unwrap();

        let mut scope = grant(Rights::ReadWrite);
        scope.limits.max_rows = 1;
        let mut limited = connect(cx, addr, &scope).await;
        let begun = limited.begin_transaction(cx).await.unwrap();
        limited
            .execute_transaction(
                cx,
                begun.handle,
                &statement(ExecuteMode::Write, "CREATE (:Person {age:20})"),
            )
            .await
            .unwrap();
        let first = limited
            .execute_transaction(
                cx,
                begun.handle,
                &statement(ExecuteMode::Read, "RETURN 1 AS value"),
            )
            .await
            .unwrap();
        assert_eq!(first.rows, [vec![WireValue::Int(1)]]);
        assert_eq!(
            server_code(
                limited
                    .execute_transaction(
                        cx,
                        begun.handle,
                        &statement(ExecuteMode::Read, "RETURN 2 AS value"),
                    )
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Budget
        );
        assert!(ages(cx, &mut observer).await.is_empty());

        // Closing the socket with an uncommitted prefix also drops its owner.
        let begun = writer.begin_transaction(cx).await.unwrap();
        writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(ExecuteMode::Write, "CREATE (:Person {age:30})"),
            )
            .await
            .unwrap();
        drop(writer);
        assert!(ages(cx, &mut observer).await.is_empty());
        limited.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn provisional_callback_cancellation_aborts_before_and_after_end_can_fit() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "explicit-transaction-cancel").await;
        let mut writer = connect(cx, addr, &grant(Rights::ReadWrite)).await;
        let mut observer = connect(cx, addr, &grant(Rights::Read)).await;
        for count in [1, 12] {
            let begun = writer.begin_transaction(cx).await.unwrap();
            writer
                .execute_transaction(
                    cx,
                    begun.handle,
                    &statement(ExecuteMode::Write, "CREATE (:Person {age:50})"),
                )
                .await
                .unwrap();
            let query = statement(
                ExecuteMode::Read,
                &format!("UNWIND range(1,{count}) AS n RETURN n"),
            );
            let mut calls = 0;
            let error = writer
                .execute_transaction_streaming(
                    cx,
                    begun.handle,
                    &query,
                    |_| Ok(()),
                    |_| {
                        calls += 1;
                        Err(ClientError::Protocol(
                            "application rejected provisional row",
                        ))
                    },
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                ClientError::Protocol("application rejected provisional row")
            ));
            assert_eq!(calls, 1);
            // count=1 fits together with END in the initial flow allowance;
            // its QUERY_CANCEL can arrive after the server restores the owner.
            // The next BEGIN is ordered after cancel on the same TCP stream.
            let next = writer.begin_transaction(cx).await.unwrap();
            assert_eq!(next.basis, 0);
            writer.rollback_transaction(cx, next.handle).await.unwrap();
            assert!(ages(cx, &mut observer).await.is_empty());
        }
        writer.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

async fn send_raw<W: AsyncWrite + Unpin>(
    writer: &mut FrameWriter<W>,
    cx: &Cx,
    kind: FrameKind,
    request: u64,
    stream: StreamId,
    binding: Binding,
    body: &impl Body,
) {
    let frame = Frame::new(
        kind,
        request,
        stream,
        binding,
        body.encode().unwrap(),
        FrameLimits::new(4096).unwrap(),
    )
    .unwrap();
    writer.queue(cx, &frame).unwrap();
    writer.send(cx, |_| Ok(())).await.unwrap();
}

async fn raw_connection(
    cx: &Cx,
    addr: SocketAddr,
    credential: Vec<u8>,
) -> (
    FrameReader<impl AsyncRead + Unpin>,
    FrameWriter<impl AsyncWrite + Unpin>,
    Binding,
) {
    let socket = asupersync::net::TcpStream::connect(addr).await.unwrap();
    let (read, write) = socket.into_split();
    let limits = FrameLimits::new(4096).unwrap();
    let mut reader = FrameReader::new(read, limits);
    let mut writer = FrameWriter::new(write, limits);
    send_raw(
        &mut writer,
        cx,
        FrameKind::Hello,
        1,
        StreamId::CONTROL,
        Binding::Transport,
        &Hello {
            min_version: fgdb_protocol::PROTOCOL_VERSION,
            max_version: fgdb_protocol::PROTOCOL_VERSION,
            client_nonce: [7; 32],
            max_frame_len: 4096,
        },
    )
    .await;
    let hello = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
    assert_eq!(hello.header().kind(), FrameKind::HelloAck);
    send_raw(
        &mut writer,
        cx,
        FrameKind::Auth,
        2,
        StreamId::CONTROL,
        Binding::Transport,
        &Auth {
            credential: Credential::WardenCapability(credential),
        },
    )
    .await;
    let auth = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
    let session = AuthOk::decode(auth.payload()).unwrap().session;
    send_raw(
        &mut writer,
        cx,
        FrameKind::SelectDatabase,
        3,
        StreamId::CONTROL,
        Binding::Session(session),
        &SelectDatabase {
            name: "social".into(),
        },
    )
    .await;
    let ready = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
    let binding = Binding::Ready(Ready::decode(ready.payload()).unwrap().binding(session));
    (reader, writer, binding)
}

#[test]
fn raw_write_mode_aborts_a_read_only_owner_even_with_write_credentials() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "read-transaction-raw-write").await;
        let scope = grant(Rights::ReadWrite);
        let (mut reader, mut writer, binding) = raw_connection(cx, addr, token(&scope)).await;
        send_raw(
            &mut writer,
            cx,
            FrameKind::BeginReadTransaction,
            4,
            StreamId::CONTROL,
            binding,
            &Empty,
        )
        .await;
        let begun = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
        assert_eq!(begun.header().kind(), FrameKind::TransactionBegun);
        let begun = TransactionBegun::decode(begun.payload()).unwrap();
        send_raw(
            &mut writer,
            cx,
            FrameKind::ExecuteTransaction,
            5,
            StreamId::CONTROL,
            binding,
            &ExecuteTransaction {
                handle: begun.handle,
                statement: statement(ExecuteMode::Write, "CREATE (:Person {age:999})"),
            },
        )
        .await;
        let error = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
        assert_eq!(error.header().kind(), FrameKind::Error);
        assert_eq!(
            ErrorBody::decode(error.payload()).unwrap().code,
            ErrorCode::PermissionDenied
        );
        send_raw(
            &mut writer,
            cx,
            FrameKind::CommitTransaction,
            6,
            StreamId::CONTROL,
            binding,
            &TransactionSelector {
                handle: begun.handle,
            },
        )
        .await;
        let error = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
        assert_eq!(
            ErrorBody::decode(error.payload()).unwrap().code,
            ErrorCode::Statement
        );
        let mut observer = connect(cx, addr, &grant(Rights::Read)).await;
        assert!(ages(cx, &mut observer).await.is_empty());
        observer.close(cx).await.unwrap();
        drop(reader);
        drop(writer);
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn raw_transaction_handles_are_connection_scoped_and_busy_requests_keep_the_owner() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "explicit-transaction-ownership").await;
        let scope = grant(Rights::ReadWrite);
        let mut owner = connect(cx, addr, &scope).await;
        let own = owner.begin_transaction(cx).await.unwrap();
        owner
            .execute_transaction(
                cx,
                own.handle,
                &statement(ExecuteMode::Write, "CREATE (:Person {age:1})"),
            )
            .await
            .unwrap();
        let credential = token(&scope);
        let (mut reader, mut writer, binding) = raw_connection(cx, addr, credential.clone()).await;
        // The identical bearer on another connection has no transaction owner.
        send_raw(
            &mut writer,
            cx,
            FrameKind::CommitTransaction,
            4,
            StreamId::CONTROL,
            binding,
            &TransactionSelector { handle: own.handle },
        )
        .await;
        let error = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
        assert_eq!(error.header().kind(), FrameKind::Error);
        assert_eq!(
            ErrorBody::decode(error.payload()).unwrap().code,
            ErrorCode::Statement
        );
        send_raw(
            &mut writer,
            cx,
            FrameKind::BeginTransaction,
            5,
            StreamId::CONTROL,
            binding,
            &Empty,
        )
        .await;
        let begun = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
        assert_eq!(begun.header().kind(), FrameKind::TransactionBegun);
        let begun = TransactionBegun::decode(begun.payload()).unwrap();
        assert_ne!(begun.handle, own.handle);
        send_raw(
            &mut writer,
            cx,
            FrameKind::Execute,
            6,
            StreamId::CONTROL,
            binding,
            &statement(ExecuteMode::Write, "CREATE (:Person {age:999})"),
        )
        .await;
        let error = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
        assert_eq!(
            ErrorBody::decode(error.payload()).unwrap().code,
            ErrorCode::Busy
        );
        let replacement = fgdb_warden::CapabilityToken::decode(&credential)
            .unwrap()
            .attenuate(Restriction::MaxRows(1))
            .unwrap()
            .encode();
        send_raw(
            &mut writer,
            cx,
            FrameKind::AuthRefresh,
            7,
            StreamId::CONTROL,
            binding,
            &AuthRefresh {
                credential: Credential::WardenCapability(replacement),
            },
        )
        .await;
        let error = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
        assert_eq!(
            ErrorBody::decode(error.payload()).unwrap().code,
            ErrorCode::Busy
        );

        // The original owner and grant survived both Busy replies. Two rows
        // would fail if AUTH_REFRESH had silently replaced its MaxRows.
        send_raw(
            &mut writer,
            cx,
            FrameKind::ExecuteTransaction,
            8,
            StreamId::CONTROL,
            binding,
            &ExecuteTransaction {
                handle: begun.handle,
                statement: statement(
                    ExecuteMode::Write,
                    "UNWIND [2,3] AS age CREATE (n:Person {age:age}) RETURN n.age AS age",
                ),
            },
        )
        .await;
        let mut values = Vec::new();
        let stream = loop {
            let frame = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
            assert_eq!(frame.header().request_id(), 8);
            assert_eq!(frame.header().binding(), binding);
            match frame.header().kind() {
                FrameKind::TransactionResultChunk => {
                    values.extend(ResultChunk::decode(frame.payload()).unwrap().rows);
                }
                FrameKind::TransactionResultEnd => {
                    let end = TransactionResultEnd::decode(frame.payload()).unwrap();
                    assert_eq!(end.handle, begun.handle);
                    assert_eq!((end.statement, end.rows), (1, 2));
                    break frame.header().stream_id();
                }
                other => panic!("unexpected provisional response: {other:?}"),
            }
        };
        values.sort_by_key(|row| match &row[0] {
            WireValue::Int(age) => *age,
            _ => panic!("integer age"),
        });
        assert_eq!(values, [vec![WireValue::Int(2)], vec![WireValue::Int(3)]]);
        // Deliberately send cancel AFTER consuming the entire END: this is a
        // deterministic witness of the late-cancellation race, not a timing test.
        send_raw(
            &mut writer,
            cx,
            FrameKind::QueryCancel,
            9,
            stream,
            binding,
            &Empty,
        )
        .await;
        send_raw(
            &mut writer,
            cx,
            FrameKind::CommitTransaction,
            10,
            StreamId::CONTROL,
            binding,
            &TransactionSelector {
                handle: begun.handle,
            },
        )
        .await;
        let error = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
        assert_eq!(error.header().request_id(), 10);
        assert_eq!(
            ErrorBody::decode(error.payload()).unwrap().code,
            ErrorCode::Statement
        );
        drop(reader);
        drop(writer);

        assert_eq!(
            owner.commit_transaction(cx, own.handle).await.unwrap(),
            Outcome::WriteCommitted {
                seq: 1,
                statements: 1
            }
        );
        let mut observer = connect(cx, addr, &grant(Rights::Read)).await;
        assert_eq!(ages(cx, &mut observer).await, [vec![WireValue::Int(1)]]);
        owner.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn explicit_savepoints_restore_the_private_graph_and_keep_completion_classification() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "explicit-savepoints").await;
        let mut writer = connect(cx, addr, &grant(Rights::ReadWrite)).await;
        let mut observer = connect(cx, addr, &grant(Rights::Read)).await;
        let begun = writer.begin_transaction(cx).await.unwrap();
        writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(
                    ExecuteMode::Write,
                    "CREATE (a:Person {age:1})-[:KNOWS]->(b:Person {age:2})",
                ),
            )
            .await
            .unwrap();
        assert_eq!(
            writer
                .create_savepoint(cx, begun.handle, "雪 kept")
                .await
                .unwrap()
                .statement,
            2
        );
        writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(
                    ExecuteMode::Write,
                    "MATCH (n:Person {age:1}) DETACH DELETE n",
                ),
            )
            .await
            .unwrap();
        writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(ExecuteMode::Write, "CREATE (:Person {age:3})"),
            )
            .await
            .unwrap();
        let rolled_back = writer
            .rollback_to_savepoint(cx, begun.handle, "雪 kept")
            .await
            .unwrap();
        assert_eq!(rolled_back.statement, 5);
        assert_eq!(rolled_back.rows, 0);
        let restored = writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(
                    ExecuteMode::Read,
                    "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.age AS a,b.age AS b",
                ),
            )
            .await
            .unwrap();
        assert_eq!(restored.rows, [vec![WireValue::Int(1), WireValue::Int(2)]]);
        writer
            .release_savepoint(cx, begun.handle, "雪 kept")
            .await
            .unwrap();
        writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(ExecuteMode::Write, "MATCH (n:Person {age:1}) SET n.age=9"),
            )
            .await
            .unwrap();
        assert!(ages(cx, &mut observer).await.is_empty());
        assert_eq!(
            writer.commit_transaction(cx, begun.handle).await.unwrap(),
            Outcome::WriteCommitted {
                seq: 1,
                statements: 8
            }
        );
        let expected = vec![vec![WireValue::Int(2)], vec![WireValue::Int(9)]];
        assert_eq!(ages(cx, &mut observer).await, expected);

        let begun = writer.begin_transaction(cx).await.unwrap();
        writer
            .create_savepoint(cx, begun.handle, "empty")
            .await
            .unwrap();
        writer
            .rollback_to_savepoint(cx, begun.handle, "empty")
            .await
            .unwrap();
        writer
            .release_savepoint(cx, begun.handle, "empty")
            .await
            .unwrap();
        assert_eq!(
            writer.commit_transaction(cx, begun.handle).await.unwrap(),
            Outcome::ReadClosed {
                seq: 1,
                statements: 3
            }
        );

        let begun = writer.begin_transaction(cx).await.unwrap();
        writer
            .create_savepoint(cx, begun.handle, "empty")
            .await
            .unwrap();
        writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(ExecuteMode::Write, "CREATE (:Person {age:100})"),
            )
            .await
            .unwrap();
        writer
            .rollback_to_savepoint(cx, begun.handle, "empty")
            .await
            .unwrap();
        assert_eq!(
            writer.commit_transaction(cx, begun.handle).await.unwrap(),
            Outcome::WriteCommitted {
                seq: 2,
                statements: 3
            }
        );
        assert_eq!(ages(cx, &mut observer).await, expected);
        writer.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn refused_savepoint_statements_abort_every_earlier_provisional_write() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "explicit-savepoint-refusals").await;
        let mut writer = connect(cx, addr, &grant(Rights::ReadWrite)).await;
        let mut observer = connect(cx, addr, &grant(Rights::Read)).await;
        for (mode, text) in [
            (ExecuteMode::Write, "ROLLBACK TO missing"),
            (ExecuteMode::Write, "SAVEPOINT kept extra"),
            (
                ExecuteMode::Write,
                "SAVEPOINT kept; CREATE (:Person {age:99})",
            ),
            (ExecuteMode::Read, "SAVEPOINT kept"),
        ] {
            let begun = writer.begin_transaction(cx).await.unwrap();
            writer
                .execute_transaction(
                    cx,
                    begun.handle,
                    &statement(ExecuteMode::Write, "CREATE (:Person {age:10})"),
                )
                .await
                .unwrap();
            assert!(
                writer
                    .execute_transaction(cx, begun.handle, &statement(mode, text))
                    .await
                    .is_err()
            );
            assert!(writer.commit_transaction(cx, begun.handle).await.is_err());
            assert!(ages(cx, &mut observer).await.is_empty());
        }
        // Rolling back an older prefix discards its younger named savepoint.
        let begun = writer.begin_transaction(cx).await.unwrap();
        writer
            .create_savepoint(cx, begun.handle, "older")
            .await
            .unwrap();
        writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(ExecuteMode::Write, "CREATE (:Person {age:11})"),
            )
            .await
            .unwrap();
        writer
            .create_savepoint(cx, begun.handle, "younger")
            .await
            .unwrap();
        writer
            .rollback_to_savepoint(cx, begun.handle, "older")
            .await
            .unwrap();
        assert!(
            writer
                .release_savepoint(cx, begun.handle, "younger")
                .await
                .is_err()
        );
        assert!(ages(cx, &mut observer).await.is_empty());
        writer.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn http_transactions_apply_savepoints_before_the_single_native_completion() {
    run(async |cx| {
        let (address, shutdown, mut server) = start_http(cx, "http-savepoints").await;
        let scope = grant(Rights::ReadWrite);
        let (status, body) = http_transaction(
            address,
            &scope,
            r#"{"statements":[
            {"mode":"write","statement":"CREATE (:Person {age:1})"},
            {"mode":"write","statement":"SAVEPOINT kept"},
            {"mode":"write","statement":"MATCH (n:Person) SET n.age=2"},
            {"mode":"write","statement":"ROLLBACK TO SAVEPOINT kept"},
            {"mode":"read","statement":"MATCH (n:Person) RETURN n.age AS age"}
        ]}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            r#"{"v":1,"basis":0,"results":[{"columns":[],"rows":[]},{"columns":[],"rows":[]},{"columns":[],"rows":[]},{"columns":[],"rows":[]},{"columns":["age"],"rows":[[{"type":"int","value":"1"}]]}],"seq":1,"statements":5,"committed":true}"#
        );
        let (status, body) = http_transaction(
            address,
            &scope,
            r#"{"statements":[
            {"mode":"write","statement":"CREATE (:Person {age:3})"},
            {"mode":"write","statement":"RELEASE absent"}
        ]}"#,
        )
        .await;
        assert!(status >= 400, "{body}");
        assert!(!body.contains("\"results\""), "{body}");
        assert_eq!(
            http_ages(address).await,
            r#"{"v":1,"columns":["age"],"rows":[[{"type":"int","value":"1"}]],"seq":1}"#
        );
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}
