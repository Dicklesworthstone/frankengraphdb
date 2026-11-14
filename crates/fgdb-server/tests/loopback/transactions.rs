//! Explicit native transactions over real FGP sockets. Provisional statements
//! share one signed allowance and publish only at the separate COMMIT boundary.

use super::*;
use asupersync::io::{AsyncRead, AsyncWrite};
use fgdb_protocol::body::{
    Auth, AuthOk, AuthRefresh, Body, Credential, Empty, ErrorBody, Execute,
    ExecuteTransaction, Hello, Ready, ResultChunk, SelectDatabase, TransactionBegun,
    TransactionResultEnd, TransactionSelector,
};
use fgdb_protocol::transport::{FrameReader, FrameWriter};
use fgdb_protocol::{Binding, Frame, FrameKind, FrameLimits, StreamId};

fn statement(mode: ExecuteMode, text: &str) -> Execute {
    Execute {
        mode,
        statement: text.into(),
        parameters: vec![],
    }
}

async fn connect(cx: &Cx, addr: SocketAddr, scope: &Grant) -> Client {
    let mut client = Client::connect(cx, addr, token(scope)).await.unwrap();
    client.select(cx, "social").await.unwrap();
    client
}

async fn ages(cx: &Cx, client: &mut Client) -> Vec<Vec<WireValue>> {
    client
        .execute(
            cx,
            ExecuteMode::Read,
            "MATCH (n:Person) RETURN n.age AS age ORDER BY age",
            vec![],
        )
        .await
        .unwrap()
        .rows
}

