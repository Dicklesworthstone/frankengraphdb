//! An FGP client: the handshake, statements, and drain, as `fgdbd` serves
//! them. It needs nothing from the engine, only this crate's codec and the
//! foundation's TCP stream, so any surface (the CLI, tests, tools) can use it.
//!
//! The client validates every server frame against the binding it expects at
//! that point of the handshake (transport, then the AUTH_OK session, then the
//! READY binding) before reading any body, and it never grants flow credit it
//! has not consumed. A statement's rows are delivered to a callback as they
//! arrive, so a caller can stream a large result without buffering it.

use crate::body::{
    Auth, AuthOk, AuthRefresh, AuthRefreshed, Body, BodyError, Credential, Empty, ErrorBody,
    ErrorCode, Execute, ExecuteBatch, ExecuteMode, ExecutePrepared, ExecutePreparedBatch, Hello,
    HelloAck, Outcome, Ping, Prepare, PrepareMode, Prepared, PreparedHandle, PreparedReadMode,
    Ready, ReleasePrepared, ResultChunk, ResultEnd, SelectDatabase, SubscriptionBatch,
    WindowUpdate, WireValue,
};
use crate::transport::{
    DuplexIo, DuplexReader, DuplexWriter, FrameReader, FrameWriter, TransportError, split_duplex,
};
use crate::{
    Binding, Frame, FrameKind, FrameLimits, Header, ProtocolError, ReadyBinding, StreamId,
};
use asupersync::Cx;
use asupersync::net::TcpStream;
use std::net::SocketAddr;

#[path = "client/conditional_write.rs"]
mod conditional_write;
#[path = "client/diff.rs"]
mod diff;
pub use diff::DiffAnswer;
#[path = "client/read_batch.rs"]
mod read_batch;
pub use read_batch::{ReadBatchAnswer, ReadBatchResult};
#[path = "client/transaction.rs"]
mod transaction;
pub use transaction::{
    MAX_TRANSACTION_BATCH_STATEMENTS, TransactionAnswer, TransactionBatchAnswer, TransactionBatchEnd,
    TransactionBatchResult,
};

/// What a client can fail with. Server refusals keep their public class.
#[derive(Debug)]
pub enum ClientError {
    Io(std::io::ErrorKind),
    Transport(TransportError),
    Body(BodyError),
    /// The server broke the protocol, or unfinished work requires reconnecting.
    Protocol(&'static str),
    /// The server refused, with its public error class and diagnostics.
    Server {
        code: ErrorCode,
        message: String,
    },
    /// Recovery replaced a session-local subscription. Any incomplete batch
    /// was discarded; subscribe again and replace the bag with its baseline.
    /// This is the last batch accepted by `on_change`, which can precede the
    /// separately validated wire checkpoint if cancellation suppressed a late
    /// callback. Neither checkpoint is a durable resume capability.
    SubscriptionReset {
        last_delivered_seq: Option<u64>,
    },
    /// The server closed the connection.
    Closed,
}

impl core::fmt::Display for ClientError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(kind) => write!(f, "connection failed: {kind}"),
            Self::Transport(error) => write!(f, "transport failed: {error}"),
            Self::Body(error) => write!(f, "invalid server frame: {error}"),
            Self::Protocol(what) => write!(f, "server protocol violation: {what}"),
            Self::Server { code, message } => write!(f, "{}: {message}", code.name()),
            Self::SubscriptionReset { last_delivered_seq } => write!(
                f,
                "subscription reset by database recovery after {last_delivered_seq:?}; subscribe again for a replacement baseline"
            ),
            Self::Closed => f.write_str("the server closed the connection"),
        }
    }
}
impl core::error::Error for ClientError {}

impl From<TransportError> for ClientError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}
impl From<BodyError> for ClientError {
    fn from(error: BodyError) -> Self {
        Self::Body(error)
    }
}

/// The client's frame limit: the largest frame it accepts from the server.
pub const CLIENT_MAX_FRAME: u32 = 1 << 20;

/// What SELECT_DATABASE established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selected {
    /// The frontier the selection observed.
    pub frontier: u64,
    pub binding: ReadyBinding,
}

/// One complete subscription batch: a replacement baseline (`snapshot`) or
/// the exact bag delta up to `frontier`, as (signed weight, row) entries.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub frontier: u64,
    pub snapshot: bool,
    pub entries: Vec<(i128, Vec<WireValue>)>,
}

/// A complete statement answer, as [`Client::execute`] collects it.
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<WireValue>>,
    pub outcome: Outcome,
}

/// One sequential FGP connection. Once a request is queued, a dropped future,
/// a transport failure or an invalid response leaves that request outstanding.
/// Further requests then refuse locally without sending bytes; reconnect to
/// continue. Reconnecting does not establish whether an interrupted write
/// committed, and this client never automatically replays one.
///
/// A validated terminal or server refusal releases the connection, including
/// after a callback error has cancelled and drained delivery. Dropping an
/// unpolled future or failing local request encoding sends nothing and leaves
/// an idle connection usable.
pub struct Client {
    reader: FrameReader<DuplexReader<Box<dyn DuplexIo>>>,
    writer: FrameWriter<DuplexWriter<Box<dyn DuplexIo>>>,
    /// The binding every server frame must carry from now on.
    expected: Binding,
    /// What this client sends with: identical to `expected` once Ready.
    binding: Binding,
    send_limits: FrameLimits,
    initial_window: (u64, u64),
    next_request: u64,
    /// Survives a dropped future: only a validated response can release it.
    outstanding_request: Option<u64>,
    selected: Option<Selected>,
    transaction: Option<transaction::TransactionState>,
}

impl core::fmt::Debug for Client {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Client")
            .field("selected", &self.selected.map(|s| s.frontier))
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Connect, negotiate the protocol version, and authenticate with a
    /// Warden capability token. Select a database next.
    pub async fn connect(
        cx: &Cx,
        addr: SocketAddr,
        credential: Vec<u8>,
    ) -> Result<Self, ClientError> {
        let stream = TcpStream::connect(addr)
            .await
            .map_err(|error| ClientError::Io(error.kind()))?;
        let _ = stream.set_nodelay(true);
        Self::connect_stream(cx, stream, credential).await
    }

    /// Negotiate FGP and authenticate over an already established transport.
    /// A TLS caller must complete certificate/hostname verification before
    /// passing its foundation TlsStream here. This method has no plaintext
    /// fallback and never sends a credential before that transport exists.
    pub async fn connect_stream(
        cx: &Cx,
        stream: impl DuplexIo + 'static,
        credential: Vec<u8>,
    ) -> Result<Self, ClientError> {
        let limits = FrameLimits::new(CLIENT_MAX_FRAME as usize)
            .map_err(|_| ClientError::Protocol("invalid client frame limit"))?;
        let stream: Box<dyn DuplexIo> = Box::new(stream);
        let (read, write) = split_duplex(cx, stream)?;
        let mut client = Self {
            reader: FrameReader::new(read, limits),
            writer: FrameWriter::new(write, limits),
            expected: Binding::Transport,
            binding: Binding::Transport,
            send_limits: limits,
            initial_window: (0, 0),
            next_request: 1,
            outstanding_request: None,
            selected: None,
            transaction: None,
        };
        let mut client_nonce = [0u8; 32];
        cx.random_bytes(&mut client_nonce);
        let hello = Hello {
            min_version: crate::PROTOCOL_VERSION,
            max_version: crate::PROTOCOL_VERSION,
            client_nonce,
            max_frame_len: CLIENT_MAX_FRAME,
        };
        let request = client
            .send(cx, FrameKind::Hello, StreamId::CONTROL, &hello)
            .await?;
        let frame = client.reply(cx, request).await?;
        let ack: HelloAck = expect(&frame, FrameKind::HelloAck)?;
        if ack.version != crate::PROTOCOL_VERSION {
            return Err(ClientError::Protocol("server chose an unoffered version"));
        }
        client.send_limits = FrameLimits::new(CLIENT_MAX_FRAME.min(ack.max_frame_len) as usize)
            .map_err(|_| ClientError::Protocol("server frame limit too small"))?;
        client.initial_window = (ack.initial_window_bytes, ack.initial_window_rows);
        client.finish_request(request)?;
        let auth = Auth {
            credential: Credential::WardenCapability(credential),
        };
        let request = client
            .send(cx, FrameKind::Auth, StreamId::CONTROL, &auth)
            .await?;
        let frame = client.reply(cx, request).await?;
        let ok: AuthOk = expect(&frame, FrameKind::AuthOk)?;
        client.expected = Binding::Session(ok.session);
        client.binding = Binding::Session(ok.session);
        client.finish_request(request)?;
        Ok(client)
    }

    /// Select one database. A refusal leaves the connection authenticated,
    /// so another name may be tried.
    pub async fn select(&mut self, cx: &Cx, database: &str) -> Result<Selected, ClientError> {
        let Binding::Session(session) = self.binding else {
            return Err(ClientError::Protocol("a database is already selected"));
        };
        let body = SelectDatabase {
            name: database.to_owned(),
        };
        let request = self
            .send(cx, FrameKind::SelectDatabase, StreamId::CONTROL, &body)
            .await?;
        let frame = self.reply(cx, request).await?;
        let ready: Ready = expect(&frame, FrameKind::Ready)?;
        let binding = ready.binding(session);
        self.expected = Binding::Ready(binding);
        self.binding = Binding::Ready(binding);
        let selected = Selected {
            frontier: ready.frontier,
            binding,
        };
        self.selected = Some(selected);
        self.finish_request(request)?;
        Ok(selected)
    }

    /// Narrow this idle connection's authority without changing its transcript,
    /// selected database or read frontier metadata. The server authenticates
    /// both credentials and rejects every widening, including expiry extension.
    /// A refusal leaves the old binding usable. Existing statements must finish
    /// or cancel before calling this; refresh neither resets their budgets nor
    /// reauthorizes their results. A lost response requires reconnecting.
    pub async fn refresh_authority(
        &mut self,
        cx: &Cx,
        credential: Vec<u8>,
    ) -> Result<crate::SessionBinding, ClientError> {
        let previous = self.binding;
        let successor = refresh_successor(previous)?;
        let request = self
            .send(
                cx,
                FrameKind::AuthRefresh,
                StreamId::CONTROL,
                &AuthRefresh {
                    credential: Credential::WardenCapability(credential),
                },
            )
            .await?;
        let frame = self
            .reader
            .receive(cx, |header| {
                refresh_header(header, request, previous, successor)
            })
            .await?
            .ok_or(ClientError::Closed)?;
        if frame.header().kind() == FrameKind::Error {
            let refusal = ErrorBody::decode(frame.payload())?;
            self.finish_request(request)?;
            return Err(ClientError::Server {
                code: refusal.code,
                message: refusal.message,
            });
        }
        let refreshed: AuthRefreshed = expect(&frame, FrameKind::AuthRefreshed)?;
        if Some(refreshed.session) != successor.session() {
            return Err(ClientError::Protocol(
                "AUTH_REFRESHED changed the session or skipped a generation",
            ));
        }
        self.binding = successor;
        self.expected = successor;
        if let (Some(selected), Binding::Ready(binding)) = (&mut self.selected, successor) {
            selected.binding = binding;
        }
        self.finish_request(request)?;
        Ok(refreshed.session)
    }

    /// Run one statement, collecting its complete answer.
    pub async fn execute(
        &mut self,
        cx: &Cx,
        mode: ExecuteMode,
        statement: &str,
        parameters: Vec<(String, WireValue)>,
    ) -> Result<Answer, ClientError> {
        let mut columns = Vec::new();
        let mut rows = Vec::new();
        let outcome = self
            .execute_streaming(
                cx,
                mode,
                statement,
                parameters,
                |names| {
                    columns = names.to_vec();
                    Ok(())
                },
                |row| {
                    rows.push(row);
                    Ok(())
                },
            )
            .await?;
        Ok(Answer {
            columns,
            rows,
            outcome,
        })
    }

    /// Bind every argument set to one native write-script definition and
    /// execute the resulting program atomically. The server allocates IDs;
    /// the complete batch shares one signed allowance and one commit. Only
    /// statement statistics/completion are returned, and RETURN is refused
    /// before effects. A transport failure can leave the outcome unknown:
    /// this method never retries, and repeating it can duplicate writes.
    pub async fn execute_batch(
        &mut self,
        cx: &Cx,
        statement: &str,
        mut argument_sets: Vec<Vec<(String, WireValue)>>,
    ) -> Result<Outcome, ClientError> {
        if self.selected.is_none() {
            return Err(ClientError::Protocol("select a database first"));
        }
        for parameters in &mut argument_sets {
            parameters.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        }
        let request = self
            .send(
                cx,
                FrameKind::ExecuteBatch,
                StreamId::CONTROL,
                &ExecuteBatch {
                    statement: statement.to_owned(),
                    argument_sets,
                },
            )
            .await?;
        let mut unexpected_columns = false;
        let outcome = self
            .receive_result(
                cx,
                request,
                |columns| {
                    unexpected_columns |= !columns.is_empty();
                    Ok(())
                },
                |_| Err(ClientError::Protocol("a stats-only batch returned a row")),
            )
            .await?;
        if unexpected_columns || matches!(outcome, Outcome::Rows { .. }) {
            return Err(ClientError::Protocol(
                "a stats-only write batch returned a query result",
            ));
        }
        Ok(outcome)
    }

    /// Prepare one native read template without executing it or pinning a
    /// snapshot. Representative parameters declare structural operand types;
    /// execute supplies every value again. Handles belong to this connection
    /// and are invalidated by authority refresh, database recovery or release.
    pub async fn prepare_read(
        &mut self,
        cx: &Cx,
        statement: &str,
        parameters: Vec<(String, WireValue)>,
    ) -> Result<PreparedHandle, ClientError> {
        self.prepare(cx, PrepareMode::Read, statement, parameters)
            .await
    }

    /// Prepare a stats-only native write script without executing it. Every
    /// execution supplies new operands and obtains fresh live Write authority.
    /// RETURN is refused. Older peers refuse the additive write preparation
    /// profile; the client never retries it as ordinary EXECUTE.
    pub async fn prepare_write(
        &mut self,
        cx: &Cx,
        statement: &str,
        parameters: Vec<(String, WireValue)>,
    ) -> Result<PreparedHandle, ClientError> {
        self.prepare(cx, PrepareMode::Write, statement, parameters)
            .await
    }

    /// Prepare a native write with RETURN without executing it or retaining
    /// operand values. Each execution binds new operands and uses fresh Write
    /// authority. This profile is separate from stats-only scripts and batches.
    pub async fn prepare_write_returning(
        &mut self,
        cx: &Cx,
        statement: &str,
        parameters: Vec<(String, WireValue)>,
    ) -> Result<PreparedHandle, ClientError> {
        self.prepare(cx, PrepareMode::WriteReturning, statement, parameters)
            .await
    }

    async fn prepare(
        &mut self,
        cx: &Cx,
        mode: PrepareMode,
        statement: &str,
        mut parameters: Vec<(String, WireValue)>,
    ) -> Result<PreparedHandle, ClientError> {
        if self.selected.is_none() {
            return Err(ClientError::Protocol("select a database first"));
        }
        parameters.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let request = self
            .send(
                cx,
                FrameKind::Prepare,
                StreamId::CONTROL,
                &Prepare {
                    mode,
                    statement: statement.to_owned(),
                    parameters,
                },
            )
            .await?;
        let frame = self.reply(cx, request).await?;
        if !frame.header().stream_id().is_control() {
            return Err(ClientError::Protocol("PREPARED on a child stream"));
        }
        let prepared: Prepared = expect(&frame, FrameKind::Prepared)?;
        self.finish_request(request)?;
        Ok(prepared.handle)
    }

    /// Drop a connection-owned statement template. Repeated/unknown releases have
    /// the same response. This releases no transaction or durable result.
    pub async fn release_prepared(
        &mut self,
        cx: &Cx,
        handle: PreparedHandle,
    ) -> Result<(), ClientError> {
        if self.selected.is_none() {
            return Err(ClientError::Protocol("select a database first"));
        }
        let request = self
            .send(
                cx,
                FrameKind::ReleasePrepared,
                StreamId::CONTROL,
                &ReleasePrepared { handle },
            )
            .await?;
        let frame = self.reply(cx, request).await?;
        if !frame.header().stream_id().is_control() {
            return Err(ClientError::Protocol("PREPARED_RELEASED on a child stream"));
        }
        let released: ReleasePrepared = expect(&frame, FrameKind::PreparedReleased)?;
        if released.handle != handle {
            return Err(ClientError::Protocol(
                "released a different prepared handle",
            ));
        }
        self.finish_request(request)?;
        Ok(())
    }

    /// Execute a prepared read at the current frontier, collecting its answer.
    pub async fn execute_prepared(
        &mut self,
        cx: &Cx,
        handle: PreparedHandle,
        parameters: Vec<(String, WireValue)>,
    ) -> Result<Answer, ClientError> {
        let mut columns = Vec::new();
        let mut rows = Vec::new();
        let outcome = self
            .execute_prepared_streaming(
                cx,
                PreparedReadMode::Buffered,
                handle,
                parameters,
                |names| {
                    columns = names.to_vec();
                    Ok(())
                },
                |row| {
                    rows.push(row);
                    Ok(())
                },
            )
            .await?;
        Ok(Answer {
            columns,
            rows,
            outcome,
        })
    }

    /// Rebind real typed operands and deliver a prepared read to callbacks.
    /// `SourceStream` makes flow credit drive the native source, retaining one
    /// pinned generation and execution allowance. It admits the same row and
    /// aggregate profiles as `ExecuteMode::ReadStream`; unsupported profiles
    /// refuse without falling back to buffered execution. `Buffered` retains
    /// the broader native read profile and chunks its completed answer.
    /// `OrderedStream` uses the server's bounded external ordering profile:
    /// the complete private order, DISTINCT and window finish before the first
    /// result row. Credit controls delivery of final rows. The server owns all
    /// scratch limits; an unsupported profile or missing host scratch refuses.
    ///
    /// A refusal from either callback sends QUERY_CANCEL and drains its
    /// terminal before returning, so later execution cannot consume cancelled
    /// result frames. Columns can arrive before any ordered source work; a
    /// schema refusal cancels then, without waiting for a first row.
    pub async fn execute_prepared_streaming(
        &mut self,
        cx: &Cx,
        mode: PreparedReadMode,
        handle: PreparedHandle,
        mut parameters: Vec<(String, WireValue)>,
        on_columns: impl FnMut(&[String]) -> Result<(), ClientError>,
        on_row: impl FnMut(Vec<WireValue>) -> Result<(), ClientError>,
    ) -> Result<Outcome, ClientError> {
        if matches!(
            mode,
            PreparedReadMode::Write | PreparedReadMode::WriteReturning
        ) {
            return Err(ClientError::Protocol(
                "a prepared read cannot request a write mode",
            ));
        }
        if self.selected.is_none() {
            return Err(ClientError::Protocol("select a database first"));
        }
        parameters.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let request = self
            .send(
                cx,
                FrameKind::ExecutePrepared,
                StreamId::CONTROL,
                &ExecutePrepared {
                    handle,
                    mode,
                    parameters,
                },
            )
            .await?;
        let outcome = self.receive_result(cx, request, on_columns, on_row).await?;
        if !matches!(outcome, Outcome::Rows { .. }) {
            return Err(ClientError::Protocol(
                "prepared read returned a write outcome",
            ));
        }
        Ok(outcome)
    }

    /// Execute one prepared write as an atomic, stats-only autocommit. A read
    /// handle is refused by the server. Repeating this call is a new write;
    /// unknown completion is never retried or described as a rollback.
    pub async fn execute_prepared_write(
        &mut self,
        cx: &Cx,
        handle: PreparedHandle,
        mut parameters: Vec<(String, WireValue)>,
    ) -> Result<Outcome, ClientError> {
        if self.selected.is_none() {
            return Err(ClientError::Protocol("select a database first"));
        }
        parameters.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let request = self
            .send(
                cx,
                FrameKind::ExecutePrepared,
                StreamId::CONTROL,
                &ExecutePrepared {
                    handle,
                    mode: PreparedReadMode::Write,
                    parameters,
                },
            )
            .await?;
        let mut unexpected_columns = false;
        let outcome = self
            .receive_result(
                cx,
                request,
                |columns| {
                    unexpected_columns |= !columns.is_empty();
                    Ok(())
                },
                |_| {
                    Err(ClientError::Protocol(
                        "a stats-only prepared write returned a row",
                    ))
                },
            )
            .await?;
        if unexpected_columns || matches!(outcome, Outcome::Rows { .. }) {
            return Err(ClientError::Protocol(
                "a stats-only prepared write returned a query result",
            ));
        }
        Ok(outcome)
    }

    /// Execute a prepared write with RETURN, collecting its admitted answer.
    /// A transport failure can follow a committed write; this never retries.
    pub async fn execute_prepared_write_returning(
        &mut self,
        cx: &Cx,
        handle: PreparedHandle,
        parameters: Vec<(String, WireValue)>,
    ) -> Result<Answer, ClientError> {
        let mut columns = Vec::new();
        let mut rows = Vec::new();
        let outcome = self
            .execute_prepared_write_returning_streaming(
                cx,
                handle,
                parameters,
                |names| {
                    columns = names.to_vec();
                    Ok(())
                },
                |row| {
                    rows.push(row);
                    Ok(())
                },
            )
            .await?;
        Ok(Answer {
            columns,
            rows,
            outcome,
        })
    }

    /// Deliver a prepared write's RETURN rows under ordinary FGP flow credit.
    /// The server computes and admits the complete bounded answer before the
    /// sole native completion; callbacks receive it only after that boundary.
    /// Refusing a callback cancels and drains delivery, NOT the committed
    /// effects. No disconnect, cancellation or unknown outcome is replayed.
    pub async fn execute_prepared_write_returning_streaming(
        &mut self,
        cx: &Cx,
        handle: PreparedHandle,
        mut parameters: Vec<(String, WireValue)>,
        on_columns: impl FnMut(&[String]) -> Result<(), ClientError>,
        on_row: impl FnMut(Vec<WireValue>) -> Result<(), ClientError>,
    ) -> Result<Outcome, ClientError> {
        if self.selected.is_none() {
            return Err(ClientError::Protocol("select a database first"));
        }
        parameters.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let request = self
            .send(
                cx,
                FrameKind::ExecutePrepared,
                StreamId::CONTROL,
                &ExecutePrepared {
                    handle,
                    mode: PreparedReadMode::WriteReturning,
                    parameters,
                },
            )
            .await?;
        let outcome = self.receive_result(cx, request, on_columns, on_row).await?;
        if matches!(outcome, Outcome::Rows { .. }) {
            return Err(ClientError::Protocol(
                "prepared write RETURN returned a read outcome",
            ));
        }
        Ok(outcome)
    }

    /// Execute one retained write template for every supplied operand set as
    /// one atomic, stats-only program. The server binds all sets before effects,
    /// reuses the immutable compiler/catalog bindings, and applies one signed
    /// execution allowance and expanded-statement limit to the complete batch.
    /// Every invocation uses the current frontier and fresh live authority.
    ///
    /// The handle must come from prepare_write(). Empty input, a read handle,
    /// incompatible operands and native RETURN profiles refuse. A transport
    /// failure can follow a committed batch; this method never retries or
    /// falls back to individual writes. Repeating it can duplicate effects.
    pub async fn execute_prepared_batch(
        &mut self,
        cx: &Cx,
        handle: PreparedHandle,
        mut argument_sets: Vec<Vec<(String, WireValue)>>,
    ) -> Result<Outcome, ClientError> {
        if self.selected.is_none() {
            return Err(ClientError::Protocol("select a database first"));
        }
        for parameters in &mut argument_sets {
            parameters.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        }
        let request = self
            .send(
                cx,
                FrameKind::ExecutePreparedBatch,
                StreamId::CONTROL,
                &ExecutePreparedBatch {
                    handle,
                    argument_sets,
                },
            )
            .await?;
        let mut unexpected_columns = false;
        let outcome = self
            .receive_result(
                cx,
                request,
                |columns| {
                    unexpected_columns |= !columns.is_empty();
                    Ok(())
                },
                |_| {
                    Err(ClientError::Protocol(
                        "a stats-only prepared batch returned a row",
                    ))
                },
            )
            .await?;
        if unexpected_columns || matches!(outcome, Outcome::Rows { .. }) {
            return Err(ClientError::Protocol(
                "a stats-only prepared batch returned a query result",
            ));
        }
        Ok(outcome)
    }

