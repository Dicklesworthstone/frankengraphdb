//! The public server route must expose authorized occurrence-preserving RETURN.
//! Real TCP clients, native writes and independent reads share one durable DB.

use asupersync::net::TcpListener;
use asupersync::security::key::AuthKey;
use asupersync::{Budget, Cx};
use fgdb::{Database, DatabaseKeys};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::GraphSymbolKind;
use fgdb_protocol::body::{ErrorCode, ExecuteMode, Outcome, WireValue};
use fgdb_protocol::client::{Answer, Client, ClientError};
use fgdb_server::{DatabaseConfig, Server, ServerLimits, Symbols, issue_token, issuer};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Grant, QueryLimits, Rights, Scope};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const UPSERT: &str = "MATCH (a)-[:S]->(b) MERGE (a)-[r:R]->(b) \
    ON CREATE SET r.p=7 ON MATCH SET r.p=r.p+1";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x91; 32],
        DatabaseSecurityNamespaceId([0x92; 32]),
        [0x93; 32],
    )
}
fn issuer_key() -> AuthKey {
    AuthKey::from_seed(0xed93)
}
fn grant(rows: u64) -> Grant {
    Grant {
        branch: fgdb_server::TRUNK.into(),
        labels: Scope::All,
        relations: Scope::All,
        properties: Scope::All,
        rights: Rights::ReadWrite,
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
    path: PathBuf,
    address: SocketAddr,
    shutdown: fgdb_server::Shutdown,
    task: asupersync::runtime::TaskHandle<()>,
}
impl Fixture {
    async fn start(cx: &Cx, name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "fgdb-server-edge-return-{name}-{}-{time}-{}",
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
                initial_window_rows: 1,
                max_window_bytes: 1 << 20,
                max_window_rows: 1 << 10,
                max_connections: 8,
            },
        )
        .unwrap();
        let mut symbols = Symbols::new();
        for (kind, name, id) in [
            (GraphSymbolKind::Relation, "R", 1),
            (GraphSymbolKind::Relation, "S", 2),
            (GraphSymbolKind::Property, "p", 1),
        ] {
            symbols.bind(kind, name, id).unwrap();
        }
        let mut config = DatabaseConfig::new("graph", keys(), issuer_key());
        config.symbols = symbols;
        server.open_database(cx, &path, config).await.unwrap();
        let shutdown = server.shutdown();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = Arc::new(server);
        let task = cx
            .spawn(move |child| async move {
                server.serve(&child, listener).await.unwrap();
            })
            .unwrap();
        Self {
            path,
            address,
            shutdown,
            task,
        }
    }
    async fn client(&self, cx: &Cx, rows: u64) -> Client {
        let authority = issuer(issuer_key(), &keys(), 1).unwrap();
        let credential = issue_token(&authority, &grant(rows)).unwrap();
        let mut client = Client::connect(cx, self.address, credential).await.unwrap();
        client.select(cx, "graph").await.unwrap();
        client
    }
    async fn stop(mut self, cx: &Cx, seq: u64, vertices: usize, expected: &[i64]) {
        self.shutdown.trigger();
        self.task.join(cx).await.unwrap();
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        let db = Database::open(&contexts.commit(), &self.path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap(), CommitSeq(seq));
        assert_eq!(db.vertices().unwrap().len(), vertices);
        let mut values = db
            .edges()
            .unwrap()
            .into_iter()
            .filter(|edge| edge.entry.relation == RelationId(1))
            .map(|edge| {
                let value = edge
                    .props
                    .iter()
                    .find(|(key, _)| *key == PropertyKeyId(1))
                    .unwrap();
                let Some(CanonicalScalar::Int(value)) = value.1.as_scalar() else {
                    panic!("integer property")
                };
                *value
            })
            .collect::<Vec<_>>();
        values.sort_unstable();
        assert_eq!(values, expected);
    }
}

async fn write(cx: &Cx, client: &mut Client, text: &str) -> Answer {
    client
        .execute(cx, ExecuteMode::Write, text, vec![])
        .await
        .unwrap()
}
async fn state(cx: &Cx, client: &mut Client) -> (Answer, Answer) {
    let vertices = client
        .execute(
            cx,
            ExecuteMode::Read,
            "MATCH (n) RETURN n,n.p AS p ORDER BY p",
            vec![],
        )
        .await
        .unwrap();
    let edges = client
        .execute(
            cx,
            ExecuteMode::Read,
            "MATCH (a)-[r:R]->(b) RETURN a.p AS a,b.p AS b,r,r.p AS p ORDER BY a,b,p",
            vec![],
        )
        .await
        .unwrap();
    assert_eq!(vertices.outcome, edges.outcome);
    (vertices, edges)
}
async fn refused(cx: &Cx, client: &mut Client, text: &str) -> (ErrorCode, String) {
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
        "no uncommitted prefix may be delivered"
    );
    match result {
        Err(ClientError::Server { code, message }) => (code, message),
        other => panic!("expected a server refusal: {other:?}"),
    }
}
fn committed(answer: &Answer, seq: u64) {
    assert_eq!(
        answer.outcome,
        Outcome::WriteCommitted { seq, statements: 1 }
    );
}

