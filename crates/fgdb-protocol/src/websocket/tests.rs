use super::*;
use asupersync::{Budget, runtime::RuntimeBuilder};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Wake, Waker};

#[derive(Default)]
struct WireState {
    input: VecDeque<u8>,
    output: Vec<u8>,
    buffered: Vec<u8>,
    read_step: usize,
    write_step: usize,
    buffer_writes: bool,
    write_blocked: bool,
    flush_blocked: bool,
    eof: bool,
    writes: usize,
    flushes: usize,
    shutdowns: usize,
}

struct Wire(Arc<Mutex<WireState>>);

fn wire(input: Vec<u8>) -> (Wire, Arc<Mutex<WireState>>) {
    let state = Arc::new(Mutex::new(WireState {
        input: input.into(),
        read_step: READ_AHEAD,
        write_step: usize::MAX,
        ..WireState::default()
    }));
    (Wire(Arc::clone(&state)), state)
}

impl AsyncRead for Wire {
    fn poll_read(
        self: Pin<&mut Self>,
        _task: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut state = self.0.lock().unwrap();
        if state.input.is_empty() {
            return if state.eof {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            };
        }
        let count = state
            .read_step
            .min(buffer.remaining())
            .min(state.input.len());
        for _ in 0..count {
            buffer.put_slice(&[state.input.pop_front().unwrap()]);
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for Wire {
    fn poll_write(
        self: Pin<&mut Self>,
        _task: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut state = self.0.lock().unwrap();
        state.writes += 1;
        if state.write_blocked {
            return Poll::Pending;
        }
        let count = state.write_step.min(bytes.len());
        if state.buffer_writes {
            // A later write could flush earlier ciphertext, like a TLS writer.
            // Reader-driven controls must not reach this path while dirty.
            let previous = std::mem::take(&mut state.buffered);
            state.output.extend_from_slice(&previous);
            state.buffered.extend_from_slice(&bytes[..count]);
        } else {
            state.output.extend_from_slice(&bytes[..count]);
        }
        Poll::Ready(Ok(count))
    }

    fn poll_flush(self: Pin<&mut Self>, _task: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut state = self.0.lock().unwrap();
        state.flushes += 1;
        if state.flush_blocked {
            return Poll::Pending;
        }
        let pending = std::mem::take(&mut state.buffered);
        state.output.extend_from_slice(&pending);
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _task: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut state = self.0.lock().unwrap();
        state.shutdowns += 1;
        let pending = std::mem::take(&mut state.buffered);
        state.output.extend_from_slice(&pending);
        Poll::Ready(Ok(()))
    }
}

fn with_context(test: impl FnOnce(&Cx)) {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    test(&cx);
}

fn encoded(role: Role, frames: impl IntoIterator<Item = Frame>) -> Vec<u8> {
    let mut codec = FrameCodec::new(role);
    let mut bytes = BytesMut::new();
    for frame in frames {
        codec.encode(frame, &mut bytes).unwrap();
    }
    bytes.to_vec()
}

fn fragment(bytes: &[u8], first: bool, fin: bool) -> Frame {
    let mut frame = Frame::binary(bytes.to_vec());
    frame.fin = fin;
    if !first {
        frame.opcode = Opcode::Continuation;
    }
    frame
}

fn decoded(role: Role, bytes: &[u8]) -> Vec<Frame> {
    let mut codec = FrameCodec::new(role);
    let mut input = BytesMut::from(bytes);
    let mut frames = Vec::new();
    while let Some(frame) = codec.decode(&mut input).unwrap() {
        frames.push(frame);
    }
    assert!(input.is_empty());
    frames
}

fn read_once(io: &mut WebSocketIo<Wire>, size: usize) -> Poll<io::Result<Vec<u8>>> {
    let mut bytes = vec![0; size];
    let mut buffer = ReadBuf::new(&mut bytes);
    let mut task = Context::from_waker(Waker::noop());
    match Pin::new(io).poll_read(&mut task, &mut buffer) {
        Poll::Ready(Ok(())) => Poll::Ready(Ok(buffer.filled().to_vec())),
        Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
        Poll::Pending => Poll::Pending,
    }
}

fn write_all(io: &mut WebSocketIo<Wire>, bytes: &[u8]) {
    let mut accepted = 0;
    for _ in 0..100_000 {
        let mut task = Context::from_waker(Waker::noop());
        match Pin::new(&mut *io).poll_write(&mut task, &bytes[accepted..]) {
            Poll::Ready(Ok(count)) => {
                assert!(count > 0);
                accepted += count;
                if accepted == bytes.len() {
                    return;
                }
            }
            Poll::Ready(Err(error)) => panic!("write failed: {error}"),
            Poll::Pending => {}
        }
    }
    panic!("bounded write did not finish");
}

fn flush(io: &mut WebSocketIo<Wire>) {
    for _ in 0..1000 {
        let mut task = Context::from_waker(Waker::noop());
        match Pin::new(&mut *io).poll_flush(&mut task) {
            Poll::Ready(Ok(())) => return,
            Poll::Ready(Err(error)) => panic!("flush failed: {error}"),
            Poll::Pending => {}
        }
    }
    panic!("bounded flush did not finish");
}

#[test]
fn binary_fragments_upgrade_read_ahead_and_controls_preserve_exact_byte_order() {
    with_context(|cx| {
        let bytes = encoded(
            Role::Client,
            [
                fragment(b"abc", true, false),
                Frame::ping(b"control!".to_vec()),
                fragment(b"de", false, false),
                Frame::pong(b"ignored".to_vec()),
                fragment(b"f", false, true),
                Frame::binary(Vec::new()),
                Frame::binary(b"gh".to_vec()),
                Frame::close(Some(1000), None),
            ],
        );
        let prefix = bytes[..11].to_vec();
        let (inner, state) = wire(bytes[11..].to_vec());
        {
            let mut state = state.lock().unwrap();
            state.read_step = 1;
            state.write_step = 2;
        }
        let mut io = WebSocketIo::server(cx, inner, 6, prefix).unwrap();
        let mut result = Vec::new();
        let mut ended = false;
        for _ in 0..1000 {
            match read_once(&mut io, 2) {
                Poll::Ready(Ok(bytes)) if bytes.is_empty() => {
                    ended = true;
                    break;
                }
                Poll::Ready(Ok(bytes)) => result.extend(bytes),
                Poll::Ready(Err(error)) => panic!("read failed: {error}"),
                Poll::Pending => {}
            }
        }
        assert!(ended);
        assert_eq!(result, b"abcdefgh");
        let replies = decoded(Role::Client, &state.lock().unwrap().output);
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0].opcode, Opcode::Pong);
        assert_eq!(&replies[0].payload[..], b"control!");
        assert_eq!(replies[1].opcode, Opcode::Close);
        assert_eq!(&replies[1].payload[..], &1000_u16.to_be_bytes());
    });
}

#[test]
fn text_invalid_fragment_sequences_and_cumulative_limits_poison_the_stream() {
    with_context(|cx| {
        let cases = [
            vec![Frame::text(b"private".to_vec())],
            vec![fragment(b"x", false, true)],
            vec![fragment(b"x", true, false), Frame::binary(b"y".to_vec())],
            vec![
                fragment(b"12345", true, false),
                fragment(b"6789", false, true),
            ],
            vec![Frame::binary(b"123456789".to_vec())],
        ];
        for frames in cases {
            let (inner, state) = wire(encoded(Role::Client, frames));
            let mut io = WebSocketIo::server(cx, inner, 8, Vec::new()).unwrap();
            let mut refused = false;
            for _ in 0..100 {
                if let Poll::Ready(Err(error)) = read_once(&mut io, 64) {
                    assert_eq!(error.kind(), ErrorKind::InvalidData);
                    refused = true;
                    break;
                }
            }
            assert!(refused);
            let before = state.lock().unwrap().output.clone();
            assert!(matches!(read_once(&mut io, 1), Poll::Ready(Err(_))));
            assert!(matches!(
                Pin::new(&mut io).poll_write(&mut Context::from_waker(Waker::noop()), b"later"),
                Poll::Ready(Err(_))
            ));
            assert_eq!(state.lock().unwrap().output, before);
        }
    });
}

#[test]
fn masking_announced_lengths_and_truncated_wire_are_checked_by_the_codec() {
    with_context(|cx| {
        let mut oversized = encoded(Role::Client, [Frame::binary(vec![1; 129])]);
        oversized.truncate(4); // Length refusal precedes mask or payload intake.
        for (input, limit, expected) in [
            (vec![0x82, 0x01, b'x'], 128, ErrorKind::InvalidData),
            (oversized, 128, ErrorKind::InvalidData),
            (vec![0x82], 128, ErrorKind::UnexpectedEof),
        ] {
            let (inner, state) = wire(input);
            state.lock().unwrap().eof = true;
            let mut io = WebSocketIo::server(cx, inner, limit, Vec::new()).unwrap();
            let mut found = None;
            for _ in 0..10 {
                if let Poll::Ready(Err(error)) = read_once(&mut io, 16) {
                    found = Some(error.kind());
                    break;
                }
            }
            assert_eq!(found, Some(expected));
        }
        let (inner, _) = wire(Vec::new());
        assert!(WebSocketIo::server(cx, inner, 0, Vec::new()).is_err());
        let (inner, _) = wire(Vec::new());
        assert!(WebSocketIo::server(cx, inner, 1, vec![0; MAX_UPGRADE_PREFIX + 1]).is_err());
    });
}

#[test]
fn partial_payload_counts_and_read_side_controls_do_not_bypass_the_write_guard() {
    with_context(|cx| {
        let (inner, state) = wire(encoded(
            Role::Client,
            [
                Frame::ping(b"ping".to_vec()),
                Frame::binary(b"request".to_vec()),
            ],
        ));
        state.lock().unwrap().write_step = 1;
        let mut io = WebSocketIo::server(cx, inner, 128, Vec::new()).unwrap();
        assert!(
            Pin::new(&mut io)
                .poll_write(&mut Context::from_waker(Waker::noop()), b"protected")
                .is_pending()
        );
        assert_eq!(state.lock().unwrap().output, [0x82]); // Header only.
        let before = state.lock().unwrap().writes;
        let mut request = Vec::new();
        for _ in 0..10 {
            if let Poll::Ready(Ok(bytes)) = read_once(&mut io, 64) {
                request.extend(bytes);
                break;
            }
        }
        assert_eq!(request, b"request");
        assert_eq!(state.lock().unwrap().writes, before);
        assert_eq!(state.lock().unwrap().flushes, 0);

        let mut accepted = 0;
        for _ in 0..100 {
            let before = state.lock().unwrap().writes;
            let progress = Pin::new(&mut io).poll_write(
                &mut Context::from_waker(Waker::noop()),
                &b"protected"[accepted..],
            );
            assert_eq!(state.lock().unwrap().writes, before + 1);
            match progress {
                Poll::Ready(Ok(count)) => {
                    assert_eq!(count, 1); // The WS header never earns FGP credit.
                    accepted += count;
                }
                Poll::Pending => {}
                Poll::Ready(Err(error)) => panic!("write failed: {error}"),
            }
            if accepted == b"protected".len() {
                break;
            }
        }
        assert_eq!(accepted, b"protected".len());
        let before = state.lock().unwrap().writes;
        assert!(read_once(&mut io, 16).is_pending());
        assert_eq!(state.lock().unwrap().writes, before);
        assert_eq!(state.lock().unwrap().flushes, 0);

        flush(&mut io); // Represents the caller's fresh output authorization.
        for _ in 0..100 {
            assert!(read_once(&mut io, 16).is_pending());
            if !io.has_control() {
                break;
            }
        }
        assert!(!io.has_control());
        let sent = decoded(Role::Client, &state.lock().unwrap().output);
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].opcode, Opcode::Binary);
        assert_eq!(&sent[0].payload[..], b"protected");
        assert_eq!(sent[1].opcode, Opcode::Pong);
        assert_eq!(&sent[1].payload[..], b"ping");
    });
}

