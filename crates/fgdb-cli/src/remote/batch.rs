//! A finite, typed file becomes ONE remote native write batch, never a loop
//! of independently committed EXECUTEs. Complete local admission precedes
//! authentication; the server still owns binding, authority, budgets and the
//! sole commit. A lost response never authorizes replay of the batch.

use super::{Failure, Json, emit, failure, parameter, parse_json};
use asupersync::Cx;
use asupersync::io::AsyncReadExt;
use fgdb_protocol::MAX_HEADER_LEN;
use fgdb_protocol::body::{
    Body, ExecuteBatch, MAX_BATCH_ARGUMENT_SETS, MAX_NAME_BYTES, MAX_PARAMETERS, Outcome,
};
use fgdb_protocol::client::{CLIENT_MAX_FRAME, Client, ClientError};
use std::io::Write;
use std::path::Path;

/// Bounds raw input separately from the complete encoded request. The
/// negotiated peer limit can be smaller; the client enforces it before send.
const MAX_INPUT_BYTES: usize = CLIENT_MAX_FRAME as usize;

/// No Clone or payload-bearing Debug: consumption transfers this one request
/// to the native client exactly once. This is not a durable prepared handle.
pub(super) struct PreparedBatch {
    body: ExecuteBatch,
}

impl PreparedBatch {
    /// Values are the ordinary --param spellings, carried as JSON strings.
    /// This retains uint64, packed vectors, nulls and nested json: parameters
    /// without inferring types from JSON numbers or interpolating query text.
    pub(super) fn parse(
        statement: &str,
        input: &str,
        mut checkpoint: impl FnMut() -> Result<(), Failure>,
    ) -> Result<Self, Failure> {
        checkpoint()?;
        if input.len() > MAX_INPUT_BYTES {
            return Err(Failure::usage("--rows-file exceeds 1048576 bytes"));
        }
        let json = parse_json(input, super::MAX_JSON_VALUES, super::MAX_JSON_TOKEN_BYTES)
            .map_err(|_| Failure::usage("--rows-file must contain one valid JSON array"))?;
        checkpoint()?;
        let Json::Array(records) = json else {
            return Err(Failure::usage("--rows-file must contain a JSON array"));
        };
        if records.is_empty() || records.len() > MAX_BATCH_ARGUMENT_SETS {
            return Err(Failure::usage(format!(
                "--rows-file needs 1..={MAX_BATCH_ARGUMENT_SETS} argument sets"
            )));
        }
        let mut argument_sets = Vec::new();
        argument_sets
            .try_reserve_exact(records.len())
            .map_err(Failure::io)?;
        for (index, record) in records.into_iter().enumerate() {
            checkpoint()?;
            let at = index + 1;
            let Json::Object(fields) = record else {
                return Err(Failure::usage(format!("argument set {at} must be an object")));
            };
            if fields.len() > MAX_PARAMETERS {
                return Err(Failure::usage(format!(
                    "argument set {at} exceeds {MAX_PARAMETERS} parameters"
                )));
            }
            let mut parameters = Vec::new();
            parameters
                .try_reserve_exact(fields.len())
                .map_err(Failure::io)?;
            // The shared JSON decoder rejects duplicate keys and owns a
            // BTreeMap: its order is already canonical UTF-8 name order.
            for (name, value) in fields {
                checkpoint()?;
                if name.is_empty() || name.len() > MAX_NAME_BYTES {
                    return Err(Failure::usage(format!(
                        "argument set {at} has an empty or oversized parameter name"
                    )));
                }
                let Json::String(typed) = value else {
                    return Err(Failure::usage(format!(
                        "argument set {at} values must be typed strings, such as text:Ada or int:7"
                    )));
                };
                let value = parameter(&typed).map_err(|_| {
                    // Do not echo a source value, source path or bearer token.
                    Failure::usage(format!("argument set {at} has an invalid typed parameter"))
                })?;
                parameters.push((name, value));
            }
            argument_sets.push(parameters);
        }
        let body = ExecuteBatch {
            statement: statement.to_owned(),
            argument_sets,
        };
        // Apply the authoritative wire value/depth/size contract before any
        // connection or credential read. Also cap the entire Ready frame,
        // rather than treating per-value bounds as a total memory/wire cap.
        let encoded = body.encode().map_err(Failure::usage)?;
        if encoded.len() > (CLIENT_MAX_FRAME as usize).saturating_sub(MAX_HEADER_LEN) {
            return Err(Failure::usage(
                "batch exceeds the client frame limit; it was not sent",
            ));
        }
        drop(encoded);
        checkpoint()?;
        Ok(Self { body })
    }

