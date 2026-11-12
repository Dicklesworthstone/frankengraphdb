//! Bolt writes over real TCP, observed through an independent FGP client.
//! Both listeners use the same on-disk Chronicle-backed database and authority.

use asupersync::io::{AsyncReadExt, AsyncWriteExt};
use asupersync::net::{TcpListener, TcpStream};
use asupersync::security::key::AuthKey;
use asupersync::{Budget, Cx};
use fgdb::{Database, DatabaseKeys};
use fgdb_bolt::message::{Dechunker, MAGIC, frame};
use fgdb_bolt::packstream::{Value, decode, encode, get};
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::GraphSymbolKind;
use fgdb_protocol::body::{ExecuteMode, Outcome, WireValue};
use fgdb_protocol::client::Client;
use fgdb_server::{DatabaseConfig, Server, ServerLimits, Symbols, issue_token, issuer};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Grant, QueryLimits, Rights, Scope};
use std::net::SocketAddr;
use std::sync::Arc;

const SUCCESS: u8 = 0x70;
const FAILURE: u8 = 0x7f;
const IGNORED: u8 = 0x7e;

fn issuer_key() -> AuthKey {
    AuthKey::from_seed(8501)
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32])
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
    issue_token(&issuer(issuer_key(), &keys(), 1).unwrap(), grant).unwrap()
}

fn run(test: impl AsyncFnOnce(&Cx)) {
    let runtime = fgdb::runtime_builder().build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(test(&cx));
}

fn text(value: &str) -> WireValue {
    WireValue::Text(value.into())
}

