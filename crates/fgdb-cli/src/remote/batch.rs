//! A finite typed file is admitted before authentication. Writes become one
//! atomic native batch; queries reuse one prepared read on one connection.
//! Ordinary prepared reads complete independently. Consistent read batches
//! share one pinned session and allowance, with one final output completion.
//! The server owns binding, authority and budgets.
//! Refusals never retry, reprepare or switch execution modes.

use super::{Failure, Json, emit, failure, parameter};
use asupersync::Cx;
use asupersync::io::AsyncReadExt;
use fgdb_gql::csv_parameters::{CsvParameterLimits, decode_csv_parameters};
use fgdb_gql::{GqlParameterSpec, GqlParameterType, GqlParameterValue, GqlParameters};
use fgdb_protocol::MAX_HEADER_LEN;
use fgdb_protocol::body::{
    Body, Execute, ExecuteBatch, ExecuteMode, ExecutePreparedReadBatch, ExecuteReadStatements,
    MAX_BATCH_ARGUMENT_SETS, MAX_NAME_BYTES, MAX_PARAMETERS, MAX_READ_BATCH_STATEMENTS,
    Outcome, Prepare, PrepareMode, PreparedHandle, PreparedReadMode, WireValue,
};
use fgdb_protocol::client::{CLIENT_MAX_FRAME, Client, ClientError};
use fgdb_types::{CanonicalScalar, CanonicalScalarKind};
use std::io::Write;
use std::path::Path;

/// Bounds raw input separately from the complete encoded request. The
/// negotiated peer limit can be smaller; the client enforces it before send.
const MAX_INPUT_BYTES: usize = CLIENT_MAX_FRAME as usize;

/// Read a finite file before credentials or transport are acquired. Metadata
/// and actual bytes both enforce the bound, including a file growing mid-read.
async fn read_input(cx: &Cx, path: &Path, flag: &str) -> Result<Vec<u8>, Failure> {
    cx.checkpoint().map_err(Failure::io)?;
    let named = asupersync::fs::metadata(path)
        .await
        .map_err(|_| Failure::io(format!("cannot inspect {flag}")))?;
    if !named.is_file() {
        return Err(Failure::usage(format!("{flag} must be a regular file")));
    }
    let mut file = asupersync::fs::File::open(path)
        .await
        .map_err(|_| Failure::io(format!("cannot open {flag}")))?;
    let metadata = file
        .metadata()
        .await
        .map_err(|_| Failure::io(format!("cannot inspect {flag}")))?;
    if !metadata.is_file() {
        return Err(Failure::usage(format!("{flag} must be a regular file")));
    }
    if metadata.len() > MAX_INPUT_BYTES as u64 {
        return Err(Failure::usage(format!("{flag} exceeds 1048576 bytes")));
    }
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        cx.checkpoint().map_err(Failure::io)?;
        let capacity = buffer.len().min(MAX_INPUT_BYTES - bytes.len() + 1);
        let count = match file.read(&mut buffer[..capacity]).await {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(Failure::io(format!("cannot read {flag}"))),
        };
        if count == 0 {
            break;
        }
        if count > MAX_INPUT_BYTES - bytes.len() {
            return Err(Failure::usage(format!("{flag} exceeds 1048576 bytes")));
        }
        bytes.try_reserve(count).map_err(Failure::io)?;
        bytes.extend_from_slice(&buffer[..count]);
    }
    cx.checkpoint().map_err(Failure::io)?;
    Ok(bytes)
}

/// Heterogeneous reads retain separate texts and argument maps. They use the
/// server's native batch operation; never a loop of independent read calls.
pub(super) struct ReadStatements {
    body: ExecuteReadStatements,
}

