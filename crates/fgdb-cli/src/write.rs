//! Local-owner writes use the native compiler and ONE engine-owned transaction.
//!
//! Preparation owns all input admission and both possible success envelopes.
//! Execution must use the ordinary governed program API. Neither this module
//! nor a CSV record allocates graph identities, stages storage or retries work.

use crate::{Command, Error, Format, MAX_INPUT_BYTES, MAX_OUTPUT_BYTES, Options, Symbols};
use fgdb::{WriteError, WriteTxnError};
use fgdb_gql::csv_parameters::CsvParameterLimits;
use fgdb_gql::{
    GqlParameterType, GqlParameterValue, GqlParameters, GqlQueryPolicy,
    GraphMutationProgramError, GraphWriteProgramError, GraphWriteProgramPolicy,
    PreparedGraphWriteProgram, PreparedGraphWriteScript,
};
use fgdb_types::{CanonicalScalarKind, EmbeddedTxnCompletion};

/// An admitted complete program, not a transaction or a durability receipt.
/// Counts describe input records/statements, never changed graph rows.
pub struct PreparedCliWrite {
    program: PreparedGraphWriteProgram,
    policy: GraphWriteProgramPolicy,
    committed: String,
    read_closed: String,
}

impl core::fmt::Debug for PreparedCliWrite {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedCliWrite")
            .field("statements", &self.program.statements().len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

impl PreparedCliWrite {
    /// Bind every input before the caller opens a database or allocates IDs.
    /// Statements and values use the native compiler; no keyword fallback,
    /// interpolation, inferred catalog or per-record commit is introduced.
    pub fn prepare(
        options: &Options,
        statement: &str,
        symbols: &Symbols,
        parameters: &GqlParameters,
        csv: Option<&str>,
        types: Option<&str>,
    ) -> Result<Self, Error> {
        let write = options.write.as_ref().ok_or(Error::Usage)?;
        if statement.len() > MAX_INPUT_BYTES {
            return Err(Error::Input);
        }
        let ceiling = if options.command == Command::ImportCsv {
            PreparedGraphWriteScript::MAX_BATCH_STATEMENTS
        } else {
            fgdb_gql::MAX_GRAPH_MUTATION_STATEMENTS
        };
        if write.max_statements == 0 || write.max_statements > ceiling
            || write.max_input_bytes == 0
            || write.max_input_bytes > CsvParameterLimits::HARD.max_input_bytes
            || write.max_changes == 0 || write.max_changes > 1_000_000
            || options.max_rows == 0 || options.max_rows > 1_000_000
            || options.max_work == 0 || options.max_work > 100_000_000
        {
            return Err(Error::Usage);
        }
        let (program, records, operation) = match options.command {
            Command::Write => {
                if csv.is_some() || types.is_some() {
                    return Err(Error::Usage);
                }
                let declarations = parameters.parameter_types().collect::<Vec<_>>();
                let script = PreparedGraphWriteScript::prepare_with_parameter_types(
                    statement, write.relation, &declarations, |kind, name| symbols.resolve(kind, name),
                ).map_err(|_| Error::Input)?;
                if script.statements().len() > write.max_statements {
                    return Err(Error::Input);
                }
                (script.bind_parameters(parameters).map_err(|_| Error::Input)?, 1, "write")
            }
            Command::ImportCsv => {
                if !parameters.is_empty() {
                    return Err(Error::Usage);
                }
                let declarations = parse_types(types.unwrap_or(""))?;
                let script = PreparedGraphWriteScript::prepare_with_parameter_types(
                    statement, write.relation, &declarations, |kind, name| symbols.resolve(kind, name),
                ).map_err(|_| Error::Input)?;
                let batch = script.bind_csv_with_statement_limit(
                    csv.ok_or(Error::Usage)?,
                    CsvParameterLimits {
                        max_input_bytes: write.max_input_bytes,
                        max_records: write.max_statements,
                        ..CsvParameterLimits::default()
                    },
                    write.max_statements,
                ).map_err(|_| Error::Input)?;
                let records = batch.argument_sets();
                (batch.into_program(), records, "import-csv")
            }
            _ => return Err(Error::Usage),
        };
        // A no-effect script can finish as ReadClosed. Admit BOTH complete
        // responses before any durable work; choosing one later cannot fail.
        let statements = program.statements().len();
        let committed = response(operation, records, statements, "write_committed",
            options.format, options.max_output_bytes)?;
        let read_closed = response(operation, records, statements, "read_closed",
            options.format, options.max_output_bytes)?;
        let policy = GraphWriteProgramPolicy::new(
            GqlQueryPolicy::new(options.max_work, options.max_rows,
                options.max_work, options.max_work),
            write.max_changes, write.max_changes, write.max_changes,
        );
        Ok(Self { program, policy, committed, read_closed })
    }

    pub fn program(&self) -> &PreparedGraphWriteProgram {
        &self.program
    }

    pub fn policy(&self) -> GraphWriteProgramPolicy {
        self.policy
    }

    /// Call only with the actual successful completion of this program.
    /// No allocation, formatting, I/O or cancellation point after commit.
    pub fn into_response(self, completion: EmbeddedTxnCompletion) -> String {
        if completion.commit_seq().is_some() { self.committed } else { self.read_closed }
    }
}

fn parse_types(text: &str) -> Result<Vec<(&str, GqlParameterType)>, Error> {
    if text.len() > MAX_INPUT_BYTES { return Err(Error::Input); }
    let mut declarations = Vec::new();
    let mut names = GqlParameters::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let (kind, name) = line.split_once('\t').ok_or(Error::Input)?;
        let kind = match kind {
            "int64" => GqlParameterType::Int64,
            "uint64" => GqlParameterType::UInt64,
            "int" => GqlParameterType::Scalar(CanonicalScalarKind::Int),
            "text" => GqlParameterType::Scalar(CanonicalScalarKind::Text),
            "bool" => GqlParameterType::Scalar(CanonicalScalarKind::Bool),
            "null" => GqlParameterType::Scalar(CanonicalScalarKind::Null),
            _ => return Err(Error::Input),
        };
        // Reuse native count/name/duplicate validation, not another grammar.
        if name.len() > 64 { return Err(Error::Input); }
        names.insert(name, GqlParameterValue::Int64(0)).map_err(|_| Error::Input)?;
        declarations.push((name, kind));
    }
    Ok(declarations)
}

fn response(operation: &str, records: usize, statements: usize, completion: &str,
    format: Format, limit: usize) -> Result<String, Error>
{
    // Only fixed internal strings and bounded counts reach this formatter.
    let text = match format {
        Format::Ndjson => format!(
            "{{\"version\":1,\"type\":\"complete\",\"operation\":\"{operation}\",\"records\":{records},\"statements\":{statements},\"completion\":\"{completion}\"}}\n"
        ),
        Format::Human => format!("{operation}: {completion}; {records} input record(s), {statements} statement(s)\n"),
    };
    if limit == 0 || limit > MAX_OUTPUT_BYTES || text.len() > limit {
        return Err(Error::OutputLimit);
    }
    Ok(text)
}

/// Finish errors keep the engine's durability classification. A broken reply
/// must never be translated into a retry-safe rollback. No source text, keys,
/// parameters or protected engine diagnostics escape the local CLI boundary.
pub fn classify_execution(
    source: &GraphWriteProgramError<WriteTxnError, WriteTxnError, Box<asupersync::error::Error>>,
) -> Error {
    match source {
        GraphWriteProgramError::Program(GraphMutationProgramError::Preflight(
            WriteTxnError::Write(WriteError::CommitOutcomeUnknown { .. }
                | WriteError::HandleCommitOutcomeUnknown { .. }),
        )) => Error::CommitOutcomeUnknown,
        GraphWriteProgramError::Program(GraphMutationProgramError::Preflight(
            WriteTxnError::Write(WriteError::CommittedNeedsRecovery { .. }),
        )) => Error::CommittedNeedsRecovery,
        _ => Error::Write,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(command: &str) -> Options {
        let csv = if command == "import-csv" { " --csv-file data" } else { "" };
        let args = format!("{command} --db db --keys-file keys --query-file script --relation 1 --format ndjson{csv}");
        let crate::Invocation::Run(options) = crate::parse_args(args.split_whitespace().map(str::to_owned)).unwrap()
            else { panic!("run") };
        options
    }

    fn symbols() -> Symbols {
        Symbols::parse("label\tPerson\t1\nproperty\tp\t1\nproperty\tname\t2\n").unwrap()
    }

    #[test]
    fn csv_and_explicit_native_programs_have_identical_bytes() {
        let options = options("import-csv");
        let symbols = symbols();
        let statement = "CREATE (n:Person {p:$key,name:$name})";
        let csv = "name,key\n\"'; MATCH (n) DELETE n; --\",1\n\\N,2\n\"\",3\n";
        let prepared = PreparedCliWrite::prepare(&options, statement, &symbols,
            &GqlParameters::new(), Some(csv), Some("text\tname")).unwrap();
        let script = PreparedGraphWriteScript::prepare_with_parameter_types(statement,
            fgdb_delta_types::RelationId(1),
            &[("name", GqlParameterType::Scalar(CanonicalScalarKind::Text))],
            |kind, name| symbols.resolve(kind, name),
        ).unwrap();
        let arguments = [
            GqlParameters::new().with_int64("key", 1).unwrap().with_text("name", "'; MATCH (n) DELETE n; --").unwrap(),
            GqlParameters::new().with_int64("key", 2).unwrap().with_null("name").unwrap(),
            GqlParameters::new().with_int64("key", 3).unwrap().with_text("name", "").unwrap(),
        ];
        let expected = script.bind_parameter_sets(&arguments).unwrap();
        assert_eq!(prepared.program().canonical_bytes(), expected.program().canonical_bytes());
        assert!(prepared.committed.contains("\"records\":3,\"statements\":3"));
        assert!(!format!("{prepared:?}").contains("Person"));
    }

    #[test]
    fn write_types_come_from_typed_arguments_not_text_interpolation() {
        let options = options("write");
        let symbols = symbols();
        let arguments = GqlParameters::new().with_text("name", "secret; \"λ\"").unwrap();
        let prepared = PreparedCliWrite::prepare(&options, "CREATE (n:Person {name:$name})",
            &symbols, &arguments, None, None).unwrap();
        assert_eq!(prepared.program().statements().len(), 1);
        assert!(!prepared.committed.contains("secret"));
        assert!(prepared.read_closed.contains("read_closed"));
    }

    #[test]
    fn complete_success_envelopes_are_admitted_before_any_execution() {
        for format in [Format::Human, Format::Ndjson] {
            let mut options = options("write");
            options.format = format;
            let symbols = symbols();
            let args = GqlParameters::new();
            let admitted = PreparedCliWrite::prepare(&options, "CREATE (n)", &symbols, &args, None, None).unwrap();
            options.max_output_bytes = admitted.committed.len().max(admitted.read_closed.len());
            assert!(PreparedCliWrite::prepare(&options, "CREATE (n)", &symbols, &args, None, None).is_ok());
            options.max_output_bytes -= 1;
            assert_eq!(PreparedCliWrite::prepare(&options, "CREATE (n)", &symbols, &args, None, None)
                .unwrap_err(), Error::OutputLimit);
        }
    }

    #[test]
    fn malformed_schema_or_late_records_never_produce_a_program() {
        let options = options("import-csv");
        let symbols = symbols();
        for types in ["text", "float\tname", "text\tname\textra", "text\tbad name",
            "text\tname\ntext\tname", "text\tunused"] {
            assert!(PreparedCliWrite::prepare(&options, "CREATE (n {name:$name})", &symbols,
                &GqlParameters::new(), Some("name\nx"), Some(types)).is_err());
        }
        for csv in ["key\n1\n2\nsecret", "key\n1\n\"unfinished", "key\n"] {
            assert!(PreparedCliWrite::prepare(&options, "CREATE (n {p:$key})", &symbols,
                &GqlParameters::new(), Some(csv), None).is_err());
        }
    }

    #[test]
    fn expanded_statement_admission_is_independent_of_shared_execution_policy() {
        let mut options = options("import-csv");
        let write = options.write.as_mut().unwrap();
        write.max_statements = 2;
        write.max_changes = 3;
        let statement = "CREATE (n {p:$key});MATCH (n) SET n.p=$key";
        let symbols = symbols();
        let empty = GqlParameters::new();
        let prepared = PreparedCliWrite::prepare(&options, statement, &symbols, &empty, Some("key\n1"), None).unwrap();
        assert_eq!(prepared.policy().max_created_vertices, 3);
        assert_eq!(prepared.policy().max_created_edges, 3);
        assert_eq!(prepared.policy().mutations.max_effects, 3);
        assert!(prepared.committed.contains("\"records\":1,\"statements\":2"));
        assert!(PreparedCliWrite::prepare(&options, statement, &symbols, &empty, Some("key\n1\n2"), None).is_err());
    }

    #[test]
    fn uncertain_and_committed_finish_failures_are_not_reported_as_rollbacks() {
        let wrap = |error| GraphWriteProgramError::Program(
            GraphMutationProgramError::Preflight(WriteTxnError::Write(error)));
        let unknown = wrap(WriteError::HandleCommitOutcomeUnknown { published_frontier: fgdb_types::CommitSeq(7) });
        assert_eq!(classify_execution(&unknown), Error::CommitOutcomeUnknown);
        let stage = fgdb::DerivedPublicationStage::FoldCommittedTemplate;
        let committed = wrap(WriteError::CommittedNeedsRecovery {
            recovery: fgdb::RecoveryRequired {
                durable_frontier: fgdb_types::CommitSeq(8),
                published_frontier: fgdb_types::CommitSeq(7),
                failed_stage: stage,
            },
            source: Box::new(fgdb::RebuildError::InjectedPublicationFailure(stage)),
        });
        assert_eq!(classify_execution(&committed), Error::CommittedNeedsRecovery);
        assert_eq!(classify_execution(&wrap(WriteError::EmptyBatch)), Error::Write);
        assert_eq!(Error::WriteOutput.code(), "write_completed_output_failed");
    }
}
