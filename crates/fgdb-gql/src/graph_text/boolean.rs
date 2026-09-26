//! Boolean WHERE preparation through the shared lexer, schema and GLA compiler.
//! The parser emits bounded postfix instructions, never a second graph matcher.
//! Flat conjunctions retain their existing lowering. Extended expressions use
//! eager three-valued evaluation and may span several bound vertices. EXISTS
//! remains a scoped clause, not a Boolean leaf in this deliberately bounded
//! profile; unsupported mixtures are rejected before catalog resolution.
//! Property IN/NOT IN lists and BETWEEN/NOT BETWEEN ranges lower to the same
//! checked comparisons, preserving UNKNOWN, typed arguments and source errors.

use super::*;
use crate::algebra::{
    GraphBooleanExpression, GraphBooleanOp as Op, GraphBooleanOperand as Operand,
    MAX_BOOLEAN_INSTRUCTIONS, ScalarPredicate,
};
use fgdb_types::CanonicalScalar;

const MAX_BOOLEAN_NESTING: usize = 64;

pub(super) enum SyntaxItem<'a> {
    Atom(Filter<'a>),
    Expression {
        columns: Vec<(Name<'a>, Name<'a>)>,
        program: Vec<crate::mutation_text::MutationIntegerTemplateOp>,
    },
    Truth(Option<bool>),
    And,
    Or,
    Not,
}

struct Parsed<'a> {
    program: Vec<SyntaxItem<'a>>,
    extended: bool,
}
impl<'a> Parsed<'a> {
    fn push(&mut self, item: SyntaxItem<'a>, at: usize) -> Result<(), GraphPatternTextError> {
        if self.program.len() >= MAX_BOOLEAN_INSTRUCTIONS {
            return Err(error(at, GraphPatternTextErrorKind::BooleanExpression));
        }
        self.program.push(item);
        Ok(())
    }
}

impl<'a> Parser<'a> {
    pub(super) fn boolean_predicates(&mut self) -> Result<bool, GraphPatternTextError> {
        let at = self.current.at;
        let mut parsed = Parsed {
            program: Vec::new(),
            extended: false,
        };
        self.boolean_or(0, &mut parsed)?;
        if !parsed.extended {
            // This is the old AND-only profile, not a migration to a different
            // predicate order, cache or transcript merely because syntax grew.
            for item in parsed.program {
                match item {
                    SyntaxItem::Atom(filter) => self.syntax.filters.push(filter),
                    SyntaxItem::And => {}
                    _ => unreachable!("extended syntax sets its marker during parsing"),
                }
            }
            return Ok(false);
        }
        // Identity atoms in a compound expression are ordinary Boolean leaves,
        // not unconditional structural constraints. Move their admission count
        // into the common predicate budget without charging any atom twice.
        let identities = parsed
            .program
            .iter()
            .filter(|item| matches!(item, SyntaxItem::Atom(Filter::Identity { .. })))
            .count();
        let predicates = self.predicates.saturating_add(identities);
        if predicates > MAX_PATTERN_PREDICATES {
            return Err(error(
                at,
                GraphPatternTextErrorKind::Build(PatternBuildError::LimitExceeded {
                    dimension: crate::algebra::PatternLimitDimension::Predicates,
                    limit: MAX_PATTERN_PREDICATES,
                    observed: predicates,
                }),
            ));
        }
        self.identities -= identities;
        self.predicates = predicates;
        self.syntax.filters.push(Filter::Boolean {
            program: parsed.program,
            at,
        });
        Ok(true)
    }

    fn boolean_or(
        &mut self,
        depth: usize,
        parsed: &mut Parsed<'a>,
    ) -> Result<(), GraphPatternTextError> {
        self.boolean_and(depth, parsed)?;
        while self.is_word("OR") {
            let at = self.current.at;
            self.advance()?;
            parsed.extended = true;
            self.boolean_and(depth, parsed)?;
            parsed.push(SyntaxItem::Or, at)?;
        }
        Ok(())
    }

    fn boolean_and(
        &mut self,
        depth: usize,
        parsed: &mut Parsed<'a>,
    ) -> Result<(), GraphPatternTextError> {
        self.boolean_unary(depth, parsed)?;
        while self.is_word("AND") {
            // Leave the delimiter itself to the existing scoped clause parser.
            // A compound expression plus a scope is refused by that owner; it
            // is never silently rearranged into (A OR B) AND EXISTS.
            if depth == 0 && self.and_starts_existence()? {
                break;
            }
            let at = self.current.at;
            self.advance()?;
            self.boolean_unary(depth, parsed)?;
            parsed.push(SyntaxItem::And, at)?;
        }
        Ok(())
    }

    fn and_starts_existence(&self) -> Result<bool, GraphPatternTextError> {
        let mut lexer = self.lexer.clone();
        let mut token = lexer.next()?;
        if matches!(token.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("NOT")) {
            token = lexer.next()?;
        }
        if !matches!(token.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("EXISTS")) {
            return super::scoped::pattern_predicate_follows(token.kind, &mut lexer);
        }
        Ok(matches!(lexer.next()?.kind, TokenKind::Punct(b'{')))
    }

    // Preserve preexisting variables called not/true/false/null when followed
    // by an atom's identifier syntax. Quoted strings are already one token and
    // cannot introduce operators, parentheses or scopes through this lookahead.
    fn boolean_word_is_variable(&self) -> Result<bool, GraphPatternTextError> {
        let token = self.lexer.clone().next()?;
        Ok(matches!(
            token.kind,
            TokenKind::Punct(b'.' | b'=' | b'!' | b'<' | b'>')
        ) || matches!(token.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("IS")))
    }

