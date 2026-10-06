//! One FGP connection: the handshake, statement dispatch, result streaming
//! under flow credit, and drain.
//!
//! The connection is strictly sequential: one admitted statement at a time
//! (Appendix D's v1 serialized profile). While a result stream waits for
//! credit, the connection keeps reading frames, so WINDOW_UPDATE,
//! QUERY_CANCEL, PING and DRAIN are never starved by the data they control.

use crate::convert;
use crate::shutdown::Waiter;
use crate::{Served, Server, TRUNK, unix_millis};
use asupersync::Cx;
use asupersync::net::TcpStream;
use asupersync::net::tcp::split::{OwnedReadHalf, OwnedWriteHalf};
use core::future::poll_fn;
use core::task::Poll;
use fgdb::{QueryError, QueryResult};
use fgdb_gql::GqlQueryError;
use fgdb_protocol::body::{
    Auth, AuthOk, Body, Credential, Empty, ErrorBody, ErrorCode, Execute, ExecuteMode, Hello,
    HelloAck, Outcome, Ping, Ready, ResultChunk, ResultEnd, SelectDatabase, WindowUpdate,
    WireValue,
};
use fgdb_protocol::transport::{FrameReader, FrameWriter};
use fgdb_protocol::{
    Binding, ChildKind, ChildTerminus, Connection, CreditUpdate, FlowWindow, Frame, FrameKind,
    FrameLimits, Header, MAX_HEADER_LEN, Posture, ProtocolError, SendCost, SendTerminus,
    SessionBinding, StreamId,
};
use fgdb_types::{EmbeddedTxnCompletion, PurposeContexts};
use fgdb_warden::CapabilityToken;
use std::collections::VecDeque;
use std::sync::Arc;

/// Finished child streams whose late WINDOW_UPDATE/QUERY_CANCEL is benign: a
/// client may grant credit for a stream the server has just ended.
const RECENTLY_FINISHED: usize = 64;
/// Bytes a chunk body spends besides its rows: the column-presence byte and
/// the row count (the column list is measured exactly).
const CHUNK_OVERHEAD: usize = 1 + 4;

enum Inbound {
    Frame(Box<Frame>),
    Closed,
    Shutdown,
}

/// Why a result stream stopped before its END.
enum Stop {
    /// The transport failed; nothing more can be sent.
    Transport,
    /// The client cancelled; an ERROR has been sent on the stream.
    Cancelled,
    /// The client asked to drain, or the server is shutting down.
    Drain,
}

struct Lane {
    reader: FrameReader<OwnedReadHalf>,
    writer: FrameWriter<OwnedWriteHalf>,
    conn: Connection,
    /// Never larger than the server's limit or the client's HELLO limit.
    send_limits: FrameLimits,
    /// Initial and maximum (bytes, rows) credit of every result stream.
    initial_window: SendCost,
    maximum_window: SendCost,
    finished: VecDeque<StreamId>,
}

/// A refusal of one statement, reported on its child stream.
struct Refusal {
    code: ErrorCode,
    message: String,
}

