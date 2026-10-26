//! fgdb-29iyk: result-row grants limit delivered data, not discarded identities.
//! Exercise the shared server write path over real FGP/TCP and reopen the same
//! durable database after shutdown. No permit, executor or transaction is mocked.

use asupersync::net::TcpListener;
use asupersync::security::key::AuthKey;
use asupersync::{Budget, Cx};
use fgdb::{Database, DatabaseKeys};
use fgdb_gql::GraphSymbolKind;
use fgdb_protocol::body::{ErrorCode, ExecuteMode, Outcome, WireValue};
use fgdb_protocol::client::{Answer, Client, ClientError};
use fgdb_server::{DatabaseConfig, Server, ServerLimits, Symbols, issue_token, issuer};
use fgdb_types::{CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Grant, QueryLimits, Rights, Scope};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "no_return_rows/prepared_batch.rs"]
mod prepared_batch;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x31; 32],
        DatabaseSecurityNamespaceId([0x32; 32]),
        [0x33; 32],
    )
}

fn issuer_key() -> AuthKey {
    AuthKey::from_seed(0x29_1a)
}

fn grant(rights: Rights, rows: u64) -> Grant {
    Grant {
        branch: fgdb_server::TRUNK.into(),
        labels: Scope::All,
        relations: Scope::All,
        properties: Scope::All,
        rights,
        limits: QueryLimits {
            max_nodes: 1_000_000,
            max_work: 100_000_000,
            max_rows: rows,
        },
        expires_at_ms: u64::MAX / 2,
    }
}

fn run(test: impl AsyncFnOnce(&Cx)) {
    let runtime = fgdb::runtime_builder().build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(test(&cx));
}

struct Fixture {
    address: SocketAddr,
    path: PathBuf,
    shutdown: fgdb_server::Shutdown,
    task: asupersync::runtime::TaskHandle<()>,
}

impl Fixture {
    async fn start(cx: &Cx, name: &str, configure: impl FnOnce(&mut DatabaseConfig)) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "fgdb-server-zero-rows-{name}-{}-{time}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        drop(
            Database::create(&contexts.commit(), &path, keys())
                .await
                .unwrap(),
        );
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
        let mut symbols = Symbols::new();
        for (kind, name, id) in [
            (GraphSymbolKind::Label, "Person", 1),
            (GraphSymbolKind::Property, "name", 1),
            (GraphSymbolKind::Property, "age", 2),
            (GraphSymbolKind::Relation, "KNOWS", 1),
        ] {
            symbols.bind(kind, name, id).unwrap();
        }
        let mut config = DatabaseConfig::new("social", keys(), issuer_key());
        config.symbols = symbols;
        configure(&mut config);
        server.open_database(cx, &path, config).await.unwrap();
        let shutdown = server.shutdown();
        let server = Arc::new(server);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = cx
            .spawn(move |child| async move { server.serve(&child, listener).await.unwrap() })
            .unwrap();
        Self {
            address,
            path,
            shutdown,
            task,
        }
    }

    async fn client(&self, cx: &Cx, grant: &Grant) -> Client {
        let authority = issuer(issuer_key(), &keys(), 1).unwrap();
        let token = issue_token(&authority, grant).unwrap();
        let mut client = Client::connect(cx, self.address, token).await.unwrap();
        client.select(cx, "social").await.unwrap();
        client
    }

    async fn stop(mut self, cx: &Cx, seq: u64, vertices: usize, edges: usize) {
        self.shutdown.trigger();
        self.task.join(cx).await.unwrap();
        // The server task and its database owner have gone away. Reopen the
        // actual published history, not a second fixture or a cached response.
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        let database = Database::open(&contexts.commit(), &self.path, keys())
            .await
            .unwrap();
        assert_eq!(database.frontier().unwrap(), CommitSeq(seq));
        assert_eq!(database.vertices().unwrap().len(), vertices);
        assert_eq!(database.edges().unwrap().len(), edges);
    }
}

async fn state(cx: &Cx, observer: &mut Client) -> (Answer, Answer) {
    let vertices = observer
        .execute(
            cx,
            ExecuteMode::Read,
            "MATCH (n:Person) RETURN n,n.name AS name,n.age AS age ORDER BY name",
            vec![],
        )
        .await
        .unwrap();
    let edges = observer
        .execute(
            cx,
            ExecuteMode::Read,
            "MATCH (a)-[r:KNOWS]->(b) RETURN a.name AS a,b.name AS b,r,r.age AS age ORDER BY a,b",
            vec![],
        )
        .await
        .unwrap();
    assert_eq!(vertices.outcome, edges.outcome);
    (vertices, edges)
}

