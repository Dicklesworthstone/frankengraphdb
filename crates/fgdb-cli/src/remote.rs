//! `fgdb remote`: run one statement against an `fgdbd` server over FGP.
//!
//! Output is the local `query`/`write` contract: robot `columns`, `row` and
//! `result` records with the same cell encoding (kinds `rows` and `written`),
//! or the human table. The server's error classes map onto the CLI's exit
//! codes: refused statements, permissions and budgets are query failures (3),
//! authentication and database selection are open failures (4), and
//! transport failures or an unknown commit outcome are I/O failures (5).

use super::{Failure, emit, float_text, hex, quoted, render_rows};
use crate::load::{Json, parse_json};
use asupersync::Budget;
use fgdb_protocol::body::{ErrorCode, ExecuteMode, Outcome, WireTimestamp, WireValue};
use fgdb_protocol::client::{Client, ClientError};
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;

const MAX_TOKEN_FILE_BYTES: u64 = 65_536;
const MAX_JSON_VALUES: usize = 65_536;
const MAX_JSON_TOKEN_BYTES: usize = 65_536;

struct RemoteOptions {
    addr: SocketAddr,
    token_file: PathBuf,
    database: String,
    mode: ExecuteMode,
    statement: String,
    parameters: Vec<(String, WireValue)>,
}

fn parse(args: &[String]) -> Result<RemoteOptions, Failure> {
    let mut addr = None;
    let mut token_file = None;
    let mut database = None;
    let mut positional = Vec::new();
    let mut parameters: Vec<(String, WireValue)> = Vec::new();
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
        .map_err(|_| Failure::usage("remote needs query|write and one statement"))?;
    let mode = match verb.as_str() {
        "query" => ExecuteMode::Read,
        "write" => ExecuteMode::Write,
        _ => return Err(Failure::usage("remote needs query or write")),
    };
    Ok(RemoteOptions {
        addr: addr.ok_or_else(|| Failure::usage("remote needs --addr"))?,
        token_file: token_file.ok_or_else(|| Failure::usage("remote needs --token-file"))?,
        database: database.ok_or_else(|| Failure::usage("remote needs --database"))?,
        mode,
        statement,
        parameters,
    })
}

/// The local CLI's parameter spellings, as wire values: `int:`, `float:`,
/// `text:`, `bool:true|false`, `null`, and `json:<array>` (a list whose
/// objects are maps).
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
    if let Some(value) = raw.strip_prefix("json:") {
        let json = parse_json(value, MAX_JSON_VALUES, MAX_JSON_TOKEN_BYTES)
            .map_err(|error| Failure::usage(format!("invalid json parameter: {error}")))?;
        if !matches!(json, Json::Array(_)) {
            return Err(Failure::usage(
                "invalid json parameter: expected a JSON array",
            ));
        }
        return json_value(&json);
    }
    match raw {
        "bool:true" => Ok(WireValue::Bool(true)),
        "bool:false" => Ok(WireValue::Bool(false)),
        "null" => Ok(WireValue::Null),
        _ => Err(Failure::usage("invalid parameter type or value")),
    }
}

