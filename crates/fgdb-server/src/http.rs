//! The HTTP/1.1 JSON adapter: the same autocommit statements as FGP EXECUTE,
//! for clients that speak plain HTTP (curl, scripts, any language).
//!
//! An adapter may not weaken the native protocol (Appendix D), so this one
//! is a framing over the exact same execution path ([`crate::execute`]): the
//! same capability check, the same per-statement authorized session, the same
//! statement classes and the same closed error classes. Results are the same
//! ephemeral class as FGP's SNAPSHOT_RESULT stream, buffered whole in this
//! adapter. Headers and body still pass through a guarded transport that
//! rechecks the selected issuer's cooperative expiry/retirement fence before
//! each physical write and flush. Durable audit/time-authority evidence and
//! resumable result ownership remain separate, unimplemented contracts.
//!
//! ```text
//! GET  /v1/health                          -> {"v":1,"status":"ok"}
//! GET  /v1/databases/<name>/schema         (the names the token may see)
//! POST /v1/databases/<name>/query          (read)
//! POST /v1/databases/<name>/query-diff     (finite historical result changes)
//! POST /v1/databases/<name>/query-batch    (up to 32 reads, one pinned generation)
//! POST /v1/databases/<name>/write          (write)
//! POST /v1/databases/<name>/write-if-current (exact-frontier conditional write)
//! POST /v1/databases/<name>/write-batch    (one atomic ingestion program)
//! POST /v1/databases/<name>/compact        (Admin only, empty body)
//! POST /v1/databases/<name>/scrub          (Admin only, empty body)
//!      Authorization: Bearer <hex capability token>
//!      {"statement": "<gql>", "parameters": {"name": <json>, ...}}
//!      {"statement": "<write script>", "argument_sets": [{"name": <json>}, ...]}
//!      {"statements": [{"statement": "<read>", "parameters": {...}}, ...]}
//! ```
//!
//! A query answers `{"v":1,"columns":[...],"rows":[[cell,...],...],"seq":N}`
//! with the CLI robot contract's cells. A query-batch takes the same
//! statement/argument_sets body as a write-batch, or a `statements` array of
//! distinct read statements with their own parameter objects, and answers
//! {"v":1,"results":[{"columns":[...],"rows":[...],"seq":N},...]}.
//! Its 1–32 reads share one pinned generation and one cumulative signed
//! allowance; each result names its own selected historical cut. Any failure
//! returns one refusal with no successful prefix. Native host limits still
//! apply per statement. Unsigned parameters such as an AS OF SEQ cut use
//! `{"$uint64":"1"}`; ordinary JSON integers retain their signed type.
//! A write answers
//! `{"v":1,"seq":N,"statements":M,"committed":true|false}`, with `columns`
//! and `rows` first when it is a `CREATE/INSERT ... RETURN`. A refusal answers
//! `{"v":1,"error":{"code":"<class>","message":"..."}}` under a status that
//! follows the class. A missing database and a token that may not select it
//! share one 404, as they share one FGP refusal. The health route reveals
//! nothing about databases.
//!
//! `GET /studio/` serves the embedded, local-first browser workspace. The
//! static shell has no database authority; it uses the same authenticated
//! schema/read routes below. See `docs/STUDIO.md` for the loopback launch.

#[path = "studio/mod.rs"]
mod studio;

use crate::Server;
use crate::commits::CommitWatcher;
use crate::execute::{
    Answer, DifferenceAnswer, ReadBatchAnswer, Refusal, authorize_write, read, read_batch,
    read_difference, read_statement_batch, write, write_batch, write_if_current,
};
use crate::recovery::Generation;
use asupersync::Cx;
use asupersync::http::h1::types::{Method, Request, Response};
use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
use asupersync::sync::Mutex;
use core::pin::Pin;
use core::task::{Context, Poll};
use fgdb_protocol::body::{
    Body, DiffMode, ErrorCode, Execute, ExecuteBatch, ExecuteConditionalWrite, ExecuteDiff,
    ExecuteMode, Outcome,
};
use fgdb_protocol::json::{Json, argument, cell, parse_json, quote};
use fgdb_warden::{Authority, CapabilityToken, VerifiedCapability};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Authorization for the response currently owned by the HTTP/1 writer.
/// Http1Server fully flushes one response before invoking the next handler;
/// only that later handler may replace this decision. No parsed request can
/// clear the authority of an earlier response still under backpressure.
enum OutputDecision<'s> {
    Public,
    Protected {
        issuer: &'s Authority,
        // Boxed: the capability is most of the variant, and Public is empty.
        verified: Box<VerifiedCapability<'s>>,
        generation: Option<(Generation, CommitWatcher)>,
    },
}

pub(crate) struct OutputAuthority<'s> {
    decision: Mutex<OutputDecision<'s>>,
    stopped: AtomicBool,
}

#[cfg(test)]
mod recovery_tests;

impl<'s> OutputAuthority<'s> {
    pub(crate) fn new() -> Self {
        Self {
            decision: Mutex::new(OutputDecision::Public),
            stopped: AtomicBool::new(false),
        }
    }

