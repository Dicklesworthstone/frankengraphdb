//! Prepared writes through the actual FGP connection and authorized engine.

use super::*;

#[test]
fn fgp_prepared_writes_rebind_atomically_and_enforce_handle_class_and_ownership() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "prepared-write-lifecycle").await;
        let mut scope = grant(Rights::Write);
        scope.limits.max_rows = 0;
        let credential = token(&scope);
        let mut writer = Client::connect(cx, addr, credential.clone()).await.unwrap();
        writer.select(cx, "social").await.unwrap();
        let mut reader = Client::connect(cx, addr, token(&grant(Rights::Read)))
            .await
            .unwrap();
        reader.select(cx, "social").await.unwrap();
        let arguments = |name: &str, age| {
            vec![
                ("name".into(), text(name)),
                ("age".into(), WireValue::Int(age)),
            ]
        };
        let handle = writer
            .prepare_write(
                cx,
                "CREATE (:Person {name:$name,age:$age})",
                arguments("representative only", 99),
            )
            .await
            .unwrap();
        let query = "MATCH (n:Person) RETURN n.name AS name, n.age AS age ORDER BY age";
        let read_handle = reader.prepare_read(cx, query, vec![]).await.unwrap();
        let before = reader
            .execute_prepared(cx, read_handle, vec![])
            .await
            .unwrap();
        assert!(
            before.rows.is_empty(),
            "preparation cannot allocate or publish"
        );
        assert_eq!(before.outcome, Outcome::Rows { seq: 0 });
        for mode in [
            PreparedReadMode::Buffered,
            PreparedReadMode::SourceStream,
            PreparedReadMode::OrderedStream,
        ] {
            assert_eq!(
                server_code(
                    writer
                        .execute_prepared_streaming(
                            cx,
                            mode,
                            handle,
                            arguments("must not write", 1),
                            |_| Ok(()),
                            |_| Ok(()),
                        )
                        .await
                        .unwrap_err()
                ),
                ErrorCode::Statement,
                "a read mode cannot execute a write handle"
            );
        }
        assert_eq!(
            server_code(
                reader
                    .execute_prepared_write(cx, read_handle, vec![])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement,
            "a write mode cannot execute a read handle"
        );
        let mut foreign = Client::connect(cx, addr, credential).await.unwrap();
        foreign.select(cx, "social").await.unwrap();
        assert_eq!(
            server_code(
                foreign
                    .execute_prepared_write(cx, handle, arguments("foreign", 1))
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement
        );
        foreign.release_prepared(cx, handle).await.unwrap();
        foreign.close(cx).await.unwrap();
        let literal = "'); MATCH (n) DETACH DELETE n; //";
        for (seq, name, age) in [(1, "Ann", 30), (2, literal, 7)] {
            assert_eq!(
                writer
                    .execute_prepared_write(cx, handle, arguments(name, age))
                    .await
                    .unwrap(),
                Outcome::WriteCommitted { seq, statements: 1 },
                "write-only max_rows=0 grants need no identity receipts"
            );
        }
        assert_eq!(
            server_code(
                writer
                    .execute_prepared_write(cx, handle, vec![])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement,
            "representative operands are never defaults"
        );
        let atomic = writer.prepare_write(
            cx,
            "CREATE (:Person {name:$name,age:40}); CREATE (:Person {name:'last',age:100/$divisor})",
            vec![("name".into(), text("sample")), ("divisor".into(), WireValue::Int(2))],
        ).await.unwrap();
        let failed = writer
            .execute_prepared_write(
                cx,
                atomic,
                vec![
                    ("name".into(), text("must roll back")),
                    ("divisor".into(), WireValue::Int(0)),
                ],
            )
            .await
            .unwrap_err();
        assert!(matches!(
            server_code(failed),
            ErrorCode::Statement | ErrorCode::Execution
        ));
        let unchanged = reader
            .execute_prepared(cx, read_handle, vec![])
            .await
            .unwrap();
        assert_eq!(
            unchanged.rows,
            [
                vec![text(literal), WireValue::Int(7)],
                vec![text("Ann"), WireValue::Int(30)]
            ]
        );
        assert_eq!(unchanged.outcome, Outcome::Rows { seq: 2 });
        assert_eq!(
            writer
                .execute_prepared_write(
                    cx,
                    atomic,
                    vec![
                        ("name".into(), text("new")),
                        ("divisor".into(), WireValue::Int(2))
                    ],
                )
                .await
                .unwrap(),
            Outcome::WriteCommitted {
                seq: 3,
                statements: 2
            },
            "the entire script has one completion after a fresh rebind"
        );
        writer.release_prepared(cx, atomic).await.unwrap();
        writer.release_prepared(cx, atomic).await.unwrap();
        assert_eq!(
            server_code(
                writer
                    .execute_prepared_write(cx, atomic, vec![])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement
        );
        scope.limits.max_work = 100_000;
        writer.refresh_authority(cx, token(&scope)).await.unwrap();
        assert_eq!(
            server_code(
                writer
                    .execute_prepared_write(cx, handle, arguments("stale", 1))
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement,
            "AUTH_REFRESH discards every write template"
        );
        assert_eq!(
            server_code(
                reader
                    .prepare_write(cx, "malformed syntax", vec![])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::PermissionDenied,
            "Write rights precede syntax inspection"
        );
        let final_rows = reader
            .execute_prepared(cx, read_handle, vec![])
            .await
            .unwrap();
        assert_eq!(final_rows.rows.len(), 4);
        assert_eq!(final_rows.outcome, Outcome::Rows { seq: 3 });
        writer.close(cx).await.unwrap();
        reader.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn fgp_prepared_merge_uses_current_state_and_preparation_preserves_authority_budgets() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "prepared-write-admission").await;
        let mut scope = grant(Rights::ReadWrite);
        scope.limits.max_rows = 0;
        let mut writer = Client::connect(cx, addr, token(&scope)).await.unwrap();
        writer.select(cx, "social").await.unwrap();
        let statement =
            "MERGE (n:Person {name:$name}) ON CREATE SET n.age=$age ON MATCH SET n.age=n.age+$age";
        let args = |age| {
            vec![
                ("name".into(), text("Ann")),
                ("age".into(), WireValue::Int(age)),
            ]
        };
        let handle = writer.prepare_write(cx, statement, args(99)).await.unwrap();
        for (seq, age) in [(1, 2), (2, 3)] {
            assert_eq!(
                writer
                    .execute_prepared_write(cx, handle, args(age))
                    .await
                    .unwrap(),
                Outcome::WriteCommitted { seq, statements: 1 }
            );
        }
        for text in [
            "MATCH (n) RETURN n",
            "CREATE (n:Person) RETURN n",
            "CREATE (n:Person) RETURN 1/0 LIMIT 0",
        ] {
            assert_eq!(
                server_code(writer.prepare_write(cx, text, vec![]).await.unwrap_err()),
                ErrorCode::Statement
            );
        }
        let mut only_write = Client::connect(cx, addr, token(&grant(Rights::Write)))
            .await
            .unwrap();
        only_write.select(cx, "social").await.unwrap();
        assert_eq!(
            server_code(
                only_write
                    .prepare_write(cx, statement, args(1))
                    .await
                    .unwrap_err()
            ),
            ErrorCode::PermissionDenied
        );
        let mut tiny = grant(Rights::Write);
        tiny.limits.max_work = 1;
        let mut limited = Client::connect(cx, addr, token(&tiny)).await.unwrap();
        limited.select(cx, "social").await.unwrap();
        assert_eq!(
            server_code(
                limited
                    .prepare_write(cx, "CREATE (:Person)", vec![])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Budget
        );
        let mut reader = Client::connect(cx, addr, token(&grant(Rights::Read)))
            .await
            .unwrap();
        reader.select(cx, "social").await.unwrap();
        let answer = reader
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (n:Person) RETURN n.name AS name, n.age AS age",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(answer.rows, [vec![text("Ann"), WireValue::Int(5)]]);
        assert_eq!(answer.outcome, Outcome::Rows { seq: 2 });
        reader.close(cx).await.unwrap();
        limited.close(cx).await.unwrap();
        only_write.close(cx).await.unwrap();
        writer.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn fgp_prepared_returning_covers_all_native_write_families() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "prepared-returning-families").await;
        let mut client = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        client.select(cx, "social").await.unwrap();
        let args = |name: &str, age| {
            vec![
                ("name".into(), text(name)),
                ("age".into(), WireValue::Int(age)),
            ]
        };
        let insert = client
            .prepare_write_returning(
                cx,
                "CREATE (n:Person {name:$name,age:$age}) RETURN n.name AS name,n.age AS age",
                args("sample", 99),
            )
            .await
            .unwrap();
        assert_eq!(
            server_code(
                client
                    .execute_prepared_write(cx, insert, args("wrong", 0))
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement
        );
        assert_eq!(
            server_code(
                client
                    .execute_prepared_batch(cx, insert, vec![args("wrong", 0)])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement
        );
        assert_eq!(
            server_code(
                client
                    .execute_prepared(cx, insert, args("wrong", 0))
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement
        );
        for (seq, name, age) in [(1, "Ada", 1), (2, "Bob", 2)] {
            let answer = client
                .execute_prepared_write_returning(cx, insert, args(name, age))
                .await
                .unwrap();
            assert_eq!(answer.columns, ["name", "age"]);
            assert_eq!(answer.rows, [vec![text(name), WireValue::Int(age)]]);
            assert_eq!(
                answer.outcome,
                Outcome::WriteCommitted { seq, statements: 1 }
            );
        }
        let mutation = client
            .prepare_write_returning(
                cx,
                "MATCH (n:Person {name:$name}) SET n.age=$age RETURN n.age AS age",
                args("sample", 99),
            )
            .await
            .unwrap();
        let answer = client
            .execute_prepared_write_returning(cx, mutation, args("Ada", 3))
            .await
            .unwrap();
        assert_eq!(answer.rows, [vec![WireValue::Int(3)]]);
        assert_eq!(
            answer.outcome,
            Outcome::WriteCommitted {
                seq: 3,
                statements: 1
            }
        );

        let merge = client.prepare_write_returning(
            cx,
            "MERGE (n:Person {name:$name}) ON CREATE SET n.age=$age ON MATCH SET n.age=n.age+$age RETURN n.age AS age",
            args("sample", 99),
        ).await.unwrap();
        for (seq, name, age, expected) in [(4, "Ada", 1, 4), (5, "Carl", 5, 5)] {
            let answer = client
                .execute_prepared_write_returning(cx, merge, args(name, age))
                .await
                .unwrap();
            assert_eq!(answer.rows, [vec![WireValue::Int(expected)]]);
            assert_eq!(
                answer.outcome,
                Outcome::WriteCommitted { seq, statements: 1 }
            );
        }

        let edge_args = |name: &str, age| {
            vec![
                ("edge".into(), text(name)),
                ("age".into(), WireValue::Int(age)),
            ]
        };
        let edge = client.prepare_write_returning(
            cx,
            "MATCH (a:Person {name:'Ada'}),(b:Person {name:'Bob'}) MERGE (a)-[r:KNOWS {name:$edge,age:$age}]->(b) RETURN r.name AS name",
            edge_args("sample", 99),
        ).await.unwrap();
        for changed in [true, false] {
            let answer = client
                .execute_prepared_write_returning(cx, edge, edge_args("friend", 1))
                .await
                .unwrap();
            assert_eq!(answer.rows, [vec![text("friend")]]);
            assert_eq!(
                answer.outcome,
                if changed {
                    Outcome::WriteCommitted {
                        seq: 6,
                        statements: 1,
                    }
                } else {
                    Outcome::ReadClosed {
                        seq: 6,
                        statements: 1,
                    }
                }
            );
        }

        let upsert = client.prepare_write_returning(
            cx,
            "MATCH (a:Person {name:'Ada'}),(b:Person {name:'Bob'}) MERGE (a)-[r:KNOWS {name:$edge}]->(b) ON CREATE SET r.age=$age ON MATCH SET r.age=r.age+$age RETURN r.age AS age",
            edge_args("sample", 99),
        ).await.unwrap();
        let answer = client
            .execute_prepared_write_returning(cx, upsert, edge_args("friend", 2))
            .await
            .unwrap();
        assert_eq!(answer.rows, [vec![WireValue::Int(3)]]);
        assert_eq!(
            answer.outcome,
            Outcome::WriteCommitted {
                seq: 7,
                statements: 1
            }
        );

        let chain_args = |first: &str, second: &str| {
            vec![
                ("first".into(), text(first)),
                ("second".into(), text(second)),
            ]
        };
        let chain = client.prepare_write_returning(
            cx,
            "CREATE (a:Person {name:$first}) WITH a CREATE (b:Person {name:$second}) RETURN a.name AS first,b.name AS second",
            chain_args("sample", "other sample"),
        ).await.unwrap();
        let answer = client
            .execute_prepared_write_returning(cx, chain, chain_args("Dora", "Eve"))
            .await
            .unwrap();
        assert_eq!(answer.columns, ["first", "second"]);
        assert_eq!(answer.rows, [vec![text("Dora"), text("Eve")]]);
        assert_eq!(
            answer.outcome,
            Outcome::WriteCommitted {
                seq: 8,
                statements: 1
            }
        );
        let stored = client
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (n:Person) RETURN n.name AS name ORDER BY name",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(
            stored.rows,
            ["Ada", "Bob", "Carl", "Dora", "Eve"].map(|name| vec![text(name)])
        );
        assert_eq!(stored.outcome, Outcome::Rows { seq: 8 });
        client.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn fgp_prepared_returning_cancellation_only_stops_delivery_and_authority_stays_live() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "prepared-returning-delivery").await;
        let mut client = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        client.select(cx, "social").await.unwrap();
        let handle = client.prepare_write_returning(
            cx,
            "UNWIND [1,2,3,4,5,6] AS x CREATE (n:Person {age:x}) RETURN x AS value ORDER BY value",
            vec![],
        ).await.unwrap();
        let mut delivered = 0;
        let error = client
            .execute_prepared_write_returning_streaming(
                cx,
                handle,
                vec![],
                |_| Ok(()),
                |_| {
                    delivered += 1;
                    Err(ClientError::Protocol("consumer stopped"))
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ClientError::Protocol("consumer stopped")));
        assert_eq!(delivered, 1);
        let read = "MATCH (n:Person) RETURN n.age AS age ORDER BY age";
        let stored = client
            .execute(cx, ExecuteMode::Read, read, vec![])
            .await
            .unwrap();
        assert_eq!(
            stored.rows,
            (1..=6)
                .map(|age| vec![WireValue::Int(age)])
                .collect::<Vec<_>>()
        );
        assert_eq!(stored.outcome, Outcome::Rows { seq: 1 });

        let mut reader = Client::connect(cx, addr, token(&grant(Rights::Read)))
            .await
            .unwrap();
        reader.select(cx, "social").await.unwrap();
        assert_eq!(
            server_code(
                reader
                    .prepare_write_returning(cx, "malformed syntax", vec![])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::PermissionDenied,
            "Write authority is checked before syntax"
        );
        let mut tiny = grant(Rights::ReadWrite);
        tiny.limits.max_work = 1;
        let mut limited = Client::connect(cx, addr, token(&tiny)).await.unwrap();
        limited.select(cx, "social").await.unwrap();
        assert_eq!(
            server_code(
                limited
                    .prepare_write_returning(cx, "CREATE (n) RETURN n", vec![])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Budget
        );
        let mut no_rows = grant(Rights::ReadWrite);
        no_rows.limits.max_rows = 0;
        assert_eq!(
            server_code(
                limited
                    .refresh_authority(cx, token(&no_rows))
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Unauthenticated,
            "zero rows cannot authorize widening the previous one-unit work allowance"
        );
        let mut limited = Client::connect(cx, addr, token(&no_rows)).await.unwrap();
        limited.select(cx, "social").await.unwrap();
        let limited_handle = limited
            .prepare_write_returning(cx, "CREATE (n:Person {age:99}) RETURN n.age AS age", vec![])
            .await
            .unwrap();
        assert_eq!(
            server_code(
                limited
                    .execute_prepared_write_returning(cx, limited_handle, vec![])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Budget
        );
        let after = reader
            .execute(cx, ExecuteMode::Read, read, vec![])
            .await
            .unwrap();
        assert_eq!(after.rows, stored.rows);
        assert_eq!(after.outcome, stored.outcome);
        client
            .refresh_authority(cx, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        assert_eq!(
            server_code(
                client
                    .execute_prepared_write_returning(cx, handle, vec![])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement,
            "AUTH_REFRESH discards returning handles too"
        );
        limited.close(cx).await.unwrap();
        reader.close(cx).await.unwrap();
        client.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

/// fgdb-ch9re (fgdb-0s2pq): a driver sends a record as one map parameter.
/// Through the server, a direct write and a direct RETURN write splice it as
/// the literal map of its keys. A prepared write and a prepared RETURN write
/// bind each execution's map with the keys they were prepared with; a map
/// with other keys refuses typed and writes nothing.
#[test]
fn fgp_map_parameters_bind_as_property_maps() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "map-parameter-writes").await;
        let mut client = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        client.select(cx, "social").await.unwrap();
        let props = |entries: &[(&str, WireValue)]| {
            let entries = (entries.iter())
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect();
            ("props".to_owned(), WireValue::Map(entries))
        };
        let direct = client
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (n:Person $props)",
                vec![props(&[("age", WireValue::Int(30)), ("name", text("Ann"))])],
            )
            .await
            .unwrap();
        assert!(matches!(direct.outcome, Outcome::WriteCommitted { .. }));
        let returned = client
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (n:Person {name: 'Ann'}) SET n += $props RETURN n.name AS name, n.age AS age",
                vec![props(&[("age", WireValue::Int(31))])],
            )
            .await
            .unwrap();
        assert_eq!(returned.rows, [vec![text("Ann"), WireValue::Int(31)]]);

        let set = client
            .prepare_write(
                cx,
                "MATCH (n:Person {name: $name}) SET n += $props",
                vec![
                    ("name".into(), text("representative")),
                    props(&[("age", WireValue::Int(0))]),
                ],
            )
            .await
            .unwrap();
        let committed = client
            .execute_prepared_write(
                cx,
                set,
                vec![
                    ("name".into(), text("Ann")),
                    props(&[("age", WireValue::Int(32))]),
                ],
            )
            .await
            .unwrap();
        assert!(matches!(committed, Outcome::WriteCommitted { .. }));
        let create = client
            .prepare_write_returning(
                cx,
                "CREATE (n:Person $props) RETURN n.name AS name, n.age AS age",
                vec![props(&[
                    ("age", WireValue::Int(0)),
                    ("name", text("sample")),
                ])],
            )
            .await
            .unwrap();
        let created = client
            .execute_prepared_write_returning(
                cx,
                create,
                vec![props(&[("age", WireValue::Int(40)), ("name", text("Bob"))])],
            )
            .await
            .unwrap();
        assert_eq!(created.rows, [vec![text("Bob"), WireValue::Int(40)]]);

        // A batch splices with its first set's keys and binds every set with
        // them; a set with other keys refuses the whole batch.
        let batch = "MATCH (n:Person {name: $name}) SET n += $props";
        let record = |name: &str, entries: &[(&str, WireValue)]| {
            vec![("name".to_owned(), text(name)), props(entries)]
        };
        let batched = client
            .execute_batch(
                cx,
                batch,
                vec![
                    record("Ann", &[("age", WireValue::Int(33))]),
                    record("Bob", &[("age", WireValue::Int(41))]),
                ],
            )
            .await
            .unwrap();
        assert!(matches!(batched, Outcome::WriteCommitted { .. }));
        let mixed = client
            .execute_batch(
                cx,
                batch,
                vec![
                    record("Ann", &[("age", WireValue::Int(1))]),
                    record("Bob", &[("name", text("Zed"))]),
                ],
            )
            .await
            .unwrap_err();
        assert_eq!(server_code(mixed), ErrorCode::Statement);

        // Another key set refuses typed, on both handles, and writes nothing.
        let other = client
            .execute_prepared_write(
                cx,
                set,
                vec![
                    ("name".into(), text("Ann")),
                    props(&[("name", text("Zed"))]),
                ],
            )
            .await
            .unwrap_err();
        assert_eq!(server_code(other), ErrorCode::Statement);
        let other = client
            .execute_prepared_write_returning(cx, create, vec![props(&[("name", text("Cy"))])])
            .await
            .unwrap_err();
        assert_eq!(server_code(other), ErrorCode::Statement);
        let people = client
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (n:Person) RETURN n.name AS name, n.age AS age ORDER BY name",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(
            people.rows,
            [
                vec![text("Ann"), WireValue::Int(33)],
                vec![text("Bob"), WireValue::Int(41)],
            ]
        );
        client.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}
