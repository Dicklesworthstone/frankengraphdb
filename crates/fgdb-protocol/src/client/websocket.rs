//! The native FGP client over the server's bounded WebSocket upgrade profile.
//!
//! The foundation owns the RFC 6455 nonce/accept transcript and frame codec.
//! This module supplies the endpoint policy and completes the upgrade before
//! handing any FGP bytes or credentials to the existing client state machine.

use super::{CLIENT_MAX_FRAME, Client, ClientError};
use crate::transport::{DuplexIo, TransportError};
use crate::websocket::WebSocketIo;
use asupersync::Cx;
use asupersync::io::{AsyncReadExt as _, AsyncWriteExt as _};
use asupersync::net::TcpStream;
use asupersync::net::websocket::{ClientHandshake, HttpResponse};
use core::future::{Future, poll_fn};
use core::pin::Pin;
use core::task::Poll;
use std::collections::BTreeSet;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

const PATH: &str = "/v1/fgp";
const SUBPROTOCOL: &str = "fgp.v1";
const MAX_HEADERS: usize = 16 << 10;
const MAX_HEADER_COUNT: usize = 128;
const ESTABLISHMENT_TIMEOUT: Duration = Duration::from_secs(30);

/// The HTTP authority for the native FGP WebSocket endpoint, /v1/fgp.
///
/// A target contains a DNS name, IPv4 address or bracketed IPv6 address, with
/// an optional numeric port. It contains no credentials, path or query. The
/// connection address and verified TLS identity are supplied independently:
/// this value only selects the HTTP Host, and never authorizes a fallback or
/// a redirect. Native clients send no Origin header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebSocketTarget {
    authority: String,
}

impl WebSocketTarget {
    /// Validate the complete authority before opening a socket or reading a
    /// credential. Unicode hostnames must first be represented as DNS ASCII.
    pub fn new(authority: String) -> Result<Self, ClientError> {
        if !valid_authority(&authority) {
            return Err(ClientError::Protocol("invalid WebSocket authority"));
        }
        Ok(Self { authority })
    }

    #[must_use]
    pub fn authority(&self) -> &str {
        &self.authority
    }
}

fn valid_port(port: &str) -> bool {
    !port.is_empty()
        && port.len() <= 5
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|port| port != 0)
}

fn valid_authority(authority: &str) -> bool {
    // A DNS name is at most 253 ASCII bytes, plus an optional :65535. This
    // also bounds every later request allocation independently of the caller.
    if authority.is_empty() || authority.len() > 259 {
        return false;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let Some(end) = rest.find(']') else {
            return false;
        };
        if rest[..end].parse::<Ipv6Addr>().is_err() {
            return false;
        }
        let suffix = &rest[end + 1..];
        return suffix.is_empty() || suffix.strip_prefix(':').is_some_and(valid_port);
    }
    let (host, port) = authority
        .split_once(':')
        .map_or((authority, None), |(host, port)| (host, Some(port)));
    if port.is_some_and(|port| !valid_port(port)) {
        return false;
    }
    let host = host.strip_suffix('.').unwrap_or(host);
    if host
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return host.parse::<Ipv4Addr>().is_ok();
    }
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.as_bytes()[0].is_ascii_alphanumeric()
                && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

impl Client {
    /// Open TCP, upgrade to WebSocket, then negotiate FGP and authenticate.
    /// The HTTP Host comes from target; addr is the actual TCP endpoint.
    /// One context-clock deadline bounds the entire thirty-second setup.
    /// No redirect, retry, plaintext fallback or automatic replay is attempted.
    pub async fn connect_websocket(
        cx: &Cx,
        target: &WebSocketTarget,
        addr: SocketAddr,
        credential: Vec<u8>,
    ) -> Result<Self, ClientError> {
        establish(cx, async {
            let stream = TcpStream::connect(addr)
                .await
                .map_err(|error| ClientError::Io(error.kind()))?;
            let _ = stream.set_nodelay(true);
            connect_upgraded(cx, target, stream, credential).await
        })
        .await
    }

    /// Upgrade an established transport, then negotiate FGP and authenticate.
    ///
    /// A TLS caller must complete certificate and hostname verification and
    /// negotiate HTTP/1.1 before passing its stream here. This method does not
    /// add TLS or infer transport security from the HTTP authority. No FGP
    /// HELLO or credential is written until the complete HTTP response passes
    /// validation, including the accept transcript and exact fgp.v1 choice.
    /// The thirty-second context-clock deadline spans upgrade through AUTH_OK.
    pub async fn connect_websocket_stream(
        cx: &Cx,
        target: &WebSocketTarget,
        stream: impl DuplexIo + 'static,
        credential: Vec<u8>,
    ) -> Result<Self, ClientError> {
        establish(cx, connect_upgraded(cx, target, stream, credential)).await
    }
}