async fn served(cx: &Cx, name: &str) -> Server {
    let path = std::env::temp_dir().join(format!(
        "fgdb-bolt-autocommit-tcp-{}-{name}",
        std::process::id()
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
    server.open_database(cx, &path, config).await.unwrap();
    server
}

/// Test client uses the production PackStream codec and actual socket framing.
struct Bolt {
    stream: TcpStream,
    dechunker: Dechunker,
}

impl Bolt {
    async fn connect(address: SocketAddr) -> (Self, [u8; 4]) {
        let mut stream = TcpStream::connect(address).await.unwrap();
        let mut hello = MAGIC.to_vec();
        hello.extend_from_slice(&[0, 0, 0, 5, 0, 2, 4, 4, 0, 0, 0, 0, 0, 0, 0, 0]);
        stream.write_all(&hello).await.unwrap();
        let mut version = [0; 4];
        stream.read_exact(&mut version).await.unwrap();
        (
            Self {
                stream,
                dechunker: Dechunker::new(1 << 24),
            },
            version,
        )
    }

    async fn send(&mut self, tag: u8, fields: Vec<Value>) {
        let mut message = Vec::new();
        encode(&Value::Struct { tag, fields }, &mut message);
        let mut framed = Vec::new();
        frame(&message, &mut framed);
        self.stream.write_all(&framed).await.unwrap();
    }

    async fn receive(&mut self) -> Option<(u8, Vec<Value>)> {
        loop {
            if let Some(message) = self.dechunker.next_message().unwrap() {
                let Value::Struct { tag, fields } = decode(&message).unwrap() else {
                    panic!("response must be a structure");
                };
                return Some((tag, fields));
            }
            let mut buffer = [0; 4096];
            let read = self.stream.read(&mut buffer).await.ok()?;
            if read == 0 {
                return None;
            }
            self.dechunker.push(&buffer[..read]);
        }
    }

    async fn expect(&mut self, tag: u8, fields: Vec<Value>, want: u8) -> Vec<(String, Value)> {
        self.send(tag, fields).await;
        let (got, fields) = self.receive().await.expect("a response");
        assert_eq!(got, want, "{fields:?}");
        match fields.into_iter().next() {
            Some(Value::Map(metadata)) => metadata,
            _ => Vec::new(),
        }
    }

    async fn pull_all(&mut self) -> (Vec<Vec<Value>>, Vec<(String, Value)>) {
        self.pull(-1).await
    }

    async fn pull(&mut self, count: i64) -> (Vec<Vec<Value>>, Vec<(String, Value)>) {
        self.send(
            0x3f,
            vec![Value::Map(vec![("n".into(), Value::Int(count))])],
        )
        .await;
        let mut rows = Vec::new();
        loop {
            match self.receive().await.expect("a response") {
                (0x71, mut fields) => {
                    let Some(Value::List(row)) = fields.pop() else {
                        panic!("RECORD holds a list");
                    };
                    rows.push(row);
                }
                (SUCCESS, mut fields) => {
                    let Some(Value::Map(metadata)) = fields.pop() else {
                        panic!("SUCCESS holds a map");
                    };
                    return (rows, metadata);
                }
                other => panic!("unexpected response {other:?}"),
            }
        }
    }
}

async fn start_both(
    cx: &Cx,
    name: &str,
) -> (
    SocketAddr,
    SocketAddr,
    fgdb_server::Shutdown,
    Vec<asupersync::runtime::TaskHandle<()>>,
) {
    let server = Arc::new(served(cx, name).await);
    let fgp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bolt = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addresses = (fgp.local_addr().unwrap(), bolt.local_addr().unwrap());
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
    (addresses.0, addresses.1, shutdown, serving)
}

async fn finish(
    cx: &Cx,
    shutdown: fgdb_server::Shutdown,
    serving: Vec<asupersync::runtime::TaskHandle<()>>,
) {
    shutdown.trigger();
    for mut handle in serving {
        handle.join(cx).await.unwrap();
    }
}

async fn login(address: SocketAddr, grant: &Grant) -> Bolt {
    let (mut client, version) = Bolt::connect(address).await;
    assert_eq!(version, [0, 0, 0, 5]);
    let hex: String = token(grant)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let ready = client
        .expect(
            0x01,
            vec![Value::Map(vec![
                ("scheme".into(), Value::string("bearer")),
                ("credentials".into(), Value::string(hex)),
            ])],
            SUCCESS,
        )
        .await;
    assert_eq!(
        get(&ready, "fgdb_autocommit_writes"),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        get(&ready, "fgdb_explicit_write_transactions"),
        Some(&Value::Bool(true))
    );
    client
}

fn request(
    query: &str,
    parameters: Vec<(String, Value)>,
    extra: Vec<(String, Value)>,
) -> Vec<Value> {
    vec![
        Value::string(query),
        Value::Map(parameters),
        Value::Map(extra),
    ]
}

fn consume() -> Vec<Value> {
    vec![Value::Map(vec![("n".into(), Value::Int(-1))])]
}

fn graph_fields(value: &Value, expected_tag: u8) -> &[Value] {
    let Value::Struct { tag, fields } = value else {
        panic!("expected a Bolt graph structure, got {value:?}");
    };
    assert_eq!(*tag, expected_tag);
    fields
}

fn node_properties(value: &Value) -> &[(String, Value)] {
    let fields = graph_fields(value, 0x4e);
    assert_eq!(fields.len(), 4, "Bolt 5 nodes carry their full element ID");
    assert_eq!(fields[1], Value::List(vec![Value::string("Person")]));
    assert!(fields[3].as_str().is_some_and(|id| !id.is_empty()));
    let Value::Map(properties) = &fields[2] else {
        panic!("node properties must be a map");
    };
    properties
}

#[test]
fn bolt_document_upserts_publish_on_pull_and_discard_honors_the_returned_bookmark() {
    run(async |cx| {
        let (fgp, address, shutdown, serving) = start_both(cx, "documents").await;
        let mut client = login(address, &grant(Rights::ReadWrite)).await;
        let mut observer = Client::connect(cx, fgp, token(&grant(Rights::Read)))
            .await
            .unwrap();
        observer.select(cx, "social").await.unwrap();
        let hostile = "Not code'); MATCH (n) DETACH DELETE n; //";
        let person = |name: &str, age| {
            Value::Map(vec![
                ("name".into(), Value::string(name)),
                ("age".into(), Value::Int(age)),
            ])
        };
        let document = Value::Map(vec![
            (
                "members".into(),
                Value::List(vec![
                    person("Ann", 1),
                    person(hostile, 10),
                    person("Ann", 9),
                ]),
            ),
            (
                "ignored".into(),
                Value::string("metadata is not a mutation operand"),
            ),
        ]);
        let fields = client
            .expect(
                0x10,
                request(
                    "UNWIND $payload.members AS row MERGE (n:Person {name:row.name}) SET n.age=row.age",
                    vec![("payload".into(), document)],
                    vec![],
                ),
                SUCCESS,
            )
            .await;
        assert_eq!(get(&fields, "fields"), Some(&Value::List(vec![])));
        let read = "MATCH (n:Person) RETURN n.name AS name, n.age AS age ORDER BY name";
        let before = observer
            .execute(cx, ExecuteMode::Read, read, vec![])
            .await
            .unwrap();
        assert!(before.rows.is_empty(), "RUN must not publish a prefix");
        assert_eq!(before.outcome, Outcome::Rows { seq: 0 });
        let (rows, done) = client.pull_all().await;
        assert!(rows.is_empty());
        assert_eq!(get(&done, "type").and_then(Value::as_str), Some("w"));
        assert_eq!(get(&done, "fgdb_statements"), Some(&Value::Int(3)));
        assert_eq!(
            get(&done, "bookmark").and_then(Value::as_str),
            Some("fgdb:social:1"),
        );
        let after = observer
            .execute(cx, ExecuteMode::Read, read, vec![])
            .await
            .unwrap();
        assert_eq!(
            after.rows,
            vec![
                vec![text("Ann"), WireValue::Int(9)],
                vec![text(hostile), WireValue::Int(10)],
            ]
        );
        assert_eq!(after.outcome, Outcome::Rows { seq: 1 });
        client
            .expect(
                0x10,
                request(
                    "CREATE (:Person {name:'Discarded',age:7})",
                    vec![],
                    vec![(
                        "bookmarks".into(),
                        Value::List(vec![get(&done, "bookmark").unwrap().clone()]),
                    )],
                ),
                SUCCESS,
            )
            .await;
        let discarded = client.expect(0x2f, consume(), SUCCESS).await;
        assert_eq!(get(&discarded, "type").and_then(Value::as_str), Some("w"));
        assert_eq!(
            get(&discarded, "bookmark").and_then(Value::as_str),
            Some("fgdb:social:2"),
        );
        let after = observer
            .execute(cx, ExecuteMode::Read, read, vec![])
            .await
            .unwrap();
        assert_eq!(after.rows.len(), 3, "DISCARD must execute a queued write");
        assert_eq!(after.outcome, Outcome::Rows { seq: 2 });
        client.send(0x02, vec![]).await;
        assert!(client.receive().await.is_none());
        observer.close(cx).await.unwrap();
        finish(cx, shutdown, serving).await;
    });
}

#[test]
fn bolt_reset_disconnect_and_read_boundaries_preserve_an_unexecuted_queue() {
    run(async |cx| {
        let (fgp, address, shutdown, serving) = start_both(cx, "abandon").await;
        let mut observer = Client::connect(cx, fgp, token(&grant(Rights::Read)))
            .await
            .unwrap();
        observer.select(cx, "social").await.unwrap();
        let read = "MATCH (n:Person) RETURN n.name AS name";
        for returning in [false, true] {
            let mut client = login(address, &grant(Rights::ReadWrite)).await;
            for name in ["Reset", "Disconnect"] {
                let statement = if returning {
                    "CREATE (n:Person {name:$name}) RETURN n.name AS name"
                } else {
                    "CREATE (:Person {name:$name})"
                };
                client
                    .expect(
                        0x10,
                        request(
                            statement,
                            vec![("name".into(), Value::string(name))],
                            vec![],
                        ),
                        SUCCESS,
                    )
                    .await;
                if name == "Reset" {
                    client.expect(0x0f, vec![], SUCCESS).await;
                } else {
                    // Raw disconnect, without RESET or GOODBYE, has no effects.
                    drop(client);
                    break;
                }
            }
        }
        let before = observer
            .execute(cx, ExecuteMode::Read, read, vec![])
            .await
            .unwrap();
        assert!(before.rows.is_empty());
        assert_eq!(before.outcome, Outcome::Rows { seq: 0 });
        // A write-only token needs no artificial read session for a no-row write.
        let mut write_grant = grant(Rights::Write);
        write_grant.limits.max_rows = 0;
        let mut writer = login(address, &write_grant).await;
        writer
            .expect(
                0x10,
                request("CREATE (:Person {name:'Kept'})", vec![], vec![]),
                SUCCESS,
            )
            .await;
        assert!(writer.pull_all().await.0.is_empty());
        writer.send(0x02, vec![]).await;
        assert!(writer.receive().await.is_none());
        for explicit in [false, true] {
            let mut client = login(address, &grant(Rights::ReadWrite)).await;
            if explicit {
                client
                    .expect(
                        0x11,
                        vec![Value::Map(vec![("mode".into(), Value::string("r"))])],
                        SUCCESS,
                    )
                    .await;
            }
            let options = if explicit {
                vec![]
            } else {
                vec![("mode".into(), Value::string("r"))]
            };
            let failure = client
                .expect(
                    0x10,
                    request("CREATE (:Person {name:'Forbidden'})", vec![], options),
                    FAILURE,
                )
                .await;
            assert_eq!(
                get(&failure, "code").and_then(Value::as_str),
                Some("Neo.ClientError.Statement.AccessMode"),
            );
            client.expect(0x0f, vec![], SUCCESS).await;
            client.send(0x02, vec![]).await;
            assert!(client.receive().await.is_none());
        }
        let after = observer
            .execute(cx, ExecuteMode::Read, read, vec![])
            .await
            .unwrap();
        assert_eq!(after.rows, vec![vec![text("Kept")]]);
        assert_eq!(after.outcome, Outcome::Rows { seq: 1 });
        observer.close(cx).await.unwrap();
        finish(cx, shutdown, serving).await;
    });
}

#[test]
fn bolt_late_script_failure_rolls_back_every_statement_and_cannot_be_pulled_again() {
    run(async |cx| {
        let (fgp, address, shutdown, serving) = start_both(cx, "rollback").await;
        let mut client = login(address, &grant(Rights::ReadWrite)).await;
        let mut observer = Client::connect(cx, fgp, token(&grant(Rights::Read)))
            .await
            .unwrap();
        observer.select(cx, "social").await.unwrap();
        client
            .expect(
                0x10,
                request(
                    "CREATE (:Person {name:'Good',age:2}), (:Person {name:'Bad',age:0})",
                    vec![],
                    vec![],
                ),
                SUCCESS,
            )
            .await;
        client.pull_all().await;
        let read = "MATCH (n:Person) RETURN n.name AS name, n.age AS age ORDER BY name";
        let before = observer
            .execute(cx, ExecuteMode::Read, read, vec![])
            .await
            .unwrap();
        client
            .expect(
                0x10,
                request(
                    "MATCH (n:Person {name:'Good'}) SET n.age=8; \
                     MATCH (n:Person {name:'Bad'}) SET n.age=10/n.age",
                    vec![],
                    vec![],
                ),
                SUCCESS,
            )
            .await;
        let failure = client.expect(0x3f, consume(), FAILURE).await;
        assert!(get(&failure, "bookmark").is_none());
        assert!(
            !get(&failure, "code")
                .and_then(Value::as_str)
                .unwrap()
                .starts_with("Neo.TransientError.")
        );
        client.expect(0x3f, consume(), IGNORED).await;
        client.expect(0x0f, vec![], SUCCESS).await;
        let after = observer
            .execute(cx, ExecuteMode::Read, read, vec![])
            .await
            .unwrap();
        assert_eq!(after.rows, before.rows);
        assert_eq!(after.outcome, before.outcome);
        client.send(0x02, vec![]).await;
        assert!(client.receive().await.is_none());
        observer.close(cx).await.unwrap();
        finish(cx, shutdown, serving).await;
    });
}

#[test]
fn bolt_write_admission_keeps_read_queries_with_write_spelling_on_the_read_path() {
    run(async |cx| {
        let (_, address, shutdown, serving) = start_both(cx, "read-dispatch").await;
        let mut client = login(address, &grant(Rights::ReadWrite)).await;
        for (query, expected) in [
            (
                "RETURN 'CREATE ()' AS text",
                vec![vec![Value::string("CREATE ()")]],
            ),
            (
                "UNWIND [1,2] AS create RETURN create",
                vec![vec![Value::Int(1)], vec![Value::Int(2)]],
            ),
            (
                "MATCH (create:Person) RETURN count(create) AS count",
                vec![vec![Value::Int(0)]],
            ),
            ("RETURN 1 AS one // CREATE ()", vec![vec![Value::Int(1)]]),
        ] {
            client
                .expect(0x10, request(query, vec![], vec![]), SUCCESS)
                .await;
            let (rows, done) = client.pull_all().await;
            assert_eq!(rows, expected, "{query}");
            assert_eq!(get(&done, "type").and_then(Value::as_str), Some("r"));
        }
        client
            .expect(
                0x10,
                request(
                    "EXPLAIN MATCH (create:Person) RETURN create.name",
                    vec![],
                    vec![],
                ),
                SUCCESS,
            )
            .await;
        assert!(!client.pull_all().await.0.is_empty());
        client.send(0x02, vec![]).await;
        assert!(client.receive().await.is_none());
        finish(cx, shutdown, serving).await;
    });
}

#[test]
fn bolt_unsupported_write_options_and_future_bookmarks_publish_nothing() {
    run(async |cx| {
        let (fgp, address, shutdown, serving) = start_both(cx, "options").await;
        let mut observer = Client::connect(cx, fgp, token(&grant(Rights::Read)))
            .await
            .unwrap();
        observer.select(cx, "social").await.unwrap();
        let mut client = login(address, &grant(Rights::ReadWrite)).await;
        for options in [
            vec![("tx_timeout".into(), Value::Int(1))],
            vec![("imp_user".into(), Value::string("other"))],
            vec![(
                "bookmarks".into(),
                Value::List(vec![Value::string("fgdb:other:1")]),
            )],
        ] {
            client
                .expect(
                    0x10,
                    request("CREATE (:Person {name:'Refused'})", vec![], options),
                    FAILURE,
                )
                .await;
            client.expect(0x0f, vec![], SUCCESS).await;
        }
        client
            .expect(
                0x10,
                request(
                    "CREATE (:Person {name:'TooEarly'})",
                    vec![],
                    vec![(
                        "bookmarks".into(),
                        Value::List(vec![Value::string("fgdb:social:1")]),
                    )],
                ),
                SUCCESS,
            )
            .await;
        client.expect(0x3f, consume(), FAILURE).await;
        let read = observer
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (n:Person) RETURN n.name",
                vec![],
            )
            .await
            .unwrap();
        assert!(read.rows.is_empty());
        assert_eq!(read.outcome, Outcome::Rows { seq: 0 });
        client.send(0x02, vec![]).await;
        assert!(client.receive().await.is_none());
        observer.close(cx).await.unwrap();
        finish(cx, shutdown, serving).await;
    });
}

/// fgdb-ch9re: a driver's map parameter is the property map it holds in a
/// Bolt autocommit write: `CREATE (:Person $props)`, `SET n += $props` and
/// both MERGE branches, each published on PULL. A key no symbol binds makes
/// no native write, and the RUN fails without publishing.
#[test]
fn bolt_map_parameters_bind_as_property_maps() {
    run(async |cx| {
        let (fgp, address, shutdown, serving) = start_both(cx, "map-parameters").await;
        let mut client = login(address, &grant(Rights::ReadWrite)).await;
        let mut observer = Client::connect(cx, fgp, token(&grant(Rights::Read)))
            .await
            .unwrap();
        observer.select(cx, "social").await.unwrap();
        let map = |entries: &[(&str, Value)]| {
            Value::Map(
                (entries.iter())
                    .map(|(key, value)| ((*key).into(), value.clone()))
                    .collect(),
            )
        };
        let merge = "MERGE (n:Person {name: $name}) ON CREATE SET n += $props \
                     ON MATCH SET n += $seen";
        let statements = [
            (
                "CREATE (:Person $props)",
                vec![(
                    "props".into(),
                    map(&[("name", Value::string("Ann")), ("age", Value::Int(33))]),
                )],
            ),
            (
                "MATCH (n:Person {name: 'Ann'}) SET n += $props",
                vec![("props".into(), map(&[("age", Value::Int(34))]))],
            ),
            (
                merge,
                vec![
                    ("name".into(), Value::string("Bob")),
                    ("props".into(), map(&[("age", Value::Int(41))])),
                    ("seen".into(), map(&[("age", Value::Int(0))])),
                ],
            ),
            (
                merge,
                vec![
                    ("name".into(), Value::string("Ann")),
                    ("props".into(), map(&[("age", Value::Int(0))])),
                    ("seen".into(), map(&[("age", Value::Int(35))])),
                ],
            ),
        ];
        for (seq, (query, parameters)) in (1..).zip(statements) {
            client
                .expect(0x10, request(query, parameters, vec![]), SUCCESS)
                .await;
            let (rows, done) = client.pull_all().await;
            assert!(rows.is_empty(), "{query}");
            assert_eq!(get(&done, "type").and_then(Value::as_str), Some("w"));
            assert_eq!(
                get(&done, "bookmark").and_then(Value::as_str),
                Some(format!("fgdb:social:{seq}").as_str()),
                "{query}"
            );
        }
        let read = "MATCH (n:Person) RETURN n.name AS name, n.age AS age ORDER BY name";
        let people = vec![
            vec![text("Ann"), WireValue::Int(35)],
            vec![text("Bob"), WireValue::Int(41)],
        ];
        let answer = observer
            .execute(cx, ExecuteMode::Read, read, vec![])
            .await
            .unwrap();
        assert_eq!(answer.rows, people);
        assert_eq!(answer.outcome, Outcome::Rows { seq: 4 });
        client
            .expect(
                0x10,
                request(
                    "MATCH (n:Person) SET n += $props",
                    vec![("props".into(), map(&[("nickname", Value::string("A"))]))],
                    vec![],
                ),
                FAILURE,
            )
            .await;
        client.expect(0x0f, vec![], SUCCESS).await;
        let answer = observer
            .execute(cx, ExecuteMode::Read, read, vec![])
            .await
            .unwrap();
        assert_eq!(answer.rows, people);
        assert_eq!(answer.outcome, Outcome::Rows { seq: 4 });
        // A record field keys a Bolt read pattern too (fgdb-8srlm).
        client
            .expect(
                0x10,
                request(
                    "MATCH (n:Person {name: $m.name}) RETURN n.age AS age",
                    vec![("m".into(), map(&[("name", Value::string("Ann"))]))],
                    vec![],
                ),
                SUCCESS,
            )
            .await;
        let (rows, done) = client.pull_all().await;
        assert_eq!(rows, vec![vec![Value::Int(35)]]);
        assert_eq!(get(&done, "type").and_then(Value::as_str), Some("r"));
        client.send(0x02, vec![]).await;
        assert!(client.receive().await.is_none());
        observer.close(cx).await.unwrap();
        finish(cx, shutdown, serving).await;
    });
}

/// fgdb-xwmqs: a Bolt write's SUCCESS carries `stats`, which official drivers
/// read as their counters. The program reports exactly what it created and
/// whether it published anything; a write that changes nothing says so.
#[test]
fn bolt_write_stats_report_what_the_write_created() {
    run(async |cx| {
        let (_, address, shutdown, serving) = start_both(cx, "write-stats").await;
        let mut client = login(address, &grant(Rights::ReadWrite)).await;
        let stats = |entries: &[(&str, Value)]| {
            Value::Map(
                (entries.iter())
                    .map(|(key, value)| ((*key).into(), value.clone()))
                    .collect(),
            )
        };
        for (query, expected) in [
            (
                "CREATE (:Person {name: 'Ann'}), (:Person {name: 'Bob'})",
                stats(&[
                    ("contains-updates", Value::Bool(true)),
                    ("nodes-created", Value::Int(2)),
                ]),
            ),
            (
                "MATCH (a:Person {name: 'Ann'}), (b:Person {name: 'Bob'}) \
                 CREATE (a)-[:KNOWS]->(b)",
                stats(&[
                    ("contains-updates", Value::Bool(true)),
                    ("relationships-created", Value::Int(1)),
                ]),
            ),
            (
                "CREATE (a:Person {name: 'Cy'})-[:KNOWS]->(b:Person {name: 'Di'})",
                stats(&[
                    ("contains-updates", Value::Bool(true)),
                    ("nodes-created", Value::Int(2)),
                    ("relationships-created", Value::Int(1)),
                ]),
            ),
            (
                "MATCH (n:Person {name: 'Ann'}) SET n.age = 33",
                stats(&[("contains-updates", Value::Bool(true))]),
            ),
            (
                "MATCH (n:Person {name: 'Nobody'}) SET n.age = 1",
                stats(&[("contains-updates", Value::Bool(false))]),
            ),
            // Chains run through the RETURN families with or without a
            // RETURN; their creations count the same.
            (
                "CREATE (a:Person {name: 'Eve'}) WITH a \
                 CREATE (a)-[:KNOWS]->(:Person {name: 'Fay'})",
                stats(&[
                    ("contains-updates", Value::Bool(true)),
                    ("nodes-created", Value::Int(2)),
                    ("relationships-created", Value::Int(1)),
                ]),
            ),
            (
                "UNWIND $rows AS row CREATE (n:Person) SET n += row",
                stats(&[
                    ("contains-updates", Value::Bool(true)),
                    ("nodes-created", Value::Int(2)),
                ]),
            ),
        ] {
            let rows = Value::List(vec![
                Value::Map(vec![("name".into(), Value::string("Gus"))]),
                Value::Map(vec![("name".into(), Value::string("Hal"))]),
            ]);
            let parameters = if query.contains("$rows") {
                vec![("rows".into(), rows)]
            } else {
                vec![]
            };
            client
                .expect(0x10, request(query, parameters, vec![]), SUCCESS)
                .await;
            let (rows, done) = client.pull_all().await;
            assert!(rows.is_empty(), "{query}");
            assert_eq!(get(&done, "stats"), Some(&expected), "{query}");
        }
        client.send(0x02, vec![]).await;
        assert!(client.receive().await.is_none());
        finish(cx, shutdown, serving).await;
    });
}

#[test]
fn bolt_write_returning_publishes_once_and_streams_the_committed_result() {
    run(async |cx| {
        let (fgp, address, shutdown, serving) = start_both(cx, "returning-pages").await;
        let mut client = login(address, &grant(Rights::ReadWrite)).await;
        let mut observer = Client::connect(cx, fgp, token(&grant(Rights::Read)))
            .await
            .unwrap();
        observer.select(cx, "social").await.unwrap();
        let fields = client
            .expect(
                0x10,
                request(
                    "UNWIND [3,1,2] AS x CREATE (n:Person {name:'Returned',age:x}) \
                     RETURN n.age AS age,[n.age,NULL] AS values,{age:n.age} AS meta ORDER BY age",
                    vec![],
                    vec![],
                ),
                SUCCESS,
            )
            .await;
        assert_eq!(
            get(&fields, "fields"),
            Some(&Value::List(vec![
                Value::string("age"),
                Value::string("values"),
                Value::string("meta")
            ]))
        );
        let read = "MATCH (n:Person) RETURN n.age AS age ORDER BY age";
        let before = observer
            .execute(cx, ExecuteMode::Read, read, vec![])
            .await
            .unwrap();
        assert!(before.rows.is_empty(), "RUN must only queue the write");
        assert_eq!(before.outcome, Outcome::Rows { seq: 0 });
        let expected_row = |age| {
            vec![
                Value::Int(age),
                Value::List(vec![Value::Int(age), Value::Null]),
                Value::Map(vec![("age".into(), Value::Int(age))]),
            ]
        };
        let (rows, more) = client.pull(1).await;
        assert_eq!(rows, vec![expected_row(1)]);
        assert_eq!(get(&more, "has_more"), Some(&Value::Bool(true)));
        assert!(get(&more, "bookmark").is_none());
        let published = observer
            .execute(cx, ExecuteMode::Read, read, vec![])
            .await
            .unwrap();
        assert_eq!(
            published.rows,
            vec![
                vec![WireValue::Int(1)],
                vec![WireValue::Int(2)],
                vec![WireValue::Int(3)]
            ]
        );
        assert_eq!(published.outcome, Outcome::Rows { seq: 1 });
        let more = client
            .expect(
                0x2f,
                vec![Value::Map(vec![("n".into(), Value::Int(1))])],
                SUCCESS,
            )
            .await;
        assert_eq!(get(&more, "has_more"), Some(&Value::Bool(true)));
        let (rows, done) = client.pull(1).await;
        assert_eq!(rows, vec![expected_row(3)]);
        assert_eq!(get(&done, "type").and_then(Value::as_str), Some("w"));
        assert_eq!(
            get(&done, "bookmark").and_then(Value::as_str),
            Some("fgdb:social:1")
        );
        let Some(Value::Map(stats)) = get(&done, "stats") else {
            panic!("completed write returns its native counters");
        };
        assert_eq!(get(stats, "nodes-created"), Some(&Value::Int(3)));
        assert_eq!(get(stats, "contains-updates"), Some(&Value::Bool(true)));
        client.expect(0x3f, consume(), FAILURE).await;
        client.expect(0x0f, vec![], SUCCESS).await;
        // DISCARD also executes a returning write, including its complete
        // result admission, even though no RECORD is sent to this client.
        client
            .expect(
                0x10,
                request(
                    "CREATE (n:Person {name:'Discarded',age:4}) RETURN n.age AS age",
                    vec![],
                    vec![(
                        "bookmarks".into(),
                        Value::List(vec![get(&done, "bookmark").unwrap().clone()]),
                    )],
                ),
                SUCCESS,
            )
            .await;
        let discarded = client.expect(0x2f, consume(), SUCCESS).await;
        assert_eq!(get(&discarded, "type").and_then(Value::as_str), Some("w"));
        assert_eq!(
            get(&discarded, "bookmark").and_then(Value::as_str),
            Some("fgdb:social:2")
        );
        let after = observer
            .execute(cx, ExecuteMode::Read, read, vec![])
            .await
            .unwrap();
        assert_eq!(after.rows.len(), 4);
        assert_eq!(
            after.outcome,
            Outcome::Rows { seq: 2 },
            "consumption never replays a write"
        );
        client.send(0x02, vec![]).await;
        assert!(client.receive().await.is_none());
        observer.close(cx).await.unwrap();
        finish(cx, shutdown, serving).await;
    });
}

#[test]
fn bolt_graph_returning_uses_final_mutations_and_masks_nested_entity_properties() {
    run(async |cx| {
        let (fgp, address, shutdown, serving) = start_both(cx, "returning-graph").await;
        let mut client = login(address, &grant(Rights::ReadWrite)).await;
        let mut observer = Client::connect(cx, fgp, token(&grant(Rights::Read)))
            .await
            .unwrap();
        observer.select(cx, "social").await.unwrap();
        let read = "MATCH (n:Person) RETURN n,n.name AS name,n.age AS age";
        let columns = client
            .expect(
                0x10,
                request(
                    "CREATE (n:Person {name:'Ada',age:1}) \
                     RETURN n AS node,[n] AS nodes,{node:n} AS metadata",
                    vec![],
                    vec![],
                ),
                SUCCESS,
            )
            .await;
        assert_eq!(
            get(&columns, "fields"),
            Some(&Value::List(vec![
                Value::string("node"),
                Value::string("nodes"),
                Value::string("metadata"),
            ])),
        );
        let queued = observer
            .execute(cx, ExecuteMode::Read, read, vec![])
            .await
            .unwrap();
        assert!(
            queued.rows.is_empty(),
            "RUN must leave the graph unpublished"
        );
        assert_eq!(queued.outcome, Outcome::Rows { seq: 0 });
        let (rows, done) = client.pull_all().await;
        assert_eq!(rows.len(), 1);
        let [node, nodes, metadata] = rows[0].as_slice() else {
            panic!("the returning projection has three columns");
        };
        assert_eq!(
            get(node_properties(node), "name"),
            Some(&Value::string("Ada"))
        );
        assert_eq!(get(node_properties(node), "age"), Some(&Value::Int(1)));
        assert_eq!(nodes, &Value::List(vec![node.clone()]));
        assert_eq!(metadata, &Value::Map(vec![("node".into(), node.clone())]));
        let element_id = graph_fields(node, 0x4e)[3].clone();
        assert_eq!(
            get(&done, "bookmark").and_then(Value::as_str),
            Some("fgdb:social:1"),
        );
        let published = observer
            .execute(cx, ExecuteMode::Read, read, vec![])
            .await
            .unwrap();
        assert_eq!(published.rows.len(), 1);
        let [WireValue::Vertex(id), name, age] = published.rows[0].as_slice() else {
            panic!("the independent native reader must see the created node");
        };
        assert_eq!(element_id, Value::string(id.to_string()));
        assert_eq!((name, age), (&text("Ada"), &WireValue::Int(1)));
        assert_eq!(published.outcome, Outcome::Rows { seq: 1 });

        client
            .expect(
                0x10,
                request(
                    "MERGE (n:Person {name:'Ada'}) ON MATCH SET n.age=2 RETURN n",
                    vec![],
                    vec![],
                ),
                SUCCESS,
            )
            .await;
        let (rows, done) = client.pull_all().await;
        assert_eq!(rows.len(), 1);
        assert_eq!(graph_fields(&rows[0][0], 0x4e)[3], element_id);
        assert_eq!(
            get(node_properties(&rows[0][0]), "age"),
            Some(&Value::Int(2))
        );
        assert_eq!(
            get(&done, "bookmark").and_then(Value::as_str),
            Some("fgdb:social:2"),
        );

        let mut restricted = grant(Rights::ReadWrite);
        restricted.properties = Scope::only([PropertyKeyId(1)]);
        let mut masked = login(address, &restricted).await;
        masked
            .expect(
                0x10,
                request(
                    "MATCH (n:Person {name:'Ada'}) SET n.name='Ada masked' \
                     RETURN {nodes:[n]} AS metadata",
                    vec![],
                    vec![],
                ),
                SUCCESS,
            )
            .await;
        let (rows, done) = masked.pull_all().await;
        assert_eq!(rows.len(), 1);
        let Value::Map(metadata) = &rows[0][0] else {
            panic!("metadata must remain a nested map");
        };
        let Some(Value::List(nodes)) = get(metadata, "nodes") else {
            panic!("metadata must hold the returned node list");
        };
        assert_eq!(nodes.len(), 1);
        assert_eq!(graph_fields(&nodes[0], 0x4e)[3], element_id);
        assert_eq!(
            node_properties(&nodes[0]),
            &[("name".into(), Value::string("Ada masked"))],
            "graph hydration must not disclose a property the token masks",
        );
        assert_eq!(
            get(&done, "bookmark").and_then(Value::as_str),
            Some("fgdb:social:3"),
        );
        let after = observer
            .execute(cx, ExecuteMode::Read, read, vec![])
            .await
            .unwrap();
        assert_eq!(after.rows.len(), 1);
        assert_eq!(
            &after.rows[0][1..],
            &[text("Ada masked"), WireValue::Int(2)]
        );
        assert_eq!(after.outcome, Outcome::Rows { seq: 3 });
        for writer in [&mut client, &mut masked] {
            writer.send(0x02, vec![]).await;
            assert!(writer.receive().await.is_none());
        }
        observer.close(cx).await.unwrap();
        finish(cx, shutdown, serving).await;
    });
}

#[test]
fn bolt_graph_returning_keeps_relationship_endpoints_and_reverse_path_orientation() {
    run(async |cx| {
        let (fgp, address, shutdown, serving) = start_both(cx, "returning-path").await;
        let mut client = login(address, &grant(Rights::ReadWrite)).await;
        let mut observer = Client::connect(cx, fgp, token(&grant(Rights::Read)))
            .await
            .unwrap();
        observer.select(cx, "social").await.unwrap();
        client
            .expect(
                0x10,
                request(
                    "CREATE (a:Person {name:'Ann'})-[r:KNOWS {age:7}]->\
                     (b:Person {name:'Bob'}) RETURN a,r,b",
                    vec![],
                    vec![],
                ),
                SUCCESS,
            )
            .await;
        let (created, done) = client.pull_all().await;
        assert_eq!(created.len(), 1);
        let [ann, relationship, bob] = created[0].as_slice() else {
            panic!("creation returns both endpoints and their relationship");
        };
        assert_eq!(
            get(node_properties(ann), "name"),
            Some(&Value::string("Ann"))
        );
        assert_eq!(
            get(node_properties(bob), "name"),
            Some(&Value::string("Bob"))
        );
        let edge = graph_fields(relationship, 0x52);
        assert_eq!(edge.len(), 8);
        assert_eq!(edge[3], Value::string("KNOWS"));
        assert_eq!(edge[4], Value::Map(vec![("age".into(), Value::Int(7))]));
        assert_eq!(edge[6], graph_fields(ann, 0x4e)[3]);
        assert_eq!(edge[7], graph_fields(bob, 0x4e)[3]);
        assert_eq!(
            get(&done, "bookmark").and_then(Value::as_str),
            Some("fgdb:social:1"),
        );
        client
            .expect(
                0x10,
                request(
                    "MATCH p=(b:Person {name:'Bob'})<-[r:KNOWS]-(a:Person {name:'Ann'}) \
                     SET r.age=8 RETURN p,r",
                    vec![],
                    vec![],
                ),
                SUCCESS,
            )
            .await;
        let (rows, done) = client.pull_all().await;
        assert_eq!(rows.len(), 1);
        let [path, updated_relationship] = rows[0].as_slice() else {
            panic!("mutation returns a path and its relationship");
        };
        let path = graph_fields(path, 0x50);
        assert_eq!(path.len(), 3);
        assert_eq!(path[0], Value::List(vec![bob.clone(), ann.clone()]));
        assert_eq!(path[2], Value::List(vec![Value::Int(-1), Value::Int(1)]));
        let Value::List(relationships) = &path[1] else {
            panic!("the path must carry its unbound relationships");
        };
        assert_eq!(relationships.len(), 1);
        let unbound = graph_fields(&relationships[0], 0x72);
        assert_eq!(unbound.len(), 4);
        assert_eq!(unbound[1], Value::string("KNOWS"));
        assert_eq!(unbound[2], Value::Map(vec![("age".into(), Value::Int(8))]));
        assert_eq!(unbound[3], edge[5]);
        let updated = graph_fields(updated_relationship, 0x52);
        assert_eq!(updated[4], unbound[2]);
        assert_eq!(&updated[5..], &edge[5..]);
        assert_eq!(
            get(&done, "bookmark").and_then(Value::as_str),
            Some("fgdb:social:2"),
        );
        let after = observer
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (a:Person)-[r:KNOWS]->(b:Person) \
                 RETURN a.name AS source,r.age AS age,b.name AS target",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(
            after.rows,
            vec![vec![text("Ann"), WireValue::Int(8), text("Bob")]]
        );
        assert_eq!(after.outcome, Outcome::Rows { seq: 2 });
        client.send(0x02, vec![]).await;
        assert!(client.receive().await.is_none());
        observer.close(cx).await.unwrap();
        finish(cx, shutdown, serving).await;
    });
}

