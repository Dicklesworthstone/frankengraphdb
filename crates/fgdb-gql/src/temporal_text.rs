//! Parse-once temporal wrapper over the existing bounded graph-text compiler.
//!
//! `FOR SYSTEM_TIME AS OF SEQ ...` selects the immutable snapshot used by the
//! host executor. The graph statement itself is still prepared by
//! `PreparedGraphText`; this module does not interpret graph patterns or execute
//! storage. The temporal clause is replaced by equal-length whitespace before
//! ordinary preparation so every downstream byte offset remains an offset in the
//! caller's original UTF-8 statement.

use crate::algebra::{GraphValueRow, PreparedGraphPattern};
use crate::{
    GqlParameterSpec, GqlParameterType, GqlParameterValue, GqlParameters, GraphPatternTextError,
    GraphPatternTextErrorKind, GraphSymbolResolver, MAX_GRAPH_TEXT_BYTES, MAX_GRAPH_TEXT_TOKENS,
    PreparedGraphText,
};
use fgdb_types::CommitSeq;
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphTemporalTextErrorKind {
    Query(GraphPatternTextErrorKind),
    Pipeline(crate::GraphPipelineAggregateTextErrorKind),
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
pub struct GraphTemporalTextError {
    pub offset: usize,
    pub kind: GraphTemporalTextErrorKind,
}

impl core::fmt::Display for GraphTemporalTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "temporal graph text error at byte {}: {:?}",
            self.offset, self.kind
        )
    }
}
impl core::error::Error for GraphTemporalTextError {}
impl From<GraphPatternTextError> for GraphTemporalTextError {
    fn from(error: GraphPatternTextError) -> Self {
        Self {
            offset: error.offset,
            kind: GraphTemporalTextErrorKind::Query(error.kind),
        }
    }
}

impl From<crate::GraphPipelineAggregateTextError> for GraphTemporalTextError {
    fn from(error: crate::GraphPipelineAggregateTextError) -> Self {
        Self {
            offset: error.offset,
            kind: GraphTemporalTextErrorKind::Pipeline(error.kind),
        }
    }
}

fn failure(offset: usize, kind: GraphTemporalTextErrorKind) -> GraphTemporalTextError {
    GraphTemporalTextError { offset, kind }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SequenceSelector {
    Literal(u64),
    Parameter { name: String, offset: usize },
}

pub(super) enum TemplateSequenceSelector<'a> {
    Literal(u64),
    Parameter(&'a str),
}

/// Frame a resolved, unbound sequence selector without diagnostic offsets.
pub(super) fn append_template_sequence_selector(
    bytes: &mut Vec<u8>,
    selector: TemplateSequenceSelector<'_>,
) {
    match selector {
        TemplateSequenceSelector::Literal(value) => {
            bytes.push(0);
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        TemplateSequenceSelector::Parameter(name) => {
            bytes.push(1);
            bytes.extend_from_slice(&(name.len() as u64).to_be_bytes());
            bytes.extend_from_slice(name.as_bytes());
        }
    }
}

#[derive(Clone, Copy)]
enum RootTokenKind<'a> {
    Word(&'a str),
    Parameter(&'a str),
    Digits(&'a str),
    Other,
}
#[derive(Clone, Copy)]
struct RootToken<'a> {
    start: usize,
    end: usize,
    kind: RootTokenKind<'a>,
}
impl RootToken<'_> {
    fn word(self, expected: &str) -> bool {
        matches!(self.kind, RootTokenKind::Word(actual) if actual.eq_ignore_ascii_case(expected))
    }
}

fn root_tokens(text: &str) -> Vec<RootToken<'_>> {
    let bytes = text.as_bytes();
    let mut result = Vec::new();
    let mut at = 0;
    let mut parens = 0_u32;
    let mut brackets = 0_u32;
    let mut braces = 0_u32;
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
                parens = parens.saturating_add(1);
                at += 1;
                continue;
            }
            b')' => {
                parens = parens.saturating_sub(1);
                at += 1;
                continue;
            }
            b'[' => {
                brackets = brackets.saturating_add(1);
                at += 1;
                continue;
            }
            b']' => {
                brackets = brackets.saturating_sub(1);
                at += 1;
                continue;
            }
            b'{' => {
                braces = braces.saturating_add(1);
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
            result.push(RootToken {
                start,
                end: at,
                kind: RootTokenKind::Word(&text[start..at]),
            });
            continue;
        }
        if ch == b'$' {
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
                result.push(RootToken {
                    start,
                    end: at,
                    kind: RootTokenKind::Parameter(&text[name..at]),
                });
            } else {
                result.push(RootToken {
                    start,
                    end: at,
                    kind: RootTokenKind::Other,
                });
            }
            continue;
        }
        if ch.is_ascii_digit() {
            at += 1;
            while bytes.get(at).is_some_and(u8::is_ascii_digit) {
                at += 1;
            }
            result.push(RootToken {
                start,
                end: at,
                kind: RootTokenKind::Digits(&text[start..at]),
            });
            continue;
        }
        at += text[at..].chars().next().map_or(1, char::len_utf8);
        result.push(RootToken {
            start,
            end: at,
            kind: RootTokenKind::Other,
        });
    }
    result
}

