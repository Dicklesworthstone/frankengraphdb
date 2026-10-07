//! Bounded, cancellation-resumable FGP I/O on the foundation's stream traits.
//!
//! TCP, TLS and laboratory streams use the same driver. The driver owns all
//! partial-read/write state, not the short-lived future returned by `receive`
//! or `send`. Dropping either future therefore cannot discard read-ahead or
//! restart a partially written frame at byte zero.
//!
//! This is transport machinery, not authentication or result ownership. The
//! host validates incoming headers against `Connection`, reauthorizes before
//! dispatch, reserves flow credit and queues a send obligation before `queue`,
//! and provides a fresh send-guard check at each physical write attempt. A
//! `SendCompletion` discharges only that transport obligation, never a result
//! ACK. A callback must not approve a protected frame using queue-time state.

use crate::{Decoder, Frame, FrameLimits, Header, ProtocolError};
use asupersync::Cx;
use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
use core::future::poll_fn;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::io::ErrorKind;
use std::sync::{Arc, Mutex, TryLockError};

const READ_AHEAD: usize = 4096;
const IO_POLLS_PER_TURN: usize = 16;

/// The foundation's duplex streams, including TCP, TLS and laboratory I/O.
/// No protocol framing or encryption is implemented by this marker trait.
pub trait DuplexIo: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> DuplexIo for T {}

/// One receive handle for a duplex stream whose TLS state cannot be split.
pub struct DuplexReader<T> {
    io: Arc<Mutex<T>>,
}

/// The corresponding send handle. Its mutex is held for one nonblocking poll
/// only, never across an await. Readers and writers retain independent frame
/// state while sharing the foundation's single encryption state.
pub struct DuplexWriter<T> {
    io: Arc<Mutex<T>>,
}

/// Split ownership without splitting a TLS session or requiring a !Send
/// borrowed stream. The two handles never expose the underlying stream.
pub fn split_duplex<T: DuplexIo>(
    cx: &Cx,
    io: T,
) -> Result<(DuplexReader<T>, DuplexWriter<T>), TransportError> {
    cx.checkpoint()
        .map_err(|_| TransportError::ContextStopped)?;
    let io = Arc::new(Mutex::new(io));
    Ok((
        DuplexReader {
            io: Arc::clone(&io),
        },
        DuplexWriter { io },
    ))
}

// A different task can momentarily own the other half. Do not block a runtime
// worker or hold a guard on Pending; arrange another bounded poll instead.
fn duplex_poll<T, U>(
    io: &Mutex<T>,
    task: &mut Context<'_>,
    poll: impl FnOnce(&mut T, &mut Context<'_>) -> Poll<std::io::Result<U>>,
) -> Poll<std::io::Result<U>> {
    match io.try_lock() {
        Ok(mut io) => poll(&mut io, task),
        Err(TryLockError::WouldBlock) => {
            task.waker().wake_by_ref();
            Poll::Pending
        }
        Err(TryLockError::Poisoned(_)) => Poll::Ready(Err(std::io::Error::other(
            "duplex transport state is unavailable",
        ))),
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for DuplexReader<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        task: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        duplex_poll(&self.io, task, |io, task| {
            Pin::new(io).poll_read(task, buffer)
        })
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for DuplexWriter<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        task: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        duplex_poll(&self.io, task, |io, task| {
            Pin::new(io).poll_write(task, bytes)
        })
    }

    fn poll_flush(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        duplex_poll(&self.io, task, |io, task| Pin::new(io).poll_flush(task))
    }

    fn poll_shutdown(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        duplex_poll(&self.io, task, |io, task| Pin::new(io).poll_shutdown(task))
    }
}

/// Redacted transport failures. OS error text and peer-controlled bytes are
/// deliberately excluded from both Display and Debug.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportError {
    Protocol(ProtocolError),
    Io(ErrorKind),
    ContextStopped,
    Closed,
    SendInProgress,
    NoPendingSend,
}

impl From<ProtocolError> for TransportError {
    fn from(error: ProtocolError) -> Self {
        Self::Protocol(error)
    }
}

impl core::fmt::Display for TransportError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Protocol(error) => error.fmt(f),
            Self::Io(_) => f.write_str("FGP transport I/O failed"),
            Self::ContextStopped => f.write_str("FGP transport context stopped"),
            Self::Closed => f.write_str("FGP transport is closed"),
            Self::SendInProgress => f.write_str("an FGP frame is already queued"),
            Self::NoPendingSend => f.write_str("no FGP frame is queued"),
        }
    }
}
impl core::error::Error for TransportError {}

