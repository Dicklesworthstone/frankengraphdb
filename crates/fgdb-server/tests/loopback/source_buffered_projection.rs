//! Computed vertex projections over the configured bounded cold source.
use super::*;

fn projection_args(by: i64) -> Vec<(String, WireValue)> {
    vec![("by".into(), WireValue::Int(by))]
}

#[test]
fn buffered_computed_projections_keep_private_keys_scope_and_duplicate_occurrences() {
    run(async |cx| {
        let (addr, shutdown, mut task) =
            start_buffered_sources(cx, "buffered-computed-values").await;
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
        let identities = reader
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (n) RETURN n AS id,n.age AS age",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(identities.rows.len(), 12);
        assert_eq!(identities.outcome, Outcome::Rows { seq: 3 });
        let mut ages = identities
            .rows
            .iter()
            .map(|row| match row[1] {
                WireValue::Int(age) => age,
                ref other => panic!("integer age expected: {other:?}"),
            })
            .collect::<Vec<_>>();
        ages.sort_unstable();
        assert_eq!(ages, (0..12).collect::<Vec<i64>>());

        for (shape, statement, columns) in [
            (
                0,
                "MATCH (n) RETURN n AS id,n.age+$by AS shifted,n.name+$by AS masked ORDER BY id",
                vec!["id", "shifted", "masked"],
            ),
            (
                1,
                "MATCH (n) RETURN $by AS shifted ORDER BY n",
                vec!["shifted"],
            ),
            (
                2,
                "MATCH (n) RETURN {node:n,shifted:n.age+$by,masked:n.name+$by} AS doc ORDER BY n",
                vec!["doc"],
            ),
        ] {
            let handle = reader
                .prepare_read(cx, statement, projection_args(1))
                .await
                .unwrap();
            for by in [4, -2] {
                let expected = identities
                    .rows
                    .iter()
                    .map(|row| {
                        let WireValue::Int(age) = row[1] else {
                            panic!("integer age");
                        };
                        match shape {
                            0 => vec![row[0].clone(), WireValue::Int(age + by), WireValue::Null],
                            1 => vec![WireValue::Int(by)],
                            2 => vec![WireValue::Map(vec![
                                ("masked".into(), WireValue::Null),
                                ("node".into(), row[0].clone()),
                                ("shifted".into(), WireValue::Int(age + by)),
                            ])],
                            _ => unreachable!(),
                        }
                    })
                    .collect::<Vec<_>>();
                let streamed = reader
                    .execute(cx, ExecuteMode::ReadStream, statement, projection_args(by))
                    .await
                    .unwrap();
                assert_eq!(streamed.columns, columns, "{statement}");
                assert_eq!(streamed.rows, expected, "{statement}");
                assert_eq!(streamed.outcome, Outcome::Rows { seq: 3 });
                assert_eq!(
                    prepared_source(cx, &mut reader, handle, projection_args(by))
                        .await
                        .unwrap(),
                    streamed,
                    "{statement}"
                );
                assert_eq!(
                    reader
                        .execute(cx, ExecuteMode::Read, statement, projection_args(by))
                        .await
                        .unwrap(),
                    streamed,
                    "{statement}"
                );
                // Denied names are masked before arithmetic: the stored text
                // would otherwise be a type error. Private ranking cells never
                // enter the output, and twelve equal scalar projections remain
                // twelve occurrences across three four-row credit windows.
                assert!(streamed.rows.iter().all(|row| row.len() == columns.len()));
            }
            reader.release_prepared(cx, handle).await.unwrap();
        }

        // Native source order is a physical contract, including an empty page.
        // Cold projections retain their proved source order and cannot sort,
        // collect a graph, or retry an unsupported projection through resident
        // evaluation. Eager reads still accept these ordinary queries.
        for statement in [
            "MATCH (n:Person) RETURN n AS id,n.age+$by AS shifted ORDER BY id DESC",
            "MATCH (n:Person) RETURN n AS id,n.age+$by AS shifted ORDER BY id DESC LIMIT 0",
            "MATCH (n:Person) RETURN n.age+$by AS shifted",
            "MATCH (n:Person) RETURN n.age+$by AS shifted ORDER BY n",
            "MATCH (a)-[r]->(b) RETURN r.age+$by AS shifted ORDER BY r",
        ] {
            let handle = reader
                .prepare_read(cx, statement, projection_args(1))
                .await
                .unwrap();
            assert_eq!(
                server_code(
                    reader
                        .execute(cx, ExecuteMode::ReadStream, statement, projection_args(1))
                        .await
                        .unwrap_err()
                ),
                ErrorCode::Statement,
                "{statement}"
            );
            assert_eq!(
                server_code(
                    prepared_source(cx, &mut reader, handle, projection_args(1))
                        .await
                        .unwrap_err()
                ),
                ErrorCode::Statement,
                "{statement}"
            );
            assert!(
                reader
                    .execute(cx, ExecuteMode::Read, statement, projection_args(1))
                    .await
                    .is_ok(),
                "{statement}"
            );
            reader.release_prepared(cx, handle).await.unwrap();
        }
        reader.ping(cx, 180).await.unwrap();
        reader.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn buffered_computed_prepared_reads_rebind_exact_history_and_private_key_pages() {
    run(async |cx| {
        let (addr, shutdown, mut task) =
            start_buffered_sources(cx, "buffered-computed-history").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed(cx, &mut owner).await;
        let original = owner
            .execute(cx, ExecuteMode::Read, ROWS, vec![])
            .await
            .unwrap();
        assert_eq!(original.outcome, Outcome::Rows { seq: 2 });
        let mut scope = grant(Rights::Read);
        scope.labels = Scope::only([LabelId(1)]);
        scope.properties = Scope::only([PropertyKeyId(2)]);
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let statement = "AT BRANCH $route MATCH (n) FOR SYSTEM_TIME AS OF SEQ $cut \
                         RETURN {node:n,shifted:n.age+$by,masked:n.name+$by} AS doc \
                         ORDER BY n SKIP $skip LIMIT $take";
        let args = |cut, by, skip, take, route: &str| {
            vec![
                ("route".into(), text(route)),
                ("cut".into(), WireValue::Count(cut)),
                ("by".into(), WireValue::Int(by)),
                ("skip".into(), WireValue::Count(skip)),
                ("take".into(), WireValue::Count(take)),
            ]
        };
        let handle = reader
            .prepare_read(cx, statement, args(2, 1, 0, 12, "main"))
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
            (2, 7, 0, 12),
            (3, -4, 0, 12),
            (2, 17, 1, 3),
            (3, 2, 2, 0),
            (2, 0, 0, 12),
        ] {
            let result =
                prepared_source(cx, &mut reader, handle, args(cut, by, skip, take, "main"))
                    .await
                    .unwrap();
            let expected = original
                .rows
                .iter()
                .skip(skip as usize)
                .take(take as usize)
                .map(|row| {
                    let WireValue::Int(age) = row[2] else {
                        panic!("integer age");
                    };
                    vec![WireValue::Map(vec![
                        ("masked".into(), WireValue::Null),
                        ("node".into(), row[0].clone()),
                        (
                            "shifted".into(),
                            WireValue::Int(age + by + if cut == 3 { 100 } else { 0 }),
                        ),
                    ])]
                })
                .collect::<Vec<_>>();
            assert_eq!(result.columns, ["doc"]);
            assert_eq!(result.rows, expected);
            assert_eq!(result.outcome, Outcome::Rows { seq: cut });
            for mode in [ExecuteMode::Read, ExecuteMode::ReadStream] {
                assert_eq!(
                    reader
                        .execute(cx, mode, statement, args(cut, by, skip, take, "main"))
                        .await
                        .unwrap(),
                    result
                );
            }
        }
        for (parameters, code) in [
            (args(2, 0, 0, 1, "elsewhere"), ErrorCode::PermissionDenied),
            (vec![], ErrorCode::Statement),
        ] {
            assert_eq!(
                server_code(
                    prepared_source(cx, &mut reader, handle, parameters)
                        .await
                        .unwrap_err()
                ),
                code
            );
        }
        assert_eq!(
            prepared_source(cx, &mut reader, handle, args(2, 0, 0, 1, "main"))
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
fn buffered_computed_cursors_keep_cumulative_budgets_cancel_release_and_suffix_failures() {
    run(async |cx| {
        let (addr, shutdown, mut task) =
            start_buffered_sources(cx, "buffered-computed-controls").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed(cx, &mut owner).await;
        let original = owner
            .execute(cx, ExecuteMode::Read, ROWS, vec![])
            .await
            .unwrap();
        let expected = original
            .rows
            .iter()
            .map(|row| {
                let WireValue::Int(age) = row[2] else {
                    panic!("integer age");
                };
                vec![row[0].clone(), WireValue::Int(age + 5), WireValue::Null]
            })
            .collect::<Vec<_>>();
        let bad = match original.rows.last().unwrap()[2] {
            WireValue::Int(age) => age,
            ref other => panic!("integer age expected: {other:?}"),
        };
        let statement = "MATCH (n:Person) RETURN n AS id,n.age+$by AS shifted,n.name+$by AS masked \
                         ORDER BY id LIMIT $take";
        let args = |take| {
            vec![
                ("by".into(), WireValue::Int(5)),
                ("take".into(), WireValue::Count(take)),
            ]
        };
        let mut scope = grant(Rights::Read);
        scope.labels = Scope::only([LabelId(1)]);
        scope.properties = Scope::only([PropertyKeyId(2)]);
        scope.limits.max_nodes = 4;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let handle = reader.prepare_read(cx, statement, args(12)).await.unwrap();
        // Only one credit window fits this node allowance. Full-result
        // materialization cannot reach the first callback successfully.
        // The same handle is cancelled twice to prove its prior cold source
        // and the sole configured buffered admission slot have been released.
        for nonce in [181, 182] {
            let mut first = Vec::new();
            let error = reader
                .execute_prepared_streaming(
                    cx,
                    PreparedReadMode::SourceStream,
                    handle,
                    args(12),
                    |columns| {
                        assert_eq!(columns, ["id", "shifted", "masked"]);
                        Ok(())
                    },
                    |row| {
                        first.push(row);
                        Err(ClientError::Protocol("stop one cold computed row"))
                    },
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                ClientError::Protocol("stop one cold computed row")
            ));
            assert_eq!(first, expected[..1]);
            reader.ping(cx, nonce).await.unwrap();
            assert_eq!(
                reader
                    .execute(
                        cx,
                        ExecuteMode::ReadStream,
                        "MATCH (n:Person) RETURN n AS id LIMIT 1",
                        vec![],
                    )
                    .await
                    .unwrap()
                    .rows
                    .len(),
                1
            );
        }
        reader.release_prepared(cx, handle).await.unwrap();
        reader.close(cx).await.unwrap();

        scope.limits.max_nodes = 1_000;
        scope.limits.max_rows = 4;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let handle = reader.prepare_read(cx, statement, args(12)).await.unwrap();
        // Input occurrences and private keys do not consume final-row credit.
        // Both pages evaluate twelve rows under one four-result-row allowance.
        for take in [1, 0] {
            assert_eq!(
                prepared_source(cx, &mut reader, handle, args(take))
                    .await
                    .unwrap()
                    .rows,
                expected[..take as usize]
            );
        }
        let mut prefix = Vec::new();
        let error = reader
            .execute_prepared_streaming(
                cx,
                PreparedReadMode::SourceStream,
                handle,
                args(12),
                |_| Ok(()),
                |row| {
                    prefix.push(row);
                    Ok(())
                },
            )
            .await
            .unwrap_err();
        assert_eq!(server_code(error), ErrorCode::Budget);
        assert_eq!(prefix, expected[..4]);
        reader.ping(cx, 183).await.unwrap();
        assert_eq!(
            prepared_source(cx, &mut reader, handle, args(1))
                .await
                .unwrap()
                .rows,
            expected[..1]
        );
        reader.release_prepared(cx, handle).await.unwrap();

        // The unique final identity fails after the selected page. Neither
        // four emitted rows nor LIMIT 0 may turn its arithmetic error into EOF.
        let statement = "MATCH (n:Person) RETURN n AS id,100/(n.age-$bad) AS quotient \
                         ORDER BY id LIMIT $take";
        let args = |bad, take| {
            vec![
                ("bad".into(), WireValue::Int(bad)),
                ("take".into(), WireValue::Count(take)),
            ]
        };
        let handle = reader
            .prepare_read(cx, statement, args(bad, 4))
            .await
            .unwrap();
        for take in [4, 0] {
            let mut prefix = Vec::new();
            let error = reader
                .execute_prepared_streaming(
                    cx,
                    PreparedReadMode::SourceStream,
                    handle,
                    args(bad, take),
                    |_| Ok(()),
                    |row| {
                        prefix.push(row);
                        Ok(())
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(server_code(error), ErrorCode::Execution);
            assert_eq!(prefix.len(), take as usize);
            reader.ping(cx, 184 + take).await.unwrap();
        }
        assert_eq!(
            prepared_source(cx, &mut reader, handle, args(-1, 1))
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