impl Refusal {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// A complete, already-decided statement answer.
struct Answer {
    columns: Vec<String>,
    rows: Vec<Vec<WireValue>>,
    outcome: Outcome,
}

pub(crate) async fn run(cx: &Cx, server: &Server, stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let limits = FrameLimits::new(server.limits.max_frame_len as usize)
        .expect("server limits were validated at construction");
    let (read, write) = stream.into_split();
    let Ok(conn) = Connection::new(1, 64) else {
        return;
    };
    let mut lane = Lane {
        reader: FrameReader::new(read, limits),
        writer: FrameWriter::new(write, limits),
        conn,
        send_limits: limits,
        initial_window: SendCost {
            bytes: server.limits.initial_window_bytes,
            rows: server.limits.initial_window_rows,
        },
        maximum_window: SendCost {
            bytes: server.limits.max_window_bytes,
            rows: server.limits.max_window_rows,
        },
        finished: VecDeque::new(),
    };
    let waiter = server.shutdown.waiter();
    let mut transcript: Option<[u8; 32]> = None;
    let mut token: Option<CapabilityToken> = None;
    let mut selected: Option<Arc<Served>> = None;
    loop {
        let frame = match lane.receive(cx, &waiter).await {
            Inbound::Frame(frame) => frame,
            Inbound::Closed => return,
            Inbound::Shutdown => {
                lane.goodbye(cx).await;
                return;
            }
        };
        let header = *frame.header();
        let request = header.request_id();
        match header.kind() {
            FrameKind::Hello => {
                let Ok(hello) = Hello::decode(frame.payload()) else {
                    return lane
                        .fatal(cx, request, ErrorCode::Protocol, "malformed HELLO")
                        .await;
                };
                if hello.min_version > crate::SERVED_VERSION
                    || hello.max_version < crate::SERVED_VERSION
                {
                    return lane
                        .fatal(
                            cx,
                            request,
                            ErrorCode::UnsupportedVersion,
                            "no common protocol version",
                        )
                        .await;
                }
                let peer = hello.max_frame_len as usize;
                if peer < MAX_HEADER_LEN + 1024 {
                    return lane
                        .fatal(
                            cx,
                            request,
                            ErrorCode::Protocol,
                            "HELLO frame limit too small",
                        )
                        .await;
                }
                let Ok(send_limits) =
                    FrameLimits::new(peer.min(server.limits.max_frame_len as usize))
                else {
                    return lane
                        .fatal(
                            cx,
                            request,
                            ErrorCode::Protocol,
                            "invalid HELLO frame limit",
                        )
                        .await;
                };
                lane.send_limits = send_limits;
                let mut server_nonce = [0u8; 32];
                cx.random_bytes(&mut server_nonce);
                let ack = HelloAck {
                    version: crate::SERVED_VERSION,
                    server_nonce,
                    max_frame_len: server.limits.max_frame_len,
                    initial_window_bytes: server.limits.initial_window_bytes,
                    initial_window_rows: server.limits.initial_window_rows,
                };
                let Ok(ack_bytes) = ack.encode() else { return };
                // Freeze the HELLO/HELLO_ACK transcript before the transition.
                let mut input = b"fgdb-server:hello:v1".to_vec();
                input.extend_from_slice(&(frame.payload().len() as u64).to_be_bytes());
                input.extend_from_slice(frame.payload());
                input.extend_from_slice(&ack_bytes);
                transcript = Some(fgdb_crypto::hash(&input).0);
                if lane.conn.negotiated().is_err() {
                    return;
                }
                if !lane
                    .send_bytes(
                        cx,
                        FrameKind::HelloAck,
                        request,
                        StreamId::CONTROL,
                        Binding::Transport,
                        ack_bytes,
                    )
                    .await
                {
                    return;
                }
            }
            FrameKind::Auth => {
                let Ok(auth) = Auth::decode(frame.payload()) else {
                    return lane
                        .fatal(cx, request, ErrorCode::Protocol, "malformed AUTH")
                        .await;
                };
                let Credential::WardenCapability(bytes) = auth.credential;
                // One uniform failure: a malformed token, a bad signature and
                // a token no served database accepts look the same.
                let admitted = CapabilityToken::decode(&bytes)
                    .ok()
                    .filter(|token| server.databases.values().any(|db| db.admits(token)));
                let (Some(admitted), Some(hello)) = (admitted, transcript) else {
                    return lane
                        .fatal(
                            cx,
                            request,
                            ErrorCode::Unauthenticated,
                            "credential not accepted",
                        )
                        .await;
                };
                let mut input = b"fgdb-server:session:v1".to_vec();
                input.extend_from_slice(&hello);
                input.extend_from_slice(&fgdb_crypto::hash(frame.payload()).0);
                let session = SessionBinding {
                    transcript: fgdb_crypto::keyed_hash(&server.secret, &input).0,
                    auth_generation: 1,
                };
                // The binding is derived before AUTH_OK is encoded.
                if lane.conn.authenticated(session).is_err() {
                    return;
                }
                token = Some(admitted);
                if !lane
                    .send(
                        cx,
                        FrameKind::AuthOk,
                        request,
                        StreamId::CONTROL,
                        Binding::Transport,
                        &AuthOk { session },
                    )
                    .await
                {
                    return;
                }
            }
            FrameKind::SelectDatabase => {
                let Ok(select) = SelectDatabase::decode(frame.payload()) else {
                    return lane
                        .fatal(
                            cx,
                            request,
                            ErrorCode::Protocol,
                            "malformed SELECT_DATABASE",
                        )
                        .await;
                };
                let (Some(token), Some(session)) = (token.as_ref(), lane.conn.binding().session())
                else {
                    return;
                };
                let binding = Binding::Session(session);
                let chosen = server
                    .databases
                    .get(&select.name)
                    .filter(|db| db.admits(token))
                    .cloned();
                let Some(chosen) = chosen else {
                    // Nonexistent and unauthorized share this exact reply.
                    let refusal = ErrorBody {
                        code: ErrorCode::NotFoundOrUnauthorized,
                        message: "database not found or not authorized".into(),
                    };
                    if !lane
                        .send(
                            cx,
                            FrameKind::Error,
                            request,
                            StreamId::CONTROL,
                            binding,
                            &refusal,
                        )
                        .await
                    {
                        return;
                    }
                    continue;
                };
                let frontier = match chosen.db.read(cx).await {
                    Ok(db) => db.frontier().map(|seq| seq.0),
                    Err(_) => return,
                };
                let Ok(frontier) = frontier else {
                    return lane
                        .fatal(cx, request, ErrorCode::Execution, "database unavailable")
                        .await;
                };
                let ready = Ready {
                    namespace: chosen.namespace,
                    incarnation: chosen.incarnation,
                    service_epoch: 1,
                    posture: Posture::Local,
                    authority_commitment: chosen.authority_commitment,
                    frontier,
                };
                if lane.conn.selected(ready.binding(session)).is_err() {
                    return;
                }
                selected = Some(chosen);
                if !lane
                    .send(
                        cx,
                        FrameKind::Ready,
                        request,
                        StreamId::CONTROL,
                        binding,
                        &ready,
                    )
                    .await
                {
                    return;
                }
            }
            FrameKind::Execute => {
                let (Some(db), Some(token)) = (selected.as_ref(), token.as_ref()) else {
                    return;
                };
                let Ok(statement) = Execute::decode(frame.payload()) else {
                    return lane
                        .fatal(cx, request, ErrorCode::Protocol, "malformed EXECUTE")
                        .await;
                };
                match lane
                    .execute(cx, &waiter, db, token, request, statement)
                    .await
                {
                    Ok(()) => {}
                    Err(Stop::Cancelled) => {}
                    Err(Stop::Transport) => return,
                    Err(Stop::Drain) => {
                        lane.goodbye(cx).await;
                        return;
                    }
                }
            }
            FrameKind::Ping => {
                let Ok(ping) = Ping::decode(frame.payload()) else {
                    return lane
                        .fatal(cx, request, ErrorCode::Protocol, "malformed PING")
                        .await;
                };
                let binding = lane.conn.binding();
                if !lane
                    .send(
                        cx,
                        FrameKind::Pong,
                        request,
                        StreamId::CONTROL,
                        binding,
                        &ping,
                    )
                    .await
                {
                    return;
                }
            }
            FrameKind::Drain => {
                lane.goodbye(cx).await;
                return;
            }
            // Only a recently finished stream can be addressed here (header
            // validation refuses any other): its credit or cancel is moot.
            FrameKind::WindowUpdate | FrameKind::QueryCancel => {}
            FrameKind::Prepare
            | FrameKind::AuthRefresh
            | FrameKind::ResultAck
            | FrameKind::ResultRelease => {
                let refusal = ErrorBody {
                    code: ErrorCode::Protocol,
                    message: "this server serves only autocommit EXECUTE with ephemeral results"
                        .into(),
                };
                let binding = lane.conn.binding();
                if !lane
                    .send(
                        cx,
                        FrameKind::Error,
                        request,
                        StreamId::CONTROL,
                        binding,
                        &refusal,
                    )
                    .await
                {
                    return;
                }
            }
            // Header validation admits no server-to-client kind.
            _ => return,
        }
    }
}

impl Lane {
    async fn receive(&mut self, cx: &Cx, waiter: &Waiter) -> Inbound {
        let Self {
            reader,
            conn,
            finished,
            ..
        } = self;
        poll_fn(|task| {
            if waiter.poll_triggered(task) {
                return Poll::Ready(Inbound::Shutdown);
            }
            reader
                .poll_receive(cx, task, |header| validate(conn, finished, header))
                .map(|received| match received {
                    Ok(Some(frame)) => Inbound::Frame(Box::new(frame)),
                    Ok(None) | Err(_) => Inbound::Closed,
                })
        })
        .await
    }

