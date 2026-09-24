//! One CSV file becomes ONE atomic native write program.
//!
//! Every input — the statement, the optional type declarations and the CSV
//! records — is read under a byte bound and bound by the native compiler
//! before the database is opened. The engine allocates identities and
//! executes the whole batch under one shared execution allowance; a binding
//! or execution failure in any record commits nothing. This module never
//! chunks, retries per record, or infers a catalog.

use super::{Failure, Options, emit, execution_failure, policy};
use asupersync::fs::Vfs;
use fgdb::Database;
use fgdb_gql::csv_parameters::CsvParameterLimits;
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
    let csv = read_bounded(cx, input, limit, "--input")?;
    let script = PreparedGraphWriteScript::prepare_with_parameter_types(
        &statement,
        options.coordinate,
        &declarations,
        |kind, name| options.resolve(kind, name),
    )
    .map_err(Failure::query)?;
    let batch = script
        .bind_csv_with_statement_limit(
            &csv,
            CsvParameterLimits {
                max_input_bytes: usize::try_from(limit)
                    .map_err(|_| Failure::usage("--max-input-bytes exceeds this platform"))?,
                max_records: CsvParameterLimits::HARD.max_records,
                ..CsvParameterLimits::default()
            },
            PreparedGraphWriteScript::MAX_BATCH_STATEMENTS,
        )
        .map_err(Failure::query)?;
    let records = batch.argument_sets();
    Ok(PreparedImport {
        program: batch.into_program(),
        records,
        changes: csv_options.max_changes.unwrap_or(DEFAULT_CHANGES),
    })
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

/// Read a whole UTF-8 input, refusing once it exceeds `limit` bytes. The
/// refusal names the flag and the bound, never any input bytes.
fn read_bounded(cx: &QueryCx, path: &Path, limit: u64, flag: &str) -> Result<String, Failure> {
    let refused = || Failure::query(format!("{flag} exceeds {limit} bytes"));
    let mut bytes = Vec::new();
    let mut chunk = vec![0u8; CHUNK];
    let mut source: Box<dyn Read> = if is_stdin(path) {
        Box::new(std::io::stdin().lock())
    } else {
        let file = std::fs::File::open(path)
            .map_err(|_| Failure::io(format!("cannot read {flag} file")))?;
        if file.metadata().is_ok_and(|meta| meta.len() > limit) {
            return Err(refused());
        }
        Box::new(file)
    };
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
