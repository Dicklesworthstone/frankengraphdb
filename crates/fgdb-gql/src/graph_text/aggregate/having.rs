//! HAVING precedence and immutable operands over the shared aggregate parser.
//! No extra lexer, source query, catalog lookup or text interpolation at bind.
//! Output references resolve through the SAME returned-column resolver used
//! by ORDER BY. No unprojected aggregate or key is synthesized by this clause.

use super::*;
use crate::{GraphHavingExpression, GraphHavingOp, GraphHavingOperand, MAX_HAVING_INSTRUCTIONS};
use crate::algebra::ScalarPredicate;
use fgdb_types::CanonicalScalar;

#[derive(Clone)]
enum Operand {
    Column(GraphAggregateColumn),
    Number { value: Number, legacy_int64: bool },
    Scalar(ScalarPredicate),
}
impl Operand {
    fn bind(&self, values: &[GqlParameterValue]) -> GraphHavingOperand {
        match self {
            Self::Column(column) => GraphHavingOperand::Column(*column),
            Self::Scalar(value) => GraphHavingOperand::Scalar(value.clone()),
            Self::Number { value, .. } => match value.value(values) {
                GqlParameterValue::Int64(value) => GraphHavingOperand::Integer(i128::from(value)),
                GqlParameterValue::UInt64(value) => GraphHavingOperand::Integer(i128::from(value)),
                GqlParameterValue::Scalar(value) => GraphHavingOperand::Scalar(value.predicate(IntegerComparison::Equal)),
            },
        }
    }
}

#[derive(Clone)]
enum Op {
    Compare { left: Operand, comparison: IntegerComparison, right: Operand },
    IsNull { operand: Operand, is_null: bool },
    Truth(Option<bool>),
    And,
    Or,
    Not,
}

#[derive(Clone)]
pub(super) struct HavingTemplate {
    program: Vec<Op>,
    at: usize,
}
impl HavingTemplate {
    pub(super) fn attach(&self, aggregate: PreparedGraphAggregate, values: &[GqlParameterValue])
        -> Result<PreparedGraphAggregate, GraphPatternTextError> {
        let program = self.program.iter().map(|op| match op {
            Op::Compare { left, comparison, right } => GraphHavingOp::Compare {
                left: left.bind(values), comparison: *comparison, right: right.bind(values),
            },
            Op::IsNull { operand, is_null } => GraphHavingOp::IsNull { operand: operand.bind(values), is_null: *is_null },
            Op::Truth(value) => GraphHavingOp::Truth(*value),
            Op::And => GraphHavingOp::And, Op::Or => GraphHavingOp::Or, Op::Not => GraphHavingOp::Not,
        }).collect::<Vec<_>>();
        let failure = |_| error(self.at, GraphPatternTextErrorKind::Expected("valid bounded HAVING expression"));
        let expression = GraphHavingExpression::prepare(&program).map_err(failure)?;
        aggregate.with_having_expression(&expression).map_err(failure)
    }
}

pub(super) fn parse<'a>(parser: &mut Parser<'a>, returned: &[ReturnItem<'a>], groups: &[Expression<'a>])
    -> Result<(Vec<Having>, Option<HavingTemplate>), GraphPatternTextError> {
    if !parser.take_word("HAVING")? { return Ok((Vec::new(), None)); }
    let at = parser.current.at;
    let mut input = HavingParser { parser, returned, groups, program: Vec::new(), leaves: 0, compound: false };
    input.disjunction(0)?;
    // One grammar, not retry-after-error. Preserve the old flat conjunction's
    // lowering, eager numeric-domain checks, counters and transcript bytes.
    if !input.compound {
        let mut flat = Vec::new();
        let mut eligible = true;
        for op in &input.program {
            match op {
                Op::Compare { left: Operand::Column(column), comparison,
                    right: Operand::Number { value, legacy_int64: true } } => flat.push(Having {
                        column: *column, test: HavingTest::Integer { comparison: *comparison, value: value.clone() },
                    }),
                Op::IsNull { operand: Operand::Column(column), is_null } => flat.push(Having {
                    column: *column, test: if *is_null { HavingTest::IsNull } else { HavingTest::IsNotNull },
                }),
                Op::And => {}
                _ => { eligible = false; break; }
            }
        }
        if eligible { return Ok((flat, None)); }
    }
    Ok((Vec::new(), Some(HavingTemplate { program: input.program, at })))
}

