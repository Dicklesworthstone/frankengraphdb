//! Binary WebSocket messages as one bounded FGP byte stream.
//!
//! The foundation codec owns RFC 6455 framing, masking and control validation.
//! This adapter owns only binary-message boundaries and the byte-stream bridge.
//! HTTP upgrade, origin/Host policy and subprotocol negotiation precede it.
//!
//! An application write advances at most one underlying write per poll. The
//! reported count covers payload bytes, never the WebSocket header. A partial
//! encoded frame retains its exact unsent input: dropping a write future cannot
//! restart it, and substituting a different suffix poisons the stream.
//!
//! The FGP FrameWriter must retain its fresh authorization callback around every
//! write and flush. Once application output is attempted, reads cannot perform
//! ANY underlying write or flush until the application frame is complete and a
//! guarded flush succeeds. In particular, Ping and Close handling cannot drain
//! protected TLS ciphertext. Shutdown refuses unfinished application output.
//! A completed send remains transport completion, never a durable result ACK.

use crate::transport::DuplexIo;
use asupersync::Cx;
use asupersync::bytes::{Bytes, BytesMut};
use asupersync::codec::{Decoder, Encoder};
use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
use asupersync::net::websocket::{CloseReason, Frame, FrameCodec, Opcode, Role, WsError};
use core::pin::Pin;
use core::task::{Context, Poll};
use std::io::{self, ErrorKind};

const READ_AHEAD: usize = 4096;
const MAX_UPGRADE_PREFIX: usize = 16 * 1024;
const WRITE_CHUNK: usize = 16 * 1024;
const FRAMES_PER_POLL: usize = 16;
const MAX_FRAME_HEADER: usize = 14;
const CONTROL_PAYLOAD: usize = 125;

struct DataWrite {
    wire: BytesMut,
    payload: Bytes,
    header_len: usize,
    written: usize,
    accepted: usize,
}

struct ControlWrite {
    wire: BytesMut,
    written: usize,
    close: bool,
}

/// An upgraded WebSocket connection carrying the ordinary FGP byte stream.
///
/// Messages may contain several FGP frames, and an FGP frame may cross message
/// or fragment boundaries. Only binary messages are accepted. Each complete
/// fragmented message has the configured byte ceiling; fragments are exposed
/// incrementally and a later protocol failure cannot undo an earlier FGP
/// operation. Native FGP framing, authorization and transaction rules still
/// determine when an operation may run.
///
/// Use this with the cancellation-resumable FGP FrameReader/FrameWriter, whose
/// pending writes retain their original byte suffix. A changed suffix during a
/// partial WebSocket frame is refused instead of completing an abandoned send.
///
/// The stored context supplies cancellation checkpoints. The underlying stream
/// must retain the normal AsyncWrite contract, and any buffering below this
/// adapter must not flush application output from its read implementation.
pub struct WebSocketIo<T> {
    io: T,
    cx: Cx,
    codec: FrameCodec,
    max_message_bytes: usize,
    max_input_bytes: usize,
    input: BytesMut,
    fragment_bytes: Option<usize>,
    payload: Bytes,
    payload_at: usize,
    data: Option<DataWrite>,
    application_dirty: bool,
    control: Option<ControlWrite>,
    queued_control: Option<Frame>,
    peer_closed: bool,
    sent_close: bool,
    shutting_down: bool,
    failed: Option<ErrorKind>,
}

impl<T: DuplexIo> WebSocketIo<T> {
    /// Wrap a server-side stream after a successful HTTP upgrade.
    ///
    /// The prefix is the upgrade reader's unconsumed suffix, bounded to 16 KiB
    /// independently of the message ceiling because it may contain several
    /// small frames. The ceiling must be nonzero and fit the native u32 frame
    /// domain. No buffer of that maximum size is allocated at construction.
    pub fn server(cx: &Cx, io: T, max_message_bytes: usize, prefix: Vec<u8>) -> io::Result<Self> {
        Self::new(cx, io, Role::Server, max_message_bytes, prefix)
    }

    /// Wrap a client-side stream after validating the server's HTTP upgrade.
    ///
    /// The foundation encoder uses OS entropy for RFC 6455 masking keys.
    /// This client carrier therefore does not offer seeded wire-byte replay;
    /// logical FGP bytes and result identity are independent of masking.
    /// Server-role framing consumes no masking entropy.
    pub fn client(cx: &Cx, io: T, max_message_bytes: usize, prefix: Vec<u8>) -> io::Result<Self> {
        Self::new(cx, io, Role::Client, max_message_bytes, prefix)
    }

