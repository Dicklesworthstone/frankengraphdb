use super::*;
use crate::body::{Auth, AuthOk, Body, Credential, Hello, HelloAck};
use crate::{Binding, Decoder, Frame, FrameKind, FrameLimits, SessionBinding, StreamId};
use asupersync::Budget;
use asupersync::bytes::BytesMut;
use asupersync::codec::{Decoder as _, Encoder as _};
use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
use asupersync::net::websocket::{
    Frame as WsFrame, FrameCodec, HttpRequest, Opcode, ServerHandshake,
};
use asupersync::runtime::RuntimeBuilder;
use asupersync::time::{TimerDriverHandle, VirtualClock};
use asupersync::types::{CancelKind, Time};
use core::task::Context;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Wake, Waker};

#[derive(Default)]
struct WireState {
    input: VecDeque<u8>,
    output: Vec<u8>,
    reader: Option<Waker>,
    read_step: usize,
    write_step: usize,
    read_bytes: usize,
    block_write: bool,
    block_flush: bool,
    eof: bool,
    dropped: bool,
}

struct Wire(Arc<Mutex<WireState>>);

fn wire() -> (Wire, Arc<Mutex<WireState>>) {
    let state = Arc::new(Mutex::new(WireState {
        read_step: usize::MAX,
        write_step: usize::MAX,
        ..WireState::default()
    }));
    (Wire(Arc::clone(&state)), state)
}

impl Drop for Wire {
    fn drop(&mut self) {
        self.0.lock().unwrap().dropped = true;
    }
}