impl ReadStatements {
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
            return Err(Failure::usage(
                "--statements-file must contain a JSON array",
            ));
        };
        if records.is_empty() || records.len() > MAX_READ_BATCH_STATEMENTS {
            return Err(Failure::usage(format!(
                "--statements-file needs 1..={MAX_READ_BATCH_STATEMENTS} reads"
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
                return Err(Failure::usage(format!("read {at} must be an object")));
            };
            let Some(Json::String(statement)) = fields.remove("statement") else {
                return Err(Failure::usage(format!(
                    "read {at} needs a statement string"
                )));
            };
            if statement.trim().is_empty() {
                return Err(Failure::usage(format!("read {at} has an empty statement")));
            }
            let arguments = fields.remove("parameters");
            if !fields.is_empty() {
                return Err(Failure::usage(format!(
                    "read {at} accepts only statement and parameters fields"
                )));
            }
            let parameters = match arguments {
                Some(arguments) => parameters(arguments, at, &mut checkpoint)?,
                None => Vec::new(),
            };
            statements.push(Execute {
                mode: ExecuteMode::Read,
                statement,
                parameters,
            });
        }
        checkpoint()?;
        let body = ExecuteReadStatements { statements };
        // The codec applies one value-node allowance to the WHOLE body, as
        // well as per-read limits. The Ready header has its own wire charge.
        let encoded = body.encode().map_err(Failure::usage)?;
        if encoded.len() > (CLIENT_MAX_FRAME as usize).saturating_sub(MAX_HEADER_LEN) {
            return Err(Failure::usage(
                "read statements exceed the client frame limit; they were not sent",
            ));
        }
        drop(encoded);
        checkpoint()?;
        Ok(Self { body })
    }

    pub(super) async fn read(cx: &Cx, path: &Path) -> Result<Self, Failure> {
        let bytes = read_input(cx, path, "--statements-file").await?;
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
        execute_consistent_reads(
            cx,
            client,
            ConsistentReads::Statements(self.body),
            robot,
            out,
        )
        .await
    }
}

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

/// No Clone or payload-bearing Debug: consumption transfers the argument file
/// exactly once. The bounded codec envelope is shared by both commands; it is
/// sent as ExecuteBatch only for writes. This is not a server prepared handle.
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
        let json = fgdb_protocol::json::parse_json_controlled(
            input,
            super::MAX_JSON_VALUES,
            super::MAX_JSON_TOKEN_BYTES,
            |_| checkpoint(),
        )
        .map_err(|error| match error {
            fgdb_protocol::json::JsonParseError::Invalid(_) => {
                Failure::usage("--rows-file must contain one valid JSON array")
            }
            fgdb_protocol::json::JsonParseError::Interrupted(error) => error,
        })?;
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
            argument_sets.push(parameters(record, at, &mut checkpoint)?);
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

    /// Both file grammars share wire/value admission and a total input bound.
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
        let bytes = read_input(cx, path, "--rows-file").await?;
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

    /// Prepare once, then stream each independently completed read without
    /// retaining an answer bag or granting a fresh fallback after refusal.
    /// The first argument set supplies structural operand types to PREPARE;
    /// every execution, including the first, binds its actual values again.
    pub(super) async fn execute_reads(
        self,
        cx: &Cx,
        client: &mut Client,
        mode: ExecuteMode,
        robot: bool,
        out: &mut impl Write,
    ) -> Result<(), Failure> {
        let mode = match mode {
            ExecuteMode::Read => PreparedReadMode::Buffered,
            ExecuteMode::ReadStream => PreparedReadMode::SourceStream,
            ExecuteMode::ReadOrdered => PreparedReadMode::OrderedStream,
            _ => {
                return Err(Failure::usage(
                    "prepared argument files require a read mode",
                ));
            }
        };
        cx.checkpoint().map_err(Failure::query)?;
        let representative = self
            .body
            .argument_sets
            .first()
            .ok_or_else(|| Failure::usage("--rows-file needs at least one argument set"))?
            .clone();
        let handle = client
            .prepare_read(cx, &self.body.statement, representative)
            .await
            .map_err(failure)?;
        for (index, parameters) in self.body.argument_sets.into_iter().enumerate() {
            cx.checkpoint().map_err(Failure::query)?;
            let sink = std::sync::Mutex::new(super::ReadOutput {
                out: &mut *out,
                robot,
                width: None,
                sent: 0,
                failed: None,
            });
            let lock = || {
                sink.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            };
            let result = client
                .execute_prepared_streaming(
                    cx,
                    mode,
                    handle,
                    parameters,
                    |columns| lock().columns(cx, columns),
                    |row| lock().row(cx, &row),
                )
                .await;
            sink.into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .finish(result)
                .map_err(|mut error| {
                    error.message = format!("argument set {}: {}", index + 1, error.message);
                    error
                })?;
        }
        // The caller closes this one connection, which releases its template.
        // No extra release round trip is needed after the final flushed answer.
        Ok(())
    }
    /// All statements share one native session and one execution allowance.
    /// A successful per-result boundary is provisional until the batch END.
    pub(super) async fn execute_consistent_reads(
        self,
        cx: &Cx,
        client: &mut Client,
        robot: bool,
        out: &mut impl Write,
    ) -> Result<(), Failure> {
        execute_consistent_reads(
            cx,
            client,
            ConsistentReads::Parameters(self.body),
            robot,
            out,
        )
        .await
    }

    /// Both requests must fit before credentials are read. The handle is
    /// fixed-width and nonzero; these placeholder bytes never leave the client.
    /// The negotiated limit is checked again by the client before each send.
    pub(super) fn admit_prepared_reads(&self) -> Result<(), Failure> {
        if self.body.argument_sets.is_empty()
            || self.body.argument_sets.len() > MAX_READ_BATCH_STATEMENTS
        {
            return Err(Failure::usage(format!(
                "--prepared-batch needs 1..={MAX_READ_BATCH_STATEMENTS} argument sets"
            )));
        }
        let prepare = Prepare {
            mode: PrepareMode::Read,
            statement: self.body.statement.clone(),
            parameters: self.body.argument_sets[0].clone(),
        };
        let execute = ExecutePreparedReadBatch {
            handle: PreparedHandle([1; 16]),
            argument_sets: self.body.argument_sets.clone(),
        };
        let maximum = (CLIENT_MAX_FRAME as usize).saturating_sub(MAX_HEADER_LEN);
        if prepare.encode().map_err(Failure::usage)?.len() > maximum
            || execute.encode().map_err(Failure::usage)?.len() > maximum
        {
            return Err(Failure::usage(
                "prepared read batch exceeds the client frame limit; it was not sent",
            ));
        }
        Ok(())
    }

    /// Prepare once, then rebind every argument set in one native read batch.
    /// Preparing does not acquire the execution snapshot or consume a row.
    pub(super) async fn execute_prepared_reads(
        self,
        cx: &Cx,
        client: &mut Client,
        robot: bool,
        out: &mut impl Write,
    ) -> Result<(), Failure> {
        cx.checkpoint().map_err(Failure::query)?;
        let representative = self
            .body
            .argument_sets
            .first()
            .ok_or_else(|| Failure::usage("--rows-file needs at least one argument set"))?
            .clone();
        let handle = client
            .prepare_read(cx, &self.body.statement, representative)
            .await
            .map_err(failure)?;
        execute_consistent_reads(
            cx,
            client,
            ConsistentReads::Prepared(ExecutePreparedReadBatch {
                handle,
                argument_sets: self.body.argument_sets,
            }),
            robot,
            out,
        )
        .await
        // Closing the invocation's connection releases its prepared template.
    }
}

