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
//! POST /v1/databases/<name>/write          (write)
//!      Authorization: Bearer <hex capability token>
//!      {"statement": "<gql>", "parameters": {"name": <json>, ...}}
//! ```
//!
//! A query answers `{"v":1,"columns":[...],"rows":[[cell,...],...],"seq":N}`
//! with the CLI robot contract's cells; a write answers
//! `{"v":1,"seq":N,"statements":M,"committed":true|false}`, with `columns`
//! and `rows` first when it is a `CREATE/INSERT ... RETURN`. A refusal answers
//! `{"v":1,"error":{"code":"<class>","message":"..."}}` under a status that
//! follows the class. A missing database and a token that may not select it
//! share one 404, as they share one FGP refusal. The health route reveals
//! nothing about databases.

use crate::Server;
use crate::execute::{Answer, Refusal, read, write};
use asupersync::Cx;
use asupersync::http::h1::types::{Method, Request, Response};
use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
use asupersync::sync::Mutex;
use core::pin::Pin;
use core::task::{Context, Poll};
use fgdb_protocol::body::{ErrorCode, Execute, ExecuteMode, Outcome};
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
        verified: VerifiedCapability<'s>,
    },
}

pub(crate) struct OutputAuthority<'s> {
    decision: Mutex<OutputDecision<'s>>,
    stopped: AtomicBool,
}

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

    fn protected(
        &self,
        cx: &Cx,
        issuer: &'s Authority,
        verified: VerifiedCapability<'s>,
    ) -> bool {
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
        *decision = OutputDecision::Protected { issuer, verified };
        true
    }

    fn authorize(&self, now: u64) -> io::Result<()> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(self.stop());
        }
        let decision = self.decision.try_lock().map_err(|_| self.stop())?;
        if let OutputDecision::Protected { issuer, verified } = &*decision {
            issuer
                .recheck_at(verified, crate::TRUNK, now)
                .map_err(|_| self.stop())?;
        }
        Ok(())
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
        if let Err(error) = self.live().and_then(|()| self.authority.authorize(now)) {
            return Poll::Ready(Err(error));
        }
        match Pin::new(&mut self.inner).poll_write(task, bytes) {
            Poll::Ready(Err(error)) => {
                self.authority.stop();
                Poll::Ready(Err(error))
            }
            result => result,
        }
    }

    fn poll_flush_at(&mut self, task: &mut Context<'_>, now: u64) -> Poll<io::Result<()>> {
        if let Err(error) = self.live().and_then(|()| self.authority.authorize(now)) {
            return Poll::Ready(Err(error));
        }
        match Pin::new(&mut self.inner).poll_flush(task) {
            Poll::Ready(Err(error)) => {
                self.authority.stop();
                Poll::Ready(Err(error))
            }
            result => result,
        }
    }

    fn poll_shutdown_at(
        &mut self,
        task: &mut Context<'_>,
        now: u64,
    ) -> Poll<io::Result<()>> {
        if let Err(error) = self.live().and_then(|()| self.authority.authorize(now)) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.inner).poll_shutdown(task)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for GuardedIo<'_, T> {
    fn poll_write(
        self: Pin<&mut Self>,
        task: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().poll_write_at(task, bytes, crate::unix_millis())
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
    crate::capability_from_hex(hex).ok_or_else(unauthenticated)
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
    match crate::execute::schema(db, &token) {
        Ok(schema) => {
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
            let (inner, state) =
                MemoryIo::new(requests(3), [usize::MAX, 0, usize::MAX, 0], true);
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
