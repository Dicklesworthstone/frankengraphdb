//! The Bolt-compat adapter: `BoltCompatProfileV1` (plan §13.5) over TCP.
//!
//! Official Neo4j drivers connect with `bolt://` (or `neo4j://`, answered
//! by a single-server routing table) and run Cypher/GQL reads and bounded
//! autocommit and explicit native write transactions. The
//! profile is a negotiated downgrade, never a drop-in-Neo4j claim:
//!
//! - Authentication is a Warden capability token in HELLO: the hex token as
//!   the `bearer` credential, or as the password of `basic` auth. A HELLO
//!   whose token no served database accepts is refused.
//! - Reads use the same capability-authorized READ session as FGP and HTTP.
//!   Outside BEGIN and without read mode, a write-capable token may queue a
//!   native write. PULL or DISCARD consumes it as one atomic program
//!   through the existing authorized writer; RUN alone publishes nothing.
//!   RESET/disconnect before consumption drops it. Scalar, temporal, list and
//!   map and graph RETURN values are converted and admitted before commit.
//!   A lost terminal response never proves rollback and is never auto-replayed.
//! - BEGIN with read mode or a read-only capability holds one pinned read
//!   session. Write mode with write authority owns the existing native
//!   workspace: RUN stages provisional results and effects, COMMIT alone
//!   publishes, and ROLLBACK/RESET/drop discard the complete prefix. Reads see
//!   earlier staged writes. Native cumulative limits and stale-basis refusals
//!   apply throughout. No database lock spans network round trips.
//! - The database is RUN/BEGIN's `db`, or the only served database.
//! - Local database-qualified bookmarks constrain BEGIN, autocommit reads,
//!   writes and schema procedures. An unmet floor refuses before execution;
//!   an open read transaction never advances its pinned snapshot to satisfy
//!   a newer bookmark. Unsupported timeouts/impersonation refuse explicitly.
//!   These local bookmarks are not signed RYW tokens or cluster read proofs.
//! - Graph values come back as Bolt nodes, relationships and paths. Labels,
//!   types, endpoints and properties are read through the same authorized
//!   session at the statement's effective snapshot. The server's catalog
//!   bindings determine the property names, with capability masking applied
//!   by the engine before every lookup. Nested lists and maps preserve these
//!   graph structures; paths retain traversal direction and repeated elements.
//! - `CALL db.labels()`, `db.relationshipTypes()` and `db.propertyKeys()`
//!   answer the schema names the token may see (the server's bindings,
//!   scope-filtered), the same answer as HTTP's schema route.
//! - Read results are ephemeral FGP-style snapshot streams: buffered per
//!   statement and dropped on disconnect, DISCARD or RESET. The exact selected
//!   issuer's expiry/retirement fence is rechecked before every socket write
//!   and flush, including delayed PULLs and partial writes. This is the existing
//!   cooperative Warden fence, not durable audit or revocation evidence.

mod autocommit;
mod causal;
mod cursors;

use crate::execute::{ReadSession, Refusal, SessionError, query_refusal, read_session};
use crate::execute::transaction::{
    TransactionStatement, looks_like_write, prepare_transaction_statement,
};
use crate::recovery::Generation;
use crate::shutdown::Waiter;
use crate::{Served, Server, convert};
use asupersync::Cx;
use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
use core::future::poll_fn;
use core::pin::Pin;
use core::task::Poll;
use fgdb::{AuthorizedWriteTransaction, PreparedNativeRead, QueryError, QueryResult};
use fgdb_bolt::message::{
    Dechunker, MAGIC, Map, Request, Response, decode_request, negotiate, structure,
};
use fgdb_bolt::packstream::{Value, get};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, PreparedGraphBranchText};
use fgdb_protocol::body::{Body, ErrorCode, Execute, ExecuteMode, WireTimestamp, WireValue};
use fgdb_protocol::transport::DuplexIo;
use fgdb_types::{
    CanonicalDate, CanonicalDuration, CanonicalLocalDateTime, CanonicalLocalTime, CanonicalTime,
    CommitSeq, EId, EmbeddedTxnCompletion, PurposeContexts, VId,
};
use fgdb_warden::{Authority, CapabilityToken};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};

static CONNECTIONS: AtomicU64 = AtomicU64::new(0);

/// Records written before the socket is flushed.
const RECORDS_PER_WRITE: usize = 256;

/// The most vertices one hydration lookup binds as a list.
const HYDRATION_BATCH: usize = 4096;

/// Why a connection ends without another message.
#[derive(Debug, PartialEq, Eq)]
struct Closed;

#[cfg(test)]
mod recovery_tests;

/// Buffered socket I/O. Reads stop at the drain signal while idle.
struct Io {
    stream: Box<dyn DuplexIo>,
    /// The local address the peer reached, read from the socket before TLS
    /// wrapped it (a boxed duplex stream no longer knows it).
    local: Option<std::net::SocketAddr>,
    dechunker: Dechunker,
    out: Vec<u8>,
    /// A failed/cancelled partial flush cannot be restarted or followed by a
    /// different response on the same byte stream.
    failed: bool,
}

impl Io {
    async fn read_some(&mut self, cx: &Cx, waiter: &Waiter) -> Result<Vec<u8>, Closed> {
        let mut buffer = [0_u8; 8192];
        let read = poll_fn(|task| {
            if waiter.poll_triggered(task) || cx.checkpoint().is_err() {
                return Poll::Ready(Err(Closed));
            }
            let mut filled = ReadBuf::new(&mut buffer);
            match Pin::new(&mut self.stream).poll_read(task, &mut filled) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(_)) => Poll::Ready(Err(Closed)),
                Poll::Ready(Ok(())) if filled.filled().is_empty() => Poll::Ready(Err(Closed)),
                Poll::Ready(Ok(())) => Poll::Ready(Ok(filled.filled().len())),
            }
        })
        .await?;
        Ok(buffer[..read].to_vec())
    }

    async fn read_exact(
        &mut self,
        cx: &Cx,
        waiter: &Waiter,
        len: usize,
    ) -> Result<Vec<u8>, Closed> {
        let mut bytes = Vec::with_capacity(len);
        while bytes.len() < len {
            let more = self.read_some(cx, waiter).await?;
            bytes.extend_from_slice(&more);
        }
        // The handshake is the only exact read, and a client sends nothing
        // more until the server answers it; any extra bytes start messages.
        let extra = bytes.split_off(len);
        self.dechunker.push(&extra);
        Ok(bytes)
    }

    async fn next_message(&mut self, cx: &Cx, waiter: &Waiter) -> Result<Vec<u8>, Closed> {
        loop {
            match self.dechunker.next_message() {
                Ok(Some(message)) => return Ok(message),
                Ok(None) => {}
                Err(_) => return Err(Closed),
            }
            let bytes = self.read_some(cx, waiter).await?;
            self.dechunker.push(&bytes);
        }
    }

    async fn flush(
        &mut self,
        cx: &Cx,
        authority: Option<(&Authority, &CapabilityToken)>,
        generation: Option<&Generation>,
    ) -> Result<(), Closed> {
        flush_generation_output(
            &mut self.stream,
            cx,
            &mut self.out,
            &mut self.failed,
            authority,
            generation,
            crate::unix_millis,
        )
        .await
    }

    fn respond(&mut self, response: &Response) {
        response.frame(&mut self.out);
    }
}

/// A flush's terminal state spans retries and future cancellation. Once a
/// prefix might have escaped, another response must never restart the buffer.
async fn flush_generation_output<W: AsyncWrite + Unpin>(
    stream: &mut W,
    cx: &Cx,
    out: &mut Vec<u8>,
    failed: &mut bool,
    authority: Option<(&Authority, &CapabilityToken)>,
    generation: Option<&Generation>,
    clock: impl FnMut() -> u64,
) -> Result<(), Closed> {
    if *failed {
        return Err(Closed);
    }
    *failed = true;
    write_output(stream, cx, out, authority, generation, clock).await?;
    out.clear();
    *failed = false;
    Ok(())
}

/// Write exactly one buffered response batch under current output authority.
/// The private clock argument makes partial-write/expiry laws deterministic;
/// production always supplies the existing host clock.
async fn write_output<W: AsyncWrite + Unpin>(
    stream: &mut W,
    cx: &Cx,
    bytes: &[u8],
    authority: Option<(&Authority, &CapabilityToken)>,
    generation: Option<&Generation>,
    mut clock: impl FnMut() -> u64,
) -> Result<(), Closed> {
    let authority = match authority {
        Some((issuer, token)) => {
            let verified = issuer
                .verify_at(token, crate::TRUNK, clock())
                .map_err(|_| Closed)?;
            Some((issuer, verified))
        }
        None => None,
    };
    let mut authorize = || {
        cx.checkpoint().map_err(|_| Closed)?;
        if let Some((issuer, verified)) = &authority {
            issuer
                .recheck_at(verified, crate::TRUNK, clock())
                .map_err(|_| Closed)?;
        }
        Ok::<_, Closed>(())
    };
    let mut watcher = generation.map(Generation::watcher);
    let mut written = 0;
    while written < bytes.len() {
        let count = poll_fn(|task| {
            while watcher
                .as_mut()
                .is_some_and(|watcher| watcher.poll_changed(task))
            {}
            if authorize().is_err() {
                return Poll::Ready(Err(Closed));
            }
            let remaining = &bytes[written..];
            let mut physical = || Pin::new(&mut *stream).poll_write(task, remaining);
            let progress = match generation {
                Some(generation) => generation.with_current(physical).map_err(|_| Closed),
                None => Ok(physical()),
            };
            match progress {
                Err(error) => Poll::Ready(Err(error)),
                Ok(Poll::Pending) => Poll::Pending,
                Ok(Poll::Ready(Ok(count))) if count > 0 && count <= remaining.len() => {
                    Poll::Ready(Ok(count))
                }
                Ok(Poll::Ready(_)) => Poll::Ready(Err(Closed)),
            }
        })
        .await?;
        written += count;
    }
    poll_fn(|task| {
        while watcher
            .as_mut()
            .is_some_and(|watcher| watcher.poll_changed(task))
        {}
        if authorize().is_err() {
            return Poll::Ready(Err(Closed));
        }
        let mut physical = || Pin::new(&mut *stream).poll_flush(task);
        let progress = match generation {
            Some(generation) => generation.with_current(physical).map_err(|_| Closed),
            None => Ok(physical()),
        };
        match progress {
            Err(error) => Poll::Ready(Err(error)),
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(result)) => Poll::Ready(result.map_err(|_| Closed)),
        }
    })
    .await
}

/// A Bolt-visible refusal: a Neo4j status code and a message.
#[derive(Debug)]
struct Failure {
    code: &'static str,
    message: String,
}

