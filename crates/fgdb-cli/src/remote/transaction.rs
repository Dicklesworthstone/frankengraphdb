//! A finite mixed read/write script executes in one native transaction.
//! Every input and request envelope is admitted before authentication. Rows
//! are flushed provisionally as statements finish; only the final transaction
//! receipt establishes completion. Refusal never retries a statement or batch.

use super::{Failure, Json, batch, emit, failure};
use asupersync::Cx;
use fgdb_protocol::MAX_HEADER_LEN;
use fgdb_protocol::body::{
    Body, Execute, ExecuteMode, ExecuteTransaction, Outcome, TransactionHandle, WireValue,
};
use fgdb_protocol::client::{
    CLIENT_MAX_FRAME, Client, ClientError, MAX_TRANSACTION_BATCH_STATEMENTS, TransactionBatchEnd,
};
use std::io::Write;
use std::path::Path;

const MAX_INPUT_BYTES: usize = CLIENT_MAX_FRAME as usize;

/// Original typed operands remain data throughout admission and execution.
/// The whole finite script is retained; its answers are never collected.
pub(super) struct TransactionScript {
    statements: Vec<Execute>,
}

impl TransactionScript {
    pub(super) fn parse(
        input: &str,
        mut checkpoint: impl FnMut() -> Result<(), Failure>,
    ) -> Result<Self, Failure> {
        checkpoint()?;
        if input.len() > MAX_INPUT_BYTES {
            return Err(Failure::usage("--statements-file exceeds 1048576 bytes"));
        }
        let json = fgdb_protocol::json::parse_json_controlled(
            input,
            super::MAX_JSON_VALUES,
            super::MAX_JSON_TOKEN_BYTES,
            |_| checkpoint(),
        )
        .map_err(|error| match error {
            fgdb_protocol::json::JsonParseError::Invalid(_) => {
                Failure::usage("--statements-file must contain one valid JSON array")
            }
            fgdb_protocol::json::JsonParseError::Interrupted(error) => error,
        })?;
        checkpoint()?;
        let Json::Array(records) = json else {
            return Err(Failure::usage("--statements-file must contain a JSON array"));
        };
        if records.is_empty() || records.len() > MAX_TRANSACTION_BATCH_STATEMENTS {
            return Err(Failure::usage(format!(
                "--statements-file needs 1..={MAX_TRANSACTION_BATCH_STATEMENTS} transaction statements"
            )));
        }
        let mut statements = Vec::new();
        statements
            .try_reserve_exact(records.len())
            .map_err(Failure::io)?;
        for (index, record) in records.into_iter().enumerate() {
            checkpoint()?;
            let at = index + 1;
            let Json::Object(mut fields) = record else {
                return Err(Failure::usage(format!(
                    "transaction statement {at} must be an object"
                )));
            };
            let mode = match fields.remove("mode") {
                Some(Json::String(mode)) if mode == "read" => ExecuteMode::Read,
                Some(Json::String(mode)) if mode == "write" => ExecuteMode::Write,
                _ => {
                    return Err(Failure::usage(format!(
                        "transaction statement {at} needs mode read or write"
                    )));
                }
            };
            let Some(Json::String(statement)) = fields.remove("statement") else {
                return Err(Failure::usage(format!(
                    "transaction statement {at} needs a statement string"
                )));
            };
            if statement.trim().is_empty() {
                return Err(Failure::usage(format!(
                    "transaction statement {at} has an empty statement"
                )));
            }
            let arguments = fields.remove("parameters");
            if !fields.is_empty() {
                return Err(Failure::usage(format!(
                    "transaction statement {at} accepts only mode, statement and parameters fields"
                )));
            }
            let parameters = match arguments {
                Some(arguments) => batch::parameters(arguments, at, &mut checkpoint)?,
                None => Vec::new(),
            };
            // This nonzero selector is used only to measure the exact request
            // envelope, including its sixteen handle bytes. It is never sent
            // or returned: the SDK obtains the real selector from BEGIN.
            let envelope = ExecuteTransaction {
                handle: TransactionHandle([1; 16]),
                statement: Execute {
                    mode,
                    statement,
                    parameters,
                },
            };
            let encoded = envelope.encode().map_err(|error| {
                Failure::usage(format!("transaction statement {at}: {error}"))
            })?;
            if encoded.len() > (CLIENT_MAX_FRAME as usize).saturating_sub(MAX_HEADER_LEN) {
                return Err(Failure::usage(format!(
                    "transaction statement {at} exceeds the client frame limit; the script was not sent"
                )));
            }
            drop(encoded);
            checkpoint()?;
            statements.push(envelope.statement);
        }
        checkpoint()?;
        Ok(Self { statements })
    }