    /// Queue, guard and write one frame. Every write attempt rechecks that
    /// the frame still carries the connection's current binding, so a frame
    /// built under a stale binding can never reach the wire.
    async fn send_frame(&mut self, cx: &Cx, frame: &Frame) -> bool {
        let Ok(ticket) = self.conn.queue_send() else {
            return false;
        };
        if self.writer.queue(cx, frame).is_err() {
            let _ = self
                .conn
                .send_terminal(&ticket, SendTerminus::CancelledBeforeWrite);
            return false;
        }
        let current = self.conn.binding();
        let result = self.writer.send(cx, |header| guard(header, current)).await;
        let terminus = if result.is_ok() {
            SendTerminus::Sent
        } else {
            SendTerminus::Failed
        };
        let _ = self.conn.send_terminal(&ticket, terminus);
        result.is_ok()
    }

    async fn send_bytes(
        &mut self,
        cx: &Cx,
        kind: FrameKind,
        request: u64,
        stream: StreamId,
        binding: Binding,
        payload: Vec<u8>,
    ) -> bool {
        match Frame::new(kind, request, stream, binding, payload, self.send_limits) {
            Ok(frame) => self.send_frame(cx, &frame).await,
            Err(_) => false,
        }
    }

    async fn send(
        &mut self,
        cx: &Cx,
        kind: FrameKind,
        request: u64,
        stream: StreamId,
        binding: Binding,
        body: &impl Body,
    ) -> bool {
        match body.encode() {
            Ok(payload) => {
                self.send_bytes(cx, kind, request, stream, binding, payload)
                    .await
            }
            Err(_) => false,
        }
    }