impl Failure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new("Neo.ClientError.Request.Invalid", message)
    }

    /// The closed FGP error classes onto Neo4j status codes. Only transient
    /// classes use a `TransientError` code, the class drivers retry.
    fn from_refusal(refusal: Refusal) -> Self {
        let code = match refusal.code {
            ErrorCode::Statement | ErrorCode::Protocol | ErrorCode::UnsupportedVersion => {
                "Neo.ClientError.Statement.SyntaxError"
            }
            ErrorCode::Unauthenticated => "Neo.ClientError.Security.Unauthorized",
            ErrorCode::PermissionDenied => "Neo.ClientError.Security.Forbidden",
            ErrorCode::NotFoundOrUnauthorized => "Neo.ClientError.Database.DatabaseNotFound",
            ErrorCode::Budget => "Neo.ClientError.Statement.ExecutionFailed",
            ErrorCode::Busy | ErrorCode::Draining | ErrorCode::DatabaseRecovering => {
                "Neo.TransientError.General.DatabaseUnavailable"
            }
            ErrorCode::DatabaseUnavailable => "Neo.DatabaseError.General.UnknownError",
            ErrorCode::Execution
            | ErrorCode::Conflict
            | ErrorCode::OutcomeUnknown
            | ErrorCode::Cancelled => "Neo.DatabaseError.Statement.ExecutionFailed",
        };
        Self::new(code, refusal.message)
    }
}

/// A statement's buffered rows awaiting PULL.
struct Pending<'s> {
    rows: VecDeque<Vec<Value>>,
    database: String,
    /// The exact issuer that authorized RUN, retained across delayed PULLs.
    db: &'s Served,
    /// The generation read, which names the bookmark; none for an answer
    /// read from the server's bindings rather than the graph.
    seq: Option<CommitSeq>,
    generation: Generation,
    /// A deferred autocommit or the summary of its already-admitted rows.
    /// Only the deferred state can execute; partial delivery retains completion.
    write: Option<autocommit::PendingWrite>,
}

enum TransactionSession<'s> {
    Read(Box<ReadSession<'s>>),
    Write {
        owner: Box<AuthorizedWriteTransaction<'s, fn() -> u64>>,
        generation: Generation,
    },
}

impl TransactionSession<'_> {
    fn generation(&self) -> Generation {
        match self {
            Self::Read(session) => session.generation(),
            Self::Write { generation, .. } => generation.clone(),
        }
    }
}

/// One explicit owner and its independently retained provisional results.
struct Transaction<'s> {
    name: String,
    db: &'s Served,
    session: TransactionSession<'s>,
    seq: CommitSeq,
    cursors: cursors::Cursors<Pending<'s>>,
}

/// Serve one Bolt connection until GOODBYE, disconnect or drain.
pub(crate) async fn run(
    cx: &Cx,
    server: &Server,
    stream: Box<dyn DuplexIo>,
    local: Option<std::net::SocketAddr>,
) {
    let waiter = server.shutdown.waiter();
    let mut io = Io {
        stream,
        local,
        dechunker: Dechunker::new(server.limits.max_frame_len as usize),
        out: Vec::new(),
        failed: false,
    };
    let Ok(preamble) = io.read_exact(cx, &waiter, 20).await else {
        return;
    };
    if preamble[..4] != MAGIC {
        return;
    }
    let proposals: [u8; 16] = preamble[4..20].try_into().expect("sixteen bytes");
    let Some(version) = negotiate(&proposals) else {
        io.out.extend_from_slice(&[0, 0, 0, 0]);
        let _ = io.flush(cx, None, None).await;
        return;
    };
    io.out.extend_from_slice(&version.response());
    if io.flush(cx, None, None).await.is_err() {
        return;
    }
    let mut connection = Connection {
        server,
        token: None,
        failed: false,
        pending: None,
        transaction: None,
        output_database: None,
        output_generation: None,
        id: CONNECTIONS.fetch_add(1, Ordering::Relaxed),
    };
    while let Ok(message) = io.next_message(cx, &waiter).await {
        let request = match decode_request(&message) {
            Ok(request) => request,
            Err(error) => {
                // An undecodable message leaves the stream state unknown.
                io.respond(&failure(Failure::invalid(error.to_string())));
                let _ = io.flush(cx, connection.output_authority(), None).await;
                return;
            }
        };
        let close = connection.handle(cx, &mut io, request).await;
        if io
            .flush(
                cx,
                connection.output_authority(),
                connection.output_generation.as_ref(),
            )
            .await
            .is_err()
            || close
        {
            return;
        }
    }
}

fn failure(failure: Failure) -> Response {
    Response::Failure {
        code: failure.code.to_owned(),
        message: failure.message,
    }
}

struct Connection<'s> {
    server: &'s Server,
    token: Option<CapabilityToken>,
    failed: bool,
    pending: Option<Pending<'s>>,
    transaction: Option<Transaction<'s>>,
    /// Exact issuer selected by the request producing the buffered response,
    /// retained through final metadata after PULL/COMMIT consumes its owner.
    output_database: Option<&'s Served>,
    output_generation: Option<Generation>,
    id: u64,
}

impl<'s> Connection<'s> {
    fn output_authority(&self) -> Option<(&Authority, &CapabilityToken)> {
        self.output_database
            .zip(self.token.as_ref())
            .map(|(database, token)| (&database.authority, token))
    }

    /// Answer one request; true closes the connection.
    async fn handle(&mut self, cx: &Cx, io: &mut Io, request: Request) -> bool {
        self.output_database = None;
        self.output_generation = None;
        match request {
            Request::Goodbye => {
                self.pending = None;
                self.transaction = None;
                return true;
            }
            Request::Reset if self.token.is_none() => {
                io.respond(&failure(Failure::new(
                    "Neo.ClientError.Security.Unauthorized",
                    "HELLO must authenticate before RESET",
                )));
                return true;
            }
            Request::Reset => {
                self.failed = false;
                self.pending = None;
                self.transaction = None;
                io.respond(&Response::Success(Vec::new()));
                return false;
            }
            _ if self.failed => {
                io.respond(&Response::Ignored);
                return false;
            }
            Request::Hello { .. } if self.token.is_some() => {
                io.respond(&failure(Failure::invalid("HELLO was already sent")));
                return true;
            }
            Request::Hello { extra } => {
                match self.hello(&extra) {
                    Ok(metadata) => io.respond(&Response::Success(metadata)),
                    Err(refusal) => {
                        io.respond(&failure(refusal));
                        return true;
                    }
                }
                return false;
            }
            _ if self.token.is_none() => {
                io.respond(&failure(Failure::new(
                    "Neo.ClientError.Security.Unauthorized",
                    "HELLO must authenticate first",
                )));
                return true;
            }
            _ => {}
        }
        let outcome = match request {
            Request::Run {
                query,
                parameters,
                extra,
            } => self.run(cx, &query, &parameters, &extra).await,
            Request::Pull { n, qid: -1 } => self.pull(cx, io, n, false).await,
            Request::Discard { n, qid: -1 } => self.pull(cx, io, n, true).await,
            Request::Pull { n, qid } => self.pull_selected(cx, io, n, qid, false).await,
            Request::Discard { n, qid } => self.pull_selected(cx, io, n, qid, true).await,
            Request::Begin { extra } => self.begin(cx, &extra).await,
            Request::Commit
                if self.pending.is_some()
                    || self
                        .transaction
                        .as_ref()
                        .is_some_and(|transaction| !transaction.cursors.is_empty()) =>
            {
                Err(Failure::invalid(
                    "PULL or DISCARD every result before COMMIT",
                ))
            }
            Request::Commit => self.commit_transaction(cx).await,
            Request::Rollback => match self.transaction.take() {
                Some(transaction) => {
                    self.output_database = Some(transaction.db);
                    self.pending = None;
                    Ok(Vec::new())
                }
                None => Err(Failure::invalid("ROLLBACK outside a transaction")),
            },
            Request::Route { routing, extra, .. } => self.route(io, &routing, &extra),
            Request::Hello { .. } | Request::Goodbye | Request::Reset => {
                unreachable!("handled above")
            }
        };
        match outcome {
            Ok(metadata) => io.respond(&Response::Success(metadata)),
            Err(refusal) => {
                // A failure ends any transaction and drops buffered rows.
                self.failed = true;
                self.pending = None;
                self.transaction = None;
                self.output_generation = None;
                io.respond(&failure(refusal));
            }
        }
        false
    }

    fn hello(&mut self, extra: &Map) -> Result<Map, Failure> {
        let unauthorized = || {
            Failure::new(
                "Neo.ClientError.Security.Unauthorized",
                "credential not accepted",
            )
        };
        let credential = match get(extra, "scheme").and_then(Value::as_str) {
            Some("bearer" | "basic") => get(extra, "credentials").and_then(Value::as_str),
            _ => None,
        };
        let token = credential
            .and_then(crate::capability_from_hex)
            .ok_or_else(unauthorized)?;
        // A token no served database accepts can never run a statement.
        if !self.server.databases.values().any(|db| db.admits(&token)) {
            return Err(unauthorized());
        }
        self.token = Some(token);
        Ok(vec![
            (
                "server".to_owned(),
                Value::string(concat!("FrankenGraphDB/", env!("CARGO_PKG_VERSION"))),
            ),
            (
                "connection_id".to_owned(),
                Value::string(format!("bolt-{}", self.id)),
            ),
            ("hints".to_owned(), Value::Map(Vec::new())),
            ("fgdb_profile".to_owned(), Value::string(fgdb_bolt::PROFILE)),
            ("fgdb_autocommit_writes".to_owned(), Value::Bool(true)),
            (
                "fgdb_explicit_write_transactions".to_owned(),
                Value::Bool(true),
            ),
        ])
    }

    /// The database a RUN, BEGIN or ROUTE names, or the only one served,
    /// if this connection's token may select it. A missing database and an
    /// unauthorized one look the same, as on every other surface.
    fn database(&self, extra: &Map) -> Result<(String, &'s Served), Failure> {
        let not_found = || {
            Failure::new(
                "Neo.ClientError.Database.DatabaseNotFound",
                "database not found or not authorized",
            )
        };
        let name = match get(extra, "db") {
            Some(Value::String(name)) if !name.is_empty() => name.clone(),
            None | Some(Value::Null) | Some(Value::String(_)) => {
                match self.server.databases.keys().collect::<Vec<_>>().as_slice() {
                    [only] => (*only).clone(),
                    _ => return Err(not_found()),
                }
            }
            Some(_) => return Err(Failure::invalid("db must be a string")),
        };
        let token = self.token.as_ref().ok_or_else(not_found)?;
        let db = self
            .server
            .databases
            .get(&name)
            .filter(|db| db.admits(token))
            .ok_or_else(not_found)?;
        Ok((name, db))
    }