    pub(super) async fn read(cx: &Cx, path: &Path) -> Result<Self, Failure> {
        let bytes = batch::read_input(cx, path, "--statements-file").await?;
        let input = core::str::from_utf8(&bytes)
            .map_err(|_| Failure::usage("--statements-file must be UTF-8"))?;
        Self::parse(input, || cx.checkpoint().map_err(Failure::io))
    }

    pub(super) async fn execute(
        self,
        cx: &Cx,
        client: &mut Client,
        robot: bool,
        out: &mut impl Write,
    ) -> Result<(), Failure> {
        let expected = u32::try_from(self.statements.len())
            .map_err(|_| Failure::usage("too many transaction statements"))?;
        // The callbacks share only output shape, counters and the first error.
        // No lock survives a callback or an await. A Send writer therefore
        // retains the same Send future shape as the other remote adapters.
        let sink = std::sync::Mutex::new(TransactionOutput {
            out,
            robot,
            expected,
            basis: None,
            active: None,
            completed: 0,
            sent: 0,
            failed: None,
        });
        let lock = || {
            sink.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        let result = client
            .execute_transaction_batch_streaming(
                cx,
                self.statements,
                |index, basis, columns| lock().columns(cx, index, basis, columns),
                |index, row| lock().row(cx, index, &row),
                |index, rows| lock().result_end(cx, index, rows),
            )
            .await;
        sink.into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .finish(result)
    }
}

/// The active tuple is (zero-based statement, width, flushed row count).
/// Only one encoded row is retained between source callbacks.
struct TransactionOutput<'a, W> {
    out: &'a mut W,
    robot: bool,
    expected: u32,
    basis: Option<u64>,
    active: Option<(u32, usize, u64)>,
    completed: u32,
    sent: u64,
    failed: Option<Failure>,
}

