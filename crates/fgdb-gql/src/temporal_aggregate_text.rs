//! Parse-once `FOR SYSTEM_TIME AS OF SEQ` wrapper for aggregate GQL text.
//!
//! The temporal selector is removed with equal-length ASCII whitespace and the
//! remaining bytes are compiled by the direct or relational aggregate compiler.
//! This preserves original UTF-8 offsets, row-stage boundaries,
//! catalog resolution and parameter binding. Execution remains host-owned.

use crate::temporal_text::{TemplateSequenceSelector, append_template_sequence_selector};
use crate::{
    GqlParameterSpec, GqlParameterType, GqlParameterValue, GqlParameters, GraphAggregateTextSlot,
    GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind, GraphTemporalTextError,
    GraphTemporalTextErrorKind, MAX_GRAPH_TEXT_BYTES, MAX_GRAPH_TEXT_TOKENS,
    PreparedGraphAggregate, PreparedGraphAggregateText, PreparedGraphPipelineAggregateText,
    PreparedGraphSetAggregate, PreparedGraphText,
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

fn tokens(text: &str) -> Vec<Token<'_>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    let (mut parens, mut brackets, mut braces) = (0_u32, 0_u32, 0_u32);
    while at < bytes.len() {
        let ch = bytes[at];
        if let Some(end) = crate::graph_text::literal::opaque_end(text, at) {
            at = end;
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

fn parameter_names(text: &str) -> BTreeSet<String> {
    let bytes = text.as_bytes();
    let mut names = BTreeSet::new();
    let mut at = 0;
    while at < bytes.len() {
        if let Some(end) = crate::graph_text::literal::opaque_end(text, at) {
            at = end;
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
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

fn locate(statement: &str) -> Result<(usize, usize, SequenceSelector), GraphTemporalTextError> {
    if statement.len() > MAX_GRAPH_TEXT_BYTES {
        return Err(fail(
            MAX_GRAPH_TEXT_BYTES,
            GraphTemporalTextErrorKind::Query(GraphPatternTextErrorKind::DefinitionTooLarge),
        ));
    }
    let tokens = tokens(statement);
    if tokens.len() > MAX_GRAPH_TEXT_TOKENS {
        return Err(fail(
            statement.len(),
            GraphTemporalTextErrorKind::Query(GraphPatternTextErrorKind::TooManyTokens),
        ));
    }
    let first_tail = tokens.iter().position(|token| {
        token.word("WHERE") || token.word("OPTIONAL") || token.word("WITH") || token.word("RETURN")
    });
    let mut found = Vec::new();
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
            Kind::Digits(raw) => SequenceSelector::Literal(raw.parse::<u64>().map_err(|_| {
                fail(
                    tokens[at + 5].start,
                    GraphTemporalTextErrorKind::InvalidSequenceSelector,
                )
            })?),
            Kind::Parameter(name) if valid_name(name) => SequenceSelector::Parameter {
                name: name.to_owned(),
                offset: tokens[at + 5].start,
            },
            _ => {
                return Err(fail(
                    tokens[at + 5].start,
                    GraphTemporalTextErrorKind::InvalidSequenceSelector,
                ));
            }
        };
        found.push((at, tokens[at].start, tokens[at + 5].end, selector));
    }
    if found.is_empty() {
        return Err(fail(
            loose_for.unwrap_or(0),
            GraphTemporalTextErrorKind::MissingSystemTimeClause,
        ));
    }
    if found.len() != 1 {
        return Err(fail(
            found[1].1,
            GraphTemporalTextErrorKind::DuplicateSystemTimeClause,
        ));
    }
    let (at, start, end, selector) = found.pop().expect("one temporal clause");
    if first_tail.is_some_and(|tail| at >= tail) {
        return Err(fail(
            start,
            GraphTemporalTextErrorKind::InvalidSystemTimePosition,
        ));
    }
    if !tokens.get(at + 6).is_some_and(|next| {
        next.word("WHERE")
            || next.word("OPTIONAL")
            || next.word("WITH")
            || next.word("RETURN")
            || next.word("MATCH")
            || next.word("CALL")
            || next.word("UNWIND")
    }) {
        return Err(fail(
            end,
            GraphTemporalTextErrorKind::InvalidSystemTimePosition,
        ));
    }
    Ok((start, end, selector))
}

/// Admit leading and intermediate row sources through the shared lexical
/// tokens. Quoted text, delimited names and property names cannot forge stages.
fn has_row_sources(
    statement: &str,
    declarations: &[(&str, GqlParameterType)],
) -> Result<bool, crate::GraphPatternTextError> {
    use crate::set_text::TextKind;

    let tokens = PreparedGraphText::composition_tokens(statement, declarations)?;
    let mut depth = 0_usize;
    for (index, token) in tokens.iter().enumerate() {
        match token.kind {
            TextKind::Punct(b'(' | b'[' | b'{') => depth += 1,
            TextKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
            TextKind::Word(word) if depth == 0 => {
                let previous = index.checked_sub(1).and_then(|at| tokens.get(at));
                if previous.is_some_and(|previous| {
                    matches!(previous.kind, TextKind::Punct(b'.' | b':'))
                        || matches!(
                            previous.kind,
                            TextKind::Word(name) if name.eq_ignore_ascii_case("AS")
                        )
                }) {
                    continue;
                }
                if word.eq_ignore_ascii_case("RETURN") {
                    break;
                }
                if word.eq_ignore_ascii_case("CALL") || word.eq_ignore_ascii_case("UNWIND") {
                    return Ok(true);
                }
            }
            _ => {}
        }
    }
    Ok(false)
}

#[derive(Clone)]
enum AggregateTemplate {
    Direct(PreparedGraphAggregateText),
    Pipeline(PreparedGraphPipelineAggregateText),
}

impl AggregateTemplate {
    fn columns(&self) -> &[String] {
        match self {
            Self::Direct(inner) => inner.columns(),
            Self::Pipeline(inner) => inner.columns(),
        }
    }

    fn output_slots(&self) -> &[GraphAggregateTextSlot] {
        match self {
            Self::Direct(inner) => inner.output_slots(),
            Self::Pipeline(inner) => inner.output_slots(),
        }
    }

    fn parameter_schema(&self) -> &[GqlParameterSpec] {
        match self {
            Self::Direct(inner) => inner.parameter_schema(),
            Self::Pipeline(inner) => inner.parameter_schema(),
        }
    }

    fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphAggregate, GraphTemporalTextError> {
        match self {
            Self::Direct(inner) => Ok(inner.bind_parameters(arguments)?),
            Self::Pipeline(inner) => Ok(inner.bind_parameters(arguments)?),
        }
    }

    fn bind_source_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<TemporalAggregatePlan, GraphTemporalTextError> {
        match self {
            Self::Pipeline(inner) if inner.requires_relational_input() => Ok(
                TemporalAggregatePlan::Relation(inner.bind_relation_parameters(arguments)?),
            ),
            _ => Ok(TemporalAggregatePlan::Graph(self.bind_parameters(arguments)?)),
        }
    }
}

#[derive(Clone)]
pub struct PreparedTemporalGraphAggregateText {
    statement: String,
    inner: AggregateTemplate,
    selector: SequenceSelector,
    parameters: Vec<GqlParameterSpec>,
}
impl core::fmt::Debug for PreparedTemporalGraphAggregateText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedTemporalGraphAggregateText")
            .field("columns", &self.inner.columns().len())
            .field("parameters", &self.parameters.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedTemporalGraphAggregateText {
    pub fn prepare(
        statement: &str,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphTemporalTextError> {
        Self::prepare_with_parameter_types(statement, &[], resolve)
    }

    pub fn prepare_with_parameter_types(
        statement: &str,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphTemporalTextError> {
        Self::prepare_with_parameter_types_and_resolver(statement, declarations, resolve)
    }

    /// Prepare with the host's complete resolver, including its pinned tzdb
    /// and reverse catalog. The selected aggregate compiler owns resolution;
    /// this wrapper must not reduce the resolver to a symbol-only closure.
    /// The child's canonical transcript retains the timezone artifact identity.
    pub fn prepare_with_parameter_types_and_resolver(
        statement: &str,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl crate::GraphSymbolResolver,
    ) -> Result<Self, GraphTemporalTextError> {
        let (start, end, selector) = locate(statement)?;
        let selector_offset = match &selector {
            SequenceSelector::Parameter { offset, .. } => *offset,
            _ => start,
        };
        let temporal_name = match &selector {
            SequenceSelector::Parameter { name, .. } => Some(name.as_str()),
            _ => None,
        };
        let mut seen = BTreeSet::new();
        for &(name, kind) in declarations {
            if !valid_name(name) || !seen.insert(name) {
                return Err(fail(
                    0,
                    GraphTemporalTextErrorKind::Query(
                        GraphPatternTextErrorKind::ParameterDeclaration,
                    ),
                ));
            }
            if temporal_name == Some(name) && kind != GqlParameterType::UInt64 {
                return Err(fail(
                    selector_offset,
                    GraphTemporalTextErrorKind::ConflictingParameterType,
                ));
            }
        }
        let mut blanked = statement.as_bytes().to_vec();
        blanked[start..end].fill(b' ');
        let blanked = String::from_utf8(blanked).expect("ASCII blanking preserves UTF-8");
        let inner_names = parameter_names(&blanked);
        for &(name, _) in declarations {
            if !inner_names.contains(name) && temporal_name != Some(name) {
                return Err(fail(
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
        // Preserve the selected compiler and canonical transcript for every
        // already-admitted WITH definition. Newly admitted CALL/UNWIND sources
        // use the shared lexer so a name or literal cannot select a new owner.
        let pipeline = tokens(&blanked)
            .iter()
            .take_while(|token| !token.word("RETURN"))
            .any(|token| token.word("WITH"))
            || has_row_sources(&blanked, &inner_declarations)?;
        let inner = if pipeline {
            AggregateTemplate::Pipeline(
                PreparedGraphPipelineAggregateText::prepare_with_parameter_types_and_resolver(
                    &blanked,
                    &inner_declarations,
                    resolve,
                )?,
            )
        } else {
            AggregateTemplate::Direct(
                PreparedGraphAggregateText::prepare_with_parameter_types_and_resolver(
                    &blanked,
                    &inner_declarations,
                    resolve,
                )?,
            )
        };
        let mut parameters = inner.parameter_schema().to_vec();
        if let Some(name) = temporal_name {
            if let Some(spec) = parameters.iter_mut().find(|spec| spec.name == name) {
                if spec.parameter_type != GqlParameterType::UInt64 {
                    return Err(fail(
                        selector_offset,
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
    #[must_use]
    pub fn columns(&self) -> &[String] {
        self.inner.columns()
    }
    /// RETURN-position slot mapping of the wrapped aggregate, in RETURN order.
    /// Required by any text entrypoint that must map rows without re-deriving
    /// slots from aliases, which repeated returned aliases make impossible.
    #[must_use]
    pub fn output_slots(&self) -> &[GraphAggregateTextSlot] {
        self.inner.output_slots()
    }

    /// Versioned resolved aggregate template transcript, including the snapshot
    /// selector and direct/pipeline shape. Literal constants and parameter
    /// names are retained; statement text, diagnostic offsets and bound
    /// argument values are excluded.
    #[must_use]
    pub fn canonical_template_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:gql:temporal-aggregate-text-template:v1\0".to_vec();
        let (tag, inner) = match &self.inner {
            AggregateTemplate::Direct(inner) => (0, inner.canonical_template_bytes()),
            AggregateTemplate::Pipeline(inner) => (1, inner.canonical_template_bytes()),
        };
        bytes.extend_from_slice(&(inner.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&inner);
        bytes.push(tag);
        let selector = match &self.selector {
            SequenceSelector::Literal(value) => TemplateSequenceSelector::Literal(*value),
            SequenceSelector::Parameter { name, .. } => TemplateSequenceSelector::Parameter(name),
        };
        append_template_sequence_selector(&mut bytes, selector);
        bytes
    }

    /// Snapshot selection followed by the wrapped logical aggregate operators.
    #[must_use]
    pub fn template_operators(&self) -> Vec<&'static str> {
        let mut operators = match &self.inner {
            AggregateTemplate::Direct(inner) => inner.template_operators(),
            AggregateTemplate::Pipeline(inner) => inner.template_operators(),
        };
        operators.insert(0, "TemporalSnapshotSelect");
        operators
    }

    /// Bind a definition representable by the single-graph aggregate owner.
    /// Source-aware definitions retain its ordinary shape refusal; hosts that
    /// own every relational source use [Self::bind_source_parameters].
    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<BoundTemporalGraphAggregateQuery, GraphTemporalTextError> {
        let (as_of, aggregate) = self.bind_with(arguments, AggregateTemplate::bind_parameters)?;
        Ok(BoundTemporalGraphAggregateQuery { aggregate, as_of })
    }

    /// Bind the complete aggregate while retaining its actual source owner.
    /// The selected sequence applies to every graph and procedure source.
    /// Ordinary single-graph definitions keep their existing execution lane;
    /// leading rows, source-free inputs and continuations retain the complete
    /// native relation. Binding performs no execution or catalog access.
    pub fn bind_source_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<BoundTemporalAggregateQuery, GraphTemporalTextError> {
        let (as_of, aggregate) =
            self.bind_with(arguments, AggregateTemplate::bind_source_parameters)?;
        Ok(BoundTemporalAggregateQuery { aggregate, as_of })
    }

    fn bind_with<T>(
        &self,
        arguments: &GqlParameters,
        bind: impl FnOnce(&AggregateTemplate, &GqlParameters) -> Result<T, GraphTemporalTextError>,
    ) -> Result<(CommitSeq, T), GraphTemporalTextError> {
        let as_of = match &self.selector {
            SequenceSelector::Literal(value) => *value,
            SequenceSelector::Parameter { name, offset } => match arguments.get(name) {
                None => return Err(fail(*offset, GraphTemporalTextErrorKind::MissingParameter)),
                Some(GqlParameterValue::UInt64(value)) => value,
                Some(value) => {
                    return Err(fail(
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
        let aggregate = bind(&self.inner, &local)?;
        let recognized = self
            .parameters
            .iter()
            .filter(|spec| arguments.get(&spec.name).is_some())
            .count();
        if recognized != arguments.len() {
            return Err(fail(
                self.statement.len(),
                GraphTemporalTextErrorKind::UnexpectedArguments,
            ));
        }
        Ok((CommitSeq(as_of), aggregate))
    }
}

/// The admitted aggregate and its genuine source ownership. Hosts must pin the
/// bound sequence before executing either arm and share that cut across every
/// relational leaf; this enum grants no graph or procedure authorization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TemporalAggregatePlan {
    Graph(PreparedGraphAggregate),
    Relation(PreparedGraphSetAggregate),
}

/// A complete native aggregate bound to one historical database sequence.
#[derive(Clone, PartialEq, Eq)]
pub struct BoundTemporalAggregateQuery {
    aggregate: TemporalAggregatePlan,
    as_of: CommitSeq,
}

impl core::fmt::Debug for BoundTemporalAggregateQuery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BoundTemporalAggregateQuery")
            .field("aggregate", &self.aggregate)
            .field("snapshot", &"[BOUND]")
            .finish()
    }
}

impl BoundTemporalAggregateQuery {
    #[must_use]
    pub fn aggregate(&self) -> &TemporalAggregatePlan {
        &self.aggregate
    }

    #[must_use]
    pub const fn as_of(&self) -> CommitSeq {
        self.as_of
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
            .field("snapshot", &"[BOUND]")
            .finish()
    }
}
impl BoundTemporalGraphAggregateQuery {
    #[must_use]
    pub fn aggregate(&self) -> &PreparedGraphAggregate {
        &self.aggregate
    }
    #[must_use]
    pub const fn as_of(&self) -> CommitSeq {
        self.as_of
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
    fn aggregate_selector_reuses_grouping_having_and_computed_input_compiler() {
        let source = "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN ABS(n.p) AS bucket,SUM(n.p*2) AS total GROUP BY ABS(n.p) HAVING total >= $floor ORDER BY total DESC";
        let calls = Cell::new(0);
        let template = PreparedTemporalGraphAggregateText::prepare(source, |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        })
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(template.columns(), &["bucket", "total"]);
        let args = GqlParameters::new()
            .with_uint64("at", 8)
            .unwrap()
            .with_int64("floor", 10)
            .unwrap();
        let first = template.bind_parameters(&args).unwrap();
        let second = template
            .bind_parameters(
                &GqlParameters::new()
                    .with_uint64("at", 9)
                    .unwrap()
                    .with_int64("floor", 10)
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(first.as_of(), CommitSeq(8));
        assert_eq!(second.as_of(), CommitSeq(9));
        assert_eq!(calls.get(), 1);
        assert_eq!(
            first.aggregate(),
            second.aggregate(),
            "snapshot choice is outside aggregate logical identity"
        );
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
            assert!(
                PreparedTemporalGraphAggregateText::prepare(source, |kind, name| {
                    calls.set(calls.get() + 1);
                    symbols(kind, name)
                })
                .is_err(),
                "{source}"
            );
            assert_eq!(calls.get(), 0, "{source}");
        }
    }

    struct ObservedResolver<'a> {
        zones: &'a Cell<usize>,
        catalog: &'a Cell<usize>,
    }

    impl crate::GraphSymbolResolver for ObservedResolver<'_> {
        fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
            symbols(kind, name)
        }

        fn time_zones(&self) -> Option<crate::GqlTimeZones> {
            self.zones.set(self.zones.get() + 1);
            None
        }

        fn reverse_catalog(&self) -> Option<crate::ReverseSymbolCatalog> {
            self.catalog.set(self.catalog.get() + 1);
            Some(crate::ReverseSymbolCatalog::new())
        }
    }

    #[test]
    fn temporal_aggregates_forward_resolver_without_reopening_it_when_binding() {
        for source in [
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN SUM(n.p) AS total",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at WITH n.p AS p RETURN SUM(p) AS total",
        ] {
            let zones = Cell::new(0);
            let catalog = Cell::new(0);
            let template =
                PreparedTemporalGraphAggregateText::prepare_with_parameter_types_and_resolver(
                    source,
                    &[("at", GqlParameterType::UInt64)],
                    ObservedResolver {
                        zones: &zones,
                        catalog: &catalog,
                    },
                )
                .unwrap();
            assert!(
                zones.get() > 0,
                "the aggregate compiler must see the host resolver"
            );
            let before = (zones.get(), catalog.get());
            for at in [8, 9] {
                let bound = template
                    .bind_parameters(&GqlParameters::new().with_uint64("at", at).unwrap())
                    .unwrap();
                assert_eq!(bound.as_of(), CommitSeq(at));
            }
            assert_eq!((zones.get(), catalog.get()), before);
        }
    }

    #[test]
    fn resolver_entrypoint_preserves_closure_transcripts_and_parameter_contracts() {
        for source in [
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN SUM(n.p) AS total",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at WITH n.p AS p RETURN SUM(p) AS total",
        ] {
            let declarations = [("at", GqlParameterType::UInt64)];
            let original = PreparedTemporalGraphAggregateText::prepare_with_parameter_types(
                source,
                &declarations,
                symbols,
            )
            .unwrap();
            let resolved =
                PreparedTemporalGraphAggregateText::prepare_with_parameter_types_and_resolver(
                    source,
                    &declarations,
                    symbols,
                )
                .unwrap();
            assert_eq!(
                original.canonical_template_bytes(),
                resolved.canonical_template_bytes()
            );
            assert_eq!(original.parameter_schema(), resolved.parameter_schema());
            assert_eq!(original.output_slots(), resolved.output_slots());
            assert_eq!(original.template_operators(), resolved.template_operators());
        }
    }

    #[test]
    fn invalid_temporal_declarations_do_not_open_the_resolver() {
        let zones = Cell::new(0);
        let catalog = Cell::new(0);
        let result = PreparedTemporalGraphAggregateText::prepare_with_parameter_types_and_resolver(
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN COUNT(*) AS total",
            &[("at", GqlParameterType::Int64)],
            ObservedResolver {
                zones: &zones,
                catalog: &catalog,
            },
        );
        assert!(matches!(
            result.unwrap_err().kind,
            GraphTemporalTextErrorKind::ConflictingParameterType
        ));
        assert_eq!((zones.get(), catalog.get()), (0, 0));
    }

    #[test]
    fn source_binding_preserves_single_graph_plans_and_snapshot_independence() {
        for source in [
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN SUM(n.p) AS total",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at WITH n.p AS p RETURN SUM(p) AS total",
        ] {
            let template = PreparedTemporalGraphAggregateText::prepare(source, symbols).unwrap();
            let frozen = template.canonical_template_bytes();
            let arguments = GqlParameters::new().with_uint64("at", 8).unwrap();
            let narrow = template.bind_parameters(&arguments).unwrap();
            let bound = template.bind_source_parameters(&arguments).unwrap();
            let TemporalAggregatePlan::Graph(aggregate) = bound.aggregate() else {
                panic!("ordinary single-graph execution must retain its owner");
            };
            assert_eq!(aggregate, narrow.aggregate());
            assert_eq!(bound.as_of(), narrow.as_of());
            let later = template
                .bind_source_parameters(&GqlParameters::new().with_uint64("at", 9).unwrap())
                .unwrap();
            assert_eq!(bound.aggregate(), later.aggregate());
            assert_eq!(later.as_of(), CommitSeq(9));
            assert_eq!(template.canonical_template_bytes(), frozen);
        }
    }

    #[test]
    fn temporal_source_free_aggregate_keeps_exact_values_without_a_graph_callback() {
        let source = "UNWIND [1,1,3] AS x FOR SYSTEM_TIME AS OF SEQ $at \
                      RETURN COUNT(*) AS c,SUM(x) AS s,AVG(x) AS a";
        let template = PreparedTemporalGraphAggregateText::prepare(source, symbols).unwrap();
        let arguments = GqlParameters::new().with_uint64("at", 7).unwrap();
        assert!(
            template.bind_parameters(&arguments).is_err(),
            "the narrow owner must not fabricate a graph source"
        );
        let bound = template.bind_source_parameters(&arguments).unwrap();
        assert_eq!(bound.as_of(), CommitSeq(7));
        let TemporalAggregatePlan::Relation(aggregate) = bound.aggregate() else {
            panic!("UNWIND owns a real relation");
        };
        let rows = aggregate
            .execute_governed::<(), ()>(
                crate::GqlQueryPolicy::new(100_000, 100_000, 5_000_000, 5_000_000),
                |_, _| panic!("source-free aggregate cannot scan a graph"),
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values()[0].as_count(), Some(3));
        assert_eq!(rows[0].values()[1].as_integer(), Some(5));
        assert_eq!(
            rows[0].values()[2].as_average(),
            Some(crate::GraphExactAverage::new(5, 3).unwrap())
        );
    }

    #[test]
    fn source_binding_preserves_complete_mixed_prefix_and_resolves_only_once() {
        let source = "UNWIND [1,2] AS factor CALL test.rows(10) YIELD vertex AS n \
                      FOR SYSTEM_TIME AS OF SEQ $at \
                      CALL test.rows(20) YIELD vertex AS m MATCH (n) \
                      RETURN COUNT(*) AS c,SUM(n.p*factor) AS s";
        let calls = Cell::new(0);
        let template = PreparedTemporalGraphAggregateText::prepare(source, |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        })
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(
            template
                .template_operators()
                .iter()
                .filter(|operator| **operator == "ProcedureCall")
                .count(),
            2
        );
        let AggregateTemplate::Pipeline(inner) = &template.inner else {
            panic!("leading CALL and UNWIND require the relational compiler");
        };
        assert!(inner.requires_relational_input());
        assert_eq!(inner.graph_source_count(), 1);
        let expected = inner
            .bind_relation_parameters(&GqlParameters::new())
            .unwrap();
        for at in [7, 8] {
            let arguments = GqlParameters::new().with_uint64("at", at).unwrap();
            let bound = template.bind_source_parameters(&arguments).unwrap();
            assert_eq!(bound.as_of(), CommitSeq(at));
            let TemporalAggregatePlan::Relation(aggregate) = bound.aggregate() else {
                panic!("leading sources cannot be replaced with a single graph");
            };
            assert_eq!(aggregate, &expected);
            assert!(template.bind_parameters(&arguments).is_err());
        }
        assert_eq!(calls.get(), 1, "binding cannot reopen the catalog");
    }

    #[test]
    fn source_binding_keeps_selector_argument_precedence_and_original_offsets() {
        let source = "/* λ */ UNWIND [$value] AS x FOR SYSTEM_TIME AS OF SEQ $at \
                      RETURN SUM(x) AS s LIMIT $at";
        let template = PreparedTemporalGraphAggregateText::prepare_with_parameter_types(
            source,
            &[
                ("value", GqlParameterType::Int64),
                ("at", GqlParameterType::UInt64),
            ],
            symbols,
        )
        .unwrap();
        let selector = template
            .parameter_schema()
            .iter()
            .find(|spec| spec.name == "at")
            .unwrap();
        assert_eq!(selector.occurrences, 2);
        let missing = template
            .bind_source_parameters(&GqlParameters::new())
            .unwrap_err();
        assert_eq!(missing.offset, source.find("$at").unwrap());
        assert_eq!(missing.kind, GraphTemporalTextErrorKind::MissingParameter);
        let wrong = template
            .bind_source_parameters(&GqlParameters::new().with_int64("at", 7).unwrap())
            .unwrap_err();
        assert_eq!(wrong.offset, source.find("$at").unwrap());
        assert_eq!(
            wrong.kind,
            GraphTemporalTextErrorKind::ParameterTypeMismatch {
                found: GqlParameterType::Int64,
            }
        );
        let missing_value = template
            .bind_source_parameters(
                &GqlParameters::new()
                    .with_uint64("at", 7)
                    .unwrap()
                    .with_int64("extra", 1)
                    .unwrap(),
            )
            .unwrap_err();
        assert_eq!(missing_value.offset, source.find("$value").unwrap());
        assert!(matches!(
            missing_value.kind,
            GraphTemporalTextErrorKind::Pipeline(
                crate::GraphPipelineAggregateTextErrorKind::Input(
                    crate::GraphSetTextErrorKind::Pattern(
                        GraphPatternTextErrorKind::MissingParameter
                    )
                )
            )
        ));
        let arguments = GqlParameters::new()
            .with_uint64("at", 7)
            .unwrap()
            .with_int64("value", 4)
            .unwrap();
        assert!(template.bind_source_parameters(&arguments).is_ok());
        let unexpected = template
            .bind_source_parameters(&arguments.with_int64("extra", 1).unwrap())
            .unwrap_err();
        assert_eq!(unexpected.offset, source.len());
        assert_eq!(
            unexpected.kind,
            GraphTemporalTextErrorKind::UnexpectedArguments
        );
    }

    #[test]
    fn temporal_pipeline_selection_keeps_literals_and_nested_clauses_opaque() {
        let direct = PreparedTemporalGraphAggregateText::prepare(
            "MATCH (n) /* CALL test.rows() UNWIND [1] AS x WITH x */ \
             FOR SYSTEM_TIME AS OF SEQ 7 RETURN COUNT(*) AS c",
            symbols,
        )
        .unwrap()
        .bind_source_parameters(&GqlParameters::new())
        .unwrap();
        assert!(matches!(direct.aggregate(), TemporalAggregatePlan::Graph(_)));

        let inline = PreparedTemporalGraphAggregateText::prepare(
            "CALL { RETURN 1 AS x UNION ALL RETURN 2 AS x } \
             FOR SYSTEM_TIME AS OF SEQ 7 RETURN SUM(x) AS s",
            symbols,
        )
        .unwrap()
        .bind_source_parameters(&GqlParameters::new())
        .unwrap();
        let TemporalAggregatePlan::Relation(aggregate) = inline.aggregate() else {
            panic!("inline CALL owns the complete native relation");
        };
        let rows = aggregate
            .execute_governed::<(), ()>(
                crate::GqlQueryPolicy::new(100_000, 100_000, 5_000_000, 5_000_000),
                |_, _| panic!("inline row subquery cannot scan a graph"),
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values()[0].as_integer(), Some(3));

        for source in [
            "UNWIND [1] AS x WITH x FOR SYSTEM_TIME AS OF SEQ 7 RETURN COUNT(*)",
            "UNWIND [1] AS x FOR SYSTEM_TIME AS OF SEQ 7 \
             CALL test.rows() YIELD score FOR SYSTEM_TIME AS OF SEQ 8 RETURN COUNT(*)",
        ] {
            let calls = Cell::new(0);
            assert!(
                PreparedTemporalGraphAggregateText::prepare(source, |kind, name| {
                    calls.set(calls.get() + 1);
                    symbols(kind, name)
                })
                .is_err(),
                "{source}"
            );
            assert_eq!(calls.get(), 0, "{source}");
        }
    }

    #[test]
    fn call_after_with_retains_its_host_beside_the_single_graph_leaf() {
        let source = "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 7 WITH n \
                      CALL test.rows() YIELD score \
                      RETURN COUNT(*) AS c,SUM(score) AS total";
        let template = PreparedTemporalGraphAggregateText::prepare(source, symbols).unwrap();
        let AggregateTemplate::Pipeline(inner) = &template.inner else {
            panic!("WITH retains the row pipeline");
        };
        assert_eq!(inner.graph_source_count(), 1);
        assert!(inner.requires_relational_input());
        let arguments = GqlParameters::new();
        assert!(template.bind_parameters(&arguments).is_err());
        let bound = template.bind_source_parameters(&arguments).unwrap();
        let TemporalAggregatePlan::Relation(aggregate) = bound.aggregate() else {
            panic!("CALL after WITH still requires the procedure host");
        };
        assert!(aggregate.input().calls_procedure());
        assert_eq!(bound.as_of(), CommitSeq(7));
    }
}