    /// Report a connection-fatal refusal on the control stream, then close.
    async fn fatal(&mut self, cx: &Cx, request: u64, code: ErrorCode, message: &str) {
        let body = ErrorBody {
            code,
            message: message.into(),
        };
        let binding = self.conn.binding();
        let _ = self
            .send(
                cx,
                FrameKind::Error,
                request,
                StreamId::CONTROL,
                binding,
                &body,
            )
            .await;
    }

    /// Drain: no child is in flight between statements, so the connection
    /// can close at once. Closing first is what authorizes GOODBYE; the frame
    /// is then the connection's last write and owes no tracked obligation.
    async fn goodbye(&mut self, cx: &Cx) {
        let binding = self.conn.binding();
        if self.conn.begin_drain().is_err() || self.conn.complete_drain().is_err() {
            return;
        }
        let Ok(payload) = Empty.encode() else { return };
        let Ok(frame) = Frame::new(
            FrameKind::Goodbye,
            0,
            StreamId::CONTROL,
            binding,
            payload,
            self.send_limits,
        ) else {
            return;
        };
        if self.writer.queue(cx, &frame).is_ok() {
            let _ = self.writer.send(cx, |header| guard(header, binding)).await;
        }
    }

    async fn execute(
        &mut self,
        cx: &Cx,
        waiter: &Waiter,
        db: &Served,
        token: &CapabilityToken,
        request: u64,
        statement: Execute,
    ) -> Result<(), Stop> {
        let binding = self.conn.binding();
        let stream = loop {
            let mut id = [0u8; 16];
            cx.random_bytes(&mut id);
            let candidate = StreamId(id);
            if !candidate.is_control() && !self.finished.contains(&candidate) {
                break candidate;
            }
        };
        let Ok(generation) = self.conn.admit_child(stream, ChildKind::Query) else {
            let busy = ErrorBody {
                code: ErrorCode::Busy,
                message: "a statement is already in flight on this connection".into(),
            };
            return if self
                .send(
                    cx,
                    FrameKind::Error,
                    request,
                    StreamId::CONTROL,
                    binding,
                    &busy,
                )
                .await
            {
                Ok(())
            } else {
                Err(Stop::Transport)
            };
        };
        let mode = statement.mode;
        let answer = match mode {
            ExecuteMode::Read => read(cx, db, token, &statement).await,
            ExecuteMode::Write => write(cx, db, token, &statement).await,
        };
        let committed = matches!(
            answer,
            Ok(Answer {
                outcome: Outcome::WriteCommitted { .. },
                ..
            })
        );
        let delivered = match answer {
            Ok(answer) => {
                self.stream(cx, waiter, request, stream, binding, answer)
                    .await
            }
            Err(refusal) => {
                let body = ErrorBody {
                    code: refusal.code,
                    message: refusal.message,
                };
                if self
                    .send(cx, FrameKind::Error, request, stream, binding, &body)
                    .await
                {
                    Ok(())
                } else {
                    Err(Stop::Transport)
                }
            }
        };
        // A committed write's semantic terminal is durable whether or not its
        // END reached the client; an ephemeral read simply ends.
        let terminus = if committed {
            ChildTerminus::SemanticTerminalDurable
        } else {
            ChildTerminus::EphemeralCompleted
        };
        let _ = self.conn.child_terminal(stream, generation, terminus);
        if self.finished.len() == RECENTLY_FINISHED {
            self.finished.pop_front();
        }
        self.finished.push_back(stream);
        delivered
    }