/// One bounded receive lane. At most one decoded frame and READ_AHEAD bytes
/// are retained. There is no frame queue whose size the peer can amplify.
/// The supplied stream must not be independently read by another task.
pub struct FrameReader<R> {
    io: R,
    decoder: Decoder,
    bytes: [u8; READ_AHEAD],
    start: usize,
    end: usize,
    eof: bool,
    failed: bool,
}

impl<R> FrameReader<R> {
    pub fn new(io: R, limits: FrameLimits) -> Self {
        Self {
            io,
            decoder: Decoder::new(limits),
            bytes: [0; READ_AHEAD],
            start: 0,
            end: 0,
            eof: false,
            failed: false,
        }
    }

    pub fn buffered_bytes(&self) -> usize {
        self.end - self.start + self.decoder.buffered_payload_bytes()
    }
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    /// Header validation runs before body allocation AND again on complete
    /// frame assembly. It must be a repeatable check, not an admission effect.
    /// A partially received header cannot retain stale authority across await.
    /// Dispatch must still independently authorize the request's body.
    pub fn poll_receive(
        &mut self,
        cx: &Cx,
        task: &mut Context<'_>,
        mut validate: impl FnMut(&Header) -> Result<(), ProtocolError>,
    ) -> Poll<Result<Option<Frame>, TransportError>> {
        if self.failed {
            return Poll::Ready(Err(TransportError::Closed));
        }
        for _ in 0..IO_POLLS_PER_TURN {
            if cx.checkpoint().is_err() {
                return Poll::Ready(Err(TransportError::ContextStopped));
            }
            if self.start != self.end {
                let progress = match self
                    .decoder
                    .decode(&self.bytes[self.start..self.end], &mut validate)
                {
                    Ok(progress) => progress,
                    Err(error) => {
                        self.failed = true;
                        return Poll::Ready(Err(error.into()));
                    }
                };
                self.start += progress.consumed;
                if let Some(frame) = progress.frame {
                    if let Err(error) = validate(frame.header()) {
                        self.failed = true;
                        return Poll::Ready(Err(error.into()));
                    }
                    // Do not decode the unread suffix until the caller has
                    // applied this frame's connection-state transition.
                    return Poll::Ready(Ok(Some(frame)));
                }
            }
            if self.eof {
                return Poll::Ready(Ok(None));
            }
            let mut buffer = ReadBuf::new(&mut self.bytes);
            match Pin::new(&mut self.io).poll_read(task, &mut buffer) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) if error.kind() == ErrorKind::Interrupted => continue,
                Poll::Ready(Err(error)) => {
                    self.failed = true;
                    return Poll::Ready(Err(TransportError::Io(error.kind())));
                }
                Poll::Ready(Ok(())) => {
                    self.start = 0;
                    self.end = buffer.filled().len();
                    if self.end == 0 {
                        self.eof = true;
                        if let Err(error) = self.decoder.finish_eof() {
                            self.failed = true;
                            return Poll::Ready(Err(error.into()));
                        }
                        return Poll::Ready(Ok(None));
                    }
                }
            }
        }
        // Even an always-ready one-byte reader cannot monopolize a worker.
        task.waker().wake_by_ref();
        Poll::Pending
    }

    pub async fn receive(
        &mut self,
        cx: &Cx,
        mut validate: impl FnMut(&Header) -> Result<(), ProtocolError>,
    ) -> Result<Option<Frame>, TransportError> {
        poll_fn(|task| self.poll_receive(cx, task, &mut validate)).await
    }
}

struct PendingFrame {
    header: Header,
    bytes: Vec<u8>,
    written: usize,
}

/// Physical completion only. It is intentionally not convertible into any
/// durable release, result-ACK or transaction-outcome evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendCompletion {
    pub encoded_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbandonedSend {
    CancelledBeforeWrite,
    /// The old stream is permanently unusable. Flow credit must not be
    /// refunded and an uncertain result must not be acknowledged.
    FailedAfterWrite {
        accepted_bytes: usize,
    },
}

/// One serialized writer. No API exposes its underlying stream while a frame
/// is queued. Application code cannot accidentally interleave two frames.
pub struct FrameWriter<W> {
    io: W,
    limits: FrameLimits,
    pending: Option<PendingFrame>,
    failed: bool,
}

