//! TLS terminates at the existing transport boundary. The foundation owns
//! certificates, handshake, record protection and hostname verification.
//! Every listener configured on a TLS-enabled Server requires TLS 1.3;
//! failed handshakes never fall back to plaintext or enter authentication.

use crate::shutdown::Waiter;
use asupersync::Cx;
use asupersync::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, ReadBuf};
use asupersync::net::TcpStream;
use asupersync::net::tcp::split::{OwnedReadHalf, OwnedWriteHalf};
use asupersync::tls::{CertificateChain, PrivateKey, TlsAcceptor, TlsStream};
use core::future::{Future, poll_fn};
use core::pin::Pin;
use core::task::{Context, Poll};
use fgdb_protocol::transport::DuplexIo;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

const MAX_CERTIFICATE_BYTES: u64 = 1 << 20;
const TLS13: u16 = 0x0304;
const PROTOCOL_ESTABLISHMENT_TIMEOUT: Duration = Duration::from_secs(30);

/// One absolute protocol-establishment allowance after the separately bounded
/// TLS handshake. Partial messages, failed database selections and blocked
/// response writes cannot renew it. Once the authenticated setup response is
/// completely flushed, the ordinary connection owns all later lifetimes.
#[derive(Default)]
pub(crate) struct Establishment {
    complete: AtomicBool,
}

impl Establishment {
    pub(crate) fn complete(&self) {
        self.complete.store(true, Ordering::Release);
    }

    pub(crate) async fn serve(
        &self,
        cx: &Cx,
        shutdown: &crate::Shutdown,
        connection: Pin<Box<dyn Future<Output = ()> + Send + '_>>,
    ) {
        let deadline = cx
            .now()
            .saturating_add_nanos(PROTOCOL_ESTABLISHMENT_TIMEOUT.as_nanos() as u64);
        let wake_at = cx
            .budget()
            .deadline
            .map_or(deadline, |limit| limit.min(deadline));
        let mut timer = Some(asupersync::time::sleep_until(wake_at));
        let mut waiter = Some(shutdown.waiter());
        // Register cancellation even for embedding hosts polling directly,
        // outside a scheduler task. No message can complete this receiver.
        let (_sender, mut receiver) = asupersync::channel::oneshot::channel::<()>();
        let mut cancelled = Some(Box::pin(receiver.recv(cx)));
        let mut connection = connection;
        poll_fn(|task| {
            if let Some(timer) = &mut timer {
                if waiter
                    .as_ref()
                    .is_some_and(|waiter| waiter.poll_triggered(task))
                    || cx.checkpoint().is_err()
                    || cancelled
                        .as_mut()
                        .is_some_and(|cancelled| cancelled.as_mut().poll(task).is_ready())
                {
                    return Poll::Ready(());
                }
                // The foundation timer must use this connection's clock and
                // driver, including when a host polls outside an ambient Cx.
                let _current = Cx::set_current(Some(cx.clone()));
                if Pin::new(timer).poll(task).is_ready() {
                    return Poll::Ready(());
                }
            }
            let result = connection.as_mut().poll(task);
            if self.complete.load(Ordering::Acquire) {
                // Retire the timer in the SAME poll that finishes setup. A
                // long-lived authenticated stream never retains this alarm
                // or inherits an idle timeout from its connection admission.
                timer = None;
                cancelled = None;
                waiter = None;
            }
            result
        })
        .await;
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Protocol {
    Fgp,
    Http,
    Bolt,
}

/// Validated TLS identity and protocol-specific foundation acceptors.
/// Keys and certificate contents never appear in Debug or error messages.
#[derive(Clone)]
pub struct TlsConfig {
    fgp: TlsAcceptor,
    http: TlsAcceptor,
    bolt: TlsAcceptor,
}

impl core::fmt::Debug for TlsConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TlsConfig")
            .field("protocol", &"TLS 1.3")
            .finish_non_exhaustive()
    }
}

/// A bounded configuration failure with no certificate or secret bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TlsConfigError(&'static str);

impl core::fmt::Display for TlsConfigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}
impl core::error::Error for TlsConfigError {}