fn committed(answer: Answer, seq: u64, statements: u64) {
    assert!(answer.columns.is_empty(), "no identity columns: {answer:?}");
    assert!(answer.rows.is_empty(), "no identity rows: {answer:?}");
    assert_eq!(answer.outcome, Outcome::WriteCommitted { seq, statements });
}

async fn refuse_without_rows(cx: &Cx, client: &mut Client, text: &str, code: ErrorCode) {
    let mut columns = 0;
    let mut rows = 0;
    let result = client
        .execute_streaming(
            cx,
            ExecuteMode::Write,
            text,
            vec![],
            |_| {
                columns += 1;
                Ok(())
            },
            |_| {
                rows += 1;
                Ok(())
            },
        )
        .await;
    assert!(
        matches!(result, Err(ClientError::Server { code: actual, .. }) if actual == code),
        "{text}: {result:?}"
    );
    assert_eq!(
        (columns, rows),
        (0, 0),
        "a refused write leaked tentative results"
    );
}

#[test]
fn weighted_relationship_action_imports_and_updates_keep_zero_row_wire_contract() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "relationship-action-import", |_| {}).await;
        let mut writer = fixture.client(cx, &grant(Rights::ReadWrite, 0)).await;
        let mut observer = fixture.client(cx, &grant(Rights::Read, 100)).await;
        let initial = state(cx, &mut observer).await;
        assert!(initial.0.rows.is_empty() && initial.1.rows.is_empty());
        // state() issues an ordinary read. An empty answer still has the
        // Rows outcome; ReadClosed belongs to no-effect write completion.
        assert_eq!(initial.0.outcome, Outcome::Rows { seq: 0 });
        let statement = "UNWIND $rows AS row MERGE (a:Person {name:row.s}) \
            MERGE (b:Person {name:row.d}) MERGE (a)-[r:KNOWS]->(b) \
            ON CREATE SET r.age=row.w ON MATCH SET r.age=row.w";
        let parameters = || {
            vec![(
                "rows".into(),
                WireValue::List(
                    [("Ada", "Bob", 3), ("Bob", "Cy", 5), ("Cy", "Cy", 7)]
                        .into_iter()
                        .map(|(source, target, weight)| {
                            WireValue::Map(vec![
                                ("d".into(), WireValue::Text(target.into())),
                                ("s".into(), WireValue::Text(source.into())),
                                ("w".into(), WireValue::Int(weight)),
                            ])
                        })
                        .collect(),
                ),
            )]
        };
        committed(
            writer
                .execute(cx, ExecuteMode::Write, statement, parameters())
                .await
                .unwrap(),
            1,
            1,
        );
        let imported = state(cx, &mut observer).await;
        assert_eq!((imported.0.rows.len(), imported.1.rows.len()), (3, 3));
        assert_eq!(
            imported
                .1
                .rows
                .iter()
                .map(|row| row[3].clone())
                .collect::<Vec<_>>(),
            vec![WireValue::Int(3), WireValue::Int(5), WireValue::Int(7)]
        );
        let repeated = writer
            .execute(cx, ExecuteMode::Write, statement, parameters())
            .await
            .unwrap();
        // Reapplying the weights still executes SET, whose normalized no-op
        // commits once while preserving all graph identities and properties.
        committed(repeated, 2, 1);
        let retried = state(cx, &mut observer).await;
        assert_eq!(
            (
                retried.0.columns,
                retried.0.rows,
                retried.1.columns,
                retried.1.rows
            ),
            (
                imported.0.columns,
                imported.0.rows,
                imported.1.columns,
                imported.1.rows
            )
        );

        committed(
            writer
                .execute(
                    cx,
                    ExecuteMode::Write,
                    "UNWIND [1,2] AS delta MERGE (a:Person {name:'Ada'}) \
                 MERGE (b:Person {name:'Bob'}) MERGE (a)-[r:KNOWS]->(b) \
                 ON MATCH SET r.age=r.age+delta",
                    vec![],
                )
                .await
                .unwrap(),
            3,
            1,
        );
        let updated = state(cx, &mut observer).await;
        assert_eq!(
            updated
                .1
                .rows
                .iter()
                .map(|row| row[3].clone())
                .collect::<Vec<_>>(),
            vec![WireValue::Int(6), WireValue::Int(5), WireValue::Int(7)]
        );
        refuse_without_rows(
            cx,
            &mut writer,
            "UNWIND [1] AS delta MERGE (a:Person {name:'Ada'}) \
             MERGE (b:Person {name:'Bob'}) MERGE (a)-[r:KNOWS]->(b) \
             ON MATCH SET r.age=99 RETURN r.age AS weight",
            ErrorCode::Budget,
        )
        .await;
        assert_eq!(state(cx, &mut observer).await, updated);
        let (mut columns, mut rows) = (0, 0);
        let failure = writer
            .execute_streaming(
                cx,
                ExecuteMode::Write,
                "UNWIND [1,2] AS delta MERGE (a:Person {name:'Ada'}) \
                 MERGE (b:Person {name:'Bob'}) MERGE (a)-[r:KNOWS]->(b) \
                 ON MATCH SET r.age=10/(2-delta)",
                vec![],
                |_| {
                    columns += 1;
                    Ok(())
                },
                |_| {
                    rows += 1;
                    Ok(())
                },
            )
            .await;
        let Err(ClientError::Server { message, .. }) = failure else {
            panic!("late relationship arithmetic must refuse: {failure:?}");
        };
        assert!(
            message.contains("DivisionByZero") || message.contains("division by zero"),
            "not an arithmetic refusal: {message}"
        );
        assert_eq!((columns, rows), (0, 0));
        assert_eq!(state(cx, &mut observer).await, updated);
        writer.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        fixture.stop(cx, 3, 3, 3).await;
    });
}