    async fn begin(&mut self, cx: &Cx, extra: &Map) -> Result<Map, Failure> {
        if self.transaction.is_some() || self.pending.is_some() {
            return Err(Failure::invalid(
                "BEGIN while a statement or transaction is open",
            ));
        }
        let (name, db) = self.database(extra)?;
        self.output_database = Some(db);
        let token = self.token.as_ref().expect("authenticated");
        let options = causal::options(extra, &name)?;
        let verified = db
            .authority
            .verify_at(token, crate::TRUNK, crate::unix_millis())
            .map_err(|error| Failure::from_refusal(crate::execute::write_refusal(&error)))?;
        let writable = options.mode == causal::AccessMode::Write
            && matches!(
                verified.predicates().rights(),
                fgdb_warden::Rights::Write | fgdb_warden::Rights::ReadWrite
            );
        let (session, seq) = if writable {
            crate::execute::authorize_write(db, token).map_err(Failure::from_refusal)?;
            let generation = db
                .db
                .generation()
                .map_err(|error| Failure::from_refusal(error.into()))?;
            let _operation = generation
                .enter()
                .map_err(|error| Failure::from_refusal(error.into()))?;
            let contexts = PurposeContexts::narrow_runtime_root(cx);
            let mut guard = db
                .db
                .write(cx)
                .await
                .map_err(|error| Failure::from_refusal(error.into()))?;
            generation
                .check()
                .map_err(|error| Failure::from_refusal(error.into()))?;
            let owner = guard
                .begin_authorized_write(
                    &contexts.txn(),
                    &contexts.commit(),
                    &db.authority,
                    token,
                    crate::TRUNK,
                    db.write_policy.insertion_policy(),
                    db.max_statements,
                    crate::unix_millis as fn() -> u64,
                )
                .map_err(|error| Failure::from_refusal(crate::execute::write_refusal(&error)))?;
            let seq = owner.basis();
            if seq < options.minimum_seq {
                return Err(Failure::new(
                    "Neo.TransientError.General.DatabaseUnavailable",
                    "the local bookmark has not been reached; no transaction was opened",
                ));
            }
            generation
                .check()
                .map_err(|error| Failure::from_refusal(error.into()))?;
            (
                TransactionSession::Write {
                    owner: Box::new(owner),
                    generation,
                },
                seq,
            )
        } else {
            let (session, seq) = causal::session(cx, db, token, &name, extra).await?;
            (TransactionSession::Read(Box::new(session)), seq)
        };
        self.output_generation = Some(session.generation());
        self.transaction = Some(Transaction {
            name,
            db,
            session,
            seq,
            cursors: cursors::Cursors::new(),
        });
        Ok(Vec::new())
    }

    /// Remove ownership before the first await. Cancellation, recovery or an
    /// ambiguous completion cannot leave a transaction that COMMIT can replay.
    async fn commit_transaction(&mut self, cx: &Cx) -> Result<Map, Failure> {
        let transaction = self
            .transaction
            .take()
            .ok_or_else(|| Failure::invalid("COMMIT outside a transaction"))?;
        self.output_database = Some(transaction.db);
        self.pending = None;
        let generation = transaction.session.generation();
        self.output_generation = Some(generation.clone());
        let _operation = generation
            .enter()
            .map_err(|error| Failure::from_refusal(error.into()))?;
        let seq = match transaction.session {
            TransactionSession::Read(_) => transaction.seq,
            TransactionSession::Write { owner, .. } => {
                let mut guard = transaction
                    .db
                    .db
                    .write(cx)
                    .await
                    .map_err(|error| Failure::from_refusal(error.into()))?;
                generation
                    .check()
                    .map_err(|error| Failure::from_refusal(error.into()))?;
                let completion = (*owner).commit(&mut guard).await.map_err(|error| {
                    Failure::from_refusal(crate::execute::write_refusal(&error))
                })?;
                drop(guard);
                match completion {
                    EmbeddedTxnCompletion::WriteCommitted { commit_seq } => {
                        transaction.db.commits.committed();
                        commit_seq
                    }
                    EmbeddedTxnCompletion::ReadClosed { snapshot_seq, .. } => snapshot_seq,
                }
            }
        };
        // Publication has already decided a write outcome. A later recovery
        // fence must close the guarded socket, never turn this success into a
        // retryable refusal that could repeat a committed transaction.
        Ok(vec![(
            "bookmark".to_owned(),
            Value::string(bookmark(&transaction.name, seq)),
        )])
    }

    /// Stage while the owner is removed from the connection. Any refusal or
    /// cancelled future abandons the entire prefix. Graph metadata is owned
    /// from the authorized private overlay before a cursor can be published.
    async fn run_transaction_statement(
        &mut self,
        cx: &Cx,
        text: &str,
        arguments: &Map,
        parameters: &GqlParameters,
        request_read_only: bool,
    ) -> Result<Map, Failure> {
        let mut transaction = self.transaction.take().expect("write owner selected");
        let db = transaction.db;
        let token = self.token.as_ref().expect("authenticated");
        crate::execute::authorize_write(db, token).map_err(Failure::from_refusal)?;
        crate::execute::refuse_element_identity(text).map_err(Failure::from_refusal)?;
        let generation = transaction.session.generation();
        self.output_database = Some(db);
        self.output_generation = Some(generation.clone());
        let _operation = generation
            .enter()
            .map_err(|error| Failure::from_refusal(error.into()))?;
        let TransactionSession::Write { owner, .. } = &mut transaction.session else {
            unreachable!("write owner selected");
        };
        // Preserve the complete request/value bounds of the other adapters.
        Execute {
            mode: ExecuteMode::Write,
            statement: text.to_owned(),
            parameters: wire_parameters(arguments)?,
        }
        .encode()
        .map_err(|error| Failure::invalid(error.to_string()))?;
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        let query_cx = crate::query_cx(&contexts);
        query_cx
            .checkpoint()
            .map_err(|_| Failure::invalid("transaction preparation cancelled"))?;
        let mut guard = db
            .db
            .write(cx)
            .await
            .map_err(|error| Failure::from_refusal(error.into()))?;
        generation
            .check()
            .map_err(|error| Failure::from_refusal(error.into()))?;
        crate::execute::authorize_write(db, token).map_err(Failure::from_refusal)?;
        let mut preparation_refusal = None;
        let statement = owner
            .prepare_statement(
                &guard,
                &query_cx,
                text,
                parameters,
                |parameters, scope, work| match prepare_transaction_statement(
                    db, text, parameters, scope, work,
                )
                .map_err(Failure::from_refusal)
                {
                    Ok(statement) => Ok(statement),
                    Err(error) => {
                        preparation_refusal = Some(error);
                        Err(fgdb::WriteTxnError::AuthorizedMutationRefused)
                    }
                },
            )
            .map_err(|error| {
                // The native owner retains control failures even if a parser's
                // Option resolver reports an unrelated missing-symbol error.
                if matches!(&error, fgdb::WriteTxnError::AuthorizedMutationRefused)
                    && let Some(refusal) = preparation_refusal
                {
                    return refusal;
                }
                Failure::from_refusal(crate::execute::write_refusal(&error))
            })?;
        let columns = statement.columns().to_vec();
        let is_write = matches!(&statement, TransactionStatement::Write(_));
        if is_write && request_read_only {
            return Err(read_only());
        }
        let before = owner
            .statistics()
            .ok_or_else(|| Failure::invalid("transaction is finished"))?;
        let mut admitted = None;
        let mut output_refusal = None;
        let mut collect =
            |rows: Result<Vec<Vec<WireValue>>, Refusal>,
             work: &mut dyn FnMut(u64) -> Result<(), fgdb::WriteTxnError>,
             metadata: &mut dyn fgdb::AuthorizedWriteOutput| {
                let output = rows.and_then(|rows| {
                    autocommit::collect_returning(
                        rows,
                        self.server.limits.max_frame_len as usize,
                        work,
                        metadata,
                        db.symbols.borrowed_reverse_catalog(),
                    )
                });
                match output {
                    Ok(rows) => {
                        admitted = Some(rows);
                        Ok(())
                    }
                    Err(error) => {
                        output_refusal = Some(error);
                        Err(fgdb::WriteTxnError::AuthorizedMutationRefused)
                    }
                }
            };
        let result = match &statement {
            TransactionStatement::Read(statement) => owner
                .read(&mut guard, &query_cx, statement, |rows, work, metadata| {
                    collect(
                        crate::execute::graph_output_rows(&columns, rows, work),
                        work,
                        metadata,
                    )
                })
                .map(|_| ()),
            TransactionStatement::Aggregate {
                statement, slots, ..
            } => owner
                .read_aggregate(&mut guard, &query_cx, statement, |rows, work, metadata| {
                    collect(
                        crate::execute::graph_output_aggregate(&columns, slots, rows, work),
                        work,
                        metadata,
                    )
                })
                .map(|_| ()),
            TransactionStatement::Write(statement) => owner
                .stage_chain(&mut guard, &query_cx, statement, |rows, work, metadata| {
                    collect(
                        crate::execute::graph_output_rows(&columns, rows, work),
                        work,
                        metadata,
                    )
                })
                .map(|_| ()),
        };
        drop(guard);
        result.map_err(|error| {
            Failure::from_refusal(
                output_refusal.unwrap_or_else(|| crate::execute::write_refusal(&error)),
            )
        })?;
        generation
            .check()
            .map_err(|error| Failure::from_refusal(error.into()))?;
        let after = owner
            .statistics()
            .expect("successful statement retains owner");
        let vertices = after.created_vertices - before.created_vertices;
        let edges = after.created_edges - before.created_edges;
        let changed =
            vertices != 0 || edges != 0 || after.mutation_effects != before.mutation_effects;
        let mut stats = vec![("contains-updates".to_owned(), Value::Bool(changed))];
        for (name, count) in [
            ("nodes-created", vertices),
            ("relationships-created", edges),
        ] {
            if count != 0 {
                let count = i64::try_from(count)
                    .map_err(|_| Failure::invalid("transaction statement count is out of range"))?;
                stats.push((name.to_owned(), Value::Int(count)));
            }
        }
        // Counters describe this provisional statement. Only COMMIT returns
        // the native completion bookmark; PULL/DISCARD never publish a write.
        let summary = vec![
            (
                "type".to_owned(),
                Value::string(if is_write { "w" } else { "r" }),
            ),
            ("t_last".to_owned(), Value::Int(0)),
            ("db".to_owned(), Value::string(transaction.name.clone())),
            ("stats".to_owned(), Value::Map(stats)),
            ("fgdb_transaction_local".to_owned(), Value::Bool(true)),
        ];
        let pending = Pending {
            rows: admitted.expect("checked statement output admission ran"),
            database: transaction.name.clone(),
            db,
            seq: None,
            generation,
            write: Some(autocommit::PendingWrite::Completed(summary)),
        };
        self.transaction = Some(transaction);
        let qid = match self.buffer_result(cx, pending) {
            Ok(Some(qid)) => qid,
            Ok(None) => unreachable!("transaction result has an owner"),
            Err(error) => {
                self.transaction = None;
                return Err(error);
            }
        };
        Ok(vec![
            (
                "fields".to_owned(),
                Value::List(columns.into_iter().map(Value::String).collect()),
            ),
            ("t_first".to_owned(), Value::Int(0)),
            ("qid".to_owned(), Value::Int(qid)),
        ])
    }