impl TlsConfig {
    /// Read a PEM certificate chain and an owner-only PEM private key. Both
    /// reads use the runtime filesystem, are bounded, and validate the opened
    /// regular file. Certificate validity/key matching is checked at startup.
    pub async fn from_pem_files(
        cx: &Cx,
        certificate_chain: &Path,
        private_key: &Path,
    ) -> Result<Self, TlsConfigError> {
        let cert = read_certificate(cx, certificate_chain).await?;
        let key = crate::keys::read_owner_only(cx, private_key)
            .await
            .map_err(|_| TlsConfigError("cannot read an owner-only TLS private key"))?;
        Self::from_pem(cx, &cert, key.as_bytes())
    }

    /// Validate an in-memory identity. The runtime context admits the shared
    /// acceptor allocations; the host is responsible for protecting key bytes.
    pub fn from_pem(
        cx: &Cx,
        certificate_chain: &[u8],
        private_key: &[u8],
    ) -> Result<Self, TlsConfigError> {
        cx.checkpoint()
            .map_err(|_| TlsConfigError("TLS configuration cancelled"))?;
        if certificate_chain.len() as u64 > MAX_CERTIFICATE_BYTES || private_key.len() > 65_536 {
            return Err(TlsConfigError("TLS identity exceeds its size limit"));
        }
        let chain = CertificateChain::from_pem(certificate_chain)
            .map_err(|_| TlsConfigError("invalid TLS certificate chain"))?;
        let key = PrivateKey::from_pem(private_key)
            .map_err(|_| TlsConfigError("invalid TLS private key"))?;
        let acceptor = |alpn: Option<&[u8]>| {
            let mut builder = TlsAcceptor::builder(chain.clone(), key.clone())
                .min_protocol_version(TLS13.into())
                .max_protocol_version(TLS13.into())
                .disable_early_data()
                .handshake_timeout(Duration::from_secs(10));
            if let Some(alpn) = alpn {
                builder = builder.alpn_protocols_required(vec![alpn.to_vec()]);
            }
            builder.build().map_err(|_| {
                TlsConfigError("TLS certificate, key, or protocol configuration refused")
            })
        };
        // Bolt's official drivers do not all advertise ALPN. Its existing
        // magic/version negotiation still runs inside the encrypted channel.
        Ok(Self {
            fgp: acceptor(Some(b"fgp/1"))?,
            http: acceptor(Some(b"http/1.1"))?,
            bolt: acceptor(None)?,
        })
    }

    fn acceptor(&self, protocol: Protocol) -> &TlsAcceptor {
        match protocol {
            Protocol::Fgp => &self.fgp,
            Protocol::Http => &self.http,
            Protocol::Bolt => &self.bolt,
        }
    }
}

async fn read_certificate(cx: &Cx, path: &Path) -> Result<Vec<u8>, TlsConfigError> {
    cx.checkpoint()
        .map_err(|_| TlsConfigError("TLS certificate read cancelled"))?;
    let unreadable = |_| TlsConfigError("cannot read TLS certificate file");
    let named = asupersync::fs::metadata(path).await.map_err(unreadable)?;
    if !named.is_file() {
        return Err(TlsConfigError("TLS certificate must be a regular file"));
    }
    let file = asupersync::fs::File::open(path).await.map_err(unreadable)?;
    if !file.metadata().await.map_err(unreadable)?.is_file() {
        return Err(TlsConfigError("TLS certificate must be a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_CERTIFICATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(unreadable)?;
    if bytes.len() as u64 > MAX_CERTIFICATE_BYTES {
        return Err(TlsConfigError("TLS certificate exceeds its size limit"));
    }
    cx.checkpoint()
        .map_err(|_| TlsConfigError("TLS certificate read cancelled"))?;
    Ok(bytes)
}

/// Keep the foundation's per-direction readiness registrations even though
/// TLS and the protocol drivers require one duplex object. A plain TcpStream
/// arms only its most recent direction: sharing its task waker is not enough
/// to preserve READABLE while a write is waiting for WRITABLE, or vice versa.
/// Do not reunite these halves or implement another reactor in this layer.
struct TcpIo {
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
}

impl AsyncRead for TcpIo {
    fn poll_read(
        self: Pin<&mut Self>,
        task: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().reader).poll_read(task, buffer)
    }
}

impl AsyncWrite for TcpIo {
    fn poll_write(
        self: Pin<&mut Self>,
        task: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().writer).poll_write(task, bytes)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        task: &mut Context<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().writer).poll_write_vectored(task, buffers)
    }

    fn is_write_vectored(&self) -> bool {
        self.writer.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_flush(task)
    }

    fn poll_shutdown(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_shutdown(task)
    }
}