#[test]
fn bolt_write_returning_admits_every_row_before_publication_or_delivery() {
    run(async |cx| {
        let (fgp, address, shutdown, serving) = start_both(cx, "returning-rollback").await;
        let mut client = login(address, &grant(Rights::ReadWrite)).await;
        let mut observer = Client::connect(cx, fgp, token(&grant(Rights::Read)))
            .await
            .unwrap();
        observer.select(cx, "social").await.unwrap();
        let graph_statement =
            "UNWIND $rows AS row CREATE (n:Person) SET n += row RETURN [n,n] AS nodes";
        let rows_parameter = |length| {
            vec![(
                "rows".into(),
                Value::List(vec![
                    Value::Map(vec![
                        ("name".into(), Value::string("short")),
                        ("age".into(), Value::Int(1)),
                    ]),
                    Value::Map(vec![
                        ("name".into(), Value::string("x".repeat(length))),
                        ("age".into(), Value::Int(2)),
                    ]),
                ]),
            )]
        };
        // The request is below the 4096-byte frame limit. The second RECORD
        // hydrates the same large node twice and exceeds that limit; the
        // first RECORD fits. PULL 1 and DISCARD must still reject every effect.
        for (statement, parameters, discard) in [
            (
                "UNWIND [2,0] AS x CREATE (n:Person {name:'Late',age:x}) RETURN 10/n.age AS value",
                vec![],
                false,
            ),
            (graph_statement, rows_parameter(2300), false),
            (graph_statement, rows_parameter(2300), true),
        ] {
            client
                .expect(0x10, request(statement, parameters, vec![]), SUCCESS)
                .await;
            let failure = client
                .expect(
                    if discard { 0x2f } else { 0x3f },
                    vec![Value::Map(vec![("n".into(), Value::Int(1))])],
                    FAILURE,
                )
                .await;
            assert!(get(&failure, "bookmark").is_none());
            client.expect(0x3f, consume(), IGNORED).await;
            client.expect(0x0f, vec![], SUCCESS).await;
            let answer = observer
                .execute(
                    cx,
                    ExecuteMode::Read,
                    "MATCH (n:Person) RETURN n.name AS name",
                    vec![],
                )
                .await
                .unwrap();
            assert!(answer.rows.is_empty(), "{statement}");
            assert_eq!(answer.outcome, Outcome::Rows { seq: 0 }, "{statement}");
        }
        // The identical graph-returning shape with smaller properties must
        // succeed, distinguishing frame admission from a syntax/type refusal.
        client
            .expect(
                0x10,
                request(graph_statement, rows_parameter(32), vec![]),
                SUCCESS,
            )
            .await;
        let (rows, done) = client.pull_all().await;
        assert_eq!(rows.len(), 2);
        for row in &rows {
            let [Value::List(nodes)] = row.as_slice() else {
                panic!("each admitted RECORD must contain a node list");
            };
            assert_eq!(nodes.len(), 2);
            assert_eq!(nodes[0], nodes[1]);
            assert!(get(node_properties(&nodes[0]), "name").is_some());
        }
        assert_eq!(
            get(&done, "bookmark").and_then(Value::as_str),
            Some("fgdb:social:1")
        );
        let published = observer
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (n:Person) RETURN n.name AS name",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(published.rows.len(), 2);
        assert_eq!(published.outcome, Outcome::Rows { seq: 1 });
        client.send(0x02, vec![]).await;
        assert!(client.receive().await.is_none());
        observer.close(cx).await.unwrap();
        finish(cx, shutdown, serving).await;
    });
}