    async fn run(
        &mut self,
        cx: &Cx,
        query: &str,
        parameters: &Map,
        extra: &Map,
    ) -> Result<Map, Failure> {
        if self.pending.is_some() {
            return Err(Failure::invalid(
                "RUN while a result is still streaming; PULL or DISCARD it first",
            ));
        }
        if let Some(transaction) = self.transaction.as_ref() {
            self.output_database = Some(transaction.db);
            let generation = transaction.session.generation();
            generation
                .check()
                .map_err(|error| Failure::from_refusal(error.into()))?;
            self.output_generation = Some(generation);
            causal::in_transaction(extra, &transaction.name, transaction.seq)?;
            transaction.cursors.check_admission()?;
        }
        // Retain the legacy standalone schema response's lack of a bookmark.
        // Execution and authorization now belong to the ordinary native CALL
        // path, including for these forms; this hint grants no authority.
        let schema_metadata = self.transaction.is_none() && schema_procedure(query).is_some();
        let arguments = statement_parameters(parameters)?;
        if self.transaction.as_ref().is_some_and(|transaction| {
            matches!(&transaction.session, TransactionSession::Write { .. })
        }) {
            return self
                .run_transaction_statement(
                    cx,
                    query,
                    parameters,
                    &arguments,
                    get(extra, "mode").and_then(Value::as_str) == Some("r"),
                )
                .await;
        }
        if let Some(metadata) =
            self.queue_autocommit_write(cx, query, parameters, &arguments, extra)?
        {
            return Ok(metadata);
        }
        let parameters = arguments;
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        let query_cx = crate::query_cx(&contexts);
        // An autocommit statement pins its own generation; one inside a
        // transaction reads the transaction's.
        let mut autocommit = None;
        let (name, db, seq) = if let Some(transaction) = self.transaction.as_mut() {
            (transaction.name.clone(), transaction.db, transaction.seq)
        } else {
            let (name, db) = self.database(extra)?;
            self.output_database = Some(db);
            let token = self.token.as_ref().expect("authenticated");
            let (session, seq) = causal::session(cx, db, token, &name, extra).await?;
            autocommit = Some(session);
            (name, db, seq)
        };
        self.output_database = Some(db);
        let session = match autocommit.as_mut() {
            Some(session) => session,
            None => match &mut self.transaction.as_mut().expect("in a transaction").session {
                TransactionSession::Read(session) => session.as_mut(),
                TransactionSession::Write { .. } => unreachable!("write owner dispatched above"),
            },
        };
        let generation = session.generation();
        self.output_generation = Some(generation.clone());
        let (columns, rows, selected) =
            match session.query_with_snapshot(&query_cx, query, &parameters) {
                Ok((QueryResult::Rows { columns, rows }, selected)) => (
                    columns,
                    rows.iter()
                        .map(|row| row.iter().map(convert::cell).collect::<Vec<_>>())
                        .collect::<Vec<_>>(),
                    selected,
                ),
                Ok((QueryResult::Write { .. }, _)) => return Err(read_only()),
                Err(error) => {
                    return Err(if looks_like_write(query) {
                        read_only()
                    } else {
                        Failure::from_refusal(query_refusal(error))
                    });
                }
            };
        let entities = hydrate(&query_cx, session, db, &rows, selected)?;
        let rows = rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|cell| value(cell, &entities))
                    .collect::<Result<Vec<_>, _>>()
            })
            .collect::<Result<VecDeque<_>, _>>()?;
        let qid = self.buffer_result(
            cx,
            Pending {
                rows,
                database: name,
                db,
                seq: (!schema_metadata).then_some(seq),
                generation,
                write: None,
            },
        )?;
        let mut metadata = vec![
            (
                "fields".to_owned(),
                Value::List(columns.into_iter().map(Value::String).collect()),
            ),
            ("t_first".to_owned(), Value::Int(0)),
        ];
        if let Some(qid) = qid {
            metadata.push(("qid".to_owned(), Value::Int(qid)));
        }
        Ok(metadata)
    }

    async fn pull_pending(
        &mut self,
        cx: &Cx,
        io: &mut Io,
        n: i64,
        discard: bool,
    ) -> Result<Map, Failure> {
        if self.pending.as_ref().is_some_and(|pending| {
            matches!(&pending.write, Some(autocommit::PendingWrite::Deferred(_)))
        }) {
            // Execute and admit the complete result once before sending or
            // discarding any row, even when this request asks for one row.
            // Only RESET/disconnect before this point abandons the write.
            self.finish_autocommit_write(cx).await?;
        }
        let Some(pending) = self.pending.as_mut() else {
            return Err(Failure::invalid("no result to PULL or DISCARD"));
        };
        let db = pending.db;
        self.output_database = Some(db);
        pending
            .generation
            .check()
            .map_err(|error| Failure::from_refusal(error.into()))?;
        self.output_generation = Some(pending.generation.clone());
        let token = self.token.as_ref().expect("authenticated");
        let count = if n < 0 {
            pending.rows.len()
        } else {
            pending
                .rows
                .len()
                .min(usize::try_from(n).unwrap_or(usize::MAX))
        };
        for (index, row) in pending.rows.drain(..count).enumerate() {
            // DISCARD has no socket awaits: it needs the same bounded
            // cancellation cadence as delivery, before skipping each batch.
            if index % RECORDS_PER_WRITE == 0 {
                cursors::checkpoint(cx)?;
            }
            if discard {
                continue;
            }
            io.respond(&Response::Record(row));
            // Stream large results instead of buffering every record.
            if index % RECORDS_PER_WRITE == RECORDS_PER_WRITE - 1
                && io
                    .flush(cx, Some((&db.authority, token)), Some(&pending.generation))
                    .await
                    .is_err()
            {
                return Err(Failure::invalid("connection closed while streaming"));
            }
        }
        if !pending.rows.is_empty() {
            return Ok(vec![("has_more".to_owned(), Value::Bool(true))]);
        }
        let pending = self.pending.take().expect("checked above");
        match pending.write {
            Some(autocommit::PendingWrite::Completed(summary)) => return Ok(summary),
            Some(autocommit::PendingWrite::Deferred(_)) => {
                return Err(Failure::invalid("unconsumed write cannot finish as a read"));
            }
            None => {}
        }
        let mut metadata = vec![
            ("type".to_owned(), Value::string("r")),
            ("t_last".to_owned(), Value::Int(0)),
            ("db".to_owned(), Value::string(pending.database.clone())),
        ];
        if let (None, Some(seq)) = (&self.transaction, pending.seq) {
            metadata.push((
                "bookmark".to_owned(),
                Value::string(bookmark(&pending.database, seq)),
            ));
        }
        Ok(metadata)
    }

    /// A single-server routing table, so `neo4j://` URIs work: this server
    /// advertises READ and WRITE roles. Writes use autocommit or an admitted
    /// explicit write owner; routing cannot widen an existing read-only owner.
    fn route(&mut self, io: &Io, routing: &Map, extra: &Map) -> Result<Map, Failure> {
        let (name, db) = self.database(extra)?;
        self.output_database = Some(db);
        self.output_generation = Some(
            db.db
                .generation()
                .map_err(|error| Failure::from_refusal(error.into()))?,
        );
        let address = get(routing, "address")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| io.local.map(|addr| addr.to_string()))
            .ok_or_else(|| Failure::invalid("no routing address"))?;
        let server = |role: &str| {
            Value::Map(vec![
                (
                    "addresses".to_owned(),
                    Value::List(vec![Value::string(address.clone())]),
                ),
                ("role".to_owned(), Value::string(role)),
            ])
        };
        Ok(vec![(
            "rt".to_owned(),
            Value::Map(vec![
                ("ttl".to_owned(), Value::Int(300)),
                ("db".to_owned(), Value::string(name)),
                (
                    "servers".to_owned(),
                    Value::List(vec![server("WRITE"), server("READ"), server("ROUTE")]),
                ),
            ]),
        )])
    }
}

#[derive(Clone, Copy)]
enum SchemaKind {
    Labels,
    Relations,
    Properties,
}

/// Recognize the legacy schema response's no-bookmark metadata, never an
/// execution shortcut. The native compiler and authorized session execute
/// every call, including these plain forms: `CALL db.labels()`, optionally
/// `YIELD label`, and likewise for
/// `db.relationshipTypes()` / `relationshipType` and `db.propertyKeys()` /
/// `propertyKey`. Case-insensitive, whitespace-tolerant, one optional `;`.
fn schema_procedure(query: &str) -> Option<(&'static str, SchemaKind)> {
    let words: Vec<String> = query
        .trim()
        .trim_end_matches(';')
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect();
    let (procedure, rest) = match words.as_slice() {
        [call, procedure, rest @ ..] if call == "call" => (procedure.as_str(), rest),
        _ => return None,
    };
    let (column, kind) = match procedure {
        "db.labels()" => ("label", SchemaKind::Labels),
        "db.relationshiptypes()" => ("relationshipType", SchemaKind::Relations),
        "db.propertykeys()" => ("propertyKey", SchemaKind::Properties),
        _ => return None,
    };
    match rest {
        [] => Some((column, kind)),
        [yield_, output] if yield_ == "yield" && output.eq_ignore_ascii_case(column) => {
            Some((column, kind))
        }
        _ => None,
    }
}

fn read_only() -> Failure {
    Failure::new(
        "Neo.ClientError.Statement.AccessMode",
        "Bolt writes require write authority and a write-mode transaction or autocommit",
    )
}

fn bookmark(database: &str, seq: CommitSeq) -> String {
    format!("fgdb:{database}:{}", seq.0)
}

/// RUN parameters as statement arguments, through the same conversion and
/// domain rules as FGP's.
fn statement_parameters(parameters: &Map) -> Result<GqlParameters, Failure> {
    let wire = wire_parameters(parameters)?;
    convert::parameters(&wire, None)
        .map_err(|error| Failure::new("Neo.ClientError.Statement.ArgumentError", error.to_string()))
}

fn wire_parameters(parameters: &Map) -> Result<Vec<(String, WireValue)>, Failure> {
    let mut wire = Vec::with_capacity(parameters.len());
    for (name, value) in parameters {
        wire.push((name.clone(), argument(value)?));
    }
    wire.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(wire)
}