impl<W: Write> TransactionOutput<'_, W> {
    fn capture(&mut self, result: Result<(), Failure>) -> Result<(), ClientError> {
        if let Err(error) = result {
            self.failed.get_or_insert(error);
            // Preserve the original CLI diagnostic and exit class. This marker
            // asks the SDK to stop delivery and settle the owned transaction.
            Err(ClientError::Io(std::io::ErrorKind::BrokenPipe))
        } else {
            Ok(())
        }
    }

    fn columns(
        &mut self,
        cx: &Cx,
        index: u32,
        basis: u64,
        columns: &[String],
    ) -> Result<(), ClientError> {
        let result = (|| {
            cx.checkpoint().map_err(Failure::query)?;
            if self.active.is_some() || index != self.completed || index >= self.expected {
                return Err(Failure::io("remote transaction result order is invalid"));
            }
            if self.basis.is_some_and(|previous| previous != basis) {
                return Err(Failure::io("remote transaction basis changed between statements"));
            }
            let statement = u64::from(index) + 1;
            let line = if self.robot {
                format!(
                    r#"{{"v":1,"event":"columns","statement":{statement},"stream":true,"provisional":true,"basis_seq":{basis},"columns":[{}]}}"#,
                    columns
                        .iter()
                        .map(|name| fgdb_protocol::json::quote(name))
                        .collect::<Vec<_>>()
                        .join(",")
                )
            } else {
                format!(
                    "provisional statement {statement} at basis seq {basis} (pending transaction completion)\n{}",
                    columns
                        .iter()
                        .map(|name| name
                            .chars()
                            .flat_map(char::escape_default)
                            .collect::<String>())
                        .collect::<Vec<_>>()
                        .join("\t")
                )
            };
            emit(self.out, &line)?;
            self.out.flush().map_err(Failure::io)?;
            self.basis = Some(basis);
            self.active = Some((index, columns.len(), 0));
            Ok(())
        })();
        self.capture(result)
    }

    fn row(&mut self, cx: &Cx, index: u32, row: &[WireValue]) -> Result<(), ClientError> {
        let result = (|| {
            cx.checkpoint().map_err(Failure::query)?;
            let Some((active, width, rows)) = self.active else {
                return Err(Failure::io("remote transaction row has no columns"));
            };
            if active != index || width != row.len() {
                return Err(Failure::io("remote transaction row does not match its result"));
            }
            let rows = rows
                .checked_add(1)
                .ok_or_else(|| Failure::io("transaction row counter overflow"))?;
            let sent = self
                .sent
                .checked_add(1)
                .ok_or_else(|| Failure::io("transaction delivery counter overflow"))?;
            let mut cells = Vec::with_capacity(row.len());
            for value in row {
                cx.checkpoint().map_err(Failure::query)?;
                cells.push(if self.robot {
                    fgdb_protocol::json::cell(value)
                } else {
                    super::human(value)
                });
            }
            let statement = u64::from(index) + 1;
            let line = if self.robot {
                format!(
                    r#"{{"v":1,"event":"row","statement":{statement},"provisional":true,"cells":[{}]}}"#,
                    cells.join(",")
                )
            } else {
                format!("provisional statement {statement}: {}", cells.join("\t"))
            };
            emit(self.out, &line)?;
            self.out.flush().map_err(Failure::io)?;
            self.active = Some((index, width, rows));
            self.sent = sent;
            Ok(())
        })();
        self.capture(result)
    }

    fn result_end(&mut self, cx: &Cx, index: u32, rows: u64) -> Result<(), ClientError> {
        let result = (|| {
            cx.checkpoint().map_err(Failure::query)?;
            if !self
                .active
                .is_some_and(|(at, _, sent)| at == index && sent == rows)
            {
                return Err(Failure::io("remote transaction statement count is invalid"));
            }
            self.completed = self
                .completed
                .checked_add(1)
                .ok_or_else(|| Failure::io("transaction statement counter overflow"))?;
            self.active = None;
            Ok(())
        })();
        // A statement end settles only provisional delivery, never the script.
        // It deliberately emits no success record.
        self.capture(result)
    }

    fn finish(mut self, result: Result<TransactionBatchEnd, ClientError>) -> Result<(), Failure> {
        let end = match result {
            Ok(end) => end,
            Err(error) => {
                let error = self.failed.take().unwrap_or_else(|| transaction_failure(error));
                return Err(unconfirmed(error, self.sent));
            }
        };
        let (outcome, completed, seq) = match end.outcome {
            Outcome::WriteCommitted { seq, .. } => ("write_committed", "write commit", seq),
            Outcome::ReadClosed { seq, .. } => ("read_closed", "read close", seq),
            Outcome::Rows { .. } => {
                let error = self.failed.take().unwrap_or_else(|| {
                    Failure::io("remote transaction returned no completion outcome")
                });
                return Err(unconfirmed(error, self.sent));
            }
        };
        // A valid completion receipt remains authoritative even when local
        // output or its defensive consistency checks subsequently fail.
        if let Some(error) = self.failed.take() {
            return Err(known_completion(error, completed, seq, self.sent));
        }
        if self.active.is_some()
            || self.basis != Some(end.basis)
            || end.statements != self.expected
            || end.statements != self.completed
            || end.rows != self.sent
        {
            return Err(known_completion(
                Failure::io("remote transaction completion does not match delivered results"),
                completed,
                seq,
                self.sent,
            ));
        }
        let line = if self.robot {
            format!(
                r#"{{"v":1,"event":"result","kind":"transaction","outcome":"{outcome}","seq":{seq},"basis_seq":{},"statements":{},"count":{}}}"#,
                end.basis, end.statements, end.rows
            )
        } else {
            format!(
                "transaction {completed} confirmed at seq {seq} from basis seq {}: {} statement(s), {} row(s)",
                end.basis, end.statements, end.rows
            )
        };
        let output = emit(self.out, &line).and_then(|()| self.out.flush().map_err(Failure::io));
        output.map_err(|error| known_completion(error, completed, seq, self.sent))
    }
}

/// This owner already has an authenticated connection. Transport loss during
/// the script is an execution I/O failure, not a fresh connection-open failure.
fn transaction_failure(error: ClientError) -> Failure {
    match error {
        error @ ClientError::Server { .. } => failure(error),
        error => Failure::io(error),
    }
}

/// The SDK may have confirmed rollback or fenced an uncertain connection.
/// Neither case is a successful script; an absent receipt never proves abort.
fn unconfirmed(mut error: Failure, sent: u64) -> Failure {
    error.message = format!(
        "{}; transaction completion was not confirmed; {sent} row(s) were delivered provisionally; do not retry automatically",
        error.message
    );
    error
}

fn known_completion(mut error: Failure, completed: &str, seq: u64, sent: u64) -> Failure {
    error.message = format!(
        "{}; transaction {completed} at seq {seq} is confirmed, but output is incomplete after {sent} provisional row(s); do not retry the transaction",
        error.message
    );
    error
}

#[cfg(test)]
mod tests {
    use super::super::{MAX_JSON_TOKEN_BYTES, MAX_JSON_VALUES};
    use super::*;
    use fgdb_protocol::body::{MAX_NAME_BYTES, MAX_PARAMETERS};

