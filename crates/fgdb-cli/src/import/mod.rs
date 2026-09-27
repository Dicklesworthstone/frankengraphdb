//! One CSV file becomes ONE atomic native write program.
//!
//! Statement/type files are bounded text; CSV source bytes are streamed through
//! the native framer, value decoder and script binder. Only the header/current
//! record and the bounded final program are retained, not the complete CSV and
//! argument batch. Everything is bound before the database is opened.
//!
//! The engine allocates identities and executes the whole batch under one
//! shared execution allowance. A source, binding or execution failure before
//! commit publishes nothing. This module never commits/retries individual
//! records or infers a catalog. Commit-outcome errors retain their native form.

use super::{Failure, Options, emit, execution_failure, policy};
use asupersync::fs::Vfs;
use fgdb::Database;
use fgdb_gql::csv_parameters::CsvParameterLimits;
use fgdb_gql::csv_write_script::GraphWriteScriptCsvStreamError;
use fgdb_gql::{
    GqlParameterType, GqlParameterValue, GqlParameters, GraphWriteProgramPolicy,
    PreparedGraphWriteProgram, PreparedGraphWriteScript,
};
use fgdb_types::{CanonicalScalarKind, EmbeddedTxnCompletion, PurposeContexts, QueryCx};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Default CSV source bound; the engine's hard ceiling still applies.
const DEFAULT_INPUT_BYTES: u64 = 16 * 1024 * 1024;
/// Statement and type-declaration files are small by construction.
const MAX_TEXT_BYTES: u64 = 64 * 1024;
/// Default whole-program allowance for effects, new vertices and new edges,
/// matching the `write` command's allowance.
const DEFAULT_CHANGES: u64 = 100_000;
const MAX_CHANGES: u64 = 1_000_000;
const CHUNK: usize = 64 * 1024;

#[derive(Default)]
pub(super) struct CsvOptions {
    types_file: Option<PathBuf>,
    query_file: Option<PathBuf>,
    max_input_bytes: Option<u64>,
    max_changes: Option<u64>,
}

impl CsvOptions {
    /// Flags owned by `import-csv` (`--input` is shared with `load`).
    pub(super) fn set(&mut self, flag: &str, raw: &str) -> Result<(), Failure> {
        match flag {
            "--types-file" | "--query-file" => {
                let target = if flag == "--types-file" {
                    &mut self.types_file
                } else {
                    &mut self.query_file
                };
                if target.replace(PathBuf::from(raw)).is_some() {
                    return Err(Failure::usage(format!("{flag} must be supplied only once")));
                }
            }
            "--max-input-bytes" | "--max-changes" => {
                let (target, ceiling) = if flag == "--max-input-bytes" {
                    (
                        &mut self.max_input_bytes,
                        CsvParameterLimits::HARD.max_input_bytes as u64,
                    )
                } else {
                    (&mut self.max_changes, MAX_CHANGES)
                };
                if target.is_some() {
                    return Err(Failure::usage(format!("{flag} must be supplied only once")));
                }
                if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(Failure::usage(format!("{flag} requires a decimal integer")));
                }
                let value: u64 = raw
                    .parse()
                    .map_err(|_| Failure::usage(format!("{flag} is out of range")))?;
                if value == 0 || value > ceiling {
                    return Err(Failure::usage(format!(
                        "{flag} must be between 1 and {ceiling}"
                    )));
                }
                *target = Some(value);
            }
            _ => return Err(Failure::usage("unknown import-csv flag")),
        }
        Ok(())
    }

    pub(super) fn query_file(&self) -> Option<&Path> {
        self.query_file.as_deref()
    }
}

/// A fully bound program, admitted before any storage is opened.
pub(super) struct PreparedImport {
    program: PreparedGraphWriteProgram,
    records: usize,
    changes: u64,
}

