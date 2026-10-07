//! The Bolt-compat adapter: `BoltCompatProfileV1` (plan §13.5) over TCP.
//!
//! Official Neo4j drivers connect with `bolt://` (or `neo4j://`, answered
//! by a single-server routing table) and run read-only Cypher/GQL. The
//! profile is a negotiated downgrade, never a drop-in-Neo4j claim:
//!
//! - Authentication is a Warden capability token in HELLO: the hex token as
//!   the `bearer` credential, or as the password of `basic` auth. A HELLO
//!   whose token no served database accepts is refused.
//! - Every RUN executes on a capability-authorized READ session, the same
//!   path as FGP and HTTP reads. A read session cannot express a write, so a
//!   mutating statement is refused before any graph access, with
//!   `Neo.ClientError.Statement.AccessMode`. Writes go over FGP or HTTP.
//! - An explicit transaction holds one read session from BEGIN to COMMIT,
//!   so all of its statements read one pinned generation; autocommit RUNs
//!   each pin their own. No lock is held across network round trips.
//! - The database is RUN/BEGIN's `db`, or the only served database.
//! - Vertices come back as Bolt nodes with their labels and properties,
//!   read through the same session (so capability masking applies): each
//!   label and property binding the server was given is looked up for the
//!   returned vertices. Relationships and paths have no Bolt encoding here
//!   and refuse with `Neo.ClientError.Statement.FeatureNotSupported`;
//!   return `type(r)`, `r.prop` or the endpoints instead.
//! - `CALL db.labels()`, `db.relationshipTypes()` and `db.propertyKeys()`
//!   answer the schema names the token may see (the server's bindings,
//!   scope-filtered), the same answer as HTTP's schema route.
//! - Results are the ephemeral class of FGP's snapshot stream: buffered per
//!   statement and dropped on disconnect, DISCARD or RESET. The exact selected
//!   issuer's expiry/retirement fence is rechecked before every socket write
//!   and flush, including delayed PULLs and partial writes. This is the existing
//!   cooperative Warden fence, not durable audit or revocation evidence.

use crate::execute::{ReadSession, Refusal, query_refusal, read_session};
use crate::shutdown::Waiter;
use crate::{Served, Server, convert};
use asupersync::Cx;
use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
use core::future::poll_fn;
use core::pin::Pin;
use core::task::Poll;
use fgdb::{QueryError, QueryResult};
use fgdb_bolt::message::{
    Dechunker, MAGIC, Map, Request, Response, decode_request, negotiate, structure,
};
use fgdb_bolt::packstream::{Value, get};
use fgdb_gql::GqlParameters;
use fgdb_gql::algebra::GraphValue;
use fgdb_protocol::body::{ErrorCode, WireTimestamp, WireValue};
use fgdb_protocol::transport::DuplexIo;
use fgdb_types::{CommitSeq, PurposeContexts, VId};
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

/// Buffered socket I/O. Reads stop at the drain signal while idle.
struct Io {
    stream: Box<dyn DuplexIo>,
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
    ) -> Result<(), Closed> {
        flush_output(
            &mut self.stream,
            cx,
            &mut self.out,
            &mut self.failed,
            authority,
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
async fn flush_output<W: AsyncWrite + Unpin>(
    stream: &mut W,
    cx: &Cx,
    out: &mut Vec<u8>,
    failed: &mut bool,
    authority: Option<(&Authority, &CapabilityToken)>,
    clock: impl FnMut() -> u64,
) -> Result<(), Closed> {
    if *failed {
        return Err(Closed);
    }
    *failed = true;
    write_output(stream, cx, out, authority, clock).await?;
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
    let mut written = 0;
    while written < bytes.len() {
        let count = poll_fn(|task| {
            if authorize().is_err() {
                return Poll::Ready(Err(Closed));
            }
            let remaining = &bytes[written..];
            match Pin::new(&mut *stream).poll_write(task, remaining) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Ok(count)) if count > 0 && count <= remaining.len() => {
                    Poll::Ready(Ok(count))
                }
                Poll::Ready(_) => Poll::Ready(Err(Closed)),
            }
        })
        .await?;
        written += count;
    }
    poll_fn(|task| {
        if authorize().is_err() {
            return Poll::Ready(Err(Closed));
        }
        match Pin::new(&mut *stream).poll_flush(task) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => Poll::Ready(result.map_err(|_| Closed)),
        }
    })
    .await
}

/// A Bolt-visible refusal: a Neo4j status code and a message.
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
            ErrorCode::Busy | ErrorCode::Draining => {
                "Neo.TransientError.General.DatabaseUnavailable"
            }
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
}

/// An explicit transaction: one read session, one pinned generation.
struct Transaction<'s> {
    name: String,
    db: &'s Served,
    session: ReadSession<'s>,
    seq: CommitSeq,
    statements: i64,
}

