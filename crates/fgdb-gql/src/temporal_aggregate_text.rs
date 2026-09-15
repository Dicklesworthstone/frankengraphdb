//! Parse-once `FOR SYSTEM_TIME AS OF SEQ` wrapper for aggregate GQL text.
//!
//! The temporal selector is removed with equal-length ASCII whitespace and the
//! remaining bytes are compiled by `PreparedGraphAggregateText`. This preserves
//! the original UTF-8 offsets while reusing grouping, HAVING, CASE/arithmetic,
//! catalog resolution and parameter binding. Execution remains host-owned.

use crate::{
    GqlParameterSpec, GqlParameterType, GqlParameterValue, GqlParameters,
    GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind, GraphTemporalTextError,
    GraphTemporalTextErrorKind, MAX_GRAPH_TEXT_BYTES, MAX_GRAPH_TEXT_TOKENS,
    PreparedGraphAggregate, PreparedGraphAggregateText,
};
use fgdb_types::CommitSeq;
use std::collections::BTreeSet;

fn fail(offset: usize, kind: GraphTemporalTextErrorKind) -> GraphTemporalTextError {
    GraphTemporalTextError { offset, kind }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SequenceSelector {
    Literal(u64),
    Parameter { name: String, offset: usize },
}
#[derive(Clone, Copy)]
enum Kind<'a> { Word(&'a str), Parameter(&'a str), Digits(&'a str), Other }
#[derive(Clone, Copy)]
struct Token<'a> { start: usize, end: usize, kind: Kind<'a> }
impl Token<'_> {
    fn word(self, expected: &str) -> bool {
        matches!(self.kind, Kind::Word(actual) if actual.eq_ignore_ascii_case(expected))
    }
}

fn tokens(text: &str) -> Vec<Token<'_>> {
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
                    if bytes.get(at + 1) == Some(&b'\'') { at += 2; } else { at += 1; break; }
                } else { at += text[at..].chars().next().map_or(1, char::len_utf8); }
            }
            continue;
        }
        match ch {
            b'(' => { parens += 1; at += 1; continue; }
            b')' => { parens = parens.saturating_sub(1); at += 1; continue; }
            b'[' => { brackets += 1; at += 1; continue; }
            b']' => { brackets = brackets.saturating_sub(1); at += 1; continue; }
            b'{' => { braces += 1; at += 1; continue; }
            b'}' => { braces = braces.saturating_sub(1); at += 1; continue; }
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
            while bytes.get(at).is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_') { at += 1; }
            out.push(Token { start, end: at, kind: Kind::Word(&text[start..at]) });
        } else if ch == b'$' {
            at += 1;
            let name = at;
            if bytes.get(at).is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_') {
                at += 1;
                while bytes.get(at).is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_') { at += 1; }
                out.push(Token { start, end: at, kind: Kind::Parameter(&text[name..at]) });
            } else {
                out.push(Token { start, end: at, kind: Kind::Other });
            }
        } else if ch.is_ascii_digit() {
            at += 1;
            while bytes.get(at).is_some_and(u8::is_ascii_digit) { at += 1; }
            out.push(Token { start, end: at, kind: Kind::Digits(&text[start..at]) });
        } else {
            at += text[at..].chars().next().map_or(1, char::len_utf8);
            out.push(Token { start, end: at, kind: Kind::Other });
        }
    }
    out
}

fn parameter_names(text: &str) -> BTreeSet<String> {
    let bytes = text.as_bytes();
    let mut names = BTreeSet::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'\'' {
            at += 1;
            while at < bytes.len() {
                if bytes[at] == b'\'' {
                    if bytes.get(at + 1) == Some(&b'\'') { at += 2; } else { at += 1; break; }
                } else { at += text[at..].chars().next().map_or(1, char::len_utf8); }
            }
            continue;
        }
        if bytes[at] == b'$' {
            let start = at + 1;
            let mut end = start;
            if bytes.get(end).is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_') {
                end += 1;
                while bytes.get(end).is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_') { end += 1; }
                names.insert(text[start..end].to_owned());
                at = end;
                continue;
            }
        }
        at += text[at..].chars().next().map_or(1, char::len_utf8);
    }
    names
}
fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= crate::algebra::MAX_PATTERN_NAME_BYTES
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

