//! FgdbCliResultV1: bounded, complete-before-release, exact-domain NDJSON.
//!
//! This is a CLI rendering contract, not an alternative database/wire codec.
//! Scalars reuse STRICT_PORTABLE canonical bytes, and composite graph cells
//! reuse GraphValue's canonical encoding. Counts, i128 sums and averages keep
//! their distinct domains; JSON numbers never round 64/128-bit identities.

use crate::{Error, Format, MAX_OUTPUT_BYTES, MAX_RECORD_BYTES};
use fgdb::{QueryResult, QueryValue};
use fgdb_gql::algebra::GraphValue;
use fgdb_types::CanonicalScalar;
use std::fmt::{self, Write};

struct Buffer {
    text: String,
    limit: usize,
    record_start: usize,
}
impl Buffer {
    fn new(limit: usize) -> Result<Self, Error> {
        if limit == 0 || limit > MAX_OUTPUT_BYTES { return Err(Error::OutputLimit); }
        Ok(Self { text: String::new(), limit, record_start: 0 })
    }
    fn end_record(&mut self) -> fmt::Result {
        self.write_char('\n')?;
        self.record_start = self.text.len();
        Ok(())
    }
    fn quoted(&mut self, text: &str) -> fmt::Result {
        self.write_char('"')?;
        for c in text.chars() {
            match c {
                '"' => self.write_str("\\\"")?,
                '\\' => self.write_str("\\\\")?,
                '\n' => self.write_str("\\n")?,
                '\r' => self.write_str("\\r")?,
                '\t' => self.write_str("\\t")?,
                // Escape all control characters, including terminal control
                // and DEL/C1, in human mode as well as the machine contract.
                c if c.is_control() => write!(self, "\\u{:04x}", c as u32)?,
                c => self.write_char(c)?,
            }
        }
        self.write_char('"')
    }
    fn hex(&mut self, bytes: &[u8]) -> fmt::Result {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let needed = bytes.len().checked_mul(2).and_then(|n| n.checked_add(2)).ok_or(fmt::Error)?;
        self.check(needed)?;
        self.write_char('"')?;
        for &byte in bytes {
            self.write_char(HEX[(byte >> 4) as usize] as char)?;
            self.write_char(HEX[(byte & 15) as usize] as char)?;
        }
        self.write_char('"')
    }
    fn check(&self, additional: usize) -> fmt::Result {
        let next = self.text.len().checked_add(additional).ok_or(fmt::Error)?;
        if next > self.limit || next - self.record_start > MAX_RECORD_BYTES { return Err(fmt::Error); }
        Ok(())
    }
}
impl Write for Buffer {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.check(text.len())?;
        self.text.try_reserve(text.len()).map_err(|_| fmt::Error)?;
        self.text.push_str(text);
        Ok(())
    }
}

/// Render the entire bounded result before stdout receives ANY record. A row
/// that exceeds the output cap cannot leave an apparently successful prefix.
/// The consumer must nevertheless require `complete`: an OS write may fail.
pub fn render_result(result: &QueryResult, format: Format, limit: usize) -> Result<String, Error> {
    let QueryResult::Rows { columns, rows } = result else { return Err(Error::Query); };
    let mut out = Buffer::new(limit)?;
    let render = |out: &mut Buffer| -> fmt::Result {
        if format == Format::Ndjson { out.write_str("{\"version\":1,\"type\":\"columns\",\"columns\":[")?; }
        for (i, column) in columns.iter().enumerate() {
            if i > 0 { out.write_str(if format == Format::Ndjson { "," } else { "\t" })?; }
            out.quoted(column)?;
        }
        if format == Format::Ndjson { out.write_str("]}")?; }
        out.end_record()?;
        for row in rows {
            if row.len() != columns.len() { return Err(fmt::Error); }
            if format == Format::Ndjson { out.write_str("{\"version\":1,\"type\":\"row\",\"values\":[")?; }
            for (i, cell) in row.iter().enumerate() {
                if i > 0 { out.write_str(if format == Format::Ndjson { "," } else { "\t" })?; }
                value(out, cell, format)?;
            }
            if format == Format::Ndjson { out.write_str("]}")?; }
            out.end_record()?;
        }
        if format == Format::Ndjson {
            write!(out, "{{\"version\":1,\"type\":\"complete\",\"rows\":{}}}", rows.len())?;
        } else {
            write!(out, "{} row(s)", rows.len())?;
        }
        out.end_record()
    };
    render(&mut out).map_err(|_| Error::OutputLimit)?;
    Ok(out.text)
}