    fn new(
        cx: &Cx,
        io: T,
        role: Role,
        max_message_bytes: usize,
        prefix: Vec<u8>,
    ) -> io::Result<Self> {
        if max_message_bytes == 0
            || u32::try_from(max_message_bytes).is_err()
            || prefix.len() > MAX_UPGRADE_PREFIX
        {
            return Err(refusal(ErrorKind::InvalidInput));
        }
        cx.checkpoint()
            .map_err(|_| refusal(ErrorKind::ConnectionAborted))?;
        // A small binary-message ceiling must not reject a lawful control
        // payload. Data frames receive their separate cumulative check below.
        let frame_limit = max_message_bytes.max(CONTROL_PAYLOAD);
        let max_input_bytes = frame_limit
            .checked_add(MAX_UPGRADE_PREFIX)
            .and_then(|value| value.checked_add(MAX_FRAME_HEADER))
            .ok_or_else(|| refusal(ErrorKind::InvalidInput))?;
        Ok(Self {
            io,
            cx: cx.clone(),
            codec: FrameCodec::new(role).max_payload_size(frame_limit),
            max_message_bytes,
            max_input_bytes,
            input: prefix.into(),
            fragment_bytes: None,
            payload: Bytes::new(),
            payload_at: 0,
            data: None,
            application_dirty: false,
            control: None,
            queued_control: None,
            peer_closed: false,
            sent_close: false,
            shutting_down: false,
            failed: None,
        })
    }

    fn fail(&mut self, kind: ErrorKind) -> io::Error {
        // Interrupted I/O is retryable with the same retained frame, as it is
        // in the native transport. All protocol/closure errors are terminal.
        if kind != ErrorKind::Interrupted {
            self.failed = Some(kind);
        }
        refusal(kind)
    }

    fn check(&mut self) -> io::Result<()> {
        if let Some(kind) = self.failed {
            return Err(refusal(kind));
        }
        if self.cx.checkpoint().is_err() {
            return Err(self.fail(ErrorKind::ConnectionAborted));
        }
        Ok(())
    }

    fn codec_failure(&mut self, error: WsError) -> io::Error {
        let kind = match error {
            WsError::Io(error) => error.kind(),
            _ => ErrorKind::InvalidData,
        };
        self.fail(kind)
    }

    fn protected_output_pending(&self) -> bool {
        self.data.is_some() || self.application_dirty
    }

    fn has_control(&self) -> bool {
        self.control.is_some() || self.queued_control.is_some()
    }

    fn queue_control(&mut self, frame: Frame) {
        if self.sent_close
            || self
                .queued_control
                .as_ref()
                .is_some_and(|pending| pending.opcode == Opcode::Close)
        {
            return;
        }
        // RFC 6455 permits replying only to the most recently processed Ping.
        // Keep one in-flight control and one latest successor, never an
        // unbounded queue a peer can grow while its socket is backpressured.
        self.queued_control = Some(frame);
    }

    /// Only called at a boundary with no unfinished/unflushed application
    /// output. Exactly one lower write OR flush is attempted per invocation.
    fn poll_control(&mut self, task: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.protected_output_pending() {
            return Poll::Ready(Ok(()));
        }
        if self.control.is_none() {
            let Some(frame) = self.queued_control.take() else {
                return Poll::Ready(Ok(()));
            };
            let close = frame.opcode == Opcode::Close;
            let mut wire = BytesMut::new();
            if let Err(error) = self.codec.encode(frame, &mut wire) {
                return Poll::Ready(Err(self.codec_failure(error)));
            }
            self.control = Some(ControlWrite {
                wire,
                written: 0,
                close,
            });
        }
        let Some(pending) = self.control.as_mut() else {
            return Poll::Ready(Err(self.fail(ErrorKind::InvalidData)));
        };
        if pending.written < pending.wire.len() {
            let remaining = &pending.wire[pending.written..];
            match Pin::new(&mut self.io).poll_write(task, remaining) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(error)) => Poll::Ready(Err(self.fail(error.kind()))),
                Poll::Ready(Ok(count)) if count > 0 && count <= remaining.len() => {
                    pending.written += count;
                    task.waker().wake_by_ref();
                    Poll::Pending
                }
                Poll::Ready(Ok(_)) => Poll::Ready(Err(self.fail(ErrorKind::WriteZero))),
            }
        } else {
            match Pin::new(&mut self.io).poll_flush(task) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(error)) => Poll::Ready(Err(self.fail(error.kind()))),
                Poll::Ready(Ok(())) => {
                    let close = pending.close;
                    self.control = None;
                    if close {
                        self.sent_close = true;
                        self.queued_control = None;
                    } else if self.queued_control.is_some() {
                        // A read may otherwise wait forever for new input
                        // after this flush, leaving its successor unscheduled.
                        task.waker().wake_by_ref();
                    }
                    Poll::Ready(Ok(()))
                }
            }
        }
    }

    fn receive_data(&mut self, frame: Frame) -> io::Result<()> {
        let previous = match (frame.opcode, self.fragment_bytes) {
            (Opcode::Binary, None) => 0,
            (Opcode::Continuation, Some(bytes)) => bytes,
            _ => return Err(self.fail(ErrorKind::InvalidData)),
        };
        let Some(total) = previous.checked_add(frame.payload.len()) else {
            return Err(self.fail(ErrorKind::InvalidData));
        };
        if total > self.max_message_bytes {
            return Err(self.fail(ErrorKind::InvalidData));
        }
        self.fragment_bytes = (!frame.fin).then_some(total);
        self.payload = frame.payload;
        self.payload_at = 0;
        Ok(())
    }
}

