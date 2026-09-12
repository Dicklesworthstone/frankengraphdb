//! Boolean WHERE preparation through the shared lexer, schema and GLA compiler.
//! The parser emits bounded postfix instructions, never a second graph matcher.
//! Flat conjunctions retain their existing lowering. Extended expressions use
//! eager three-valued evaluation and may span several bound vertices. EXISTS
//! remains a scoped clause, not a Boolean leaf in this deliberately bounded
//! profile; unsupported mixtures are rejected before catalog resolution.

use super::*;
use crate::algebra::{GraphBooleanExpression, GraphBooleanOp as Op,
    GraphBooleanOperand as Operand, MAX_BOOLEAN_INSTRUCTIONS, ScalarPredicate};
use fgdb_types::CanonicalScalar;

const MAX_BOOLEAN_NESTING: usize = 64;

pub(super) enum SyntaxItem<'a> {
    Atom(Filter<'a>),
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
        let mut parsed = Parsed { program: Vec::new(), extended: false };
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
        let identities = parsed.program.iter().filter(|item| matches!(item,
            SyntaxItem::Atom(Filter::Identity { .. }))).count();
        let predicates = self.predicates.saturating_add(identities);
        if predicates > MAX_PATTERN_PREDICATES {
            return Err(error(at, GraphPatternTextErrorKind::Build(PatternBuildError::LimitExceeded {
                dimension: crate::algebra::PatternLimitDimension::Predicates,
                limit: MAX_PATTERN_PREDICATES, observed: predicates,
            })));
        }
        self.identities -= identities;
        self.predicates = predicates;
        self.syntax.filters.push(Filter::Boolean { program: parsed.program, at });
        Ok(true)
    }

    fn boolean_or(&mut self, depth: usize, parsed: &mut Parsed<'a>) -> Result<(), GraphPatternTextError> {
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

    fn boolean_and(&mut self, depth: usize, parsed: &mut Parsed<'a>) -> Result<(), GraphPatternTextError> {
        self.boolean_unary(depth, parsed)?;
        while self.is_word("AND") {
            // Leave the delimiter itself to the existing scoped clause parser.
            // A compound expression plus a scope is refused by that owner; it
            // is never silently rearranged into (A OR B) AND EXISTS.
            if depth == 0 && self.and_starts_existence()? { break; }
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
            return Ok(false);
        }
        Ok(matches!(lexer.next()?.kind, TokenKind::Punct(b'{')))
    }

    // Preserve preexisting variables called not/true/false/null when followed
    // by an atom's identifier syntax. Quoted strings are already one token and
    // cannot introduce operators, parentheses or scopes through this lookahead.
    fn boolean_word_is_variable(&self) -> Result<bool, GraphPatternTextError> {
        let token = self.lexer.clone().next()?;
        Ok(matches!(token.kind, TokenKind::Punct(b'.' | b'=' | b'!' | b'<' | b'>'))
            || matches!(token.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("IS")))
    }

    fn boolean_unary(&mut self, depth: usize, parsed: &mut Parsed<'a>) -> Result<(), GraphPatternTextError> {
        if depth > MAX_BOOLEAN_NESTING {
            return Err(error(self.current.at, GraphPatternTextErrorKind::BooleanNesting {
                limit: MAX_BOOLEAN_NESTING,
            }));
        }
        if self.starts_existence()? {
            return Err(error(self.current.at, GraphPatternTextErrorKind::UnsupportedBooleanScope));
        }
        let at = self.current.at;
        if self.is_word("NOT") && !self.boolean_word_is_variable()? {
            self.advance()?;
            parsed.extended = true;
            self.boolean_unary(depth + 1, parsed)?;
            return parsed.push(SyntaxItem::Not, at);
        }
        if self.take(b'(')? {
            parsed.extended = true;
            self.boolean_or(depth + 1, parsed)?;
            self.punct(b')', ")")?;
            return Ok(());
        }
        if (self.is_word("TRUE") || self.is_word("FALSE") || self.is_word("NULL"))
            && !self.boolean_word_is_variable()? {
            self.capacity(self.predicates, MAX_PATTERN_PREDICATES,
                crate::algebra::PatternLimitDimension::Predicates)?;
            let value = if self.is_word("NULL") { None } else { Some(self.is_word("TRUE")) };
            self.advance()?;
            self.predicates += 1;
            parsed.extended = true;
            return parsed.push(SyntaxItem::Truth(value), at);
        }
        self.positive_predicate()?;
        let filter = self.syntax.filters.pop().expect("one positive predicate was just parsed");
        parsed.extended |= matches!(filter, Filter::VertexNull { .. });
        parsed.push(SyntaxItem::Atom(filter), at)
    }
}

#[derive(Clone)]
enum Atom {
    Property { variable: String, key: PropertyKeyId, comparison: IntegerComparison, value: Number },
    Scalar { variable: String, key: PropertyKeyId, predicate: ScalarPredicate },
    Null { variable: String, key: PropertyKeyId, is_null: bool },
    VertexNull { variable: String, is_null: bool },
    Identity { left: String, right: String, equal: bool },
    Properties { left: String, left_key: PropertyKeyId, right: String,
        right_key: PropertyKeyId, comparison: IntegerComparison },
}
#[derive(Clone)]
enum Item { Atom(Atom), Truth(Option<bool>), And, Or, Not }