    fn stop(&self) -> io::Error {
        self.stopped.store(true, Ordering::Release);
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "HTTP output is no longer authorized",
        )
    }

    fn public(&self, cx: &Cx) -> bool {
        if cx.checkpoint().is_err() || self.stopped.load(Ordering::Acquire) {
            self.stop();
            return false;
        }
        let Ok(mut decision) = self.decision.try_lock() else {
            self.stop();
            return false;
        };
        *decision = OutputDecision::Public;
        true
    }

    fn protected(&self, cx: &Cx, issuer: &'s Authority, verified: VerifiedCapability<'s>) -> bool {
        self.protected_at(cx, issuer, verified, crate::unix_millis())
    }

    fn protected_at(
        &self,
        cx: &Cx,
        issuer: &'s Authority,
        verified: VerifiedCapability<'s>,
        now: u64,
    ) -> bool {
        if cx.checkpoint().is_err()
            || self.stopped.load(Ordering::Acquire)
            || issuer.recheck_at(&verified, crate::TRUNK, now).is_err()
        {
            self.stop();
            return false;
        }
        let Ok(mut decision) = self.decision.try_lock() else {
            self.stop();
            return false;
        };
        *decision = OutputDecision::Protected {
            issuer,
            verified: Box::new(verified),
            generation: None,
        };
        true
    }

    fn pin_generation(&self, generation: Generation) -> bool {
        let Ok(mut decision) = self.decision.try_lock() else {
            self.stop();
            return false;
        };
        if let OutputDecision::Protected {
            generation: pinned, ..
        } = &mut *decision
        {
            let watcher = generation.watcher();
            *pinned = Some((generation, watcher));
            true
        } else {
            self.stop();
            false
        }
    }

    fn poll_authorized<T>(
        &self,
        now: u64,
        task: &mut Context<'_>,
        physical: impl FnOnce(&mut Context<'_>) -> T,
    ) -> io::Result<T> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(self.stop());
        }
        let mut decision = self.decision.try_lock().map_err(|_| self.stop())?;
        if let OutputDecision::Protected {
            issuer,
            verified,
            generation,
        } = &mut *decision
        {
            issuer
                .recheck_at(verified, crate::TRUNK, now)
                .map_err(|_| self.stop())?;
            if let Some((generation, watcher)) = generation {
                while watcher.poll_changed(task) {}
                return generation
                    .with_current(|| physical(task))
                    .map_err(|_| self.stop());
            }
        }
        Ok(physical(task))
    }

    #[cfg(test)]
    fn authorize(&self, now: u64) -> io::Result<()> {
        let mut task = Context::from_waker(std::task::Waker::noop());
        self.poll_authorized(now, &mut task, |_| ())
    }
}

/// Keep the foundation's HTTP parser, framing, keep-alive and drain driver.
/// The wrapper fences the actual transport polls, so a buffered Response and
/// its status/headers cannot retain queue-time permission through a slow peer.
pub(crate) struct GuardedIo<'s, T> {
    inner: T,
    cx: Cx,
    authority: Arc<OutputAuthority<'s>>,
}

impl<'s, T> GuardedIo<'s, T> {
    pub(crate) fn new(inner: T, cx: Cx, authority: Arc<OutputAuthority<'s>>) -> Self {
        Self {
            inner,
            cx,
            authority,
        }
    }

