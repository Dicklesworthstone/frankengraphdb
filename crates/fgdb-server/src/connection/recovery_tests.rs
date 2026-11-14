use super::*;
use crate::recovery::tests::{
    WakeCounter, WireIo, WireState, fail_after_marker, served, settled, vertex,
};
use asupersync::lab::run_async_under_lab;
use fgdb_protocol::body::{ReadBatchResultChunk, SubscriptionBatch, SubscriptionReset};
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
    lane_with_limits(
        cx,
        db,
        token,
        writes,
        flush_pending,
        FrameLimits::new(4096).unwrap(),
    )
}

fn lane_with_limits(
    cx: &Cx,
    db: &Arc<Served>,
    token: &CapabilityToken,
    writes: impl IntoIterator<Item = usize>,
    flush_pending: bool,
    limits: FrameLimits,
) -> (Lane, Arc<WireState>) {
    let (io, state) = WireIo::new(writes, flush_pending);
    let (reader, writer) = split_duplex(cx, Box::new(io) as Box<dyn DuplexIo>).unwrap();
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
    frames_with_limits(bytes, FrameLimits::new(4096).unwrap())
}

fn frames_with_limits(bytes: &[u8], limits: FrameLimits) -> Vec<Frame> {
    let mut decoder = fgdb_protocol::Decoder::new(limits);
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
fn subscription_schema_and_exact_frame_entry_can_use_separate_fragments() {
    let ((), report) = run_async_under_lab(0x79a0_0110, |root| async move {
        let (server, token, _) = served(&root, "subscription-schema-fragment").await;
        let db = &server.databases["test"];
        for oversized in [false, true] {
            let (mut lane, wire) = lane(&root, db, &token, [], false);
            let binding = lane.conn.binding();
            let stream = StreamId([0x39; 16]);
            let child = lane.conn.admit_child(stream, ChildKind::Subscription).unwrap();
            lane.send_generation = Some(db.db.generation().unwrap());
            let waiter = server.shutdown.waiter();
            let mut window = FlowWindow::new(lane.initial_window, lane.maximum_window).unwrap();
            let framing = binding.header_len() + 15;
            let budget = lane.send_limits.max_frame_len() - framing;
            // The entry is exactly weight(16) + row width(4) + text tag(1)
            // + text length(4) + payload. Its column list fits separately.
            let text = "x".repeat(budget - 25 + usize::from(oversized));
            let columns = vec!["payload".repeat(100)];
            let mut pending_columns = Some(columns.clone());
            let result = lane
                .send_batch(
                    &root,
                    &waiter,
                    &mut window,
                    1,
                    stream,
                    binding,
                    7,
                    true,
                    &mut pending_columns,
                    vec![(1, vec![WireValue::Text(text.clone())])],
                )
                .await;
            let sent = frames(&wire.bytes());
            if oversized {
                assert!(matches!(result, Err(Stop::Cancelled)));
                assert_eq!(sent.len(), 1, "no baseline prefix before refusal");
                assert_eq!(sent[0].header().kind(), FrameKind::Error);
                assert_eq!(
                    ErrorBody::decode(sent[0].payload()).unwrap().code,
                    ErrorCode::Execution
                );
                assert_eq!(pending_columns, Some(columns));
            } else {
                assert!(result.is_ok());
                assert_eq!(sent.len(), 2);
                assert!(sent.iter().all(|frame| {
                    frame.header().kind() == FrameKind::SubscriptionBatch
                        && frame.header().stream_id() == stream
                        && frame.header().request_id() == 1
                }));
                let schema = SubscriptionBatch::decode(sent[0].payload()).unwrap();
                assert_eq!(schema.columns, Some(columns));
                assert!(schema.entries.is_empty());
                assert!(!schema.last);
                let data = SubscriptionBatch::decode(sent[1].payload()).unwrap();
                assert!(data.columns.is_none());
                assert_eq!(data.entries, [(1, vec![WireValue::Text(text)])]);
                assert!(data.last);
                assert!(schema.snapshot && data.snapshot);
                assert_eq!((schema.frontier, data.frontier), (7, 7));
                assert_eq!(sent[1].header().frame_len(), 4096);
                assert!(pending_columns.is_none());
                assert_eq!(
                    window.available(),
                    SendCost {
                        bytes: lane.initial_window.bytes
                            - sent.iter().map(|frame| frame.header().frame_len() as u64).sum::<u64>(),
                        rows: 0,
                    }
                );
            }
            lane.conn.child_terminal(stream, child, ChildTerminus::EphemeralCompleted).unwrap();
            assert_eq!(lane.conn.sends_in_flight(), 0);
        }
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn subscription_fragments_obey_body_node_and_entry_limits_with_large_flow_credit() {
    let ((), report) = run_async_under_lab(0x79a0_0111, |root| async move {
        let (server, token, _) = served(&root, "subscription-body-fragments").await;
        let db = &server.databases["test"];
        for node_bound in [true, false] {
            let limits = FrameLimits::new(80 << 20).unwrap();
            let (mut lane, wire) = lane_with_limits(&root, db, &token, [], false, limits);
            lane.initial_window = SendCost {
                bytes: 160 << 20,
                rows: MAX_ROWS_PER_CHUNK as u64 + 2,
            };
            lane.maximum_window = lane.initial_window;
            let binding = lane.conn.binding();
            let stream = StreamId([0x3a; 16]);
            let child = lane.conn.admit_child(stream, ChildKind::Subscription).unwrap();
            lane.send_generation = Some(db.db.generation().unwrap());
            let waiter = server.shutdown.waiter();
            let mut window = FlowWindow::new(lane.initial_window, lane.maximum_window).unwrap();
            let (mut columns, entries) = if node_bound {
                // Both packed lists fit separately; their roots make the
                // combined node count exceed the body limit by exactly two.
                (
                    Some(vec!["ids".to_owned()]),
                    vec![
                        (3, vec![WireValue::Vertices(vec![1; MAX_VALUE_NODES / 2])]),
                        (-2, vec![WireValue::Edges(vec![2; MAX_VALUE_NODES / 2])]),
                    ],
                )
            } else {
                (Some(Vec::new()), vec![(1, Vec::new()); MAX_ROWS_PER_CHUNK + 1])
            };
            let expected_rows = entries.len() as u64;
            assert!(lane
                .send_batch(
                    &root, &waiter, &mut window, 1, stream, binding, 9, false,
                    &mut columns, entries,
                )
                .await
                .is_ok());
            let sent = frames_with_limits(&wire.bytes(), limits);
            assert_eq!(sent.len(), 2);
            let mut delivered = 0;
            for (index, frame) in sent.iter().enumerate() {
                assert_eq!(frame.header().kind(), FrameKind::SubscriptionBatch);
                assert_eq!(frame.header().stream_id(), stream);
                let part = SubscriptionBatch::decode(frame.payload()).unwrap();
                assert_eq!(part.frontier, 9);
                assert!(!part.snapshot);
                assert_eq!(part.last, index == 1);
                assert_eq!(part.columns.is_some(), index == 0);
                delivered += part.entries.len() as u64;
                if node_bound {
                    assert_eq!(part.entries.len(), 1);
                    assert_eq!(part.entries[0].0, if index == 0 { 3 } else { -2 });
                    match &part.entries[0].1[..] {
                        [WireValue::Vertices(ids)] if index == 0 => {
                            assert_eq!(ids.len(), MAX_VALUE_NODES / 2);
                            assert!(ids.iter().all(|id| *id == 1));
                        }
                        [WireValue::Edges(ids)] if index == 1 => {
                            assert_eq!(ids.len(), MAX_VALUE_NODES / 2);
                            assert!(ids.iter().all(|id| *id == 2));
                        }
                        other => panic!("unexpected subscription fragment: {other:?}"),
                    }
                } else {
                    assert_eq!(part.entries.len(), if index == 0 { MAX_ROWS_PER_CHUNK } else { 1 });
                    assert!(part.entries.iter().all(|(weight, row)| *weight == 1 && row.is_empty()));
                }
            }
            assert_eq!(delivered, expected_rows);
            assert_eq!(window.available().rows, lane.initial_window.rows - expected_rows);
            lane.conn.child_terminal(stream, child, ChildTerminus::EphemeralCompleted).unwrap();
            assert_eq!(lane.conn.sends_in_flight(), 0);
        }
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Feed real FGP frames through the complete connection dispatcher while the
/// test controls the writer lock independently. Writes use the existing
/// transport sink; reads remain pending when no frame has been supplied.
struct SelectionIo {
    input: Arc<std::sync::Mutex<VecDeque<u8>>>,
    output: WireIo,
}

impl asupersync::io::AsyncRead for SelectionIo {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        buffer: &mut asupersync::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let mut input = self.input.lock().unwrap();
        if input.is_empty() {
            return Poll::Pending;
        }
        while buffer.remaining() != 0 {
            let Some(byte) = input.pop_front() else {
                break;
            };
            buffer.put_slice(&[byte]);
        }
        Poll::Ready(Ok(()))
    }
}

impl asupersync::io::AsyncWrite for SelectionIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        task: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.output).poll_write(task, bytes)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        task: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.output).poll_flush(task)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        task: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.output).poll_shutdown(task)
    }
}

#[test]
fn fresh_connection_selection_and_reads_use_the_published_frontier_during_a_write() {
    let ((), report) = run_async_under_lab(0x79a0_0108, |root| async move {
        let (server, token, _) = served(&root, "fresh-selection-overlap").await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), vertex(1))
            .await
            .unwrap();

        // There is no pinned reader. Holding precisely the lock retained by
        // commit I/O makes a lock-dependent SELECT_DATABASE deterministically
        // stall, without timing assumptions or a scheduler race.
        let held = db.db.write(&root).await.unwrap();
        let input = Arc::new(std::sync::Mutex::new(VecDeque::new()));
        let (output, wire) = WireIo::new([], false);
        let mut connection = run(
            &root,
            &server,
            Box::new(SelectionIo {
                input: Arc::clone(&input),
                output,
            }),
        );
        let limits = FrameLimits::new(4096).unwrap();
        let inject = |kind, request, binding, body: Vec<u8>| {
            let frame =
                Frame::new(kind, request, StreamId::CONTROL, binding, body, limits).unwrap();
            input.lock().unwrap().extend(frame.encode(limits).unwrap());
        };
        let advance = |connection: Pin<&mut (dyn Future<Output = ()> + Send)>| {
            assert!(
                connection
                    .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                    .is_pending(),
                "the connection must remain ready for its next request"
            );
        };
        inject(
            FrameKind::Hello,
            1,
            Binding::Transport,
            Hello {
                min_version: crate::SERVED_VERSION,
                max_version: crate::SERVED_VERSION,
                client_nonce: [0x18; 32],
                max_frame_len: 4096,
            }
            .encode()
            .unwrap(),
        );
        inject(
            FrameKind::Auth,
            2,
            Binding::Transport,
            Auth {
                credential: Credential::WardenCapability(token.encode()),
            }
            .encode()
            .unwrap(),
        );
        advance(connection.as_mut());
        let sent = frames(&wire.bytes());
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].header().kind(), FrameKind::HelloAck);
        assert_eq!(sent[1].header().kind(), FrameKind::AuthOk);
        let session = AuthOk::decode(sent[1].payload()).unwrap().session;

        // Unknown databases still refuse before touching graph state and do
        // not prevent a later authorized selection on the same connection.
        for (request, name) in [(3, "absent"), (4, "test")] {
            inject(
                FrameKind::SelectDatabase,
                request,
                Binding::Session(session),
                SelectDatabase { name: name.into() }.encode().unwrap(),
            );
            advance(connection.as_mut());
        }
        let sent = frames(&wire.bytes());
        assert_eq!(sent.len(), 4, "READY must not wait behind the writer");
        assert_eq!(sent[2].header().kind(), FrameKind::Error);
        assert_eq!(
            ErrorBody::decode(sent[2].payload()).unwrap().code,
            ErrorCode::NotFoundOrUnauthorized
        );
        assert_eq!(sent[3].header().kind(), FrameKind::Ready);
        let ready = Ready::decode(sent[3].payload()).unwrap();
        assert_eq!(ready.frontier, 1);
        assert_eq!(ready.namespace, db.namespace);
        let binding = Binding::Ready(ready.binding(session));

        let read = Execute {
            mode: ExecuteMode::Read,
            statement: "MATCH (n) RETURN 7 AS value".into(),
            parameters: vec![],
        };
        inject(FrameKind::Execute, 5, binding, read.encode().unwrap());
        advance(connection.as_mut());
        let sent = frames(&wire.bytes());
        assert_eq!(sent.len(), 6, "the first read must also bypass the writer");
        assert_eq!(
            ResultChunk::decode(sent[4].payload()).unwrap().rows,
            vec![vec![WireValue::Int(7)]]
        );
        assert_eq!(
            ResultEnd::decode(sent[5].payload()).unwrap().outcome,
            Outcome::Rows { seq: 1 }
        );
        drop(held);

        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), vertex(2))
            .await
            .unwrap();
        inject(FrameKind::Execute, 6, binding, read.encode().unwrap());
        advance(connection.as_mut());
        let sent = frames(&wire.bytes());
        assert_eq!(sent.len(), 8);
        assert_eq!(
            ResultChunk::decode(sent[6].payload()).unwrap().rows,
            vec![vec![WireValue::Int(7)], vec![WireValue::Int(7)]]
        );
        assert_eq!(
            ResultEnd::decode(sent[7].payload()).unwrap().outcome,
            Outcome::Rows { seq: 2 },
            "selection must not pin subsequent reads to its old frontier"
        );
        drop(connection);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
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
fn recovery_refuses_consistent_read_batches_before_send_and_while_credit_blocked() {
    let ((), report) = run_async_under_lab(0x79a0_0109, |root| async move {
        for (before_first_byte, prepared) in [(true, false), (false, false), (true, true), (false, true)] {
            let (server, token, _) = served(
                &root,
                match (before_first_byte, prepared) {
                    (true, false) => "read-batch-unwritten",
                    (false, false) => "read-batch-credit",
                    (true, true) => "prepared-read-batch-unwritten",
                    (false, true) => "prepared-read-batch-credit",
                },
            )
            .await;
            let db = &server.databases["test"];
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            db.db
                .write(&root)
                .await
                .unwrap()
                .write(&contexts.commit(), vertex(1))
                .await
                .unwrap();
            let (mut lane, wire) = lane(
                &root,
                db,
                &token,
                if before_first_byte { vec![0] } else { vec![] },
                false,
            );
            let waiter = server.shutdown.waiter();
            let statement = if prepared {
                let definition = crate::execute::prepare_read(
                    &root,
                    db,
                    &token,
                    &Prepare {
                        mode: fgdb_protocol::body::PrepareMode::Read,
                        statement: "MATCH (n) RETURN 7 AS value".into(),
                        parameters: vec![],
                    },
                )
                .await
                .unwrap_or_else(|error| panic!("{}", error.message));
                Statement::PreparedReadBatch {
                    definition: Ok(Arc::new(PreparedDefinition::from(definition))),
                    argument_sets: vec![vec![], vec![]],
                }
            } else {
                Statement::ReadBatch(ExecuteBatch {
                    statement: "MATCH (n) RETURN 7 AS value".into(),
                    argument_sets: vec![vec![], vec![]],
                })
            };
            let mut execution = Box::pin(lane.execute_statement(
                &root,
                &waiter,
                db,
                &token,
                1,
                statement,
            ));
            poll_fn(|task| {
                assert!(execution.as_mut().poll(task).is_pending());
                if wire.writes.load(Ordering::Acquire) > 0 {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            let before = frames(&wire.bytes());
            if before_first_byte {
                assert!(before.is_empty());
            } else {
                assert!(!before.is_empty());
                assert!(
                    before
                        .iter()
                        .all(|frame| frame.header().kind() == FrameKind::ReadBatchResultChunk)
                );
                assert!(before.iter().all(|frame| {
                    ReadBatchResultChunk::decode(frame.payload()).unwrap().seq == 1
                }));
            }
            // The batch released its execution locks after complete native
            // admission. A failed concurrent write can therefore retire its
            // generation even while delivery waits for I/O or row credit.
            fail_after_marker(&root, db, 99).await;
            assert!(execution.await.is_ok());
            let sent = frames(&wire.bytes());
            assert_eq!(sent.len(), before.len() + 1);
            let terminal = sent.last().unwrap();
            assert_eq!(terminal.header().kind(), FrameKind::Error);
            assert_eq!(terminal.header().request_id(), 1);
            assert!(!terminal.header().stream_id().is_control());
            assert!(matches!(
                ErrorBody::decode(terminal.payload()).unwrap().code,
                ErrorCode::DatabaseRecovering | ErrorCode::DatabaseUnavailable,
            ));
            assert!(
                !sent
                    .iter()
                    .any(|frame| frame.header().kind() == FrameKind::ReadBatchResultEnd)
            );
            assert_eq!(lane.conn.children_in_flight(), 0);
            assert_eq!(lane.conn.sends_in_flight(), 0);
            settled(db).await;
            server.join_database_workers(&root).await;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn committed_atomic_batch_becomes_unknown_if_recovery_precedes_its_first_output_byte() {
    let ((), report) = run_async_under_lab(0x79a0_0103, |root| async move {
        let (server, token, _) = served(&root, "committed-batch-output-fence").await;
        let db = &server.databases["test"];
        let (mut lane, wire) = lane(&root, db, &token, [0], false);
        let waiter = server.shutdown.waiter();
        let mut execution = Box::pin(lane.execute_statement(
            &root,
            &waiter,
            db,
            &token,
            1,
            Statement::Batch(ExecuteBatch {
                statement: "CREATE ()".into(),
                argument_sets: vec![vec![], vec![]],
            }),
        ));
        poll_fn(|task| {
            assert!(execution.as_mut().poll(task).is_pending());
            if wire.writes.load(Ordering::Acquire) > 0 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        assert!(wire.bytes().is_empty());
        {
            let guard = db.db.read(&root).await.unwrap();
            assert_eq!(guard.frontier().unwrap(), CommitSeq(1));
            assert_eq!(guard.vertices().unwrap().len(), 2);
        }
        fail_after_marker(&root, db, 777).await;
        assert!(execution.await.is_ok());
        let sent = frames(&wire.bytes());
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].header().kind(), FrameKind::Error);
        let refusal = ErrorBody::decode(sent[0].payload()).unwrap();
        assert_eq!(refusal.code, ErrorCode::OutcomeUnknown);
        assert!(refusal.message.contains("do not replay"));
        assert_eq!(lane.conn.children_in_flight(), 0);
        assert_eq!(lane.conn.sends_in_flight(), 0);
        settled(db).await;
        {
            let guard = db.db.read(&root).await.unwrap();
            assert_eq!(guard.frontier().unwrap(), CommitSeq(2));
            assert_eq!(
                guard.vertices().unwrap().len(),
                3,
                "the batch committed exactly once"
            );
        }
        server.join_database_workers(&root).await;
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
            let stop = send.await.expect_err("the original generation must stop");
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

#[test]
fn explicit_transaction_native_refusal_is_fenced_and_discards_staged_prefix() {
    let ((), report) = run_async_under_lab(0x79a0_0112, |root| async move {
        for recovery in [false, true] {
            let name = if recovery {
                "transaction-refusal-recovery"
            } else {
                "transaction-refusal-current"
            };
            let (server, token, _) = served(&root, name).await;
            let db = &server.databases["test"];
            // BEGIN, the provisional schema and its END complete. The next
            // native refusal blocks before any byte, without timing races.
            let writes = if recovery {
                vec![usize::MAX, usize::MAX, usize::MAX, 0]
            } else {
                vec![]
            };
            let (mut lane, wire) = lane(&root, db, &token, writes, false);
            let waiter = server.shutdown.waiter();
            let binding = lane.conn.binding();
            let mut active = None;
            let request = |kind, id, body| {
                Frame::new(
                    kind, id, StreamId::CONTROL, binding, body,
                    FrameLimits::new(4096).unwrap(),
                ).unwrap()
            };
            let begin = request(FrameKind::BeginTransaction, 1, Empty.encode().unwrap());
            assert!(lane
                .transaction_request(&root, &waiter, db, &token, &mut active, &begin)
                .await
                .is_ok());
            let sent = frames(&wire.bytes());
            assert_eq!(sent.len(), 1);
            let begun = fgdb_protocol::body::TransactionBegun::decode(sent[0].payload()).unwrap();
            let statement = |mode| fgdb_protocol::body::ExecuteTransaction {
                handle: begun.handle,
                statement: Execute {
                    mode,
                    statement: "CREATE (n)".to_owned(),
                    parameters: vec![],
                },
            };
            let write = request(
                FrameKind::ExecuteTransaction, 2,
                statement(ExecuteMode::Write).encode().unwrap(),
            );
            assert!(lane
                .transaction_request(&root, &waiter, db, &token, &mut active, &write)
                .await
                .is_ok());
            let sent = frames(&wire.bytes());
            assert_eq!(sent.len(), 3);
            assert_eq!(sent[1].header().kind(), FrameKind::TransactionResultChunk);
            assert_eq!(sent[2].header().kind(), FrameKind::TransactionResultEnd);
            assert!(active.is_some());
            assert_eq!(db.db.read(&root).await.unwrap().frontier().unwrap(), CommitSeq(0));

            let refused = request(
                FrameKind::ExecuteTransaction, 3,
                statement(ExecuteMode::Read).encode().unwrap(),
            );
            let mut execution = Box::pin(lane.transaction_request(
                &root, &waiter, db, &token, &mut active, &refused,
            ));
            if recovery {
                poll_fn(|task| {
                    assert!(execution.as_mut().poll(task).is_pending());
                    if wire.writes.load(Ordering::Acquire) > 3 {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                }).await;
                assert_eq!(frames(&wire.bytes()).len(), 3);
                fail_after_marker(&root, db, 777).await;
            }
            assert!(execution.await.is_ok());
            assert!(active.is_none(), "a refused statement cannot retain its staged prefix");
            let sent = frames(&wire.bytes());
            assert_eq!(sent.len(), 4);
            let last = sent.last().unwrap();
            assert_eq!(last.header().kind(), FrameKind::Error);
            assert_eq!(last.header().request_id(), 3);
            let error = ErrorBody::decode(last.payload()).unwrap();
            if recovery {
                assert!(matches!(
                    error.code,
                    ErrorCode::DatabaseRecovering | ErrorCode::DatabaseUnavailable,
                ));
                assert!(!error.message.contains("Read request"));
                settled(db).await;
            } else {
                assert_eq!(error.code, ErrorCode::PermissionDenied);
                assert!(error.message.contains("Read request"));
            }
            let guard = db.db.read(&root).await.unwrap();
            assert_eq!(guard.frontier().unwrap(), CommitSeq(u64::from(recovery)));
            assert_eq!(guard.vertices().unwrap().len(), usize::from(recovery));
            drop(guard);
            assert_eq!(lane.conn.children_in_flight(), 0);
            assert_eq!(lane.conn.sends_in_flight(), 0);
            server.join_database_workers(&root).await;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn transaction_refusal_fence_closes_partial_output_but_preserves_unknown_outcomes() {
    let ((), report) = run_async_under_lab(0x79a0_0113, |root| async move {
        for flush_pending in [false, true] {
            let name = if flush_pending {
                "transaction-refusal-flush"
            } else {
                "transaction-refusal-partial"
            };
            let (server, token, _) = served(&root, name).await;
            let db = &server.databases["test"];
            let generation = db.db.generation().unwrap();
            let writes = if flush_pending { vec![] } else { vec![3, 0] };
            let (mut lane, wire) = lane(&root, db, &token, writes, flush_pending);
            let binding = lane.conn.binding();
            let mut sending = Box::pin(lane.refuse_transaction(
                &root,
                1,
                StreamId::CONTROL,
                binding,
                generation.clone(),
                Refusal::new(ErrorCode::Conflict, "private transaction diagnostic"),
            ));
            poll_fn(|task| {
                assert!(sending.as_mut().poll(task).is_pending());
                if wire.writes.load(Ordering::Acquire) > 0 {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }).await;
            let before = wire.bytes();
            if flush_pending {
                assert_eq!(frames(&before).len(), 1);
                assert!(wire.flushes.load(Ordering::Acquire) > 0);
            } else {
                assert_eq!(before.len(), 3);
            }
            fail_after_marker(&root, db, 778).await;
            assert!(matches!(sending.await, Err(Stop::Transport)));
            assert_eq!(wire.bytes(), before, "no replacement can follow a partial diagnostic");
            assert_eq!(lane.conn.sends_in_flight(), 0);
            settled(db).await;

            // An outcome control is deliberately independent of the retired
            // result generation. Suppressing it would lose the no-replay warning.
            let (mut terminal_lane, terminal_wire) = self::lane(&root, db, &token, [], false);
            let binding = terminal_lane.conn.binding();
            assert!(terminal_lane
                .refuse_transaction(
                    &root,
                    2,
                    StreamId::CONTROL,
                    binding,
                    generation,
                    Refusal::new(ErrorCode::OutcomeUnknown, "outcome unknown; do not replay"),
                )
                .await
                .is_ok());
            let terminal = frames(&terminal_wire.bytes());
            assert_eq!(terminal.len(), 1);
            let error = ErrorBody::decode(terminal[0].payload()).unwrap();
            assert_eq!(error.code, ErrorCode::OutcomeUnknown);
            assert_eq!(error.message, "outcome unknown; do not replay");
            assert_eq!(terminal_lane.conn.sends_in_flight(), 0);
            server.join_database_workers(&root).await;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
