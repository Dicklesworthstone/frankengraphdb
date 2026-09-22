#![cfg(feature = "transport")]

use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
use asupersync::{Budget, Cx, runtime::RuntimeBuilder};
use fgdb_protocol::transport::{AbandonedSend, FrameReader, FrameWriter, TransportError};
use fgdb_protocol::{Binding, Connection, Frame, FrameKind, FrameLimits, ProtocolError, StreamId};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::io::{self, ErrorKind};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

fn with_cx(test: impl FnOnce(&Cx)) {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    test(&cx);
}
fn limits() -> FrameLimits {
    FrameLimits::new(16384).unwrap()
}
fn frame(kind: FrameKind, payload: Vec<u8>) -> Frame {
    Frame::new(
        kind,
        1,
        StreamId::CONTROL,
        Binding::Transport,
        payload,
        limits(),
    )
    .unwrap()
}
fn drive<T>(mut poll: impl FnMut(&mut Context<'_>) -> Poll<T>) -> T {
    let mut task = Context::from_waker(Waker::noop());
    for _ in 0..100_000 {
        if let Poll::Ready(value) = poll(&mut task) {
            return value;
        }
    }
    panic!("driver made no bounded progress");
}

struct Reader {
    bytes: Vec<u8>,
    offset: usize,
    // Zero is a single Pending, never an EOF.
    chunks: VecDeque<usize>,
}
impl Reader {
    fn new(bytes: Vec<u8>, chunks: impl IntoIterator<Item = usize>) -> Self {
        Self {
            bytes,
            offset: 0,
            chunks: chunks.into_iter().collect(),
        }
    }
}
impl AsyncRead for Reader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        task: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let chunk = self.chunks.pop_front().unwrap_or(usize::MAX);
        if chunk == 0 {
            task.waker().wake_by_ref();
            return Poll::Pending;
        }
        let count = chunk
            .min(buf.remaining())
            .min(self.bytes.len() - self.offset);
        buf.put_slice(&self.bytes[self.offset..self.offset + count]);
        self.offset += count;
        Poll::Ready(Ok(()))
    }
}