fn refusal(kind: ErrorKind) -> io::Error {
    // Neither peer bytes nor foundation/OS diagnostics escape the adapter.
    io::Error::new(kind, "WebSocket byte transport refused")
}

impl<T: DuplexIo> AsyncRead for WebSocketIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        task: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if output.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if let Err(error) = this.check() {
            return Poll::Ready(Err(error));
        }
        // A public control can progress during an idle read, but never carry
        // an abandoned application's encoded bytes or buffered TLS ciphertext.
        // A pending control write does not prevent incoming credit/cancel data
        // from being read: otherwise a slow peer could deadlock both lanes.
        let control_pending = if this.protected_output_pending() {
            false
        } else {
            match this.poll_control(task) {
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) => false,
                Poll::Pending => true,
            }
        };
        if this.peer_closed {
            if this.protected_output_pending() || this.sent_close {
                return Poll::Ready(Ok(()));
            }
            if !control_pending {
                task.waker().wake_by_ref();
            }
            return Poll::Pending;
        }
        if this.sent_close || this.shutting_down {
            return Poll::Ready(Ok(()));
        }
        let mut read_attempted = false;
        for _ in 0..FRAMES_PER_POLL {
            if let Err(error) = this.check() {
                return Poll::Ready(Err(error));
            }
            if this.payload_at < this.payload.len() {
                let count = output.remaining().min(this.payload.len() - this.payload_at);
                output.put_slice(&this.payload[this.payload_at..this.payload_at + count]);
                this.payload_at += count;
                if this.payload_at == this.payload.len() {
                    this.payload = Bytes::new();
                    this.payload_at = 0;
                }
                return Poll::Ready(Ok(()));
            }
            let frame = match this.codec.decode(&mut this.input) {
                Ok(Some(frame)) => frame,
                Err(error) => return Poll::Ready(Err(this.codec_failure(error))),
                Ok(None) => {
                    if read_attempted {
                        task.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                    read_attempted = true;
                    let mut scratch = [0; READ_AHEAD];
                    let mut buffer = ReadBuf::new(&mut scratch);
                    match Pin::new(&mut this.io).poll_read(task, &mut buffer) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(error)) => {
                            return Poll::Ready(Err(this.fail(error.kind())));
                        }
                        Poll::Ready(Ok(())) if buffer.filled().is_empty() => {
                            // TCP loss is not a WebSocket Close handshake.
                            return Poll::Ready(Err(this.fail(ErrorKind::UnexpectedEof)));
                        }
                        Poll::Ready(Ok(())) => {
                            let bytes = buffer.filled();
                            if bytes.len() > this.max_input_bytes.saturating_sub(this.input.len()) {
                                return Poll::Ready(Err(this.fail(ErrorKind::InvalidData)));
                            }
                            this.input.put_slice(bytes);
                            continue;
                        }
                    }
                }
            };
            match frame.opcode {
                Opcode::Binary | Opcode::Continuation => {
                    if let Err(error) = this.receive_data(frame) {
                        return Poll::Ready(Err(error));
                    }
                }
                Opcode::Ping => {
                    this.queue_control(Frame::pong(frame.payload));
                    task.waker().wake_by_ref();
                }
                Opcode::Pong => {}
                Opcode::Close => {
                    // The foundation normalizes receive-only reserved close
                    // codes to an empty reply instead of illegally echoing one.
                    let reason = match CloseReason::parse(&frame.payload) {
                        Ok(reason) => reason,
                        Err(error) => return Poll::Ready(Err(this.codec_failure(error))),
                    };
                    this.peer_closed = true;
                    this.queue_control(reason.to_frame());
                    task.waker().wake_by_ref();
                    return Poll::Pending;
                }
                Opcode::Text => return Poll::Ready(Err(this.fail(ErrorKind::InvalidData))),
            }
        }
        task.waker().wake_by_ref();
        Poll::Pending
    }
}

