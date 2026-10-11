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
fn native_aggregate_stream_preserves_exact_values_masking_history_and_group_windows() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "native-aggregate-stream").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed(cx, &mut owner).await;
        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (:Company {age:999,name:'hidden'})",
                vec![],
            )
            .await
            .unwrap();
        let mut scope = grant(Rights::Read);
        scope.labels = Scope::only([LabelId(1)]);
        scope.relations = Scope::only([RelationId(1)]);
        scope.properties = Scope::only([PropertyKeyId(2)]);
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();

        let summary = "MATCH (n) RETURN count(*) AS n, sum(n.age) AS total, avg(n.age) AS average, count(n.name) AS hidden, collect(n.age) AS ages";
        let eager = reader
            .execute(cx, ExecuteMode::Read, summary, vec![])
            .await
            .unwrap();
        let streamed = reader
            .execute(cx, ExecuteMode::ReadStream, summary, vec![])
            .await
            .unwrap();
        assert_eq!(streamed, eager);
        assert_eq!(streamed.rows.len(), 1);
        assert_eq!(
            streamed.rows[0][..4],
            [
                WireValue::Count(12),
                WireValue::WideInt(66),
                WireValue::Average {
                    numerator: 11,
                    denominator: 2,
                },
                WireValue::Count(0),
            ]
        );
        assert_eq!(streamed.outcome, Outcome::Rows { seq: 2 });

        let expressions = reader
            .execute(
                cx,
                ExecuteMode::ReadStream,
                "MATCH (n) RETURN [age IN collect(n.age) WHERE age > 8 | age+1] AS ages, \
                 reduce(total=0, age IN collect(n.age) | total+age) AS total",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(
            expressions.rows,
            [vec![
                WireValue::List(vec![
                    WireValue::Int(10),
                    WireValue::Int(11),
                    WireValue::Int(12)
                ]),
                WireValue::Int(66),
            ]]
        );

        let groups = "MATCH (n:Person) RETURN n.age AS age, count(*) AS n, n.age AS repeated GROUP BY n.age ORDER BY age DESC";
        let streamed = reader
            .execute(cx, ExecuteMode::ReadStream, groups, vec![])
            .await
            .unwrap();
        assert_eq!(streamed.columns, ["age", "n", "repeated"]);
        assert_eq!(
            streamed.rows,
            (0..12)
                .rev()
                .map(|age| vec![
                    WireValue::Int(age),
                    WireValue::Count(1),
                    WireValue::Int(age),
                ])
                .collect::<Vec<_>>()
        ); // Three exact four-group credit windows.

        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (n:Person) SET n.age = n.age + 100",
                vec![],
            )
            .await
            .unwrap();
        let historical = reader
            .execute(
                cx,
                ExecuteMode::ReadStream,
                "MATCH (n:Person) FOR SYSTEM_TIME AS OF SEQ 1 RETURN sum(n.age) AS total, avg(n.age) AS average",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(
            historical.rows,
            [vec![
                WireValue::WideInt(66),
                WireValue::Average {
                    numerator: 11,
                    denominator: 2,
                },
            ]]
        );
        assert_eq!(historical.outcome, Outcome::Rows { seq: 1 });

        // Empty global aggregates retain their native empty-group values;
        // grouping an empty input produces no rows, never an invented group.
        for (statement, rows) in [
            (
                "MATCH (n:Person) WHERE n.age < 0 RETURN count(*) AS n, collect(n.age) AS ages",
                vec![vec![WireValue::Count(0), WireValue::List(vec![])]],
            ),
            (
                "MATCH (n:Person) WHERE n.age < 0 RETURN n.age AS age, count(*) AS n GROUP BY n.age",
                vec![],
            ),
        ] {
            assert_eq!(
                reader
                    .execute(cx, ExecuteMode::ReadStream, statement, vec![])
                    .await
                    .unwrap()
                    .rows,
                rows
            );
        }

        // Preserve sums wider than a graph integer and exact averages all the
        // way through the native cursor's RETURN slots and FGP value codec.
        owner
            .execute_batch(
                cx,
                "CREATE (:Person {age:$age})",
                vec![vec![("age".into(), WireValue::Int(i64::MAX))]; 2],
            )
            .await
            .unwrap();
        let wide = reader
            .execute(
                cx,
                ExecuteMode::ReadStream,
                "MATCH (n:Person) WHERE n.age=$age RETURN sum(n.age) AS total, avg(n.age) AS average",
                vec![("age".into(), WireValue::Int(i64::MAX))],
            )
            .await
            .unwrap();
        assert_eq!(
            wide.rows,
            [vec![
                WireValue::WideInt(i128::from(i64::MAX) * 2),
                WireValue::Average {
                    numerator: i128::from(i64::MAX),
                    denominator: 1,
                },
            ]]
        );

        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (a:Person {age:7}), (b:Person {age:9}), \
                 (c:Company {age:99}), \
                 (a)-[:KNOWS {age:3,name:'visible'}]->(b), \
                 (a)-[:WORKS_AT {age:999}]->(b), \
                 (a)-[:KNOWS {age:999}]->(c)",
                vec![],
            )
            .await
            .unwrap();
        for pattern in ["(a)-[r:KNOWS]->(b)", "(a)-[r]->(b)"] {
            let statement = format!(
                "MATCH {pattern} RETURN count(*) AS n, sum(r.age) AS total, \
                 avg(r.age) AS average, count(r.name) AS hidden, collect(r.age) AS ages"
            );
            let eager = reader
                .execute(cx, ExecuteMode::Read, &statement, vec![])
                .await
                .unwrap();
            let streamed = reader
                .execute(cx, ExecuteMode::ReadStream, &statement, vec![])
                .await
                .unwrap();
            assert_eq!(streamed, eager);
            assert_eq!(
                streamed.rows,
                [vec![
                    WireValue::Count(1),
                    WireValue::WideInt(3),
                    WireValue::Average {
                        numerator: 3,
                        denominator: 1,
                    },
                    WireValue::Count(0),
                    WireValue::List(vec![WireValue::Int(3)]),
                ]]
            ); // Hidden relation types and endpoint labels never enter a group.
        }
        reader.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn native_aggregate_stream_budgets_and_cancellation_count_final_groups() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "native-aggregate-stream-limits").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed(cx, &mut owner).await;
        let summary = "MATCH (n:Person) RETURN count(*) AS n";
        let groups =
            "MATCH (n:Person) RETURN n.age AS age, count(*) AS n GROUP BY n.age ORDER BY age";

        let mut scope = grant(Rights::Read);
        scope.limits.max_rows = 4;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        assert_eq!(
            reader
                .execute(cx, ExecuteMode::ReadStream, summary, vec![])
                .await
                .unwrap()
                .rows,
            [vec![WireValue::Count(12)]]
        ); // Input rows are not charged as delivered groups.
        let mut prefix = Vec::new();
        let error = reader
            .execute_streaming(
                cx,
                ExecuteMode::ReadStream,
                groups,
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
        assert_eq!(prefix.len(), 4);
        reader.ping(cx, 81).await.unwrap();
        let mut callbacks = 0;
        let error = reader
            .execute_streaming(
                cx,
                ExecuteMode::ReadStream,
                groups,
                vec![],
                |_| {},
                |_| {
                    callbacks += 1;
                    Err(ClientError::Protocol("stop after one aggregate group"))
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ClientError::Protocol("stop after one aggregate group")
        ));
        assert_eq!(callbacks, 1);
        reader.ping(cx, 82).await.unwrap();
        reader.close(cx).await.unwrap();

        // Aggregate errors require complete input validation, even when output
        // paging requests no groups. No partial group may precede the failure.
        scope.limits.max_nodes = 11;
        let mut limited = Client::connect(cx, addr, token(&scope)).await.unwrap();
        limited.select(cx, "social").await.unwrap();
        for statement in [summary.to_owned(), format!("{summary} LIMIT 0")] {
            let mut rows = 0;
            let error = limited
                .execute_streaming(
                    cx,
                    ExecuteMode::ReadStream,
                    &statement,
                    vec![],
                    |_| {},
                    |_| {
                        rows += 1;
                        Ok(())
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(server_code(error), ErrorCode::Budget);
            assert_eq!(rows, 0);
            limited.ping(cx, 83).await.unwrap();
        }
        limited.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

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