#[test]
fn explicit_statements_share_staged_state_and_publish_one_atomic_commit() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "explicit-transaction-atomic").await;
        let mut writer = connect(cx, addr, &grant(Rights::ReadWrite)).await;
        let mut observer = connect(cx, addr, &grant(Rights::Read)).await;
        let begun = writer.begin_transaction(cx).await.unwrap();
        assert_eq!(begun.basis, 0);
        let inserted = writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(
                    ExecuteMode::Write,
                    "UNWIND range(1,6) AS age CREATE (:Person {age:age})",
                ),
            )
            .await
            .unwrap();
        assert!(inserted.rows.is_empty());
        assert_eq!(inserted.end.handle, begun.handle);
        assert_eq!(inserted.end.statement, 1);
        assert_eq!(inserted.end.rows, 0);
        assert!(ages(cx, &mut observer).await.is_empty());

        // Six rows require more than the fixture's four-row initial credit.
        // The owner reads its own overlay while the second connection sees no
        // graph changes and no extra commit sequence.
        let selected = writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(
                    ExecuteMode::Read,
                    "MATCH (n:Person) RETURN n.age AS age ORDER BY age",
                ),
            )
            .await
            .unwrap();
        assert_eq!(selected.columns, ["age"]);
        assert_eq!(
            selected.rows,
            (1..=6).map(|age| vec![WireValue::Int(age)]).collect::<Vec<_>>()
        );
        assert_eq!(selected.end.statement, 2);
        assert_eq!(selected.end.rows, 6);
        let updated = writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(
                    ExecuteMode::Write,
                    "MATCH (n:Person) WHERE n.age=6 SET n.age=60 RETURN n.age AS age",
                ),
            )
            .await
            .unwrap();
        assert_eq!(updated.rows, [vec![WireValue::Int(60)]]);
        assert_eq!(updated.end.statement, 3);
        assert!(ages(cx, &mut observer).await.is_empty());
        assert_eq!(
            writer.commit_transaction(cx, begun.handle).await.unwrap(),
            Outcome::WriteCommitted {
                seq: 1,
                statements: 3,
            }
        );
        let expected: Vec<_> = [1, 2, 3, 4, 5, 60]
            .into_iter()
            .map(|age| vec![WireValue::Int(age)])
            .collect();
        assert_eq!(ages(cx, &mut observer).await, expected);

        // A read-only use of the write-capable owner closes without inventing
        // a write marker or advancing the published graph frontier.
        let begun = writer.begin_transaction(cx).await.unwrap();
        assert_eq!(begun.basis, 1);
        let count = writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(ExecuteMode::Read, "MATCH (n:Person) RETURN count(*) AS n"),
            )
            .await
            .unwrap();
        assert_eq!(count.rows, [vec![WireValue::Count(6)]]);
        assert_eq!(
            writer.commit_transaction(cx, begun.handle).await.unwrap(),
            Outcome::ReadClosed {
                seq: 1,
                statements: 1,
            }
        );

        let begun = writer.begin_transaction(cx).await.unwrap();
        writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(ExecuteMode::Write, "MATCH (n:Person) SET n.age=99"),
            )
            .await
            .unwrap();
        writer.rollback_transaction(cx, begun.handle).await.unwrap();
        assert_eq!(ages(cx, &mut observer).await, expected);
        writer.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn statement_errors_and_cumulative_delivery_limits_abort_the_whole_prefix() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "explicit-transaction-failures").await;
        let mut writer = connect(cx, addr, &grant(Rights::ReadWrite)).await;
        let mut observer = connect(cx, addr, &grant(Rights::Read)).await;
        let begun = writer.begin_transaction(cx).await.unwrap();
        writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(ExecuteMode::Write, "CREATE (:Person {age:10})"),
            )
            .await
            .unwrap();
        let error = writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(ExecuteMode::Write, "CREATE (:Person {age:1/0})"),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            server_code(error),
            ErrorCode::Statement | ErrorCode::Execution
        ));
        assert!(ages(cx, &mut observer).await.is_empty());
        // Definite statement failure ends the owner; a fresh BEGIN succeeds.
        let next = writer.begin_transaction(cx).await.unwrap();
        assert_eq!(next.basis, 0);
        writer.rollback_transaction(cx, next.handle).await.unwrap();

        let mut scope = grant(Rights::ReadWrite);
        scope.limits.max_rows = 1;
        let mut limited = connect(cx, addr, &scope).await;
        let begun = limited.begin_transaction(cx).await.unwrap();
        limited
            .execute_transaction(
                cx,
                begun.handle,
                &statement(ExecuteMode::Write, "CREATE (:Person {age:20})"),
            )
            .await
            .unwrap();
        let first = limited
            .execute_transaction(
                cx,
                begun.handle,
                &statement(ExecuteMode::Read, "RETURN 1 AS value"),
            )
            .await
            .unwrap();
        assert_eq!(first.rows, [vec![WireValue::Int(1)]]);
        assert_eq!(
            server_code(
                limited
                    .execute_transaction(
                        cx,
                        begun.handle,
                        &statement(ExecuteMode::Read, "RETURN 2 AS value"),
                    )
                    .await
                    .unwrap_err()
            ),
            ErrorCode::Budget
        );
        assert!(ages(cx, &mut observer).await.is_empty());

        // Closing the socket with an uncommitted prefix also drops its owner.
        let begun = writer.begin_transaction(cx).await.unwrap();
        writer
            .execute_transaction(
                cx,
                begun.handle,
                &statement(ExecuteMode::Write, "CREATE (:Person {age:30})"),
            )
            .await
            .unwrap();
        drop(writer);
        assert!(ages(cx, &mut observer).await.is_empty());
        limited.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

#[test]
fn provisional_callback_cancellation_aborts_before_and_after_end_can_fit() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "explicit-transaction-cancel").await;
        let mut writer = connect(cx, addr, &grant(Rights::ReadWrite)).await;
        let mut observer = connect(cx, addr, &grant(Rights::Read)).await;
        for count in [1, 12] {
            let begun = writer.begin_transaction(cx).await.unwrap();
            writer
                .execute_transaction(
                    cx,
                    begun.handle,
                    &statement(ExecuteMode::Write, "CREATE (:Person {age:50})"),
                )
                .await
                .unwrap();
            let query = statement(
                ExecuteMode::Read,
                &format!("UNWIND range(1,{count}) AS n RETURN n"),
            );
            let mut calls = 0;
            let error = writer
                .execute_transaction_streaming(
                    cx,
                    begun.handle,
                    &query,
                    |_| Ok(()),
                    |_| {
                        calls += 1;
                        Err(ClientError::Protocol("application rejected provisional row"))
                    },
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                ClientError::Protocol("application rejected provisional row")
            ));
            assert_eq!(calls, 1);
            // count=1 fits together with END in the initial flow allowance;
            // its QUERY_CANCEL can arrive after the server restores the owner.
            // The next BEGIN is ordered after cancel on the same TCP stream.
            let next = writer.begin_transaction(cx).await.unwrap();
            assert_eq!(next.basis, 0);
            writer.rollback_transaction(cx, next.handle).await.unwrap();
            assert!(ages(cx, &mut observer).await.is_empty());
        }
        writer.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}