    fn boolean_unary(
        &mut self,
        depth: usize,
        parsed: &mut Parsed<'a>,
    ) -> Result<(), GraphPatternTextError> {
        if depth > MAX_BOOLEAN_NESTING {
            return Err(error(
                self.current.at,
                GraphPatternTextErrorKind::BooleanNesting {
                    limit: MAX_BOOLEAN_NESTING,
                },
            ));
        }
        if self.starts_existence()? {
            return Err(error(
                self.current.at,
                GraphPatternTextErrorKind::UnsupportedBooleanScope,
            ));
        }
        let at = self.current.at;
        if self.starts_scalar_predicate()? {
            self.admit_compound_leaf()?;
            let (columns, program) = self.boolean_scalar_expression()?;
            parsed.extended = true;
            return parsed.push(SyntaxItem::Expression { columns, program }, at);
        }
        if self.is_word("NOT") && !self.boolean_word_is_variable()? {
            self.advance()?;
            parsed.extended = true;
            self.boolean_unary(depth + 1, parsed)?;
            return parsed.push(SyntaxItem::Not, at);
        }
        // A bare literal left operand with an IN/NOT IN list takes the shared
        // scalar path, so three-valued membership keeps its unknown result
        // instead of collapsing into a Boolean Truth leaf. Property-left lists
        // retain their pinned OR-of-equality lowering below.
        if (self.is_word("TRUE")
            || self.is_word("FALSE")
            || self.is_word("NULL")
            || matches!(self.current.kind, TokenKind::Quoted(_)))
            && !self.boolean_word_is_variable()?
            && self.literal_starts_in_list()?
        {
            self.admit_compound_leaf()?;
            let (columns, program) = self.boolean_scalar_expression()?;
            parsed.extended = true;
            return parsed.push(SyntaxItem::Expression { columns, program }, at);
        }
        if self.take(b'(')? {
            parsed.extended = true;
            self.boolean_or(depth + 1, parsed)?;
            self.punct(b')', ")")?;
            return Ok(());
        }
        if (self.is_word("TRUE") || self.is_word("FALSE") || self.is_word("NULL"))
            && !self.boolean_word_is_variable()?
        {
            self.capacity(
                self.predicates,
                MAX_PATTERN_PREDICATES,
                crate::algebra::PatternLimitDimension::Predicates,
            )?;
            let value = if self.is_word("NULL") {
                None
            } else {
                Some(self.is_word("TRUE"))
            };
            self.advance()?;
            self.predicates += 1;
            parsed.extended = true;
            return parsed.push(SyntaxItem::Truth(value), at);
        }
        if self.compound_property_predicate(parsed)? {
            return Ok(());
        }
        self.positive_predicate()?;
        let filter = self
            .syntax
            .filters
            .pop()
            .expect("one positive predicate was just parsed");
        parsed.extended |= matches!(filter, Filter::VertexNull { .. });
        parsed.push(SyntaxItem::Atom(filter), at)
    }

    fn literal_starts_in_list(&self) -> Result<bool, GraphPatternTextError> {
        let mut lexer = self.lexer.clone();
        let mut token = lexer.next()?;
        if matches!(token.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("NOT")) {
            token = lexer.next()?;
        }
        Ok(matches!(token.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("IN")))
    }

    fn starts_scalar_predicate(&self) -> Result<bool, GraphPatternTextError> {
        let mut lexer = self.lexer.clone();
        let mut token = self.current;
        let mut depth = 0_usize;
        let mut arithmetic = false;
        let mut operand_end = false;
        let mut after_dot = false;
        let property_compound = self.starts_compound_property_predicate()?;
        loop {
            match token.kind {
                TokenKind::End | TokenKind::Punct(b'{' | b'}') => break,
                TokenKind::Punct(b'(') => depth += 1,
                TokenKind::Punct(b')') if depth == 0 => break,
                TokenKind::Punct(b')') => depth -= 1,
                TokenKind::Punct(b'|') => return Ok(true),
                TokenKind::Punct(b'+' | b'*' | b'/' | b'%') => arithmetic = true,
                TokenKind::Punct(b'-') => {
                    // A signed numeric RHS keeps the original typed comparison
                    // (including i64::MIN). Binary subtraction and nonliteral
                    // unary negation require the existing scalar compiler.
                    if operand_end
                        || token.at == self.current.at
                        || !matches!(lexer.clone().next()?.kind, TokenKind::Digits(_))
                    {
                        arithmetic = true;
                    }
                }
                TokenKind::Word(word) => {
                    if !after_dot
                        && super::SCALAR_FUNCTIONS
                            .iter()
                            .any(|keyword| word.eq_ignore_ascii_case(keyword))
                        && matches!(lexer.clone().next()?.kind, TokenKind::Punct(b'('))
                    {
                        return Ok(true);
                    }
                    if !after_dot
                        && operand_end
                        && ["STARTS", "ENDS", "CONTAINS"]
                            .iter()
                            .any(|keyword| word.eq_ignore_ascii_case(keyword))
                    {
                        return Ok(true);
                    }
                    if depth == 0
                        && !after_dot
                        && [
                            "AND", "OR", "RETURN", "SET", "REMOVE", "WITH", "MATCH", "OPTIONAL",
                        ]
                        .iter()
                        .any(|keyword| word.eq_ignore_ascii_case(keyword))
                    {
                        break;
                    }
                    // Ordinary property membership retains comparison lowering
                    // and its per-member admission. Continue scanning so an
                    // actual computed list operand still selects scalar IR.
                    // Other left operands (including literals) need scalar IN.
                    // NOT is scanned normally, never by consuming an extra token.
                    if depth == 0
                        && !after_dot
                        && !property_compound
                        && word.eq_ignore_ascii_case("IN")
                    {
                        return Ok(true);
                    }
                }
                _ => {}
            }
            operand_end = match token.kind {
                TokenKind::Digits(_)
                | TokenKind::Parameter(_)
                | TokenKind::Quoted(_)
                | TokenKind::Punct(b')' | b']') => true,
                TokenKind::Word(word) => {
                    after_dot
                        || !["NOT", "IN", "BETWEEN", "IS", "AND", "OR"]
                            .iter()
                            .any(|keyword| word.eq_ignore_ascii_case(keyword))
                }
                _ => false,
            };
            after_dot = matches!(token.kind, TokenKind::Punct(b'.'));
            token = lexer.next()?;
        }
        Ok(arithmetic)
    }

