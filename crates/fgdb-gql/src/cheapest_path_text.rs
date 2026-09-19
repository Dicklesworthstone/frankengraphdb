//! An anchored, finite weighted-path text profile over the existing PathFind.
//!
//! The shared graph-text lexer owns lexical admission. This module only binds
//! the relation, cost property, mode, selector and finite hop interval. It does
//! not interpret a graph, interpolate arguments, or execute a captured WALK bag.

use crate::algebra::GlaDirection;
use crate::set_text::{TextKind, TextToken};
use crate::{
    GqlParameterSpec, GqlParameterType, GqlParameterValue, GqlParameters, GraphCheapestPathMode,
    GraphPatternTextError, GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind,
    GraphSymbolResolver, GraphWalkBounds, MAX_GRAPH_WALK_HOPS, PreparedGraphCheapestPath,
    PreparedGraphText,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::VId;

/// Diagnostics retain original byte positions, never names, IDs or values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphCheapestPathTextError {
    pub offset: usize,
    pub kind: GraphCheapestPathTextErrorKind,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphCheapestPathTextErrorKind {
    Pattern(GraphPatternTextErrorKind),
    InvalidHopBounds,
    /// The same endpoint variable was bound to two different vertex identities.
    AnchorMismatch,
}
impl core::fmt::Display for GraphCheapestPathTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "weighted path text error at byte {}: {:?}",
            self.offset, self.kind
        )
    }
}
impl core::error::Error for GraphCheapestPathTextError {}
impl From<GraphPatternTextError> for GraphCheapestPathTextError {
    fn from(error: GraphPatternTextError) -> Self {
        fail(
            error.offset,
            GraphCheapestPathTextErrorKind::Pattern(error.kind),
        )
    }
}
fn fail(offset: usize, kind: GraphCheapestPathTextErrorKind) -> GraphCheapestPathTextError {
    GraphCheapestPathTextError { offset, kind }
}
fn pattern(offset: usize, kind: GraphPatternTextErrorKind) -> GraphCheapestPathTextError {
    fail(offset, GraphCheapestPathTextErrorKind::Pattern(kind))
}
fn expected(offset: usize, what: &'static str) -> GraphCheapestPathTextError {
    pattern(offset, GraphPatternTextErrorKind::Expected(what))
}

#[derive(Clone, Copy)]
enum Operand {
    Literal(u64),
    Parameter(usize),
}
#[derive(Clone)]
struct Number {
    operand: Operand,
    at: usize,
}
impl Number {
    fn literal(&self) -> Option<u64> {
        match self.operand {
            Operand::Literal(value) => Some(value),
            Operand::Parameter(_) => None,
        }
    }
    fn bind(&self, values: &[u64]) -> u64 {
        match self.operand {
            Operand::Literal(value) => value,
            Operand::Parameter(at) => values[at],
        }
    }
}