#[test]
fn tcp_parallel_upsert_groups_once_and_refused_output_never_commits_a_prefix() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "parallel").await;
        let mut observer = fixture.client(cx, 100).await;
        let seeded = write(
            cx,
            &mut observer,
            "CREATE (a {p:1}),(b {p:2}),(a)-[:S]->(b),(a)-[:S]->(b),\
             (a)-[:R {p:10}]->(b),(a)-[:R {p:20}]->(b)",
        )
        .await;
        committed(&seeded, 1);
        let mut writer = fixture.client(cx, 1).await;
        let aggregate = write(cx, &mut writer, &format!("{UPSERT} RETURN max(r.p) AS p")).await;
        committed(&aggregate, 2);
        assert_eq!(aggregate.columns, ["p"]);
        assert_eq!(aggregate.rows, vec![vec![WireValue::Int(22)]]);
        let before = state(cx, &mut observer).await;
        assert_eq!(before.1.rows.len(), 2);
        assert_eq!(before.1.rows[0][3], WireValue::Int(12));
        assert_eq!(before.1.rows[1][3], WireValue::Int(22));
        let (code, _) = refused(cx, &mut writer, &format!("{UPSERT} RETURN r.p AS p")).await;
        assert_eq!(code, ErrorCode::Budget);
        assert_eq!(state(cx, &mut observer).await, before);
        let mut zero = fixture.client(cx, 0).await;
        let (_, message) = refused(
            cx,
            &mut zero,
            &format!("{UPSERT} RETURN 1/(r.p-24) AS bad LIMIT 0"),
        )
        .await;
        assert!(
            message.contains("DivisionByZero") || message.contains("division by zero"),
            "{message}"
        );
        assert_eq!(state(cx, &mut observer).await, before);
        let empty = write(cx, &mut zero, &format!("{UPSERT} RETURN r.p AS p LIMIT 0")).await;
        committed(&empty, 3);
        assert!(empty.rows.is_empty());
        let after = state(cx, &mut observer).await;
        assert_eq!(after.1.rows[0][3], WireValue::Int(14));
        assert_eq!(after.1.rows[1][3], WireValue::Int(24));
        writer.close(cx).await.unwrap();
        zero.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        fixture.stop(cx, 3, 2, &[14, 24]).await;
    });
}

#[test]
fn tcp_multiple_pairs_preserve_duplicate_results_and_parameter_rebinding() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "multiple-pairs").await;
        let mut observer = fixture.client(cx, 100).await;
        let seeded = write(
            cx,
            &mut observer,
            "CREATE (a {p:1}),(b {p:2}),(c {p:3}),\
             (a)-[:S]->(b),(a)-[:S]->(b),(a)-[:S]->(c)",
        )
        .await;
        committed(&seeded, 1);
        let mut writer = fixture.client(cx, 3).await;
        let text = "MATCH (a)-[:S]->(b) MERGE (a)-[r:R]->(b) \
                    ON CREATE SET r.p=7 ON MATCH SET r.p=r.p+$step \
                    RETURN r.p AS p ORDER BY p";
        for (step, expected, seq) in [(1, vec![7, 8, 8], 2), (3, vec![10, 14, 14], 3)] {
            let answer = writer
                .execute(
                    cx,
                    ExecuteMode::Write,
                    text,
                    vec![("step".into(), WireValue::Int(step))],
                )
                .await
                .unwrap();
            committed(&answer, seq);
            assert_eq!(
                answer.rows,
                expected
                    .into_iter()
                    .map(|value| vec![WireValue::Int(value)])
                    .collect::<Vec<_>>()
            );
            let stored = state(cx, &mut observer).await;
            assert_eq!(stored.0.rows.len(), 3);
            assert_eq!(
                stored.1.rows.len(),
                2,
                "duplicate input must not create duplicate edges"
            );
        }
        let before = state(cx, &mut observer).await;
        assert!(
            writer
                .execute(
                    cx,
                    ExecuteMode::Read,
                    text,
                    vec![("step".into(), WireValue::Int(1))],
                )
                .await
                .is_err()
        );
        assert_eq!(state(cx, &mut observer).await, before);
        writer.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        fixture.stop(cx, 3, 3, &[10, 14]).await;
    });
}
