//! CSV ingestion binds one ordinary native atomic write program.
//!
//! This does not execute or commit records independently. CSV decoding completes
//! before any script binding. The native batch binder then lowers all records
//! into the existing program with its shared execution budgets and rollback
//! boundary. Give the result to the ordinary WriteTxn or database autocommit
//! program API; successful binding is not a durability receipt.
//!
//! CSV limits bound source bytes, records, decoded fields and retained parameter
//! transcripts. Expanded statements retain the native batch admission ceiling.
//! These are not exact allocator-byte bounds or an unbounded storage bulk loader.

use crate::csv_parameters::{CsvParameterError, CsvParameterLimits, decode_csv_parameters};
use crate::{BoundGraphWriteScriptBatch, GraphWriteScriptBatchError, PreparedGraphWriteScript};

/// CSV decoding failed before native binding, or the native batch binder refused
/// the decoded arguments. Neither arm indicates that execution has started.
/// CSV coordinates use record zero for the header. The native Binding source
/// retains zero-based argument-set indices and original SCRIPT byte offsets;
/// those must not be mistaken for CSV byte offsets.
#[derive(Debug)]
pub enum GraphWriteScriptCsvError {
    Csv(CsvParameterError),
    Binding(GraphWriteScriptBatchError),
}

impl core::fmt::Display for GraphWriteScriptCsvError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Csv(source) => core::fmt::Display::fmt(source, f),
            Self::Binding(source) => core::fmt::Display::fmt(source, f),
        }
    }
}

impl core::error::Error for GraphWriteScriptCsvError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Csv(source) => Some(source),
            Self::Binding(source) => Some(source),
        }
    }
}

impl PreparedGraphWriteScript {
    /// Decode an exact-name CSV header and typed records into one native batch.
    ///
    /// The prepared script's parameter schema determines column types; no type
    /// inference, catalog resolution or query-text interpolation occurs. The
    /// default cap remains 64 EXPANDED statements across all records, rather
    /// than 64 records per statement. All CSV and native parameter limits apply.
    /// Empty input, header-only input and parameterless scripts refuse.
    ///
    /// ```
    /// use fgdb_delta_types::{PropertyKeyId, RelationId};
    /// use fgdb_gql::{GraphSymbol, GraphSymbolKind, PreparedGraphWriteScript};
    /// use fgdb_gql::csv_parameters::CsvParameterLimits;
    ///
    /// let script = PreparedGraphWriteScript::prepare(
    ///     "CREATE (n {p:$key})",
    ///     RelationId(1),
    ///     |kind, name| match (kind, name) {
    ///         (GraphSymbolKind::Property, "p") => {
    ///             Some(GraphSymbol::Property(PropertyKeyId(1)))
    ///         }
    ///         _ => None,
    ///     },
    /// )?;
    /// let batch = script.bind_csv("key\n1\n2", CsvParameterLimits::default())?;
    /// assert_eq!(batch.argument_sets(), 2);
    /// assert_eq!(batch.program().statements().len(), 2);
    /// // Execute batch.program() through the ordinary governed program API.
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn bind_csv(
        &self,
        input: &str,
        limits: CsvParameterLimits,
    ) -> Result<BoundGraphWriteScriptBatch, GraphWriteScriptCsvError> {
        self.bind_csv_with_statement_limit(input, limits, crate::MAX_GRAPH_MUTATION_STATEMENTS)
    }

    /// Explicitly admit a larger finite CSV batch without splitting its commit.
    ///
    /// The effective record cap is the smaller of the CSV record allowance and
    /// floor(min(max_statements, MAX_BATCH_STATEMENTS) / script statements).
    /// This cap is installed BEFORE decoding any data records, so a record
    /// beyond the expanded-program allowance refuses before its fields are
    /// decoded or its typed values allocated. Such a refusal is a CSV Records
    /// limit error. Raising this allowance never raises execution quotas.
    ///
    /// The result is the same native batch as bind_parameter_sets_with_limit:
    /// record-major statement order, one shared execution allowance, one final
    /// acceptance checkpoint, and no successful prefix on a binding failure.
    /// Larger inputs require explicit caller partitioning and its own partial-
    /// commit policy; this adapter never silently chunks an atomic import.
    pub fn bind_csv_with_statement_limit(
        &self,
        input: &str,
        mut limits: CsvParameterLimits,
        max_statements: usize,
    ) -> Result<BoundGraphWriteScriptBatch, GraphWriteScriptCsvError> {
        let max_statements = max_statements.min(Self::MAX_BATCH_STATEMENTS);
        // Prepared scripts are nonempty. A defensive zero divisor still
        // refuses admission instead of panicking or treating it as unlimited.
        let max_records = max_statements
            .checked_div(self.statements().len())
            .unwrap_or(0);
        limits.max_records = limits.max_records.min(max_records);
        let arguments = decode_csv_parameters(input, self.parameter_schema(), limits)
            .map_err(GraphWriteScriptCsvError::Csv)?;
        self.bind_parameter_sets_with_limit(&arguments, max_statements)
            .map_err(GraphWriteScriptCsvError::Binding)
    }
}