    pub(super) async fn read(cx: &Cx, path: &Path, statement: &str) -> Result<Self, Failure> {
        cx.checkpoint().map_err(Failure::io)?;
        // Refuse ordinary FIFOs/devices before opening. Recheck the opened
        // handle and bound actual reads too; path metadata is not a byte cap.
        let named = asupersync::fs::metadata(path)
            .await
            .map_err(|_| Failure::io("cannot inspect --rows-file"))?;
        if !named.is_file() {
            return Err(Failure::usage("--rows-file must be a regular file"));
        }
        let mut file = asupersync::fs::File::open(path)
            .await
            .map_err(|_| Failure::io("cannot open --rows-file"))?;
        let metadata = file
            .metadata()
            .await
            .map_err(|_| Failure::io("cannot inspect --rows-file"))?;
        if !metadata.is_file() {
            return Err(Failure::usage("--rows-file must be a regular file"));
        }
        if metadata.len() > MAX_INPUT_BYTES as u64 {
            return Err(Failure::usage("--rows-file exceeds 1048576 bytes"));
        }
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            cx.checkpoint().map_err(Failure::io)?;
            let capacity = buffer.len().min(MAX_INPUT_BYTES - bytes.len() + 1);
            let count = match file.read(&mut buffer[..capacity]).await {
                Ok(count) => count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(Failure::io("cannot read --rows-file")),
            };
            if count == 0 {
                break;
            }
            if count > MAX_INPUT_BYTES - bytes.len() {
                return Err(Failure::usage("--rows-file exceeds 1048576 bytes"));
            }
            bytes.try_reserve(count).map_err(Failure::io)?;
            bytes.extend_from_slice(&buffer[..count]);
        }
        let input = core::str::from_utf8(&bytes)
            .map_err(|_| Failure::usage("--rows-file must be UTF-8"))?;
        Self::parse(statement, input, || cx.checkpoint().map_err(Failure::io))
    }

    pub(super) async fn execute(
        self,
        cx: &Cx,
        client: &mut Client,
        robot: bool,
        out: &mut impl Write,
    ) -> Result<(), Failure> {
        let records = self.body.argument_sets.len();
        let outcome = client
            .execute_batch(cx, &self.body.statement, self.body.argument_sets)
            .await
            .map_err(batch_failure)?;
        let (seq, statements) = match outcome {
            Outcome::WriteCommitted { seq, statements }
            | Outcome::ReadClosed { seq, statements } => (seq, statements),
            // Never turn a post-submission protocol fault into a retry hint.
            Outcome::Rows { .. } => {
                return Err(Failure::io(
                    "unexpected batch outcome; do not replay automatically",
                ));
            }
        };
        let delivered = if robot {
            emit(
                out,
                &format!(
                    r#"{{"v":1,"event":"result","kind":"written","seq":{seq},"statements":{statements},"records":{records}}}"#
                ),
            )
        } else {
            emit(
                out,
                &format!(
                    "completed {records} argument set(s), {statements} statement(s) at seq {seq}"
                ),
            )
        };
        delivered
            .and_then(|()| out.flush().map_err(Failure::io))
            .map_err(|error| {
                Failure::io(format!(
                    "batch completed at seq {seq}, but output delivery failed; do not replay: {}",
                    error.message
                ))
            })
    }
}