/// Serve one Bolt connection until GOODBYE, disconnect or drain.
pub(crate) async fn run(cx: &Cx, server: &Server, stream: Box<dyn DuplexIo>) {
    let waiter = server.shutdown.waiter();
    let mut io = Io {
        stream,
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
        let _ = io.flush(cx, None).await;
        return;
    };
    io.out.extend_from_slice(&version.response());
    if io.flush(cx, None).await.is_err() {
        return;
    }
    let mut connection = Connection {
        server,
        token: None,
        failed: false,
        pending: None,
        transaction: None,
        output_database: None,
        id: CONNECTIONS.fetch_add(1, Ordering::Relaxed),
    };
    while let Ok(message) = io.next_message(cx, &waiter).await {
        let request = match decode_request(&message) {
            Ok(request) => request,
            Err(error) => {
                // An undecodable message leaves the stream state unknown.
                io.respond(&failure(Failure::invalid(error.to_string())));
                let _ = io.flush(cx, connection.output_authority()).await;
                return;
            }
        };
        let close = connection.handle(cx, &mut io, request).await;
        if io.flush(cx, connection.output_authority()).await.is_err() || close {
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
        match request {
            Request::Goodbye => return true,
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
            Request::Pull { n, .. } => self.pull(cx, io, n, false).await,
            Request::Discard { n, .. } => self.pull(cx, io, n, true).await,
            Request::Begin { extra } => self.begin(cx, &extra).await,
            Request::Commit => match self.transaction.take() {
                Some(transaction) => {
                    self.output_database = Some(transaction.db);
                    self.pending = None;
                    Ok(vec![(
                        "bookmark".to_owned(),
                        Value::string(bookmark(&transaction.name, transaction.seq)),
                    )])
                }
                None => Err(Failure::invalid("COMMIT outside a transaction")),
            },
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
        let (session, seq) = read_session(cx, db, token)
            .await
            .map_err(Failure::from_refusal)?;
        self.transaction = Some(Transaction {
            name,
            db,
            session,
            seq,
            statements: 0,
        });
        Ok(Vec::new())
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
        if let Some((column, kind)) = schema_procedure(query) {
            return self.schema_rows(extra, column, kind);
        }
        let parameters = statement_parameters(parameters)?;
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        let query_cx = contexts.query();
        // An autocommit statement pins its own generation; one inside a
        // transaction reads the transaction's.
        let mut autocommit = None;
        let (name, db, seq, qid) = if let Some(transaction) = self.transaction.as_mut() {
            transaction.statements += 1;
            (
                transaction.name.clone(),
                transaction.db,
                transaction.seq,
                Some(transaction.statements - 1),
            )
        } else {
            let (name, db) = self.database(extra)?;
            self.output_database = Some(db);
            let token = self.token.as_ref().expect("authenticated");
            let (session, seq) = read_session(cx, db, token)
                .await
                .map_err(Failure::from_refusal)?;
            autocommit = Some(session);
            (name, db, seq, None)
        };
        self.output_database = Some(db);
        let session = match autocommit.as_mut() {
            Some(session) => session,
            None => &mut self.transaction.as_mut().expect("in a transaction").session,
        };
        let (columns, rows) = match session.query(&query_cx, query, &parameters) {
            Ok(QueryResult::Rows { columns, rows }) => (
                columns,
                rows.iter()
                    .map(|row| row.iter().map(convert::cell).collect::<Vec<_>>())
                    .collect::<Vec<_>>(),
            ),
            Ok(QueryResult::Write { .. }) => return Err(read_only()),
            Err(error) => {
                return Err(if looks_like_write(query) {
                    read_only()
                } else {
                    Failure::from_refusal(query_refusal(error))
                });
            }
        };
        let nodes = hydrate(&query_cx, session, db, &rows)?;
        let rows = rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|cell| value(cell, &nodes))
                    .collect::<Result<Vec<_>, _>>()
            })
            .collect::<Result<VecDeque<_>, _>>()?;
        self.pending = Some(Pending {
            rows,
            database: name,
            db,
            seq: Some(seq),
        });
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

    async fn pull(&mut self, cx: &Cx, io: &mut Io, n: i64, discard: bool) -> Result<Map, Failure> {
        let Some(pending) = self.pending.as_mut() else {
            return Err(Failure::invalid("no result to PULL or DISCARD"));
        };
        let db = pending.db;
        self.output_database = Some(db);
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
            if discard {
                continue;
            }
            io.respond(&Response::Record(row));
            // Stream large results instead of buffering every record.
            if index % RECORDS_PER_WRITE == RECORDS_PER_WRITE - 1
                && io.flush(cx, Some((&db.authority, token))).await.is_err()
            {
                return Err(Failure::invalid("connection closed while streaming"));
            }
        }
        if !pending.rows.is_empty() {
            return Ok(vec![("has_more".to_owned(), Value::Bool(true))]);
        }
        let pending = self.pending.take().expect("checked above");
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

    /// `CALL db.labels()` and its siblings: the schema names this token may
    /// see, the same filtered answer as HTTP's schema route.
    fn schema_rows(&mut self, extra: &Map, column: &str, kind: SchemaKind) -> Result<Map, Failure> {
        let (name, db, seq) = match self.transaction.as_ref() {
            Some(transaction) => (
                transaction.name.clone(),
                transaction.db,
                Some(transaction.seq),
            ),
            None => {
                let (name, db) = self.database(extra)?;
                (name, db, None)
            }
        };
        self.output_database = Some(db);
        let token = self.token.as_ref().expect("authenticated");
        let schema = crate::execute::schema(db, token).map_err(Failure::from_refusal)?;
        let names = match kind {
            SchemaKind::Labels => schema.labels,
            SchemaKind::Relations => schema.relations,
            SchemaKind::Properties => schema.properties,
        };
        self.pending = Some(Pending {
            rows: names
                .into_iter()
                .map(|name| vec![Value::String(name)])
                .collect(),
            database: name,
            db,
            seq,
        });
        Ok(vec![
            (
                "fields".to_owned(),
                Value::List(vec![Value::string(column)]),
            ),
            ("t_first".to_owned(), Value::Int(0)),
        ])
    }

    /// A single-server routing table, so `neo4j://` URIs work: this server
    /// is every role. Writes it receives are still refused by the profile.
    fn route(&mut self, io: &Io, routing: &Map, extra: &Map) -> Result<Map, Failure> {
        let (name, db) = self.database(extra)?;
        self.output_database = Some(db);
        let address = get(routing, "address")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| io.stream.local_addr().ok().map(|addr| addr.to_string()))
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

/// The Neo4j schema procedures drivers and tools call, in their plain
/// forms: `CALL db.labels()`, optionally `YIELD label`, and likewise for
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
        "fgdbd's Bolt profile (BoltCompatProfileV1) is read-only: send writes over FGP or HTTP",
    )
}

