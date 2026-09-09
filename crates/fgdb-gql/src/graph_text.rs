//! Bounded connected-pattern text preparation, not a query interpreter.
//!
//! Parse once, resolve graph names once, then bind typed numeric arguments into
//! the existing GraphPatternBuilder. Execution uses the ordinary governed GLA
//! entrypoints. This profile is separate from the legacy two-hop statement and
//! artifact contract; it never falls back to that parser after a refusal.

use crate::algebra::{
    GlaDirection, GraphColumn, GraphPatternBuilder, GraphValueRow, IntegerComparison,
    MAX_PATTERN_EDGES, MAX_PATTERN_IDENTITIES, MAX_PATTERN_NAME_BYTES,
    MAX_PATTERN_PREDICATES, MAX_PATTERN_VERTICES, PatternBuildError, PreparedGraphPattern,
    VertexPredicate,
};
use crate::{GqlParameterSpec, GqlParameterType, GqlParameterValue, GqlParameters};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use std::collections::BTreeMap;

/// Definition admission, not execution work or a promise of cheap matching.
pub const MAX_GRAPH_TEXT_BYTES: usize = 65_536;
pub const MAX_GRAPH_TEXT_TOKENS: usize = 8_192;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GraphSymbolKind {
    Relation,
    Label,
    Property,
}

/// A host catalog resolves a name in the requested domain. Returning a symbol
/// from another domain is an error, never an integer cast or a guessed name.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GraphSymbol {
    Relation(RelationId),
    Label(LabelId),
    Property(PropertyKeyId),
}

impl GraphSymbol {
    #[must_use]
    pub const fn kind(self) -> GraphSymbolKind {
        match self {
            Self::Relation(_) => GraphSymbolKind::Relation,
            Self::Label(_) => GraphSymbolKind::Label,
            Self::Property(_) => GraphSymbolKind::Property,
        }
    }
}

impl core::fmt::Debug for GraphSymbol {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}([REDACTED])", self.kind())
    }
}

/// Diagnostics contain byte positions and structural classes, not query text,
/// identifiers, catalog IDs, or supplied argument values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPatternTextError {
    pub offset: usize,
    pub kind: GraphPatternTextErrorKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphPatternTextErrorKind {
    DefinitionTooLarge,
    TooManyTokens,
    InvalidToken,
    NameTooLong,
    Expected(&'static str),
    IntegerOutOfRange,
    UnknownVariable,
    UnknownSymbol(GraphSymbolKind),
    WrongSymbolKind { expected: GraphSymbolKind, found: GraphSymbolKind },
    ConflictingParameterTypes,
    MissingParameter,
    ParameterTypeMismatch { expected: GqlParameterType, found: GqlParameterType },
    UnexpectedArguments,
    Build(PatternBuildError),
}

impl core::fmt::Display for GraphPatternTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "graph-pattern text error at byte {}: {:?}", self.offset, self.kind)
    }
}
impl core::error::Error for GraphPatternTextError {}

fn error(offset: usize, kind: GraphPatternTextErrorKind) -> GraphPatternTextError {
    GraphPatternTextError { offset, kind }
}
fn built<T>(offset: usize, result: Result<T, PatternBuildError>) -> Result<T, GraphPatternTextError> {
    result.map_err(|kind| error(offset, GraphPatternTextErrorKind::Build(kind)))
}

#[derive(Clone, Copy)]
struct Name<'a> { text: &'a str, at: usize }
#[derive(Clone, Copy)]
enum TokenKind<'a> { Word(&'a str), Digits(&'a str), Parameter(&'a str), Punct(u8), End }
#[derive(Clone, Copy)]
struct Token<'a> { kind: TokenKind<'a>, at: usize }

struct Lexer<'a> { text: &'a str, at: usize, tokens: usize }
impl<'a> Lexer<'a> {
    fn next(&mut self) -> Result<Token<'a>, GraphPatternTextError> {
        while let Some(ch) = self.text[self.at..].chars().next() {
            if !ch.is_whitespace() { break; }
            self.at += ch.len_utf8();
        }
        let at = self.at;
        let bytes = self.text.as_bytes();
        if at == bytes.len() { return Ok(Token { kind: TokenKind::End, at }); }
        if self.tokens == MAX_GRAPH_TEXT_TOKENS {
            return Err(error(at, GraphPatternTextErrorKind::TooManyTokens));
        }
        self.tokens += 1;
        let ch = bytes[at];
        let parameter = ch == b'$';
        if parameter { self.at += 1; }
        let start = self.at;
        if bytes.get(start).is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_') {
            self.at += 1;
            while bytes.get(self.at).is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_') {
                self.at += 1;
            }
            if self.at - start > MAX_PATTERN_NAME_BYTES {
                return Err(error(at, GraphPatternTextErrorKind::NameTooLong));
            }
            let name = &self.text[start..self.at];
            return Ok(Token { kind: if parameter { TokenKind::Parameter(name) } else { TokenKind::Word(name) }, at });
        }
        if parameter { return Err(error(at, GraphPatternTextErrorKind::Expected("parameter name immediately after $"))); }
        if ch.is_ascii_digit() {
            self.at += 1;
            while bytes.get(self.at).is_some_and(u8::is_ascii_digit) { self.at += 1; }
            return Ok(Token { kind: TokenKind::Digits(&self.text[at..self.at]), at });
        }
        if b"()[]:,.<>=!-*".contains(&ch) {
            self.at += 1;
            return Ok(Token { kind: TokenKind::Punct(ch), at });
        }
        Err(error(at, GraphPatternTextErrorKind::InvalidToken))
    }
}