enum ConsistentReads {
    Parameters(ExecuteBatch),
    Prepared(ExecutePreparedReadBatch),
    Statements(ExecuteReadStatements),
}

async fn execute_consistent_reads(
    cx: &Cx,
    client: &mut Client,
    request: ConsistentReads,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let label = match &request {
        ConsistentReads::Parameters(_) | ConsistentReads::Prepared(_) => "argument set",
        ConsistentReads::Statements(_) => "statement",
    };
    let sink = std::sync::Mutex::new(BatchReadOutput {
        out,
        robot,
        label,
        active: None,
        completed: 0,
        sent: 0,
        failed: None,
    });
    let lock = || {
        sink.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    };
    let result = match request {
        ConsistentReads::Parameters(body) => {
            client
                .execute_read_batch_streaming(
                    cx,
                    &body.statement,
                    body.argument_sets,
                    |index, seq, columns| lock().columns(cx, index, seq, columns),
                    |index, row| lock().row(cx, index, &row),
                    |index, seq, rows| lock().result_end(index, seq, rows),
                )
                .await
        }

        ConsistentReads::Prepared(body) => {
            client
                .execute_prepared_read_batch_streaming(
                    cx,
                    body.handle,
                    body.argument_sets,
                    |index, seq, columns| lock().columns(cx, index, seq, columns),
                    |index, row| lock().row(cx, index, &row),
                    |index, seq, rows| lock().result_end(index, seq, rows),
                )
                .await
        }
        ConsistentReads::Statements(body) => {
            client
                .execute_read_statements_streaming(
                    cx,
                    body.statements,
                    |index, seq, columns| lock().columns(cx, index, seq, columns),
                    |index, row| lock().row(cx, index, &row),
                    |index, seq, rows| lock().result_end(index, seq, rows),
                )
                .await
        }
    };
    sink.into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .finish(result)
}