    /// Send the answer as chunks under the stream's flow credit, then END.
    /// Every chunk is sized to the credit available when it is assembled
    /// (rows and bytes, header included), so the server never waits on a
    /// grant the client could only send after receiving that very chunk.
    async fn stream(
        &mut self,
        cx: &Cx,
        waiter: &Waiter,
        request: u64,
        stream: StreamId,
        binding: Binding,
        answer: Answer,
    ) -> Result<(), Stop> {
        let mut window = FlowWindow::new(self.initial_window, self.maximum_window)
            .map_err(|_| Stop::Transport)?;
        let framing = binding.header_len() + CHUNK_OVERHEAD;
        let frame_budget = self.send_limits.max_frame_len() - framing;
        let mut columns = Some(answer.columns);
        let mut rows = answer.rows.into_iter().peekable();
        let mut delivered = 0u64;
        while columns.is_some() || rows.peek().is_some() {
            let available = window.available();
            let byte_room = usize::try_from(available.bytes)
                .unwrap_or(usize::MAX)
                .saturating_sub(framing)
                .min(frame_budget);
            let row_room = usize::try_from(available.rows).unwrap_or(usize::MAX);
            let mut size = columns.as_ref().map_or(0, |names| {
                4 + names.iter().map(|name| 4 + name.len()).sum::<usize>()
            });
            if size > frame_budget {
                return self
                    .refuse(
                        cx,
                        request,
                        stream,
                        binding,
                        ErrorCode::Execution,
                        "the column list exceeds the negotiated frame limit",
                    )
                    .await;
            }
            let mut chunk_rows = Vec::new();
            if size <= byte_room {
                while chunk_rows.len() < row_room {
                    let Some(row) = rows.peek() else { break };
                    let Ok(len) = ResultChunk::row_len(row) else {
                        return self
                            .refuse(
                                cx,
                                request,
                                stream,
                                binding,
                                ErrorCode::Execution,
                                "a result value exceeds the wire bounds",
                            )
                            .await;
                    };
                    if size + len > frame_budget && chunk_rows.is_empty() && columns.is_none() {
                        return self
                            .refuse(
                                cx,
                                request,
                                stream,
                                binding,
                                ErrorCode::Execution,
                                "a result row exceeds the negotiated frame limit",
                            )
                            .await;
                    }
                    if size + len > byte_room {
                        break;
                    }
                    size += len;
                    chunk_rows.extend(rows.next());
                }
            }
            // Nothing fits the current credit: wait for a grant and retry.
            if size > byte_room || (chunk_rows.is_empty() && columns.is_none()) {
                self.await_credit(cx, waiter, &mut window, stream, request, binding)
                    .await?;
                continue;
            }
            let count = chunk_rows.len() as u64;
            let chunk = ResultChunk {
                columns: columns.take(),
                rows: chunk_rows,
            };
            let Ok(payload) = chunk.encode() else {
                return self
                    .refuse(
                        cx,
                        request,
                        stream,
                        binding,
                        ErrorCode::Execution,
                        "a result value exceeds the wire bounds",
                    )
                    .await;
            };
            let frame = Frame::new(
                FrameKind::SnapshotResultChunk,
                request,
                stream,
                binding,
                payload,
                self.send_limits,
            )
            .map_err(|_| Stop::Transport)?;
            self.credited(cx, waiter, &mut window, stream, &frame, count)
                .await?;
            delivered += count;
        }
        let end = ResultEnd {
            outcome: answer.outcome,
            rows: delivered,
        };
        let payload = end.encode().map_err(|_| Stop::Transport)?;
        let frame = Frame::new(
            FrameKind::SnapshotResultEnd,
            request,
            stream,
            binding,
            payload,
            self.send_limits,
        )
        .map_err(|_| Stop::Transport)?;
        self.credited(cx, waiter, &mut window, stream, &frame, 0)
            .await
    }

