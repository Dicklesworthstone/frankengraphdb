//! `fgdb remote`: run one statement against an `fgdbd` server over FGP.
//!
//! Output is the local `query`/`write` contract: robot `columns`, `row` and
//! `result` records with the same cell encoding (kinds `rows` and `written`),
//! or the human table. The server's error classes map onto the CLI's exit
//! codes: refused statements, permissions and budgets are query failures (3),
//! authentication and database selection are open failures (4), and
//! transport failures or an unknown commit outcome are I/O failures (5).

use super::{Failure, emit, float_text, hex, render_row_body, render_rows};
use crate::load::{Json, parse_json};
use asupersync::Budget;
use asupersync::io::AsyncReadExt;
use asupersync::net::TcpStream;
use asupersync::tls::{Certificate, TlsConnector, TlsConnectorBuilder};
use core::future::{Future, poll_fn};
use core::pin::pin;
use core::task::Poll;
use fgdb_protocol::body::{ErrorCode, ExecuteMode, Outcome, WireValue};
use fgdb_protocol::client::{Client, ClientError};
use fgdb_protocol::json::{argument, cell};
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;

const MAX_TOKEN_FILE_BYTES: u64 = 65_536;
const MAX_TLS_CA_BYTES: usize = 1 << 20;
const MAX_JSON_VALUES: usize = 65_536;
const MAX_JSON_TOKEN_BYTES: usize = 65_536;

struct RemoteOptions {
    addr: SocketAddr,
    token_file: PathBuf,
    database: String,
    mode: ExecuteMode,
    statement: String,
    parameters: Vec<(String, WireValue)>,
    /// `subscribe` stops after this many batches (default: until interrupted).
    max_batches: Option<u64>,
    tls: Option<RemoteTls>,
}

struct RemoteTls {
    server_name: String,
    ca_file: PathBuf,
}

fn parse(args: &[String]) -> Result<RemoteOptions, Failure> {
    let mut addr = None;
    let mut token_file = None;
    let mut database = None;
    let mut positional = Vec::new();
    let mut parameters: Vec<(String, WireValue)> = Vec::new();
    let mut max_batches = None;
    let mut tls_server_name = None;
    let mut tls_ca_file = None;
    let mut at = 0;
    while at < args.len() {
        let flag = args[at].as_str();
        let mut value = || {
            at += 1;
            args.get(at)
                .cloned()
                .ok_or_else(|| Failure::usage(format!("{flag} needs a value")))
        };
        match flag {
            "--addr" if addr.is_none() => {
                addr = Some(
                    value()?
                        .parse::<SocketAddr>()
                        .map_err(|_| Failure::usage("--addr needs <ip>:<port>"))?,
                );
            }
            "--token-file" if token_file.is_none() => token_file = Some(PathBuf::from(value()?)),
            "--database" if database.is_none() => database = Some(value()?),
            "--tls-server-name" if tls_server_name.is_none() => {
                let name = value()?;
                if name.is_empty() || name.chars().any(char::is_whitespace) {
                    return Err(Failure::usage(
                        "--tls-server-name needs a DNS name or IP address",
                    ));
                }
                tls_server_name = Some(name);
            }
            "--tls-ca-file" if tls_ca_file.is_none() => {
                tls_ca_file = Some(PathBuf::from(value()?));
            }
            "--max-batches" if max_batches.is_none() => {
                let raw = value()?;
                if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(Failure::usage("--max-batches needs a decimal number"));
                }
                max_batches = Some(
                    raw.parse::<u64>()
                        .map_err(|_| Failure::usage("--max-batches is out of range"))?,
                );
            }
            "--param" => {
                let raw = value()?;
                let (name, typed) = raw
                    .split_once('=')
                    .ok_or_else(|| Failure::usage("--param needs name=type:value"))?;
                if parameters.iter().any(|(existing, _)| existing == name) {
                    return Err(Failure::usage("duplicate --param name"));
                }
                parameters.push((name.to_owned(), parameter(typed)?));
            }
            other if other.starts_with("--") => {
                return Err(Failure::usage(format!("unknown or duplicate flag {other}")));
            }
            _ => positional.push(args[at].clone()),
        }
        at += 1;
    }
    let [verb, statement] = <[String; 2]>::try_from(positional)
        .map_err(|_| Failure::usage("remote needs query|write|subscribe and one statement"))?;
    let mode = match verb.as_str() {
        "query" => ExecuteMode::Read,
        "write" => ExecuteMode::Write,
        "subscribe" => ExecuteMode::Subscribe,
        _ => return Err(Failure::usage("remote needs query, write or subscribe")),
    };
    if max_batches.is_some() && mode != ExecuteMode::Subscribe {
        return Err(Failure::usage("--max-batches applies only to subscribe"));
    }
    let tls = match (tls_server_name, tls_ca_file) {
        (Some(server_name), Some(ca_file)) => Some(RemoteTls {
            server_name,
            ca_file,
        }),
        (None, None) => None,
        _ => {
            return Err(Failure::usage(
                "TLS needs both --tls-server-name and --tls-ca-file",
            ));
        }
    };
    // `subscribe` takes the read itself; the SUBSCRIBE TO header is optional.
    let statement = if mode == ExecuteMode::Subscribe
        && !statement
            .trim_start()
            .get(..9)
            .is_some_and(|head| head.eq_ignore_ascii_case("SUBSCRIBE"))
    {
        format!("SUBSCRIBE TO {statement}")
    } else {
        statement
    };
    Ok(RemoteOptions {
        addr: addr.ok_or_else(|| Failure::usage("remote needs --addr"))?,
        token_file: token_file.ok_or_else(|| Failure::usage("remote needs --token-file"))?,
        database: database.ok_or_else(|| Failure::usage("remote needs --database"))?,
        mode,
        statement,
        parameters,
        max_batches,
        tls,
    })
}