async fn send_raw<W: AsyncWrite + Unpin>(
    writer: &mut FrameWriter<W>,
    cx: &Cx,
    kind: FrameKind,
    request: u64,
    stream: StreamId,
    binding: Binding,
    body: &impl Body,
) {
    let frame = Frame::new(
        kind,
        request,
        stream,
        binding,
        body.encode().unwrap(),
        FrameLimits::new(4096).unwrap(),
    )
    .unwrap();
    writer.queue(cx, &frame).unwrap();
    writer.send(cx, |_| Ok(())).await.unwrap();
}

async fn raw_connection(
    cx: &Cx,
    addr: SocketAddr,
    credential: Vec<u8>,
) -> (
    FrameReader<impl AsyncRead + Unpin>,
    FrameWriter<impl AsyncWrite + Unpin>,
    Binding,
) {
    let socket = asupersync::net::TcpStream::connect(addr).await.unwrap();
    let (read, write) = socket.into_split();
    let limits = FrameLimits::new(4096).unwrap();
    let mut reader = FrameReader::new(read, limits);
    let mut writer = FrameWriter::new(write, limits);
    send_raw(
        &mut writer,
        cx,
        FrameKind::Hello,
        1,
        StreamId::CONTROL,
        Binding::Transport,
        &Hello {
            min_version: fgdb_protocol::PROTOCOL_VERSION,
            max_version: fgdb_protocol::PROTOCOL_VERSION,
            client_nonce: [7; 32],
            max_frame_len: 4096,
        },
    )
    .await;
    let hello = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
    assert_eq!(hello.header().kind(), FrameKind::HelloAck);
    send_raw(
        &mut writer,
        cx,
        FrameKind::Auth,
        2,
        StreamId::CONTROL,
        Binding::Transport,
        &Auth {
            credential: Credential::WardenCapability(credential),
        },
    )
    .await;
    let auth = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
    let session = AuthOk::decode(auth.payload()).unwrap().session;
    send_raw(
        &mut writer,
        cx,
        FrameKind::SelectDatabase,
        3,
        StreamId::CONTROL,
        Binding::Session(session),
        &SelectDatabase {
            name: "social".into(),
        },
    )
    .await;
    let ready = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
    let binding = Binding::Ready(Ready::decode(ready.payload()).unwrap().binding(session));
    (reader, writer, binding)
}

