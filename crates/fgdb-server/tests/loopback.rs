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
    let server = Arc::new(served(cx, name).await);
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

/// The fresh on-disk database every test serves, before any listener.
async fn served(cx: &Cx, name: &str) -> Server {
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
    server
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

/// One HTTP/1.1 exchange on a fresh connection: the status and the body.
async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    host: &str,
    token: Option<&[u8]>,
    body: &str,
) -> (u16, String) {
    use asupersync::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut stream = asupersync::net::TcpStream::connect(addr).await.unwrap();
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    if let Some(token) = token {
        let hex: String = token.iter().map(|byte| format!("{byte:02x}")).collect();
        request.push_str(&format!("Authorization: Bearer {hex}\r\n"));
    }
    request.push_str(&format!(
        "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    ));
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8(response).unwrap();
    let status = response
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {response:?}"));
    let body = response
        .split_once("\r\n\r\n")
        .map_or(String::new(), |(_, body)| body.to_owned());
    (status, body)
}

#[test]
fn http_adapter_serves_the_same_authorized_statements() {
    run(async |cx| {
        let path = scratch("http");
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        drop(
            Database::create(&contexts.commit(), &path, keys())
                .await
                .unwrap(),
        );
        let mut server = Server::new(cx, ServerLimits::default()).unwrap();
        let mut config = DatabaseConfig::new("social", keys(), issuer_key());
        config.symbols = symbols();
        server.open_database(cx, &path, config).await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = Arc::new(server);
        let shutdown = server.shutdown();
        let mut handle = cx
            .spawn(move |child| async move {
                server
                    .serve_http(&child, listener, vec!["127.0.0.1".into()])
                    .await
                    .unwrap();
            })
            .unwrap();
        let rw = token(&grant(Rights::ReadWrite));
        let ro = token(&grant(Rights::Read));
        let host = "127.0.0.1";

        let (status, body) = http(addr, "GET", "/v1/health", host, None, "").await;
        assert_eq!((status, body.as_str()), (200, r#"{"v":1,"status":"ok"}"#));

        let (status, body) = http(
            addr,
            "POST",
            "/v1/databases/social/write",
            host,
            Some(&rw),
            r#"{"statement": "UNWIND $rows AS row CREATE (:Person {name: row.name, age: row.age})",
                "parameters": {"rows": [{"name": "Ann", "age": 30}, {"name": "Bob", "age": 25}]}}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body, r#"{"v":1,"seq":1,"statements":1,"committed":true}"#);

        let (status, body) = http(
            addr,
            "POST",
            "/v1/databases/social/query",
            host,
            Some(&ro),
            r#"{"statement": "MATCH (p:Person) WHERE p.age > $min RETURN p.name AS name",
                "parameters": {"min": 26}}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            r#"{"v":1,"columns":["name"],"rows":[[{"type":"text","value":"Ann"}]],"seq":1}"#
        );

        // Typed refusals map onto statuses; none reveals a hidden fact.
        let refusals = [
            (
                "/v1/databases/social/query",
                Some(&ro),
                r#"{"statement": "MATCH (n:Nobody) RETURN n"}"#,
                400,
                "statement",
            ),
            (
                "/v1/databases/social/write",
                Some(&ro),
                r#"{"statement": "CREATE (:Person {name: 'Eve'})"}"#,
                403,
                "permission_denied",
            ),
            (
                "/v1/databases/nope/query",
                Some(&rw),
                r#"{"statement": "MATCH (n) RETURN n"}"#,
                404,
                "not_found_or_unauthorized",
            ),
            (
                "/v1/databases/social/query",
                None,
                r#"{"statement": "MATCH (n) RETURN n"}"#,
                401,
                "unauthenticated",
            ),
            (
                "/v1/databases/social/query",
                Some(&rw),
                r#"{"statment": "typo"}"#,
                400,
                "protocol",
            ),
        ];
        for (path, credential, body, expected, code) in refusals {
            let (status, answer) = http(
                addr,
                "POST",
                path,
                host,
                credential.map(Vec::as_slice),
                body,
            )
            .await;
            assert_eq!(status, expected, "{path} {body}: {answer}");
            assert!(answer.contains(&format!(r#""code":"{code}""#)), "{answer}");
        }

        // A Host the operator did not allow is refused before routing.
        let (status, _) = http(addr, "GET", "/v1/health", "evil.example", None, "").await;
        assert!((400..500).contains(&status), "{status}");

        shutdown.trigger();
        handle.join(cx).await.unwrap();
    });
}

/// Wait (in short real-time sleeps) until `ready` holds.
async fn until(cx: &Cx, mut ready: impl FnMut() -> bool) {
    for _ in 0..2000 {
        if ready() {
            return;
        }
        asupersync::time::sleep(cx.now(), std::time::Duration::from_millis(5)).await;
    }
    panic!("condition not reached");
}

#[test]
fn subscriptions_push_a_baseline_then_exact_deltas_until_cancelled() {
    use fgdb_protocol::client::Change;
    use std::sync::Mutex;
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "subscribe").await;
        let mut writer = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        writer.select(cx, "social").await.unwrap();
        writer
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (:Person {name: 'Ann', age: 30})",
                vec![],
            )
            .await
            .unwrap();

        // A capability that hides anything cannot subscribe: maintained
        // queries are not masked yet, so the refusal precedes registration.
        let mut scoped = Client::connect(
            cx,
            addr,
            token(&Grant {
                labels: Scope::only([LabelId(1)]),
                ..grant(Rights::Read)
            }),
        )
        .await
        .unwrap();
        scoped.select(cx, "social").await.unwrap();
        let refused = scoped
            .subscribe(
                cx,
                "SUBSCRIBE TO MATCH (p:Person) RETURN p.name AS name",
                vec![],
                |_| {},
                |_| Ok(true),
            )
            .await
            .unwrap_err();
        assert_eq!(server_code(refused), ErrorCode::PermissionDenied);

        let changes: Arc<Mutex<Vec<Change>>> = Arc::default();
        let columns: Arc<Mutex<Vec<String>>> = Arc::default();
        let (seen, names) = (Arc::clone(&changes), Arc::clone(&columns));
        let read_token = token(&grant(Rights::Read));
        let mut subscriber = cx
            .spawn(move |child| async move {
                let mut client = Client::connect(&child, addr, read_token).await.unwrap();
                client.select(&child, "social").await.unwrap();
                let end = client
                    .subscribe(
                        &child,
                        "SUBSCRIBE TO MATCH (p:Person) WHERE p.age >= $min RETURN p.name AS name",
                        vec![("min".into(), WireValue::Int(20))],
                        |list| *names.lock().unwrap() = list.to_vec(),
                        |change| {
                            let mut seen = seen.lock().unwrap();
                            // An empty delta reports progress: the frontier
                            // moved and nothing the query returns changed.
                            if change.entries.is_empty() && !change.snapshot {
                                assert!(
                                    seen.last()
                                        .is_some_and(|last| last.frontier < change.frontier)
                                );
                                return Ok(true);
                            }
                            seen.push(change);
                            // Baseline, one insert, a 10-row batch, a delete.
                            Ok(seen.len() < 4)
                        },
                    )
                    .await
                    .unwrap();
                client.close(&child).await.unwrap();
                end
            })
            .unwrap();
        until(cx, || changes.lock().unwrap().len() == 1).await;
        {
            let changes = changes.lock().unwrap();
            assert!(changes[0].snapshot, "the first batch is the baseline");
            assert_eq!(changes[0].frontier, 1);
            assert_eq!(changes[0].entries, [(1, vec![text("Ann")])]);
        }
        assert_eq!(*columns.lock().unwrap(), ["name"]);

        writer
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (:Person {name: 'Bob', age: 25})",
                vec![],
            )
            .await
            .unwrap();
        until(cx, || changes.lock().unwrap().len() == 2).await;
        // Ten people through a four-row window: one batch over several frames.
        let rows = WireValue::List(
            (0..10)
                .map(|i| {
                    WireValue::Map(vec![
                        ("age".into(), WireValue::Int(40 + i)),
                        ("name".into(), text(&format!("p{i}"))),
                    ])
                })
                .collect(),
        );
        writer
            .execute(
                cx,
                ExecuteMode::Write,
                "UNWIND $rows AS row CREATE (:Person {name: row.name, age: row.age})",
                vec![("rows".into(), rows)],
            )
            .await
            .unwrap();
        until(cx, || changes.lock().unwrap().len() == 3).await;
        // A person below the threshold changes nothing the query returns,
        // and a delete retracts exactly one row.
        writer
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (:Person {name: 'Kid', age: 9})",
                vec![],
            )
            .await
            .unwrap();
        writer
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (p:Person {name: 'Ann'}) DETACH DELETE p",
                vec![],
            )
            .await
            .unwrap();
        let end = subscriber.join(cx).await.unwrap();
        {
            let changes = changes.lock().unwrap();
            assert_eq!(changes.len(), 4);
            assert!(!changes[1].snapshot);
            assert_eq!(
                (changes[1].frontier, changes[1].entries.clone()),
                (2, vec![(1, vec![text("Bob")])])
            );
            assert_eq!(changes[2].frontier, 3);
            let mut batch: Vec<_> = changes[2]
                .entries
                .iter()
                .map(|(w, row)| (*w, row[0].clone()))
                .collect();
            batch.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
            assert_eq!(
                batch,
                (0..10)
                    .map(|i| (1, text(&format!("p{i}"))))
                    .collect::<Vec<_>>()
            );
            // The empty change at seq 4 may be coalesced into the delete's batch.
            assert!(
                matches!(changes[3].frontier, 4 | 5),
                "{}",
                changes[3].frontier
            );
            let retracted: Vec<_> = changes[3]
                .entries
                .iter()
                .filter(|(w, _)| *w != 0)
                .cloned()
                .collect();
            assert_eq!(retracted, [(-1, vec![text("Ann")])]);
            assert_eq!(end, changes[3].frontier);
        }

        // A pattern with no RETURN subscribes to its bound variables.
        let mut bare = Client::connect(cx, addr, token(&grant(Rights::Read)))
            .await
            .unwrap();
        bare.select(cx, "social").await.unwrap();
        let mut bound = Vec::new();
        let mut baseline = Vec::new();
        bare.subscribe(
            cx,
            "SUBSCRIBE TO MATCH (p:Person) WHERE p.name = 'Bob';",
            vec![],
            |list| bound = list.to_vec(),
            |change| {
                baseline = change.entries;
                Ok(false)
            },
        )
        .await
        .unwrap();
        assert_eq!(bound, ["p"]);
        assert!(
            matches!(baseline.as_slice(), [(1, row)] if matches!(row.as_slice(), [WireValue::Vertex(_)])),
            "{baseline:?}"
        );
        bare.close(cx).await.unwrap();
        writer.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

/// A minimal Bolt 5.0 client over the fgdb-bolt codec.
struct Bolt {
    stream: asupersync::net::TcpStream,
    dechunker: fgdb_bolt::message::Dechunker,
}

impl Bolt {
    async fn connect(addr: SocketAddr) -> (Self, [u8; 4]) {
        use asupersync::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = asupersync::net::TcpStream::connect(addr).await.unwrap();
        let mut hello = fgdb_bolt::message::MAGIC.to_vec();
        // 5.0 exactly, then 4.4..4.2, as a driver would propose.
        hello.extend_from_slice(&[0, 0, 0, 5, 0, 2, 4, 4, 0, 0, 0, 0, 0, 0, 0, 0]);
        stream.write_all(&hello).await.unwrap();
        let mut version = [0_u8; 4];
        stream.read_exact(&mut version).await.unwrap();
        let bolt = Self {
            stream,
            dechunker: fgdb_bolt::message::Dechunker::new(1 << 24),
        };
        (bolt, version)
    }

    async fn send(&mut self, tag: u8, fields: Vec<fgdb_bolt::packstream::Value>) {
        use asupersync::io::AsyncWriteExt;
        let mut message = Vec::new();
        fgdb_bolt::packstream::encode(
            &fgdb_bolt::packstream::Value::Struct { tag, fields },
            &mut message,
        );
        let mut framed = Vec::new();
        fgdb_bolt::message::frame(&message, &mut framed);
        self.stream.write_all(&framed).await.unwrap();
    }

    /// The next response's tag and fields, or None once the server closed.
    async fn receive(&mut self) -> Option<(u8, Vec<fgdb_bolt::packstream::Value>)> {
        use asupersync::io::AsyncReadExt;
        loop {
            if let Some(message) = self.dechunker.next_message().unwrap() {
                let fgdb_bolt::packstream::Value::Struct { tag, fields } =
                    fgdb_bolt::packstream::decode(&message).unwrap()
                else {
                    panic!("a response is a structure");
                };
                return Some((tag, fields));
            }
            let mut buffer = [0_u8; 4096];
            let read = self.stream.read(&mut buffer).await.ok()?;
            if read == 0 {
                return None;
            }
            self.dechunker.push(&buffer[..read]);
        }
    }

    /// Send and expect one response of `want`'s tag; returns its metadata.
    async fn expect(
        &mut self,
        tag: u8,
        fields: Vec<fgdb_bolt::packstream::Value>,
        want: u8,
    ) -> Vec<(String, fgdb_bolt::packstream::Value)> {
        self.send(tag, fields).await;
        let (got, fields) = self.receive().await.expect("a response");
        assert_eq!(got, want, "{fields:?}");
        match fields.into_iter().next() {
            Some(fgdb_bolt::packstream::Value::Map(metadata)) => metadata,
            _ => Vec::new(),
        }
    }

    /// PULL everything: the records, then the final metadata.
    async fn pull_all(
        &mut self,
    ) -> (
        Vec<Vec<fgdb_bolt::packstream::Value>>,
        Vec<(String, fgdb_bolt::packstream::Value)>,
    ) {
        use fgdb_bolt::packstream::Value;
        self.send(0x3F, vec![Value::Map(vec![("n".into(), Value::Int(-1))])])
            .await;
        let mut records = Vec::new();
        loop {
            match self.receive().await.expect("a response") {
                (0x71, mut fields) => {
                    let Some(Value::List(values)) = fields.pop() else {
                        panic!("a record holds a list");
                    };
                    records.push(values);
                }
                (0x70, mut fields) => {
                    let Some(Value::Map(metadata)) = fields.pop() else {
                        panic!("SUCCESS holds a map");
                    };
                    return (records, metadata);
                }
                other => panic!("unexpected response {other:?}"),
            }
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn bolt_drivers_read_hydrated_nodes_in_pinned_transactions_and_writes_refuse() {
    use fgdb_bolt::packstream::{Value, get};
    const SUCCESS: u8 = 0x70;
    const IGNORED: u8 = 0x7E;
    const FAILURE: u8 = 0x7F;
    let hello = |token: &[u8]| {
        vec![Value::Map(vec![
            ("user_agent".into(), Value::string("loopback/1")),
            ("scheme".into(), Value::string("bearer")),
            ("credentials".into(), Value::string(hex(token))),
        ])]
    };
    let run_message = |query: &str, parameters: Vec<(String, Value)>| {
        vec![
            Value::string(query),
            Value::Map(parameters),
            Value::Map(Vec::new()),
        ]
    };
    let code = |metadata: &[(String, Value)]| {
        get(metadata, "code")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    run(async |cx| {
        let server = Arc::new(served(cx, "bolt").await);
        let fgp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bolt = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (fgp_addr, bolt_addr) = (fgp.local_addr().unwrap(), bolt.local_addr().unwrap());
        let shutdown = server.shutdown();
        let mut serving = Vec::new();
        for (listener, as_bolt) in [(fgp, false), (bolt, true)] {
            let server = Arc::clone(&server);
            serving.push(
                cx.spawn(move |child| async move {
                    if as_bolt {
                        server.serve_bolt(&child, listener).await.unwrap();
                    } else {
                        server.serve(&child, listener).await.unwrap();
                    }
                })
                .unwrap(),
            );
        }
        let mut writer = Client::connect(cx, fgp_addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        writer.select(cx, "social").await.unwrap();
        writer
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (:Person {name: 'Ann', age: 30}), (:Person {name: 'Bob'})",
                vec![],
            )
            .await
            .unwrap();

        // A token no database accepts is refused at HELLO and disconnected.
        let (mut stranger, version) = Bolt::connect(bolt_addr).await;
        assert_eq!(version, [0, 0, 0, 5]);
        let foreign = issue_token(
            &issuer(AuthKey::from_seed(4242), &keys(), 1).unwrap(),
            &grant(Rights::Read),
        )
        .unwrap();
        let refused = stranger.expect(0x01, hello(&foreign), FAILURE).await;
        assert_eq!(code(&refused), "Neo.ClientError.Security.Unauthorized");
        assert!(stranger.receive().await.is_none());

        let (mut client, _) = Bolt::connect(bolt_addr).await;
        let ready = client
            .expect(0x01, hello(&token(&grant(Rights::Read))), SUCCESS)
            .await;
        assert_eq!(
            get(&ready, "fgdb_profile").and_then(Value::as_str),
            Some("BoltCompatProfileV1")
        );

        // A vertex comes back as a node with its labels and properties.
        let fields = client
            .expect(
                0x10,
                run_message(
                    "MATCH (p:Person) WHERE p.name = $n RETURN p, p.age AS age",
                    vec![("n".into(), Value::string("Ann"))],
                ),
                SUCCESS,
            )
            .await;
        assert_eq!(
            get(&fields, "fields"),
            Some(&Value::List(vec![Value::string("p"), Value::string("age")]))
        );
        let (records, done) = client.pull_all().await;
        let [record] = records.as_slice() else {
            panic!("one record: {records:?}");
        };
        let Value::Struct { tag: 0x4E, fields } = &record[0] else {
            panic!("a node: {record:?}");
        };
        assert_eq!(fields[1], Value::List(vec![Value::string("Person")]));
        // Properties arrive in name order, deterministically.
        assert_eq!(
            fields[2],
            Value::Map(vec![
                ("age".into(), Value::Int(30)),
                ("name".into(), Value::string("Ann")),
            ])
        );
        assert_eq!(record[1], Value::Int(30));
        assert_eq!(get(&done, "type").and_then(Value::as_str), Some("r"));
        assert!(get(&done, "bookmark").is_some());

        // A write is refused before graph access; the failure state ignores
        // further requests until RESET.
        let failure = client
            .expect(
                0x10,
                run_message("CREATE (:Person {name: 'Eve'})", vec![]),
                FAILURE,
            )
            .await;
        assert_eq!(code(&failure), "Neo.ClientError.Statement.AccessMode");
        client
            .expect(0x10, run_message("RETURN 1 AS one", vec![]), IGNORED)
            .await;
        client.expect(0x0F, vec![], SUCCESS).await;

        // An explicit transaction reads one pinned generation: a commit
        // landing mid-transaction is invisible until the next statement
        // outside it.
        let count = "MATCH (p:Person) RETURN count(p) AS people";
        client
            .expect(0x11, vec![Value::Map(Vec::new())], SUCCESS)
            .await;
        client
            .expect(0x10, run_message(count, vec![]), SUCCESS)
            .await;
        assert_eq!(client.pull_all().await.0, [[Value::Int(2)]]);
        writer
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (:Person {name: 'Cy'})",
                vec![],
            )
            .await
            .unwrap();
        client
            .expect(0x10, run_message(count, vec![]), SUCCESS)
            .await;
        assert_eq!(client.pull_all().await.0, [[Value::Int(2)]]);
        let committed = client.expect(0x12, vec![], SUCCESS).await;
        assert!(get(&committed, "bookmark").is_some());
        client
            .expect(0x10, run_message(count, vec![]), SUCCESS)
            .await;
        assert_eq!(client.pull_all().await.0, [[Value::Int(3)]]);

        // A relationship value has no encoding in the profile.
        writer
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (a:Person {name: 'Ann'}), (b:Person {name: 'Bob'}) CREATE (a)-[:KNOWS]->(b)",
                vec![],
            )
            .await
            .unwrap();
        let failure = client
            .expect(
                0x10,
                run_message("MATCH (a)-[r:KNOWS]->(b) RETURN r", vec![]),
                FAILURE,
            )
            .await;
        assert_eq!(
            code(&failure),
            "Neo.ClientError.Statement.FeatureNotSupported"
        );
        client.expect(0x0F, vec![], SUCCESS).await;
        client.send(0x02, vec![]).await;
        assert!(client.receive().await.is_none());

        // A capability that may not see `age` gets nodes without it.
        let (mut scoped, _) = Bolt::connect(bolt_addr).await;
        let narrow = Grant {
            properties: Scope::only([PropertyKeyId(1)]),
            ..grant(Rights::Read)
        };
        scoped.expect(0x01, hello(&token(&narrow)), SUCCESS).await;
        scoped
            .expect(
                0x10,
                run_message("MATCH (p:Person) WHERE p.name = 'Ann' RETURN p", vec![]),
                SUCCESS,
            )
            .await;
        let (records, _) = scoped.pull_all().await;
        let Value::Struct { fields, .. } = &records[0][0] else {
            panic!("a node");
        };
        assert_eq!(
            fields[2],
            Value::Map(vec![("name".into(), Value::string("Ann"))])
        );

        writer.close(cx).await.unwrap();
        shutdown.trigger();
        for mut handle in serving {
            handle.join(cx).await.unwrap();
        }
    });
}

/// `CREATE/INSERT ... RETURN` answers its projected rows with the commit,
/// including a MATCH-selected creation; it needs ReadWrite rights, since the
/// rows can read matched data.
#[test]
fn writes_that_return_answer_their_rows_with_the_commit() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "returning").await;
        let mut client = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        client.select(cx, "social").await.unwrap();
        let answer = client
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (p:Person {name: $n, age: 41}) RETURN p.name AS name, p.age AS age",
                vec![("n".into(), text("Ann"))],
            )
            .await
            .unwrap();
        assert_eq!(answer.columns, ["name", "age"]);
        assert_eq!(answer.rows, [[text("Ann"), WireValue::Int(41)]]);
        assert!(matches!(
            answer.outcome,
            Outcome::WriteCommitted { statements: 1, .. }
        ));
        let answer = client
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (a:Person {name: 'Ann'}) CREATE (a)-[:KNOWS]->(b:Person {name: 'Bob'}) \
                 RETURN a.name AS from, b.name AS to",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(answer.rows, [[text("Ann"), text("Bob")]]);
        // The creation is durable and visible to an ordinary read.
        let read = client
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name AS a, b.name AS b",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(read.rows, [[text("Ann"), text("Bob")]]);
        client.close(cx).await.unwrap();

        let mut writer = Client::connect(cx, addr, token(&grant(Rights::Write)))
            .await
            .unwrap();
        writer.select(cx, "social").await.unwrap();
        let refused = writer
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (p:Person {name: 'Eve'}) RETURN p.name",
                vec![],
            )
            .await
            .unwrap_err();
        assert_eq!(server_code(refused), ErrorCode::PermissionDenied);
        writer.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}
