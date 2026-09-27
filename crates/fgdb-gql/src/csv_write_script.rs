//! CSV ingestion binds one ordinary native atomic write program.
//!
//! The string API decodes all arguments before binding. The streaming API
//! instead retains the header and one raw record, binds that record through the
//! SAME CSV value decoder and native script binder, then drops its arguments.
//! Both retain the final bounded program until complete source acceptance;
//! neither executes or commits records independently.
//!
//! CSV limits bound source bytes, records, decoded fields and parameter
//! transcripts. Expanded statements retain the native batch admission ceiling.
//! These are not exact allocator-byte bounds or an unbounded storage bulk loader.

use crate::csv_parameters::{
    CsvParameterError, CsvParameterErrorKind, CsvParameterLimit, CsvParameterLimits,
    decode_csv_parameters,
};
use crate::csv_records::{CsvRecordError, CsvRecordFramer, CsvRecordLimits};
use crate::{
    BoundGraphWriteScriptBatch, GraphWriteScriptBatchError, GraphWriteStatement,
    PreparedGraphWriteProgram, PreparedGraphWriteScript,
};

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

/// A streaming preparation refusal, never a commit or partial-success outcome.
/// CSV and framing coordinates refer to the original source (including a BOM).
/// Binding coordinates retain the original SCRIPT offsets and zero-based
/// argument-set indices. Caller-owned source/control errors retain their type.
#[derive(Debug)]
pub enum GraphWriteScriptCsvStreamError<C> {
    Csv(CsvParameterError),
    Framing(CsvRecordError),
    Binding(GraphWriteScriptBatchError),
    Source(C),
    Interrupted(C),
    InvalidReadCount { capacity: usize, returned: usize },
    InvalidUtf8 { record: usize, offset: usize },
    Allocation { record: usize },
}
impl<C: core::fmt::Display> core::fmt::Display for GraphWriteScriptCsvStreamError<C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Csv(source) => source.fmt(f),
            Self::Framing(source) => source.fmt(f),
            Self::Binding(source) => source.fmt(f),
            Self::Source(source) => write!(f, "CSV source failed: {source}"),
            Self::Interrupted(source) => write!(f, "CSV preparation interrupted: {source}"),
            Self::InvalidReadCount { capacity, returned } => {
                write!(f, "CSV source returned {returned} bytes for a {capacity}-byte buffer")
            }
            Self::InvalidUtf8 { record, offset } => {
                write!(f, "CSV record {record} at byte {offset}: invalid UTF-8")
            }
            Self::Allocation { record } => {
                write!(f, "CSV record {record}: allocation refused")
            }
        }
    }
}
impl<C: core::error::Error + 'static> core::error::Error for GraphWriteScriptCsvStreamError<C> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Csv(source) => Some(source),
            Self::Framing(source) => Some(source),
            Self::Binding(source) => Some(source),
            Self::Source(source) | Self::Interrupted(source) => Some(source),
            _ => None,
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

    /// Read and bind one atomic program without retaining the complete CSV or
    /// a complete vector of argument maps. Returns (program, data-record count).
    ///
    /// `read` follows Read::read's contract: return at most the supplied buffer
    /// length, zero only for EOF, and Err for failure (never a successful EOF).
    /// It owns I/O, retry policy and source authorization. No filesystem or
    /// network access is performed by this adapter. `checkpoint` runs before
    /// every read, before binding each complete record, and at final acceptance.
    /// The private program is dropped on ANY source, control, CSV or bind error.
    ///
    /// Source reads are at most 8 KiB and at most the remaining byte allowance
    /// plus one overflow-probe byte. UTF-8, BOM, CRLF and doubled quotes may span
    /// reads. Header/value semantics use decode_csv_parameters unchanged,
    /// including quote-sensitive nulls. The header is revalidated per record;
    /// this trades some binding work for a single authoritative value decoder.
    ///
    /// The source byte cap includes the optional initial BOM. Data-record and
    /// expanded-statement caps refuse BEFORE buffering an extra record. The
    /// parameter-transcript allowance is cumulative even though arguments are
    /// discarded per record. Only the header/current-record buffer and bounded
    /// final program are retained; this is NOT constant-memory execution or a
    /// spillable storage bulk loader. Execute the result once through the
    /// ordinary governed program API, with its unchanged execution quotas.
    pub fn bind_csv_stream_controlled<C>(
        &self,
        limits: CsvParameterLimits,
        max_statements: usize,
        mut read: impl FnMut(&mut [u8]) -> Result<usize, C>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<(PreparedGraphWriteProgram, usize), GraphWriteScriptCsvStreamError<C>> {
        use GraphWriteScriptCsvStreamError as Error;
        let mut compiler = CsvStreamCompiler::new(self, limits, max_statements)?;
        let mut chunk = [0u8; 8192];
        let mut prefix = [0u8; 3];
        let mut prefix_len = 0;
        let mut started = false;
        let mut input_bytes = 0;
        loop {
            checkpoint().map_err(Error::Interrupted)?;
            // Effective limits are <= 64 MiB, so the one-byte probe cannot
            // overflow, even on 32-bit targets. Never ask for a zero-byte read.
            let capacity = chunk.len().min(compiler.limits.max_input_bytes - input_bytes + 1);
            let read = read(&mut chunk[..capacity]).map_err(Error::Source)?;
            if read > capacity {
                return Err(Error::InvalidReadCount { capacity, returned: read });
            }
            if read == 0 {
                break;
            }
            input_bytes += read;
            if input_bytes > compiler.limits.max_input_bytes {
                return Err(compiler.limit_error(
                    CsvParameterLimit::InputBytes,
                    compiler.limits.max_input_bytes,
                    input_bytes,
                    compiler.framer.position().offset as usize + compiler.bom_bytes,
                ));
            }
            for &byte in &chunk[..read] {
                if started {
                    compiler.push(byte, &mut checkpoint)?;
                    continue;
                }
                prefix[prefix_len] = byte;
                prefix_len += 1;
                if prefix_len < 3 && [0xef, 0xbb, 0xbf].starts_with(&prefix[..prefix_len]) {
                    continue;
                }
                started = true;
                if prefix_len == 3 && prefix == [0xef, 0xbb, 0xbf] {
                    compiler.bom_bytes = 3;
                    // Keep the BOM in the canonical decoder input: otherwise
                    // a second BOM would be stripped again and accepted.
                    compiler.buffer.extend_from_slice(&prefix);
                } else {
                    for &byte in &prefix[..prefix_len] {
                        compiler.push(byte, &mut checkpoint)?;
                    }
                }
            }
        }
        if !started {
            // A partial BOM is data, not a silently discarded prefix. UTF-8
            // validation below rejects its incomplete encoding.
            for &byte in &prefix[..prefix_len] {
                compiler.push(byte, &mut checkpoint)?;
            }
        }
        if compiler.framer.finish().map_err(|error| compiler.framing_error(error))? {
            compiler.complete_record(&mut checkpoint)?;
        }
        if compiler.records == 0 {
            return Err(Error::Csv(CsvParameterError {
                record: if compiler.header_len.is_some() { 1 } else { 0 },
                column: None,
                offset: input_bytes,
                kind: if compiler.header_len.is_some() {
                    CsvParameterErrorKind::EmptyData
                } else {
                    CsvParameterErrorKind::MissingHeader
                },
            }));
        }
        let program = PreparedGraphWriteProgram::prepare_with_statement_limit(
            compiler.statements,
            compiler.max_statements,
        )
        .map_err(|source| Error::Binding(GraphWriteScriptBatchError::Definition(source)))?;
        checkpoint().map_err(Error::Interrupted)?;
        Ok((program, compiler.records))
    }
}

