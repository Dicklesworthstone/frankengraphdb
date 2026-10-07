//! One FGP connection: the handshake, statement dispatch, result streaming
//! under flow credit, and drain.
//!
//! The connection is strictly sequential: one admitted statement at a time
//! (Appendix D's v1 serialized profile). While a result stream waits for
//! credit, the connection keeps reading frames, so WINDOW_UPDATE,
//! QUERY_CANCEL, PING and DRAIN are never starved by the data they control.

use crate::commits::CommitWatcher;
use crate::execute::{Answer, poll, read, subscribe, write};
use crate::shutdown::Waiter;
use crate::{Served, Server};
use asupersync::Cx;
use core::future::poll_fn;
use core::task::Poll;
use fgdb_protocol::body::{
    Auth, AuthOk, Body, Credential, Empty, ErrorBody, ErrorCode, Execute, ExecuteMode, Hello,
    HelloAck, Outcome, Ping, Ready, ResultChunk, ResultEnd, SelectDatabase, WindowUpdate,
    WireValue,
};
use fgdb_protocol::transport::{
    DuplexIo, DuplexReader, DuplexWriter, FrameReader, FrameWriter, split_duplex,
};
use fgdb_protocol::{
    Binding, ChildKind, ChildTerminus, Connection, CreditUpdate, FlowWindow, Frame, FrameKind,
    FrameLimits, Header, MAX_HEADER_LEN, Posture, ProtocolError, SendCost, SendTerminus,
    SessionBinding, StreamId,
};
use fgdb_warden::{Authority, CapabilityToken, VerifiedCapability};
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

/// What woke a caught-up subscription.
enum Idle {
    Commit,
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
    reader: FrameReader<DuplexReader<Box<dyn DuplexIo>>>,
    writer: FrameWriter<DuplexWriter<Box<dyn DuplexIo>>>,
    conn: Connection,
    /// Never larger than the server's limit or the client's HELLO limit.
    send_limits: FrameLimits,
    /// Initial and maximum (bytes, rows) credit of every result stream.
    initial_window: SendCost,
    maximum_window: SendCost,
    finished: VecDeque<StreamId>,
    /// The selected issuer and bearer credential remain live while output can
    /// wait for credit or socket readiness. A Ready binding is not authority.
    send_authority: Option<(Arc<Served>, CapabilityToken)>,
}