#[test]
fn acknowledged_but_unflushed_tls_bytes_still_fence_read_controls_and_shutdown() {
    with_context(|cx| {
        let (inner, state) = wire(encoded(
            Role::Client,
            [Frame::ping(b"p".to_vec()), Frame::binary(b"r".to_vec())],
        ));
        state.lock().unwrap().buffer_writes = true;
        let mut io = WebSocketIo::server(cx, inner, 128, Vec::new()).unwrap();
        write_all(&mut io, b"protected");
        assert!(io.data.is_none());
        assert!(state.lock().unwrap().output.is_empty());
        let mut received = false;
        for _ in 0..10 {
            if let Poll::Ready(Ok(bytes)) = read_once(&mut io, 16) {
                assert_eq!(bytes, b"r");
                received = true;
                break;
            }
        }
        assert!(received);
        assert!(state.lock().unwrap().output.is_empty());
        assert_eq!(state.lock().unwrap().writes, 1);
        assert!(matches!(
            Pin::new(&mut io).poll_shutdown(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Err(error)) if error.kind() == ErrorKind::InvalidInput
        ));
        assert_eq!(state.lock().unwrap().shutdowns, 0);
        assert_eq!(state.lock().unwrap().flushes, 0);
        assert!(state.lock().unwrap().output.is_empty());
        flush(&mut io);
        let sent = decoded(Role::Client, &state.lock().unwrap().output);
        assert_eq!(sent.len(), 1);
        assert_eq!(&sent[0].payload[..], b"protected");
    });
}