/// A resolved template for one anchored weighted relationship expansion.
///
/// ```text
/// MATCH p = ANY CHEAPEST WALK (s)-[e:ROAD*1..8]->(t)
/// COST e.distance RETURN p
///
/// MATCH p = CHEAPEST $k TRAIL (s)-[e:ROAD*$min..$max]->(t)
/// COST e.distance RETURN p AS route
/// ```
///
/// WALK (the default), TRAIL, ACYCLIC and SIMPLE use the existing exact signed
/// Int64-cost pathfinder. `CHEAPEST n` requests a ranked prefix; `ANY CHEAPEST`
/// selects one minimum using the existing single-answer specialization. Both
/// break cost ties by canonical edge/vertex path order. Zero K is legal and is
/// NOT a source-validation bypass. A single `*n` is an exact-length interval.
///
/// This is an explicit bounded extension, not full ISO GQL. COST accepts only
/// the declared edge's property. No filters, extra patterns, cost expressions,
/// ALL CHEAPEST, result sorting, or generic pagination are silently accepted.
/// RETURN exposes a named path; the typed GraphCostPath row also carries its
/// exact i128 cost. It is not coerced into a narrower scalar projection.
///
/// `bind` takes two typed VIds in endpoint declaration order. Identities are
/// never decimal UInt64 arguments, so all 128 bits survive. Numeric controls
/// use the existing exact-name GqlParameters map, with no text substitution.
/// Binding the same endpoint variable twice requires identical identities.
#[derive(Clone)]
pub struct PreparedGraphCheapestPathText {
    relation: RelationId,
    weight: PropertyKeyId,
    direction: GlaDirection,
    mode: GraphCheapestPathMode,
    count: Option<Number>,
    minimum: Number,
    maximum: Number,
    parameters: Vec<GqlParameterSpec>,
    parameter_offsets: Vec<usize>,
    source: String,
    target: String,
    target_at: usize,
    column: String,
    return_at: usize,
}
impl PreparedGraphCheapestPathText {
    /// Parse and structurally validate the complete statement before the first
    /// catalog call. Each of the two symbols is resolved once in its own domain;
    /// subsequent binds do not consult a catalog or retain a resolver.
    pub fn prepare(
        statement: &str,
        mut resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphCheapestPathTextError> {
        let mut parser = Parser {
            tokens: PreparedGraphText::composition_tokens(statement, &[])?,
            at: 0,
            parameters: Vec::new(),
            parameter_offsets: Vec::new(),
        };
        parser.word("MATCH")?;
        let path = parser.name()?;
        parser.punct(b'=', "=")?;
        let count = if parser.take_word("ANY") {
            parser.word("CHEAPEST")?;
            None
        } else {
            parser.word("CHEAPEST")?;
            Some(parser.number()?)
        };
        let mode = if parser.take_word("TRAIL") {
            GraphCheapestPathMode::Trail
        } else if parser.take_word("ACYCLIC") {
            GraphCheapestPathMode::Acyclic
        } else if parser.take_word("SIMPLE") {
            GraphCheapestPathMode::Simple
        } else {
            parser.take_word("WALK");
            GraphCheapestPathMode::Walk
        };
        parser.punct(b'(', "(")?;
        let source = parser.name()?;
        parser.punct(b')', ")")?;
        let reverse = parser.take(b'<');
        parser.punct(b'-', "-")?;
        parser.punct(b'[', "[")?;
        let edge = parser.name()?;
        parser.punct(b':', ":")?;
        let relation = parser.catalog_name()?;
        parser.punct(b'*', "finite hop interval")?;
        let minimum = parser.number()?;
        let maximum = if parser.take(b'.') {
            parser.punct(b'.', "..")?;
            parser.number()?
        } else {
            minimum.clone()
        };
        parser.punct(b']', "]")?;
        parser.punct(b'-', "-")?;
        let forward = parser.take(b'>');
        if reverse && forward {
            return Err(expected(parser.offset(), "one traversal direction"));
        }
        let direction = if reverse {
            GlaDirection::Reverse
        } else if forward {
            GlaDirection::Forward
        } else {
            GlaDirection::Undirected
        };
        parser.punct(b'(', "(")?;
        let target = parser.name()?;
        parser.punct(b')', ")")?;
        for name in [source, edge, target] {
            if name.0 == path.0 {
                return Err(expected(name.1, "distinct path and element variables"));
            }
        }
        if edge.0 == source.0 || edge.0 == target.0 {
            return Err(expected(edge.1, "distinct edge and vertex variables"));
        }
        parser.word("COST")?;
        let cost_edge = parser.name()?;
        if cost_edge.0 != edge.0 {
            return Err(pattern(
                cost_edge.1,
                GraphPatternTextErrorKind::UnknownVariable,
            ));
        }
        parser.punct(b'.', ".")?;
        let weight = parser.catalog_name()?;
        parser.word("RETURN")?;
        let returned = parser.name()?;
        if returned.0 != path.0 {
            return Err(pattern(
                returned.1,
                GraphPatternTextErrorKind::UnknownVariable,
            ));
        }
        let column = if parser.take_word("AS") {
            parser.name()?
        } else {
            returned
        };
        if !matches!(&parser.current().kind, TextKind::End) {
            return Err(expected(parser.offset(), "end of weighted path statement"));
        }
        // Impossible literals refuse before catalog access, even alongside
        // dynamic bounds. Dynamic pairs are checked again at every bind.
        for number in [&minimum, &maximum] {
            if number
                .literal()
                .is_some_and(|n| n > u64::from(MAX_GRAPH_WALK_HOPS))
            {
                return Err(fail(
                    number.at,
                    GraphCheapestPathTextErrorKind::InvalidHopBounds,
                ));
            }
        }
        if let (Some(lo), Some(hi)) = (minimum.literal(), maximum.literal()) {
            bounds(lo, hi, maximum.at)?;
        }
        let GraphSymbol::Relation(relation_id) =
            symbol(&mut resolve, GraphSymbolKind::Relation, relation)?
        else {
            unreachable!("checked relation domain")
        };
        let GraphSymbol::Property(weight_id) =
            symbol(&mut resolve, GraphSymbolKind::Property, weight)?
        else {
            unreachable!("checked property domain")
        };
        Ok(Self {
            relation: relation_id,
            weight: weight_id,
            direction,
            mode,
            count,
            minimum,
            maximum,
            parameters: parser.parameters,
            parameter_offsets: parser.parameter_offsets,
            source: source.0.to_owned(),
            target: target.0.to_owned(),
            target_at: target.1,
            column: column.0.to_owned(),
            return_at: returned.1,
        })
    }

    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        &self.parameters
    }
    #[must_use]
    pub fn source_variable(&self) -> &str {
        &self.source
    }
    #[must_use]
    pub fn target_variable(&self) -> &str {
        &self.target
    }
    #[must_use]
    pub fn column_name(&self) -> &str {
        &self.column
    }

