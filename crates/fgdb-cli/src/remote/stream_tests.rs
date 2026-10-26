//! Real FGP decoder/callback tests for incremental remote CLI delivery.
#[path = "batch_wire_tests.rs"]
mod batch_tests;

#[path = "prepared_write_tests.rs"]
mod prepared_write_tests;

use super::*;
use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
use asupersync::lab::run_async_under_lab;
use core::pin::Pin;
use core::task::{Context, Poll};
use fgdb_protocol::body::{
    AuthOk, Body, ErrorBody, Execute, HelloAck, Ready, ResultChunk, ResultEnd,
};
use fgdb_protocol::{
    Binding, Decoder, Frame, FrameKind, FrameLimits, Posture, SessionBinding, StreamId,
};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn options(verb: &str, flags: &[&str]) -> Result<RemoteOptions, Failure> {
    let mut args = vec![
        "--addr",
        "127.0.0.1:7688",
        "--token-file",
        "token.hex",
        "--database",
        "social",
        verb,
        "MATCH (n) RETURN n.p AS value ORDER BY value LIMIT $limit",
    ];
    args.extend_from_slice(flags);
    parse(&args.into_iter().map(str::to_owned).collect::<Vec<_>>())
}

#[test]
fn read_modes_are_query_only_and_bind_full_width_unsigned_windows() {
    assert_eq!(
        options("query", &[])
            .unwrap_or_else(|error| panic!("{}", error.message))
            .mode,
        ExecuteMode::Read
    );
    for (name, mode) in [
        ("buffered", ExecuteMode::Read),
        ("source", ExecuteMode::ReadStream),
        ("ordered", ExecuteMode::ReadOrdered),
    ] {
        let parsed = options(
            "query",
            &[
                "--read-mode",
                name,
                "--param",
                "limit=uint:18446744073709551615",
            ],
        )
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(parsed.mode, mode);
        assert_eq!(
            parsed.parameters,
            [("limit".to_owned(), WireValue::Count(u64::MAX))]
        );
        for verb in ["write", "subscribe"] {
            assert_eq!(
                options(verb, &["--read-mode", name])
                    .err()
                    .expect("read mode is query-only")
                    .code,
                2
            );
        }
        assert!(options("query", &["--read-mode", name, "--max-batches", "1"]).is_err());
    }
    for flags in [
        vec!["--read-mode"],
        vec!["--read-mode", "eager"],
        vec!["--read-mode", "ordered", "--read-mode", "source"],
        vec!["--param", "limit=uint:-1"],
        vec!["--param", "limit=uint:+1"],
        vec!["--param", "limit=uint:1.0"],
        vec!["--param", "limit=uint:18446744073709551616"],
    ] {
        assert_eq!(options("query", &flags).err().unwrap().code, 2);
    }
    assert_eq!(
        parameter("uint:0").unwrap_or_else(|error| panic!("{}", error.message)),
        WireValue::Count(0)
    );
}

fn session() -> SessionBinding {
    SessionBinding {
        transcript: [1; 32],
        auth_generation: 1,
    }
}
fn ready() -> Ready {
    Ready {
        namespace: [2; 32],
        incarnation: [3; 32],
        service_epoch: 1,
        posture: Posture::Local,
        authority_commitment: [4; 32],
        // This deliberately differs from the query cut in END.
        frontier: 99,
    }
}
fn frame(
    kind: FrameKind,
    request: u64,
    stream: StreamId,
    binding: Binding,
    body: &impl Body,
) -> Frame {
    Frame::new(
        kind,
        request,
        stream,
        binding,
        body.encode().unwrap(),
        FrameLimits::new(4096).unwrap(),
    )
    .unwrap()
}
fn result(kind: FrameKind, request: u64, body: &impl Body) -> Frame {
    frame(
        kind,
        request,
        StreamId([5; 16]),
        Binding::Ready(ready().binding(session())),
        body,
    )
}
fn chunk(request: u64, columns: Option<Vec<String>>, rows: Vec<Vec<WireValue>>) -> Frame {
    result(
        FrameKind::SnapshotResultChunk,
        request,
        &ResultChunk { columns, rows },
    )
}
fn end(request: u64, rows: u64) -> Frame {
    result(
        FrameKind::SnapshotResultEnd,
        request,
        &ResultEnd {
            outcome: Outcome::Rows { seq: 7 },
            rows,
        },
    )
}
fn refusal(request: u64, code: ErrorCode) -> Frame {
    result(
        FrameKind::Error,
        request,
        &ErrorBody {
            code,
            message: "native query refused".into(),
        },
    )
}