#[derive(Clone)]
enum Number { Literal(GqlParameterValue), Parameter(usize) }
impl Number {
    fn value(&self, arguments: &[GqlParameterValue]) -> GqlParameterValue {
        match self { Self::Literal(value) => *value, Self::Parameter(at) => arguments[*at] }
    }
    fn signed(&self, arguments: &[GqlParameterValue]) -> i64 {
        match self.value(arguments) {
            GqlParameterValue::Int64(value) => value,
            GqlParameterValue::UInt64(_) => unreachable!("private numeric syntax and schema agree"),
        }
    }
    fn unsigned(&self, arguments: &[GqlParameterValue]) -> u64 {
        match self.value(arguments) {
            GqlParameterValue::UInt64(value) => value,
            GqlParameterValue::Int64(_) => unreachable!("private numeric syntax and schema agree"),
        }
    }
}

struct Edge<'a> { source: Name<'a>, relation: Name<'a>, direction: GlaDirection, destination: Name<'a> }
enum Filter<'a> {
    Property { variable: Name<'a>, key: Name<'a>, comparison: IntegerComparison, value: Number },
    Identity { left: Name<'a>, right: Name<'a>, equal: bool },
}
struct Column<'a> { variable: Name<'a>, property: Option<Name<'a>>, alias: Name<'a> }
struct Syntax<'a> {
    variables: Vec<Name<'a>>,
    labels: Vec<(Name<'a>, Name<'a>)>,
    edges: Vec<Edge<'a>>,
    filters: Vec<Filter<'a>>,
    columns: Vec<Column<'a>>,
    parameters: Vec<GqlParameterSpec>,
    parameter_offsets: Vec<usize>,
    offset: Number,
    count: Option<Number>,
    distinct: bool,
    return_at: usize,
}