impl<W> FrameWriter<W> {
    pub fn new(io: W, limits: FrameLimits) -> Self {
        Self {
            io,
            limits,
            pending: None,
            failed: false,
        }
    }

    /// Reserve the application's flow credit and transport obligation first.
    /// This method does not perform I/O and grants no authority to send.
    pub fn queue(&mut self, cx: &Cx, frame: &Frame) -> Result<(), TransportError> {
        if self.failed {
            return Err(TransportError::Closed);
        }
        if self.pending.is_some() {
            return Err(TransportError::SendInProgress);
        }
        cx.checkpoint()
            .map_err(|_| TransportError::ContextStopped)?;
        let bytes = frame.encode(self.limits)?;
        self.pending = Some(PendingFrame {
            header: *frame.header(),
            bytes,
            written: 0,
        });
        Ok(())
    }

    pub fn accepted_bytes(&self) -> Option<usize> {
        self.pending.as_ref().map(|pending| pending.written)
    }

    /// Explicit local transport cancellation, never durable result release.
    /// A frame whose prefix was accepted cannot be replaced on this stream.
    pub fn abandon(&mut self, _cx: &Cx) -> Result<AbandonedSend, TransportError> {
        let pending = self.pending.take().ok_or(TransportError::NoPendingSend)?;
        if pending.written == 0 && !self.failed {
            Ok(AbandonedSend::CancelledBeforeWrite)
        } else {
            self.failed = true;
            Ok(AbandonedSend::FailedAfterWrite {
                accepted_bytes: pending.written,
            })
        }
    }
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    /// The host's callback must check the current binding, current authority
    /// and exact frame's send-guard evidence immediately before EACH I/O
    /// attempt, including flush. Returning Pending from the stream does not
    /// cache authorization. Revocation after a prefix closes the send lane.
    ///
    /// A context stop preserves offsets for explicit structured cleanup under
    /// a live cleanup context; it does not silently restart or discard a send.
    pub fn poll_send(
        &mut self,
        cx: &Cx,
        task: &mut Context<'_>,
        mut authorize: impl FnMut(&Header) -> Result<(), ProtocolError>,
    ) -> Poll<Result<SendCompletion, TransportError>> {
        if self.failed {
            return Poll::Ready(Err(TransportError::Closed));
        }
        for _ in 0..IO_POLLS_PER_TURN {
            if cx.checkpoint().is_err() {
                return Poll::Ready(Err(TransportError::ContextStopped));
            }
            let Some(pending) = self.pending.as_mut() else {
                return Poll::Ready(Err(TransportError::NoPendingSend));
            };
            if let Err(error) = authorize(&pending.header) {
                self.failed = true;
                return Poll::Ready(Err(error.into()));
            }
            if pending.written == pending.bytes.len() {
                match Pin::new(&mut self.io).poll_flush(task) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) if error.kind() == ErrorKind::Interrupted => continue,
                    Poll::Ready(Err(error)) => {
                        self.failed = true;
                        return Poll::Ready(Err(TransportError::Io(error.kind())));
                    }
                    Poll::Ready(Ok(())) => {
                        let completion = SendCompletion {
                            encoded_bytes: pending.written,
                        };
                        self.pending = None;
                        return Poll::Ready(Ok(completion));
                    }
                }
            }
            let remaining = &pending.bytes[pending.written..];
            match Pin::new(&mut self.io).poll_write(task, remaining) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) if error.kind() == ErrorKind::Interrupted => continue,
                Poll::Ready(Err(error)) => {
                    self.failed = true;
                    return Poll::Ready(Err(TransportError::Io(error.kind())));
                }
                Poll::Ready(Ok(count)) if count > 0 && count <= remaining.len() => {
                    pending.written += count;
                }
                Poll::Ready(Ok(_)) => {
                    self.failed = true;
                    return Poll::Ready(Err(TransportError::Io(ErrorKind::WriteZero)));
                }
            }
        }
        task.waker().wake_by_ref();
        Poll::Pending
    }

    pub async fn send(
        &mut self,
        cx: &Cx,
        mut authorize: impl FnMut(&Header) -> Result<(), ProtocolError>,
    ) -> Result<SendCompletion, TransportError> {
        poll_fn(|task| self.poll_send(cx, task, &mut authorize)).await
    }
}