#[test]
fn a_changed_retry_cannot_complete_an_abandoned_encoded_frame() {
    with_context(|cx| {
        let (inner, state) = wire(Vec::new());
        state.lock().unwrap().write_step = 1;
        let mut io = WebSocketIo::server(cx, inner, 128, Vec::new()).unwrap();
        assert!(
            Pin::new(&mut io)
                .poll_write(&mut Context::from_waker(Waker::noop()), b"original")
                .is_pending()
        );
        let before = state.lock().unwrap().output.clone();
        assert!(matches!(
            Pin::new(&mut io).poll_write(
                &mut Context::from_waker(Waker::noop()),
                b"replacement"
            ),
            Poll::Ready(Err(error)) if error.kind() == ErrorKind::InvalidInput
        ));
        assert!(matches!(read_once(&mut io, 1), Poll::Ready(Err(_))));
        assert_eq!(state.lock().unwrap().output, before);
        assert_eq!(state.lock().unwrap().writes, 1);
        assert_eq!(state.lock().unwrap().flushes, 0);
    });
}

#[test]
fn client_messages_are_masked_and_bounded_without_changing_fgp_payload_bytes() {
    with_context(|cx| {
        let (inner, state) = wire(Vec::new());
        let mut io = WebSocketIo::client(cx, inner, 1 << 20, Vec::new()).unwrap();
        let bytes: Vec<u8> = (0..35_000).map(|i| (i % 251) as u8).collect();
        write_all(&mut io, &bytes);
        flush(&mut io);
        let frames = decoded(Role::Server, &state.lock().unwrap().output);
        assert_eq!(frames.len(), 3);
        assert!(frames.iter().all(|frame| {
            frame.masked
                && frame.fin
                && frame.opcode == Opcode::Binary
                && frame.payload.len() <= WRITE_CHUNK
        }));
        let restored: Vec<u8> = frames
            .iter()
            .flat_map(|frame| frame.payload.iter().copied())
            .collect();
        assert_eq!(restored, bytes);
        // The random mask bytes themselves deliberately have no seed-replay law.
    });
}

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