struct CsvStreamCompiler<'a> {
    script: &'a PreparedGraphWriteScript,
    limits: CsvParameterLimits,
    max_statements: usize,
    framer: CsvRecordFramer,
    buffer: Vec<u8>,
    header_len: Option<usize>,
    record_start: usize,
    bom_bytes: usize,
    records: usize,
    parameter_bytes: usize,
    statements: Vec<GraphWriteStatement>,
}
impl<'a> CsvStreamCompiler<'a> {
    fn new<C>(
        script: &'a PreparedGraphWriteScript,
        limits: CsvParameterLimits,
        max_statements: usize,
    ) -> Result<Self, GraphWriteScriptCsvStreamError<C>> {
        let hard = CsvParameterLimits::HARD;
        let max_statements = max_statements.min(PreparedGraphWriteScript::MAX_BATCH_STATEMENTS);
        let max_records = max_statements.checked_div(script.statements().len()).unwrap_or(0);
        let limits = CsvParameterLimits {
            max_input_bytes: limits.max_input_bytes.min(hard.max_input_bytes),
            max_records: limits.max_records.min(hard.max_records).min(max_records),
            max_columns: limits.max_columns.min(hard.max_columns),
            max_field_bytes: limits.max_field_bytes.min(hard.max_field_bytes),
            max_parameter_bytes: limits.max_parameter_bytes.min(hard.max_parameter_bytes),
        };
        // Reuse the native decoder's declaration admission before touching the
        // source. MissingHeader is the only expected result for a valid schema
        // and empty input; no synthetic record or value is used for validation.
        match decode_csv_parameters("", script.parameter_schema(), limits) {
            Err(error) if error.kind == CsvParameterErrorKind::MissingHeader => {}
            Err(error) => return Err(GraphWriteScriptCsvStreamError::Csv(error)),
            Ok(_) => unreachable!("empty CSV cannot contain a header"),
        }
        Ok(Self {
            script,
            limits,
            max_statements,
            framer: CsvRecordFramer::new(CsvRecordLimits {
                max_record_bytes: limits.max_input_bytes,
                max_field_bytes: limits.max_field_bytes,
                max_columns: limits.max_columns,
            }),
            buffer: Vec::new(),
            header_len: None,
            record_start: 0,
            bom_bytes: 0,
            records: 0,
            parameter_bytes: 0,
            statements: Vec::new(),
        })
    }