struct Writer {
    bytes: Rc<RefCell<Vec<u8>>>,
    chunks: VecDeque<usize>,
    flush_pending: bool,
    error: Option<ErrorKind>,
}
impl Writer {
    fn new(chunks: impl IntoIterator<Item = usize>) -> (Self, Rc<RefCell<Vec<u8>>>) {
        let bytes = Rc::new(RefCell::new(Vec::new()));
        (
            Self {
                bytes: Rc::clone(&bytes),
                chunks: chunks.into_iter().collect(),
                flush_pending: false,
                error: None,
            },
            bytes,
        )
    }
}
impl AsyncWrite for Writer {
    fn poll_write(
        mut self: Pin<&mut Self>,
        task: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if let Some(kind) = self.error.take() {
            return Poll::Ready(Err(io::Error::new(kind, "secret peer text")));
        }
        let count = self.chunks.pop_front().unwrap_or(usize::MAX);
        if count == 0 {
            task.waker().wake_by_ref();
            return Poll::Pending;
        }
        let count = count.min(buf.len());
        self.bytes.borrow_mut().extend_from_slice(&buf[..count]);
        Poll::Ready(Ok(count))
    }
    fn poll_flush(mut self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<io::Result<()>> {
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

#[test]
fn every_frame_split_survives_dropping_receive_future() {
    with_cx(|cx| {
        let expected = frame(FrameKind::Hello, (0..=255).collect());
        let wire = expected.encode(limits()).unwrap();
        for split in 1..wire.len() {
            let mut reader = FrameReader::new(Reader::new(wire.clone(), [split, 0]), limits());
            let mut task = Context::from_waker(Waker::noop());
            {
                let mut future = Box::pin(reader.receive(cx, |_| Ok(())));
                assert!(future.as_mut().poll(&mut task).is_pending());
            }
            assert_eq!(
                drive(|task| reader.poll_receive(cx, task, |_| Ok(()))).unwrap(),
                Some(expected.clone())
            );
            assert_eq!(
                drive(|task| reader.poll_receive(cx, task, |_| Ok(()))).unwrap(),
                None
            );
        }
    });
}

#[test]
fn coalesced_handshake_is_not_validated_ahead_of_transition() {
    with_cx(|cx| {
        let hello = frame(FrameKind::Hello, vec![]);
        let auth = frame(FrameKind::Auth, vec![7; 8]);
        let mut wire = hello.encode(limits()).unwrap();
        wire.extend(auth.encode(limits()).unwrap());
        let mut reader = FrameReader::new(Reader::new(wire, []), limits());
        let mut connection = Connection::new(8, 8).unwrap();
        let first =
            drive(|task| reader.poll_receive(cx, task, |h| connection.validate_client_header(h)))
                .unwrap();
        assert_eq!(first, Some(hello));
        connection.negotiated().unwrap();
        let second =
            drive(|task| reader.poll_receive(cx, task, |h| connection.validate_client_header(h)))
                .unwrap();
        assert_eq!(second, Some(auth));
    });
}

#[test]
fn incomplete_eof_at_every_byte_is_never_a_success() {
    with_cx(|cx| {
        let wire = frame(FrameKind::Hello, vec![1; 32])
            .encode(limits())
            .unwrap();
        for end in 1..wire.len() {
            let mut reader = FrameReader::new(Reader::new(wire[..end].to_vec(), []), limits());
            assert_eq!(
                drive(|task| reader.poll_receive(cx, task, |_| Ok(()))),
                Err(TransportError::Protocol(ProtocolError::TruncatedFrame))
            );
            assert_eq!(
                drive(|task| reader.poll_receive(cx, task, |_| Ok(()))),
                Err(TransportError::Closed)
            );
        }
    });
}

#[test]
fn header_and_completion_revalidate_before_dispatch() {
    with_cx(|cx| {
        let wire = frame(FrameKind::Hello, vec![1; 32])
            .encode(limits())
            .unwrap();
        let mut reader = FrameReader::new(Reader::new(wire, [35, 0]), limits());
        let mut task = Context::from_waker(Waker::noop());
        assert!(reader.poll_receive(cx, &mut task, |_| Ok(())).is_pending());
        assert_eq!(
            drive(|task| reader.poll_receive(cx, task, |_| Err(ProtocolError::InvalidBinding))),
            Err(TransportError::Protocol(ProtocolError::InvalidBinding))
        );
    });
}

#[test]
fn oversized_declaration_fails_before_body_allocation() {
    with_cx(|cx| {
        let mut reader =
            FrameReader::new(Reader::new(u32::MAX.to_be_bytes().to_vec(), []), limits());
        assert_eq!(
            drive(|task| reader.poll_receive(cx, task, |_| Ok(()))),
            Err(TransportError::Protocol(ProtocolError::FrameTooLarge))
        );
        assert!(reader.buffered_bytes() <= 4096);
    });
}

#[test]
fn every_short_write_survives_dropped_future_without_duplicate_prefix() {
    with_cx(|cx| {
        let expected = frame(FrameKind::HelloAck, vec![9; 32]);
        let wire = expected.encode(limits()).unwrap();
        for split in 1..wire.len() {
            let (io, output) = Writer::new([split, 0]);
            let mut writer = FrameWriter::new(io, limits());
            writer.queue(cx, &expected).unwrap();
            let mut task = Context::from_waker(Waker::noop());
            {
                let mut future = Box::pin(writer.send(cx, |_| Ok(())));
                assert!(future.as_mut().poll(&mut task).is_pending());
            }
            assert_eq!(writer.accepted_bytes(), Some(split));
            let completion = drive(|task| writer.poll_send(cx, task, |_| Ok(()))).unwrap();
            assert_eq!(completion.encoded_bytes, wire.len());
            assert_eq!(*output.borrow(), wire);
            assert_eq!(writer.accepted_bytes(), None);
        }
    });
}

#[test]
fn flush_pending_does_not_resend_complete_frame() {
    with_cx(|cx| {
        let expected = frame(FrameKind::HelloAck, vec![]);
        let (mut io, output) = Writer::new([]);
        io.flush_pending = true;
        let mut writer = FrameWriter::new(io, limits());
        writer.queue(cx, &expected).unwrap();
        let mut task = Context::from_waker(Waker::noop());
        assert!(writer.poll_send(cx, &mut task, |_| Ok(())).is_pending());
        drive(|task| writer.poll_send(cx, task, |_| Ok(()))).unwrap();
        assert_eq!(*output.borrow(), expected.encode(limits()).unwrap());
    });
}

#[test]
fn revocation_after_pending_is_checked_before_any_bytes() {
    with_cx(|cx| {
        let (io, output) = Writer::new([0]);
        let mut writer = FrameWriter::new(io, limits());
        writer
            .queue(cx, &frame(FrameKind::HelloAck, vec![]))
            .unwrap();
        let mut task = Context::from_waker(Waker::noop());
        assert!(writer.poll_send(cx, &mut task, |_| Ok(())).is_pending());
        assert_eq!(
            drive(|task| writer.poll_send(cx, task, |_| Err(ProtocolError::InvalidBinding))),
            Err(TransportError::Protocol(ProtocolError::InvalidBinding))
        );
        assert!(output.borrow().is_empty());
        assert!(matches!(
            writer.queue(cx, &frame(FrameKind::HelloAck, vec![])),
            Err(TransportError::Closed)
        ));
    });
}

#[test]
fn partial_cancellation_permanently_fences_old_stream() {
    with_cx(|cx| {
        let expected = frame(FrameKind::HelloAck, vec![]);
        let (io, _) = Writer::new([3, 0]);
        let mut writer = FrameWriter::new(io, limits());
        writer.queue(cx, &expected).unwrap();
        assert_eq!(
            writer.queue(cx, &expected),
            Err(TransportError::SendInProgress)
        );
        let mut task = Context::from_waker(Waker::noop());
        assert!(writer.poll_send(cx, &mut task, |_| Ok(())).is_pending());
        assert_eq!(
            writer.abandon(cx),
            Ok(AbandonedSend::FailedAfterWrite { accepted_bytes: 3 })
        );
        assert_eq!(writer.queue(cx, &expected), Err(TransportError::Closed));
    });
}

#[test]
fn unsent_cancellation_allows_next_frame_without_bytes() {
    with_cx(|cx| {
        let expected = frame(FrameKind::HelloAck, vec![]);
        let (io, output) = Writer::new([]);
        let mut writer = FrameWriter::new(io, limits());
        writer.queue(cx, &expected).unwrap();
        assert_eq!(writer.abandon(cx), Ok(AbandonedSend::CancelledBeforeWrite));
        assert!(output.borrow().is_empty());
        writer.queue(cx, &expected).unwrap();
        drive(|task| writer.poll_send(cx, task, |_| Ok(()))).unwrap();
        assert_eq!(*output.borrow(), expected.encode(limits()).unwrap());
    });
}

#[test]
fn transport_errors_are_redacted_and_terminal() {
    with_cx(|cx| {
        let (mut io, _) = Writer::new([]);
        io.error = Some(ErrorKind::BrokenPipe);
        let mut writer = FrameWriter::new(io, limits());
        writer
            .queue(cx, &frame(FrameKind::HelloAck, vec![]))
            .unwrap();
        let error = drive(|task| writer.poll_send(cx, task, |_| Ok(()))).unwrap_err();
        assert_eq!(error, TransportError::Io(ErrorKind::BrokenPipe));
        assert!(!format!("{error:?} {error}").contains("secret"));
        assert_eq!(
            drive(|task| writer.poll_send(cx, task, |_| Ok(()))),
            Err(TransportError::Closed)
        );
    });
}
