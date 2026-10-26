//! Host-admitted external ordering over real FGP flow-control windows.
use super::*;
use fgdb_server::OrderedReadConfig;

pub(super) async fn start_ordered(
    cx: &Cx,
    name: &str,
    max_input_rows: u64,
) -> (
    SocketAddr,
    fgdb_server::Shutdown,
    asupersync::runtime::TaskHandle<()>,
    PathBuf,
) {
    let path = scratch(name);
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    drop(
        Database::create(&contexts.commit(), &path, keys())
            .await
            .unwrap(),
    );
    let directory = path.join("query-scratch");
    asupersync::fs::create_dir(&directory).await.unwrap();
    let mut order = OrderedReadConfig::new(&directory);
    order.memory_bytes = 64 * 1024;
    order.sort.run_rows = 2;
    order.sort.page_bytes = 31;
    order.sort.max_row_bytes = 4096;
    order.sort.max_sort_work = 1_000_000;
    order.sort.max_input_rows = max_input_rows;
    let mut config = DatabaseConfig::new("social", keys(), issuer_key());
    config.symbols = symbols();
    config.ordered_reads = Some(order);
    let mut server = Server::new(
        cx,
        ServerLimits {
            max_frame_len: 4096,
            initial_window_bytes: 8192,
            initial_window_rows: 2,
            max_window_bytes: 1 << 20,
            max_window_rows: 1 << 10,
            max_connections: 8,
        },
    )
    .unwrap();
    server.open_database(cx, &path, config).await.unwrap();
    let server = Arc::new(server);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = server.shutdown();
    let task = cx
        .spawn(move |child| async move {
            server.serve(&child, listener).await.unwrap();
        })
        .unwrap();
    (addr, shutdown, task, directory)
}

async fn seed(cx: &Cx, owner: &mut Client) {
    owner
        .execute_batch(
            cx,
            "CREATE (:Person {name:$name,age:$age})",
            (0..12)
                .map(|index| {
                    vec![
                        ("name".into(), text(&format!("person-{:02}", 11 - index))),
                        ("age".into(), WireValue::Int(index % 6)),
                    ]
                })
                .collect(),
        )
        .await
        .unwrap();
}

pub(super) async fn empty_scratch(directory: &std::path::Path) {
    assert!(
        asupersync::fs::read_dir(directory)
            .await
            .unwrap()
            .next_entry()
            .await
            .unwrap()
            .is_none()
    );
}

