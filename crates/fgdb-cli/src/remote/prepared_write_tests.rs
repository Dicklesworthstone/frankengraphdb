//! Real FGP client/decoder proofs for the prepared JSONL CLI adapter.
//! Input reads and server replies are gated on completed output flushes.

use super::super::prepared_writes::PreparedWrites;
use super::*;
use fgdb_protocol::body::{
    ExecutePrepared, Prepare, PrepareMode, Prepared, PreparedExecutionMode, PreparedHandle,
};

const HANDLE: PreparedHandle = PreparedHandle([9; 16]);
const STATEMENT: &str = "CREATE (:Person {name:$name})";

struct Rows {
    rows: VecDeque<Vec<u8>>,
    offset: usize,
    flushes: Arc<AtomicUsize>,
    started: Arc<AtomicUsize>,
}

impl Rows {
    fn new(rows: &[&str], flushes: Arc<AtomicUsize>) -> Self {
        Self {
            rows: rows.iter().map(|row| row.as_bytes().to_vec()).collect(),
            offset: 0,
            flushes,
            started: Arc::default(),
        }
    }
}

impl AsyncRead for Rows {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let Some(row) = self.rows.front() else {
            return Poll::Ready(Ok(()));
        };
        if self.offset == 0 {
            let completed = self.started.fetch_add(1, Ordering::SeqCst);
            assert!(
                self.flushes.load(Ordering::SeqCst) >= completed,
                "input advanced before the preceding completion was flushed"
            );
        }
        let count = 11.min(buffer.remaining()).min(row.len() - self.offset);
        buffer.put_slice(&row[self.offset..self.offset + count]);
        let complete = self.offset + count == row.len();
        self.offset += count;
        if complete {
            self.rows.pop_front();
            self.offset = 0;
        }
        Poll::Ready(Ok(()))
    }
}

fn prepared() -> (Frame, usize) {
    (
        frame(
            FrameKind::Prepared,
            4,
            StreamId::CONTROL,
            Binding::Ready(ready().binding(session())),
            &Prepared { handle: HANDLE },
        ),
        0,
    )
}

fn completion(request: u64, seq: u64, changed: bool, flushes: usize) -> Vec<(Frame, usize)> {
    vec![
        (chunk(request, Some(vec![]), vec![]), flushes),
        (
            result(
                FrameKind::SnapshotResultEnd,
                request,
                &ResultEnd {
                    outcome: if changed {
                        Outcome::WriteCommitted { seq, statements: 1 }
                    } else {
                        Outcome::ReadClosed { seq, statements: 1 }
                    },
                    rows: 0,
                },
            ),
            flushes,
        ),
    ]
}

fn assert_executions(written: &[u8], expected: usize) -> Vec<ExecutePrepared> {
    let frames = sent_frames(written);
    let prepares: Vec<_> = frames
        .iter()
        .filter(|frame| frame.header().kind() == FrameKind::Prepare)
        .collect();
    assert_eq!(prepares.len(), 1);
    let request = Prepare::decode(prepares[0].payload()).unwrap();
    assert_eq!(request.mode, PrepareMode::Write);
    assert_eq!(request.statement, STATEMENT);
    assert_eq!(
        request.parameters,
        [("name".into(), WireValue::Text("Ada".into()))]
    );
    let executions: Vec<_> = frames
        .iter()
        .filter(|frame| frame.header().kind() == FrameKind::ExecutePrepared)
        .map(|frame| ExecutePrepared::decode(frame.payload()).unwrap())
        .collect();
    assert_eq!(executions.len(), expected);
    for execution in &executions {
        assert_eq!(execution.handle, HANDLE);
        assert_eq!(execution.mode, PreparedExecutionMode::Write);
    }
    assert!(
        !frames.iter().any(|frame| matches!(
            frame.header().kind(),
            FrameKind::Execute | FrameKind::ExecuteBatch
        )),
        "prepared writes never fall back to unprepared execution"
    );
    executions
}