    async fn refuse(
        &mut self,
        cx: &Cx,
        request: u64,
        stream: StreamId,
        binding: Binding,
        code: ErrorCode,
        message: &str,
    ) -> Result<(), Stop> {
        let body = ErrorBody {
            code,
            message: message.into(),
        };
        if self
            .send(cx, FrameKind::Error, request, stream, binding, &body)
            .await
        {
            Ok(())
        } else {
            Err(Stop::Transport)
        }
    }

    /// Reserve credit for `frame` (waiting for WINDOW_UPDATE while the window
    /// is short), then write it.
    async fn credited(
        &mut self,
        cx: &Cx,
        waiter: &Waiter,
        window: &mut FlowWindow,
        stream: StreamId,
        frame: &Frame,
        rows: u64,
    ) -> Result<(), Stop> {
        let cost = SendCost {
            bytes: frame.header().frame_len() as u64,
            rows,
        };
        loop {
            let available = window.available();
            if cost.bytes > available.bytes || cost.rows > available.rows {
                let header = frame.header();
                self.await_credit(
                    cx,
                    waiter,
                    window,
                    stream,
                    header.request_id(),
                    header.binding(),
                )
                .await?;
                continue;
            }
            let mut reservation = window.reserve(cost).map_err(|_| Stop::Transport)?;
            reservation.begin_write().map_err(|_| Stop::Transport)?;
            return if self.send_frame(cx, frame).await {
                reservation.sent().map_err(|_| Stop::Transport)
            } else {
                let _ = reservation.failed();
                Err(Stop::Transport)
            };
        }
    }