#[test]
fn raw_transaction_handles_are_connection_scoped_and_busy_requests_keep_the_owner() {
    run(async |cx| {
        let (addr, shutdown, mut server) = start(cx, "explicit-transaction-ownership").await;
        let scope = grant(Rights::ReadWrite);
        let mut owner = connect(cx, addr, &scope).await;
        let own = owner.begin_transaction(cx).await.unwrap();
        owner
            .execute_transaction(
                cx,
                own.handle,
                &statement(ExecuteMode::Write, "CREATE (:Person {age:1})"),
            )
            .await
            .unwrap();
        let credential = token(&scope);
        let (mut reader, mut writer, binding) =
            raw_connection(cx, addr, credential.clone()).await;
        // The identical bearer on another connection has no transaction owner.
        send_raw(
            &mut writer,
            cx,
            FrameKind::CommitTransaction,
            4,
            StreamId::CONTROL,
            binding,
            &TransactionSelector { handle: own.handle },
        )
        .await;
        let error = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
        assert_eq!(error.header().kind(), FrameKind::Error);
        assert_eq!(ErrorBody::decode(error.payload()).unwrap().code, ErrorCode::Statement);
        send_raw(
            &mut writer,
            cx,
            FrameKind::BeginTransaction,
            5,
            StreamId::CONTROL,
            binding,
            &Empty,
        )
        .await;
        let begun = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
        assert_eq!(begun.header().kind(), FrameKind::TransactionBegun);
        let begun = TransactionBegun::decode(begun.payload()).unwrap();
        assert_ne!(begun.handle, own.handle);
        send_raw(
            &mut writer,
            cx,
            FrameKind::Execute,
            6,
            StreamId::CONTROL,
            binding,
            &statement(ExecuteMode::Write, "CREATE (:Person {age:999})"),
        )
        .await;
        let error = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
        assert_eq!(ErrorBody::decode(error.payload()).unwrap().code, ErrorCode::Busy);
        let replacement = fgdb_warden::CapabilityToken::decode(&credential)
            .unwrap()
            .attenuate(Restriction::MaxRows(1))
            .unwrap()
            .encode();
        send_raw(
            &mut writer,
            cx,
            FrameKind::AuthRefresh,
            7,
            StreamId::CONTROL,
            binding,
            &AuthRefresh {
                credential: Credential::WardenCapability(replacement),
            },
        )
        .await;
        let error = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
        assert_eq!(ErrorBody::decode(error.payload()).unwrap().code, ErrorCode::Busy);

        // The original owner and grant survived both Busy replies. Two rows
        // would fail if AUTH_REFRESH had silently replaced its MaxRows.
        send_raw(
            &mut writer,
            cx,
            FrameKind::ExecuteTransaction,
            8,
            StreamId::CONTROL,
            binding,
            &ExecuteTransaction {
                handle: begun.handle,
                statement: statement(
                    ExecuteMode::Write,
                    "UNWIND [2,3] AS age CREATE (n:Person {age:age}) RETURN n.age AS age",
                ),
            },
        )
        .await;
        let mut values = Vec::new();
        let stream = loop {
            let frame = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
            assert_eq!(frame.header().request_id(), 8);
            assert_eq!(frame.header().binding(), binding);
            match frame.header().kind() {
                FrameKind::TransactionResultChunk => {
                    values.extend(ResultChunk::decode(frame.payload()).unwrap().rows);
                }
                FrameKind::TransactionResultEnd => {
                    let end = TransactionResultEnd::decode(frame.payload()).unwrap();
                    assert_eq!(end.handle, begun.handle);
                    assert_eq!((end.statement, end.rows), (1, 2));
                    break frame.header().stream_id();
                }
                other => panic!("unexpected provisional response: {other:?}"),
            }
        };
        values.sort_by_key(|row| match &row[0] {
            WireValue::Int(age) => *age,
            _ => panic!("integer age"),
        });
        assert_eq!(values, [vec![WireValue::Int(2)], vec![WireValue::Int(3)]]);
        // Deliberately send cancel AFTER consuming the entire END: this is a
        // deterministic witness of the late-cancellation race, not a timing test.
        send_raw(
            &mut writer,
            cx,
            FrameKind::QueryCancel,
            9,
            stream,
            binding,
            &Empty,
        )
        .await;
        send_raw(
            &mut writer,
            cx,
            FrameKind::CommitTransaction,
            10,
            StreamId::CONTROL,
            binding,
            &TransactionSelector { handle: begun.handle },
        )
        .await;
        let error = reader.receive(cx, |_| Ok(())).await.unwrap().unwrap();
        assert_eq!(error.header().request_id(), 10);
        assert_eq!(ErrorBody::decode(error.payload()).unwrap().code, ErrorCode::Statement);
        drop(reader);
        drop(writer);

        assert_eq!(
            owner.commit_transaction(cx, own.handle).await.unwrap(),
            Outcome::WriteCommitted { seq: 1, statements: 1 }
        );
        let mut observer = connect(cx, addr, &grant(Rights::Read)).await;
        assert_eq!(ages(cx, &mut observer).await, [vec![WireValue::Int(1)]]);
        owner.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        shutdown.trigger();
        server.join(cx).await.unwrap();
    });
}