#[test]
fn write_only_zero_row_grant_creates_but_never_acquires_read_rights() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "write-only", |_| {}).await;
        let mut writer = fixture.client(cx, &grant(Rights::Write, 0)).await;
        let mut observer = fixture.client(cx, &grant(Rights::Read, 100)).await;
        committed(
            writer
                .execute(
                    cx,
                    ExecuteMode::Write,
                    "CREATE (:Person {name:'Ada',age:1}),(:Person {name:'Bob',age:2})",
                    vec![],
                )
                .await
                .unwrap(),
            1,
            1,
        );
        let before = state(cx, &mut observer).await;
        assert_eq!(before.0.rows.len(), 2);
        assert!(before.1.rows.is_empty());
        for text in [
            "MATCH (n:Person) SET n.age=99",
            "MATCH (n:Person {name:'absent'}) SET n.age=99",
            "CREATE (n:Person {name:'Forbidden'}) RETURN n.name AS name",
            "CREATE (n:Person {name:'Forbidden'}) RETURN n.name AS name LIMIT 0",
        ] {
            refuse_without_rows(cx, &mut writer, text, ErrorCode::PermissionDenied).await;
            assert_eq!(state(cx, &mut observer).await, before);
        }
        refuse_without_rows(
            cx,
            &mut observer,
            "CREATE (:Person)",
            ErrorCode::PermissionDenied,
        )
        .await;
        assert_eq!(state(cx, &mut observer).await, before);
        writer.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        fixture.stop(cx, 1, 2, 0).await;
    });
}