    /// Process exactly one inbound frame while a stream waits for credit.
    /// Reading continues while waiting, so cancel, ping and drain still get
    /// through, and a second EXECUTE is refused as busy rather than queued.
    async fn await_credit(
        &mut self,
        cx: &Cx,
        waiter: &Waiter,
        window: &mut FlowWindow,
        stream: StreamId,
        request: u64,
        binding: Binding,
    ) -> Result<(), Stop> {
        let inbound = match self.receive(cx, waiter).await {
            Inbound::Frame(inbound) => inbound,
            Inbound::Closed => return Err(Stop::Transport),
            Inbound::Shutdown => return Err(Stop::Drain),
        };
        let header = *inbound.header();
        match header.kind() {
            FrameKind::WindowUpdate if header.stream_id() == stream => {
                let Ok(update) = WindowUpdate::decode(inbound.payload()) else {
                    return Err(Stop::Transport);
                };
                let grant = CreditUpdate {
                    sequence: update.sequence,
                    bytes: update.bytes,
                    rows: update.rows,
                };
                if window.grant(grant).is_err() {
                    let _ = self
                        .refuse(
                            cx,
                            request,
                            stream,
                            binding,
                            ErrorCode::Protocol,
                            "invalid flow-credit update",
                        )
                        .await;
                    return Err(Stop::Transport);
                }
                Ok(())
            }
            FrameKind::QueryCancel if header.stream_id() == stream => {
                self.refuse(
                    cx,
                    request,
                    stream,
                    binding,
                    ErrorCode::Cancelled,
                    "cancelled by the client",
                )
                .await?;
                Err(Stop::Cancelled)
            }
            FrameKind::Ping => {
                let Ok(ping) = Ping::decode(inbound.payload()) else {
                    return Err(Stop::Transport);
                };
                let current = self.conn.binding();
                if self
                    .send(
                        cx,
                        FrameKind::Pong,
                        header.request_id(),
                        StreamId::CONTROL,
                        current,
                        &ping,
                    )
                    .await
                {
                    Ok(())
                } else {
                    Err(Stop::Transport)
                }
            }
            FrameKind::Drain => {
                let _ = self
                    .refuse(
                        cx,
                        request,
                        stream,
                        binding,
                        ErrorCode::Draining,
                        "the connection is draining",
                    )
                    .await;
                Err(Stop::Drain)
            }
            FrameKind::Execute => {
                let busy = ErrorBody {
                    code: ErrorCode::Busy,
                    message: "a statement is already in flight on this connection".into(),
                };
                let current = self.conn.binding();
                if self
                    .send(
                        cx,
                        FrameKind::Error,
                        header.request_id(),
                        StreamId::CONTROL,
                        current,
                        &busy,
                    )
                    .await
                {
                    Ok(())
                } else {
                    Err(Stop::Transport)
                }
            }
            // Late credit or cancel for an earlier, finished stream.
            FrameKind::WindowUpdate | FrameKind::QueryCancel => Ok(()),
            _ => {
                let refusal = ErrorBody {
                    code: ErrorCode::Protocol,
                    message: "frame not legal while a result streams".into(),
                };
                let current = self.conn.binding();
                let _ = self
                    .send(
                        cx,
                        FrameKind::Error,
                        header.request_id(),
                        StreamId::CONTROL,
                        current,
                        &refusal,
                    )
                    .await;
                Err(Stop::Transport)
            }
        }
    }
}

/// The receive-side header check: the protocol crate's validation, widened
/// only for credit/cancel that races the END of a just-finished stream.
fn validate(
    conn: &Connection,
    finished: &VecDeque<StreamId>,
    header: &Header,
) -> Result<(), ProtocolError> {
    match conn.validate_client_header(header) {
        Err(ProtocolError::InvalidStream)
            if matches!(
                header.kind(),
                FrameKind::WindowUpdate | FrameKind::QueryCancel
            ) && header.binding() == conn.binding()
                && finished.contains(&header.stream_id()) =>
        {
            Ok(())
        }
        other => other,
    }
}

/// The send guard: a frame goes out only under the binding the connection
/// holds at the moment of the write, except for the handshake replies whose
/// binding the client cannot know yet: HELLO_ACK and AUTH_OK travel on the
/// transport header, READY on the session header it completes.
fn guard(header: &Header, current: Binding) -> Result<(), ProtocolError> {
    let expected = match (header.kind(), current) {
        (FrameKind::HelloAck | FrameKind::AuthOk, _) => Binding::Transport,
        (FrameKind::Ready, Binding::Ready(ready)) => Binding::Session(ready.session),
        _ => current,
    };
    if header.binding() == expected {
        Ok(())
    } else {
        Err(ProtocolError::InvalidBinding)
    }
}

async fn read(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    statement: &Execute,
) -> Result<Answer, Refusal> {
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let query = contexts.query();
    let parameters = convert::parameters(&statement.parameters, None)
        .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))?;
    let (session, frontier) = {
        let guard = db
            .db
            .read(cx)
            .await
            .map_err(|_| Refusal::new(ErrorCode::Execution, "database unavailable"))?;
        // Under the read lock no write can land between these two reads, so
        // the session's pinned generation is exactly this frontier.
        let frontier = guard
            .frontier()
            .map_err(|error| Refusal::new(ErrorCode::Execution, error.to_string()))?;
        let session = guard.authorized_read_session(
            &query,
            &db.authority,
            token,
            TRUNK,
            db.symbols.clone(),
            db.query_policy,
            unix_millis,
        );
        (session, frontier)
    };
    let mut session = session.map_err(query_refusal)?;
    match session.query(&query, &statement.statement, &parameters) {
        Ok(QueryResult::Rows { columns, rows }) => Ok(Answer {
            columns,
            rows: rows
                .iter()
                .map(|row| row.iter().map(convert::cell).collect())
                .collect(),
            outcome: Outcome::Rows { seq: frontier.0 },
        }),
        Ok(QueryResult::Write { .. }) => Err(Refusal::new(
            ErrorCode::Statement,
            "a write statement cannot execute as a read",
        )),
        Err(error) => Err(query_refusal(error)),
    }
}