fn bookmark(database: &str, seq: CommitSeq) -> String {
    format!("fgdb:{database}:{}", seq.0)
}

/// Whether refused statement text contains a write clause keyword outside
/// strings and comments. This only picks the refusal's status code: the
/// read session already refused the statement, and could not have run it.
fn looks_like_write(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            quote @ (b'\'' | b'"' | b'`') => {
                at += 1;
                while at < bytes.len() && bytes[at] != quote {
                    at += if bytes[at] == b'\\' { 2 } else { 1 };
                }
                at += 1;
            }
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            byte if byte.is_ascii_alphabetic() => {
                let start = at;
                while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
                    at += 1;
                }
                let word = &text[start..at];
                let after_dot = start > 0 && bytes[start - 1] == b'.';
                if !after_dot
                    && [
                        "CREATE", "MERGE", "SET", "DELETE", "DETACH", "REMOVE", "INSERT", "FOREACH",
                    ]
                    .iter()
                    .any(|keyword| word.eq_ignore_ascii_case(keyword))
                {
                    return true;
                }
            }
            _ => at += 1,
        }
    }
    false
}

/// RUN parameters as statement arguments, through the same conversion and
/// domain rules as FGP's.
fn statement_parameters(parameters: &Map) -> Result<GqlParameters, Failure> {
    let mut wire = Vec::with_capacity(parameters.len());
    for (name, value) in parameters {
        wire.push((name.clone(), argument(value)?));
    }
    wire.sort_by(|a, b| a.0.cmp(&b.0));
    convert::parameters(&wire, None)
        .map_err(|error| Failure::new("Neo.ClientError.Statement.ArgumentError", error.to_string()))
}

fn argument(value: &Value) -> Result<WireValue, Failure> {
    let unsupported = |what: &str| {
        Failure::new(
            "Neo.ClientError.Statement.TypeError",
            format!("{what} parameters are not supported over Bolt"),
        )
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
        Value::Struct { .. } => return Err(unsupported("this structure's")),
    })
}

/// A hydrated vertex: its visible labels and properties.
type Nodes = BTreeMap<u128, (Vec<String>, Vec<(String, Value)>)>;

/// Every vertex a result row carries, directly or inside a list or map.
fn collect_vertices(value: &WireValue, out: &mut BTreeSet<u128>) {
    match value {
        WireValue::Vertex(id) => {
            out.insert(*id);
        }
        WireValue::Vertices(ids) => out.extend(ids.iter().copied()),
        WireValue::List(items) => items.iter().for_each(|item| collect_vertices(item, out)),
        WireValue::Map(entries) => entries
            .iter()
            .for_each(|(_, value)| collect_vertices(value, out)),
        _ => {}
    }
}

