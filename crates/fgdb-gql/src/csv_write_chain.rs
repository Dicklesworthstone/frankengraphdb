//! Streamed CSV preparation for one ordinary atomic write chain.
//!
//! The compiler replaces its validated leading row-source UNWIND with a typed
//! owned relation. Every CSV record is admitted independently as a map; the
//! relation is not one scalar list and does not borrow its 65,536-node ceiling.
//! Source bytes, records, fields and cumulative parameter transcripts remain
//! bounded by the unchanged CSV limits. Preparation retains one bounded row
//! relation, the header and the current record. Execution is resident, not a
//! constant-memory or spillable bulk loader, and uses the ordinary chain host.

use crate::algebra::{GraphValue, GraphValueRow};
use crate::csv_parameters::CsvParameterLimits;
use crate::csv_write_script::{GraphWriteScriptCsvStreamError, stream_csv_parameters_controlled};
use crate::{
    GqlMapParameter, GqlParameterError, GqlParameterSpec, GqlParameterValue,
    GraphWriteChainTextError, PreparedGraphWriteChain, PreparedGraphWriteChainText,
};
use fgdb_types::CanonicalScalar;

/// A refusal while preparing the row-source definition or one admitted CSV
/// row. Record numbers are one-based data-record positions, after header zero.
#[derive(Debug)]
pub enum GraphWriteChainCsvBindingError {
    Definition(GraphWriteChainTextError),
    /// A declared uint64 does not fit the graph value language's signed integer.
    /// `column` is the zero-based schema position, independent of header order.
    RowValue {
        record: usize,
        column: usize,
    },
    RowMap {
        record: usize,
        source: GqlParameterError,
    },
}

impl core::fmt::Display for GraphWriteChainCsvBindingError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Definition(source) => source.fmt(f),
            Self::RowValue { record, column } => {
                write!(
                    f,
                    "CSV record {record}, schema column {column}: value exceeds int64 in a chain import"
                )
            }
            Self::RowMap { record, source } => write!(f, "CSV record {record}: {source}"),
        }
    }
}

impl core::error::Error for GraphWriteChainCsvBindingError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Definition(source) => Some(source),
            Self::RowMap { source, .. } => Some(source),
            Self::RowValue { .. } => None,
        }
    }
}

/// Shared framing/source errors with a chain-specific binding refusal.
pub type GraphWriteChainCsvStreamError<C> =
    GraphWriteScriptCsvStreamError<C, GraphWriteChainCsvBindingError>;

impl PreparedGraphWriteChainText {
    /// Bind `UNWIND $row_parameter AS row ...` to one streamed CSV relation.
    /// The parameter must be the statement's sole parameter and occur only in
    /// that first UNWIND; CSV fields are accessed through `row.field` later.
    /// All already-supported MATCH/WITH/UNWIND prefixes and chained writes keep
    /// their native lowering. Each clause runs over all input occurrences
    /// before the next clause, never as a separate statement for each record.
    ///
    /// The schema owns CSV field names/types. A complete source is accepted
    /// before returning a bound chain; a late syntax, source, budget or control
    /// failure returns no executable prefix. The source/checkpoint contract is
    /// the same as `PreparedGraphWriteScript::bind_csv_stream_controlled`.
    /// Input rows are copied only after the executing host's preflight, under
    /// its cumulative work/scratch allowance. This binds no graph authority,
    /// snapshot, transaction, identifier allocation or final commit.
    pub fn bind_csv_rows_stream_controlled<C>(
        &self,
        row_parameter: &str,
        schema: &[GqlParameterSpec],
        limits: CsvParameterLimits,
        read: impl FnMut(&mut [u8]) -> Result<usize, C>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<(PreparedGraphWriteChain, usize), GraphWriteChainCsvStreamError<C>> {
        use GraphWriteChainCsvBindingError as Binding;
        use GraphWriteScriptCsvStreamError as Error;
        // Definition refusal must precede even the first source read (stdin
        // included), and no input value participates in planning or typing.
        let mut chain = self
            .bind_csv_row_source(row_parameter)
            .map_err(|source| Error::Binding(Binding::Definition(source)))?;
        let mut rows = Vec::new();
        let records = stream_csv_parameters_controlled(
            schema,
            limits,
            read,
            &mut checkpoint,
            |arguments, record| {
                let mut fields = Vec::new();
                fields
                    .try_reserve(schema.len())
                    .map_err(|_| Error::Allocation { record })?;
                for (column, spec) in schema.iter().enumerate() {
                    let value = match arguments.get(&spec.name) {
                        Some(GqlParameterValue::Int64(value)) => {
                            GraphValue::Scalar(CanonicalScalar::Int(value))
                        }
                        Some(GqlParameterValue::UInt64(value)) => {
                            GraphValue::Scalar(CanonicalScalar::Int(i64::try_from(value).map_err(
                                |_| Error::Binding(Binding::RowValue { record, column }),
                            )?))
                        }
                        Some(GqlParameterValue::Scalar(value)) => {
                            GraphValue::Scalar(value.value().clone())
                        }
                        Some(GqlParameterValue::List(value)) => value.value().clone(),
                        Some(GqlParameterValue::Map(value)) => value.value().clone(),
                        None => unreachable!("the CSV decoder validated every schema field"),
                    };
                    fields.push((spec.name.as_str().into(), value));
                }
                // Retain the exact individual map/value admission contract.
                // Only the surrounding relation is no longer a scalar list.
                let map = GqlMapParameter::new(fields)
                    .map_err(|source| Error::Binding(Binding::RowMap { record, source }))?;
                rows.try_reserve(1)
                    .map_err(|_| Error::Allocation { record })?;
                rows.push(GraphValueRow::from_owned_values(vec![map.value().clone()]));
                Ok(())
            },
        )?;
        chain.initial_rows = Some(rows.into());
        checkpoint().map_err(Error::Interrupted)?;
        Ok((chain, records))
    }
}

#[cfg(test)]
mod tests;