fn parameter_names(text: &str) -> BTreeSet<String> {
    let bytes = text.as_bytes();
    let mut result = BTreeSet::new();
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
                result.insert(text[start..end].to_owned());
                at = end;
                continue;
            }
        }
        at += text[at..].chars().next().map_or(1, char::len_utf8);
    }
    result
}

fn valid_parameter_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= crate::algebra::MAX_PATTERN_NAME_BYTES
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

fn locate_clause(
    statement: &str,
) -> Result<(usize, usize, SequenceSelector), GraphTemporalTextError> {
    if statement.len() > MAX_GRAPH_TEXT_BYTES {
        return Err(failure(
            MAX_GRAPH_TEXT_BYTES,
            GraphTemporalTextErrorKind::Query(GraphPatternTextErrorKind::DefinitionTooLarge),
        ));
    }
    let tokens = root_tokens(statement);
    if tokens.len() > MAX_GRAPH_TEXT_TOKENS {
        return Err(failure(
            statement.len(),
            GraphTemporalTextErrorKind::Query(GraphPatternTextErrorKind::TooManyTokens),
        ));
    }
    let first_tail = tokens
        .iter()
        .position(|token| token.word("WHERE") || token.word("OPTIONAL") || token.word("RETURN"));
    let mut matches = Vec::new();
    let mut loose_for = None;
    for at in 0..tokens.len() {
        if !tokens[at].word("FOR") {
            continue;
        }
        loose_for.get_or_insert(tokens[at].start);
        if at + 5 >= tokens.len()
            || !tokens[at + 1].word("SYSTEM_TIME")
            || !tokens[at + 2].word("AS")
            || !tokens[at + 3].word("OF")
            || !tokens[at + 4].word("SEQ")
        {
            continue;
        }
        let selector = match tokens[at + 5].kind {
            RootTokenKind::Digits(raw) => {
                let value = raw.parse::<u64>().map_err(|_| {
                    failure(
                        tokens[at + 5].start,
                        GraphTemporalTextErrorKind::InvalidSequenceSelector,
                    )
                })?;
                SequenceSelector::Literal(value)
            }
            RootTokenKind::Parameter(name) if valid_parameter_name(name) => {
                SequenceSelector::Parameter {
                    name: name.to_owned(),
                    offset: tokens[at + 5].start,
                }
            }
            _ => {
                return Err(failure(
                    tokens[at + 5].start,
                    GraphTemporalTextErrorKind::InvalidSequenceSelector,
                ));
            }
        };
        matches.push((at, tokens[at].start, tokens[at + 5].end, selector));
    }
    if matches.is_empty() {
        return Err(failure(
            loose_for.unwrap_or(0),
            GraphTemporalTextErrorKind::MissingSystemTimeClause,
        ));
    }
    if matches.len() != 1 {
        return Err(failure(
            matches[1].1,
            GraphTemporalTextErrorKind::DuplicateSystemTimeClause,
        ));
    }
    let (token_at, start, end, selector) = matches.pop().expect("one temporal selector");
    if first_tail.is_some_and(|tail| token_at >= tail) {
        return Err(failure(
            start,
            GraphTemporalTextErrorKind::InvalidSystemTimePosition,
        ));
    }
    let next = tokens.get(token_at + 6);
    if !next
        .is_some_and(|token| token.word("WHERE") || token.word("OPTIONAL") || token.word("RETURN"))
    {
        return Err(failure(
            end,
            GraphTemporalTextErrorKind::InvalidSystemTimePosition,
        ));
    }
    Ok((start, end, selector))
}