/// Holds only one result's shape and counters, never its answer bag.
/// The active tuple is (zero-based index, selected sequence, width, flushed rows).
struct BatchReadOutput<'a, W> {
    out: &'a mut W,
    robot: bool,
    label: &'static str,
    active: Option<(u32, u64, usize, u64)>,
    completed: u32,
    sent: u64,
    failed: Option<Failure>,
}

impl<W: Write> BatchReadOutput<'_, W> {
    fn capture(&mut self, result: Result<(), Failure>) -> Result<(), ClientError> {
        if let Err(error) = result {
            self.failed.get_or_insert(error);
            // Retain the original diagnostic while asking the client to cancel
            // and drain, exactly as ordinary remote stream delivery does.
            Err(ClientError::Io(std::io::ErrorKind::BrokenPipe))
        } else {
            Ok(())
        }
    }

    fn columns(
        &mut self,
        cx: &Cx,
        index: u32,
        seq: u64,
        columns: &[String],
    ) -> Result<(), ClientError> {
        let result = (|| {
            cx.checkpoint().map_err(Failure::query)?;
            if self.active.is_some() || index != self.completed {
                return Err(Failure::io("remote batch result order is invalid"));
            }
            let statement = u64::from(index) + 1;
            let line = if self.robot {
                format!(
                    r#"{{"v":1,"event":"columns","statement":{statement},"stream":true,"seq":{seq},"columns":[{}]}}"#,
                    columns
                        .iter()
                        .map(|name| fgdb_protocol::json::quote(name))
                        .collect::<Vec<_>>()
                        .join(",")
                )
            } else {
                format!(
                    "{} {statement} at seq {seq}\n{}",
                    self.label,
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
            self.active = Some((index, seq, columns.len(), 0));
            Ok(())
        })();
        self.capture(result)
    }

    fn row(&mut self, cx: &Cx, index: u32, row: &[WireValue]) -> Result<(), ClientError> {
        let result = (|| {
            cx.checkpoint().map_err(Failure::query)?;
            let Some((active, seq, width, rows)) = self.active else {
                return Err(Failure::io("remote batch row has no columns"));
            };
            if active != index || width != row.len() {
                return Err(Failure::io("remote batch row does not match its result"));
            }
            let rows = rows
                .checked_add(1)
                .ok_or_else(|| Failure::io("batch row counter overflow"))?;
            let sent = self
                .sent
                .checked_add(1)
                .ok_or_else(|| Failure::io("batch delivery counter overflow"))?;
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
                    r#"{{"v":1,"event":"row","statement":{statement},"cells":[{}]}}"#,
                    cells.join(",")
                )
            } else {
                cells.join("\t")
            };
            emit(self.out, &line)?;
            self.out.flush().map_err(Failure::io)?;
            self.active = Some((index, seq, width, rows));
            self.sent = sent;
            Ok(())
        })();
        self.capture(result)
    }

    fn result_end(&mut self, index: u32, seq: u64, rows: u64) -> Result<(), ClientError> {
        let result = (|| {
            if !self
                .active
                .is_some_and(|(at, cut, _, sent)| at == index && cut == seq && sent == rows)
            {
                return Err(Failure::io(
                    "remote batch result count or snapshot is invalid",
                ));
            }
            self.completed = self
                .completed
                .checked_add(1)
                .ok_or_else(|| Failure::io("batch result counter overflow"))?;
            self.active = None;
            Ok(())
        })();
        self.capture(result)
    }

    fn finish(
        self,
        result: Result<fgdb_protocol::body::ReadBatchResultEnd, ClientError>,
    ) -> Result<(), Failure> {
        if let Some(error) = self.failed {
            return Err(super::super::stream::incomplete(error, self.sent));
        }
        let result = (|| {
            let end = result.map_err(failure)?;
            if self.active.is_some() || end.results != self.completed || end.rows != self.sent {
                return Err(Failure::io(
                    "remote batch completion does not match delivered results",
                ));
            }
            let line = if self.robot {
                format!(
                    r#"{{"v":1,"event":"result","kind":"read_batch","statements":{},"count":{}}}"#,
                    end.results, end.rows
                )
            } else {
                format!(
                    "completed {} {}(s), {} row(s)",
                    end.results, self.label, end.rows
                )
            };
            emit(self.out, &line)?;
            self.out.flush().map_err(Failure::io)
        })();
        result.map_err(|error| super::super::stream::incomplete(error, self.sent))
    }
}