    /// Look ahead with the existing bounded lexer. Never consume a partial
    /// ordinary comparison, and do not mistake keyword-looking names for
    /// operators. Syntax errors still precede every catalog callback.
    fn starts_compound_property_predicate(&self) -> Result<bool, GraphPatternTextError> {
        if !matches!(self.current.kind, TokenKind::Word(_)) {
            return Ok(false);
        }
        let mut lexer = self.lexer.clone();
        if !matches!(lexer.next()?.kind, TokenKind::Punct(b'.'))
            || !matches!(lexer.next()?.kind, TokenKind::Word(_))
        {
            return Ok(false);
        }
        let mut token = lexer.next()?;
        if matches!(token.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("NOT")) {
            token = lexer.next()?;
        }
        Ok(matches!(token.kind, TokenKind::Word(word)
            if word.eq_ignore_ascii_case("IN") || word.eq_ignore_ascii_case("BETWEEN")))
    }

    fn admit_compound_leaf(&mut self) -> Result<(), GraphPatternTextError> {
        self.capacity(
            self.predicates,
            MAX_PATTERN_PREDICATES,
            crate::algebra::PatternLimitDimension::Predicates,
        )?;
        self.predicates += 1;
        Ok(())
    }

    fn compound_operand(
        &mut self,
        variable: Name<'a>,
        key: Name<'a>,
        comparison: IntegerComparison,
        parsed: &mut Parsed<'a>,
    ) -> Result<(), GraphPatternTextError> {
        let at = self.current.at;
        self.admit_compound_leaf()?;
        let filter = self.property_operand(variable, key, comparison)?;
        parsed.push(SyntaxItem::Atom(filter), at)
    }

    /// IN is a left-associated OR of equality comparisons; BETWEEN is the
    /// conjunction of inclusive comparisons. Apply NOT to the complete result,
    /// never to individual operands: a NULL member must remain UNKNOWN when
    /// nothing matches. Explicit operands can be literals, typed parameters or
    /// bound properties. These are bounded list constructors, not list-valued
    /// parameters, subqueries, or a new scalar comparison implementation.
    fn compound_property_predicate(
        &mut self,
        parsed: &mut Parsed<'a>,
    ) -> Result<bool, GraphPatternTextError> {
        if !self.starts_compound_property_predicate()? {
            return Ok(false);
        }
        let at = self.current.at;
        let variable = self.property_variable()?;
        self.punct(b'.', ".")?;
        let key = self.name()?;
        let negate = self.take_word("NOT")?;
        parsed.extended = true;
        if self.take_word("IN")? {
            self.punct(b'[', "[")?;
            if self.take(b']')? {
                // Even an empty list must resolve its left property and retain
                // the eager evaluator's source-error boundary. IS NULL is
                // total; (property IS NULL AND FALSE) is always FALSE, including
                // missing/stored NULL. Negation below makes NOT IN [] TRUE.
                self.admit_compound_leaf()?;
                parsed.push(
                    SyntaxItem::Atom(Filter::Null {
                        variable,
                        key,
                        is_null: true,
                    }),
                    at,
                )?;
                self.admit_compound_leaf()?;
                parsed.push(SyntaxItem::Truth(Some(false)), at)?;
                parsed.push(SyntaxItem::And, at)?;
            } else {
                let mut first = true;
                loop {
                    self.compound_operand(variable, key, IntegerComparison::Equal, parsed)?;
                    if !first {
                        parsed.push(SyntaxItem::Or, at)?;
                    }
                    first = false;
                    if self.take(b']')? {
                        break;
                    }
                    self.punct(b',', ", or ]")?;
                }
            }
        } else {
            self.word("BETWEEN")?;
            self.compound_operand(variable, key, IntegerComparison::GreaterOrEqual, parsed)?;
            // This AND belongs to the range, not to the surrounding Boolean
            // conjunction. Parentheses, NOT and OR retain their usual binding.
            self.word("AND")?;
            self.compound_operand(variable, key, IntegerComparison::LessOrEqual, parsed)?;
            parsed.push(SyntaxItem::And, at)?;
        }
        if negate {
            parsed.push(SyntaxItem::Not, at)?;
        }
        Ok(true)
    }
}