pub(crate) async fn run(cx: &Cx, server: &Server, stream: Box<dyn DuplexIo>) {
    let limits = FrameLimits::new(server.limits.max_frame_len as usize)
        .expect("server limits were validated at construction");
    let Ok((read, write)) = split_duplex(cx, stream) else {
        return;
    };
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
        send_authority: None,
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
                lane.send_authority = Some((Arc::clone(&chosen), token.clone()));
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

    /// Queue, guard and write one frame. The exact selected issuer verifies
    /// the bearer once here, then expiry and retirement are rechecked before
    /// every physical write/flush attempt, including a resumed partial write.
    /// An invalidated output closes delivery; it cannot undo a decided commit.
    async fn send_frame(&mut self, cx: &Cx, frame: &Frame) -> bool {
        let authority = self
            .send_authority
            .as_ref()
            .map(|(database, token)| (&database.authority, token));
        let Ok(authorization) =
            OutputGuard::new(self.conn.binding(), authority, crate::unix_millis())
        else {
            return false;
        };
        let Ok(ticket) = self.conn.queue_send() else {
            return false;
        };
        if self.writer.queue(cx, frame).is_err() {
            let _ = self
                .conn
                .send_terminal(&ticket, SendTerminus::CancelledBeforeWrite);
            return false;
        }
        let result = self
            .writer
            .send(cx, |header| {
                authorization.authorize(header, crate::unix_millis())
            })
            .await;
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
        let kind = if statement.mode == ExecuteMode::Subscribe {
            ChildKind::Subscription
        } else {
            ChildKind::Query
        };
        let Ok(generation) = self.conn.admit_child(stream, kind) else {
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
        if statement.mode == ExecuteMode::Subscribe {
            let delivered = self
                .subscription(cx, waiter, db, token, request, stream, binding, &statement)
                .await;
            let _ = self
                .conn
                .child_terminal(stream, generation, ChildTerminus::EphemeralCompleted);
            self.finish(stream);
            return delivered;
        }
        let answer = match statement.mode {
            ExecuteMode::Read => read(cx, db, token, &statement).await,
            _ => write(cx, db, token, &statement).await,
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
        self.finish(stream);
        delivered
    }

    fn finish(&mut self, stream: StreamId) {
        if self.finished.len() == RECENTLY_FINISHED {
            self.finished.pop_front();
        }
        self.finished.push_back(stream);
    }

    /// Register a subscription and push its baseline, then one delta batch
    /// per change, until the client cancels (answered with END at the last
    /// delivered frontier), drains, or the capability lapses. Each batch is
    /// acknowledged to the engine only after its last frame is written, and
    /// the next delta always starts at the last acknowledged frontier, so a
    /// slow subscriber receives coalesced deltas rather than a backlog.
    #[allow(clippy::too_many_arguments)]
    async fn subscription(
        &mut self,
        cx: &Cx,
        waiter: &Waiter,
        db: &Served,
        token: &CapabilityToken,
        request: u64,
        stream: StreamId,
        binding: Binding,
        statement: &Execute,
    ) -> Result<(), Stop> {
        let mut subscription = match subscribe(cx, db, token, statement).await {
            Ok(subscription) => subscription,
            Err(refusal) => {
                return self
                    .refuse(cx, request, stream, binding, refusal.code, &refusal.message)
                    .await;
            }
        };
        let mut window = FlowWindow::new(self.initial_window, self.maximum_window)
            .map_err(|_| Stop::Transport)?;
        let mut columns = Some(core::mem::take(&mut subscription.columns));
        let mut watcher = db.commits.watcher();
        let mut delivered = 0u64;
        let result = loop {
            let batch = match poll(cx, db, token, &mut subscription).await {
                Ok(batch) => batch,
                Err(refusal) => {
                    break self
                        .refuse(cx, request, stream, binding, refusal.code, &refusal.message)
                        .await;
                }
            };
            if let Some(batch) = batch {
                let mut entries = Vec::with_capacity(batch.rows().len());
                for (row, weight) in batch.rows().iter() {
                    let Some(weight) = weight.to_i128() else {
                        break;
                    };
                    entries.push((weight, row.iter().map(crate::convert::cell).collect()));
                }
                if entries.len() != batch.rows().len() {
                    break self
                        .refuse(
                            cx,
                            request,
                            stream,
                            binding,
                            ErrorCode::Execution,
                            "a change weight exceeds the wire range",
                        )
                        .await;
                }
                let frontier = batch.frontier().0;
                if let Err(stop) = self
                    .send_batch(
                        cx,
                        waiter,
                        &mut window,
                        request,
                        stream,
                        binding,
                        frontier,
                        batch.is_snapshot(),
                        &mut columns,
                        entries,
                    )
                    .await
                {
                    break Err(stop);
                }
                if subscription.consumer.acknowledge(batch.receipt()).is_err() {
                    break self
                        .refuse(
                            cx,
                            request,
                            stream,
                            binding,
                            ErrorCode::Execution,
                            "subscription acknowledgement refused",
                        )
                        .await;
                }
                delivered = frontier;
                continue;
            }
            match self.idle(cx, waiter, &mut watcher).await {
                Idle::Commit => {}
                Idle::Closed => break Err(Stop::Transport),
                Idle::Shutdown => {
                    let _ = self
                        .end(cx, &mut window, request, stream, binding, delivered)
                        .await;
                    break Err(Stop::Drain);
                }
                Idle::Frame(frame) => {
                    let header = *frame.header();
                    match header.kind() {
                        FrameKind::QueryCancel if header.stream_id() == stream => {
                            break self
                                .end(cx, &mut window, request, stream, binding, delivered)
                                .await;
                        }
                        FrameKind::WindowUpdate if header.stream_id() == stream => {
                            let Ok(update) = WindowUpdate::decode(frame.payload()) else {
                                break Err(Stop::Transport);
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
                                break Err(Stop::Transport);
                            }
                        }
                        FrameKind::Drain => {
                            let _ = self
                                .end(cx, &mut window, request, stream, binding, delivered)
                                .await;
                            break Err(Stop::Drain);
                        }
                        _ => {
                            if let Err(stop) = self.interleaved(cx, &frame).await {
                                break Err(stop);
                            }
                        }
                    }
                }
            }
        };
        subscription.consumer.close();
        result
    }

    /// One subscription batch, split across frames sized to the stream's
    /// credit and the frame limit; only the final frame sets `last`.
    #[allow(clippy::too_many_arguments)]
    async fn send_batch(
        &mut self,
        cx: &Cx,
        waiter: &Waiter,
        window: &mut FlowWindow,
        request: u64,
        stream: StreamId,
        binding: Binding,
        frontier: u64,
        snapshot: bool,
        columns: &mut Option<Vec<String>>,
        entries: Vec<(i128, Vec<WireValue>)>,
    ) -> Result<(), Stop> {
        // frontier, snapshot, last, column presence and the entry count.
        let framing = binding.header_len() + 8 + 1 + 1 + 1 + 4;
        let frame_budget = self.send_limits.max_frame_len() - framing;
        let mut entries = entries.into_iter().peekable();
        loop {
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
                    .await
                    .and(Err(Stop::Cancelled));
            }
            let mut part = Vec::new();
            if size <= byte_room {
                while part.len() < row_room {
                    let Some((_, row)) = entries.peek() else {
                        break;
                    };
                    let Ok(len) = fgdb_protocol::body::SubscriptionBatch::entry_len(row) else {
                        return self
                            .refuse(
                                cx,
                                request,
                                stream,
                                binding,
                                ErrorCode::Execution,
                                "a change value exceeds the wire bounds",
                            )
                            .await
                            .and(Err(Stop::Cancelled));
                    };
                    if size + len > frame_budget && part.is_empty() {
                        return self
                            .refuse(
                                cx,
                                request,
                                stream,
                                binding,
                                ErrorCode::Execution,
                                "a change row exceeds the negotiated frame limit",
                            )
                            .await
                            .and(Err(Stop::Cancelled));
                    }
                    if size + len > byte_room {
                        break;
                    }
                    size += len;
                    part.extend(entries.next());
                }
            }
            let last = entries.peek().is_none();
            // A frame that carries nothing new (no columns, no entries, more to
            // come) waits for credit instead of spending it.
            if size > byte_room || (part.is_empty() && columns.is_none() && !last) {
                self.await_credit(cx, waiter, window, stream, request, binding)
                    .await?;
                continue;
            }
            let count = part.len() as u64;
            let body = fgdb_protocol::body::SubscriptionBatch {
                frontier,
                snapshot,
                last,
                columns: columns.take(),
                entries: part,
            };
            let payload = body.encode().map_err(|_| Stop::Transport)?;
            let frame = Frame::new(
                FrameKind::SubscriptionBatch,
                request,
                stream,
                binding,
                payload,
                self.send_limits,
            )
            .map_err(|_| Stop::Transport)?;
            self.credited(cx, waiter, window, stream, &frame, count)
                .await?;
            if last {
                return Ok(());
            }
        }
    }

    /// END a subscription at the last frontier it delivered.
    async fn end(
        &mut self,
        cx: &Cx,
        window: &mut FlowWindow,
        request: u64,
        stream: StreamId,
        binding: Binding,
        delivered: u64,
    ) -> Result<(), Stop> {
        let end = ResultEnd {
            outcome: Outcome::Rows { seq: delivered },
            rows: 0,
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
        // END is a control-sized frame; it is not withheld for credit here,
        // because a cancelled stream may have none left and must still end.
        let _ = window;
        if self.send_frame(cx, &frame).await {
            Ok(())
        } else {
            Err(Stop::Transport)
        }
    }

    /// Wait while a subscription is caught up: for a commit, a client frame,
    /// the connection closing, or the drain signal.
    async fn idle(&mut self, cx: &Cx, waiter: &Waiter, watcher: &mut CommitWatcher<'_>) -> Idle {
        let Self {
            reader,
            conn,
            finished,
            ..
        } = self;
        poll_fn(|task| {
            if waiter.poll_triggered(task) {
                return Poll::Ready(Idle::Shutdown);
            }
            if watcher.poll_changed(task) {
                return Poll::Ready(Idle::Commit);
            }
            reader
                .poll_receive(cx, task, |header| validate(conn, finished, header))
                .map(|received| match received {
                    Ok(Some(frame)) => Idle::Frame(Box::new(frame)),
                    Ok(None) | Err(_) => Idle::Closed,
                })
        })
        .await
    }

    /// Answer a frame that arrives while a stream is open but does not
    /// address it: PING is answered, a second EXECUTE is refused as busy,
    /// late credit or cancel for a finished stream is ignored, and anything
    /// else ends the connection.
    async fn interleaved(&mut self, cx: &Cx, frame: &Frame) -> Result<(), Stop> {
        let header = *frame.header();
        let current = self.conn.binding();
        match header.kind() {
            FrameKind::Ping => {
                let Ok(ping) = Ping::decode(frame.payload()) else {
                    return Err(Stop::Transport);
                };
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
            FrameKind::Execute => {
                let busy = ErrorBody {
                    code: ErrorCode::Busy,
                    message: "a statement is already in flight on this connection".into(),
                };
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
            FrameKind::WindowUpdate | FrameKind::QueryCancel => Ok(()),
            _ => {
                let refusal = ErrorBody {
                    code: ErrorCode::Protocol,
                    message: "frame not legal while a result streams".into(),
                };
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

/// One frame's live authority, independent of how long framing or flow control
/// took. The callback clock is sampled at each I/O attempt, never at queue time.
/// This implements Warden's cooperative expiry/retirement fence, not the
/// unimplemented durable audit visibility or time-authority evidence machinery.
struct OutputGuard<'a> {
    binding: Binding,
    authority: Option<(&'a Authority, VerifiedCapability<'a>)>,
}

impl<'a> OutputGuard<'a> {
    fn new(
        binding: Binding,
        authority: Option<(&'a Authority, &CapabilityToken)>,
        now_ms: u64,
    ) -> Result<Self, ProtocolError> {
        let authority = if matches!(binding, Binding::Ready(_)) {
            let (issuer, token) = authority.ok_or(ProtocolError::InvalidBinding)?;
            let verified = issuer
                .verify_at(token, crate::TRUNK, now_ms)
                .map_err(|_| ProtocolError::InvalidBinding)?;
            Some((issuer, verified))
        } else {
            None
        };
        Ok(Self { binding, authority })
    }

    fn authorize(&self, header: &Header, now_ms: u64) -> Result<(), ProtocolError> {
        guard(header, self.binding)?;
        if let Some((issuer, verified)) = &self.authority {
            issuer
                .recheck_at(verified, crate::TRUNK, now_ms)
                .map_err(|_| ProtocolError::InvalidBinding)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod output_guard_tests {
    use super::*;
    use asupersync::io::AsyncWrite;
    use asupersync::security::key::AuthKey;
    use asupersync::{Budget, runtime::RuntimeBuilder};
    use fgdb_delta_types::SchemaEpoch;
    use fgdb_protocol::ReadyBinding;
    use fgdb_protocol::transport::{SendCompletion, TransportError};
    use fgdb_types::DatabaseSecurityNamespaceId;
    use fgdb_warden::{Grant, QueryLimits};
    use std::cell::{Cell, RefCell};
    use std::io;
    use std::pin::Pin;
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

    fn binding() -> Binding {
        Binding::Ready(ReadyBinding {
            session: SessionBinding {
                transcript: [1; 32],
                auth_generation: 1,
            },
            namespace: [7; 32],
            incarnation: [2; 32],
            service_epoch: 1,
            posture: Posture::Local,
            authority_commitment: [3; 32],
        })
    }

    fn limits() -> FrameLimits {
        FrameLimits::new(4096).unwrap()
    }

    fn frame(kind: FrameKind) -> Frame {
        let current = binding();
        let header_binding = if kind == FrameKind::Ready {
            Binding::Session(current.session().unwrap())
        } else {
            current
        };
        Frame::new(
            kind,
            1,
            if kind == FrameKind::Ready {
                StreamId::CONTROL
            } else {
                StreamId([5; 16])
            },
            header_binding,
            b"protected result bytes".to_vec(),
            limits(),
        )
        .unwrap()
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
        writer: FrameWriter<Writer>,
        accepted: Rc<RefCell<Vec<u8>>>,
        flushes: Rc<Cell<usize>>,
    }

    impl Fixture {
        fn new(chunks: impl IntoIterator<Item = usize>, flush_pending: bool) -> Self {
            let accepted = Rc::new(RefCell::new(Vec::new()));
            let flushes = Rc::new(Cell::new(0));
            Self {
                writer: FrameWriter::new(
                    Writer {
                        accepted: Rc::clone(&accepted),
                        chunks: chunks.into_iter().collect(),
                        flush_pending,
                        flushes: Rc::clone(&flushes),
                    },
                    limits(),
                ),
                accepted,
                flushes,
            }
        }

        fn poll(
            &mut self,
            cx: &Cx,
            guard: &OutputGuard<'_>,
            now: u64,
        ) -> Poll<Result<SendCompletion, TransportError>> {
            let mut task = Context::from_waker(Waker::noop());
            self.writer
                .poll_send(cx, &mut task, |header| guard.authorize(header, now))
        }
    }

    fn with_cx(test: impl FnOnce(&Cx)) {
        let runtime = RuntimeBuilder::new().build().unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        test(&cx);
    }

    fn denied(result: Poll<Result<SendCompletion, TransportError>>) {
        assert_eq!(
            result,
            Poll::Ready(Err(TransportError::Protocol(ProtocolError::InvalidBinding)))
        );
    }

    #[test]
    fn selected_output_including_ready_uses_live_exact_issuer() {
        with_cx(|cx| {
            let issuer = authority(41);
            let credential = token(&issuer);
            let guard = OutputGuard::new(binding(), Some((&issuer, &credential)), START).unwrap();
            for kind in [
                FrameKind::Ready,
                FrameKind::SnapshotResultChunk,
                FrameKind::SnapshotResultEnd,
                FrameKind::SubscriptionBatch,
                FrameKind::Error,
            ] {
                let frame = frame(kind);
                let mut io = Fixture::new([3, 0, 7, 0], true);
                io.writer.queue(cx, &frame).unwrap();
                let mut completion = None;
                for _ in 0..8 {
                    if let Poll::Ready(result) = io.poll(cx, &guard, EXPIRES - 1) {
                        completion = Some(result.unwrap());
                        break;
                    }
                }
                let completion = completion.expect("bounded partial writes complete");
                let expected = frame.encode(limits()).unwrap();
                assert_eq!(*io.accepted.borrow(), expected);
                assert_eq!(completion.encoded_bytes, expected.len());
                assert_eq!(io.flushes.get(), 2);
            }
            assert!(OutputGuard::new(binding(), None, START).is_err());
            let foreign = authority(42);
            assert!(OutputGuard::new(binding(), Some((&foreign, &credential)), START).is_err());
            assert!(OutputGuard::new(binding(), Some((&issuer, &credential)), EXPIRES).is_err());
        });
    }

    #[test]
    fn credit_granted_after_expiry_cannot_release_buffered_output() {
        with_cx(|cx| {
            let issuer = authority(41);
            let credential = token(&issuer);
            let guard = OutputGuard::new(binding(), Some((&issuer, &credential)), START).unwrap();
            for kind in [
                FrameKind::SnapshotResultChunk,
                FrameKind::SnapshotResultEnd,
                FrameKind::SubscriptionBatch,
            ] {
                let frame = frame(kind);
                let cost = SendCost {
                    bytes: frame.header().frame_len() as u64,
                    rows: 1,
                };
                let mut window = FlowWindow::new(SendCost { bytes: 0, rows: 0 }, cost).unwrap();
                assert!(matches!(
                    window.reserve(cost),
                    Err(ProtocolError::CreditExceeded)
                ));
                let mut io = Fixture::new([], false);
                io.writer.queue(cx, &frame).unwrap();
                // Credit arrives at the exact expiry boundary. A previously
                // verified/queued frame has no cached permission to send.
                window
                    .grant(CreditUpdate {
                        sequence: 1,
                        bytes: cost.bytes,
                        rows: cost.rows,
                    })
                    .unwrap();
                let mut reservation = window.reserve(cost).unwrap();
                reservation.begin_write().unwrap();
                denied(io.poll(cx, &guard, EXPIRES));
                reservation.failed().unwrap();
                assert!(io.accepted.borrow().is_empty());
                assert_eq!(io.flushes.get(), 0);
                assert_eq!(window.sent(), SendCost { bytes: 0, rows: 0 });
                assert_eq!(window.failed_after_write(), 1);
            }
        });
    }

    #[test]
    fn expiry_while_socket_pending_before_first_byte_is_terminal() {
        with_cx(|cx| {
            let issuer = authority(41);
            let credential = token(&issuer);
            let guard = OutputGuard::new(binding(), Some((&issuer, &credential)), START).unwrap();
            let mut io = Fixture::new([0], false);
            io.writer
                .queue(cx, &frame(FrameKind::SnapshotResultChunk))
                .unwrap();
            assert!(io.poll(cx, &guard, START).is_pending());
            assert!(io.accepted.borrow().is_empty());
            denied(io.poll(cx, &guard, EXPIRES));
            assert!(io.accepted.borrow().is_empty());
            // Neither retry nor a backwards clock can resurrect this lane.
            assert_eq!(
                io.poll(cx, &guard, START),
                Poll::Ready(Err(TransportError::Closed))
            );
        });
    }

    #[test]
    fn retirement_after_partial_write_never_sends_the_suffix() {
        with_cx(|cx| {
            let issuer = authority(41);
            let credential = token(&issuer);
            let guard = OutputGuard::new(binding(), Some((&issuer, &credential)), START).unwrap();
            let mut io = Fixture::new([7, 0], false);
            io.writer
                .queue(cx, &frame(FrameKind::SubscriptionBatch))
                .unwrap();
            assert!(io.poll(cx, &guard, START).is_pending());
            let prefix = io.accepted.borrow().clone();
            assert_eq!(prefix.len(), 7);
            assert!(issuer.retire());
            denied(io.poll(cx, &guard, START));
            assert_eq!(*io.accepted.borrow(), prefix);
            assert_eq!(io.flushes.get(), 0);
            assert_eq!(
                io.poll(cx, &guard, START),
                Poll::Ready(Err(TransportError::Closed))
            );
        });
    }

    #[test]
    fn resumed_flush_rechecks_expiry_and_retirement_before_completion() {
        with_cx(|cx| {
            for retire in [false, true] {
                let issuer = authority(41);
                let credential = token(&issuer);
                let guard =
                    OutputGuard::new(binding(), Some((&issuer, &credential)), START).unwrap();
                let frame = frame(FrameKind::SnapshotResultEnd);
                let mut io = Fixture::new([], true);
                io.writer.queue(cx, &frame).unwrap();
                assert!(io.poll(cx, &guard, START).is_pending());
                let bytes = io.accepted.borrow().clone();
                assert_eq!(bytes, frame.encode(limits()).unwrap());
                assert_eq!(io.flushes.get(), 1);
                let now = if retire {
                    issuer.retire();
                    START
                } else {
                    EXPIRES
                };
                denied(io.poll(cx, &guard, now));
                assert_eq!(*io.accepted.borrow(), bytes);
                assert_eq!(io.flushes.get(), 1, "no flush after invalidation");
            }
        });
    }

    #[test]
    fn preselection_errors_keep_their_uniform_session_binding() {
        let session = binding().session().unwrap();
        let guard = OutputGuard::new(Binding::Session(session), None, EXPIRES).unwrap();
        let refusal = Frame::new(
            FrameKind::Error,
            1,
            StreamId::CONTROL,
            Binding::Session(session),
            b"database not found or not authorized".to_vec(),
            limits(),
        )
        .unwrap();
        assert_eq!(guard.authorize(refusal.header(), EXPIRES), Ok(()));
        let protected = frame(FrameKind::SnapshotResultChunk);
        assert_eq!(
            guard.authorize(protected.header(), START),
            Err(ProtocolError::InvalidBinding)
        );
    }
}
