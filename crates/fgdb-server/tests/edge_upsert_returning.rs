//! Capability-scoped edge upsert RETURN over real FGP/TCP and durable reopen.
use asupersync::net::TcpListener;
use asupersync::security::key::AuthKey;
use asupersync::{Budget, Cx};
use fgdb::{Database, DatabaseKeys};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::GraphSymbolKind;
use fgdb_protocol::body::{ErrorCode, ExecuteMode, Outcome, WireValue};
use fgdb_protocol::client::{Answer, Client, ClientError};
use fgdb_server::{DatabaseConfig, Server, ServerLimits, Symbols, issue_token, issuer};
use fgdb_types::{
    CanonicalPropertyValue, CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId,
    PurposeContexts,
};
use fgdb_warden::{Grant, QueryLimits, Rights, Scope};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const MATCH: &str = "MATCH (a),(b) WHERE a.key=1 AND b.key=2";
const CREATE: &str = "CREATE (:Person {key:1}),(:Person {key:2})";
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x31; 32],
        DatabaseSecurityNamespaceId([0x32; 32]),
        [0x33; 32],
    )
}
fn issuer_key() -> AuthKey {
    AuthKey::from_seed(0x29_1b)
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
    async fn start(cx: &Cx) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "fgdb-edge-upsert-return-{}-{time}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
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
            (GraphSymbolKind::Label, "Hidden", 2),
            (GraphSymbolKind::Relation, "R", 1),
            (GraphSymbolKind::Property, "key", 1),
            (GraphSymbolKind::Property, "value", 2),
            (GraphSymbolKind::Property, "secret", 3),
        ] {
            symbols.bind(kind, name, id).unwrap();
        }
        let mut config = DatabaseConfig::new("social", keys(), issuer_key());
        config.symbols = symbols;
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
        let token = issue_token(&issuer(issuer_key(), &keys(), 1).unwrap(), grant).unwrap();
        let mut client = Client::connect(cx, self.address, token).await.unwrap();
        client.select(cx, "social").await.unwrap();
        client
    }
    async fn stop(mut self, cx: &Cx, seq: u64, vertices: usize, value: i64, secret: bool) {
        self.shutdown.trigger();
        self.task.join(cx).await.unwrap();
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        let db = Database::open(&contexts.commit(), &self.path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap(), CommitSeq(seq));
        assert_eq!(db.vertices().unwrap().len(), vertices);
        let edges = db.edges().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].entry.relation, RelationId(1));
        let mut props = vec![(
            PropertyKeyId(2),
            CanonicalPropertyValue::from(CanonicalScalar::Int(value)),
        )];
        if secret {
            props.push((PropertyKeyId(3), CanonicalScalar::Int(99).into()));
        }
        assert_eq!(edges[0].props, props);
        for (id, key) in [(edges[0].entry.src, 1), (edges[0].entry.dst, 2)] {
            let vertex = db.vertex(id).unwrap().unwrap();
            assert_eq!(vertex.labels, vec![LabelId(1)]);
            assert_eq!(
                vertex.props,
                vec![(PropertyKeyId(1), CanonicalScalar::Int(key).into())]
            );
        }
    }
}
async fn write(cx: &Cx, client: &mut Client, text: &str) -> Answer {
    client
        .execute(cx, ExecuteMode::Write, text, vec![])
        .await
        .unwrap()
}
async fn state(cx: &Cx, client: &mut Client) -> Answer {
    client.execute(cx, ExecuteMode::Read,
        "MATCH (a)-[r:R]->(b) RETURN a.key AS a,b.key AS b,r.value AS value,r.secret AS secret ORDER BY a,b",
        vec![]).await.unwrap()
}
async fn refusal(cx: &Cx, client: &mut Client, text: &str) -> (ErrorCode, String) {
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
    assert_eq!(
        (columns, rows),
        (0, 0),
        "a refused write exposed a result prefix"
    );
    match result {
        Err(ClientError::Server { code, message }) => (code, message),
        other => panic!("expected server refusal: {other:?}"),
    }
}