#[derive(Clone)]
enum Atom {
    Property {
        variable: String,
        key: PropertyKeyId,
        comparison: IntegerComparison,
        value: Number,
    },
    Scalar {
        variable: String,
        key: PropertyKeyId,
        predicate: ScalarPredicate,
    },
    Null {
        variable: String,
        key: PropertyKeyId,
        is_null: bool,
    },
    VertexNull {
        variable: String,
        is_null: bool,
    },
    Identity {
        left: String,
        right: String,
        equal: bool,
    },
    Properties {
        left: String,
        left_key: PropertyKeyId,
        right: String,
        right_key: PropertyKeyId,
        comparison: IntegerComparison,
    },
}
#[derive(Clone)]
enum Item {
    Atom(Atom),
    Expression {
        columns: Vec<(String, PropertyKeyId)>,
        program: Vec<crate::mutation_text::MutationIntegerTemplateOp>,
    },
    Truth(Option<bool>),
    And,
    Or,
    Not,
}

/// Resolved immutable syntax. Only typed numeric/scalar operands vary during
/// binding; names are not resolved again and arguments never become text.
#[derive(Clone)]
pub(super) struct BoundBooleanTemplate {
    program: Vec<Item>,
    at: usize,
    edge_variables: Vec<String>,
}

impl BoundBooleanTemplate {
    /// Append the resolved unbound program: variable/property identities,
    /// comparisons and structure only. Numeric operands encode as typed
    /// literals or parameter indices through the shared helper; scalar
    /// predicates keep their canonical transcript. Values never enter.
    pub(super) fn append_template_transcript(&self, bytes: &mut Vec<u8>) {
        fn encode_atom(bytes: &mut Vec<u8>, atom: &Atom) {
            match atom {
                Atom::Property {
                    variable,
                    key,
                    comparison,
                    value,
                } => {
                    bytes.push(0);
                    append_name(bytes, variable);
                    bytes.extend_from_slice(&key.0.to_be_bytes());
                    bytes.push(comparison_tag(*comparison));
                    append_number(bytes, value);
                }
                Atom::Scalar {
                    variable,
                    key,
                    predicate,
                } => {
                    bytes.push(1);
                    append_name(bytes, variable);
                    bytes.extend_from_slice(&key.0.to_be_bytes());
                    append_scalar_predicate(bytes, predicate);
                }
                Atom::Null {
                    variable,
                    key,
                    is_null,
                } => {
                    bytes.push(2);
                    append_name(bytes, variable);
                    bytes.extend_from_slice(&key.0.to_be_bytes());
                    bytes.push(u8::from(*is_null));
                }
                Atom::VertexNull { variable, is_null } => {
                    bytes.push(3);
                    append_name(bytes, variable);
                    bytes.push(u8::from(*is_null));
                }
                Atom::Identity { left, right, equal } => {
                    bytes.push(4);
                    append_name(bytes, left);
                    append_name(bytes, right);
                    bytes.push(u8::from(*equal));
                }
                Atom::Properties {
                    left,
                    left_key,
                    right,
                    right_key,
                    comparison,
                } => {
                    bytes.push(5);
                    append_name(bytes, left);
                    bytes.extend_from_slice(&left_key.0.to_be_bytes());
                    append_name(bytes, right);
                    bytes.extend_from_slice(&right_key.0.to_be_bytes());
                    bytes.push(comparison_tag(*comparison));
                }
            }
        }
        fn encode_item(bytes: &mut Vec<u8>, item: &Item) {
            match item {
                Item::Atom(atom) => {
                    bytes.push(0);
                    encode_atom(bytes, atom);
                }
                Item::Expression { columns, program } => {
                    bytes.push(1);
                    bytes.extend_from_slice(&(columns.len() as u64).to_be_bytes());
                    for (variable, key) in columns {
                        append_name(bytes, variable);
                        bytes.extend_from_slice(&key.0.to_be_bytes());
                    }
                    bytes.extend_from_slice(&(program.len() as u64).to_be_bytes());
                    for op in program {
                        op.append_template_transcript(bytes);
                    }
                }
                Item::Truth(value) => {
                    bytes.push(2);
                    bytes.push(match value {
                        None => 0,
                        Some(false) => 1,
                        Some(true) => 2,
                    });
                }
                Item::And => bytes.push(3),
                Item::Or => bytes.push(4),
                Item::Not => bytes.push(5),
            }
        }
        bytes.extend_from_slice(&(self.program.len() as u64).to_be_bytes());
        for instruction in &self.program {
            encode_item(bytes, instruction);
        }
        if !self.edge_variables.is_empty() {
            bytes.extend_from_slice(b"edge-properties\0");
            bytes.extend_from_slice(&(self.edge_variables.len() as u64).to_be_bytes());
            for variable in &self.edge_variables {
                append_name(bytes, variable);
            }
        }
    }
}

/// Comparison tag mirroring the algebra transcript convention, shared by
/// sibling aggregate and having transcripts within this module tree.
pub(super) fn comparison_tag(comparison: IntegerComparison) -> u8 {
    match comparison {
        IntegerComparison::Equal => 0,
        IntegerComparison::NotEqual => 1,
        IntegerComparison::Less => 2,
        IntegerComparison::LessOrEqual => 3,
        IntegerComparison::Greater => 4,
        IntegerComparison::GreaterOrEqual => 5,
    }
}

/// Canonical scalar predicate identity: kind-tagged canonical value plus the
/// comparison tag, without the algebra-private transcript convention.
fn append_scalar_predicate(bytes: &mut Vec<u8>, predicate: &ScalarPredicate) {
    bytes.push(comparison_tag(predicate.comparison()));
    let value = predicate.canonical_value_bytes();
    bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
    bytes.extend_from_slice(value);
}

/// Fixed-width identifier framing over resolved, immutable variable names.
fn append_name(bytes: &mut Vec<u8>, name: &str) {
    bytes.extend_from_slice(&(name.len() as u64).to_be_bytes());
    bytes.extend_from_slice(name.as_bytes());
}