struct Packet {
    bytes: Vec<u8>,
    // No bytes in this next frame may be read before this many flush calls.
    flushes: usize,
}
struct ScriptIo {
    packets: VecDeque<Packet>,
    offset: usize,
    written: Arc<Mutex<Vec<u8>>>,
    flushes: Arc<AtomicUsize>,
}
impl AsyncRead for ScriptIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let Some(packet) = self.packets.front() else {
            return Poll::Ready(Ok(()));
        };
        assert!(
            self.flushes.load(Ordering::SeqCst) >= packet.flushes,
            "remote receive advanced ahead of flushed output"
        );
        let count = 11
            .min(buffer.remaining())
            .min(packet.bytes.len() - self.offset);
        buffer.put_slice(&packet.bytes[self.offset..self.offset + count]);
        let complete = self.offset + count == packet.bytes.len();
        self.offset += count;
        if complete {
            self.packets.pop_front();
            self.offset = 0;
        }
        Poll::Ready(Ok(()))
    }
}
impl AsyncWrite for ScriptIo {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.written.lock().unwrap().extend_from_slice(bytes);
        Poll::Ready(Ok(bytes.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
fn script(replies: Vec<(Frame, usize)>, flushes: Arc<AtomicUsize>) -> ScriptIo {
    let handshake = [
        frame(
            FrameKind::HelloAck,
            1,
            StreamId::CONTROL,
            Binding::Transport,
            &HelloAck {
                version: fgdb_protocol::PROTOCOL_VERSION,
                server_nonce: [0; 32],
                max_frame_len: 4096,
                initial_window_bytes: 65_536,
                initial_window_rows: 32,
            },
        ),
        frame(
            FrameKind::AuthOk,
            2,
            StreamId::CONTROL,
            Binding::Transport,
            &AuthOk { session: session() },
        ),
        frame(
            FrameKind::Ready,
            3,
            StreamId::CONTROL,
            Binding::Session(session()),
            &ready(),
        ),
    ];
    ScriptIo {
        packets: handshake
            .into_iter()
            .map(|frame| (frame, 0))
            .chain(replies)
            .map(|(frame, flushes)| Packet {
                bytes: frame.encode(FrameLimits::new(4096).unwrap()).unwrap(),
                flushes,
            })
            .collect(),
        offset: 0,
        written: Arc::default(),
        flushes,
    }
}
fn sent_frames(bytes: &[u8]) -> Vec<Frame> {
    let mut decoder = Decoder::new(FrameLimits::new(4096).unwrap());
    let mut at = 0;
    let mut frames = Vec::new();
    while at < bytes.len() {
        let next = decoder.decode(&bytes[at..], |_| Ok(())).unwrap();
        assert!(next.consumed > 0);
        at += next.consumed;
        if let Some(frame) = next.frame {
            frames.push(frame);
        }
    }
    frames
}
struct Output {
    bytes: Vec<u8>,
    flushes: Arc<AtomicUsize>,
    fail_on: Option<usize>,
}
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let count = self.flushes.fetch_add(1, Ordering::SeqCst) + 1;
        if self.fail_on == Some(count) {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "injected output flush failure",
            ))
        } else {
            Ok(())
        }
    }
}

