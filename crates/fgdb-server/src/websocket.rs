//! Browser-compatible FGP over bounded WebSocket binary messages.
//!
//! HTTP upgrade is protocol establishment only: database selection and Warden
//! authentication still belong to the ordinary FGP connection machine. One
//! absolute establishment deadline covers the upgrade and the final READY flush.

use crate::{Server, connection, tls};
use asupersync::Cx;
use asupersync::io::{AsyncReadExt as _, AsyncWriteExt as _};
use asupersync::net::websocket::{HttpRequest, ServerHandshake};
use fgdb_protocol::transport::DuplexIo;
use fgdb_protocol::websocket::WebSocketIo;
use std::collections::BTreeSet;
use std::io;
use std::net::Ipv6Addr;

const PATH: &str = "/v1/fgp";
const SUBPROTOCOL: &str = "fgp.v1";
const MAX_HEADERS: usize = 16 << 10;
const MAX_HEADER_COUNT: usize = 128;

/// Explicit network-origin policy for an FGP WebSocket listener.
///
/// Hosts have no port (IPv6 literals include brackets). A request may add a
/// valid numeric port to its Host. Origins are exact serialized HTTP(S) origins,
/// including an explicit port when used, with no path or wildcard. Native
/// clients may omit Origin; browser requests must match an allowed origin.
/// This policy admits an upgrade only and never replaces FGP authentication.
#[derive(Clone, Debug)]
pub struct WebSocketConfig {
    hosts: Vec<String>,
    origins: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WebSocketConfigError(&'static str);

impl core::fmt::Display for WebSocketConfigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}

impl core::error::Error for WebSocketConfigError {}

impl WebSocketConfig {
    pub fn new(
        allowed_hosts: Vec<String>,
        allowed_origins: Vec<String>,
    ) -> Result<Self, WebSocketConfigError> {
        if allowed_hosts.is_empty()
            || allowed_hosts
                .iter()
                .any(|host| authority_host(host) != Some(host.as_str()))
        {
            return Err(WebSocketConfigError(
                "WebSocket hosts must be explicit hostnames or bracketed IPv6 literals without ports",
            ));
        }
        if allowed_origins.iter().any(|origin| !valid_origin(origin)) {
            return Err(WebSocketConfigError(
                "WebSocket origins must be exact http:// or https:// origins without paths or wildcards",
            ));
        }
        Ok(Self {
            hosts: allowed_hosts,
            origins: allowed_origins,
        })
    }

    fn admits(&self, request: &HttpRequest) -> bool {
        let Some(host) = request.header("host").and_then(authority_host) else {
            return false;
        };
        self.hosts
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(host))
            && request
                .header("origin")
                .is_none_or(|origin| self.origins.iter().any(|allowed| allowed == origin))
    }
}

fn valid_port(port: &str) -> bool {
    !port.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok()
}

/// A deliberately narrow authority validator for the explicit Host policy.
/// It never interprets userinfo, escaped hostnames, wildcard names or paths.
fn authority_host(authority: &str) -> Option<&str> {
    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest.find(']')? + 1;
        authority[1..end].parse::<Ipv6Addr>().ok()?;
        let suffix = &authority[end + 1..];
        if !suffix.is_empty() && !suffix.strip_prefix(':').is_some_and(valid_port) {
            return None;
        }
        return Some(&authority[..=end]);
    }
    let (host, port) = authority
        .split_once(':')
        .map_or((authority, None), |(host, port)| (host, Some(port)));
    if host.is_empty()
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        || port.is_some_and(|port| !valid_port(port))
    {
        return None;
    }
    Some(host)
}

fn valid_origin(origin: &str) -> bool {
    origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .is_some_and(|authority| authority_host(authority).is_some())
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

fn invalid_upgrade() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "WebSocket upgrade refused")
}

