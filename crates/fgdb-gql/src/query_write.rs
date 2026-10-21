//! A frozen relational input feeding one ordinary atomic write-script batch.
//!
//! The source's projected names bind the script's parameters. Both definitions
//! are prepared before execution; no row value is parsed as statement text.
//! The embedded adapter reads the source once, binds ALL rows, then executes
//! the resulting sequential program in the SAME transaction. In particular,
//! repeated MERGE keys see earlier creates without rerunning the source MATCH.

mod execution;
mod values;
pub use execution::{GraphQueryWriteExecution, GraphQueryWriteExecutionError};

use crate::algebra::{GraphValue, GraphValueRow};
use crate::{
    BoundGraphWriteScriptBatch, GqlParameterError, GqlParameterType, GqlParameterValue,
    GqlParameters, GqlScalarParameter, GraphSetColumnType, GraphWriteScriptBatchBindError,
    GraphWriteScriptBatchError, PreparedGraphSet, PreparedGraphWriteScript,
};
use core::convert::Infallible;
use fgdb_types::CanonicalScalar;

/// Coordinates address the script's parameter schema, never user values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphQueryWriteBuildError {
    MissingColumn { parameter: usize },
    NonScalarColumn { parameter: usize, column: usize },
    CollectionColumn {
        parameter: usize,
        column: usize,
        expected: GqlParameterType,
    },
}
impl core::fmt::Display for GraphQueryWriteBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "query-fed write definition: {self:?}")
    }
}
impl core::error::Error for GraphQueryWriteBuildError {}

/// A retained logical entry consumes one work unit and one scratch entry.
/// Zero entries is a checkpoint only. This is not an allocator-byte bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphQueryWriteBindEvent {
    pub row: Option<usize>,
    pub entries: u64,
}