/// Look up the labels and properties of every returned vertex, through the
/// statement's own session: one membership query per bound label and one
/// projection per bound property, each over all returned vertices. A
/// binding the capability cannot see is skipped, exactly as if absent.
fn hydrate(
    query_cx: &fgdb_types::QueryCx,
    session: &mut ReadSession<'_>,
    db: &Served,
    rows: &[Vec<WireValue>],
) -> Result<Nodes, Failure> {
    let mut vertices = BTreeSet::new();
    for row in rows {
        for cell in row {
            collect_vertices(cell, &mut vertices);
        }
    }
    let mut nodes: Nodes = vertices
        .iter()
        .map(|&id| (id, (Vec::new(), Vec::new())))
        .collect();
    if vertices.is_empty() {
        return Ok(nodes);
    }
    let ids: Vec<u128> = vertices.into_iter().collect();
    for batch in ids.chunks(HYDRATION_BATCH) {
        let list: Vec<GraphValue> = batch
            .iter()
            .map(|&id| GraphValue::Vertex(VId(id)))
            .collect();
        let parameters = GqlParameters::new()
            .with_list("__fgdb_vertices", list)
            .map_err(|error| Failure::invalid(error.to_string()))?;
        let mut lookup = |text: String| -> Result<Option<Vec<Vec<WireValue>>>, Failure> {
            match session.query(query_cx, &text, &parameters) {
                Ok(QueryResult::Rows { rows, .. }) => Ok(Some(
                    rows.iter()
                        .map(|row| row.iter().map(convert::cell).collect())
                        .collect(),
                )),
                Ok(QueryResult::Write { .. }) => Err(read_only()),
                // A binding outside the capability's scope reads as absent.
                Err(QueryError::Authorization(_)) => Ok(None),
                Err(error) => Err(Failure::from_refusal(query_refusal(error))),
            }
        };
        for (label, _) in db.symbols.labels() {
            let text = format!(
                "UNWIND $__fgdb_vertices AS n MATCH (n:{}) RETURN n",
                quoted(label)
            );
            for row in lookup(text)?.unwrap_or_default() {
                if let Some(WireValue::Vertex(id)) = row.first()
                    && let Some((labels, _)) = nodes.get_mut(id)
                {
                    labels.push(label.to_owned());
                }
            }
        }
        for (property, _) in db.symbols.properties() {
            let text = format!(
                "UNWIND $__fgdb_vertices AS n MATCH (n) RETURN n, n.{} AS v",
                quoted(property)
            );
            for row in lookup(text)?.unwrap_or_default() {
                let [WireValue::Vertex(id), cell] = row.as_slice() else {
                    continue;
                };
                if matches!(cell, WireValue::Null) {
                    continue;
                }
                let value = value(cell.clone(), &Nodes::new())?;
                if let Some((_, properties)) = nodes.get_mut(id) {
                    properties.push((property.to_owned(), value));
                }
            }
        }
    }
    Ok(nodes)
}

/// A schema name as a delimited identifier.
fn quoted(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

fn feature_not_supported(what: &str) -> Failure {
    Failure::new(
        "Neo.ClientError.Statement.FeatureNotSupported",
        format!(
            "{what} values have no encoding in fgdbd's Bolt profile; return type(r), r.prop or the endpoints instead"
        ),
    )
}

/// One result cell as a Bolt value.
fn value(cell: WireValue, nodes: &Nodes) -> Result<Value, Failure> {
    let node = |id: u128| {
        let (labels, properties) = nodes.get(&id).cloned().unwrap_or_default();
        structure::node(
            i64::try_from(id).unwrap_or(-1),
            labels,
            properties,
            id.to_string(),
        )
    };
    Ok(match cell {
        WireValue::Null => Value::Null,
        WireValue::Bool(value) => Value::Bool(value),
        WireValue::Int(value) => Value::Int(value),
        WireValue::Float(value) => Value::Float(value),
        WireValue::Decimal(text) => Value::String(text),
        WireValue::Text(text) => Value::String(text),
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
        WireValue::Vertex(id) => node(id),
        WireValue::Vertices(ids) => Value::List(ids.into_iter().map(node).collect()),
        WireValue::Edge(_) | WireValue::Edges(_) => {
            return Err(feature_not_supported("Relationship"));
        }
        WireValue::Path { .. } => return Err(feature_not_supported("Path")),
        WireValue::List(items) => Value::List(
            items
                .into_iter()
                .map(|item| value(item, nodes))
                .collect::<Result<_, _>>()?,
        ),
        WireValue::Map(entries) => Value::Map(
            entries
                .into_iter()
                .map(|(key, item)| Ok((key, value(item, nodes)?)))
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
