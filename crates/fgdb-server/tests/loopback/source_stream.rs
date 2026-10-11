//! Native source streaming over actual flow-controlled loopback connections.
use super::*;

async fn seed(cx: &Cx, owner: &mut Client) {
    owner
        .execute_batch(
            cx,
            "CREATE (:Person {name:$name,age:$age})",
            (0..12)
                .map(|age| {
                    vec![
                        ("name".into(), text(&format!("person-{age}"))),
                        ("age".into(), WireValue::Int(age)),
                    ]
                })
                .collect(),
        )
        .await
        .unwrap();
}

const ROWS: &str = "MATCH (n:Person) RETURN n AS id, n.name AS name, n.age AS age";

#[test]
fn native_source_stream_preserves_scoped_rows_across_exact_credit_windows() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "native-source-stream").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed(cx, &mut owner).await;
        let mut scope = grant(Rights::Read);
        scope.labels = Scope::only([LabelId(1)]);
        scope.properties = Scope::only([PropertyKeyId(2)]);
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let eager = reader
            .execute(cx, ExecuteMode::Read, ROWS, vec![])
            .await
            .unwrap();
        let streamed = reader
            .execute(cx, ExecuteMode::ReadStream, ROWS, vec![])
            .await
            .unwrap();
        assert_eq!(streamed, eager);
        assert_eq!(streamed.rows.len(), 12); // three EXACT four-row windows, then EOF demand.
        assert!(streamed.rows.iter().all(|row| row[1] == WireValue::Null));
        assert_eq!(streamed.outcome, Outcome::Rows { seq: 1 });
        for text in [
            "MATCH (n:Person) RETURN n.age AS age",
            "MATCH (n:Person) RETURN count(*) AS count",
            "MATCH (n:Person) RETURN n AS id ORDER BY n.age DESC",
        ] {
            let error = reader
                .execute(cx, ExecuteMode::ReadStream, text, vec![])
                .await
                .unwrap_err();
            assert_eq!(server_code(error), ErrorCode::Statement);
            assert!(
                reader
                    .execute(cx, ExecuteMode::Read, text, vec![])
                    .await
                    .is_ok(),
                "explicit cursor-profile refusal changed ordinary native execution"
            );
        }
        reader.close(cx).await.unwrap();

        scope.limits.max_nodes = 0;
        let mut empty = Client::connect(cx, addr, token(&scope)).await.unwrap();
        empty.select(cx, "social").await.unwrap();
        let result = empty
            .execute(
                cx,
                ExecuteMode::ReadStream,
                "MATCH (n:Person) RETURN n AS id LIMIT 0",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(result.columns, ["id"]);
        assert!(result.rows.is_empty());
        assert_eq!(result.outcome, Outcome::Rows { seq: 1 });
        empty.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn native_source_stream_cancellation_and_late_budget_keep_the_connection_usable() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "native-source-stream-stops").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed(cx, &mut owner).await;

        // The whole graph cannot fit this native node allowance. A successful
        // first window therefore proves the server did not execute eagerly.
        let mut scope = grant(Rights::Read);
        scope.limits.max_nodes = 4;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let mut callbacks = 0;
        let stopped = reader
            .execute_streaming(
                cx,
                ExecuteMode::ReadStream,
                ROWS,
                vec![],
                |_| {},
                |_| {
                    callbacks += 1;
                    Err(ClientError::Protocol(
                        "application stopped after its first row",
                    ))
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            stopped,
            ClientError::Protocol("application stopped after its first row")
        ));
        assert_eq!(callbacks, 1);
        reader.ping(cx, 71).await.unwrap();
        let one = reader
            .execute(
                cx,
                ExecuteMode::ReadStream,
                "MATCH (n:Person) RETURN n AS id LIMIT 1",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(one.rows.len(), 1);
        reader.close(cx).await.unwrap();

        scope.limits.max_nodes = 1_000;
        scope.limits.max_rows = 4;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let mut prefix = Vec::new();
        let error = reader
            .execute_streaming(
                cx,
                ExecuteMode::ReadStream,
                ROWS,
                vec![],
                |_| {},
                |row| {
                    prefix.push(row);
                    Ok(())
                },
            )
            .await
            .unwrap_err();
        assert_eq!(server_code(error), ErrorCode::Budget);
        assert_eq!(
            prefix.len(),
            4,
            "retained signed row allowance spans credit windows"
        );
        reader.ping(cx, 72).await.unwrap();
        let one = reader
            .execute(
                cx,
                ExecuteMode::ReadStream,
                "MATCH (n:Person) RETURN n AS id LIMIT 1",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(one.rows.len(), 1);
        reader.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}
