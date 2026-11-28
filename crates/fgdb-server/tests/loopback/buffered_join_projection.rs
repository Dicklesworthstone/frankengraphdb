//! Computed fixed-hop occurrences through the authorized bounded join source.
use super::super::source_stream::prepared_source;
use super::*;

fn integer(value: &WireValue) -> Option<i64> {
    match value {
        WireValue::Int(value) => Some(*value),
        WireValue::Null => None,
        other => panic!("integer or NULL expected: {other:?}"),
    }
}

// An independent finite graph oracle: weight identifies each physical edge,
// including the two parallel a->b edges. It does not use native query output.
fn oriented_edges(direction: u8) -> Vec<(i64, usize, usize)> {
    let mut result = Vec::new();
    for (weight, from, to) in [(1, 0, 1), (2, 0, 1), (3, 1, 0), (4, 0, 0), (5, 0, 2)] {
        if direction == 1 {
            result.push((weight, to, from));
        } else {
            result.push((weight, from, to));
            if direction == 2 && from != to {
                result.push((weight, to, from));
            }
        }
    }
    result
}

#[test]
fn computed_cold_joins_preserve_scoped_occurrences_across_directions_branches_and_cycles() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start_buffered(cx, "computed-join-shapes").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed_endpoint_edges(cx, &mut owner).await;
        let scope = reader_grant();
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let mut oracle = Client::connect(cx, addr, token(&scope)).await.unwrap();
        oracle.select(cx, "social").await.unwrap();

        for (pattern, direction, shape, repeatable, count) in [
            ("(a)-[r:KNOWS]->(b)-[s:KNOWS]->(c)", 0, 0, false, 9),
            ("(a)<-[r:KNOWS]-(b)<-[s:KNOWS]-(c)", 1, 0, false, 9),
            ("(a)-[r:KNOWS]-(b)-[s:KNOWS]-(c)", 2, 0, false, 26),
            ("(a)-[r:KNOWS]->(b),(a)-[s:KNOWS]->(c)", 0, 1, false, 12),
            ("(a)-[r:KNOWS]->(b)-[s:KNOWS]->(a)", 0, 2, false, 4),
            ("(a)-[r:KNOWS|WORKS_AT]->(b)-[s]->(c)", 0, 0, false, 9),
            ("(a)-[r:KNOWS]->(b)-[s:KNOWS]->(c)", 0, 0, true, 10),
        ] {
            let mode = if repeatable {
                "REPEATABLE ELEMENTS "
            } else {
                ""
            };
            let finish = if shape == 2 { "a" } else { "c" };
            let (identities, order) = if direction == 2 {
                ("r AS first,a AS origin,s AS second", "first,origin,second")
            } else {
                ("r AS first,s AS second,a AS origin", "first,second")
            };
            let statement = format!(
                "MATCH {mode}{pattern} RETURN {identities},b AS middle,{finish} AS finish,\
                 r.age+s.age+$by AS cost,a.age+$by AS start_age,b.age+$by AS middle_age,\
                 {finish}.age+$by AS finish_age,{finish}.name+$by AS masked ORDER BY {order}"
            );
            let args = |by| vec![("by".into(), WireValue::Int(by))];
            let handle = reader.prepare_read(cx, &statement, args(2)).await.unwrap();
            for by in [2, -3] {
                let resident = oracle
                    .execute(cx, ExecuteMode::Read, &statement, args(by))
                    .await
                    .unwrap();
                let result = reader
                    .execute(cx, ExecuteMode::ReadStream, &statement, args(by))
                    .await
                    .unwrap();
                assert_eq!(result, resident, "{pattern}");
                assert_eq!(
                    prepared_source(cx, &mut reader, handle, args(by))
                        .await
                        .unwrap(),
                    resident,
                    "{pattern}",
                );
                assert_eq!(result.outcome, Outcome::Rows { seq: 2 });
                assert_eq!(result.rows.len(), count, "{pattern}");
                let mut actual = result
                    .rows
                    .iter()
                    .map(|row| {
                        assert_eq!(row[9], WireValue::Null);
                        [
                            integer(&row[5]),
                            integer(&row[6]),
                            integer(&row[7]),
                            integer(&row[8]),
                        ]
                    })
                    .collect::<Vec<_>>();
                let mut expected = Vec::new();
                let ages = [Some(4), Some(9), None];
                for (r, a, b) in oriented_edges(direction) {
                    for (s, from, c) in oriented_edges(direction) {
                        if (!repeatable && r == s)
                            || from != if shape == 1 { a } else { b }
                            || (shape == 2 && c != a)
                        {
                            continue;
                        }
                        expected.push([
                            Some(r + s + by),
                            ages[a].map(|age| age + by),
                            ages[b].map(|age| age + by),
                            ages[c].map(|age| age + by),
                        ]);
                    }
                }
                assert_eq!(expected.len(), count);
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
fn computed_cold_join_templates_keep_private_keys_and_exact_rebound_history() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start_buffered(cx, "computed-join-history").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed_endpoint_edges(cx, &mut owner).await;
        let mut reader = Client::connect(cx, addr, token(&reader_grant()))
            .await
            .unwrap();
        reader.select(cx, "social").await.unwrap();
        let original = reader
            .execute(
                cx,
                ExecuteMode::ReadStream,
                "MATCH (a)-[r:KNOWS]->(b)-[s:KNOWS]->(c) \
             RETURN r,s,a,c,r.age,s.age,a.age,c.age ORDER BY r,s",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(original.rows.len(), 9);
        let statement = "AT BRANCH $route MATCH (a)-[r:KNOWS]->(b)-[s:KNOWS]->(c) \
            FOR SYSTEM_TIME AS OF SEQ $cut RETURN {first:r,second:s,origin:a,target:c,\
            weight:r.age+s.age+$by,start_age:a.age,finish_age:c.age,masked:c.name+$by} AS doc \
            ORDER BY r,s SKIP $skip LIMIT $take";
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
            .prepare_read(cx, statement, args(2, 0, 0, 9, "main"))
            .await
            .unwrap();
        for mutation in [
            "MATCH ()-[r:KNOWS]->() SET r.age=r.age+10",
            "MATCH (n:Person) WHERE n.age IS NOT NULL SET n.age=n.age+100",
        ] {
            owner
                .execute(cx, ExecuteMode::Write, mutation, vec![])
                .await
                .unwrap();
        }
        for (cut, by, skip, take) in [
            (2, 7, 0, 9),
            (3, -4, 0, 9),
            (4, 1, 0, 9),
            (2, 2, 1, 3),
            (4, 0, 2, 0),
        ] {
            let result =
                prepared_source(cx, &mut reader, handle, args(cut, by, skip, take, "main"))
                    .await
                    .unwrap();
            let endpoint = |value: &WireValue| {
                integer(value).map_or(WireValue::Null, |age| {
                    WireValue::Int(age + if cut == 4 { 100 } else { 0 })
                })
            };
            let expected = original
                .rows
                .iter()
                .skip(skip as usize)
                .take(take as usize)
                .map(|row| {
                    vec![WireValue::Map(vec![
                        ("finish_age".into(), endpoint(&row[7])),
                        ("first".into(), row[0].clone()),
                        ("masked".into(), WireValue::Null),
                        ("origin".into(), row[2].clone()),
                        ("second".into(), row[1].clone()),
                        ("start_age".into(), endpoint(&row[6])),
                        ("target".into(), row[3].clone()),
                        (
                            "weight".into(),
                            WireValue::Int(
                                integer(&row[4]).unwrap()
                                    + integer(&row[5]).unwrap()
                                    + by
                                    + if cut >= 3 { 20 } else { 0 },
                            ),
                        ),
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
fn computed_cold_joins_keep_cumulative_credit_limits_cancel_reuse_and_late_failures() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start_buffered(cx, "computed-join-controls").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed_endpoint_edges(cx, &mut owner).await;
        let mut scope = reader_grant();
        scope.limits.max_rows = 4;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let statement = "MATCH (a)-[r:KNOWS]->(b)-[s:KNOWS]->(c) \
            RETURN r AS first,s AS second,r.age+s.age+1 AS cost ORDER BY first,second LIMIT $take";
        let args = |take| vec![("take".into(), WireValue::Count(take))];
        let handle = reader.prepare_read(cx, statement, args(9)).await.unwrap();
        for nonce in [241, 242] {
            let mut count = 0;
            let error = reader
                .execute_prepared_streaming(
                    cx,
                    PreparedReadMode::SourceStream,
                    handle,
                    args(9),
                    |_| Ok(()),
                    |_| {
                        count += 1;
                        Err(ClientError::Protocol("stop one computed join"))
                    },
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                ClientError::Protocol("stop one computed join")
            ));
            assert_eq!(count, 1);
            reader.ping(cx, nonce).await.unwrap();
        }
        for take in [4, 0] {
            assert_eq!(
                prepared_source(cx, &mut reader, handle, args(take))
                    .await
                    .unwrap()
                    .rows
                    .len(),
                take as usize
            );
        }
        let mut count = 0;
        let error = reader
            .execute_prepared_streaming(
                cx,
                PreparedReadMode::SourceStream,
                handle,
                args(9),
                |_| Ok(()),
                |_| {
                    count += 1;
                    Ok(())
                },
            )
            .await
            .unwrap_err();
        assert_eq!(server_code(error), ErrorCode::Budget);
        assert_eq!(count, 4);
        reader.ping(cx, 243).await.unwrap();
        reader.release_prepared(cx, handle).await.unwrap();

        // Select the last canonical occurrence without assuming either EId's
        // hash order. This denominator is zero only for that ordered pair.
        let original = owner
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (a:Person)-[r:KNOWS]->(b:Person)-[s:KNOWS]->(c:Person) \
             RETURN r AS first,s AS second,r.age AS one,s.age AS two ORDER BY first,second",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(original.rows.len(), 9);
        let last = original.rows.last().unwrap();
        let bad_r = integer(&last[2]).unwrap();
        let bad_s = integer(&last[3]).unwrap();
        let statement = "MATCH (a)-[r:KNOWS]->(b)-[s:KNOWS]->(c) \
            RETURN r AS first,s AS second,\
            100/((r.age-$bad_r)*(r.age-$bad_r)+(s.age-$bad_s)*(s.age-$bad_s)) AS ratio \
            ORDER BY first,second LIMIT $take";
        let args = |bad_r, bad_s, take| {
            vec![
                ("bad_r".into(), WireValue::Int(bad_r)),
                ("bad_s".into(), WireValue::Int(bad_s)),
                ("take".into(), WireValue::Count(take)),
            ]
        };
        let handle = reader
            .prepare_read(cx, statement, args(bad_r, bad_s, 4))
            .await
            .unwrap();
        for take in [4, 0] {
            let mut count = 0;
            let error = reader
                .execute_prepared_streaming(
                    cx,
                    PreparedReadMode::SourceStream,
                    handle,
                    args(bad_r, bad_s, take),
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
            reader.ping(cx, 244 + take).await.unwrap();
        }
        assert_eq!(
            prepared_source(cx, &mut reader, handle, args(-1, -1, 1))
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
