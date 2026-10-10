use super::*;
use crate::recovery::tests::{
    WakeCounter, WireIo, WireState, fail_after_marker, served, settled, vertex,
};
use asupersync::lab::run_async_under_lab;
use fgdb_protocol::body::{SubscriptionBatch, SubscriptionReset};
use fgdb_types::{CommitSeq, PurposeContexts};
use std::future::Future;
use std::sync::atomic::Ordering;

fn lane(
    cx: &Cx,
    db: &Arc<Served>,
    token: &CapabilityToken,
    writes: impl IntoIterator<Item = usize>,
    flush_pending: bool,
) -> (Lane, Arc<WireState>) {
    let (io, state) = WireIo::new(writes, flush_pending);
    let (reader, writer) = split_duplex(cx, Box::new(io) as Box<dyn DuplexIo>).unwrap();
    let limits = FrameLimits::new(4096).unwrap();
    let session = SessionBinding {
        transcript: [0x71; 32],
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
        state,
    )
}

fn frames(bytes: &[u8]) -> Vec<Frame> {
    let mut decoder = fgdb_protocol::Decoder::new(FrameLimits::new(4096).unwrap());
    let mut offset = 0;
    let mut result = Vec::new();
    while offset < bytes.len() {
        let part = decoder.decode(&bytes[offset..], |_| Ok(())).unwrap();
        assert!(part.consumed > 0);
        offset += part.consumed;
        result.push(
            part.frame
                .expect("the sink must contain only complete frames"),
        );
    }
    result
}