impl AsyncRead for Wire {
    fn poll_read(
        self: Pin<&mut Self>,
        task: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut state = self.0.lock().unwrap();
        if state.input.is_empty() {
            state.reader = Some(task.waker().clone());
            return if state.eof {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            };
        }
        let count = buffer
            .remaining()
            .min(state.input.len())
            .min(state.read_step);
        for _ in 0..count {
            buffer.put_slice(&[state.input.pop_front().unwrap()]);
        }
        state.read_bytes += count;
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for Wire {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut state = self.0.lock().unwrap();
        if state.block_write {
            return Poll::Pending;
        }
        let count = state.write_step.min(bytes.len());
        state.output.extend_from_slice(&bytes[..count]);
        Poll::Ready(Ok(count))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.0.lock().unwrap().block_flush {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn feed(state: &Arc<Mutex<WireState>>, bytes: &[u8]) {
    let waker = {
        let mut state = state.lock().unwrap();
        state.input.extend(bytes);
        state.reader.take()
    };
    if let Some(waker) = waker {
        waker.wake();
    }
}

fn with_clock(test: impl FnOnce(&Cx, &VirtualClock, &TimerDriverHandle)) {
    let clock = Arc::new(VirtualClock::new());
    let timer = TimerDriverHandle::with_virtual_clock(Arc::clone(&clock));
    let runtime = RuntimeBuilder::current_thread()
        .with_timer_driver(timer.clone())
        .build()
        .unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async { test(&cx, &clock, &timer) });
}

#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn finish<T>(mut future: Pin<&mut impl Future<Output = T>>) -> T {
    let mut task = Context::from_waker(Waker::noop());
    for _ in 0..512 {
        if let Poll::Ready(result) = future.as_mut().poll(&mut task) {
            return result;
        }
    }
    panic!("the finite scripted exchange did not complete");
}

fn response(request: &[u8]) -> Vec<u8> {
    let request = HttpRequest::parse(request).unwrap();
    assert_eq!(request.path, PATH);
    assert_eq!(request.header("sec-websocket-protocol"), Some(SUBPROTOCOL));
    assert_eq!(request.header("origin"), None);
    assert_eq!(request.header("authorization"), None);
    ServerHandshake::new()
        .protocol(SUBPROTOCOL)
        .accept(&request)
        .unwrap()
        .response_bytes()
}

fn fgp(kind: FrameKind, request: u64, body: impl Body) -> Vec<u8> {
    let limits = FrameLimits::new(CLIENT_MAX_FRAME as usize).unwrap();
    Frame::new(
        kind,
        request,
        StreamId::CONTROL,
        Binding::Transport,
        body.encode().unwrap(),
        limits,
    )
    .unwrap()
    .encode(limits)
    .unwrap()
}

fn hello_ack() -> Vec<u8> {
    fgp(
        FrameKind::HelloAck,
        1,
        HelloAck {
            version: crate::PROTOCOL_VERSION,
            server_nonce: [0x42; 32],
            max_frame_len: CLIENT_MAX_FRAME,
            initial_window_bytes: 65536,
            initial_window_rows: 16,
        },
    )
}

fn ws(bytes: Vec<u8>) -> Vec<u8> {
    let mut output = BytesMut::new();
    FrameCodec::server()
        .encode(WsFrame::binary(bytes), &mut output)
        .unwrap();
    output.to_vec()
}

fn written_frames(bytes: &[u8]) -> Vec<Frame> {
    let mut input = BytesMut::from(bytes);
    let mut codec = FrameCodec::server();
    let mut application = Vec::new();
    while let Some(frame) = codec.decode(&mut input).unwrap() {
        assert_eq!(frame.opcode, Opcode::Binary);
        assert!(frame.fin);
        application.extend_from_slice(&frame.payload);
    }
    assert!(input.is_empty());
    let mut decoder = Decoder::new(FrameLimits::new(CLIENT_MAX_FRAME as usize).unwrap());
    let mut frames = Vec::new();
    let mut rest = application.as_slice();
    while !rest.is_empty() {
        let decoded = decoder.decode(rest, |_| Ok(())).unwrap();
        assert!(decoded.consumed > 0);
        rest = &rest[decoded.consumed..];
        if let Some(frame) = decoded.frame {
            frames.push(frame);
        }
    }
    frames
}

#[test]
fn target_accepts_bounded_authorities_and_rejects_url_or_header_injection() {
    for authority in [
        "localhost",
        "db.example:9001",
        "DB.Example.",
        "xn--bcher-kva.example:443",
        "127.0.0.1:65535",
        "[::1]",
        "[2001:db8::1]:9001",
    ] {
        let target = WebSocketTarget::new(authority.into()).unwrap();
        assert_eq!(target.authority(), authority);
    }
    for authority in [
        "",
        ".",
        "..",
        "-db.example",
        "db-.example",
        "db..example",
        "db_example",
        "bücher.example",
        "999.0.0.1",
        "localhost:",
        "localhost:0",
        "localhost:65536",
        "localhost:+80",
        "localhost:80:90",
        "user@localhost",
        "localhost/v1/fgp",
        "localhost?token=secret",
        "localhost#fragment",
        "ws://localhost",
        "localhost\r\nAuthorization: secret",
        "localhost\t",
        "[::1",
        "::1",
        "[127.0.0.1]",
        "[::1]suffix",
        "[fe80::1%25eth0]",
        "[::1]:",
    ] {
        assert!(
            WebSocketTarget::new(authority.into()).is_err(),
            "{authority:?}"
        );
    }
    assert!(WebSocketTarget::new(format!("{}.example", "a".repeat(64))).is_err());
    assert!(WebSocketTarget::new("a".repeat(260)).is_err());
}

#[test]
fn refused_upgrades_never_write_fgp_or_the_credential() {
    with_clock(|cx, _, timer| {
        let target = WebSocketTarget::new("db.example:9001".into()).unwrap();
        let mut keys = BTreeSet::new();
        for case in 0..19 {
            let (io, state) = wire();
            let mut connection = Box::pin(Client::connect_websocket_stream(
                cx,
                &target,
                io,
                b"must-never-cross-a-refused-upgrade".to_vec(),
            ));
            assert!(
                connection
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            let request = state.lock().unwrap().output.clone();
            let parsed = HttpRequest::parse(&request).unwrap();
            assert_eq!(parsed.header("host"), Some("db.example:9001"));
            assert!(keys.insert(parsed.header("sec-websocket-key").unwrap().to_owned()));
            let valid = String::from_utf8(response(&request)).unwrap();
            let append = |header: &str| {
                format!(
                    "{}\r\n{header}\r\n\r\n",
                    valid.strip_suffix("\r\n\r\n").unwrap()
                )
            };
            let bad = match case {
                0 => valid.replace("HTTP/1.1 101", "HTTP/1.0 101"),
                1 => valid.replace("HTTP/1.1 101", "HTTP/1.1 302"),
                2 => valid.replace("Sec-WebSocket-Protocol: fgp.v1\r\n", ""),
                3 => valid.replace("fgp.v1", "other"),
                4 => valid.replace("fgp.v1", "fgp.v1, other"),
                5 => append("Sec-WebSocket-Extensions: permessage-deflate"),
                6 => append("Sec-WebSocket-Extensions:"),
                7 => append("Content-Length: 0"),
                8 => append("Transfer-Encoding: chunked"),
                9 => append("sEc-WeBsOcKeT-aCcEpT: duplicate"),
                10 => append("No-Colon-Here"),
                11 => append(" Folded: value"),
                12 => append("X-Control: value\u{7f}"),
                13 => valid.replace("Upgrade: websocket", "Upgrade: websocket, h2c"),
                14 => valid.replace("Connection: Upgrade", "Connection: Upgrade,"),
                15 => valid.replace("Sec-WebSocket-Accept: ", "Sec-WebSocket-Accept: invalid"),
                16 => append(&format!("X-Large: {}", "x".repeat(MAX_HEADERS))),
                17 => append(
                    &(0..MAX_HEADER_COUNT)
                        .map(|index| format!("X-{index}: value"))
                        .collect::<Vec<_>>()
                        .join("\r\n"),
                ),
                18 => valid.replace("\r\n", "\n"),
                _ => unreachable!(),
            };
            feed(&state, bad.as_bytes());
            // A response with no valid CRLF terminator must finish on EOF,
            // without waiting for a timeout or trying an alternate transport.
            state.lock().unwrap().eof = true;
            let result = finish(connection.as_mut());
            assert!(result.is_err(), "case {case}");
            drop(connection);
            let state = state.lock().unwrap();
            assert_eq!(state.output, request, "case {case} sent post-upgrade bytes");
            assert!(state.dropped, "case {case} retained the transport");
            assert!(state.read_bytes <= MAX_HEADERS);
            assert_eq!(timer.pending_count(), 0);
        }
    });
}

#[test]
fn validated_upgrade_preserves_read_ahead_and_fragmented_fgp_authentication() {
    for fragment in [false, true] {
        with_clock(|cx, clock, timer| {
            let target = WebSocketTarget::new("[::1]:9001".into()).unwrap();
            let (io, state) = wire();
            if fragment {
                let mut state = state.lock().unwrap();
                state.read_step = 1;
                state.write_step = 3;
            }
            let token = b"authenticated-only-inside-fgp".to_vec();
            let mut connection = Box::pin(Client::connect_websocket_stream(
                cx,
                &target,
                io,
                token.clone(),
            ));
            assert!(
                connection
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            let request = state.lock().unwrap().output.clone();
            assert_eq!(
                HttpRequest::parse(&request).unwrap().header("host"),
                Some("[::1]:9001"),
            );
            let session = SessionBinding {
                transcript: [0x91; 32],
                auth_generation: 1,
            };
            let mut replies = hello_ack();
            replies.extend(fgp(FrameKind::AuthOk, 2, AuthOk { session }));
            let mut input = response(&request);
            // A fast peer can coalesce HTTP and binary FGP responses into one
            // lower read. Those already-read bytes must reach the FGP decoder.
            input.extend(ws(replies));
            feed(&state, &input);
            let client = finish(connection.as_mut()).unwrap();
            assert_eq!(client.binding, Binding::Session(session));
            assert_eq!(timer.pending_count(), 0);
            drop(connection);
            let output = state.lock().unwrap().output.clone();
            let frames = written_frames(&output[request.len()..]);
            assert_eq!(frames.len(), 2);
            assert_eq!(frames[0].header().kind(), FrameKind::Hello);
            assert_eq!(frames[1].header().kind(), FrameKind::Auth);
            assert_eq!(
                Hello::decode(frames[0].payload()).unwrap().max_frame_len,
                CLIENT_MAX_FRAME
            );
            assert_eq!(
                Auth::decode(frames[1].payload()).unwrap().credential,
                Credential::WardenCapability(token),
            );
            // Successful setup retires its timer; it is not a connection idle
            // timeout that can unexpectedly terminate an authenticated client.
            clock.advance_to(Time::from_secs(60));
            assert_eq!(timer.process_timers(), 0);
            assert!(!state.lock().unwrap().dropped);
            drop(client);
            assert!(state.lock().unwrap().dropped);
        });
    }
}

#[test]
fn late_upgrade_does_not_renew_the_authentication_deadline() {
    with_clock(|cx, clock, timer| {
        let target = WebSocketTarget::new("localhost".into()).unwrap();
        let (io, state) = wire();
        let mut connection = Box::pin(Client::connect_websocket_stream(
            cx,
            &target,
            io,
            b"token".to_vec(),
        ));
        let wakes = Arc::new(WakeCount::default());
        let waker = Waker::from(Arc::clone(&wakes));
        let mut task = Context::from_waker(&waker);
        assert!(connection.as_mut().poll(&mut task).is_pending());
        let request = state.lock().unwrap().output.clone();
        let response = response(&request);
        let boundary = response.len() - 2;
        feed(&state, &response[..boundary]);
        assert!(connection.as_mut().poll(&mut task).is_pending());
        assert_eq!(state.lock().unwrap().output, request);
        assert_eq!(timer.pending_count(), 1);

        clock.advance_to(Time::from_secs(29));
        let mut tail = response[boundary..].to_vec();
        tail.extend(ws(hello_ack()));
        feed(&state, &tail);
        assert!(connection.as_mut().poll(&mut task).is_pending());
        let frames = written_frames(&state.lock().unwrap().output[request.len()..]);
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.header().kind())
                .collect::<Vec<_>>(),
            vec![FrameKind::Hello, FrameKind::Auth],
        );
        assert_eq!(timer.pending_count(), 1);

        let before = wakes.0.load(Ordering::Relaxed);
        clock.advance_to(Time::from_secs(30));
        assert_eq!(timer.process_timers(), 1);
        assert!(wakes.0.load(Ordering::Relaxed) > before);
        assert!(matches!(
            connection.as_mut().poll(&mut task),
            Poll::Ready(Err(ClientError::Io(io::ErrorKind::TimedOut))),
        ));
        drop(connection);
        assert!(state.lock().unwrap().dropped);
        assert_eq!(timer.pending_count(), 0);
    });
}

#[test]
fn cancellation_wakes_blocked_upgrade_write_flush_and_read() {
    for blocked in 0..3 {
        with_clock(|cx, _, timer| {
            let target = WebSocketTarget::new("localhost".into()).unwrap();
            let (io, state) = wire();
            {
                let mut state = state.lock().unwrap();
                state.block_write = blocked == 0;
                state.block_flush = blocked == 1;
            }
            let mut connection = Box::pin(Client::connect_websocket_stream(
                cx,
                &target,
                io,
                b"unwritten-credential".to_vec(),
            ));
            let wakes = Arc::new(WakeCount::default());
            let waker = Waker::from(Arc::clone(&wakes));
            let mut task = Context::from_waker(&waker);
            assert!(connection.as_mut().poll(&mut task).is_pending());
            let output = state.lock().unwrap().output.clone();
            assert_eq!(timer.pending_count(), 1);
            let before = wakes.0.load(Ordering::Relaxed);
            cx.cancel_fast(CancelKind::User);
            assert!(wakes.0.load(Ordering::Relaxed) > before);
            assert!(matches!(
                connection.as_mut().poll(&mut task),
                Poll::Ready(Err(ClientError::Transport(TransportError::ContextStopped))),
            ));
            drop(connection);
            let state = state.lock().unwrap();
            assert_eq!(state.output, output);
            assert!(state.dropped);
            assert_eq!(timer.pending_count(), 0);
            assert_eq!(cx.now(), Time::ZERO);
        });
    }
}