struct Parser<'a> { lexer: Lexer<'a>, current: Token<'a>, syntax: Syntax<'a>, predicates: usize, identities: usize }
impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Result<Self, GraphPatternTextError> {
        if text.len() > MAX_GRAPH_TEXT_BYTES { return Err(error(MAX_GRAPH_TEXT_BYTES, GraphPatternTextErrorKind::DefinitionTooLarge)); }
        let mut lexer = Lexer { text, at: 0, tokens: 0 };
        let current = lexer.next()?;
        Ok(Self { lexer, current, predicates: 0, identities: 0, syntax: Syntax {
            variables: Vec::new(), labels: Vec::new(), edges: Vec::new(), filters: Vec::new(), columns: Vec::new(),
            parameters: Vec::new(), parameter_offsets: Vec::new(), offset: Number::Literal(GqlParameterValue::UInt64(0)),
            count: None, distinct: false, return_at: 0,
        } })
    }
    fn advance(&mut self) -> Result<(), GraphPatternTextError> { self.current = self.lexer.next()?; Ok(()) }
    fn is_word(&self, word: &str) -> bool { matches!(self.current.kind, TokenKind::Word(actual) if actual.eq_ignore_ascii_case(word)) }
    fn word(&mut self, word: &'static str) -> Result<(), GraphPatternTextError> {
        if !self.is_word(word) { return Err(error(self.current.at, GraphPatternTextErrorKind::Expected(word))); }
        self.advance()
    }
    fn take_word(&mut self, word: &'static str) -> Result<bool, GraphPatternTextError> {
        if !self.is_word(word) { return Ok(false); }
        self.advance()?; Ok(true)
    }
    fn is_punct(&self, ch: u8) -> bool { matches!(self.current.kind, TokenKind::Punct(actual) if actual == ch) }
    fn take(&mut self, ch: u8) -> Result<bool, GraphPatternTextError> {
        if !self.is_punct(ch) { return Ok(false); }
        self.advance()?; Ok(true)
    }
    fn punct(&mut self, ch: u8, expected: &'static str) -> Result<(), GraphPatternTextError> {
        if self.take(ch)? { Ok(()) } else { Err(error(self.current.at, GraphPatternTextErrorKind::Expected(expected))) }
    }
    fn name(&mut self) -> Result<Name<'a>, GraphPatternTextError> {
        let TokenKind::Word(text) = self.current.kind else { return Err(error(self.current.at, GraphPatternTextErrorKind::Expected("identifier"))); };
        if ["MATCH", "WHERE", "RETURN", "ALL", "DISTINCT", "AS", "AND", "OR", "SKIP", "LIMIT", "OPTIONAL", "ORDER", "BY"]
            .iter().any(|word| text.eq_ignore_ascii_case(word)) {
            return Err(error(self.current.at, GraphPatternTextErrorKind::Expected("non-keyword identifier")));
        }
        let name = Name { text, at: self.current.at }; self.advance()?; Ok(name)
    }
    fn variable(&mut self) -> Result<Name<'a>, GraphPatternTextError> {
        let name = self.name()?;
        if !self.syntax.variables.iter().any(|var| var.text == name.text) {
            return Err(error(name.at, GraphPatternTextErrorKind::UnknownVariable));
        }
        Ok(name)
    }
    fn capacity(&self, count: usize, limit: usize, dimension: crate::algebra::PatternLimitDimension) -> Result<(), GraphPatternTextError> {
        if count < limit { return Ok(()); }
        Err(error(self.current.at, GraphPatternTextErrorKind::Build(PatternBuildError::LimitExceeded { dimension, limit, observed: count + 1 })))
    }
    fn node(&mut self) -> Result<Name<'a>, GraphPatternTextError> {
        use crate::algebra::PatternLimitDimension;
        self.punct(b'(', "(")?;
        let name = self.name()?;
        if !self.syntax.variables.iter().any(|var| var.text == name.text) {
            self.capacity(self.syntax.variables.len(), MAX_PATTERN_VERTICES, PatternLimitDimension::Vertices)?;
            self.syntax.variables.push(name);
        }
        while self.take(b':')? {
            self.capacity(self.predicates, MAX_PATTERN_PREDICATES, PatternLimitDimension::Predicates)?;
            let label = self.name()?; self.syntax.labels.push((name, label)); self.predicates += 1;
        }
        self.punct(b')', ")")?; Ok(name)
    }
    fn number(&mut self, expected: GqlParameterType) -> Result<Number, GraphPatternTextError> {
        let at = self.current.at;
        if let TokenKind::Parameter(name) = self.current.kind {
            let index = if let Some(index) = self.syntax.parameters.iter().position(|spec| spec.name == name) {
                let spec = &mut self.syntax.parameters[index];
                if spec.parameter_type != expected { return Err(error(at, GraphPatternTextErrorKind::ConflictingParameterTypes)); }
                spec.occurrences += 1; index
            } else {
                let index = self.syntax.parameters.len();
                self.syntax.parameters.push(GqlParameterSpec { name: name.to_owned(), parameter_type: expected, requires_positive: false, occurrences: 1 });
                self.syntax.parameter_offsets.push(at); index
            };
            self.advance()?; return Ok(Number::Parameter(index));
        }
        let negative = expected == GqlParameterType::Int64 && self.take(b'-')?;
        let TokenKind::Digits(digits) = self.current.kind else { return Err(error(at, GraphPatternTextErrorKind::Expected("typed integer or numeric parameter"))); };
        let magnitude = digits.parse::<u64>().map_err(|_| error(at, GraphPatternTextErrorKind::IntegerOutOfRange))?;
        let value = match expected {
            GqlParameterType::UInt64 => GqlParameterValue::UInt64(magnitude),
            GqlParameterType::Int64 => {
                let signed = if negative { -i128::from(magnitude) } else { i128::from(magnitude) };
                GqlParameterValue::Int64(i64::try_from(signed).map_err(|_| error(at, GraphPatternTextErrorKind::IntegerOutOfRange))?)
            }
        };
        self.advance()?; Ok(Number::Literal(value))
    }
    fn comparison(&mut self) -> Result<IntegerComparison, GraphPatternTextError> {
        use IntegerComparison::*;
        if self.take(b'=')? { return Ok(Equal); }
        if self.take(b'!')? { self.punct(b'=', "!=")?; return Ok(NotEqual); }
        if self.take(b'<')? {
            return Ok(if self.take(b'=')? { LessOrEqual } else if self.take(b'>')? { NotEqual } else { Less });
        }
        if self.take(b'>')? { return Ok(if self.take(b'=')? { GreaterOrEqual } else { Greater }); }
        Err(error(self.current.at, GraphPatternTextErrorKind::Expected("comparison operator")))
    }
    fn parse(mut self) -> Result<Syntax<'a>, GraphPatternTextError> {
        use crate::algebra::PatternLimitDimension;
        self.word("MATCH")?;
        loop {
            let mut left = self.node()?;
            while self.is_punct(b'-') || self.is_punct(b'<') {
                self.capacity(self.syntax.edges.len(), MAX_PATTERN_EDGES, PatternLimitDimension::Edges)?;
                let incoming = self.take(b'<')?;
                self.punct(b'-', "-")?; self.punct(b'[', "[")?; self.punct(b':', ":")?;
                let relation = self.name()?;
                self.punct(b']', "]")?; self.punct(b'-', "-")?;
                let outgoing = self.take(b'>')?;
                if incoming && outgoing { return Err(error(relation.at, GraphPatternTextErrorKind::Expected("one edge direction"))); }
                let right = self.node()?;
                self.syntax.edges.push(Edge { source: left, relation, destination: right, direction:
                    if incoming { GlaDirection::Reverse } else if outgoing { GlaDirection::Forward } else { GlaDirection::Undirected } });
                left = right;
            }
            if !self.take(b',')? { break; }
        }
        if self.take_word("WHERE")? {
            loop {
                let left = self.variable()?;
                if self.take(b'.')? {
                    self.capacity(self.predicates, MAX_PATTERN_PREDICATES, PatternLimitDimension::Predicates)?;
                    let key = self.name()?; let comparison = self.comparison()?;
                    let value = self.number(GqlParameterType::Int64)?;
                    self.syntax.filters.push(Filter::Property { variable: left, key, comparison, value }); self.predicates += 1;
                } else {
                    self.capacity(self.identities, MAX_PATTERN_IDENTITIES, PatternLimitDimension::Identities)?;
                    let comparison = self.comparison()?;
                    if !matches!(comparison, IntegerComparison::Equal | IntegerComparison::NotEqual) {
                        return Err(error(left.at, GraphPatternTextErrorKind::Expected("vertex equality or inequality")));
                    }
                    let right = self.variable()?;
                    self.syntax.filters.push(Filter::Identity { left, right, equal: comparison == IntegerComparison::Equal }); self.identities += 1;
                }
                if !self.take_word("AND")? { break; }
            }
        }
        self.syntax.return_at = self.current.at; self.word("RETURN")?;
        self.syntax.distinct = self.take_word("DISTINCT")?;
        if !self.syntax.distinct { self.take_word("ALL")?; }
        if self.take(b'*')? {
            self.syntax.columns.extend(self.syntax.variables.iter().copied().map(|name| Column { variable: name, property: None, alias: name }));
        } else {
            loop {
                self.capacity(self.syntax.columns.len(), MAX_PATTERN_VERTICES, PatternLimitDimension::Columns)?;
                let variable = self.variable()?;
                let property = if self.take(b'.')? { Some(self.name()?) } else { None };
                let alias = if self.take_word("AS")? { self.name()? } else { property.unwrap_or(variable) };
                if self.syntax.columns.iter().any(|column| column.alias.text == alias.text) {
                    return Err(error(alias.at, GraphPatternTextErrorKind::Build(PatternBuildError::DuplicateProjection)));
                }
                self.syntax.columns.push(Column { variable, property, alias });
                if !self.take(b',')? { break; }
            }
        }
        if self.take_word("SKIP")? { self.syntax.offset = self.number(GqlParameterType::UInt64)?; }
        if self.take_word("LIMIT")? { self.syntax.count = Some(self.number(GqlParameterType::UInt64)?); }
        if !matches!(self.current.kind, TokenKind::End) { return Err(error(self.current.at, GraphPatternTextErrorKind::Expected("end of statement"))); }
        Ok(self.syntax)
    }
}

