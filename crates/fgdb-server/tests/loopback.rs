//! fgdbd end to end over real loopback TCP: a durable database on disk, the
//! FGP handshake, writes and reads through capability-authorized sessions,
//! flow-controlled result streaming, typed refusals, scope masking, and drain.

use asupersync::net::TcpListener;
use asupersync::security::key::AuthKey;
use asupersync::{Budget, Cx};
use fgdb::{Database, DatabaseKeys};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::GraphSymbolKind;
use fgdb_protocol::body::{ErrorCode, ExecuteMode, Outcome, WireValue};
use fgdb_protocol::client::{Client, ClientError};
use fgdb_server::{DatabaseConfig, Server, ServerLimits, Symbols, issue_token, issuer};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Grant, QueryLimits, Rights, Scope};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

fn issuer_key() -> AuthKey {
    AuthKey::from_seed(8501)
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32])
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fgdb-server-loopback-{}-{name}",
        std::process::id()
    ))
}

fn symbols() -> Symbols {
    let mut symbols = Symbols::new();
    for (kind, name, id) in [
        (GraphSymbolKind::Label, "Person", 1),
        (GraphSymbolKind::Label, "Company", 2),
        (GraphSymbolKind::Relation, "KNOWS", 1),
        (GraphSymbolKind::Relation, "WORKS_AT", 2),
        (GraphSymbolKind::Property, "name", 1),
        (GraphSymbolKind::Property, "age", 2),
    ] {
        symbols.bind(kind, name, id).unwrap();
    }
    symbols
}

fn grant(rights: Rights) -> Grant {
    Grant {
        branch: fgdb_server::TRUNK.into(),
        labels: Scope::All,
        relations: Scope::All,
        properties: Scope::All,
        rights,
        limits: QueryLimits {
            max_nodes: 1_000_000,
            max_work: 100_000_000,
            max_rows: 1_000_000,
        },
        expires_at_ms: u64::MAX / 2,
    }
}

fn token(grant: &Grant) -> Vec<u8> {
    let authority = issuer(issuer_key(), &keys(), 1).unwrap();
    issue_token(&authority, grant).unwrap()
}

/// A served database plus a running server task, with a deliberately tiny
/// flow window so every multi-row result needs several WINDOW_UPDATEs.
async fn start(
    cx: &Cx,
    name: &str,
) -> (
    SocketAddr,
    fgdb_server::Shutdown,
    asupersync::runtime::TaskHandle<()>,
) {
    let path = scratch(name);
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    drop(
        Database::create(&contexts.commit(), &path, keys())
            .await
            .unwrap(),
    );
    let limits = ServerLimits {
        max_frame_len: 4096,
        initial_window_bytes: 8192,
        initial_window_rows: 4,
        max_window_bytes: 1 << 20,
        max_window_rows: 1 << 10,
        max_connections: 8,
    };
    let mut server = Server::new(cx, limits).unwrap();
    let mut config = DatabaseConfig::new("social", keys(), issuer_key());
    config.symbols = symbols();
    server.open_database(cx, &path, config).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = Arc::new(server);
    let shutdown = server.shutdown();
    let handle = cx
        .spawn(move |child| async move {
            server.serve(&child, listener).await.unwrap();
        })
        .unwrap();
    (addr, shutdown, handle)
}

fn run(test: impl AsyncFnOnce(&Cx)) {
    let runtime = fgdb::runtime_builder().build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(test(&cx));
}

fn text(value: &str) -> WireValue {
    WireValue::Text(value.into())
}

fn server_code(error: ClientError) -> ErrorCode {
    match error {
        ClientError::Server { code, .. } => code,
        other => panic!("expected a server refusal, got {other:?}"),
    }
}

