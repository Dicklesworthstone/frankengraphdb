//! The server retains document rows across MATCH and a later nested UNWIND.
//! Direct and prepared execution share native authorization and one completion.

use super::*;

fn parameters() -> Vec<(String, WireValue)> {
    let record = |name: &str, age, friends: WireValue| {
        WireValue::Map(vec![
            ("age".into(), WireValue::Int(age)),
            ("friends".into(), friends),
            ("name".into(), WireValue::Text(name.into())),
        ])
    };
    let names = |values: &[&str]| {
        WireValue::List(
            values
                .iter()
                .map(|name| WireValue::Text((*name).into()))
                .collect(),
        )
    };
    vec![(
        "rows".into(),
        WireValue::List(vec![
            record("Ada", 10, names(&["Bob", "Bob"])),
            record("Bob", 20, names(&["Ada", "Cy"])),
            record("Cy", 30, names(&[])),
            record("Ada", 40, WireValue::Null),
            WireValue::Map(vec![("name".into(), WireValue::Text("Cy".into()))]),
        ]),
    )]
}

fn statement(carry: &str, tail: &str) -> String {
    format!(
        "UNWIND $rows AS row MATCH (p:Person {{name:row.name}}) \
         {carry}UNWIND row.friends AS friend MATCH (f:Person {{name:friend}}) {tail}"
    )
}

fn field_parameters(name: &str, ages: [i64; 2]) -> Vec<(String, WireValue)> {
    vec![(
        "rows".into(),
        WireValue::List(
            ages.into_iter()
                .map(|age| {
                    WireValue::Map(vec![
                        ("age".into(), WireValue::Int(age)),
                        (
                            "friends".into(),
                            WireValue::List(vec![WireValue::Text("Bob".into())]),
                        ),
                        ("name".into(), WireValue::Text(name.into())),
                    ])
                })
                .collect(),
        ),
    )]
}

