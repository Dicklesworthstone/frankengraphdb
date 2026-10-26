//! One retained write template, one fresh autocommit per typed JSONL record.
//! Raw input is bounded before growing the record buffer. The next record is
//! decoded only after the previous completion has been flushed to the caller.

use super::{Failure, batch, emit, failure, parse_json};
use asupersync::Cx;
use asupersync::io::{AsyncRead, AsyncReadExt};
use fgdb_protocol::MAX_HEADER_LEN;
use fgdb_protocol::body::{
    Body, ExecutePrepared, Outcome, Prepare, PrepareMode, PreparedExecutionMode, PreparedHandle,
    WireValue,
};
use fgdb_protocol::client::{CLIENT_MAX_FRAME, Client, ClientError};
use std::io::Write;
use std::path::Path;

/// Raw bytes excluding LF and including CR in a CRLF terminator. Each record
/// additionally passes the shared JSON/value limits and an encoded frame cap.
const MAX_RECORD_BYTES: usize = CLIENT_MAX_FRAME as usize;
const READ_BYTES: usize = 8192;

struct Record {
    number: usize,
    parameters: Vec<(String, WireValue)>,
}

struct JsonLines<R> {
    reader: R,
    buffer: [u8; READ_BYTES],
    start: usize,
    end: usize,
    next_record: usize,
    record_limit: usize,
}

impl<R: AsyncRead + Unpin> JsonLines<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            buffer: [0; READ_BYTES],
            start: 0,
            end: 0,
            next_record: 1,
            record_limit: MAX_RECORD_BYTES,
        }
    }

    async fn line(&mut self, cx: &Cx) -> Result<Option<Vec<u8>>, Failure> {
        let mut bytes = Vec::new();
        loop {
            cx.checkpoint().map_err(Failure::io)?;
            if self.start == self.end {
                self.start = 0;
                self.end = match self.reader.read(&mut self.buffer).await {
                    Ok(count) => count,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return Err(Failure::io("cannot read --prepared-jsonl")),
                };
                if self.end == 0 {
                    return Ok((!bytes.is_empty()).then_some(bytes));
                }
            }
            let available = &self.buffer[self.start..self.end];
            let newline = available.iter().position(|&byte| byte == b'\n');
            let count = newline.unwrap_or(available.len());
            if count > self.record_limit.saturating_sub(bytes.len()) {
                return Err(Failure::usage(format!(
                    "--prepared-jsonl record exceeds {} bytes",
                    self.record_limit
                )));
            }
            // The bound precedes allocation and copying, including a record
            // that spans many reads or has no final newline.
            let required = bytes.len() + count;
            if bytes.capacity() < required {
                let capacity = bytes
                    .capacity()
                    .saturating_mul(2)
                    .max(READ_BYTES)
                    .max(required)
                    .min(self.record_limit);
                bytes
                    .try_reserve_exact(capacity - bytes.len())
                    .map_err(Failure::io)?;
            }
            bytes.extend_from_slice(&available[..count]);
            self.start += count;
            if newline.is_some() {
                self.start += 1;
                return Ok(Some(bytes));
            }
        }
    }

    async fn next(&mut self, cx: &Cx) -> Result<Option<Record>, Failure> {
        let number = self.next_record;
        let bytes = self
            .line(cx)
            .await
            .map_err(|error| at_record(number, error))?;
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        self.next_record = number
            .checked_add(1)
            .ok_or_else(|| at_record(number, Failure::usage("too many JSONL records")))?;
        let decoded =
            (|| {
                cx.checkpoint().map_err(Failure::io)?;
                let text = core::str::from_utf8(&bytes)
                    .map_err(|_| Failure::usage("--prepared-jsonl must be UTF-8"))?;
                let json = parse_json(text, super::MAX_JSON_VALUES, super::MAX_JSON_TOKEN_BYTES)
                .map_err(|_| Failure::usage(
                    "--prepared-jsonl needs one JSON object per line; blank lines are invalid",
                ))?;
                let parameters =
                    batch::parameters(json, number, &mut || cx.checkpoint().map_err(Failure::io))?;
                // A placeholder has exactly the same fixed-width encoding as the
                // eventual connection-owned handle; it is never sent to the server.
                let body = ExecutePrepared {
                    handle: PreparedHandle([1; 16]),
                    mode: PreparedExecutionMode::Write,
                    parameters,
                };
                admit_frame(&body)?;
                cx.checkpoint().map_err(Failure::io)?;
                Ok::<_, Failure>(body.parameters)
            })();
        Ok(Some(Record {
            number,
            parameters: decoded.map_err(|error| at_record(number, error))?,
        }))
    }
}

fn admit_frame(body: &impl Body) -> Result<(), Failure> {
    let encoded = body.encode().map_err(Failure::usage)?;
    if encoded.len() > (CLIENT_MAX_FRAME as usize).saturating_sub(MAX_HEADER_LEN) {
        return Err(Failure::usage(
            "prepared write exceeds the client frame limit; this record was not sent",
        ));
    }
    Ok(())
}