struct HavingParser<'p, 'a> {
    parser: &'p mut Parser<'a>,
    returned: &'p [ReturnItem<'a>],
    groups: &'p [Expression<'a>],
    program: Vec<Op>,
    leaves: usize,
    compound: bool,
}
impl HavingParser<'_, '_> {
    fn emit(&mut self, op: Op) -> Result<(), GraphPatternTextError> {
        if self.program.len() == MAX_HAVING_INSTRUCTIONS {
            return Err(error(self.parser.current.at, GraphPatternTextErrorKind::Expected("at most 1024 HAVING instructions")));
        }
        self.program.push(op);
        Ok(())
    }
    fn leaf(&mut self, op: Op) -> Result<(), GraphPatternTextError> {
        self.parser.capacity(self.leaves, MAX_AGGREGATE_FILTERS, crate::algebra::PatternLimitDimension::Predicates)?;
        self.leaves += 1;
        self.emit(op)
    }
    fn disjunction(&mut self, depth: usize) -> Result<(), GraphPatternTextError> {
        self.conjunction(depth)?;
        while self.parser.take_word("OR")? {
            self.compound = true;
            self.conjunction(depth)?;
            self.emit(Op::Or)?;
        }
        Ok(())
    }
    fn conjunction(&mut self, depth: usize) -> Result<(), GraphPatternTextError> {
        self.unary(depth)?;
        while self.parser.take_word("AND")? {
            self.unary(depth)?;
            self.emit(Op::And)?;
        }
        Ok(())
    }
    fn unary(&mut self, depth: usize) -> Result<(), GraphPatternTextError> {
        if depth > 64 {
            return Err(error(self.parser.current.at, GraphPatternTextErrorKind::Expected("HAVING nesting at most 64")));
        }
        // Preserve an output named `not` when followed by an operand suffix.
        // NOT (...) or NOT <condition> is unary syntax, not a function call.
        let not_reference = if self.parser.is_word("NOT") {
            let next = self.parser.lexer.clone().next()?;
            matches!(next.kind, TokenKind::Punct(b'.' | b'=' | b'<' | b'>' | b'!'))
                || matches!(next.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("IS"))
        } else { false };
        if self.parser.is_word("NOT") && !not_reference {
            self.parser.advance()?;
            self.compound = true;
            self.unary(depth + 1)?;
            return self.emit(Op::Not);
        }
        if self.parser.take(b'(')? {
            self.compound = true;
            self.disjunction(depth + 1)?;
            return self.parser.punct(b')', ")");
        }
        self.atom()
    }
    fn operand(&mut self) -> Result<Operand, GraphPatternTextError> {
        let at = self.parser.current.at;
        let scalar = match self.parser.current.kind {
            TokenKind::Quoted(raw) => Some(super::super::literal::text_scalar(raw, at)?),
            TokenKind::Word(word) => {
                let next = self.parser.lexer.clone().next()?;
                if matches!(next.kind, TokenKind::Punct(b'.' | b'('))
                    || self.returned.iter().any(|item| item.alias.text == word) {
                    return self.parser.result_column(self.returned, self.groups).map(Operand::Column);
                }
                if word.eq_ignore_ascii_case("TRUE") { Some(CanonicalScalar::Bool(true)) }
                else if word.eq_ignore_ascii_case("FALSE") { Some(CanonicalScalar::Bool(false)) }
                else if word.eq_ignore_ascii_case("NULL") { Some(CanonicalScalar::Null) }
                else { return self.parser.result_column(self.returned, self.groups).map(Operand::Column); }
            }
            _ => None,
        };
        if let Some(value) = scalar {
            self.parser.advance()?;
            return ScalarPredicate::new(value, IntegerComparison::Equal).map(Operand::Scalar)
                .map_err(|_| error(at, GraphPatternTextErrorKind::ScalarLiteral));
        }
        let kind = match self.parser.current.kind {
            TokenKind::Parameter(name) => self.parser.parameter_types.get(name).copied().unwrap_or(GqlParameterType::Int64),
            _ => GqlParameterType::Int64,
        };
        let value = self.parser.number(kind)?;
        Ok(Operand::Number { value, legacy_int64: kind == GqlParameterType::Int64 })
    }
    fn atom(&mut self) -> Result<(), GraphPatternTextError> {
        let at = self.parser.current.at;
        let left = self.operand()?;
        if self.parser.take_word("IS")? {
            let negate = self.parser.take_word("NOT")?;
            self.parser.word("NULL")?;
            return self.leaf(Op::IsNull { operand: left, is_null: !negate });
        }
        if matches!(self.parser.current.kind, TokenKind::Punct(b'=' | b'<' | b'>' | b'!')) {
            let comparison = self.parser.comparison()?;
            let right = self.operand()?;
            return self.leaf(Op::Compare { left, comparison, right });
        }
        if let Operand::Scalar(value) = left {
            let truth = match value.value() {
                CanonicalScalar::Bool(value) => Some(*value),
                CanonicalScalar::Null => None,
                _ => return Err(error(at, GraphPatternTextErrorKind::Expected("HAVING comparison or null test"))),
            };
            return self.leaf(Op::Truth(truth));
        }
        Err(error(at, GraphPatternTextErrorKind::Expected("HAVING comparison or null test")))
    }
}