#[test]
fn native_field_roots_keep_rank_frozen_keys_and_one_statement_over_fgp() {
    run(async |cx| {
        for (prepared, carry, ages) in [
            (false, "WITH p,row ", [100, 200]),
            (true, "WITH p,row ", [200, 100]),
            (false, "WITH row,p ", [200, 100]),
            (true, "WITH row,p ", [100, 200]),
        ] {
            let fixture = Fixture::start(cx, "native-field-roots", |config| {
                config.max_statements = 2;
            })
            .await;
            let mut writer = fixture.client(cx, &grant(Rights::ReadWrite, 0)).await;
            let mut observer = fixture.client(cx, &grant(Rights::Read, 100)).await;
            committed(writer.execute(cx, ExecuteMode::Write,
                "CREATE (:Person {name:'Ada'}),(:Person {name:'Bob'}),(:Person {name:'Cy'})",
                vec![],
            ).await.unwrap(), 1, 1);
            let before = state(cx, &mut observer).await;
            // The first root evaluates successfully, but the second divides
            // by zero. Neither root may publish its proposed key/age update.
            let bad = statement(carry, "SET p.name='Renamed',f.age=10/(200-row.age)");
            let refusal = if prepared {
                let handle = writer
                    .prepare_write(cx, &bad, field_parameters("Ada", [100, 200]))
                    .await
                    .unwrap();
                writer
                    .execute_prepared_write(cx, handle, field_parameters("Ada", [100, 200]))
                    .await
                    .unwrap_err()
            } else {
                writer
                    .execute(
                        cx,
                        ExecuteMode::Write,
                        &bad,
                        field_parameters("Ada", [100, 200]),
                    )
                    .await
                    .unwrap_err()
            };
            assert!(matches!(
                refusal,
                ClientError::Server {
                    code: ErrorCode::Execution,
                    ..
                }
            ));
            assert_eq!(state(cx, &mut observer).await, before);

            let text = statement(carry, "SET p.name='Renamed',f.age=row.age");
            let mut excess = field_parameters("Ada", ages);
            if let WireValue::List(rows) = &mut excess[0].1 {
                rows.push(rows[0].clone());
            }
            let cap_error = if prepared {
                let handle = writer
                    .prepare_write(cx, &text, field_parameters("Ada", ages))
                    .await
                    .unwrap();
                writer
                    .execute_prepared_write(cx, handle, excess)
                    .await
                    .unwrap_err()
            } else {
                writer
                    .execute(cx, ExecuteMode::Write, &text, excess)
                    .await
                    .unwrap_err()
            };
            assert!(matches!(
                cap_error,
                ClientError::Server {
                    code: ErrorCode::Budget,
                    ..
                }
            ));
            assert_eq!(state(cx, &mut observer).await, before);
            let mut limited = grant(Rights::ReadWrite, 0);
            limited.properties = Scope::only([fgdb_delta_types::PropertyKeyId(1)]);
            let mut limited = fixture.client(cx, &limited).await;
            assert!(matches!(
                limited
                    .execute(cx, ExecuteMode::Write, &text, field_parameters("Ada", ages))
                    .await
                    .unwrap_err(),
                ClientError::Server {
                    code: ErrorCode::PermissionDenied,
                    ..
                }
            ));
            assert_eq!(state(cx, &mut observer).await, before);
            limited.close(cx).await.unwrap();

            if prepared {
                let handle = writer
                    .prepare_write(cx, &text, field_parameters("Ada", ages))
                    .await
                    .unwrap();
                assert_eq!(
                    writer
                        .execute_prepared_write(cx, handle, field_parameters("Ada", ages))
                        .await
                        .unwrap(),
                    Outcome::WriteCommitted {
                        seq: 2,
                        statements: 1
                    }
                );
                let mut extra = vec![("rows".into(), WireValue::List(vec![]))];
                extra.push(("unused".into(), WireValue::Int(1)));
                assert!(matches!(
                    writer
                        .execute_prepared_write(cx, handle, extra)
                        .await
                        .unwrap_err(),
                    ClientError::Server {
                        code: ErrorCode::Statement,
                        ..
                    }
                ));
                assert_eq!(
                    writer
                        .execute_prepared_write(
                            cx,
                            handle,
                            vec![("rows".into(), WireValue::List(vec![]))]
                        )
                        .await
                        .unwrap(),
                    Outcome::ReadClosed {
                        seq: 2,
                        statements: 1
                    }
                );
            } else {
                committed(
                    writer
                        .execute(cx, ExecuteMode::Write, &text, field_parameters("Ada", ages))
                        .await
                        .unwrap(),
                    2,
                    1,
                );
            }
            let after = state(cx, &mut observer).await;
            let bob = after
                .0
                .rows
                .iter()
                .find(|row| row[1] == WireValue::Text("Bob".into()))
                .unwrap();
            assert_eq!(bob[2], WireValue::Int(ages[1]));
            assert_eq!(
                after
                    .0
                    .rows
                    .iter()
                    .filter(|row| row[1] == WireValue::Text("Renamed".into()))
                    .count(),
                1
            );
            assert_eq!(after.0.rows.len(), 3);
            assert!(after.1.rows.is_empty());

            // Literal-only assignments keep the ordinary simultaneous owner.
            committed(
                writer
                    .execute(
                        cx,
                        ExecuteMode::Write,
                        &statement(carry, "SET f.age=777"),
                        field_parameters("Renamed", ages),
                    )
                    .await
                    .unwrap(),
                3,
                1,
            );
            // An explicit two-record batch retains its actual expanded count.
            let batch = writer
                .prepare_write(
                    cx,
                    "MATCH (p:Person {name:'Cy'}) SET p.age=$age",
                    vec![("age".into(), WireValue::Int(1))],
                )
                .await
                .unwrap();
            assert_eq!(
                writer
                    .execute_prepared_batch(
                        cx,
                        batch,
                        vec![
                            vec![("age".into(), WireValue::Int(1))],
                            vec![("age".into(), WireValue::Int(2))],
                        ]
                    )
                    .await
                    .unwrap(),
                Outcome::WriteCommitted {
                    seq: 4,
                    statements: 2
                }
            );
            writer.close(cx).await.unwrap();
            observer.close(cx).await.unwrap();
            fixture.stop(cx, 4, 3, 0).await;
        }
    });
}