#[test]
fn conditional_edge_results_are_bound_projected_and_durable_before_delivery() {
    run(async |cx| {
        let fixture = Fixture::start(cx).await;
        let mut writer = fixture.client(cx, &grant(Rights::ReadWrite, 1)).await;
        let mut observer = fixture.client(cx, &grant(Rights::Read, 100)).await;
        write(cx, &mut writer, CREATE).await;
        let text = format!(
            "{MATCH} MERGE (a)-[r:R]->(b) ON CREATE SET r.value=$initial \
            ON MATCH SET r.value=r.value+3 SET r.value=r.value*2 RETURN r.value AS value"
        );
        for (seq, expected) in [(2, 14), (3, 34)] {
            let answer = writer
                .execute(
                    cx,
                    ExecuteMode::Write,
                    &text,
                    vec![("initial".into(), WireValue::Int(7))],
                )
                .await
                .unwrap();
            assert_eq!(answer.columns, ["value"]);
            assert_eq!(answer.rows, vec![vec![WireValue::Int(expected)]]);
            assert_eq!(
                answer.outcome,
                Outcome::WriteCommitted { seq, statements: 1 }
            );
            assert_eq!(
                state(cx, &mut observer).await.rows,
                vec![vec![
                    WireValue::Int(1),
                    WireValue::Int(2),
                    WireValue::Int(expected),
                    WireValue::Null
                ]]
            );
        }
        // Plain MERGE keeps its own classifier and no-effect completion.
        let plain = write(
            cx,
            &mut writer,
            &format!("{MATCH} MERGE (a)-[r:R]->(b) RETURN r.value AS value"),
        )
        .await;
        assert_eq!(plain.rows, vec![vec![WireValue::Int(34)]]);
        assert_eq!(
            plain.outcome,
            Outcome::ReadClosed {
                seq: 3,
                statements: 1
            }
        );
        let before = state(cx, &mut observer).await;
        assert_eq!(
            refusal(cx, &mut writer, &text).await.0,
            ErrorCode::Statement
        );
        assert_eq!(state(cx, &mut observer).await, before);
        writer.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        fixture.stop(cx, 3, 2, 34, false).await;
    });
}

#[test]
fn scope_masks_return_fields_and_denied_actions_never_commit_or_deliver() {
    run(async |cx| {
        let fixture = Fixture::start(cx).await;
        let mut admin = fixture.client(cx, &grant(Rights::ReadWrite, 100)).await;
        write(
            cx,
            &mut admin,
            "CREATE (:Person {key:1}),(:Person {key:2}),(:Hidden {key:1})",
        )
        .await;
        write(
            cx,
            &mut admin,
            "MATCH (a:Person),(b:Person) WHERE a.key=1 AND b.key=2 \
            MERGE (a)-[r:R]->(b) ON CREATE SET r.value=5,r.secret=99",
        )
        .await;
        let mut restricted = grant(Rights::ReadWrite, 1);
        restricted.labels = Scope::only([LabelId(1)]);
        restricted.properties = Scope::only([PropertyKeyId(1), PropertyKeyId(2)]);
        let mut writer = fixture.client(cx, &restricted).await;
        // Hidden vertex key=1 is excluded BEFORE matching; otherwise this
        // singular upsert would see two endpoint pairs and refuse.
        let answer = write(
            cx,
            &mut writer,
            &format!(
                "{MATCH} MERGE (a)-[r:R]->(b) \
            ON MATCH SET r.value=r.value+1 RETURN r.value AS value,r.secret AS secret"
            ),
        )
        .await;
        assert_eq!(answer.rows, vec![vec![WireValue::Int(6), WireValue::Null]]);
        assert_eq!(
            answer.outcome,
            Outcome::WriteCommitted {
                seq: 3,
                statements: 1
            }
        );
        let before = state(cx, &mut admin).await;
        assert_eq!(
            before.rows,
            vec![vec![
                WireValue::Int(1),
                WireValue::Int(2),
                WireValue::Int(6),
                WireValue::Int(99)
            ]]
        );
        for text in [
            format!(
                "{MATCH} MERGE (a)-[r:R]->(b) ON MATCH SET r.value=8,r.secret=99 RETURN r.value LIMIT 0"
            ),
            format!(
                "{MATCH} MERGE (a)-[r:R {{secret:99}}]->(b) ON MATCH SET r.value=8 RETURN r.value"
            ),
            format!("{MATCH} MERGE (a)-[r:R]->(b) ON MATCH SET r.value=8 RETURN id(r)"),
        ] {
            assert_eq!(
                refusal(cx, &mut writer, &text).await.0,
                ErrorCode::PermissionDenied
            );
            assert_eq!(state(cx, &mut admin).await, before);
        }
        let mut write_only = fixture.client(cx, &grant(Rights::Write, 0)).await;
        let text = "MATCH (a),(b) WHERE a.key=999 AND b.key=2 MERGE (a)-[r:R]->(b) \
            ON CREATE SET r.value=1 RETURN r.value LIMIT 0";
        assert_eq!(
            refusal(cx, &mut write_only, text).await.0,
            ErrorCode::PermissionDenied
        );
        assert_eq!(state(cx, &mut admin).await, before);
        write_only.close(cx).await.unwrap();
        writer.close(cx).await.unwrap();
        admin.close(cx).await.unwrap();
        fixture.stop(cx, 3, 3, 6, true).await;
    });
}