    /// Run one statement, handing over its columns once and then each row as
    /// it arrives. An error from either callback cancels and drains the stream
    /// before returning that error, leaving the connection ready for another
    /// request. A columns-only announcement can be refused immediately, even
    /// if the server is still completing a private sort before its first row.
    /// `ReadStream` additionally makes flow credit drive the server's actual
    /// native source, retaining one pinned generation and execution allowance.
    /// It accepts the native cursor's identity-leading row profile or admitted
    /// vertex/fixed-edge aggregates. Aggregate input is consumed on first demand
    /// and only complete groups are delivered. Unsupported plans refuse.
    /// `Read` retains its broader eager execution semantics; callbacks alone
    /// do not make that server source lazy.
    /// `ReadOrdered` completes the native ORDER BY, DISTINCT and window in
    /// host-owned bounded scratch before returning rows. It accepts admitted
    /// property projections and hidden sort keys, and applies credit to final
    /// delivery. Missing scratch or an unsupported physical profile refuses
    /// without retrying another mode.
    /// The client replenishes an exactly exhausted row window before waiting
    /// for END, allowing a native source to discover natural EOF without an
    /// unauthorized pull at zero credit. No automatic query retry occurs.
    pub async fn execute_streaming(
        &mut self,
        cx: &Cx,
        mode: ExecuteMode,
        statement: &str,
        mut parameters: Vec<(String, WireValue)>,
        on_columns: impl FnMut(&[String]) -> Result<(), ClientError>,
        on_row: impl FnMut(Vec<WireValue>) -> Result<(), ClientError>,
    ) -> Result<Outcome, ClientError> {
        if self.selected.is_none() {
            return Err(ClientError::Protocol("select a database first"));
        }
        parameters.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let body = Execute {
            mode,
            statement: statement.to_owned(),
            parameters,
        };
        let request = self
            .send(cx, FrameKind::Execute, StreamId::CONTROL, &body)
            .await?;
        self.receive_result(cx, request, on_columns, on_row).await
    }

    async fn receive_result(
        &mut self,
        cx: &Cx,
        request: u64,
        mut on_columns: impl FnMut(&[String]) -> Result<(), ClientError>,
        mut on_row: impl FnMut(Vec<WireValue>) -> Result<(), ClientError>,
    ) -> Result<Outcome, ClientError> {
        let mut stream: Option<StreamId> = None;
        let mut callback_error = None;
        // The server sends only within its credit, so the client models that
        // credit exactly and replenishes it whenever the server might be
        // unable to fit its next frame (one maximal frame, or one row).
        let (initial_bytes, initial_rows) = self.initial_window;
        let mut available = (initial_bytes, initial_rows);
        let mut grants = 0u64;
        let mut progress = ResultProgress::default();
        loop {
            let frame = self.receive(cx).await?.ok_or(ClientError::Closed)?;
            let header = *frame.header();
            if header.kind() == FrameKind::Goodbye {
                return Err(ClientError::Closed);
            }
            if header.request_id() != request {
                return Err(ClientError::Protocol(
                    "result frame for a different request",
                ));
            }
            match stream {
                None if !header.stream_id().is_control() => stream = Some(header.stream_id()),
                Some(id) if id != header.stream_id() => {
                    return Err(ClientError::Protocol("result frame on a foreign stream"));
                }
                _ => {}
            }
            match header.kind() {
                FrameKind::SnapshotResultChunk => {
                    let Some(id) = stream else {
                        return Err(ClientError::Protocol("chunk on the control stream"));
                    };
                    let chunk = ResultChunk::decode(frame.payload())?;
                    let count = chunk.rows.len() as u64;
                    let cost = header.frame_len() as u64;
                    if cost > available.0 || count > available.1 {
                        return Err(ClientError::Protocol("server exceeded its flow credit"));
                    }
                    available = (available.0 - cost, available.1 - count);
                    // Validate the complete chunk before exposing any of it
                    // to callbacks, including while draining a refused result.
                    progress
                        .observe_chunk(&chunk)
                        .map_err(ClientError::Protocol)?;
                    if let Some(names) = chunk.columns
                        && let Err(error) = on_columns(&names)
                    {
                        self.send(cx, FrameKind::QueryCancel, id, &Empty).await?;
                        callback_error = Some(error);
                    }
                    for row in chunk.rows {
                        if callback_error.is_none()
                            && let Err(error) = on_row(row)
                        {
                            self.send(cx, FrameKind::QueryCancel, id, &Empty).await?;
                            callback_error = Some(error);
                        }
                    }
                    let frame_limit = self.send_limits.max_frame_len() as u64;
                    if callback_error.is_none() && (available.0 < frame_limit || available.1 == 0) {
                        grants = grants
                            .checked_add(1)
                            .ok_or(ClientError::Protocol("result flow sequence overflow"))?;
                        let update = WindowUpdate {
                            sequence: grants,
                            bytes: initial_bytes - available.0,
                            rows: initial_rows - available.1,
                        };
                        self.send(cx, FrameKind::WindowUpdate, id, &update).await?;
                        available = (initial_bytes, initial_rows);
                    }
                }
                FrameKind::SnapshotResultEnd => {
                    let end = ResultEnd::decode(frame.payload())?;
                    progress
                        .finish(&end, header.frame_len() as u64, available.0)
                        .map_err(ClientError::Protocol)?;
                    self.finish_request(request)?;
                    return callback_error.map_or(Ok(end.outcome), Err);
                }
                FrameKind::Error => {
                    let refusal = ErrorBody::decode(frame.payload())?;
                    self.finish_request(request)?;
                    return Err(callback_error.unwrap_or(ClientError::Server {
                        code: refusal.code,
                        message: refusal.message,
                    }));
                }
                _ => return Err(ClientError::Protocol("unexpected frame in a result stream")),
            }
        }
    }

    /// Subscribe to `SUBSCRIBE TO <read>`: hand over the columns once, then
    /// every complete batch (the baseline first) as it arrives. Returning
    /// `Ok(false)` from `on_change` cancels the subscription; the server then
    /// ends the stream and this returns the last frontier it delivered.
    /// A callback error also cancels and drains the child before returning the
    /// original error. Later batches are validated but not delivered, and no
    /// more flow credit is granted while cancellation is draining.
    pub async fn subscribe(
        &mut self,
        cx: &Cx,
        statement: &str,
        mut parameters: Vec<(String, WireValue)>,
        mut on_columns: impl FnMut(&[String]),
        mut on_change: impl FnMut(Change) -> Result<bool, ClientError>,
    ) -> Result<u64, ClientError> {
        if self.selected.is_none() {
            return Err(ClientError::Protocol("select a database first"));
        }
        parameters.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let body = Execute {
            mode: ExecuteMode::Subscribe,
            statement: statement.to_owned(),
            parameters,
        };
        let request = self
            .send(cx, FrameKind::Execute, StreamId::CONTROL, &body)
            .await?;
        let (initial_bytes, initial_rows) = self.initial_window;
        let mut available = (initial_bytes, initial_rows);
        let mut grants = 0u64;
        let mut stream: Option<StreamId> = None;
        let mut progress = SubscriptionProgress::default();
        let mut cancelled = false;
        let mut callback_error = None;
        let mut pending: Option<Change> = None;
        let mut delivered_to_callback = None;
        loop {
            let frame = self.receive(cx).await?.ok_or(ClientError::Closed)?;
            let header = *frame.header();
            if header.kind() == FrameKind::Goodbye {
                return Err(ClientError::Closed);
            }
            if header.request_id() != request {
                return Err(ClientError::Protocol(
                    "subscription frame for a different request",
                ));
            }
            match stream {
                None if !header.stream_id().is_control() => stream = Some(header.stream_id()),
                Some(id) if id != header.stream_id() => {
                    return Err(ClientError::Protocol(
                        "subscription frame on a foreign stream",
                    ));
                }
                _ => {}
            }
            match header.kind() {
                FrameKind::SubscriptionBatch => {
                    let id = stream.ok_or(ClientError::Protocol("batch on the control stream"))?;
                    let part = SubscriptionBatch::decode(frame.payload())?;
                    let count = part.entries.len() as u64;
                    let cost = header.frame_len() as u64;
                    if cost > available.0 || count > available.1 {
                        return Err(ClientError::Protocol("server exceeded its flow credit"));
                    }
                    // Validate the entire part before either callback can see
                    // its schema or values, including late parts after cancel.
                    progress.batch(&part).map_err(ClientError::Protocol)?;
                    available = (available.0 - cost, available.1 - count);
                    if let Some(names) = part.columns {
                        on_columns(&names);
                    }
                    let change = pending.get_or_insert_with(|| Change {
                        frontier: part.frontier,
                        snapshot: part.snapshot,
                        entries: Vec::new(),
                    });
                    change.entries.extend(part.entries);
                    if part.last {
                        let change = pending.take().expect("a pending batch was just extended");
                        if !cancelled {
                            let frontier = change.frontier;
                            let keep = match on_change(change) {
                                Ok(keep) => {
                                    delivered_to_callback = Some(frontier);
                                    keep
                                }
                                Err(error) => {
                                    callback_error = Some(error);
                                    false
                                }
                            };
                            if !keep {
                                cancelled = true;
                                self.send(cx, FrameKind::QueryCancel, id, &Empty).await?;
                            }
                        }
                    }
                    let frame_limit = self.send_limits.max_frame_len() as u64;
                    if !cancelled && (available.0 < frame_limit || available.1 == 0) {
                        grants = grants
                            .checked_add(1)
                            .ok_or(ClientError::Protocol("flow-credit sequence exhausted"))?;
                        let update = WindowUpdate {
                            sequence: grants,
                            bytes: initial_bytes - available.0,
                            rows: initial_rows - available.1,
                        };
                        self.send(cx, FrameKind::WindowUpdate, id, &update).await?;
                        available = (initial_bytes, initial_rows);
                    }
                }
                FrameKind::SnapshotResultEnd => {
                    let end = ResultEnd::decode(frame.payload())?;
                    progress
                        .end(&end, cancelled)
                        .map_err(ClientError::Protocol)?;
                    let Outcome::Rows { seq } = end.outcome else {
                        return Err(ClientError::Protocol(
                            "a subscription ended with a write outcome",
                        ));
                    };
                    let delivered = if cancelled {
                        delivered_to_callback.unwrap_or(0)
                    } else {
                        seq
                    };
                    self.finish_request(request)?;
                    return callback_error.map_or(Ok(delivered), Err);
                }
                FrameKind::SubscriptionReset => {
                    let reset = crate::body::SubscriptionReset::decode(frame.payload())?;
                    if stream.is_none() {
                        return Err(ClientError::Protocol(
                            "subscription reset on the control stream",
                        ));
                    }
                    progress.reset(&reset).map_err(ClientError::Protocol)?;
                    self.finish_request(request)?;
                    return Err(callback_error.unwrap_or(ClientError::SubscriptionReset {
                        last_delivered_seq: delivered_to_callback,
                    }));
                }
                FrameKind::Error => {
                    // A malformed terminal must not be hidden by a callback
                    // failure. Only a validated terminal finishes the drain.
                    let refusal = ErrorBody::decode(frame.payload())?;
                    self.finish_request(request)?;
                    if let Some(error) = callback_error {
                        return Err(error);
                    }
                    // A cancel while a batch waits for credit ends with this
                    // class instead of END and discards any pending fragment.
                    if cancelled && refusal.code == ErrorCode::Cancelled {
                        return Ok(delivered_to_callback.unwrap_or(0));
                    }
                    return Err(ClientError::Server {
                        code: refusal.code,
                        message: refusal.message,
                    });
                }
                _ => return Err(ClientError::Protocol("unexpected frame in a subscription")),
            }
        }
    }

    /// Round-trip a PING.
    pub async fn ping(&mut self, cx: &Cx, nonce: u64) -> Result<(), ClientError> {
        let request = self
            .send(cx, FrameKind::Ping, StreamId::CONTROL, &Ping { nonce })
            .await?;
        let frame = self.reply(cx, request).await?;
        let pong: Ping = expect(&frame, FrameKind::Pong)?;
        if pong.nonce == nonce {
            self.finish_request(request)?;
            Ok(())
        } else {
            Err(ClientError::Protocol("PONG echoed a different value"))
        }
    }

    /// Ask the server to drain and wait for its GOODBYE. An unfinished request
    /// refuses locally; consuming this client then drops its transport.
    pub async fn close(mut self, cx: &Cx) -> Result<(), ClientError> {
        self.send(cx, FrameKind::Drain, StreamId::CONTROL, &Empty)
            .await?;
        loop {
            match self.receive(cx).await? {
                Some(frame) if frame.header().kind() == FrameKind::Goodbye => return Ok(()),
                Some(_) => {}
                None => return Err(ClientError::Closed),
            }
        }
    }

    async fn send(
        &mut self,
        cx: &Cx,
        kind: FrameKind,
        stream: StreamId,
        body: &impl Body,
    ) -> Result<u64, ClientError> {
        let continuation = matches!(kind, FrameKind::WindowUpdate | FrameKind::QueryCancel);
        if !continuation && self.outstanding_request.is_some() {
            return Err(ClientError::Protocol(
                "an earlier request is unfinished; reconnect",
            ));
        }
        if self.transaction.is_some()
            && !matches!(
                kind,
                FrameKind::ExecuteTransaction
                    | FrameKind::CommitTransaction
                    | FrameKind::RollbackTransaction
                    | FrameKind::Ping
                    | FrameKind::Drain
                    | FrameKind::WindowUpdate
                    | FrameKind::QueryCancel
            )
        {
            return Err(ClientError::Protocol(
                "an explicit transaction is active; finish it first",
            ));
        }
        let request = self.next_request;
        let next_request = request
            .checked_add(1)
            .ok_or(ClientError::Protocol("request sequence exhausted"))?;
        let frame = Frame::new(
            kind,
            request,
            stream,
            self.binding,
            body.encode()?,
            self.send_limits,
        )
        .map_err(|_| ClientError::Protocol("request exceeds the negotiated frame limit"))?;
        // queue is synchronous and writes no bytes. Local encoding, size or
        // admission errors therefore leave an idle connection reusable.
        self.writer.queue(cx, &frame)?;
        self.next_request = next_request;
        if !continuation {
            self.outstanding_request = Some(request);
        }
        // Arm before the first await: a partial write, a stopped context or a
        // dropped receive future must never admit another business request.
        self.writer.send(cx, |_| Ok(())).await?;
        Ok(request)
    }

    /// A validated terminal releases wire ownership. Callers may still refuse
    /// a fully drained answer whose semantic mode differs from their API.
    fn finish_request(&mut self, request: u64) -> Result<(), ClientError> {
        if self.outstanding_request != Some(request) {
            return Err(ClientError::Protocol("response has no outstanding request"));
        }
        self.outstanding_request = None;
        Ok(())
    }

    async fn receive(&mut self, cx: &Cx) -> Result<Option<Frame>, ClientError> {
        let expected = self.expected;
        Ok(self
            .reader
            .receive(cx, |header| server_header(header, expected))
            .await?)
    }

    /// The next control frame answering `request`. A GOODBYE or EOF ends
    /// the session; an unrelated response cannot complete the pending request.
    async fn reply(&mut self, cx: &Cx, request: u64) -> Result<Frame, ClientError> {
        let frame = self.receive(cx).await?.ok_or(ClientError::Closed)?;
        let header = frame.header();
        if header.kind() == FrameKind::Goodbye {
            return Err(ClientError::Closed);
        }
        if header.request_id() != request {
            return Err(ClientError::Protocol("reply for a different request"));
        }
        if !header.stream_id().is_control() {
            return Err(ClientError::Protocol("control reply on a child stream"));
        }
        if header.kind() == FrameKind::Error {
            let refusal = ErrorBody::decode(frame.payload())?;
            self.finish_request(request)?;
            return Err(ClientError::Server {
                code: refusal.code,
                message: refusal.message,
            });
        }
        Ok(frame)
    }
}

fn refresh_successor(binding: Binding) -> Result<Binding, ClientError> {
    let mut session = binding
        .session()
        .ok_or(ClientError::Protocol("not authenticated"))?;
    session.auth_generation = session
        .auth_generation
        .checked_add(1)
        .ok_or(ClientError::Protocol("authentication generation exhausted"))?;
    Ok(match binding {
        Binding::Session(_) => Binding::Session(session),
        Binding::Ready(mut ready) => {
            ready.session = session;
            Binding::Ready(ready)
        }
        Binding::Transport => return Err(ClientError::Protocol("not authenticated")),
    })
}

fn refresh_header(
    header: &Header,
    request: u64,
    previous: Binding,
    successor: Binding,
) -> Result<(), ProtocolError> {
    if header.request_id() != request || !header.stream_id().is_control() {
        return Err(ProtocolError::InvalidRequest);
    }
    let expected = match header.kind() {
        FrameKind::AuthRefreshed => successor,
        FrameKind::Error => previous,
        _ => return Err(ProtocolError::InvalidState),
    };
    if header.binding() == expected {
        Ok(())
    } else {
        Err(ProtocolError::InvalidBinding)
    }
}

/// Server frames: only server-to-client kinds, under the expected binding,
/// except the handshake replies that complete a binding.
fn server_header(header: &Header, expected: Binding) -> Result<(), ProtocolError> {
    let binding_ok = match header.kind() {
        FrameKind::HelloAck | FrameKind::AuthOk => header.binding() == Binding::Transport,
        FrameKind::Ready => matches!(expected, Binding::Session(_)) && header.binding() == expected,
        FrameKind::Error => header.binding() == expected,
        FrameKind::Prepared
        | FrameKind::PreparedReleased
        | FrameKind::TransactionBegun
        | FrameKind::TransactionCommitted
        | FrameKind::TransactionRolledBack => {
            if !matches!(expected, Binding::Ready(_)) || !header.stream_id().is_control() {
                return Err(ProtocolError::InvalidState);
            }
            header.binding() == expected
        }
        FrameKind::TransactionResultChunk | FrameKind::TransactionResultEnd => {
            if !matches!(expected, Binding::Ready(_)) || header.stream_id().is_control() {
                return Err(ProtocolError::InvalidState);
            }
            header.binding() == expected
        }
        FrameKind::Pong
        | FrameKind::Goodbye
        | FrameKind::SnapshotResultChunk
        | FrameKind::SnapshotResultEnd
        | FrameKind::DiffResultChunk
        | FrameKind::DiffResultEnd
        | FrameKind::ReadBatchResultChunk
        | FrameKind::ReadBatchResultEnd
        | FrameKind::SubscriptionBatch
        | FrameKind::SubscriptionReset => header.binding() == expected,
        _ => return Err(ProtocolError::InvalidState),
    };
    if binding_ok {
        Ok(())
    } else {
        Err(ProtocolError::InvalidBinding)
    }
}

/// Validated progress of one finite result stream. Transport owns request and
/// stream identities and chunk credit; this state binds every row to the first
/// schema and the final delivered-row count. Callbacks may reject a prefix, but
/// the receiver still records and validates every row it drains afterward.
#[derive(Default)]
pub(crate) struct ResultProgress {
    columns: Option<usize>,
    rows: u64,
}

impl ResultProgress {
    pub(crate) fn observe_chunk(
        &mut self,
        chunk: &crate::body::ResultChunk,
    ) -> Result<(), &'static str> {
        let columns = match (&chunk.columns, self.columns) {
            (Some(names), None) => names.len(),
            (None, Some(columns)) => columns,
            _ => return Err("columns out of order"),
        };
        if chunk.rows.iter().any(|row| row.len() != columns) {
            return Err("result row width differs from columns");
        }
        let count = u64::try_from(chunk.rows.len()).map_err(|_| "result row count overflow")?;
        let rows = self
            .rows
            .checked_add(count)
            .ok_or("result row count overflow")?;
        // A malformed chunk changes neither the schema nor the row count.
        self.columns = Some(columns);
        self.rows = rows;
        Ok(())
    }

    pub(crate) fn finish(
        &self,
        end: &crate::body::ResultEnd,
        frame_bytes: u64,
        available_bytes: u64,
    ) -> Result<(), &'static str> {
        self.finish_rows(end.rows, frame_bytes, available_bytes)
    }

    fn finish_rows(
        &self,
        rows: u64,
        frame_bytes: u64,
        available_bytes: u64,
    ) -> Result<(), &'static str> {
        if frame_bytes > available_bytes {
            return Err("server exceeded its flow credit");
        }
        if self.columns.is_none() {
            return Err("END before columns");
        }
        if rows != self.rows {
            return Err("END row count differs from delivery");
        }
        Ok(())
    }
}

/// Progress of one session-local subscription. Frames partition complete
/// replacement baselines or exact bag deltas; partial batches are not checkpoints.
#[derive(Default)]
pub(crate) struct SubscriptionProgress {
    columns: Option<usize>,
    pending: Option<(u64, bool)>,
    completed: Option<u64>,
}