#[test]
fn zero_row_grant_applies_crud_and_keeps_real_return_rows_bounded() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "crud", |_| {}).await;
        let mut writer = fixture.client(cx, &grant(Rights::ReadWrite, 0)).await;
        let mut observer = fixture.client(cx, &grant(Rights::Read, 100)).await;
        let writes = [
            "CREATE (:Person {name:'Ada',age:1}),(:Person {name:'Bob',age:2})",
            "MATCH (n:Person) SET n.age=n.age+10",
            "MERGE (n:Person {name:'Ada'}) ON MATCH SET n.age=n.age+1",
            "MATCH (a:Person {name:'Ada'}),(b:Person {name:'Bob'}) MERGE (a)-[r:KNOWS]->(b)",
            "MATCH (a:Person {name:'Ada'}),(b:Person {name:'Bob'}) MERGE (a)-[r:KNOWS]->(b) ON MATCH SET r.age=7",
            "MATCH ()-[r:KNOWS]->() SET r.age=r.age+1",
        ];
        for (index, text) in writes.into_iter().enumerate() {
            committed(
                writer
                    .execute(cx, ExecuteMode::Write, text, vec![])
                    .await
                    .unwrap(),
                index as u64 + 1,
                1,
            );
        }
        let before = state(cx, &mut observer).await;
        assert_eq!((before.0.rows.len(), before.1.rows.len()), (2, 1));
        let checked = observer.execute(cx, ExecuteMode::Read,
            "MATCH (a:Person)-[r:KNOWS]->(b:Person) WHERE a.age=12 AND b.age=12 AND r.age=8 RETURN a.name AS name",
            vec![]).await.unwrap();
        assert_eq!(checked.rows, vec![vec![WireValue::Text("Ada".into())]]);
        for text in [
            "MATCH (n:Person {name:'Ada'}) SET n.age=13 RETURN n.name AS name",
            "CREATE (n:Person {name:'NotCommitted'}) RETURN n.name AS name",
        ] {
            refuse_without_rows(cx, &mut writer, text, ErrorCode::Budget).await;
            assert_eq!(state(cx, &mut observer).await, before);
        }
        let empty = writer
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (n:Person {name:'absent'}) SET n.age=0",
                vec![],
            )
            .await
            .unwrap();
        assert!(empty.rows.is_empty() && empty.columns.is_empty());
        assert_eq!(
            empty.outcome,
            Outcome::ReadClosed {
                seq: 6,
                statements: 1
            }
        );
        committed(
            writer
                .execute(
                    cx,
                    ExecuteMode::Write,
                    "MATCH (n:Person {name:'Bob'}) DETACH DELETE n",
                    vec![],
                )
                .await
                .unwrap(),
            7,
            1,
        );
        let after = state(cx, &mut observer).await;
        assert_eq!((after.0.rows.len(), after.1.rows.len()), (1, 0));
        writer.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        fixture.stop(cx, 7, 1, 0).await;
    });
}

#[test]
fn zero_row_writes_preserve_late_rollback_and_signed_work_limits() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "rollback", |_| {}).await;
        let mut writer = fixture.client(cx, &grant(Rights::ReadWrite, 0)).await;
        let mut observer = fixture.client(cx, &grant(Rights::Read, 100)).await;
        committed(
            writer
                .execute(
                    cx,
                    ExecuteMode::Write,
                    "CREATE (:Person {name:'Ada',age:1})",
                    vec![],
                )
                .await
                .unwrap(),
            1,
            1,
        );
        let before = state(cx, &mut observer).await;
        // Establish a changed durable before-image with the same mutation path.
        committed(
            writer
                .execute(
                    cx,
                    ExecuteMode::Write,
                    "MATCH (n:Person) SET n.age=2",
                    vec![],
                )
                .await
                .unwrap(),
            2,
            1,
        );
        let staged_control = state(cx, &mut observer).await;
        assert_ne!(staged_control, before);
        // The denominator is nonzero in committed state. It becomes zero only
        // after the first statement's private update is visible to the second.
        let failure = writer
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (n:Person) SET n.age=3; MATCH (n:Person) SET n.age=10/(n.age-3)",
                vec![],
            )
            .await;
        let Err(ClientError::Server { message, .. }) = failure else {
            panic!("late arithmetic must refuse: {failure:?}");
        };
        assert!(
            message.contains("DivisionByZero") || message.contains("division by zero"),
            "not an arithmetic refusal: {message}"
        );
        assert_eq!(state(cx, &mut observer).await, staged_control);
        let mut empty_work = grant(Rights::Write, 0);
        empty_work.limits.max_work = 0;
        let mut stopped = fixture.client(cx, &empty_work).await;
        refuse_without_rows(cx, &mut stopped, "CREATE (:Person)", ErrorCode::Budget).await;
        assert_eq!(state(cx, &mut observer).await, staged_control);
        stopped.close(cx).await.unwrap();
        writer.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        fixture.stop(cx, 2, 1, 0).await;
    });
}

#[test]
fn stats_only_dispatch_keeps_the_hosts_cumulative_creation_ceiling() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "creation-cap", |config| {
            config.write_policy.max_created_vertices = 1;
        })
        .await;
        let mut writer = fixture.client(cx, &grant(Rights::Write, 0)).await;
        let mut observer = fixture.client(cx, &grant(Rights::Read, 100)).await;
        let before = state(cx, &mut observer).await;
        let failure = writer
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (:Person {name:'First'}); CREATE (:Person {name:'Second'})",
                vec![],
            )
            .await;
        assert!(
            matches!(failure, Err(ClientError::Server { .. })),
            "{failure:?}"
        );
        assert_eq!(state(cx, &mut observer).await, before);
        committed(
            writer
                .execute(
                    cx,
                    ExecuteMode::Write,
                    "CREATE (:Person {name:'Only'})",
                    vec![],
                )
                .await
                .unwrap(),
            1,
            1,
        );
        writer.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        fixture.stop(cx, 1, 1, 0).await;
    });
}