fn argument(value: &Value) -> Result<WireValue, Failure> {
    let unsupported = |what: &str| {
        Failure::new(
            "Neo.ClientError.Statement.TypeError",
            format!("{what} parameters are not supported over Bolt"),
        )
    };
    let local_time = |nanos: i64| {
        u64::try_from(nanos)
            .ok()
            .and_then(|nanos| CanonicalLocalTime::from_nanos_of_day(nanos).ok())
            .ok_or_else(|| unsupported("out-of-range time"))
    };
    Ok(match value {
        Value::Null => WireValue::Null,
        Value::Bool(value) => WireValue::Bool(*value),
        Value::Int(value) => WireValue::Int(*value),
        Value::Float(value) => WireValue::Float(*value),
        Value::String(text) => WireValue::Text(text.clone()),
        Value::Bytes(bytes) => WireValue::Bytes(bytes.clone()),
        Value::List(items) => {
            WireValue::List(items.iter().map(argument).collect::<Result<_, _>>()?)
        }
        Value::Map(entries) => {
            let mut entries = entries
                .iter()
                .map(|(key, value)| Ok((key.clone(), argument(value)?)))
                .collect::<Result<Vec<_>, Failure>>()?;
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            WireValue::Map(entries)
        }
        Value::Struct { tag, fields } if *tag == structure::DATE_TIME => match fields.as_slice() {
            [Value::Int(seconds), Value::Int(nanos), Value::Int(offset)] => {
                WireValue::Timestamp(WireTimestamp {
                    instant_utc_nanos: i128::from(*seconds) * 1_000_000_000 + i128::from(*nanos),
                    utc_offset_seconds: i32::try_from(*offset)
                        .map_err(|_| unsupported("out-of-range date-time"))?,
                    zone: None,
                })
            }
            _ => return Err(unsupported("malformed date-time")),
        },
        Value::Struct { tag, fields } if *tag == structure::DATE => match fields.as_slice() {
            [Value::Int(days)] => WireValue::Date(
                CanonicalDate::from_epoch_day(*days)
                    .map_err(|_| unsupported("out-of-range date"))?
                    .to_string(),
            ),
            _ => return Err(unsupported("malformed date")),
        },
        Value::Struct { tag, fields } if *tag == structure::LOCAL_TIME => match fields.as_slice() {
            [Value::Int(nanos)] => WireValue::LocalTime(local_time(*nanos)?.to_string()),
            _ => return Err(unsupported("malformed local time")),
        },
        Value::Struct { tag, fields } if *tag == structure::TIME => match fields.as_slice() {
            [Value::Int(nanos), Value::Int(offset)] => {
                let offset =
                    i32::try_from(*offset).map_err(|_| unsupported("out-of-range time"))?;
                WireValue::Time(
                    CanonicalTime::new(local_time(*nanos)?, offset)
                        .map_err(|_| unsupported("out-of-range time"))?
                        .to_string(),
                )
            }
            _ => return Err(unsupported("malformed time")),
        },
        Value::Struct { tag, fields } if *tag == structure::LOCAL_DATE_TIME => {
            match fields.as_slice() {
                [Value::Int(seconds), Value::Int(nanos)] if (0..1_000_000_000).contains(nanos) => {
                    let date = CanonicalDate::from_epoch_day(seconds.div_euclid(86_400))
                        .map_err(|_| unsupported("out-of-range local date-time"))?;
                    let time = local_time(seconds.rem_euclid(86_400) * 1_000_000_000 + nanos)?;
                    WireValue::LocalDateTime(CanonicalLocalDateTime::new(date, time).to_string())
                }
                _ => return Err(unsupported("malformed local date-time")),
            }
        }
        Value::Struct { tag, fields } if *tag == structure::DURATION => match fields.as_slice() {
            [
                Value::Int(months),
                Value::Int(days),
                Value::Int(seconds),
                Value::Int(nanos),
            ] => WireValue::Duration(
                CanonicalDuration::new(
                    *months,
                    *days,
                    i128::from(*seconds) * 1_000_000_000 + i128::from(*nanos),
                )
                .map_err(|_| unsupported("out-of-range duration"))?
                .to_string(),
            ),
            _ => return Err(unsupported("malformed duration")),
        },
        Value::Struct { .. } => return Err(unsupported("this structure's")),
    })
}

/// A hydrated vertex: its visible labels and properties.
type Nodes = BTreeMap<u128, (Vec<String>, Vec<(String, Value)>)>;

/// A relationship's stored orientation, visible type and masked properties.
struct Relationship {
    source: u128,
    target: u128,
    kind: String,
    properties: Vec<(String, Value)>,
}

#[derive(Default)]
struct Entities {
    nodes: Nodes,
    relationships: BTreeMap<u128, Relationship>,
}

/// Every graph identity a result carries, including paths inside collections.
fn collect_entities(value: &WireValue, vertices: &mut BTreeSet<u128>, edges: &mut BTreeSet<u128>) {
    match value {
        WireValue::Vertex(id) => {
            vertices.insert(*id);
        }
        WireValue::Vertices(ids) => vertices.extend(ids.iter().copied()),
        WireValue::Edge(id) => {
            edges.insert(*id);
        }
        WireValue::Edges(ids) => edges.extend(ids.iter().copied()),
        WireValue::Path { start, steps } => {
            vertices.insert(*start);
            for &(edge, vertex) in steps {
                edges.insert(edge);
                vertices.insert(vertex);
            }
        }
        WireValue::List(items) => items
            .iter()
            .for_each(|item| collect_entities(item, vertices, edges)),
        WireValue::Map(entries) => entries
            .iter()
            .for_each(|(_, value)| collect_entities(value, vertices, edges)),
        _ => {}
    }
}

/// Execute a metadata lookup through the original authorized session. Only a
/// hidden catalog binding reads as absent; expiry, cancellation and exhausted
/// signed budgets abort the whole RUN instead of emitting incomplete values.
fn lookup(
    query_cx: &fgdb_types::QueryCx,
    session: &mut ReadSession<'_>,
    text: &str,
    parameters: &GqlParameters,
) -> Result<Vec<Vec<WireValue>>, Failure> {
    match session.query(query_cx, text, parameters) {
        Ok(QueryResult::Rows { rows, .. }) => Ok(rows
            .iter()
            .map(|row| row.iter().map(convert::cell).collect())
            .collect()),
        Ok(QueryResult::Write { .. }) => Err(read_only()),
        Err(SessionError::Query(QueryError::Authorization(fgdb_warden::Error::ScopeDenied))) => {
            Ok(Vec::new())
        }
        Err(error) => Err(Failure::from_refusal(query_refusal(error))),
    }
}

fn unavailable_graph_value() -> Failure {
    Failure::new(
        "Neo.DatabaseError.Statement.ExecutionFailed",
        "graph value is unavailable in the selected snapshot",
    )
}

/// Hydrate only graph identities present in authorized output. Every lookup
/// uses this statement's session and effective cut; no database lock or raw
/// storage accessor is reacquired. Catalog-sized lookup batches retain the
/// existing native and signed per-execution limits.
fn hydrate(
    query_cx: &fgdb_types::QueryCx,
    session: &mut ReadSession<'_>,
    db: &Served,
    rows: &[Vec<WireValue>],
    at: CommitSeq,
) -> Result<Entities, Failure> {
    let mut vertices = BTreeSet::new();
    let mut edges = BTreeSet::new();
    for row in rows {
        for cell in row {
            collect_entities(cell, &mut vertices, &mut edges);
        }
    }
    if vertices.is_empty() && edges.is_empty() {
        return Ok(Entities::default());
    }
    let temporal = format!("FOR SYSTEM_TIME AS OF SEQ {}", at.0);
    let mut entities = Entities {
        nodes: vertices
            .iter()
            .map(|&id| (id, (Vec::new(), Vec::new())))
            .collect(),
        relationships: BTreeMap::new(),
    };
    let ids: Vec<u128> = vertices.into_iter().collect();
    for batch in ids.chunks(HYDRATION_BATCH) {
        let list: Vec<GraphValue> = batch
            .iter()
            .map(|&id| GraphValue::Vertex(VId(id)))
            .collect();
        let parameters = GqlParameters::new()
            .with_list("__fgdb_vertices", list)
            .map_err(|error| Failure::invalid(error.to_string()))?;
        for (label, _) in db.symbols.labels() {
            let text = format!(
                "UNWIND $__fgdb_vertices AS n MATCH (n:{}) {temporal} RETURN n",
                quoted(label)
            );
            for row in lookup(query_cx, session, &text, &parameters)? {
                if let Some(WireValue::Vertex(id)) = row.first()
                    && let Some((labels, _)) = entities.nodes.get_mut(id)
                {
                    labels.push(label.to_owned());
                }
            }
        }
        for (property, _) in db.symbols.properties() {
            let text = format!(
                "UNWIND $__fgdb_vertices AS n MATCH (n) {temporal} RETURN n, n.{} AS v",
                quoted(property)
            );
            for row in lookup(query_cx, session, &text, &parameters)? {
                let [WireValue::Vertex(id), cell] = row.as_slice() else {
                    continue;
                };
                if matches!(cell, WireValue::Null) {
                    continue;
                }
                let value = value(cell.clone(), &Entities::default())?;
                if let Some((_, properties)) = entities.nodes.get_mut(id) {
                    properties.push((property.to_owned(), value));
                }
            }
        }
    }
    let ids: Vec<u128> = edges.into_iter().collect();
    for batch in ids.chunks(HYDRATION_BATCH) {
        let parameters = GqlParameters::new()
            .with_list(
                "__fgdb_edges",
                batch.iter().map(|&id| GraphValue::Edge(EId(id))).collect(),
            )
            .map_err(|error| Failure::invalid(error.to_string()))?;
        // Identity-list membership belongs to the relational WITH filter;
        // the graph WHERE scalar compiler accepts property operands only.
        // This uses the existing native row algebra for full-width EIds.
        // Names come from exactly the declared catalog, as node labels do.
        // A typed source is rejected before expansion when its relation is
        // outside the capability, and no reverse catalog is guessed from IDs.
        for (kind, _) in db.symbols.relations() {
            let source = format!("MATCH (s)-[r:{}]->(t) {temporal}", quoted(kind));
            let text = format!("{source} WITH r, s, t WHERE r IN $__fgdb_edges RETURN r, s, t");
            let metadata = lookup(query_cx, session, &text, &parameters)?;
            if metadata.is_empty() {
                continue;
            }
            for row in metadata {
                let [
                    WireValue::Edge(id),
                    WireValue::Vertex(source),
                    WireValue::Vertex(target),
                ] = row.as_slice()
                else {
                    return Err(unavailable_graph_value());
                };
                if batch.binary_search(id).is_err() {
                    return Err(unavailable_graph_value());
                }
                entities.relationships.insert(
                    *id,
                    Relationship {
                        source: *source,
                        target: *target,
                        kind: kind.to_owned(),
                        properties: Vec::new(),
                    },
                );
            }
            for (property, _) in db.symbols.properties() {
                let text = format!(
                    "{source} WITH r, r.{} AS v WHERE r IN $__fgdb_edges RETURN r, v",
                    quoted(property)
                );
                for row in lookup(query_cx, session, &text, &parameters)? {
                    let [WireValue::Edge(id), cell] = row.as_slice() else {
                        return Err(unavailable_graph_value());
                    };
                    if matches!(cell, WireValue::Null) {
                        continue;
                    }
                    let value = value(cell.clone(), &Entities::default())?;
                    let relationship = entities
                        .relationships
                        .get_mut(id)
                        .ok_or_else(unavailable_graph_value)?;
                    relationship.properties.push((property.to_owned(), value));
                }
            }
        }
        if batch
            .iter()
            .any(|id| !entities.relationships.contains_key(id))
        {
            return Err(unavailable_graph_value());
        }
    }
    Ok(entities)
}

