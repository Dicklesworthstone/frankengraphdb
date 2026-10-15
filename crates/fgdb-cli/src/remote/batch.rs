//! A finite, typed file becomes ONE remote native write batch, never a loop
//! of independently committed EXECUTEs. Complete local admission precedes
//! authentication; the server still owns binding, authority, budgets and the
//! sole commit. A lost response never authorizes replay of the batch.

use super::{Failure, Json, emit, failure, parameter, parse_json};
use asupersync::Cx;
use asupersync::io::AsyncReadExt;
use fgdb_gql::csv_parameters::{CsvParameterLimits, decode_csv_parameters};
use fgdb_gql::{GqlParameterSpec, GqlParameterType, GqlParameterValue, GqlParameters};
use fgdb_protocol::MAX_HEADER_LEN;
use fgdb_protocol::body::{
    Body, ExecuteBatch, MAX_BATCH_ARGUMENT_SETS, MAX_NAME_BYTES, MAX_PARAMETERS, Outcome, WireValue,
};
use fgdb_protocol::client::{CLIENT_MAX_FRAME, Client, ClientError};
use fgdb_types::{CanonicalScalar, CanonicalScalarKind};
use std::io::Write;
use std::path::Path;

/// Bounds raw input separately from the complete encoded request. The
/// negotiated peer limit can be smaller; the client enforces it before send.
const MAX_INPUT_BYTES: usize = CLIENT_MAX_FRAME as usize;

/// A decoding schema, not a statement/capability schema. The native server
/// still binds the resulting values and may refuse them. No values infer types.
pub(super) struct CsvSchema {
    columns: Vec<GqlParameterSpec>,
}