#[test]
fn recovery_resets_credit_blocked_subscriptions_at_the_last_complete_batch() {
    let ((), report) = run_async_under_lab(0x79a0_0101, |root| async move {
        for partial_baseline in [true, false] {
            let (server, token, _) = served(
                &root,
                if partial_baseline {
                    "partial-baseline"
                } else {
                    "completed-baseline"
                },
            )
            .await;
            let db = &server.databases["test"];
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let mut seed = vertex(1);
            if partial_baseline {
                seed.create_vertex(fgdb_types::VId(2), vec![], vec![]);
            }
            let seed_seq = db
                .db
                .write(&root)
                .await
                .unwrap()
                .write(&contexts.commit(), seed)
                .await
                .unwrap();
            assert_eq!(seed_seq, CommitSeq(1));
            let (mut lane, wire) = lane(&root, db, &token, [], false);
            let waiter = server.shutdown.waiter();
            let mut stream = Box::pin(lane.execute(
                &root,
                &waiter,
                db,
                &token,
                1,
                Execute {
                    mode: ExecuteMode::Subscribe,
                    statement: "SUBSCRIBE TO MATCH (n) RETURN n".into(),
                    parameters: vec![],
                },
            ));
            poll_fn(|task| {
                assert!(stream.as_mut().poll(task).is_pending());
                if wire.bytes().is_empty() {
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            })
            .await;
            let first = frames(&wire.bytes());
            assert_eq!(first.len(), 1);
            let first_batch = SubscriptionBatch::decode(first[0].payload()).unwrap();
            assert_eq!(first_batch.last, !partial_baseline);
            if !partial_baseline {
                db.db
                    .write(&root)
                    .await
                    .unwrap()
                    .write(&contexts.commit(), vertex(2))
                    .await
                    .unwrap();
                db.commits.committed();
                // The next delta has no row credit. Poll it into that wait;
                // this sink never receives a WINDOW_UPDATE or wakes its I/O.
                poll_fn(|task| {
                    assert!(stream.as_mut().poll(task).is_pending());
                    Poll::Ready(())
                })
                .await;
                assert_eq!(frames(&wire.bytes()).len(), 1);
            }
            let wakes = Arc::new(WakeCounter::default());
            let waker = std::task::Waker::from(Arc::clone(&wakes));
            assert!(
                stream
                    .as_mut()
                    .poll(&mut std::task::Context::from_waker(&waker))
                    .is_pending()
            );
            let before_wakes = wakes.0.load(Ordering::Acquire);
            fail_after_marker(&root, db, 99).await;
            assert!(
                wakes.0.load(Ordering::Acquire) > before_wakes,
                "the recovery fence must wake a stream with no flow credit or readable input"
            );
            assert!(stream.await.is_ok());
            let sent = frames(&wire.bytes());
            assert_eq!(sent.len(), 2);
            assert_eq!(sent[1].header().kind(), FrameKind::SubscriptionReset);
            assert_eq!(sent[1].header().stream_id(), sent[0].header().stream_id());
            assert_eq!(
                SubscriptionReset::decode(sent[1].payload())
                    .unwrap()
                    .last_delivered_seq,
                if partial_baseline { None } else { Some(1) }
            );
            assert_eq!(lane.conn.children_in_flight(), 0);
            assert_eq!(lane.conn.sends_in_flight(), 0);
            assert!(matches!(
                settled(db).await,
                crate::DatabaseStatus::Ready { generation: 2 }
            ));
            server.join_database_workers(&root).await;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn generation_fence_cancels_unwritten_frames_but_closes_partial_frames_and_flushes() {
    let ((), report) = run_async_under_lab(0x79a0_0102, |root| async move {
        for (name, writes, flush_pending, unwritten) in [
            ("unwritten-frame", vec![0], false, true),
            ("partial-frame", vec![7, 0], false, false),
            ("pending-flush", vec![], true, false),
        ] {
            let (server, token, _) = served(&root, name).await;
            let db = &server.databases["test"];
            let (mut lane, wire) = lane(&root, db, &token, writes, flush_pending);
            lane.send_generation = Some(db.db.generation().unwrap());
            let frame = Frame::new(
                FrameKind::SnapshotResultChunk,
                1,
                StreamId([9; 16]),
                lane.conn.binding(),
                ResultChunk {
                    columns: Some(vec!["n".into()]),
                    rows: vec![vec![WireValue::Int(17)]],
                }
                .encode()
                .unwrap(),
                lane.send_limits,
            )
            .unwrap();
            let mut send = Box::pin(lane.send_generation_frame(&root, &frame));
            let wakes = Arc::new(WakeCounter::default());
            let waker = std::task::Waker::from(Arc::clone(&wakes));
            assert!(
                send.as_mut()
                    .poll(&mut std::task::Context::from_waker(&waker))
                    .is_pending()
            );
            let before = wire.bytes();
            assert_eq!(before.is_empty(), unwritten);
            let writes = wire.writes.load(Ordering::Acquire);
            let flushes = wire.flushes.load(Ordering::Acquire);
            let before_wakes = wakes.0.load(Ordering::Acquire);
            fail_after_marker(&root, db, 21).await;
            assert!(
                wakes.0.load(Ordering::Acquire) > before_wakes,
                "fencing must wake a blocked physical send without socket readiness"
            );
            let stop = send.await.err().expect("the original generation must stop");
            assert_eq!(matches!(stop, Stop::Recovery(_)), unwritten);
            assert_eq!(wire.bytes(), before);
            assert_eq!(wire.writes.load(Ordering::Acquire), writes);
            assert_eq!(wire.flushes.load(Ordering::Acquire), flushes);
            if unwritten {
                // A terminal control owes live Warden authority, not the
                // invalidated generation. The original write stays unknown.
                assert!(
                    lane.refuse(
                        &root,
                        1,
                        StreamId([9; 16]),
                        lane.conn.binding(),
                        ErrorCode::OutcomeUnknown,
                        "commit outcome is unknown; do not replay"
                    )
                    .await
                    .is_ok()
                );
                let sent = frames(&wire.bytes());
                assert_eq!(sent.len(), 1);
                assert_eq!(
                    ErrorBody::decode(sent[0].payload()).unwrap().code,
                    ErrorCode::OutcomeUnknown
                );
            } else {
                assert!(!lane.send_frame(&root, &frame).await);
                assert_eq!(wire.bytes(), before);
            }
            assert_eq!(lane.conn.sends_in_flight(), 0);
            settled(db).await;
            server.join_database_workers(&root).await;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