/// Use only the explicitly supplied trust roots. The foundation validates
/// both the chain and peer name before the FGP client receives the stream.
fn tls_connector(pem: &[u8]) -> Result<TlsConnector, Failure> {
    if pem.len() > MAX_TLS_CA_BYTES {
        return Err(Failure::open("TLS CA file exceeds 1048576 bytes"));
    }
    let roots = Certificate::from_pem(pem)
        .map_err(|_| Failure::open("TLS CA file contains no valid PEM certificates"))?;
    TlsConnectorBuilder::new()
        .add_root_certificates(roots)
        .min_protocol_version(0x0304_u16.into())
        .max_protocol_version(0x0304_u16.into())
        .alpn_protocols_required(vec![b"fgp/1".to_vec()])
        .enable_early_data(false)
        .handshake_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|_| Failure::open("TLS CA file cannot establish a verified client configuration"))
}

async fn read_tls_connector(
    cx: &asupersync::Cx,
    path: &std::path::Path,
) -> Result<TlsConnector, Failure> {
    cx.checkpoint()
        .map_err(|_| Failure::open("TLS setup cancelled"))?;
    let mut file = asupersync::fs::File::open(path)
        .await
        .map_err(|_| Failure::open("cannot open TLS CA file"))?;
    let metadata = file
        .metadata()
        .await
        .map_err(|_| Failure::open("cannot inspect TLS CA file"))?;
    if !metadata.is_file() || metadata.len() > MAX_TLS_CA_BYTES as u64 {
        return Err(Failure::open(
            "TLS CA file must be a regular file of at most 1048576 bytes",
        ));
    }
    // Bound the read itself, including growth after the metadata check. File
    // and metadata refer to the same open handle, not two pathname lookups.
    let mut pem = Vec::with_capacity(metadata.len() as usize);
    let mut buffer = [0u8; 8192];
    loop {
        cx.checkpoint()
            .map_err(|_| Failure::open("TLS setup cancelled"))?;
        let remaining = (MAX_TLS_CA_BYTES - pem.len() + 1).min(buffer.len());
        let count = file
            .read(&mut buffer[..remaining])
            .await
            .map_err(|_| Failure::open("cannot read TLS CA file"))?;
        if count == 0 {
            break;
        }
        if count > MAX_TLS_CA_BYTES - pem.len() {
            return Err(Failure::open("TLS CA file exceeds 1048576 bytes"));
        }
        pem.extend_from_slice(&buffer[..count]);
    }
    tls_connector(&pem)
}