fn at_record(number: usize, mut error: Failure) -> Failure {
    error.message = format!("JSONL record {number}: {}", error.message);
    error
}

fn after_completed(mut error: Failure, completed: Option<(usize, u64)>) -> Failure {
    if let Some((records, seq)) = completed {
        error.message = format!(
            "{}; {records} earlier record(s) already completed through seq {seq}",
            error.message
        );
    }
    error
}

/// Owns one bounded decoded record and a fixed read buffer, never a file-sized
/// argument bag. The server alone binds the typed script and authorizes effects.
pub(super) struct PreparedWrites<R> {
    statement: String,
    input: JsonLines<R>,
    first: Record,
}

impl PreparedWrites<asupersync::fs::File> {
    pub(super) async fn open(cx: &Cx, path: &Path, statement: &str) -> Result<Self, Failure> {
        cx.checkpoint().map_err(Failure::io)?;
        let named = asupersync::fs::metadata(path)
            .await
            .map_err(|_| Failure::io("cannot inspect --prepared-jsonl"))?;
        if !named.is_file() {
            return Err(Failure::usage("--prepared-jsonl must be a regular file"));
        }
        let file = asupersync::fs::File::open(path)
            .await
            .map_err(|_| Failure::io("cannot open --prepared-jsonl"))?;
        if !file
            .metadata()
            .await
            .map_err(|_| Failure::io("cannot inspect --prepared-jsonl"))?
            .is_file()
        {
            return Err(Failure::usage("--prepared-jsonl must be a regular file"));
        }
        Self::from_reader(cx, file, statement).await
    }
}

impl<R: AsyncRead + Unpin> PreparedWrites<R> {
    /// First-record and preparation-envelope admission precede credential
    /// access and connection. Empty input never creates a prepared handle.
    pub(super) async fn from_reader(cx: &Cx, reader: R, statement: &str) -> Result<Self, Failure> {
        let mut input = JsonLines::new(reader);
        let first = input
            .next(cx)
            .await?
            .ok_or_else(|| Failure::usage("--prepared-jsonl needs at least one argument object"))?;
        let request = Prepare {
            mode: PrepareMode::Write,
            statement: statement.to_owned(),
            parameters: first.parameters.clone(),
        };
        admit_frame(&request).map_err(|error| at_record(first.number, error))?;
        Ok(Self {
            statement: request.statement,
            input,
            first,
        })
    }

