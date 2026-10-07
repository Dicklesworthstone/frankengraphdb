//! TLS terminates at the existing transport boundary. The foundation owns
//! certificates, handshake, record protection and hostname verification.
//! Every listener configured on a TLS-enabled Server requires TLS 1.3;
//! failed handshakes never fall back to plaintext or enter authentication.

use crate::shutdown::Waiter;
use asupersync::Cx;
use asupersync::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, ReadBuf};
use asupersync::net::TcpStream;
use asupersync::tls::{CertificateChain, PrivateKey, TlsAcceptor, TlsStream};
use core::future::{Future, poll_fn};
use core::pin::Pin;
use core::task::{Context, Poll};
use fgdb_protocol::transport::DuplexIo;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

const MAX_CERTIFICATE_BYTES: u64 = 1 << 20;
const TLS13: u16 = 0x0304;

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
    stream: TlsStream<CiphertextIo<TcpStream>>,
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
    let _ = stream.set_nodelay(true);
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
    use std::sync::atomic::AtomicUsize;
    use std::task::Waker;

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