    fn limit_error<C>(
        &self,
        dimension: CsvParameterLimit,
        limit: usize,
        observed: usize,
        offset: usize,
    ) -> GraphWriteScriptCsvStreamError<C> {
        GraphWriteScriptCsvStreamError::Csv(CsvParameterError {
            record: if self.header_len.is_some() { self.records + 1 } else { 0 },
            column: None,
            offset,
            kind: CsvParameterErrorKind::Limit { dimension, limit, observed },
        })
    }

    fn framing_error<C>(&self, mut error: CsvRecordError) -> GraphWriteScriptCsvStreamError<C> {
        error.position.offset += self.bom_bytes as u64;
        GraphWriteScriptCsvStreamError::Framing(error)
    }

    fn source_offset(&self, offset: usize) -> usize {
        match self.header_len {
            Some(header_len) if offset >= header_len => self.record_start + offset - header_len,
            _ => offset,
        }
    }

    fn csv_error<C>(&self, mut error: CsvParameterError) -> GraphWriteScriptCsvStreamError<C> {
        error.offset = self.source_offset(error.offset);
        if error.record != 0 {
            error.record = self.records + 1;
        }
        if let CsvParameterErrorKind::Limit {
            dimension: CsvParameterLimit::ParameterBytes,
            limit,
            observed,
        } = &mut error.kind
        {
            *limit = self.limits.max_parameter_bytes;
            *observed = observed.saturating_add(self.parameter_bytes);
        }
        GraphWriteScriptCsvStreamError::Csv(error)
    }

    fn push<C>(
        &mut self,
        byte: u8,
        checkpoint: &mut impl FnMut() -> Result<(), C>,
    ) -> Result<(), GraphWriteScriptCsvStreamError<C>> {
        if self.header_len == Some(self.buffer.len()) && self.records >= self.limits.max_records {
            return Err(self.limit_error(
                CsvParameterLimit::Records,
                self.limits.max_records,
                self.records + 1,
                self.record_start,
            ));
        }
        let completed = self.framer.push(byte).map_err(|error| self.framing_error(error))?;
        // The allocation-free framer admits syntax/field size BEFORE raw input
        // retention. Source and record counts were admitted before this point.
        self.buffer.try_reserve(1).map_err(|_| GraphWriteScriptCsvStreamError::Allocation {
            record: if self.header_len.is_some() { self.records + 1 } else { 0 },
        })?;
        self.buffer.push(byte);
        if completed {
            self.complete_record(checkpoint)?;
        }
        Ok(())
    }

