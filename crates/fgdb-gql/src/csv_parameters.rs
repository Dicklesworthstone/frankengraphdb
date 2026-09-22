//! Bounded CSV records become typed arguments, never interpolated query text.
//!
//! This is preparation work, not an execution or storage lane. The caller must
//! authorize and charge the resulting ordinary program through its existing
//! executor. No files, catalog, identities, transactions or network are touched.
//!
//! The first record is an exact-name header; its order may differ from the
//! declared schema. Commas, double quotes, doubled quote escapes, LF and CRLF
//! record endings, quoted newlines and an optional initial UTF-8 BOM are accepted.
//! Whitespace is data. Blank records are not silently skipped. A final record
//! terminator does not invent another record. Bare CR record endings refuse.
//!
//! Supported declarations are Int64, UInt64 and canonical Null/Bool/Int/Text.
//! Other kinds refuse explicitly rather than guessing a representation. Only
//! unquoted `\N` means canonical null; quoted `"\N"` remains text. Empty text
//! and null are distinct. Numeric legacy declarations never accept null.
//!
//! Count, input, decoded-field and aggregate canonical-parameter bounds all
//! apply. These are admission bounds, not an exact allocator-byte accounting
//! promise. Any failure drops the entire private prefix, never returning a
//! partially decoded batch. Errors contain coordinates, not names or values.

use crate::{GqlParameterSpec, GqlParameterType, GqlParameterValue, GqlParameters};
use fgdb_types::{CanonicalScalar, CanonicalScalarKind};
use std::borrow::Cow;
use std::collections::BTreeMap;

/// Admission limits. Values above the hard ceilings are clamped, not unlimited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CsvParameterLimits {
    /// Encoded UTF-8 input, including header, separators and quoting.
    pub max_input_bytes: usize,
    /// Data records only; the mandatory header is not a data record.
    pub max_records: usize,
    /// Columns in both the schema and every input record.
    pub max_columns: usize,
    /// Decoded UTF-8 bytes per field, including header fields.
    pub max_field_bytes: usize,
    /// Sum of complete canonical parameter transcripts for all data records.
    pub max_parameter_bytes: usize,
}

impl CsvParameterLimits {
    /// Absolute count and byte admission ceilings for one decoding operation.
    pub const HARD: Self = Self {
        max_input_bytes: 64 * 1024 * 1024,
        max_records: 65_536,
        max_columns: crate::parameters::MAX_GQL_PARAMETER_COUNT,
        max_field_bytes: 65_536,
        max_parameter_bytes: 64 * 1024 * 1024,
    };

    fn effective(self) -> Self {
        Self {
            max_input_bytes: self.max_input_bytes.min(Self::HARD.max_input_bytes),
            max_records: self.max_records.min(Self::HARD.max_records),
            max_columns: self.max_columns.min(Self::HARD.max_columns),
            max_field_bytes: self.max_field_bytes.min(Self::HARD.max_field_bytes),
            max_parameter_bytes: self.max_parameter_bytes.min(Self::HARD.max_parameter_bytes),
        }
    }
}

impl Default for CsvParameterLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 16 * 1024 * 1024,
            max_records: 4_096,
            max_columns: Self::HARD.max_columns,
            max_field_bytes: 16_384,
            max_parameter_bytes: 32 * 1024 * 1024,
        }
    }
}

/// The independently enforced dimension that refused admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CsvParameterLimit {
    InputBytes,
    Records,
    Columns,
    FieldBytes,
    ParameterBytes,
}

/// Redacted CSV syntax, schema, value or admission refusal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CsvParameterErrorKind {
    Limit { dimension: CsvParameterLimit, limit: usize, observed: usize },
    MissingHeader,
    EmptyData,
    EmptySchema,
    InvalidSchema,
    UnsupportedType,
    UnknownHeader,
    DuplicateHeader,
    Width { expected: usize, observed: usize },
    UnterminatedQuote,
    UnexpectedQuote,
    TrailingCharacters,
    InvalidLineEnding,
    InvalidValue,
    NullNotAllowed,
    ParameterRejected,
}

