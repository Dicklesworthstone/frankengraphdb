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
use core::task::{Context, Poll, Waker};
use std::io::ErrorKind;
use std::sync::{Arc, Mutex, PoisonError, TryLockError};
use std::task::Wake;

const READ_AHEAD: usize = 4096;
const IO_POLLS_PER_TURN: usize = 16;

/// The foundation's duplex streams, including TCP, TLS and laboratory I/O.
/// No protocol framing or encryption is implemented by this marker trait.
pub trait DuplexIo: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> DuplexIo for T {}

const READ_LANE: usize = 0;
const WRITE_LANE: usize = 1;

/// A duplex stream may retain only its most recent I/O waker. Both halves
/// therefore poll it with ONE stable waker, which forwards readiness to the
/// latest task registered on each lane. No stream or TLS state is retained by
/// this waker, so an underlying I/O registration cannot keep the stream alive.
#[derive(Default)]
struct DuplexWake {
    tasks: Mutex<[Option<Waker>; 2]>,
}

impl DuplexWake {
    fn register(&self, lane: usize, task: &Waker) {
        // User-supplied clone/drop/wake callbacks never run under this mutex.
        let task = task.clone();
        let previous =
            self.tasks.lock().unwrap_or_else(PoisonError::into_inner)[lane].replace(task);
        drop(previous);
    }

    fn clear(&self, lane: usize) {
        let previous = self.tasks.lock().unwrap_or_else(PoisonError::into_inner)[lane].take();
        drop(previous);
    }

    fn wake_lane(&self, lane: usize) {
        let task = self.tasks.lock().unwrap_or_else(PoisonError::into_inner)[lane].take();
        if let Some(task) = task {
            task.wake();
        }
    }

    fn wake_tasks(&self) {
        let [reader, writer] =
            core::mem::take(&mut *self.tasks.lock().unwrap_or_else(PoisonError::into_inner));
        match (reader, writer) {
            (Some(reader), Some(writer)) if reader.will_wake(&writer) => reader.wake(),
            (reader, writer) => {
                if let Some(reader) = reader {
                    reader.wake();
                }
                if let Some(writer) = writer {
                    writer.wake();
                }
            }
        }
    }
}

impl Wake for DuplexWake {
    fn wake(self: Arc<Self>) {
        self.wake_tasks();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.wake_tasks();
    }
}

struct DuplexState<T> {
    io: Mutex<T>,
    wake: Arc<DuplexWake>,
    io_waker: Waker,
}

/// One receive handle for a duplex stream whose TLS state cannot be split.
pub struct DuplexReader<T> {
    io: Arc<DuplexState<T>>,
}

/// The corresponding send handle. Its mutex is held for one nonblocking poll
/// only, never across an await. Readers and writers retain independent frame
/// state while sharing the foundation's single encryption state.
pub struct DuplexWriter<T> {
    io: Arc<DuplexState<T>>,
}