    pub(super) async fn execute(
        self,
        cx: &Cx,
        client: &mut Client,
        robot: bool,
        out: &mut impl Write,
    ) -> Result<(), Failure> {
        let Self {
            statement,
            mut input,
            first,
        } = self;
        cx.checkpoint().map_err(Failure::io)?;
        let handle = client
            .prepare_write(cx, &statement, first.parameters.clone())
            .await
            .map_err(|error| at_record(first.number, failure(error)))?;
        drop(statement);
        let mut current = first;
        let mut completed = None;
        loop {
            let number = current.number;
            cx.checkpoint().map_err(|error| {
                after_completed(at_record(number, Failure::io(error)), completed)
            })?;
            let outcome = client
                .execute_prepared_write(cx, handle, current.parameters)
                .await
                .map_err(|error| {
                    let error = match error {
                        error @ ClientError::Server { .. } => {
                            let mut mapped = failure(error);
                            if mapped.code == 5 {
                                mapped.message = format!(
                                    "{}; completion was not confirmed; do not replay automatically",
                                    mapped.message
                                );
                            }
                            mapped
                        }
                        error => Failure::io(format!(
                            "completion was not confirmed; do not replay automatically: {error}"
                        )),
                    };
                    after_completed(at_record(number, error), completed)
                })?;
            let (kind, seq, statements) = match outcome {
                Outcome::WriteCommitted { seq, statements } => ("written", seq, statements),
                Outcome::ReadClosed { seq, statements } => ("read_closed", seq, statements),
                Outcome::Rows { .. } => {
                    return Err(after_completed(
                        at_record(
                            number,
                            Failure::io(
                                "unexpected prepared write outcome; do not replay automatically",
                            ),
                        ),
                        completed,
                    ));
                }
            };
            let delivered = if robot {
                emit(
                    out,
                    &format!(
                        r#"{{"v":1,"event":"result","kind":"{kind}","seq":{seq},"statements":{statements},"records":1}}"#
                    ),
                )
            } else {
                let completion = if kind == "written" {
                    "committed"
                } else {
                    "completed without changes"
                };
                emit(
                    out,
                    &format!(
                        "record {number}: {statements} statement(s) {completion} at seq {seq}"
                    ),
                )
            };
            delivered.and_then(|()| out.flush().map_err(Failure::io))
                .map_err(|error| after_completed(Failure::io(format!(
                    "JSONL record {number} completed at seq {seq}, but output delivery failed; do not replay it: {}",
                    error.message
                )), completed))?;
            completed = Some((number, seq));
            current = match input
                .next(cx)
                .await
                .map_err(|error| after_completed(error, completed))?
            {
                Some(record) => record,
                None => break,
            };
        }
        // The caller closes this connection, releasing its one template. A
        // failure never reconnects, reparses, falls back, or replays any record.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::io::ReadBuf;
    use asupersync::lab::run_async_under_lab;
    use core::pin::Pin;
    use core::task::{Context, Poll};

    struct Bytes {
        input: Vec<u8>,
        at: usize,
        quantum: usize,
    }

    impl Bytes {
        fn new(input: &[u8], quantum: usize) -> Self {
            Self {
                input: input.to_vec(),
                at: 0,
                quantum,
            }
        }
    }

    impl AsyncRead for Bytes {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            out: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let count = self
                .quantum
                .min(out.remaining())
                .min(self.input.len() - self.at);
            out.put_slice(&self.input[self.at..self.at + count]);
            self.at += count;
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn jsonl_framing_preserves_crlf_utf8_last_lines_and_typed_values() {
        let ((), report) = run_async_under_lab(0x79a0_0751, |cx| async move {
            let source = "{\"z\":\"uint:18446744073709551615\",\"name\":\"text:λ Ada\"}\r\n{\"name\":\"text:Bob\",\"z\":\"int:7\"}";
            for quantum in [1, 2, 11, READ_BYTES] {
                let mut input = JsonLines::new(Bytes::new(source.as_bytes(), quantum));
                let first = input
                    .next(&cx)
                    .await
                    .unwrap_or_else(|error| panic!("{}", error.message))
                    .unwrap();
                assert_eq!(first.number, 1);
                assert_eq!(
                    first.parameters,
                    [
                        ("name".into(), WireValue::Text("λ Ada".into())),
                        ("z".into(), WireValue::Count(u64::MAX)),
                    ]
                );
                let second = input
                    .next(&cx)
                    .await
                    .unwrap_or_else(|error| panic!("{}", error.message))
                    .unwrap();
                assert_eq!(second.number, 2);
                assert_eq!(
                    second.parameters,
                    [
                        ("name".into(), WireValue::Text("Bob".into())),
                        ("z".into(), WireValue::Int(7)),
                    ]
                );
                assert!(
                    input
                        .next(&cx)
                        .await
                        .unwrap_or_else(|error| panic!("{}", error.message))
                        .is_none()
                );
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn jsonl_refuses_empty_blank_oversized_and_invalid_records_before_execution() {
        let ((), report) = run_async_under_lab(0x79a0_0752, |cx| async move {
            assert!(
                PreparedWrites::from_reader(&cx, Bytes::new(b"", 1), "CREATE ()")
                    .await
                    .is_err()
            );
            for bad in [
                "\n",
                "\r\n",
                "  \n",
                "[]\n",
                "{\"x\":\"int:1\",\"x\":\"int:2\"}\n",
                "{\"x\":1}\n",
                "{\"\":\"int:1\"}\n",
                "{\"x\":\"int:secret\"}\n",
            ] {
                let failure =
                    PreparedWrites::from_reader(&cx, Bytes::new(bad.as_bytes(), 1), "CREATE ()")
                        .await
                        .err()
                        .expect("invalid first input must refuse");
                assert_eq!(failure.code, 2);
                assert!(!failure.message.contains("secret"));
            }
            let mut invalid_utf8 = JsonLines::new(Bytes::new(b"{\"x\":\"text:\xff\"}\n", 1));
            assert_eq!(invalid_utf8.next(&cx).await.err().unwrap().code, 2);
            for quantum in [1, READ_BYTES] {
                for source in [b"{}\n".as_slice(), b"{}".as_slice()] {
                    let mut exact = JsonLines::new(Bytes::new(source, quantum));
                    exact.record_limit = 2;
                    assert!(
                        exact
                            .next(&cx)
                            .await
                            .unwrap_or_else(|error| panic!("{}", error.message))
                            .is_some()
                    );
                    let mut over = JsonLines::new(Bytes::new(source, quantum));
                    over.record_limit = 1;
                    let failure = over.next(&cx).await.err().unwrap();
                    assert!(failure.message.contains("record exceeds 1 bytes"));
                }
                let mut late = JsonLines::new(Bytes::new(b"{}\n   ", quantum));
                late.record_limit = 2;
                assert!(
                    late.next(&cx)
                        .await
                        .unwrap_or_else(|error| panic!("{}", error.message))
                        .is_some()
                );
                let failure = late.next(&cx).await.err().unwrap();
                assert!(failure.message.starts_with("JSONL record 2:"));
                assert!(failure.message.contains("record exceeds 2 bytes"));
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
