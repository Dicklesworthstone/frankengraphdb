//! Real TLS 1.3 transports exercise the ordinary FGP, HTTP and Bolt owners.
//! These certificates and keys are public test fixtures, valid only for the
//! local test names. They are never deployment identities.

use asupersync::io::{AsyncReadExt, AsyncWriteExt};
use asupersync::net::{TcpListener, TcpStream};
use asupersync::security::key::AuthKey;
use asupersync::tls::{Certificate, TlsConnector, TlsConnectorBuilder, TlsStream};
use asupersync::{Budget, Cx};
use fgdb::{Database, DatabaseKeys};
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_gql::GraphSymbolKind;
use fgdb_protocol::body::{ExecuteMode, Outcome, WireValue};
use fgdb_protocol::client::Client;
use fgdb_server::{DatabaseConfig, Server, ServerLimits, Symbols, TlsConfig, issue_token, issuer};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Grant, QueryLimits, Rights, Scope};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

const CA: &[u8] = br#"-----BEGIN CERTIFICATE-----
MIIBdDCCARqgAwIBAgICE4kwCgYIKoZIzj0EAwIwLjEsMCoGA1UEAwwjRnJhbmtl
bkdyYXBoREIgdGVzdCBmaXh0dXJlIENBIG9ubHkwIBcNMjAwMTAxMDAwMDAwWhgP
MjEyMDAxMDEwMDAwMDBaMC4xLDAqBgNVBAMMI0ZyYW5rZW5HcmFwaERCIHRlc3Qg
Zml4dHVyZSBDQSBvbmx5MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAENlNXR+u9
u2VU1q9jsv/E1eRq+0f+Zh2ru9nYOZfJCastw2JtROElpDnb78istal2nx6aZloW
XudGkEOmP+XzzqMmMCQwEgYDVR0TAQH/BAgwBgEB/wIBADAOBgNVHQ8BAf8EBAMC
AYYwCgYIKoZIzj0EAwIDSAAwRQIgd2c3ee/jya3d5QhTnj+0voGC8Ll1f4qBsRTD
e5grddcCIQDaMN3IFKk+QQ91YorO566mJl+GSW+DOH7Hym/+weMHiQ==
-----END CERTIFICATE-----
"#;
const CHAIN: &[u8] = br#"-----BEGIN CERTIFICATE-----
MIIBhTCCASugAwIBAgICE4owCgYIKoZIzj0EAwIwLjEsMCoGA1UEAwwjRnJhbmtl
bkdyYXBoREIgdGVzdCBmaXh0dXJlIENBIG9ubHkwIBcNMjAwMTAxMDAwMDAwWhgP
MjEyMDAxMDEwMDAwMDBaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDBZMBMGByqGSM49
AgEGCCqGSM49AwEHA0IABMUCMDbAhZ4+1+Ns1Y+Y0Cp65mVHbnPHCq17EfgruU3+
KXwv9LWPghQx3zMJ43/fRK6nBIjsUo6Y1DOfrdyrgxSjUTBPMAwGA1UdEwEB/wQC
MAAwGgYDVR0RBBMwEYIJbG9jYWxob3N0hwR/AAABMBMGA1UdJQQMMAoGCCsGAQUF
BwMBMA4GA1UdDwEB/wQEAwIHgDAKBggqhkjOPQQDAgNIADBFAiEA2ef+LpleZCce
zbwngpzt8VLMIcSzXNw5ATapKIxZPGECIFBLyNNiRV/J9qVTs0oUk77/OINqvVeQ
t9Q/+V1f/yq+
-----END CERTIFICATE-----
-----BEGIN CERTIFICATE-----
MIIBdDCCARqgAwIBAgICE4kwCgYIKoZIzj0EAwIwLjEsMCoGA1UEAwwjRnJhbmtl
bkdyYXBoREIgdGVzdCBmaXh0dXJlIENBIG9ubHkwIBcNMjAwMTAxMDAwMDAwWhgP
MjEyMDAxMDEwMDAwMDBaMC4xLDAqBgNVBAMMI0ZyYW5rZW5HcmFwaERCIHRlc3Qg
Zml4dHVyZSBDQSBvbmx5MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAENlNXR+u9
u2VU1q9jsv/E1eRq+0f+Zh2ru9nYOZfJCastw2JtROElpDnb78istal2nx6aZloW
XudGkEOmP+XzzqMmMCQwEgYDVR0TAQH/BAgwBgEB/wIBADAOBgNVHQ8BAf8EBAMC
AYYwCgYIKoZIzj0EAwIDSAAwRQIgd2c3ee/jya3d5QhTnj+0voGC8Ll1f4qBsRTD
e5grddcCIQDaMN3IFKk+QQ91YorO566mJl+GSW+DOH7Hym/+weMHiQ==
-----END CERTIFICATE-----
"#;
const KEY: &[u8] = br#"-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgMvL2pEw7WJS90yku
fqqCHSfRUdxOql39jNXrqGIDBWShRANCAATFAjA2wIWePtfjbNWPmNAqeuZlR25z
xwqtexH4K7lN/il8L/S1j4IUMd8zCeN/30SupwSI7FKOmNQzn63cq4MU
-----END PRIVATE KEY-----
"#;
const OTHER_KEY: &[u8] = br#"-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgJFIlQhx0N2GUhM49
eWssds8jv5l+oJcK23J9awh1YBKhRANCAAQ2U1dH6727ZVTWr2Oy/8TV5Gr7R/5m
Hau72dg5l8kJqy3DYm1E4SWkOdvvyKy1qXafHppmWhZe50aQQ6Y/5fPO
-----END PRIVATE KEY-----
"#;