/// Zero-based logical record/column and absolute byte offset in the input.
/// Record zero is the header, even when it spans multiple physical lines.
/// Schema-wide and input-wide failures have no input column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CsvParameterError {
    pub record: usize,
    pub column: Option<usize>,
    pub offset: usize,
    pub kind: CsvParameterErrorKind,
}

impl core::fmt::Display for CsvParameterError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "CSV record {}", self.record)?;
        if let Some(column) = self.column {
            write!(f, ", column {column}")?;
        }
        write!(f, " at byte {}: {:?}", self.offset, self.kind)
    }
}
impl core::error::Error for CsvParameterError {}

fn error(record: usize, column: Option<usize>, offset: usize, kind: CsvParameterErrorKind)
    -> CsvParameterError
{
    CsvParameterError { record, column, offset, kind }
}

fn limit_error(record: usize, column: Option<usize>, offset: usize,
    dimension: CsvParameterLimit, limit: usize, observed: usize) -> CsvParameterError
{
    error(record, column, offset, CsvParameterErrorKind::Limit { dimension, limit, observed })
}

#[derive(Debug)]
struct Field<'a> {
    text: Cow<'a, str>,
    quoted: bool,
    offset: usize,
}

struct Records<'a> {
    input: &'a str,
    at: usize,
    limits: CsvParameterLimits,
}

impl<'a> Records<'a> {
    fn field(&mut self, record: usize, column: usize) -> Result<Field<'a>, CsvParameterError> {
        let bytes = self.input.as_bytes();
        let offset = self.at;
        let quoted = bytes.get(self.at) == Some(&b'"');
        if quoted { self.at += 1; }
        let start = self.at;
        let mut decoded = 0;
        let mut escaped = false;
        let end;
        loop {
            let Some(&byte) = bytes.get(self.at) else {
                if quoted {
                    return Err(error(record, Some(column), offset,
                        CsvParameterErrorKind::UnterminatedQuote));
                }
                end = self.at;
                break;
            };
            if quoted && byte == b'"' {
                if bytes.get(self.at + 1) == Some(&b'"') {
                    escaped = true;
                    self.at += 2;
                } else {
                    end = self.at;
                    self.at += 1;
                    if bytes.get(self.at).is_some_and(|b| !matches!(*b, b',' | b'\r' | b'\n')) {
                        return Err(error(record, Some(column), self.at,
                            CsvParameterErrorKind::TrailingCharacters));
                    }
                    break;
                }
            } else if !quoted && matches!(byte, b',' | b'\r' | b'\n') {
                end = self.at;
                break;
            } else if !quoted && byte == b'"' {
                return Err(error(record, Some(column), self.at,
                    CsvParameterErrorKind::UnexpectedQuote));
            } else {
                self.at += 1;
            }
            decoded += 1;
            if decoded > self.limits.max_field_bytes {
                return Err(limit_error(record, Some(column), offset,
                    CsvParameterLimit::FieldBytes, self.limits.max_field_bytes, decoded));
            }
        }
        // Delimiters and quote boundaries are ASCII, hence UTF-8 boundaries.
        // Admission precedes allocation; doubled quotes can only shrink data.
        let raw = &self.input[start..end];
        let text = if escaped {
            Cow::Owned(raw.replace("\"\"", "\""))
        } else {
            Cow::Borrowed(raw)
        };
        Ok(Field { text, quoted, offset })
    }

    fn next(&mut self, record: usize) -> Result<Option<Vec<Field<'a>>>, CsvParameterError> {
        if self.at == self.input.len() { return Ok(None); }
        let mut fields = Vec::new();
        loop {
            if fields.len() >= self.limits.max_columns {
                return Err(limit_error(record, Some(fields.len()), self.at,
                    CsvParameterLimit::Columns, self.limits.max_columns, fields.len() + 1));
            }
            fields.push(self.field(record, fields.len())?);
            match self.input.as_bytes().get(self.at) {
                None => break,
                Some(b',') => self.at += 1,
                Some(b'\n') => { self.at += 1; break; }
                Some(b'\r') => {
                    if self.input.as_bytes().get(self.at + 1) != Some(&b'\n') {
                        return Err(error(record, Some(fields.len() - 1), self.at,
                            CsvParameterErrorKind::InvalidLineEnding));
                    }
                    self.at += 2;
                    break;
                }
                // field() only stops at a checked delimiter or EOF.
                Some(_) => unreachable!("CSV field boundary invariant"),
            }
        }
        Ok(Some(fields))
    }
}