#[test]
fn backpressured_public_control_waits_for_readiness_without_a_self_wake_loop() {
    with_context(|cx| {
        for close in [false, true] {
            let frame = if close {
                Frame::close(Some(1000), None)
            } else {
                Frame::ping(b"wait".to_vec())
            };
            let (inner, state) = wire(encoded(Role::Client, [frame]));
            state.lock().unwrap().write_blocked = true;
            let mut io = WebSocketIo::server(cx, inner, 128, Vec::new()).unwrap();
            let wakes = Arc::new(WakeCount::default());
            let waker = Waker::from(Arc::clone(&wakes));
            let mut task = Context::from_waker(&waker);
            let mut storage = [0; 16];
            assert!(
                Pin::new(&mut io)
                    .poll_read(&mut task, &mut ReadBuf::new(&mut storage))
                    .is_pending()
            );
            wakes.0.store(0, Ordering::SeqCst);
            assert!(
                Pin::new(&mut io)
                    .poll_read(&mut task, &mut ReadBuf::new(&mut storage))
                    .is_pending()
            );
            assert_eq!(wakes.0.load(Ordering::SeqCst), 0);
            if !close {
                assert!(
                    Pin::new(&mut io)
                        .poll_write(&mut task, b"data")
                        .is_pending()
                );
                assert_eq!(wakes.0.load(Ordering::SeqCst), 0);
                assert!(Pin::new(&mut io).poll_shutdown(&mut task).is_pending());
                assert_eq!(wakes.0.load(Ordering::SeqCst), 0);
            }
        }
    });
}