async fn establish<T>(
    cx: &Cx,
    operation: impl Future<Output = Result<T, ClientError>>,
) -> Result<T, ClientError> {
    let deadline = cx
        .now()
        .saturating_add_nanos(ESTABLISHMENT_TIMEOUT.as_nanos() as u64);
    let wake_at = cx
        .budget()
        .deadline
        .map_or(deadline, |limit| limit.min(deadline));
    let mut timer = asupersync::time::sleep_until(wake_at);
    // Register a cancellation waker even if an embedding host polls outside
    // a scheduler task and the lower stream is permanently backpressured.
    let (_sender, mut receiver) = asupersync::channel::oneshot::channel::<()>();
    let mut cancelled = core::pin::pin!(receiver.recv(cx));
    let mut operation = core::pin::pin!(operation);
    poll_fn(|task| {
        if cx.checkpoint().is_err() || cancelled.as_mut().poll(task).is_ready() {
            return Poll::Ready(Err(ClientError::Transport(TransportError::ContextStopped)));
        }
        // The timer uses this caller's clock/driver, including virtual time;
        // the guard never crosses an await or becomes part of the connection.
        let _current = Cx::set_current(Some(cx.clone()));
        if Pin::new(&mut timer).poll(task).is_ready() {
            return Poll::Ready(Err(ClientError::Io(io::ErrorKind::TimedOut)));
        }
        operation.as_mut().poll(task)
    })
    .await
}

async fn connect_upgraded(
    cx: &Cx,
    target: &WebSocketTarget,
    mut stream: impl DuplexIo + 'static,
    credential: Vec<u8>,
) -> Result<Client, ClientError> {
    // This URL supplies HTTP metadata only. The established stream owns the
    // actual security posture; no foundation URL dialer or fallback is used.
    let handshake =
        ClientHandshake::new(&format!("ws://{}{PATH}", target.authority()), cx.entropy())
            .map_err(|_| invalid_upgrade())?
            .protocol(SUBPROTOCOL);
    stream
        .write_all(&handshake.request_bytes())
        .await
        .map_err(|error| ClientError::Io(error.kind()))?;
    stream
        .flush()
        .await
        .map_err(|error| ClientError::Io(error.kind()))?;
    let mut input = Vec::new();
    let mut buffer = [0_u8; 1024];
    let header_end = loop {
        if let Some(end) = input.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break end + 4;
        }
        let remaining = MAX_HEADERS.saturating_sub(input.len());
        if remaining == 0 {
            return Err(invalid_upgrade());
        }
        cx.checkpoint()
            .map_err(|_| ClientError::Transport(TransportError::ContextStopped))?;
        let count = stream
            .read(&mut buffer[..remaining.min(1024)])
            .await
            .map_err(|error| ClientError::Io(error.kind()))?;
        if count == 0 {
            return Err(ClientError::Closed);
        }
        input.extend_from_slice(&buffer[..count]);
    };
    validate_response(&handshake, &input[..header_end])?;
    let prefix = input.split_off(header_end);
    let stream = WebSocketIo::client(cx, stream, CLIENT_MAX_FRAME as usize, prefix)
        .map_err(|error| ClientError::Io(error.kind()))?;
    Client::connect_stream(cx, stream, credential).await
}

fn invalid_upgrade() -> ClientError {
    ClientError::Protocol("invalid WebSocket upgrade response")
}

fn token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// Validate the strict HTTP envelope before the foundation's general parser
/// can merge repeated headers or overlook a malformed line. The transcript
/// itself remains the foundation's responsibility.
fn validate_response(handshake: &ClientHandshake, head: &[u8]) -> Result<(), ClientError> {
    if head.len() > MAX_HEADERS {
        return Err(invalid_upgrade());
    }
    let text = core::str::from_utf8(head).map_err(|_| invalid_upgrade())?;
    let text = text.strip_suffix("\r\n\r\n").ok_or_else(invalid_upgrade)?;
    let mut lines = text.split("\r\n");
    let reason = lines
        .next()
        .and_then(|line| line.strip_prefix("HTTP/1.1 101 "))
        .ok_or_else(invalid_upgrade)?;
    if !reason
        .bytes()
        .all(|byte| byte == b'\t' || (32..=126).contains(&byte))
    {
        return Err(invalid_upgrade());
    }
    let mut names = BTreeSet::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or_else(invalid_upgrade)?;
        if name.is_empty()
            || !name.bytes().all(token_byte)
            || !value
                .bytes()
                .all(|byte| byte == b'\t' || (32..=126).contains(&byte))
            || !names.insert(name.to_ascii_lowercase())
            || names.len() > MAX_HEADER_COUNT
        {
            return Err(invalid_upgrade());
        }
    }
    if names.contains("content-length")
        || names.contains("transfer-encoding")
        || names.contains("sec-websocket-extensions")
    {
        return Err(invalid_upgrade());
    }
    let response = HttpResponse::parse(head).map_err(|_| invalid_upgrade())?;
    if response.header("sec-websocket-protocol") != Some(SUBPROTOCOL)
        || !response
            .header("upgrade")
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
        || !response.header("connection").is_some_and(|value| {
            value.split(',').all(|token| {
                let token = token.trim();
                !token.is_empty() && token.bytes().all(token_byte)
            })
        })
    {
        return Err(invalid_upgrade());
    }
    handshake
        .validate_response(&response)
        .map_err(|_| invalid_upgrade())
}

#[cfg(test)]
mod tests;