/// Read every input and bind every CSV record. Nothing here touches the
/// database; a refusal leaves it byte-identical.
pub(super) fn prepare(options: &Options, cx: &QueryCx) -> Result<PreparedImport, Failure> {
    let input = options
        .input
        .as_deref()
        .ok_or_else(|| Failure::usage("import-csv requires --input"))?;
    let csv_options = &options.csv;
    let stdin_users = [Some(input), csv_options.query_file.as_deref()]
        .into_iter()
        .flatten()
        .filter(|path| is_stdin(path))
        .count();
    if stdin_users > 1 {
        return Err(Failure::usage("only one import-csv input may read stdin"));
    }
    if csv_options.types_file.as_deref().is_some_and(is_stdin) {
        return Err(Failure::usage("--types-file cannot read stdin"));
    }
    let statement = match csv_options.query_file.as_deref() {
        Some(path) => read_bounded(cx, path, MAX_TEXT_BYTES, "--query-file")?,
        None => options.text.clone(),
    };
    let types = csv_options
        .types_file
        .as_deref()
        .map(|path| read_bounded(cx, path, MAX_TEXT_BYTES, "--types-file"))
        .transpose()?;
    let declarations = parse_types(types.as_deref().unwrap_or(""))?;
    let limit = csv_options.max_input_bytes.unwrap_or(DEFAULT_INPUT_BYTES);
    cx.checkpoint().map_err(Failure::io)?;
    // Reject the statement before opening/consuming its CSV source. This is
    // especially important for stdin and potentially blocking pipe sources.
    let script = PreparedGraphWriteScript::prepare_with_parameter_types(
        &statement,
        options.coordinate,
        &declarations,
        |kind, name| options.resolve(kind, name),
    )
    .map_err(Failure::query)?;
    let mut source = open_bounded(input, limit, "--input")?;
    let (program, records) = bind_source(&script, &mut source, limit, || {
        cx.checkpoint().map_err(Failure::io)
    })?;
    Ok(PreparedImport {
        program,
        records,
        changes: csv_options.max_changes.unwrap_or(DEFAULT_CHANGES),
    })
}

/// The native adapter owns CSV framing, cumulative source/parameter admission,
/// per-record binding and final acceptance. This wrapper owns only input I/O
/// and CLI error classification; it never executes a bound prefix.
fn bind_source(
    script: &PreparedGraphWriteScript,
    source: &mut impl Read,
    limit: u64,
    checkpoint: impl FnMut() -> Result<(), Failure>,
) -> Result<(PreparedGraphWriteProgram, usize), Failure> {
    script
        .bind_csv_stream_controlled(
            CsvParameterLimits {
                max_input_bytes: usize::try_from(limit)
                    .map_err(|_| Failure::usage("--max-input-bytes exceeds this platform"))?,
                max_records: CsvParameterLimits::HARD.max_records,
                ..CsvParameterLimits::default()
            },
            PreparedGraphWriteScript::MAX_BATCH_STATEMENTS,
            |buffer| {
                source
                    .read(buffer)
                    .map_err(|_| Failure::io("cannot read --input"))
            },
            checkpoint,
        )
        .map_err(stream_failure)
}

fn stream_failure(error: GraphWriteScriptCsvStreamError<Failure>) -> Failure {
    use GraphWriteScriptCsvStreamError as E;
    match error {
        // Preserve the caller's exit code and redacted diagnostics, rather
        // than relabeling a source failure/cancellation as malformed CSV.
        E::Source(source) | E::Interrupted(source) => source,
        E::Csv(source) => Failure::query(source),
        E::Framing(source) => Failure::query(source),
        E::Binding(source) => Failure::query(source),
        E::InvalidReadCount { .. } => Failure::io("invalid --input read count"),
        E::InvalidUtf8 { record, offset } => Failure::query(format!(
            "--input is not UTF-8 at CSV record {record}, byte {offset}"
        )),
        E::Allocation { record } => Failure::query(format!(
            "CSV preparation allocation refused at record {record}"
        )),
    }
}

