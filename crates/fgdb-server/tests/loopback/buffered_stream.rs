//! Public FGP source reads over the explicitly configured cold backend.
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
             (a)-[:KNOWS {age:3}]->(b), (a)-[:KNOWS {age:4}]->(b), \
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
                "MATCH (a)-[r:KNOWS]->(b) RETURN r.age AS weight, b.name AS hidden ORDER BY r",
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
                    "MATCH (n:Person) RETURN n.age AS age LIMIT 1",
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
                    "MATCH (a)-[r:WORKS_AT]->(b) RETURN r.age AS age",
                    vec![],
                )
                .await
                .unwrap()
                .rows
                .is_empty()
        );
        assert_eq!(
            server_code(
                reader
                    .execute(
                        cx,
                        ExecuteMode::ReadStream,
                        "MATCH (n:Person) RETURN count(*) AS n",
                        vec![],
                    )
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Statement
        );
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
