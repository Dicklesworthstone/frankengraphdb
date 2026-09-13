//! HAVING precedence and immutable operands over the shared aggregate parser.
//! No extra lexer, source query, catalog lookup or text interpolation at bind.
//! Output references and private aggregate calls use the SAME resolver and
//! bounded registry as ORDER BY. Hidden grouping keys keep evaluation indices.
//! IN/NOT IN list constructors and BETWEEN/NOT BETWEEN inclusive ranges lower
//! to the existing eager three-valued comparisons. An operand can be a public
//! alias, grouping expression, hidden aggregate, scalar literal or parameter.

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

pub(super) fn parse<'a>(
    parser: &mut Parser<'a>,
    returned: &[ReturnItem<'a>],
    groups: &[Expression<'a>],
    hidden: &mut Vec<HiddenSummary<'a>>,
)
    -> Result<(Vec<Having>, Option<HavingTemplate>), GraphPatternTextError> {
    if !parser.take_word("HAVING")? { return Ok((Vec::new(), None)); }
    let at = parser.current.at;
    let mut input = HavingParser { parser, returned, groups, hidden, program: Vec::new(), leaves: 0, compound: false };
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
    hidden: &'p mut Vec<HiddenSummary<'a>>,
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
            let mut lexer = self.parser.lexer.clone();
            let next = lexer.next()?;
            let named = matches!(self.parser.current.kind, TokenKind::Word(word)
                if self.returned.iter().any(|item| item.alias.text == word)
                    || self.groups.iter().any(|group| group.property.is_none()
                        && group.variable.text == word));
            matches!(next.kind, TokenKind::Punct(b'.' | b'=' | b'<' | b'>' | b'!'))
                || matches!(next.kind, TokenKind::Word(word)
                    if word.eq_ignore_ascii_case("IS") || word.eq_ignore_ascii_case("IN")
                        || word.eq_ignore_ascii_case("BETWEEN"))
                || (named && matches!(next.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("NOT"))
                    && matches!(lexer.next()?.kind, TokenKind::Word(word)
                        if word.eq_ignore_ascii_case("IN") || word.eq_ignore_ascii_case("BETWEEN")))
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
                    || self.returned.iter().any(|item| item.alias.text == word)
                    || self.groups.iter().any(|group| group.property.is_none() && group.variable.text == word) {
                    return self.parser.result_column(self.returned, self.groups, self.hidden).map(Operand::Column);
                }
                if word.eq_ignore_ascii_case("TRUE") { Some(CanonicalScalar::Bool(true)) }
                else if word.eq_ignore_ascii_case("FALSE") { Some(CanonicalScalar::Bool(false)) }
                else if word.eq_ignore_ascii_case("NULL") { Some(CanonicalScalar::Null) }
                else { return self.parser.result_column(self.returned, self.groups, self.hidden).map(Operand::Column); }
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
        if self.parser.is_word("IN") || self.parser.is_word("BETWEEN") || self.parser.is_word("NOT") {
            return self.membership_or_range(left);
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

    /// Reuse resolved operands, not text. A left parameter or hidden aggregate
    /// is registered once, even when its immutable value is compared with many
    /// members. Cloning a reference does not create another aggregate state.
    fn membership_or_range(&mut self, left: Operand) -> Result<(), GraphPatternTextError> {
        self.compound = true;
        let negate = self.parser.take_word("NOT")?;
        if self.parser.take_word("IN")? {
            self.parser.punct(b'[', "[")?;
            if self.parser.take(b']')? {
                // Do not prune the left operand's hidden aggregate or its source
                // errors merely because membership in an empty list is false.
                self.leaf(Op::IsNull { operand: left, is_null: true })?;
                self.leaf(Op::Truth(Some(false)))?;
                self.emit(Op::And)?;
            } else {
                let mut first = true;
                loop {
                    self.parser.capacity(self.leaves, MAX_AGGREGATE_FILTERS,
                        crate::algebra::PatternLimitDimension::Predicates)?;
                    let right = self.operand()?;
                    self.leaf(Op::Compare {
                        left: left.clone(), comparison: IntegerComparison::Equal, right,
                    })?;
                    if !first { self.emit(Op::Or)?; }
                    first = false;
                    if self.parser.take(b']')? { break; }
                    self.parser.punct(b',', ", or ]")?;
                }
            }
        } else {
            self.parser.word("BETWEEN")?;
            let lower = self.operand()?;
            self.leaf(Op::Compare {
                left: left.clone(), comparison: IntegerComparison::GreaterOrEqual, right: lower,
            })?;
            // Consume only the range delimiter; the outer conjunction remains
            // the owner of any subsequent AND. No symmetric-bound reordering.
            self.parser.word("AND")?;
            let upper = self.operand()?;
            self.leaf(Op::Compare {
                left, comparison: IntegerComparison::LessOrEqual, right: upper,
            })?;
            self.emit(Op::And)?;
        }
        if negate { self.emit(Op::Not)?; }
        Ok(())
    }
}

#[cfg(test)]
mod compound_tests {
    use super::*;
    use std::cell::Cell;

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        }
    }

    fn prepare(predicate: &str) -> PreparedGraphAggregate {
        PreparedGraphAggregateText::prepare(
            &format!("MATCH (n) RETURN COUNT(*) AS c HAVING {predicate}"), symbols,
        ).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
    }

    #[test]
    fn membership_and_ranges_reuse_the_exact_having_program() {
        for (compound, expanded) in [
            ("c IN [0, 1, NULL]", "(c = 0 OR c = 1 OR c = NULL)"),
            ("c NOT IN [1, 2]", "NOT (c = 1 OR c = 2)"),
            ("c BETWEEN -1 AND 3", "(c >= -1 AND c <= 3)"),
            ("c NOT BETWEEN 1 AND 2", "NOT (c >= 1 AND c <= 2)"),
            ("c IN []", "(c IS NULL AND FALSE)"),
            ("c NOT IN []", "NOT (c IS NULL AND FALSE)"),
        ] {
            assert_eq!(prepare(compound), prepare(expanded), "{compound}");
        }
    }

    #[test]
    fn hidden_aggregate_operands_share_one_summary_and_remain_hidden() {
        for (compound, expanded) in [
            ("MIN(n.p) IN [1, 2]", "(MIN(n.p) = 1 OR MIN(n.p) = 2)"),
            ("c IN [MIN(n.p), MAX(n.p)]", "(c = MIN(n.p) OR c = MAX(n.p))"),
            ("c BETWEEN MIN(n.p) AND MAX(n.p)", "(c >= MIN(n.p) AND c <= MAX(n.p))"),
            ("MIN(n.p) NOT IN []", "NOT (MIN(n.p) IS NULL AND FALSE)"),
        ] {
            assert_eq!(prepare(compound), prepare(expanded), "{compound}");
        }
        let template = PreparedGraphAggregateText::prepare(
            "MATCH (n) RETURN COUNT(*) AS c HAVING MIN(n.p) IN [1, 2, 3]", symbols,
        ).unwrap();
        assert_eq!(template.columns(), &["c"]);
        assert_eq!(template.output_slots().len(), 1);
    }

    #[test]
    fn left_parameter_occurrences_are_not_multiplied_by_lowering() {
        let calls = Cell::new(0);
        let template = PreparedGraphAggregateText::prepare(
            "MATCH (n) RETURN COUNT(*) AS c HAVING $x IN [c, $x] AND MIN(n.p) BETWEEN $lo AND $hi",
            |kind, name| { calls.set(calls.get() + 1); symbols(kind, name) },
        ).unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(template.parameter_schema()[0].occurrences, 2);
        let arguments = GqlParameters::new().with_int64("x", 1).unwrap()
            .with_int64("lo", i64::MIN).unwrap().with_int64("hi", i64::MAX).unwrap();
        let bound = template.bind_parameters(&arguments).unwrap();
        assert_eq!(bound, template.bind_parameters(&arguments).unwrap());
        assert_eq!(calls.get(), 1);
        assert!(matches!(template.bind_parameters(&GqlParameters::new()).unwrap_err().kind,
            GraphPatternTextErrorKind::MissingParameter));
    }

    #[test]
    fn not_alias_and_prefix_negation_keep_their_distinct_roles() {
        for predicate in ["not IN [1]", "not NOT IN [1]", "NOT not IN [1]",
            "not BETWEEN 0 AND 2", "not NOT BETWEEN 0 AND 2", "NOT not BETWEEN 0 AND 2"] {
            PreparedGraphAggregateText::prepare(
                &format!("MATCH (n) RETURN COUNT(*) AS not HAVING {predicate}"), symbols,
            ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
        }
    }

    #[test]
    fn malformed_and_excessive_membership_refuses_before_catalog_access() {
        for predicate in ["MIN(n.p) IN [1,]", "MIN(n.p) IN [1 2]", "MIN(n.p) IN $list",
            "MIN(n.p) BETWEEN 1", "MIN(n.p) NOT BETWEEN 1 OR 2"] {
            let calls = Cell::new(0);
            assert!(PreparedGraphAggregateText::prepare(
                &format!("MATCH (n) RETURN COUNT(*) AS c HAVING {predicate}"),
                |kind, name| { calls.set(calls.get() + 1); symbols(kind, name) },
            ).is_err(), "{predicate}");
            assert_eq!(calls.get(), 0, "{predicate}");
        }
        let members = vec!["1"; MAX_AGGREGATE_FILTERS + 1].join(",");
        let calls = Cell::new(0);
        assert!(PreparedGraphAggregateText::prepare(
            &format!("MATCH (n) RETURN COUNT(*) AS c HAVING MIN(n.p) IN [{members}]"),
            |kind, name| { calls.set(calls.get() + 1); symbols(kind, name) },
        ).is_err());
        assert_eq!(calls.get(), 0);
    }
}