/// Execute the bound batch as one autocommit program and report it.
pub(super) async fn run<V: Vfs + Clone>(
    db: &mut Database<V>,
    contexts: &PurposeContexts,
    prepared: PreparedImport,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let changes = prepared.changes;
    let statements = prepared.program.statements().len();
    let (_, completion) = db
        .execute_graph_write_program_returning_autocommit_engine_governed(
            &contexts.txn(),
            &contexts.query(),
            &contexts.commit(),
            &prepared.program,
            GraphWriteProgramPolicy::new(policy(), changes, changes, changes),
        )
        .await
        .map_err(execution_failure)?;
    let seq = match completion {
        EmbeddedTxnCompletion::WriteCommitted { commit_seq } => commit_seq.0,
        EmbeddedTxnCompletion::ReadClosed { snapshot_seq, .. } => snapshot_seq.0,
    };
    let records = prepared.records;
    if robot {
        emit(
            out,
            &format!(
                r#"{{"v":1,"event":"result","kind":"imported_csv","seq":{seq},"records":{records},"statements":{statements}}}"#
            ),
        )
    } else {
        writeln!(
            out,
            "imported {records} record(s), {statements} statement(s) at seq {seq}"
        )
        .map_err(Failure::io)
    }
}

fn is_stdin(path: &Path) -> bool {
    path.as_os_str() == "-"
}

/// Metadata is only an early refusal, never authority for source length. The
/// actual readers enforce their byte caps too, including for stdin and growing
/// files. Diagnostics name flags, not source paths or OS error payloads.
fn open_bounded(path: &Path, limit: u64, flag: &str) -> Result<Box<dyn Read>, Failure> {
    if is_stdin(path) {
        Ok(Box::new(std::io::stdin().lock()))
    } else {
        let file = std::fs::File::open(path)
            .map_err(|_| Failure::io(format!("cannot read {flag} file")))?;
        if file.metadata().is_ok_and(|meta| meta.len() > limit) {
            return Err(Failure::query(format!("{flag} exceeds {limit} bytes")));
        }
        Ok(Box::new(file))
    }
}

