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
use fgdb_server::{
    DatabaseConfig, Server, ServerLimits, Symbols, attenuate_token, issue_token, issuer,
};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Grant, QueryLimits, Restriction, Rights, Scope};
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
fn fgp_refresh_narrows_live_authority_and_rejects_restoration_without_losing_the_session() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "refresh-scope").await;
        let original = token(&grant(Rights::ReadWrite));
        let mut client = Client::connect(cx, addr, original.clone()).await.unwrap();
        let selected = client.select(cx, "social").await.unwrap();
        client
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (:Person {name:'Ann',age:30}), (:Company {name:'Hidden',age:99})",
                vec![],
            )
            .await
            .unwrap();
        let narrow = token(&Grant {
            labels: Scope::only([LabelId(1)]),
            properties: Scope::only([PropertyKeyId(1)]),
            ..grant(Rights::Read)
        });
        let session = client.refresh_authority(cx, narrow.clone()).await.unwrap();
        assert_eq!(session.transcript, selected.binding.session.transcript);
        assert_eq!(
            session.auth_generation,
            selected.binding.session.auth_generation + 1
        );
        let result = client
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (n) RETURN n.name AS name,n.age AS age ORDER BY name",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(result.rows, [vec![text("Ann"), WireValue::Null]]);
        assert_eq!(
            server_code(
                client
                    .execute(
                        cx,
                        ExecuteMode::Write,
                        "CREATE (:Person {name:'forbidden'})",
                        vec![]
                    )
                    .await
                    .unwrap_err()
            ),
            ErrorCode::PermissionDenied
        );
        for replacement in [original, vec![0xff, 0, 1]] {
            assert_eq!(
                server_code(client.refresh_authority(cx, replacement).await.unwrap_err()),
                ErrorCode::Unauthenticated
            );
        }
        let unchanged = client.refresh_authority(cx, narrow).await.unwrap();
        assert_eq!(unchanged.transcript, session.transcript);
        assert_eq!(unchanged.auth_generation, session.auth_generation + 1);
        let result = client
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (n) RETURN count(*) AS n",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(result.rows, [[WireValue::Count(1)]]);
        assert_eq!(result.outcome, Outcome::Rows { seq: 1 });
        client.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn fgp_refresh_before_selection_preserves_the_bound_subject_and_enforces_new_row_limits() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "refresh-before-select").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (:Person {name:'Ann'}), (:Person {name:'Bob'})",
                vec![],
            )
            .await
            .unwrap();
        let root = fgdb_warden::CapabilityToken::decode(&token(&grant(Rights::Read))).unwrap();
        let narrowed = root
            .attenuate(fgdb_warden::Restriction::MaxRows(1))
            .unwrap()
            .encode();
        let mut client = Client::connect(cx, addr, root.encode()).await.unwrap();
        let refreshed = client.refresh_authority(cx, narrowed).await.unwrap();
        assert_eq!(refreshed.auth_generation, 2);
        let selected = client.select(cx, "social").await.unwrap();
        assert_eq!(selected.binding.session, refreshed);
        assert_eq!(selected.frontier, 1);
        assert_eq!(
            server_code(
                client
                    .execute(
                        cx,
                        ExecuteMode::Read,
                        "MATCH (n:Person) RETURN n.name AS name",
                        vec![]
                    )
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Budget
        );
        // Each execution has its own narrowed allowance; refresh didn't mint
        // a session-lifetime quota or leave the budget refusal as partial rows.
        let one = client
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (n:Person) RETURN n.name AS name ORDER BY name LIMIT 1",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(one.rows, [[text("Ann")]]);
        client.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn fgp_refresh_waits_for_child_quiescence_then_fences_old_headers() {
    use asupersync::io::AsyncWrite;
    use fgdb_protocol::body::{
        Auth, AuthOk, AuthRefresh, AuthRefreshed, Body, Credential, Empty, ErrorBody, Execute,
        Hello, Ready, SelectDatabase,
    };
    use fgdb_protocol::transport::{FrameReader, FrameWriter};
    use fgdb_protocol::{Binding, Frame, FrameKind, FrameLimits, StreamId};

    async fn send<W: AsyncWrite + Unpin>(
        writer: &mut FrameWriter<W>,
        cx: &Cx,
        kind: FrameKind,
        request: u64,
        stream: StreamId,
        binding: Binding,
        body: &impl Body,
    ) {
        let frame = Frame::new(
            kind,
            request,
            stream,
            binding,
            body.encode().unwrap(),
            FrameLimits::new(4096).unwrap(),
        )
        .unwrap();
        writer.queue(cx, &frame).unwrap();
        writer.send(cx, |_| Ok(())).await.unwrap();
    }

    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "refresh-busy").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        owner.execute(cx, ExecuteMode::Write,
            "CREATE (:Person {age:1}),(:Person {age:2}),(:Person {age:3}),(:Person {age:4}),(:Person {age:5}),(:Person {age:6})", vec![]).await.unwrap();
        let original = fgdb_warden::CapabilityToken::decode(&token(&grant(Rights::Read))).unwrap();
        let narrowed = original
            .attenuate(fgdb_warden::Restriction::MaxRows(1))
            .unwrap()
            .encode();
        let socket = asupersync::net::TcpStream::connect(addr).await.unwrap();
        let (read, write) = socket.into_split();
        let limits = FrameLimits::new(4096).unwrap();
        let mut reader = FrameReader::new(read, limits);
        let mut writer = FrameWriter::new(write, limits);
        send(
            &mut writer,
            cx,
            FrameKind::Hello,
            1,
            StreamId::CONTROL,
            Binding::Transport,
            &Hello {
                min_version: fgdb_protocol::PROTOCOL_VERSION,
                max_version: fgdb_protocol::PROTOCOL_VERSION,
                client_nonce: [3; 32],
                max_frame_len: 4096,
            },
        )
        .await;
        assert_eq!(
            reader
                .receive(cx, |_| Ok(()))
                .await
                .unwrap()
                .unwrap()
                .header()
                .kind(),
            FrameKind::HelloAck
        );
        send(
            &mut writer,
            cx,
            FrameKind::Auth,
            2,
            StreamId::CONTROL,
            Binding::Transport,
            &Auth {
                credential: Credential::WardenCapability(original.encode()),
            },
        )
        .await;
        let auth = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
        let session = AuthOk::decode(auth.payload()).unwrap().session;
        send(
            &mut writer,
            cx,
            FrameKind::SelectDatabase,
            3,
            StreamId::CONTROL,
            Binding::Session(session),
            &SelectDatabase {
                name: "social".into(),
            },
        )
        .await;
        let ready = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
        let ready = Ready::decode(ready.payload()).unwrap().binding(session);
        let binding = Binding::Ready(ready);
        send(
            &mut writer,
            cx,
            FrameKind::Execute,
            4,
            StreamId::CONTROL,
            binding,
            &Execute {
                mode: ExecuteMode::Read,
                statement: "MATCH (n:Person) RETURN n.age AS age ORDER BY age".into(),
                parameters: vec![],
            },
        )
        .await;
        let first = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
        assert_eq!(first.header().kind(), FrameKind::SnapshotResultChunk);
        let child = first.header().stream_id();
        send(
            &mut writer,
            cx,
            FrameKind::AuthRefresh,
            5,
            StreamId::CONTROL,
            binding,
            &AuthRefresh {
                credential: Credential::WardenCapability(narrowed.clone()),
            },
        )
        .await;
        loop {
            let frame = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
            assert_eq!(frame.header().binding(), binding);
            if frame.header().request_id() == 5 {
                assert_eq!(frame.header().kind(), FrameKind::Error);
                assert_eq!(
                    ErrorBody::decode(frame.payload()).unwrap().code,
                    ErrorCode::Busy
                );
                break;
            }
            assert_eq!(frame.header().kind(), FrameKind::SnapshotResultChunk);
        }
        send(
            &mut writer,
            cx,
            FrameKind::QueryCancel,
            6,
            child,
            binding,
            &Empty,
        )
        .await;
        let cancelled = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
        assert_eq!(cancelled.header().stream_id(), child);
        assert_eq!(
            ErrorBody::decode(cancelled.payload()).unwrap().code,
            ErrorCode::Cancelled
        );
        send(
            &mut writer,
            cx,
            FrameKind::AuthRefresh,
            7,
            StreamId::CONTROL,
            binding,
            &AuthRefresh {
                credential: Credential::WardenCapability(narrowed),
            },
        )
        .await;
        let refreshed = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
        assert_eq!(refreshed.header().kind(), FrameKind::AuthRefreshed);
        let next = AuthRefreshed::decode(refreshed.payload()).unwrap().session;
        assert_eq!(next.auth_generation, session.auth_generation + 1);
        assert_eq!(
            refreshed.header().binding(),
            Binding::Ready(fgdb_protocol::ReadyBinding {
                session: next,
                ..ready
            })
        );
        // The old header is rejected before its body can authorize anything.
        send(
            &mut writer,
            cx,
            FrameKind::Ping,
            8,
            StreamId::CONTROL,
            binding,
            &fgdb_protocol::body::Ping { nonce: 55 },
        )
        .await;
        assert!(reader.receive(cx, |_| Ok(())).await.unwrap().is_none());
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn top_level_map_parameters_round_trip_and_drive_atomic_fgp_writes() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "map-parameters").await;
        let mut client = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        client.select(cx, "social").await.unwrap();
        let payload = WireValue::Map(vec![
            ("label".into(), text("input")),
            (
                "rows".into(),
                WireValue::List(vec![
                    WireValue::Map(vec![
                        ("age".into(), WireValue::Int(30)),
                        ("name".into(), text("Ann")),
                    ]),
                    WireValue::Map(vec![
                        ("age".into(), WireValue::Int(25)),
                        ("name".into(), text("Bob")),
                    ]),
                ]),
            ),
        ]);
        let read = client.execute(cx, ExecuteMode::Read,
            "RETURN $payload AS payload, $payload.rows[0].name AS name, $payload.missing AS absent",
            vec![("payload".into(), payload.clone())]).await.unwrap();
        assert_eq!(
            read.rows,
            [vec![payload.clone(), text("Ann"), WireValue::Null]]
        );
        let statement = "UNWIND $payload.rows AS row CREATE (n:Person {name:row.name, age:row.age}) RETURN n.name AS name, $payload.label AS label ORDER BY name";
        let result = client
            .execute(
                cx,
                ExecuteMode::Write,
                statement,
                vec![("payload".into(), payload)],
            )
            .await
            .unwrap();
        assert_eq!(
            result.rows,
            [
                vec![text("Ann"), text("input")],
                vec![text("Bob"), text("input")]
            ]
        );
        assert_eq!(
            result.outcome,
            Outcome::WriteCommitted {
                seq: 1,
                statements: 1
            }
        );
        let bad = WireValue::Map(vec![
            ("label".into(), text("refused")),
            (
                "rows".into(),
                WireValue::List(vec![
                    WireValue::Map(vec![("name".into(), text("must not commit"))]),
                    WireValue::Map(vec![("name".into(), WireValue::Map(vec![]))]),
                ]),
            ),
        ]);
        let error = client
            .execute(
                cx,
                ExecuteMode::Write,
                statement,
                vec![("payload".into(), bad)],
            )
            .await
            .unwrap_err();
        assert_eq!(server_code(error), ErrorCode::Statement);
        let count = client
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (n:Person) RETURN count(n) AS n",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(count.rows, [[WireValue::Count(2)]]);
        assert_eq!(count.outcome, Outcome::Rows { seq: 1 });
        client.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn http_map_parameters_bind_nested_bulk_input_and_return_typed_maps() {
    run(async |cx| {
        let server = Arc::new(served(cx, "map-http").await);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = server.shutdown();
        let mut task = cx
            .spawn(move |child| async move {
                server
                    .serve_http(&child, listener, vec!["127.0.0.1".into()])
                    .await
                    .unwrap();
            })
            .unwrap();
        let rw = token(&grant(Rights::ReadWrite));
        let (status, body) = http(addr, "POST", "/v1/databases/social/write", "127.0.0.1", Some(&rw),
            r#"{"statement":"UNWIND $payload.rows AS row CREATE (n:Person {name:row.name,age:row.age}) RETURN n.name AS name, $payload.meta AS meta ORDER BY name","parameters":{"payload":{"rows":[{"name":"Ann","age":30},{"name":"Bob","age":25}],"meta":{"z":2,"a":1}}}}"#).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            r#"{"v":1,"columns":["name","meta"],"rows":[[{"type":"text","value":"Ann"},{"type":"map","value":{"a":{"type":"int","value":"1"},"z":{"type":"int","value":"2"}}}],[{"type":"text","value":"Bob"},{"type":"map","value":{"a":{"type":"int","value":"1"},"z":{"type":"int","value":"2"}}}]],"seq":1,"statements":1,"committed":true}"#
        );
        let (status, body) = http(addr, "POST", "/v1/databases/social/query", "127.0.0.1", Some(&rw),
            r#"{"statement":"MATCH (n:Person) RETURN n.name AS name, $payload.meta.value AS value ORDER BY name","parameters":{"payload":{"meta":{"value":7}}}}"#).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            r#"{"v":1,"columns":["name","value"],"rows":[[{"type":"text","value":"Ann"},{"type":"int","value":"7"}],[{"type":"text","value":"Bob"},{"type":"int","value":"7"}]],"seq":1}"#
        );
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}

#[test]
fn fgp_map_fields_select_and_update_through_native_scalar_expressions() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "map-field-expressions").await;
        let mut client = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        client.select(cx, "social").await.unwrap();
        client
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (:Person {name:'Ann',age:30})",
                vec![],
            )
            .await
            .unwrap();
        let arguments = |delta| {
            vec![(
                "patch".into(),
                WireValue::Map(vec![
                    ("delta".into(), delta),
                    ("name".into(), text("Ann")),
                    (
                        "nested".into(),
                        WireValue::Map(vec![("tag".into(), text("ok"))]),
                    ),
                ]),
            )]
        };
        let read = client
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (n:Person) WHERE n.name=$patch.name RETURN n.age+$patch.delta AS age",
                arguments(WireValue::Int(2)),
            )
            .await
            .unwrap();
        assert_eq!(read.rows, [[WireValue::Int(32)]]);
        assert_eq!(read.outcome, Outcome::Rows { seq: 1 });
        let update = "MATCH (n:Person) WHERE n.name=$patch.name SET n.age=n.age+$patch.delta RETURN n.age AS age, upper($patch.nested.tag) AS tag, $patch.nested AS metadata";
        let changed = client
            .execute(cx, ExecuteMode::Write, update, arguments(WireValue::Int(2)))
            .await
            .unwrap();
        assert_eq!(
            changed.rows,
            [vec![
                WireValue::Int(32),
                text("OK"),
                WireValue::Map(vec![("tag".into(), text("ok"))])
            ]]
        );
        assert_eq!(
            changed.outcome,
            Outcome::WriteCommitted {
                seq: 2,
                statements: 1
            }
        );
        let refused = client
            .execute(
                cx,
                ExecuteMode::Write,
                update,
                arguments(WireValue::Bool(true)),
            )
            .await
            .unwrap_err();
        assert_eq!(server_code(refused), ErrorCode::Statement);
        let after = client
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (n:Person) RETURN n.age AS age",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(after.rows, [[WireValue::Int(32)]]);
        assert_eq!(after.outcome, Outcome::Rows { seq: 2 });
        client.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn http_map_fields_bind_where_set_and_return_without_parameter_substitution() {
    run(async |cx| {
        let server = Arc::new(served(cx, "map-field-http").await);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = server.shutdown();
        let mut task = cx
            .spawn(move |child| async move {
                server
                    .serve_http(&child, listener, vec!["127.0.0.1".into()])
                    .await
                    .unwrap();
            })
            .unwrap();
        let rw = token(&grant(Rights::ReadWrite));
        let (status, body) = http(
            addr,
            "POST",
            "/v1/databases/social/write",
            "127.0.0.1",
            Some(&rw),
            r#"{"statement":"CREATE (:Person {name:'Ann',age:30})"}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let (status, body) = http(addr, "POST", "/v1/databases/social/write", "127.0.0.1", Some(&rw),
            r#"{"statement":"MATCH (n:Person) WHERE n.name=$patch.name SET n.age=n.age+$patch.delta RETURN n.age AS age, upper($patch.tag) AS tag","parameters":{"patch":{"name":"Ann","delta":2,"tag":"ok"}}}"#).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            r#"{"v":1,"columns":["age","tag"],"rows":[[{"type":"int","value":"32"},{"type":"text","value":"OK"}]],"seq":2,"statements":1,"committed":true}"#
        );
        let (status, body) = http(addr, "POST", "/v1/databases/social/query", "127.0.0.1", Some(&rw),
            r#"{"statement":"MATCH (n:Person) WHERE n.age=$patch.age RETURN n.name AS name","parameters":{"patch":{"age":32}}}"#).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            r#"{"v":1,"columns":["name"],"rows":[[{"type":"text","value":"Ann"}]],"seq":2}"#
        );
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
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
        // EXPLAIN reads through the authorized session (fgdb-ooiik): the
        // listing derives from the text, and the certificate form refuses.
        let explained = client
            .execute(
                cx,
                ExecuteMode::Read,
                "EXPLAIN MATCH (p:Person) RETURN p.name AS name",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(explained.columns, ["operator", "detail"]);
        assert_eq!(explained.outcome, Outcome::Rows { seq: 2 });
        assert_eq!(explained.rows[0][0], text("NativeRead"));
        let certificate = client
            .execute(
                cx,
                ExecuteMode::Read,
                "EXPLAIN (CERTIFICATE) MATCH (p:Person) RETURN p.name AS name",
                vec![],
            )
            .await
            .unwrap_err();
        assert_eq!(server_code(certificate), ErrorCode::Statement);
        // id()/elementId() refuse under any capability (fgdb-j687q), on the
        // read session and on the server's own RETURNING write parser, which
        // runs before any session sees the text. Nothing is created.
        for (mode, text) in [
            (ExecuteMode::Read, "MATCH (p:Person) RETURN id(p) AS i"),
            (
                ExecuteMode::Write,
                "CREATE (p:Person {name:'Id'}) RETURN elementId(p) AS e",
            ),
        ] {
            let refused = client.execute(cx, mode, text, vec![]).await.unwrap_err();
            assert_eq!(server_code(refused), ErrorCode::PermissionDenied, "{text}");
        }
        let unchanged = client
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (p:Person) RETURN count(p) AS n",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(unchanged.rows, [[WireValue::Count(22)]]);
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

/// Run `fgdbd attenuate <flags>` with `stdin`; return (exit code, stdout).
fn fgdbd_attenuate(flags: &[&str], stdin: &str) -> (Option<i32>, String) {
    use std::io::Write as _;
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_fgdbd"))
        .arg("attenuate")
        .args(flags)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    (
        output.status.code(),
        String::from_utf8(output.stdout).unwrap(),
    )
}

#[test]
fn attenuated_tokens_only_narrow_compose_and_need_no_key() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "attenuate").await;
        let full = token(&grant(Rights::ReadWrite));
        let mut writer = Client::connect(cx, addr, full.clone()).await.unwrap();
        writer.select(cx, "social").await.unwrap();
        writer
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (:Person {name: 'Ann', age: 30})-[:KNOWS]->(:Person {name: 'Bob', age: 25})",
                vec![],
            )
            .await
            .unwrap();
        writer.close(cx).await.unwrap();
        let count = async |token: Vec<u8>| {
            let mut client = Client::connect(cx, addr, token).await.unwrap();
            client.select(cx, "social").await.unwrap();
            let people = client
                .execute(
                    cx,
                    ExecuteMode::Read,
                    "MATCH (n) RETURN count(n) AS n",
                    vec![],
                )
                .await
                .unwrap();
            let denied = client
                .execute(
                    cx,
                    ExecuteMode::Write,
                    "CREATE (:Person {name: 'Eve'})",
                    vec![],
                )
                .await
                .map_err(server_code);
            client.close(cx).await.unwrap();
            (people.rows, denied.err())
        };

        // Through the binary, as an operator delegates: no key file, a hex
        // token on stdin, a narrower hex token on stdout.
        let (code, out) = fgdbd_attenuate(&["--rights", "read"], &hex(&full));
        assert_eq!(code, Some(0));
        let read_only = fgdb_protocol::json::bytes_from_hex(out.trim()).unwrap();
        assert_eq!(
            count(read_only.clone()).await,
            (
                vec![vec![WireValue::Count(2)]],
                Some(ErrorCode::PermissionDenied)
            )
        );

        // A caveat can only narrow: asking a read-only token for read-write
        // leaves it read-only.
        let asked_wider =
            attenuate_token(&read_only, &[Restriction::Rights(Rights::ReadWrite)]).unwrap();
        assert_eq!(
            count(asked_wider).await.1,
            Some(ErrorCode::PermissionDenied)
        );

        // Scopes compose by intersection: Person+Company, then Company alone,
        // observes no Person vertex at all.
        let both = attenuate_token(
            &full,
            &[Restriction::Labels(Scope::only([LabelId(1), LabelId(2)]))],
        )
        .unwrap();
        assert_eq!(count(both.clone()).await.0, vec![vec![WireValue::Count(2)]]);
        let (code, out) = fgdbd_attenuate(&["--allow-label", "2"], &hex(&both));
        assert_eq!(code, Some(0));
        let company = fgdb_protocol::json::bytes_from_hex(out.trim()).unwrap();
        assert_eq!(count(company).await.0, vec![vec![WireValue::Count(0)]]);

        // Refusals print nothing: no flag, a malformed token, junk on stdin.
        assert_eq!(fgdbd_attenuate(&[], &hex(&full)), (Some(2), String::new()));
        assert_eq!(
            fgdbd_attenuate(&["--rights", "read"], "abcd"),
            (Some(2), String::new())
        );
        assert_eq!(
            fgdbd_attenuate(&["--rights", "read"], "not hex"),
            (Some(2), String::new())
        );
        assert!(attenuate_token(b"junk", &[Restriction::MaxRows(1)]).is_err());

        shutdown.trigger();
        server.join(cx).await.unwrap();
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

#[test]
fn mutation_returning_is_atomic_and_capability_masked_over_fgp() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "mutation-returning").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (:Person {name: 'Ann', age: 30}), (:Person {name: 'Bob', age: 25})",
                vec![],
            )
            .await
            .unwrap();
        let answer = owner.execute(cx, ExecuteMode::Write,
            "MATCH (p:Person) SET p.age = p.age + 1 RETURN p.name AS name, p.age AS age ORDER BY name", vec![])
            .await.unwrap();
        assert_eq!(answer.columns, ["name", "age"]);
        assert_eq!(
            answer.rows,
            [
                vec![text("Ann"), WireValue::Int(31)],
                vec![text("Bob"), WireValue::Int(26)]
            ]
        );
        assert!(matches!(
            answer.outcome,
            Outcome::WriteCommitted {
                seq: 2,
                statements: 1
            }
        ));

        let mut scope = grant(Rights::ReadWrite);
        scope.properties = Scope::only([PropertyKeyId(1)]);
        let mut scoped = Client::connect(cx, addr, token(&scope)).await.unwrap();
        scoped.select(cx, "social").await.unwrap();
        let answer = scoped.execute(cx, ExecuteMode::Write,
            "MATCH (p:Person) WHERE p.name = 'Ann' SET p.name = 'Anne' RETURN p.name AS name, p.age AS age", vec![])
            .await.unwrap();
        assert_eq!(answer.rows, [vec![text("Anne"), WireValue::Null]]);
        let refused = scoped.execute(cx, ExecuteMode::Write,
            "MATCH (p:Person) WHERE p.name = 'Anne' SET p.name = 'Lost', p.age = 99 RETURN p.name", vec![])
            .await.unwrap_err();
        assert_eq!(server_code(refused), ErrorCode::PermissionDenied);
        scoped.close(cx).await.unwrap();

        let mut limited = grant(Rights::ReadWrite);
        limited.limits.max_rows = 0;
        let mut limited = Client::connect(cx, addr, token(&limited)).await.unwrap();
        limited.select(cx, "social").await.unwrap();
        let refused = limited
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (p:Person) SET p.age = 0 RETURN p.age",
                vec![],
            )
            .await
            .unwrap_err();
        assert_eq!(server_code(refused), ErrorCode::Budget);
        let answer = limited
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (p:Person) WHERE p.name = 'Bob' SET p.age = 27 RETURN p.age LIMIT 0",
                vec![],
            )
            .await
            .unwrap();
        assert!(answer.rows.is_empty());
        assert!(matches!(
            answer.outcome,
            Outcome::WriteCommitted { seq: 4, .. }
        ));
        limited.close(cx).await.unwrap();
        let answer = owner
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (p:Person) RETURN p.name AS name, p.age AS age ORDER BY name",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(
            answer.rows,
            [
                vec![text("Anne"), WireValue::Int(31)],
                vec![text("Bob"), WireValue::Int(27)]
            ]
        );
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn merge_returning_preserves_the_selected_vertex_and_atomic_failure_over_fgp() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "merge-returning").await;
        let mut owner = Client::connect(cx, addr, token(&grant(Rights::ReadWrite)))
            .await
            .unwrap();
        owner.select(cx, "social").await.unwrap();
        let statement = "MERGE (p:Person {name:'Ann'}) ON CREATE SET p.age = 1 ON MATCH SET p.age = p.age + 1 SET p.age = p.age + 10 RETURN p, p.age AS age";
        let first = owner
            .execute(cx, ExecuteMode::Write, statement, vec![])
            .await
            .unwrap();
        assert_eq!(first.rows.len(), 1);
        let vertex = first.rows[0][0].clone();
        assert!(matches!(vertex, WireValue::Vertex(_)));
        assert_eq!(first.rows[0], [vertex.clone(), WireValue::Int(11)]);
        assert!(matches!(
            first.outcome,
            Outcome::WriteCommitted {
                seq: 1,
                statements: 1
            }
        ));
        let second = owner
            .execute(cx, ExecuteMode::Write, statement, vec![])
            .await
            .unwrap();
        assert_eq!(second.rows, [vec![vertex.clone(), WireValue::Int(22)]]);
        assert!(matches!(
            second.outcome,
            Outcome::WriteCommitted { seq: 2, .. }
        ));
        let matched = owner
            .execute(
                cx,
                ExecuteMode::Write,
                "MERGE (p:Person {name:'Ann'}) RETURN p, p.age AS age",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(matched.rows, [vec![vertex, WireValue::Int(22)]]);
        assert!(matches!(
            matched.outcome,
            Outcome::ReadClosed { seq: 2, .. }
        ));
        let refused = owner
            .execute(
                cx,
                ExecuteMode::Write,
                "MERGE (p:Person {name:'Lost'}) ON CREATE SET p.age = 99 RETURN 1 / 0",
                vec![],
            )
            .await
            .unwrap_err();
        assert_eq!(server_code(refused), ErrorCode::Statement);

        let scoped = Grant {
            properties: Scope::only([PropertyKeyId(1)]),
            ..grant(Rights::ReadWrite)
        };
        let mut scoped = Client::connect(cx, addr, token(&scoped)).await.unwrap();
        scoped.select(cx, "social").await.unwrap();
        let answer = scoped
            .execute(
                cx,
                ExecuteMode::Write,
                "MERGE (p:Person {name:'Ann'}) RETURN p.name AS name, p.age AS age",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(answer.rows, [vec![text("Ann"), WireValue::Null]]);
        let refused = scoped
            .execute(
                cx,
                ExecuteMode::Write,
                "MERGE (p:Person {name:'Ann'}) SET p.age = 99 RETURN p.name",
                vec![],
            )
            .await
            .unwrap_err();
        assert_eq!(server_code(refused), ErrorCode::PermissionDenied);
        scoped.close(cx).await.unwrap();
        let answer = owner
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (p:Person) RETURN p.name AS name, p.age AS age",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(answer.rows, [vec![text("Ann"), WireValue::Int(22)]]);
        assert!(matches!(answer.outcome, Outcome::Rows { seq: 2 }));
        owner.close(cx).await.unwrap();
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

        // Schema discovery answers the bound names a token may see, and no
        // others: a scoped token learns nothing about hidden names.
        let (status, body) = http(
            addr,
            "GET",
            "/v1/databases/social/schema",
            host,
            Some(&ro),
            "",
        )
        .await;
        assert_eq!(
            (status, body.as_str()),
            (
                200,
                r#"{"v":1,"labels":["Company","Person"],"relations":["KNOWS","WORKS_AT"],"properties":["age","name"]}"#
            )
        );
        let scoped = token(&Grant {
            labels: Scope::only([LabelId(1)]),
            relations: Scope::only([RelationId(1)]),
            properties: Scope::only([PropertyKeyId(1)]),
            ..grant(Rights::Read)
        });
        let (status, body) = http(
            addr,
            "GET",
            "/v1/databases/social/schema",
            host,
            Some(&scoped),
            "",
        )
        .await;
        assert_eq!(
            (status, body.as_str()),
            (
                200,
                r#"{"v":1,"labels":["Person"],"relations":["KNOWS"],"properties":["name"]}"#
            )
        );
        let (status, _) = http(addr, "GET", "/v1/databases/social/schema", host, None, "").await;
        assert_eq!(status, 401);

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

        let (status, body) = http(
            addr, "POST", "/v1/databases/social/write", host, Some(&rw),
            r#"{"statement":"MATCH (p:Person) WHERE p.name = 'Ann' SET p.age = p.age + 1 RETURN p.name AS name, p.age AS age"}"#,
        ).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            r#"{"v":1,"columns":["name","age"],"rows":[[{"type":"text","value":"Ann"},{"type":"int","value":"31"}]],"seq":2,"statements":1,"committed":true}"#
        );
        let (status, body) = http(
            addr,
            "POST",
            "/v1/databases/social/write",
            host,
            Some(&rw),
            r#"{"statement":"MATCH (p:Person) WHERE p.name = 'Ann' SET p.age = 99 RETURN 1 / 0"}"#,
        )
        .await;
        assert_eq!(status, 400, "{body}");
        let (status, body) = http(
            addr,
            "POST",
            "/v1/databases/social/query",
            host,
            Some(&ro),
            r#"{"statement":"MATCH (p:Person) WHERE p.name = 'Ann' RETURN p.age AS age"}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            r#"{"v":1,"columns":["age"],"rows":[[{"type":"int","value":"31"}]],"seq":2}"#
        );

        let (status, body) = http(
            addr, "POST", "/v1/databases/social/write", host, Some(&rw),
            r#"{"statement":"MERGE (p:Person {name:'Ann'}) ON MATCH SET p.age = p.age + 1 RETURN p.name AS name, p.age AS age"}"#,
        ).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            r#"{"v":1,"columns":["name","age"],"rows":[[{"type":"text","value":"Ann"},{"type":"int","value":"32"}]],"seq":3,"statements":1,"committed":true}"#
        );
        let (status, body) = http(
            addr,
            "POST",
            "/v1/databases/social/write",
            host,
            Some(&rw),
            r#"{"statement":"MERGE (p:Person {name:'Ann'}) RETURN p.age AS age"}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            r#"{"v":1,"columns":["age"],"rows":[[{"type":"int","value":"32"}]],"seq":3,"statements":1,"committed":false}"#
        );
        let (status, body) = http(
            addr,
            "POST",
            "/v1/databases/social/query",
            host,
            Some(&ro),
            r#"{"statement":"EXPLAIN MATCH (p:Person) RETURN p.age AS age"}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(
            body.starts_with(
                r#"{"v":1,"columns":["operator","detail"],"rows":[[{"type":"text","value":"NativeRead"}"#
            ),
            "{body}"
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

        // EXPLAIN streams the authorized session's text-derived listing as
        // operator/detail records (fgdb-ooiik), not a Neo4j summary plan.
        let fields = client
            .expect(
                0x10,
                run_message("EXPLAIN MATCH (p:Person) RETURN p.age AS age", vec![]),
                SUCCESS,
            )
            .await;
        assert_eq!(
            get(&fields, "fields"),
            Some(&Value::List(vec![
                Value::string("operator"),
                Value::string("detail")
            ]))
        );
        let (records, _) = client.pull_all().await;
        assert_eq!(records[0][0], Value::string("NativeRead"), "{records:?}");

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

        // A returned relationship carries its real endpoints, type and
        // properties using the negotiated Bolt 5.0 structure.
        let edge_write = writer
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (a:Person {name: 'Ann'}), (b:Person {name: 'Bob'}) CREATE (a)-[:KNOWS {name:'friends', age:7}]->(b)",
                vec![],
            )
            .await
            .unwrap();
        let Outcome::WriteCommitted { seq: edge_seq, .. } = edge_write.outcome else {
            panic!("relationship create commits");
        };
        let expected = writer
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (a)-[r:KNOWS]->(b) RETURN r, a, b",
                vec![],
            )
            .await
            .unwrap();
        let [
            WireValue::Edge(edge),
            WireValue::Vertex(source),
            WireValue::Vertex(target),
        ] = expected.rows[0].as_slice()
        else {
            panic!("native relationship identities");
        };
        client
            .expect(
                0x10,
                run_message("MATCH (a)-[r:KNOWS]->(b) RETURN r", vec![]),
                SUCCESS,
            )
            .await;
        let (records, _) = client.pull_all().await;
        let expected_relationship = Value::Struct {
            tag: 0x52,
            fields: vec![
                Value::Int(i64::try_from(*edge).unwrap()),
                Value::Int(i64::try_from(*source).unwrap()),
                Value::Int(i64::try_from(*target).unwrap()),
                Value::string("KNOWS"),
                Value::Map(vec![
                    ("age".to_owned(), Value::Int(7)),
                    ("name".to_owned(), Value::string("friends")),
                ]),
                Value::string(edge.to_string()),
                Value::string(source.to_string()),
                Value::string(target.to_string()),
            ],
        };
        assert_eq!(records, [[expected_relationship.clone()]]);

        let forward = "MATCH p=(a:Person {name:'Ann'})-[:KNOWS]->(b:Person) RETURN p";
        client
            .expect(0x10, run_message(forward, vec![]), SUCCESS)
            .await;
        let historical_path = client.pull_all().await.0;
        let Value::Struct { tag, fields } = &historical_path[0][0] else {
            panic!("a path");
        };
        assert_eq!(*tag, 0x50);
        assert_eq!(fields[2], Value::List(vec![Value::Int(1), Value::Int(1)]));
        let Value::List(path_relationships) = &fields[1] else {
            panic!("unbound path relationships");
        };
        assert_eq!(
            path_relationships,
            &[Value::Struct {
                tag: 0x72,
                fields: vec![
                    Value::Int(i64::try_from(*edge).unwrap()),
                    Value::string("KNOWS"),
                    Value::Map(vec![
                        ("age".to_owned(), Value::Int(7)),
                        ("name".to_owned(), Value::string("friends")),
                    ]),
                    Value::string(edge.to_string()),
                ],
            }]
        );
        client
            .expect(
                0x10,
                run_message(
                    "MATCH p=(b:Person {name:'Bob'})<-[:KNOWS]-(a:Person) RETURN p",
                    vec![],
                ),
                SUCCESS,
            )
            .await;
        let reverse = client.pull_all().await.0;
        let Value::Struct { fields, .. } = &reverse[0][0] else {
            panic!("reverse path");
        };
        assert_eq!(fields[2], Value::List(vec![Value::Int(-1), Value::Int(1)]));

        // Hydration shares the explicit transaction's old generation, then
        // honors an explicit historical selector after newer data is visible.
        client
            .expect(0x11, vec![Value::Map(Vec::new())], SUCCESS)
            .await;
        writer
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (a:Person {name:'Ann'})-[r:KNOWS]->(b:Person) SET a.age=31, r.age=8, r.name='current'",
                vec![],
            )
            .await
            .unwrap();
        client
            .expect(0x10, run_message(forward, vec![]), SUCCESS)
            .await;
        assert_eq!(client.pull_all().await.0, historical_path);
        client.expect(0x12, vec![], SUCCESS).await;
        client
            .expect(0x10, run_message(forward, vec![]), SUCCESS)
            .await;
        assert_ne!(client.pull_all().await.0, historical_path);
        let historical = format!(
            "MATCH p=(a:Person {{name:'Ann'}})-[:KNOWS]->(b:Person) FOR SYSTEM_TIME AS OF SEQ {edge_seq} RETURN p"
        );
        client
            .expect(0x10, run_message(&historical, vec![]), SUCCESS)
            .await;
        assert_eq!(client.pull_all().await.0, historical_path);
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
        scoped
            .expect(
                0x10,
                run_message("MATCH (a)-[r:KNOWS]->(b) RETURN r", vec![]),
                SUCCESS,
            )
            .await;
        let (records, _) = scoped.pull_all().await;
        let Value::Struct { tag, fields } = &records[0][0] else {
            panic!("a masked relationship");
        };
        assert_eq!(*tag, 0x52);
        assert_eq!(
            fields[4],
            Value::Map(vec![("name".into(), Value::string("current"))])
        );
        // Neo4j's schema procedures answer the names this token may see.
        let keys = scoped
            .expect(0x10, run_message("CALL db.propertyKeys()", vec![]), SUCCESS)
            .await;
        assert_eq!(
            get(&keys, "fields"),
            Some(&Value::List(vec![Value::string("propertyKey")]))
        );
        assert_eq!(scoped.pull_all().await.0, [[Value::string("name")]]);
        scoped
            .expect(
                0x10,
                run_message("CALL db.labels() YIELD label", vec![]),
                SUCCESS,
            )
            .await;
        assert_eq!(
            scoped.pull_all().await.0,
            [[Value::string("Company")], [Value::string("Person")]]
        );

        // Deleting the current edge must not erase historical path metadata.
        writer
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (a)-[r:KNOWS]->(b) DELETE r",
                vec![],
            )
            .await
            .unwrap();
        scoped
            .expect(0x10, run_message(&historical, vec![]), SUCCESS)
            .await;
        let old_masked = scoped.pull_all().await.0;
        let Value::Struct { fields, .. } = &old_masked[0][0] else {
            panic!("historical path after delete");
        };
        let Value::List(edges) = &fields[1] else {
            panic!("historical relationships");
        };
        let Value::Struct { fields, .. } = &edges[0] else {
            panic!("historical relationship metadata");
        };
        assert_eq!(
            fields[2],
            Value::Map(vec![("name".into(), Value::string("friends"))])
        );
        scoped
            .expect(0x10, run_message(forward, vec![]), SUCCESS)
            .await;
        assert!(scoped.pull_all().await.0.is_empty());

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