    /// Bind immutable identities and strictly typed numeric controls. No graph
    /// read occurs here. Every missing, mistyped or unexpected argument refuses
    /// before an executable definition is released, including when K is zero.
    pub fn bind(
        &self,
        source: VId,
        target: VId,
        arguments: &GqlParameters,
    ) -> Result<BoundGraphCheapestPathQuery, GraphCheapestPathTextError> {
        let mut values = Vec::new();
        for (spec, &at) in self.parameters.iter().zip(&self.parameter_offsets) {
            match arguments.get(&spec.name) {
                Some(GqlParameterValue::UInt64(value)) => values.push(value),
                Some(value) => {
                    return Err(pattern(
                        at,
                        GraphPatternTextErrorKind::ParameterTypeMismatch {
                            expected: GqlParameterType::UInt64,
                            found: value.parameter_type(),
                        },
                    ));
                }
                None => return Err(pattern(at, GraphPatternTextErrorKind::MissingParameter)),
            }
        }
        if arguments.len() != self.parameters.len() {
            return Err(pattern(
                self.return_at,
                GraphPatternTextErrorKind::UnexpectedArguments,
            ));
        }
        if self.source == self.target && source != target {
            return Err(fail(
                self.target_at,
                GraphCheapestPathTextErrorKind::AnchorMismatch,
            ));
        }
        let bounds = bounds(
            self.minimum.bind(&values),
            self.maximum.bind(&values),
            self.maximum.at,
        )?;
        let query = PreparedGraphCheapestPath::new(
            source,
            target,
            self.relation,
            self.direction,
            self.weight,
            bounds,
        )
        .map_err(|kind| pattern(self.return_at, GraphPatternTextErrorKind::Build(kind)))?
        .with_mode(self.mode);
        Ok(BoundGraphCheapestPathQuery {
            query,
            count: self.count.as_ref().map(|n| n.bind(&values)),
            column: self.column.clone(),
        })
    }
}

/// A fully bound executable request; no query text or mutable catalog remains.
/// Database hosts can delegate directly to the existing single/K entrypoints.
#[derive(Clone)]
pub struct BoundGraphCheapestPathQuery {
    query: PreparedGraphCheapestPath,
    count: Option<u64>,
    column: String,
}
impl BoundGraphCheapestPathQuery {
    #[must_use]
    pub fn query(&self) -> &PreparedGraphCheapestPath {
        &self.query
    }
    /// None selects ANY CHEAPEST; Some(k) selects a K-ranked prefix, including 0.
    #[must_use]
    pub const fn ranked_count(&self) -> Option<u64> {
        self.count
    }
    #[must_use]
    pub fn column_name(&self) -> &str {
        &self.column
    }

    /// Explicit plaintext application identity, not a durable format or an
    /// authorization certificate. Pin selection and output name as well as all
    /// native anchors, relation, weight, orientation, mode and finite bounds.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:weighted-path-text:bound:v1\0".to_vec();
        match self.count {
            None => bytes.push(0),
            Some(count) => {
                bytes.push(1);
                bytes.extend_from_slice(&count.to_be_bytes());
            }
        }
        let query = self.query.canonical_bytes();
        for part in [query.as_slice(), self.column.as_bytes()] {
            bytes.extend_from_slice(&(part.len() as u64).to_be_bytes());
            bytes.extend_from_slice(part);
        }
        bytes
    }
}
impl core::fmt::Debug for PreparedGraphCheapestPathText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphCheapestPathText")
            .field("parameter_count", &self.parameters.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl core::fmt::Debug for BoundGraphCheapestPathQuery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("BoundGraphCheapestPathQuery([REDACTED])")
    }
}