/// Read a whole UTF-8 statement/type input, refusing once it exceeds `limit`.
/// CSV data uses bind_source instead and is never collected into this buffer.
fn read_bounded(cx: &QueryCx, path: &Path, limit: u64, flag: &str) -> Result<String, Failure> {
    let refused = || Failure::query(format!("{flag} exceeds {limit} bytes"));
    let mut bytes = Vec::new();
    let mut chunk = vec![0u8; CHUNK];
    let mut source = open_bounded(path, limit, flag)?;
    loop {
        cx.checkpoint().map_err(Failure::io)?;
        let read = source
            .read(&mut chunk)
            .map_err(|_| Failure::io(format!("cannot read {flag}")))?;
        if read == 0 {
            break;
        }
        if (bytes.len() + read) as u64 > limit {
            return Err(refused());
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    String::from_utf8(bytes).map_err(|_| Failure::query(format!("{flag} is not UTF-8")))
}

/// `kind<TAB>name` lines. Undeclared parameters keep native inference.
fn parse_types(text: &str) -> Result<Vec<(&str, GqlParameterType)>, Failure> {
    let mut declarations = Vec::new();
    // Reuse the native parameter-name, count and duplicate validation.
    let mut names = GqlParameters::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let (kind, name) = line
            .split_once('\t')
            .ok_or_else(|| Failure::usage("type lines are kind<TAB>name"))?;
        let kind = match kind {
            "int64" => GqlParameterType::Int64,
            "uint64" => GqlParameterType::UInt64,
            "int" => GqlParameterType::Scalar(CanonicalScalarKind::Int),
            "text" => GqlParameterType::Scalar(CanonicalScalarKind::Text),
            "bool" => GqlParameterType::Scalar(CanonicalScalarKind::Bool),
            "null" => GqlParameterType::Scalar(CanonicalScalarKind::Null),
            _ => return Err(Failure::usage("unknown CSV parameter type")),
        };
        names
            .insert(name, GqlParameterValue::Int64(0))
            .map_err(Failure::usage)?;
        declarations.push((name, kind));
    }
    Ok(declarations)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_gql::{GraphSymbol, GraphSymbolKind};
    use std::io::{self, Cursor};

    fn script() -> PreparedGraphWriteScript {
        PreparedGraphWriteScript::prepare("CREATE (n {p:$value})", RelationId(1), |kind, name| {
            match (kind, name) {
                (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
                _ => None,
            }
        })
        .unwrap()
    }

    #[test]
    fn source_adapter_preserves_native_program_and_record_count() {
        let script = script();
        let input = "value\n1\n2\n3";
        let expected = script
            .bind_csv(input, CsvParameterLimits::default())
            .unwrap();
        let (program, records) = bind_source(
            &script,
            &mut Cursor::new(input.as_bytes()),
            DEFAULT_INPUT_BYTES,
            || Ok(()),
        )
        .map_err(|error| error.message)
        .unwrap();
        assert_eq!(records, 3);
        assert_eq!(
            program.canonical_bytes(),
            expected.program().canonical_bytes()
        );
    }

    struct BrokenSource {
        prefix: Cursor<&'static [u8]>,
    }
    impl Read for BrokenSource {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let count = self.prefix.read(buffer)?;
            if count == 0 {
                Err(io::Error::other("private source path and payload"))
            } else {
                Ok(count)
            }
        }
    }

    #[test]
    fn source_failure_after_valid_records_is_io_not_success_or_query_error() {
        let mut source = BrokenSource {
            prefix: Cursor::new(b"value\n1\n"),
        };
        let error = bind_source(&script(), &mut source, DEFAULT_INPUT_BYTES, || Ok(()))
            .err()
            .unwrap();
        assert_eq!(error.code, 5);
        assert_eq!(error.message, "cannot read --input");
        assert!(!error.message.contains("private"));
    }

    #[test]
    fn malformed_late_record_and_invalid_utf8_keep_query_exit_class() {
        for input in [b"value\n1\nsecret".as_slice(), b"value\n1\n\xff"] {
            let error = bind_source(
                &script(),
                &mut Cursor::new(input),
                DEFAULT_INPUT_BYTES,
                || Ok(()),
            )
            .err()
            .unwrap();
            assert_eq!(error.code, 3);
            assert!(!error.message.contains("secret"));
        }
    }

    #[test]
    fn source_size_is_checked_without_trusting_file_metadata() {
        let input = b"value\n1\n";
        assert!(
            bind_source(
                &script(),
                &mut Cursor::new(input),
                input.len() as u64,
                || Ok(())
            )
            .is_ok()
        );
        let error = bind_source(
            &script(),
            &mut Cursor::new(input),
            input.len() as u64 - 1,
            || Ok(()),
        )
        .err()
        .unwrap();
        assert_eq!(error.code, 3);
    }

    #[test]
    fn cancellation_is_polled_between_records_and_keeps_original_failure() {
        let mut boundaries = 0;
        let result = bind_source(
            &script(),
            &mut Cursor::new(b"value\n1\n2\n3\n"),
            DEFAULT_INPUT_BYTES,
            || {
                boundaries += 1;
                if boundaries == 4 {
                    Err(Failure::io("cancelled"))
                } else {
                    Ok(())
                }
            },
        );
        let error = result.err().unwrap();
        assert_eq!(boundaries, 4);
        assert_eq!(error.code, 5);
        assert_eq!(error.message, "cancelled");
    }

    #[test]
    fn initial_cancellation_does_not_consume_the_input_source() {
        let mut input = Cursor::new(b"value\n1\n");
        let result = bind_source(&script(), &mut input, DEFAULT_INPUT_BYTES, || {
            Err(Failure::io("cancelled"))
        });
        assert!(result.is_err());
        assert_eq!(input.position(), 0);
    }
}