#[test]
fn limit_zero_keeps_effects_but_not_errors_and_noinput_does_not_invent_a_write() {
    run(async |cx| {
        let fixture = Fixture::start(cx).await;
        let mut writer = fixture.client(cx, &grant(Rights::ReadWrite, 0)).await;
        let mut observer = fixture.client(cx, &grant(Rights::Read, 100)).await;
        write(cx, &mut writer, CREATE).await;
        let answer = write(
            cx,
            &mut writer,
            &format!(
                "{MATCH} MERGE (a)-[r:R]->(b) \
            ON CREATE SET r.value=3 RETURN r.value AS value LIMIT 0"
            ),
        )
        .await;
        assert!(answer.rows.is_empty());
        assert_eq!(answer.columns, ["value"]);
        assert_eq!(
            answer.outcome,
            Outcome::WriteCommitted {
                seq: 2,
                statements: 1
            }
        );
        let before = state(cx, &mut observer).await;
        assert_eq!(
            before.rows,
            vec![vec![
                WireValue::Int(1),
                WireValue::Int(2),
                WireValue::Int(3),
                WireValue::Null
            ]]
        );
        let no_rows = format!("{MATCH} MERGE (a)-[r:R]->(b) ON MATCH SET r.value=4 RETURN r.value");
        assert_eq!(
            refusal(cx, &mut writer, &no_rows).await.0,
            ErrorCode::Budget
        );
        assert_eq!(state(cx, &mut observer).await, before);
        let (_, message) = refusal(
            cx,
            &mut writer,
            &format!(
                "{MATCH} MERGE (a)-[r:R]->(b) \
            ON MATCH SET r.value=0 RETURN 10/r.value LIMIT 0"
            ),
        )
        .await;
        assert!(
            message.contains("DivisionByZero") || message.contains("division by zero"),
            "{message}"
        );
        assert_eq!(state(cx, &mut observer).await, before);
        let empty = write(
            cx,
            &mut writer,
            "MATCH (a),(b) WHERE a.key=999 AND b.key=2 \
            MERGE (a)-[r:R]->(b) ON CREATE SET r.value=1 RETURN r.value",
        )
        .await;
        assert!(empty.rows.is_empty());
        assert_eq!(
            empty.outcome,
            Outcome::ReadClosed {
                seq: 2,
                statements: 1
            }
        );
        assert_eq!(state(cx, &mut observer).await, before);
        writer.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        fixture.stop(cx, 2, 2, 3, false).await;
    });
}