const UNRELATED_CA: &[u8] = br#"-----BEGIN CERTIFICATE-----
MIIBRzCB7qADAgECAgITizAKBggqhkjOPQQDAjAgMR4wHAYDVQQDDBVVbnJlbGF0
ZWQgVExTIHRlc3QgQ0EwIBcNMjAwMTAxMDAwMDAwWhgPMjEyMDAxMDEwMDAwMDBa
MCAxHjAcBgNVBAMMFVVucmVsYXRlZCBUTFMgdGVzdCBDQTBZMBMGByqGSM49AgEG
CCqGSM49AwEHA0IABLDf0gMr0g9AP4xyBjHvh6y2FQ2H7VFJWNYXBh0Y77HMYG0e
5K+MN5DXgvkZOr0EsOddXsilzPFuFucF4+D1DZKjFjAUMBIGA1UdEwEB/wQIMAYB
Af8CAQAwCgYIKoZIzj0EAwIDSAAwRQIhAIXmy2reTFLT+XLhSMnQvsVj4h4cRVPN
08e/pzSWTiYoAiAnc8XiMg/sEU9FbUUPQuVhHy/MMSaJq2Qjjq+X6X7D6g==
-----END CERTIFICATE-----
"#;

fn run(test: impl AsyncFnOnce(&Cx)) {
    let runtime = fgdb::runtime_builder().build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(test(&cx));
}

fn connector(alpn: Option<&[u8]>, version: u16) -> TlsConnector {
    let mut builder = TlsConnectorBuilder::new()
        .add_root_certificates(Certificate::from_pem(CA).unwrap())
        .min_protocol_version(version.into())
        .max_protocol_version(version.into())
        .enable_early_data(false)
        .handshake_timeout(Duration::from_secs(3));
    if let Some(alpn) = alpn {
        builder = builder.alpn_protocols_required(vec![alpn.to_vec()]);
    }
    builder.build().unwrap()
}

async fn encrypted(addr: SocketAddr, alpn: Option<&[u8]>) -> TlsStream<TcpStream> {
    connector(alpn, 0x0304)
        .connect("localhost", TcpStream::connect(addr).await.unwrap())
        .await
        .unwrap()
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([51; 32], DatabaseSecurityNamespaceId([52; 32]), [53; 32])
}

fn token(scoped: bool) -> Vec<u8> {
    issue_token(
        &issuer(AuthKey::from_seed(5501), &keys(), 1).unwrap(),
        &Grant {
            branch: fgdb_server::TRUNK.into(),
            labels: if scoped {
                Scope::only([LabelId(1)])
            } else {
                Scope::All
            },
            relations: Scope::All,
            properties: if scoped {
                Scope::only([PropertyKeyId(1)])
            } else {
                Scope::All
            },
            rights: if scoped {
                Rights::Read
            } else {
                Rights::ReadWrite
            },
            limits: QueryLimits {
                max_nodes: 100_000,
                max_work: 10_000_000,
                max_rows: 100_000,
            },
            expires_at_ms: u64::MAX / 2,
        },
    )
    .unwrap()
}

