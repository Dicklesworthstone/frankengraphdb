//! Computed edge and endpoint values through the bounded, authorized FGP source.
use super::super::source_stream::prepared_source;
use super::*;

fn integer(value: &WireValue) -> Option<i64> {
    match value {
        WireValue::Int(value) => Some(*value),
        WireValue::Null => None,
        other => panic!("integer or NULL expected: {other:?}"),
    }
}

#[test]
fn buffered_edge_projections_preserve_direction_scope_and_physical_candidate_budget() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start_buffered(cx, "computed-edge-directions").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed_endpoint_edges(cx, &mut owner).await;
        let mut scope = reader_grant();
        // Five admitted physical edges require nine endpoint visits. The
        // second orientation and repeated VM lookups must not spend them again.
        scope.limits.max_nodes = 9;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let mut oracle_scope = scope.clone();
        oracle_scope.limits.max_nodes = 100;
        let mut oracle = Client::connect(cx, addr, token(&oracle_scope))
            .await
            .unwrap();
        oracle.select(cx, "social").await.unwrap();

        for (pattern, incoming, undirected) in [
            ("(a)-[r:KNOWS]->(b)", false, false),
            ("(a)<-[r:KNOWS]-(b)", true, false),
            ("(a)-[r]-(b)", false, true),
        ] {
            let order = if undirected { "id,origin" } else { "id" };
            let statement = format!(
                "MATCH {pattern} RETURN r AS id,a AS origin,b AS target,\
                 r.age+$by AS cost,a.age+$by AS from_age,b.age+$by AS to_age,\
                 a.name+$by AS masked ORDER BY {order}"
            );
            let args = |by| vec![("by".into(), WireValue::Int(by))];
            let handle = reader.prepare_read(cx, &statement, args(4)).await.unwrap();
            for by in [4, -2] {
                let resident = oracle
                    .execute(cx, ExecuteMode::Read, &statement, args(by))
                    .await
                    .unwrap();
                let streamed = reader
                    .execute(cx, ExecuteMode::ReadStream, &statement, args(by))
                    .await
                    .unwrap();
                let prepared = prepared_source(cx, &mut reader, handle, args(by))
                    .await
                    .unwrap();
                assert_eq!(streamed, resident, "{pattern}");
                assert_eq!(prepared, resident, "{pattern}");
                assert_eq!(streamed.outcome, Outcome::Rows { seq: 2 });
                assert_eq!(streamed.rows.len(), if undirected { 9 } else { 5 });
                let mut actual = streamed
                    .rows
                    .iter()
                    .map(|row| {
                        assert_eq!(row[6], WireValue::Null);
                        (integer(&row[3]), integer(&row[4]), integer(&row[5]))
                    })
                    .collect::<Vec<_>>();
                let mut expected = Vec::new();
                for (weight, mut from, mut to) in [
                    (1, Some(4), Some(9)),
                    (2, Some(4), Some(9)),
                    (3, Some(9), Some(4)),
                    (4, Some(4), Some(4)),
                    (5, Some(4), None),
                ] {
                    if incoming {
                        core::mem::swap(&mut from, &mut to);
                    }
                    let from = from.map(|value| value + by);
                    let to = to.map(|value| value + by);
                    expected.push((Some(weight + by), from, to));
                    if undirected && weight != 4 {
                        expected.push((Some(weight + by), to, from));
                    }
                }
                actual.sort_unstable();
                expected.sort_unstable();
                assert_eq!(actual, expected, "{pattern}");
            }
            reader.release_prepared(cx, handle).await.unwrap();
        }
        oracle.close(cx).await.unwrap();
        reader.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn buffered_edge_projection_templates_rebind_history_and_private_identity_pages() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start_buffered(cx, "computed-edge-history").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed_endpoint_edges(cx, &mut owner).await;
        let mut scope = reader_grant();
        scope.limits.max_nodes = 9;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let original = reader
            .execute(
                cx,
                ExecuteMode::ReadStream,
                "MATCH (a)-[r:KNOWS]-(b) RETURN r AS id,a AS origin,b AS target,\
                 r.age AS age,a.age AS from_age,b.age AS to_age ORDER BY id,origin",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(original.rows.len(), 9);
        let statement = "AT BRANCH $route MATCH (a)-[r:KNOWS]-(b) FOR SYSTEM_TIME AS OF SEQ $cut \
            RETURN {edge:r,origin:a,target:b,shifted:r.age+$by,from_age:a.age,\
            to_age:b.age,masked:b.name+$by} AS doc ORDER BY r,a SKIP $skip LIMIT $take";
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
            .prepare_read(cx, statement, args(2, 1, 0, 9, "main"))
            .await
            .unwrap();
        for mutation in [
            "MATCH (n:Person) WHERE n.age IS NOT NULL SET n.age=n.age+100",
            "MATCH ()-[r:KNOWS]->() SET r.age=r.age+10",
        ] {
            owner
                .execute(cx, ExecuteMode::Write, mutation, vec![])
                .await
                .unwrap();
        }
        for (cut, by, skip, take) in [
            (2, 7, 0, 9),
            (4, -4, 0, 9),
            (2, 17, 1, 3),
            (4, 2, 3, 2),
            (2, 0, 2, 0),
        ] {
            let result =
                prepared_source(cx, &mut reader, handle, args(cut, by, skip, take, "main"))
                    .await
                    .unwrap();
            let endpoint = |value: &WireValue| {
                integer(value).map_or(WireValue::Null, |value| {
                    WireValue::Int(value + if cut == 4 { 100 } else { 0 })
                })
            };
            let expected = original
                .rows
                .iter()
                .skip(skip as usize)
                .take(take as usize)
                .map(|row| {
                    vec![WireValue::Map(vec![
                        ("edge".into(), row[0].clone()),
                        ("from_age".into(), endpoint(&row[4])),
                        ("masked".into(), WireValue::Null),
                        ("origin".into(), row[1].clone()),
                        (
                            "shifted".into(),
                            WireValue::Int(
                                integer(&row[3]).unwrap() + by + if cut == 4 { 10 } else { 0 },
                            ),
                        ),
                        ("target".into(), row[2].clone()),
                        ("to_age".into(), endpoint(&row[5])),
                    ])]
                })
                .collect::<Vec<_>>();
            assert_eq!(result.columns, ["doc"]);
            assert_eq!(result.rows, expected);
            assert_eq!(result.outcome, Outcome::Rows { seq: cut });
            assert_eq!(
                reader
                    .execute(
                        cx,
                        ExecuteMode::ReadStream,
                        statement,
                        args(cut, by, skip, take, "main")
                    )
                    .await
                    .unwrap(),
                result
            );
        }
        assert_eq!(
            server_code(
                prepared_source(cx, &mut reader, handle, args(2, 0, 0, 1, "elsewhere"))
                    .await
                    .unwrap_err()
            ),
            ErrorCode::PermissionDenied
        );
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
fn buffered_edge_projections_keep_credit_budgets_cancel_release_and_suffix_errors() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start_buffered(cx, "computed-edge-controls").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed_endpoint_edges(cx, &mut owner).await;
        let mut scope = reader_grant();
        // Only one two-row credit window can fit this allowance. Successful
        // first-row delivery proves the source did not materialize the suffix.
        scope.limits.max_nodes = 4;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let statement =
            "MATCH (a)-[r:KNOWS]->(b) RETURN r AS id,r.age+1 AS cost ORDER BY id LIMIT $take";
        let args = |take| vec![("take".into(), WireValue::Count(take))];
        let handle = reader.prepare_read(cx, statement, args(5)).await.unwrap();
        for nonce in [221, 222] {
            let mut count = 0;
            let error = reader
                .execute_prepared_streaming(
                    cx,
                    PreparedReadMode::SourceStream,
                    handle,
                    args(5),
                    |_| Ok(()),
                    |_| {
                        count += 1;
                        Err(ClientError::Protocol("stop one computed edge"))
                    },
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                ClientError::Protocol("stop one computed edge")
            ));
            assert_eq!(count, 1);
            reader.ping(cx, nonce).await.unwrap();
        }
        reader.release_prepared(cx, handle).await.unwrap();
        reader.close(cx).await.unwrap();

        scope.limits.max_nodes = 9;
        scope.limits.max_rows = 4;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let handle = reader.prepare_read(cx, statement, args(5)).await.unwrap();
        for take in [1, 0] {
            assert_eq!(
                prepared_source(cx, &mut reader, handle, args(take))
                    .await
                    .unwrap()
                    .rows
                    .len(),
                take as usize
            );
        }
        let mut prefix = Vec::new();
        let error = reader
            .execute_prepared_streaming(
                cx,
                PreparedReadMode::SourceStream,
                handle,
                args(5),
                |_| Ok(()),
                |row| {
                    prefix.push(row);
                    Ok(())
                },
            )
            .await
            .unwrap_err();
        assert_eq!(server_code(error), ErrorCode::Budget);
        assert_eq!(prefix.len(), 4);
        reader.ping(cx, 223).await.unwrap();
        reader.release_prepared(cx, handle).await.unwrap();

        // Pick the last physical identity without assuming the hash order.
        let original = owner
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN r AS id,r.age AS age ORDER BY id",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(original.rows.len(), 5);
        let bad = integer(&original.rows.last().unwrap()[1]).unwrap();
        let statement = "MATCH (a)-[r:KNOWS]->(b) RETURN r AS id,100/(r.age-$bad) AS ratio ORDER BY id LIMIT $take";
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
            let mut count = 0;
            let error = reader
                .execute_prepared_streaming(
                    cx,
                    PreparedReadMode::SourceStream,
                    handle,
                    args(bad, take),
                    |_| Ok(()),
                    |_| {
                        count += 1;
                        Ok(())
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(server_code(error), ErrorCode::Execution);
            assert_eq!(count, take as usize);
            reader.ping(cx, 224 + take).await.unwrap();
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