fn locate(statement: &str) -> Result<(usize, usize, SequenceSelector), GraphTemporalTextError> {
    if statement.len() > MAX_GRAPH_TEXT_BYTES {
        return Err(fail(MAX_GRAPH_TEXT_BYTES,
            GraphTemporalTextErrorKind::Query(GraphPatternTextErrorKind::DefinitionTooLarge)));
    }
    let tokens = tokens(statement);
    if tokens.len() > MAX_GRAPH_TEXT_TOKENS {
        return Err(fail(statement.len(),
            GraphTemporalTextErrorKind::Query(GraphPatternTextErrorKind::TooManyTokens)));
    }
    let first_tail = tokens.iter().position(|token|
        token.word("WHERE") || token.word("OPTIONAL") || token.word("RETURN"));
    let mut found = Vec::new();
    let mut loose_for = None;
    for at in 0..tokens.len() {
        if !tokens[at].word("FOR") { continue; }
        loose_for.get_or_insert(tokens[at].start);
        if at + 5 >= tokens.len() || !tokens[at + 1].word("SYSTEM_TIME")
            || !tokens[at + 2].word("AS") || !tokens[at + 3].word("OF")
            || !tokens[at + 4].word("SEQ") { continue; }
        let selector = match tokens[at + 5].kind {
            Kind::Digits(raw) => SequenceSelector::Literal(raw.parse::<u64>().map_err(|_| {
                fail(tokens[at + 5].start, GraphTemporalTextErrorKind::InvalidSequenceSelector)
            })?),
            Kind::Parameter(name) if valid_name(name) => SequenceSelector::Parameter {
                name: name.to_owned(), offset: tokens[at + 5].start,
            },
            _ => return Err(fail(tokens[at + 5].start, GraphTemporalTextErrorKind::InvalidSequenceSelector)),
        };
        found.push((at, tokens[at].start, tokens[at + 5].end, selector));
    }
    if found.is_empty() {
        return Err(fail(loose_for.unwrap_or(0), GraphTemporalTextErrorKind::MissingSystemTimeClause));
    }
    if found.len() != 1 {
        return Err(fail(found[1].1, GraphTemporalTextErrorKind::DuplicateSystemTimeClause));
    }
    let (at, start, end, selector) = found.pop().expect("one temporal clause");
    if first_tail.is_some_and(|tail| at >= tail) {
        return Err(fail(start, GraphTemporalTextErrorKind::InvalidSystemTimePosition));
    }
    if !tokens.get(at + 6).is_some_and(|next|
        next.word("WHERE") || next.word("OPTIONAL") || next.word("RETURN"))
    {
        return Err(fail(end, GraphTemporalTextErrorKind::InvalidSystemTimePosition));
    }
    Ok((start, end, selector))
}

#[derive(Clone)]
pub struct PreparedTemporalGraphAggregateText {
    statement: String,
    inner: PreparedGraphAggregateText,
    selector: SequenceSelector,
    parameters: Vec<GqlParameterSpec>,
}
impl core::fmt::Debug for PreparedTemporalGraphAggregateText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedTemporalGraphAggregateText")
            .field("columns", &self.inner.columns().len())
            .field("parameters", &self.parameters.len())
            .field("definition", &"[REDACTED]").finish()
    }
}
impl PreparedTemporalGraphAggregateText {
    pub fn prepare(statement: &str,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>)
        -> Result<Self, GraphTemporalTextError> {
        Self::prepare_with_parameter_types(statement, &[], resolve)
    }

    pub fn prepare_with_parameter_types(statement: &str, declarations: &[(&str, GqlParameterType)],
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>)
        -> Result<Self, GraphTemporalTextError> {
        let (start, end, selector) = locate(statement)?;
        let selector_offset = match &selector { SequenceSelector::Parameter { offset, .. } => *offset, _ => start };
        let temporal_name = match &selector { SequenceSelector::Parameter { name, .. } => Some(name.as_str()), _ => None };
        let mut seen = BTreeSet::new();
        for &(name, kind) in declarations {
            if !valid_name(name) || !seen.insert(name) {
                return Err(fail(0, GraphTemporalTextErrorKind::Query(GraphPatternTextErrorKind::ParameterDeclaration)));
            }
            if temporal_name == Some(name) && kind != GqlParameterType::UInt64 {
                return Err(fail(selector_offset, GraphTemporalTextErrorKind::ConflictingParameterType));
            }
        }
        let mut blanked = statement.as_bytes().to_vec();
        blanked[start..end].fill(b' ');
        let blanked = String::from_utf8(blanked).expect("ASCII blanking preserves UTF-8");
        let inner_names = parameter_names(&blanked);
        for &(name, _) in declarations {
            if !inner_names.contains(name) && temporal_name != Some(name) {
                return Err(fail(statement.len(),
                    GraphTemporalTextErrorKind::Query(GraphPatternTextErrorKind::UnusedParameterDeclaration)));
            }
        }
        let inner_declarations = declarations.iter().copied()
            .filter(|(name, _)| inner_names.contains(*name)).collect::<Vec<_>>();
        let inner = PreparedGraphAggregateText::prepare_with_parameter_types(&blanked, &inner_declarations, resolve)?;
        let mut parameters = inner.parameter_schema().to_vec();
        if let Some(name) = temporal_name {
            if let Some(spec) = parameters.iter_mut().find(|spec| spec.name == name) {
                if spec.parameter_type != GqlParameterType::UInt64 {
                    return Err(fail(selector_offset, GraphTemporalTextErrorKind::ConflictingParameterType));
                }
                spec.occurrences += 1;
            } else {
                parameters.insert(0, GqlParameterSpec { name: name.to_owned(),
                    parameter_type: GqlParameterType::UInt64, requires_positive: false, occurrences: 1 });
            }
        }
        Ok(Self { statement: statement.to_owned(), inner, selector, parameters })
    }