impl SubscriptionProgress {
    pub(crate) fn batch(
        &mut self,
        part: &crate::body::SubscriptionBatch,
    ) -> Result<(), &'static str> {
        let width = match (&part.columns, self.columns) {
            (Some(names), None) => names.len(),
            (None, Some(width)) => width,
            _ => return Err("subscription columns out of order"),
        };
        if part.entries.iter().any(|(_, row)| row.len() != width) {
            return Err("subscription row width differs from columns");
        }
        if part
            .entries
            .iter()
            .any(|(weight, _)| *weight == 0 || (part.snapshot && *weight < 0))
        {
            return Err("invalid subscription multiplicity");
        }
        match self.pending {
            Some((frontier, snapshot))
                if frontier != part.frontier || snapshot != part.snapshot =>
            {
                return Err("a batch changed frontier or kind mid-stream");
            }
            None => match self.completed {
                None if !part.snapshot => return Err("subscription delta before baseline"),
                Some(frontier)
                    if part.frontier < frontier
                        || (!part.snapshot && part.frontier == frontier) =>
                {
                    return Err("subscription frontier did not advance");
                }
                _ => {}
            },
            _ => {}
        }
        // A retention gap may replace the baseline at the same or a later cut.
        self.columns = Some(width);
        if part.last {
            self.pending = None;
            self.completed = Some(part.frontier);
        } else {
            self.pending = Some((part.frontier, part.snapshot));
        }
        Ok(())
    }

    pub(crate) fn end(
        &self,
        end: &crate::body::ResultEnd,
        cancelled: bool,
    ) -> Result<(), &'static str> {
        if self.columns.is_none() {
            return Err("subscription END before columns");
        }
        if self.pending.is_some() && !cancelled {
            return Err("subscription END inside an incomplete batch");
        }
        let crate::body::Outcome::Rows { seq } = end.outcome else {
            return Err("a subscription ended with a write outcome");
        };
        // END is the existing credit-free control terminal. Cancellation may
        // abandon an undelivered fragment, never advance its checkpoint.
        if end.rows != 0 || Some(seq) != self.completed {
            return Err("subscription END does not match completed delivery");
        }
        Ok(())
    }

    pub(crate) fn reset(&self, reset: &crate::body::SubscriptionReset) -> Result<(), &'static str> {
        if reset.last_delivered_seq != self.completed {
            return Err("subscription reset checkpoint does not match completed delivery");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::{
        DiffMode, DiffResultChunk, DiffResultEnd, ErrorBody, ExecuteConditionalWrite, ExecuteDiff,
        ExecutePreparedDiff, ExecutePreparedReadBatch, ReadBatchResultChunk, ReadBatchResultEnd,
        SubscriptionReset,
    };
    use crate::{Posture, SessionBinding};
    use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
    use asupersync::lab::run_async_under_lab;
    use core::pin::Pin;
    use core::task::{Context, Poll};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ScriptIo {
        input: Vec<u8>,
        offset: usize,
        written: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl AsyncRead for ScriptIo {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            // Every server frame crosses physical reads in the real decoder.
            let count = 7
                .min(buffer.remaining())
                .min(self.input.len() - self.offset);
            buffer.put_slice(&self.input[self.offset..self.offset + count]);
            self.offset += count;
            Poll::Ready(Ok(()))
        }
    }
    impl AsyncWrite for ScriptIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.written.lock().unwrap().extend_from_slice(bytes);
            Poll::Ready(Ok(bytes.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    // Complete the handshake, then keep reads pending. The shared write
    // allowance can stop an application frame after an exact physical prefix.
    struct PausedIo {
        inner: ScriptIo,
        write_budget: std::sync::Arc<AtomicUsize>,
    }

    impl AsyncRead for PausedIo {
        fn poll_read(
            mut self: Pin<&mut Self>,
            task: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.inner.offset == self.inner.input.len() {
                return Poll::Pending;
            }
            Pin::new(&mut self.inner).poll_read(task, buffer)
        }
    }

    impl AsyncWrite for PausedIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let count = bytes.len().min(self.write_budget.load(Ordering::SeqCst));
            if count == 0 {
                return Poll::Pending;
            }
            self.inner
                .written
                .lock()
                .unwrap()
                .extend_from_slice(&bytes[..count]);
            self.write_budget.fetch_sub(count, Ordering::SeqCst);
            Poll::Ready(Ok(count))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn requires_reconnect<T>(result: Result<T, ClientError>) {
        assert!(matches!(
            result,
            Err(ClientError::Protocol(
                "an earlier request is unfinished; reconnect"
            ))
        ));
    }

    fn ready() -> Ready {
        Ready {
            namespace: [2; 32],
            incarnation: [3; 32],
            service_epoch: 1,
            posture: Posture::Local,
            authority_commitment: [4; 32],
            frontier: 0,
        }
    }
    fn session() -> SessionBinding {
        SessionBinding {
            transcript: [1; 32],
            auth_generation: 1,
        }
    }
    fn output(kind: FrameKind, body: &impl Body) -> Frame {
        Frame::new(
            kind,
            4,
            StreamId([5; 16]),
            Binding::Ready(ready().binding(session())),
            body.encode().unwrap(),
            FrameLimits::new(4096).unwrap(),
        )
        .unwrap()
    }
    fn batch(frontier: u64, first: bool, last: bool) -> Frame {
        output(
            FrameKind::SubscriptionBatch,
            &SubscriptionBatch {
                frontier,
                snapshot: first,
                last,
                columns: first.then(|| vec!["n".into()]),
                entries: vec![(1, vec![WireValue::Int(i64::try_from(frontier).unwrap())])],
            },
        )
    }
    fn script(frames: Vec<Frame>) -> ScriptIo {
        script_with_window(frames, 65536, 32)
    }

    fn script_with_window(frames: Vec<Frame>, bytes: u64, rows: u64) -> ScriptIo {
        let limits = FrameLimits::new(4096).unwrap();
        let mut input = Vec::new();
        for frame in [
            Frame::new(
                FrameKind::HelloAck,
                1,
                StreamId::CONTROL,
                Binding::Transport,
                HelloAck {
                    version: crate::PROTOCOL_VERSION,
                    server_nonce: [0; 32],
                    max_frame_len: 4096,
                    initial_window_bytes: bytes,
                    initial_window_rows: rows,
                }
                .encode()
                .unwrap(),
                limits,
            )
            .unwrap(),
            Frame::new(
                FrameKind::AuthOk,
                2,
                StreamId::CONTROL,
                Binding::Transport,
                AuthOk { session: session() }.encode().unwrap(),
                limits,
            )
            .unwrap(),
            Frame::new(
                FrameKind::Ready,
                3,
                StreamId::CONTROL,
                Binding::Session(session()),
                ready().encode().unwrap(),
                limits,
            )
            .unwrap(),
        ]
        .into_iter()
        .chain(frames)
        {
            input.extend(frame.encode(limits).unwrap());
        }
        ScriptIo {
            input,
            offset: 0,
            written: std::sync::Arc::default(),
        }
    }

    fn rpc(kind: FrameKind, request: u64, stream: StreamId, body: &impl Body) -> Frame {
        Frame::new(
            kind,
            request,
            stream,
            Binding::Ready(ready().binding(session())),
            body.encode().unwrap(),
            FrameLimits::new(4096).unwrap(),
        )
        .unwrap()
    }

    fn result_chunk(request: u64, values: &[i64]) -> Frame {
        rpc(
            FrameKind::SnapshotResultChunk,
            request,
            StreamId([5; 16]),
            &ResultChunk {
                columns: Some(vec!["value".into()]),
                rows: values.iter().map(|v| vec![WireValue::Int(*v)]).collect(),
            },
        )
    }

    fn result_end(request: u64, rows: u64) -> Frame {
        rpc(
            FrameKind::SnapshotResultEnd,
            request,
            StreamId([5; 16]),
            &ResultEnd {
                outcome: Outcome::Rows { seq: 3 },
                rows,
            },
        )
    }

    fn sent_frames(bytes: &[u8]) -> Vec<Frame> {
        let mut decoder = crate::Decoder::new(FrameLimits::new(4096).unwrap());
        let mut at = 0;
        let mut frames = Vec::new();
        while at < bytes.len() {
            let part = decoder.decode(&bytes[at..], |_| Ok(())).unwrap();
            assert!(part.consumed > 0);
            at += part.consumed;
            frames.push(part.frame.expect("complete sent frame"));
        }
        frames
    }

    fn diff_chunk(request: u64, columns: bool, entries: Vec<(i128, Vec<WireValue>)>) -> Frame {
        rpc(
            FrameKind::DiffResultChunk,
            request,
            StreamId([5; 16]),
            &DiffResultChunk {
                columns: columns.then(|| vec!["value".into()]),
                entries,
            },
        )
    }

    fn conditional_end(request: u64, outcome: Outcome, rows: u64) -> Frame {
        rpc(
            FrameKind::SnapshotResultEnd,
            request,
            StreamId([5; 16]),
            &ResultEnd { outcome, rows },
        )
    }

    fn read_batch_part(index: u32, seq: u64, first: bool, last: bool, values: &[i64]) -> Frame {
        output(
            FrameKind::ReadBatchResultChunk,
            &ReadBatchResultChunk {
                index,
                seq,
                last,
                chunk: ResultChunk {
                    columns: first.then(|| vec!["value".into()]),
                    rows: values
                        .iter()
                        .map(|value| vec![WireValue::Int(*value)])
                        .collect(),
                },
            },
        )
    }

    fn read_batch_end(results: u32, rows: u64) -> Frame {
        output(
            FrameKind::ReadBatchResultEnd,
            &ReadBatchResultEnd { results, rows },
        )
    }

    #[test]
    fn consistent_read_batches_preserve_order_empty_results_exact_cuts_and_shared_credit() {
        let ((), report) = run_async_under_lab(0x79a0_04b0, |root| async move {
            let io = script_with_window(
                vec![
                    read_batch_part(0, u64::MAX, true, false, &[10]),
                    read_batch_part(0, u64::MAX, false, true, &[20]),
                    read_batch_part(1, 7, true, true, &[]),
                    read_batch_part(2, 11, true, true, &[30]),
                    read_batch_end(3, 3),
                ],
                8192,
                1,
            );
            let written = std::sync::Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "test").await.unwrap();
            let arguments = vec![
                ("z".into(), WireValue::Int(2)),
                ("a".into(), WireValue::Int(1)),
            ];
            let answer = client
                .execute_read_batch(&root, "RETURN $a AS value", vec![arguments; 3])
                .await
                .unwrap();
            assert_eq!(
                answer.end,
                ReadBatchResultEnd {
                    results: 3,
                    rows: 3
                }
            );
            assert_eq!(answer.results.len(), 3);
            assert_eq!(answer.results[0].seq, u64::MAX);
            assert_eq!(answer.results[1].seq, 7);
            assert_eq!(answer.results[2].seq, 11);
            assert!(
                answer
                    .results
                    .iter()
                    .all(|result| result.columns == ["value"])
            );
            assert_eq!(
                answer.results[0].rows,
                vec![vec![WireValue::Int(10)], vec![WireValue::Int(20)]]
            );
            assert!(answer.results[1].rows.is_empty());
            assert_eq!(answer.results[2].rows, vec![vec![WireValue::Int(30)]]);
            let sent = sent_frames(&written.lock().unwrap());
            assert_eq!(sent[3].header().kind(), FrameKind::ExecuteReadBatch);
            let request = ExecuteBatch::decode(sent[3].payload()).unwrap();
            assert_eq!(request.argument_sets.len(), 3);
            assert!(
                request
                    .argument_sets
                    .iter()
                    .all(|set| set[0].0 == "a" && set[1].0 == "z")
            );
            let updates = sent[4..]
                .iter()
                .map(|frame| {
                    assert_eq!(frame.header().kind(), FrameKind::WindowUpdate);
                    WindowUpdate::decode(frame.payload()).unwrap()
                })
                .collect::<Vec<_>>();
            assert_eq!(updates.len(), 3);
            for (index, update) in updates.iter().enumerate() {
                assert_eq!(update.sequence, index as u64 + 1);
                assert_eq!(update.rows, 1);
                assert!(update.bytes > 0 && update.bytes <= 8192);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn prepared_read_batch_client_keeps_exact_handle_cuts_and_fresh_canonical_arguments() {
        let ((), report) = run_async_under_lab(0x79a0_04b8, |root| async move {
            let io = script_with_window(
                vec![
                    read_batch_part(0, 17, true, false, &[4]),
                    read_batch_part(0, 17, false, true, &[5]),
                    read_batch_part(1, 2, true, true, &[]),
                    read_batch_end(2, 2),
                ],
                8192,
                1,
            );
            let written = std::sync::Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "test").await.unwrap();
            let handle = PreparedHandle([0x2d; 16]);
            let answer = client
                .execute_prepared_read_batch(
                    &root,
                    handle,
                    vec![
                        vec![
                            ("z".into(), WireValue::Int(4)),
                            ("a".into(), WireValue::Int(3)),
                        ],
                        vec![("a".into(), WireValue::Int(7))],
                    ],
                )
                .await
                .unwrap();
            assert_eq!((answer.end.results, answer.end.rows), (2, 2));
            assert_eq!(answer.results[0].seq, 17);
            assert_eq!(answer.results[1].seq, 2);
            assert_eq!(
                answer.results[0].rows,
                vec![vec![WireValue::Int(4)], vec![WireValue::Int(5)]]
            );
            assert!(answer.results[1].rows.is_empty());
            let sent = sent_frames(&written.lock().unwrap());
            assert_eq!(sent[3].header().kind(), FrameKind::ExecutePreparedReadBatch);
            let request = ExecutePreparedReadBatch::decode(sent[3].payload()).unwrap();
            assert_eq!(request.handle, handle);
            assert_eq!(
                request.argument_sets,
                vec![
                    vec![
                        ("a".into(), WireValue::Int(3)),
                        ("z".into(), WireValue::Int(4)),
                    ],
                    vec![("a".into(), WireValue::Int(7))],
                ]
            );
            assert!(sent[4..].iter().all(|frame| {
                frame.header().kind() == FrameKind::WindowUpdate
            }));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn prepared_read_batch_local_refusals_send_nothing_and_leave_the_connection_usable() {
        let ((), report) = run_async_under_lab(0x79a0_04b9, |root| async move {
            let io = script(vec![
                read_batch_part(0, 3, true, true, &[7]),
                read_batch_end(1, 1),
            ]);
            let written = std::sync::Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "test").await.unwrap();
            let handle = PreparedHandle([0x2d; 16]);
            for (selected, arguments) in [
                (handle, vec![]),
                (handle, vec![vec![]; 33]),
                (PreparedHandle([0; 16]), vec![vec![]]),
                (
                    handle,
                    vec![vec![("x".into(), WireValue::Null), ("x".into(), WireValue::Null)]],
                ),
            ] {
                assert!(matches!(
                    client.execute_prepared_read_batch(&root, selected, arguments).await,
                    Err(ClientError::Body(_))
                ));
                assert_eq!(sent_frames(&written.lock().unwrap()).len(), 3);
            }
            let answer = client
                .execute_prepared_read_batch(&root, handle, vec![vec![]])
                .await
                .unwrap();
            assert_eq!(answer.results[0].rows, vec![vec![WireValue::Int(7)]]);
            let sent = sent_frames(&written.lock().unwrap());
            assert_eq!(sent[3].header().request_id(), 4);
            assert_eq!(sent[3].header().kind(), FrameKind::ExecutePreparedReadBatch);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn read_batch_callback_errors_cancel_once_suppress_later_schemas_and_allow_reuse() {
        let ((), report) = run_async_under_lab(0x79a0_04b1, |root| async move {
            for stage in 0..3 {
                for cancelled in [false, true] {
                    let mut responses = vec![read_batch_part(0, 5, true, true, &[1])];
                    if cancelled {
                        responses.push(output(
                            FrameKind::Error,
                            &ErrorBody {
                                code: ErrorCode::Cancelled,
                                message: "cancelled".into(),
                            },
                        ));
                    } else {
                        responses.extend([
                            read_batch_part(1, 6, true, true, &[2]),
                            read_batch_end(2, 2),
                        ]);
                    }
                    // Cancellation takes request 5. The original terminal is
                    // drained before the unrelated request 6 can be sent.
                    responses.extend([result_chunk(6, &[7]), result_end(6, 1)]);
                    let io = script(responses);
                    let written = std::sync::Arc::clone(&io.written);
                    let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                    client.select(&root, "test").await.unwrap();
                    let mut columns = 0;
                    let mut rows = 0;
                    let mut ends = 0;
                    let result = client
                        .execute_read_batch_streaming(
                            &root,
                            "RETURN 1 AS value",
                            vec![vec![], vec![]],
                            |index, seq, names| {
                                assert_eq!((index, seq), (0, 5));
                                assert_eq!(names, ["value"]);
                                columns += 1;
                                if stage == 0 {
                                    Err(ClientError::Protocol("sink stopped"))
                                } else {
                                    Ok(())
                                }
                            },
                            |index, row| {
                                assert_eq!(index, 0);
                                assert_eq!(row, vec![WireValue::Int(1)]);
                                rows += 1;
                                if stage == 1 {
                                    Err(ClientError::Protocol("sink stopped"))
                                } else {
                                    Ok(())
                                }
                            },
                            |index, seq, count| {
                                assert_eq!((index, seq, count), (0, 5, 1));
                                ends += 1;
                                Err(ClientError::Protocol("sink stopped"))
                            },
                        )
                        .await;
                    assert!(matches!(result, Err(ClientError::Protocol("sink stopped"))));
                    assert_eq!(
                        (columns, rows, ends),
                        (1, usize::from(stage > 0), usize::from(stage > 1))
                    );
                    let next = client
                        .execute(&root, ExecuteMode::Read, "RETURN 7 AS value", vec![])
                        .await
                        .unwrap();
                    assert_eq!(next.rows, vec![vec![WireValue::Int(7)]]);
                    let sent = sent_frames(&written.lock().unwrap());
                    assert_eq!(
                        sent.iter()
                            .map(|frame| frame.header().kind())
                            .collect::<Vec<_>>(),
                        vec![
                            FrameKind::Hello,
                            FrameKind::Auth,
                            FrameKind::SelectDatabase,
                            FrameKind::ExecuteReadBatch,
                            FrameKind::QueryCancel,
                            FrameKind::Execute
                        ]
                    );
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn malformed_read_batch_delivery_fences_connection_without_fallback_or_retry() {
        let ((), report) = run_async_under_lab(0x79a0_04b2, |root| async move {
            let wrong_width = output(
                FrameKind::ReadBatchResultChunk,
                &ReadBatchResultChunk {
                    index: 0,
                    seq: 1,
                    last: true,
                    chunk: ResultChunk {
                        columns: Some(vec!["value".into()]),
                        rows: vec![vec![]],
                    },
                },
            );
            let mut traces = vec![
                vec![read_batch_end(2, 0)],
                vec![read_batch_part(1, 1, true, true, &[])],
                vec![read_batch_part(0, 1, false, true, &[])],
                vec![wrong_width],
                vec![
                    read_batch_part(0, 1, true, false, &[1]),
                    read_batch_part(0, 2, false, true, &[2]),
                ],
                vec![
                    read_batch_part(0, 1, true, true, &[1]),
                    read_batch_part(0, 1, true, true, &[2]),
                ],
                vec![
                    read_batch_part(0, 1, true, false, &[1]),
                    read_batch_end(2, 1),
                ],
                vec![
                    read_batch_part(0, 1, true, true, &[1]),
                    read_batch_part(1, 1, true, true, &[]),
                    read_batch_end(1, 1),
                ],
                vec![
                    read_batch_part(0, 1, true, true, &[1]),
                    read_batch_part(1, 1, true, true, &[]),
                    read_batch_end(2, 2),
                ],
            ];
            let valid =
                ReadBatchResultChunk::decode(read_batch_part(0, 1, true, true, &[1]).payload())
                    .unwrap();
            for (request, stream) in [(5, StreamId([5; 16])), (4, StreamId::CONTROL)] {
                traces.push(vec![rpc(
                    FrameKind::ReadBatchResultChunk,
                    request,
                    stream,
                    &valid,
                )]);
            }
            for responses in traces {
                let io = script(responses);
                let written = std::sync::Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                assert!(matches!(
                    client
                        .execute_read_batch(&root, "RETURN 1 AS value", vec![vec![], vec![]])
                        .await,
                    Err(ClientError::Protocol(_))
                ));
                let before = written.lock().unwrap().len();
                requires_reconnect(client.ping(&root, 1).await);
                assert_eq!(written.lock().unwrap().len(), before);
                let sent = sent_frames(&written.lock().unwrap());
                assert_eq!(sent.len(), 4, "no fallback, retry or unexpected credit");
                assert_eq!(sent[3].header().kind(), FrameKind::ExecuteReadBatch);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn read_batch_credit_overruns_refuse_before_callbacks_and_late_eof_is_incomplete() {
        let ((), report) = run_async_under_lab(0x79a0_04b3, |root| async move {
            for row_credit in [false, true] {
                let first = read_batch_part(0, 1, true, true, &[1, 2]);
                let bytes = if row_credit {
                    8192
                } else {
                    first.header().frame_len() as u64 - 1
                };
                let io = script_with_window(vec![first], bytes, if row_credit { 1 } else { 4 });
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                let seen = AtomicUsize::new(0);
                let result = client
                    .execute_read_batch_streaming(
                        &root,
                        "RETURN 1",
                        vec![vec![]],
                        |_, _, _| {
                            seen.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        },
                        |_, _| {
                            seen.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        },
                        |_, _, _| {
                            seen.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        },
                    )
                    .await;
                assert!(matches!(
                    result,
                    Err(ClientError::Protocol("server exceeded its flow credit"))
                ));
                assert_eq!(seen.load(Ordering::SeqCst), 0);
            }
            let io = script(vec![read_batch_part(0, 1, true, true, &[1])]);
            let written = std::sync::Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "test").await.unwrap();
            assert!(matches!(
                client
                    .execute_read_batch(&root, "RETURN 1", vec![vec![]])
                    .await,
                Err(ClientError::Closed)
            ));
            let before = written.lock().unwrap().len();
            requires_reconnect(
                client
                    .execute_read_batch(&root, "RETURN 1", vec![vec![]])
                    .await,
            );
            assert_eq!(written.lock().unwrap().len(), before);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn dropped_requests_fence_every_new_operation_before_any_more_bytes() {
        let ((), report) = run_async_under_lab(0x79a0_0460, |root| async move {
            for partial_write in [false, true] {
                for operation in 0..8 {
                    let inner = script(vec![]);
                    let written = std::sync::Arc::clone(&inner.written);
                    let write_budget = std::sync::Arc::new(AtomicUsize::new(usize::MAX));
                    let io = PausedIo {
                        inner,
                        write_budget: std::sync::Arc::clone(&write_budget),
                    };
                    let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                    client.select(&root, "test").await.unwrap();
                    let before = written.lock().unwrap().len();
                    if partial_write {
                        write_budget.store(17, Ordering::SeqCst);
                    }
                    {
                        let mut pending = Box::pin(async {
                            match operation {
                                0 => client
                                    .execute(&root, ExecuteMode::Write, "CREATE ()", vec![])
                                    .await
                                    .map(|_| ()),
                                1 => client
                                    .execute_diff(
                                        &root,
                                        DiffMode::Buffered,
                                        1,
                                        2,
                                        "RETURN 1",
                                        vec![],
                                    )
                                    .await
                                    .map(|_| ()),
                                2 => client
                                    .subscribe(
                                        &root,
                                        "SUBSCRIBE TO RETURN 1",
                                        vec![],
                                        |_| {},
                                        |_| Ok(true),
                                    )
                                    .await
                                    .map(|_| ()),
                                3 => client.refresh_authority(&root, vec![2]).await.map(|_| ()),
                                4 => client
                                    .prepare_read(&root, "RETURN 1", vec![])
                                    .await
                                    .map(|_| ()),
                                5 => client.ping(&root, 1).await,
                                6 => client
                                    .execute_write_if_current(&root, 0, "CREATE ()", vec![])
                                    .await
                                    .map(|_| ()),
                                _ => client
                                    .execute_read_batch(&root, "RETURN 1", vec![vec![]])
                                    .await
                                    .map(|_| ()),
                            }
                        });
                        std::future::poll_fn(|task| {
                            assert!(std::future::Future::poll(pending.as_mut(), task).is_pending());
                            Poll::Ready(())
                        })
                        .await;
                        // Dropping loses the operation-local receiver state.
                    }
                    let after = written.lock().unwrap().len();
                    if partial_write {
                        assert_eq!(after - before, 17);
                    } else {
                        let bytes = written.lock().unwrap();
                        assert_eq!(sent_frames(&bytes[before..]).len(), 1);
                    }
                    let handle = PreparedHandle([9; 16]);
                    requires_reconnect(
                        client
                            .execute(&root, ExecuteMode::Write, "CREATE ()", vec![])
                            .await,
                    );
                    requires_reconnect(
                        client.execute_batch(&root, "CREATE ()", vec![vec![]]).await,
                    );
                    requires_reconnect(
                        client
                            .execute_read_batch(&root, "RETURN 1", vec![vec![]])
                            .await,
                    );
                    requires_reconnect(client.prepare_read(&root, "RETURN 1", vec![]).await);
                    requires_reconnect(client.release_prepared(&root, handle).await);
                    requires_reconnect(client.execute_prepared(&root, handle, vec![]).await);
                    requires_reconnect(client.execute_prepared_write(&root, handle, vec![]).await);
                    requires_reconnect(
                        client
                            .execute_prepared_write_returning(&root, handle, vec![])
                            .await,
                    );
                    requires_reconnect(
                        client
                            .execute_prepared_batch(&root, handle, vec![vec![]])
                            .await,
                    );
                    requires_reconnect(
                        client
                            .execute_diff(&root, DiffMode::Buffered, 1, 2, "RETURN 1", vec![])
                            .await,
                    );
                    requires_reconnect(
                        client
                            .execute_prepared_diff(&root, handle, DiffMode::Buffered, 1, 2, vec![])
                            .await,
                    );
                    requires_reconnect(
                        client
                            .execute_write_if_current(&root, 0, "CREATE ()", vec![])
                            .await,
                    );
                    requires_reconnect(
                        client
                            .subscribe(
                                &root,
                                "SUBSCRIBE TO RETURN 1",
                                vec![],
                                |_| {},
                                |_| Ok(false),
                            )
                            .await,
                    );
                    requires_reconnect(client.refresh_authority(&root, vec![2]).await);
                    requires_reconnect(client.ping(&root, 2).await);
                    requires_reconnect(client.close(&root).await);
                    assert_eq!(written.lock().unwrap().len(), after);
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn unsent_local_failures_and_unpolled_futures_leave_the_connection_idle() {
        let ((), report) = run_async_under_lab(0x79a0_0461, |root| async move {
            let io = script(vec![
                rpc(FrameKind::Pong, 4, StreamId::CONTROL, &Ping { nonce: 7 }),
                rpc(FrameKind::Pong, 5, StreamId::CONTROL, &Ping { nonce: 8 }),
            ]);
            let written = std::sync::Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "test").await.unwrap();
            let before = written.lock().unwrap().len();
            drop(client.execute(&root, ExecuteMode::Write, "CREATE ()", vec![]));
            assert!(matches!(
                client
                    .execute(
                        &root,
                        ExecuteMode::Write,
                        "CREATE ()",
                        vec![
                            ("x".into(), WireValue::Int(1)),
                            ("x".into(), WireValue::Int(2))
                        ],
                    )
                    .await,
                Err(ClientError::Body(BodyError::Noncanonical))
            ));
            assert!(matches!(
                client
                    .execute(&root, ExecuteMode::Write, &"x".repeat(8192), vec![])
                    .await,
                Err(ClientError::Protocol(
                    "request exceeds the negotiated frame limit"
                ))
            ));
            assert_eq!(written.lock().unwrap().len(), before);
            client.ping(&root, 7).await.unwrap();
            client.ping(&root, 8).await.unwrap();
            let bytes = written.lock().unwrap();
            let sent = sent_frames(&bytes[before..]);
            assert_eq!(sent.len(), 2);
            assert_eq!(sent[0].header().request_id(), 4);
            assert_eq!(sent[1].header().request_id(), 5);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn invalid_or_lost_responses_keep_the_request_fenced() {
        let ((), report) = run_async_under_lab(0x79a0_0462, |root| async move {
            for scenario in 0..7 {
                let frames = match scenario {
                    0 => vec![rpc(
                        FrameKind::Pong,
                        4,
                        StreamId::CONTROL,
                        &Ping { nonce: 99 },
                    )],
                    1 => vec![rpc(FrameKind::Pong, 4, StreamId::CONTROL, &Empty)],
                    2 => vec![rpc(
                        FrameKind::Pong,
                        5,
                        StreamId::CONTROL,
                        &Ping { nonce: 1 },
                    )],
                    3 => vec![rpc(
                        FrameKind::Pong,
                        4,
                        StreamId([5; 16]),
                        &Ping { nonce: 1 },
                    )],
                    4 => vec![rpc(FrameKind::Error, 4, StreamId::CONTROL, &Empty)],
                    5 => vec![result_chunk(4, &[1]), result_end(4, 0)],
                    _ => vec![],
                };
                let io = script(frames);
                let written = std::sync::Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                let result = if scenario >= 5 {
                    client
                        .execute(&root, ExecuteMode::Read, "RETURN 1", vec![])
                        .await
                        .map(|_| ())
                } else {
                    client.ping(&root, 1).await
                };
                assert!(result.is_err());
                let before = written.lock().unwrap().len();
                requires_reconnect(
                    client
                        .execute(&root, ExecuteMode::Write, "CREATE ()", vec![])
                        .await,
                );
                requires_reconnect(client.ping(&root, 2).await);
                assert_eq!(written.lock().unwrap().len(), before);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn decoded_control_refusals_release_only_their_matching_request() {
        let ((), report) = run_async_under_lab(0x79a0_0463, |root| async move {
            for request in [4, 3] {
                let io = script(vec![
                    rpc(
                        FrameKind::Error,
                        request,
                        StreamId::CONTROL,
                        &ErrorBody {
                            code: ErrorCode::Statement,
                            message: "statement refused".into(),
                        },
                    ),
                    rpc(FrameKind::Pong, 5, StreamId::CONTROL, &Ping { nonce: 7 }),
                ]);
                let written = std::sync::Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                let refused = client.prepare_read(&root, "RETURN 1", vec![]).await;
                if request == 4 {
                    assert!(matches!(
                        refused,
                        Err(ClientError::Server {
                            code: ErrorCode::Statement,
                            ..
                        })
                    ));
                    client.ping(&root, 7).await.unwrap();
                    assert_eq!(sent_frames(&written.lock().unwrap()).len(), 5);
                } else {
                    assert!(matches!(
                        refused,
                        Err(ClientError::Protocol("reply for a different request"))
                    ));
                    let before = written.lock().unwrap().len();
                    requires_reconnect(client.ping(&root, 7).await);
                    assert_eq!(written.lock().unwrap().len(), before);
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn exhausted_request_ids_refuse_without_writing_or_wrapping() {
        let ((), report) = run_async_under_lab(0x79a0_0464, |root| async move {
            let io = script(vec![]);
            let written = std::sync::Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "test").await.unwrap();
            client.next_request = u64::MAX;
            let before = written.lock().unwrap().len();
            for _ in 0..2 {
                assert!(matches!(
                    client.ping(&root, 1).await,
                    Err(ClientError::Protocol("request sequence exhausted"))
                ));
            }
            assert_eq!(written.lock().unwrap().len(), before);
            assert_eq!(client.next_request, u64::MAX);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn conditional_write_client_keeps_each_expected_frontier_and_returned_value() {
        let ((), report) = run_async_under_lab(0x79a0_0430, |root| async move {
            let io = script(vec![
                result_chunk(4, &[7]),
                conditional_end(
                    4,
                    Outcome::WriteCommitted {
                        seq: 1,
                        statements: 1,
                    },
                    1,
                ),
                result_chunk(5, &[19]),
                conditional_end(
                    5,
                    Outcome::WriteCommitted {
                        seq: 2,
                        statements: 1,
                    },
                    1,
                ),
                result_chunk(6, &[]),
                conditional_end(
                    6,
                    Outcome::ReadClosed {
                        seq: u64::MAX,
                        statements: 1,
                    },
                    0,
                ),
            ]);
            let written = std::sync::Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            assert!(matches!(
                client
                    .execute_write_if_current(&root, 0, "CREATE ()", vec![])
                    .await,
                Err(ClientError::Protocol("select a database first"))
            ));
            client.select(&root, "test").await.unwrap();
            for (expected_seq, value) in [(0, 7), (1, 19)] {
                let answer = client
                    .execute_write_if_current(
                        &root,
                        expected_seq,
                        "MATCH (n) SET n.p=$value RETURN n.p",
                        vec![
                            ("value".into(), WireValue::Int(value)),
                            ("a".into(), WireValue::Null),
                        ],
                    )
                    .await
                    .unwrap();
                assert_eq!(answer.columns, ["value"]);
                assert_eq!(answer.rows, [vec![WireValue::Int(value)]]);
                assert_eq!(
                    answer.outcome,
                    Outcome::WriteCommitted {
                        seq: expected_seq + 1,
                        statements: 1
                    }
                );
            }
            let empty = client
                .execute_write_if_current(
                    &root,
                    u64::MAX,
                    "MATCH (n) WHERE false SET n.p=1",
                    vec![],
                )
                .await
                .unwrap();
            assert!(empty.rows.is_empty());
            assert_eq!(
                empty.outcome,
                Outcome::ReadClosed {
                    seq: u64::MAX,
                    statements: 1
                }
            );
            let frames = sent_frames(&written.lock().unwrap());
            let requests: Vec<_> = frames
                .iter()
                .filter(|f| f.header().kind() == FrameKind::ExecuteConditionalWrite)
                .collect();
            assert_eq!(requests.len(), 3);
            for (request, expected_seq) in requests.iter().zip([0, 1, u64::MAX]) {
                assert!(request.header().stream_id().is_control());
                let body = ExecuteConditionalWrite::decode(request.payload()).unwrap();
                assert_eq!(body.expected_seq, expected_seq);
                if expected_seq != u64::MAX {
                    assert_eq!(
                        body.parameters,
                        [
                            ("a".into(), WireValue::Null),
                            (
                                "value".into(),
                                WireValue::Int(if expected_seq == 0 { 7 } else { 19 })
                            )
                        ]
                    );
                }
            }
            assert!(
                !frames
                    .iter()
                    .any(|f| f.header().kind() == FrameKind::Execute)
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn conditional_write_client_never_retries_conflict_unknown_or_disconnected_completion() {
        let ((), report) = run_async_under_lab(0x79a0_0431, |root| async move {
            for code in [
                Some(ErrorCode::Conflict),
                Some(ErrorCode::OutcomeUnknown),
                Some(ErrorCode::PermissionDenied),
                None,
            ] {
                let replies = code
                    .map(|code| {
                        output(
                            FrameKind::Error,
                            &ErrorBody {
                                code,
                                message: "conditional write refused or outcome unconfirmed".into(),
                            },
                        )
                    })
                    .into_iter()
                    .collect();
                let io = script(replies);
                let written = std::sync::Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                let result = client
                    .execute_write_if_current(&root, u64::MAX, "CREATE (:N)", vec![])
                    .await;
                match code {
                    Some(expected) => assert!(
                        matches!(result, Err(ClientError::Server { code, .. }) if code == expected)
                    ),
                    None => assert!(matches!(result, Err(ClientError::Closed))),
                }
                let frames = sent_frames(&written.lock().unwrap());
                assert_eq!(
                    frames
                        .iter()
                        .filter(|f| f.header().kind() == FrameKind::ExecuteConditionalWrite)
                        .count(),
                    1
                );
                assert!(!frames.iter().any(|f| matches!(
                    f.header().kind(),
                    FrameKind::Execute | FrameKind::Prepare | FrameKind::ExecutePrepared
                )));
            }
            let mut client = Client::connect_stream(
                &root,
                script(vec![result_chunk(4, &[1]), result_end(4, 1)]),
                vec![1],
            )
            .await
            .unwrap();
            client.select(&root, "test").await.unwrap();
            assert!(matches!(
                client
                    .execute_write_if_current(&root, 0, "CREATE (:N)", vec![])
                    .await,
                Err(ClientError::Protocol(
                    "conditional write returned a read outcome"
                ))
            ));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn conditional_write_returning_callback_cancellation_drains_without_replaying_effects() {
        let ((), report) = run_async_under_lab(0x79a0_0432, |root| async move {
            for columns_fail in [false, true] {
                for terminal_error in [false, true] {
                    let terminal = if terminal_error {
                        output(
                            FrameKind::Error,
                            &ErrorBody {
                                code: ErrorCode::Cancelled,
                                message: "delivery cancelled after native completion".into(),
                            },
                        )
                    } else {
                        conditional_end(
                            4,
                            Outcome::WriteCommitted {
                                seq: 1,
                                statements: 1,
                            },
                            2,
                        )
                    };
                    let io = script(vec![
                        result_chunk(4, &[7, 19]),
                        terminal,
                        result_chunk(6, &[19]),
                        result_end(6, 1),
                    ]);
                    let written = std::sync::Arc::clone(&io.written);
                    let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                    client.select(&root, "test").await.unwrap();
                    let mut rows_seen = 0;
                    let result = client
                        .execute_write_if_current_streaming(
                            &root,
                            0,
                            "CREATE (n) RETURN n.p",
                            vec![],
                            |_| {
                                if columns_fail {
                                    Err(ClientError::Protocol("consumer stopped"))
                                } else {
                                    Ok(())
                                }
                            },
                            |_| {
                                rows_seen += 1;
                                Err(ClientError::Protocol("consumer stopped"))
                            },
                        )
                        .await;
                    assert!(matches!(
                        result,
                        Err(ClientError::Protocol("consumer stopped"))
                    ));
                    assert_eq!(rows_seen, usize::from(!columns_fail));
                    let next = client
                        .execute(&root, ExecuteMode::Read, "RETURN 19", vec![])
                        .await
                        .unwrap();
                    assert_eq!(next.rows, [vec![WireValue::Int(19)]]);
                    let frames = sent_frames(&written.lock().unwrap());
                    assert_eq!(
                        frames
                            .iter()
                            .filter(|f| f.header().kind() == FrameKind::ExecuteConditionalWrite)
                            .count(),
                        1
                    );
                    let cancels: Vec<_> = frames
                        .iter()
                        .filter(|f| f.header().kind() == FrameKind::QueryCancel)
                        .collect();
                    assert_eq!(cancels.len(), 1);
                    assert_eq!(cancels[0].header().stream_id(), StreamId([5; 16]));
                    assert!(
                        !frames
                            .iter()
                            .any(|f| f.header().kind() == FrameKind::WindowUpdate)
                    );
                    assert_eq!(
                        frames
                            .iter()
                            .filter(|f| f.header().kind() == FrameKind::Execute)
                            .count(),
                        1,
                        "only the explicitly requested subsequent read is sent"
                    );
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    fn diff_end(request: u64, before: u64, after: u64, rows: u64) -> Frame {
        rpc(
            FrameKind::DiffResultEnd,
            request,
            StreamId([5; 16]),
            &DiffResultEnd {
                before,
                after,
                rows,
            },
        )
    }

    #[test]
    fn finite_diff_clients_preserve_signed_values_exact_cuts_and_request_mode() {
        let ((), report) = run_async_under_lab(0x79a0_0420, |root| async move {
            for mode in [DiffMode::Buffered, DiffMode::OrderedStream] {
                for prepared in [false, true] {
                    let entries = vec![
                        (i128::MIN, vec![WireValue::Null]),
                        (i128::MAX, vec![WireValue::Int(7)]),
                    ];
                    let io = script(vec![
                        diff_chunk(4, true, entries.clone()),
                        diff_end(4, u64::MAX, 0, 2),
                    ]);
                    let written = std::sync::Arc::clone(&io.written);
                    let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                    client.select(&root, "test").await.unwrap();
                    let params = vec![
                        ("z".into(), WireValue::Int(9)),
                        ("a".into(), WireValue::Null),
                    ];
                    let handle = PreparedHandle([9; 16]);
                    let answer = if prepared {
                        client
                            .execute_prepared_diff(&root, handle, mode, u64::MAX, 0, params)
                            .await
                    } else {
                        client
                            .execute_diff(&root, mode, u64::MAX, 0, "RETURN $a, $z", params)
                            .await
                    }
                    .unwrap();
                    assert_eq!(answer.columns, ["value"]);
                    assert_eq!(answer.entries, entries);
                    assert_eq!(
                        answer.end,
                        DiffResultEnd {
                            before: u64::MAX,
                            after: 0,
                            rows: 2
                        }
                    );
                    let frames = sent_frames(&written.lock().unwrap());
                    let requests: Vec<_> = frames
                        .iter()
                        .filter(|f| {
                            matches!(
                                f.header().kind(),
                                FrameKind::ExecuteDiff | FrameKind::ExecutePreparedDiff
                            )
                        })
                        .collect();
                    assert_eq!(requests.len(), 1);
                    assert!(requests[0].header().stream_id().is_control());
                    let (actual_mode, before, after, params) = if prepared {
                        let request = ExecutePreparedDiff::decode(requests[0].payload()).unwrap();
                        assert_eq!(request.handle, handle);
                        (
                            request.mode,
                            request.before,
                            request.after,
                            request.parameters,
                        )
                    } else {
                        let request = ExecuteDiff::decode(requests[0].payload()).unwrap();
                        assert_eq!(request.statement, "RETURN $a, $z");
                        (
                            request.mode,
                            request.before,
                            request.after,
                            request.parameters,
                        )
                    };
                    assert_eq!((actual_mode, before, after), (mode, u64::MAX, 0));
                    assert_eq!(
                        params,
                        [
                            ("a".into(), WireValue::Null),
                            ("z".into(), WireValue::Int(9))
                        ]
                    );
                }
            }
            let io = script(vec![diff_chunk(4, true, vec![]), diff_end(4, 0, 0, 0)]);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "test").await.unwrap();
            let answer = client
                .execute_diff(&root, DiffMode::Buffered, 0, 0, "RETURN 1", vec![])
                .await
                .unwrap();
            assert_eq!(answer.columns, ["value"]);
            assert!(answer.entries.is_empty());
            assert_eq!(answer.end.rows, 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn finite_diff_credit_counts_changed_tuples_and_replenishes_before_eof_without_retry() {
        let ((), report) = run_async_under_lab(0x79a0_0421, |root| async move {
            for fails in [false, true] {
                let entries: Vec<_> = (0..32)
                    .map(|v| {
                        (
                            if v % 2 == 0 { i128::MAX } else { i128::MIN },
                            vec![WireValue::Int(v)],
                        )
                    })
                    .collect();
                let chunk = diff_chunk(4, true, entries);
                let cost = chunk.header().frame_len() as u64;
                let terminal = if fails {
                    output(
                        FrameKind::Error,
                        &ErrorBody {
                            code: ErrorCode::Budget,
                            message: "late diff refusal".into(),
                        },
                    )
                } else {
                    diff_end(4, 2, 5, 32)
                };
                let io = script(vec![chunk, terminal]);
                let written = std::sync::Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                let mut delivered = 0;
                let result = client
                    .execute_diff_streaming(
                        &root,
                        DiffMode::OrderedStream,
                        2,
                        5,
                        "RETURN 1",
                        vec![],
                        |_| Ok(()),
                        |_, _| {
                            delivered += 1;
                            Ok(())
                        },
                    )
                    .await;
                assert_eq!(delivered, 32);
                if fails {
                    assert!(matches!(
                        result,
                        Err(ClientError::Server {
                            code: ErrorCode::Budget,
                            ..
                        })
                    ));
                } else {
                    assert_eq!(result.unwrap().rows, 32);
                }
                let frames = sent_frames(&written.lock().unwrap());
                assert_eq!(
                    frames
                        .iter()
                        .filter(|f| f.header().kind() == FrameKind::ExecuteDiff)
                        .count(),
                    1
                );
                let grants: Vec<_> = frames
                    .iter()
                    .filter(|f| f.header().kind() == FrameKind::WindowUpdate)
                    .collect();
                assert_eq!(grants.len(), 1);
                assert_eq!(
                    WindowUpdate::decode(grants[0].payload()).unwrap(),
                    WindowUpdate {
                        sequence: 1,
                        bytes: cost,
                        rows: 32
                    }
                );
                assert_eq!(grants[0].header().stream_id(), StreamId([5; 16]));
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn finite_diff_refuses_wrong_completion_schema_stream_and_credit() {
        let ((), report) = run_async_under_lab(0x79a0_0422, |root| async move {
            let entry = || vec![(1, vec![WireValue::Int(1)])];
            let valid = || diff_chunk(4, true, entry());
            let chunk = DiffResultChunk {
                columns: Some(vec!["value".into()]),
                entries: entry(),
            };
            for frames in [
                vec![valid(), diff_end(4, 8, 3, 1)],
                vec![valid(), diff_end(4, 9, 4, 1)],
                vec![valid(), diff_end(4, 9, 3, 0)],
                vec![diff_end(4, 9, 3, 0)],
                vec![diff_chunk(4, false, entry())],
                vec![valid(), valid()],
                vec![diff_chunk(4, true, vec![(1, vec![])])],
                vec![diff_chunk(5, true, entry())],
                vec![
                    valid(),
                    rpc(
                        FrameKind::DiffResultEnd,
                        4,
                        StreamId([6; 16]),
                        &DiffResultEnd {
                            before: 9,
                            after: 3,
                            rows: 1,
                        },
                    ),
                ],
                vec![rpc(
                    FrameKind::DiffResultChunk,
                    4,
                    StreamId::CONTROL,
                    &chunk,
                )],
                vec![result_chunk(4, &[1])],
                vec![diff_chunk(4, true, vec![(1, vec![WireValue::Null]); 33])],
            ] {
                let io = script(frames);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                assert!(matches!(
                    client
                        .execute_diff(&root, DiffMode::Buffered, 9, 3, "RETURN 1", vec![])
                        .await,
                    Err(ClientError::Protocol(_))
                ));
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn finite_diff_end_requires_remaining_byte_credit_after_cancellation() {
        let ((), report) = run_async_under_lab(0x79a0_0424, |root| async move {
            let chunk = diff_chunk(4, true, vec![(1, vec![WireValue::Text("x".repeat(3700))])]);
            let terminal = diff_end(4, 1, 2, 1);
            assert!(chunk.header().frame_len() < 4096);
            assert!(4096 - chunk.header().frame_len() < terminal.header().frame_len());
            let io = script_with_window(vec![chunk, terminal], 4096, 32);
            let written = std::sync::Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "test").await.unwrap();
            let result = client
                .execute_diff_streaming(
                    &root,
                    DiffMode::OrderedStream,
                    1,
                    2,
                    "RETURN 1",
                    vec![],
                    |_| Err(ClientError::Protocol("consumer stopped")),
                    |_, _| panic!("cancelled before any row callback"),
                )
                .await;
            assert!(matches!(
                result,
                Err(ClientError::Protocol("server exceeded its flow credit"))
            ));
            let frames = sent_frames(&written.lock().unwrap());
            assert_eq!(
                frames
                    .iter()
                    .filter(|f| f.header().kind() == FrameKind::QueryCancel)
                    .count(),
                1
            );
            assert!(
                !frames
                    .iter()
                    .any(|f| f.header().kind() == FrameKind::WindowUpdate)
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn finite_diff_callback_cancel_drains_exactly_once_and_next_query_succeeds() {
        let ((), report) = run_async_under_lab(0x79a0_0423, |root| async move {
            for columns_fail in [false, true] {
                for terminal_error in [false, true] {
                    let terminal = if terminal_error {
                        output(
                            FrameKind::Error,
                            &ErrorBody {
                                code: ErrorCode::Cancelled,
                                message: "cancelled".into(),
                            },
                        )
                    } else {
                        diff_end(4, 1, 2, 3)
                    };
                    let io = script(vec![
                        diff_chunk(
                            4,
                            true,
                            vec![(1, vec![WireValue::Int(1)]), (-1, vec![WireValue::Int(2)])],
                        ),
                        diff_chunk(4, false, vec![(1, vec![WireValue::Int(3)])]),
                        terminal,
                        result_chunk(6, &[9]),
                        result_end(6, 1),
                    ]);
                    let written = std::sync::Arc::clone(&io.written);
                    let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                    client.select(&root, "test").await.unwrap();
                    let mut called = 0;
                    let result = client
                        .execute_diff_streaming(
                            &root,
                            DiffMode::OrderedStream,
                            1,
                            2,
                            "RETURN 1",
                            vec![],
                            |_| {
                                if columns_fail {
                                    Err(ClientError::Protocol("consumer stopped"))
                                } else {
                                    Ok(())
                                }
                            },
                            |_, _| {
                                called += 1;
                                Err(ClientError::Protocol("consumer stopped"))
                            },
                        )
                        .await;
                    assert!(matches!(
                        result,
                        Err(ClientError::Protocol("consumer stopped"))
                    ));
                    assert_eq!(called, usize::from(!columns_fail));
                    let next = client
                        .execute(&root, ExecuteMode::Read, "RETURN 9", vec![])
                        .await
                        .unwrap();
                    assert_eq!(next.rows, [vec![WireValue::Int(9)]]);
                    let frames = sent_frames(&written.lock().unwrap());
                    let cancels: Vec<_> = frames
                        .iter()
                        .filter(|f| f.header().kind() == FrameKind::QueryCancel)
                        .collect();
                    assert_eq!(cancels.len(), 1);
                    assert_eq!(cancels[0].header().stream_id(), StreamId([5; 16]));
                    assert!(
                        !frames
                            .iter()
                            .any(|f| f.header().kind() == FrameKind::WindowUpdate)
                    );
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn native_read_modes_replenish_exact_row_windows_and_never_retry_a_late_refusal() {
        let ((), report) = run_async_under_lab(0x79a0_0313, |root| async move {
            for (mode, fails) in [ExecuteMode::ReadStream, ExecuteMode::ReadOrdered]
                .into_iter()
                .flat_map(|mode| [false, true].map(|fails| (mode, fails)))
            {
                let values: Vec<i64> = (0..32).collect();
                let terminal = if fails {
                    rpc(
                        FrameKind::Error,
                        4,
                        StreamId([5; 16]),
                        &ErrorBody {
                            code: ErrorCode::Budget,
                            message: "source allowance exhausted after the delivered prefix".into(),
                        },
                    )
                } else {
                    result_end(4, 32)
                };
                let io = script(vec![result_chunk(4, &values), terminal]);
                let written = std::sync::Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                let mut delivered = Vec::new();
                let result = client
                    .execute_streaming(
                        &root,
                        mode,
                        "MATCH (n) RETURN n",
                        vec![],
                        |_| Ok(()),
                        |row| {
                            delivered.push(row);
                            Ok(())
                        },
                    )
                    .await;
                assert_eq!(delivered.len(), 32);
                if fails {
                    assert!(matches!(
                        result,
                        Err(ClientError::Server {
                            code: ErrorCode::Budget,
                            ..
                        })
                    ));
                } else {
                    assert_eq!(result.unwrap(), Outcome::Rows { seq: 3 });
                }
                let frames = sent_frames(&written.lock().unwrap());
                let executes: Vec<_> = frames
                    .iter()
                    .filter(|frame| frame.header().kind() == FrameKind::Execute)
                    .collect();
                assert_eq!(
                    executes.len(),
                    1,
                    "the requested physical mode must be sent only once"
                );
                assert_eq!(Execute::decode(executes[0].payload()).unwrap().mode, mode);
                let grants: Vec<_> = frames
                    .iter()
                    .filter(|frame| frame.header().kind() == FrameKind::WindowUpdate)
                    .collect();
                assert_eq!(
                    grants.len(),
                    1,
                    "natural EOF requires demand after an exact row window"
                );
                let grant = WindowUpdate::decode(grants[0].payload()).unwrap();
                assert_eq!(grant.rows, 32);
                assert_eq!(grant.sequence, 1);
                assert_eq!(grants[0].header().stream_id(), StreamId([5; 16]));
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn ordered_refusals_keep_the_requested_mode_and_leave_the_connection_usable() {
        let ((), report) = run_async_under_lab(0x79a0_0314, |root| async move {
            for prepared in [false, true] {
                let handle = PreparedHandle([0x32; 16]);
                let request = if prepared { 5 } else { 4 };
                let mut replies = Vec::new();
                if prepared {
                    replies.push(rpc(
                        FrameKind::Prepared,
                        4,
                        StreamId::CONTROL,
                        &Prepared { handle },
                    ));
                }
                replies.extend([
                    rpc(
                        FrameKind::Error,
                        request,
                        StreamId([5; 16]),
                        &ErrorBody {
                            code: ErrorCode::Statement,
                            message: "ordered execution profile is unsupported".into(),
                        },
                    ),
                    result_chunk(request + 1, &[20]),
                    result_end(request + 1, 1),
                ]);
                let io = script(replies);
                let written = std::sync::Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                let text = "MATCH (n) RETURN n.p AS value ORDER BY value LIMIT $limit";
                let parameters = vec![("limit".into(), WireValue::Count(2))];
                let callbacks = AtomicUsize::new(0);
                let result = if prepared {
                    assert_eq!(
                        client
                            .prepare_read(&root, text, parameters.clone())
                            .await
                            .unwrap(),
                        handle
                    );
                    client
                        .execute_prepared_streaming(
                            &root,
                            PreparedReadMode::OrderedStream,
                            handle,
                            parameters.clone(),
                            |_| {
                                callbacks.fetch_add(1, Ordering::Relaxed);
                                Ok(())
                            },
                            |_| {
                                callbacks.fetch_add(1, Ordering::Relaxed);
                                Ok(())
                            },
                        )
                        .await
                } else {
                    client
                        .execute_streaming(
                            &root,
                            ExecuteMode::ReadOrdered,
                            text,
                            parameters.clone(),
                            |_| {
                                callbacks.fetch_add(1, Ordering::Relaxed);
                                Ok(())
                            },
                            |_| {
                                callbacks.fetch_add(1, Ordering::Relaxed);
                                Ok(())
                            },
                        )
                        .await
                };
                assert!(matches!(
                    result,
                    Err(ClientError::Server {
                        code: ErrorCode::Statement,
                        ..
                    })
                ));
                assert_eq!(callbacks.load(Ordering::Relaxed), 0);
                let answer = client
                    .execute(&root, ExecuteMode::Read, "MATCH (n) RETURN n", vec![])
                    .await
                    .unwrap();
                assert_eq!(answer.rows, [vec![WireValue::Int(20)]]);
                let frames = sent_frames(&written.lock().unwrap());
                let executions: Vec<_> = frames
                    .iter()
                    .filter(|frame| {
                        matches!(
                            frame.header().kind(),
                            FrameKind::Execute | FrameKind::ExecutePrepared
                        )
                    })
                    .collect();
                assert_eq!(
                    executions.len(),
                    2,
                    "a refusal must not trigger an automatic mode retry"
                );
                if prepared {
                    let sent = ExecutePrepared::decode(executions[0].payload()).unwrap();
                    assert_eq!(sent.mode, PreparedReadMode::OrderedStream);
                    assert_eq!(sent.parameters, parameters);
                } else {
                    let sent = Execute::decode(executions[0].payload()).unwrap();
                    assert_eq!(sent.mode, ExecuteMode::ReadOrdered);
                    assert_eq!(sent.parameters, parameters);
                }
                assert_eq!(
                    Execute::decode(executions[1].payload()).unwrap().mode,
                    ExecuteMode::Read
                );
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn schema_refusal_cancels_before_rows_and_drains_for_connection_reuse() {
        let ((), report) = run_async_under_lab(0x79a0_0315, |root| async move {
            for (mode, prepared_mode) in [
                (ExecuteMode::ReadStream, PreparedReadMode::SourceStream),
                (ExecuteMode::ReadOrdered, PreparedReadMode::OrderedStream),
            ] {
                for (prepared, cancelled) in [false, true]
                    .into_iter()
                    .flat_map(|prepared| [false, true].map(|cancelled| (prepared, cancelled)))
                {
                    let handle = PreparedHandle([0x33; 16]);
                    let request = if prepared { 5 } else { 4 };
                    let stream = StreamId([5; 16]);
                    let mut replies = Vec::new();
                    if prepared {
                        replies.push(rpc(
                            FrameKind::Prepared,
                            4,
                            StreamId::CONTROL,
                            &Prepared { handle },
                        ));
                    }
                    replies.push(rpc(
                        FrameKind::SnapshotResultChunk,
                        request,
                        stream,
                        &ResultChunk {
                            columns: Some(vec!["value".into()]),
                            rows: vec![],
                        },
                    ));
                    if cancelled {
                        // A pending sort can observe cancellation before it
                        // produces any row; the client must not wait for one.
                        replies.push(rpc(
                            FrameKind::Error,
                            request,
                            stream,
                            &ErrorBody {
                                code: ErrorCode::Cancelled,
                                message: "private sort cancelled".into(),
                            },
                        ));
                    } else {
                        // A send already queued when cancellation arrives is
                        // still validated and drained, without row callbacks.
                        replies.push(rpc(
                            FrameKind::SnapshotResultChunk,
                            request,
                            stream,
                            &ResultChunk {
                                columns: None,
                                rows: vec![vec![WireValue::Int(1)]],
                            },
                        ));
                        replies.push(result_end(request, 1));
                    }
                    replies.extend([result_chunk(request + 2, &[20]), result_end(request + 2, 1)]);
                    let io = script(replies);
                    let written = std::sync::Arc::clone(&io.written);
                    let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                    client.select(&root, "test").await.unwrap();
                    let text = "MATCH (n) RETURN n.p AS value ORDER BY value";
                    let columns = AtomicUsize::new(0);
                    let rows = AtomicUsize::new(0);
                    let on_columns = |names: &[String]| {
                        assert_eq!(names, &["value"]);
                        columns.fetch_add(1, Ordering::Relaxed);
                        Err(ClientError::Io(std::io::ErrorKind::BrokenPipe))
                    };
                    let on_row = |_| {
                        rows.fetch_add(1, Ordering::Relaxed);
                        Ok(())
                    };
                    let result = if prepared {
                        client.prepare_read(&root, text, vec![]).await.unwrap();
                        client
                            .execute_prepared_streaming(
                                &root,
                                prepared_mode,
                                handle,
                                vec![],
                                on_columns,
                                on_row,
                            )
                            .await
                    } else {
                        client
                            .execute_streaming(&root, mode, text, vec![], on_columns, on_row)
                            .await
                    };
                    assert!(matches!(
                        result,
                        Err(ClientError::Io(std::io::ErrorKind::BrokenPipe))
                    ));
                    assert_eq!(
                        (
                            columns.load(Ordering::Relaxed),
                            rows.load(Ordering::Relaxed)
                        ),
                        (1, 0)
                    );
                    assert_eq!(
                        client
                            .execute(&root, ExecuteMode::Read, "MATCH (n) RETURN n", vec![])
                            .await
                            .unwrap()
                            .rows,
                        [vec![WireValue::Int(20)]]
                    );
                    let sent = sent_frames(&written.lock().unwrap());
                    let controls: Vec<_> = sent
                        .iter()
                        .filter(|frame| {
                            matches!(
                                frame.header().kind(),
                                FrameKind::QueryCancel | FrameKind::WindowUpdate
                            )
                        })
                        .collect();
                    assert_eq!(controls.len(), 1, "no credit follows a refused schema");
                    assert_eq!(controls[0].header().kind(), FrameKind::QueryCancel);
                    assert_eq!(controls[0].header().stream_id(), stream);
                    assert_eq!(controls[0].header().request_id(), request + 1);
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn atomic_batch_client_sends_once_and_preserves_unknown_outcomes() {
        let ((), report) = run_async_under_lab(0x79a0_0303, |root| async move {
            for scenario in 0..3 {
                let replies = match scenario {
                    0 => vec![
                        rpc(
                            FrameKind::SnapshotResultChunk,
                            4,
                            StreamId([5; 16]),
                            &ResultChunk {
                                columns: Some(vec![]),
                                rows: vec![],
                            },
                        ),
                        rpc(
                            FrameKind::SnapshotResultEnd,
                            4,
                            StreamId([5; 16]),
                            &ResultEnd {
                                outcome: Outcome::WriteCommitted {
                                    seq: 8,
                                    statements: 2,
                                },
                                rows: 0,
                            },
                        ),
                    ],
                    1 => vec![rpc(
                        FrameKind::Error,
                        4,
                        StreamId([5; 16]),
                        &ErrorBody {
                            code: ErrorCode::OutcomeUnknown,
                            message: "write outcome unknown; do not replay".into(),
                        },
                    )],
                    _ => vec![],
                };
                let io = script(replies);
                let written = std::sync::Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                let result = client
                    .execute_batch(
                        &root,
                        "CREATE (:Person {name:$name,age:$age})",
                        vec![
                            vec![
                                ("name".into(), WireValue::Text("Ann".into())),
                                ("age".into(), WireValue::Int(30)),
                            ],
                            vec![
                                ("name".into(), WireValue::Text("Bob".into())),
                                ("age".into(), WireValue::Int(25)),
                            ],
                        ],
                    )
                    .await;
                match scenario {
                    0 => assert_eq!(
                        result.unwrap(),
                        Outcome::WriteCommitted {
                            seq: 8,
                            statements: 2
                        }
                    ),
                    1 => assert!(matches!(
                        result,
                        Err(ClientError::Server {
                            code: ErrorCode::OutcomeUnknown,
                            ..
                        })
                    )),
                    _ => assert!(matches!(result, Err(ClientError::Closed))),
                }
                let sent = sent_frames(&written.lock().unwrap());
                assert_eq!(
                    sent.len(),
                    4,
                    "unknown outcome/EOF must never resend a write"
                );
                assert_eq!(sent[3].header().kind(), FrameKind::ExecuteBatch);
                let request = ExecuteBatch::decode(sent[3].payload()).unwrap();
                assert_eq!(
                    request.argument_sets,
                    vec![
                        vec![
                            ("age".into(), WireValue::Int(30)),
                            ("name".into(), WireValue::Text("Ann".into()))
                        ],
                        vec![
                            ("age".into(), WireValue::Int(25)),
                            ("name".into(), WireValue::Text("Bob".into()))
                        ],
                    ]
                );
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn prepared_replies_require_the_current_ready_control_binding() {
        let current = Binding::Ready(ready().binding(session()));
        let limits = FrameLimits::new(4096).unwrap();
        for kind in [FrameKind::Prepared, FrameKind::PreparedReleased] {
            let reply = rpc(
                kind,
                4,
                StreamId::CONTROL,
                &Prepared {
                    handle: PreparedHandle([0x31; 16]),
                },
            );
            assert_eq!(server_header(reply.header(), current), Ok(()));
            let child = rpc(
                kind,
                4,
                StreamId([5; 16]),
                &Prepared {
                    handle: PreparedHandle([0x31; 16]),
                },
            );
            assert_eq!(
                server_header(child.header(), current),
                Err(ProtocolError::InvalidState),
            );
            for unselected in [Binding::Transport, Binding::Session(session())] {
                let frame =
                    Frame::new(kind, 4, StreamId::CONTROL, unselected, vec![], limits).unwrap();
                assert_eq!(
                    server_header(frame.header(), unselected),
                    Err(ProtocolError::InvalidState),
                );
            }
            let mut stale = ready().binding(session());
            stale.session.auth_generation += 1;
            let frame = Frame::new(
                kind,
                4,
                StreamId::CONTROL,
                Binding::Ready(stale),
                vec![],
                limits,
            )
            .unwrap();
            assert_eq!(
                server_header(frame.header(), current),
                Err(ProtocolError::InvalidBinding),
            );
        }
    }

    #[test]
    fn prepared_returning_clients_preserve_results_and_never_retry_completion() {
        let ((), report) = run_async_under_lab(0x79a0_0331, |root| async move {
            for scenario in 0..5 {
                let handle = PreparedHandle([0x51; 16]);
                let mut replies = vec![rpc(
                    FrameKind::Prepared,
                    4,
                    StreamId::CONTROL,
                    &Prepared { handle },
                )];
                match scenario {
                    0 | 1 | 4 => {
                        replies.push(rpc(
                            FrameKind::SnapshotResultChunk,
                            5,
                            StreamId([5; 16]),
                            &ResultChunk {
                                columns: Some(vec!["value".into()]),
                                rows: vec![vec![WireValue::Int(23)]],
                            },
                        ));
                        replies.push(rpc(
                            FrameKind::SnapshotResultEnd,
                            5,
                            StreamId([5; 16]),
                            &ResultEnd {
                                outcome: match scenario {
                                    0 => Outcome::WriteCommitted {
                                        seq: 8,
                                        statements: 1,
                                    },
                                    1 => Outcome::ReadClosed {
                                        seq: 7,
                                        statements: 1,
                                    },
                                    _ => Outcome::Rows { seq: 7 },
                                },
                                rows: 1,
                            },
                        ));
                    }
                    2 => replies.push(rpc(
                        FrameKind::Error,
                        5,
                        StreamId([5; 16]),
                        &ErrorBody {
                            code: ErrorCode::OutcomeUnknown,
                            message: "write outcome unknown; do not replay".into(),
                        },
                    )),
                    _ => {}
                }
                let io = script(replies);
                let written = std::sync::Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                let prepared = client
                    .prepare_write_returning(
                        &root,
                        "CREATE (n) RETURN $value AS value",
                        vec![("value".into(), WireValue::Int(99))],
                    )
                    .await
                    .unwrap();
                assert_eq!(prepared, handle);
                assert!(matches!(
                    client
                        .execute_prepared_streaming(
                            &root,
                            PreparedReadMode::WriteReturning,
                            handle,
                            vec![],
                            |_| Ok(()),
                            |_| Ok(()),
                        )
                        .await,
                    Err(ClientError::Protocol(_))
                ));
                let result = client
                    .execute_prepared_write_returning(
                        &root,
                        handle,
                        vec![("value".into(), WireValue::Int(23))],
                    )
                    .await;
                match scenario {
                    0 | 1 => {
                        let answer = result.unwrap();
                        assert_eq!(answer.columns, ["value"]);
                        assert_eq!(answer.rows, [vec![WireValue::Int(23)]]);
                        assert_eq!(
                            answer.outcome,
                            if scenario == 0 {
                                Outcome::WriteCommitted {
                                    seq: 8,
                                    statements: 1,
                                }
                            } else {
                                Outcome::ReadClosed {
                                    seq: 7,
                                    statements: 1,
                                }
                            }
                        );
                    }
                    2 => assert!(matches!(
                        result,
                        Err(ClientError::Server {
                            code: ErrorCode::OutcomeUnknown,
                            ..
                        })
                    )),
                    3 => assert!(matches!(result, Err(ClientError::Closed))),
                    _ => assert!(matches!(result, Err(ClientError::Protocol(_)))),
                }
                let frames = sent_frames(&written.lock().unwrap());
                assert_eq!(frames.len(), 5, "no fallback, reprepare or write replay");
                let prepare = Prepare::decode(frames[3].payload()).unwrap();
                assert_eq!(prepare.mode, PrepareMode::WriteReturning);
                let execution = ExecutePrepared::decode(frames[4].payload()).unwrap();
                assert_eq!(execution.mode, PreparedReadMode::WriteReturning);
                assert_eq!(execution.handle, handle);
                assert_eq!(execution.parameters, [("value".into(), WireValue::Int(23))]);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn prepared_returning_callback_refusal_drains_delivery_before_next_request() {
        let ((), report) = run_async_under_lab(0x79a0_0332, |root| async move {
            let handle = PreparedHandle([0x52; 16]);
            let io = script(vec![
                rpc(
                    FrameKind::Prepared,
                    4,
                    StreamId::CONTROL,
                    &Prepared { handle },
                ),
                result_chunk(5, &[10, 11, 12]),
                rpc(
                    FrameKind::SnapshotResultEnd,
                    5,
                    StreamId([5; 16]),
                    &ResultEnd {
                        outcome: Outcome::WriteCommitted {
                            seq: 8,
                            statements: 1,
                        },
                        rows: 3,
                    },
                ),
                result_chunk(7, &[20]),
                result_end(7, 1),
            ]);
            let written = std::sync::Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "test").await.unwrap();
            client
                .prepare_write_returning(&root, "CREATE (n) RETURN n", vec![])
                .await
                .unwrap();
            let mut seen = 0;
            let result = client
                .execute_prepared_write_returning_streaming(
                    &root,
                    handle,
                    vec![],
                    |_| Ok(()),
                    |_| {
                        seen += 1;
                        Err(ClientError::Protocol("consumer stopped"))
                    },
                )
                .await;
            assert!(matches!(
                result,
                Err(ClientError::Protocol("consumer stopped"))
            ));
            assert_eq!(seen, 1);
            let next = client
                .execute(&root, ExecuteMode::Read, "RETURN 20", vec![])
                .await
                .unwrap();
            assert_eq!(next.rows, [vec![WireValue::Int(20)]]);
            let frames = sent_frames(&written.lock().unwrap());
            assert_eq!(frames.len(), 7);
            assert_eq!(frames[5].header().kind(), FrameKind::QueryCancel);
            assert_eq!(frames[6].header().kind(), FrameKind::Execute);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn prepared_write_clients_use_explicit_modes_and_never_replay_unknown_completion() {
        let ((), report) = run_async_under_lab(0x79a0_0321, |root| async move {
            for (batch, scenario) in [false, true]
                .into_iter()
                .flat_map(|batch| (0..5).map(move |scenario| (batch, scenario)))
            {
                let handle = PreparedHandle([0x41; 16]);
                let mut replies = vec![rpc(
                    FrameKind::Prepared,
                    4,
                    StreamId::CONTROL,
                    &Prepared { handle },
                )];
                match scenario {
                    0 | 1 | 4 => {
                        replies.push(rpc(
                            FrameKind::SnapshotResultChunk,
                            5,
                            StreamId([5; 16]),
                            &ResultChunk {
                                columns: Some(vec![]),
                                rows: vec![],
                            },
                        ));
                        replies.push(rpc(
                            FrameKind::SnapshotResultEnd,
                            5,
                            StreamId([5; 16]),
                            &ResultEnd {
                                outcome: match scenario {
                                    0 => Outcome::WriteCommitted {
                                        seq: 8,
                                        statements: 2,
                                    },
                                    1 => Outcome::ReadClosed {
                                        seq: 7,
                                        statements: 2,
                                    },
                                    _ => Outcome::Rows { seq: 7 },
                                },
                                rows: 0,
                            },
                        ));
                    }
                    2 => replies.push(rpc(
                        FrameKind::Error,
                        5,
                        StreamId([5; 16]),
                        &ErrorBody {
                            code: ErrorCode::OutcomeUnknown,
                            message: "write outcome unknown; do not replay".into(),
                        },
                    )),
                    _ => {}
                }
                let io = script(replies);
                let written = std::sync::Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                assert_eq!(
                    client
                        .prepare_write(
                            &root,
                            "CREATE (:Person {name:$name,age:$age})",
                            vec![
                                ("name".into(), WireValue::Text("sample".into())),
                                ("age".into(), WireValue::Int(99))
                            ],
                        )
                        .await
                        .unwrap(),
                    handle
                );
                assert!(
                    matches!(
                        client
                            .execute_prepared_streaming(
                                &root,
                                PreparedReadMode::Write,
                                handle,
                                vec![],
                                |_| Ok(()),
                                |_| Ok(()),
                            )
                            .await,
                        Err(ClientError::Protocol(_))
                    ),
                    "read APIs refuse locally before sending"
                );
                let arguments = vec![
                    ("name".into(), WireValue::Text("Ann".into())),
                    ("age".into(), WireValue::Int(30)),
                ];
                let result = if batch {
                    client
                        .execute_prepared_batch(
                            &root,
                            handle,
                            vec![
                                arguments,
                                vec![
                                    ("name".into(), WireValue::Text("Bob".into())),
                                    ("age".into(), WireValue::Int(25)),
                                ],
                            ],
                        )
                        .await
                } else {
                    client
                        .execute_prepared_write(&root, handle, arguments)
                        .await
                };
                match scenario {
                    0 => assert_eq!(
                        result.unwrap(),
                        Outcome::WriteCommitted {
                            seq: 8,
                            statements: 2
                        }
                    ),
                    1 => assert_eq!(
                        result.unwrap(),
                        Outcome::ReadClosed {
                            seq: 7,
                            statements: 2
                        }
                    ),
                    2 => assert!(matches!(
                        result,
                        Err(ClientError::Server {
                            code: ErrorCode::OutcomeUnknown,
                            ..
                        })
                    )),
                    3 => assert!(matches!(result, Err(ClientError::Closed))),
                    _ => assert!(matches!(result, Err(ClientError::Protocol(_)))),
                }
                let frames = sent_frames(&written.lock().unwrap());
                assert_eq!(
                    frames.len(),
                    5,
                    "no fallback or replay after a failed prepared write"
                );
                let preparation = Prepare::decode(frames[3].payload()).unwrap();
                assert_eq!(preparation.mode, PrepareMode::Write);
                let expected = vec![
                    ("age".into(), WireValue::Int(30)),
                    ("name".into(), WireValue::Text("Ann".into())),
                ];
                if batch {
                    assert_eq!(frames[4].header().kind(), FrameKind::ExecutePreparedBatch);
                    let execution = ExecutePreparedBatch::decode(frames[4].payload()).unwrap();
                    assert_eq!(execution.handle, handle);
                    assert_eq!(
                        execution.argument_sets,
                        [
                            expected,
                            vec![
                                ("age".into(), WireValue::Int(25)),
                                ("name".into(), WireValue::Text("Bob".into())),
                            ]
                        ]
                    );
                } else {
                    assert_eq!(frames[4].header().kind(), FrameKind::ExecutePrepared);
                    let execution = ExecutePrepared::decode(frames[4].payload()).unwrap();
                    assert_eq!(execution.mode, PreparedReadMode::Write);
                    assert_eq!(execution.handle, handle);
                    assert_eq!(execution.parameters, expected);
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn prepared_writes_refuse_query_output_and_drain_unexpected_rows() {
        let ((), report) = run_async_under_lab(0x79a0_0324, |root| async move {
            for (batch, scenario) in [false, true]
                .into_iter()
                .flat_map(|batch| (0..3).map(move |scenario| (batch, scenario)))
            {
                let handle = PreparedHandle([0x24; 16]);
                let mut replies = vec![
                    rpc(
                        FrameKind::Prepared,
                        4,
                        StreamId::CONTROL,
                        &Prepared { handle },
                    ),
                    rpc(
                        FrameKind::SnapshotResultChunk,
                        5,
                        StreamId([5; 16]),
                        &ResultChunk {
                            columns: Some(if scenario == 0 {
                                vec!["n".into()]
                            } else {
                                vec![]
                            }),
                            rows: if scenario == 1 { vec![vec![]] } else { vec![] },
                        },
                    ),
                    rpc(
                        FrameKind::SnapshotResultEnd,
                        5,
                        StreamId([5; 16]),
                        &ResultEnd {
                            outcome: if scenario == 2 {
                                Outcome::Rows { seq: 8 }
                            } else {
                                Outcome::WriteCommitted {
                                    seq: 8,
                                    statements: 1,
                                }
                            },
                            rows: u64::from(scenario == 1),
                        },
                    ),
                ];
                let next_request = if scenario == 1 { 7 } else { 6 };
                replies.extend([
                    result_chunk(next_request, &[20]),
                    result_end(next_request, 1),
                ]);
                let io = script(replies);
                let written = std::sync::Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                let prepared = client
                    .prepare_write(&root, "CREATE (:Person)", vec![])
                    .await
                    .unwrap();
                let result = if batch {
                    client
                        .execute_prepared_batch(&root, prepared, vec![vec![]])
                        .await
                } else {
                    client.execute_prepared_write(&root, prepared, vec![]).await
                };
                assert!(matches!(result, Err(ClientError::Protocol(_))));
                // Profile errors still drain the terminal, including a bad
                // row's cancellation, before another request can receive data.
                assert_eq!(
                    client
                        .execute(&root, ExecuteMode::Read, "RETURN 20", vec![])
                        .await
                        .unwrap()
                        .rows,
                    [vec![WireValue::Int(20)]]
                );
                let sent = sent_frames(&written.lock().unwrap());
                assert_eq!(sent.len(), next_request as usize);
                if scenario == 1 {
                    assert_eq!(sent[5].header().kind(), FrameKind::QueryCancel);
                    assert_eq!(sent[5].header().stream_id(), StreamId([5; 16]));
                }
                assert_eq!(sent.last().unwrap().header().kind(), FrameKind::Execute);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn prepared_batch_rejects_invalid_local_arguments_without_sending() {
        let ((), report) = run_async_under_lab(0x79a0_0326, |root| async move {
            let io = script(vec![]);
            let written = std::sync::Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "test").await.unwrap();
            for arguments in [
                vec![],
                vec![vec![
                    ("age".into(), WireValue::Int(1)),
                    ("age".into(), WireValue::Int(2)),
                ]],
            ] {
                assert!(matches!(
                    client
                        .execute_prepared_batch(&root, PreparedHandle([0x23; 16]), arguments)
                        .await,
                    Err(ClientError::Body(BodyError::Noncanonical))
                ));
            }
            assert_eq!(sent_frames(&written.lock().unwrap()).len(), 3);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn prepared_client_rebinds_and_reuses_the_connection_after_callback_cancellation() {
        let ((), report) = run_async_under_lab(0x79a0_0301, |root| async move {
            for (mode, cancelled_terminal) in [
                PreparedReadMode::SourceStream,
                PreparedReadMode::OrderedStream,
            ]
            .into_iter()
            .flat_map(|mode| [false, true].map(|cancelled| (mode, cancelled)))
            {
                let handle = PreparedHandle([0x31; 16]);
                let terminal = if cancelled_terminal {
                    rpc(
                        FrameKind::Error,
                        5,
                        StreamId([5; 16]),
                        &ErrorBody {
                            code: ErrorCode::Cancelled,
                            message: "cancelled".into(),
                        },
                    )
                } else {
                    result_end(5, 2)
                };
                let io = script(vec![
                    rpc(
                        FrameKind::Prepared,
                        4,
                        StreamId::CONTROL,
                        &Prepared { handle },
                    ),
                    result_chunk(5, &[1, 2]),
                    terminal,
                    result_chunk(7, &[20]),
                    result_end(7, 1),
                    rpc(
                        FrameKind::PreparedReleased,
                        8,
                        StreamId::CONTROL,
                        &ReleasePrepared { handle },
                    ),
                    rpc(
                        FrameKind::PreparedReleased,
                        9,
                        StreamId::CONTROL,
                        &ReleasePrepared { handle },
                    ),
                ]);
                let written = std::sync::Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                let prepared = client
                    .prepare_read(
                        &root,
                        "MATCH (n) RETURN $value AS value",
                        vec![("value".into(), WireValue::Int(99))],
                    )
                    .await
                    .unwrap();
                assert_eq!(prepared, handle);
                let mut delivered = 0;
                let stopped = client
                    .execute_prepared_streaming(
                        &root,
                        mode,
                        handle,
                        vec![("value".into(), WireValue::Int(1))],
                        |_| Ok(()),
                        |_| {
                            delivered += 1;
                            Err(ClientError::Protocol("caller stopped"))
                        },
                    )
                    .await;
                assert!(matches!(
                    stopped,
                    Err(ClientError::Protocol("caller stopped"))
                ));
                assert_eq!(delivered, 1, "cancel suppresses all remaining callbacks");
                let result = client
                    .execute_prepared(&root, handle, vec![("value".into(), WireValue::Int(20))])
                    .await
                    .unwrap();
                assert_eq!(result.rows, [vec![WireValue::Int(20)]]);
                client.release_prepared(&root, handle).await.unwrap();
                client.release_prepared(&root, handle).await.unwrap();
                let frames = sent_frames(&written.lock().unwrap());
                assert_eq!(
                    frames
                        .iter()
                        .map(|frame| frame.header().kind())
                        .collect::<Vec<_>>(),
                    [
                        FrameKind::Hello,
                        FrameKind::Auth,
                        FrameKind::SelectDatabase,
                        FrameKind::Prepare,
                        FrameKind::ExecutePrepared,
                        FrameKind::QueryCancel,
                        FrameKind::ExecutePrepared,
                        FrameKind::ReleasePrepared,
                        FrameKind::ReleasePrepared,
                    ]
                );
                let first = ExecutePrepared::decode(frames[4].payload()).unwrap();
                let second = ExecutePrepared::decode(frames[6].payload()).unwrap();
                assert_eq!(first.mode, mode);
                assert_eq!(second.mode, PreparedReadMode::Buffered);
                assert_eq!(first.parameters, [("value".into(), WireValue::Int(1))]);
                assert_eq!(second.parameters, [("value".into(), WireValue::Int(20))]);
                assert_eq!(frames[5].header().stream_id(), StreamId([5; 16]));
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn finite_results_validate_chunk_shapes_and_terminal_counts() {
        let ((), report) = run_async_under_lab(0x79a0_0450, |root| async move {
            for prepared in [false, true] {
                for scenario in 0..4 {
                    let handle = PreparedHandle([0x61; 16]);
                    let request = if prepared { 5 } else { 4 };
                    let mut replies = Vec::new();
                    if prepared {
                        replies.push(rpc(
                            FrameKind::Prepared,
                            4,
                            StreamId::CONTROL,
                            &Prepared { handle },
                        ));
                    }
                    if scenario < 2 {
                        if scenario == 1 {
                            replies.push(result_chunk(request, &[]));
                        }
                        replies.push(rpc(
                            FrameKind::SnapshotResultChunk,
                            request,
                            StreamId([5; 16]),
                            &ResultChunk {
                                columns: (scenario == 0).then(|| vec!["value".into()]),
                                rows: vec![
                                    vec![WireValue::Int(1)],
                                    vec![WireValue::Int(2), WireValue::Int(3)],
                                ],
                            },
                        ));
                    } else {
                        replies.push(result_chunk(request, &[1]));
                        replies.push(result_end(request, if scenario == 2 { 0 } else { 2 }));
                    }
                    let io = script(replies);
                    let written = std::sync::Arc::clone(&io.written);
                    let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                    client.select(&root, "test").await.unwrap();
                    if prepared {
                        client
                            .prepare_read(&root, "RETURN 1", vec![])
                            .await
                            .unwrap();
                    }
                    let columns = AtomicUsize::new(0);
                    let rows = AtomicUsize::new(0);
                    let on_columns = |_: &[String]| {
                        columns.fetch_add(1, Ordering::Relaxed);
                        Ok(())
                    };
                    let on_row = |_| {
                        rows.fetch_add(1, Ordering::Relaxed);
                        Ok(())
                    };
                    let result = if prepared {
                        client
                            .execute_prepared_streaming(
                                &root,
                                PreparedReadMode::SourceStream,
                                handle,
                                vec![],
                                on_columns,
                                on_row,
                            )
                            .await
                    } else {
                        client
                            .execute_streaming(
                                &root,
                                ExecuteMode::ReadStream,
                                "RETURN 1",
                                vec![],
                                on_columns,
                                on_row,
                            )
                            .await
                    };
                    match scenario {
                        0 | 1 => assert!(matches!(
                            result,
                            Err(ClientError::Protocol(
                                "result row width differs from columns"
                            ))
                        )),
                        _ => assert!(matches!(
                            result,
                            Err(ClientError::Protocol("END row count differs from delivery"))
                        )),
                    }
                    assert_eq!(columns.load(Ordering::Relaxed), usize::from(scenario != 0));
                    assert_eq!(rows.load(Ordering::Relaxed), usize::from(scenario >= 2));
                    let sent = sent_frames(&written.lock().unwrap());
                    assert_eq!(
                        sent.iter()
                            .filter(|frame| matches!(
                                frame.header().kind(),
                                FrameKind::Execute | FrameKind::ExecutePrepared
                            ))
                            .count(),
                        1,
                        "an invalid response is never retried"
                    );
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn cancelled_finite_results_validate_all_drained_rows_before_reuse() {
        let ((), report) = run_async_under_lab(0x79a0_0451, |root| async move {
            for declared_rows in [1, 2, 3] {
                let mut replies = vec![result_chunk(4, &[1, 2]), result_end(4, declared_rows)];
                if declared_rows == 2 {
                    replies.extend([result_chunk(6, &[20]), result_end(6, 1)]);
                }
                let io = script(replies);
                let written = std::sync::Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                let callbacks = AtomicUsize::new(0);
                let result = client
                    .execute_streaming(
                        &root,
                        ExecuteMode::ReadStream,
                        "RETURN 1",
                        vec![],
                        |_| Ok(()),
                        |_| {
                            callbacks.fetch_add(1, Ordering::Relaxed);
                            Err(ClientError::Protocol("consumer stopped"))
                        },
                    )
                    .await;
                assert_eq!(callbacks.load(Ordering::Relaxed), 1);
                if declared_rows == 2 {
                    assert!(matches!(
                        result,
                        Err(ClientError::Protocol("consumer stopped"))
                    ));
                    assert_eq!(
                        client
                            .execute(&root, ExecuteMode::Read, "RETURN 20", vec![],)
                            .await
                            .unwrap()
                            .rows,
                        [vec![WireValue::Int(20)]]
                    );
                } else {
                    assert!(matches!(
                        result,
                        Err(ClientError::Protocol("END row count differs from delivery"))
                    ));
                }
                let sent = sent_frames(&written.lock().unwrap());
                assert_eq!(
                    sent.iter()
                        .filter(|frame| frame.header().kind() == FrameKind::QueryCancel)
                        .count(),
                    1
                );
                assert!(
                    !sent
                        .iter()
                        .any(|frame| frame.header().kind() == FrameKind::WindowUpdate)
                );
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn finite_result_end_cannot_exceed_credit_after_schema_refusal() {
        let ((), report) = run_async_under_lab(0x79a0_0452, |root| async move {
            let chunk = rpc(
                FrameKind::SnapshotResultChunk,
                4,
                StreamId([5; 16]),
                &ResultChunk {
                    columns: Some(vec!["value".into()]),
                    rows: vec![vec![WireValue::Text("x".repeat(3700))]],
                },
            );
            let end = result_end(4, 1);
            assert!(chunk.header().frame_len() < 4096);
            assert!(4096 - chunk.header().frame_len() < end.header().frame_len());
            let io = script_with_window(vec![chunk, end], 4096, 32);
            let written = std::sync::Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "test").await.unwrap();
            let result = client
                .execute_streaming(
                    &root,
                    ExecuteMode::ReadOrdered,
                    "RETURN 1",
                    vec![],
                    |_| Err(ClientError::Protocol("consumer stopped")),
                    |_| panic!("schema was refused before row delivery"),
                )
                .await;
            assert!(matches!(
                result,
                Err(ClientError::Protocol("server exceeded its flow credit"))
            ));
            let sent = sent_frames(&written.lock().unwrap());
            assert_eq!(
                sent.iter()
                    .filter(|frame| frame.header().kind() == FrameKind::QueryCancel)
                    .count(),
                1
            );
            assert!(
                !sent
                    .iter()
                    .any(|frame| frame.header().kind() == FrameKind::WindowUpdate)
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn callback_cancellation_never_hides_foreign_or_malformed_result_terminals() {
        let ((), report) = run_async_under_lab(0x79a0_0302, |root| async move {
            let handle = PreparedHandle([0x31; 16]);
            let mut stale = ready().binding(session());
            stale.session.auth_generation += 1;
            let limits = FrameLimits::new(4096).unwrap();
            let foreign = rpc(
                FrameKind::SnapshotResultEnd,
                5,
                StreamId([6; 16]),
                &ResultEnd {
                    outcome: Outcome::Rows { seq: 3 },
                    rows: 1,
                },
            );
            let wrong_request = result_end(6, 1);
            let malformed = Frame::new(
                FrameKind::Error,
                5,
                StreamId([5; 16]),
                Binding::Ready(ready().binding(session())),
                vec![0],
                limits,
            )
            .unwrap();
            let stale = Frame::new(
                FrameKind::SnapshotResultEnd,
                5,
                StreamId([5; 16]),
                Binding::Ready(stale),
                ResultEnd {
                    outcome: Outcome::Rows { seq: 3 },
                    rows: 1,
                }
                .encode()
                .unwrap(),
                limits,
            )
            .unwrap();
            for terminal in [foreign, wrong_request, malformed, stale] {
                let io = script(vec![
                    rpc(
                        FrameKind::Prepared,
                        4,
                        StreamId::CONTROL,
                        &Prepared { handle },
                    ),
                    result_chunk(5, &[1]),
                    terminal,
                ]);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                client
                    .prepare_read(&root, "MATCH (n) RETURN n", vec![])
                    .await
                    .unwrap();
                let error = client
                    .execute_prepared_streaming(
                        &root,
                        PreparedReadMode::SourceStream,
                        handle,
                        vec![],
                        |_| Ok(()),
                        |_| Err(ClientError::Protocol("caller stopped")),
                    )
                    .await
                    .unwrap_err();
                assert!(!matches!(error, ClientError::Protocol("caller stopped")));
                assert!(matches!(
                    error,
                    ClientError::Protocol(_) | ClientError::Body(_) | ClientError::Transport(_)
                ));
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn subscription_callback_errors_cancel_drain_and_preserve_connection_reuse() {
        let ((), report) = run_async_under_lab(0x79a0_0203, |root| async move {
            for complete_late_batch in [false, true] {
                for kind in [
                    FrameKind::SnapshotResultEnd,
                    FrameKind::Error,
                    FrameKind::SubscriptionReset,
                ] {
                    let checkpoint = if complete_late_batch { 8 } else { 7 };
                    let terminal = match kind {
                        FrameKind::SnapshotResultEnd => output(
                            kind,
                            &ResultEnd {
                                outcome: Outcome::Rows { seq: checkpoint },
                                rows: 0,
                            },
                        ),
                        FrameKind::SubscriptionReset => output(
                            kind,
                            &SubscriptionReset {
                                last_delivered_seq: Some(checkpoint),
                            },
                        ),
                        _ => output(
                            kind,
                            &ErrorBody {
                                code: ErrorCode::Cancelled,
                                message: "cancelled".into(),
                            },
                        ),
                    };
                    let io = script(vec![
                        batch(7, true, true),
                        batch(8, false, complete_late_batch),
                        terminal,
                        result_chunk(6, &[42]),
                        result_end(6, 1),
                    ]);
                    let written = io.written.clone();
                    let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                    client.select(&root, "test").await.unwrap();
                    let mut seen = Vec::new();
                    let result = client
                        .subscribe(
                            &root,
                            "SUBSCRIBE TO MATCH (n) RETURN n",
                            vec![],
                            |_| {},
                            |change| {
                                seen.push(change.frontier);
                                Err(ClientError::Protocol("subscription sink failed"))
                            },
                        )
                        .await;
                    assert!(matches!(
                        result,
                        Err(ClientError::Protocol("subscription sink failed"))
                    ));
                    assert_eq!(seen, [7], "a failed callback must never be called again");
                    let answer = client
                        .execute(&root, ExecuteMode::Read, "RETURN 42", vec![])
                        .await
                        .unwrap();
                    assert_eq!(answer.rows, [vec![WireValue::Int(42)]]);
                    let sent = sent_frames(&written.lock().unwrap());
                    let kinds: Vec<_> = sent.iter().map(|frame| frame.header().kind()).collect();
                    assert_eq!(
                        kinds,
                        [
                            FrameKind::Hello,
                            FrameKind::Auth,
                            FrameKind::SelectDatabase,
                            FrameKind::Execute,
                            FrameKind::QueryCancel,
                            FrameKind::Execute,
                        ]
                    );
                    assert_eq!(sent[4].header().stream_id(), StreamId([5; 16]));
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn subscription_cancel_stops_grants_and_accepts_its_credit_free_terminal() {
        let ((), report) = run_async_under_lab(0x79a0_0204, |root| async move {
            let first = batch(7, true, true);
            let bytes = first.header().frame_len() as u64;
            let io = script_with_window(
                vec![
                    first,
                    output(
                        FrameKind::SnapshotResultEnd,
                        &ResultEnd {
                            outcome: Outcome::Rows { seq: 7 },
                            rows: 0,
                        },
                    ),
                ],
                bytes,
                1,
            );
            let written = io.written.clone();
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "test").await.unwrap();
            let delivered = client
                .subscribe(
                    &root,
                    "SUBSCRIBE TO MATCH (n) RETURN n",
                    vec![],
                    |_| {},
                    |_| Ok(false),
                )
                .await
                .unwrap();
            assert_eq!(delivered, 7);
            let sent = sent_frames(&written.lock().unwrap());
            assert_eq!(
                sent.iter()
                    .filter(|frame| frame.header().kind() == FrameKind::QueryCancel)
                    .count(),
                1
            );
            assert!(
                sent.iter()
                    .all(|frame| frame.header().kind() != FrameKind::WindowUpdate)
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn subscription_rejects_bad_batch_shapes_before_callbacks() {
        let ((), report) = run_async_under_lab(0x79a0_0205, |root| async move {
            let valid = SubscriptionBatch {
                frontier: 7,
                snapshot: true,
                last: true,
                columns: Some(vec!["n".into()]),
                entries: vec![(1, vec![WireValue::Int(7)])],
            };
            let mut wide = valid.clone();
            wide.entries[0].1.push(WireValue::Null);
            let mut negative = valid.clone();
            negative.entries[0].0 = -1;
            let mut delta = valid.clone();
            delta.snapshot = false;
            let control = rpc(FrameKind::SubscriptionBatch, 4, StreamId::CONTROL, &valid);
            for first in [
                output(FrameKind::SubscriptionBatch, &wide),
                output(FrameKind::SubscriptionBatch, &negative),
                output(FrameKind::SubscriptionBatch, &delta),
                control,
            ] {
                let mut client = Client::connect_stream(&root, script(vec![first]), vec![1])
                    .await
                    .unwrap();
                client.select(&root, "test").await.unwrap();
                let mut schemas = 0;
                let mut changes = 0;
                let result = client
                    .subscribe(
                        &root,
                        "SUBSCRIBE TO MATCH (n) RETURN n",
                        vec![],
                        |_| schemas += 1,
                        |_| {
                            changes += 1;
                            Ok(true)
                        },
                    )
                    .await;
                assert!(matches!(result, Err(ClientError::Protocol(_))));
                assert_eq!((schemas, changes), (0, 0));
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn subscription_callback_failure_cannot_hide_bad_or_foreign_terminals() {
        let ((), report) = run_async_under_lab(0x79a0_0206, |root| async move {
            let mut stale = ready().binding(session());
            stale.session.auth_generation += 1;
            let limits = FrameLimits::new(4096).unwrap();
            let end = ResultEnd {
                outcome: Outcome::Rows { seq: 7 },
                rows: 0,
            };
            for terminal in [
                rpc(FrameKind::SnapshotResultEnd, 5, StreamId([5; 16]), &end),
                rpc(FrameKind::SnapshotResultEnd, 4, StreamId([6; 16]), &end),
                output(
                    FrameKind::SnapshotResultEnd,
                    &ResultEnd {
                        outcome: Outcome::Rows { seq: 8 },
                        rows: 0,
                    },
                ),
                output(
                    FrameKind::SubscriptionReset,
                    &SubscriptionReset {
                        last_delivered_seq: Some(8),
                    },
                ),
                Frame::new(
                    FrameKind::Error,
                    4,
                    StreamId([5; 16]),
                    Binding::Ready(ready().binding(session())),
                    vec![0],
                    limits,
                )
                .unwrap(),
                Frame::new(
                    FrameKind::SnapshotResultEnd,
                    4,
                    StreamId([5; 16]),
                    Binding::Ready(stale),
                    end.encode().unwrap(),
                    limits,
                )
                .unwrap(),
            ] {
                let mut client = Client::connect_stream(
                    &root,
                    script(vec![batch(7, true, true), terminal]),
                    vec![1],
                )
                .await
                .unwrap();
                client.select(&root, "test").await.unwrap();
                let error = client
                    .subscribe(
                        &root,
                        "SUBSCRIBE TO MATCH (n) RETURN n",
                        vec![],
                        |_| {},
                        |_| Err(ClientError::Protocol("subscription sink failed")),
                    )
                    .await
                    .unwrap_err();
                assert!(!matches!(
                    error,
                    ClientError::Protocol("subscription sink failed")
                ));
                assert!(matches!(
                    error,
                    ClientError::Protocol(_) | ClientError::Body(_) | ClientError::Transport(_)
                ));
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn cancel_and_recovery_terminals_report_only_successful_callback_delivery() {
        let ((), report) = run_async_under_lab(0x79a0_0201, |root| async move {
            for terminal in [
                output(
                    FrameKind::Error,
                    &ErrorBody {
                        code: ErrorCode::Cancelled,
                        message: "cancelled".into(),
                    },
                ),
                output(
                    FrameKind::SnapshotResultEnd,
                    &ResultEnd {
                        outcome: Outcome::Rows { seq: 8 },
                        rows: 0,
                    },
                ),
                output(
                    FrameKind::SubscriptionReset,
                    &SubscriptionReset {
                        last_delivered_seq: Some(8),
                    },
                ),
            ] {
                let reset = terminal.header().kind() == FrameKind::SubscriptionReset;
                let io = script(vec![batch(7, true, true), batch(8, false, true), terminal]);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                let mut seen = Vec::new();
                let result = client
                    .subscribe(
                        &root,
                        "SUBSCRIBE TO MATCH (n) RETURN n",
                        vec![],
                        |_| {},
                        |change| {
                            seen.push(change.frontier);
                            Ok(false)
                        },
                    )
                    .await;
                assert_eq!(seen, [7]);
                if reset {
                    assert!(matches!(
                        result,
                        Err(ClientError::SubscriptionReset {
                            last_delivered_seq: Some(7)
                        })
                    ));
                } else {
                    assert_eq!(result.unwrap(), 7);
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn reset_discards_partial_batches_and_rejects_an_unearned_wire_checkpoint() {
        let ((), report) = run_async_under_lab(0x79a0_0202, |root| async move {
            for (complete_baseline, reported, valid) in [
                (false, None, true),
                (true, Some(7), true),
                (true, Some(8), false),
            ] {
                let mut frames = vec![batch(7, true, complete_baseline)];
                if complete_baseline {
                    frames.push(batch(8, false, false));
                }
                frames.push(output(
                    FrameKind::SubscriptionReset,
                    &SubscriptionReset {
                        last_delivered_seq: reported,
                    },
                ));
                let mut client = Client::connect_stream(&root, script(frames), vec![1])
                    .await
                    .unwrap();
                client.select(&root, "test").await.unwrap();
                let mut seen = Vec::new();
                let result = client
                    .subscribe(
                        &root,
                        "SUBSCRIBE TO MATCH (n) RETURN n",
                        vec![],
                        |_| {},
                        |change| {
                            seen.push(change.frontier);
                            Ok(true)
                        },
                    )
                    .await;
                assert_eq!(seen, if complete_baseline { vec![7] } else { vec![] });
                if valid {
                    assert!(
                        matches!(result, Err(ClientError::SubscriptionReset { last_delivered_seq }) if last_delivered_seq == reported)
                    );
                } else {
                    assert!(matches!(result, Err(ClientError::Protocol(_))));
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}

fn expect<B: Body>(frame: &Frame, kind: FrameKind) -> Result<B, ClientError> {
    let header = frame.header();
    if header.kind() == FrameKind::Error {
        return Err(server_error(frame));
    }
    if header.kind() != kind {
        return Err(ClientError::Protocol("unexpected reply kind"));
    }
    Ok(B::decode(frame.payload())?)
}

fn server_error(frame: &Frame) -> ClientError {
    match ErrorBody::decode(frame.payload()) {
        Ok(body) => ClientError::Server {
            code: body.code,
            message: body.message,
        },
        Err(error) => ClientError::Body(error),
    }

    fn transaction_begun(request: u64, handle: crate::body::TransactionHandle) -> Frame {
        rpc(
            FrameKind::TransactionBegun,
            request,
            StreamId::CONTROL,
            &crate::body::TransactionBegun { handle, basis: 3 },
        )
    }

    fn transaction_chunk(request: u64, values: &[i64]) -> Frame {
        rpc(
            FrameKind::TransactionResultChunk,
            request,
            StreamId([5; 16]),
            &ResultChunk {
                columns: Some(vec!["value".into()]),
                rows: values.iter().map(|v| vec![WireValue::Int(*v)]).collect(),
            },
        )
    }

    fn transaction_end(
        request: u64,
        handle: crate::body::TransactionHandle,
        statement: u64,
        rows: u64,
    ) -> Frame {
        rpc(
            FrameKind::TransactionResultEnd,
            request,
            StreamId([5; 16]),
            &crate::body::TransactionResultEnd { handle, statement, rows },
        )
    }

    fn transaction_statement(mode: ExecuteMode) -> Execute {
        Execute {
            mode,
            statement: "RETURN $a".into(),
            parameters: vec![("a".into(), WireValue::Int(11))],
        }
    }

    #[test]
    fn explicit_transaction_client_separates_provisional_rows_from_one_commit() {
        let ((), report) = run_async_under_lab(0x79a0_0410, |root| async move {
            let handle = crate::body::TransactionHandle([0x61; 16]);
            let completion = Outcome::WriteCommitted { seq: 4, statements: 2 };
            let io = script(vec![
                transaction_begun(4, handle),
                transaction_chunk(5, &[11]),
                transaction_end(5, handle, 1, 1),
                transaction_chunk(6, &[12]),
                transaction_end(6, handle, 2, 1),
                rpc(
                    FrameKind::TransactionCommitted,
                    7,
                    StreamId::CONTROL,
                    &crate::body::TransactionCompletion { handle, outcome: completion },
                ),
                rpc(FrameKind::Pong, 8, StreamId::CONTROL, &Ping { nonce: 7 }),
            ]);
            let written = std::sync::Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "test").await.unwrap();
            assert_eq!(client.begin_transaction(&root).await.unwrap().basis, 3);
            let before = written.lock().unwrap().len();
            assert!(client.execute(&root, ExecuteMode::Read, "RETURN 9", vec![]).await.is_err());
            assert!(client.refresh_authority(&root, vec![1]).await.is_err());
            assert!(client.begin_transaction(&root).await.is_err());
            assert!(client.execute_transaction(
                &root,
                crate::body::TransactionHandle([0x62; 16]),
                &transaction_statement(ExecuteMode::Read),
            ).await.is_err());
            assert_eq!(written.lock().unwrap().len(), before);
            let first = client.execute_transaction(
                &root, handle, &transaction_statement(ExecuteMode::Write),
            ).await.unwrap();
            assert_eq!(first.columns, ["value"]);
            assert_eq!(first.rows, [vec![WireValue::Int(11)]]);
            assert_eq!(first.end.statement, 1);
            let second = client.execute_transaction(
                &root, handle, &transaction_statement(ExecuteMode::Read),
            ).await.unwrap();
            assert_eq!(second.rows, [vec![WireValue::Int(12)]]);
            assert_eq!(second.end.statement, 2);
            assert_eq!(client.commit_transaction(&root, handle).await.unwrap(), completion);
            client.ping(&root, 7).await.unwrap();
            let frames = sent_frames(&written.lock().unwrap());
            assert_eq!(
                frames.iter().filter(|f| f.header().kind() == FrameKind::CommitTransaction).count(),
                1
            );
            assert_eq!(
                frames.iter().filter(|f| f.header().kind() == FrameKind::ExecuteTransaction).count(),
                2
            );
            assert!(!frames.iter().any(|f| f.header().kind() == FrameKind::Execute));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn explicit_transaction_local_admission_preserves_owner_until_rollback() {
        let ((), report) = run_async_under_lab(0x79a0_0411, |root| async move {
            let handle = crate::body::TransactionHandle([0x61; 16]);
            let io = script(vec![
                transaction_begun(4, handle),
                rpc(
                    FrameKind::TransactionRolledBack, 5, StreamId::CONTROL,
                    &crate::body::TransactionSelector { handle },
                ),
                rpc(FrameKind::Pong, 6, StreamId::CONTROL, &Ping { nonce: 8 }),
            ]);
            let written = std::sync::Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "test").await.unwrap();
            client.begin_transaction(&root).await.unwrap();
            let before = written.lock().unwrap().len();
            for mode in [ExecuteMode::Subscribe, ExecuteMode::ReadStream, ExecuteMode::ReadOrdered] {
                assert!(client.execute_transaction(
                    &root, handle, &transaction_statement(mode),
                ).await.is_err());
            }
            let mut duplicate = transaction_statement(ExecuteMode::Write);
            duplicate.parameters.push(("a".into(), WireValue::Int(99)));
            assert!(client.execute_transaction(&root, handle, &duplicate).await.is_err());
            let mut oversized = transaction_statement(ExecuteMode::Write);
            oversized.statement = "x".repeat(5000);
            assert!(client.execute_transaction(&root, handle, &oversized).await.is_err());
            assert_eq!(written.lock().unwrap().len(), before);
            client.rollback_transaction(&root, handle).await.unwrap();
            client.ping(&root, 8).await.unwrap();
            let frames = sent_frames(&written.lock().unwrap());
            assert!(!frames.iter().any(|f| f.header().kind() == FrameKind::ExecuteTransaction));
            assert!(!frames.iter().any(|f| f.header().kind() == FrameKind::CommitTransaction));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn rejected_provisional_output_cancels_before_another_transaction_can_begin() {
        let ((), report) = run_async_under_lab(0x79a0_0412, |root| async move {
            for reject_columns in [false, true] {
                let handle = crate::body::TransactionHandle([0x61; 16]);
                let successor = crate::body::TransactionHandle([0x62; 16]);
                // The complete result fits the initial window. END can already
                // be in flight before the callback's QUERY_CANCEL reaches the
                // peer; it still cannot be interpreted as a committed result.
                let io = script(vec![
                    transaction_begun(4, handle),
                    transaction_chunk(5, &[11, 12]),
                    transaction_end(5, handle, 1, 2),
                    transaction_begun(7, successor),
                    rpc(
                        FrameKind::TransactionRolledBack, 8, StreamId::CONTROL,
                        &crate::body::TransactionSelector { handle: successor },
                    ),
                ]);
                let written = std::sync::Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                client.begin_transaction(&root).await.unwrap();
                let mut rows_seen = 0;
                let result = client.execute_transaction_streaming(
                    &root, handle, &transaction_statement(ExecuteMode::Write),
                    |_| {
                        if reject_columns {
                            Err(ClientError::Protocol("application rejected provisional output"))
                        } else {
                            Ok(())
                        }
                    },
                    |_| {
                        rows_seen += 1;
                        Err(ClientError::Protocol("application rejected provisional output"))
                    },
                ).await;
                assert!(matches!(
                    result,
                    Err(ClientError::Protocol("application rejected provisional output"))
                ));
                assert_eq!(rows_seen, usize::from(!reject_columns));
                assert!(client.commit_transaction(&root, handle).await.is_err());
                assert_eq!(client.begin_transaction(&root).await.unwrap().handle, successor);
                client.rollback_transaction(&root, successor).await.unwrap();
                let frames = sent_frames(&written.lock().unwrap());
                let tail: Vec<_> = frames[3..].iter().map(|f| f.header().kind()).collect();
                assert_eq!(tail, [
                    FrameKind::BeginTransaction,
                    FrameKind::ExecuteTransaction,
                    FrameKind::QueryCancel,
                    FrameKind::BeginTransaction,
                    FrameKind::RollbackTransaction,
                ]);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn malformed_provisional_results_poison_the_request_and_cannot_be_committed() {
        let ((), report) = run_async_under_lab(0x79a0_0413, |root| async move {
            for scenario in 0..7 {
                let handle = crate::body::TransactionHandle([0x61; 16]);
                let mut frames = vec![transaction_begun(4, handle)];
                match scenario {
                    0 => frames.push(transaction_end(5, handle, 1, 0)),
                    1 => {
                        frames.push(transaction_chunk(5, &[11]));
                        frames.push(transaction_end(
                            5, crate::body::TransactionHandle([0x62; 16]), 1, 1,
                        ));
                    }
                    2 => {
                        frames.push(transaction_chunk(5, &[11]));
                        frames.push(transaction_end(5, handle, 2, 1));
                    }
                    3 => {
                        frames.push(transaction_chunk(5, &[11]));
                        frames.push(transaction_end(5, handle, 1, 2));
                    }
                    4 => frames.push(result_chunk(5, &[11])),
                    5 => frames.push(rpc(
                        FrameKind::TransactionResultChunk, 5, StreamId([5; 16]),
                        &ResultChunk {
                            columns: Some(vec!["value".into()]),
                            rows: vec![vec![]],
                        },
                    )),
                    _ => frames.push(transaction_chunk(5, &[11, 12, 13])),
                }
                let io = script_with_window(frames, 65536, 2);
                let written = std::sync::Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                client.begin_transaction(&root).await.unwrap();
                assert!(client.execute_transaction(
                    &root, handle, &transaction_statement(ExecuteMode::Write),
                ).await.is_err());
                let before = written.lock().unwrap().len();
                requires_reconnect(client.commit_transaction(&root, handle).await);
                requires_reconnect(client.ping(&root, 1).await);
                assert_eq!(written.lock().unwrap().len(), before);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn lost_or_ambiguous_transaction_completion_never_replays_commit() {
        let ((), report) = run_async_under_lab(0x79a0_0414, |root| async move {
            for scenario in 0..7 {
                let handle = crate::body::TransactionHandle([0x61; 16]);
                let mut frames = vec![transaction_begun(4, handle)];
                match scenario {
                    0 => {}
                    1 => frames.push(rpc(
                        FrameKind::TransactionCommitted, 5, StreamId::CONTROL,
                        &crate::body::TransactionCompletion {
                            handle: crate::body::TransactionHandle([0x62; 16]),
                            outcome: Outcome::WriteCommitted { seq: 4, statements: 1 },
                        },
                    )),
                    2 => frames.push(rpc(
                        FrameKind::Error, 5, StreamId::CONTROL,
                        &ErrorBody { code: ErrorCode::OutcomeUnknown, message: "unknown".into() },
                    )),
                    _ => frames.push(rpc(
                        FrameKind::TransactionCommitted, 5, StreamId::CONTROL,
                        &crate::body::TransactionCompletion {
                            handle,
                            outcome: match scenario {
                                3 => Outcome::WriteCommitted { seq: 3, statements: 0 },
                                4 => Outcome::ReadClosed { seq: 4, statements: 0 },
                                5 => Outcome::ReadClosed { seq: 3, statements: 1 },
                                _ => Outcome::WriteCommitted { seq: 4, statements: 0 },
                            },
                        },
                    )),
                }
                let io = script(frames);
                let written = std::sync::Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                client.begin_transaction(&root).await.unwrap();
                assert!(client.commit_transaction(&root, handle).await.is_err());
                let before = written.lock().unwrap().len();
                assert!(client.commit_transaction(&root, handle).await.is_err());
                assert_eq!(written.lock().unwrap().len(), before);
                let frames = sent_frames(&written.lock().unwrap());
                assert_eq!(
                    frames.iter().filter(|f| f.header().kind() == FrameKind::CommitTransaction).count(),
                    1
                );
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn empty_explicit_transaction_accepts_only_its_own_read_close() {
        let ((), report) = run_async_under_lab(0x79a0_0415, |root| async move {
            let handle = crate::body::TransactionHandle([0x61; 16]);
            let outcome = Outcome::ReadClosed { seq: 3, statements: 0 };
            let io = script(vec![
                transaction_begun(4, handle),
                rpc(
                    FrameKind::TransactionCommitted,
                    5,
                    StreamId::CONTROL,
                    &crate::body::TransactionCompletion { handle, outcome },
                ),
                rpc(FrameKind::Pong, 6, StreamId::CONTROL, &Ping { nonce: 9 }),
            ]);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "test").await.unwrap();
            client.begin_transaction(&root).await.unwrap();
            assert_eq!(client.commit_transaction(&root, handle).await.unwrap(), outcome);
            client.ping(&root, 9).await.unwrap();
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
    fn managed_transaction_statements() -> Vec<Execute> {
        vec![
            Execute {
                mode: ExecuteMode::Write,
                statement: "CREATE (n {value:$a}) RETURN $z AS value".into(),
                parameters: vec![
                    ("z".into(), WireValue::Int(11)),
                    ("a".into(), WireValue::Int(101)),
                ],
            },
            Execute {
                mode: ExecuteMode::Read,
                statement: "MATCH (n) WHERE n.value=$a RETURN $z AS value".into(),
                parameters: vec![
                    ("z".into(), WireValue::Int(21)),
                    ("a".into(), WireValue::Int(202)),
                ],
            },
        ]
    }

    fn transaction_completion(
        request: u64,
        handle: crate::body::TransactionHandle,
        outcome: Outcome,
    ) -> Frame {
        rpc(
            FrameKind::TransactionCommitted,
            request,
            StreamId::CONTROL,
            &crate::body::TransactionCompletion { handle, outcome },
        )
    }

    #[test]
    fn transaction_batch_collects_mixed_statements_and_fresh_parameters() {
        let ((), report) = run_async_under_lab(0x79a0_0610, |root| async move {
            let handle = crate::body::TransactionHandle([0x71; 16]);
            let outcome = Outcome::WriteCommitted {
                seq: 4,
                statements: 3,
            };
            let io = script(vec![
                transaction_begun(4, handle),
                rpc(
                    FrameKind::TransactionResultChunk,
                    5,
                    StreamId([5; 16]),
                    &ResultChunk {
                        columns: Some(vec![]),
                        rows: vec![],
                    },
                ),
                transaction_end(5, handle, 1, 0),
                transaction_chunk(6, &[11, 12]),
                transaction_end(6, handle, 2, 2),
                transaction_chunk(7, &[21]),
                transaction_end(7, handle, 3, 1),
                transaction_completion(8, handle, outcome),
                rpc(FrameKind::Pong, 9, StreamId::CONTROL, &Ping { nonce: 17 }),
            ]);
            let written = std::sync::Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "test").await.unwrap();
            let mut statements = managed_transaction_statements();
            statements.insert(
                0,
                Execute {
                    mode: ExecuteMode::Write,
                    statement: "CREATE (n {value:$a})".into(),
                    parameters: vec![("a".into(), WireValue::Int(303))],
                },
            );
            let mut expected = statements.clone();
            for statement in &mut expected {
                statement.parameters.sort_by(|a, b| a.0.cmp(&b.0));
            }
            let answer = client
                .execute_transaction_batch(&root, statements)
                .await
                .unwrap();
            assert_eq!(answer.results.len(), 3);
            assert!(answer.results[0].columns.is_empty());
            assert!(answer.results[0].rows.is_empty());
            assert_eq!(answer.results[1].columns, ["value"]);
            assert_eq!(
                answer.results[1].rows,
                [vec![WireValue::Int(11)], vec![WireValue::Int(12)]]
            );
            assert_eq!(answer.results[2].columns, ["value"]);
            assert_eq!(answer.results[2].rows, [vec![WireValue::Int(21)]]);
            assert_eq!(answer.end.basis, 3);
            assert_eq!(answer.end.statements, 3);
            assert_eq!(answer.end.rows, 3);
            assert_eq!(answer.end.outcome, outcome);
            client.ping(&root, 17).await.unwrap();

            let frames = sent_frames(&written.lock().unwrap());
            let executed: Vec<_> = frames
                .iter()
                .filter(|frame| frame.header().kind() == FrameKind::ExecuteTransaction)
                .map(|frame| crate::body::ExecuteTransaction::decode(frame.payload()).unwrap())
                .collect();
            assert_eq!(executed.len(), 3);
            for (actual, expected) in executed.iter().zip(&expected) {
                assert_eq!(actual.handle, handle);
                assert_eq!(&actual.statement, expected);
            }
            let tail: Vec<_> = frames[3..]
                .iter()
                .map(|frame| frame.header().kind())
                .collect();
            assert_eq!(
                tail,
                [
                    FrameKind::BeginTransaction,
                    FrameKind::ExecuteTransaction,
                    FrameKind::ExecuteTransaction,
                    FrameKind::ExecuteTransaction,
                    FrameKind::CommitTransaction,
                    FrameKind::Ping,
                ]
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn transaction_batch_callbacks_are_ordered_and_finish_before_commit() {
        #[derive(Debug, PartialEq)]
        enum Event {
            Columns(u32, u64, Vec<String>),
            Row(u32, Vec<WireValue>),
            End(u32, u64),
        }

        let ((), report) = run_async_under_lab(0x79a0_0611, |root| async move {
            let handle = crate::body::TransactionHandle([0x71; 16]);
            let outcome = Outcome::WriteCommitted {
                seq: 4,
                statements: 2,
            };
            let io = script(vec![
                transaction_begun(4, handle),
                transaction_chunk(5, &[11, 12]),
                transaction_end(5, handle, 1, 2),
                rpc(
                    FrameKind::TransactionResultChunk,
                    6,
                    StreamId([6; 16]),
                    &ResultChunk {
                        columns: Some(vec!["other".into()]),
                        rows: vec![vec![WireValue::Text("fresh".into())]],
                    },
                ),
                rpc(
                    FrameKind::TransactionResultEnd,
                    6,
                    StreamId([6; 16]),
                    &crate::body::TransactionResultEnd {
                        handle,
                        statement: 2,
                        rows: 1,
                    },
                ),
                transaction_completion(7, handle, outcome),
            ]);
            let written = std::sync::Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "test").await.unwrap();
            let events = std::sync::Mutex::new(Vec::new());
            let end = client
                .execute_transaction_batch_streaming(
                    &root,
                    managed_transaction_statements(),
                    |index, basis, columns| {
                        events.lock().unwrap().push(Event::Columns(
                            index,
                            basis,
                            columns.to_vec(),
                        ));
                        Ok(())
                    },
                    |index, row| {
                        events.lock().unwrap().push(Event::Row(index, row));
                        Ok(())
                    },
                    |index, rows| {
                        let frames = sent_frames(&written.lock().unwrap());
                        assert!(
                            !frames.iter().any(|frame| {
                                frame.header().kind() == FrameKind::CommitTransaction
                            }),
                            "sink admission must finish before COMMIT is sent"
                        );
                        events.lock().unwrap().push(Event::End(index, rows));
                        Ok(())
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                events.into_inner().unwrap(),
                [
                    Event::Columns(0, 3, vec!["value".into()]),
                    Event::Row(0, vec![WireValue::Int(11)]),
                    Event::Row(0, vec![WireValue::Int(12)]),
                    Event::End(0, 2),
                    Event::Columns(1, 3, vec!["other".into()]),
                    Event::Row(1, vec![WireValue::Text("fresh".into())]),
                    Event::End(1, 1),
                ]
            );
            assert_eq!(end.basis, 3);
            assert_eq!(end.statements, 2);
            assert_eq!(end.rows, 3);
            assert_eq!(end.outcome, outcome);
            let frames = sent_frames(&written.lock().unwrap());
            assert_eq!(
                frames
                    .iter()
                    .filter(|frame| frame.header().kind() == FrameKind::CommitTransaction)
                    .count(),
                1
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn transaction_batch_admits_all_inputs_and_selector_overhead_before_begin() {
        let ((), report) = run_async_under_lab(0x79a0_0612, |root| async move {
            assert_eq!(transaction::MAX_TRANSACTION_BATCH_STATEMENTS, 32);
            for scenario in 0..8 {
                let io = script(vec![rpc(
                    FrameKind::Pong,
                    4,
                    StreamId::CONTROL,
                    &Ping { nonce: 18 },
                )]);
                let written = std::sync::Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                let mut statements = managed_transaction_statements();
                match scenario {
                    0 => statements.clear(),
                    1 => {
                        statements = vec![
                            transaction_statement(ExecuteMode::Read);
                            transaction::MAX_TRANSACTION_BATCH_STATEMENTS + 1
                        ];
                    }
                    2 => statements[1].mode = ExecuteMode::ReadOrdered,
                    3 => {
                        statements[1]
                            .parameters
                            .push(("a".into(), WireValue::Int(999)));
                    }
                    4 => statements[1].statement = "x".repeat(5000),
                    5 => statements[1].statement = "   ".into(),
                    6 => {
                        statements[1].parameters = vec![(
                            "map".into(),
                            WireValue::Map(vec![
                                ("z".into(), WireValue::Int(1)),
                                ("a".into(), WireValue::Int(2)),
                            ]),
                        )];
                    }
                    _ => {
                        // A plain EXECUTE fits exactly, but the transaction
                        // selector adds sixteen bytes. Admission must exercise
                        // the actual request envelope before opening any owner.
                        let mut statement = Execute {
                            mode: ExecuteMode::Read,
                            statement: String::new(),
                            parameters: vec![],
                        };
                        let binding = Binding::Ready(ready().binding(session()));
                        let capacity = 4096
                            - binding.header_len()
                            - statement.encode().unwrap().len();
                        statement.statement = "x".repeat(capacity);
                        assert!(Frame::new(
                            FrameKind::Execute,
                            4,
                            StreamId::CONTROL,
                            binding,
                            statement.encode().unwrap(),
                            FrameLimits::new(4096).unwrap(),
                        )
                        .is_ok());
                        assert!(Frame::new(
                            FrameKind::ExecuteTransaction,
                            4,
                            StreamId::CONTROL,
                            binding,
                            crate::body::ExecuteTransaction {
                                handle: crate::body::TransactionHandle([1; 16]),
                                statement: statement.clone(),
                            }
                            .encode()
                            .unwrap(),
                            FrameLimits::new(4096).unwrap(),
                        )
                        .is_err());
                        statements[1] = statement;
                    }
                }
                let before = written.lock().unwrap().len();
                assert!(
                    client
                        .execute_transaction_batch(&root, statements)
                        .await
                        .is_err(),
                    "scenario {scenario}"
                );
                assert_eq!(written.lock().unwrap().len(), before, "scenario {scenario}");
                client.ping(&root, 18).await.unwrap();
                let frames = sent_frames(&written.lock().unwrap());
                assert_eq!(frames[3..].len(), 1);
                assert_eq!(frames[3].header().kind(), FrameKind::Ping);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn transaction_batch_end_callback_refusal_rolls_back_before_reuse() {
        let ((), report) = run_async_under_lab(0x79a0_0613, |root| async move {
            let handle = crate::body::TransactionHandle([0x71; 16]);
            let io = script(vec![
                transaction_begun(4, handle),
                transaction_chunk(5, &[11]),
                transaction_end(5, handle, 1, 1),
                transaction_chunk(6, &[21, 22]),
                transaction_end(6, handle, 2, 2),
                rpc(
                    FrameKind::TransactionRolledBack,
                    7,
                    StreamId::CONTROL,
                    &crate::body::TransactionSelector { handle },
                ),
                rpc(FrameKind::Pong, 8, StreamId::CONTROL, &Ping { nonce: 19 }),
            ]);
            let written = std::sync::Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "test").await.unwrap();
            let mut statements = managed_transaction_statements();
            statements.push(transaction_statement(ExecuteMode::Write));
            let mut ends = Vec::new();
            let result = client
                .execute_transaction_batch_streaming(
                    &root,
                    statements,
                    |_, _, _| Ok(()),
                    |_, _| Ok(()),
                    |index, rows| {
                        ends.push((index, rows));
                        if index == 1 {
                            Err(ClientError::Io(std::io::ErrorKind::BrokenPipe))
                        } else {
                            Ok(())
                        }
                    },
                )
                .await;
            assert!(matches!(
                result,
                Err(ClientError::Io(std::io::ErrorKind::BrokenPipe))
            ));
            assert_eq!(ends, [(0, 1), (1, 2)]);
            client.ping(&root, 19).await.unwrap();
            let frames = sent_frames(&written.lock().unwrap());
            let tail: Vec<_> = frames[3..]
                .iter()
                .map(|frame| frame.header().kind())
                .collect();
            assert_eq!(
                tail,
                [
                    FrameKind::BeginTransaction,
                    FrameKind::ExecuteTransaction,
                    FrameKind::ExecuteTransaction,
                    FrameKind::RollbackTransaction,
                    FrameKind::Ping,
                ]
            );
            let rollback = &frames[6];
            assert_eq!(
                crate::body::TransactionSelector::decode(rollback.payload())
                    .unwrap()
                    .handle,
                handle
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn transaction_batch_failed_cleanup_fences_the_connection() {
        let ((), report) = run_async_under_lab(0x79a0_0614, |root| async move {
            for scenario in 0..3 {
                let handle = crate::body::TransactionHandle([0x71; 16]);
                let mut replies = vec![
                    transaction_begun(4, handle),
                    transaction_chunk(5, &[11]),
                    transaction_end(5, handle, 1, 1),
                ];
                match scenario {
                    0 => {}
                    1 => replies.push(rpc(
                        FrameKind::TransactionRolledBack,
                        6,
                        StreamId::CONTROL,
                        &crate::body::TransactionSelector {
                            handle: crate::body::TransactionHandle([0x72; 16]),
                        },
                    )),
                    _ => replies.push(rpc(
                        FrameKind::Error,
                        6,
                        StreamId::CONTROL,
                        &ErrorBody {
                            code: ErrorCode::Busy,
                            message: "rollback was not acknowledged".into(),
                        },
                    )),
                }
                let io = script(replies);
                let written = std::sync::Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                assert!(
                    client
                        .execute_transaction_batch_streaming(
                            &root,
                            vec![transaction_statement(ExecuteMode::Write)],
                            |_, _, _| Ok(()),
                            |_, _| Ok(()),
                            |_, _| Err(ClientError::Protocol("sink refused completed output")),
                        )
                        .await
                        .is_err()
                );
                let before = written.lock().unwrap().len();
                assert!(client.ping(&root, 20).await.is_err());
                assert!(client.begin_transaction(&root).await.is_err());
                assert_eq!(written.lock().unwrap().len(), before, "scenario {scenario}");
                let frames = sent_frames(&written.lock().unwrap());
                let tail: Vec<_> = frames[3..]
                    .iter()
                    .map(|frame| frame.header().kind())
                    .collect();
                assert_eq!(
                    tail,
                    [
                        FrameKind::BeginTransaction,
                        FrameKind::ExecuteTransaction,
                        FrameKind::RollbackTransaction,
                    ]
                );
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn transaction_batch_late_statement_refusal_never_commits_a_prefix() {
        let ((), report) = run_async_under_lab(0x79a0_0615, |root| async move {
            let handle = crate::body::TransactionHandle([0x71; 16]);
            let io = script(vec![
                transaction_begun(4, handle),
                transaction_chunk(5, &[11]),
                transaction_end(5, handle, 1, 1),
                rpc(
                    FrameKind::Error,
                    6,
                    StreamId([6; 16]),
                    &ErrorBody {
                        code: ErrorCode::Execution,
                        message: "second statement refused".into(),
                    },
                ),
                rpc(FrameKind::Pong, 7, StreamId::CONTROL, &Ping { nonce: 21 }),
            ]);
            let written = std::sync::Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "test").await.unwrap();
            let mut statements = managed_transaction_statements();
            statements.push(transaction_statement(ExecuteMode::Write));
            assert!(matches!(
                client.execute_transaction_batch(&root, statements).await,
                Err(ClientError::Server {
                    code: ErrorCode::Execution,
                    ..
                })
            ));
            client.ping(&root, 21).await.unwrap();
            let frames = sent_frames(&written.lock().unwrap());
            let tail: Vec<_> = frames[3..]
                .iter()
                .map(|frame| frame.header().kind())
                .collect();
            assert_eq!(
                tail,
                [
                    FrameKind::BeginTransaction,
                    FrameKind::ExecuteTransaction,
                    FrameKind::ExecuteTransaction,
                    FrameKind::Ping,
                ]
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn transaction_batch_callback_cancellation_stops_every_later_statement() {
        let ((), report) = run_async_under_lab(0x79a0_0616, |root| async move {
            for reject_columns in [false, true] {
                let handle = crate::body::TransactionHandle([0x71; 16]);
                let io = script(vec![
                    transaction_begun(4, handle),
                    transaction_chunk(5, &[11, 12]),
                    // END may already be in flight when the callback rejects.
                    transaction_end(5, handle, 1, 2),
                    rpc(FrameKind::Pong, 7, StreamId::CONTROL, &Ping { nonce: 22 }),
                ]);
                let written = std::sync::Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                let mut rows_seen = 0;
                let mut ends_seen = 0;
                let result = client
                    .execute_transaction_batch_streaming(
                        &root,
                        managed_transaction_statements(),
                        |_, _, _| {
                            if reject_columns {
                                Err(ClientError::Protocol("sink refused provisional output"))
                            } else {
                                Ok(())
                            }
                        },
                        |_, _| {
                            rows_seen += 1;
                            Err(ClientError::Protocol("sink refused provisional output"))
                        },
                        |_, _| {
                            ends_seen += 1;
                            Ok(())
                        },
                    )
                    .await;
                assert!(matches!(
                    result,
                    Err(ClientError::Protocol("sink refused provisional output"))
                ));
                assert_eq!(rows_seen, usize::from(!reject_columns));
                assert_eq!(ends_seen, 0);
                client.ping(&root, 22).await.unwrap();
                let frames = sent_frames(&written.lock().unwrap());
                let tail: Vec<_> = frames[3..]
                    .iter()
                    .map(|frame| frame.header().kind())
                    .collect();
                assert_eq!(
                    tail,
                    [
                        FrameKind::BeginTransaction,
                        FrameKind::ExecuteTransaction,
                        FrameKind::QueryCancel,
                        FrameKind::Ping,
                    ]
                );
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn transaction_batch_ambiguous_commit_never_retries_or_rolls_back() {
        let ((), report) = run_async_under_lab(0x79a0_0617, |root| async move {
            for scenario in 0..6 {
                let handle = crate::body::TransactionHandle([0x71; 16]);
                let mut replies = vec![
                    transaction_begun(4, handle),
                    transaction_chunk(5, &[11]),
                    transaction_end(5, handle, 1, 1),
                ];
                match scenario {
                    0 => {}
                    1 => replies.push(transaction_completion(
                        6,
                        crate::body::TransactionHandle([0x72; 16]),
                        Outcome::WriteCommitted {
                            seq: 4,
                            statements: 1,
                        },
                    )),
                    2 => replies.push(rpc(
                        FrameKind::Error,
                        6,
                        StreamId::CONTROL,
                        &ErrorBody {
                            code: ErrorCode::OutcomeUnknown,
                            message: "commit outcome unknown; do not replay".into(),
                        },
                    )),
                    3 => replies.push(transaction_completion(
                        6,
                        handle,
                        Outcome::ReadClosed {
                            seq: 4,
                            statements: 1,
                        },
                    )),
                    4 => replies.push(transaction_completion(
                        6,
                        handle,
                        Outcome::WriteCommitted {
                            seq: 4,
                            statements: 2,
                        },
                    )),
                    _ => replies.push(
                        Frame::new(
                            FrameKind::TransactionCommitted,
                            6,
                            StreamId::CONTROL,
                            Binding::Ready(ready().binding(session())),
                            vec![0x71; 16],
                            FrameLimits::new(4096).unwrap(),
                        )
                        .unwrap(),
                    ),
                }
                let io = script(replies);
                let written = std::sync::Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                let result = client
                    .execute_transaction_batch(
                        &root,
                        vec![transaction_statement(ExecuteMode::Write)],
                    )
                    .await;
                if scenario == 2 {
                    assert!(matches!(
                        result,
                        Err(ClientError::Server {
                            code: ErrorCode::OutcomeUnknown,
                            ..
                        })
                    ));
                } else {
                    assert!(result.is_err(), "scenario {scenario}");
                }
                let before = written.lock().unwrap().len();
                assert!(client.commit_transaction(&root, handle).await.is_err());
                if scenario != 2 {
                    assert!(client.ping(&root, 23).await.is_err());
                }
                assert_eq!(written.lock().unwrap().len(), before, "scenario {scenario}");
                let frames = sent_frames(&written.lock().unwrap());
                let tail: Vec<_> = frames[3..]
                    .iter()
                    .map(|frame| frame.header().kind())
                    .collect();
                assert_eq!(
                    tail,
                    [
                        FrameKind::BeginTransaction,
                        FrameKind::ExecuteTransaction,
                        FrameKind::CommitTransaction,
                    ]
                );
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn transaction_batch_end_callback_unwind_fences_the_idle_owner() {
        let ((), report) = run_async_under_lab(0x79a0_0618, |root| async move {
            let handle = crate::body::TransactionHandle([0x71; 16]);
            let io = script(vec![
                transaction_begun(4, handle),
                transaction_chunk(5, &[11]),
                transaction_end(5, handle, 1, 1),
            ]);
            let written = std::sync::Arc::clone(&io.written);
            let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
            client.select(&root, "test").await.unwrap();
            {
                let mut pending = Box::pin(client.execute_transaction_batch_streaming(
                    &root,
                    vec![transaction_statement(ExecuteMode::Write)],
                    |_, _, _| Ok(()),
                    |_, _| Ok(()),
                    |_, _| panic!("sink unwound after the provisional END"),
                ));
                std::future::poll_fn(|task| {
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        std::future::Future::poll(pending.as_mut(), task)
                    })) {
                        Err(_) => Poll::Ready(()),
                        Ok(Poll::Pending) => Poll::Pending,
                        Ok(Poll::Ready(_)) => panic!("the sink must unwind"),
                    }
                })
                .await;
            }
            let before = written.lock().unwrap().len();
            assert!(client.ping(&root, 24).await.is_err());
            assert!(client.begin_transaction(&root).await.is_err());
            assert_eq!(written.lock().unwrap().len(), before);
            let frames = sent_frames(&written.lock().unwrap());
            let tail: Vec<_> = frames[3..]
                .iter()
                .map(|frame| frame.header().kind())
                .collect();
            assert_eq!(
                tail,
                [FrameKind::BeginTransaction, FrameKind::ExecuteTransaction]
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

}

#[cfg(test)]
mod refresh_tests {
    use super::*;

    #[test]
    fn refresh_requires_the_exact_successor_before_accepting_a_body() {
        let session = crate::SessionBinding {
            transcript: [7; 32],
            auth_generation: 9,
        };
        let ready = ReadyBinding {
            session,
            namespace: [1; 32],
            incarnation: [2; 32],
            service_epoch: 3,
            posture: crate::Posture::Local,
            authority_commitment: [4; 32],
        };
        for previous in [Binding::Session(session), Binding::Ready(ready)] {
            let successor = refresh_successor(previous).unwrap();
            assert_eq!(successor.session().unwrap().transcript, session.transcript);
            assert_eq!(successor.session().unwrap().auth_generation, 10);
            let frame = |kind, request, stream, binding| {
                Frame::new(
                    kind,
                    request,
                    stream,
                    binding,
                    vec![],
                    FrameLimits::new(4096).unwrap(),
                )
                .unwrap()
            };
            let ok = frame(FrameKind::AuthRefreshed, 12, StreamId::CONTROL, successor);
            assert_eq!(refresh_header(ok.header(), 12, previous, successor), Ok(()));
            let refusal = frame(FrameKind::Error, 12, StreamId::CONTROL, previous);
            assert_eq!(
                refresh_header(refusal.header(), 12, previous, successor),
                Ok(())
            );
            for bad in [
                frame(FrameKind::AuthRefreshed, 12, StreamId::CONTROL, previous),
                frame(
                    FrameKind::AuthRefreshed,
                    12,
                    StreamId::CONTROL,
                    refresh_successor(successor).unwrap(),
                ),
                frame(FrameKind::AuthRefreshed, 13, StreamId::CONTROL, successor),
                frame(FrameKind::AuthRefreshed, 12, StreamId([1; 16]), successor),
                frame(FrameKind::AuthOk, 12, StreamId::CONTROL, Binding::Transport),
                frame(FrameKind::Error, 12, StreamId::CONTROL, successor),
            ] {
                assert!(refresh_header(bad.header(), 12, previous, successor).is_err());
            }
        }
        let Binding::Ready(successor) = refresh_successor(Binding::Ready(ready)).unwrap() else {
            panic!("Ready remains Ready")
        };
        assert_eq!(
            ReadyBinding {
                session,
                ..successor
            },
            ready
        );
        for changed in [
            ReadyBinding {
                namespace: [9; 32],
                ..successor
            },
            ReadyBinding {
                incarnation: [9; 32],
                ..successor
            },
            ReadyBinding {
                authority_commitment: [9; 32],
                ..successor
            },
            ReadyBinding {
                service_epoch: 4,
                ..successor
            },
            ReadyBinding {
                posture: crate::Posture::Sharded,
                ..successor
            },
        ] {
            let frame = Frame::new(
                FrameKind::AuthRefreshed,
                12,
                StreamId::CONTROL,
                Binding::Ready(changed),
                vec![],
                FrameLimits::new(4096).unwrap(),
            )
            .unwrap();
            assert!(
                refresh_header(
                    frame.header(),
                    12,
                    Binding::Ready(ready),
                    Binding::Ready(successor)
                )
                .is_err()
            );
        }
        assert!(refresh_successor(Binding::Transport).is_err());
        assert!(
            refresh_successor(Binding::Session(crate::SessionBinding {
                auth_generation: u64::MAX,
                ..session
            }))
            .is_err()
        );
    }
}

#[cfg(test)]
mod result_tests {
    use super::ResultProgress;
    use crate::body::{Body, Outcome, ResultChunk, ResultEnd, WireValue};

    fn chunk(columns: bool, rows: &[&[i64]]) -> ResultChunk {
        let body = ResultChunk {
            columns: columns.then(|| vec!["value".into()]),
            rows: rows
                .iter()
                .map(|row| row.iter().map(|value| WireValue::Int(*value)).collect())
                .collect(),
        };
        ResultChunk::decode(&body.encode().unwrap()).unwrap()
    }

    fn end(rows: u64) -> ResultEnd {
        ResultEnd {
            outcome: Outcome::Rows { seq: 3 },
            rows,
        }
    }

    #[test]
    fn finite_result_requires_one_schema_and_exact_terminal_rows() {
        let mut progress = ResultProgress::default();
        assert_eq!(
            progress.finish(&end(0), 196, 196),
            Err("END before columns")
        );
        assert_eq!(
            progress.observe_chunk(&chunk(false, &[&[1]])),
            Err("columns out of order")
        );
        progress.observe_chunk(&chunk(true, &[])).unwrap();
        assert_eq!(
            progress.observe_chunk(&chunk(true, &[&[1]])),
            Err("columns out of order")
        );
        progress
            .observe_chunk(&chunk(false, &[&[1], &[2]]))
            .unwrap();
        progress.observe_chunk(&chunk(false, &[])).unwrap();
        progress.observe_chunk(&chunk(false, &[&[3]])).unwrap();
        for rows in [0, 2, 4, u64::MAX] {
            assert_eq!(
                progress.finish(&end(rows), 196, 196),
                Err("END row count differs from delivery")
            );
        }
        assert_eq!(progress.finish(&end(3), 196, 196), Ok(()));
        assert_eq!(
            progress.finish(&end(3), 196, 195),
            Err("server exceeded its flow credit")
        );
    }

    #[test]
    fn malformed_rows_never_install_a_schema_or_a_partial_chunk_count() {
        let mut progress = ResultProgress::default();
        for rows in [&[&[1][..], &[][..]][..], &[&[1][..], &[2, 3][..]][..]] {
            assert_eq!(
                progress.observe_chunk(&chunk(true, rows)),
                Err("result row width differs from columns")
            );
            assert_eq!(progress.columns, None);
            assert_eq!(progress.rows, 0);
        }
        progress.observe_chunk(&chunk(true, &[&[1]])).unwrap();
        for rows in [&[&[2][..], &[][..]][..], &[&[2][..], &[3, 4][..]][..]] {
            assert_eq!(
                progress.observe_chunk(&chunk(false, rows)),
                Err("result row width differs from columns")
            );
            assert_eq!(progress.columns, Some(1));
            assert_eq!(progress.rows, 1);
        }
        assert_eq!(progress.finish(&end(1), 196, 196), Ok(()));
    }

    #[test]
    fn finite_result_accepts_empty_schemas_and_every_completion_class() {
        for rows in 0..=2 {
            let mut progress = ResultProgress::default();
            progress
                .observe_chunk(&ResultChunk {
                    columns: Some(vec![]),
                    rows: vec![vec![]; rows],
                })
                .unwrap();
            for outcome in [
                Outcome::Rows { seq: 3 },
                Outcome::WriteCommitted {
                    seq: 3,
                    statements: 1,
                },
                Outcome::ReadClosed {
                    seq: 3,
                    statements: 1,
                },
            ] {
                assert_eq!(
                    progress.finish(
                        &ResultEnd {
                            outcome,
                            rows: rows as u64
                        },
                        212,
                        212
                    ),
                    Ok(())
                );
            }
        }
    }

    #[test]
    fn finite_result_row_overflow_refuses_without_changing_progress() {
        let mut progress = ResultProgress {
            columns: Some(1),
            rows: u64::MAX,
        };
        assert_eq!(
            progress.observe_chunk(&chunk(false, &[&[1]])),
            Err("result row count overflow")
        );
        assert_eq!(progress.columns, Some(1));
        assert_eq!(progress.rows, u64::MAX);
        assert_eq!(progress.finish(&end(u64::MAX), 196, 196), Ok(()));
    }
}

#[cfg(test)]
mod subscription_progress_tests {
    use super::SubscriptionProgress;
    use crate::body::{Body, Outcome, ResultEnd, SubscriptionBatch, SubscriptionReset, WireValue};

    fn subscription_part(
        frontier: u64,
        snapshot: bool,
        last: bool,
        columns: bool,
    ) -> SubscriptionBatch {
        SubscriptionBatch {
            frontier,
            snapshot,
            last,
            columns: columns.then(|| vec!["value".into()]),
            entries: vec![(1, vec![WireValue::Int(7)])],
        }
    }

    fn subscription_wire(part: &SubscriptionBatch) -> SubscriptionBatch {
        SubscriptionBatch::decode(&part.encode().unwrap()).unwrap()
    }

    #[test]
    fn subscription_progress_preserves_fragmented_baselines_deltas_and_gap_replacements() {
        let mut progress = SubscriptionProgress::default();
        progress
            .batch(&subscription_wire(&subscription_part(7, true, false, true)))
            .unwrap();
        let end = ResultEnd {
            outcome: Outcome::Rows { seq: 7 },
            rows: 0,
        };
        assert!(progress.end(&end, false).is_err());
        assert!(progress.end(&end, true).is_err());
        progress
            .reset(&SubscriptionReset {
                last_delivered_seq: None,
            })
            .unwrap();
        progress
            .batch(&subscription_wire(&subscription_part(7, true, true, false)))
            .unwrap();
        let mut delta = subscription_part(9, false, true, false);
        delta.entries = vec![(-2, vec![WireValue::Int(7)]), (3, vec![WireValue::Int(8)])];
        progress.batch(&subscription_wire(&delta)).unwrap();
        let mut empty = subscription_part(10, false, true, false);
        empty.entries.clear();
        progress.batch(&subscription_wire(&empty)).unwrap();
        for frontier in [10, 20] {
            progress
                .batch(&subscription_wire(&subscription_part(
                    frontier, true, true, false,
                )))
                .unwrap();
            progress
                .end(
                    &ResultEnd {
                        outcome: Outcome::Rows { seq: frontier },
                        rows: 0,
                    },
                    false,
                )
                .unwrap();
            progress
                .reset(&SubscriptionReset {
                    last_delivered_seq: Some(frontier),
                })
                .unwrap();
        }
    }

    #[test]
    fn subscription_progress_rejects_bad_shapes_without_advancing_delivery() {
        let mut progress = SubscriptionProgress::default();
        for bad in [
            subscription_part(7, false, true, true),
            subscription_part(7, true, true, false),
        ] {
            assert!(progress.batch(&subscription_wire(&bad)).is_err());
        }
        let mut wrong_width = subscription_part(7, true, true, true);
        wrong_width.entries[0].1.push(WireValue::Null);
        assert!(progress.batch(&subscription_wire(&wrong_width)).is_err());
        let mut negative_baseline = subscription_part(7, true, true, true);
        negative_baseline.entries[0].0 = -1;
        assert!(
            progress
                .batch(&subscription_wire(&negative_baseline))
                .is_err()
        );
        progress
            .batch(&subscription_part(7, true, true, true))
            .unwrap();
        for (frontier, snapshot) in [(6, false), (7, false), (6, true)] {
            assert!(
                progress
                    .batch(&subscription_part(frontier, snapshot, true, false))
                    .is_err()
            );
        }
        assert!(
            progress
                .batch(&subscription_part(8, false, true, true))
                .is_err()
        );
        progress
            .batch(&subscription_part(8, false, false, false))
            .unwrap();
        for (frontier, snapshot) in [(9, false), (8, true)] {
            assert!(
                progress
                    .batch(&subscription_part(frontier, snapshot, true, false))
                    .is_err()
            );
        }
        progress
            .reset(&SubscriptionReset {
                last_delivered_seq: Some(7),
            })
            .unwrap();
        assert!(
            progress
                .reset(&SubscriptionReset {
                    last_delivered_seq: Some(8),
                })
                .is_err()
        );
        progress
            .batch(&subscription_part(8, false, true, false))
            .unwrap();
    }

    #[test]
    fn subscription_terminals_require_the_last_complete_wire_checkpoint() {
        let end = ResultEnd {
            outcome: Outcome::Rows { seq: 7 },
            rows: 0,
        };
        let mut progress = SubscriptionProgress::default();
        assert!(progress.end(&end, false).is_err());
        progress
            .batch(&subscription_part(7, true, true, true))
            .unwrap();
        for invalid in [
            ResultEnd {
                outcome: Outcome::Rows { seq: 8 },
                rows: 0,
            },
            ResultEnd {
                outcome: Outcome::Rows { seq: 7 },
                rows: 1,
            },
            ResultEnd {
                outcome: Outcome::WriteCommitted {
                    seq: 7,
                    statements: 1,
                },
                rows: 0,
            },
            ResultEnd {
                outcome: Outcome::ReadClosed {
                    seq: 7,
                    statements: 1,
                },
                rows: 0,
            },
        ] {
            assert!(progress.end(&invalid, false).is_err());
            assert!(progress.end(&invalid, true).is_err());
        }
        progress.end(&end, false).unwrap();
        progress
            .batch(&subscription_part(8, false, false, false))
            .unwrap();
        assert!(progress.end(&end, false).is_err());
        progress.end(&end, true).unwrap();
        progress
            .reset(&SubscriptionReset {
                last_delivered_seq: Some(7),
            })
            .unwrap();
        assert!(
            progress
                .end(
                    &ResultEnd {
                        outcome: Outcome::Rows { seq: 8 },
                        rows: 0,
                    },
                    true,
                )
                .is_err()
        );
    }
}
