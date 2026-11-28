//! Real WebSocket upgrades carry the ordinary authenticated FGP connection.
//! These laws compare a WebSocket transaction with an independent TCP reader;
//! message framing must not create a second execution or authorization path.

use asupersync::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use asupersync::net::websocket::{ClientHandshake, HttpResponse, WsUrl};
use asupersync::net::{TcpListener, TcpStream};
use asupersync::security::key::AuthKey;
use asupersync::{Budget, Cx};
use fgdb::{Database, DatabaseKeys};
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_gql::GraphSymbolKind;
use fgdb_protocol::body::{ErrorCode, Execute, ExecuteMode, Outcome, PreparedReadMode, WireValue};
use fgdb_protocol::client::{Client, ClientError, WebSocketTarget};
use fgdb_protocol::websocket::WebSocketIo;
use fgdb_server::{
    DatabaseConfig, Server, ServerLimits, Shutdown, Symbols, WebSocketConfig, issue_token, issuer,
};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Grant, QueryLimits, Rights, Scope};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const ORIGIN: &str = "https://console.example";
const PROTOCOL: &str = "fgp.v1";
const CLIENT_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([81; 32], DatabaseSecurityNamespaceId([82; 32]), [83; 32])
}

fn grant(rights: Rights) -> Grant {
    Grant {
        branch: fgdb_server::TRUNK.into(),
        labels: Scope::All,
        relations: Scope::All,
        properties: Scope::All,
        rights,
        limits: QueryLimits {
            max_nodes: 100_000,
            max_work: 10_000_000,
            max_rows: 100_000,
        },
        expires_at_ms: u64::MAX / 2,
    }
}

fn token(grant: &Grant) -> Vec<u8> {
    issue_token(
        &issuer(AuthKey::from_seed(5801), &keys(), 1).unwrap(),
        grant,
    )
    .unwrap()
}

fn run(test: impl AsyncFnOnce(&Cx)) {
    let runtime = fgdb::runtime_builder().build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(test(&cx));
}

async fn start(
    cx: &Cx,
    name: &str,
) -> (
    SocketAddr,
    SocketAddr,
    Shutdown,
    Vec<asupersync::runtime::TaskHandle<()>>,
) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "fgdb-websocket-{}-{epoch}-{}-{name}",
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
            initial_window_rows: 2,
            max_window_bytes: 1 << 20,
            max_window_rows: 1024,
            max_connections: 16,
        },
    )
    .unwrap();
    let mut symbols = Symbols::new();
    for (kind, name, id) in [
        (GraphSymbolKind::Label, "Person", 1),
        (GraphSymbolKind::Label, "Secret", 2),
        (GraphSymbolKind::Property, "name", 1),
        (GraphSymbolKind::Property, "age", 2),
        (GraphSymbolKind::Property, "secret", 3),
    ] {
        symbols.bind(kind, name, id).unwrap();
    }
    let mut config = DatabaseConfig::new("social", keys(), AuthKey::from_seed(5801));
    config.symbols = symbols;
    server.open_database(cx, &path, config).await.unwrap();
    let server = Arc::new(server);
    let shutdown = server.shutdown();
    let websocket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let websocket_address = websocket.local_addr().unwrap();
    let tcp_address = tcp.local_addr().unwrap();
    let websocket_server = Arc::clone(&server);
    let config = WebSocketConfig::new(vec!["127.0.0.1".into()], vec![ORIGIN.into()]).unwrap();
    let websocket_task = cx
        .spawn(move |child| async move {
            websocket_server
                .serve_websocket(&child, websocket, config)
                .await
                .unwrap();
        })
        .unwrap();
    let tcp_task = cx
        .spawn(move |child| async move {
            server.serve(&child, tcp).await.unwrap();
        })
        .unwrap();
    (
        websocket_address,
        tcp_address,
        shutdown,
        vec![websocket_task, tcp_task],
    )
}

fn handshake(address: SocketAddr, origin: Option<&str>) -> ClientHandshake {
    let handshake = ClientHandshake::new_for_test(
        WsUrl::parse(&format!("ws://{address}/v1/fgp")).unwrap(),
        CLIENT_KEY.into(),
        vec![PROTOCOL.into()],
        Vec::new(),
        BTreeMap::new(),
    );
    match origin {
        Some(origin) => handshake.header("Origin", origin),
        None => handshake,
    }
}

// Read exactly the HTTP header boundary. Any following bytes belong to FGP's
// WebSocket transport, and may not disappear into an HTTP parser's buffer.
async fn headers(stream: &mut (impl AsyncRead + Unpin)) -> Vec<u8> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 16 * 1024, "bounded upgrade response");
        let mut byte = [0];
        assert_eq!(stream.read(&mut byte).await.unwrap(), 1, "complete headers");
        bytes.push(byte[0]);
    }
    bytes
}