/// A schema name as a delimited identifier.
fn quoted(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

fn feature_not_supported(what: &str) -> Failure {
    Failure::new(
        "Neo.ClientError.Statement.FeatureNotSupported",
        format!("{what} values have no encoding in fgdbd's Bolt profile"),
    )
}

/// A local time's nanoseconds since midnight, below 86_400 * 10^9 < 2^47.
fn nanos_of_day(time: CanonicalLocalTime) -> i64 {
    i64::try_from(time.nanos_of_day()).expect("a time of day is under one day")
}

impl Entities {
    fn node(&self, id: u128) -> Result<Value, Failure> {
        let (labels, properties) = self.nodes.get(&id).ok_or_else(unavailable_graph_value)?;
        Ok(structure::node(
            i64::try_from(id).unwrap_or(-1),
            labels.clone(),
            properties.clone(),
            id.to_string(),
        ))
    }

    fn relationship(&self, id: u128) -> Result<Value, Failure> {
        let relationship = self
            .relationships
            .get(&id)
            .ok_or_else(unavailable_graph_value)?;
        Ok(structure::relationship(
            i64::try_from(id).unwrap_or(-1),
            i64::try_from(relationship.source).unwrap_or(-1),
            i64::try_from(relationship.target).unwrap_or(-1),
            relationship.kind.clone(),
            relationship.properties.clone(),
            [
                id.to_string(),
                relationship.source.to_string(),
                relationship.target.to_string(),
            ],
        ))
    }

    fn path(&self, start: u128, steps: &[(u128, u128)]) -> Result<Value, Failure> {
        let mut nodes = vec![self.node(start)?];
        let mut relationships = Vec::new();
        let mut node_indices = BTreeMap::from([(start, 0_i64)]);
        let mut relationship_indices = BTreeMap::new();
        let mut indices = Vec::with_capacity(steps.len().saturating_mul(2));
        let mut current = start;
        for &(edge, next) in steps {
            let relationship = self
                .relationships
                .get(&edge)
                .ok_or_else(unavailable_graph_value)?;
            let forward = if (relationship.source, relationship.target) == (current, next) {
                true
            } else if (relationship.source, relationship.target) == (next, current) {
                false
            } else {
                return Err(unavailable_graph_value());
            };
            let edge_index = if let Some(&index) = relationship_indices.get(&edge) {
                index
            } else {
                let index = i64::try_from(relationships.len())
                    .ok()
                    .and_then(|index| index.checked_add(1))
                    .ok_or_else(unavailable_graph_value)?;
                relationships.push(structure::unbound_relationship(
                    i64::try_from(edge).unwrap_or(-1),
                    relationship.kind.clone(),
                    relationship.properties.clone(),
                    edge.to_string(),
                ));
                relationship_indices.insert(edge, index);
                index
            };
            let node_index = if let Some(&index) = node_indices.get(&next) {
                index
            } else {
                let index = i64::try_from(nodes.len()).map_err(|_| unavailable_graph_value())?;
                nodes.push(self.node(next)?);
                node_indices.insert(next, index);
                index
            };
            indices.push(if forward { edge_index } else { -edge_index });
            indices.push(node_index);
            current = next;
        }
        Ok(structure::path(nodes, relationships, indices))
    }
}

/// One result cell as a Bolt value; graph identities cannot be emitted
/// without their authorized metadata, even inside nested collections.
fn value(cell: WireValue, entities: &Entities) -> Result<Value, Failure> {
    Ok(match cell {
        WireValue::Null => Value::Null,
        WireValue::Bool(value) => Value::Bool(value),
        WireValue::Int(value) => Value::Int(value),
        WireValue::Float(value) => Value::Float(value),
        WireValue::Decimal(text) => Value::String(text),
        WireValue::Text(text) => Value::String(text),
        WireValue::Date(text) => structure::date(
            CanonicalDate::parse_iso(&text)
                .map_err(|_| feature_not_supported("A non-canonical date"))?
                .epoch_day(),
        ),
        WireValue::LocalTime(text) => structure::local_time(nanos_of_day(
            CanonicalLocalTime::parse_iso(&text)
                .map_err(|_| feature_not_supported("A non-canonical local time"))?,
        )),
        WireValue::Time(text) => {
            let time = CanonicalTime::parse_iso(&text)
                .map_err(|_| feature_not_supported("A non-canonical time"))?;
            structure::time(nanos_of_day(time.local()), i64::from(time.offset_seconds()))
        }
        WireValue::LocalDateTime(text) => {
            let value = CanonicalLocalDateTime::parse_iso(&text)
                .map_err(|_| feature_not_supported("A non-canonical local date-time"))?;
            let nanos = nanos_of_day(value.time());
            // |epoch day| < 4e11, so the seconds fit an i64 with room to spare.
            structure::local_date_time(
                value.date().epoch_day() * 86_400 + nanos / 1_000_000_000,
                nanos % 1_000_000_000,
            )
        }
        WireValue::Duration(text) => {
            let value = CanonicalDuration::parse_iso(&text)
                .map_err(|_| feature_not_supported("A non-canonical duration"))?;
            let second = 1_000_000_000_i128;
            // The time part is bounded by an i64 of whole seconds.
            let seconds = i64::try_from(value.nanos().div_euclid(second))
                .map_err(|_| feature_not_supported("An out-of-range duration"))?;
            let nanos = i64::try_from(value.nanos().rem_euclid(second))
                .expect("a remainder of one second fits an i64");
            structure::duration(value.months(), value.days(), seconds, nanos)
        }
        WireValue::Bytes(bytes) => Value::Bytes(bytes),
        WireValue::Timestamp(timestamp) => {
            let seconds = timestamp.instant_utc_nanos.div_euclid(1_000_000_000);
            let nanos = timestamp.instant_utc_nanos.rem_euclid(1_000_000_000);
            let seconds = i64::try_from(seconds)
                .map_err(|_| feature_not_supported("Out-of-range date-time"))?;
            match timestamp.zone {
                Some(zone) => structure::date_time_zone(seconds, nanos as i64, zone.identifier),
                None => structure::date_time(
                    seconds,
                    nanos as i64,
                    i64::from(timestamp.utc_offset_seconds),
                ),
            }
        }
        WireValue::Vertex(id) => entities.node(id)?,
        WireValue::Vertices(ids) => Value::List(
            ids.into_iter()
                .map(|id| entities.node(id))
                .collect::<Result<_, _>>()?,
        ),
        WireValue::Edge(id) => entities.relationship(id)?,
        WireValue::Edges(ids) => Value::List(
            ids.into_iter()
                .map(|id| entities.relationship(id))
                .collect::<Result<_, _>>()?,
        ),
        WireValue::Path { start, steps } => entities.path(start, &steps)?,
        WireValue::List(items) => Value::List(
            items
                .into_iter()
                .map(|item| value(item, entities))
                .collect::<Result<_, _>>()?,
        ),
        WireValue::Map(entries) => Value::Map(
            entries
                .into_iter()
                .map(|(key, item)| Ok((key, value(item, entities)?)))
                .collect::<Result<_, Failure>>()?,
        ),
        WireValue::Count(count) => {
            i64::try_from(count).map_or_else(|_| Value::String(count.to_string()), Value::Int)
        }
        WireValue::WideInt(value) => {
            i64::try_from(value).map_or_else(|_| Value::String(value.to_string()), Value::Int)
        }
        // Neo4j's avg is a float; the exact rational is narrowed only here.
        WireValue::Average {
            numerator,
            denominator,
        } => Value::Float(numerator as f64 / denominator as f64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute::transaction::TransactionCatalog;
    use fgdb_gql::{GraphSymbolKind, GraphSymbolResolver, ReverseSymbolCatalog};

    // Existing authority laws use the same production writer without a
    // database generation; lifecycle laws supply one explicitly below.
    async fn flush_output<W: AsyncWrite + Unpin>(
        stream: &mut W,
        cx: &Cx,
        out: &mut Vec<u8>,
        failed: &mut bool,
        authority: Option<(&Authority, &CapabilityToken)>,
        clock: impl FnMut() -> u64,
    ) -> Result<(), Closed> {
        flush_generation_output(stream, cx, out, failed, authority, None, clock).await
    }

    fn graph_entities() -> Entities {
        Entities {
            nodes: [42, 69, 1]
                .into_iter()
                .map(|id| {
                    (
                        id,
                        (
                            vec!["Person".to_owned()],
                            vec![("name".to_owned(), Value::string(id.to_string()))],
                        ),
                    )
                })
                .collect(),
            relationships: [
                (1000, 42, 69, "KNOWS"),
                (1001, 1, 42, "WORKS_AT"),
                (1002, 42, 42, "KNOWS"),
                (u128::MAX, 42, 69, "KNOWS"),
            ]
            .into_iter()
            .map(|(id, source, target, kind)| {
                (
                    id,
                    Relationship {
                        source,
                        target,
                        kind: kind.to_owned(),
                        properties: vec![("age".to_owned(), Value::Int(7))],
                    },
                )
            })
            .collect(),
        }
    }

    /// fgdb-txbg9: each temporal kind is its Bolt structure in both
    /// directions, through the same canonical text the FGP cell carries.
    #[test]
    fn temporal_values_are_their_bolt_structures_both_ways() {
        let none = Entities::default();
        for (cell, tag, fields) in [
            (WireValue::Date("-0001-07-21".into()), 0x44, vec![-719_692]),
            (
                WireValue::LocalTime("21:40:32.142".into()),
                0x74,
                vec![78_032_142_000_000],
            ),
            (
                WireValue::Time("00:00:01-02:05:07".into()),
                0x54,
                vec![1_000_000_000, -7_507],
            ),
            (
                WireValue::LocalDateTime("1969-12-31T23:59:59.999999999".into()),
                0x64,
                vec![-1, 999_999_999],
            ),
            (
                WireValue::LocalDateTime("2015-07-21T21:40".into()),
                0x64,
                vec![1_437_514_800, 0],
            ),
            (
                WireValue::Duration("P1Y-2DT-1.999S".into()),
                0x45,
                vec![12, -2, -2, 1_000_000],
            ),
        ] {
            let encoded = Value::Struct {
                tag,
                fields: fields.into_iter().map(Value::Int).collect(),
            };
            assert_eq!(value(cell.clone(), &none).unwrap(), encoded, "{cell:?}");
            assert_eq!(argument(&encoded).unwrap(), cell, "{cell:?}");
        }
        for malformed in [
            Value::Struct {
                tag: 0x74,
                fields: vec![Value::Int(86_400_000_000_000)],
            },
            Value::Struct {
                tag: 0x54,
                fields: vec![Value::Int(0), Value::Int(18 * 3_600 + 1)],
            },
            Value::Struct {
                tag: 0x64,
                fields: vec![Value::Int(0), Value::Int(1_000_000_000)],
            },
            Value::Struct {
                tag: 0x64,
                fields: vec![Value::Int(0)],
            },
            Value::Struct {
                tag: 0x45,
                fields: vec![Value::Int(0), Value::Int(0), Value::Int(0)],
            },
        ] {
            assert!(argument(&malformed).is_err(), "{malformed:?}");
        }
    }

    #[test]
    fn relationship_structures_keep_orientation_properties_and_full_element_ids() {
        let entities = graph_entities();
        for id in [1000, u128::MAX] {
            let encoded = value(WireValue::Edge(id), &entities).unwrap();
            let Value::Struct { tag, fields } = &encoded else {
                panic!("relationship structure");
            };
            assert_eq!(*tag, 0x52);
            assert_eq!(fields.len(), 8);
            assert_eq!(fields[0], Value::Int(i64::try_from(id).unwrap_or(-1)));
            assert_eq!(fields[1], Value::Int(42));
            assert_eq!(fields[2], Value::Int(69));
            assert_eq!(fields[3], Value::string("KNOWS"));
            assert_eq!(
                fields[4],
                Value::Map(vec![("age".to_owned(), Value::Int(7))])
            );
            assert_eq!(fields[5], Value::string(id.to_string()));
            assert_eq!(fields[6], Value::string("42"));
            assert_eq!(fields[7], Value::string("69"));
            let mut bytes = Vec::new();
            fgdb_bolt::packstream::encode(&encoded, &mut bytes);
            assert_eq!(&bytes[..2], &[0xB8, 0x52]);
            assert_eq!(fgdb_bolt::packstream::decode(&bytes).unwrap(), encoded);
        }
    }

    #[test]
    fn paths_encode_reverse_traversal_repeated_elements_and_zero_hops() {
        let entities = graph_entities();
        let path = WireValue::Path {
            start: 42,
            steps: vec![(1000, 69), (1000, 42), (1001, 1)],
        };
        let encoded = value(path, &entities).unwrap();
        let Value::Struct { tag, fields } = &encoded else {
            panic!("path structure");
        };
        assert_eq!(*tag, 0x50);
        assert_eq!(fields.len(), 3);
        let Value::List(nodes) = &fields[0] else {
            panic!("path nodes");
        };
        assert_eq!(
            nodes,
            &[
                entities.node(42).unwrap(),
                entities.node(69).unwrap(),
                entities.node(1).unwrap()
            ]
        );
        let Value::List(relationships) = &fields[1] else {
            panic!("path relationships");
        };
        assert_eq!(relationships.len(), 2);
        for (relationship, expected) in relationships.iter().zip([1000, 1001]) {
            let Value::Struct { tag, fields } = relationship else {
                panic!("unbound relationship");
            };
            assert_eq!(*tag, 0x72);
            assert_eq!(fields.len(), 4);
            assert_eq!(fields[0], Value::Int(expected));
            assert_eq!(fields[3], Value::string(expected.to_string()));
        }
        assert_eq!(
            fields[2],
            Value::List([1, 1, -1, 0, -2, 2].into_iter().map(Value::Int).collect())
        );
        let mut bytes = Vec::new();
        fgdb_bolt::packstream::encode(&encoded, &mut bytes);
        assert_eq!(&bytes[..2], &[0xB3, 0x50]);
        assert_eq!(fgdb_bolt::packstream::decode(&bytes).unwrap(), encoded);

        assert_eq!(
            entities.path(42, &[]).unwrap(),
            Value::Struct {
                tag: 0x50,
                fields: vec![
                    Value::List(vec![entities.node(42).unwrap()]),
                    Value::List(Vec::new()),
                    Value::List(Vec::new()),
                ],
            }
        );
        let Value::Struct { fields, .. } = entities.path(42, &[(1002, 42)]).unwrap() else {
            panic!("self-loop path");
        };
        assert_eq!(fields[2], Value::List(vec![Value::Int(1), Value::Int(0)]));
        for steps in [vec![(9999, 69)], vec![(1000, 1)]] {
            assert!(entities.path(42, &steps).is_err());
        }
        assert!(entities.node(9999).is_err());
        assert!(entities.relationship(9999).is_err());
    }

    #[test]
    fn nested_graph_values_collect_and_encode_every_path_member() {
        let path = WireValue::Path {
            start: 42,
            steps: vec![(1000, 69)],
        };
        let nested = WireValue::Map(vec![
            ("edges".to_owned(), WireValue::Edges(vec![1000, 1000])),
            ("paths".to_owned(), WireValue::List(vec![path])),
            ("vertices".to_owned(), WireValue::Vertices(vec![1, 42])),
        ]);
        let mut vertices = BTreeSet::new();
        let mut edges = BTreeSet::new();
        collect_entities(&nested, &mut vertices, &mut edges);
        assert_eq!(vertices, BTreeSet::from([1, 42, 69]));
        assert_eq!(edges, BTreeSet::from([1000]));
        let entities = graph_entities();
        let Value::Map(entries) = value(nested, &entities).unwrap() else {
            panic!("nested graph values");
        };
        assert_eq!(
            get(&entries, "edges"),
            Some(&Value::List(vec![
                entities.relationship(1000).unwrap(),
                entities.relationship(1000).unwrap(),
            ]))
        );
        assert_eq!(
            get(&entries, "paths"),
            Some(&Value::List(vec![
                entities.path(42, &[(1000, 69)]).unwrap()
            ]))
        );
    }

    #[test]
    fn graph_metadata_lookup_shapes_bind_through_the_native_temporal_compiler() {
        let mut symbols = crate::Symbols::new();
        symbols
            .bind(fgdb_gql::GraphSymbolKind::Label, "Person", 1)
            .unwrap();
        symbols
            .bind(fgdb_gql::GraphSymbolKind::Relation, "KNOWS", 1)
            .unwrap();
        symbols
            .bind(fgdb_gql::GraphSymbolKind::Property, "name", 1)
            .unwrap();
        let vertices = GqlParameters::new()
            .with_list("__fgdb_vertices", vec![GraphValue::Vertex(VId(1))])
            .unwrap();
        let edges = GqlParameters::new()
            .with_list("__fgdb_edges", vec![GraphValue::Edge(EId(1))])
            .unwrap();
        for (text, parameters) in [
            (
                "UNWIND $__fgdb_vertices AS n MATCH (n:`Person`) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n",
                &vertices,
            ),
            (
                "UNWIND $__fgdb_vertices AS n MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n, n.`name` AS v",
                &vertices,
            ),
            (
                "MATCH (s)-[r:`KNOWS`]->(t) FOR SYSTEM_TIME AS OF SEQ 1 WITH r, s, t WHERE r IN $__fgdb_edges RETURN r, s, t",
                &edges,
            ),
            (
                "MATCH (s)-[r:`KNOWS`]->(t) FOR SYSTEM_TIME AS OF SEQ 1 WITH r, r.`name` AS v WHERE r IN $__fgdb_edges RETURN r, v",
                &edges,
            ),
        ] {
            let prepared = PreparedNativeRead::prepare(text, parameters, symbols.clone())
                .unwrap_or_else(|error| panic!("{text}: {error}"));
            let as_of = match prepared {
                PreparedNativeRead::TemporalPattern(prepared) => prepared
                    .bind_parameters(parameters)
                    .unwrap_or_else(|error| panic!("{text}: {error}"))
                    .as_of(),
                PreparedNativeRead::TemporalSet(prepared) => prepared
                    .bind_parameters(parameters)
                    .unwrap_or_else(|error| panic!("{text}: {error}"))
                    .as_of(),
                _ => panic!("metadata query must retain its exact snapshot: {text}"),
            };
            assert_eq!(as_of, CommitSeq(1));
        }
    }

    #[test]
    fn schema_procedures_match_their_plain_forms_only() {
        assert!(matches!(
            schema_procedure("CALL db.labels()"),
            Some(("label", SchemaKind::Labels))
        ));
        assert!(matches!(
            schema_procedure("  call DB.relationshipTypes()  YIELD relationshipType ;"),
            Some(("relationshipType", SchemaKind::Relations))
        ));
        assert!(matches!(
            schema_procedure("CALL db.propertyKeys() YIELD propertyKey"),
            Some(("propertyKey", SchemaKind::Properties))
        ));
        assert!(schema_procedure("CALL db.labels() YIELD x").is_none());
        assert!(schema_procedure("CALL db.labels() YIELD label RETURN label").is_none());
        assert!(schema_procedure("CALL db.indexes()").is_none());
        assert!(schema_procedure("MATCH (n) RETURN n").is_none());
    }

    #[test]
    fn write_keywords_are_found_outside_strings_comments_and_property_names() {
        assert!(looks_like_write("CREATE (n:Person {name: 'x'})"));
        assert!(looks_like_write("match (n) detach delete n"));
        assert!(looks_like_write("MATCH (n) SET n.x = 1"));
        assert!(!looks_like_write(
            "MATCH (n) WHERE n.name = 'CREATE' RETURN n"
        ));
        assert!(!looks_like_write(
            "MATCH (n) RETURN n.set, n.`delete` // CREATE"
        ));
        assert!(!looks_like_write("MATCH (n:Person) RETURN n.name"));
    }

    use asupersync::security::key::AuthKey;
    use asupersync::{Budget, runtime::RuntimeBuilder};
    use fgdb_delta_types::SchemaEpoch;
    use fgdb_types::DatabaseSecurityNamespaceId;
    use fgdb_warden::{Grant, QueryLimits};
    use std::cell::{Cell, RefCell};
    use std::future::Future;
    use std::io;
    use std::rc::Rc;
    use std::task::{Context, Waker};

    const START: u64 = 100;
    const EXPIRES: u64 = 1000;

    fn authority(seed: u64) -> Authority {
        Authority::new(
            AuthKey::from_seed(seed),
            DatabaseSecurityNamespaceId([7; 32]),
            crate::GRAPH_NAME,
            SchemaEpoch(0),
            1,
        )
        .unwrap()
    }

    fn token(issuer: &Authority) -> CapabilityToken {
        issuer
            .issue_at(
                &Grant::read_only(
                    crate::TRUNK,
                    EXPIRES,
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

    #[test]
    fn transaction_catalog_hidden_growth_preserves_visible_output_and_signed_work() {
        use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
        use fgdb_warden::{PlannerPredicates, Rights, Scope};

        fn observe(
            symbols: &crate::Symbols,
            scope: &PlannerPredicates,
        ) -> (ReverseSymbolCatalog, u64, usize) {
            let mut units = 0;
            let mut polls = 0;
            let visible = {
                let mut work = |bill| {
                    units += bill;
                    polls += 1;
                    Ok(())
                };
                let catalog = TransactionCatalog::new(symbols, scope, &mut work);
                let visible = (&catalog).reverse_catalog().unwrap();
                catalog.check().map_err(Failure::from_refusal).unwrap();
                visible
            };
            (visible, units, polls)
        }

        let issuer = authority(0xb017_ca7a);
        let token = issuer
            .issue_at(
                &Grant {
                    branch: crate::TRUNK.into(),
                    labels: Scope::only([LabelId(1), LabelId(2)]),
                    relations: Scope::only([RelationId(1), RelationId(2)]),
                    properties: Scope::only([PropertyKeyId(1), PropertyKeyId(2)]),
                    rights: Rights::ReadWrite,
                    limits: QueryLimits {
                        max_nodes: 100,
                        max_work: 1000,
                        max_rows: 100,
                    },
                    expires_at_ms: EXPIRES,
                },
                START,
            )
            .unwrap();
        let verified = issuer.verify_at(&token, crate::TRUNK, START).unwrap();
        let scope = verified.predicates();
        let kinds = [
            GraphSymbolKind::Label,
            GraphSymbolKind::Relation,
            GraphSymbolKind::Property,
        ];
        let mut symbols = crate::Symbols::new();
        for kind in kinds {
            symbols.bind(kind, "visible", 1).unwrap();
        }
        let (before, before_units, before_polls) = observe(&symbols, scope);
        assert_eq!(before.labels[&LabelId(1)], "visible");
        assert_eq!(before.relations[&RelationId(1)], "visible");
        assert_eq!(before.properties[&PropertyKeyId(1)], "visible");
        for id in 3..131 {
            for kind in kinds {
                symbols
                    .bind(kind, &format!("hidden_{id}_{}", "x".repeat(96)), id)
                    .unwrap();
            }
        }
        let (hidden, hidden_units, hidden_polls) = observe(&symbols, scope);
        assert_eq!(hidden, before);
        assert_eq!(hidden_units, before_units);
        assert!(
            hidden_polls > before_polls,
            "hidden traversal must still poll live controls"
        );
        for kind in kinds {
            symbols.bind(kind, "also_visible", 2).unwrap();
        }
        let (expanded, expanded_units, _) = observe(&symbols, scope);
        assert_eq!(expanded.labels.len(), 2);
        assert_eq!(expanded.relations.len(), 2);
        assert_eq!(expanded.properties.len(), 2);
        assert!(
            expanded_units > hidden_units,
            "permitted catalog copies must spend signed work"
        );

        // A zero-cost hidden-entry poll is still a live control boundary.
        // Its refusal latches and prevents both output and any later callback.
        let calls = Cell::new(0);
        let polls = Cell::new(0);
        let mut work = |units| {
            calls.set(calls.get() + 1);
            if units == 0 {
                polls.set(polls.get() + 1);
                if polls.get() == 3 {
                    // The first two label IDs are visible; the third is hidden.
                    return Err(fgdb::WriteTxnError::AuthorizedMutationRefused);
                }
            }
            Ok(())
        };
        let catalog = TransactionCatalog::new(&symbols, scope, &mut work);
        assert!((&catalog).reverse_catalog().is_none());
        let stopped = calls.get();
        assert!(catalog.check().is_err());
        assert!((&catalog).reverse_catalog().is_none());
        assert_eq!(calls.get(), stopped);
    }

    fn framed_output() -> Vec<u8> {
        let mut bytes = Vec::new();
        Response::Record(vec![Value::string("protected"), Value::Int(17)]).frame(&mut bytes);
        Response::Success(vec![("has_more".to_owned(), Value::Bool(false))]).frame(&mut bytes);
        bytes
    }

    struct Writer {
        accepted: Rc<RefCell<Vec<u8>>>,
        chunks: VecDeque<usize>,
        flush_pending: bool,
        flushes: Rc<Cell<usize>>,
    }

    impl AsyncWrite for Writer {
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
            self.accepted
                .borrow_mut()
                .extend_from_slice(&bytes[..count]);
            Poll::Ready(Ok(count))
        }

        fn poll_flush(mut self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.flushes.set(self.flushes.get() + 1);
            if self.flush_pending {
                self.flush_pending = false;
                task.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(Ok(()))
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    struct Fixture {
        writer: Writer,
        accepted: Rc<RefCell<Vec<u8>>>,
        flushes: Rc<Cell<usize>>,
    }

    impl Fixture {
        fn new(chunks: impl IntoIterator<Item = usize>, flush_pending: bool) -> Self {
            let accepted = Rc::new(RefCell::new(Vec::new()));
            let flushes = Rc::new(Cell::new(0));
            Self {
                writer: Writer {
                    accepted: Rc::clone(&accepted),
                    chunks: chunks.into_iter().collect(),
                    flush_pending,
                    flushes: Rc::clone(&flushes),
                },
                accepted,
                flushes,
            }
        }
    }

    fn poll<T>(future: Pin<&mut impl Future<Output = T>>) -> Poll<T> {
        future.poll(&mut Context::from_waker(Waker::noop()))
    }

    fn with_cx(test: impl FnOnce(&Cx)) {
        let runtime = RuntimeBuilder::new().build().unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        test(&cx);
    }

    #[test]
    fn guarded_bolt_flush_delivers_exact_frames_through_partial_writes() {
        with_cx(|cx| {
            let issuer = authority(41);
            let credential = token(&issuer);
            let mut io = Fixture::new([3, 0, 5, 0], true);
            let mut out = framed_output();
            let expected = out.clone();
            let mut failed = false;
            let mut flush = Box::pin(flush_output(
                &mut io.writer,
                cx,
                &mut out,
                &mut failed,
                Some((&issuer, &credential)),
                || EXPIRES - 1,
            ));
            let mut completed = false;
            for _ in 0..8 {
                if let Poll::Ready(result) = poll(flush.as_mut()) {
                    assert_eq!(result, Ok(()));
                    completed = true;
                    break;
                }
            }
            assert!(completed);
            drop(flush);
            assert!(!failed);
            assert!(out.is_empty());
            assert_eq!(*io.accepted.borrow(), expected);
            assert_eq!(io.flushes.get(), 2);
        });
    }

    #[test]
    fn delayed_bolt_output_refuses_at_exact_expiry_without_one_byte() {
        with_cx(|cx| {
            let issuer = authority(41);
            let credential = token(&issuer);
            let mut io = Fixture::new([], false);
            let mut out = framed_output();
            let expected = out.clone();
            let mut failed = false;
            let mut flush = Box::pin(flush_output(
                &mut io.writer,
                cx,
                &mut out,
                &mut failed,
                Some((&issuer, &credential)),
                || EXPIRES,
            ));
            assert_eq!(poll(flush.as_mut()), Poll::Ready(Err(Closed)));
            drop(flush);
            assert!(failed);
            assert_eq!(out, expected);
            assert!(io.accepted.borrow().is_empty());
            assert_eq!(io.flushes.get(), 0);
        });
    }

    #[test]
    fn expiry_after_a_partial_record_cannot_send_a_suffix_or_later_failure() {
        with_cx(|cx| {
            let issuer = authority(41);
            let credential = token(&issuer);
            let mut io = Fixture::new([7, 0], false);
            let mut out = framed_output();
            let mut failed = false;
            let now = Cell::new(START);
            let mut flush = Box::pin(flush_output(
                &mut io.writer,
                cx,
                &mut out,
                &mut failed,
                Some((&issuer, &credential)),
                || now.get(),
            ));
            assert!(poll(flush.as_mut()).is_pending());
            let prefix = io.accepted.borrow().clone();
            assert_eq!(prefix.len(), 7);
            now.set(EXPIRES);
            assert_eq!(poll(flush.as_mut()), Poll::Ready(Err(Closed)));
            drop(flush);
            assert!(failed);
            // handle() may construct an error after a failed batch flush. The
            // terminal lane must refuse that outer flush without restarting
            // the original partial record, even under a valid clock sample.
            Response::Failure {
                code: "Neo.ClientError.Security.Unauthorized".to_owned(),
                message: "credential not accepted".to_owned(),
            }
            .frame(&mut out);
            let mut retry = Box::pin(flush_output(
                &mut io.writer,
                cx,
                &mut out,
                &mut failed,
                Some((&issuer, &credential)),
                || START,
            ));
            assert_eq!(poll(retry.as_mut()), Poll::Ready(Err(Closed)));
            drop(retry);
            assert_eq!(*io.accepted.borrow(), prefix);
            assert_eq!(io.flushes.get(), 0);
        });
    }

    #[test]
    fn bolt_pending_flush_rechecks_both_expiry_and_issuer_retirement() {
        with_cx(|cx| {
            for retire in [false, true] {
                let issuer = authority(41);
                let credential = token(&issuer);
                let mut io = Fixture::new([], true);
                let mut out = framed_output();
                let expected = out.clone();
                let mut failed = false;
                let now = Cell::new(START);
                let mut flush = Box::pin(flush_output(
                    &mut io.writer,
                    cx,
                    &mut out,
                    &mut failed,
                    Some((&issuer, &credential)),
                    || now.get(),
                ));
                assert!(poll(flush.as_mut()).is_pending());
                assert_eq!(*io.accepted.borrow(), expected);
                assert_eq!(io.flushes.get(), 1);
                if retire {
                    issuer.retire();
                } else {
                    now.set(EXPIRES);
                }
                assert_eq!(poll(flush.as_mut()), Poll::Ready(Err(Closed)));
                drop(flush);
                assert!(failed);
                assert_eq!(out, expected);
                assert_eq!(io.flushes.get(), 1, "no flush after invalidation");
            }
        });
    }

    #[test]
    fn cancelling_a_partial_bolt_flush_permanently_closes_its_buffer() {
        with_cx(|cx| {
            let issuer = authority(41);
            let credential = token(&issuer);
            let mut io = Fixture::new([5, 0], false);
            let mut out = framed_output();
            let mut failed = false;
            let mut flush = Box::pin(flush_output(
                &mut io.writer,
                cx,
                &mut out,
                &mut failed,
                Some((&issuer, &credential)),
                || START,
            ));
            assert!(poll(flush.as_mut()).is_pending());
            let prefix = io.accepted.borrow().clone();
            assert_eq!(prefix.len(), 5);
            drop(flush);
            assert!(failed);
            let mut retry = Box::pin(flush_output(
                &mut io.writer,
                cx,
                &mut out,
                &mut failed,
                Some((&issuer, &credential)),
                || START,
            ));
            assert_eq!(poll(retry.as_mut()), Poll::Ready(Err(Closed)));
            drop(retry);
            assert_eq!(*io.accepted.borrow(), prefix);
            assert_eq!(io.flushes.get(), 0);
        });
    }

    #[test]
    fn foreign_issuer_cannot_flush_a_saved_result() {
        with_cx(|cx| {
            let issuer = authority(41);
            let credential = token(&issuer);
            let foreign = authority(42);
            let mut io = Fixture::new([], false);
            let mut out = framed_output();
            let mut failed = false;
            let mut flush = Box::pin(flush_output(
                &mut io.writer,
                cx,
                &mut out,
                &mut failed,
                Some((&foreign, &credential)),
                || START,
            ));
            assert_eq!(poll(flush.as_mut()), Poll::Ready(Err(Closed)));
            drop(flush);
            assert!(failed);
            assert!(io.accepted.borrow().is_empty());
        });
    }
}