async fn connect(cx: &asupersync::Cx, options: &RemoteOptions) -> Result<Client, Failure> {
    let token = read_token(&options.token_file).await?;
    let Some(tls) = &options.tls else {
        return Client::connect(cx, options.addr, token)
            .await
            .map_err(failure);
    };
    let connector = read_tls_connector(cx, &tls.ca_file).await?;
    let stream = TcpStream::connect(options.addr)
        .await
        .map_err(Failure::open)?;
    let _ = stream.set_nodelay(true);
    let mut handshake = pin!(connector.connect(&tls.server_name, stream));
    let stream = poll_fn(|task| {
        if cx.checkpoint().is_err() {
            return Poll::Ready(Err(Failure::open("TLS handshake cancelled")));
        }
        handshake.as_mut().poll(task).map(|result| {
            result.map_err(|error| Failure::open(format!("TLS handshake failed: {error}")))
        })
    })
    .await?;
    // Cancellation or a TLS error drops the transport. In particular, there
    // is no retry that could send the credential or a write over plaintext.
    Client::connect_stream(cx, stream, token)
        .await
        .map_err(failure)
}

/// The local CLI's parameter spellings, as wire values: `int:`, `float:`,
/// `text:`, `bool:true|false`, `null`, `json:<array|object>` (nested lists
/// and maps), `bytes:<hex>`, and `vector:<x,y,...>` (an embedding as
/// packed little-endian f32 bytes).
fn parameter(raw: &str) -> Result<WireValue, Failure> {
    if let Some(value) = raw.strip_prefix("int:") {
        return value
            .parse()
            .map(WireValue::Int)
            .map_err(|_| Failure::usage("invalid int parameter"));
    }
    if let Some(value) = raw.strip_prefix("float:") {
        let value: f64 = value
            .parse()
            .map_err(|_| Failure::usage("invalid float parameter"))?;
        if !value.is_finite() {
            return Err(Failure::usage("a float parameter must be finite"));
        }
        return Ok(WireValue::Float(value));
    }
    if let Some(value) = raw.strip_prefix("text:") {
        return Ok(WireValue::Text(value.to_owned()));
    }
    if let Some(value) = raw.strip_prefix("bytes:") {
        return fgdb_protocol::json::bytes_from_hex(value)
            .map(WireValue::Bytes)
            .map_err(Failure::usage);
    }
    if let Some(value) = raw.strip_prefix("vector:") {
        return packed_vector(value).map(WireValue::Bytes);
    }
    if let Some(value) = raw.strip_prefix("json:") {
        let json = parse_json(value, MAX_JSON_VALUES, MAX_JSON_TOKEN_BYTES)
            .map_err(|error| Failure::usage(format!("invalid json parameter: {error}")))?;
        if !matches!(json, Json::Array(_) | Json::Object(_)) {
            return Err(Failure::usage(
                "invalid json parameter: expected a JSON array or object",
            ));
        }
        return argument(&json)
            .map_err(|error| Failure::usage(format!("invalid json parameter: {error}")));
    }
    match raw {
        "bool:true" => Ok(WireValue::Bool(true)),
        "bool:false" => Ok(WireValue::Bool(false)),
        "null" => Ok(WireValue::Null),
        _ => Err(Failure::usage("invalid parameter type or value")),
    }
}