async fn server(cx: &Cx, name: &str) -> Server {
    let path = std::env::temp_dir().join(format!("fgdb-server-tls-{}-{name}", std::process::id()));
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
    server.enable_tls(TlsConfig::from_pem(cx, CHAIN, KEY).unwrap());
    let mut symbols = Symbols::new();
    for (kind, name, id) in [
        (GraphSymbolKind::Label, "Person", 1),
        (GraphSymbolKind::Label, "Secret", 2),
        (GraphSymbolKind::Property, "name", 1),
        (GraphSymbolKind::Property, "secret", 2),
    ] {
        symbols.bind(kind, name, id).unwrap();
    }
    let mut config = DatabaseConfig::new("social", keys(), AuthKey::from_seed(5501));
    config.symbols = symbols;
    server.open_database(cx, &path, config).await.unwrap();
    server
}

async fn http_response(stream: &mut TlsStream<TcpStream>) -> String {
    let mut bytes = Vec::new();
    loop {
        if let Some(end) = bytes.windows(4).position(|chunk| chunk == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .expect("bounded HTTP response has Content-Length");
            if bytes.len() == end + 4 + length {
                return String::from_utf8(bytes).unwrap();
            }
            assert!(bytes.len() < end + 4 + length);
        }
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0, "complete HTTP response");
        bytes.extend_from_slice(&chunk[..count]);
    }
}