    fn live(&self) -> io::Result<()> {
        if self.cx.checkpoint().is_err() || self.authority.stopped.load(Ordering::Acquire) {
            Err(self.authority.stop())
        } else {
            Ok(())
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for GuardedIo<'_, T> {
    fn poll_read(
        self: Pin<&mut Self>,
        task: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = this.live() {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut this.inner).poll_read(task, buffer)
    }
}

impl<T: AsyncWrite + Unpin> GuardedIo<'_, T> {
    // The time argument is private for deterministic expiry tests. The public
    // transport trait always samples the existing host clock for each poll.
    fn poll_write_at(
        &mut self,
        task: &mut Context<'_>,
        bytes: &[u8],
        now: u64,
    ) -> Poll<io::Result<usize>> {
        if let Err(error) = self.live() {
            return Poll::Ready(Err(error));
        }
        let result = self
            .authority
            .poll_authorized(now, task, |task| {
                Pin::new(&mut self.inner).poll_write(task, bytes)
            })
            .unwrap_or_else(|error| Poll::Ready(Err(error)));
        match result {
            Poll::Ready(Err(error)) => {
                self.authority.stop();
                Poll::Ready(Err(error))
            }
            result => result,
        }
    }

    fn poll_flush_at(&mut self, task: &mut Context<'_>, now: u64) -> Poll<io::Result<()>> {
        if let Err(error) = self.live() {
            return Poll::Ready(Err(error));
        }
        let result = self
            .authority
            .poll_authorized(now, task, |task| Pin::new(&mut self.inner).poll_flush(task))
            .unwrap_or_else(|error| Poll::Ready(Err(error)));
        match result {
            Poll::Ready(Err(error)) => {
                self.authority.stop();
                Poll::Ready(Err(error))
            }
            result => result,
        }
    }

    fn poll_shutdown_at(&mut self, task: &mut Context<'_>, now: u64) -> Poll<io::Result<()>> {
        if let Err(error) = self.live() {
            return Poll::Ready(Err(error));
        }
        self.authority
            .poll_authorized(now, task, |task| {
                Pin::new(&mut self.inner).poll_shutdown(task)
            })
            .unwrap_or_else(|error| Poll::Ready(Err(error)))
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for GuardedIo<'_, T> {
    fn poll_write(
        self: Pin<&mut Self>,
        task: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut()
            .poll_write_at(task, bytes, crate::unix_millis())
    }

    fn poll_flush(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_flush_at(task, crate::unix_millis())
    }

    fn poll_shutdown(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_shutdown_at(task, crate::unix_millis())
    }
}

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
        ErrorCode::Draining | ErrorCode::DatabaseRecovering | ErrorCode::DatabaseUnavailable => 503,
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
    crate::capability_from_hex(hex).ok_or_else(unauthenticated)
}

fn body_fields(request: &Request) -> Result<std::collections::BTreeMap<String, Json>, Response> {
    let malformed = |detail: &str| refusal_response(ErrorCode::Protocol, detail);
    let text = core::str::from_utf8(&request.body).map_err(|_| malformed("body is not UTF-8"))?;
    let json = parse_json(text, MAX_JSON_VALUES, MAX_JSON_TOKEN_BYTES)
        .map_err(|error| malformed(&format!("invalid JSON body: {error}")))?;
    let Json::Object(fields) = json else {
        return Err(malformed("body must be a JSON object"));
    };
    Ok(fields)
}

/// The statement and arguments of one request body.
fn statement(request: &Request, mode: ExecuteMode) -> Result<Execute, Response> {
    statement_fields(body_fields(request)?, mode)
}

fn statement_fields(
    mut fields: std::collections::BTreeMap<String, Json>,
    mode: ExecuteMode,
) -> Result<Execute, Response> {
    let malformed = |detail: &str| refusal_response(ErrorCode::Protocol, detail);
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

/// The expected frontier is exact unsigned decimal text, never an f64 or a
/// lower bound. The body cannot select a different write mode or batch API.
fn conditional_statement(request: &Request) -> Result<ExecuteConditionalWrite, Response> {
    let malformed = |detail: &str| refusal_response(ErrorCode::Protocol, detail);
    let mut fields = body_fields(request)?;
    let expected = match fields.remove("expected_seq") {
        Some(Json::String(text) | Json::Number(text)) => text,
        _ => {
            return Err(malformed(
                "\"expected_seq\" must be an unsigned decimal sequence",
            ));
        }
    };
    if expected.is_empty() || !expected.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(malformed(
            "\"expected_seq\" must be an unsigned decimal sequence",
        ));
    }
    let expected_seq = expected
        .parse::<u64>()
        .map_err(|_| malformed("\"expected_seq\" exceeds the u64 sequence range"))?;
    let Some(Json::String(statement)) = fields.remove("statement") else {
        return Err(malformed("body needs a \"statement\" string"));
    };
    let parameters = match fields.remove("parameters") {
        None | Some(Json::Null) => Vec::new(),
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
    let conditional = ExecuteConditionalWrite {
        expected_seq,
        statement,
        parameters,
    };
    conditional
        .encode()
        .map_err(|error| malformed(&format!("invalid conditional write: {error}")))?;
    Ok(conditional)
}

/// Cuts retain their exact decimal text through JSON parsing. Strings let
/// callers represent every u64 without their JSON library rounding a number.
fn difference_statement(request: &Request) -> Result<ExecuteDiff, Response> {
    let malformed = |detail: &str| refusal_response(ErrorCode::Protocol, detail);
    let mut fields = body_fields(request)?;
    let endpoint = |value: Option<Json>, name: &str| -> Result<u64, Response> {
        let text = match value {
            Some(Json::String(text) | Json::Number(text)) => text,
            _ => {
                return Err(malformed(&format!(
                    "{name:?} must be an unsigned decimal cut"
                )));
            }
        };
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(malformed(&format!(
                "{name:?} must be an unsigned decimal cut"
            )));
        }
        text.parse()
            .map_err(|_| malformed(&format!("{name:?} exceeds the u64 cut range")))
    };
    let before = endpoint(fields.remove("before"), "before")?;
    let after = endpoint(fields.remove("after"), "after")?;
    let Some(Json::String(statement)) = fields.remove("statement") else {
        return Err(malformed("body needs a \"statement\" string"));
    };
    let parameters = match fields.remove("parameters") {
        None | Some(Json::Null) => Vec::new(),
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
    let difference = ExecuteDiff {
        mode: DiffMode::Buffered,
        before,
        after,
        statement,
        parameters,
    };
    // The alternative framing admits exactly the same statement and operand
    // limits. HTTP remains a whole-response adapter with no stream fallback.
    difference
        .encode()
        .map_err(|error| malformed(&format!("invalid difference: {error}")))?;
    Ok(difference)
}

/// The batch uses exactly the native FGP value/schema limits, plus HTTP's
/// existing whole-body JSON limits. No field can select raw graph identities.
fn batch_statement(request: &Request) -> Result<ExecuteBatch, Response> {
    batch_fields(body_fields(request)?)
}

fn batch_fields(
    mut fields: std::collections::BTreeMap<String, Json>,
) -> Result<ExecuteBatch, Response> {
    let malformed = |detail: &str| refusal_response(ErrorCode::Protocol, detail);
    let Some(Json::String(statement)) = fields.remove("statement") else {
        return Err(malformed("body needs a \"statement\" string"));
    };
    let Some(Json::Array(records)) = fields.remove("argument_sets") else {
        return Err(malformed(
            "\"argument_sets\" must be a nonempty array of objects",
        ));
    };
    if records.is_empty() || records.len() > fgdb_protocol::body::MAX_BATCH_ARGUMENT_SETS {
        return Err(malformed(
            "\"argument_sets\" exceeds the batch count bounds",
        ));
    }
    if let Some(unknown) = fields.keys().next() {
        return Err(malformed(&format!("unknown body field {unknown:?}")));
    }
    let argument_sets = records
        .into_iter()
        .enumerate()
        .map(|(record, value)| {
            let Json::Object(parameters) = value else {
                return Err(malformed("every argument set must be an object"));
            };
            parameters
                .iter()
                .map(|(name, value)| {
                    argument(value)
                        .map(|value| (name.clone(), value))
                        .map_err(|error| {
                            malformed(&format!(
                                "argument set {record}, parameter ${name}: {error}"
                            ))
                        })
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    let batch = ExecuteBatch {
        statement,
        argument_sets,
    };
    // Enforce every FGP name, value, recursion and parameter-map bound before
    // the shared executor sees this alternative transport's request.
    batch
        .encode()
        .map_err(|error| malformed(&format!("invalid batch: {error}")))?;
    Ok(batch)
}

enum ReadBatchRequest {
    Parameterized(ExecuteBatch),
    Statements(Vec<Execute>),
}

fn read_batch_request(request: &Request) -> Result<ReadBatchRequest, Response> {
    let malformed = |detail: &str| refusal_response(ErrorCode::Protocol, detail);
    // Parse once so JSON nesting, token and value limits cover both shapes
    // cumulatively. Neither shape can silently discard fields from the other.
    let mut fields = body_fields(request)?;
    let Some(statements) = fields.remove("statements") else {
        return batch_fields(fields).map(ReadBatchRequest::Parameterized);
    };
    let Json::Array(statements) = statements else {
        return Err(malformed(
            "\"statements\" must be a nonempty array of objects",
        ));
    };
    let maximum = fgdb::AuthorizedReadSession::<crate::Symbols, fn() -> u64>::MAX_BATCH_STATEMENTS;
    if statements.is_empty() || statements.len() > maximum {
        return Err(malformed(&format!(
            "a read batch requires between 1 and {maximum} statements"
        )));
    }
    if let Some(unknown) = fields.keys().next() {
        return Err(malformed(&format!("unknown body field {unknown:?}")));
    }
    statements
        .into_iter()
        .map(|statement| {
            let Json::Object(fields) = statement else {
                return Err(malformed("each statement must be an object"));
            };
            let statement = statement_fields(fields, ExecuteMode::Read)?;
            // Apply the same native name/value/depth bounds. A statement
            // cannot select a write, stream or raw graph identity via framing.
            statement
                .encode()
                .map_err(|error| malformed(&format!("invalid read statement: {error}")))?;
            Ok(statement)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(ReadBatchRequest::Statements)
}

fn answer_response(answer: &Answer) -> Response {
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
    // A write that RETURNs carries its rows beside the commit outcome.
    let returned = if answer.columns.is_empty() {
        String::new()
    } else {
        format!(r#""columns":[{columns}],"rows":[{rows}],"#)
    };
    match answer.outcome {
        Outcome::Rows { seq } => json_response(
            200,
            format!(r#"{{"v":1,"columns":[{columns}],"rows":[{rows}],"seq":{seq}}}"#),
        ),
        Outcome::WriteCommitted { seq, statements } => json_response(
            200,
            format!(
                r#"{{"v":1,{returned}"seq":{seq},"statements":{statements},"committed":true}}"#
            ),
        ),
        Outcome::ReadClosed { seq, statements } => json_response(
            200,
            format!(
                r#"{{"v":1,{returned}"seq":{seq},"statements":{statements},"committed":false}}"#
            ),
        ),
    }
}

fn read_batch_response(answer: &ReadBatchAnswer) -> Response {
    let results = answer
        .results
        .iter()
        .map(|result| {
            let columns = result
                .columns
                .iter()
                .map(|column| quote(column))
                .collect::<Vec<_>>()
                .join(",");
            let rows = result
                .rows
                .iter()
                .map(|row| format!("[{}]", row.iter().map(cell).collect::<Vec<_>>().join(",")))
                .collect::<Vec<_>>()
                .join(",");
            format!(
                r#"{{"columns":[{columns}],"rows":[{rows}],"seq":{}}}"#,
                result.seq,
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    json_response(200, format!(r#"{{"v":1,"results":[{results}]}}"#))
}

fn difference_response(answer: &DifferenceAnswer) -> Response {
    let columns = answer
        .columns
        .iter()
        .map(|column| quote(column))
        .collect::<Vec<_>>()
        .join(",");
    let changes = answer
        .entries
        .iter()
        .map(|(weight, row)| {
            format!(
                r#"{{"weight":"{weight}","row":[{}]}}"#,
                row.iter().map(cell).collect::<Vec<_>>().join(",")
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    json_response(
        200,
        format!(
            r#"{{"v":1,"columns":[{columns}],"changes":[{changes}],"before":"{}","after":"{}","changed_rows":"{}"}}"#,
            answer.before,
            answer.after,
            answer.entries.len(),
        ),
    )
}

/// `GET /v1/databases/<name>/schema`: the label, relation and property
/// names this token may see.
fn schema_response<'s>(
    cx: &Cx,
    server: &'s Server,
    name: &str,
    request: &Request,
    output: &OutputAuthority<'s>,
) -> Response {
    if request.method != Method::Get {
        return json_response(
            405,
            r#"{"v":1,"error":{"code":"protocol","message":"use GET"}}"#.to_owned(),
        );
    }
    let token = match bearer(request) {
        Ok(token) => token,
        Err(response) => return response,
    };
    let Some((db, verified)) = server.databases.get(name).and_then(|db| {
        db.authority
            .verify_at(&token, crate::TRUNK, crate::unix_millis())
            .ok()
            .map(|verified| (db, verified))
    }) else {
        return refusal_response(
            ErrorCode::NotFoundOrUnauthorized,
            "database not found or not authorized",
        );
    };
    if !output.protected(cx, &db.authority, verified) {
        return refusal_response(ErrorCode::Execution, "connection unavailable");
    }
    let generation = match db.db.generation() {
        Ok(generation) => generation,
        Err(error) => return refusal_response(error.code(), error.message()),
    };
    match crate::execute::schema(db, &token) {
        Ok(schema) => {
            if let Err(error) = generation.check() {
                return refusal_response(error.code(), error.message());
            }
            if !output.pin_generation(generation) {
                return refusal_response(ErrorCode::Execution, "connection unavailable");
            }
            let list = |names: &[String]| {
                names
                    .iter()
                    .map(|name| quote(name))
                    .collect::<Vec<_>>()
                    .join(",")
            };
            json_response(
                200,
                format!(
                    r#"{{"v":1,"labels":[{}],"relations":[{}],"properties":[{}]}}"#,
                    list(&schema.labels),
                    list(&schema.relations),
                    list(&schema.properties)
                ),
            )
        }
        Err(Refusal { code, message }) => refusal_response(code, &message),
    }
}

/// Physical maintenance is an explicit Admin operation, never a GQL write.
/// Verify before lifecycle/storage admission and again after waiting for the
/// existing writer. The usual HTTP output fence guards the returned summary.
async fn maintenance_response<'s>(
    cx: &Cx,
    server: &'s Server,
    name: &str,
    verb: &str,
    request: &Request,
    output: &OutputAuthority<'s>,
) -> Response {
    if request.method != Method::Post {
        return json_response(
            405,
            r#"{"v":1,"error":{"code":"protocol","message":"use POST"}}"#.to_owned(),
        );
    }
    let token = match bearer(request) {
        Ok(token) => token,
        Err(response) => return response,
    };
    let Some((db, verified)) = server.databases.get(name).and_then(|db| {
        db.authority
            .verify_at(&token, crate::TRUNK, crate::unix_millis())
            .ok()
            .map(|verified| (db, verified))
    }) else {
        return refusal_response(
            ErrorCode::NotFoundOrUnauthorized,
            "database not found or not authorized",
        );
    };
    if verified
        .check_admin_at(crate::TRUNK, crate::unix_millis())
        .is_err()
    {
        return refusal_response(
            ErrorCode::PermissionDenied,
            "maintenance requires unrestricted Admin authority",
        );
    }
    if !output.protected(cx, &db.authority, verified) {
        return refusal_response(ErrorCode::Execution, "connection unavailable");
    }
    if !request.body.is_empty() {
        return refusal_response(ErrorCode::Protocol, "maintenance takes an empty body");
    }
    let operation = async {
        let (served, _operation) = server.maintenance_admission(cx, name)?;
        let generation = served.db.generation().map_err(|_| crate::MaintenanceError::unavailable(cx, served))?;
        let mut database = served.db.write(cx).await.map_err(|_| crate::MaintenanceError::unavailable(cx, served))?;
        // A request may have waited behind another write until its credential
        // expired or its issuer retired. No requested maintenance precedes this.
        if served.authority.verify_at(&token, crate::TRUNK, crate::unix_millis())
            .and_then(|verified| verified.check_admin_at(crate::TRUNK, crate::unix_millis()))
            .is_err()
        {
            return Ok(refusal_response(ErrorCode::PermissionDenied, "maintenance authority is no longer valid"));
        }
        let contexts = fgdb_types::PurposeContexts::narrow_runtime_root(cx);
        let body = if verb == "compact" {
            database.compact(&contexts.commit()).await.map_err(crate::MaintenanceError::Engine)?;
            r#"{"v":1,"operation":"compact","completed":true}"#.to_owned()
        } else {
            let summary = database.scrub(&contexts.commit()).await
                .map_err(|error| crate::MaintenanceError::Engine(error.into()))?;
            format!(r#"{{"v":1,"operation":"scrub","objects":{},"clean":{},"repaired":{},"block_objects":{},"metadata_objects":{}}}"#,
                summary.objects, summary.clean.len(), summary.repaired.len(), summary.block_objects, summary.metadata_objects)
        };
        // Dropping the writer preserves the existing recovery/fencing path
        // for scrub losses or ambiguous publication. Never report success on
        // a retired generation, including while retained readers still exist.
        drop(database);
        if let Err(error) = generation.check() {
            return Ok(refusal_response(error.code(), error.message()));
        }
        if !output.pin_generation(generation) {
            return Ok(refusal_response(ErrorCode::Execution, "connection unavailable"));
        }
        Ok(json_response(200, body))
    }.await;
    match operation {
        Ok(response) => response,
        Err(error) => {
            let code = match error {
                crate::MaintenanceError::UnknownDatabase(_) => ErrorCode::NotFoundOrUnauthorized,
                crate::MaintenanceError::Interrupted => ErrorCode::Cancelled,
                crate::MaintenanceError::Draining => ErrorCode::Draining,
                crate::MaintenanceError::DatabaseUnavailable(
                    crate::DatabaseStatus::Recovering { .. },
                ) => ErrorCode::DatabaseRecovering,
                crate::MaintenanceError::DatabaseUnavailable(_) => ErrorCode::DatabaseUnavailable,
                crate::MaintenanceError::Engine(_) => ErrorCode::Execution,
            };
            refusal_response(code, "database maintenance did not complete")
        }
    }
}

/// Answer one HTTP request.
pub(crate) async fn respond<'s>(
    cx: &Cx,
    server: &'s Server,
    request: Request,
    output: &OutputAuthority<'s>,
) -> Response {
    // The framework has completed the previous response's physical flush.
    // Public errors cannot reveal whether a database/token lookup succeeded.
    if !output.public(cx) {
        return refusal_response(ErrorCode::Execution, "connection unavailable");
    }
    if server.shutdown.is_triggered() {
        return refusal_response(ErrorCode::Draining, "the server is draining");
    }
    let path = request.uri.split('?').next().unwrap_or("");
    if let Some(response) = studio::respond(path, &request.method) {
        return response;
    }
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
    if verb == "schema" {
        return schema_response(cx, server, name, &request, output);
    }
    if matches!(verb, "compact" | "scrub") {
        return maintenance_response(cx, server, name, verb, &request, output).await;
    }
    let mode = match verb {
        "query" => Some(ExecuteMode::Read),
        "write" => Some(ExecuteMode::Write),
        "write-batch" | "query-batch" | "query-diff" | "write-if-current" => None,
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
    let Some((db, verified)) = server.databases.get(name).and_then(|db| {
        db.authority
            .verify_at(&token, crate::TRUNK, crate::unix_millis())
            .ok()
            .map(|verified| (db, verified))
    }) else {
        return refusal_response(
            ErrorCode::NotFoundOrUnauthorized,
            "database not found or not authorized",
        );
    };
    if !output.protected(cx, &db.authority, verified) {
        return refusal_response(ErrorCode::Execution, "connection unavailable");
    }
    if verb == "query-batch" {
        // As with historical differences, read rights precede body parsing.
        // The same generation fences adaptation, execution and every send.
        let generation = match crate::execute::ordered::ordered_generation(db, &token) {
            Ok(generation) => generation,
            Err(Refusal { code, message }) => return refusal_response(code, &message),
        };
        // Errors are protected output too. A late failure can retain
        // data-dependent diagnostics; fence it even while the response waits.
        if !output.pin_generation(generation.clone()) {
            return refusal_response(ErrorCode::Execution, "connection unavailable");
        }
        let statement = match read_batch_request(&request) {
            Ok(statement) => statement,
            Err(response) => return response,
        };
        let answer = match statement {
            ReadBatchRequest::Parameterized(statement) => {
                read_batch(cx, db, &token, &statement).await
            }
            ReadBatchRequest::Statements(statements) => {
                read_statement_batch(cx, db, &token, &statements).await
            }
        };
        if let Err(error) = generation.check() {
            return refusal_response(error.code(), error.message());
        }
        return match answer {
            Ok(answer) => {
                if let Err(error) = answer.generation.check() {
                    return refusal_response(error.code(), error.message());
                }
                if !output.pin_generation(answer.generation.clone()) {
                    return refusal_response(ErrorCode::Execution, "connection unavailable");
                }
                read_batch_response(&answer)
            }
            Err(Refusal { code, message }) => refusal_response(code, &message),
        };
    }
    if verb == "query-diff" {
        // Read rights precede even body inspection. Keep the admitted history
        // generation through parsing, evaluation and protected output.
        let generation = match crate::execute::ordered::ordered_generation(db, &token) {
            Ok(generation) => generation,
            Err(Refusal { code, message }) => return refusal_response(code, &message),
        };
        // A refused historical read can still expose data-dependent details.
        // Its buffered error must expire with the same generation as a result.
        if !output.pin_generation(generation.clone()) {
            return refusal_response(ErrorCode::Execution, "connection unavailable");
        }
        let statement = match difference_statement(&request) {
            Ok(statement) => statement,
            Err(response) => return response,
        };
        let answer = read_difference(
            cx,
            db,
            &token,
            &statement.statement,
            &statement.parameters,
            statement.before,
            statement.after,
        )
        .await;
        if let Err(error) = generation.check() {
            return refusal_response(error.code(), error.message());
        }
        return match answer {
            Ok(answer) => {
                if let Err(error) = answer.generation.check() {
                    return refusal_response(error.code(), error.message());
                }
                if !output.pin_generation(answer.generation.clone()) {
                    return refusal_response(ErrorCode::Execution, "connection unavailable");
                }
                difference_response(&answer)
            }
            Err(Refusal { code, message }) => refusal_response(code, &message),
        };
    }
    let answer = if verb == "write-if-current" {
        // Reject missing write rights before parsing any body fields. The
        // native owner authenticates again after waiting for its writer guard.
        if let Err(Refusal { code, message }) = authorize_write(db, &token) {
            return refusal_response(code, &message);
        }
        let statement = match conditional_statement(&request) {
            Ok(statement) => statement,
            Err(response) => return response,
        };
        write_if_current(cx, db, &token, &statement).await
    } else if let Some(mode) = mode {
        let statement = match statement(&request, mode) {
            Ok(statement) => statement,
            Err(response) => return response,
        };
        match mode {
            ExecuteMode::Read => read(cx, db, &token, &statement).await,
            ExecuteMode::Write => write(cx, db, &token, &statement).await,
            // Source and change streams need a flow-controlled connection.
            ExecuteMode::Subscribe | ExecuteMode::ReadStream | ExecuteMode::ReadOrdered => {
                return refusal_response(
                    ErrorCode::Protocol,
                    "source and change streams are served over FGP",
                );
            }
        }
    } else {
        let statement = match batch_statement(&request) {
            Ok(statement) => statement,
            Err(response) => return response,
        };
        write_batch(cx, db, &token, &statement).await
    };
    match answer {
        Ok(answer) => {
            if let Err(error) = answer.generation.check() {
                return if matches!(answer.outcome, Outcome::WriteCommitted { .. }) {
                    refusal_response(
                        ErrorCode::OutcomeUnknown,
                        "write completed before database recovery, but its result was not fully delivered; do not replay the write",
                    )
                } else {
                    refusal_response(error.code(), error.message())
                };
            }
            if !output.pin_generation(answer.generation.clone()) {
                return refusal_response(ErrorCode::Execution, "connection unavailable");
            }
            answer_response(&answer)
        }
        Err(Refusal { code, message }) => refusal_response(code, &message),
    }
}

#[cfg(test)]
mod output_tests {
    use super::*;
    use asupersync::http::h1::server::{HostPolicy, Http1Config, Http1Server};
    use asupersync::security::key::AuthKey;
    use asupersync::{Budget, runtime::RuntimeBuilder};
    use fgdb_delta_types::SchemaEpoch;
    use fgdb_types::DatabaseSecurityNamespaceId;
    use fgdb_warden::{Grant, QueryLimits};
    use std::collections::VecDeque;
    use std::future::Future;
    use std::sync::atomic::AtomicUsize;
    use std::task::Waker;

    const START: u64 = 100;
    const EXPIRES: u64 = 1000;
    const LIVE_EXPIRY: u64 = u64::MAX / 2;

    fn issuer(seed: u64) -> Authority {
        Authority::new(
            AuthKey::from_seed(seed),
            DatabaseSecurityNamespaceId([7; 32]),
            crate::GRAPH_NAME,
            SchemaEpoch(0),
            1,
        )
        .unwrap()
    }

    fn token(issuer: &Authority, expiry: u64) -> CapabilityToken {
        issuer
            .issue_at(
                &Grant::read_only(
                    crate::TRUNK,
                    expiry,
                    QueryLimits {
                        max_nodes: 100,
                        max_work: 1000,
                        max_rows: 100,
                    },
                ),
                START,
            )
            .unwrap()
    }

    fn select_at<'s>(
        output: &OutputAuthority<'s>,
        cx: &Cx,
        issuer: &'s Authority,
        token: &CapabilityToken,
        now: u64,
    ) {
        let verified = issuer.verify_at(token, crate::TRUNK, now).unwrap();
        assert!(output.protected_at(cx, issuer, verified, now));
    }

    struct State {
        accepted: Mutex<Vec<u8>>,
        flush_ready: AtomicBool,
        flushes: AtomicUsize,
        shutdowns: AtomicUsize,
    }

    impl State {
        fn bytes(&self) -> Vec<u8> {
            self.accepted.try_lock().unwrap().clone()
        }
    }

    struct MemoryIo {
        input: Vec<u8>,
        read: usize,
        chunks: VecDeque<usize>,
        state: Arc<State>,
    }

    impl MemoryIo {
        fn new(
            input: Vec<u8>,
            chunks: impl IntoIterator<Item = usize>,
            flush_ready: bool,
        ) -> (Self, Arc<State>) {
            let state = Arc::new(State {
                accepted: Mutex::new(Vec::new()),
                flush_ready: AtomicBool::new(flush_ready),
                flushes: AtomicUsize::new(0),
                shutdowns: AtomicUsize::new(0),
            });
            (
                Self {
                    input,
                    read: 0,
                    chunks: chunks.into_iter().collect(),
                    state: Arc::clone(&state),
                },
                state,
            )
        }
    }

    impl AsyncRead for MemoryIo {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let count = buffer.remaining().min(self.input.len() - self.read);
            buffer.put_slice(&self.input[self.read..self.read + count]);
            self.read += count;
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for MemoryIo {
        fn poll_write(
            mut self: Pin<&mut Self>,
            task: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            let count = self.chunks.pop_front().unwrap_or(usize::MAX);
            if count == 0 {
                task.waker().wake_by_ref();
                return Poll::Pending;
            }
            let count = count.min(bytes.len());
            self.state
                .accepted
                .try_lock()
                .unwrap()
                .extend_from_slice(&bytes[..count]);
            Poll::Ready(Ok(count))
        }

        fn poll_flush(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.state.flushes.fetch_add(1, Ordering::Relaxed);
            if self.state.flush_ready.load(Ordering::Acquire) {
                Poll::Ready(Ok(()))
            } else {
                task.waker().wake_by_ref();
                Poll::Pending
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.state.shutdowns.fetch_add(1, Ordering::Relaxed);
            Poll::Ready(Ok(()))
        }
    }

    fn task() -> Context<'static> {
        Context::from_waker(Waker::noop())
    }

    fn poll<T>(future: Pin<&mut impl Future<Output = T>>) -> Poll<T> {
        future.poll(&mut task())
    }

    fn finish<T>(mut future: Pin<&mut impl Future<Output = T>>) -> T {
        for _ in 0..128 {
            if let Poll::Ready(value) = poll(future.as_mut()) {
                return value;
            }
        }
        panic!("bounded HTTP driver did not complete");
    }

    fn with_cx(test: impl FnOnce(&Cx)) {
        let runtime = RuntimeBuilder::new().build().unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        test(&cx);
    }

    fn denied<T>(result: Poll<io::Result<T>>) {
        match result {
            Poll::Ready(Err(error)) => {
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            }
            _ => panic!("protected output unexpectedly remained live"),
        }
    }

    #[test]
    fn expiry_before_http_headers_is_terminal_even_after_public_reset() {
        with_cx(|cx| {
            let issuer = issuer(41);
            let credential = token(&issuer, EXPIRES);
            let output = Arc::new(OutputAuthority::new());
            select_at(&output, cx, &issuer, &credential, START);
            let (inner, state) = MemoryIo::new(b"next request".to_vec(), [], true);
            let mut io = GuardedIo::new(inner, cx.clone(), Arc::clone(&output));
            denied(io.poll_write_at(&mut task(), b"HTTP/1.1 200 OK\r\n", EXPIRES));
            assert!(state.bytes().is_empty());
            assert!(!output.public(cx));
            let verified = issuer.verify_at(&credential, crate::TRUNK, START).unwrap();
            assert!(!output.protected_at(cx, &issuer, verified, START));
            denied(io.poll_write_at(&mut task(), b"public", START));
            denied(io.poll_flush_at(&mut task(), START));
            denied(io.poll_shutdown_at(&mut task(), START));
            let mut bytes = [0; 32];
            denied(Pin::new(&mut io).poll_read(&mut task(), &mut ReadBuf::new(&mut bytes)));
            assert!(state.bytes().is_empty());
            assert_eq!(state.flushes.load(Ordering::Relaxed), 0);
            assert_eq!(state.shutdowns.load(Ordering::Relaxed), 0);
        });
    }

    #[test]
    fn http_header_prefix_does_not_authorize_its_delayed_suffix() {
        with_cx(|cx| {
            let issuer = issuer(41);
            let credential = token(&issuer, EXPIRES);
            let output = Arc::new(OutputAuthority::new());
            select_at(&output, cx, &issuer, &credential, START);
            let (inner, state) = MemoryIo::new(Vec::new(), [5, 0], true);
            let mut io = GuardedIo::new(inner, cx.clone(), output);
            let header = b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nprotected";
            assert!(matches!(
                io.poll_write_at(&mut task(), header, START),
                Poll::Ready(Ok(5))
            ));
            assert!(
                io.poll_write_at(&mut task(), &header[5..], START)
                    .is_pending()
            );
            denied(io.poll_write_at(&mut task(), &header[5..], EXPIRES));
            assert_eq!(state.bytes(), header[..5]);
            denied(io.poll_flush_at(&mut task(), START));
            assert_eq!(state.flushes.load(Ordering::Relaxed), 0);
        });
    }

    #[test]
    fn http_pending_flush_rechecks_the_original_issuer() {
        with_cx(|cx| {
            let issuer = issuer(41);
            let credential = token(&issuer, EXPIRES);
            let output = Arc::new(OutputAuthority::new());
            select_at(&output, cx, &issuer, &credential, START);
            let (inner, state) = MemoryIo::new(Vec::new(), [], false);
            let mut io = GuardedIo::new(inner, cx.clone(), output);
            assert!(matches!(
                io.poll_write_at(&mut task(), b"protected response", START),
                Poll::Ready(Ok(18))
            ));
            assert!(io.poll_flush_at(&mut task(), START).is_pending());
            let flushes = state.flushes.load(Ordering::Relaxed);
            issuer.retire();
            state.flush_ready.store(true, Ordering::Release);
            denied(io.poll_flush_at(&mut task(), START));
            assert_eq!(state.bytes(), b"protected response");
            assert_eq!(state.flushes.load(Ordering::Relaxed), flushes);
        });
    }

    #[test]
    fn a_verified_capability_cannot_be_rebound_to_another_issuer_instance() {
        with_cx(|cx| {
            // Identical public identity AND key still do not make these the
            // same live retirement fence. Warden's pointer law must survive.
            let original = issuer(41);
            let other = issuer(41);
            let credential = token(&original, EXPIRES);
            let verified = original
                .verify_at(&credential, crate::TRUNK, START)
                .unwrap();
            let output = OutputAuthority::new();
            assert!(!output.protected_at(cx, &other, verified, START));
            assert!(output.authorize(START).is_err());
            assert!(!output.public(cx));
        });
    }

    fn requests(count: usize) -> Vec<u8> {
        "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n"
            .repeat(count)
            .into_bytes()
    }

    fn config(max_requests: u64) -> Http1Config {
        Http1Config::default()
            .host_policy(HostPolicy::allow_list(vec!["localhost".to_owned()]))
            .max_requests(Some(max_requests))
            .idle_timeout(None)
    }

    #[test]
    fn real_http_driver_changes_guards_only_between_flushed_responses() {
        with_cx(|cx| {
            let first = issuer(41);
            let second = issuer(42);
            let first_token = token(&first, LIVE_EXPIRY);
            let second_token = token(&second, LIVE_EXPIRY);
            let output = Arc::new(OutputAuthority::new());
            let calls = AtomicUsize::new(0);
            let (inner, state) = MemoryIo::new(requests(3), [usize::MAX, 0, usize::MAX, 0], true);
            let io = GuardedIo::new(inner, cx.clone(), Arc::clone(&output));
            let handler = |_: Request| {
                let request = calls.fetch_add(1, Ordering::Relaxed);
                let output = Arc::clone(&output);
                let first = &first;
                let second = &second;
                let first_token = &first_token;
                let second_token = &second_token;
                async move {
                    assert!(output.public(cx));
                    let body = match request {
                        0 => {
                            let verified = first
                                .verify_at(first_token, crate::TRUNK, crate::unix_millis())
                                .unwrap();
                            assert!(output.protected(cx, first, verified));
                            "first-protected"
                        }
                        1 => "public-health",
                        2 => {
                            let verified = second
                                .verify_at(second_token, crate::TRUNK, crate::unix_millis())
                                .unwrap();
                            assert!(output.protected(cx, second, verified));
                            "second-protected"
                        }
                        _ => panic!("too many requests"),
                    };
                    Response::new(200, "OK", body.as_bytes().to_vec())
                }
            };
            let mut server = Box::pin(Http1Server::with_config(handler, config(3)).serve(io));
            for _ in 0..128 {
                assert!(poll(server.as_mut()).is_pending());
                if calls.load(Ordering::Relaxed) == 2 {
                    break;
                }
            }
            assert_eq!(calls.load(Ordering::Relaxed), 2);
            let first_response = String::from_utf8(state.bytes()).unwrap();
            assert!(first_response.contains("first-protected"));
            assert!(!first_response.contains("public-health"));
            first.retire();
            // The fully flushed first response no longer controls the public
            // second response or the independently selected third issuer.
            let result = finish(server.as_mut()).unwrap();
            assert_eq!(result.requests_served, 3);
            let bytes = String::from_utf8(state.bytes()).unwrap();
            assert!(bytes.contains("first-protected"));
            assert!(bytes.contains("public-health"));
            assert!(bytes.contains("second-protected"));
            assert_eq!(bytes.matches("HTTP/1.1 200 OK").count(), 3);
            assert_eq!(state.shutdowns.load(Ordering::Relaxed), 1);
        });
    }

    #[test]
    fn pipelined_next_request_cannot_replace_a_response_waiting_to_flush() {
        with_cx(|cx| {
            let first = issuer(41);
            let second = issuer(42);
            let first_token = token(&first, LIVE_EXPIRY);
            let second_token = token(&second, LIVE_EXPIRY);
            let output = Arc::new(OutputAuthority::new());
            let calls = AtomicUsize::new(0);
            let (inner, state) = MemoryIo::new(requests(2), [], false);
            let io = GuardedIo::new(inner, cx.clone(), Arc::clone(&output));
            let handler = |_: Request| {
                let request = calls.fetch_add(1, Ordering::Relaxed);
                let output = Arc::clone(&output);
                let (issuer, token) = if request == 0 {
                    (&first, &first_token)
                } else {
                    (&second, &second_token)
                };
                async move {
                    assert!(output.public(cx));
                    let verified = issuer
                        .verify_at(token, crate::TRUNK, crate::unix_millis())
                        .unwrap();
                    assert!(output.protected(cx, issuer, verified));
                    Response::new(200, "OK", format!("protected-{request}").into_bytes())
                }
            };
            let mut server = Box::pin(Http1Server::with_config(handler, config(2)).serve(io));
            for _ in 0..128 {
                assert!(poll(server.as_mut()).is_pending());
                if state.flushes.load(Ordering::Relaxed) != 0 {
                    break;
                }
            }
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            let accepted = state.bytes();
            assert!(
                String::from_utf8(accepted.clone())
                    .unwrap()
                    .contains("protected-0")
            );
            first.retire();
            state.flush_ready.store(true, Ordering::Release);
            assert!(finish(server.as_mut()).is_err());
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            assert_eq!(state.bytes(), accepted);
            assert!(!output.public(cx));
        });
    }
}