/// A bearer token is a credential: an owner-only file of hex text.
async fn read_token(path: &std::path::Path) -> Result<Vec<u8>, Failure> {
    let unreadable = |_| Failure::open("cannot read token file");
    let metadata = asupersync::fs::metadata(path).await.map_err(unreadable)?;
    if !metadata.is_file() {
        return Err(Failure::open("token file must be a regular file"));
    }
    #[cfg(unix)]
    {
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Failure::open(
                "token file must not be accessible to group or others (chmod 600)",
            ));
        }
    }
    if metadata.len() > MAX_TOKEN_FILE_BYTES {
        return Err(Failure::open("token file exceeds 65536 bytes"));
    }
    let text = asupersync::fs::read_to_string(path)
        .await
        .map_err(unreadable)?;
    let text = text.trim();
    if text.len() % 2 != 0 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Failure::open("token file must hold one hexadecimal token"));
    }
    Ok(text
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let digit = |b: u8| {
                if b.is_ascii_digit() {
                    b - b'0'
                } else {
                    b.to_ascii_lowercase() - b'a' + 10
                }
            };
            (digit(pair[0]) << 4) | digit(pair[1])
        })
        .collect())
}

fn failure(error: ClientError) -> Failure {
    match error {
        ClientError::Server { code, message } => {
            let message = format!("{}: {message}", code.name());
            match code {
                ErrorCode::Statement
                | ErrorCode::PermissionDenied
                | ErrorCode::Budget
                | ErrorCode::Conflict
                | ErrorCode::Execution
                | ErrorCode::Busy
                | ErrorCode::Cancelled => Failure::query(message),
                ErrorCode::Unauthenticated
                | ErrorCode::NotFoundOrUnauthorized
                | ErrorCode::UnsupportedVersion => Failure::open(message),
                ErrorCode::Protocol | ErrorCode::OutcomeUnknown | ErrorCode::Draining => {
                    Failure::io(message)
                }
            }
        }
        ClientError::Io(_) => Failure::open(error),
        other => Failure::io(other),
    }
}

pub(crate) fn run(args: &[String], robot: bool, out: &mut impl Write) -> Result<(), Failure> {
    let options = parse(args)?;
    let runtime = fgdb::runtime_builder().build().map_err(Failure::io)?;
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    if options.mode == ExecuteMode::Subscribe {
        return runtime.block_on(subscribe(&cx, options, robot, out));
    }
    let answer = runtime.block_on(async {
        let mut client = connect(&cx, &options).await?;
        client
            .select(&cx, &options.database)
            .await
            .map_err(failure)?;
        let answer = client
            .execute(&cx, options.mode, &options.statement, options.parameters)
            .await
            .map_err(failure)?;
        // The answer is complete; a failed DRAIN cannot change it.
        let _ = client.close(&cx).await;
        Ok::<_, Failure>(answer)
    })?;
    match answer.outcome {
        Outcome::Rows { seq } => {
            let rendered = answer
                .rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|value| if robot { cell(value) } else { human(value) })
                        .collect()
                })
                .collect();
            render_rows(&answer.columns, rendered, seq, "rows", robot, out)
        }
        // A CREATE/INSERT ... RETURN answers its rows with the commit, in the
        // same frames as the embedded write path.
        Outcome::WriteCommitted { seq, statements } | Outcome::ReadClosed { seq, statements }
            if !answer.columns.is_empty() =>
        {
            let rendered: Vec<Vec<String>> = answer
                .rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|value| if robot { cell(value) } else { human(value) })
                        .collect()
                })
                .collect();
            let count = rendered.len();
            render_row_body(&answer.columns, &rendered, robot, out)?;
            if robot {
                emit(
                    out,
                    &format!(
                        r#"{{"v":1,"event":"result","kind":"written","seq":{seq},"count":{count},"statements":{statements}}}"#
                    ),
                )
            } else {
                emit(out, &format!("{count} row(s), completed at seq {seq}"))
            }
        }
        Outcome::WriteCommitted { seq, statements } | Outcome::ReadClosed { seq, statements } => {
            if robot {
                emit(
                    out,
                    &format!(
                        r#"{{"v":1,"event":"result","kind":"written","seq":{seq},"statements":{statements}}}"#
                    ),
                )
            } else {
                emit(out, &format!("completed at seq {seq}"))
            }
        }
    }
}