#[test]
fn encrypted_fgp_http_and_bolt_share_authorized_execution_and_drain() {
    run(async |cx| {
        let server = Arc::new(server(cx, "all-transports").await);
        let shutdown = server.shutdown();
        let mut addresses = Vec::new();
        let mut tasks = Vec::new();
        for protocol in 0..3 {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            addresses.push(listener.local_addr().unwrap());
            let server = Arc::clone(&server);
            tasks.push(
                cx.spawn(move |child| async move {
                    match protocol {
                        0 => server.serve(&child, listener).await.unwrap(),
                        1 => server
                            .serve_http(&child, listener, vec!["localhost".into()])
                            .await
                            .unwrap(),
                        _ => server.serve_bolt(&child, listener).await.unwrap(),
                    }
                })
                .unwrap(),
            );
        }
        let mut writer = Client::connect_stream(
            cx,
            encrypted(addresses[0], Some(b"fgp/1")).await,
            token(false),
        )
        .await
        .unwrap();
        writer.select(cx, "social").await.unwrap();
        let answer = writer.execute(cx, ExecuteMode::Write,
            "CREATE (:Person {name:'Ann',secret:'hidden'}), (:Person {name:'Bob'}), (:Person {name:'Cleo'}), (:Secret {name:'private'})",
            vec![]).await.unwrap();
        assert!(matches!(answer.outcome, Outcome::WriteCommitted { .. }));

        let mut reader = Client::connect_stream(
            cx,
            encrypted(addresses[0], Some(b"fgp/1")).await,
            token(true),
        )
        .await
        .unwrap();
        reader.select(cx, "social").await.unwrap();
        let answer = reader
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (p) RETURN p.name AS name, p.secret AS secret ORDER BY name",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(
            answer.rows,
            ["Ann", "Bob", "Cleo"].map(|name| vec![WireValue::Text(name.into()), WireValue::Null])
        );
        // Three rows cross the two-row initial window and require another
        // encrypted control frame before the result stream can complete.
        reader.ping(cx, 1001).await.unwrap();

        let credential: String = token(true).iter().map(|b| format!("{b:02x}")).collect();
        let body =
            r#"{"statement":"MATCH (p) RETURN p.name AS name, p.secret AS secret ORDER BY name"}"#;
        let request = format!(
            "POST /v1/databases/social/query HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {credential}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut http = encrypted(addresses[1], Some(b"http/1.1")).await;
        http.write_all(request.as_bytes()).await.unwrap();
        http.flush().await.unwrap();
        let response = http_response(&mut http).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("Ann") && response.contains("Cleo"));
        assert!(!response.contains("private") && !response.contains("hidden"));

        // The official Bolt 5.0 magic/version exchange and authenticated HELLO
        // use precisely the same codec after TLS, with no required Bolt ALPN.
        use fgdb_bolt::packstream::Value;
        let mut bolt = encrypted(addresses[2], None).await;
        let mut preamble = fgdb_bolt::message::MAGIC.to_vec();
        preamble.extend_from_slice(&[0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        bolt.write_all(&preamble).await.unwrap();
        bolt.flush().await.unwrap();
        let mut version = [0; 4];
        bolt.read_exact(&mut version).await.unwrap();
        assert_eq!(version, [0, 0, 0, 5]);
        let mut message = Vec::new();
        fgdb_bolt::packstream::encode(
            &Value::Struct {
                tag: 0x01,
                fields: vec![Value::Map(vec![
                    ("user_agent".into(), Value::string("fgdb-tls-test")),
                    ("scheme".into(), Value::string("bearer")),
                    ("credentials".into(), Value::string(credential)),
                ])],
            },
            &mut message,
        );
        let mut framed = Vec::new();
        fgdb_bolt::message::frame(&message, &mut framed);
        bolt.write_all(&framed).await.unwrap();
        bolt.flush().await.unwrap();
        let mut dechunker = fgdb_bolt::message::Dechunker::new(4096);
        let hello = loop {
            if let Some(message) = dechunker.next_message().unwrap() {
                break message;
            }
            let mut chunk = [0; 4096];
            let count = bolt.read(&mut chunk).await.unwrap();
            assert_ne!(count, 0);
            dechunker.push(&chunk[..count]);
        };
        assert!(matches!(
            fgdb_bolt::packstream::decode(&hello).unwrap(),
            Value::Struct { tag: 0x70, .. }
        ));
        reader.close(cx).await.unwrap();
        writer.close(cx).await.unwrap();
        drop(bolt);
        drop(http);
        // An idle peer with an unfinished TLS handshake cannot hold drain.
        let idle = TcpStream::connect(addresses[0]).await.unwrap();
        shutdown.trigger();
        for mut task in tasks {
            task.join(cx).await.unwrap();
        }
        drop(idle);
    });
}

#[test]
fn tls_refuses_plaintext_wrong_hostname_protocol_and_identity_without_fallback() {
    run(async |cx| {
        assert!(TlsConfig::from_pem(cx, CHAIN, OTHER_KEY).is_err());
        assert!(TlsConfig::from_pem(cx, b"invalid certificate", KEY).is_err());
        assert!(TlsConfig::from_pem(cx, CHAIN, b"invalid private key").is_err());
        let server = Arc::new(server(cx, "refusals").await);
        let shutdown = server.shutdown();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut task = cx
            .spawn(move |child| async move {
                server.serve(&child, listener).await.unwrap();
            })
            .unwrap();
        let unrelated = TlsConnectorBuilder::new()
            .add_root_certificates(Certificate::from_pem(UNRELATED_CA).unwrap())
            .alpn_protocols_required(vec![b"fgp/1".to_vec()])
            .min_protocol_version(0x0304_u16.into())
            .handshake_timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        assert!(
            unrelated
                .connect("localhost", TcpStream::connect(addr).await.unwrap())
                .await
                .is_err()
        );
        // Do not send any token: malformed plaintext cannot enter FGP AUTH.
        let mut plain = TcpStream::connect(addr).await.unwrap();
        plain
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        plain.flush().await.unwrap();
        let mut refused = [0; 64];
        assert!(
            plain
                .read(&mut refused)
                .await
                .map_or(true, |n| n == 0 || refused[0] == 21)
        );
        assert!(
            connector(Some(b"fgp/1"), 0x0304)
                .connect("wrong.invalid", TcpStream::connect(addr).await.unwrap())
                .await
                .is_err()
        );
        assert!(
            connector(Some(b"wrong-protocol"), 0x0304)
                .connect("localhost", TcpStream::connect(addr).await.unwrap())
                .await
                .is_err()
        );
        assert!(
            connector(Some(b"fgp/1"), 0x0303)
                .connect("localhost", TcpStream::connect(addr).await.unwrap())
                .await
                .is_err()
        );
        let mut valid =
            Client::connect_stream(cx, encrypted(addr, Some(b"fgp/1")).await, token(false))
                .await
                .unwrap();
        valid.select(cx, "social").await.unwrap();
        let answer = valid
            .execute(cx, ExecuteMode::Read, "MATCH (p) RETURN p", vec![])
            .await
            .unwrap();
        assert!(answer.rows.is_empty(), "refused transports made no effects");
        valid.close(cx).await.unwrap();
        shutdown.trigger();
        task.join(cx).await.unwrap();
    });
}