#[test]
fn post_match_lists_work_over_fgp_and_prepared_writes_with_atomic_refusals() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "post-match-unwind", |config| {
            config.write_policy.max_created_edges = 3;
        })
        .await;
        let mut writer = fixture.client(cx, &grant(Rights::ReadWrite, 0)).await;
        let mut observer = fixture.client(cx, &grant(Rights::Read, 100)).await;
        committed(
            writer
                .execute(
                    cx,
                    ExecuteMode::Write,
                    "CREATE (:Person {name:'Ada'}),(:Person {name:'Bob'}),(:Person {name:'Cy'})",
                    vec![],
                )
                .await
                .unwrap(),
            1,
            1,
        );
        let tail = "MERGE (p)-[r:KNOWS]->(f) ON CREATE SET r.age=row.age \
            ON MATCH SET r.age=r.age+1";
        committed(
            writer
                .execute(cx, ExecuteMode::Write, &statement("", tail), parameters())
                .await
                .unwrap(),
            2,
            1,
        );
        let imported = state(cx, &mut observer).await;
        assert_eq!((imported.0.rows.len(), imported.1.rows.len()), (3, 3));
        assert_eq!(
            imported
                .1
                .rows
                .iter()
                .map(|row| (row[0].clone(), row[1].clone(), row[3].clone()))
                .collect::<Vec<_>>(),
            [("Ada", "Bob", 11), ("Bob", "Ada", 20), ("Bob", "Cy", 20)].map(|(a, b, age)| (
                WireValue::Text(a.into()),
                WireValue::Text(b.into()),
                WireValue::Int(age)
            ))
        );

        let prepared = writer
            .prepare_write(cx, &statement("WITH p, row ", tail), parameters())
            .await
            .unwrap();
        assert_eq!(
            writer
                .execute_prepared_write(cx, prepared, parameters())
                .await
                .unwrap(),
            Outcome::WriteCommitted {
                seq: 3,
                statements: 1
            }
        );
        let expected = state(cx, &mut observer).await;
        assert_eq!(
            expected
                .1
                .rows
                .iter()
                .map(|row| row[3].clone())
                .collect::<Vec<_>>(),
            [13, 21, 21].map(WireValue::Int)
        );
        assert_eq!(
            writer
                .execute_prepared_write(
                    cx,
                    prepared,
                    vec![("rows".into(), WireValue::List(vec![]))],
                )
                .await
                .unwrap(),
            Outcome::ReadClosed {
                seq: 3,
                statements: 1
            }
        );

        for (tail, expected_code) in [
            ("CREATE (p)-[:KNOWS]->(f)", ErrorCode::Budget),
            (
                "MERGE (p)-[r:KNOWS]->(f) ON MATCH SET r.age=10/(20-row.age)",
                ErrorCode::Execution,
            ),
        ] {
            let (mut columns, mut rows) = (0, 0);
            let error = writer
                .execute_streaming(
                    cx,
                    ExecuteMode::Write,
                    &statement("", tail),
                    parameters(),
                    |_| {
                        columns += 1;
                        Ok(())
                    },
                    |_| {
                        rows += 1;
                        Ok(())
                    },
                )
                .await
                .unwrap_err();
            let ClientError::Server { code, message } = error else {
                panic!("native execution should refuse: {error:?}");
            };
            assert_eq!(code, expected_code, "{message}");
            assert!(!message.is_empty());
            assert_eq!((columns, rows), (0, 0));
            assert_eq!(state(cx, &mut observer).await, expected);
        }

        let mut restricted = grant(Rights::ReadWrite, 0);
        restricted.properties = Scope::only([fgdb_delta_types::PropertyKeyId(1)]);
        let mut restricted = fixture.client(cx, &restricted).await;
        let refusal = restricted
            .execute(cx, ExecuteMode::Write, &statement("", tail), parameters())
            .await
            .unwrap_err();
        assert!(matches!(
            refusal,
            ClientError::Server {
                code: ErrorCode::PermissionDenied,
                ..
            }
        ));
        assert_eq!(state(cx, &mut observer).await, expected);
        restricted.close(cx).await.unwrap();

        committed(
            writer
                .execute(
                    cx,
                    ExecuteMode::Write,
                    &statement("WITH p, row ", "SET p.name='Renamed', f.age=row.age"),
                    vec![(
                        "rows".into(),
                        WireValue::List(
                            [100, 200]
                                .into_iter()
                                .map(|age| {
                                    WireValue::Map(vec![
                                        ("age".into(), WireValue::Int(age)),
                                        (
                                            "friends".into(),
                                            WireValue::List(vec![WireValue::Text("Bob".into())]),
                                        ),
                                        ("name".into(), WireValue::Text("Ada".into())),
                                    ])
                                })
                                .collect(),
                        ),
                    )],
                )
                .await
                .unwrap(),
            4,
            1,
        );
        // Changing the first MATCH key cannot erase the second occurrence
        // already selected by this statement. Row-by-row rematching yields
        // age=100; the native clause pipeline must retain age=200.
        let final_state = state(cx, &mut observer).await;
        let bob = final_state
            .0
            .rows
            .iter()
            .find(|row| row[1] == WireValue::Text("Bob".into()))
            .unwrap();
        assert_eq!(bob[2], WireValue::Int(200));
        writer.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        fixture.stop(cx, 4, 3, 3).await;
    });
}