#[test]
fn authorized_creation_chain_keeps_private_rows_and_commits_once() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "creation-chain", |_| {}).await;
        let mut writer = fixture.client(cx, &grant(Rights::Write, 0)).await;
        let mut observer = fixture.client(cx, &grant(Rights::Read, 100)).await;
        committed(
            writer
                .execute(
                    cx,
                    ExecuteMode::Write,
                    "CREATE (a:Person {name:'Ada',age:1}) WITH a \
                     CREATE (b:Person {name:'Bob',age:2}),(a)-[:KNOWS]->(b)",
                    vec![],
                )
                .await
                .unwrap(),
            1,
            1,
        );
        let after = state(cx, &mut observer).await;
        assert_eq!((after.0.rows.len(), after.1.rows.len()), (2, 1));
        assert_eq!(after.1.rows[0][0], WireValue::Text("Ada".into()));
        assert_eq!(after.1.rows[0][1], WireValue::Text("Bob".into()));
        // Private WITH input needs no delivered-row allowance, but RETURN
        // still requires read authority even when its final limit is zero.
        refuse_without_rows(
            cx,
            &mut writer,
            "CREATE (a:Person) WITH a CREATE (b:Person) RETURN 1 AS value LIMIT 0",
            ErrorCode::PermissionDenied,
        )
        .await;
        assert_eq!(state(cx, &mut observer).await, after);
        writer.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        fixture.stop(cx, 1, 2, 1).await;
    });
}

#[test]
fn authorized_creation_chain_refusal_rolls_back_every_step() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "chain-late-refusal", |config| {
            config.write_policy.max_created_vertices = 1;
        })
        .await;
        let mut writer = fixture.client(cx, &grant(Rights::ReadWrite, 100)).await;
        let before = state(cx, &mut writer).await;
        let failure = writer
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (a:Person {name:'First'}) WITH a \
                 CREATE (b:Person {name:'Second'}) RETURN b.name AS name",
                vec![],
            )
            .await;
        let Err(ClientError::Server { message, .. }) = failure else {
            panic!("the shared creation allowance must refuse: {failure:?}");
        };
        assert!(
            message.contains("limit") || message.contains("budget"),
            "{message}"
        );
        assert_eq!(state(cx, &mut writer).await, before);
        writer.close(cx).await.unwrap();
        fixture.stop(cx, 0, 0, 0).await;
    });
}

#[test]
fn zero_row_mixed_chains_update_remove_and_delete_with_one_completion_each() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "mixed-chain-zero-rows", |_| {}).await;
        let mut writer = fixture.client(cx, &grant(Rights::ReadWrite, 0)).await;
        let mut observer = fixture.client(cx, &grant(Rights::Read, 100)).await;
        committed(
            writer
                .execute(
                    cx,
                    ExecuteMode::Write,
                    "CREATE (a:Person {name:'Ada',age:1}) WITH a SET a.age=a.age+10",
                    vec![],
                )
                .await
                .unwrap(),
            1,
            1,
        );
        let after_set = state(cx, &mut observer).await;
        assert_eq!(after_set.0.rows.len(), 1);
        assert_eq!(after_set.0.rows[0][2], WireValue::Int(11));
        committed(
            writer
                .execute(
                    cx,
                    ExecuteMode::Write,
                    "MATCH (a:Person {name:'Ada'}) CREATE (b:Person {name:'Bob',age:2}) \
                     WITH a,b REMOVE a.age",
                    vec![],
                )
                .await
                .unwrap(),
            2,
            1,
        );
        let after_remove = state(cx, &mut observer).await;
        assert_eq!(after_remove.0.rows.len(), 2);
        assert_eq!(after_remove.0.rows[0][1], WireValue::Text("Ada".into()));
        assert_eq!(after_remove.0.rows[0][2], WireValue::Null);
        committed(
            writer
                .execute(
                    cx,
                    ExecuteMode::Write,
                    "MATCH (a:Person {name:'Bob'}) CREATE (b:Person {name:'Survivor'}) \
                     WITH a,b DELETE a",
                    vec![],
                )
                .await
                .unwrap(),
            3,
            1,
        );
        let after_delete = state(cx, &mut observer).await;
        assert_eq!(
            after_delete
                .0
                .rows
                .iter()
                .map(|row| row[1].clone())
                .collect::<Vec<_>>(),
            vec![
                WireValue::Text("Ada".into()),
                WireValue::Text("Survivor".into())
            ]
        );
        assert!(after_delete.1.rows.is_empty());
        writer.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        fixture.stop(cx, 3, 2, 0).await;
    });
}

