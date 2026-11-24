//! Public FGP source and ordered reads over the configured cold backend.
use super::*;
use fgdb_delta_types::catalog::{CatalogSymbol, SymbolCatalog};

async fn start_buffered(
    cx: &Cx,
    name: &str,
) -> (
    SocketAddr,
    fgdb_server::Shutdown,
    asupersync::runtime::TaskHandle<()>,
) {
    let (addr, shutdown, task, _) = start_buffered_configured(cx, name, false).await;
    (addr, shutdown, task)
}

async fn start_buffered_configured(
    cx: &Cx,
    name: &str,
    ordered: bool,
) -> (
    SocketAddr,
    fgdb_server::Shutdown,
    asupersync::runtime::TaskHandle<()>,
    PathBuf,
) {
    let path = scratch(name);
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let mut database = Database::create(&contexts.commit(), &path, keys())
        .await
        .unwrap();
    let mut catalog = SymbolCatalog::default();
    catalog
        .bind_all([
            ("Person", CatalogSymbol::Label(LabelId(1))),
            ("Company", CatalogSymbol::Label(LabelId(2))),
            ("KNOWS", CatalogSymbol::Relation(RelationId(1))),
            ("WORKS_AT", CatalogSymbol::Relation(RelationId(2))),
            ("LIKES", CatalogSymbol::Relation(RelationId(3))),
            ("name", CatalogSymbol::Property(PropertyKeyId(1))),
            ("age", CatalogSymbol::Property(PropertyKeyId(2))),
        ])
        .unwrap();
    assert_eq!(
        database
            .register_symbols(&contexts.commit(), &catalog)
            .await
            .unwrap()
            .0,
        1
    );
    drop(database);
    let limits = ServerLimits {
        max_frame_len: 4096,
        initial_window_bytes: 8192,
        initial_window_rows: 2,
        max_window_bytes: 1 << 20,
        max_window_rows: 1 << 10,
        max_connections: 8,
    };
    let mut server = Server::new(cx, limits).unwrap();
    let mut config = DatabaseConfig::new("social", keys(), issuer_key());
    // Exact durable redeclarations are accepted. No request injects a resolver.
    config.symbols = symbols();
    let mut buffered = fgdb_server::BufferedReadConfig::new(64 * 1024 * 1024);
    buffered.max_concurrent = 1;
    config.buffered_reads = Some(buffered);
    let directory = path.join("query-scratch");
    if ordered {
        asupersync::fs::create_dir(&directory).await.unwrap();
        let mut order = fgdb_server::OrderedReadConfig::new(&directory);
        order.memory_bytes = 64 * 1024;
        order.sort.group_capacity = 2;
        order.sort.max_partitions = 32;
        order.sort.run_rows = 2;
        order.sort.page_bytes = 31;
        order.sort.max_row_bytes = 4096;
        order.sort.max_sort_work = 1_000_000;
        order.sort.max_input_rows = 100;
        config.ordered_reads = Some(order);
    }
    server.open_database(cx, &path, config).await.unwrap();
    let server = Arc::new(server);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = server.shutdown();
    let handle = cx
        .spawn(move |child| async move {
            server.serve(&child, listener).await.unwrap();
        })
        .unwrap();
    (addr, shutdown, handle, directory)
}

async fn seed_people(cx: &Cx, owner: &mut Client) {
    let outcome = owner
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
    assert!(matches!(
        outcome.outcome,
        Outcome::WriteCommitted { seq: 2, .. }
    ));
}

fn reader_grant() -> Grant {
    let mut scope = grant(Rights::Read);
    scope.labels = Scope::only([LabelId(1)]);
    scope.relations = Scope::only([RelationId(1)]);
    scope.properties = Scope::only([PropertyKeyId(2)]);
    scope
}

fn ages(rows: &[Vec<WireValue>]) -> Vec<i64> {
    let mut ages: Vec<_> = rows
        .iter()
        .map(|row| {
            assert_eq!(row.len(), 2);
            assert_eq!(row[1], WireValue::Null);
            match row[0] {
                WireValue::Int(age) => age,
                ref other => panic!("unexpected age: {other:?}"),
            }
        })
        .collect();
    ages.sort_unstable();
    ages
}

async fn seed_endpoint_edges(cx: &Cx, owner: &mut Client) {
    let result = owner
        .execute(
            cx,
            ExecuteMode::Write,
            "CREATE (a:Person {age:4,name:'left'}), (b:Person {age:9,name:'right'}), \
             (c:Person {name:'missing'}), (h:Company {age:999,name:'hidden'}), \
             (a)-[:KNOWS {age:1}]->(b), (a)-[:KNOWS {age:2}]->(b), \
             (b)-[:KNOWS {age:3}]->(a), (a)-[:KNOWS {age:4}]->(a), \
             (a)-[:KNOWS {age:5}]->(c), (a)-[:WORKS_AT {age:999}]->(b), \
             (a)-[:KNOWS {age:999}]->(h), (h)-[:KNOWS {age:999}]->(a)",
            vec![],
        )
        .await
        .unwrap();
    assert!(matches!(
        result.outcome,
        Outcome::WriteCommitted { seq: 2, .. }
    ));
}

