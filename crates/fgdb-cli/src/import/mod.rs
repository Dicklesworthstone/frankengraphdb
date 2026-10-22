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
//!
//! A chained write that the script grammar refuses (`MERGE ... MERGE ...`)
//! runs instead as one chain over every record ([`record_chain_text`]). That
//! path reads the complete CSV and its row list into memory before binding.

use super::{Failure, Options, emit, execution_failure};
use asupersync::fs::Vfs;
use fgdb::Database;
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::csv_parameters::{
    CsvParameterError, CsvParameterErrorKind, CsvParameterLimits, decode_csv_parameters,
};
use fgdb_gql::csv_write_script::GraphWriteScriptCsvStreamError;
use fgdb_gql::insertion::GraphInsertPolicy;
use fgdb_gql::{
    GqlParameterSpec, GqlParameterType, GqlParameterValue, GqlParameters, GqlQueryPolicy,
    GraphWriteProgramPolicy, PreparedGraphWriteChain, PreparedGraphWriteChainText,
    PreparedGraphWriteProgram, PreparedGraphWriteScript,
};
use fgdb_types::{
    CanonicalScalar, CanonicalScalarKind, EmbeddedTxnCompletion, EmbeddedTxnState, PurposeContexts,
    QueryCx,
};
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
    body: Body,
    records: usize,
    changes: u64,
    /// One execution allowance for the entire bound CSV, never per record.
    allowance: GqlQueryPolicy,
}

/// What the CSV became: a write script bound once per record, or one chain
/// over every record (see [`record_chain_text`]).
enum Body {
    Script(PreparedGraphWriteProgram),
    Chain(Box<PreparedGraphWriteChain>),
}

/// The list parameter and row variable a chain import reads its records from.
const ROWS: &str = "__csv_rows";
const ROW: &str = "__csv_row";

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
    let changes = csv_options.max_changes.unwrap_or(DEFAULT_CHANGES);
    let script = match PreparedGraphWriteScript::prepare_with_parameter_types(
        &statement,
        options.coordinate,
        &declarations,
        |kind, name| options.resolve(kind, name),
    ) {
        Ok(script) => script,
        Err(error) => {
            // Only a statement the script grammar refuses runs as a chain, so
            // every script import keeps its per-record semantics.
            let Some((template, names)) = prepare_chain(options, &statement, &declarations)? else {
                return Err(Failure::query(error));
            };
            let input = read_bounded(cx, input, limit, "--input")?;
            let (chain, records) = bind_chain(&template, &names, &declarations, &input, limit)?;
            return Ok(PreparedImport {
                body: Body::Chain(Box::new(chain)),
                records,
                changes,
                allowance: options.budget.policy(),
            });
        }
    };
    let mut source = open_bounded(input, limit, "--input")?;
    let (program, records) = bind_source(&script, &mut source, limit, || {
        cx.checkpoint().map_err(Failure::io)
    })?;
    Ok(PreparedImport {
        body: Body::Script(program),
        records,
        changes,
        allowance: options.budget.policy(),
    })
}