fn bounds(lo: u64, hi: u64, at: usize) -> Result<GraphWalkBounds, GraphCheapestPathTextError> {
    // Deliberately do not embed argument values in the public bounds error.
    let invalid = || fail(at, GraphCheapestPathTextErrorKind::InvalidHopBounds);
    let lo = u32::try_from(lo).map_err(|_| invalid())?;
    let hi = u32::try_from(hi).map_err(|_| invalid())?;
    GraphWalkBounds::new(lo, hi).map_err(|_| invalid())
}
fn symbol(
    resolve: &mut impl GraphSymbolResolver,
    kind: GraphSymbolKind,
    name: (&str, usize),
) -> Result<GraphSymbol, GraphCheapestPathTextError> {
    let value = resolve
        .resolve_symbol(kind, name.0)
        .ok_or_else(|| pattern(name.1, GraphPatternTextErrorKind::UnknownSymbol(kind)))?;
    if value.kind() != kind {
        return Err(pattern(
            name.1,
            GraphPatternTextErrorKind::WrongSymbolKind {
                expected: kind,
                found: value.kind(),
            },
        ));
    }
    Ok(value)
}

struct Parser<'a> {
    tokens: Vec<TextToken<'a>>,
    at: usize,
    parameters: Vec<GqlParameterSpec>,
    parameter_offsets: Vec<usize>,
}
impl<'a> Parser<'a> {
    fn current(&self) -> &TextToken<'a> {
        &self.tokens[self.at]
    }
    fn offset(&self) -> usize {
        self.current().at
    }
    fn take_word(&mut self, word: &str) -> bool {
        if matches!(&self.current().kind, TextKind::Word(actual) if actual.eq_ignore_ascii_case(word))
        {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn word(&mut self, word: &'static str) -> Result<(), GraphCheapestPathTextError> {
        if self.take_word(word) {
            Ok(())
        } else {
            Err(expected(self.offset(), word))
        }
    }
    fn take(&mut self, ch: u8) -> bool {
        if matches!(&self.current().kind, TextKind::Punct(actual) if *actual == ch) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn punct(&mut self, ch: u8, what: &'static str) -> Result<(), GraphCheapestPathTextError> {
        if self.take(ch) {
            Ok(())
        } else {
            Err(expected(self.offset(), what))
        }
    }
    // Catalog symbols occur after ':' or '.', so keywords such as the common
    // property name "cost" are unambiguous. Variable names remain restricted.
    fn catalog_name(&mut self) -> Result<(&'a str, usize), GraphCheapestPathTextError> {
        let at = self.offset();
        let TextKind::Word(text) = &self.current().kind else {
            return Err(expected(at, "catalog identifier"));
        };
        let text = *text;
        self.at += 1;
        Ok((text, at))
    }
    fn name(&mut self) -> Result<(&'a str, usize), GraphCheapestPathTextError> {
        let at = self.offset();
        let TextKind::Word(text) = &self.current().kind else {
            return Err(expected(at, "identifier"));
        };
        let text = *text;
        if [
            "MATCH", "ANY", "CHEAPEST", "WALK", "TRAIL", "ACYCLIC", "SIMPLE", "COST", "RETURN",
            "AS", "WHERE", "ALL", "DISTINCT", "AND", "OR", "SKIP", "LIMIT", "OPTIONAL", "ORDER",
            "BY",
        ]
        .iter()
        .any(|word| text.eq_ignore_ascii_case(word))
            || text.starts_with("__fgdb_anonymous_")
        {
            return Err(expected(at, "non-keyword identifier"));
        }
        self.at += 1;
        Ok((text, at))
    }
    fn number(&mut self) -> Result<Number, GraphCheapestPathTextError> {
        let at = self.offset();
        let operand = match &self.current().kind {
            TextKind::Digits(digits) => Operand::Literal(
                digits
                    .parse::<u64>()
                    .map_err(|_| pattern(at, GraphPatternTextErrorKind::IntegerOutOfRange))?,
            ),
            TextKind::Parameter(name) => {
                let name = *name;
                let index = if let Some(index) = self.parameters.iter().position(|p| p.name == name)
                {
                    self.parameters[index].occurrences += 1;
                    index
                } else {
                    let index = self.parameters.len();
                    self.parameters.push(GqlParameterSpec {
                        name: name.to_owned(),
                        parameter_type: GqlParameterType::UInt64,
                        requires_positive: false,
                        occurrences: 1,
                    });
                    self.parameter_offsets.push(at);
                    index
                };
                Operand::Parameter(index)
            }
            _ => return Err(expected(at, "unsigned integer or parameter")),
        };
        self.at += 1;
        Ok(Number { operand, at })
    }
}

#[cfg(test)]
mod tests;
