//! One historical sequence for an entire bounded set-algebra statement.
//!
//! The selector is written in the first MATCH head as
//! `FOR SYSTEM_TIME AS OF SEQ <u64|$param>`. Equal-length blanking then hands the
//! complete statement to `PreparedGraphSetText`, so UNION/INTERSECT/EXCEPT,
//! computed projections, ordering and pagination retain their existing parser
//! and diagnostics. Every set operand executes against the same selected snapshot.

use crate::temporal_text::{TemplateSequenceSelector, append_template_sequence_selector};
use crate::{
    GqlParameterSpec, GqlParameterType, GqlParameterValue, GqlParameters,
    GraphPatternTextErrorKind, GraphSetTextErrorKind, GraphSymbol, GraphSymbolKind,
    MAX_GRAPH_TEXT_BYTES, MAX_GRAPH_TEXT_TOKENS, PreparedGraphSet, PreparedGraphSetText,
};
use fgdb_types::CommitSeq;
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphTemporalSetTextErrorKind {
    Set(GraphSetTextErrorKind),
    MissingSystemTimeClause,
    DuplicateSystemTimeClause,
    InvalidSystemTimePosition,
    InvalidSequenceSelector,
    ConflictingParameterType,
    MissingParameter,
    ParameterTypeMismatch { found: GqlParameterType },
    UnexpectedArguments,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphTemporalSetTextError {
    pub offset: usize,
    pub kind: GraphTemporalSetTextErrorKind,
}
impl core::fmt::Display for GraphTemporalSetTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "temporal graph set text error at byte {}: {:?}",
            self.offset, self.kind
        )
    }
}
impl core::error::Error for GraphTemporalSetTextError {}
fn fail(offset: usize, kind: GraphTemporalSetTextErrorKind) -> GraphTemporalSetTextError {
    GraphTemporalSetTextError { offset, kind }
}
fn pattern(offset: usize, kind: GraphPatternTextErrorKind) -> GraphTemporalSetTextError {
    fail(
        offset,
        GraphTemporalSetTextErrorKind::Set(GraphSetTextErrorKind::Pattern(kind)),
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Selector {
    Literal(u64),
    Parameter { name: String, offset: usize },
}
#[derive(Clone, Copy)]
enum Kind<'a> {
    Word(&'a str),
    Parameter(&'a str),
    Digits(&'a str),
    Other,
}
#[derive(Clone, Copy)]
struct Token<'a> {
    start: usize,
    end: usize,
    kind: Kind<'a>,
}
impl Token<'_> {
    fn word(self, expected: &str) -> bool {
        matches!(self.kind, Kind::Word(actual) if actual.eq_ignore_ascii_case(expected))
    }
}
fn scan(text: &str) -> Vec<Token<'_>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    let (mut parens, mut brackets, mut braces) = (0_u32, 0_u32, 0_u32);
    while at < bytes.len() {
        let ch = bytes[at];
        if ch == b'\'' {
            at += 1;
            while at < bytes.len() {
                if bytes[at] == b'\'' {
                    if bytes.get(at + 1) == Some(&b'\'') {
                        at += 2;
                    } else {
                        at += 1;
                        break;
                    }
                } else {
                    at += text[at..].chars().next().map_or(1, char::len_utf8);
                }
            }
            continue;
        }
        match ch {
            b'(' => {
                parens += 1;
                at += 1;
                continue;
            }
            b')' => {
                parens = parens.saturating_sub(1);
                at += 1;
                continue;
            }
            b'[' => {
                brackets += 1;
                at += 1;
                continue;
            }
            b']' => {
                brackets = brackets.saturating_sub(1);
                at += 1;
                continue;
            }
            b'{' => {
                braces += 1;
                at += 1;
                continue;
            }
            b'}' => {
                braces = braces.saturating_sub(1);
                at += 1;
                continue;
            }
            _ => {}
        }
        if parens != 0 || brackets != 0 || braces != 0 {
            at += text[at..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        if text[at..].chars().next().is_some_and(char::is_whitespace) {
            at += text[at..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        let start = at;
        if ch.is_ascii_alphabetic() || ch == b'_' {
            at += 1;
            while bytes
                .get(at)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
            {
                at += 1;
            }
            out.push(Token {
                start,
                end: at,
                kind: Kind::Word(&text[start..at]),
            });
        } else if ch == b'$' {
            at += 1;
            let name = at;
            if bytes
                .get(at)
                .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
            {
                at += 1;
                while bytes
                    .get(at)
                    .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
                {
                    at += 1;
                }
                out.push(Token {
                    start,
                    end: at,
                    kind: Kind::Parameter(&text[name..at]),
                });
            } else {
                out.push(Token {
                    start,
                    end: at,
                    kind: Kind::Other,
                });
            }
        } else if ch.is_ascii_digit() {
            at += 1;
            while bytes.get(at).is_some_and(u8::is_ascii_digit) {
                at += 1;
            }
            out.push(Token {
                start,
                end: at,
                kind: Kind::Digits(&text[start..at]),
            });
        } else {
            at += text[at..].chars().next().map_or(1, char::len_utf8);
            out.push(Token {
                start,
                end: at,
                kind: Kind::Other,
            });
        }
    }
    out
}
fn params(text: &str) -> BTreeSet<String> {
    let bytes = text.as_bytes();
    let mut out = BTreeSet::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'\'' {
            at += 1;
            while at < bytes.len() {
                if bytes[at] == b'\'' {
                    if bytes.get(at + 1) == Some(&b'\'') {
                        at += 2;
                    } else {
                        at += 1;
                        break;
                    }
                } else {
                    at += text[at..].chars().next().map_or(1, char::len_utf8);
                }
            }
            continue;
        }
        if bytes[at] == b'$' {
            let start = at + 1;
            let mut end = start;
            if bytes
                .get(end)
                .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
            {
                end += 1;
                while bytes
                    .get(end)
                    .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
                {
                    end += 1;
                }
                out.insert(text[start..end].to_owned());
                at = end;
                continue;
            }
        }
        at += text[at..].chars().next().map_or(1, char::len_utf8);
    }
    out
}
fn valid(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= crate::algebra::MAX_PATTERN_NAME_BYTES
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}
fn locate(statement: &str) -> Result<(usize, usize, Selector), GraphTemporalSetTextError> {
    if statement.len() > MAX_GRAPH_TEXT_BYTES {
        return Err(pattern(
            MAX_GRAPH_TEXT_BYTES,
            GraphPatternTextErrorKind::DefinitionTooLarge,
        ));
    }
    let tokens = scan(statement);
    if tokens.len() > MAX_GRAPH_TEXT_TOKENS {
        return Err(pattern(
            statement.len(),
            GraphPatternTextErrorKind::TooManyTokens,
        ));
    }
    let first_return = tokens.iter().position(|token| token.word("RETURN"));
    let mut found = Vec::new();
    let mut loose = None;
    for at in 0..tokens.len() {
        if !tokens[at].word("FOR") {
            continue;
        }
        loose.get_or_insert(tokens[at].start);
        if at + 5 >= tokens.len()
            || !tokens[at + 1].word("SYSTEM_TIME")
            || !tokens[at + 2].word("AS")
            || !tokens[at + 3].word("OF")
            || !tokens[at + 4].word("SEQ")
        {
            continue;
        }
        let selector = match tokens[at + 5].kind {
            Kind::Digits(raw) => Selector::Literal(raw.parse::<u64>().map_err(|_| {
                fail(
                    tokens[at + 5].start,
                    GraphTemporalSetTextErrorKind::InvalidSequenceSelector,
                )
            })?),
            Kind::Parameter(name) if valid(name) => Selector::Parameter {
                name: name.to_owned(),
                offset: tokens[at + 5].start,
            },
            _ => {
                return Err(fail(
                    tokens[at + 5].start,
                    GraphTemporalSetTextErrorKind::InvalidSequenceSelector,
                ));
            }
        };
        found.push((at, tokens[at].start, tokens[at + 5].end, selector));
    }
    if found.is_empty() {
        return Err(fail(
            loose.unwrap_or(0),
            GraphTemporalSetTextErrorKind::MissingSystemTimeClause,
        ));
    }
    if found.len() != 1 {
        return Err(fail(
            found[1].1,
            GraphTemporalSetTextErrorKind::DuplicateSystemTimeClause,
        ));
    }
    let (at, start, end, selector) = found.pop().expect("one selector");
    if first_return.is_some_and(|tail| at >= tail) {
        return Err(fail(
            start,
            GraphTemporalSetTextErrorKind::InvalidSystemTimePosition,
        ));
    }
    if !tokens
        .get(at + 6)
        .is_some_and(|next| next.word("WHERE") || next.word("OPTIONAL") || next.word("RETURN"))
    {
        return Err(fail(
            end,
            GraphTemporalSetTextErrorKind::InvalidSystemTimePosition,
        ));
    }
    Ok((start, end, selector))
}

#[derive(Clone)]
pub struct PreparedTemporalGraphSetText {
    statement: String,
    inner: PreparedGraphSetText,
    selector: Selector,
    parameters: Vec<GqlParameterSpec>,
}
impl core::fmt::Debug for PreparedTemporalGraphSetText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedTemporalGraphSetText")
            .field("columns", &self.inner.columns().len())
            .field("parameters", &self.parameters.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedTemporalGraphSetText {
    pub fn prepare(
        statement: &str,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphTemporalSetTextError> {
        Self::prepare_with_parameter_types(statement, &[], resolve)
    }
    pub fn prepare_with_parameter_types(
        statement: &str,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphTemporalSetTextError> {
        let (start, end, selector) = locate(statement)?;
        let selector_offset = match &selector {
            Selector::Parameter { offset, .. } => *offset,
            _ => start,
        };
        let temporal_name = match &selector {
            Selector::Parameter { name, .. } => Some(name.as_str()),
            _ => None,
        };
        let mut seen = BTreeSet::new();
        for &(name, kind) in declarations {
            if !valid(name) || !seen.insert(name) {
                return Err(pattern(0, GraphPatternTextErrorKind::ParameterDeclaration));
            }
            if temporal_name == Some(name) && kind != GqlParameterType::UInt64 {
                return Err(fail(
                    selector_offset,
                    GraphTemporalSetTextErrorKind::ConflictingParameterType,
                ));
            }
        }
        let mut blanked = statement.as_bytes().to_vec();
        blanked[start..end].fill(b' ');
        let blanked = String::from_utf8(blanked).expect("ASCII blanking preserves UTF-8");
        let names = params(&blanked);
        for &(name, _) in declarations {
            if !names.contains(name) && temporal_name != Some(name) {
                return Err(pattern(
                    statement.len(),
                    GraphPatternTextErrorKind::UnusedParameterDeclaration,
                ));
            }
        }
        let local = declarations
            .iter()
            .copied()
            .filter(|(name, _)| names.contains(*name))
            .collect::<Vec<_>>();
        let inner = PreparedGraphSetText::prepare_with_parameter_types(&blanked, &local, resolve)
            .map_err(|error| {
            fail(error.offset, GraphTemporalSetTextErrorKind::Set(error.kind))
        })?;
        let mut parameters = inner.parameter_schema().to_vec();
        if let Some(name) = temporal_name {
            if let Some(spec) = parameters.iter_mut().find(|spec| spec.name == name) {
                if spec.parameter_type != GqlParameterType::UInt64 {
                    return Err(fail(
                        selector_offset,
                        GraphTemporalSetTextErrorKind::ConflictingParameterType,
                    ));
                }
                spec.occurrences += 1;
            } else {
                parameters.insert(
                    0,
                    GqlParameterSpec {
                        name: name.to_owned(),
                        parameter_type: GqlParameterType::UInt64,
                        requires_positive: false,
                        occurrences: 1,
                    },
                );
            }
        }
        Ok(Self {
            statement: statement.to_owned(),
            inner,
            selector,
            parameters,
        })
    }
    #[must_use]
    pub fn statement(&self) -> &str {
        &self.statement
    }
    #[must_use]
    pub fn columns(&self) -> &[String] {
        self.inner.columns()
    }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        &self.parameters
    }

    /// Versioned resolved set template transcript, including the snapshot selector.
    /// Literal constants and parameter names are retained; statement text,
    /// diagnostic offsets and bound argument values are excluded.
    #[must_use]
    pub fn canonical_template_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:gql:temporal-set-text-template:v1\0".to_vec();
        let inner = self.inner.canonical_template_bytes();
        bytes.extend_from_slice(&(inner.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&inner);
        let selector = match &self.selector {
            Selector::Literal(value) => TemplateSequenceSelector::Literal(*value),
            Selector::Parameter { name, .. } => TemplateSequenceSelector::Parameter(name),
        };
        append_template_sequence_selector(&mut bytes, selector);
        bytes
    }

    /// Snapshot selection followed by the wrapped logical set operators.
    #[must_use]
    pub fn template_operators(&self) -> Vec<&'static str> {
        let mut operators = self.inner.template_operators();
        operators.insert(0, "TemporalSnapshotSelect");
        operators
    }

    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<BoundTemporalGraphSetQuery, GraphTemporalSetTextError> {
        let as_of = match &self.selector {
            Selector::Literal(value) => *value,
            Selector::Parameter { name, offset } => match arguments.get(name) {
                None => {
                    return Err(fail(
                        *offset,
                        GraphTemporalSetTextErrorKind::MissingParameter,
                    ));
                }
                Some(GqlParameterValue::UInt64(value)) => value,
                Some(value) => {
                    return Err(fail(
                        *offset,
                        GraphTemporalSetTextErrorKind::ParameterTypeMismatch {
                            found: value.parameter_type(),
                        },
                    ));
                }
            },
        };
        let mut local = GqlParameters::new();
        for spec in self.inner.parameter_schema() {
            if let Some(value) = arguments.get(&spec.name) {
                local
                    .insert(spec.name.clone(), value)
                    .expect("prepared argument name");
            }
        }
        let query = self
            .inner
            .bind_parameters(&local)
            .map_err(|error| fail(error.offset, GraphTemporalSetTextErrorKind::Set(error.kind)))?;
        let recognized = self
            .parameters
            .iter()
            .filter(|spec| arguments.get(&spec.name).is_some())
            .count();
        if recognized != arguments.len() {
            return Err(fail(
                self.statement.len(),
                GraphTemporalSetTextErrorKind::UnexpectedArguments,
            ));
        }
        Ok(BoundTemporalGraphSetQuery {
            query,
            as_of: CommitSeq(as_of),
        })
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct BoundTemporalGraphSetQuery {
    query: PreparedGraphSet,
    as_of: CommitSeq,
}
impl core::fmt::Debug for BoundTemporalGraphSetQuery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BoundTemporalGraphSetQuery")
            .field("columns", &self.query.columns().len())
            .field("snapshot", &"[BOUND]")
            .finish()
    }
}
impl BoundTemporalGraphSetQuery {
    #[must_use]
    pub fn query(&self) -> &PreparedGraphSet {
        &self.query
    }
    #[must_use]
    pub const fn as_of(&self) -> CommitSeq {
        self.as_of
    }
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:temporal-set-query:v1\0".to_vec();
        bytes.extend_from_slice(&self.as_of.0.to_be_bytes());
        let query = self.query.canonical_bytes();
        bytes.extend_from_slice(&(query.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&query);
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_delta_types::PropertyKeyId;
    use std::cell::Cell;
    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        }
    }
    #[test]
    fn whole_compound_statement_shares_one_temporal_selector_and_catalog() {
        let text = "MATCH (a) FOR SYSTEM_TIME AS OF SEQ $at WHERE a.p >= $lo RETURN a.p AS p UNION ALL MATCH (b) WHERE b.p <= $hi RETURN b.p AS p ORDER BY p";
        let calls = Cell::new(0);
        let template = PreparedTemporalGraphSetText::prepare(text, |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        })
        .unwrap();
        assert_eq!(calls.get(), 1);
        let args = GqlParameters::new()
            .with_uint64("at", 7)
            .unwrap()
            .with_int64("lo", 1)
            .unwrap()
            .with_int64("hi", 4)
            .unwrap();
        let first = template.bind_parameters(&args).unwrap();
        let second = template
            .bind_parameters(
                &GqlParameters::new()
                    .with_uint64("at", 8)
                    .unwrap()
                    .with_int64("lo", 1)
                    .unwrap()
                    .with_int64("hi", 4)
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(first.query(), second.query());
        assert_eq!(calls.get(), 1);
        assert_ne!(first.canonical_bytes(), second.canonical_bytes());
    }
    #[test]
    fn second_temporal_selector_or_late_selector_refuses_before_catalog() {
        for text in [
            "MATCH (a) RETURN a UNION MATCH (b) RETURN b",
            "MATCH (a) RETURN a FOR SYSTEM_TIME AS OF SEQ 1 UNION MATCH (b) RETURN b",
            "MATCH (a) FOR SYSTEM_TIME AS OF SEQ 1 RETURN a UNION MATCH (b) FOR SYSTEM_TIME AS OF SEQ 1 RETURN b",
        ] {
            let calls = Cell::new(0);
            assert!(
                PreparedTemporalGraphSetText::prepare(text, |kind, name| {
                    calls.set(calls.get() + 1);
                    symbols(kind, name)
                })
                .is_err(),
                "{text}"
            );
            assert_eq!(calls.get(), 0);
        }
    }
}