/// Stream a subscription: the columns once, then per batch one `change`
/// record per entry (its signed multiplicity change; a baseline's weights are
/// multiplicities) and one `progress` record carrying the batch's entry count
/// and frontier. Each batch is flushed as it arrives. With --max-batches the
/// subscription is cancelled after that many batches and a final `result`
/// record (kind `rows`, the last delivered frontier, the total change count)
/// marks a complete stream; without it the stream runs until interrupted.
async fn subscribe(
    cx: &asupersync::Cx,
    options: RemoteOptions,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let mut client = connect(cx, &options).await?;
    client
        .select(cx, &options.database)
        .await
        .map_err(failure)?;
    // Both callbacks write the one output stream; a cell shares it.
    let sink = core::cell::RefCell::new((out, None::<Failure>));
    let write_line = |line: &str, flush: bool| {
        let (out, failed) = &mut *sink.borrow_mut();
        let result = emit(*out, line).and_then(|()| {
            if flush {
                out.flush().map_err(Failure::io)
            } else {
                Ok(())
            }
        });
        if let Err(error) = result {
            failed.get_or_insert(error);
        }
    };
    let mut batches = 0u64;
    let mut changes = 0u64;
    let limit = options.max_batches;
    let end = client
        .subscribe(
            cx,
            &options.statement,
            options.parameters,
            |columns| {
                let line = if robot {
                    format!(
                        r#"{{"v":1,"event":"columns","columns":[{}]}}"#,
                        columns
                            .iter()
                            .map(|c| fgdb_protocol::json::quote(c))
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                } else {
                    format!("weight | {}", columns.join(" | "))
                };
                write_line(&line, false);
            },
            |change| {
                for (weight, row) in &change.entries {
                    let line = if robot {
                        format!(
                            r#"{{"v":1,"event":"change","weight":"{weight}","cells":[{}]}}"#,
                            row.iter().map(cell).collect::<Vec<_>>().join(",")
                        )
                    } else {
                        format!(
                            "{weight:+} | {}",
                            row.iter().map(human).collect::<Vec<_>>().join(" | ")
                        )
                    };
                    write_line(&line, false);
                }
                changes += change.entries.len() as u64;
                let line = if robot {
                    format!(
                        r#"{{"v":1,"event":"progress","rows":{},"seq":{}}}"#,
                        change.entries.len(),
                        change.frontier
                    )
                } else {
                    let kind = if change.snapshot { "baseline" } else { "delta" };
                    format!(
                        "-- {kind} at seq {} ({} change(s))",
                        change.frontier,
                        change.entries.len()
                    )
                };
                write_line(&line, true);
                batches += 1;
                // Stop on an output failure or at the requested batch count.
                Ok(sink.borrow().1.is_none() && limit.is_none_or(|limit| batches < limit))
            },
        )
        .await
        .map_err(failure)?;
    let _ = client.close(cx).await;
    let (out, failed) = sink.into_inner();
    if let Some(error) = failed {
        return Err(error);
    }
    if robot {
        emit(
            out,
            &format!(r#"{{"v":1,"event":"result","kind":"rows","seq":{end},"count":{changes}}}"#),
        )
    } else {
        emit(out, &format!("{changes} change(s) through seq {end}"))
    }
}

fn human(value: &WireValue) -> String {
    let ids = |ids: &[u128]| {
        ids.iter()
            .map(u128::to_string)
            .collect::<Vec<_>>()
            .join(",")
    };
    match value {
        WireValue::Null => "NULL".into(),
        WireValue::Bool(v) => v.to_string(),
        WireValue::Int(v) => v.to_string(),
        WireValue::Text(v) => v.chars().flat_map(char::escape_default).collect(),
        WireValue::Decimal(v) => v.clone(),
        WireValue::Float(v) => float_text(*v),
        WireValue::Timestamp(_) => cell(value),
        WireValue::Bytes(v) => format!("0x{}", hex(v)),
        WireValue::Vertex(v) => format!("vertex {v}"),
        WireValue::Edge(v) => format!("edge {v}"),
        WireValue::Path { start, steps } => {
            let mut text = format!("path({start}");
            for (edge, vertex) in steps {
                text.push_str(&format!(" -{edge}-> {vertex}"));
            }
            text.push(')');
            text
        }
        WireValue::Vertices(v) => format!("vertices({})", ids(v)),
        WireValue::Edges(v) => format!("edges({})", ids(v)),
        WireValue::List(items) => format!(
            "[{}]",
            items.iter().map(human).collect::<Vec<_>>().join(", ")
        ),
        WireValue::Map(entries) => format!(
            "{{{}}}",
            entries
                .iter()
                .map(|(key, value)| {
                    let key: String = key.chars().flat_map(char::escape_default).collect();
                    format!("{key}: {}", human(value))
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        WireValue::Count(v) => v.to_string(),
        WireValue::WideInt(v) => v.to_string(),
        WireValue::Average {
            numerator,
            denominator,
        } => format!("{numerator}/{denominator}"),
    }
}

/// `x,y,...` as packed little-endian f32 bytes, each coordinate finite.
pub(crate) fn packed_vector(text: &str) -> Result<Vec<u8>, Failure> {
    let mut bytes = Vec::new();
    for raw in text.split(',') {
        let value: f32 = raw
            .trim()
            .parse()
            .map_err(|_| Failure::usage("vector: is comma-separated numbers"))?;
        if !value.is_finite() {
            return Err(Failure::usage("vector: coordinates must be finite f32"));
        }
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(verb: &str, flags: &[&str]) -> Result<RemoteOptions, Failure> {
        let mut args = vec![
            "--addr",
            "127.0.0.1:7688",
            "--token-file",
            "token.hex",
            "--database",
            "social",
            verb,
            "MATCH (n) RETURN n",
        ];
        args.extend_from_slice(flags);
        parse(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn tls_requires_one_explicit_name_and_trust_file_for_every_remote_mode() {
        for verb in ["query", "write", "subscribe"] {
            let plain = options(verb, &[]).unwrap_or_else(|error| panic!("{}", error.message));
            assert!(plain.tls.is_none());
            let encrypted = options(
                verb,
                &[
                    "--tls-server-name",
                    "db.example.test",
                    "--tls-ca-file",
                    "ca.pem",
                ],
            )
            .unwrap_or_else(|error| panic!("{}", error.message));
            let tls = encrypted.tls.expect("explicit encrypted connection");
            assert_eq!(tls.server_name, "db.example.test");
            assert_eq!(tls.ca_file, PathBuf::from("ca.pem"));
            for flags in [
                vec!["--tls-server-name", "db.example.test"],
                vec!["--tls-ca-file", "ca.pem"],
                vec!["--tls-server-name", "", "--tls-ca-file", "ca.pem"],
                vec!["--tls-server-name", "db example", "--tls-ca-file", "ca.pem"],
                vec![
                    "--tls-server-name",
                    "db.example.test",
                    "--tls-ca-file",
                    "ca.pem",
                    "--tls-ca-file",
                    "other.pem",
                ],
                vec!["--tls-insecure"],
            ] {
                let failure = options(verb, &flags)
                    .err()
                    .expect("invalid TLS flags refuse");
                assert_eq!(failure.code, 2);
            }
        }
    }

    #[test]
    fn tls_trust_configuration_fails_closed_before_connecting() {
        for pem in [
            Vec::new(),
            b"not a certificate".to_vec(),
            vec![b'x'; MAX_TLS_CA_BYTES + 1],
        ] {
            let error = tls_connector(&pem).expect_err("untrusted configuration must refuse");
            assert_eq!(error.code, 4);
        }
    }

    #[test]
    fn remote_json_parameters_accept_nested_objects_without_coercion() {
        let value =
            parameter(r#"json:{"name":"Ada","nested":{"active":true},"scores":[2,null,3.5]}"#)
                .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(
            value,
            WireValue::Map(vec![
                ("name".into(), WireValue::Text("Ada".into())),
                (
                    "nested".into(),
                    WireValue::Map(vec![("active".into(), WireValue::Bool(true))])
                ),
                (
                    "scores".into(),
                    WireValue::List(vec![
                        WireValue::Int(2),
                        WireValue::Null,
                        WireValue::Float(3.5)
                    ])
                ),
            ])
        );
        assert!(parameter("json:42").is_err());
        assert!(parameter(r#"json:{"a":1,"a":2}"#).is_err());
    }
}