/// Split ownership without splitting a TLS session or requiring a !Send
/// borrowed stream. The two handles never expose the underlying stream.
pub fn split_duplex<T: DuplexIo>(
    cx: &Cx,
    io: T,
) -> Result<(DuplexReader<T>, DuplexWriter<T>), TransportError> {
    cx.checkpoint()
        .map_err(|_| TransportError::ContextStopped)?;
    let wake = Arc::new(DuplexWake::default());
    let io = Arc::new(DuplexState {
        io: Mutex::new(io),
        io_waker: Waker::from(Arc::clone(&wake)),
        wake,
    });
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
    state: &DuplexState<T>,
    lane: usize,
    task: &mut Context<'_>,
    poll: impl FnOnce(&mut T, &mut Context<'_>) -> Poll<std::io::Result<U>>,
) -> Poll<std::io::Result<U>> {
    // Register BEFORE polling: an I/O implementation can wake synchronously
    // and still return Pending. The other half never replaces this task.
    state.wake.register(lane, task.waker());
    let result = match state.io.try_lock() {
        Ok(mut io) => poll(&mut io, &mut Context::from_waker(&state.io_waker)),
        Err(TryLockError::WouldBlock) => {
            task.waker().wake_by_ref();
            Poll::Pending
        }
        Err(TryLockError::Poisoned(_)) => Poll::Ready(Err(std::io::Error::other(
            "duplex transport state is unavailable",
        ))),
    };
    if result.is_ready() {
        state.wake.clear(lane);
        // TLS progress can satisfy the opposite operation from buffered state
        // without another kernel readiness edge. Wake only after unlocking I/O.
        state.wake.wake_lane(1 - lane);
    }
    result
}

impl<T> Drop for DuplexReader<T> {
    fn drop(&mut self) {
        self.io.wake.clear(READ_LANE);
    }
}

impl<T> Drop for DuplexWriter<T> {
    fn drop(&mut self) {
        self.io.wake.clear(WRITE_LANE);
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for DuplexReader<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        task: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        duplex_poll(&self.io, READ_LANE, task, |io, task| {
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
        duplex_poll(&self.io, WRITE_LANE, task, |io, task| {
            Pin::new(io).poll_write(task, bytes)
        })
    }

    fn poll_flush(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        duplex_poll(&self.io, WRITE_LANE, task, |io, task| {
            Pin::new(io).poll_flush(task)
        })
    }

    fn poll_shutdown(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        duplex_poll(&self.io, WRITE_LANE, task, |io, task| {
            Pin::new(io).poll_shutdown(task)
        })
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

#[cfg(test)]
mod duplex_tests {
    use super::*;
    use asupersync::{Budget, runtime::RuntimeBuilder};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl WakeCount {
        fn count(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn task() -> (Arc<WakeCount>, Waker) {
        let count = Arc::new(WakeCount::default());
        let waker = Waker::from(Arc::clone(&count));
        (count, waker)
    }

    /// Deliberately retains ONE registration across all four I/O methods.
    /// Directly forwarding per-half task wakers loses the earlier waiter.
    #[derive(Default)]
    struct OneRegistration {
        waker: Mutex<Option<Waker>>,
        ready: AtomicBool,
        synchronous_wake: AtomicBool,
    }

    impl OneRegistration {
        fn poll_ready(&self, task: &Context<'_>) -> bool {
            if self.ready.load(Ordering::Acquire) {
                return true;
            }
            *self.waker.lock().unwrap() = Some(task.waker().clone());
            if self.synchronous_wake.swap(false, Ordering::AcqRel) {
                task.waker().wake_by_ref();
            }
            false
        }

        fn signal(&self) {
            self.ready.store(true, Ordering::Release);
            let waker = self.waker.lock().unwrap().take();
            waker.expect("I/O was polled").wake();
        }
    }

    struct OneWakerIo(Arc<OneRegistration>);

    impl AsyncRead for OneWakerIo {
        fn poll_read(
            self: Pin<&mut Self>,
            task: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if !self.0.poll_ready(task) {
                return Poll::Pending;
            }
            buffer.put_slice(b"r");
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for OneWakerIo {
        fn poll_write(
            self: Pin<&mut Self>,
            task: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if !self.0.poll_ready(task) {
                return Poll::Pending;
            }
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            if !self.0.poll_ready(task) {
                return Poll::Pending;
            }
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            task: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            self.poll_flush(task)
        }
    }

    fn with_duplex(
        test: impl FnOnce(Arc<OneRegistration>, DuplexReader<OneWakerIo>, DuplexWriter<OneWakerIo>),
    ) {
        let runtime = RuntimeBuilder::new().build().unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        let io = Arc::new(OneRegistration::default());
        let (reader, writer) = split_duplex(&cx, OneWakerIo(Arc::clone(&io))).unwrap();
        test(io, reader, writer);
    }

    fn read_once(reader: &mut DuplexReader<OneWakerIo>, waker: &Waker) -> bool {
        let mut bytes = [0; 1];
        let mut buffer = ReadBuf::new(&mut bytes);
        match Pin::new(reader).poll_read(&mut Context::from_waker(waker), &mut buffer) {
            Poll::Pending => false,
            Poll::Ready(Ok(())) => {
                assert_eq!(buffer.filled(), b"r");
                true
            }
            Poll::Ready(Err(error)) => panic!("unexpected read error: {error}"),
        }
    }

    #[test]
    fn both_pending_halves_are_woken_in_either_poll_order() {
        for read_first in [true, false] {
            with_duplex(|io, mut reader, mut writer| {
                let (reads, read_waker) = task();
                let (writes, write_waker) = task();
                if read_first {
                    assert!(!read_once(&mut reader, &read_waker));
                }
                assert!(
                    Pin::new(&mut writer)
                        .poll_write(&mut Context::from_waker(&write_waker), b"request")
                        .is_pending()
                );
                if !read_first {
                    assert!(!read_once(&mut reader, &read_waker));
                }
                assert_eq!((reads.count(), writes.count()), (0, 0));
                io.signal();
                // No blind re-poll: the I/O event must schedule BOTH tasks.
                assert_eq!((reads.count(), writes.count()), (1, 1));
                assert!(read_once(&mut reader, &read_waker));
                assert!(matches!(
                    Pin::new(&mut writer)
                        .poll_write(&mut Context::from_waker(&write_waker), b"request"),
                    Poll::Ready(Ok(7))
                ));
            });
        }
    }

    #[test]
    fn pending_flush_and_shutdown_do_not_replace_the_reader() {
        for shutdown in [true, false] {
            with_duplex(|io, mut reader, mut writer| {
                let (reads, read_waker) = task();
                let (writes, write_waker) = task();
                assert!(!read_once(&mut reader, &read_waker));
                let mut context = Context::from_waker(&write_waker);
                let result = if shutdown {
                    Pin::new(&mut writer).poll_shutdown(&mut context)
                } else {
                    Pin::new(&mut writer).poll_flush(&mut context)
                };
                assert!(result.is_pending());
                io.signal();
                assert_eq!((reads.count(), writes.count()), (1, 1));
            });
        }
    }

    #[test]
    fn repoll_replaces_only_its_own_task_registration() {
        with_duplex(|io, mut reader, mut writer| {
            let (old_reads, old_waker) = task();
            let (new_reads, new_waker) = task();
            let (writes, write_waker) = task();
            assert!(!read_once(&mut reader, &old_waker));
            assert!(
                Pin::new(&mut writer)
                    .poll_write(&mut Context::from_waker(&write_waker), b"request")
                    .is_pending()
            );
            assert!(!read_once(&mut reader, &new_waker));
            io.signal();
            assert_eq!(old_reads.count(), 0);
            assert_eq!((new_reads.count(), writes.count()), (1, 1));
        });
    }

    #[test]
    fn ready_progress_wakes_the_other_half_without_an_io_edge() {
        with_duplex(|io, mut reader, mut writer| {
            let (reads, read_waker) = task();
            let (writes, write_waker) = task();
            assert!(!read_once(&mut reader, &read_waker));
            assert!(
                Pin::new(&mut writer)
                    .poll_write(&mut Context::from_waker(&write_waker), b"request")
                    .is_pending()
            );
            // Model shared TLS state becoming ready while its writer polls.
            // There is deliberately no reactor wake here.
            io.ready.store(true, Ordering::Release);
            assert!(matches!(
                Pin::new(&mut writer)
                    .poll_write(&mut Context::from_waker(&write_waker), b"request"),
                Poll::Ready(Ok(7))
            ));
            assert_eq!((reads.count(), writes.count()), (1, 0));
            assert!(read_once(&mut reader, &read_waker));
            // Neither completed operation retains a task registration.
            io.signal();
            assert_eq!((reads.count(), writes.count()), (1, 0));
        });
    }

    #[test]
    fn synchronous_wake_inside_a_pending_poll_is_not_lost() {
        with_duplex(|io, mut reader, _writer| {
            let (reads, read_waker) = task();
            io.synchronous_wake.store(true, Ordering::Release);
            assert!(!read_once(&mut reader, &read_waker));
            assert_eq!(reads.count(), 1);
            assert!(!read_once(&mut reader, &read_waker));
            io.signal();
            assert_eq!(reads.count(), 2);
            assert!(read_once(&mut reader, &read_waker));
        });
    }

    #[test]
    fn one_task_waiting_on_both_halves_is_woken_once() {
        with_duplex(|io, mut reader, mut writer| {
            let (wakes, waker) = task();
            assert!(!read_once(&mut reader, &waker));
            assert!(
                Pin::new(&mut writer)
                    .poll_flush(&mut Context::from_waker(&waker))
                    .is_pending()
            );
            io.signal();
            assert_eq!(wakes.count(), 1);
        });
    }

    #[test]
    fn dropped_halves_release_tasks_and_do_not_keep_io_alive() {
        with_duplex(|io, mut reader, mut writer| {
            let (reads, read_waker) = task();
            let (writes, write_waker) = task();
            let read_task = Arc::downgrade(&reads);
            let write_task = Arc::downgrade(&writes);
            let stream = Arc::downgrade(&reader.io);
            assert!(!read_once(&mut reader, &read_waker));
            assert!(
                Pin::new(&mut writer)
                    .poll_flush(&mut Context::from_waker(&write_waker))
                    .is_pending()
            );
            drop((reads, read_waker, writes, write_waker));
            assert!(read_task.upgrade().is_some());
            assert!(write_task.upgrade().is_some());
            drop(reader);
            assert!(read_task.upgrade().is_none());
            assert!(write_task.upgrade().is_some());
            drop(writer);
            assert!(write_task.upgrade().is_none());
            assert!(stream.upgrade().is_none());
            // A late reactor wake holds only the empty notifier, not the I/O.
            io.signal();
        });
    }

    #[test]
    fn wake_callbacks_run_outside_the_registration_lock() {
        struct ReentrantWake(Arc<DuplexWake>, AtomicUsize);
        impl Wake for ReentrantWake {
            fn wake(self: Arc<Self>) {
                assert!(self.0.tasks.try_lock().is_ok());
                self.0.clear(READ_LANE);
                self.0.clear(WRITE_LANE);
                self.1.fetch_add(1, Ordering::SeqCst);
            }
        }
        let signal = Arc::new(DuplexWake::default());
        let callback = Arc::new(ReentrantWake(Arc::clone(&signal), AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&callback));
        signal.register(READ_LANE, &waker);
        signal.register(WRITE_LANE, &waker);
        signal.wake_tasks();
        assert_eq!(callback.1.load(Ordering::SeqCst), 1);
    }
}