#[derive(Clone)]
pub struct PreparedTemporalGraphText {
    statement: String,
    inner: PreparedGraphText,
    selector: SequenceSelector,
    parameters: Vec<GqlParameterSpec>,
}
impl core::fmt::Debug for PreparedTemporalGraphText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedTemporalGraphText")
            .field("parameters", &self.parameters.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

impl PreparedTemporalGraphText {
    pub fn prepare(
        statement: &str,
        resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphTemporalTextError> {
        Self::prepare_with_parameter_types(statement, &[], resolve)
    }

    pub fn prepare_with_parameter_types(
        statement: &str,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphTemporalTextError> {
        let (start, end, selector) = locate_clause(statement)?;
        let mut seen = BTreeSet::new();
        for &(name, _) in declarations {
            if !valid_parameter_name(name) || !seen.insert(name) {
                return Err(failure(
                    0,
                    GraphTemporalTextErrorKind::Query(
                        GraphPatternTextErrorKind::ParameterDeclaration,
                    ),
                ));
            }
        }
        let temporal_name = match &selector {
            SequenceSelector::Parameter { name, .. } => Some(name.as_str()),
            SequenceSelector::Literal(_) => None,
        };
        if let Some(name) = temporal_name
            && declarations
                .iter()
                .find(|(declared, _)| *declared == name)
                .is_some_and(|(_, kind)| *kind != GqlParameterType::UInt64)
        {
            let offset = match &selector {
                SequenceSelector::Parameter { offset, .. } => *offset,
                _ => start,
            };
            return Err(failure(
                offset,
                GraphTemporalTextErrorKind::ConflictingParameterType,
            ));
        }
        let mut blanked = statement.as_bytes().to_vec();
        blanked[start..end].fill(b' ');
        let blanked =
            String::from_utf8(blanked).expect("replacing bytes with ASCII spaces preserves UTF-8");
        let inner_names = parameter_names(&blanked);
        for &(name, _) in declarations {
            if !inner_names.contains(name) && temporal_name != Some(name) {
                return Err(failure(
                    statement.len(),
                    GraphTemporalTextErrorKind::Query(
                        GraphPatternTextErrorKind::UnusedParameterDeclaration,
                    ),
                ));
            }
        }
        let inner_declarations = declarations
            .iter()
            .copied()
            .filter(|(name, _)| inner_names.contains(*name))
            .collect::<Vec<_>>();
        let inner = PreparedGraphText::prepare_with_parameter_types(
            &blanked,
            &inner_declarations,
            resolve,
        )?;
        let mut parameters = inner.parameter_schema().to_vec();
        if let Some(name) = temporal_name {
            if let Some(spec) = parameters.iter_mut().find(|spec| spec.name == name) {
                if spec.parameter_type != GqlParameterType::UInt64 {
                    let offset = match &selector {
                        SequenceSelector::Parameter { offset, .. } => *offset,
                        _ => start,
                    };
                    return Err(failure(
                        offset,
                        GraphTemporalTextErrorKind::ConflictingParameterType,
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
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        &self.parameters
    }

    /// Versioned resolved template transcript, including the snapshot selector.
    /// Literal constants and parameter names are retained; statement text,
    /// diagnostic offsets and bound argument values are excluded.
    #[must_use]
    pub fn canonical_template_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:gql:temporal-text-template:v1\0".to_vec();
        let inner = self.inner.template_bytes();
        bytes.extend_from_slice(&(inner.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&inner);
        let selector = match &self.selector {
            SequenceSelector::Literal(value) => TemplateSequenceSelector::Literal(*value),
            SequenceSelector::Parameter { name, .. } => TemplateSequenceSelector::Parameter(name),
        };
        append_template_sequence_selector(&mut bytes, selector);
        bytes
    }

    /// Snapshot selection followed by the wrapped logical template operators.
    #[must_use]
    pub fn template_operators(&self) -> Vec<&'static str> {
        let mut operators = self.inner.template_operators();
        operators.insert(0, "TemporalSnapshotSelect");
        operators
    }

    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<BoundTemporalGraphQuery, GraphTemporalTextError> {
        let as_of = match &self.selector {
            SequenceSelector::Literal(value) => *value,
            SequenceSelector::Parameter { name, offset } => match arguments.get(name) {
                None => {
                    return Err(failure(
                        *offset,
                        GraphTemporalTextErrorKind::MissingParameter,
                    ));
                }
                Some(GqlParameterValue::UInt64(value)) => value,
                Some(value) => {
                    return Err(failure(
                        *offset,
                        GraphTemporalTextErrorKind::ParameterTypeMismatch {
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
                    .expect("prepared parameter names are valid and unique");
            }
        }
        let pattern = self.inner.bind_parameters(&local)?;
        let recognized = self
            .parameters
            .iter()
            .filter(|spec| arguments.get(&spec.name).is_some())
            .count();
        if recognized != arguments.len() {
            return Err(failure(
                self.statement.len(),
                GraphTemporalTextErrorKind::UnexpectedArguments,
            ));
        }
        Ok(BoundTemporalGraphQuery {
            pattern,
            as_of: CommitSeq(as_of),
        })
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct BoundTemporalGraphQuery {
    pattern: PreparedGraphPattern<GraphValueRow>,
    as_of: CommitSeq,
}
impl core::fmt::Debug for BoundTemporalGraphQuery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BoundTemporalGraphQuery")
            .field("columns", &self.pattern.columns().len())
            .field("snapshot", &"[BOUND]")
            .finish()
    }
}
impl BoundTemporalGraphQuery {
    #[must_use]
    pub fn pattern(&self) -> &PreparedGraphPattern<GraphValueRow> {
        &self.pattern
    }
    #[must_use]
    pub const fn as_of(&self) -> CommitSeq {
        self.as_of
    }
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:temporal-graph-query:v1\0".to_vec();
        bytes.extend_from_slice(&self.as_of.0.to_be_bytes());
        let pattern = self.pattern.canonical_bytes();
        bytes.extend_from_slice(&(pattern.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&pattern);
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use std::cell::Cell;

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        }
    }

    #[test]
    fn literal_selector_reuses_the_ordinary_graph_compiler() {
        let source = "MATCH (a)-[:R]->(b) FOR SYSTEM_TIME AS OF SEQ 41 WHERE a.p >= 3 RETURN b";
        let mut calls = Cell::new(0);
        let temporal = PreparedTemporalGraphText::prepare(source, |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        })
        .unwrap();
        let bound = temporal.bind_parameters(&GqlParameters::new()).unwrap();
        assert_eq!(bound.as_of(), CommitSeq(41));
        let ordinary = PreparedGraphText::prepare(
            "MATCH (a)-[:R]->(b)                             WHERE a.p >= 3 RETURN b",
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        assert_eq!(bound.pattern(), &ordinary);
        assert_eq!(calls.get(), 2);
        assert_eq!(temporal.statement(), source);
        assert!(!format!("{temporal:?} {bound:?}").contains("41"));
    }

    #[test]
    fn selector_parameter_shares_one_exact_argument_contract() {
        let source =
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $asof WHERE n.p >= $floor RETURN n LIMIT $page";
        let template = PreparedTemporalGraphText::prepare(source, symbols).unwrap();
        assert_eq!(template.parameter_schema().len(), 3);
        assert_eq!(template.parameter_schema()[0].name, "asof");
        let args = GqlParameters::new()
            .with_uint64("asof", 7)
            .unwrap()
            .with_int64("floor", -2)
            .unwrap()
            .with_uint64("page", 3)
            .unwrap();
        let first = template.bind_parameters(&args).unwrap();
        assert_eq!(first.as_of(), CommitSeq(7));
        let second = template
            .bind_parameters(
                &GqlParameters::new()
                    .with_uint64("asof", 9)
                    .unwrap()
                    .with_int64("floor", -2)
                    .unwrap()
                    .with_uint64("page", 3)
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(second.as_of(), CommitSeq(9));
        assert_ne!(first.canonical_bytes(), second.canonical_bytes());
        assert!(matches!(
            template
                .bind_parameters(&GqlParameters::new())
                .unwrap_err()
                .kind,
            GraphTemporalTextErrorKind::MissingParameter
        ));
        let wrong = GqlParameters::new()
            .with_int64("asof", 7)
            .unwrap()
            .with_int64("floor", -2)
            .unwrap()
            .with_uint64("page", 3)
            .unwrap();
        assert!(matches!(
            template.bind_parameters(&wrong).unwrap_err().kind,
            GraphTemporalTextErrorKind::ParameterTypeMismatch { .. }
        ));
    }

    #[test]
    fn temporal_parameter_can_be_shared_only_as_uint64() {
        let ok = PreparedTemporalGraphText::prepare(
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $x RETURN n LIMIT $x",
            symbols,
        )
        .unwrap();
        assert_eq!(ok.parameter_schema().len(), 1);
        assert_eq!(ok.parameter_schema()[0].occurrences, 2);
        assert_eq!(
            ok.bind_parameters(&GqlParameters::new().with_uint64("x", 4).unwrap())
                .unwrap()
                .as_of(),
            CommitSeq(4)
        );
        let bad = PreparedTemporalGraphText::prepare(
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $x WHERE n.p = $x RETURN n",
            symbols,
        )
        .unwrap_err();
        assert!(matches!(
            bad.kind,
            GraphTemporalTextErrorKind::Query(GraphPatternTextErrorKind::ConflictingParameterTypes)
                | GraphTemporalTextErrorKind::ConflictingParameterType
        ));
    }

    #[test]
    fn malformed_position_and_declarations_refuse_before_catalog_access() {
        for source in [
            "MATCH (n) RETURN n",
            "FOR SYSTEM_TIME AS OF SEQ 1 MATCH (n) RETURN n",
            "MATCH (n) WHERE n.p=1 FOR SYSTEM_TIME AS OF SEQ 1 RETURN n",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ -1 RETURN n",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 FOR SYSTEM_TIME AS OF SEQ 2 RETURN n",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ nope RETURN n",
        ] {
            let calls = Cell::new(0);
            assert!(
                PreparedTemporalGraphText::prepare(source, |kind, name| {
                    calls.set(calls.get() + 1);
                    symbols(kind, name)
                })
                .is_err(),
                "{source}"
            );
            assert_eq!(
                calls.get(),
                0,
                "malformed temporal syntax reached catalog: {source}"
            );
        }
        let calls = Cell::new(0);
        let result = PreparedTemporalGraphText::prepare_with_parameter_types(
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN n",
            &[("at", GqlParameterType::Int64)],
            |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            },
        );
        assert!(matches!(
            result.unwrap_err().kind,
            GraphTemporalTextErrorKind::ConflictingParameterType
        ));
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn blanking_preserves_original_error_offsets_and_quoted_text_is_not_a_clause() {
        let source = "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 3 WHERE n.unknown = 1 RETURN n";
        let failed = PreparedTemporalGraphText::prepare(source, symbols).unwrap_err();
        assert_eq!(failed.offset, source.find("unknown").unwrap());
        assert!(matches!(
            failed.kind,
            GraphTemporalTextErrorKind::Query(GraphPatternTextErrorKind::UnknownSymbol(
                GraphSymbolKind::Property
            ))
        ));
        let quoted = "MATCH (n) WHERE n.p = 'FOR SYSTEM_TIME AS OF SEQ 999' RETURN n";
        assert!(matches!(
            PreparedTemporalGraphText::prepare(quoted, symbols)
                .unwrap_err()
                .kind,
            GraphTemporalTextErrorKind::MissingSystemTimeClause
        ));
    }
}