impl CsvSchema {
    /// `name=text,age=int64`; names use the native parameter-name validator.
    pub(super) fn parse(text: &str) -> Result<Self, Failure> {
        if text.is_empty() || text.len() > super::MAX_JSON_TOKEN_BYTES {
            return Err(Failure::usage("--csv-types needs a bounded name=type list"));
        }
        let mut names = GqlParameters::new();
        let mut columns = Vec::new();
        for declaration in text.split(',') {
            let (name, kind) = declaration
                .split_once('=')
                .ok_or_else(|| Failure::usage("--csv-types needs name=type,name=type,..."))?;
            let parameter_type = match kind {
                "int64" => GqlParameterType::Int64,
                "uint64" => GqlParameterType::UInt64,
                "int" => GqlParameterType::Scalar(CanonicalScalarKind::Int),
                "text" => GqlParameterType::Scalar(CanonicalScalarKind::Text),
                "bool" => GqlParameterType::Scalar(CanonicalScalarKind::Bool),
                "null" => GqlParameterType::Scalar(CanonicalScalarKind::Null),
                "float" => GqlParameterType::Scalar(CanonicalScalarKind::Float),
                "decimal" => GqlParameterType::Scalar(CanonicalScalarKind::Decimal),
                "bytes" => GqlParameterType::Scalar(CanonicalScalarKind::Bytes),
                _ => return Err(Failure::usage("unsupported --csv-types column type")),
            };
            names
                .insert(name, GqlParameterValue::Int64(0))
                .map_err(|_| Failure::usage("invalid, duplicate or over-limit CSV column name"))?;
            columns.push(GqlParameterSpec {
                name: name.to_owned(),
                parameter_type,
                requires_positive: false,
                occurrences: 1,
            });
        }
        columns.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        Ok(Self { columns })
    }
}

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
                return Err(Failure::usage(format!(
                    "argument set {at} must be an object"
                )));
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
        Self::admit(statement, argument_sets, checkpoint)
    }

    /// Decode ordinary CSV through the SAME native decoder as embedded
    /// imports. Only the exact scalar-to-wire bridge belongs to this adapter.
    /// One bounded decode runs between checkpoints; conversion checkpoints
    /// every row and field. No partial batch escapes any failure.
    pub(super) fn parse_csv(
        statement: &str,
        input: &str,
        schema: &CsvSchema,
        mut checkpoint: impl FnMut() -> Result<(), Failure>,
    ) -> Result<Self, Failure> {
        checkpoint()?;
        let rows = decode_csv_parameters(
            input,
            &schema.columns,
            CsvParameterLimits {
                max_input_bytes: MAX_INPUT_BYTES,
                max_records: MAX_BATCH_ARGUMENT_SETS,
                max_columns: MAX_PARAMETERS,
                max_field_bytes: super::MAX_JSON_TOKEN_BYTES,
                max_parameter_bytes: MAX_INPUT_BYTES,
            },
        )
        .map_err(Failure::usage)?;
        checkpoint()?;
        let mut argument_sets = Vec::new();
        argument_sets
            .try_reserve_exact(rows.len())
            .map_err(Failure::io)?;
        for row in rows {
            checkpoint()?;
            let mut parameters = Vec::new();
            parameters
                .try_reserve_exact(schema.columns.len())
                .map_err(Failure::io)?;
            for column in &schema.columns {
                checkpoint()?;
                let value = row
                    .get(&column.name)
                    .ok_or_else(|| Failure::usage("CSV decoder omitted a declared parameter"))?;
                parameters.push((column.name.clone(), csv_wire(value)?));
            }
            argument_sets.push(parameters);
        }
        Self::admit(statement, argument_sets, checkpoint)
    }

    /// Both file grammars share wire admission and the single submission path.
    fn admit(
        statement: &str,
        argument_sets: Vec<Vec<(String, WireValue)>>,
        mut checkpoint: impl FnMut() -> Result<(), Failure>,
    ) -> Result<Self, Failure> {
        checkpoint()?;
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

    pub(super) async fn read(
        cx: &Cx,
        path: &Path,
        statement: &str,
        csv: Option<&CsvSchema>,
    ) -> Result<Self, Failure> {
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
        match csv {
            Some(schema) => Self::parse_csv(statement, input, schema, || {
                cx.checkpoint().map_err(Failure::io)
            }),
            None => Self::parse(statement, input, || cx.checkpoint().map_err(Failure::io)),
        }
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

fn csv_wire(value: GqlParameterValue) -> Result<WireValue, Failure> {
    match value {
        GqlParameterValue::Int64(value) => Ok(WireValue::Int(value)),
        GqlParameterValue::UInt64(value) => Ok(WireValue::Count(value)),
        GqlParameterValue::Scalar(value) => match value.value() {
            CanonicalScalar::Null => Ok(WireValue::Null),
            CanonicalScalar::Bool(value) => Ok(WireValue::Bool(*value)),
            CanonicalScalar::Int(value) => Ok(WireValue::Int(*value)),
            CanonicalScalar::Float(value) => Ok(WireValue::Float(value.get())),
            CanonicalScalar::Decimal(value) => Ok(WireValue::Decimal(value.to_string())),
            CanonicalScalar::Text(value) => Ok(WireValue::Text(value.as_str().to_owned())),
            CanonicalScalar::Bytes(value) => Ok(WireValue::Bytes(value.as_slice().to_vec())),
            _ => Err(Failure::usage("unsupported native CSV scalar")),
        },
        _ => Err(Failure::usage("unsupported native CSV parameter")),
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
                (
                    "name".into(),
                    WireValue::Text("'); DETACH DELETE n; //".into())
                ),
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

#[cfg(test)]
mod csv_tests {
    use super::*;

    fn schema(text: &str) -> CsvSchema {
        CsvSchema::parse(text).unwrap_or_else(|error| panic!("{}", error.message))
    }

    fn prepare(input: &str, types: &str) -> Result<PreparedBatch, Failure> {
        PreparedBatch::parse_csv(
            "CREATE (:Person {name:$name})",
            input,
            &schema(types),
            || Ok(()),
        )
    }

    #[test]
    fn explicit_csv_schema_uses_native_name_count_and_type_rules() {
        for invalid in [
            "",
            "name",
            "name=",
            "=text",
            "$name=text",
            "name=text,name=int64",
            "name=map",
            "name=text,",
            "name=text,other=not_a_type",
        ] {
            assert_eq!(CsvSchema::parse(invalid).err().unwrap().code, 2);
        }
        let maximum = "n".repeat(64);
        assert!(CsvSchema::parse(&format!("{maximum}=text")).is_ok());
        assert!(CsvSchema::parse(&format!("{maximum}x=text")).is_err());
        let columns = (0..1_000)
            .map(|n| format!("p{n}=text"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(CsvSchema::parse(&columns).is_ok());
        assert!(CsvSchema::parse(&format!("{columns},extra=text")).is_err());
        assert!(CsvSchema::parse(&"x".repeat(super::super::MAX_JSON_TOKEN_BYTES + 1)).is_err());
    }

    #[test]
    fn csv_preserves_exact_domains_reordered_headers_quotes_and_nulls() {
        let input = concat!(
            "\u{feff}payload,score,amount,id,active,name\r\n",
            "00Aa,1.25,9007199254740993.000000000000000001,18446744073709551615,true,\"Ada, \"\"A\"\"\"\r\n",
            "\\N,\\N,\\N,0,\\N,\"\\N\"\r\n",
        );
        let prepared = prepare(
            input,
            "name=text,id=uint64,score=float,amount=decimal,payload=bytes,active=bool",
        )
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(
            prepared.body.argument_sets,
            vec![
                vec![
                    ("active".into(), WireValue::Bool(true)),
                    (
                        "amount".into(),
                        WireValue::Decimal("9007199254740993.000000000000000001".into())
                    ),
                    ("id".into(), WireValue::Count(u64::MAX)),
                    ("name".into(), WireValue::Text("Ada, \"A\"".into())),
                    ("payload".into(), WireValue::Bytes(vec![0, 170])),
                    ("score".into(), WireValue::Float(1.25)),
                ],
                vec![
                    ("active".into(), WireValue::Null),
                    ("amount".into(), WireValue::Null),
                    ("id".into(), WireValue::Count(0)),
                    ("name".into(), WireValue::Text("\\N".into())),
                    ("payload".into(), WireValue::Null),
                    ("score".into(), WireValue::Null),
                ],
            ]
        );
        assert_eq!(
            ExecuteBatch::decode(&prepared.body.encode().unwrap()).unwrap(),
            prepared.body
        );
    }

    #[test]
    fn csv_keeps_empty_binary_text_and_nullable_integer_distinct() {
        let prepared = prepare(
            "b,t,n\n\"\",\"\",\\N\n\\N,\"multi\nline\",7",
            "t=text,n=int,b=bytes",
        )
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(
            prepared.body.argument_sets,
            vec![
                vec![
                    ("b".into(), WireValue::Bytes(vec![])),
                    ("n".into(), WireValue::Null),
                    ("t".into(), WireValue::Text(String::new())),
                ],
                vec![
                    ("b".into(), WireValue::Null),
                    ("n".into(), WireValue::Int(7)),
                    ("t".into(), WireValue::Text("multi\nline".into())),
                ],
            ]
        );
        assert!(prepare("n\n\\N", "n=int64").is_err());
        assert!(prepare("n\n-1", "n=uint64").is_err());
        assert!(prepare("n\n18446744073709551616", "n=uint64").is_err());
        assert!(prepare("n\nNaN", "n=float").is_err());
        assert!(prepare("n\n1e2", "n=decimal").is_err());
    }

    #[test]
    fn csv_late_errors_and_limits_never_return_a_private_prefix() {
        for input in [
            "",
            "name\n",
            "name,name\na,b",
            "other\na",
            "name\na\nb,c",
            "name\na\n\"broken",
        ] {
            assert_eq!(prepare(input, "name=text").err().unwrap().code, 2);
        }
        let exact = format!("n\n{}", "0\n".repeat(MAX_BATCH_ARGUMENT_SETS));
        assert_eq!(
            prepare(&exact, "n=int64")
                .unwrap_or_else(|error| panic!("{}", error.message))
                .body
                .argument_sets
                .len(),
            MAX_BATCH_ARGUMENT_SETS
        );
        // Admission of the next record fails before even decoding its value.
        let error = prepare(&(exact + "not_an_integer"), "n=int64")
            .err()
            .unwrap();
        assert!(error.message.contains("Records"), "{}", error.message);
        assert!(prepare(&"x".repeat(MAX_INPUT_BYTES + 1), "name=text").is_err());
        let field = format!(
            "name\n{}",
            "x".repeat(super::super::MAX_JSON_TOKEN_BYTES + 1)
        );
        assert!(prepare(&field, "name=text").is_err());
    }

    #[test]
    fn every_csv_adapter_checkpoint_can_refuse_before_submission() {
        let schema = schema("name=text,n=int64");
        let input = "name,n\nAda,1\nBob,2";
        let mut total = 0;
        PreparedBatch::parse_csv("CREATE (:Person {name:$name})", input, &schema, || {
            total += 1;
            Ok(())
        })
        .unwrap_or_else(|error| panic!("{}", error.message));
        for stop in 1..=total {
            let mut seen = 0;
            let result =
                PreparedBatch::parse_csv("CREATE (:Person {name:$name})", input, &schema, || {
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

    #[test]
    fn csv_flags_require_an_exclusive_remote_write_file() {
        let options = |verb: &str, flags: &[&str]| {
            let mut args: Vec<String> = [
                verb,
                "CREATE (:Person {name:$name})",
                "--addr",
                "127.0.0.1:7688",
                "--database",
                "social",
                "--token-file",
                "unused-token",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect();
            args.extend(flags.iter().map(|flag| (*flag).to_owned()));
            super::super::parse(&args)
        };
        let good = options(
            "write",
            &["--rows-file", "people.csv", "--csv-types", "name=text"],
        )
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert!(good.csv_types.is_some());
        assert!(
            options("write", &["--rows-file", "people.json"])
                .unwrap_or_else(|error| panic!("{}", error.message))
                .csv_types
                .is_none()
        );
        for bad in [
            vec!["--csv-types", "name=text"],
            vec!["--rows-file", "a", "--csv-types"],
            vec![
                "--rows-file",
                "a",
                "--csv-types",
                "name=text",
                "--csv-types",
                "name=int",
            ],
            vec![
                "--rows-file",
                "a",
                "--csv-types",
                "name=text",
                "--param",
                "name=text:Ada",
            ],
        ] {
            assert_eq!(options("write", &bad).err().unwrap().code, 2);
        }
        for verb in ["query", "subscribe"] {
            assert_eq!(
                options(verb, &["--rows-file", "a", "--csv-types", "name=text"])
                    .err()
                    .unwrap()
                    .code,
                2
            );
        }
    }
}