#[derive(Clone)]
struct BoundFilter { variable: String, key: PropertyKeyId, comparison: IntegerComparison, value: Number }
#[derive(Clone)]
struct BoundColumn { alias: String, variable: String, key: Option<PropertyKeyId> }

/// Prepared syntax and schema, independent of parameter values and database
/// generations. Binding never lexes text, calls the catalog, or reads storage.
/// The host pins catalog/authorization validity; this is not a session lease.
#[derive(Clone)]
pub struct PreparedGraphText {
    statement: String,
    builder: GraphPatternBuilder,
    filters: Vec<BoundFilter>,
    columns: Vec<BoundColumn>,
    parameters: Vec<GqlParameterSpec>,
    parameter_offsets: Vec<usize>,
    offset: Number,
    count: Option<Number>,
    distinct: bool,
    return_at: usize,
}

impl core::fmt::Debug for PreparedGraphText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphText").field("columns", &self.columns.len())
            .field("parameters", &self.parameters.len()).field("definition", &"[REDACTED]").finish()
    }
}

impl PreparedGraphText {
    /// Prepare the bounded connected-pattern text profile. Keywords are ASCII
    /// case-insensitive; names are case-sensitive. RETURN defaults to ALL;
    /// DISTINCT is explicit. Integer predicates, numeric parameters, comma-
    /// connected paths, mixed directions, property columns, aliases, RETURN *,
    /// SKIP and LIMIT lower through the existing typed pattern compiler.
    ///
    /// Syntax is completely validated before calling `resolve`. Each unique
    /// (kind,name) is resolved once. Unknown or wrong-kind names fail closed.
    /// Resolution is a host-catalog seam, not a second catalog or authorization.
    pub fn prepare(
        statement: &str,
        mut resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphPatternTextError> {
        let syntax = Parser::new(statement)?.parse()?;
        let mut cache = BTreeMap::new();
        let mut symbol = |kind, name: Name<'_>| -> Result<GraphSymbol, GraphPatternTextError> {
            let key = (kind, name.text.to_owned());
            if let Some(value) = cache.get(&key) { return Ok(*value); }
            let value = resolve(kind, name.text).ok_or_else(|| error(name.at, GraphPatternTextErrorKind::UnknownSymbol(kind)))?;
            if value.kind() != kind {
                return Err(error(name.at, GraphPatternTextErrorKind::WrongSymbolKind { expected: kind, found: value.kind() }));
            }
            cache.insert(key, value); Ok(value)
        };
        let mut builder = GraphPatternBuilder::new();
        for name in &syntax.variables { built(name.at, builder.vertex(name.text))?; }
        for &(variable, label) in &syntax.labels {
            let GraphSymbol::Label(label_id) = symbol(GraphSymbolKind::Label, label)? else { unreachable!("symbol domain checked above") };
            built(variable.at, builder.filter(variable.text, VertexPredicate::HasLabel(label_id)))?;
        }
        for edge in &syntax.edges {
            let GraphSymbol::Relation(relation) = symbol(GraphSymbolKind::Relation, edge.relation)? else { unreachable!("symbol domain checked above") };
            built(edge.relation.at, builder.edge(edge.source.text, relation, edge.direction, edge.destination.text))?;
        }
        let mut filters = Vec::new();
        for filter in syntax.filters {
            match filter {
                Filter::Identity { left, right, equal } => { built(left.at, builder.identity(left.text, right.text, equal))?; }
                Filter::Property { variable, key, comparison, value } => {
                    let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, key)? else { unreachable!("symbol domain checked above") };
                    filters.push(BoundFilter { variable: variable.text.to_owned(), key, comparison, value });
                }
            }
        }
        let mut columns = Vec::new();
        for column in syntax.columns {
            let key = if let Some(name) = column.property {
                let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, name)? else { unreachable!("symbol domain checked above") };
                Some(key)
            } else { None };
            columns.push(BoundColumn { alias: column.alias.text.to_owned(), variable: column.variable.text.to_owned(), key });
        }
        // Refuse disconnected definitions at preparation, not on first binding.
        // This structural compilation does not observe numeric argument values.
        built(syntax.return_at, builder.prepare(syntax.variables[0].text, 0, None))?;
        Ok(Self { statement: statement.to_owned(), builder, filters, columns,
            parameters: syntax.parameters, parameter_offsets: syntax.parameter_offsets,
            offset: syntax.offset, count: syntax.count, distinct: syntax.distinct, return_at: syntax.return_at })
    }

    /// Explicit plaintext definition export; Debug never emits it.
    #[must_use]
    pub fn statement(&self) -> &str { &self.statement }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] { &self.parameters }

    /// Validate the exact argument set before building any concrete predicate.
    /// Returned plans are owned and immutable and use all existing governed
    /// snapshot/transaction entrypoints, including their original refusals.
    pub fn bind_parameters(&self, arguments: &GqlParameters) -> Result<PreparedGraphPattern<GraphValueRow>, GraphPatternTextError> {
        let mut values = Vec::new();
        for (index, spec) in self.parameters.iter().enumerate() {
            let at = self.parameter_offsets[index];
            let value = arguments.get(&spec.name).ok_or_else(|| error(at, GraphPatternTextErrorKind::MissingParameter))?;
            if value.parameter_type() != spec.parameter_type {
                return Err(error(at, GraphPatternTextErrorKind::ParameterTypeMismatch { expected: spec.parameter_type, found: value.parameter_type() }));
            }
            values.push(value);
        }
        if arguments.len() != values.len() { return Err(error(self.statement.len(), GraphPatternTextErrorKind::UnexpectedArguments)); }
        let mut builder = self.builder.clone();
        for filter in &self.filters {
            built(self.return_at, builder.filter(&filter.variable, VertexPredicate::IntegerProperty {
                key: filter.key, comparison: filter.comparison, value: filter.value.signed(&values),
            }))?;
        }
        let columns: Vec<_> = self.columns.iter().map(|column| match column.key {
            Some(key) => GraphColumn::property(&column.alias, &column.variable, key),
            None => GraphColumn::vertex(&column.alias, &column.variable),
        }).collect();
        let pattern = built(self.return_at, builder.prepare_values(&columns,
            self.offset.unsigned(&values), self.count.as_ref().map(|count| count.unsigned(&values))))?;
        Ok(if self.distinct { pattern } else { pattern.with_duplicates() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GqlQueryError, GqlQueryPolicy};
    use fgdb_types::{CanonicalScalar, VId};
    use std::cell::Cell;

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
            (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(3))),
            (GraphSymbolKind::Property, "n") => Some(GraphSymbol::Property(PropertyKeyId(4))),
            _ => None,
        }
    }
    fn query(text: &str) -> PreparedGraphPattern<GraphValueRow> {
        PreparedGraphText::prepare(text, symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
    }
    fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000) }
    fn vertex_rows(rows: &[GraphValueRow]) -> Vec<Vec<VId>> {
        rows.iter().map(|row| row.values().iter().map(|value| value.as_vertex().unwrap()).collect()).collect()
    }

    #[test]
    fn text_lowering_matches_manual_compiler_and_resolves_each_name_once() {
        let text = "MATCH (a:L)-[:R]->(b), (c)<-[:S]-(d), (b)-[:R]->(c), (d)-[:S]->(a) \
            WHERE c.n >= $min AND a <> d RETURN DISTINCT a AS owner,c.n AS score,d AS carrier SKIP $off LIMIT $count";
        let mut calls = BTreeMap::new();
        let template = PreparedGraphText::prepare(text, |kind, name| {
            *calls.entry((kind, name.to_owned())).or_insert(0) += 1;
            symbols(kind, name)
        }).unwrap();
        assert_eq!(calls.len(), 4);
        assert!(calls.values().all(|count| *count == 1));
        let arguments = GqlParameters::new().with_int64("min", 7).unwrap()
            .with_uint64("off", 1).unwrap().with_uint64("count", 3).unwrap();
        let actual = template.bind_parameters(&arguments).unwrap();
        let mut expected = GraphPatternBuilder::new();
        for name in ["a", "b", "c", "d"] { expected.vertex(name).unwrap(); }
        expected.filter("a", VertexPredicate::HasLabel(LabelId(3))).unwrap();
        for (left, relation, direction, right) in [
            ("a", 1, GlaDirection::Forward, "b"), ("c", 2, GlaDirection::Reverse, "d"),
            ("b", 1, GlaDirection::Forward, "c"), ("d", 2, GlaDirection::Forward, "a"),
        ] { expected.edge(left, RelationId(relation), direction, right).unwrap(); }
        expected.identity("a", "d", false).unwrap();
        expected.filter("c", VertexPredicate::IntegerProperty { key: PropertyKeyId(4), comparison: IntegerComparison::GreaterOrEqual, value: 7 }).unwrap();
        let expected = expected.prepare_values(&[
            GraphColumn::vertex("owner", "a"), GraphColumn::property("score", "c", PropertyKeyId(4)), GraphColumn::vertex("carrier", "d"),
        ], 1, Some(3)).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(template.statement(), text);
        assert_eq!(actual.columns(), &["owner", "score", "carrier"]);
    }

    #[test]
    fn text_bags_and_cycles_match_independent_complete_assignment_enumeration() {
        type Atom = (usize, u64, u8, usize);
        let cases: [(&str, &[Atom], [usize; 2], bool, bool); 3] = [
            ("MATCH (a)-[:R]->(b)-[:S]->(c) RETURN a,c", &[(0, 1, 0, 1), (1, 2, 0, 2)], [0, 2], false, false),
            ("MATCH (a)<-[:R]-(b)-[:S]-(c) WHERE a <> c RETURN ALL c,a", &[(0, 1, 1, 1), (1, 2, 2, 2)], [2, 0], false, true),
            ("MATCH (a)-[:R]-(b),(c)-[:S]->(b),(c)-[:R]->(a) RETURN DISTINCT c,a", &[(0, 1, 2, 1), (2, 2, 0, 1), (2, 1, 0, 0)], [2, 0], true, false),
        ];
        let universe: Vec<_> = (1..=2).flat_map(|r| (1..=2).flat_map(move |s| (1..=2).map(move |d| (VId(s), RelationId(r), VId(d))))).collect();
        for mask in 0..256_usize {
            let mut edges: Vec<_> = universe.iter().enumerate().filter(|(at, _)| mask & (1 << at) != 0).map(|(_, edge)| *edge).collect();
            if let Some(first) = edges.first().copied() { edges.push(first); }
            for (text, atoms, selected, distinct, unequal) in cases {
                let mut expected = Vec::new();
                for bits in 0..8 {
                    let assignment = [VId(1 + (bits & 1)), VId(1 + ((bits >> 1) & 1)), VId(1 + ((bits >> 2) & 1))];
                    if unequal && assignment[0] == assignment[2] { continue; }
                    let multiplicity = atoms.iter().map(|&(left, relation, direction, right)| edges.iter().filter(|&&(s, r, d)| {
                        if r != RelationId(relation) { return false; }
                        let (a, b) = (assignment[left], assignment[right]);
                        match direction { 0 => s == a && d == b, 1 => d == a && s == b, _ => (s == a && d == b) || (s == b && d == a) }
                    }).count()).product::<usize>();
                    for _ in 0..multiplicity { expected.push(selected.map(|at| assignment[at]).to_vec()); }
                }
                expected.sort(); if distinct { expected.dedup(); }
                let pattern = query(text);
                let actual = pattern.plan().execute_governed_with_properties(edges.len() as u64, [], edges.iter().copied(),
                    |_, _| Ok::<_, ()>(true), |_, _| Ok(None), policy(), || Ok::<_, ()>(())).unwrap();
                assert_eq!(vertex_rows(&actual.value), expected, "mask={mask}, {text}");
            }
        }
    }

    #[test]
    fn binding_reuses_resolved_structure_and_enforces_exact_typed_arguments() {
        let calls = Cell::new(0);
        let source = "MATCH (n:L) WHERE n.n >= $x AND n.n <= $x RETURN n SKIP $page LIMIT $page";
        let template = PreparedGraphText::prepare(source, |kind, name| { calls.set(calls.get() + 1); symbols(kind, name) }).unwrap();
        let before = calls.get();
        assert_eq!(template.parameter_schema().len(), 2);
        assert!(template.parameter_schema().iter().all(|spec| spec.occurrences == 2));
        let args = GqlParameters::new().with_int64("x", i64::MIN).unwrap().with_uint64("page", 0).unwrap();
        let first = template.bind_parameters(&args).unwrap();
        let frozen = first.canonical_bytes();
        let changed = GqlParameters::new().with_int64("x", i64::MAX).unwrap().with_uint64("page", 1).unwrap();
        assert_ne!(frozen, template.bind_parameters(&changed).unwrap().canonical_bytes());
        assert_eq!(first.canonical_bytes(), frozen);
        assert_eq!(calls.get(), before);
        assert!(matches!(template.bind_parameters(&GqlParameters::new()).unwrap_err().kind, GraphPatternTextErrorKind::MissingParameter));
        let wrong = GqlParameters::new().with_uint64("x", 1).unwrap().with_uint64("page", 1).unwrap();
        assert!(matches!(template.bind_parameters(&wrong).unwrap_err().kind, GraphPatternTextErrorKind::ParameterTypeMismatch { .. }));
        let extra = changed.with_int64("unused", 5).unwrap();
        assert_eq!(template.bind_parameters(&extra).unwrap_err().kind, GraphPatternTextErrorKind::UnexpectedArguments);
        let mut resolves = 0;
        let conflict = PreparedGraphText::prepare("MATCH (n:L) WHERE n.n = $x RETURN n LIMIT $x", |kind, name| { resolves += 1; symbols(kind, name) }).unwrap_err();
        assert_eq!(conflict.kind, GraphPatternTextErrorKind::ConflictingParameterTypes);
        assert_eq!(resolves, 0);
    }

    #[test]
    fn unsupported_or_malformed_input_never_reaches_name_resolution() {
        for text in [
            "", "MATCH", "MATCH () RETURN *", "MATCH (a)-[e:R]->(b) RETURN a", "MATCH (a)<-[:R]->(b) RETURN a",
            "MATCH (a)-[:R]->(b) RETURN DISTINCT ALL a", "MATCH (a)-[:R]->(b) RETURN missing",
            "MATCH (a) RETURN a;", "MATCH (a) RETURN a DROP GRAPH x", "MATCH (a) RETURN a ORDER BY a",
            "MATCH (a) WHERE a.n = 1 OR a.n = 2 RETURN a", "MATCH (a) WHERE a > a RETURN a",
            "MATCH (a) WHERE a.n = 1.5 RETURN a", "MATCH (a) WHERE a.n = $ x RETURN a",
            "MATCH (a) RETURN a SKIP -1", "MATCH (a) RETURN a LIMIT 18446744073709551616",
            "MATCH (a) WHERE a.n = 9223372036854775808 RETURN a", "MATCH (a) WHERE a.n = -9223372036854775809 RETURN a",
            "MATCH (a {n:1}) RETURN a", "MATCH (a) RETURN *,a", "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) RETURN a",
        ] {
            let mut calls = 0;
            assert!(PreparedGraphText::prepare(text, |kind, name| { calls += 1; symbols(kind, name) }).is_err(), "{text}");
            assert_eq!(calls, 0, "failed syntax called the catalog: {text}");
        }
    }

    #[test]
    fn lexical_and_structural_limits_refuse_before_unbounded_preparation() {
        let oversized = " ".repeat(MAX_GRAPH_TEXT_BYTES + 1);
        assert_eq!(PreparedGraphText::prepare(&oversized, symbols).unwrap_err().kind, GraphPatternTextErrorKind::DefinitionTooLarge);
        let long_name = format!("MATCH ({}) RETURN *", "x".repeat(MAX_PATTERN_NAME_BYTES + 1));
        assert_eq!(PreparedGraphText::prepare(&long_name, symbols).unwrap_err().kind, GraphPatternTextErrorKind::NameTooLong);
        let tokens = format!("MATCH {}(n) RETURN n", "(n),".repeat(MAX_GRAPH_TEXT_TOKENS / 3));
        assert_eq!(PreparedGraphText::prepare(&tokens, symbols).unwrap_err().kind, GraphPatternTextErrorKind::TooManyTokens);
        let mut longest = "MATCH (n0)".to_owned();
        for at in 1..=MAX_PATTERN_EDGES { longest.push_str(&format!("-[:R]->(n{at})")); }
        let valid = query(&format!("{longest} RETURN * LIMIT 0"));
        assert_eq!(valid.columns().len(), MAX_PATTERN_VERTICES);
        longest.push_str("-[:R]->(overflow) RETURN *");
        assert!(matches!(PreparedGraphText::prepare(&longest, symbols).unwrap_err().kind,
            GraphPatternTextErrorKind::Build(PatternBuildError::LimitExceeded { .. })));
        let disconnected = PreparedGraphText::prepare("MATCH (a)-[:R]->(b),(c)-[:S]->(d) RETURN *", symbols).unwrap_err();
        assert_eq!(disconnected.kind, GraphPatternTextErrorKind::Build(PatternBuildError::Disconnected));
    }

    #[test]
    fn property_nulls_and_duplicate_pagination_use_existing_value_semantics() {
        let null = CanonicalScalar::Null;
        let value = CanonicalScalar::Int(7);
        let edges = [(VId(1), RelationId(1), VId(2)), (VId(1), RelationId(1), VId(3)), (VId(1), RelationId(1), VId(4))];
        let run = |tail: &str| query(&format!("MATCH (a)-[:R]->(b) RETURN {tail}")).plan()
            .execute_governed_with_properties(3, [], edges, |_, _| Ok::<_, ()>(true),
                |vid, _| Ok(match vid { VId(2) => None, VId(3) => Some(&null), _ => Some(&value) }), policy(), || Ok::<_, ()>(())).unwrap().value;
        assert_eq!(run("b.n"), run("ALL b.n"));
        assert_eq!(run("b.n").len(), 3);
        assert_eq!(run("DISTINCT b.n").len(), 2);
        assert!(run("b.n SKIP 1 LIMIT 1")[0].get(0).unwrap().is_null());
        assert_eq!(run("b.n SKIP 2 LIMIT 1")[0].get(0).unwrap().as_scalar(), Some(&value));
        assert!(run("b.n LIMIT 0").is_empty());
        assert_eq!(query("match (a)-[:R]->(b) return b.n as value").columns(), &["value"]);
        assert_eq!(query("MATCH (a)-[:R]->(b) RETURN b.n").columns(), &["n"]);
        assert_eq!(PreparedGraphText::prepare("MATCH (a)-[:R]->(b) RETURN a.n,b.n", symbols).unwrap_err().kind,
            GraphPatternTextErrorKind::Build(PatternBuildError::DuplicateProjection));
    }

    #[test]
    fn error_messages_are_redacted_and_symbol_domains_cannot_be_confused() {
        let secret = "MATCH (private_name:SecretLabel) RETURN private_name";
        let failed = PreparedGraphText::prepare(secret, |_, _| None).unwrap_err();
        assert_eq!(failed.kind, GraphPatternTextErrorKind::UnknownSymbol(GraphSymbolKind::Label));
        assert_eq!(failed.offset, secret.find("SecretLabel").unwrap());
        assert!(!format!("{failed:?} {failed}").contains("SecretLabel"));
        let wrong = PreparedGraphText::prepare(secret, |_, _| Some(GraphSymbol::Relation(RelationId(999)))).unwrap_err();
        assert!(matches!(wrong.kind, GraphPatternTextErrorKind::WrongSymbolKind { .. }));
        let prepared = PreparedGraphText::prepare("MATCH (secret_name) RETURN secret_name", symbols).unwrap();
        assert!(!format!("{prepared:?}").contains("secret_name"));
        assert!(!format!("{:?}", GraphSymbol::Relation(RelationId(999))).contains("999"));
    }

    #[test]
    fn text_queries_share_all_policy_dimensions_and_every_interruption_checkpoint() {
        let pattern = query("MATCH (a)-[:R]->(b)-[:S]->(c) RETURN ALL a,c.n AS score");
        let edges = [(VId(1), RelationId(1), VId(2)), (VId(1), RelationId(1), VId(2)),
            (VId(2), RelationId(2), VId(3)), (VId(2), RelationId(2), VId(4))];
        let scalar = CanonicalScalar::Int(7);
        let run = |cap| pattern.plan().execute_governed_with_properties(4, [], edges, |_, _| Ok::<_, ()>(true),
            |_, _| Ok(Some(&scalar)), cap, || Ok::<_, ()>(()));
        let measured = run(policy()).unwrap();
        let exact = GqlQueryPolicy::new(4, 4, measured.evaluator.work_units, measured.evaluator.scratch_entries);
        assert_eq!(run(exact).unwrap(), measured);
        assert!(matches!(run(GqlQueryPolicy::new(3, 4, 100_000, 100_000)), Err(GqlQueryError::Rows(_))));
        assert!(matches!(run(GqlQueryPolicy::new(4, 3, 100_000, 100_000)), Err(GqlQueryError::Rows(_))));
        assert!(matches!(run(GqlQueryPolicy::new(4, 4, exact.evaluator.max_work_units - 1, 100_000)), Err(GqlQueryError::Evaluator(_))));
        assert!(matches!(run(GqlQueryPolicy::new(4, 4, 100_000, exact.evaluator.max_scratch_entries - 1)), Err(GqlQueryError::Evaluator(_))));
        let mut calls = 0;
        pattern.plan().execute_governed_with_properties(4, [], edges, |_, _| Ok::<_, ()>(true), |_, _| Ok(Some(&scalar)), policy(),
            || { calls += 1; Ok::<_, usize>(()) }).unwrap();
        for stop in 1..=calls {
            let mut at = 0;
            let result = pattern.plan().execute_governed_with_properties(4, [], edges, |_, _| Ok::<_, ()>(true), |_, _| Ok(Some(&scalar)), policy(),
                || { at += 1; if at == stop { Err(stop) } else { Ok(()) } });
            assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
            assert_eq!(at, stop);
        }
    }

    #[test]
    fn lexical_boundaries_unicode_space_and_repeated_nodes_preserve_identity() {
        let source = "\u{2003}match (n:L:L)-[:R]->(m:L)-[:S]->(n) return distinct *";
        assert_eq!(query(source).columns(), &["n", "m"]);
        // Every UTF-8 prefix must be parsed or refused without invalid slicing.
        for at in (0..=source.len()).filter(|at| source.is_char_boundary(*at)) {
            let _ = PreparedGraphText::prepare(&source[..at], symbols);
        }
        for text in ["MATCHER (a) RETURN a", "MATCH (a) RETURN a LIMITER 1", "MATCH (a) WHERE a.n=1foo RETURN a"] {
            assert!(PreparedGraphText::prepare(text, symbols).is_err());
        }
        assert!(PreparedGraphText::prepare("MATCH (n:L) WHERE n.n=-9223372036854775808 RETURN n LIMIT 18446744073709551615", symbols).is_ok());
        assert_eq!(query("MATCH (a) RETURN a AS x,a AS y").columns(), &["x", "y"]);
    }
}