fn value(out: &mut Buffer, value: &QueryValue, format: Format) -> fmt::Result {
    match value {
        QueryValue::Count(n) => tagged_number(out, "count", n, format),
        QueryValue::Integer(n) => tagged_number(out, "integer", n, format),
        QueryValue::Average(n) => {
            if format == Format::Ndjson {
                write!(out, "{{\"type\":\"average\",\"numerator\":\"{}\",\"denominator\":\"{}\"}}", n.numerator(), n.denominator())
            } else { write!(out, "average({}/{})", n.numerator(), n.denominator()) }
        }
        QueryValue::Value(GraphValue::Vertex(id)) => tagged_number(out, "vertex", &id.0, format),
        QueryValue::Value(GraphValue::Edge(id)) => tagged_number(out, "edge", &id.0, format),
        QueryValue::Value(GraphValue::Scalar(scalar)) => {
            if format == Format::Human {
                match scalar {
                    CanonicalScalar::Null => return out.write_str("null"),
                    CanonicalScalar::Bool(value) => return write!(out, "{value}"),
                    CanonicalScalar::Int(value) => return write!(out, "int({value})"),
                    _ => {}
                }
            }
            let bytes = scalar.encode().map_err(|_| fmt::Error)?;
            encoded(out, "scalar", "strict-portable-v1", &bytes, format)
        }
        QueryValue::Value(graph) => {
            if !graph.validate_bounds() { return Err(fmt::Error); }
            let bytes = graph.canonical_bytes().map_err(|_| fmt::Error)?;
            encoded(out, "graph", "graph-value-v1", &bytes, format)
        }
    }
}
fn tagged_number(out: &mut Buffer, kind: &str, n: &impl fmt::Display, format: Format) -> fmt::Result {
    if format == Format::Ndjson { write!(out, "{{\"type\":\"{kind}\",\"value\":\"{n}\"}}") }
    else { write!(out, "{kind}({n})") }
}
fn encoded(out: &mut Buffer, kind: &str, encoding: &str, bytes: &[u8], format: Format) -> fmt::Result {
    if format == Format::Ndjson {
        write!(out, "{{\"type\":\"{kind}\",\"encoding\":\"{encoding}\",\"hex\":")?;
        out.hex(bytes)?;
        out.write_char('}')
    } else {
        write!(out, "{kind}:{encoding}(")?;
        out.hex(bytes)?;
        out.write_char(')')
    }
}

pub fn render_status(operation: &str, format: Format, limit: usize) -> Result<String, Error> {
    let mut out = Buffer::new(limit)?;
    let render = |out: &mut Buffer| -> fmt::Result {
        if format == Format::Ndjson {
            out.write_str("{\"version\":1,\"type\":\"complete\",\"operation\":")?;
            out.quoted(operation)?;
            out.write_char('}')?;
        } else {
            out.quoted(operation)?;
            out.write_str(" complete")?;
        }
        out.end_record()
    };
    render(&mut out).map_err(|_| Error::OutputLimit)?;
    Ok(out.text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_gql::GraphExactAverage;
    use fgdb_types::VId;
    fn result(values: Vec<QueryValue>) -> QueryResult {
        QueryResult::Rows { columns: (0..values.len()).map(|n| format!("c{n}")).collect(), rows: vec![values] }
    }
    #[test]
    fn exact_domains_never_become_json_numbers_or_debug_strings() {
        let result = result(vec![QueryValue::Count(u64::MAX), QueryValue::Integer(i128::MIN),
            QueryValue::Average(GraphExactAverage::new(1, 3).unwrap()),
            QueryValue::Value(GraphValue::Vertex(VId(u128::MAX)))]);
        let rendered = render_result(&result, Format::Ndjson, 4096).unwrap();
        assert!(rendered.contains("\"value\":\"18446744073709551615\""));
        assert!(rendered.contains(&format!("\"value\":\"{}\"", i128::MIN)));
        assert!(rendered.contains("\"numerator\":\"1\",\"denominator\":\"3\""));
        assert!(!rendered.contains("REDACTED"));
        assert_eq!(rendered.lines().count(), 3);
        assert!(rendered.ends_with("{\"version\":1,\"type\":\"complete\",\"rows\":1}\n"));
    }
    #[test]
    fn controls_are_escaped_and_output_is_bounded_in_both_formats() {
        let result = QueryResult::Rows { columns: vec!["x\"\\\n\u{1b}[31m".into()], rows: vec![] };
        for format in [Format::Human, Format::Ndjson] {
            let rendered = render_result(&result, format, 4096).unwrap();
            assert!(!rendered.contains('\u{1b}'));
            assert!(rendered.contains("\\u001b"));
            assert_eq!(render_result(&result, format, rendered.len()), Ok(rendered.clone()));
            assert_eq!(render_result(&result, format, rendered.len() - 1), Err(Error::OutputLimit));
        }
    }
    #[test]
    fn scalars_and_composites_reuse_existing_canonical_codecs() {
        let scalar = CanonicalScalar::ucs_basic_text("quoted\"\nsecret").unwrap();
        let value = GraphValue::List(vec![GraphValue::Scalar(scalar.clone())].into_boxed_slice());
        let rendered = render_result(&result(vec![QueryValue::Value(GraphValue::Scalar(scalar)),
            QueryValue::Value(value)]), Format::Ndjson, 4096).unwrap();
        assert!(rendered.contains("strict-portable-v1"));
        assert!(rendered.contains("graph-value-v1"));
        assert!(!rendered.contains("secret"));
    }
    #[test]
    fn malformed_rows_and_oversized_records_never_release_a_prefix() {
        let malformed = QueryResult::Rows { columns: vec![], rows: vec![vec![QueryValue::Count(1)]] };
        assert_eq!(render_result(&malformed, Format::Ndjson, 4096), Err(Error::OutputLimit));
        let huge = QueryResult::Rows { columns: vec!["x".repeat(MAX_RECORD_BYTES)], rows: vec![] };
        assert_eq!(render_result(&huge, Format::Ndjson, MAX_OUTPUT_BYTES), Err(Error::OutputLimit));
    }
}
