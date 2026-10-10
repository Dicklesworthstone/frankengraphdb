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
    ErrorCode, Execute, ExecuteMode, ExecutePrepared, Hello, HelloAck, Outcome, Ping, Prepare,
    Prepared, PreparedHandle, Ready, ReleasePrepared, ResultChunk, ResultEnd, SelectDatabase,
    SubscriptionBatch, WindowUpdate, WireValue,
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

/// What a client can fail with. Server refusals keep their public class.
#[derive(Debug)]
pub enum ClientError {
    Io(std::io::ErrorKind),
    Transport(TransportError),
    Body(BodyError),
    /// The server broke the protocol (an unexpected frame, binding or body).
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
    selected: Option<Selected>,
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
            selected: None,
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
                |names| columns = names.to_vec(),
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

    /// Prepare one native read template without executing it or pinning a
    /// snapshot. Representative parameters declare structural operand types;
    /// execute supplies every value again. Handles belong to this connection
    /// and are invalidated by authority refresh, database recovery or release.
    pub async fn prepare_read(
        &mut self,
        cx: &Cx,
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
                    statement: statement.to_owned(),
                    parameters,
                },
            )
            .await?;
        let frame = self.reply(cx, request).await?;
        if !frame.header().stream_id().is_control() {
            return Err(ClientError::Protocol("PREPARED on a child stream"));
        }
        Ok(expect::<Prepared>(&frame, FrameKind::Prepared)?.handle)
    }

    /// Drop a connection-owned read template. Repeated/unknown releases have
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
            return Err(ClientError::Protocol("released a different prepared handle"));
        }
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
                handle,
                parameters,
                |names| columns = names.to_vec(),
                |row| {
                    rows.push(row);
                    Ok(())
                },
            )
            .await?;
        Ok(Answer { columns, rows, outcome })
    }

    /// Rebind real typed operands and stream a prepared read. A callback
    /// refusal sends QUERY_CANCEL and drains its terminal before returning,
    /// so a later execution cannot consume the cancelled result's frames.
    pub async fn execute_prepared_streaming(
        &mut self,
        cx: &Cx,
        handle: PreparedHandle,
        mut parameters: Vec<(String, WireValue)>,
        on_columns: impl FnMut(&[String]),
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
                &ExecutePrepared { handle, parameters },
            )
            .await?;
        let outcome = self.receive_result(cx, request, on_columns, on_row).await?;
        if !matches!(outcome, Outcome::Rows { .. }) {
            return Err(ClientError::Protocol("prepared read returned a write outcome"));
        }
        Ok(outcome)
    }

    /// Run one statement, handing over its columns once and then each row as
    /// it arrives. An error from `on_row` cancels and drains the stream before
    /// returning that error, leaving the connection ready for another request.
    pub async fn execute_streaming(
        &mut self,
        cx: &Cx,
        mode: ExecuteMode,
        statement: &str,
        mut parameters: Vec<(String, WireValue)>,
        on_columns: impl FnMut(&[String]),
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
        mut on_columns: impl FnMut(&[String]),
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
        let mut seen_columns = false;
        loop {
            let frame = self.receive(cx).await?.ok_or(ClientError::Closed)?;
            let header = *frame.header();
            if header.kind() == FrameKind::Goodbye {
                return Err(ClientError::Closed);
            }
            if header.request_id() != request {
                return Err(ClientError::Protocol("result frame for a different request"));
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
                    if stream.is_none() {
                        return Err(ClientError::Protocol("chunk on the control stream"));
                    }
                    let chunk = ResultChunk::decode(frame.payload())?;
                    match (chunk.columns, seen_columns) {
                        (Some(names), false) => {
                            seen_columns = true;
                            on_columns(&names);
                        }
                        (None, true) => {}
                        _ => return Err(ClientError::Protocol("columns out of order")),
                    }
                    let count = chunk.rows.len() as u64;
                    let cost = header.frame_len() as u64;
                    if cost > available.0 || count > available.1 {
                        return Err(ClientError::Protocol("server exceeded its flow credit"));
                    }
                    available = (available.0 - cost, available.1 - count);
                    for row in chunk.rows {
                        if callback_error.is_none()
                            && let Err(error) = on_row(row)
                        {
                            let id = stream.expect("a chunk has a child stream");
                            self.send(cx, FrameKind::QueryCancel, id, &Empty).await?;
                            callback_error = Some(error);
                        }
                    }
                    let frame_limit = self.send_limits.max_frame_len() as u64;
                    if callback_error.is_none()
                        && (available.0 < frame_limit || available.1 == 0)
                    {
                        let Some(id) = stream else {
                            return Err(ClientError::Protocol("chunk on the control stream"));
                        };
                        grants += 1;
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
                    if !seen_columns {
                        return Err(ClientError::Protocol("END before columns"));
                    }
                    let outcome = ResultEnd::decode(frame.payload())?.outcome;
                    return callback_error.map_or(Ok(outcome), Err);
                }
                FrameKind::Error => {
                    let refusal = ErrorBody::decode(frame.payload())?;
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
        let mut seen_columns = false;
        let mut cancelled = false;
        let mut pending: Option<Change> = None;
        let mut completed_wire = None;
        let mut delivered_to_callback = None;
        loop {
            let frame = self.reply(cx, request).await?;
            let header = *frame.header();
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
                    let part = SubscriptionBatch::decode(frame.payload())?;
                    match (part.columns, seen_columns) {
                        (Some(names), false) => {
                            seen_columns = true;
                            on_columns(&names);
                        }
                        (None, true) => {}
                        _ => return Err(ClientError::Protocol("columns out of order")),
                    }
                    let count = part.entries.len() as u64;
                    let cost = header.frame_len() as u64;
                    if cost > available.0 || count > available.1 {
                        return Err(ClientError::Protocol("server exceeded its flow credit"));
                    }
                    available = (available.0 - cost, available.1 - count);
                    let change = pending.get_or_insert_with(|| Change {
                        frontier: part.frontier,
                        snapshot: part.snapshot,
                        entries: Vec::new(),
                    });
                    if change.frontier != part.frontier || change.snapshot != part.snapshot {
                        return Err(ClientError::Protocol("a batch changed frontier mid-stream"));
                    }
                    change.entries.extend(part.entries);
                    if part.last {
                        let change = pending.take().expect("a pending batch was just extended");
                        completed_wire = Some(change.frontier);
                        if !cancelled {
                            let frontier = change.frontier;
                            let keep = on_change(change)?;
                            delivered_to_callback = Some(frontier);
                            if !keep {
                                cancelled = true;
                                let Some(id) = stream else {
                                    return Err(ClientError::Protocol(
                                        "batch on the control stream",
                                    ));
                                };
                                self.send(cx, FrameKind::QueryCancel, id, &Empty).await?;
                            }
                        }
                    }
                    let frame_limit = self.send_limits.max_frame_len() as u64;
                    if available.0 < frame_limit || available.1 == 0 {
                        let Some(id) = stream else {
                            return Err(ClientError::Protocol("batch on the control stream"));
                        };
                        grants += 1;
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
                    let Outcome::Rows { seq } = ResultEnd::decode(frame.payload())?.outcome else {
                        return Err(ClientError::Protocol(
                            "a subscription ended with a write outcome",
                        ));
                    };
                    return Ok(if cancelled {
                        delivered_to_callback.unwrap_or(0)
                    } else {
                        seq
                    });
                }
                FrameKind::SubscriptionReset => {
                    let reset = crate::body::SubscriptionReset::decode(frame.payload())?;
                    if stream.is_none() || reset.last_delivered_seq != completed_wire {
                        return Err(ClientError::Protocol(
                            "subscription reset checkpoint does not match completed delivery",
                        ));
                    }
                    return Err(ClientError::SubscriptionReset {
                        last_delivered_seq: delivered_to_callback,
                    });
                }
                FrameKind::Error => {
                    let error = server_error(&frame);
                    // A cancel that lands while a batch waits for credit ends
                    // the stream with this class instead of END.
                    if cancelled
                        && matches!(
                            error,
                            ClientError::Server {
                                code: ErrorCode::Cancelled,
                                ..
                            }
                        )
                    {
                        return Ok(delivered_to_callback.unwrap_or(0));
                    }
                    return Err(error);
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
            Ok(())
        } else {
            Err(ClientError::Protocol("PONG echoed a different value"))
        }
    }

    /// Ask the server to drain and wait for its GOODBYE.
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
        let request = self.next_request;
        self.next_request += 1;
        let frame = Frame::new(
            kind,
            request,
            stream,
            self.binding,
            body.encode()?,
            self.send_limits,
        )
        .map_err(|_| ClientError::Protocol("request exceeds the negotiated frame limit"))?;
        self.writer.queue(cx, &frame)?;
        self.writer.send(cx, |_| Ok(())).await?;
        Ok(request)
    }

    async fn receive(&mut self, cx: &Cx) -> Result<Option<Frame>, ClientError> {
        let expected = self.expected;
        Ok(self
            .reader
            .receive(cx, |header| server_header(header, expected))
            .await?)
    }

    /// The next frame answering `request`. A GOODBYE or EOF ends the session.
    async fn reply(&mut self, cx: &Cx, request: u64) -> Result<Frame, ClientError> {
        loop {
            let Some(frame) = self.receive(cx).await? else {
                return Err(ClientError::Closed);
            };
            let header = frame.header();
            if header.kind() == FrameKind::Goodbye {
                return Err(ClientError::Closed);
            }
            if header.request_id() == request {
                return Ok(frame);
            }
            // A connection-level ERROR (request 0 never occurs; a reply to an
            // earlier request is stale) is surfaced as is.
            if header.kind() == FrameKind::Error && header.stream_id().is_control() {
                return Err(server_error(&frame));
            }
        }
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
        FrameKind::Prepared | FrameKind::PreparedReleased => {
            if !matches!(expected, Binding::Ready(_)) || !header.stream_id().is_control() {
                return Err(ProtocolError::InvalidState);
            }
            header.binding() == expected
        }
        FrameKind::Pong
        | FrameKind::Goodbye
        | FrameKind::SnapshotResultChunk
        | FrameKind::SnapshotResultEnd
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::{ErrorBody, SubscriptionReset};
    use crate::{Posture, SessionBinding};
    use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
    use asupersync::lab::run_async_under_lab;
    use core::pin::Pin;
    use core::task::{Context, Poll};

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
                    initial_window_bytes: 65536,
                    initial_window_rows: 32,
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
            &ResultEnd { outcome: Outcome::Rows { seq: 3 }, rows },
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

    #[test]
    fn prepared_replies_require_the_current_ready_control_binding() {
        let current = Binding::Ready(ready().binding(session()));
        let limits = FrameLimits::new(4096).unwrap();
        for kind in [FrameKind::Prepared, FrameKind::PreparedReleased] {
            let reply = rpc(
                kind, 4, StreamId::CONTROL,
                &Prepared { handle: PreparedHandle([0x31; 16]) },
            );
            assert_eq!(server_header(reply.header(), current), Ok(()));
            let child = rpc(
                kind, 4, StreamId([5; 16]),
                &Prepared { handle: PreparedHandle([0x31; 16]) },
            );
            assert_eq!(
                server_header(child.header(), current),
                Err(ProtocolError::InvalidState),
            );
            for unselected in [Binding::Transport, Binding::Session(session())] {
                let frame = Frame::new(
                    kind, 4, StreamId::CONTROL, unselected, vec![], limits,
                ).unwrap();
                assert_eq!(
                    server_header(frame.header(), unselected),
                    Err(ProtocolError::InvalidState),
                );
            }
            let mut stale = ready().binding(session());
            stale.session.auth_generation += 1;
            let frame = Frame::new(
                kind, 4, StreamId::CONTROL, Binding::Ready(stale), vec![], limits,
            ).unwrap();
            assert_eq!(
                server_header(frame.header(), current),
                Err(ProtocolError::InvalidBinding),
            );
        }
    }

    #[test]
    fn prepared_client_rebinds_and_reuses_the_connection_after_callback_cancellation() {
        let ((), report) = run_async_under_lab(0x79a0_0301, |root| async move {
            for cancelled_terminal in [false, true] {
                let handle = PreparedHandle([0x31; 16]);
                let terminal = if cancelled_terminal {
                    rpc(
                        FrameKind::Error,
                        5,
                        StreamId([5; 16]),
                        &ErrorBody { code: ErrorCode::Cancelled, message: "cancelled".into() },
                    )
                } else {
                    result_end(5, 2)
                };
                let io = script(vec![
                    rpc(FrameKind::Prepared, 4, StreamId::CONTROL, &Prepared { handle }),
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
                        handle,
                        vec![("value".into(), WireValue::Int(1))],
                        |_| {},
                        |_| {
                            delivered += 1;
                            Err(ClientError::Protocol("caller stopped"))
                        },
                    )
                    .await;
                assert!(matches!(stopped, Err(ClientError::Protocol("caller stopped"))));
                assert_eq!(delivered, 1, "cancel suppresses all remaining callbacks");
                let result = client
                    .execute_prepared(
                        &root,
                        handle,
                        vec![("value".into(), WireValue::Int(20))],
                    )
                    .await
                    .unwrap();
                assert_eq!(result.rows, [vec![WireValue::Int(20)]]);
                client.release_prepared(&root, handle).await.unwrap();
                client.release_prepared(&root, handle).await.unwrap();
                let frames = sent_frames(&written.lock().unwrap());
                assert_eq!(
                    frames.iter().map(|frame| frame.header().kind()).collect::<Vec<_>>(),
                    [
                        FrameKind::Hello, FrameKind::Auth, FrameKind::SelectDatabase,
                        FrameKind::Prepare, FrameKind::ExecutePrepared, FrameKind::QueryCancel,
                        FrameKind::ExecutePrepared, FrameKind::ReleasePrepared,
                        FrameKind::ReleasePrepared,
                    ]
                );
                let first = ExecutePrepared::decode(frames[4].payload()).unwrap();
                let second = ExecutePrepared::decode(frames[6].payload()).unwrap();
                assert_eq!(first.parameters, [("value".into(), WireValue::Int(1))]);
                assert_eq!(second.parameters, [("value".into(), WireValue::Int(20))]);
                assert_eq!(frames[5].header().stream_id(), StreamId([5; 16]));
            }
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
                &ResultEnd { outcome: Outcome::Rows { seq: 3 }, rows: 1 },
            );
            let wrong_request = result_end(6, 1);
            let malformed = Frame::new(
                FrameKind::Error, 5, StreamId([5; 16]),
                Binding::Ready(ready().binding(session())), vec![0], limits,
            ).unwrap();
            let stale = Frame::new(
                FrameKind::SnapshotResultEnd, 5, StreamId([5; 16]),
                Binding::Ready(stale),
                ResultEnd { outcome: Outcome::Rows { seq: 3 }, rows: 1 }.encode().unwrap(),
                limits,
            ).unwrap();
            for terminal in [foreign, wrong_request, malformed, stale] {
                let io = script(vec![
                    rpc(FrameKind::Prepared, 4, StreamId::CONTROL, &Prepared { handle }),
                    result_chunk(5, &[1]),
                    terminal,
                ]);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "test").await.unwrap();
                client.prepare_read(&root, "MATCH (n) RETURN n", vec![]).await.unwrap();
                let error = client
                    .execute_prepared_streaming(
                        &root, handle, vec![], |_| {},
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