#[test]
fn remote_rows_flush_before_more_frames_and_keep_exact_cells_and_query_cut() {
    let ((), report) = run_async_under_lab(0x79a0_0501, |root| async move {
        for mode in [ExecuteMode::ReadStream, ExecuteMode::ReadOrdered] {
            for robot in [false, true] {
                let columns = vec![
                    "count".into(),
                    "sum".into(),
                    "mean".into(),
                    "detail\n".into(),
                ];
                let row = vec![
                    WireValue::Count(u64::MAX),
                    WireValue::WideInt(i128::MIN),
                    WireValue::Average {
                        numerator: 7,
                        denominator: 3,
                    },
                    WireValue::Map(vec![(
                        "list".into(),
                        WireValue::List(vec![WireValue::Null, WireValue::Text("a\tb".into())]),
                    )]),
                ];
                let flushes = Arc::new(AtomicUsize::new(0));
                let io = script(
                    vec![
                        (chunk(4, Some(columns), vec![]), 0),
                        (chunk(4, None, vec![row.clone()]), 1),
                        (chunk(4, None, vec![row]), 2),
                        (end(4, 2), 3),
                    ],
                    Arc::clone(&flushes),
                );
                let written = Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "social").await.unwrap();
                let mut out = Output {
                    bytes: Vec::new(),
                    flushes,
                    fail_on: None,
                };
                stream_read(
                    &root,
                    &mut client,
                    mode,
                    "MATCH (n) RETURN n",
                    vec![],
                    robot,
                    &mut out,
                )
                .await
                .unwrap_or_else(|error| panic!("{}", error.message));
                assert_eq!(out.flushes.load(Ordering::SeqCst), 4);
                let text = String::from_utf8(out.bytes).unwrap();
                let lines: Vec<_> = text.lines().collect();
                assert_eq!(lines.len(), 4);
                if robot {
                    assert_eq!(
                        lines[0],
                        r#"{"v":1,"event":"columns","stream":true,"columns":["count","sum","mean","detail\n"]}"#
                    );
                    assert!(
                        lines[1].contains(r#"{"type":"count","value":"18446744073709551615"}"#)
                    );
                    assert!(lines[1].contains(
                        r#"{"type":"wideint","value":"-170141183460469231731687303715884105728"}"#
                    ));
                    assert!(lines[1].contains(r#"{"type":"average","value":"7/3"}"#));
                    assert!(lines[1].contains(r#""type":"map""#));
                    assert_eq!(
                        lines[3],
                        r#"{"v":1,"event":"result","kind":"rows","stream":true,"seq":7,"count":2}"#
                    );
                } else {
                    assert_eq!(lines[0], "count\tsum\tmean\tdetail\\n");
                    assert!(lines[1].starts_with(
                        "18446744073709551615\t-170141183460469231731687303715884105728\t7/3\t"
                    ));
                    assert_eq!(lines[3], "2 row(s) (stream complete at seq 7)");
                }
                assert_eq!(lines[1], lines[2]);
                let sent = sent_frames(&written.lock().unwrap());
                let executions: Vec<_> = sent
                    .iter()
                    .filter(|f| f.header().kind() == FrameKind::Execute)
                    .collect();
                assert_eq!(executions.len(), 1);
                assert_eq!(Execute::decode(executions[0].payload()).unwrap().mode, mode);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn schema_and_row_flush_failures_cancel_drain_and_never_emit_success() {
    let ((), report) = run_async_under_lab(0x79a0_0502, |root| async move {
        for mode in [ExecuteMode::ReadStream, ExecuteMode::ReadOrdered] {
            for fail_on in 1..=3 {
                let flushes = Arc::new(AtomicUsize::new(0));
                let mut replies = vec![(chunk(4, Some(vec!["value".into()]), vec![]), 0)];
                // A failed header cancels without requiring any result row.
                if fail_on > 1 {
                    replies.extend([
                        (chunk(4, None, vec![vec![WireValue::Int(1)]]), 1),
                        (
                            chunk(4, None, vec![vec![WireValue::Int(2)]]),
                            fail_on.min(2),
                        ),
                    ]);
                }
                replies.extend([
                    (refusal(4, ErrorCode::Cancelled), fail_on),
                    (
                        chunk(
                            6,
                            Some(vec!["value".into()]),
                            vec![vec![WireValue::Int(20)]],
                        ),
                        fail_on,
                    ),
                    (end(6, 1), fail_on),
                ]);
                let io = script(replies, Arc::clone(&flushes));
                let written = Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "social").await.unwrap();
                let mut out = Output {
                    bytes: Vec::new(),
                    flushes,
                    fail_on: Some(fail_on),
                };
                let error = stream_read(
                    &root,
                    &mut client,
                    mode,
                    "MATCH (n) RETURN n",
                    vec![],
                    true,
                    &mut out,
                )
                .await
                .expect_err("output failure is terminal");
                assert_eq!(error.code, 5);
                assert!(error.message.contains("injected output flush failure"));
                assert!(error.message.starts_with(&format!(
                    "stream incomplete after {} fully flushed row(s)",
                    fail_on.saturating_sub(2)
                )));
                assert_eq!(out.flushes.load(Ordering::SeqCst), fail_on);
                let text = String::from_utf8(out.bytes).unwrap();
                assert!(!text.contains(r#""event":"result""#));
                assert_eq!(
                    text.lines()
                        .filter(|line| line.contains(r#""event":"row""#))
                        .count(),
                    fail_on - 1
                );
                assert_eq!(
                    client
                        .execute(&root, ExecuteMode::Read, "MATCH (n) RETURN n", vec![])
                        .await
                        .unwrap()
                        .rows,
                    [vec![WireValue::Int(20)]]
                );
                let sent = sent_frames(&written.lock().unwrap());
                let cancels: Vec<_> = sent
                    .iter()
                    .filter(|f| f.header().kind() == FrameKind::QueryCancel)
                    .collect();
                assert_eq!(cancels.len(), 1);
                assert_eq!(cancels[0].header().stream_id(), StreamId([5; 16]));
                assert!(
                    !sent
                        .iter()
                        .any(|f| f.header().kind() == FrameKind::WindowUpdate)
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_refusal_or_eof_preserves_the_flushed_prefix_without_success_or_retry() {
    let ((), report) = run_async_under_lab(0x79a0_0503, |root| async move {
        for mode in [ExecuteMode::ReadStream, ExecuteMode::ReadOrdered] {
            for terminal in [Some(ErrorCode::Budget), None] {
                let flushes = Arc::new(AtomicUsize::new(0));
                let mut replies = vec![
                    (chunk(4, Some(vec!["value".into()]), vec![]), 0),
                    (chunk(4, None, vec![vec![WireValue::Int(1)]]), 1),
                ];
                if let Some(code) = terminal {
                    replies.push((refusal(4, code), 2));
                }
                let io = script(replies, Arc::clone(&flushes));
                let written = Arc::clone(&io.written);
                let mut client = Client::connect_stream(&root, io, vec![1]).await.unwrap();
                client.select(&root, "social").await.unwrap();
                let mut out = Output {
                    bytes: Vec::new(),
                    flushes,
                    fail_on: None,
                };
                let error = stream_read(
                    &root,
                    &mut client,
                    mode,
                    "MATCH (n) RETURN n",
                    vec![],
                    true,
                    &mut out,
                )
                .await
                .expect_err("missing successful terminal");
                assert_eq!(error.code, if terminal.is_some() { 3 } else { 5 });
                assert!(
                    error
                        .message
                        .starts_with("stream incomplete after 1 fully flushed row(s)")
                );
                let text = String::from_utf8(out.bytes).unwrap();
                assert_eq!(text.lines().count(), 2);
                assert!(!text.contains(r#""event":"result""#));
                let sent = sent_frames(&written.lock().unwrap());
                assert_eq!(
                    sent.iter()
                        .filter(|f| f.header().kind() == FrameKind::Execute)
                        .count(),
                    1
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn help_and_schema_distinguish_server_physical_modes_and_final_snapshot_metadata() {
    assert!(crate::HELP.contains("--read-mode buffered|source|ordered"));
    let Json::Object(schema) =
        parse_json(crate::ROBOT_SCHEMA, MAX_JSON_VALUES, MAX_JSON_TOKEN_BYTES).unwrap()
    else {
        panic!("robot schema must be an object");
    };
    let Json::Object(remote) = &schema["remote"] else {
        panic!("remote contract must be an object");
    };
    assert!(
        matches!(&remote["flag"], Json::String(value) if value.contains("--read-mode buffered|source|ordered"))
    );
    assert!(
        matches!(&remote["delivery"], Json::String(value) if value.contains("seq is absent on columns") && value.contains("no successful result"))
    );
}