/// The foundation owns RFC 6455 parsing and the accept-key transcript. Tighten
/// its general HTTP parser to this endpoint's exact no-body HTTP/1.1 profile
/// before it can merge duplicates or ignore an invalid header line.
fn response(head: &[u8], config: &WebSocketConfig) -> io::Result<Vec<u8>> {
    let text = core::str::from_utf8(head).map_err(|_| invalid_upgrade())?;
    let text = text.strip_suffix("\r\n\r\n").ok_or_else(invalid_upgrade)?;
    let mut lines = text.split("\r\n");
    if lines.next() != Some("GET /v1/fgp HTTP/1.1") {
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
    if names.contains("content-length") || names.contains("transfer-encoding") {
        return Err(invalid_upgrade());
    }
    let request = HttpRequest::parse(head).map_err(|_| invalid_upgrade())?;
    if request.path != PATH || !config.admits(&request) {
        return Err(invalid_upgrade());
    }
    let protocols = request
        .header("sec-websocket-protocol")
        .ok_or_else(invalid_upgrade)?;
    if protocols.split(',').any(|protocol| {
        let protocol = protocol.trim();
        protocol.is_empty() || !protocol.bytes().all(token_byte)
    }) {
        return Err(invalid_upgrade());
    }
    let accepted = ServerHandshake::new()
        .protocol(SUBPROTOCOL)
        .accept(&request)
        .map_err(|_| invalid_upgrade())?;
    if accepted.protocol.as_deref() != Some(SUBPROTOCOL) || !accepted.extensions.is_empty() {
        return Err(invalid_upgrade());
    }
    Ok(accepted.response_bytes())
}

async fn upgrade(
    cx: &Cx,
    mut stream: Box<dyn DuplexIo>,
    config: &WebSocketConfig,
    max_message_bytes: usize,
) -> io::Result<WebSocketIo<Box<dyn DuplexIo>>> {
    let mut input = Vec::new();
    let mut buffer = [0_u8; 1024];
    let header_end = loop {
        if let Some(end) = input.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break end + 4;
        }
        let remaining = MAX_HEADERS.saturating_sub(input.len());
        if remaining == 0 {
            stream
                .write_all(&ServerHandshake::reject(400, "Bad Request"))
                .await?;
            stream.flush().await?;
            return Err(invalid_upgrade());
        }
        cx.checkpoint()
            .map_err(|_| io::Error::from(io::ErrorKind::Interrupted))?;
        let count = stream.read(&mut buffer[..remaining.min(1024)]).await?;
        if count == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        input.extend_from_slice(&buffer[..count]);
    };
    let accepted = match response(&input[..header_end], config) {
        Ok(accepted) => accepted,
        Err(error) => {
            // Uniform public mechanics, with no database or policy details.
            stream
                .write_all(&ServerHandshake::reject(400, "Bad Request"))
                .await?;
            stream.flush().await?;
            return Err(error);
        }
    };
    let prefix = input.split_off(header_end);
    stream.write_all(&accepted).await?;
    stream.flush().await?;
    // Preserve pipelined WebSocket bytes already read with the upgrade. They
    // enter the same codec and limits as bytes subsequently read from the wire.
    WebSocketIo::server(cx, stream, max_message_bytes, prefix)
}