/// Shared unbound numeric operand encoding: typed literal or parameter index.
pub(super) fn append_number(bytes: &mut Vec<u8>, number: &Number) {
    match number {
        Number::Literal(GqlParameterValue::Int64(value)) => {
            bytes.push(0);
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        Number::Literal(GqlParameterValue::UInt64(value)) => {
            bytes.push(1);
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        Number::Parameter(index) => {
            bytes.push(2);
            bytes.extend_from_slice(&(*index as u64).to_be_bytes());
        }
        Number::Literal(_) => unreachable!("numeric syntax and schema agree"),
    }
}

impl BoundBooleanTemplate {
    pub(super) fn resolve<'a>(
        program: Vec<SyntaxItem<'a>>,
        at: usize,
        edges: &[Edge<'a>],
        symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
    ) -> Result<Self, GraphPatternTextError> {
        let mut property = |name| {
            let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, name)? else {
                unreachable!("the shared catalog resolver checks the symbol domain")
            };
            Ok::<_, GraphPatternTextError>(key)
        };
        let mut resolved = Vec::new();
        for instruction in program {
            resolved.push(match instruction {
                SyntaxItem::Expression { columns, program } => Item::Expression {
                    columns: columns
                        .into_iter()
                        .map(|(variable, key)| {
                            property(key).map(|key| (variable.text.to_owned(), key))
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    program,
                },
                SyntaxItem::Truth(value) => Item::Truth(value),
                SyntaxItem::And => Item::And,
                SyntaxItem::Or => Item::Or,
                SyntaxItem::Not => Item::Not,
                SyntaxItem::Atom(filter) => Item::Atom(match filter {
                    Filter::Property {
                        variable,
                        key,
                        comparison,
                        value,
                    } => Atom::Property {
                        variable: variable.text.to_owned(),
                        key: property(key)?,
                        comparison,
                        value,
                    },
                    Filter::Scalar {
                        variable,
                        key,
                        predicate,
                    } => Atom::Scalar {
                        variable: variable.text.to_owned(),
                        key: property(key)?,
                        predicate,
                    },
                    Filter::Null {
                        variable,
                        key,
                        is_null,
                    } => Atom::Null {
                        variable: variable.text.to_owned(),
                        key: property(key)?,
                        is_null,
                    },
                    Filter::VertexNull { variable, is_null } => Atom::VertexNull {
                        variable: variable.text.to_owned(),
                        is_null,
                    },
                    Filter::Identity { left, right, equal } => Atom::Identity {
                        left: left.text.to_owned(),
                        right: right.text.to_owned(),
                        equal,
                    },
                    Filter::Properties {
                        left,
                        left_key,
                        right,
                        right_key,
                        comparison,
                    } => Atom::Properties {
                        left: left.text.to_owned(),
                        left_key: property(left_key)?,
                        right: right.text.to_owned(),
                        right_key: property(right_key)?,
                        comparison,
                    },
                    Filter::Boolean { .. } => {
                        return Err(error(at, GraphPatternTextErrorKind::BooleanExpression));
                    }
                    // Path predicates bind only at the root scope (see
                    // scoped::predicate_captures); a Boolean program refuses them.
                    Filter::PathCapture(_)
                    | Filter::PathLength { .. }
                    | Filter::PathNull { .. } => {
                        return Err(error(
                            at,
                            GraphPatternTextErrorKind::Expected("root path predicate"),
                        ));
                    }
                }),
            });
        }
        let edge_variables = edges
            .iter()
            .filter_map(|edge| edge.variable)
            .filter(|name| {
                resolved.iter().any(|item| match item {
                    Item::Atom(
                        Atom::Property { variable, .. }
                        | Atom::Scalar { variable, .. }
                        | Atom::Null { variable, .. },
                    ) => variable == name.text,
                    Item::Atom(Atom::Properties { left, right, .. }) => {
                        left == name.text || right == name.text
                    }
                    Item::Expression { columns, .. } => {
                        columns.iter().any(|(variable, _)| variable == name.text)
                    }
                    _ => false,
                })
            })
            .map(|name| name.text.to_owned())
            .collect();
        Ok(Self {
            program: resolved,
            at,
            edge_variables,
        })
    }

    fn property_operand<'a>(&self, variable: &'a str, key: PropertyKeyId) -> Operand<'a> {
        if self.edge_variables.iter().any(|name| name == variable) {
            Operand::EdgeProperty { variable, key }
        } else {
            Operand::Property { variable, key }
        }
    }

    pub(super) fn bind(
        &self,
        values: &[GqlParameterValue],
    ) -> Result<GraphBooleanExpression, GraphPatternTextError> {
        let property = |variable, key| self.property_operand(variable, key);
        // Finish the operand storage before taking any references into it.
        // Canonical scalar arguments reuse their checked Arc/encoding; numeric
        // operands encode once for this bound immutable expression.
        let literals = self
            .program
            .iter()
            .map(|item| match item {
                Item::Atom(Atom::Scalar { predicate, .. }) => Ok(Some(predicate.clone())),
                Item::Atom(Atom::Property { value, .. }) => match value.value(values) {
                    GqlParameterValue::Int64(value) => {
                        ScalarPredicate::new(CanonicalScalar::Int(value), IntegerComparison::Equal)
                            .map(Some)
                            .map_err(|_| error(self.at, GraphPatternTextErrorKind::ScalarLiteral))
                    }
                    GqlParameterValue::Scalar(value) => {
                        Ok(Some(value.predicate(IntegerComparison::Equal)))
                    }
                    GqlParameterValue::UInt64(_) | GqlParameterValue::List(_) => {
                        Err(error(self.at, GraphPatternTextErrorKind::BooleanExpression))
                    }
                },
                _ => Ok(None),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let expressions = self
            .program
            .iter()
            .map(|item| match item {
                Item::Expression { program, .. } => {
                    Parser::bind_boolean_scalar(program, values, self.at).map(Some)
                }
                _ => Ok(None),
            })
            .collect::<Result<Vec<_>, GraphPatternTextError>>()?;
        let expression_columns = self
            .program
            .iter()
            .map(|item| match item {
                Item::Expression { columns, .. } => columns
                    .iter()
                    .map(|(variable, key)| property(variable, *key))
                    .collect(),
                _ => Vec::new(),
            })
            .collect::<Vec<Vec<Operand<'_>>>>();
        let mut program = Vec::new();
        for (index, (item, literal)) in self.program.iter().zip(&literals).enumerate() {
            program.push(match item {
                Item::Expression { .. } => Op::Expression {
                    expression: expressions[index]
                        .as_ref()
                        .expect("bound scalar expression"),
                    columns: &expression_columns[index],
                },
                Item::Truth(value) => Op::Truth(*value),
                Item::And => Op::And,
                Item::Or => Op::Or,
                Item::Not => Op::Not,
                Item::Atom(atom) => match atom {
                    Atom::Property {
                        variable,
                        key,
                        comparison,
                        ..
                    } => Op::Compare {
                        left: property(variable, *key),
                        comparison: *comparison,
                        right: Operand::CheckedLiteral(
                            literal.as_ref().expect("bound numeric/scalar literal"),
                        ),
                    },
                    Atom::Scalar {
                        variable,
                        key,
                        predicate,
                    } => Op::Compare {
                        left: property(variable, *key),
                        comparison: predicate.comparison(),
                        right: Operand::CheckedLiteral(
                            literal.as_ref().expect("bound scalar literal"),
                        ),
                    },
                    Atom::Null {
                        variable,
                        key,
                        is_null,
                    } => Op::IsNull {
                        operand: property(variable, *key),
                        is_null: *is_null,
                    },
                    Atom::VertexNull { variable, is_null } => Op::IsNull {
                        operand: Operand::Vertex(variable),
                        is_null: *is_null,
                    },
                    Atom::Identity { left, right, equal } => Op::Compare {
                        left: Operand::Vertex(left),
                        right: Operand::Vertex(right),
                        comparison: if *equal {
                            IntegerComparison::Equal
                        } else {
                            IntegerComparison::NotEqual
                        },
                    },
                    Atom::Properties {
                        left,
                        left_key,
                        right,
                        right_key,
                        comparison,
                    } => Op::Compare {
                        left: property(left, *left_key),
                        comparison: *comparison,
                        right: property(right, *right_key),
                    },
                },
            });
        }
        GraphBooleanExpression::prepare(&program)
            .map_err(|_| error(self.at, GraphPatternTextErrorKind::BooleanExpression))
    }
    pub(super) fn template_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:gql:boolean-text-template:v1\0".to_vec();
        self.append_template_transcript(&mut bytes);
        bytes
    }
}

/// Same spelling, different resolved meaning: the transcript follows the
/// catalog symbols and argument contract, not the source text bytes.
#[cfg(test)]
mod template_tests {
    use super::*;

    fn template(
        statement: &str,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Vec<u8> {
        boolean_template_bytes(&PreparedGraphText::prepare(statement, resolve).unwrap())
    }

    fn template_with_parameters(
        statement: &str,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Vec<u8> {
        boolean_template_bytes(
            &PreparedGraphText::prepare_with_parameter_types(statement, declarations, resolve)
                .unwrap(),
        )
    }

    /// Full resolved template via the facade accessor.
    fn boolean_template_bytes(prepared: &PreparedGraphText) -> Vec<u8> {
        prepared.template_bytes()
    }

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            _ => None,
        }
    }

    fn other_symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        }
    }

    #[test]
    fn template_tracks_resolved_symbols_and_parameter_identity() {
        let base = template("MATCH (n) WHERE n.p = 1 RETURN n", symbols);
        assert_eq!(base, template("MATCH (n) WHERE n.p = 1 RETURN n", symbols));
        assert_ne!(base, template("MATCH (n) WHERE n.q = 1 RETURN n", symbols));
        assert_ne!(base, template("MATCH (n) WHERE n.p = 2 RETURN n", symbols));
        assert_ne!(
            base,
            template("MATCH (n) WHERE n.p = 1 AND TRUE RETURN n", symbols)
        );
        assert_ne!(
            base,
            template("MATCH (n) WHERE n.p = 1 OR TRUE RETURN n", symbols)
        );
        assert_ne!(
            base,
            template("MATCH (n) WHERE n.p IS NULL RETURN n", symbols)
        );
        let literal = template("MATCH (n) WHERE n.p = 1 RETURN n", symbols);
        let hole = template_with_parameters(
            "MATCH (n) WHERE n.p = $x RETURN n",
            &[("x", GqlParameterType::Int64)],
            symbols,
        );
        assert_ne!(literal, hole);
        let other_hole = template_with_parameters(
            "MATCH (n) WHERE n.p = $y RETURN n",
            &[("y", GqlParameterType::Int64)],
            symbols,
        );
        assert_eq!(hole, other_hole);
        assert_ne!(
            template("MATCH (n) WHERE n.p = 1 RETURN n", symbols),
            template("MATCH (n) WHERE n.p = 1 RETURN n", other_symbols),
        );
        assert_ne!(
            template("MATCH (n) WHERE n.p <> 1 RETURN n", symbols),
            template("MATCH (n) WHERE NOT n.p <> 1 RETURN n", symbols),
        );
    }
}