    #[must_use] pub fn statement(&self) -> &str { &self.statement }
    #[must_use] pub fn parameter_schema(&self) -> &[GqlParameterSpec] { &self.parameters }
    #[must_use] pub fn columns(&self) -> &[String] { self.inner.columns() }

    pub fn bind_parameters(&self, arguments: &GqlParameters)
        -> Result<BoundTemporalGraphAggregateQuery, GraphTemporalTextError> {
        let as_of = match &self.selector {
            SequenceSelector::Literal(value) => *value,
            SequenceSelector::Parameter { name, offset } => match arguments.get(name) {
                None => return Err(fail(*offset, GraphTemporalTextErrorKind::MissingParameter)),
                Some(GqlParameterValue::UInt64(value)) => value,
                Some(value) => return Err(fail(*offset,
                    GraphTemporalTextErrorKind::ParameterTypeMismatch { found: value.parameter_type() })),
            },
        };
        let mut local = GqlParameters::new();
        for spec in self.inner.parameter_schema() {
            if let Some(value) = arguments.get(&spec.name) {
                local.insert(spec.name.clone(), value)
                    .expect("prepared parameter names are valid and unique");
            }
        }
        let aggregate = self.inner.bind_parameters(&local)?;
        let recognized = self.parameters.iter().filter(|spec| arguments.get(&spec.name).is_some()).count();
        if recognized != arguments.len() {
            return Err(fail(self.statement.len(), GraphTemporalTextErrorKind::UnexpectedArguments));
        }
        Ok(BoundTemporalGraphAggregateQuery { aggregate, as_of: CommitSeq(as_of) })
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct BoundTemporalGraphAggregateQuery {
    aggregate: PreparedGraphAggregate,
    as_of: CommitSeq,
}
impl core::fmt::Debug for BoundTemporalGraphAggregateQuery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BoundTemporalGraphAggregateQuery")
            .field("aggregate", &self.aggregate)
            .field("snapshot", &"[BOUND]").finish()
    }
}
impl BoundTemporalGraphAggregateQuery {
    #[must_use] pub fn aggregate(&self) -> &PreparedGraphAggregate { &self.aggregate }
    #[must_use] pub const fn as_of(&self) -> CommitSeq { self.as_of }
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
    fn aggregate_selector_reuses_grouping_having_and_computed_input_compiler() {
        let source = "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN ABS(n.p) AS bucket,SUM(n.p*2) AS total GROUP BY ABS(n.p) HAVING total >= $floor ORDER BY total DESC";
        let calls = Cell::new(0);
        let template = PreparedTemporalGraphAggregateText::prepare(source, |kind, name| {
            calls.set(calls.get() + 1); symbols(kind, name)
        }).unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(template.columns(), &["bucket", "total"]);
        let args = GqlParameters::new().with_uint64("at", 8).unwrap().with_int64("floor", 10).unwrap();
        let first = template.bind_parameters(&args).unwrap();
        let second = template.bind_parameters(&GqlParameters::new().with_uint64("at", 9).unwrap()
            .with_int64("floor", 10).unwrap()).unwrap();
        assert_eq!(first.as_of(), CommitSeq(8));
        assert_eq!(second.as_of(), CommitSeq(9));
        assert_eq!(calls.get(), 1);
        assert_eq!(first.aggregate(), second.aggregate(), "snapshot choice is outside aggregate logical identity");
    }

    #[test]
    fn malformed_temporal_aggregate_refuses_before_catalog_access() {
        for source in [
            "MATCH (n) RETURN COUNT(*)",
            "MATCH (n) RETURN COUNT(*) FOR SYSTEM_TIME AS OF SEQ 1",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ -1 RETURN COUNT(*)",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 FOR SYSTEM_TIME AS OF SEQ 2 RETURN COUNT(*)",
        ] {
            let calls = Cell::new(0);
            assert!(PreparedTemporalGraphAggregateText::prepare(source, |kind, name| {
                calls.set(calls.get() + 1); symbols(kind, name)
            }).is_err(), "{source}");
            assert_eq!(calls.get(), 0, "{source}");
        }
    }
}