#[test]
fn relationship_merge_chain_continues_after_created_and_existing_matches() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "merge-chain-continuation", |_| {}).await;
        let mut writer = fixture.client(cx, &grant(Rights::ReadWrite, 0)).await;
        let mut observer = fixture.client(cx, &grant(Rights::Read, 100)).await;
        committed(
            writer
                .execute(
                    cx,
                    ExecuteMode::Write,
                    "CREATE (a:Person {name:'Ada'}),(b:Person {name:'Bob'}) \
                     MERGE (a)-[r:KNOWS]->(b) CREATE (b)-[:KNOWS]->(a)",
                    vec![],
                )
                .await
                .unwrap(),
            1,
            1,
        );
        let after_create = state(cx, &mut observer).await;
        assert_eq!(
            (after_create.0.rows.len(), after_create.1.rows.len()),
            (2, 2)
        );
        committed(
            writer
                .execute(
                    cx,
                    ExecuteMode::Write,
                    "MATCH (a:Person {name:'Ada'}),(b:Person {name:'Bob'}) \
                     MERGE (a)-[r:KNOWS]->(b) \
                     CREATE (c:Person {name:'Followup'}),(c)-[:KNOWS]->(b)",
                    vec![],
                )
                .await
                .unwrap(),
            2,
            1,
        );
        let after_match = state(cx, &mut observer).await;
        assert_eq!((after_match.0.rows.len(), after_match.1.rows.len()), (3, 3));
        assert_eq!(
            after_match
                .1
                .rows
                .iter()
                .filter(|row| {
                    row[0] == WireValue::Text("Ada".into())
                        && row[1] == WireValue::Text("Bob".into())
                })
                .count(),
            1,
            "a matched MERGE created a duplicate relationship"
        );
        writer.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        fixture.stop(cx, 2, 3, 3).await;
    });
}

#[test]
fn parameter_endpoint_merge_chain_imports_with_zero_rows_and_reuses_the_graph() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "vertex-merge-chain-import", |_| {}).await;
        let mut writer = fixture.client(cx, &grant(Rights::ReadWrite, 0)).await;
        let mut observer = fixture.client(cx, &grant(Rights::Read, 100)).await;
        let statement = "UNWIND $rows AS row MERGE (a:Person {name:row.s}) \
            MERGE (b:Person {name:row.d}) MERGE (a)-[:KNOWS]->(b)";
        let parameters = || {
            vec![(
                "rows".into(),
                WireValue::List(
                    [("Ada", "Bob"), ("Bob", "Cy"), ("Ada", "Bob"), ("Cy", "Cy")]
                        .into_iter()
                        .map(|(source, target)| {
                            WireValue::Map(vec![
                                ("d".into(), WireValue::Text(target.into())),
                                ("s".into(), WireValue::Text(source.into())),
                            ])
                        })
                        .collect(),
                ),
            )]
        };
        committed(
            writer
                .execute(cx, ExecuteMode::Write, statement, parameters())
                .await
                .unwrap(),
            1,
            1,
        );
        let first = state(cx, &mut observer).await;
        assert_eq!((first.0.rows.len(), first.1.rows.len()), (3, 3));
        assert_eq!(
            first
                .1
                .rows
                .iter()
                .map(|row| (&row[0], &row[1]))
                .collect::<Vec<_>>(),
            vec![
                (
                    &WireValue::Text("Ada".into()),
                    &WireValue::Text("Bob".into())
                ),
                (
                    &WireValue::Text("Bob".into()),
                    &WireValue::Text("Cy".into())
                ),
                (&WireValue::Text("Cy".into()), &WireValue::Text("Cy".into())),
            ],
        );
        let repeated = writer
            .execute(cx, ExecuteMode::Write, statement, parameters())
            .await
            .unwrap();
        assert!(repeated.columns.is_empty() && repeated.rows.is_empty());
        assert_eq!(
            repeated.outcome,
            Outcome::ReadClosed {
                seq: 1,
                statements: 1
            }
        );
        assert_eq!(state(cx, &mut observer).await, first);
        writer.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        fixture.stop(cx, 1, 3, 3).await;
    });
}
