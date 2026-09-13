//! Boolean WHERE preparation through the shared lexer, schema and GLA compiler.
//! The parser emits bounded postfix instructions, never a second graph matcher.
//! Flat conjunctions retain their existing lowering. Extended expressions use
//! eager three-valued evaluation and may span several bound vertices. EXISTS
//! remains a scoped clause, not a Boolean leaf in this deliberately bounded
//! profile; unsupported mixtures are rejected before catalog resolution.
//! Property IN/NOT IN lists and BETWEEN/NOT BETWEEN ranges lower to the same
//! checked comparisons, preserving UNKNOWN, typed arguments and source errors.

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
        if self.compound_property_predicate(parsed)? {
            return Ok(());
        }
        self.positive_predicate()?;
        let filter = self.syntax.filters.pop().expect("one positive predicate was just parsed");
        parsed.extended |= matches!(filter, Filter::VertexNull { .. });
        parsed.push(SyntaxItem::Atom(filter), at)
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
        let variable = self.variable()?;
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
                parsed.push(SyntaxItem::Atom(Filter::Null {
                    variable, key, is_null: true,
                }), at)?;
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
        let result = plan.plan().execute_governed_with_properties(
            values.len() as u64,
            (0..values.len()).map(|at| VId(at as u128 + 1)),
            [],
            |_, _| Ok::<_, ()>(true),
            |vid, _| Ok(values[vid.0 as usize - 1].as_ref()),
            GqlQueryPolicy::new(100, 100, 100_000, 100_000),
            || Ok::<_, ()>(()),
        ).unwrap();
        result.value.iter().map(|row| row.values()[0].as_vertex().unwrap()).collect()
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
            assert_eq!(prepare(compound).canonical_bytes(), prepare(expanded).canonical_bytes(), "{compound}");
        }
    }

    #[test]
    fn membership_and_negation_preserve_unknown_and_empty_list_laws() {
        let values = [Some(CanonicalScalar::Int(1)), Some(CanonicalScalar::Int(2)),
            Some(CanonicalScalar::Int(3)), Some(CanonicalScalar::Null), None];
        assert_eq!(rows("n.p IN [1, 3, 3]", &values), vec![VId(1), VId(3)]);
        assert_eq!(rows("n.p NOT IN [1, 3]", &values), vec![VId(2)]);
        assert_eq!(rows("n.p IN [1, NULL]", &values), vec![VId(1)]);
        assert!(rows("n.p NOT IN [1, NULL]", &values).is_empty());
        assert!(rows("n.p IN []", &values).is_empty());
        assert_eq!(rows("n.p NOT IN []", &values), (1..=5).map(VId).collect::<Vec<_>>());
    }

    #[test]
    fn ranges_are_inclusive_and_do_not_swallow_outer_boolean_operators() {
        let values = (0..=4).map(|value| Some(CanonicalScalar::Int(value))).collect::<Vec<_>>();
        assert_eq!(rows("n.p BETWEEN 1 AND 3", &values), vec![VId(2), VId(3), VId(4)]);
        assert_eq!(rows("n.p NOT BETWEEN 1 AND 3", &values), vec![VId(1), VId(5)]);
        assert!(rows("n.p BETWEEN 3 AND 1", &values).is_empty());
        assert_eq!(rows("n.p BETWEEN 1 AND 3 AND n.p IN [2, 4] OR n.p = 0", &values), vec![VId(1), VId(3)]);
        assert_eq!(rows("NOT n.p BETWEEN 1 AND 3", &values), vec![VId(1), VId(5)]);
    }

    #[test]
    fn literal_keywords_and_quotes_are_operands_not_query_fragments() {
        let value = CanonicalScalar::ucs_basic_text("x'] OR TRUE --").unwrap();
        let values = [Some(value), Some(CanonicalScalar::ucs_basic_text("other").unwrap())];
        assert_eq!(rows("n.p IN ['x''] OR TRUE --']", &values), vec![VId(1)]);
        let values = [Some(CanonicalScalar::Bool(true)), Some(CanonicalScalar::Bool(false)), None];
        assert_eq!(rows("n.p IN [TRUE]", &values), vec![VId(1)]);
        assert_eq!(rows("n.p NOT IN [TRUE]", &values), vec![VId(2)]);
    }

    #[test]
    fn parameters_are_bound_once_without_text_substitution_or_catalog_reentry() {
        let calls = Cell::new(0);
        let template = PreparedGraphText::prepare(
            "MATCH (n) WHERE n.p IN [$x, $x] AND n.p BETWEEN $lo AND $hi RETURN n",
            |kind, name| { calls.set(calls.get() + 1); symbols(kind, name) },
        ).unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(template.parameter_schema()[0].occurrences, 2);
        let args = GqlParameters::new().with_int64("x", 2).unwrap()
            .with_int64("lo", i64::MIN).unwrap().with_int64("hi", i64::MAX).unwrap();
        let first = template.bind_parameters(&args).unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(first.canonical_bytes(), template.bind_parameters(&args).unwrap().canonical_bytes());
        assert!(matches!(template.bind_parameters(&GqlParameters::new()).unwrap_err().kind,
            GraphPatternTextErrorKind::MissingParameter));
    }

    #[test]
    fn malformed_and_oversized_lists_refuse_before_catalog_resolution() {
        for predicate in ["n.p IN [1,]", "n.p IN [1 2]", "n.p IN [", "n.p IN $xs",
            "n.p BETWEEN 1", "n.p BETWEEN 1 OR 2", "n.p NOT BETWEEN AND 2"] {
            let calls = Cell::new(0);
            assert!(PreparedGraphText::prepare(&format!("MATCH (n) WHERE {predicate} RETURN n"),
                |kind, name| { calls.set(calls.get() + 1); symbols(kind, name) }).is_err(), "{predicate}");
            assert_eq!(calls.get(), 0, "{predicate}");
        }
        let members = vec!["1"; MAX_PATTERN_PREDICATES + 1].join(",");
        assert!(PreparedGraphText::prepare(
            &format!("MATCH (n) WHERE n.p IN [{members}] RETURN n"), symbols).is_err());
        assert!(matches!(PreparedGraphText::prepare(
            "MATCH (n) WHERE n.unknown IN [] RETURN n", symbols).unwrap_err().kind,
            GraphPatternTextErrorKind::UnknownSymbol(GraphSymbolKind::Property)));
    }

    #[test]
    fn empty_membership_still_propagates_property_source_failure() {
        for predicate in ["n.p IN []", "n.p NOT IN []", "n.p IN [1, n.q]"] {
            let plan = prepare(predicate);
            let value = CanonicalScalar::Int(1);
            let result = plan.plan().execute_governed_with_properties(
                1, [VId(1)], [], |_, _| Ok::<_, &str>(true),
                |_, key| {
                    if predicate.ends_with(']') && predicate.contains("n.q") && key == PropertyKeyId(1) {
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
}