#[cfg(test)]
mod compound_tests {
    use super::*;
    use crate::GqlQueryPolicy;
    use fgdb_types::VId;
    use std::cell::Cell;

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            _ => None,
        }
    }

    fn prepare(predicate: &str) -> PreparedGraphPattern<GraphValueRow> {
        PreparedGraphText::prepare(&format!("MATCH (n) WHERE {predicate} RETURN n"), symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap()
    }

    fn rows(predicate: &str, values: &[Option<CanonicalScalar>]) -> Vec<VId> {
        let plan = prepare(predicate);
        let result = plan
            .plan()
            .execute_governed_with_properties(
                values.len() as u64,
                (0..values.len()).map(|at| VId(at as u128 + 1)),
                [],
                |_, _| Ok::<_, ()>(true),
                |vid, _| Ok(values[vid.0 as usize - 1].as_ref()),
                GqlQueryPolicy::new(100, 100, 100_000, 100_000),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        result
            .value
            .iter()
            .map(|row| row.values()[0].as_vertex().unwrap())
            .collect()
    }

    #[test]
    fn compound_lowering_reuses_the_existing_boolean_transcript() {
        for (compound, expanded) in [
            ("n.p IN [1, 2, NULL]", "(n.p = 1 OR n.p = 2 OR n.p = NULL)"),
            ("n.p NOT IN [1, 2]", "NOT (n.p = 1 OR n.p = 2)"),
            ("n.p BETWEEN -2 AND 4", "(n.p >= -2 AND n.p <= 4)"),
            ("n.p NOT BETWEEN 1 AND n.q", "NOT (n.p >= 1 AND n.p <= n.q)"),
            ("n.p IN []", "(n.p IS NULL AND FALSE)"),
            ("n.p NOT IN []", "NOT (n.p IS NULL AND FALSE)"),
        ] {
            assert_eq!(
                prepare(compound).canonical_bytes(),
                prepare(expanded).canonical_bytes(),
                "{compound}"
            );
        }
    }

    #[test]
    fn membership_and_negation_preserve_unknown_and_empty_list_laws() {
        let values = [
            Some(CanonicalScalar::Int(1)),
            Some(CanonicalScalar::Int(2)),
            Some(CanonicalScalar::Int(3)),
            Some(CanonicalScalar::Null),
            None,
        ];
        assert_eq!(rows("n.p IN [1, 3, 3]", &values), vec![VId(1), VId(3)]);
        assert_eq!(rows("n.p NOT IN [1, 3]", &values), vec![VId(2)]);
        assert_eq!(rows("n.p IN [1, NULL]", &values), vec![VId(1)]);
        assert!(rows("n.p NOT IN [1, NULL]", &values).is_empty());
        assert!(rows("n.p IN []", &values).is_empty());
        assert_eq!(
            rows("n.p NOT IN []", &values),
            (1..=5).map(VId).collect::<Vec<_>>()
        );
    }

    #[test]
    fn ranges_are_inclusive_and_do_not_swallow_outer_boolean_operators() {
        let values = (0..=4)
            .map(|value| Some(CanonicalScalar::Int(value)))
            .collect::<Vec<_>>();
        assert_eq!(
            rows("n.p BETWEEN 1 AND 3", &values),
            vec![VId(2), VId(3), VId(4)]
        );
        assert_eq!(
            rows("n.p NOT BETWEEN 1 AND 3", &values),
            vec![VId(1), VId(5)]
        );
        assert!(rows("n.p BETWEEN 3 AND 1", &values).is_empty());
        assert_eq!(
            rows("n.p BETWEEN 1 AND 3 AND n.p IN [2, 4] OR n.p = 0", &values),
            vec![VId(1), VId(3)]
        );
        assert_eq!(
            rows("NOT n.p BETWEEN 1 AND 3", &values),
            vec![VId(1), VId(5)]
        );
    }

    #[test]
    fn literal_keywords_and_quotes_are_operands_not_query_fragments() {
        let value = CanonicalScalar::ucs_basic_text("x'] OR TRUE --").unwrap();
        let values = [
            Some(value),
            Some(CanonicalScalar::ucs_basic_text("other").unwrap()),
        ];
        assert_eq!(rows("n.p IN ['x''] OR TRUE --']", &values), vec![VId(1)]);
        let values = [
            Some(CanonicalScalar::Bool(true)),
            Some(CanonicalScalar::Bool(false)),
            None,
        ];
        assert_eq!(rows("n.p IN [TRUE]", &values), vec![VId(1)]);
        assert_eq!(rows("n.p NOT IN [TRUE]", &values), vec![VId(2)]);
    }

    #[test]
    fn parameters_are_bound_once_without_text_substitution_or_catalog_reentry() {
        let calls = Cell::new(0);
        let template = PreparedGraphText::prepare(
            "MATCH (n) WHERE n.p IN [$x, $x] AND n.p BETWEEN $lo AND $hi RETURN n",
            |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            },
        )
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(template.parameter_schema()[0].occurrences, 2);
        let args = GqlParameters::new()
            .with_int64("x", 2)
            .unwrap()
            .with_int64("lo", i64::MIN)
            .unwrap()
            .with_int64("hi", i64::MAX)
            .unwrap();
        let first = template.bind_parameters(&args).unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(
            first.canonical_bytes(),
            template.bind_parameters(&args).unwrap().canonical_bytes()
        );
        assert!(matches!(
            template
                .bind_parameters(&GqlParameters::new())
                .unwrap_err()
                .kind,
            GraphPatternTextErrorKind::MissingParameter
        ));
    }

    #[test]
    fn malformed_and_oversized_lists_refuse_before_catalog_resolution() {
        for predicate in [
            "n.p IN [1,]",
            "n.p IN [1 2]",
            "n.p IN [",
            "n.p IN $xs",
            "n.p BETWEEN 1",
            "n.p BETWEEN 1 OR 2",
            "n.p NOT BETWEEN AND 2",
        ] {
            let calls = Cell::new(0);
            assert!(
                PreparedGraphText::prepare(
                    &format!("MATCH (n) WHERE {predicate} RETURN n"),
                    |kind, name| {
                        calls.set(calls.get() + 1);
                        symbols(kind, name)
                    }
                )
                .is_err(),
                "{predicate}"
            );
            assert_eq!(calls.get(), 0, "{predicate}");
        }
        let members = vec!["1"; MAX_PATTERN_PREDICATES + 1].join(",");
        assert!(
            PreparedGraphText::prepare(
                &format!("MATCH (n) WHERE n.p IN [{members}] RETURN n"),
                symbols
            )
            .is_err()
        );
        assert!(matches!(
            PreparedGraphText::prepare("MATCH (n) WHERE n.unknown IN [] RETURN n", symbols)
                .unwrap_err()
                .kind,
            GraphPatternTextErrorKind::UnknownSymbol(GraphSymbolKind::Property)
        ));
    }

    #[test]
    fn empty_membership_still_propagates_property_source_failure() {
        for predicate in ["n.p IN []", "n.p NOT IN []", "n.p IN [1, n.q]"] {
            let plan = prepare(predicate);
            let value = CanonicalScalar::Int(1);
            let result = plan.plan().execute_governed_with_properties(
                1,
                [VId(1)],
                [],
                |_, _| Ok::<_, &str>(true),
                |_, key| {
                    if predicate.ends_with(']')
                        && predicate.contains("n.q")
                        && key == PropertyKeyId(1)
                    {
                        Ok(Some(&value))
                    } else {
                        Err("unreadable property")
                    }
                },
                GqlQueryPolicy::new(100, 100, 100_000, 100_000),
                || Ok::<_, &str>(()),
            );
            assert!(result.is_err(), "{predicate}");
        }
    }

    #[test]
    fn scalar_dispatch_recognizes_subtraction_and_numeric_functions() {
        let values = [
            Some(CanonicalScalar::Int(-3)),
            Some(CanonicalScalar::Int(0)),
            Some(CanonicalScalar::Int(3)),
            Some(CanonicalScalar::Null),
            None,
        ];
        for (predicate, expected) in [
            ("n.p - 1 = 2", vec![VId(3)]),
            ("-n.p = 3", vec![VId(1)]),
            ("n.p = -n.p", vec![VId(2)]),
            ("ABS(n.p) = 3", vec![VId(1), VId(3)]),
            ("COALESCE(n.p,7) = 7", vec![VId(4), VId(5)]),
            ("NULLIF(n.p,3) IS NULL", vec![VId(3), VId(4), VId(5)]),
        ] {
            assert_eq!(rows(predicate, &values), expected, "{predicate}");
        }
    }

    #[test]
    fn scalar_dispatch_preserves_keyword_identifiers_and_signed_literal_lowering() {
        for name in [
            "upper",
            "lower",
            "trim",
            "substring",
            "char_length",
            "starts",
            "ends",
            "contains",
            "abs",
            "coalesce",
            "nullif",
        ] {
            let text = format!("MATCH ({name}),(other) WHERE {name}=other RETURN {name},other");
            let pattern = PreparedGraphText::prepare(&text, |_, _| None)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            assert_eq!(pattern.columns(), &[name, "other"]);
            let property = PreparedGraphText::prepare(
                &format!("MATCH (n) WHERE n.{name}=1 RETURN n"),
                |kind, _| {
                    (kind == GraphSymbolKind::Property)
                        .then_some(GraphSymbol::Property(PropertyKeyId(1)))
                },
            )
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
            assert_eq!(
                property.canonical_bytes(),
                prepare("n.p=1").canonical_bytes()
            );
        }
        let syntax = Parser::new("MATCH (n) WHERE n.p=-9223372036854775808 RETURN n")
            .unwrap()
            .parse()
            .unwrap();
        assert!(matches!(
            syntax.filters.as_slice(),
            [Filter::Property {
                comparison: IntegerComparison::Equal,
                value: Number::Literal(GqlParameterValue::Int64(i64::MIN)),
                ..
            }]
        ));
    }

    #[test]
    fn scalar_dispatch_keeps_computed_members_and_literal_unknown_membership() {
        let values = [
            Some(CanonicalScalar::Int(1)),
            Some(CanonicalScalar::Int(2)),
            Some(CanonicalScalar::Int(3)),
            None,
        ];
        assert_eq!(rows("n.p IN [n.q+1,3]", &values), vec![VId(3)]);
        assert_eq!(
            rows("n.p NOT IN [n.q-1]", &values),
            vec![VId(1), VId(2), VId(3)]
        );
        assert!(rows("TRUE IN [NULL]", &values).is_empty());
        assert!(rows("TRUE NOT IN [NULL]", &values).is_empty());
        assert_eq!(
            rows("NULL NOT IN []", &values),
            vec![VId(1), VId(2), VId(3), VId(4)]
        );
    }
}