fn signed_decimal(text: &str) -> bool {
    let digits = text.strip_prefix('-').unwrap_or(text);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

fn bind_field(arguments: GqlParameters, field: &Field<'_>, spec: &GqlParameterSpec,
    record: usize, column: usize) -> Result<GqlParameters, CsvParameterError>
{
    let fail = |kind| error(record, Some(column), field.offset, kind);
    let invalid = || fail(CsvParameterErrorKind::InvalidValue);
    let text = field.text.as_ref();
    let result = if !field.quoted && text == "\\N" {
        if !matches!(spec.parameter_type, GqlParameterType::Scalar(_)) {
            return Err(fail(CsvParameterErrorKind::NullNotAllowed));
        }
        arguments.with_null(spec.name.as_str())
    } else {
        match spec.parameter_type {
            GqlParameterType::Int64 => {
                if !signed_decimal(text) { return Err(invalid()); }
                arguments.with_int64(spec.name.as_str(), text.parse().map_err(|_| invalid())?)
            }
            GqlParameterType::UInt64 => {
                if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(invalid());
                }
                let value = text.parse().map_err(|_| invalid())?;
                if spec.requires_positive && value == 0 { return Err(invalid()); }
                arguments.with_uint64(spec.name.as_str(), value)
            }
            GqlParameterType::Scalar(CanonicalScalarKind::Int) => {
                if !signed_decimal(text) { return Err(invalid()); }
                let value = text.parse().map_err(|_| invalid())?;
                arguments.with_scalar(spec.name.as_str(), CanonicalScalar::Int(value))
            }
            GqlParameterType::Scalar(CanonicalScalarKind::Bool) => {
                let value = match text {
                    "true" => true,
                    "false" => false,
                    _ => return Err(invalid()),
                };
                arguments.with_bool(spec.name.as_str(), value)
            }
            GqlParameterType::Scalar(CanonicalScalarKind::Text) => {
                arguments.with_text(spec.name.as_str(), text)
            }
            GqlParameterType::Scalar(CanonicalScalarKind::Null) => return Err(invalid()),
            _ => return Err(fail(CsvParameterErrorKind::UnsupportedType)),
        }
    };
    // Native map admission owns individual scalar/transcript validation. Do
    // not wrap its potentially name-bearing errors into a CSV diagnostic.
    result.map_err(|_| fail(CsvParameterErrorKind::ParameterRejected))
}