#[test]
fn buffered_endpoint_fields_keep_orientation_scopes_and_prepared_history() {
    run(async |cx| {
        let (addr, shutdown, mut task, directory) =
            start_buffered_configured(cx, "buffered-endpoint-fields", true).await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed_endpoint_edges(cx, &mut owner).await;
        let mut scope = reader_grant();
        // Four visible non-self edges charge two endpoints each; the self
        // edge charges once. Denied relations and endpoints charge nothing.
        scope.limits.max_nodes = 9;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        // Resident evaluation has its own accounting route. Its oracle keeps
        // the same scopes without imposing the cold source's candidate bound.
        let mut oracle_scope = scope.clone();
        oracle_scope.limits.max_nodes = 100;
        let mut oracle = Client::connect(cx, addr, token(&oracle_scope))
            .await
            .unwrap();
        oracle.select(cx, "social").await.unwrap();

        // This exact statement used to be refused for its endpoint field.
        let original = reader
            .execute(
                cx,
                ExecuteMode::ReadStream,
                "MATCH (a)-[r:KNOWS]->(b) RETURN r.age,b.name ORDER BY r",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(ages(&original.rows), [1, 2, 3, 4, 5]);
        assert_eq!(original.outcome, Outcome::Rows { seq: 2 });

        for (pattern, incoming) in [("(a)-[r:KNOWS]->(b)", false), ("(a)<-[r:KNOWS]-(b)", true)] {
            let statement = format!(
                "MATCH {pattern} RETURN r.age AS weight,a.age AS from_age,b.age AS to_age,\
                 a.name AS from_hidden,b.name AS to_hidden ORDER BY r"
            );
            let resident = oracle
                .execute(cx, ExecuteMode::Read, &statement, vec![])
                .await
                .unwrap();
            for mode in [ExecuteMode::ReadStream, ExecuteMode::ReadOrdered] {
                let mut result = reader.execute(cx, mode, &statement, vec![]).await.unwrap();
                assert_eq!(result, resident, "{mode:?}: {pattern}");
                assert_eq!(
                    result.columns,
                    ["weight", "from_age", "to_age", "from_hidden", "to_hidden"]
                );
                result.rows.sort_by_key(|row| match row[0] {
                    WireValue::Int(weight) => weight,
                    _ => panic!("edge weight must be an integer"),
                });
                let expected = [
                    (1, Some(4), Some(9)),
                    (2, Some(4), Some(9)),
                    (3, Some(9), Some(4)),
                    (4, Some(4), Some(4)),
                    (5, Some(4), None),
                ]
                .map(|(weight, mut from, mut to)| {
                    if incoming {
                        core::mem::swap(&mut from, &mut to);
                    }
                    vec![
                        WireValue::Int(weight),
                        from.map_or(WireValue::Null, WireValue::Int),
                        to.map_or(WireValue::Null, WireValue::Int),
                        WireValue::Null,
                        WireValue::Null,
                    ]
                });
                assert_eq!(result.rows, expected, "{mode:?}: {pattern}");
                assert_eq!(result.outcome, Outcome::Rows { seq: 2 });
            }
        }
        oracle.close(cx).await.unwrap();
        for mode in [ExecuteMode::ReadStream, ExecuteMode::ReadOrdered] {
            let masked_predicate = reader
                .execute(
                    cx,
                    mode,
                    "MATCH (a)-[r:KNOWS]->(b) WHERE b.name = 'right' \
                 RETURN r.age,a.age,b.age ORDER BY r",
                    vec![],
                )
                .await
                .unwrap();
            assert!(masked_predicate.rows.is_empty());
            assert_eq!(masked_predicate.outcome, Outcome::Rows { seq: 2 });
        }

        let summary = "MATCH (a)-[r:KNOWS]->(b) RETURN count(*) AS rows,\
            sum(a.age) AS sources,sum(b.age) AS targets,avg(b.age) AS average,\
            count(b.age) AS present,count(b.name) AS hidden";
        let initial_summary = reader
            .execute(cx, ExecuteMode::ReadOrdered, summary, vec![])
            .await
            .unwrap();
        assert_eq!(
            initial_summary.rows,
            [vec![
                WireValue::Count(5),
                WireValue::WideInt(25),
                WireValue::WideInt(26),
                WireValue::Average {
                    numerator: 13,
                    denominator: 2
                },
                WireValue::Count(4),
                WireValue::Count(0)
            ]]
        );
        assert_eq!(initial_summary.outcome, Outcome::Rows { seq: 2 });

        let source = "MATCH (a)-[r:KNOWS]->(b) FOR SYSTEM_TIME AS OF SEQ $cut \
            WHERE a.age < b.age AND b.age >= $minimum AND r.age = 2 \
            RETURN r.age AS weight,a.age AS from_age,b.age AS to_age,\
            a.name AS from_hidden,b.name AS to_hidden ORDER BY r LIMIT $take";
        let ordered = "MATCH (a)-[r:KNOWS]->(b) FOR SYSTEM_TIME AS OF SEQ $cut \
            WHERE a.age < b.age AND b.age >= $minimum \
            RETURN r.age AS weight,a.age AS from_age,b.age AS to_age,\
            a.name AS from_hidden,b.name AS to_hidden ORDER BY weight DESC LIMIT $take";
        let historical_summary = "MATCH (a)-[r:KNOWS]->(b) FOR SYSTEM_TIME AS OF SEQ $cut \
            RETURN count(*) AS rows,sum(a.age) AS sources,sum(b.age) AS targets,\
            avg(b.age) AS average,count(b.age) AS present,count(b.name) AS hidden";
        let args = |cut, minimum, take| {
            vec![
                ("cut".into(), WireValue::Count(cut)),
                ("minimum".into(), WireValue::Int(minimum)),
                ("take".into(), WireValue::Count(take)),
            ]
        };
        let source_handle = reader
            .prepare_read(cx, source, args(2, 9, 1))
            .await
            .unwrap();
        let ordered_handle = reader
            .prepare_read(cx, ordered, args(2, 9, 2))
            .await
            .unwrap();
        let summary_handle = reader
            .prepare_read(
                cx,
                historical_summary,
                vec![("cut".into(), WireValue::Count(2))],
            )
            .await
            .unwrap();
        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (n:Person) WHERE n.age IS NOT NULL SET n.age = n.age + 100",
                vec![],
            )
            .await
            .unwrap();
        for (cut, shift) in [(2, 0_i64), (3, 100), (2, 0)] {
            for (mode, handle, weights) in [
                (PreparedReadMode::SourceStream, source_handle, vec![2]),
                (PreparedReadMode::OrderedStream, ordered_handle, vec![2, 1]),
            ] {
                let mut rows = Vec::new();
                let outcome = reader
                    .execute_prepared_streaming(
                        cx,
                        mode,
                        handle,
                        args(cut, 9 + shift, weights.len() as u64),
                        |columns| {
                            assert_eq!(
                                columns,
                                ["weight", "from_age", "to_age", "from_hidden", "to_hidden"]
                            );
                            Ok(())
                        },
                        |row| {
                            rows.push(row);
                            Ok(())
                        },
                    )
                    .await
                    .unwrap();
                let expected = weights
                    .into_iter()
                    .map(|weight| {
                        vec![
                            WireValue::Int(weight),
                            WireValue::Int(4 + shift),
                            WireValue::Int(9 + shift),
                            WireValue::Null,
                            WireValue::Null,
                        ]
                    })
                    .collect::<Vec<_>>();
                assert_eq!(rows, expected, "{mode:?}, cut {cut}");
                assert_eq!(outcome, Outcome::Rows { seq: cut });
            }
            let mut rows = Vec::new();
            let outcome = reader
                .execute_prepared_streaming(
                    cx,
                    PreparedReadMode::OrderedStream,
                    summary_handle,
                    vec![("cut".into(), WireValue::Count(cut))],
                    |columns| {
                        assert_eq!(
                            columns,
                            ["rows", "sources", "targets", "average", "present", "hidden"]
                        );
                        Ok(())
                    },
                    |row| {
                        rows.push(row);
                        Ok(())
                    },
                )
                .await
                .unwrap();
            let shift = i128::from(shift);
            assert_eq!(
                rows,
                [vec![
                    WireValue::Count(5),
                    WireValue::WideInt(25 + 5 * shift),
                    WireValue::WideInt(26 + 4 * shift),
                    WireValue::Average {
                        numerator: 13 + 2 * shift,
                        denominator: 2
                    },
                    WireValue::Count(4),
                    WireValue::Count(0)
                ]]
            );
            assert_eq!(outcome, Outcome::Rows { seq: cut });
        }
        for handle in [source_handle, ordered_handle, summary_handle] {
            reader.release_prepared(cx, handle).await.unwrap();
        }
        reader.close(cx).await.unwrap();

        scope.properties = Scope::only([]);
        let mut topology = Client::connect(cx, addr, token(&scope)).await.unwrap();
        topology.select(cx, "social").await.unwrap();
        for mode in [ExecuteMode::ReadStream, ExecuteMode::ReadOrdered] {
            let result = topology
                .execute(
                    cx,
                    mode,
                    "MATCH (a)-[r:KNOWS]->(b) RETURN r.age,a.age,b.age ORDER BY r",
                    vec![],
                )
                .await
                .unwrap();
            assert_eq!(result.rows, vec![vec![WireValue::Null; 3]; 5]);
            assert_eq!(result.outcome, Outcome::Rows { seq: 3 });
        }
        topology.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
        super::ordered_stream::empty_scratch(&directory).await;
    });
}