#[test]
fn bolt_explicit_transactions_publish_once_and_isolate_rollback_and_disconnect() {
    run(async |cx| {
        let (fgp, address, shutdown, serving) = start_both(cx, "explicit-transactions").await;
        let mut observer = Client::connect(cx, fgp, token(&grant(Rights::Read)))
            .await
            .unwrap();
        observer.select(cx, "social").await.unwrap();
        let read = "MATCH (n:Person) RETURN n.name AS name, n.age AS age ORDER BY name";
        let mut client = login(address, &grant(Rights::ReadWrite)).await;
        client.expect(0x11, vec![Value::Map(vec![])], SUCCESS).await;
        let empty = client.expect(0x12, vec![], SUCCESS).await;
        assert_eq!(
            get(&empty, "bookmark").and_then(Value::as_str),
            Some("fgdb:social:0")
        );
        client
            .expect(
                0x11,
                vec![Value::Map(vec![("mode".into(), Value::string("w"))])],
                SUCCESS,
            )
            .await;
        let prepared = client
            .expect(
                0x10,
                request(
                    "CREATE (n:Person {name:$name,age:$age}) RETURN n",
                    vec![
                        ("name".into(), Value::string("Ann")),
                        ("age".into(), Value::Int(1)),
                    ],
                    vec![],
                ),
                SUCCESS,
            )
            .await;
        assert_eq!(get(&prepared, "qid"), Some(&Value::Int(0)));
        let (created, summary) = client.pull_all().await;
        assert_eq!(created.len(), 1);
        assert_eq!(
            get(node_properties(&created[0][0]), "age"),
            Some(&Value::Int(1))
        );
        assert_eq!(
            get(&summary, "fgdb_transaction_local"),
            Some(&Value::Bool(true))
        );
        assert!(get(&summary, "bookmark").is_none());
        let before = observer
            .execute(cx, ExecuteMode::Read, read, vec![])
            .await
            .unwrap();
        assert!(before.rows.is_empty());
        assert_eq!(before.outcome, Outcome::Rows { seq: 0 });
        client
            .expect(
                0x10,
                request(
                    "MATCH (n:Person) SET n.age=2 RETURN n.age AS age",
                    vec![],
                    vec![],
                ),
                SUCCESS,
            )
            .await;
        let (changed, summary) = client.pull_all().await;
        assert_eq!(changed, vec![vec![Value::Int(2)]]);
        assert!(get(&summary, "bookmark").is_none());
        client
            .expect(0x10, request(read, vec![], vec![]), SUCCESS)
            .await;
        let (own, summary) = client.pull_all().await;
        assert_eq!(own, vec![vec![Value::string("Ann"), Value::Int(2)]]);
        assert!(get(&summary, "bookmark").is_none());
        let still_private = observer
            .execute(cx, ExecuteMode::Read, read, vec![])
            .await
            .unwrap();
        assert!(still_private.rows.is_empty());
        assert_eq!(still_private.outcome, Outcome::Rows { seq: 0 });
        let committed = client.expect(0x12, vec![], SUCCESS).await;
        assert_eq!(
            get(&committed, "bookmark").and_then(Value::as_str),
            Some("fgdb:social:1")
        );
        let expected = vec![vec![text("Ann"), WireValue::Int(2)]];
        let visible = observer
            .execute(cx, ExecuteMode::Read, read, vec![])
            .await
            .unwrap();
        assert_eq!(visible.rows, expected);
        assert_eq!(visible.outcome, Outcome::Rows { seq: 1 });
        client.send(0x02, vec![]).await;
        assert!(client.receive().await.is_none());
        for rollback in [true, false] {
            let mut abandoned = login(address, &grant(Rights::ReadWrite)).await;
            abandoned
                .expect(0x11, vec![Value::Map(vec![])], SUCCESS)
                .await;
            abandoned
                .expect(
                    0x10,
                    request("CREATE (:Person {name:'Abandoned',age:99})", vec![], vec![]),
                    SUCCESS,
                )
                .await;
            let (rows, summary) = abandoned.pull_all().await;
            assert!(rows.is_empty());
            assert!(get(&summary, "bookmark").is_none());
            if rollback {
                abandoned.expect(0x13, vec![], SUCCESS).await;
                abandoned.send(0x02, vec![]).await;
                assert!(abandoned.receive().await.is_none());
            } else {
                drop(abandoned);
            }
            let observed = observer
                .execute(cx, ExecuteMode::Read, read, vec![])
                .await
                .unwrap();
            assert_eq!(observed.rows, expected);
            assert_eq!(observed.outcome, Outcome::Rows { seq: 1 });
        }
        observer.close(cx).await.unwrap();
        finish(cx, shutdown, serving).await;
    });
}