#[derive(Debug)]
pub enum GraphQueryWriteBindError<C = Infallible> {
    TooManyStatements { limit: usize, observed: u128 },
    RowShape { row: usize },
    NonScalar { row: usize, column: usize },
    CollectionValue {
        row: usize,
        column: usize,
        expected: GqlParameterType,
    },
    InvalidValue { row: usize, column: usize },
    Parameter {
        row: usize,
        column: usize,
        source: GqlParameterError,
    },
    ExpandedParametersTooLarge { limit: usize, observed: u128 },
    Script(GraphWriteScriptBatchError),
    Interrupted { row: Option<usize>, source: C },
}
impl<C: core::fmt::Display> core::fmt::Display for GraphQueryWriteBindError<C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooManyStatements { limit, observed } => {
                write!(f, "query-fed write has {observed} statements; limit {limit}")
            }
            Self::RowShape { row } => write!(f, "query-fed write row {row} has the wrong width"),
            Self::NonScalar { row, column } => {
                write!(f, "query-fed write row {row}, column {column} is not scalar")
            }
            Self::CollectionValue { row, column, expected } => {
                write!(f, "query-fed write row {row}, column {column} is not {expected:?} or null")
            }
            Self::InvalidValue { row, column } => {
                write!(f, "query-fed write row {row}, column {column} exceeds value bounds")
            }
            Self::Parameter { row, column, source } => {
                write!(f, "query-fed write row {row}, column {column}: {source}")
            }
            Self::ExpandedParametersTooLarge { limit, observed } => {
                write!(f, "query-fed write arguments use {observed} bytes; limit {limit}")
            }
            Self::Script(source) => source.fmt(f),
            Self::Interrupted { row, source } => {
                write!(f, "query-fed write binding at row {row:?}: {source}")
            }
        }
    }
}
impl<C: core::error::Error + 'static> core::error::Error for GraphQueryWriteBindError<C> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Parameter { source, .. } => Some(source),
            Self::Script(source) => Some(source),
            Self::Interrupted { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// A read query followed by a write script once per source occurrence.
///
/// Each script parameter must name a source projection. Extra source columns
/// are allowed and are not copied into arguments. Scalar, list and map arguments
/// retain their native parameter domains. Concrete incompatible source domains
/// refuse even for empty sources; Stored and Any columns are checked per row.
/// Collection parameters admit canonical null under the native nullable law,
/// never substitute an empty container, and never coerce a scalar or identity.
/// A global argument can be carried by projecting a bound read parameter.
///
/// This is an embedded composition contract, not a new native text grammar.
/// Script variables are local to each invocation. Source rows and operands
/// are frozen; the script's OWN MATCH clauses observe earlier script effects.
/// No row deduplication, per-row transaction or per-row quota reset occurs.
#[derive(Clone)]
pub struct PreparedGraphQueryWrite {
    source: PreparedGraphSet,
    script: PreparedGraphWriteScript,
    columns: Box<[usize]>,
}
impl core::fmt::Debug for PreparedGraphQueryWrite {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphQueryWrite")
            .field("parameters", &self.columns.len())
            .field("statements_per_row", &self.script.statements().len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphQueryWrite {
    pub fn prepare(
        source: PreparedGraphSet,
        script: PreparedGraphWriteScript,
    ) -> Result<Self, GraphQueryWriteBuildError> {
        let mut columns = Vec::with_capacity(script.parameter_schema().len());
        for (parameter, spec) in script.parameter_schema().iter().enumerate() {
            let column = source
                .columns()
                .iter()
                .position(|name| name == &spec.name)
                .ok_or(GraphQueryWriteBuildError::MissingColumn { parameter })?;
            if !values::accepts_column(spec.parameter_type, source.column_types()[column]) {
                return Err(match spec.parameter_type {
                    GqlParameterType::List | GqlParameterType::Map => {
                        GraphQueryWriteBuildError::CollectionColumn {
                            parameter, column, expected: spec.parameter_type,
                        }
                    }
                    _ => GraphQueryWriteBuildError::NonScalarColumn { parameter, column },
                });
            }
            columns.push(column);
        }
        Ok(Self { source, script, columns: columns.into_boxed_slice() })
    }

    #[must_use]
    pub fn source(&self) -> &PreparedGraphSet {
        &self.source
    }

    #[must_use]
    pub fn script(&self) -> &PreparedGraphWriteScript {
        &self.script
    }

    /// Maximum source occurrences allowed by this expanded statement cap.
    /// Use this to bound materialization BEFORE evaluating the source query.
    #[must_use]
    pub fn input_row_limit(&self, max_statements: usize) -> usize {
        max_statements.min(PreparedGraphWriteScript::MAX_BATCH_STATEMENTS)
            / self.script.statements().len()
    }

    /// Bind the complete frozen input. None is a validated empty-source no-op,
    /// not a synthetic statement or a relaxed empty-program invariant.
    pub fn bind_rows(
        &self,
        rows: &[GraphValueRow],
        max_statements: usize,
    ) -> Result<Option<BoundGraphWriteScriptBatch>, GraphQueryWriteBindError> {
        self.bind_rows_controlled(rows, max_statements, |_| Ok::<(), Infallible>(()))
    }

    /// Count admission precedes allocation and row traversal. Callbacks precede
    /// retained argument vectors, maps, payloads and each collection-copy event,
    /// each native binding boundary, and final release. Before copying a cell,
    /// its native encoded size and the complete argument prefix are admitted.
    /// The same 4 MiB transcript ceiling as UNWIND includes names and repeats;
    /// native scalar/list/map bounds remain independently effective.
    ///
    /// The caller must supply rows from source() under one pinned read, not
    /// execute the source again between records. This binder itself does no
    /// storage access, identity allocation or staging. Every failure drops the
    /// complete private prefix and retains no successful batch.
    pub fn bind_rows_controlled<C>(
        &self,
        rows: &[GraphValueRow],
        max_statements: usize,
        mut control: impl FnMut(GraphQueryWriteBindEvent) -> Result<(), C>,
    ) -> Result<Option<BoundGraphWriteScriptBatch>, GraphQueryWriteBindError<C>> {
        let limit = max_statements.min(PreparedGraphWriteScript::MAX_BATCH_STATEMENTS);
        let observed = rows.len() as u128 * self.script.statements().len() as u128;
        if observed > limit as u128 {
            return Err(GraphQueryWriteBindError::TooManyStatements { limit, observed });
        }
        let mut retain = |row, entries| {
            control(GraphQueryWriteBindEvent { row, entries })
                .map_err(|source| GraphQueryWriteBindError::Interrupted { row, source })
        };
        retain(None, rows.len() as u64)?;
        if rows.is_empty() {
            retain(None, 0)?;
            return Ok(None);
        }
        let mut arguments = Vec::with_capacity(rows.len());
        let mut transcript_bytes = 0_u128;
        for (row, input) in rows.iter().enumerate() {
            retain(Some(row), 1)?;
            if input.values().len() != self.source.columns().len() {
                return Err(GraphQueryWriteBindError::RowShape { row });
            }
            let mut values = GqlParameters::new();
            for (&column, spec) in self.columns.iter().zip(self.script.parameter_schema()) {
                let parameter = values::bind_cell(
                    &input.values()[column],
                    spec.parameter_type,
                    &spec.name,
                    row,
                    column,
                    transcript_bytes + values.canonical_byte_len() as u128,
                    &mut |entries| retain(Some(row), entries),
                )?;
                values.insert(spec.name.clone(), parameter)
                    .map_err(|source| GraphQueryWriteBindError::Parameter { row, column, source })?;
            }
            transcript_bytes += values.canonical_byte_len() as u128;
            let limit = crate::parameters::MAX_GQL_PARAMETER_TRANSCRIPT_BYTES;
            if transcript_bytes > limit as u128 {
                return Err(GraphQueryWriteBindError::ExpandedParametersTooLarge {
                    limit, observed: transcript_bytes,
                });
            }
            arguments.push(values);
        }
        let mut expanded_reserved = false;
        let batch = self.script.bind_parameter_sets_controlled(
            &arguments,
            max_statements,
            |row| {
                let entries = if row.is_some() {
                    1
                } else if !expanded_reserved {
                    expanded_reserved = true;
                    observed as u64
                } else {
                    0
                };
                retain(row, entries)
            },
        ).map_err(|error| match error {
            GraphWriteScriptBatchBindError::Binding(error) => GraphQueryWriteBindError::Script(error),
            GraphWriteScriptBatchBindError::Interrupted { source, .. } => source,
        })?;
        retain(None, 0)?;
        Ok(Some(batch))
    }
}

fn scalar_parameter(
    value: &CanonicalScalar,
    expected: GqlParameterType,
) -> Result<GqlParameterValue, GqlParameterError> {
    // Native integer argument slots are distinct from nullable canonical
    // scalar slots. In particular NULL never silently becomes integer zero.
    match (expected, value) {
        (GqlParameterType::Int64, CanonicalScalar::Int(value)) => {
            Ok(GqlParameterValue::Int64(*value))
        }
        (GqlParameterType::UInt64, CanonicalScalar::Int(value)) if *value >= 0 => {
            Ok(GqlParameterValue::UInt64(*value as u64))
        }
        _ => GqlScalarParameter::new(value.clone())
            .map(GqlParameterValue::Scalar)
            .map_err(|_| GqlParameterError::ScalarLiteral),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphSetText};
    use fgdb_delta_types::{PropertyKeyId, RelationId};

    fn source(text: &str) -> PreparedGraphSet {
        PreparedGraphSetText::prepare(text, |_, _| None).unwrap()
            .bind_parameters(&GqlParameters::new()).unwrap()
    }
    fn script(text: &str) -> PreparedGraphWriteScript {
        PreparedGraphWriteScript::prepare(text, RelationId(1), |kind, name| {
            match (kind, name) {
                (GraphSymbolKind::Property, "key") => Some(GraphSymbol::Property(PropertyKeyId(1))),
                _ => None,
            }
        }).unwrap()
    }
    fn rows(source: &PreparedGraphSet) -> Vec<GraphValueRow> {
        source.execute_governed(
            GqlQueryPolicy::new(100, 100, 100_000, 100_000),
            |_, _| Err(crate::GqlQueryError::Source(())),
            || Ok::<(), ()>(()),
        ).unwrap().value
    }

    #[test]
    fn binds_by_name_preserving_duplicate_occurrences_and_record_major_statements() {
        let source = source("UNWIND [2, 1, 2] AS x RETURN 99 AS unused, x AS key ORDER BY key");
        let input = rows(&source);
        let script = script("MERGE (n {key:$key}); CREATE (m {key:$key})");
        let prepared = PreparedGraphQueryWrite::prepare(source, script.clone()).unwrap();
        let batch = prepared.bind_rows(&input, 6).unwrap().unwrap();
        assert_eq!(batch.argument_sets(), 3);
        assert_eq!(batch.program().statements().len(), 6);
        for (row, value) in [1, 2, 2].into_iter().enumerate() {
            let expected = script.bind_parameters(
                &GqlParameters::new().with_int64("key", value).unwrap(),
            ).unwrap();
            assert_eq!(
                &batch.program().statements()[batch.statement_range(row).unwrap()],
                expected.statements(),
            );
        }
    }

    #[test]
    fn missing_or_element_column_refuses_before_any_source_rows_exist() {
        assert!(matches!(
            PreparedGraphQueryWrite::prepare(source("RETURN 1 AS other"), script("MERGE (n {key:$key})")),
            Err(GraphQueryWriteBuildError::MissingColumn { parameter: 0 }),
        ));
        assert!(matches!(
            PreparedGraphQueryWrite::prepare(source("MATCH (n) RETURN n AS key"), script("MERGE (n {key:$key})")),
            Err(GraphQueryWriteBuildError::NonScalarColumn { parameter: 0, column: 0 }),
        ));
    }

    #[test]
    fn empty_input_is_a_controlled_noop_and_bound_counts_precede_callbacks() {
        let prepared = PreparedGraphQueryWrite::prepare(
            source("UNWIND [] AS x RETURN x AS key"), script("MERGE (n {key:$key})"),
        ).unwrap();
        let mut callbacks = 0;
        assert!(prepared.bind_rows_controlled(&[], 0, |_| {
            callbacks += 1;
            Ok::<(), ()>(())
        }).unwrap().is_none());
        assert_eq!(callbacks, 2);
        let input = rows(&source("UNWIND [1,2] AS key RETURN key"));
        callbacks = 0;
        assert!(matches!(prepared.bind_rows_controlled(&input, 1, |_| {
            callbacks += 1;
            Ok::<(), ()>(())
        }), Err(GraphQueryWriteBindError::TooManyStatements { limit: 1, observed: 2 })));
        assert_eq!(callbacks, 0);
    }

    #[test]
    fn late_nonscalar_and_late_binding_failure_return_no_batch() {
        let prepared = PreparedGraphQueryWrite::prepare(
            source("UNWIND [1, [2]] AS key RETURN key"), script("MERGE (n {key:$key})"),
        ).unwrap();
        assert!(matches!(prepared.bind_rows(&rows(prepared.source()), 2),
            Err(GraphQueryWriteBindError::NonScalar { row: 1, column: 0 })));
        let bad = rows(&source("UNWIND [1, 'wrong'] AS key RETURN key"));
        assert!(matches!(prepared.bind_rows(&bad, 2),
            Err(GraphQueryWriteBindError::Script(GraphWriteScriptBatchError::Arguments {
                argument_set: 1, ..
            }))));
        // Failure does not poison a reusable prepared definition.
        let good = rows(&source("RETURN 3 AS key"));
        assert_eq!(prepared.bind_rows(&good, 2).unwrap().unwrap().argument_sets(), 1);
    }

    #[test]
    fn cancellation_at_every_binding_boundary_never_releases_a_prefix() {
        let prepared = PreparedGraphQueryWrite::prepare(
            source("UNWIND [1,2] AS key RETURN key"), script("MERGE (n {key:$key})"),
        ).unwrap();
        let input = rows(prepared.source());
        let mut count = 0;
        prepared.bind_rows_controlled(&input, 2, |_| {
            count += 1;
            Ok::<(), usize>(())
        }).unwrap();
        for stop in 0..count {
            let mut at = 0;
            let result = prepared.bind_rows_controlled(&input, 2, |_| {
                let current = at;
                at += 1;
                if current == stop { Err(stop) } else { Ok(()) }
            });
            assert!(matches!(result,
                Err(GraphQueryWriteBindError::Interrupted { source, .. }) if source == stop));
            assert_eq!(at, stop + 1);
        }
    }
}