/// A chained write (`MERGE (a:N {id: $s}) MERGE (b:N {id: $d})
/// MERGE (a)-[:E]->(b)`) is not a write script, so it cannot bind once per
/// record. It runs instead as ONE chain over all records:
/// `UNWIND $__csv_rows AS __csv_row` ahead of it, and every `$name` read as
/// `__csv_row.name`. A chain applies each clause to every record before the
/// next, as openCypher's LOAD CSV does. A chain of creating MERGEs stages its
/// creations once (fgdb-0s2pq), where a per-record script re-reads its staged
/// prefix for every record (fgdb-irqu2).
///
/// Parameters are found by skipping string literals, backtick names and
/// comments. None when the text already names the row variable, or has a
/// `$` that does not start a parameter name.
fn record_chain_text(statement: &str) -> Option<(String, Vec<String>)> {
    if statement.contains(ROW) {
        return None;
    }
    let bytes = statement.as_bytes();
    let mut text = format!("UNWIND ${ROWS} AS {ROW} ");
    let mut names: Vec<String> = Vec::new();
    // Every boundary below is an ASCII byte, so each slice is whole UTF-8.
    let (mut at, mut copied) = (0, 0);
    while at < bytes.len() {
        match bytes[at] {
            quote @ (b'\'' | b'"' | b'`') => {
                at += 1;
                while at < bytes.len() && bytes[at] != quote {
                    at += if bytes[at] == b'\\' && quote != b'`' {
                        2
                    } else {
                        1
                    };
                }
                at = (at + 1).min(bytes.len());
            }
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at += 2;
                while at < bytes.len() && !(bytes[at] == b'*' && bytes.get(at + 1) == Some(&b'/')) {
                    at += 1;
                }
                at = (at + 2).min(bytes.len());
            }
            b'$' => {
                let start = at + 1;
                let mut end = start;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                {
                    end += 1;
                }
                if end == start || bytes[start].is_ascii_digit() {
                    return None;
                }
                let name = &statement[start..end];
                text.push_str(&statement[copied..at]);
                text.push_str(ROW);
                text.push('.');
                text.push_str(name);
                if !names.iter().any(|known| known == name) {
                    names.push(name.to_owned());
                }
                (at, copied) = (end, end);
            }
            _ => at += 1,
        }
    }
    text.push_str(&statement[copied..]);
    Some((text, names))
}

/// Prepare a chained statement over the CSV records before the CSV is read.
/// None when the text is not a chain (the script refusal then stands).
fn prepare_chain(
    options: &Options,
    statement: &str,
    declarations: &[(&str, GqlParameterType)],
) -> Result<Option<(PreparedGraphWriteChainText, Vec<String>)>, Failure> {
    if !PreparedGraphWriteChainText::is_chain(statement).unwrap_or(false) {
        return Ok(None);
    }
    let Some((text, names)) = record_chain_text(statement) else {
        return Ok(None);
    };
    if let Some((name, _)) = declarations
        .iter()
        .find(|(name, _)| !names.iter().any(|used| used == name))
    {
        return Err(Failure::query(format!(
            "--types-file declares {name}, which the statement does not read"
        )));
    }
    let template = PreparedGraphWriteChainText::prepare_with_parameter_types(
        &text,
        options.coordinate,
        &[(ROWS, GqlParameterType::List)],
        options,
    )
    .map_err(|error| {
        Failure::query(format!(
            "as one chain over the CSV records, `{text}`: {error}"
        ))
    })?;
    Ok(Some((template, names)))
}

/// Decode every record with the statement's parameter types (declared, or
/// int64 as a script's stored-property operand is) and bind them as the
/// chain's row list. The CSV framing, value decoder and limits are the
/// script import's own.
fn bind_chain(
    template: &PreparedGraphWriteChainText,
    names: &[String],
    declarations: &[(&str, GqlParameterType)],
    input: &str,
    limit: u64,
) -> Result<(PreparedGraphWriteChain, usize), Failure> {
    let schema: Vec<GqlParameterSpec> = names
        .iter()
        .map(|name| GqlParameterSpec {
            name: name.clone(),
            parameter_type: declarations
                .iter()
                .find(|(declared, _)| declared == name)
                .map_or(GqlParameterType::Int64, |(_, kind)| *kind),
            requires_positive: false,
            occurrences: 1,
        })
        .collect();
    let limits = CsvParameterLimits {
        max_input_bytes: usize::try_from(limit)
            .map_err(|_| Failure::usage("--max-input-bytes exceeds this platform"))?,
        max_records: CsvParameterLimits::HARD.max_records,
        ..CsvParameterLimits::default()
    };
    let records = decode_csv_parameters(input, &schema, limits).map_err(csv_failure)?;
    let mut rows = Vec::with_capacity(records.len());
    for record in &records {
        let mut fields = Vec::with_capacity(names.len());
        for name in names {
            let value = match record.get(name) {
                Some(GqlParameterValue::Int64(value)) => {
                    GraphValue::Scalar(CanonicalScalar::Int(value))
                }
                Some(GqlParameterValue::UInt64(value)) => GraphValue::Scalar(CanonicalScalar::Int(
                    i64::try_from(value).map_err(|_| {
                        Failure::query(format!("CSV column {name} exceeds int64 in a chain import"))
                    })?,
                )),
                Some(GqlParameterValue::Scalar(value)) => GraphValue::Scalar(value.value().clone()),
                Some(GqlParameterValue::List(value)) => value.value().clone(),
                Some(GqlParameterValue::Map(value)) => value.value().clone(),
                None => GraphValue::Scalar(CanonicalScalar::Null),
            };
            fields.push((name.as_str().into(), value));
        }
        // Names are distinct, so the map refuses only a value it cannot hold.
        rows.push(
            GraphValue::map(fields)
                .ok_or_else(|| Failure::query("a CSV record does not form a row map"))?,
        );
    }
    let arguments = GqlParameters::new()
        .with_list(ROWS, rows)
        .map_err(Failure::query)?;
    let chain = template
        .bind_parameters(&arguments)
        .map_err(Failure::query)?;
    Ok((chain, records.len()))
}