impl<T: DuplexIo> AsyncWrite for WebSocketIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        task: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Err(error) = this.check() {
            return Poll::Ready(Err(error));
        }
        if this.peer_closed || this.sent_close || this.shutting_down {
            return Poll::Ready(Err(this.fail(ErrorKind::BrokenPipe)));
        }
        if bytes.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if this.data.is_none() && !this.application_dirty && this.has_control() {
            return match this.poll_control(task) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) => {
                    task.waker().wake_by_ref();
                    Poll::Pending
                }
            };
        }
        if this.data.is_none() {
            let count = bytes.len().min(this.max_message_bytes).min(WRITE_CHUNK);
            let payload = Bytes::from(bytes[..count].to_vec());
            let mut wire = BytesMut::new();
            if let Err(error) = this.codec.encode(Frame::binary(payload.clone()), &mut wire) {
                return Poll::Ready(Err(this.codec_failure(error)));
            }
            this.data = Some(DataWrite {
                header_len: wire.len() - payload.len(),
                wire,
                payload,
                written: 0,
                accepted: 0,
            });
        }
        let Some(pending) = this.data.as_mut() else {
            return Poll::Ready(Err(this.fail(ErrorKind::InvalidData)));
        };
        let expected = &pending.payload[pending.accepted..];
        if !bytes.starts_with(expected) {
            return Poll::Ready(Err(this.fail(ErrorKind::InvalidInput)));
        }
        // A lower TLS write may retain ciphertext even when it returns Pending.
        // Only an explicitly guarded flush below can clear this fence.
        this.application_dirty = true;
        let remaining = &pending.wire[pending.written..];
        match Pin::new(&mut this.io).poll_write(task, remaining) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(this.fail(error.kind()))),
            Poll::Ready(Ok(count)) if count > 0 && count <= remaining.len() => {
                pending.written += count;
                let accepted = pending.written.saturating_sub(pending.header_len);
                let payload_count = accepted - pending.accepted;
                pending.accepted = accepted;
                if pending.written == pending.wire.len() {
                    this.data = None;
                }
                if payload_count == 0 {
                    task.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    Poll::Ready(Ok(payload_count))
                }
            }
            Poll::Ready(Ok(_)) => Poll::Ready(Err(this.fail(ErrorKind::WriteZero))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = this.check() {
            return Poll::Ready(Err(error));
        }
        if this.protected_output_pending() {
            // Flush only bytes the lower writer already accepted. Never send
            // a retained but unacknowledged input suffix from poll_flush.
            return match Pin::new(&mut this.io).poll_flush(task) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(error)) => Poll::Ready(Err(this.fail(error.kind()))),
                Poll::Ready(Ok(())) => {
                    if this.data.is_none() {
                        this.application_dirty = false;
                    }
                    // Public controls need not hold up the FGP completion;
                    // the next read or guarded write services them.
                    Poll::Ready(Ok(()))
                }
            };
        }
        if this.has_control() {
            return match this.poll_control(task) {
                Poll::Ready(Ok(())) if this.has_control() => {
                    task.waker().wake_by_ref();
                    Poll::Pending
                }
                result => result,
            };
        }
        match Pin::new(&mut this.io).poll_flush(task) {
            Poll::Ready(Err(error)) => Poll::Ready(Err(this.fail(error.kind()))),
            result => result,
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = this.check() {
            return Poll::Ready(Err(error));
        }
        if this.protected_output_pending() {
            // Even lower shutdown may flush TLS. The caller must finish or
            // abandon the guarded application send before touching it.
            return Poll::Ready(Err(refusal(ErrorKind::InvalidInput)));
        }
        this.shutting_down = true;
        if !this.sent_close && !this.has_control() {
            this.queue_control(Frame::close(Some(1000), None));
        }
        if this.has_control() {
            return match this.poll_control(task) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) => {
                    task.waker().wake_by_ref();
                    Poll::Pending
                }
            };
        }
        match Pin::new(&mut this.io).poll_shutdown(task) {
            Poll::Ready(Err(error)) => Poll::Ready(Err(this.fail(error.kind()))),
            result => result,
        }
    }
}

#[cfg(test)]
mod tests;
