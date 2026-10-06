//! The HTTP/1.1 JSON adapter: the same autocommit statements as FGP EXECUTE,
//! for clients that speak plain HTTP (curl, scripts, any language).
//!
//! An adapter may not weaken the native protocol (Appendix D), so this one
//! is a framing over the exact same execution path ([`crate::execute`]): the
//! same capability check, the same per-statement authorized session, the same
//! statement classes and the same closed error classes. Results are the same
//! ephemeral class as FGP's SNAPSHOT_RESULT stream, buffered whole because an
//! HTTP response cannot be flow-controlled per row.
//!
//! ```text
//! GET  /v1/health                          -> {"v":1,"status":"ok"}
//! POST /v1/databases/<name>/query          (read)
//! POST /v1/databases/<name>/write          (write)
//!      Authorization: Bearer <hex capability token>
//!      {"statement": "<gql>", "parameters": {"name": <json>, ...}}
//! ```
//!
//! A query answers `{"v":1,"columns":[...],"rows":[[cell,...],...],"seq":N}`
//! with the CLI robot contract's cells; a write answers
//! `{"v":1,"seq":N,"statements":M,"committed":true|false}`. A refusal answers
//! `{"v":1,"error":{"code":"<class>","message":"..."}}` under a status that
//! follows the class. A missing database and a token that may not select it
//! share one 404, as they share one FGP refusal. The health route reveals
//! nothing about databases.

use crate::execute::{Answer, Refusal, read, write};
use crate::{Served, Server};
use asupersync::Cx;
use asupersync::http::h1::types::{Method, Request, Response};
use fgdb_protocol::body::{ErrorCode, Execute, ExecuteMode, Outcome};
use fgdb_protocol::json::{Json, argument, cell, parse_json, quote};
use fgdb_warden::CapabilityToken;
use std::sync::Arc;

/// Value and token bounds for one request body. The statement itself is
/// bounded by the FGP body limit, so one token may be as long as it.
const MAX_JSON_VALUES: usize = 1 << 20;
const MAX_JSON_TOKEN_BYTES: usize = fgdb_protocol::body::MAX_STATEMENT_BYTES;

fn json_response(status: u16, body: String) -> Response {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    Response::new(status, reason, body.into_bytes())
        .with_header("Content-Type", "application/json")
        .with_header("Cache-Control", "no-store")
}

fn refusal_response(code: ErrorCode, message: &str) -> Response {
    let status = match code {
        ErrorCode::Protocol | ErrorCode::Statement | ErrorCode::UnsupportedVersion => 400,
        ErrorCode::Unauthenticated => 401,
        ErrorCode::PermissionDenied => 403,
        ErrorCode::NotFoundOrUnauthorized => 404,
        ErrorCode::Conflict => 409,
        ErrorCode::Budget => 422,
        ErrorCode::Busy => 429,
        ErrorCode::Draining => 503,
        ErrorCode::Execution | ErrorCode::OutcomeUnknown | ErrorCode::Cancelled => 500,
    };
    json_response(
        status,
        format!(
            r#"{{"v":1,"error":{{"code":"{}","message":{}}}}}"#,
            code.name(),
            quote(message)
        ),
    )
}

/// The bearer credential, decoded but not yet verified.
fn bearer(request: &Request) -> Result<CapabilityToken, Response> {
    let unauthenticated =
        || refusal_response(ErrorCode::Unauthenticated, "credential not accepted");
    let header = request
        .header_value("authorization")
        .ok_or_else(unauthenticated)?;
    let hex = header
        .strip_prefix("Bearer ")
        .ok_or_else(unauthenticated)?
        .trim();
    if hex.is_empty() || hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(unauthenticated());
    }
    let bytes: Vec<u8> = hex
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
        .collect();
    CapabilityToken::decode(&bytes).map_err(|_| unauthenticated())
}

