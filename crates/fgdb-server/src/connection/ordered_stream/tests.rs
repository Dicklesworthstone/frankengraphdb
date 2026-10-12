use super::*;
use crate::recovery::tests::served;
use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
use asupersync::lab::run_async_under_lab;
use std::io;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Waker};

fn accepted<T>(result: Result<T, Refusal>) -> T {
    result.unwrap_or_else(|error| panic!("{:?}: {}", error.code, error.message))
}

#[derive(Default)]
struct Wire {
    incoming: VecDeque<u8>,
    outgoing: Vec<u8>,
    closed: bool,
}
struct Io(Arc<Mutex<Wire>>);
impl AsyncRead for Io {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut wire = self.0.lock().unwrap();
        if wire.incoming.is_empty() {
            return if wire.closed {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            };
        }
        while buffer.remaining() != 0 {
            let Some(byte) = wire.incoming.pop_front() else {
                break;
            };
            buffer.put_slice(&[byte]);
        }
        Poll::Ready(Ok(()))
    }
}
impl AsyncWrite for Io {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.0.lock().unwrap().outgoing.extend_from_slice(bytes);
        Poll::Ready(Ok(bytes.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn lane(cx: &Cx, db: &Arc<Served>, token: &CapabilityToken) -> (Lane, Arc<Mutex<Wire>>, StreamId) {
    let wire = Arc::new(Mutex::new(Wire::default()));
    let (reader, writer) =
        split_duplex(cx, Box::new(Io(Arc::clone(&wire))) as Box<dyn DuplexIo>).unwrap();
    let limits = FrameLimits::new(4096).unwrap();
    let session = SessionBinding {
        transcript: [0x68; 32],
        auth_generation: 1,
    };
    let mut conn = Connection::new(1, 64).unwrap();
    conn.negotiated().unwrap();
    conn.authenticated(session).unwrap();
    conn.selected(fgdb_protocol::ReadyBinding {
        session,
        namespace: db.namespace,
        incarnation: db.incarnation,
        service_epoch: 1,
        posture: Posture::Local,
        authority_commitment: db.authority_commitment,
    })
    .unwrap();
    let stream = StreamId([0x68; 16]);
    conn.admit_child(stream, ChildKind::Query).unwrap();
    (
        Lane {
            reader: FrameReader::new(reader, limits),
            writer: FrameWriter::new(writer, limits),
            conn,
            send_limits: limits,
            initial_window: SendCost {
                bytes: 8192,
                rows: 1,
            },
            maximum_window: SendCost {
                bytes: 65536,
                rows: 100,
            },
            finished: VecDeque::new(),
            send_authority: Some((Arc::clone(db), token.clone())),
            send_generation: None,
        },
        wire,
        stream,
    )
}

fn inject(
    wire: &Arc<Mutex<Wire>>,
    kind: FrameKind,
    stream: StreamId,
    binding: Binding,
    body: &impl Body,
) {
    let limits = FrameLimits::new(4096).unwrap();
    let frame = Frame::new(kind, 1, stream, binding, body.encode().unwrap(), limits).unwrap();
    wire.lock()
        .unwrap()
        .incoming
        .extend(frame.encode(limits).unwrap());
}

struct PendingOrder {
    ready: Arc<AtomicBool>,
    polls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}
impl Future for PendingOrder {
    type Output = Result<usize, Refusal>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        if self.ready.load(Ordering::SeqCst) {
            Poll::Ready(Ok(7))
        } else {
            Poll::Pending
        }
    }
}
impl Drop for PendingOrder {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn ping_credit_and_cancel_preserve_one_pending_order_future() {
    let ((), report) = run_async_under_lab(0x79a0_0604, |root| async move {
        let (server, token, _) = served(&root, "ordered-controls").await;
        let db = &server.databases["test"];
        for cancel in [false, true] {
            let (mut lane, wire, stream) = lane(&root, db, &token);
            lane.send_generation = Some(db.db.read(&root).await.unwrap().generation());
            let binding = lane.conn.binding();
            let mut window = FlowWindow::new(
                SendCost {
                    bytes: 128,
                    rows: 0,
                },
                SendCost {
                    bytes: 8192,
                    rows: 10,
                },
            )
            .unwrap();
            let ready = Arc::new(AtomicBool::new(false));
            let polls = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let waiter = server.shutdown.waiter();
            let mut progress = Box::pin(lane.read_progress(
                &root,
                &waiter,
                &mut window,
                stream,
                1,
                binding,
                PendingOrder {
                    ready: Arc::clone(&ready),
                    polls: Arc::clone(&polls),
                    drops: Arc::clone(&drops),
                },
            ));
            assert!(poll_once(progress.as_mut()).is_pending());
            let first_polls = polls.load(Ordering::SeqCst);
            assert!(first_polls > 0);
            inject(
                &wire,
                FrameKind::Ping,
                StreamId::CONTROL,
                binding,
                &Ping { nonce: 91 },
            );
            assert!(poll_once(progress.as_mut()).is_pending());
            inject(
                &wire,
                FrameKind::WindowUpdate,
                stream,
                binding,
                &WindowUpdate {
                    sequence: 1,
                    bytes: 1024,
                    rows: 2,
                },
            );
            assert!(poll_once(progress.as_mut()).is_pending());
            assert!(polls.load(Ordering::SeqCst) > first_polls);
            assert_eq!(
                drops.load(Ordering::SeqCst),
                0,
                "control input must retain the original future"
            );
            if cancel {
                inject(&wire, FrameKind::QueryCancel, stream, binding, &Empty);
                assert!(matches!(progress.await, Err(Stop::Cancelled)));
            } else {
                ready.store(true, Ordering::SeqCst);
                assert!(matches!(progress.await, Ok(Ok(7))));
            }
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(
                window.available(),
                SendCost {
                    bytes: 1152,
                    rows: 2
                }
            );
            assert!(
                !wire.lock().unwrap().outgoing.is_empty(),
                "PING must produce output during the private sort"
            );
        }
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn pending_admission_releases_on_disconnect_drain_shutdown_and_recovery_before_schema() {
    let ((), report) = run_async_under_lab(0x79a0_0605, |root| async move {
        for stop in ["disconnect", "drain", "shutdown", "recovery"] {
            let name = format!("ordered-admission-{stop}");
            let (server, token, _) = served(&root, &name).await;
            let db = &server.databases["test"];
            let (mut lane, wire, stream) = lane(&root, db, &token);
            lane.send_generation = Some(accepted(ordered_generation(db, &token)));
            let binding = lane.conn.binding();
            let mut window = FlowWindow::new(lane.initial_window, lane.maximum_window).unwrap();
            let polls = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let waiter = server.shutdown.waiter();
            // This is the admission phase: no column chunk has announced the
            // stream and no completed source/cursor exists. The exact future
            // owner still has to be dropped when control interrupts setup.
            let mut admission = Box::pin(lane.read_progress(
                &root,
                &waiter,
                &mut window,
                stream,
                1,
                binding,
                PendingOrder {
                    ready: Arc::new(AtomicBool::new(false)),
                    polls: Arc::clone(&polls),
                    drops: Arc::clone(&drops),
                },
            ));
            assert!(poll_once(admission.as_mut()).is_pending());
            assert_eq!(polls.load(Ordering::SeqCst), 1);
            assert!(wire.lock().unwrap().outgoing.is_empty());
            match stop {
                "disconnect" => wire.lock().unwrap().closed = true,
                "drain" => inject(&wire, FrameKind::Drain, StreamId::CONTROL, binding, &Empty),
                "shutdown" => server.shutdown.trigger(),
                "recovery" => {
                    crate::recovery::tests::fail_after_marker(&root, db, 1).await;
                    assert!(matches!(
                        crate::recovery::tests::settled(db).await,
                        crate::DatabaseStatus::Ready { .. }
                    ));
                }
                _ => unreachable!(),
            }
            let result = admission.await;
            match stop {
                "disconnect" => assert!(matches!(result, Err(Stop::Transport))),
                "drain" | "shutdown" => assert!(matches!(result, Err(Stop::Drain))),
                "recovery" => assert!(matches!(result, Err(Stop::Recovery(_)))),
                _ => unreachable!(),
            }
            assert_eq!(
                polls.load(Ordering::SeqCst),
                1,
                "setup may not resume after its control terminus"
            );
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            server.join_database_workers(&root).await;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_aggregate_announces_and_services_controls_during_input_without_scratch() {
    let ((), report) = run_async_under_lab(0x79a0_0610, |root| async move {
        let (server, token, _) = served(&root, "native-aggregate-controls").await;
        let db = &server.databases["test"];
        assert!(db.ordered_reads.is_none());
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut batch = crate::recovery::tests::vertex(1);
        for id in 2..=4096 {
            batch.create_vertex(fgdb_types::VId(id), vec![], vec![]);
        }
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), batch)
            .await
            .unwrap();

        for stop in ["cancel", "recovery"] {
            let (mut lane, wire, stream) = lane(&root, db, &token);
            let binding = lane.conn.binding();
            let waiter = server.shutdown.waiter();
            let mut execution = Box::pin(lane.source_read(
                &root,
                &waiter,
                db,
                &token,
                1,
                stream,
                binding,
                SourceStatement::Text("MATCH (n) RETURN count(*) AS n"),
                &[],
            ));
            assert!(poll_once(execution.as_mut()).is_pending());
            let announced = wire.lock().unwrap().outgoing.clone();
            let mut decoder = fgdb_protocol::Decoder::new(FrameLimits::new(4096).unwrap());
            let first = decoder.decode(&announced, |_| Ok(())).unwrap();
            assert_eq!(first.consumed, announced.len());
            let frame = first.frame.expect("schema precedes source accumulation");
            assert_eq!(frame.header().kind(), FrameKind::SnapshotResultChunk);
            assert_eq!(frame.header().stream_id(), stream);
            let chunk = ResultChunk::decode(frame.payload()).unwrap();
            assert_eq!(chunk.columns, Some(vec!["n".into()]));
            assert!(chunk.rows.is_empty(), "no partial aggregate may escape");

            inject(
                &wire,
                FrameKind::Ping,
                StreamId::CONTROL,
                binding,
                &Ping { nonce: 611 },
            );
            assert!(poll_once(execution.as_mut()).is_pending());
            let outgoing = wire.lock().unwrap().outgoing.clone();
            let pong = decoder
                .decode(&outgoing[announced.len()..], |_| Ok(()))
                .unwrap();
            assert_eq!(pong.consumed, outgoing.len() - announced.len());
            let pong = pong.frame.expect("PING is serviced while count is pending");
            assert_eq!(pong.header().kind(), FrameKind::Pong);
            assert_eq!(Ping::decode(pong.payload()).unwrap().nonce, 611);

            match stop {
                "cancel" => inject(&wire, FrameKind::QueryCancel, stream, binding, &Empty),
                "recovery" => {
                    crate::recovery::tests::fail_after_marker(&root, db, 4097).await;
                    assert!(matches!(
                        crate::recovery::tests::settled(db).await,
                        crate::DatabaseStatus::Ready { .. }
                    ));
                }
                _ => unreachable!(),
            }
            let result = execution.await;
            match stop {
                "cancel" => assert!(matches!(result, Err(Stop::Cancelled))),
                "recovery" => assert!(matches!(result, Err(Stop::Recovery(_)))),
                _ => unreachable!(),
            }
            let after = wire.lock().unwrap().outgoing.clone();
            assert_eq!(after[..outgoing.len()], outgoing[..]);
            let tail = &after[outgoing.len()..];
            match stop {
                // FABRIC_PROTOCOL.md: a QUERY_CANCEL is answered with one
                // stream-scoped `cancelled` ERROR. That is the stream's only
                // terminal, and no partial aggregate row precedes it.
                "cancel" => {
                    let terminal = decoder.decode(tail, |_| Ok(())).unwrap();
                    assert_eq!(terminal.consumed, tail.len());
                    let frame = terminal.frame.expect("the cancel is answered");
                    assert_eq!(frame.header().kind(), FrameKind::Error);
                    assert_eq!(frame.header().stream_id(), stream);
                    let refusal = ErrorBody::decode(frame.payload()).unwrap();
                    assert_eq!(refusal.code, ErrorCode::Cancelled);
                }
                // Recovery retires the generation; nothing more is written.
                "recovery" => assert!(tail.is_empty()),
                _ => unreachable!(),
            }
        }
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ordered_row_admission_requires_credit_and_retains_its_one_measured_row() {
    let mut chunks = Chunks::new(vec!["value".into()]);
    assert!(!chunks.needs_row(128, 0));
    assert!(!chunks.needs_row(0, 1));
    assert!(
        accepted(chunks.next(128, 0, 128, || panic!("metadata cannot demand a row")))
            .unwrap()
            .rows
            .is_empty()
    );
    assert!(!chunks.needs_row(3, 1));
    assert!(chunks.needs_row(4, 1));
    let row = vec![WireValue::Text("one retained authenticated row".into())];
    accepted(chunks.accept(Some(row.clone()), true, 128));
    assert!(!chunks.needs_row(128, 1));
    assert!(
        accepted(chunks.next(4, 1, 128, || panic!("pending row must not be recreated"))).is_none()
    );
    let chunk =
        accepted(chunks.next(128, 1, 128, || panic!("pending row must not be recreated"))).unwrap();
    assert_eq!(chunk.rows, [row]);
    assert!(chunks.done());
}