async fn connect(cx: &Cx, address: SocketAddr, scope: &Grant, origin: Option<&str>) -> Client {
    if origin.is_none() {
        let target = WebSocketTarget::new(address.to_string()).unwrap();
        let mut client = Client::connect_websocket(cx, &target, address, token(scope))
            .await
            .unwrap();
        client.select(cx, "social").await.unwrap();
        return client;
    }
    let handshake = handshake(address, origin);
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(&handshake.request_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let response = HttpResponse::parse(&headers(&mut stream).await).unwrap();
    handshake.validate_response(&response).unwrap();
    assert_eq!(response.header("sec-websocket-protocol"), Some(PROTOCOL));
    assert_eq!(response.header("sec-websocket-extensions"), None);
    let io = WebSocketIo::client(cx, stream, 1 << 20, Vec::new()).unwrap();
    let mut client = Client::connect_stream(cx, io, token(scope)).await.unwrap();
    client.select(cx, "social").await.unwrap();
    client
}

fn statement(mode: ExecuteMode, text: &str) -> Execute {
    Execute {
        mode,
        statement: text.into(),
        parameters: Vec::new(),
    }
}

async fn count(cx: &Cx, client: &mut Client) -> (Vec<Vec<WireValue>>, Outcome) {
    let answer = client
        .execute(
            cx,
            ExecuteMode::Read,
            "MATCH (n) RETURN count(*) AS n",
            Vec::new(),
        )
        .await
        .unwrap();
    (answer.rows, answer.outcome)
}

#[test]
fn websocket_preserves_atomic_transactions_scoped_streams_and_cancellation() {
    run(async |cx| {
        let (address, tcp_address, shutdown, tasks) = start(cx, "execution").await;
        let mut writer = connect(cx, address, &grant(Rights::ReadWrite), None).await;
        let mut observer = Client::connect(cx, tcp_address, token(&grant(Rights::Read)))
            .await
            .unwrap();
        observer.select(cx, "social").await.unwrap();
        let begun = writer.begin_transaction(cx).await.unwrap();
        assert_eq!(begun.basis, 0);
        for text in [
            "UNWIND range(1,6) AS age CREATE (:Person {name:'public',age:age,secret:'hidden'})",
            "CREATE (:Secret {name:'private',age:999,secret:'private'})",
        ] {
            writer
                .execute_transaction(cx, begun.handle, &statement(ExecuteMode::Write, text))
                .await
                .unwrap();
        }
        let staged = writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(ExecuteMode::Read, "MATCH (n) RETURN count(*) AS n"),
            )
            .await
            .unwrap();
        assert_eq!(staged.rows, [vec![WireValue::Count(7)]]);
        assert_eq!(
            count(cx, &mut observer).await,
            (vec![vec![WireValue::Count(0)]], Outcome::Rows { seq: 0 }),
        );
        assert_eq!(
            writer.commit_transaction(cx, begun.handle).await.unwrap(),
            Outcome::WriteCommitted {
                seq: 1,
                statements: 3
            },
        );
        assert_eq!(
            count(cx, &mut observer).await,
            (vec![vec![WireValue::Count(7)]], Outcome::Rows { seq: 1 }),
        );

        let mut scope = grant(Rights::Read);
        scope.labels = Scope::only([LabelId(1)]);
        scope.properties = Scope::only([PropertyKeyId(1), PropertyKeyId(2)]);
        let mut reader = connect(cx, address, &scope, Some(ORIGIN)).await;
        let prepared = reader
            .prepare_read(
                cx,
                "MATCH (n) RETURN n AS id,n.name AS name,n.age AS age,n.secret AS secret",
                Vec::new(),
            )
            .await
            .unwrap();
        let mut ages = Vec::new();
        let outcome = reader
            .execute_prepared_streaming(
                cx,
                PreparedReadMode::SourceStream,
                prepared,
                Vec::new(),
                |columns| {
                    assert_eq!(columns, ["id", "name", "age", "secret"]);
                    Ok(())
                },
                |row| {
                    assert!(matches!(row.first(), Some(WireValue::Vertex(_))));
                    assert_eq!(row[1], WireValue::Text("public".into()));
                    assert_eq!(row[3], WireValue::Null);
                    let WireValue::Int(age) = &row[2] else {
                        panic!("integer age")
                    };
                    ages.push(*age);
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Rows { seq: 1 });
        ages.sort_unstable();
        assert_eq!(ages, [1, 2, 3, 4, 5, 6]);
        // Six result rows cross the two-row credit window. The hidden label
        // and property do not enter results merely because the carrier is WS.
        let mut callbacks = 0;
        let error = reader
            .execute_prepared_streaming(
                cx,
                PreparedReadMode::SourceStream,
                prepared,
                Vec::new(),
                |_| Ok(()),
                |_| {
                    callbacks += 1;
                    Err(ClientError::Protocol("stop after one WebSocket row"))
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ClientError::Protocol("stop after one WebSocket row"),
        ));
        assert_eq!(callbacks, 1);
        reader.ping(cx, 5801).await.unwrap();
        reader.release_prepared(cx, prepared).await.unwrap();
        let error = reader
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (:Person {age:1000})",
                Vec::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ClientError::Server {
                code: ErrorCode::PermissionDenied,
                ..
            },
        ));
        let rolled_back = writer.begin_transaction(cx).await.unwrap();
        writer
            .execute_transaction(
                cx,
                rolled_back.handle,
                &statement(ExecuteMode::Write, "MATCH (n:Person) SET n.age=99"),
            )
            .await
            .unwrap();
        writer
            .rollback_transaction(cx, rolled_back.handle)
            .await
            .unwrap();
        let unchanged = observer
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (n:Person) RETURN n.age AS age ORDER BY age",
                Vec::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            unchanged.rows,
            (1..=6)
                .map(|age| vec![WireValue::Int(age)])
                .collect::<Vec<_>>(),
        );
        assert_eq!(unchanged.outcome, Outcome::Rows { seq: 1 });
        reader.close(cx).await.unwrap();
        writer.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        shutdown.trigger();
        for mut task in tasks {
            task.join(cx).await.unwrap();
        }
    });
}