async fn write(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    statement: &Execute,
) -> Result<Answer, Refusal> {
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let (txn, commit, query) = (contexts.txn(), contexts.commit(), contexts.query());
    let parameters = convert::parameters(&statement.parameters, None)
        .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))?;
    let mut guard = db
        .db
        .write(cx)
        .await
        .map_err(|_| Refusal::new(ErrorCode::Execution, "database unavailable"))?;
    let symbols = &db.symbols;
    let mut session = guard
        .authorized_write_session(
            &txn,
            &commit,
            &db.authority,
            token,
            TRUNK,
            |kind, name| symbols.resolve(kind, name),
            db.write_relation,
            db.write_policy,
            db.max_statements,
            unix_millis,
        )
        .map_err(|error| write_refusal(&error))?;
    let (receipt, completion) = session
        .query(&query, &statement.statement, &parameters)
        .await
        .map_err(|error| write_refusal(&error))?;
    let statements = receipt.stats().completed_statements as u64;
    let outcome = match completion {
        EmbeddedTxnCompletion::WriteCommitted { commit_seq } => Outcome::WriteCommitted {
            seq: commit_seq.0,
            statements,
        },
        EmbeddedTxnCompletion::ReadClosed { snapshot_seq, .. } => Outcome::ReadClosed {
            seq: snapshot_seq.0,
            statements,
        },
    };
    Ok(Answer {
        columns: Vec::new(),
        rows: Vec::new(),
        outcome,
    })
}

fn warden_code(error: fgdb_warden::Error) -> ErrorCode {
    match error {
        fgdb_warden::Error::LimitExceeded(_) => ErrorCode::Budget,
        _ => ErrorCode::PermissionDenied,
    }
}

fn gql_budget<E, C>(error: &GqlQueryError<E, C>) -> bool {
    matches!(error, GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_))
}

fn query_refusal(error: QueryError) -> Refusal {
    let code = match &error {
        QueryError::Authorization(error) => warden_code(*error),
        QueryError::Read(_) => ErrorCode::Execution,
        QueryError::Pattern(e) if gql_budget(e) => ErrorCode::Budget,
        QueryError::Aggregate(e) if gql_budget(e) => ErrorCode::Budget,
        QueryError::Set(e) if gql_budget(e) => ErrorCode::Budget,
        QueryError::Pattern(_) | QueryError::Aggregate(_) | QueryError::Set(_) => {
            ErrorCode::Execution
        }
        _ => ErrorCode::Statement,
    };
    Refusal::new(code, error.to_string())
}

/// Classify a write failure by walking its typed cause chain. Nothing here
/// turns an unknown commit outcome into a refusal: that class is preserved.
fn write_refusal(error: &(dyn core::error::Error + 'static)) -> Refusal {
    let mut code = ErrorCode::Statement;
    let mut source = Some(error);
    while let Some(current) = source {
        if let Some(warden) = current.downcast_ref::<fgdb_warden::Error>() {
            code = warden_code(*warden);
            break;
        }
        if let Some(write) = current.downcast_ref::<fgdb::WriteError>() {
            code = match write {
                fgdb::WriteError::FirstCommitterWins { .. } => ErrorCode::Conflict,
                fgdb::WriteError::CommitOutcomeUnknown { .. }
                | fgdb::WriteError::RecoveryRequired(_) => ErrorCode::OutcomeUnknown,
                _ => ErrorCode::Execution,
            };
            break;
        }
        if let Some(txn) = current.downcast_ref::<fgdb::WriteTxnError>() {
            match txn {
                fgdb::WriteTxnError::Authorization(warden) => {
                    code = warden_code(*warden);
                    break;
                }
                fgdb::WriteTxnError::AuthorizedMutationRefused => {
                    code = ErrorCode::PermissionDenied;
                    break;
                }
                _ => code = ErrorCode::Execution,
            }
        }
        source = current.source();
    }
    Refusal::new(code, error.to_string())
}