#[test]
fn external_order_preserves_masked_keys_distinct_prepared_history_and_join_bags() {
    run(async |cx| {
        let (addr, shutdown, mut task, directory) =
            start_ordered(cx, "external-order-results", 100).await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed(cx, &mut owner).await;
        let mut scope = grant(Rights::Read);
        scope.labels = Scope::only([LabelId(1)]);
        scope.relations = Scope::only([RelationId(1)]);
        scope.properties = Scope::only([PropertyKeyId(2)]);
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();

        let ordered = "MATCH (n:Person) RETURN n.age AS age ORDER BY n.name DESC, age ASC";
        let eager = reader
            .execute(cx, ExecuteMode::Read, ordered, vec![])
            .await
            .unwrap();
        let result = reader
            .execute(cx, ExecuteMode::ReadOrdered, ordered, vec![])
            .await
            .unwrap();
        assert_eq!(result, eager);
        assert_eq!(
            result.rows,
            (0..6)
                .flat_map(|age| [0, 1].map(|_| vec![WireValue::Int(age)]))
                .collect::<Vec<_>>()
        );
        // The primary sort key is hidden. Its original reverse-name order
        // must not survive masking, even through separate two-row spill runs.
        assert_eq!(result.outcome, Outcome::Rows { seq: 1 });
        let distinct =
            "MATCH (n:Person) RETURN DISTINCT n.age AS age ORDER BY age DESC SKIP 1 LIMIT 3";
        assert_eq!(
            reader
                .execute(cx, ExecuteMode::ReadOrdered, distinct, vec![])
                .await
                .unwrap()
                .rows,
            [
                vec![WireValue::Int(4)],
                vec![WireValue::Int(3)],
                vec![WireValue::Int(2)]
            ]
        );
        assert_eq!(
            server_code(
                reader
                    .execute(cx, ExecuteMode::ReadStream, ordered, vec![])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement
        );

        let historical = "MATCH (n:Person) FOR SYSTEM_TIME AS OF SEQ $cut WHERE n.age >= $minimum RETURN n.age AS age ORDER BY age DESC LIMIT $take";
        let parameters = vec![
            ("cut".into(), WireValue::Count(1)),
            ("minimum".into(), WireValue::Int(3)),
            ("take".into(), WireValue::Count(4)),
        ];
        let handle = reader
            .prepare_read(cx, historical, parameters.clone())
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
        let mut columns = Vec::new();
        let mut rows = Vec::new();
        let outcome = reader
            .execute_prepared_streaming(
                cx,
                PreparedReadMode::OrderedStream,
                handle,
                parameters,
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
        assert_eq!(columns, ["age"]);
        assert_eq!(
            rows,
            [
                vec![WireValue::Int(5)],
                vec![WireValue::Int(5)],
                vec![WireValue::Int(4)],
                vec![WireValue::Int(4)]
            ]
        );
        assert_eq!(outcome, Outcome::Rows { seq: 1 });
        let mut rebound = Vec::new();
        assert_eq!(
            reader
                .execute_prepared_streaming(
                    cx,
                    PreparedReadMode::OrderedStream,
                    handle,
                    vec![
                        ("cut".into(), WireValue::Count(2)),
                        ("minimum".into(), WireValue::Int(103)),
                        ("take".into(), WireValue::Count(2))
                    ],
                    |_| Ok(()),
                    |row| {
                        rebound.push(row);
                        Ok(())
                    }
                )
                .await
                .unwrap(),
            Outcome::Rows { seq: 2 }
        );
        assert_eq!(
            rebound,
            [vec![WireValue::Int(105)], vec![WireValue::Int(105)]]
        );

        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (a:Person {age:7}), (b:Person {age:8}), (c:Person {age:9}), \
             (a)-[:KNOWS {age:2}]->(b), (a)-[:KNOWS {age:1}]->(b), \
             (b)-[:KNOWS {age:3}]->(c), (a)-[:WORKS_AT {age:99}]->(b)",
                vec![],
            )
            .await
            .unwrap();
        let join = "MATCH (a:Person)-[r]->(b:Person)-[s]->(c:Person) RETURN a.age AS a, r.age AS r, b.age AS b, s.age AS s, c.age AS c ORDER BY r DESC";
        let eager = reader
            .execute(cx, ExecuteMode::Read, join, vec![])
            .await
            .unwrap();
        let result = reader
            .execute(cx, ExecuteMode::ReadOrdered, join, vec![])
            .await
            .unwrap();
        assert_eq!(result, eager);
        assert_eq!(
            result.rows,
            [
                vec![
                    WireValue::Int(7),
                    WireValue::Int(2),
                    WireValue::Int(8),
                    WireValue::Int(3),
                    WireValue::Int(9)
                ],
                vec![
                    WireValue::Int(7),
                    WireValue::Int(1),
                    WireValue::Int(8),
                    WireValue::Int(3),
                    WireValue::Int(9)
                ],
            ]
        );
        reader.release_prepared(cx, handle).await.unwrap();
        reader.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
        empty_scratch(&directory).await;
    });
}

#[test]
fn external_order_charges_delivered_rows_and_cancels_from_schema_or_row_callbacks() {
    run(async |cx| {
        let (addr, shutdown, mut task, directory) =
            start_ordered(cx, "external-order-delivery", 100).await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed(cx, &mut owner).await;
        let mut scope = grant(Rights::Read);
        scope.limits.max_rows = 2;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let statement = "MATCH (n:Person) RETURN n.age AS age ORDER BY age";
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
        assert_eq!(rows, [vec![WireValue::Int(0)], vec![WireValue::Int(0)]]);
        reader.ping(cx, 401).await.unwrap();
        assert_eq!(
            reader
                .execute(
                    cx,
                    ExecuteMode::ReadOrdered,
                    &format!("{statement} LIMIT 2"),
                    vec![]
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
                |_| Err(ClientError::Protocol("stop before sorted rows")),
                |_| {
                    callbacks += 1;
                    Ok(())
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ClientError::Protocol("stop before sorted rows")
        ));
        assert_eq!(callbacks, 0);
        reader.ping(cx, 402).await.unwrap();
        let error = reader
            .execute_streaming(
                cx,
                ExecuteMode::ReadOrdered,
                statement,
                vec![],
                |_| Ok(()),
                |_| {
                    callbacks += 1;
                    Err(ClientError::Protocol("stop after one sorted row"))
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ClientError::Protocol("stop after one sorted row")
        ));
        assert_eq!(callbacks, 1);
        reader.ping(cx, 403).await.unwrap();
        reader.close(cx).await.unwrap();

        scope.limits.max_work = 999_999; // Less than the host's prepaid 1m sort allowance.
        let mut limited = Client::connect(cx, addr, token(&scope)).await.unwrap();
        limited.select(cx, "social").await.unwrap();
        assert_eq!(
            server_code(
                limited
                    .execute(cx, ExecuteMode::ReadOrdered, statement, vec![])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Budget
        );
        limited.ping(cx, 404).await.unwrap();
        limited.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
        empty_scratch(&directory).await;
    });
}

#[test]
fn external_order_requires_host_opt_in_and_validates_all_input_with_limit_zero() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "external-order-disabled").await;
        let mut reader = Client::connect(cx, addr, token(&grant(Rights::Read)))
            .await
            .unwrap();
        reader.select(cx, "social").await.unwrap();
        let statement = "MATCH (n:Person) RETURN n.age AS age ORDER BY age LIMIT 0";
        assert_eq!(
            server_code(
                reader
                    .execute(cx, ExecuteMode::ReadOrdered, statement, vec![])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement
        );
        reader.ping(cx, 405).await.unwrap();
        reader.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();

        let (addr, shutdown, mut task, directory) =
            start_ordered(cx, "external-order-input-limit", 1).await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed(cx, &mut owner).await;
        let mut rows = 0;
        let error = owner
            .execute_streaming(
                cx,
                ExecuteMode::ReadOrdered,
                statement,
                vec![],
                |_| Ok(()),
                |_| {
                    rows += 1;
                    Ok(())
                },
            )
            .await
            .unwrap_err();
        assert_eq!(server_code(error), ErrorCode::Budget);
        assert_eq!(rows, 0);
        owner.ping(cx, 406).await.unwrap();
        // Unsupported ordered profiles refuse before LIMIT 0 can conceal the
        // missing source operator; no eager buffered execution is substituted.
        assert_eq!(
            server_code(
                owner
                    .execute(
                        cx,
                        ExecuteMode::ReadOrdered,
                        "MATCH (n:Person) RETURN count(*) AS n ORDER BY n LIMIT 0",
                        vec![]
                    )
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement
        );
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
        empty_scratch(&directory).await;
    });
}