#[test]
fn prepared_jsonl_flags_make_per_record_autocommit_explicit() {
    let accepted = options("write", &["--prepared-jsonl", "records.jsonl"])
        .unwrap_or_else(|error| panic!("{}", error.message));
    assert_eq!(
        accepted.prepared_jsonl,
        Some(PathBuf::from("records.jsonl"))
    );
    assert!(accepted.rows_file.is_none());
    for verb in ["query", "subscribe"] {
        assert_eq!(
            options(verb, &["--prepared-jsonl", "records.jsonl"])
                .err()
                .unwrap()
                .code,
            2
        );
    }
    for flags in [
        vec!["--prepared-jsonl"],
        vec!["--prepared-jsonl", "a", "--prepared-jsonl", "b"],
        vec!["--prepared-jsonl", "a", "--rows-file", "b"],
        vec!["--prepared-jsonl", "a", "--csv-types", "name=text"],
        vec!["--prepared-jsonl", "a", "--param", "name=text:Ada"],
        vec!["--prepared-jsonl", "a", "--read-mode", "buffered"],
        vec!["--prepared-jsonl", "a", "--max-batches", "1"],
    ] {
        assert_eq!(options("write", &flags).err().unwrap().code, 2);
    }
}

#[test]
fn prepared_jsonl_uses_one_template_and_flushes_each_distinct_native_outcome() {
    let ((), report) = run_async_under_lab(0x79a0_0761, |cx| async move {
        for robot in [false, true] {
            let flushes = Arc::new(AtomicUsize::new(0));
            let source = Rows::new(
                &[
                    "{\"name\":\"text:Ada\"}\r\n",
                    "{\"name\":\"text:'); MATCH (n) DETACH DELETE n; //\"}\n",
                    "{\"name\":\"text:Carol\"}",
                ],
                Arc::clone(&flushes),
            );
            let started = Arc::clone(&source.started);
            let writes = PreparedWrites::from_reader(&cx, source, STATEMENT)
                .await
                .unwrap_or_else(|error| panic!("{}", error.message));
            assert_eq!(started.load(Ordering::SeqCst), 1);
            let mut replies = vec![prepared()];
            replies.extend(completion(5, 7, true, 0));
            replies.extend(completion(6, 7, false, 1));
            replies.extend(completion(7, 8, true, 2));
            let io = script(replies, Arc::clone(&flushes));
            let written = Arc::clone(&io.written);
            let mut client = Client::connect_stream(&cx, io, vec![1]).await.unwrap();
            client.select(&cx, "social").await.unwrap();
            let mut out = Output {
                bytes: vec![],
                flushes,
                fail_on: None,
            };
            writes
                .execute(&cx, &mut client, robot, &mut out)
                .await
                .unwrap_or_else(|error| panic!("{}", error.message));
            assert_eq!(out.flushes.load(Ordering::SeqCst), 3);
            assert_eq!(started.load(Ordering::SeqCst), 3);
            let rendered = String::from_utf8(out.bytes).unwrap();
            let lines: Vec<_> = rendered.lines().collect();
            if robot {
                assert_eq!(
                    lines,
                    [
                        r#"{"v":1,"event":"result","kind":"written","seq":7,"statements":1,"records":1}"#,
                        r#"{"v":1,"event":"result","kind":"read_closed","seq":7,"statements":1,"records":1}"#,
                        r#"{"v":1,"event":"result","kind":"written","seq":8,"statements":1,"records":1}"#,
                    ]
                );
            } else {
                assert_eq!(
                    lines,
                    [
                        "record 1: 1 statement(s) committed at seq 7",
                        "record 2: 1 statement(s) completed without changes at seq 7",
                        "record 3: 1 statement(s) committed at seq 8",
                    ]
                );
            }
            let executions = assert_executions(&written.lock().unwrap(), 3);
            assert_eq!(
                executions[1].parameters,
                [(
                    "name".into(),
                    WireValue::Text("'); MATCH (n) DETACH DELETE n; //".into())
                ),]
            );
            assert_eq!(
                executions[2].parameters,
                [("name".into(), WireValue::Text("Carol".into()))]
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_jsonl_late_input_errors_stop_before_sending_a_bad_record() {
    let ((), report) = run_async_under_lab(0x79a0_0762, |cx| async move {
        let oversized = " ".repeat(fgdb_protocol::client::CLIENT_MAX_FRAME as usize + 1);
        for second in ["not JSON\n", "\n", "{\"name\":7}\n", &oversized] {
            let flushes = Arc::new(AtomicUsize::new(0));
            let source = Rows::new(
                &[
                    "{\"name\":\"text:Ada\"}\n",
                    second,
                    "{\"name\":\"text:must not read\"}\n",
                ],
                Arc::clone(&flushes),
            );
            let started = Arc::clone(&source.started);
            let writes = PreparedWrites::from_reader(&cx, source, STATEMENT)
                .await
                .unwrap_or_else(|error| panic!("{}", error.message));
            let mut replies = vec![prepared()];
            replies.extend(completion(5, 7, true, 0));
            let io = script(replies, Arc::clone(&flushes));
            let written = Arc::clone(&io.written);
            let mut client = Client::connect_stream(&cx, io, vec![1]).await.unwrap();
            client.select(&cx, "social").await.unwrap();
            let mut out = Output {
                bytes: vec![],
                flushes,
                fail_on: None,
            };
            let error = writes
                .execute(&cx, &mut client, true, &mut out)
                .await
                .unwrap_err();
            assert_eq!(error.code, 2);
            assert!(error.message.starts_with("JSONL record 2:"));
            assert!(
                error
                    .message
                    .contains("1 earlier record(s) already completed through seq 7")
            );
            assert_eq!(started.load(Ordering::SeqCst), 2);
            assert_eq!(String::from_utf8(out.bytes).unwrap().lines().count(), 1);
            assert_executions(&written.lock().unwrap(), 1);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_jsonl_refusal_type_drift_and_unknown_completion_never_replay_or_advance() {
    let ((), report) = run_async_under_lab(0x79a0_0763, |cx| async move {
        for terminal in [
            Some(ErrorCode::Statement),
            Some(ErrorCode::Budget),
            Some(ErrorCode::OutcomeUnknown),
            None,
        ] {
            let flushes = Arc::new(AtomicUsize::new(0));
            let source = Rows::new(
                &[
                    "{\"name\":\"text:Ada\"}\n",
                    "{\"name\":\"int:7\"}\n",
                    "{\"name\":\"text:must not read\"}\n",
                ],
                Arc::clone(&flushes),
            );
            let started = Arc::clone(&source.started);
            let writes = PreparedWrites::from_reader(&cx, source, STATEMENT)
                .await
                .unwrap_or_else(|error| panic!("{}", error.message));
            let mut replies = vec![prepared()];
            replies.extend(completion(5, 7, true, 0));
            replies.extend(terminal.map(|code| (refusal(6, code), 1)));
            let io = script(replies, Arc::clone(&flushes));
            let written = Arc::clone(&io.written);
            let mut client = Client::connect_stream(&cx, io, vec![1]).await.unwrap();
            client.select(&cx, "social").await.unwrap();
            let mut out = Output {
                bytes: vec![],
                flushes,
                fail_on: None,
            };
            let error = writes
                .execute(&cx, &mut client, true, &mut out)
                .await
                .unwrap_err();
            assert_eq!(
                error.code,
                if matches!(terminal, None | Some(ErrorCode::OutcomeUnknown)) {
                    5
                } else {
                    3
                }
            );
            assert!(error.message.starts_with("JSONL record 2:"));
            assert!(
                error
                    .message
                    .contains("1 earlier record(s) already completed through seq 7")
            );
            if error.code == 5 {
                assert!(error.message.contains("do not replay automatically"));
            }
            assert_eq!(started.load(Ordering::SeqCst), 2);
            assert_eq!(String::from_utf8(out.bytes).unwrap().lines().count(), 1);
            let executions = assert_executions(&written.lock().unwrap(), 2);
            assert_eq!(
                executions[1].parameters,
                [("name".into(), WireValue::Int(7))],
                "the native server binds types; the CLI must not coerce operands"
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_jsonl_output_failure_preserves_known_completion_and_stops_input() {
    let ((), report) = run_async_under_lab(0x79a0_0764, |cx| async move {
        let flushes = Arc::new(AtomicUsize::new(0));
        let source = Rows::new(
            &[
                "{\"name\":\"text:Ada\"}\n",
                "{\"name\":\"text:must not read\"}\n",
            ],
            Arc::clone(&flushes),
        );
        let started = Arc::clone(&source.started);
        let writes = PreparedWrites::from_reader(&cx, source, STATEMENT)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        let mut replies = vec![prepared()];
        replies.extend(completion(5, 7, true, 0));
        let io = script(replies, Arc::clone(&flushes));
        let written = Arc::clone(&io.written);
        let mut client = Client::connect_stream(&cx, io, vec![1]).await.unwrap();
        client.select(&cx, "social").await.unwrap();
        let mut out = Output {
            bytes: vec![],
            flushes,
            fail_on: Some(1),
        };
        let error = writes
            .execute(&cx, &mut client, true, &mut out)
            .await
            .unwrap_err();
        assert_eq!(error.code, 5);
        assert!(error.message.contains("record 1 completed at seq 7"));
        assert!(
            error
                .message
                .contains("output delivery failed; do not replay it")
        );
        assert_eq!(started.load(Ordering::SeqCst), 1);
        assert_executions(&written.lock().unwrap(), 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