#[test]
fn completing_a_control_flush_schedules_its_queued_successor_without_more_input() {
    with_context(|cx| {
        for close in [false, true] {
            let (inner, state) = wire(encoded(Role::Client, [Frame::ping(b"first".to_vec())]));
            let mut io = WebSocketIo::server(cx, inner, 128, Vec::new()).unwrap();
            let wakes = Arc::new(WakeCount::default());
            let waker = Waker::from(Arc::clone(&wakes));
            let mut task = Context::from_waker(&waker);
            let mut storage = [0; 16];
            assert!(
                Pin::new(&mut io)
                    .poll_read(&mut task, &mut ReadBuf::new(&mut storage))
                    .is_pending()
            );
            let successor = if close {
                Frame::close(Some(1000), None)
            } else {
                Frame::ping(b"second".to_vec())
            };
            {
                let mut state = state.lock().unwrap();
                state.flush_blocked = true;
                state.input.extend(encoded(Role::Client, [successor]));
            }
            // First Pong is written, and the second control arrives before
            // that write's flush completes. Nothing else will become readable.
            assert!(
                Pin::new(&mut io)
                    .poll_read(&mut task, &mut ReadBuf::new(&mut storage))
                    .is_pending()
            );
            assert!(io.control.is_some());
            assert!(io.queued_control.is_some());
            wakes.0.store(0, Ordering::SeqCst);
            assert!(
                Pin::new(&mut io)
                    .poll_read(&mut task, &mut ReadBuf::new(&mut storage))
                    .is_pending()
            );
            assert_eq!(wakes.0.load(Ordering::SeqCst), 0);

            state.lock().unwrap().flush_blocked = false;
            assert!(
                Pin::new(&mut io)
                    .poll_read(&mut task, &mut ReadBuf::new(&mut storage))
                    .is_pending()
            );
            assert!(wakes.0.load(Ordering::SeqCst) > 0);
            for _ in 0..10 {
                let _ = read_once(&mut io, 16);
                if !io.has_control() {
                    break;
                }
            }
            assert!(!io.has_control());
            let replies = decoded(Role::Client, &state.lock().unwrap().output);
            assert_eq!(replies.len(), 2);
            assert_eq!(replies[0].opcode, Opcode::Pong);
            assert_eq!(&replies[0].payload[..], b"first");
            if close {
                assert_eq!(replies[1].opcode, Opcode::Close);
            } else {
                assert_eq!(replies[1].opcode, Opcode::Pong);
                assert_eq!(&replies[1].payload[..], b"second");
            }
        }
    });
}

#[test]
fn receive_only_reserved_close_codes_use_the_foundations_empty_close_reply() {
    with_context(|cx| {
        for code in [1016_u16, 2000, 2999] {
            // Independent masked receive vector. The public foundation encoder
            // correctly refuses originating these receive-only status codes.
            let mut input = vec![0x88, 0x82, 0, 0, 0, 0];
            input.extend_from_slice(&code.to_be_bytes());
            let (inner, state) = wire(input);
            let mut io = WebSocketIo::server(cx, inner, 1, Vec::new()).unwrap();
            let mut ended = false;
            for _ in 0..10 {
                match read_once(&mut io, 16) {
                    Poll::Ready(Ok(bytes)) => {
                        assert!(bytes.is_empty());
                        ended = true;
                        break;
                    }
                    Poll::Ready(Err(error)) => panic!("legal close failed: {error}"),
                    Poll::Pending => {}
                }
            }
            assert!(ended);
            let replies = decoded(Role::Client, &state.lock().unwrap().output);
            assert_eq!(replies.len(), 1);
            assert_eq!(replies[0].opcode, Opcode::Close);
            assert!(replies[0].payload.is_empty());
        }
    });
}
