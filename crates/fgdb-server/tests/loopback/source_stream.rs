//! Native source streaming over actual flow-controlled loopback connections.
use super::*;

#[path = "source_projection.rs"]
mod source_projection;

#[path = "source_buffered_projection.rs"]
mod source_buffered_projection;

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
        // collect() follows the native input order (vertex identity, hashed
        // ids), which openCypher leaves to the input; the values are exact.
        let [row] = expressions.rows.as_slice() else {
            panic!("one aggregate row")
        };
        let WireValue::List(ages) = &row[0] else {
            panic!("a list of ages")
        };
        let mut ages: Vec<i64> = ages
            .iter()
            .map(|age| match age {
                WireValue::Int(age) => *age,
                other => panic!("an integer age, not {other:?}"),
            })
            .collect();
        ages.sort_unstable();
        assert_eq!(ages, [10, 11, 12]);
        assert_eq!(row[1], WireValue::Int(66));

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
        reader.ping(cx, 81).await.unwrap();
        let mut callbacks = 0;
        let error = reader
            .execute_streaming(
                cx,
                ExecuteMode::ReadStream,
                groups,
                vec![],
                |_| Ok(()),
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
            limited.ping(cx, 83).await.unwrap();
        }
        limited.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn aggregate_schema_cancellation_precedes_input_refusal_for_text_and_prepared_reads() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "aggregate-schema-cancel").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed(cx, &mut owner).await;
        let mut scope = grant(Rights::Read);
        scope.limits.max_nodes = 0;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let statement = "MATCH (n:Person) RETURN count(*) AS n";
        let prepared = reader.prepare_read(cx, statement, vec![]).await.unwrap();
        for use_prepared in [false, true] {
            let mut schemas = 0;
            let on_columns = |columns: &[String]| {
                schemas += 1;
                assert_eq!(columns, ["n"]);
                Err(ClientError::Protocol("cancel before aggregate input"))
            };
            let on_row = |_| -> Result<(), ClientError> {
                panic!("a zero-node grant cannot produce an aggregate row")
            };
            let error = if use_prepared {
                reader
                    .execute_prepared_streaming(
                        cx,
                        PreparedReadMode::SourceStream,
                        prepared,
                        vec![],
                        on_columns,
                        on_row,
                    )
                    .await
                    .unwrap_err()
            } else {
                reader
                    .execute_streaming(
                        cx,
                        ExecuteMode::ReadStream,
                        statement,
                        vec![],
                        on_columns,
                        on_row,
                    )
                    .await
                    .unwrap_err()
            };
            assert!(matches!(
                error,
                ClientError::Protocol("cancel before aggregate input")
            ));
            assert_eq!(schemas, 1);
            reader.ping(cx, 84).await.unwrap();
        }
        // Accepting the same metadata does not waive the input allowance.
        assert_eq!(
            server_code(
                reader
                    .execute(cx, ExecuteMode::ReadStream, statement, vec![])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Budget
        );
        reader.ping(cx, 85).await.unwrap();
        reader.close(cx).await.unwrap();
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
fn private_identity_keys_stream_scoped_properties_without_entering_schema_or_wire_rows() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "native-private-identity-keys").await;
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
        for (statement, columns, count) in [
            (
                "MATCH (n) RETURN n.age AS age, n.name AS hidden ORDER BY n",
                vec!["age", "hidden"],
                15,
            ),
            (
                "MATCH (a)-[r]->(b) RETURN r.age AS weight, a.age AS age, b.name AS hidden ORDER BY r",
                vec!["weight", "age", "hidden"],
                3,
            ),
            (
                "MATCH (a)-[r]->(b)-[s]->(c) RETURN r.age AS weight, s.age AS next_weight, c.name AS hidden ORDER BY r, s",
                vec!["weight", "next_weight", "hidden"],
                2,
            ),
            (
                "MATCH (a)-[r]-(b) RETURN r.age AS weight, a.age AS age, b.name AS hidden ORDER BY r, a",
                vec!["weight", "age", "hidden"],
                6,
            ),
        ] {
            let eager = reader
                .execute(cx, ExecuteMode::Read, statement, vec![])
                .await
                .unwrap();
            let streamed = reader
                .execute(cx, ExecuteMode::ReadStream, statement, vec![])
                .await
                .unwrap();
            assert_eq!(streamed, eager, "{statement}");
            assert_eq!(streamed.columns, columns, "{statement}");
            assert_eq!(streamed.rows.len(), count, "{statement}");
            assert!(streamed.rows.iter().all(|row| {
                row.len() == columns.len()
                    && row.last() == Some(&WireValue::Null)
                    && row
                        .iter()
                        .all(|value| !matches!(value, WireValue::Vertex(_) | WireValue::Edge(_)))
            }));
            assert_eq!(streamed.outcome, Outcome::Rows { seq: 2 });
        }
        reader.close(cx).await.unwrap();

        // This graph exceeds the signed node allowance. Delivering a private
        // key projection's first credit window therefore proves genuine source
        // pulls, and cancellation must release it before the next request.
        scope.limits.max_nodes = 4;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let mut callbacks = 0;
        let error = reader
            .execute_streaming(
                cx,
                ExecuteMode::ReadStream,
                "MATCH (n:Person) RETURN n.age AS age ORDER BY n",
                vec![],
                |columns| {
                    assert_eq!(columns, ["age"]);
                    Ok(())
                },
                |row| {
                    assert_eq!(row.len(), 1);
                    callbacks += 1;
                    Err(ClientError::Protocol("stop private identity stream"))
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ClientError::Protocol("stop private identity stream")
        ));
        assert_eq!(callbacks, 1);
        reader.ping(cx, 73).await.unwrap();
        let one = reader
            .execute(
                cx,
                ExecuteMode::ReadStream,
                "MATCH (n:Person) RETURN n.age AS age ORDER BY n LIMIT 1",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(one.columns, ["age"]);
        assert_eq!(one.rows.len(), 1);
        assert_eq!(one.rows[0].len(), 1);
        reader.close(cx).await.unwrap();
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
                |_| Ok(()),
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
                |_| Ok(()),
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

pub(super) async fn prepared_source(
    cx: &Cx,
    client: &mut Client,
    handle: fgdb_protocol::body::PreparedHandle,
    parameters: Vec<(String, WireValue)>,
) -> Result<fgdb_protocol::client::Answer, ClientError> {
    let mut columns = Vec::new();
    let mut rows = Vec::new();
    let outcome = client
        .execute_prepared_streaming(
            cx,
            PreparedReadMode::SourceStream,
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
        .await?;
    Ok(fgdb_protocol::client::Answer {
        columns,
        rows,
        outcome,
    })
}

/// A one-frame cold source behind the same four-row FGP credit window used
/// by the resident-source tests. Explicit read modes still use resident reads.
async fn start_buffered_sources(
    cx: &Cx,
    name: &str,
) -> (
    SocketAddr,
    fgdb_server::Shutdown,
    asupersync::runtime::TaskHandle<()>,
) {
    use fgdb_delta_types::catalog::{CatalogSymbol, SymbolCatalog};

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
    let mut server = Server::new(
        cx,
        ServerLimits {
            max_frame_len: 4096,
            initial_window_bytes: 8192,
            initial_window_rows: 4,
            max_window_bytes: 1 << 20,
            max_window_rows: 1 << 10,
            max_connections: 8,
        },
    )
    .unwrap();
    let mut config = DatabaseConfig::new("social", keys(), issuer_key());
    config.symbols = symbols();
    let mut buffered = fgdb_server::BufferedReadConfig::new(16 * 1024 * 1024);
    buffered.max_concurrent = 1;
    buffered.limits = fgdb::BufferedReadLimits {
        max_root_bytes: 64 * 1024,
        max_source_bytes: 4 * 1024 * 1024,
        max_blocks: 512,
        max_vertex_patches: 512,
        max_work: 1_000_000,
        buffer: fgdb::BufferLimits {
            max_frames: 1,
            max_ghost_entries: 2,
            max_extent_bytes: 16 * 1024,
        },
    };
    config.buffered_reads = Some(buffered);
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
    (addr, shutdown, handle)
}

#[test]
fn buffered_sources_preserve_prepared_pages_and_refuse_unsupported_profiles() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start_buffered_sources(cx, "buffered-source-pages").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed(cx, &mut owner).await;
        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (a:Person {age:0}), (b:Person {age:1}) \
                 CREATE (h:Company {age:99,name:'hidden'}), \
                 (a)-[:KNOWS {age:7,name:'masked'}]->(b), \
                 (a)-[:KNOWS {age:8,name:'masked'}]->(b), \
                 (a)-[:WORKS_AT {age:900}]->(b), \
                 (a)-[:KNOWS {age:900}]->(h)",
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
        let statement = "MATCH (n) RETURN n AS id,n.age AS age,n.name AS hidden";
        let visible = reader
            .execute(cx, ExecuteMode::ReadStream, statement, vec![])
            .await
            .unwrap();
        assert_eq!(
            visible,
            reader
                .execute(cx, ExecuteMode::Read, statement, vec![])
                .await
                .unwrap()
        );
        assert_eq!(visible.columns, ["id", "age", "hidden"]);
        assert_eq!(visible.outcome, Outcome::Rows { seq: 3 });
        assert!(visible.rows.iter().all(|row| row[2] == WireValue::Null));
        let mut ages: Vec<_> = visible
            .rows
            .iter()
            .map(|row| match row[1] {
                WireValue::Int(age) => age,
                ref other => panic!("expected an integer age, got {other:?}"),
            })
            .collect();
        ages.sort_unstable();
        assert_eq!(ages, (0..12).collect::<Vec<i64>>());

        let page = "AT BRANCH $route MATCH (n) WHERE n.age >= $min \
                    RETURN n AS id,n.age AS age,n.name AS hidden SKIP $skip LIMIT $take";
        let page_args = |minimum, skip, take, route: &str| {
            vec![
                ("min".into(), WireValue::Int(minimum)),
                ("skip".into(), WireValue::Count(skip)),
                ("take".into(), WireValue::Count(take)),
                ("route".into(), text(route)),
            ]
        };
        let handle = reader
            .prepare_read(cx, page, page_args(0, 0, 4, "main"))
            .await
            .unwrap();
        for (minimum, skip, take) in [(0, 0, 4), (4, 1, 5), (0, 12, 2), (10, 0, 0)] {
            let result = prepared_source(
                cx,
                &mut reader,
                handle,
                page_args(minimum, skip, take, "main"),
            )
            .await
            .unwrap();
            let expected: Vec<_> = visible
                .rows
                .iter()
                .filter(|row| matches!(row[1], WireValue::Int(age) if age >= minimum))
                .skip(usize::try_from(skip).unwrap())
                .take(usize::try_from(take).unwrap())
                .cloned()
                .collect();
            assert_eq!(result.columns, visible.columns);
            assert_eq!(result.rows, expected);
            assert_eq!(result.outcome, Outcome::Rows { seq: 3 });
            assert_eq!(
                result,
                reader
                    .execute(
                        cx,
                        ExecuteMode::ReadStream,
                        page,
                        page_args(minimum, skip, take, "main"),
                    )
                    .await
                    .unwrap()
            );
        }
        assert_eq!(
            server_code(
                prepared_source(cx, &mut reader, handle, page_args(0, 0, 4, "elsewhere"))
                    .await
                    .unwrap_err()
            ),
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            server_code(
                prepared_source(cx, &mut reader, handle, vec![])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement
        );
        assert_eq!(
            prepared_source(cx, &mut reader, handle, page_args(0, 0, 12, "main"))
                .await
                .unwrap()
                .rows,
            visible.rows
        );
        reader.release_prepared(cx, handle).await.unwrap();

        // Aggregates work on the resident source, so refusal here detects an
        // accidental fallback. Even a freshly bound LIMIT 0 keeps the profile
        // contract. The ordinary read modes on the same template still work.
        let aggregate = "MATCH (n) RETURN count(*) AS count LIMIT $take";
        let args = |take| vec![("take".into(), WireValue::Count(take))];
        let handle = reader.prepare_read(cx, aggregate, args(1)).await.unwrap();
        for take in [1, 0] {
            assert_eq!(
                server_code(
                    reader
                        .execute(cx, ExecuteMode::ReadStream, aggregate, args(take))
                        .await
                        .unwrap_err()
                ),
                ErrorCode::Statement
            );
            assert_eq!(
                server_code(
                    prepared_source(cx, &mut reader, handle, args(take))
                        .await
                        .unwrap_err()
                ),
                ErrorCode::Statement
            );
            let resident = reader
                .execute_prepared(cx, handle, args(take))
                .await
                .unwrap();
            assert_eq!(
                resident.rows,
                if take == 0 {
                    vec![]
                } else {
                    vec![vec![WireValue::Count(12)]]
                }
            );
            assert_eq!(
                resident,
                reader
                    .execute(cx, ExecuteMode::Read, aggregate, args(take))
                    .await
                    .unwrap()
            );
        }
        reader.release_prepared(cx, handle).await.unwrap();
        let any_relation = "MATCH (a)-[r]->(b) RETURN r AS id";
        let expected = owner
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN r AS id",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(expected.columns, ["id"]);
        assert_eq!(expected.outcome, Outcome::Rows { seq: 3 });
        assert_eq!(expected.rows.len(), 2);
        assert!(matches!((&expected.rows[0][..], &expected.rows[1][..]),
            ([WireValue::Edge(first)], [WireValue::Edge(second)]) if first != second));
        let handle = reader.prepare_read(cx, any_relation, vec![]).await.unwrap();
        for mode in [ExecuteMode::Read, ExecuteMode::ReadStream] {
            assert_eq!(
                reader
                    .execute(cx, mode, any_relation, vec![])
                    .await
                    .unwrap(),
                expected
            );
        }
        assert_eq!(
            reader.execute_prepared(cx, handle, vec![]).await.unwrap(),
            expected
        );
        assert_eq!(
            prepared_source(cx, &mut reader, handle, vec![])
                .await
                .unwrap(),
            expected
        );
        reader.release_prepared(cx, handle).await.unwrap();
        reader.ping(cx, 121).await.unwrap();
        reader.close(cx).await.unwrap();
        scope.relations = Scope::only([]);
        let mut denied = Client::connect(cx, addr, token(&scope)).await.unwrap();
        denied.select(cx, "social").await.unwrap();
        let empty = denied
            .execute(cx, ExecuteMode::ReadStream, any_relation, vec![])
            .await
            .unwrap();
        assert_eq!(empty.columns, expected.columns);
        assert_eq!(empty.outcome, expected.outcome);
        assert!(empty.rows.is_empty());
        denied.close(cx).await.unwrap();
        let all = owner
            .execute(cx, ExecuteMode::ReadStream, any_relation, vec![])
            .await
            .unwrap();
        assert_eq!(all.outcome, expected.outcome);
        assert_eq!(all.rows.len(), 4);
        assert!(expected.rows.iter().all(|row| all.rows.contains(row)));
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn buffered_prepared_sources_release_cancelled_and_budgeted_executions_before_reuse() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start_buffered_sources(cx, "buffered-source-limits").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed(cx, &mut owner).await;
        let statement = "MATCH (n) RETURN n AS id,n.age AS age,n.name AS hidden LIMIT $take";
        let args = |take| vec![("take".into(), WireValue::Count(take))];
        let mut scope = grant(Rights::Read);
        scope.labels = Scope::only([LabelId(1)]);
        scope.properties = Scope::only([PropertyKeyId(2)]);
        // A resident evaluation of all twelve rows cannot pass this allowance;
        // one source window can, and Cancel must stop before a second window.
        scope.limits.max_nodes = 4;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let handle = reader.prepare_read(cx, statement, args(100)).await.unwrap();
        let mut first = Vec::new();
        let error = reader
            .execute_prepared_streaming(
                cx,
                PreparedReadMode::SourceStream,
                handle,
                args(100),
                |_| Ok(()),
                |row| {
                    first.push(row);
                    Err(ClientError::Protocol("stop after one cold prepared row"))
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ClientError::Protocol("stop after one cold prepared row")
        ));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0][2], WireValue::Null);
        reader.ping(cx, 122).await.unwrap();
        assert_eq!(
            prepared_source(cx, &mut reader, handle, args(1))
                .await
                .unwrap()
                .rows,
            first
        );

        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (n:Person) SET n.age=n.age+100",
                vec![],
            )
            .await
            .unwrap();
        let WireValue::Int(age) = &mut first[0][1] else {
            panic!("an integer age")
        };
        *age += 100;
        let reused = prepared_source(cx, &mut reader, handle, args(1))
            .await
            .unwrap();
        assert_eq!(reused.rows, first);
        assert_eq!(reused.outcome, Outcome::Rows { seq: 3 });
        reader.release_prepared(cx, handle).await.unwrap();
        reader.close(cx).await.unwrap();

        scope.limits.max_nodes = 1_000;
        scope.limits.max_rows = 4;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let handle = reader.prepare_read(cx, statement, args(100)).await.unwrap();
        let mut prepared_prefix = Vec::new();
        let error = reader
            .execute_prepared_streaming(
                cx,
                PreparedReadMode::SourceStream,
                handle,
                args(100),
                |_| Ok(()),
                |row| {
                    prepared_prefix.push(row);
                    Ok(())
                },
            )
            .await
            .unwrap_err();
        assert_eq!(server_code(error), ErrorCode::Budget);
        assert_eq!(prepared_prefix.len(), 4);
        assert_eq!(&prepared_prefix[..1], first.as_slice());
        assert!(prepared_prefix.iter().all(|row| row[2] == WireValue::Null));
        reader.ping(cx, 123).await.unwrap();
        assert_eq!(
            prepared_source(cx, &mut reader, handle, args(1))
                .await
                .unwrap()
                .rows,
            first
        );
        reader.release_prepared(cx, handle).await.unwrap();
        reader.close(cx).await.unwrap();

        scope.limits.max_nodes = 0;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        let handle = reader.prepare_read(cx, statement, args(0)).await.unwrap();
        let empty = prepared_source(cx, &mut reader, handle, args(0))
            .await
            .unwrap();
        assert_eq!(empty.columns, ["id", "age", "hidden"]);
        assert!(empty.rows.is_empty());
        assert_eq!(empty.outcome, Outcome::Rows { seq: 3 });
        assert_eq!(
            empty,
            reader
                .execute(cx, ExecuteMode::ReadStream, statement, args(0))
                .await
                .unwrap()
        );
        reader.release_prepared(cx, handle).await.unwrap();
        reader.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn prepared_source_reads_preserve_scope_and_rebind_unsigned_historical_cuts() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "prepared-source-history").await;
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
        let rows_handle = reader.prepare_read(cx, ROWS, vec![]).await.unwrap();
        let rows = prepared_source(cx, &mut reader, rows_handle, vec![])
            .await
            .unwrap();
        assert_eq!(rows.rows.len(), 12);
        assert!(rows.rows.iter().all(|row| row[1] == WireValue::Null));
        assert_eq!(
            rows,
            reader
                .execute(cx, ExecuteMode::Read, ROWS, vec![])
                .await
                .unwrap()
        );
        reader.release_prepared(cx, rows_handle).await.unwrap();

        let args = |cut, limit, route: &str| {
            vec![
                ("cut".into(), WireValue::Count(cut)),
                ("limit".into(), WireValue::Count(limit)),
                ("route".into(), text(route)),
            ]
        };
        let statement = "AT BRANCH $route MATCH (n) FOR SYSTEM_TIME AS OF SEQ $cut RETURN count(*) AS n, sum(n.age) AS total, avg(n.age) AS average, count(n.name) AS hidden LIMIT $limit";
        let handle = reader
            .prepare_read(cx, statement, args(1, 1, "main"))
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
        // One retained template, all operands supplied afresh, exact old/new cuts.
        for (cut, sum, numerator) in [(1, 66, 11), (3, 1266, 211), (1, 66, 11)] {
            let result = prepared_source(cx, &mut reader, handle, args(cut, 1, "main"))
                .await
                .unwrap();
            assert_eq!(result.columns, ["n", "total", "average", "hidden"]);
            assert_eq!(
                result.rows,
                [vec![
                    WireValue::Count(12),
                    WireValue::WideInt(sum),
                    WireValue::Average {
                        numerator,
                        denominator: 2,
                    },
                    WireValue::Count(0),
                ]]
            );
            assert_eq!(result.outcome, Outcome::Rows { seq: cut });
        }
        let empty = prepared_source(cx, &mut reader, handle, args(1, 0, "main"))
            .await
            .unwrap();
        assert!(empty.rows.is_empty());
        assert_eq!(empty.outcome, Outcome::Rows { seq: 1 });
        assert_eq!(
            server_code(
                prepared_source(cx, &mut reader, handle, args(1, 1, "elsewhere"))
                    .await
                    .unwrap_err()
            ),
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            server_code(
                prepared_source(cx, &mut reader, handle, vec![])
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement
        );
        assert_eq!(
            prepared_source(cx, &mut reader, handle, args(1, 1, "main"))
                .await
                .unwrap()
                .rows[0][1],
            WireValue::WideInt(66)
        );
        reader.release_prepared(cx, handle).await.unwrap();
        reader.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn prepared_sources_keep_one_row_allowance_and_drain_cancel_before_handle_reuse() {
    run(async |cx| {
        let (addr, shutdown, mut task) = start(cx, "prepared-source-limits").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        seed(cx, &mut owner).await;
        let mut scope = grant(Rights::Read);
        scope.limits.max_rows = 4;
        let mut reader = Client::connect(cx, addr, token(&scope)).await.unwrap();
        reader.select(cx, "social").await.unwrap();
        for statement in [
            "MATCH (n:Person) WHERE n.age >= $min RETURN n AS id, n.age AS age",
            "MATCH (n:Person) WHERE n.age >= $min RETURN n.age AS age, count(*) AS n GROUP BY n.age ORDER BY age",
        ] {
            let handle = reader
                .prepare_read(cx, statement, vec![("min".into(), WireValue::Int(0))])
                .await
                .unwrap();
            let mut prefix = Vec::new();
            let error = reader
                .execute_prepared_streaming(
                    cx,
                    PreparedReadMode::SourceStream,
                    handle,
                    vec![("min".into(), WireValue::Int(0))],
                    |_| Ok(()),
                    |row| {
                        prefix.push(row);
                        Ok(())
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(server_code(error), ErrorCode::Budget);
            assert_eq!(prefix.len(), 4, "{statement}");
            reader.ping(cx, 91).await.unwrap();

            let mut callbacks = 0;
            let error = reader
                .execute_prepared_streaming(
                    cx,
                    PreparedReadMode::SourceStream,
                    handle,
                    vec![("min".into(), WireValue::Int(0))],
                    |_| Ok(()),
                    |_| {
                        callbacks += 1;
                        Err(ClientError::Protocol("stop after one prepared row"))
                    },
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                ClientError::Protocol("stop after one prepared row")
            ));
            assert_eq!(callbacks, 1);
            let reused = prepared_source(
                cx,
                &mut reader,
                handle,
                vec![("min".into(), WireValue::Int(10))],
            )
            .await
            .unwrap();
            assert_eq!(reused.rows.len(), 2);
            reader.release_prepared(cx, handle).await.unwrap();
        }
        // The mode is a physical contract. A rejected source profile stays
        // available through the explicit buffered path on the same template.
        let handle = reader
            .prepare_read(
                cx,
                "MATCH (n:Person) WHERE n.age >= $min RETURN n.age AS age",
                vec![("min".into(), WireValue::Int(10))],
            )
            .await
            .unwrap();
        assert_eq!(
            server_code(
                prepared_source(
                    cx,
                    &mut reader,
                    handle,
                    vec![("min".into(), WireValue::Int(10))],
                )
                .await
                .unwrap_err()
            ),
            ErrorCode::Statement
        );
        assert_eq!(
            reader
                .execute_prepared(cx, handle, vec![("min".into(), WireValue::Int(10))])
                .await
                .unwrap()
                .rows,
            [vec![WireValue::Int(10)], vec![WireValue::Int(11)]]
        );
        reader.release_prepared(cx, handle).await.unwrap();
        reader.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}