/// The statement and arguments of one request body.
fn statement(request: &Request, mode: ExecuteMode) -> Result<Execute, Response> {
    let malformed = |detail: &str| refusal_response(ErrorCode::Protocol, detail);
    let text = core::str::from_utf8(&request.body).map_err(|_| malformed("body is not UTF-8"))?;
    let json = parse_json(text, MAX_JSON_VALUES, MAX_JSON_TOKEN_BYTES)
        .map_err(|error| malformed(&format!("invalid JSON body: {error}")))?;
    let Json::Object(mut fields) = json else {
        return Err(malformed("body must be a JSON object"));
    };
    let Some(Json::String(statement)) = fields.remove("statement") else {
        return Err(malformed("body needs a \"statement\" string"));
    };
    let parameters = match fields.remove("parameters") {
        None | Some(Json::Null) => Vec::new(),
        // BTreeMap order is UTF-8 byte order: the canonical parameter order.
        Some(Json::Object(parameters)) => parameters
            .iter()
            .map(|(name, value)| {
                argument(value)
                    .map(|value| (name.clone(), value))
                    .map_err(|error| malformed(&format!("parameter ${name}: {error}")))
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(malformed("\"parameters\" must be an object")),
    };
    if let Some(unknown) = fields.keys().next() {
        return Err(malformed(&format!("unknown body field {unknown:?}")));
    }
    Ok(Execute {
        mode,
        statement,
        parameters,
    })
}

fn answer_response(answer: &Answer) -> Response {
    match answer.outcome {
        Outcome::Rows { seq } => {
            let columns = answer
                .columns
                .iter()
                .map(|column| quote(column))
                .collect::<Vec<_>>()
                .join(",");
            let rows = answer
                .rows
                .iter()
                .map(|row| format!("[{}]", row.iter().map(cell).collect::<Vec<_>>().join(",")))
                .collect::<Vec<_>>()
                .join(",");
            json_response(
                200,
                format!(r#"{{"v":1,"columns":[{columns}],"rows":[{rows}],"seq":{seq}}}"#),
            )
        }
        Outcome::WriteCommitted { seq, statements } => json_response(
            200,
            format!(r#"{{"v":1,"seq":{seq},"statements":{statements},"committed":true}}"#),
        ),
        Outcome::ReadClosed { seq, statements } => json_response(
            200,
            format!(r#"{{"v":1,"seq":{seq},"statements":{statements},"committed":false}}"#),
        ),
    }
}

/// Answer one HTTP request.
pub(crate) async fn respond(cx: &Cx, server: &Server, request: Request) -> Response {
    if server.shutdown.is_triggered() {
        return refusal_response(ErrorCode::Draining, "the server is draining");
    }
    let path = request.uri.split('?').next().unwrap_or("");
    if path == "/v1/health" {
        return if request.method == Method::Get {
            json_response(200, r#"{"v":1,"status":"ok"}"#.to_owned())
        } else {
            json_response(
                405,
                r#"{"v":1,"error":{"code":"protocol","message":"use GET"}}"#.to_owned(),
            )
        };
    }
    let Some((name, verb)) = path
        .strip_prefix("/v1/databases/")
        .and_then(|rest| rest.split_once('/'))
    else {
        return json_response(
            404,
            r#"{"v":1,"error":{"code":"protocol","message":"no such route"}}"#.to_owned(),
        );
    };
    let mode = match verb {
        "query" => ExecuteMode::Read,
        "write" => ExecuteMode::Write,
        _ => {
            return json_response(
                404,
                r#"{"v":1,"error":{"code":"protocol","message":"no such route"}}"#.to_owned(),
            );
        }
    };
    if request.method != Method::Post {
        return json_response(
            405,
            r#"{"v":1,"error":{"code":"protocol","message":"use POST"}}"#.to_owned(),
        );
    }
    let token = match bearer(&request) {
        Ok(token) => token,
        Err(response) => return response,
    };
    // The same uniform surface as SELECT_DATABASE: no database, or a token
    // its issuer does not accept, look identical.
    let Some(db): Option<&Arc<Served>> = server.databases.get(name).filter(|db| db.admits(&token))
    else {
        return refusal_response(
            ErrorCode::NotFoundOrUnauthorized,
            "database not found or not authorized",
        );
    };
    let statement = match statement(&request, mode) {
        Ok(statement) => statement,
        Err(response) => return response,
    };
    let answer = match mode {
        ExecuteMode::Read => read(cx, db, &token, &statement).await,
        ExecuteMode::Write => write(cx, db, &token, &statement).await,
        // A change stream needs a long-lived, flow-controlled connection.
        ExecuteMode::Subscribe => {
            return refusal_response(ErrorCode::Protocol, "subscriptions are served over FGP");
        }
    };
    match answer {
        Ok(answer) => answer_response(&answer),
        Err(Refusal { code, message }) => refusal_response(code, &message),
    }
}
