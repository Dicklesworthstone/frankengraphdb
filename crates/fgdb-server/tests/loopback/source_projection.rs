//! Computed source projections retain their proved identity order over FGP.
use super::*;

fn shifted(by: i64) -> Vec<(String, WireValue)> {
    vec![("by".into(), WireValue::Int(by))]
}

#[test]
fn computed_identity_orders_preserve_scoped_occurrences_and_prepared_arguments() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "computed-source-orders").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed(cx, &mut owner).await;
        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (a:Person {age:7,name:'left'}), (b:Person {age:7,name:'middle'}), \
                 (c:Person {age:7,name:'right'}), (h:Company {age:999}), \
                 (a)-[:KNOWS {age:3,name:'masked'}]->(b), \
                 (a)-[:KNOWS {age:3}]->(b), (b)-[:KNOWS {age:3}]->(c), \
                 (a)-[:WORKS_AT {age:999}]->(b), (a)-[:KNOWS {age:999}]->(h)",
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
        for (statement, columns, count, shifted_column) in [
            (
                "MATCH (n) RETURN n AS id,n.age+$by AS shifted,n.name AS hidden ORDER BY id",
                vec!["id", "shifted", "hidden"],
                15,
                1,
            ),
            (
                "MATCH (a)-[r]->(b) RETURN r AS id,r.age+$by AS shifted,b.name AS hidden ORDER BY id",
                vec!["id", "shifted", "hidden"],
                3,
                1,
            ),
            (
                "MATCH (a)-[r]-(b) RETURN r AS id,a AS origin,r.age+$by AS shifted,b.name AS hidden ORDER BY id,origin",
                vec!["id", "origin", "shifted", "hidden"],
                6,
                2,
            ),
            (
                "MATCH (a)-[r]->(b)-[s]->(c) RETURN r AS id,s AS next_id,r.age+$by AS shifted,c.name AS hidden ORDER BY id,next_id",
                vec!["id", "next_id", "shifted", "hidden"],
                2,
                2,
            ),
        ] {
            let handle = reader.prepare_read(cx, statement, shifted(1)).await.unwrap();
            for by in [4, -2] {
                let eager = reader
                    .execute(cx, ExecuteMode::Read, statement, shifted(by))
                    .await
                    .unwrap();
                let streamed = reader
                    .execute(cx, ExecuteMode::ReadStream, statement, shifted(by))
                    .await
                    .unwrap();
                let prepared = prepared_source(cx, &mut reader, handle, shifted(by))
                    .await
                    .unwrap();
                assert_eq!(streamed, eager, "{statement}");
                assert_eq!(prepared, eager, "{statement}");
                assert_eq!(streamed.columns, columns);
                assert_eq!(streamed.rows.len(), count);
                assert_eq!(streamed.outcome, Outcome::Rows { seq: 2 });
                assert!(streamed.rows.iter().all(|row| {
                    row.len() == columns.len() && row.last() == Some(&WireValue::Null)
                }));
                // These expectations are independent of the eager engine:
                // repeated weights preserve edge and orientation multiplicity;
                // denied types and endpoints contribute no occurrence.
                let mut values = streamed
                    .rows
                    .iter()
                    .map(|row| match row[shifted_column] {
                        WireValue::Int(value) => value,
                        ref other => panic!("integer projection expected: {other:?}"),
                    })
                    .collect::<Vec<_>>();
                values.sort_unstable();
                let mut expected = if count == 15 {
                    (0..12).chain([7, 7, 7]).map(|age| age + by).collect()
                } else {
                    vec![3 + by; count]
                };
                expected.sort_unstable();
                assert_eq!(values, expected, "{statement}");
            }
            reader.release_prepared(cx, handle).await.unwrap();
        }
        // A computation cannot erase a sort or a required orientation key.
        for statement in [
            "MATCH (n:Person) RETURN n AS id,n.age+$by AS shifted ORDER BY id DESC",
            "MATCH (n:Person) RETURN n.age+$by AS shifted",
            "MATCH (a)-[r]-(b) RETURN r AS id,r.age+$by AS shifted ORDER BY id",
        ] {
            assert_eq!(
                server_code(
                    reader
                        .execute(cx, ExecuteMode::ReadStream, statement, shifted(1))
                        .await
                        .unwrap_err()
                ),
                ErrorCode::Statement,
                "{statement}"
            );
            assert!(
                reader
                    .execute(cx, ExecuteMode::Read, statement, shifted(1))
                    .await
                    .is_ok(),
                "{statement}"
            );
        }
        reader.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn prepared_computed_identity_orders_rebind_history_and_final_pages() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "computed-source-history").await;
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
        scope.properties = Scope::only([PropertyKeyId(2)]);
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let statement = "AT BRANCH $route MATCH (n) FOR SYSTEM_TIME AS OF SEQ $cut \
                         RETURN n AS id,n.age+$by AS shifted,n.name AS hidden \
                         ORDER BY id SKIP $skip LIMIT $take";
        let arguments = |cut, by, skip, take, route: &str| {
            vec![
                ("route".into(), text(route)),
                ("cut".into(), WireValue::Count(cut)),
                ("by".into(), WireValue::Int(by)),
                ("skip".into(), WireValue::Count(skip)),
                ("take".into(), WireValue::Count(take)),
            ]
        };
        let handle = reader
            .prepare_read(cx, statement, arguments(1, 1, 0, 12, "main"))
            .await
            .unwrap();
        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (n:Person) SET n.age=n.age+100",
                vec![],
            )
            .await
            .unwrap();
        for (cut, by, skip, take) in [
            (1, 7, 0, 12),
            (3, -4, 0, 12),
            (1, 17, 1, 3),
            (1, 0, 0, 0),
        ] {
            let result = prepared_source(
                cx,
                &mut reader,
                handle,
                arguments(cut, by, skip, take, "main"),
            )
            .await
            .unwrap();
            let eager = reader
                .execute(
                    cx,
                    ExecuteMode::Read,
                    statement,
                    arguments(cut, by, skip, take, "main"),
                )
                .await
                .unwrap();
            assert_eq!(result, eager);
            assert_eq!(result.columns, ["id", "shifted", "hidden"]);
            assert_eq!(result.rows.len(), take as usize);
            assert_eq!(result.outcome, Outcome::Rows { seq: cut });
            assert!(result.rows.iter().all(|row| row[2] == WireValue::Null));
            if take == 12 {
                let mut values = result
                    .rows
                    .iter()
                    .map(|row| match row[1] {
                        WireValue::Int(value) => value,
                        ref other => panic!("integer projection expected: {other:?}"),
                    })
                    .collect::<Vec<_>>();
                values.sort_unstable();
                let base = if cut == 3 { 100 } else { 0 };
                assert_eq!(
                    values,
                    (0..12).map(|age| base + age + by).collect::<Vec<_>>()
                );
            }
        }
        for (args, code) in [
            (
                arguments(1, 0, 0, 1, "elsewhere"),
                ErrorCode::PermissionDenied,
            ),
            (vec![], ErrorCode::Statement),
        ] {
            assert_eq!(
                server_code(
                    prepared_source(cx, &mut reader, handle, args)
                        .await
                        .unwrap_err()
                ),
                code
            );
        }
        assert_eq!(
            prepared_source(cx, &mut reader, handle, arguments(1, 0, 0, 1, "main"))
                .await
                .unwrap()
                .rows
                .len(),
            1
        );
        reader.release_prepared(cx, handle).await.unwrap();
        reader.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn computed_identity_streams_keep_credit_cancellation_budgets_and_suffix_failures() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "computed-source-controls").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed(cx, &mut owner).await;
        let complete = owner
            .execute(cx, ExecuteMode::Read, ROWS, vec![])
            .await
            .unwrap();
        let bad = match complete.rows.last().unwrap()[2] {
            WireValue::Int(age) => age,
            ref other => panic!("integer age expected: {other:?}"),
        };
        let mut scope = grant(Rights::Read);
        scope.limits.max_rows = 4;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let statement =
            "MATCH (n:Person) RETURN n AS id,n.age+$by AS shifted ORDER BY id LIMIT $take";
        let arguments = |take| {
            vec![
                ("by".into(), WireValue::Int(5)),
                ("take".into(), WireValue::Count(take)),
            ]
        };
        let handle = reader
            .prepare_read(cx, statement, arguments(12))
            .await
            .unwrap();
        let mut callbacks = 0;
        let stopped = reader
            .execute_prepared_streaming(
                cx,
                PreparedReadMode::SourceStream,
                handle,
                arguments(12),
                |columns| {
                    assert_eq!(columns, ["id", "shifted"]);
                    Ok(())
                },
                |_| {
                    callbacks += 1;
                    Err(ClientError::Protocol("stop computed source after one row"))
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            stopped,
            ClientError::Protocol("stop computed source after one row")
        ));
        assert_eq!(callbacks, 1);
        reader.ping(cx, 81).await.unwrap();
        assert_eq!(
            prepared_source(cx, &mut reader, handle, arguments(1))
                .await
                .unwrap()
                .rows
                .len(),
            1
        );

        let mut prefix = Vec::new();
        let limited = reader
            .execute_prepared_streaming(
                cx,
                PreparedReadMode::SourceStream,
                handle,
                arguments(12),
                |_| Ok(()),
                |row| {
                    prefix.push(row);
                    Ok(())
                },
            )
            .await
            .unwrap_err();
        assert_eq!(server_code(limited), ErrorCode::Budget);
        assert_eq!(prefix.len(), 4, "one row allowance spans every credit window");
        reader.release_prepared(cx, handle).await.unwrap();

        // The last identity has a unique age. Its division failure lies after
        // the selected four rows and cannot disappear behind LIMIT, even zero.
        let statement = "MATCH (n:Person) RETURN n AS id,100/(n.age-$bad) AS quotient ORDER BY id LIMIT $take";
        let arguments = |bad, take| {
            vec![
                ("bad".into(), WireValue::Int(bad)),
                ("take".into(), WireValue::Count(take)),
            ]
        };
        let handle = reader
            .prepare_read(cx, statement, arguments(bad, 4))
            .await
            .unwrap();
        for take in [4, 0] {
            let mut prefix = Vec::new();
            let failure = reader
                .execute_prepared_streaming(
                    cx,
                    PreparedReadMode::SourceStream,
                    handle,
                    arguments(bad, take),
                    |_| Ok(()),
                    |row| {
                        prefix.push(row);
                        Ok(())
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(server_code(failure), ErrorCode::Execution);
            assert_eq!(prefix.len(), take as usize);
            reader.ping(cx, 82 + take).await.unwrap();
        }
        assert_eq!(
            prepared_source(cx, &mut reader, handle, arguments(-1, 1))
                .await
                .unwrap()
                .rows
                .len(),
            1
        );
        reader.release_prepared(cx, handle).await.unwrap();
        reader.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}