// One application-level TLS poll can otherwise flush many ciphertext records
// without returning to FGP/HTTP/Bolt's fresh output authorizer. Permit exactly
// one physical write OR flush per outer poll. A read poll gets no write
// authority, even if a future foundation revision tries to flush on read.
const SPENT: u8 = 0;
const ONE_WRITE: u8 = 1;
const READ_ONLY: u8 = 2;

struct CiphertextIo<T> {
    inner: T,
    allowance: Arc<AtomicU8>,
}

impl<T> CiphertextIo<T> {
    fn admit(&self, task: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.allowance.swap(SPENT, Ordering::AcqRel) {
            ONE_WRITE => Poll::Ready(Ok(())),
            READ_ONLY => {
                self.allowance.store(READ_ONLY, Ordering::Release);
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "a TLS read cannot flush protected output",
                )))
            }
            _ => {
                task.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for CiphertextIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        task: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(task, buffer)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for CiphertextIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        task: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match this.admit(task) {
            Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_write(task, bytes),
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.admit(task) {
            Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_flush(task),
            other => other,
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.admit(task) {
            Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_shutdown(task),
            other => other,
        }
    }
}

struct TlsIo {
    stream: TlsStream<CiphertextIo<TcpIo>>,
    allowance: Arc<AtomicU8>,
}

impl AsyncRead for TlsIo {
    fn poll_read(
        self: Pin<&mut Self>,
        task: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.allowance.store(READ_ONLY, Ordering::Release);
        Pin::new(&mut this.stream).poll_read(task, buffer)
    }
}

impl AsyncWrite for TlsIo {
    fn poll_write(
        self: Pin<&mut Self>,
        task: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.allowance.store(ONE_WRITE, Ordering::Release);
        Pin::new(&mut this.stream).poll_write(task, bytes)
    }

    fn poll_flush(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.allowance.store(ONE_WRITE, Ordering::Release);
        Pin::new(&mut this.stream).poll_flush(task)
    }

    fn poll_shutdown(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.allowance.store(ONE_WRITE, Ordering::Release);
        Pin::new(&mut this.stream).poll_shutdown(task)
    }
}

pub(crate) async fn establish(
    cx: &Cx,
    waiter: &Waiter,
    stream: TcpStream,
    tls: Option<&TlsConfig>,
    protocol: Protocol,
) -> Option<Box<dyn DuplexIo>> {
    cx.checkpoint().ok()?;
    let _ = stream.set_nodelay(true);
    // Split before either plaintext dispatch or TLS: the encryption state
    // stays whole, while its underlying read/write interests stay independent.
    let (reader, writer) = stream.into_split();
    let stream = TcpIo { reader, writer };
    let Some(tls) = tls else {
        return Some(Box::new(stream));
    };
    let allowance = Arc::new(AtomicU8::new(ONE_WRITE));
    let io = CiphertextIo {
        inner: stream,
        allowance: Arc::clone(&allowance),
    };
    let handshake = tls.acceptor(protocol).accept(io);
    let mut handshake = core::pin::pin!(handshake);
    let stream = poll_fn(|task| {
        if waiter.poll_triggered(task) || cx.checkpoint().is_err() {
            return Poll::Ready(None);
        }
        allowance.store(ONE_WRITE, Ordering::Release);
        handshake.as_mut().poll(task).map(Result::ok)
    })
    .await?;
    Some(Box::new(TlsIo { stream, allowance }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::Budget;
    use asupersync::runtime::RuntimeBuilder;
    use asupersync::time::{TimerDriverHandle, VirtualClock};
    use asupersync::types::{CancelKind, Time};
    use fgdb_protocol::body::{Auth, AuthOk, Body, Credential, Hello, Ping, Ready, SelectDatabase};
    use fgdb_protocol::{Binding, Decoder, Frame, FrameKind, FrameLimits, StreamId};
    use std::collections::VecDeque;
    use std::io::{Read as _, Write as _};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::{self, Receiver, SyncSender};
    use std::task::{Wake, Waker};
    use std::time::Instant;

    const SOCKET_WAIT: Duration = Duration::from_secs(5);

    #[derive(Default)]
    struct SetupWire {
        inbound: Mutex<VecDeque<u8>>,
        outbound: Mutex<Vec<u8>>,
        reader: Mutex<Option<Waker>>,
        block_write: AtomicBool,
        block_flush: AtomicBool,
        dropped: AtomicBool,
    }

    impl SetupWire {
        fn feed(&self, bytes: &[u8]) {
            self.inbound.lock().unwrap().extend(bytes);
            let waker = self.reader.lock().unwrap().take();
            if let Some(waker) = waker {
                waker.wake();
            }
        }

        fn take_output(&self) -> Vec<u8> {
            std::mem::take(&mut *self.outbound.lock().unwrap())
        }
    }

    struct SetupIo(Arc<SetupWire>);

    impl Drop for SetupIo {
        fn drop(&mut self) {
            self.0.dropped.store(true, Ordering::Release);
        }
    }

    impl AsyncRead for SetupIo {
        fn poll_read(
            self: Pin<&mut Self>,
            task: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            *self.0.reader.lock().unwrap() = Some(task.waker().clone());
            let mut inbound = self.0.inbound.lock().unwrap();
            if inbound.is_empty() {
                return Poll::Pending;
            }
            let count = inbound.len().min(buffer.remaining());
            let bytes: Vec<_> = inbound.drain(..count).collect();
            buffer.put_slice(&bytes);
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for SetupIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.0.block_write.load(Ordering::Acquire) {
                return Poll::Pending;
            }
            self.0.outbound.lock().unwrap().extend_from_slice(bytes);
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            if self.0.block_flush.load(Ordering::Acquire) {
                Poll::Pending
            } else {
                Poll::Ready(Ok(()))
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn setup_connection<'a>(
        protocol: Protocol,
        cx: &'a Cx,
        server: &'a crate::Server,
        wire: &Arc<SetupWire>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        let stream = Box::new(SetupIo(Arc::clone(wire)));
        match protocol {
            Protocol::Fgp => crate::connection::run(cx, server, stream),
            Protocol::Bolt => Box::pin(crate::bolt::run(cx, server, stream, None)),
            Protocol::Http => unreachable!("HTTP owns its existing idle timeout"),
        }
    }

    fn fgp_message(kind: FrameKind, request: u64, binding: Binding, body: impl Body) -> Vec<u8> {
        let limits = FrameLimits::new(4096).unwrap();
        Frame::new(
            kind,
            request,
            StreamId::CONTROL,
            binding,
            body.encode().unwrap(),
            limits,
        )
        .unwrap()
        .encode(limits)
        .unwrap()
    }

    fn fgp_response(wire: &SetupWire, kind: FrameKind) -> Frame {
        let bytes = wire.take_output();
        let decoded = Decoder::new(FrameLimits::new(4096).unwrap())
            .decode(&bytes, |_| Ok(()))
            .unwrap();
        assert_eq!(decoded.consumed, bytes.len());
        let frame = decoded.frame.expect("one complete setup response");
        assert_eq!(frame.header().kind(), kind);
        frame
    }

    fn setup_request(protocol: Protocol) -> Vec<u8> {
        match protocol {
            Protocol::Fgp => fgp_message(
                FrameKind::Hello,
                1,
                Binding::Transport,
                Hello {
                    min_version: 1,
                    max_version: 1,
                    client_nonce: [0x73; 32],
                    max_frame_len: 4096,
                },
            ),
            Protocol::Bolt => {
                let mut bytes = fgdb_bolt::message::MAGIC.to_vec();
                bytes.extend_from_slice(&[0, 0, 0, 5]);
                bytes.extend_from_slice(&[0; 12]);
                bytes
            }
            Protocol::Http => unreachable!(),
        }
    }

    fn with_setup_clock(test: impl FnOnce(&Cx, &VirtualClock, &TimerDriverHandle)) {
        let clock = Arc::new(VirtualClock::new());
        let timer = TimerDriverHandle::with_virtual_clock(Arc::clone(&clock));
        let runtime = RuntimeBuilder::current_thread()
            .with_timer_driver(timer.clone())
            .build()
            .unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        // Keep the runtime driver in this root poll while the test advances
        // its clock explicitly. No wall sleep or scheduler race sets a limit.
        runtime.block_on(async { test(&cx, &clock, &timer) });
    }

    #[test]
    fn empty_and_fragmented_protocol_setups_expire_at_one_absolute_deadline() {
        for protocol in [Protocol::Fgp, Protocol::Bolt] {
            for fragmented in [false, true] {
                with_setup_clock(|cx, clock, timer| {
                    let server = crate::Server::new(cx, crate::ServerLimits::default()).unwrap();
                    let wire = Arc::new(SetupWire::default());
                    let request = setup_request(protocol);
                    if fragmented {
                        wire.feed(&request[..1]);
                    }
                    let mut connection = setup_connection(protocol, cx, &server, &wire);
                    let wakes = Arc::new(crate::recovery::tests::WakeCounter::default());
                    let waker = Waker::from(Arc::clone(&wakes));
                    let mut task = Context::from_waker(&waker);
                    assert!(connection.as_mut().poll(&mut task).is_pending());
                    assert_eq!(timer.pending_count(), 1);
                    clock.advance_to(Time::from_secs(29));
                    if fragmented {
                        wire.feed(&request[1..2]);
                    }
                    assert!(connection.as_mut().poll(&mut task).is_pending());
                    let before = wakes.0.load(Ordering::Relaxed);
                    clock.advance_to(Time::from_secs(30));
                    assert_eq!(timer.process_timers(), 1);
                    assert!(wakes.0.load(Ordering::Relaxed) > before);
                    assert!(connection.as_mut().poll(&mut task).is_ready());
                    drop(connection);
                    assert!(wire.dropped.load(Ordering::Acquire));
                    assert!(wire.take_output().is_empty());
                    assert_eq!(timer.pending_count(), 0);
                });
            }
        }
    }

    #[test]
    fn protocol_setup_deadline_covers_blocked_writes_and_flushes() {
        for protocol in [Protocol::Fgp, Protocol::Bolt] {
            for flush in [false, true] {
                with_setup_clock(|cx, clock, timer| {
                    let server = crate::Server::new(cx, crate::ServerLimits::default()).unwrap();
                    let wire = Arc::new(SetupWire::default());
                    wire.block_write.store(!flush, Ordering::Release);
                    wire.block_flush.store(flush, Ordering::Release);
                    wire.feed(&setup_request(protocol));
                    let mut connection = setup_connection(protocol, cx, &server, &wire);
                    let mut task = Context::from_waker(Waker::noop());
                    assert!(connection.as_mut().poll(&mut task).is_pending());
                    assert_eq!(wire.outbound.lock().unwrap().is_empty(), !flush);
                    clock.advance_to(Time::from_secs(30));
                    assert_eq!(timer.process_timers(), 1);
                    assert!(connection.as_mut().poll(&mut task).is_ready());
                    drop(connection);
                    assert!(wire.dropped.load(Ordering::Acquire));
                    assert_eq!(timer.pending_count(), 0);
                });
            }
        }
    }

    #[test]
    fn pending_protocol_setup_wakes_on_drain_and_cancellation() {
        for protocol in [Protocol::Fgp, Protocol::Bolt] {
            for cancel in [false, true] {
                with_setup_clock(|cx, _, timer| {
                    let server = crate::Server::new(cx, crate::ServerLimits::default()).unwrap();
                    let wire = Arc::new(SetupWire::default());
                    let mut connection = setup_connection(protocol, cx, &server, &wire);
                    let wakes = Arc::new(crate::recovery::tests::WakeCounter::default());
                    let waker = Waker::from(Arc::clone(&wakes));
                    let mut task = Context::from_waker(&waker);
                    assert!(connection.as_mut().poll(&mut task).is_pending());
                    let before = wakes.0.load(Ordering::Relaxed);
                    if cancel {
                        cx.cancel_fast(CancelKind::User);
                    } else {
                        server.shutdown().trigger();
                    }
                    assert!(wakes.0.load(Ordering::Relaxed) > before);
                    assert!(connection.as_mut().poll(&mut task).is_ready());
                    drop(connection);
                    assert!(wire.dropped.load(Ordering::Acquire));
                    assert_eq!(timer.pending_count(), 0);
                    assert_eq!(cx.now(), Time::ZERO);
                });
            }
        }
    }

    fn bolt_message(tag: u8, fields: Vec<fgdb_bolt::packstream::Value>) -> Vec<u8> {
        let mut message = Vec::new();
        fgdb_bolt::packstream::encode(
            &fgdb_bolt::packstream::Value::Struct { tag, fields },
            &mut message,
        );
        let mut framed = Vec::new();
        fgdb_bolt::message::frame(&message, &mut framed);
        framed
    }

    fn bolt_success(wire: &SetupWire) {
        let mut decoder = fgdb_bolt::message::Dechunker::new(4096);
        decoder.push(&wire.take_output());
        let response = decoder.next_message().unwrap().unwrap();
        assert!(matches!(
            fgdb_bolt::packstream::decode(&response).unwrap(),
            fgdb_bolt::packstream::Value::Struct { tag: 0x70, .. }
        ));
        assert!(decoder.next_message().unwrap().is_none());
    }

    #[test]
    fn authenticated_setup_retires_its_deadline_only_after_the_final_flush() {
        use fgdb_bolt::packstream::Value;

        for (protocol, name) in [(Protocol::Fgp, "fgp"), (Protocol::Bolt, "bolt")] {
            for block_final_flush in [false, true] {
                let clock = Arc::new(VirtualClock::new());
                let timer = TimerDriverHandle::with_virtual_clock(Arc::clone(&clock));
                let runtime = RuntimeBuilder::current_thread()
                    .with_timer_driver(timer.clone())
                    .build()
                    .unwrap();
                let cx = runtime.request_cx_with_budget(Budget::INFINITE);
                runtime.block_on(async {
                    let (server, token, _) = crate::recovery::tests::served(
                        &cx,
                        &format!("setup-deadline-{name}-{block_final_flush}"),
                    )
                    .await;
                    let wire = Arc::new(SetupWire::default());
                    let mut connection = setup_connection(protocol, &cx, &server, &wire);
                    let mut task = Context::from_waker(Waker::noop());
                    wire.feed(&setup_request(protocol));
                    assert!(connection.as_mut().poll(&mut task).is_pending());
                    let ready = match protocol {
                        Protocol::Fgp => {
                            fgp_response(&wire, FrameKind::HelloAck);
                            wire.feed(&fgp_message(
                                FrameKind::Auth,
                                2,
                                Binding::Transport,
                                Auth {
                                    credential: Credential::WardenCapability(token.encode()),
                                },
                            ));
                            assert!(connection.as_mut().poll(&mut task).is_pending());
                            let authenticated = fgp_response(&wire, FrameKind::AuthOk);
                            let session = AuthOk::decode(authenticated.payload()).unwrap().session;
                            // Authentication alone cannot retire FGP setup:
                            // this connection still has no selected database.
                            assert_eq!(timer.pending_count(), 1);
                            wire.block_flush.store(block_final_flush, Ordering::Release);
                            wire.feed(&fgp_message(
                                FrameKind::SelectDatabase,
                                3,
                                Binding::Session(session),
                                SelectDatabase {
                                    name: "test".into(),
                                },
                            ));
                            assert!(connection.as_mut().poll(&mut task).is_pending());
                            let frame = fgp_response(&wire, FrameKind::Ready);
                            Some(Binding::Ready(
                                Ready::decode(frame.payload()).unwrap().binding(session),
                            ))
                        }
                        Protocol::Bolt => {
                            assert_eq!(wire.take_output(), [0, 0, 0, 5]);
                            wire.block_flush.store(block_final_flush, Ordering::Release);
                            wire.feed(&bolt_message(
                                0x01,
                                vec![Value::Map(vec![
                                    ("scheme".into(), Value::string("bearer")),
                                    (
                                        "credentials".into(),
                                        Value::string(fgdb_protocol::json::hex(&token.encode())),
                                    ),
                                ])],
                            ));
                            assert!(connection.as_mut().poll(&mut task).is_pending());
                            bolt_success(&wire);
                            None
                        }
                        Protocol::Http => unreachable!(),
                    };
                    assert_eq!(timer.pending_count(), usize::from(block_final_flush));
                    clock.advance_to(Time::from_secs(3_600));
                    assert_eq!(timer.process_timers(), usize::from(block_final_flush));
                    if block_final_flush {
                        assert!(connection.as_mut().poll(&mut task).is_ready());
                    } else {
                        assert!(connection.as_mut().poll(&mut task).is_pending());
                        assert!(!wire.dropped.load(Ordering::Acquire));
                        if let Some(binding) = ready {
                            wire.feed(&fgp_message(
                                FrameKind::Ping,
                                4,
                                binding,
                                Ping { nonce: 73 },
                            ));
                            assert!(connection.as_mut().poll(&mut task).is_pending());
                            let response = fgp_response(&wire, FrameKind::Pong);
                            assert_eq!(Ping::decode(response.payload()).unwrap().nonce, 73);
                        } else {
                            wire.feed(&bolt_message(0x0f, Vec::new()));
                            assert!(connection.as_mut().poll(&mut task).is_pending());
                            bolt_success(&wire);
                        }
                    }
                    drop(connection);
                    assert!(wire.dropped.load(Ordering::Acquire));
                    assert_eq!(timer.pending_count(), 0);
                    server.finish_shutdown(&cx).await;
                });
            }
        }
    }

    /// Establish through the real listener boundary, not a test-only adapter.
    /// Polling the result outside block_on exercises the foundation's fallback
    /// reactor; no test-owned background thread or blind repoll drives I/O.
    fn plain_connection(protocol: Protocol) -> (Box<dyn DuplexIo>, std::net::TcpStream) {
        let runtime = fgdb::runtime_builder().build().unwrap();
        let cx = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
        runtime.block_on(async {
            let listener = asupersync::net::TcpListener::bind("127.0.0.1:0")
                .await
                .unwrap();
            let peer =
                std::net::TcpStream::connect_timeout(&listener.local_addr().unwrap(), SOCKET_WAIT)
                    .unwrap();
            peer.set_read_timeout(Some(SOCKET_WAIT)).unwrap();
            peer.set_write_timeout(Some(SOCKET_WAIT)).unwrap();
            let (stream, _) = listener.accept().await.unwrap();
            let shutdown = crate::Shutdown::new();
            let waiter = shutdown.waiter();
            let io = establish(&cx, &waiter, stream, None, protocol)
                .await
                .unwrap();
            (io, peer)
        })
    }

    struct WakeSignal(SyncSender<()>);

    impl Wake for WakeSignal {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            // Coalesce notifications without blocking the reactor callback.
            let _ = self.0.try_send(());
        }
    }

    fn wake_signal() -> (Waker, Receiver<()>) {
        let (sender, receiver) = mpsc::sync_channel(1);
        (Waker::from(Arc::new(WakeSignal(sender))), receiver)
    }

    fn read_byte(io: &mut dyn DuplexIo, waker: &Waker, wakes: &Receiver<()>) -> u8 {
        let deadline = Instant::now() + SOCKET_WAIT;
        let mut byte = [0];
        let mut buffer = ReadBuf::new(&mut byte);
        loop {
            assert!(
                Instant::now() < deadline,
                "socket read exceeded its deadline"
            );
            match Pin::new(&mut *io).poll_read(&mut Context::from_waker(waker), &mut buffer) {
                Poll::Ready(Ok(())) => {
                    assert_eq!(buffer.filled().len(), 1, "unexpected EOF");
                    return buffer.filled()[0];
                }
                Poll::Ready(Err(error)) => panic!("socket read failed: {error}"),
                Poll::Pending => wakes
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .expect("read readiness must wake the pending task"),
            }
        }
    }

    #[test]
    fn every_listener_preserves_read_readiness_while_tcp_output_is_backpressured() {
        for protocol in [Protocol::Fgp, Protocol::Http, Protocol::Bolt] {
            let (mut io, mut peer) = plain_connection(protocol);
            let (read_waker, read_wakes) = wake_signal();
            let mut byte = [0];
            let mut buffer = ReadBuf::new(&mut byte);
            assert!(
                Pin::new(io.as_mut())
                    .poll_read(&mut Context::from_waker(&read_waker), &mut buffer)
                    .is_pending()
            );

            // The peer deliberately never drains output. Bound the attempt to
            // 64 MiB, retain only 16 KiB, and stop at the FIRST pending write.
            let bytes = [0; 16 * 1024];
            let mut blocked = false;
            for _ in 0..4096 {
                match Pin::new(io.as_mut())
                    .poll_write(&mut Context::from_waker(Waker::noop()), &bytes)
                {
                    Poll::Ready(Ok(count)) => assert!(count > 0),
                    Poll::Ready(Err(error)) => panic!("socket write failed: {error}"),
                    Poll::Pending => {
                        blocked = true;
                        break;
                    }
                }
            }
            assert!(blocked, "socket did not reach bounded backpressure");

            // Do not discard a notification: even a spurious wake schedules
            // the reader and may consume its one-shot reactor registration.
            // A raw unsplit socket would instead retain only the writer's
            // noop waker and writable interest after the pending write above.
            peer.write_all(b"c").unwrap();
            read_wakes
                .recv_timeout(SOCKET_WAIT)
                .expect("a pending writer must not erase the reader's readiness interest");
            assert_eq!(read_byte(io.as_mut(), &read_waker, &read_wakes), b'c');
        }
    }

    #[test]
    fn every_listener_keeps_inbound_tcp_open_after_write_shutdown() {
        for protocol in [Protocol::Fgp, Protocol::Http, Protocol::Bolt] {
            let (mut io, mut peer) = plain_connection(protocol);
            assert!(matches!(
                Pin::new(io.as_mut()).poll_shutdown(&mut Context::from_waker(Waker::noop())),
                Poll::Ready(Ok(()))
            ));
            let mut byte = [0];
            assert_eq!(peer.read(&mut byte).unwrap(), 0);
            peer.write_all(b"r").unwrap();
            let (waker, wakes) = wake_signal();
            assert_eq!(read_byte(io.as_mut(), &waker, &wakes), b'r');
        }
    }

    struct Socket(Arc<AtomicUsize>);

    impl AsyncWrite for Socket {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(Ok(bytes.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn tls_ciphertext_requires_a_fresh_outer_write_poll_and_reads_cannot_flush() {
        let calls = Arc::new(AtomicUsize::new(0));
        let allowance = Arc::new(AtomicU8::new(ONE_WRITE));
        let mut io = CiphertextIo {
            inner: Socket(Arc::clone(&calls)),
            allowance: Arc::clone(&allowance),
        };
        let mut task = Context::from_waker(Waker::noop());
        assert!(matches!(
            Pin::new(&mut io).poll_write(&mut task, b"record one"),
            Poll::Ready(Ok(10))
        ));
        assert!(
            Pin::new(&mut io)
                .poll_write(&mut task, b"record two")
                .is_pending()
        );
        assert!(Pin::new(&mut io).poll_flush(&mut task).is_pending());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // The TLS wrapper may renew this only on a write/flush/shutdown poll
        // reached through the transport's existing fresh output authorizer.
        allowance.store(ONE_WRITE, Ordering::Release);
        assert!(matches!(
            Pin::new(&mut io).poll_flush(&mut task),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        allowance.store(READ_ONLY, Ordering::Release);
        for _ in 0..3 {
            assert!(
                matches!(Pin::new(&mut io).poll_write(&mut task, b"pending protected record"), Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::PermissionDenied)
            );
            assert!(
                matches!(Pin::new(&mut io).poll_flush(&mut task), Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::PermissionDenied)
            );
            assert!(
                matches!(Pin::new(&mut io).poll_shutdown(&mut task), Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::PermissionDenied)
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