#[test]
fn bolt_explicit_aggregates_match_exact_fgp_results_and_publish_only_on_commit() {
    run(async |cx| {
        let (fgp, address, shutdown, serving) = start_both(cx, "explicit-aggregates").await;
        let mut observer = Client::connect(cx, fgp, token(&grant(Rights::Read)))
            .await
            .unwrap();
        observer.select(cx, "social").await.unwrap();
        let totals =
            "MATCH (n:Person) RETURN count(*) AS count, sum(n.age) AS total, avg(n.age) AS mean";
        let mut client = login(address, &grant(Rights::ReadWrite)).await;
        client.expect(0x11, vec![Value::Map(vec![])], SUCCESS).await;
        client
            .expect(
                0x10,
                request(
                    "CREATE (:Person {name:'Ann',age:1}), (:Person {name:'Ben',age:2}), (:Person {name:'Ann',age:4})",
                    vec![],
                    vec![],
                ),
                SUCCESS,
            )
            .await;
        let (rows, staged) = client.pull_all().await;
        assert!(rows.is_empty());
        assert!(get(&staged, "bookmark").is_none());
        for (query, expected) in [
            (
                totals,
                vec![vec![Value::Int(3), Value::Int(7), Value::Float(7.0 / 3.0)]],
            ),
            (
                "MATCH (n:Person) WITH n.name AS name, n.age AS age WHERE age >= 2 RETURN sum(age) AS total, name, count(*) AS count ORDER BY name",
                vec![
                    vec![Value::Int(4), Value::string("Ann"), Value::Int(1)],
                    vec![Value::Int(2), Value::string("Ben"), Value::Int(1)],
                ],
            ),
            (
                "MATCH (n:Person) WITH n.age AS age ORDER BY age RETURN collect(age) AS ages",
                vec![vec![Value::List(vec![
                    Value::Int(1),
                    Value::Int(2),
                    Value::Int(4),
                ])]],
            ),
        ] {
            client
                .expect(0x10, request(query, vec![], vec![]), SUCCESS)
                .await;
            let (rows, provisional) = client.pull_all().await;
            assert_eq!(rows, expected, "{query}");
            assert_eq!(
                get(&provisional, "fgdb_transaction_local"),
                Some(&Value::Bool(true))
            );
            assert!(get(&provisional, "bookmark").is_none());
            let before = observer
                .execute(cx, ExecuteMode::Read, totals, vec![])
                .await
                .unwrap();
            assert_eq!(
                before.rows,
                vec![vec![WireValue::Count(0), WireValue::Null, WireValue::Null]]
            );
            assert_eq!(before.outcome, Outcome::Rows { seq: 0 });
        }
        let committed = client.expect(0x12, vec![], SUCCESS).await;
        assert_eq!(
            get(&committed, "bookmark").and_then(Value::as_str),
            Some("fgdb:social:1")
        );
        let expected = vec![vec![
            WireValue::Count(3),
            WireValue::WideInt(7),
            WireValue::Average {
                numerator: 7,
                denominator: 3,
            },
        ]];
        let visible = observer
            .execute(cx, ExecuteMode::Read, totals, vec![])
            .await
            .unwrap();
        assert_eq!(visible.rows, expected);
        assert_eq!(visible.outcome, Outcome::Rows { seq: 1 });
        // Bolt's existing boundary spells a wide integer as decimal text and
        // narrows an exact rational mean to Float. Native/FGP values stay exact.
        client.expect(0x11, vec![Value::Map(vec![])], SUCCESS).await;
        client
            .expect(
                0x10,
                request(
                    "CREATE (:Person {name:'Wide',age:$age}), (:Person {name:'Wide',age:$age})",
                    vec![("age".into(), Value::Int(i64::MAX))],
                    vec![],
                ),
                SUCCESS,
            )
            .await;
        let discarded = client.expect(0x2f, consume(), SUCCESS).await;
        assert!(get(&discarded, "bookmark").is_none());
        let wide =
            "MATCH (n:Person) WHERE n.name='Wide' RETURN sum(n.age) AS total, avg(n.age) AS mean";
        client
            .expect(0x10, request(wide, vec![], vec![]), SUCCESS)
            .await;
        let (rows, provisional) = client.pull_all().await;
        assert_eq!(
            rows,
            vec![vec![
                Value::string("18446744073709551614"),
                Value::Float(9_223_372_036_854_775_808.0),
            ]]
        );
        assert!(get(&provisional, "bookmark").is_none());
        client
            .expect(0x10, request(totals, vec![], vec![]), SUCCESS)
            .await;
        let discarded = client.expect(0x2f, consume(), SUCCESS).await;
        assert!(get(&discarded, "bookmark").is_none());
        let still_private = observer
            .execute(cx, ExecuteMode::Read, totals, vec![])
            .await
            .unwrap();
        assert_eq!(still_private.rows, expected);
        assert_eq!(still_private.outcome, Outcome::Rows { seq: 1 });
        client.expect(0x13, vec![], SUCCESS).await;
        let rolled_back = observer
            .execute(cx, ExecuteMode::Read, totals, vec![])
            .await
            .unwrap();
        assert_eq!(rolled_back.rows, expected);
        assert_eq!(rolled_back.outcome, Outcome::Rows { seq: 1 });
        client.send(0x02, vec![]).await;
        assert!(client.receive().await.is_none());
        observer.close(cx).await.unwrap();
        finish(cx, shutdown, serving).await;
    });
}