pub(crate) async fn run(
    cx: &Cx,
    server: &Server,
    stream: Box<dyn DuplexIo>,
    config: &WebSocketConfig,
) {
    let establishment = tls::Establishment::default();
    establishment
        .serve(
            cx,
            &server.shutdown,
            Box::pin(async {
                if let Ok(stream) =
                    upgrade(cx, stream, config, server.limits.max_frame_len as usize).await
                {
                    connection::run_inner(cx, server, Box::new(stream), &establishment).await;
                }
            }),
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(extra: &str) -> String {
        format!(
            "GET /v1/fgp HTTP/1.1\r\nHost: localhost:8789\r\n\
             Upgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Protocol: fgp.v1\r\n{extra}\r\n"
        )
    }

    fn config() -> WebSocketConfig {
        WebSocketConfig::new(
            vec!["localhost".into(), "[::1]".into()],
            vec!["https://console.example".into()],
        )
        .unwrap()
    }

    #[test]
    fn upgrade_uses_foundation_accept_key_and_explicit_browser_origin() {
        let config = config();
        for extra in ["", "Origin: https://console.example\r\n"] {
            let accepted = response(request(extra).as_bytes(), &config).unwrap();
            let accepted = core::str::from_utf8(&accepted).unwrap();
            assert!(accepted.starts_with("HTTP/1.1 101 Switching Protocols\r\n"));
            assert!(accepted.contains("Sec-WebSocket-Protocol: fgp.v1\r\n"));
            assert!(accepted.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"));
            assert!(!accepted.contains("Sec-WebSocket-Extensions"));
        }
        let accepted = request("Sec-WebSocket-Extensions: permessage-deflate\r\n");
        assert!(response(accepted.as_bytes(), &config).is_ok());
        let offered = request("").replace("fgp.v1\r\n", "other, fgp.v1\r\n");
        assert!(response(offered.as_bytes(), &config).is_ok());
    }

    #[test]
    fn malformed_upgrade_and_origin_substitution_are_refused() {
        let config = config();
        for extra in [
            "Origin: https://untrusted.example\r\n",
            "Origin: null\r\n",
            "Origin: https://console.example.evil\r\n",
            "Origin: https://console.example/\r\n",
            "Origin: https://console.example:443\r\n",
            "Origin: https://console.example\r\nOrigin: https://console.example\r\n",
            "Host: localhost\r\n",
            "Content-Length: 0\r\n",
            "Transfer-Encoding: chunked\r\n",
            "not-a-header\r\n",
            " folded: header\r\n",
            "X-Control: \u{0001}\r\n",
        ] {
            assert!(
                response(request(extra).as_bytes(), &config).is_err(),
                "{extra:?}"
            );
        }
        for (from, to) in [
            ("GET /v1/fgp HTTP/1.1", "GET /v1/fgp HTTP/1.0"),
            ("GET /v1/fgp HTTP/1.1", "GET /v1/fgp HTTP/1.1 extra"),
            ("GET /v1/fgp HTTP/1.1", "POST /v1/fgp HTTP/1.1"),
            ("/v1/fgp", "/v1/fgp?token=secret"),
            ("Host: localhost:8789", "Host: localhost:65536"),
            ("Host: localhost:8789", "Host: localhost:"),
            ("Host: localhost:8789", "Host: user@localhost"),
            ("Host: localhost:8789", "Host: localhost.evil"),
            ("Sec-WebSocket-Protocol: fgp.v1\r\n", ""),
            (
                "Sec-WebSocket-Protocol: fgp.v1",
                "Sec-WebSocket-Protocol: fgp/1",
            ),
            (
                "Sec-WebSocket-Protocol: fgp.v1",
                "Sec-WebSocket-Protocol: fgp.v1,",
            ),
        ] {
            assert!(
                response(request("").replace(from, to).as_bytes(), &config).is_err(),
                "{to:?}"
            );
        }
    }

    #[test]
    fn host_policy_covers_ipv6_and_rejects_ambiguous_authorities() {
        let config = config();
        for host in ["LOCALHOST:80", "[::1]:8789", "[::1]"] {
            let head = request("").replace("localhost:8789", host);
            assert!(response(head.as_bytes(), &config).is_ok(), "{host}");
        }
        for host in [
            "",
            "::1",
            "[::1",
            "[::1]:",
            "[not-ipv6]",
            "[::1]garbage",
            "localhost:80:81",
            "localhost/path",
            "*.example",
            "localhost,evil",
        ] {
            assert!(authority_host(host).is_none(), "{host}");
        }
        assert!(WebSocketConfig::new(Vec::new(), Vec::new()).is_err());
        assert!(WebSocketConfig::new(vec!["localhost:80".into()], Vec::new()).is_err());
        for origin in [
            "*",
            "null",
            "https://console.example/",
            "https://user@console.example",
        ] {
            assert!(WebSocketConfig::new(vec!["localhost".into()], vec![origin.into()]).is_err());
        }
        assert!(
            WebSocketConfig::new(vec!["[::1]".into()], vec!["http://[::1]:8789".into()],).is_ok()
        );
    }
}