/// Resolved immutable syntax. Only typed numeric/scalar operands vary during
/// binding; names are not resolved again and arguments never become text.
#[derive(Clone)]
pub(super) struct BoundBooleanTemplate {
    program: Vec<Item>,
    at: usize,
}

impl BoundBooleanTemplate {
    pub(super) fn resolve<'a>(program: Vec<SyntaxItem<'a>>, at: usize,
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
                SyntaxItem::Truth(value) => Item::Truth(value),
                SyntaxItem::And => Item::And,
                SyntaxItem::Or => Item::Or,
                SyntaxItem::Not => Item::Not,
                SyntaxItem::Atom(filter) => Item::Atom(match filter {
                    Filter::Property { variable, key, comparison, value } => Atom::Property {
                        variable: variable.text.to_owned(), key: property(key)?, comparison, value,
                    },
                    Filter::Scalar { variable, key, predicate } => Atom::Scalar {
                        variable: variable.text.to_owned(), key: property(key)?, predicate,
                    },
                    Filter::Null { variable, key, is_null } => Atom::Null {
                        variable: variable.text.to_owned(), key: property(key)?, is_null,
                    },
                    Filter::VertexNull { variable, is_null } => Atom::VertexNull {
                        variable: variable.text.to_owned(), is_null,
                    },
                    Filter::Identity { left, right, equal } => Atom::Identity {
                        left: left.text.to_owned(), right: right.text.to_owned(), equal,
                    },
                    Filter::Properties { left, left_key, right, right_key, comparison } => Atom::Properties {
                        left: left.text.to_owned(), left_key: property(left_key)?,
                        right: right.text.to_owned(), right_key: property(right_key)?, comparison,
                    },
                    Filter::Boolean { .. } => return Err(error(at, GraphPatternTextErrorKind::BooleanExpression)),
                }),
            });
        }
        Ok(Self { program: resolved, at })
    }

    pub(super) fn bind(&self, values: &[GqlParameterValue]) -> Result<GraphBooleanExpression, GraphPatternTextError> {
        // Finish the operand storage before taking any references into it.
        // Canonical scalar arguments reuse their checked Arc/encoding; numeric
        // operands encode once for this bound immutable expression.
        let literals = self.program.iter().map(|item| match item {
            Item::Atom(Atom::Scalar { predicate, .. }) => Ok(Some(predicate.clone())),
            Item::Atom(Atom::Property { value, .. }) => match value.value(values) {
                GqlParameterValue::Int64(value) => ScalarPredicate::new(CanonicalScalar::Int(value), IntegerComparison::Equal)
                    .map(Some).map_err(|_| error(self.at, GraphPatternTextErrorKind::ScalarLiteral)),
                GqlParameterValue::Scalar(value) => Ok(Some(value.predicate(IntegerComparison::Equal))),
                GqlParameterValue::UInt64(_) => Err(error(self.at, GraphPatternTextErrorKind::BooleanExpression)),
            },
            _ => Ok(None),
        }).collect::<Result<Vec<_>, _>>()?;
        let mut program = Vec::new();
        for (item, literal) in self.program.iter().zip(&literals) {
            program.push(match item {
                Item::Truth(value) => Op::Truth(*value),
                Item::And => Op::And,
                Item::Or => Op::Or,
                Item::Not => Op::Not,
                Item::Atom(atom) => match atom {
                    Atom::Property { variable, key, comparison, .. } => Op::Compare {
                        left: Operand::Property { variable, key: *key }, comparison: *comparison,
                        right: Operand::CheckedLiteral(literal.as_ref().expect("bound numeric/scalar literal")),
                    },
                    Atom::Scalar { variable, key, predicate } => Op::Compare {
                        left: Operand::Property { variable, key: *key }, comparison: predicate.comparison(),
                        right: Operand::CheckedLiteral(literal.as_ref().expect("bound scalar literal")),
                    },
                    Atom::Null { variable, key, is_null } => Op::IsNull {
                        operand: Operand::Property { variable, key: *key }, is_null: *is_null,
                    },
                    Atom::VertexNull { variable, is_null } => Op::IsNull {
                        operand: Operand::Vertex(variable), is_null: *is_null,
                    },
                    Atom::Identity { left, right, equal } => Op::Compare {
                        left: Operand::Vertex(left), right: Operand::Vertex(right),
                        comparison: if *equal { IntegerComparison::Equal } else { IntegerComparison::NotEqual },
                    },
                    Atom::Properties { left, left_key, right, right_key, comparison } => Op::Compare {
                        left: Operand::Property { variable: left, key: *left_key }, comparison: *comparison,
                        right: Operand::Property { variable: right, key: *right_key },
                    },
                },
            });
        }
        GraphBooleanExpression::prepare(&program)
            .map_err(|_| error(self.at, GraphPatternTextErrorKind::BooleanExpression))
    }
}