#[test]
fn websocket_upgrade_refuses_cross_origin_and_ambiguous_requests_before_fgp() {
    run(async |cx| {
        let (address, _, shutdown, tasks) = start(cx, "upgrade-refusals").await;
        let valid = String::from_utf8(handshake(address, Some(ORIGIN)).request_bytes()).unwrap();
        let host = format!("Host: {address}\r\n");
        let origin = format!("Origin: {ORIGIN}\r\n");
        let protocol = format!("Sec-WebSocket-Protocol: {PROTOCOL}\r\n");
        let key = format!("Sec-WebSocket-Key: {CLIENT_KEY}\r\n");
        let cases = [
            (
                "foreign host",
                valid.replace(&host, "Host: attacker.example\r\n"),
            ),
            ("missing host", valid.replace(&host, "")),
            (
                "duplicate host",
                valid.replace(&host, &format!("{host}Host: 127.0.0.1\r\n")),
            ),
            (
                "foreign origin",
                valid.replace(&origin, "Origin: https://attacker.example\r\n"),
            ),
            ("opaque origin", valid.replace(&origin, "Origin: null\r\n")),
            (
                "duplicate origin",
                valid.replace(&origin, &format!("{origin}{origin}")),
            ),
            ("missing subprotocol", valid.replace(&protocol, "")),
            (
                "other subprotocol",
                valid.replace(&protocol, "Sec-WebSocket-Protocol: fgp.v2\r\n"),
            ),
            ("duplicate key", valid.replace(&key, &format!("{key}{key}"))),
            (
                "old HTTP",
                valid.replacen("HTTP/1.1\r\n", "HTTP/1.0\r\n", 1),
            ),
            ("wrong method", valid.replacen("GET ", "POST ", 1)),
            (
                "query credential",
                valid.replacen("/v1/fgp ", "/v1/fgp?token=forbidden ", 1),
            ),
            (
                "body header",
                valid.replace("\r\n\r\n", "\r\nContent-Length: 0\r\n\r\n"),
            ),
            (
                "transfer encoding",
                valid.replace("\r\n\r\n", "\r\nTransfer-Encoding: chunked\r\n\r\n"),
            ),
            (
                "colonless header",
                valid.replace("\r\n\r\n", "\r\nIgnoredWithoutColon\r\n\r\n"),
            ),
        ];
        for (name, request) in cases {
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream.write_all(request.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
            let bytes = headers(&mut stream).await;
            let response = HttpResponse::parse(&bytes).unwrap();
            assert_eq!(response.status, 400, "{name}");
            assert_eq!(response.header("connection"), Some("close"), "{name}");
            assert!(
                !String::from_utf8(bytes).unwrap().contains("social"),
                "database names must not appear before FGP authentication",
            );
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).await.unwrap(), 0, "{name}");
        }
        // Non-browser clients have no Origin. They still require the same
        // Host, upgrade, subprotocol, FGP authentication and database binding.
        let mut native = connect(cx, address, &grant(Rights::Read), None).await;
        let answer = native
            .execute(cx, ExecuteMode::Read, "RETURN 7 AS value", Vec::new())
            .await
            .unwrap();
        assert_eq!(answer.rows, [vec![WireValue::Int(7)]]);
        assert_eq!(answer.outcome, Outcome::Rows { seq: 0 });
        native.close(cx).await.unwrap();
        shutdown.trigger();
        for mut task in tasks {
            task.join(cx).await.unwrap();
        }
    });
}