#[test]
fn bolt_explicit_write_only_grants_commit_without_rows_and_refuse_read_followups() {
    run(async |cx| {
        let (fgp, address, shutdown, serving) = start_both(cx, "explicit-write-only").await;
        let mut write_grant = grant(Rights::Write);
        write_grant.limits.max_rows = 0;
        let mut client = login(address, &write_grant).await;
        client.expect(0x11, vec![Value::Map(vec![])], SUCCESS).await;
        client
            .expect(
                0x10,
                request("CREATE (:Person {name:'Kept',age:7})", vec![], vec![]),
                SUCCESS,
            )
            .await;
        let (rows, provisional) = client.pull_all().await;
        assert!(rows.is_empty());
        assert!(get(&provisional, "bookmark").is_none());
        let committed = client.expect(0x12, vec![], SUCCESS).await;
        assert_eq!(
            get(&committed, "bookmark").and_then(Value::as_str),
            Some("fgdb:social:1")
        );
        client.expect(0x11, vec![Value::Map(vec![])], SUCCESS).await;
        client
            .expect(
                0x10,
                request("CREATE (:Person {name:'Aborted',age:9})", vec![], vec![]),
                SUCCESS,
            )
            .await;
        assert!(client.pull_all().await.0.is_empty());
        client
            .expect(
                0x10,
                request("RETURN 1 AS forbidden", vec![], vec![]),
                FAILURE,
            )
            .await;
        client.expect(0x12, vec![], IGNORED).await;
        client.expect(0x0f, vec![], SUCCESS).await;
        let mut observer = Client::connect(cx, fgp, token(&grant(Rights::Read)))
            .await
            .unwrap();
        observer.select(cx, "social").await.unwrap();
        let observed = observer
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (n:Person) RETURN n.name AS name",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(observed.rows, vec![vec![text("Kept")]]);
        assert_eq!(observed.outcome, Outcome::Rows { seq: 1 });
        client.send(0x02, vec![]).await;
        assert!(client.receive().await.is_none());
        observer.close(cx).await.unwrap();
        finish(cx, shutdown, serving).await;
    });
}