#[test]
fn fgp_prepared_reads_rebind_current_state_scope_and_survive_cancelled_delivery() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "prepared-read-lifecycle").await;
        let original = token(&grant(Rights::ReadWrite));
        let mut owner = Client::connect(cx, addr, original.clone()).await.unwrap();
        owner.select(cx, "social").await.unwrap();
        let query = "MATCH (n:Person) WHERE n.age >= $min RETURN n.name AS name ORDER BY name";
        let handle = owner
            .prepare_read(cx, query, vec![("min".into(), WireValue::Int(99))])
            .await
            .unwrap();
        owner
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (:Person {name:'A',age:1}), (:Person {name:'B',age:2}), \
                 (:Person {name:'C',age:3}), (:Person {name:'D',age:4}), \
                 (:Person {name:'E',age:5}), (:Person {name:'F',age:6}), \
                 (:Company {name:'Hidden',age:99})",
                vec![],
            )
            .await
            .unwrap();
        let answer = owner
            .execute_prepared(cx, handle, vec![("min".into(), WireValue::Int(3))])
            .await
            .unwrap();
        assert_eq!(answer.columns, ["name"]);
        assert_eq!(answer.rows, ["C", "D", "E", "F"].map(|name| vec![text(name)]));
        assert!(matches!(answer.outcome, Outcome::Rows { seq: 1 }));
        assert_eq!(
            server_code(owner.execute_prepared(cx, handle, vec![]).await.unwrap_err()),
            ErrorCode::Statement
        );
        let mut foreign = Client::connect(cx, addr, original).await.unwrap();
        foreign.select(cx, "social").await.unwrap();
        let foreign_error = foreign
            .execute_prepared(cx, handle, vec![("min".into(), WireValue::Int(1))])
            .await
            .unwrap_err();
        assert_eq!(server_code(foreign_error), ErrorCode::Statement);
        foreign.release_prepared(cx, handle).await.unwrap();
        let mut delivered = 0;
        let cancelled = owner
            .execute_prepared_streaming(
                cx,
                handle,
                vec![("min".into(), WireValue::Int(1))],
                |_| {},
                |_| {
                    delivered += 1;
                    Err(ClientError::Protocol("the consumer stopped"))
                },
            )
            .await;
        assert!(matches!(cancelled, Err(ClientError::Protocol("the consumer stopped"))));
        assert_eq!(delivered, 1);
        let answer = owner
            .execute_prepared(cx, handle, vec![("min".into(), WireValue::Int(6))])
            .await
            .unwrap();
        assert_eq!(answer.rows, [vec![text("F")]]);
        // This is a structural text operand. Its spelling cannot create a
        // second statement, substitute catalog names, or alter the template.
        let exact_name = owner
            .prepare_read(
                cx,
                "MATCH (n:Person) WHERE n.name = $name RETURN n.name AS name",
                vec![("name".into(), text("A"))],
            )
            .await
            .unwrap();
        let injection = owner
            .execute_prepared(
                cx,
                exact_name,
                vec![("name".into(), text("A' CREATE (:Person {name:'injected'}) //"))],
            )
            .await
            .unwrap();
        assert!(injection.rows.is_empty());
        let live = owner
            .execute_prepared(cx, exact_name, vec![("name".into(), text("B"))])
            .await
            .unwrap();
        assert_eq!(live.rows, [vec![text("B")]]);
        owner.release_prepared(cx, exact_name).await.unwrap();
        owner.release_prepared(cx, exact_name).await.unwrap();
        assert_eq!(
            server_code(
                owner.execute_prepared(cx, exact_name, vec![("name".into(), text("B"))])
                    .await.unwrap_err()
            ),
            ErrorCode::Statement
        );
        assert_eq!(
            server_code(owner.prepare_read(cx, "CREATE (:Person)", vec![]).await.unwrap_err()),
            ErrorCode::Statement
        );
        let narrowed = token(&Grant {
            labels: Scope::only([LabelId(1)]),
            properties: Scope::only([PropertyKeyId(1)]),
            ..grant(Rights::Read)
        });
        owner.refresh_authority(cx, narrowed).await.unwrap();
        assert_eq!(
            server_code(
                owner.execute_prepared(cx, handle, vec![("min".into(), WireValue::Int(1))])
                    .await.unwrap_err()
            ),
            ErrorCode::Statement,
            "even narrowing discards every old compiled template"
        );
        let visible = owner
            .prepare_read(cx, "MATCH (n) RETURN n.name AS name ORDER BY name", vec![])
            .await
            .unwrap();
        let answer = owner.execute_prepared(cx, visible, vec![]).await.unwrap();
        assert_eq!(answer.rows, ["A", "B", "C", "D", "E", "F"].map(|name| vec![text(name)]));
        owner.release_prepared(cx, visible).await.unwrap();
        foreign.close(cx).await.unwrap();
        owner.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}