#[test]
fn writes_and_reads_round_trip_with_flow_control_refusals_and_drain() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "roundtrip").await;

        let mut client = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        // An unknown database and an unauthorized one share one refusal, and
        // the connection stays authenticated for another attempt.
        let refused = client.select(cx, "nonexistent").await.unwrap_err();
        assert_eq!(server_code(refused), ErrorCode::NotFoundOrUnauthorized);
        let selected = client.select(cx, "social").await.unwrap();
        assert_eq!(selected.frontier, 0);
        client.ping(cx, 77).await.unwrap();

        // One write program creates two people and the edge between them.
        let created = client
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (:Person {name: 'Ann', age: 30})-[:KNOWS]->(:Person {name: 'Bob', age: 25})",
                vec![],
            )
            .await
            .unwrap();
        let Outcome::WriteCommitted { seq: first, .. } = created.outcome else {
            panic!("a CREATE commits: {:?}", created.outcome);
        };
        assert_eq!(first, 1);

        // A parameterized batch: a list of maps drives one atomic program.
        let rows = WireValue::List(
            (0..20)
                .map(|i| {
                    WireValue::Map(vec![
                        ("age".into(), WireValue::Int(40 + i)),
                        ("name".into(), text(&format!("p{i:02}"))),
                    ])
                })
                .collect(),
        );
        let batch = client
            .execute(
                cx,
                ExecuteMode::Write,
                "UNWIND $rows AS row CREATE (:Person {name: row.name, age: row.age})",
                vec![("rows".into(), rows)],
            )
            .await
            .unwrap();
        assert_eq!(
            batch.outcome,
            Outcome::WriteCommitted {
                seq: 2,
                statements: 1
            }
        );

        // 22 rows through a 4-row window: the stream needs WINDOW_UPDATEs.
        let all = client
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (p:Person) RETURN p.name AS name, p.age AS age ORDER BY name",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(all.columns, ["name", "age"]);
        assert_eq!(all.outcome, Outcome::Rows { seq: 2 });
        assert_eq!(all.rows.len(), 22);
        assert_eq!(all.rows[0], [text("Ann"), WireValue::Int(30)]);
        assert_eq!(all.rows[1], [text("Bob"), WireValue::Int(25)]);
        assert_eq!(all.rows[21], [text("p19"), WireValue::Int(59)]);

        // A pattern read with a parameter.
        let knows = client
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age >= $min RETURN a.name AS a, b.name AS b",
                vec![("min".into(), WireValue::Int(30))],
            )
            .await
            .unwrap();
        assert_eq!(knows.rows, [[text("Ann"), text("Bob")]]);

        // Typed refusals leave the connection usable.
        let unknown = client
            .execute(cx, ExecuteMode::Read, "MATCH (p:Nobody) RETURN p", vec![])
            .await
            .unwrap_err();
        assert_eq!(server_code(unknown), ErrorCode::Statement);
        let as_read = client
            .execute(
                cx,
                ExecuteMode::Read,
                "CREATE (:Person {name: 'Zed'})",
                vec![],
            )
            .await
            .unwrap_err();
        assert_eq!(server_code(as_read), ErrorCode::Statement);
        let count = client
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (p:Person) RETURN count(p) AS n",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(count.rows, [[WireValue::Count(22)]]);
        client.close(cx).await.unwrap();

        // A read-only capability cannot write, and its refusal is typed.
        let mut reader = Client::connect(cx, addr, token(&grant(Rights::Read)))
            .await
            .unwrap();
        reader.select(cx, "social").await.unwrap();
        let denied = reader
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (:Person {name: 'Eve'})",
                vec![],
            )
            .await
            .unwrap_err();
        assert_eq!(server_code(denied), ErrorCode::PermissionDenied);
        reader.close(cx).await.unwrap();

        // Scope applies before expansion: a token that sees only Company
        // vertices observes no Person at all.
        let companies = Grant {
            labels: Scope::only([LabelId(2)]),
            relations: Scope::only([RelationId(2)]),
            properties: Scope::only([PropertyKeyId(1)]),
            ..grant(Rights::Read)
        };
        let mut scoped = Client::connect(cx, addr, token(&companies)).await.unwrap();
        scoped.select(cx, "social").await.unwrap();
        let masked = scoped
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (n) RETURN count(n) AS n",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(masked.rows, [[WireValue::Count(0)]]);
        scoped.close(cx).await.unwrap();

        // A credential no served issuer accepts fails authentication outright.
        let foreign = issuer(AuthKey::from_seed(1234), &keys(), 1).unwrap();
        let forged = issue_token(&foreign, &grant(Rights::ReadWrite)).unwrap();
        let rejected = Client::connect(cx, addr, forged).await.unwrap_err();
        assert_eq!(server_code(rejected), ErrorCode::Unauthenticated);

        // Drain: an idle connection receives GOODBYE and the server returns.
        let mut idle = Client::connect(cx, addr, token(&grant(Rights::Read)))
            .await
            .unwrap();
        idle.select(cx, "social").await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
        let after = idle
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (p:Person) RETURN count(p) AS n",
                vec![],
            )
            .await
            .unwrap_err();
        assert!(
            matches!(after, ClientError::Closed | ClientError::Transport(_)),
            "{after:?}"
        );
    });
}

#[test]
fn committed_writes_survive_a_server_restart() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "restart").await;
        let mut client = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        client.select(cx, "social").await.unwrap();
        client
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (:Company {name: 'Acme'})",
                vec![],
            )
            .await
            .unwrap();
        client.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();

        // Serve the same directory again from a fresh server process state.
        let limits = ServerLimits::default();
        let mut reopened = Server::new(cx, limits).unwrap();
        let mut config = DatabaseConfig::new("social", keys(), issuer_key());
        config.symbols = symbols();
        reopened
            .open_database(cx, &scratch("restart"), config)
            .await
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let reopened = Arc::new(reopened);
        let shutdown = reopened.shutdown();
        let mut server = cx
            .spawn(move |child| async move {
                reopened.serve(&child, listener).await.unwrap();
            })
            .unwrap();
        let mut client = Client::connect(cx, addr, token(&grant(Rights::Read)))
            .await
            .unwrap();
        let selected = client.select(cx, "social").await.unwrap();
        assert_eq!(selected.frontier, 1);
        let names = client
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (c:Company) RETURN c.name AS name",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(names.rows, [[text("Acme")]]);
        client.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}