    fn parse(input: &str) -> Result<TransactionScript, Failure> {
        TransactionScript::parse(input, || Ok(()))
    }

    #[test]
    fn mixed_modes_keep_independent_typed_bindings_in_canonical_order() {
        let script = parse(
            r#"[
                {"mode":"write","statement":"CREATE (n {id:$n})","parameters":{"n":"uint:18446744073709551615"}},
                {"mode":"read","statement":"RETURN $n, $m","parameters":{"n":"text:λ\nAda","m":"json:{\"k\":[true,null]}"}},
                {"mode":"read","statement":"RETURN 1"}
            ]"#,
        )
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(script.statements.len(), 3);
        assert_eq!(script.statements[0].mode, ExecuteMode::Write);
        assert_eq!(script.statements[1].mode, ExecuteMode::Read);
        assert_eq!(script.statements[2].mode, ExecuteMode::Read);
        assert_eq!(script.statements[0].statement, "CREATE (n {id:$n})");
        assert_eq!(
            script.statements[0].parameters,
            [("n".into(), WireValue::Count(u64::MAX))]
        );
        assert_eq!(script.statements[1].parameters[0].0, "m");
        assert_eq!(
            script.statements[1].parameters[1],
            ("n".into(), WireValue::Text("λ\nAda".into()))
        );
        assert!(matches!(
            &script.statements[1].parameters[0].1,
            WireValue::Map(_)
        ));
        assert!(script.statements[2].parameters.is_empty());
        for statement in script.statements {
            let envelope = ExecuteTransaction {
                handle: TransactionHandle([1; 16]),
                statement,
            };
            assert_eq!(
                ExecuteTransaction::decode(&envelope.encode().unwrap()).unwrap(),
                envelope
            );
        }
    }

    #[test]
    fn malformed_late_statements_and_noncanonical_fields_refuse_the_entire_script() {
        let first = r#"{"mode":"write","statement":"CREATE (n)"}"#;
        for late in [
            "null",
            "{}",
            r#"{"statement":"RETURN 1"}"#,
            r#"{"mode":"READ","statement":"RETURN 1"}"#,
            r#"{"mode":"read ","statement":"RETURN 1"}"#,
            r#"{"mode":"stats","statement":"RETURN 1"}"#,
            r#"{"mode":1,"statement":"RETURN 1"}"#,
            r#"{"mode":"read","statement":7}"#,
            r#"{"mode":"read","statement":"   "}"#,
            r#"{"mode":"read","statement":"RETURN 1","extra":true}"#,
            r#"{"mode":"read","mode":"write","statement":"RETURN 1"}"#,
            r#"{"mode":"read","statement":"RETURN 1","statement":"RETURN 2"}"#,
            r#"{"mode":"read","statement":"RETURN $n","parameters":[]}"#,
            r#"{"mode":"read","statement":"RETURN $n","parameters":{"n":1}}"#,
            r#"{"mode":"read","statement":"RETURN $n","parameters":{"n":"uint:-1"}}"#,
            r#"{"mode":"read","statement":"RETURN $n","parameters":{"n":"json:{broken}"}}"#,
            r#"{"mode":"read","statement":"RETURN $n","parameters":{"n":"int:1","n":"int:2"}}"#,
            r#"{"mode":"read","statement":"RETURN $n","parameters":{},"parameters":{}}"#,
        ] {
            let error = parse(&format!("[{first},{late}]"))
                .err()
                .expect("a late failure must refuse the complete script");
            assert_eq!(error.code, 2, "{late}");
        }
        for input in ["", "[]", "{}", "[", "[{}]trailing"] {
            assert_eq!(parse(input).err().expect("invalid input refused").code, 2);
        }
    }

    #[test]
    fn statement_count_and_raw_input_bounds_include_the_exact_limit() {
        let record = r#"{"mode":"read","statement":"RETURN 1"}"#;
        let records = vec![record; MAX_TRANSACTION_BATCH_STATEMENTS].join(",");
        assert_eq!(
            parse(&format!("[{records}]"))
                .unwrap_or_else(|error| panic!("{}", error.message))
                .statements
                .len(),
            MAX_TRANSACTION_BATCH_STATEMENTS
        );
        assert_eq!(
            parse(&format!("[{records},{record}]"))
                .err()
                .expect("one extra statement refused")
                .code,
            2
        );

        let source = format!("[{record}]");
        let exact = format!("{source}{}", " ".repeat(MAX_INPUT_BYTES - source.len()));
        assert!(parse(&exact).is_ok());
        let error = parse(&(exact + " "))
            .err()
            .expect("one extra input byte refused");
        assert_eq!(error.code, 2);
        assert!(error.message.contains("exceeds 1048576 bytes"));
    }

    #[test]
    fn json_parameter_and_expanding_wire_values_obey_each_admission_bound() {
        let oversized_token = format!(
            r#"[{{"mode":"read","statement":"{}"}}]"#,
            "x".repeat(MAX_JSON_TOKEN_BYTES + 1)
        );
        let oversized_values = format!(
            r#"[{{"mode":"read","statement":"RETURN 1","extra":[{}]}}]"#,
            vec!["null"; MAX_JSON_VALUES].join(",")
        );
        for source in [oversized_token, oversized_values] {
            assert!(source.len() < MAX_INPUT_BYTES);
            let error = parse(&source).err().expect("JSON budget refused");
            assert_eq!(error.code, 2);
            assert!(error.message.contains("valid JSON array"));
        }

        let too_many = (0..=MAX_PARAMETERS)
            .map(|index| format!(r#""p{index}":"null""#))
            .collect::<Vec<_>>()
            .join(",");
        let long_name = format!(r#""{}":"null""#, "p".repeat(MAX_NAME_BYTES + 1));
        for fields in [too_many, long_name, r#""":"null""#.into()] {
            let source =
                format!(r#"[{{"mode":"read","statement":"RETURN 1","parameters":{{{fields}}}}}]"#);
            assert_eq!(parse(&source).err().expect("parameter limit refused").code, 2);
        }

        // Short decimal coordinates expand to packed four-byte floats. Every
        // typed token and the raw file fit, but the final statement must also
        // fit its transaction handle and the complete FGP frame envelope.
        let coordinates = vec!["0"; 16_384].join(",");
        let fields = |count| {
            (0..count)
                .map(|index| format!(r#""p{index:02}":"vector:{coordinates}""#))
                .collect::<Vec<_>>()
                .join(",")
        };
        let source = |count| {
            format!(
                r#"[{{"mode":"write","statement":"CREATE (n)","parameters":{{{}}}}}]"#,
                fields(count)
            )
        };
        assert!(parse(&source(15)).is_ok());
        let oversized = source(16);
        assert!(oversized.len() < MAX_INPUT_BYTES);
        let error = parse(&oversized)
            .err()
            .expect("expanded statement exceeds its wire frame");
        assert_eq!(error.code, 2);
        assert!(error.message.contains("frame limit"));
    }

    #[test]
    fn every_admission_checkpoint_preserves_the_original_cancellation_failure() {
        let source = r#"[
            {"mode":"write","statement":"CREATE (n {id:$x})","parameters":{"x":"int:1"}},
            {"mode":"read","statement":"RETURN $x","parameters":{"x":"text:fresh"}}
        ]"#;
        let mut total = 0;
        TransactionScript::parse(source, || {
            total += 1;
            Ok(())
        })
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert!(total > 6);
        for stop in 1..=total {
            let mut seen = 0;
            let error = TransactionScript::parse(source, || {
                seen += 1;
                if seen == stop {
                    let mut failure = Failure::io("original cancellation");
                    failure.offset = Some(17);
                    failure.result_row_limit = Some(9);
                    Err(failure)
                } else {
                    Ok(())
                }
            })
            .err()
            .expect("an interrupted script cannot be admitted");
            assert_eq!(seen, stop);
            assert_eq!(error.code, 5);
            assert_eq!(error.class, "io");
            assert_eq!(error.refusal, crate::refusal::Code::Io);
            assert_eq!(error.offset, Some(17));
            assert_eq!(error.result_row_limit, Some(9));
            assert_eq!(error.message, "original cancellation");
        }
    }

    #[test]
    fn large_json_scan_observes_cancellation_before_a_malformed_late_suffix() {
        let source = format!(
            r#"[{{"mode":"write","statement":"CREATE (n)"}},{}invalid]"#,
            " ".repeat(2 * fgdb_protocol::json::JSON_PARSE_RUN_BYTES)
        );
        let mut polls = 0;
        let error = TransactionScript::parse(&source, || {
            polls += 1;
            if polls == 3 {
                Err(Failure::io("cancelled during JSON parsing"))
            } else {
                Ok(())
            }
        })
        .err()
        .expect("interruption wins over the later syntax error");
        assert_eq!(polls, 3);
        assert_eq!(error.code, 5);
        assert_eq!(error.class, "io");
        assert_eq!(error.message, "cancelled during JSON parsing");
    }
}