fn json_value(json: &Json) -> Result<WireValue, Failure> {
    let bad = |detail: &str| Failure::usage(format!("invalid json parameter: {detail}"));
    Ok(match json {
        Json::Null => WireValue::Null,
        Json::Bool(value) => WireValue::Bool(*value),
        Json::Number(text) if text.bytes().any(|b| matches!(b, b'.' | b'e' | b'E')) => {
            let value: f64 = text.parse().map_err(|_| bad("number"))?;
            if !value.is_finite() {
                return Err(bad("float out of range"));
            }
            WireValue::Float(value)
        }
        Json::Number(text) => {
            WireValue::Int(text.parse().map_err(|_| bad("integer out of range"))?)
        }
        Json::String(text) => WireValue::Text(text.clone()),
        Json::Array(items) => {
            WireValue::List(items.iter().map(json_value).collect::<Result<_, _>>()?)
        }
        // BTreeMap order is UTF-8 byte order: exactly the wire map's canonical order.
        Json::Object(fields) => WireValue::Map(
            fields
                .iter()
                .map(|(key, value)| Ok((key.clone(), json_value(value)?)))
                .collect::<Result<_, Failure>>()?,
        ),
    })
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
    let answer = runtime.block_on(async {
        let token = read_token(&options.token_file).await?;
        let mut client = Client::connect(&cx, options.addr, token)
            .await
            .map_err(failure)?;
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
                        .map(|value| {
                            if robot {
                                robot_cell(value)
                            } else {
                                human(value)
                            }
                        })
                        .collect()
                })
                .collect();
            render_rows(&answer.columns, rendered, seq, "rows", robot, out)
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

/// The robot cell encoding of the local CLI, for a wire value.
fn robot_cell(value: &WireValue) -> String {
    let ids = |ids: &[u128]| {
        ids.iter()
            .map(|id| quoted(&id.to_string()))
            .collect::<Vec<_>>()
            .join(",")
    };
    match value {
        WireValue::Null => r#"{"type":"null"}"#.to_owned(),
        WireValue::Bool(v) => format!(r#"{{"type":"bool","value":{v}}}"#),
        WireValue::Int(v) => format!(r#"{{"type":"int","value":"{v}"}}"#),
        WireValue::Text(v) => format!(r#"{{"type":"text","value":{}}}"#, quoted(v)),
        WireValue::Decimal(v) => format!(r#"{{"type":"decimal","value":"{v}"}}"#),
        WireValue::Float(v) => format!(r#"{{"type":"float","value":{}}}"#, quoted(&float_text(*v))),
        WireValue::Timestamp(v) => format!(r#"{{"type":"timestamp","value":{}}}"#, timestamp(v)),
        WireValue::Bytes(v) => format!(r#"{{"type":"bytes","value":{}}}"#, quoted(&hex(v))),
        WireValue::Vertex(v) => format!(r#"{{"type":"vertex","value":"{v}"}}"#),
        WireValue::Edge(v) => format!(r#"{{"type":"edge","value":"{v}"}}"#),
        WireValue::Path { start, steps } => {
            let mut nodes = vec![quoted(&start.to_string())];
            let mut edges = Vec::new();
            for (edge, vertex) in steps {
                edges.push(quoted(&edge.to_string()));
                nodes.push(quoted(&vertex.to_string()));
            }
            format!(
                r#"{{"type":"path","value":{{"nodes":[{}],"edges":[{}]}}}}"#,
                nodes.join(","),
                edges.join(",")
            )
        }
        WireValue::Vertices(v) => format!(r#"{{"type":"vertices","value":[{}]}}"#, ids(v)),
        WireValue::Edges(v) => format!(r#"{{"type":"edges","value":[{}]}}"#, ids(v)),
        WireValue::List(items) => format!(
            r#"{{"type":"list","value":[{}]}}"#,
            items.iter().map(robot_cell).collect::<Vec<_>>().join(",")
        ),
        WireValue::Map(entries) => format!(
            r#"{{"type":"map","value":{{{}}}}}"#,
            entries
                .iter()
                .map(|(key, value)| format!("{}:{}", quoted(key), robot_cell(value)))
                .collect::<Vec<_>>()
                .join(",")
        ),
        WireValue::Count(v) => format!(r#"{{"type":"count","value":"{v}"}}"#),
        WireValue::WideInt(v) => format!(r#"{{"type":"wideint","value":"{v}"}}"#),
        WireValue::Average {
            numerator,
            denominator,
        } => format!(r#"{{"type":"average","value":"{numerator}/{denominator}"}}"#),
    }
}

fn timestamp(value: &WireTimestamp) -> String {
    let zone = value.zone.as_ref().map_or_else(
        || "null".to_owned(),
        |zone| {
            format!(
                r#"{{"identifier":{},"tzdb_oid":"{}"}}"#,
                quoted(&zone.identifier),
                hex(&zone.tzdb_oid)
            )
        },
    );
    format!(
        r#"{{"instant_utc_nanos":"{}","utc_offset_seconds":{},"zone":{zone}}}"#,
        value.instant_utc_nanos, value.utc_offset_seconds
    )
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
        WireValue::Timestamp(v) => timestamp(v),
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
