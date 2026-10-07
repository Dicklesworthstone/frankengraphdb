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
    Auth, AuthOk, Body, BodyError, Credential, Empty, ErrorBody, ErrorCode, Execute, ExecuteMode,
    Hello, HelloAck, Outcome, Ping, Ready, ResultChunk, ResultEnd, SelectDatabase,
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

    /// Run one statement, handing over its columns once and then each row as
    /// it arrives. Returning an error from `on_row` cancels the stream.
    pub async fn execute_streaming(
        &mut self,
        cx: &Cx,
        mode: ExecuteMode,
        statement: &str,
        mut parameters: Vec<(String, WireValue)>,
        mut on_columns: impl FnMut(&[String]),
        mut on_row: impl FnMut(Vec<WireValue>) -> Result<(), ClientError>,
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
        let mut stream: Option<StreamId> = None;
        // The server sends only within its credit, so the client models that
        // credit exactly and replenishes it whenever the server might be
        // unable to fit its next frame (one maximal frame, or one row).
        let (initial_bytes, initial_rows) = self.initial_window;
        let mut available = (initial_bytes, initial_rows);
        let mut grants = 0u64;
        let mut seen_columns = false;
        loop {
            let frame = self.reply(cx, request).await?;
            let header = *frame.header();
            match stream {
                None if !header.stream_id().is_control() => stream = Some(header.stream_id()),
                Some(id) if id != header.stream_id() => {
                    return Err(ClientError::Protocol("result frame on a foreign stream"));
                }
                _ => {}
            }
            match header.kind() {
                FrameKind::SnapshotResultChunk => {
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
                        if let Err(error) = on_row(row) {
                            if let Some(id) = stream {
                                let _ = self.send(cx, FrameKind::QueryCancel, id, &Empty).await;
                            }
                            return Err(error);
                        }
                    }
                    let frame_limit = self.send_limits.max_frame_len() as u64;
                    if available.0 < frame_limit || available.1 == 0 {
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
                    return Ok(ResultEnd::decode(frame.payload())?.outcome);
                }
                FrameKind::Error => return Err(server_error(&frame)),
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
                        if !cancelled && !on_change(change)? {
                            cancelled = true;
                            let Some(id) = stream else {
                                return Err(ClientError::Protocol("batch on the control stream"));
                            };
                            self.send(cx, FrameKind::QueryCancel, id, &Empty).await?;
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
                    return Ok(seq);
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
                        return Ok(0);
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

/// Server frames: only server-to-client kinds, under the expected binding,
/// except the handshake replies that complete a binding.
fn server_header(header: &Header, expected: Binding) -> Result<(), ProtocolError> {
    let binding_ok = match header.kind() {
        FrameKind::HelloAck | FrameKind::AuthOk => header.binding() == Binding::Transport,
        FrameKind::Ready => matches!(expected, Binding::Session(_)) && header.binding() == expected,
        FrameKind::Error => header.binding() == expected,
        FrameKind::Pong
        | FrameKind::Goodbye
        | FrameKind::SnapshotResultChunk
        | FrameKind::SnapshotResultEnd
        | FrameKind::SubscriptionBatch => header.binding() == expected,
        _ => return Err(ProtocolError::InvalidState),
    };
    if binding_ok {
        Ok(())
    } else {
        Err(ProtocolError::InvalidBinding)
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