    fn complete_record<C>(
        &mut self,
        checkpoint: &mut impl FnMut() -> Result<(), C>,
    ) -> Result<(), GraphWriteScriptCsvStreamError<C>> {
        use GraphWriteScriptCsvStreamError as Error;
        checkpoint().map_err(Error::Interrupted)?;
        let text = core::str::from_utf8(&self.buffer).map_err(|error| Error::InvalidUtf8 {
            record: if self.header_len.is_some() { self.records + 1 } else { 0 },
            offset: self.source_offset(error.valid_up_to()),
        })?;
        let Some(header_len) = self.header_len else {
            match decode_csv_parameters(text, self.script.parameter_schema(), self.limits) {
                Err(error) if error.kind == CsvParameterErrorKind::EmptyData => {}
                Err(error) => return Err(self.csv_error(error)),
                Ok(_) => unreachable!("one framed header cannot contain data records"),
            }
            self.header_len = Some(self.buffer.len());
            self.record_start = self.framer.position().offset as usize + self.bom_bytes;
            return Ok(());
        };
        let row_limits = CsvParameterLimits {
            max_records: 1,
            max_parameter_bytes: self.limits.max_parameter_bytes - self.parameter_bytes,
            ..self.limits
        };
        let mut arguments = decode_csv_parameters(text, self.script.parameter_schema(), row_limits)
            .map_err(|error| self.csv_error(error))?;
        let arguments = arguments.pop().expect("one framed data record after the header");
        self.parameter_bytes += arguments.canonical_byte_len();
        let program = self.script.bind_parameters(&arguments).map_err(|source| {
            Error::Binding(GraphWriteScriptBatchError::Arguments {
                argument_set: self.records,
                source,
            })
        })?;
        self.statements.try_reserve(program.statements().len()).map_err(|_| Error::Allocation {
            record: self.records + 1,
        })?;
        self.statements.extend(program.into_statements().into_vec());
        self.records += 1;
        self.buffer.truncate(header_len);
        self.record_start = self.framer.position().offset as usize + self.bom_bytes;
        Ok(())
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use crate::{GqlParameterType, GqlParameters, GraphSymbol, GraphSymbolKind};
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_types::CanonicalScalarKind;

    fn script(text: bool) -> PreparedGraphWriteScript {
        let kind = if text {
            GqlParameterType::Scalar(CanonicalScalarKind::Text)
        } else {
            GqlParameterType::Int64
        };
        PreparedGraphWriteScript::prepare_with_parameter_types(
            "CREATE (n {p:$value}); CREATE (m {p:$value})",
            RelationId(1),
            &[("value", kind)],
            |kind, name| match (kind, name) {
                (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
                _ => None,
            },
        )
        .unwrap()
    }

    fn bind(
        script: &PreparedGraphWriteScript,
        input: &[u8],
        chunk_size: usize,
        limits: CsvParameterLimits,
        statements: usize,
    ) -> Result<(PreparedGraphWriteProgram, usize), GraphWriteScriptCsvStreamError<&'static str>> {
        let mut at = 0;
        script.bind_csv_stream_controlled(
            limits,
            statements,
            |buffer| {
                let count = buffer.len().min(chunk_size).min(input.len() - at);
                buffer[..count].copy_from_slice(&input[at..at + count]);
                at += count;
                Ok(count)
            },
            || Ok(()),
        )
    }

    #[test]
    fn every_chunk_size_preserves_bom_unicode_quotes_nulls_and_native_program_order() {
        let script = script(true);
        let input = "\u{feff}value\r\n\"snow 雪🙂, \"\"quoted\"\"\r\nline\"\r\n\\N\r\n\"\\N\"\r\n\"\"\r\nlast";
        let limits = CsvParameterLimits::default();
        let expected = script.bind_csv_with_statement_limit(input, limits, 10).unwrap();
        for chunk in 1..=input.len() + 1 {
            let (actual, records) = bind(&script, input.as_bytes(), chunk, limits, 10).unwrap();
            assert_eq!(records, 5, "chunk {chunk}");
            assert_eq!(actual.canonical_bytes(), expected.program().canonical_bytes(), "chunk {chunk}");
        }
    }

    #[test]
    fn ending_terminator_does_not_add_a_record_and_blank_records_remain_data() {
        let script = script(true);
        for input in ["value\nx", "value\nx\n", "value\nx\r\n", "value\n\n"] {
            let expected = script.bind_csv(input, CsvParameterLimits::default()).unwrap();
            let (program, records) = bind(&script, input.as_bytes(), 1, CsvParameterLimits::default(), 2).unwrap();
            assert_eq!(records, 1);
            assert_eq!(program.canonical_bytes(), expected.program().canonical_bytes());
        }
    }

    #[test]
    fn header_only_empty_input_partial_bom_and_double_bom_refuse() {
        let script = script(true);
        for input in [b"".as_slice(), b"value", b"value\n", b"\xef\xbb\xbf", b"\xef", b"\xef\xbb", b"\xef\xbb\xbf\xef\xbb\xbfvalue\nx"] {
            assert!(bind(&script, input, 1, CsvParameterLimits::default(), 64).is_err(), "{input:?}");
        }
    }

    #[test]
    fn record_and_statement_caps_refuse_before_parsing_an_extra_record() {
        let script = script(false);
        for (records, statements) in [(1, 64), (64, 2), (64, 3), (1, usize::MAX)] {
            let error = bind(
                &script,
                b"value\n1\n\"unterminated",
                1,
                CsvParameterLimits { max_records: records, ..CsvParameterLimits::default() },
                statements,
            ).unwrap_err();
            assert!(matches!(error, GraphWriteScriptCsvStreamError::Csv(CsvParameterError {
                record: 2, offset: 8,
                kind: CsvParameterErrorKind::Limit { dimension: CsvParameterLimit::Records, limit: 1, observed: 2 }, ..
            })));
        }
    }

    #[test]
    fn zero_statement_allowance_never_binds_a_record() {
        let error = bind(&script(false), b"value\n1", 1, CsvParameterLimits::default(), 0).unwrap_err();
        assert!(matches!(error, GraphWriteScriptCsvStreamError::Csv(CsvParameterError {
            record: 1, kind: CsvParameterErrorKind::Limit { dimension: CsvParameterLimit::Records, limit: 0, .. }, ..
        })));
    }

    #[test]
    fn source_byte_limit_is_inclusive_and_reads_only_one_overflow_probe() {
        let script = script(false);
        let input = b"value\n1\n";
        let limits = CsvParameterLimits { max_input_bytes: input.len(), ..CsvParameterLimits::default() };
        assert!(bind(&script, input, 8192, limits, 2).is_ok());
        let mut consumed = 0;
        let error = script.bind_csv_stream_controlled(
            limits, 2,
            |buffer| {
                consumed += buffer.len();
                buffer.fill(b'x');
                Ok::<_, &'static str>(buffer.len())
            },
            || Ok(()),
        ).unwrap_err();
        assert_eq!(consumed, input.len() + 1);
        assert!(matches!(error, GraphWriteScriptCsvStreamError::Csv(CsvParameterError {
            kind: CsvParameterErrorKind::Limit { dimension: CsvParameterLimit::InputBytes, .. }, ..
        })));
    }

    #[test]
    fn parameter_budget_is_cumulative_not_refreshed_per_record() {
        let script = script(false);
        let one = GqlParameters::new().with_int64("value", 1).unwrap().canonical_byte_len();
        let limits = CsvParameterLimits { max_parameter_bytes: one * 2, ..CsvParameterLimits::default() };
        assert!(bind(&script, b"value\n1\n2", 1, limits, 6).is_ok());
        let error = bind(&script, b"value\n1\n2\n3", 1, limits, 6).unwrap_err();
        match error {
            GraphWriteScriptCsvStreamError::Csv(CsvParameterError {
                record: 3, offset: 10,
                kind: CsvParameterErrorKind::Limit { dimension: CsvParameterLimit::ParameterBytes, limit, observed }, ..
            }) => {
                assert_eq!(limit, one * 2);
                assert_eq!(observed, one * 3);
            }
            error => panic!("wrong refusal: {error:?}"),
        }
    }

    #[test]
    fn malformed_later_record_keeps_original_source_coordinates_and_redacts_values() {
        let input = "\u{feff}value\n1\n2\nsecret";
        let error = bind(&script(false), input.as_bytes(), 1, CsvParameterLimits::default(), 6).unwrap_err();
        assert!(matches!(&error, GraphWriteScriptCsvStreamError::Csv(CsvParameterError {
            record: 3, offset: 13, kind: CsvParameterErrorKind::InvalidValue, ..
        })));
        assert!(!format!("{error:?} {error}").contains("secret"));
        let error = bind(&script(true), b"value\nok\n\xff", 1, CsvParameterLimits::default(), 4).unwrap_err();
        assert!(matches!(error, GraphWriteScriptCsvStreamError::InvalidUtf8 { record: 2, offset: 9 }));
    }

    #[test]
    fn framing_failures_after_valid_prefix_never_return_a_program() {
        for input in ["value\n1\n\"open", "value\n1\n2\r", "value\n1\n2\"x", "value\n1\n\"2\"x"] {
            assert!(matches!(
                bind(&script(false), input.as_bytes(), 1, CsvParameterLimits::default(), 6),
                Err(GraphWriteScriptCsvStreamError::Framing(_))
            ));
        }
    }

    #[test]
    fn source_error_is_not_eof_and_final_acceptance_can_cancel() {
        let script = script(false);
        let input = b"value\n1\n";
        let mut reads = 0;
        let error = script.bind_csv_stream_controlled(
            CsvParameterLimits::default(), 2,
            |buffer| {
                reads += 1;
                if reads == 1 {
                    buffer[..input.len()].copy_from_slice(input);
                    Ok(input.len())
                } else { Err("source lost") }
            },
            || Ok(()),
        ).unwrap_err();
        assert!(matches!(error, GraphWriteScriptCsvStreamError::Source("source lost")));

        // Determine all control boundaries from a successful run, then refuse
        // each independently, including after EOF and program construction.
        let mut boundaries = 0;
        let mut at = 0;
        script.bind_csv_stream_controlled(
            CsvParameterLimits::default(), 2,
            |buffer| {
                let count = input.len() - at;
                buffer[..count].copy_from_slice(&input[at..]);
                at = input.len();
                Ok::<_, usize>(count)
            },
            || { boundaries += 1; Ok(()) },
        ).unwrap();
        for cutoff in 1..=boundaries {
            let mut reached = 0;
            let mut at = 0;
            let result = script.bind_csv_stream_controlled(
                CsvParameterLimits::default(), 2,
                |buffer| {
                    let count = input.len() - at;
                    buffer[..count].copy_from_slice(&input[at..]);
                    at = input.len();
                    Ok(count)
                },
                || { reached += 1; if reached == cutoff { Err(cutoff) } else { Ok(()) } },
            );
            assert!(matches!(result, Err(GraphWriteScriptCsvStreamError::Interrupted(actual)) if actual == cutoff));
            assert_eq!(reached, cutoff);
        }
    }

    #[test]
    fn invalid_read_contract_and_schema_refuse_without_panics() {
        let script = script(false);
        let result = script.bind_csv_stream_controlled(
            CsvParameterLimits::default(), 2,
            |buffer| Ok::<_, &'static str>(buffer.len() + 1),
            || Ok(()),
        );
        assert!(matches!(result, Err(GraphWriteScriptCsvStreamError::InvalidReadCount { .. })));
        let parameterless = PreparedGraphWriteScript::prepare("CREATE (n)", RelationId(1), |_, _| None).unwrap();
        let result = parameterless.bind_csv_stream_controlled(
            CsvParameterLimits::default(), 2,
            |_| -> Result<usize, &'static str> { panic!("invalid schema must refuse before reading") },
            || Ok(()),
        );
        assert!(matches!(result, Err(GraphWriteScriptCsvStreamError::Csv(CsvParameterError { kind: CsvParameterErrorKind::EmptySchema, .. }))));
    }

    #[test]
    fn compiler_retains_header_and_one_record_not_the_source_or_argument_batch() {
        let script = script(false);
        let mut compiler = CsvStreamCompiler::new::<()>(
            &script,
            CsvParameterLimits::default(),
            200,
        ).unwrap();
        let mut checkpoint = || Ok(());
        for &byte in b"value\n" { compiler.push(byte, &mut checkpoint).unwrap(); }
        let header_len = compiler.header_len.unwrap();
        for record in 0..100 {
            for byte in format!("{record}\n").bytes() { compiler.push(byte, &mut checkpoint).unwrap(); }
            assert_eq!(compiler.buffer.len(), header_len);
            assert_eq!(compiler.records, record + 1);
            assert_eq!(compiler.statements.len(), (record + 1) * 2);
        }
    }
}