/// Decode all records into the existing canonical argument maps.
///
/// The header must contain every declared parameter exactly once. Unsupported,
/// duplicate or invalid declarations refuse before input field decoding. This
/// does not perform template binding or execute any statement. The returned
/// records can be passed to an ordinary prepared query or atomic write batch.
pub fn decode_csv_parameters(input: &str, schema: &[GqlParameterSpec],
    limits: CsvParameterLimits) -> Result<Vec<GqlParameters>, CsvParameterError>
{
    let limits = limits.effective();
    if input.len() > limits.max_input_bytes {
        return Err(limit_error(0, None, 0, CsvParameterLimit::InputBytes,
            limits.max_input_bytes, input.len()));
    }
    if schema.is_empty() {
        return Err(error(0, None, 0, CsvParameterErrorKind::EmptySchema));
    }
    if schema.len() > limits.max_columns {
        return Err(limit_error(0, None, 0, CsvParameterLimit::Columns,
            limits.max_columns, schema.len()));
    }
    let mut names = BTreeMap::new();
    let mut name_probe = GqlParameters::new();
    for (index, spec) in schema.iter().enumerate() {
        if spec.name.len() > crate::parameters::MAX_GQL_PARAMETER_NAME_BYTES
            || name_probe.insert(spec.name.as_str(), GqlParameterValue::Int64(0)).is_err()
        {
            return Err(error(0, None, 0, CsvParameterErrorKind::InvalidSchema));
        }
        if !matches!(spec.parameter_type,
            GqlParameterType::Int64 | GqlParameterType::UInt64
            | GqlParameterType::Scalar(CanonicalScalarKind::Null | CanonicalScalarKind::Bool
                | CanonicalScalarKind::Int | CanonicalScalarKind::Text))
        {
            return Err(error(0, None, 0, CsvParameterErrorKind::UnsupportedType));
        }
        names.insert(spec.name.as_str(), index);
    }
    // The temporary native map validates names without reimplementing their
    // grammar. Do not retain it alongside the whole decoded batch.
    drop(name_probe);
    let mut reader = Records {
        input,
        at: if input.starts_with('\u{feff}') { 3 } else { 0 },
        limits,
    };
    let header = reader.next(0)?
        .ok_or_else(|| error(0, None, reader.at, CsvParameterErrorKind::MissingHeader))?;
    if header.len() != schema.len() {
        return Err(error(0, None, 0, CsvParameterErrorKind::Width {
            expected: schema.len(), observed: header.len(),
        }));
    }
    let mut order = Vec::with_capacity(schema.len());
    let mut seen = vec![false; schema.len()];
    for (column, field) in header.iter().enumerate() {
        let index = *names.get(field.text.as_ref()).ok_or_else(|| {
            error(0, Some(column), field.offset, CsvParameterErrorKind::UnknownHeader)
        })?;
        if seen[index] {
            return Err(error(0, Some(column), field.offset, CsvParameterErrorKind::DuplicateHeader));
        }
        seen[index] = true;
        order.push(index);
    }
    drop(header);
    let mut rows = Vec::new();
    let mut parameter_bytes = 0usize;
    while reader.at < input.len() {
        let record = rows.len() + 1;
        let offset = reader.at;
        if rows.len() >= limits.max_records {
            return Err(limit_error(record, None, offset, CsvParameterLimit::Records,
                limits.max_records, record));
        }
        let fields = reader.next(record)?.expect("nonempty input suffix");
        if fields.len() != schema.len() {
            return Err(error(record, None, offset, CsvParameterErrorKind::Width {
                expected: schema.len(), observed: fields.len(),
            }));
        }
        let mut arguments = GqlParameters::new();
        for (column, field) in fields.iter().enumerate() {
            arguments = bind_field(arguments, field, &schema[order[column]], record, column)?;
        }
        // Account the complete native transcript, including repeated names,
        // headers and length prefixes. Sharing payloads cannot bypass this cap.
        let next = parameter_bytes.checked_add(arguments.canonical_byte_len());
        let Some(next) = next.filter(|bytes| *bytes <= limits.max_parameter_bytes) else {
            return Err(limit_error(record, None, offset, CsvParameterLimit::ParameterBytes,
                limits.max_parameter_bytes, next.unwrap_or(usize::MAX)));
        };
        parameter_bytes = next;
        rows.push(arguments);
    }
    if rows.is_empty() {
        return Err(error(1, None, reader.at, CsvParameterErrorKind::EmptyData));
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn specs(columns: &[(&str, GqlParameterType)]) -> Vec<GqlParameterSpec> {
        columns.iter().map(|(name, parameter_type)| GqlParameterSpec {
            name: (*name).into(), parameter_type: *parameter_type,
            requires_positive: false, occurrences: 1,
        }).collect()
    }
    fn text_schema() -> Vec<GqlParameterSpec> {
        specs(&[("text", GqlParameterType::Scalar(CanonicalScalarKind::Text))])
    }
    fn decode(input: &str, schema: &[GqlParameterSpec]) -> Result<Vec<GqlParameters>, CsvParameterError> {
        decode_csv_parameters(input, schema, CsvParameterLimits::default())
    }
    fn is_limit(error: &CsvParameterError, dimension: CsvParameterLimit) -> bool {
        matches!(error.kind, CsvParameterErrorKind::Limit { dimension: actual, .. } if actual == dimension)
    }

    #[test]
    fn reordered_header_and_canonical_transcripts_match_explicit_arguments() {
        let schema = specs(&[("id", GqlParameterType::Int64),
            ("text", GqlParameterType::Scalar(CanonicalScalarKind::Text))]);
        let rows = decode("text,id\r\n\"hello, \"\"world\"\"\",-7\r\n\"multi\nline\",8", &schema).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], GqlParameters::new().with_int64("id", -7).unwrap()
            .with_text("text", "hello, \"world\"").unwrap());
        assert_eq!(rows[1], GqlParameters::new().with_int64("id", 8).unwrap()
            .with_text("text", "multi\nline").unwrap());
    }

    #[test]
    fn null_empty_literal_marker_unicode_and_query_text_remain_distinct() {
        let rows = decode("\u{feff}text\n\\N\n\"\\N\"\n\"\"\n雪🙂\n\"'; MATCH (n) DETACH DELETE n; --\"\n",
            &text_schema()).unwrap();
        let expected = [
            GqlParameters::new().with_null("text").unwrap(),
            GqlParameters::new().with_text("text", "\\N").unwrap(),
            GqlParameters::new().with_text("text", "").unwrap(),
            GqlParameters::new().with_text("text", "雪🙂").unwrap(),
            GqlParameters::new().with_text("text", "'; MATCH (n) DETACH DELETE n; --").unwrap(),
        ];
        assert_eq!(rows, expected);
    }

    #[test]
    fn final_terminator_trailing_empty_field_and_blank_record_are_unambiguous() {
        let schema = specs(&[("id", GqlParameterType::UInt64),
            ("text", GqlParameterType::Scalar(CanonicalScalarKind::Text))]);
        assert_eq!(decode("id,text\n1,", &schema).unwrap(), decode("id,text\n1,\r\n", &schema).unwrap());
        assert_eq!(decode("text\n\n", &text_schema()).unwrap(),
            vec![GqlParameters::new().with_text("text", "").unwrap()]);
        assert!(matches!(decode("text\n", &text_schema()).unwrap_err().kind,
            CsvParameterErrorKind::EmptyData));
    }

    #[test]
    fn strict_syntax_and_logical_record_coordinates() {
        for (input, kind) in [
            ("text\n\"unterminated", CsvParameterErrorKind::UnterminatedQuote),
            ("text\nnot\"quoted", CsvParameterErrorKind::UnexpectedQuote),
            ("text\n\"quoted\" suffix", CsvParameterErrorKind::TrailingCharacters),
            ("text\nvalue\rnext", CsvParameterErrorKind::InvalidLineEnding),
        ] {
            let error = decode(input, &text_schema()).unwrap_err();
            assert_eq!(error.record, 1);
            assert_eq!(error.kind, kind);
        }
        let error = decode("text\n\"multi\nline\"\n\"secret\"oops", &text_schema()).unwrap_err();
        assert_eq!(error.record, 2);
        assert_eq!(error.offset, 26);
        assert!(!format!("{error:?} {error}").contains("secret"));
    }

    #[test]
    fn header_schema_and_row_width_fail_closed() {
        let schema = specs(&[("a", GqlParameterType::Int64), ("b", GqlParameterType::Int64)]);
        for (input, kind) in [
            ("a,a\n1,2", CsvParameterErrorKind::DuplicateHeader),
            ("a,other\n1,2", CsvParameterErrorKind::UnknownHeader),
            ("a,b\n1", CsvParameterErrorKind::Width { expected: 2, observed: 1 }),
            ("a,b\n1,2,3", CsvParameterErrorKind::Width { expected: 2, observed: 3 }),
        ] { assert_eq!(decode(input, &schema).unwrap_err().kind, kind); }
        let duplicates = vec![schema[0].clone(), schema[0].clone()];
        assert_eq!(decode("a,a\n1,2", &duplicates).unwrap_err().kind, CsvParameterErrorKind::InvalidSchema);
        assert_eq!(decode("", &text_schema()).unwrap_err().kind, CsvParameterErrorKind::MissingHeader);
    }

    #[test]
    fn numeric_extremes_and_boolean_values_use_exact_declared_types() {
        let schema = specs(&[("i", GqlParameterType::Int64), ("u", GqlParameterType::UInt64),
            ("b", GqlParameterType::Scalar(CanonicalScalarKind::Bool)),
            ("c", GqlParameterType::Scalar(CanonicalScalarKind::Int))]);
        let row = decode("i,u,b,c\n-9223372036854775808,18446744073709551615,true,42", &schema).unwrap();
        assert_eq!(row[0], GqlParameters::new().with_int64("i", i64::MIN).unwrap()
            .with_uint64("u", u64::MAX).unwrap().with_bool("b", true).unwrap()
            .with_scalar("c", CanonicalScalar::Int(42)).unwrap());
        let integer = specs(&[("n", GqlParameterType::Int64)]);
        for value in ["9223372036854775808", "-9223372036854775809", "+1", " 1", "1.0", "", "1e3"] {
            assert!(decode(&format!("n\n\"{value}\""), &integer).is_err());
        }
        assert_eq!(decode("n\n\\N", &integer).unwrap_err().kind, CsvParameterErrorKind::NullNotAllowed);
    }

    #[test]
    fn unsupported_schema_and_positive_limit_refuse_without_coercion() {
        for kind in [CanonicalScalarKind::Float, CanonicalScalarKind::Decimal,
            CanonicalScalarKind::Bytes, CanonicalScalarKind::Timestamp] {
            let schema = specs(&[("n", GqlParameterType::Scalar(kind))]);
            assert_eq!(decode("n\n1", &schema).unwrap_err().kind, CsvParameterErrorKind::UnsupportedType);
        }
        let mut schema = specs(&[("n", GqlParameterType::UInt64)]);
        schema[0].requires_positive = true;
        assert!(decode("n\n1", &schema).is_ok());
        assert!(decode("n\n0", &schema).is_err());
    }

    #[test]
    fn all_admission_dimensions_apply_including_cumulative_transcript_bytes() {
        let defaults = CsvParameterLimits::default();
        let input = "text\n雪\n雪";
        let rows = decode(input, &text_schema()).unwrap();
        let transcript_bytes: usize = rows.iter().map(GqlParameters::canonical_byte_len).sum();
        for (limits, dimension) in [
            (CsvParameterLimits { max_input_bytes: input.len() - 1, ..defaults }, CsvParameterLimit::InputBytes),
            (CsvParameterLimits { max_records: 1, ..defaults }, CsvParameterLimit::Records),
            (CsvParameterLimits { max_columns: 0, ..defaults }, CsvParameterLimit::Columns),
            (CsvParameterLimits { max_field_bytes: 3, ..defaults }, CsvParameterLimit::FieldBytes),
            (CsvParameterLimits { max_parameter_bytes: transcript_bytes - 1, ..defaults }, CsvParameterLimit::ParameterBytes),
        ] {
            assert!(is_limit(&decode_csv_parameters(input, &text_schema(), limits).unwrap_err(), dimension));
        }
        assert!(decode_csv_parameters(input, &text_schema(), CsvParameterLimits {
            max_input_bytes: input.len(), max_records: 2,
            max_parameter_bytes: transcript_bytes, ..defaults
        }).is_ok());
        let error = decode_csv_parameters("text\nok\n\"unfinished", &text_schema(),
            CsvParameterLimits { max_records: 1, ..defaults }).unwrap_err();
        assert!(is_limit(&error, CsvParameterLimit::Records));
    }

    #[test]
    fn decoded_field_limit_counts_utf8_bytes_and_unescaped_quotes() {
        let schema = specs(&[("x", GqlParameterType::Scalar(CanonicalScalarKind::Text))]);
        let limits = CsvParameterLimits { max_field_bytes: 3, ..Default::default() };
        assert!(decode_csv_parameters("x\n雪", &schema, limits).is_ok());
        assert!(is_limit(&decode_csv_parameters("x\n🙂", &schema, limits).unwrap_err(), CsvParameterLimit::FieldBytes));
        let limits = CsvParameterLimits { max_field_bytes: 1, ..limits };
        assert_eq!(decode_csv_parameters("x\n\"\"\"\"", &schema, limits).unwrap(),
            vec![GqlParameters::new().with_text("x", "\"").unwrap()]);
    }

    #[test]
    fn callers_cannot_disable_hard_limits_with_usize_max() {
        let requested = CsvParameterLimits {
            max_input_bytes: usize::MAX, max_records: usize::MAX,
            max_columns: usize::MAX, max_field_bytes: usize::MAX,
            max_parameter_bytes: usize::MAX,
        };
        assert_eq!(requested.effective(), CsvParameterLimits::HARD);
    }
}