fn csv_failure(source: CsvParameterError) -> Failure {
    if matches!(source.kind, CsvParameterErrorKind::InvalidValue) {
        Failure::query(format!(
            "{source} (the field does not decode as its parameter's type; an undeclared \
             parameter is int64, so declare text or bool columns with --types-file, e.g. \
             text<TAB>name)"
        ))
    } else {
        Failure::query(source)
    }
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
        // Types come from the statement, never from a value: an undeclared
        // stored-property parameter is int64. Say so instead of only a byte
        // offset, since the usual cause is a text column (fgdb-vty1w).
        E::Csv(source) if matches!(source.kind, CsvParameterErrorKind::InvalidValue) => {
            Failure::query(format!(
                "{source} (the field does not decode as its parameter's type; an undeclared \
                 property parameter is int64, so declare text or bool columns with \
                 --types-file, e.g. text<TAB>name)"
            ))
        }
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
    let (statements, completion) = match &prepared.body {
        Body::Script(program) => {
            let (_, completion) = db
                .execute_graph_write_program_returning_autocommit_engine_governed(
                    &contexts.txn(),
                    &contexts.query(),
                    &contexts.commit(),
                    program,
                    GraphWriteProgramPolicy::new(prepared.allowance, changes, changes, changes),
                )
                .await
                .map_err(execution_failure)?;
            (program.statements().len(), completion)
        }
        Body::Chain(chain) => (
            1,
            run_chain(db, contexts, chain, prepared.allowance, changes).await?,
        ),
    };
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

/// One transaction for the whole chain; a failure before commit publishes
/// nothing, and a commit-outcome error keeps its native meaning.
async fn run_chain<V: Vfs + Clone>(
    db: &mut Database<V>,
    contexts: &PurposeContexts,
    chain: &PreparedGraphWriteChain,
    allowance: GqlQueryPolicy,
    changes: u64,
) -> Result<EmbeddedTxnCompletion, Failure> {
    let mut transaction = db.begin(&contexts.txn()).map_err(execution_failure)?;
    if let Err(error) = transaction.execute_graph_write_chain_engine_governed(
        db,
        &contexts.query(),
        chain,
        GraphInsertPolicy::new(allowance, changes, changes),
    ) {
        transaction.abort();
        return Err(execution_failure(error));
    }
    match transaction.finish(db, &contexts.commit()).await {
        Ok(completion) => Ok(completion),
        Err(error) => Err(match transaction.state() {
            EmbeddedTxnState::CommitOutcomeUnknown { .. } => Failure::io(format!(
                "transaction outcome unknown; reopen and resolve before retrying: {error}"
            )),
            EmbeddedTxnState::CommittedNeedsRecovery { commit_seq } => Failure::io(format!(
                "transaction committed at seq {}; recovery required: {error}",
                commit_seq.0
            )),
            _ => execution_failure(error),
        }),
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

/// `kind<TAB>name` lines. Undeclared parameters keep the statement's own
/// type (a stored property value is int64); values never choose a type.
/// Float, exact scale-18 decimal and hexadecimal bytes delegate to the native
/// CSV decoder, including its quote-sensitive NULL and payload admission rules.
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
            "float" => GqlParameterType::Scalar(CanonicalScalarKind::Float),
            "decimal" => GqlParameterType::Scalar(CanonicalScalarKind::Decimal),
            "bytes" => GqlParameterType::Scalar(CanonicalScalarKind::Bytes),
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