#[test]
fn buffered_endpoint_fields_release_owners_after_node_row_and_cancel_stops() {
    run(async |cx| {
        let (addr, shutdown, mut task, directory) =
            start_buffered_configured(cx, "buffered-endpoint-lifetimes", true).await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed_endpoint_edges(cx, &mut owner).await;
        let statement = "MATCH (a)-[r:KNOWS]->(b) RETURN r.age AS weight,\
            a.age AS from_age,b.age AS to_age,b.name AS hidden ORDER BY r";
        let mut scope = reader_grant();
        scope.limits.max_nodes = 8;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        for (mode, expected_prefix) in [(ExecuteMode::ReadStream, 4), (ExecuteMode::ReadOrdered, 0)]
        {
            let mut rows = Vec::new();
            let error = reader
                .execute_streaming(
                    cx,
                    mode,
                    statement,
                    vec![],
                    |_| Ok(()),
                    |row| {
                        rows.push(row);
                        Ok(())
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(server_code(error), ErrorCode::Budget, "{mode:?}");
            assert_eq!(rows.len(), expected_prefix, "{mode:?}");
            reader.ping(cx, 640).await.unwrap();
            super::ordered_stream::empty_scratch(&directory).await;
            let reused = reader
                .execute(
                    cx,
                    mode,
                    "MATCH (n:Person) WHERE n.age = 4 RETURN n.age AS age ORDER BY n LIMIT 1",
                    vec![],
                )
                .await
                .unwrap();
            assert_eq!(reused.rows, [vec![WireValue::Int(4)]]);
            assert_eq!(reused.outcome, Outcome::Rows { seq: 2 });
        }
        reader.close(cx).await.unwrap();

        scope.limits.max_nodes = 9;
        scope.limits.max_rows = 2;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let args = |take| vec![("take".into(), WireValue::Count(take))];
        let handle = reader
            .prepare_read(cx, &format!("{statement} LIMIT $take"), args(5))
            .await
            .unwrap();
        for mode in [
            PreparedReadMode::SourceStream,
            PreparedReadMode::OrderedStream,
        ] {
            let mut callbacks = 0;
            let error = reader
                .execute_prepared_streaming(
                    cx,
                    mode,
                    handle,
                    args(5),
                    |_| Ok(()),
                    |row| {
                        callbacks += 1;
                        assert_eq!(row.len(), 4);
                        assert_eq!(row[3], WireValue::Null);
                        Err(ClientError::Protocol("cancel buffered endpoint fields"))
                    },
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                ClientError::Protocol("cancel buffered endpoint fields")
            ));
            assert_eq!(callbacks, 1);
            reader.ping(cx, 641).await.unwrap();
            super::ordered_stream::empty_scratch(&directory).await;

            let mut prefix = Vec::new();
            let error = reader
                .execute_prepared_streaming(
                    cx,
                    mode,
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
            assert_eq!(prefix.len(), 2);
            reader.ping(cx, 642).await.unwrap();
            super::ordered_stream::empty_scratch(&directory).await;
            let mut reused = Vec::new();
            let outcome = reader
                .execute_prepared_streaming(
                    cx,
                    mode,
                    handle,
                    args(2),
                    |_| Ok(()),
                    |row| {
                        reused.push(row);
                        Ok(())
                    },
                )
                .await
                .unwrap();
            assert_eq!(reused, prefix);
            assert_eq!(outcome, Outcome::Rows { seq: 2 });
        }
        reader.release_prepared(cx, handle).await.unwrap();
        reader.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
        super::ordered_stream::empty_scratch(&directory).await;
    });
}

#[test]
fn buffered_text_and_prepared_streams_preserve_masking_history_and_typed_edges() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start_buffered(cx, "buffered-wire-history").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed_people(cx, &mut owner).await;
        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (a:Person {age:7,name:'left'}), (b:Person {age:8,name:'right'}), \
             (h:Company {age:999,name:'hidden'}), \
             (a)-[:KNOWS {age:3,name:'private-left'}]->(b), \
             (a)-[:KNOWS {age:4,name:'private-right'}]->(b), \
             (a)-[:WORKS_AT {age:999}]->(b), (a)-[:KNOWS {age:999}]->(h)",
                vec![],
            )
            .await
            .unwrap();
        let mut scope = reader_grant();
        scope.limits.max_nodes = 14;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let query = "MATCH (n) RETURN n.age AS age, n.name AS hidden ORDER BY n";
        let visible = reader
            .execute(cx, ExecuteMode::ReadStream, query, vec![])
            .await
            .unwrap();
        assert_eq!(visible.columns, ["age", "hidden"]);
        assert_eq!(
            ages(&visible.rows),
            [0, 1, 2, 3, 4, 5, 6, 7, 7, 8, 8, 9, 10, 11]
        );
        assert_eq!(visible.outcome, Outcome::Rows { seq: 3 });
        let edges = reader
            .execute(
                cx,
                ExecuteMode::ReadStream,
                "MATCH (a)-[r:KNOWS]->(b) RETURN r.age AS weight, r.name AS hidden ORDER BY r",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(edges.columns, ["weight", "hidden"]);
        assert_eq!(ages(&edges.rows), [3, 4]);
        assert_eq!(edges.outcome, Outcome::Rows { seq: 3 });

        let statement = "MATCH (n:Person) FOR SYSTEM_TIME AS OF SEQ $cut \
            WHERE n.age >= $minimum RETURN n.age AS age, n.name AS hidden ORDER BY n";
        let args = |cut, minimum| {
            vec![
                ("cut".into(), WireValue::Count(cut)),
                ("minimum".into(), WireValue::Int(minimum)),
            ]
        };
        let handle = reader
            .prepare_read(cx, statement, args(3, 9))
            .await
            .unwrap();
        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (n:Person) SET n.age = n.age + 100",
                vec![],
            )
            .await
            .unwrap();
        for (cut, minimum, expected) in [
            (3, 9, vec![9, 10, 11]),
            (4, 109, vec![109, 110, 111]),
            (3, 9, vec![9, 10, 11]),
        ] {
            let mut columns = Vec::new();
            let mut rows = Vec::new();
            let outcome = reader
                .execute_prepared_streaming(
                    cx,
                    PreparedReadMode::SourceStream,
                    handle,
                    args(cut, minimum),
                    |names| {
                        columns = names.to_vec();
                        Ok(())
                    },
                    |row| {
                        rows.push(row);
                        Ok(())
                    },
                )
                .await
                .unwrap();
            assert_eq!(columns, ["age", "hidden"]);
            assert_eq!(ages(&rows), expected);
            assert_eq!(outcome, Outcome::Rows { seq: cut });
        }
        reader.release_prepared(cx, handle).await.unwrap();
        reader.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn buffered_any_and_finite_relation_streams_keep_scopes_and_prepared_history() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start_buffered(cx, "buffered-wire-relations").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (a:Person {age:1}), (b:Person {age:2}), (h:Company {age:999}), \
             (a)-[:KNOWS {age:3,name:'hidden-3'}]->(b), \
             (a)-[:KNOWS {age:4,name:'hidden-4'}]->(b), \
             (a)-[:KNOWS {age:5,name:'hidden-5'}]->(a), \
             (b)-[:LIKES {age:7,name:'hidden-7'}]->(a), \
             (a)-[:WORKS_AT {age:999}]->(b), (a)-[:LIKES {age:999}]->(h)",
                vec![],
            )
            .await
            .unwrap();
        let mut scope = reader_grant();
        scope.relations = Scope::only([RelationId(1), RelationId(3)]);
        scope.limits.max_nodes = 7;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let any = "MATCH (a)-[r]->(b) RETURN r.age AS weight,r.name AS hidden ORDER BY r";
        let rows = reader
            .execute(cx, ExecuteMode::ReadStream, any, vec![])
            .await
            .unwrap();
        assert_eq!(rows.columns, ["weight", "hidden"]);
        assert_eq!(ages(&rows.rows), [3, 4, 5, 7]);
        assert_eq!(rows.outcome, Outcome::Rows { seq: 2 });
        let finite = "MATCH (a)-[r:KNOWS|WORKS_AT]->(b) \
            FOR SYSTEM_TIME AS OF SEQ $cut RETURN r.age AS weight,r.name AS hidden ORDER BY r";
        let args = |cut| vec![("cut".into(), WireValue::Count(cut))];
        let prepared = reader.prepare_read(cx, finite, args(2)).await.unwrap();
        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (a)-[r:KNOWS]->(b) SET r.age = r.age + 10",
                vec![],
            )
            .await
            .unwrap();
        for (cut, expected) in [
            (2, vec![3, 4, 5]),
            (3, vec![13, 14, 15]),
            (2, vec![3, 4, 5]),
        ] {
            let mut rows = Vec::new();
            let outcome = reader
                .execute_prepared_streaming(
                    cx,
                    PreparedReadMode::SourceStream,
                    prepared,
                    args(cut),
                    |columns| {
                        assert_eq!(columns, ["weight", "hidden"]);
                        Ok(())
                    },
                    |row| {
                        rows.push(row);
                        Ok(())
                    },
                )
                .await
                .unwrap();
            assert_eq!(ages(&rows), expected);
            assert_eq!(outcome, Outcome::Rows { seq: cut });
        }
        reader.release_prepared(cx, prepared).await.unwrap();
        reader.close(cx).await.unwrap();

        scope.relations = Scope::only([RelationId(3)]);
        scope.limits.max_nodes = 0;
        let mut denied = Client::connect(cx, addr, token(&scope)).await.unwrap();
        denied.select(cx, "social").await.unwrap();
        assert!(
            denied
                .execute(
                    cx,
                    ExecuteMode::ReadStream,
                    "MATCH (a)-[r:KNOWS|WORKS_AT]->(b) RETURN r.age ORDER BY r",
                    vec![],
                )
                .await
                .unwrap()
                .rows
                .is_empty()
        );
        assert_eq!(
            server_code(
                denied
                    .execute(cx, ExecuteMode::ReadStream, any, vec![])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Budget
        );
        denied.ping(cx, 613).await.unwrap();
        denied.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn buffered_credit_cancellation_budget_and_refusal_release_the_only_source_slot() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start_buffered(cx, "buffered-wire-cancel").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed_people(cx, &mut owner).await;
        let statement = "MATCH (n:Person) RETURN n.age AS age ORDER BY n";
        let mut scope = reader_grant();
        scope.limits.max_nodes = 2;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let mut delivered = 0;
        let error = reader
            .execute_streaming(
                cx,
                ExecuteMode::ReadStream,
                statement,
                vec![],
                |_| Ok(()),
                |_| {
                    delivered += 1;
                    Err(ClientError::Protocol("cancel buffered source"))
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ClientError::Protocol("cancel buffered source")
        ));
        assert_eq!(delivered, 1);
        reader.ping(cx, 610).await.unwrap();
        assert_eq!(
            reader
                .execute(
                    cx,
                    ExecuteMode::ReadStream,
                    "MATCH (n:Person) RETURN n.age AS age ORDER BY n LIMIT 1",
                    vec![],
                )
                .await
                .unwrap()
                .rows
                .len(),
            1
        );
        reader.close(cx).await.unwrap();

        scope.limits.max_nodes = 100;
        scope.limits.max_rows = 2;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let mut prefix = Vec::new();
        let error = reader
            .execute_streaming(
                cx,
                ExecuteMode::ReadStream,
                statement,
                vec![],
                |_| Ok(()),
                |row| {
                    prefix.push(row);
                    Ok(())
                },
            )
            .await
            .unwrap_err();
        assert_eq!(server_code(error), ErrorCode::Budget);
        assert_eq!(prefix.len(), 2);
        reader.ping(cx, 611).await.unwrap();
        assert_eq!(
            reader
                .execute(
                    cx,
                    ExecuteMode::ReadStream,
                    "MATCH (n:Person) RETURN n.age AS age ORDER BY n LIMIT 1",
                    vec![],
                )
                .await
                .unwrap()
                .rows
                .len(),
            1
        );
        reader.close(cx).await.unwrap();

        scope.limits.max_nodes = 0;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        // A denied relation opens no candidate scan, and unsupported shape
        // admission wins before the signed zero-node candidate budget.
        assert!(
            reader
                .execute(
                    cx,
                    ExecuteMode::ReadStream,
                    "MATCH (a)-[r:WORKS_AT]->(b) RETURN r.age AS age ORDER BY r",
                    vec![],
                )
                .await
                .unwrap()
                .rows
                .is_empty()
        );
        for unsupported in [
            "MATCH (n:Person) RETURN count(*) AS n",
            // Without an explicit identity order the scalar-only result
            // needs a blocking order. The configured source does not sort.
            "MATCH (n:Person) RETURN n.age AS age LIMIT 1",
            // Selected endpoint fields do not admit full property maps or
            // hidden carried graph reads, even with no requested rows.
            "MATCH (a)-[r:KNOWS]->(b) RETURN r,properties(b) AS fields ORDER BY r LIMIT 0",
            "MATCH (a)-[r:KNOWS]->(b) WITH r,b AS item \
             RETURN r,properties([item][0]) AS fields ORDER BY r LIMIT 0",
        ] {
            assert_eq!(
                server_code(
                    reader
                        .execute(cx, ExecuteMode::ReadStream, unsupported, vec![])
                        .await
                        .unwrap_err()
                ),
                ErrorCode::Statement,
                "{unsupported}"
            );
        }
        assert_eq!(
            server_code(
                reader
                    .execute(cx, ExecuteMode::ReadStream, statement, vec![],)
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Budget
        );
        reader.ping(cx, 612).await.unwrap();
        reader.close(cx).await.unwrap();

        let mut writer = Client::connect(cx, addr, token(&grant(Rights::Write)))
            .await
            .unwrap();
        writer.select(cx, "social").await.unwrap();
        assert_eq!(
            server_code(
                writer
                    .execute(
                        cx,
                        ExecuteMode::ReadStream,
                        "MATCH (n) RETURN count(*)",
                        vec![],
                    )
                    .await
                    .unwrap_err()
            ),
            ErrorCode::PermissionDenied
        );
        writer.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn buffered_ordered_rows_groups_and_prepared_history_keep_scoped_exact_results() {
    run(async |cx| {
        let (addr, shutdown, mut task, directory) =
            start_buffered_configured(cx, "buffered-ordered-results", true).await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed_people(cx, &mut owner).await;
        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (a:Person {age:7,name:'left'}), (b:Person {age:8,name:'right'}), \
                 (h:Company {age:999,name:'hidden'}), \
                 (a)-[:KNOWS {age:3,name:'hidden-3'}]->(b), \
                 (a)-[:KNOWS {age:4,name:'hidden-4'}]->(b), \
                 (a)-[:KNOWS {age:5,name:'hidden-5'}]->(a), \
                 (b)-[:LIKES {age:6,name:'hidden-6'}]->(a), \
                 (a)-[:WORKS_AT {age:999}]->(b), (a)-[:LIKES {age:999}]->(h)",
                vec![],
            )
            .await
            .unwrap();
        let mut scope = reader_grant();
        scope.relations = Scope::only([RelationId(1), RelationId(3)]);
        scope.limits.max_nodes = 14;
        scope.limits.max_rows = 4;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();

        // Four delivered rows cross the two-row credit window, while fourteen
        // visible inputs cross seven spill runs. Hidden labels cannot spend
        // the exact node allowance, and hidden names cannot order the page.
        let page = reader
            .execute(
                cx,
                ExecuteMode::ReadOrdered,
                "MATCH (n) RETURN n.age AS age,n.name AS hidden \
                 ORDER BY n.name DESC,age DESC SKIP 2 LIMIT 4",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(page.columns, ["age", "hidden"]);
        assert_eq!(
            page.rows,
            [9, 8, 8, 7].map(|age| vec![WireValue::Int(age), WireValue::Null])
        );
        assert_eq!(page.outcome, Outcome::Rows { seq: 3 });
        let grouped = reader
            .execute(
                cx,
                ExecuteMode::ReadOrdered,
                "MATCH (n) RETURN n.age AS age,count(*) AS rows GROUP BY n.age \
                 ORDER BY rows DESC,age ASC SKIP 1 LIMIT 3",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(grouped.columns, ["age", "rows"]);
        assert_eq!(
            grouped.rows,
            [
                vec![WireValue::Int(8), WireValue::Count(2)],
                vec![WireValue::Int(0), WireValue::Count(1)],
                vec![WireValue::Int(1), WireValue::Count(1)],
            ]
        );
        assert_eq!(grouped.outcome, Outcome::Rows { seq: 3 });
        let summary = reader
            .execute(
                cx,
                ExecuteMode::ReadOrdered,
                "MATCH (n) RETURN count(*) AS rows,sum(n.age) AS total, \
                 avg(n.age) AS average,count(n.name) AS hidden",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(summary.columns, ["rows", "total", "average", "hidden"]);
        assert_eq!(
            summary.rows,
            [vec![
                WireValue::Count(14),
                WireValue::WideInt(81),
                WireValue::Average {
                    numerator: 81,
                    denominator: 14,
                },
                WireValue::Count(0),
            ]]
        );
        assert_eq!(summary.outcome, Outcome::Rows { seq: 3 });
        let collected = reader
            .execute(
                cx,
                ExecuteMode::ReadOrdered,
                "UNWIND [3,1,3,2] AS value RETURN count(*) AS rows, \
                 collect(value) AS items,collect(DISTINCT value) AS unique_items",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(
            collected.rows,
            [vec![
                WireValue::Count(4),
                WireValue::List([3, 1, 3, 2].map(WireValue::Int).to_vec()),
                WireValue::List([3, 1, 2].map(WireValue::Int).to_vec()),
            ]]
        );
        for (pattern, expected) in [
            ("(a)-[r]->(b)", vec![6, 5, 4, 3]),
            ("(a)-[r:KNOWS|WORKS_AT]->(b)", vec![5, 4, 3]),
        ] {
            let edges = reader
                .execute(
                    cx,
                    ExecuteMode::ReadOrdered,
                    &format!(
                        "MATCH {pattern} RETURN r.age AS age,r.name AS hidden ORDER BY age DESC"
                    ),
                    vec![],
                )
                .await
                .unwrap();
            assert_eq!(
                edges.rows,
                expected
                    .into_iter()
                    .map(|age| vec![WireValue::Int(age), WireValue::Null])
                    .collect::<Vec<_>>()
            );
            assert_eq!(edges.outcome, Outcome::Rows { seq: 3 });
        }

        // Topology needs no property authority. The exact per-edge allowance
        // proves both the finite selector and capability mask precede charging.
        for (pattern, nodes, expected) in [
            ("(a)-[r]->(b)", 7, 4),
            ("(a)-[r:KNOWS|WORKS_AT]->(b)", 5, 3),
        ] {
            let mut topology_scope = scope.clone();
            topology_scope.properties = Scope::only([]);
            topology_scope.limits.max_nodes = nodes;
            topology_scope.limits.max_rows = 1;
            let mut topology = Client::connect(cx, addr, token(&topology_scope))
                .await
                .unwrap();
            topology.select(cx, "social").await.unwrap();
            let result = topology
                .execute(
                    cx,
                    ExecuteMode::ReadOrdered,
                    &format!("MATCH {pattern} RETURN count(*) AS rows"),
                    vec![],
                )
                .await
                .unwrap();
            assert_eq!(result.rows, [vec![WireValue::Count(expected)]]);
            assert_eq!(result.outcome, Outcome::Rows { seq: 3 });
            topology.close(cx).await.unwrap();
        }

        let historical = "MATCH (n:Person) FOR SYSTEM_TIME AS OF SEQ $cut \
            WHERE n.age >= $minimum RETURN n.age AS age,n.name AS hidden \
            ORDER BY age DESC LIMIT $take";
        let arguments = |cut, minimum| {
            vec![
                ("cut".into(), WireValue::Count(cut)),
                ("minimum".into(), WireValue::Int(minimum)),
                ("take".into(), WireValue::Count(4)),
            ]
        };
        let handle = reader
            .prepare_read(cx, historical, arguments(3, 7))
            .await
            .unwrap();
        let historical_summary = "MATCH (n:Person) FOR SYSTEM_TIME AS OF SEQ $cut \
            RETURN count(*) AS rows,sum(n.age) AS total,avg(n.age) AS average";
        let summary_handle = reader
            .prepare_read(
                cx,
                historical_summary,
                vec![("cut".into(), WireValue::Count(3))],
            )
            .await
            .unwrap();
        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (n:Person) SET n.age = n.age + 100",
                vec![],
            )
            .await
            .unwrap();
        for (cut, minimum, total, expected) in [
            (3, 7, 81, [11, 10, 9, 8]),
            (4, 107, 1481, [111, 110, 109, 108]),
            (3, 7, 81, [11, 10, 9, 8]),
        ] {
            let mut columns = Vec::new();
            let mut rows = Vec::new();
            let outcome = reader
                .execute_prepared_streaming(
                    cx,
                    PreparedReadMode::OrderedStream,
                    handle,
                    arguments(cut, minimum),
                    |names| {
                        columns = names.to_vec();
                        Ok(())
                    },
                    |row| {
                        rows.push(row);
                        Ok(())
                    },
                )
                .await
                .unwrap();
            assert_eq!(columns, ["age", "hidden"]);
            assert_eq!(
                rows,
                expected.map(|age| vec![WireValue::Int(age), WireValue::Null])
            );
            assert_eq!(outcome, Outcome::Rows { seq: cut });
            let mut columns = Vec::new();
            let mut rows = Vec::new();
            let outcome = reader
                .execute_prepared_streaming(
                    cx,
                    PreparedReadMode::OrderedStream,
                    summary_handle,
                    vec![("cut".into(), WireValue::Count(cut))],
                    |names| {
                        columns = names.to_vec();
                        Ok(())
                    },
                    |row| {
                        rows.push(row);
                        Ok(())
                    },
                )
                .await
                .unwrap();
            assert_eq!(columns, ["rows", "total", "average"]);
            assert_eq!(
                rows,
                [vec![
                    WireValue::Count(14),
                    WireValue::WideInt(total),
                    WireValue::Average {
                        numerator: total,
                        denominator: 14,
                    },
                ]]
            );
            assert_eq!(outcome, Outcome::Rows { seq: cut });
        }
        reader.release_prepared(cx, handle).await.unwrap();
        reader.release_prepared(cx, summary_handle).await.unwrap();
        reader.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
        super::ordered_stream::empty_scratch(&directory).await;
    });
}

#[test]
fn buffered_ordered_credit_cancel_and_refusal_release_cold_and_scratch_owners() {
    run(async |cx| {
        let (addr, shutdown, mut task, directory) =
            start_buffered_configured(cx, "buffered-ordered-lifetimes", true).await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed_people(cx, &mut owner).await;
        let mut scope = reader_grant();
        scope.limits.max_nodes = 12;
        scope.limits.max_rows = 2;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let statement = "MATCH (n:Person) RETURN n.age AS age ORDER BY age DESC";
        let mut rows = Vec::new();
        let error = reader
            .execute_streaming(
                cx,
                ExecuteMode::ReadOrdered,
                statement,
                vec![],
                |_| Ok(()),
                |row| {
                    rows.push(row);
                    Ok(())
                },
            )
            .await
            .unwrap_err();
        assert_eq!(server_code(error), ErrorCode::Budget);
        assert_eq!(rows, [vec![WireValue::Int(11)], vec![WireValue::Int(10)]]);
        reader.ping(cx, 630).await.unwrap();
        super::ordered_stream::empty_scratch(&directory).await;
        assert_eq!(
            reader
                .execute(
                    cx,
                    ExecuteMode::ReadOrdered,
                    &format!("{statement} LIMIT 2"),
                    vec![],
                )
                .await
                .unwrap()
                .rows,
            rows
        );

        let mut callbacks = 0;
        let error = reader
            .execute_streaming(
                cx,
                ExecuteMode::ReadOrdered,
                statement,
                vec![],
                |_| Err(ClientError::Protocol("cancel cold ordered schema")),
                |_| {
                    callbacks += 1;
                    Ok(())
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ClientError::Protocol("cancel cold ordered schema")
        ));
        assert_eq!(callbacks, 0);
        reader.ping(cx, 631).await.unwrap();
        super::ordered_stream::empty_scratch(&directory).await;

        let prepared = format!("{statement} LIMIT $take");
        let handle = reader
            .prepare_read(cx, &prepared, vec![("take".into(), WireValue::Count(6))])
            .await
            .unwrap();
        let error = reader
            .execute_prepared_streaming(
                cx,
                PreparedReadMode::OrderedStream,
                handle,
                vec![("take".into(), WireValue::Count(6))],
                |_| Ok(()),
                |row| {
                    callbacks += 1;
                    assert_eq!(row, vec![WireValue::Int(11)]);
                    Err(ClientError::Protocol("cancel cold ordered row"))
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ClientError::Protocol("cancel cold ordered row")
        ));
        assert_eq!(callbacks, 1);
        reader.ping(cx, 632).await.unwrap();
        super::ordered_stream::empty_scratch(&directory).await;
        let mut rebound = Vec::new();
        assert_eq!(
            reader
                .execute_prepared_streaming(
                    cx,
                    PreparedReadMode::OrderedStream,
                    handle,
                    vec![("take".into(), WireValue::Count(2))],
                    |_| Ok(()),
                    |row| {
                        rebound.push(row);
                        Ok(())
                    },
                )
                .await
                .unwrap(),
            Outcome::Rows { seq: 2 }
        );
        assert_eq!(rebound, rows);
        reader.release_prepared(cx, handle).await.unwrap();

        // The ordinary resident path accepts these statements. Cold ordering
        // must preserve recursive native admission even with no final rows,
        // rather than silently switching to a resident execution owner.
        for unsupported in [
            "MATCH (n:Person) RETURN n,properties(n) AS fields LIMIT 0",
            "MATCH (n:Person) WITH n AS item RETURN properties([item][0]) AS fields LIMIT 0",
            "MATCH (a)-[r:KNOWS]->(b)-[s:KNOWS]->(c) RETURN a,r,b,s,c LIMIT 0",
        ] {
            assert!(
                reader
                    .execute(cx, ExecuteMode::Read, unsupported, vec![])
                    .await
                    .unwrap()
                    .rows
                    .is_empty(),
                "resident statement: {unsupported}"
            );
            let mut schemas = 0;
            let mut delivered = 0;
            let error = reader
                .execute_streaming(
                    cx,
                    ExecuteMode::ReadOrdered,
                    unsupported,
                    vec![],
                    |_| {
                        schemas += 1;
                        Ok(())
                    },
                    |_| {
                        delivered += 1;
                        Ok(())
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(server_code(error), ErrorCode::Statement, "{unsupported}");
            assert_eq!((schemas, delivered), (0, 0), "{unsupported}");
            reader.ping(cx, 633).await.unwrap();
            super::ordered_stream::empty_scratch(&directory).await;
        }
        assert_eq!(
            reader
                .execute(
                    cx,
                    ExecuteMode::ReadOrdered,
                    &format!("{statement} LIMIT 2"),
                    vec![],
                )
                .await
                .unwrap()
                .rows,
            rows
        );
        reader.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
        super::ordered_stream::empty_scratch(&directory).await;
    });
}

#[test]
fn buffered_server_configuration_rejects_process_local_symbol_overlays() {
    run(async |cx| {
        let path = scratch("buffered-symbol-overlay");
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        drop(
            Database::create(&contexts.commit(), &path, keys())
                .await
                .unwrap(),
        );
        let mut server = Server::new(cx, ServerLimits::default()).unwrap();
        let mut config = DatabaseConfig::new("social", keys(), issuer_key());
        config.symbols = symbols();
        config.buffered_reads = Some(fgdb_server::BufferedReadConfig::new(64 * 1024 * 1024));
        assert!(matches!(
            server.open_database(cx, &path, config).await,
            Err(fgdb_server::ServerError::BufferedSymbols)
        ));
    });
}