fn batch_failure(error: ClientError) -> Failure {
    match error {
        error @ ClientError::Server { .. } => failure(error),
        error => Failure::io(format!(
            "remote batch completion was not confirmed; do not replay automatically: {error}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_protocol::body::WireValue;

    fn parse(input: &str) -> Result<PreparedBatch, Failure> {
        PreparedBatch::parse("CREATE (:Person {name:$name})", input, || Ok(()))
    }

    #[test]
    fn typed_values_keep_names_order_rows_and_exact_domains() {
        let prepared = parse(
            r#"[
            {"z":"uint:18446744073709551615","name":"text:'); DETACH DELETE n; //","a":"null"},
            {"name":"text:λ\nAda","a":"int:-7","z":"json:[true,null, {\"x\":1}]"}
        ]"#,
        )
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(prepared.body.argument_sets.len(), 2);
        assert_eq!(
            prepared.body.argument_sets[0],
            vec![
                ("a".into(), WireValue::Null),
                ("name".into(), WireValue::Text("'); DETACH DELETE n; //".into())),
                ("z".into(), WireValue::Count(u64::MAX)),
            ]
        );
        assert_eq!(
            prepared.body.argument_sets[1][1].1,
            WireValue::Text("λ\nAda".into())
        );
        assert!(matches!(
            prepared.body.argument_sets[1][2].1,
            WireValue::List(_)
        ));
        assert_eq!(prepared.body.statement, "CREATE (:Person {name:$name})");
        let bytes = prepared.body.encode().unwrap();
        assert_eq!(ExecuteBatch::decode(&bytes).unwrap(), prepared.body);
    }

    #[test]
    fn complete_file_admission_refuses_malformed_late_or_ambiguous_records() {
        for source in [
            "",
            "{}",
            "[]",
            "[null]",
            "[{}]junk",
            "[{},]",
            r#"[{"name":"text:a"},{"name":7}]"#,
            r#"[{"name":"text:a"},{"name":"uint:18446744073709551616"}]"#,
            r#"[{"name":"text:a","name":"text:b"}]"#,
            r#"[{"":"text:a"}]"#,
            r#"[{"name":"text:a"},[1]]"#,
        ] {
            assert_eq!(
                parse(source).err().expect("must refuse whole batch").code,
                2,
                "{source}"
            );
        }
        // Empty maps are useful with parameterless scripts; the native binder
        // still decides whether the statement's parameter schema matches.
        assert!(parse("[{},{}]").is_ok());
    }

    #[test]
    fn raw_source_and_argument_count_bounds_are_exact() {
        let exact = format!("[{{}}]{}", " ".repeat(MAX_INPUT_BYTES - 4));
        assert!(parse(&exact).is_ok());
        assert_eq!(parse(&(exact + " ")).err().unwrap().code, 2);
        let records = vec!["{}"; MAX_BATCH_ARGUMENT_SETS].join(",");
        assert!(parse(&format!("[{records}]")).is_ok());
        assert_eq!(parse(&format!("[{records},{{}}]")).err().unwrap().code, 2);
    }

    #[test]
    fn whole_ready_frame_is_bounded_separately_from_input() {
        let maximum = CLIENT_MAX_FRAME as usize;
        // The body has two u32 lengths, one empty set count, and statement.
        // Derive overhead from the real codec, not a second encoder.
        let overhead = ExecuteBatch {
            statement: String::new(),
            argument_sets: vec![vec![]],
        }
        .encode()
        .unwrap()
        .len();
        let statement = " ".repeat(maximum - MAX_HEADER_LEN - overhead);
        assert!(PreparedBatch::parse(&statement, "[{}]", || Ok(())).is_ok());
        let error = PreparedBatch::parse(&(statement + " "), "[{}]", || Ok(()))
            .err()
            .unwrap();
        assert_eq!(error.code, 2);
        assert!(error.message.contains("frame limit"));
    }

    #[test]
    fn every_preparation_checkpoint_can_refuse_without_a_request() {
        let source = r#"[{"name":"text:Ada","n":"int:1"},{"name":"text:Bob","n":"int:2"}]"#;
        let mut total = 0;
        PreparedBatch::parse("CREATE (:Person {name:$name})", source, || {
            total += 1;
            Ok(())
        })
        .unwrap_or_else(|error| panic!("{}", error.message));
        for stop in 1..=total {
            let mut seen = 0;
            let result = PreparedBatch::parse("CREATE (:Person {name:$name})", source, || {
                seen += 1;
                if seen == stop {
                    Err(Failure::io("stopped"))
                } else {
                    Ok(())
                }
            });
            assert_eq!(result.err().expect("no admitted request").code, 5);
            assert_eq!(seen, stop);
        }
    }
}