/// The array and streaming JSONL adapters share exactly the typed operand
/// grammar. Values remain data; diagnostics never quote their payloads.
pub(super) fn parameters(
    record: Json,
    at: usize,
    checkpoint: &mut impl FnMut() -> Result<(), Failure>,
) -> Result<Vec<(String, WireValue)>, Failure> {
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
    // The shared JSON decoder rejects duplicate keys and owns a BTreeMap:
    // its order is already canonical UTF-8 parameter-name order.
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
            Failure::usage(format!("argument set {at} has an invalid typed parameter"))
        })?;
        parameters.push((name, value));
    }
    Ok(parameters)
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
    fn prepared_read_admission_checks_its_own_count_and_both_wire_requests() {
        for count in [1, MAX_READ_BATCH_STATEMENTS] {
            let input = format!("[{}]", vec!["{}"; count].join(","));
            let batch = PreparedBatch::parse("RETURN 1", &input, || Ok(()))
                .unwrap_or_else(|error| panic!("{}", error.message));
            batch
                .admit_prepared_reads()
                .unwrap_or_else(|error| panic!("{}", error.message));
        }
        // The ordinary file grammar allows more independent reads than one
        // consistent native batch. Refuse this before reading credentials.
        let input = format!("[{}]", vec!["{}"; MAX_READ_BATCH_STATEMENTS + 1].join(","));
        let batch = PreparedBatch::parse("RETURN 1", &input, || Ok(()))
            .unwrap_or_else(|error| panic!("{}", error.message));
        let error = batch.admit_prepared_reads().expect_err("batch count bound");
        assert_eq!(error.code, 2);
        assert!(error.message.contains("1..=32"));

        // Both encodings differ from the ordinary argument-file envelope.
        // Exercise the fixed handle overhead with a short, unexecuted text.
        let mut batch = PreparedBatch {
            body: ExecuteBatch {
                statement: "x".into(),
                argument_sets: vec![vec![("x".into(), WireValue::Text(String::new()))]],
            },
        };
        let overhead = ExecutePreparedReadBatch {
            handle: PreparedHandle([1; 16]),
            argument_sets: batch.body.argument_sets.clone(),
        }
        .encode()
        .unwrap()
        .len();
        let payload = (CLIENT_MAX_FRAME as usize) - MAX_HEADER_LEN - overhead;
        batch.body.argument_sets[0][0].1 = WireValue::Text("x".repeat(payload));
        batch
            .admit_prepared_reads()
            .unwrap_or_else(|error| panic!("{}", error.message));
        let WireValue::Text(value) = &mut batch.body.argument_sets[0][0].1 else {
            unreachable!();
        };
        value.push('x');
        assert!(batch.body.encode().unwrap().len()
            <= (CLIENT_MAX_FRAME as usize) - MAX_HEADER_LEN);
        let error = batch.admit_prepared_reads().expect_err("fixed handle overhead");
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
mod read_statement_tests {
    use super::*;

    fn parse(input: &str) -> Result<ReadStatements, Failure> {
        ReadStatements::parse(input, || Ok(()))
    }

    #[test]
    fn distinct_reads_keep_their_text_and_independent_typed_parameters() {
        let batch = parse(r#"[
            {"statement":"RETURN $n AS count","parameters":{"n":"uint:18446744073709551615"}},
            {"statement":"RETURN $n AS name, $m AS map","parameters":{"n":"text:λ\nAda","m":"json:{\"k\":[true,null]}"}},
            {"statement":"RETURN 1 AS one"}
        ]"#).unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(batch.body.statements.len(), 3);
        assert!(
            batch
                .body
                .statements
                .iter()
                .all(|read| read.mode == ExecuteMode::Read)
        );
        assert_eq!(batch.body.statements[0].statement, "RETURN $n AS count");
        assert_eq!(
            batch.body.statements[0].parameters,
            [("n".into(), WireValue::Count(u64::MAX))]
        );
        assert_eq!(
            batch.body.statements[1].parameters[1],
            ("n".into(), WireValue::Text("λ\nAda".into()))
        );
        assert!(batch.body.statements[2].parameters.is_empty());
        assert_eq!(
            ExecuteReadStatements::decode(&batch.body.encode().unwrap()).unwrap(),
            batch.body
        );
    }

    #[test]
    fn malformed_late_reads_and_unrecognized_fields_refuse_the_complete_file() {
        for input in [
            "",
            "[]",
            "{}",
            "[null]",
            "[{}]",
            "[{}]junk",
            r#"[{"statement":7}]"#,
            r#"[{"statement":"   "}]"#,
            r#"[{"statement":"RETURN 1","statement":"RETURN 2"}]"#,
            r#"[{"statement":"RETURN 1","mode":"write"}]"#,
            r#"[{"statement":"RETURN 1"},{"statement":"RETURN $n","parameters":[]} ]"#,
            r#"[{"statement":"RETURN 1"},{"statement":"RETURN $n","parameters":{"n":1}}]"#,
            r#"[{"statement":"RETURN 1"},{"statement":"RETURN $n","parameters":{"n":"uint:-1"}}]"#,
            r#"[{"statement":"RETURN 1"},{"statement":"RETURN $n","parameters":{"n":"int:1","n":"int:2"}}]"#,
        ] {
            assert_eq!(
                parse(input).err().expect("whole file must refuse").code,
                2,
                "{input}"
            );
        }
    }

    #[test]
    fn mixed_read_count_input_and_cumulative_wire_bounds_are_enforced() {
        let record = r#"{"statement":"RETURN 1"}"#;
        let records = vec![record; MAX_READ_BATCH_STATEMENTS].join(",");
        assert!(parse(&format!("[{records}]")).is_ok());
        assert!(parse(&format!("[{records},{record}]")).is_err());
        let source = format!("[{record}]");
        let exact = format!("{source}{}", " ".repeat(MAX_INPUT_BYTES - source.len()));
        assert!(parse(&exact).is_ok());
        assert!(parse(&(exact + " ")).is_err());

        // Packed coordinates expand from short decimal strings to four-byte
        // binary values. A legal raw file can exceed the whole Ready frame.
        let values = vec!["0"; 16_384].join(",");
        let record =
            format!(r#"{{"statement":"RETURN $p","parameters":{{"p":"vector:{values}"}}}}"#);
        assert!(parse(&format!("[{record}]")).is_ok());
        let oversized = format!("[{}]", vec![record; 17].join(","));
        assert!(oversized.len() < MAX_INPUT_BYTES);
        let error = parse(&oversized)
            .err()
            .expect("whole encoded frame exceeds limit");
        assert_eq!(error.code, 2);
        assert!(error.message.contains("frame limit"));
    }

    #[test]
    fn every_mixed_read_admission_checkpoint_can_cancel_before_submission() {
        let source =
            r#"[{"statement":"RETURN $x","parameters":{"x":"int:1"}},{"statement":"RETURN 2"}]"#;
        let mut total = 0;
        ReadStatements::parse(source, || {
            total += 1;
            Ok(())
        })
        .unwrap_or_else(|error| panic!("{}", error.message));
        for stop in 1..=total {
            let mut seen = 0;
            let error = ReadStatements::parse(source, || {
                seen += 1;
                if seen == stop {
                    Err(Failure::io("cancelled"))
                } else {
                    Ok(())
                }
            })
            .err()
            .expect("cancelled input cannot be sent");
            assert_eq!(error.code, 5);
            assert_eq!(seen, stop);
        }
    }

    #[test]
    fn mixed_read_json_checks_cancellation_while_consuming_a_large_document() {
        let source = format!(
            r#"[{{"statement":"RETURN 1"}},{}invalid]"#,
            " ".repeat(2 * fgdb_protocol::json::JSON_PARSE_RUN_BYTES)
        );
        let mut polls = 0;
        let error = ReadStatements::parse(&source, || {
            polls += 1;
            if polls == 3 {
                Err(Failure::io("cancelled during JSON parsing"))
            } else {
                Ok(())
            }
        })
        .err()
        .expect("interrupted before the malformed suffix");
        assert_eq!(polls, 3);
        assert_eq!(error.code, 5);
        assert_eq!(error.message, "cancelled during JSON parsing");
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
    fn csv_flags_require_an_exclusive_remote_query_or_write_file() {
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
        assert!(
            options("query", &["--rows-file", "a", "--csv-types", "name=text"])
                .unwrap_or_else(|error| panic!("{}", error.message))
                .csv_types
                .is_some()
        );
        assert_eq!(
            options(
                "subscribe",
                &["--rows-file", "a", "--csv-types", "name=text"]
            )
            .err()
            .unwrap()
            .code,
            2
        );
    }
}
