//! Precedence-aware integer RHS lowering on the shared MATCH lexer.
//! Only preparation sees these temporary nodes. Binding substitutes typed
//! operands into checked bytecode, never into source text or an AST evaluator.

mod conditional;

use super::*;
use crate::{
    GraphIntegerBinary, GraphIntegerBuildError, GraphIntegerExpression, GraphIntegerOp,
    GraphIntegerUnary, MAX_GRAPH_INTEGER_INSTRUCTIONS,
};
use fgdb_types::CanonicalScalarKind;

const MAX_INTEGER_NESTING: usize = 64;
// The precedence/CASE compiler is shared by graph assignments and relational
// stages. A row scope resolves only admitted aliases, never ambient graph names.
enum ExpressionColumns<'columns, 'text> {
    Graph(&'columns mut Vec<Projection<'text>>),
    Row(&'columns [(Name<'text>, crate::GraphSetColumnType)]),
    Resolved(
        &'columns mut dyn FnMut(&mut Parser<'text>) -> Result<Option<usize>, GraphPatternTextError>,
    ),
}
enum ExpressionBoundary {
    Whole,
    Predicate,
    Operand,
}
enum ParsedOp {
    Atom(Operand, usize),
    Unary(GraphIntegerUnary),
    Binary(GraphIntegerBinary),
    Coalesce,
    Bound(GraphIntegerOp),
}

fn failure(at: usize, kind: GraphMutationTextErrorKind) -> GraphMutationTextError {
    GraphMutationTextError { offset: at, kind }
}
fn emit(
    program: &mut Vec<ParsedOp>,
    op: ParsedOp,
    at: usize,
) -> Result<(), GraphMutationTextError> {
    if program.len() >= MAX_GRAPH_INTEGER_INSTRUCTIONS {
        return Err(failure(
            at,
            GraphMutationTextErrorKind::IntegerExpression(
                GraphIntegerBuildError::TooManyInstructions {
                    limit: MAX_GRAPH_INTEGER_INSTRUCTIONS,
                    observed: program.len() + 1,
                },
            ),
        ));
    }
    program.push(op);
    Ok(())
}

pub(in crate::graph_text) fn bind_integer(
    program: &[MutationIntegerTemplateOp],
    values: &[GqlParameterValue],
    at: usize,
) -> Result<GraphIntegerExpression, GraphMutationTextError> {
    let mut ops = Vec::with_capacity(program.len());
    for op in program {
        ops.push(match op {
            MutationIntegerTemplateOp::Bound(op) => op.clone(),
            MutationIntegerTemplateOp::Parameter { index, at } => {
                let value = values
                    .get(*index)
                    .ok_or_else(|| failure(*at, GraphMutationTextErrorKind::IntegerOperand))?;
                GraphIntegerOp::Scalar(
                    scalar(value.clone(), *at)?.predicate(IntegerComparison::Equal),
                )
            }
        });
    }
    GraphIntegerExpression::prepare_scalar(&ops)
        .map_err(|error| failure(at, GraphMutationTextErrorKind::IntegerExpression(error)))
}

impl<'a> Parser<'a> {
    pub(in crate::graph_text) fn bind_boolean_scalar(
        program: &[MutationIntegerTemplateOp],
        values: &[GqlParameterValue],
        at: usize,
    ) -> Result<GraphIntegerExpression, GraphPatternTextError> {
        bind_integer(program, values, at)
            .map_err(|source| error(source.offset, GraphPatternTextErrorKind::BooleanExpression))
    }

    #[allow(clippy::type_complexity)]
    pub(in crate::graph_text) fn boolean_scalar_expression(
        &mut self,
    ) -> Result<(Vec<(Name<'a>, Name<'a>)>, Vec<MutationIntegerTemplateOp>), GraphPatternTextError>
    {
        let at = self.current.at;
        let mut columns = Vec::new();
        let operand = self
            .checked_expression_with_boundary(
                &mut ExpressionColumns::Graph(&mut columns),
                ExpressionBoundary::Predicate,
            )
            .map_err(|source| error(source.offset, GraphPatternTextErrorKind::BooleanExpression))?;
        let program = match operand {
            Operand::Integer { program, .. } => program,
            Operand::Column(column) => vec![MutationIntegerTemplateOp::Bound(
                GraphIntegerOp::ScalarColumn(column),
            )],
            Operand::Literal(value) => vec![MutationIntegerTemplateOp::Bound(
                GraphIntegerOp::Scalar(value.predicate(IntegerComparison::Equal)),
            )],
            Operand::Number(Number::Literal(value)) => vec![MutationIntegerTemplateOp::Bound(
                GraphIntegerOp::Scalar(scalar(value, at)?.predicate(IntegerComparison::Equal)),
            )],
            Operand::Number(Number::Parameter(index)) => {
                vec![MutationIntegerTemplateOp::Parameter { index, at }]
            }
        };
        let columns = columns
            .into_iter()
            .map(|column| {
                column
                    .property
                    .map(|key| (column.variable, key))
                    .ok_or_else(|| error(at, GraphPatternTextErrorKind::BooleanExpression))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok((columns, program))
    }

    pub(super) fn mutation_expression(
        &mut self,
        columns: &mut Vec<Projection<'a>>,
    ) -> Result<Operand, GraphMutationTextError> {
        self.checked_expression(&mut ExpressionColumns::Graph(columns), true)
    }

    pub(super) fn aggregate_value_expression(
        &mut self,
        columns: &mut Vec<Projection<'a>>,
    ) -> Result<Operand, GraphMutationTextError> {
        // HAVING owns predicates around grouping keys and aggregate arguments.
        self.checked_expression(&mut ExpressionColumns::Graph(columns), false)
    }

    pub(super) fn row_expression(
        &mut self,
        columns: &[(Name<'a>, crate::GraphSetColumnType)],
    ) -> Result<Operand, GraphMutationTextError> {
        self.checked_expression(&mut ExpressionColumns::Row(columns), true)
    }

    pub(super) fn resolved_expression(
        &mut self,
        resolve: &mut dyn FnMut(&mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError>,
    ) -> Result<Operand, GraphMutationTextError> {
        self.checked_expression(&mut ExpressionColumns::Resolved(resolve), false)
    }

    fn checked_expression(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        predicates: bool,
    ) -> Result<Operand, GraphMutationTextError> {
        self.checked_expression_with_boundary(
            columns,
            if predicates {
                ExpressionBoundary::Whole
            } else {
                ExpressionBoundary::Operand
            },
        )
    }

    fn checked_expression_with_boundary(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        boundary: ExpressionBoundary,
    ) -> Result<Operand, GraphMutationTextError> {
        let at = self.current.at;
        let mut parsed = Vec::new();
        match boundary {
            ExpressionBoundary::Whole => self.scalar_boolean(columns, 0, &mut parsed)?,
            // The graph Boolean parser owns AND/OR between its leaves. Consuming
            // them here would regroup A AND scalar(B) OR C as A AND (B OR C).
            // Parentheses still parse their complete internal Boolean expression.
            ExpressionBoundary::Predicate => self.scalar_negation(columns, 0, &mut parsed)?,
            // HAVING owns comparisons and Boolean operators around each operand.
            ExpressionBoundary::Operand => self.scalar_concat(columns, 0, &mut parsed)?,
        }
        if parsed.len() == 1 {
            let ParsedOp::Atom(value, _) = parsed.pop().expect("one parsed operand") else {
                unreachable!("operators and CASE also contain their operands")
            };
            // Preserve all prior scalar assignments and their transcript bytes.
            // An arithmetic operator/function, not parentheses, selects i64.
            return Ok(value);
        }
        let mut program = Vec::with_capacity(parsed.len());
        for op in parsed {
            program.push(match op {
                ParsedOp::Bound(op) => MutationIntegerTemplateOp::Bound(op),
                ParsedOp::Unary(op) => MutationIntegerTemplateOp::Bound(GraphIntegerOp::Unary(op)),
                ParsedOp::Binary(op) => {
                    MutationIntegerTemplateOp::Bound(GraphIntegerOp::Binary(op))
                }
                ParsedOp::Coalesce => MutationIntegerTemplateOp::Bound(GraphIntegerOp::Coalesce),
                ParsedOp::Atom(Operand::Column(column), at) => {
                    // Refuse only columns that can never hold a scalar. A
                    // dynamically typed column (an UNWIND element, an Any
                    // projection) is checked per value by the evaluator, which
                    // answers a typed NonScalar/NonInteger error, never a panic.
                    if let ExpressionColumns::Row(schema) = columns
                        && !matches!(
                            schema[column].1,
                            crate::GraphSetColumnType::Scalar | crate::GraphSetColumnType::Any
                        )
                    {
                        return Err(failure(at, GraphMutationTextErrorKind::IntegerOperand));
                    }
                    MutationIntegerTemplateOp::Bound(GraphIntegerOp::ScalarColumn(column))
                }
                ParsedOp::Atom(Operand::Literal(value), _) => MutationIntegerTemplateOp::Bound(
                    GraphIntegerOp::Scalar(value.predicate(IntegerComparison::Equal)),
                ),
                ParsedOp::Atom(Operand::Number(Number::Literal(value)), at) => {
                    MutationIntegerTemplateOp::Bound(GraphIntegerOp::Scalar(
                        scalar(value, at)?.predicate(IntegerComparison::Equal),
                    ))
                }
                ParsedOp::Atom(Operand::Number(Number::Parameter(index)), at) => {
                    // This compiler also serves text and Boolean expressions.
                    // Binding substitutes the declared scalar and validates its
                    // operator context with GraphIntegerExpression::prepare_scalar.
                    let parameter_type = self.syntax.parameters[index].parameter_type;
                    if !matches!(
                        parameter_type,
                        GqlParameterType::Int64 | GqlParameterType::Scalar(_)
                    ) {
                        return Err(failure(at, GraphMutationTextErrorKind::IntegerOperand));
                    }
                    MutationIntegerTemplateOp::Parameter { index, at }
                }
                ParsedOp::Atom(Operand::Integer { .. }, _) => {
                    unreachable!("atoms never recurse into expression preparation")
                }
            });
        }
        // Validate declared scalar kinds before catalog access. These values
        // are type witnesses only; preparation never executes the program.
        let mut noninteger_parameter = None;
        let shape: Vec<_> = program
            .iter()
            .map(|op| match op {
                MutationIntegerTemplateOp::Bound(op) => Ok(op.clone()),
                MutationIntegerTemplateOp::Parameter { index, at } => {
                    let kind = self.syntax.parameters[*index].parameter_type;
                    let witness = match kind {
                        GqlParameterType::Int64
                        | GqlParameterType::Scalar(CanonicalScalarKind::Int) => {
                            GraphIntegerOp::Literal(Some(0))
                        }
                        GqlParameterType::Scalar(CanonicalScalarKind::Null) => {
                            GraphIntegerOp::Literal(None)
                        }
                        GqlParameterType::Scalar(CanonicalScalarKind::Bool) => {
                            noninteger_parameter.get_or_insert(*at);
                            GraphIntegerOp::Truth(Some(false))
                        }
                        GqlParameterType::Scalar(CanonicalScalarKind::Text) => {
                            noninteger_parameter.get_or_insert(*at);
                            GraphIntegerOp::Scalar(
                                crate::GqlScalarParameter::new(
                                    CanonicalScalar::ucs_basic_text("")
                                        .expect("empty text is canonical"),
                                )
                                .expect("empty text is an admitted scalar")
                                .predicate(IntegerComparison::Equal),
                            )
                        }
                        _ => return Err(failure(*at, GraphMutationTextErrorKind::IntegerOperand)),
                    };
                    Ok(witness)
                }
            })
            .collect::<Result<_, _>>()?;
        GraphIntegerExpression::prepare_scalar(&shape).map_err(|error| {
            if matches!(error, GraphIntegerBuildError::OperandType { .. })
                && let Some(parameter_at) = noninteger_parameter
            {
                failure(parameter_at, GraphMutationTextErrorKind::IntegerOperand)
            } else {
                failure(at, GraphMutationTextErrorKind::IntegerExpression(error))
            }
        })?;
        Ok(Operand::Integer { program, at })
    }

    fn scalar_boolean(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        self.scalar_conjunction(columns, depth, program)?;
        while self.take_word("OR")? {
            let at = self.current.at;
            self.scalar_conjunction(columns, depth, program)?;
            emit(program, ParsedOp::Bound(GraphIntegerOp::Or), at)?;
        }
        Ok(())
    }

    fn scalar_conjunction(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        self.scalar_negation(columns, depth, program)?;
        while self.is_word("AND") {
            // Scoped EXISTS is owned by the graph clause parser.
            let mut lexer = self.lexer.clone();
            let mut next = lexer.next()?;
            if matches!(next.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("NOT")) {
                next = lexer.next()?;
            }
            if matches!(next.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("EXISTS")) {
                break;
            }
            self.advance()?;
            let at = self.current.at;
            self.scalar_negation(columns, depth, program)?;
            emit(program, ParsedOp::Bound(GraphIntegerOp::And), at)?;
        }
        Ok(())
    }

    fn scalar_negation(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        if depth > MAX_INTEGER_NESTING {
            return Err(failure(
                self.current.at,
                GraphMutationTextErrorKind::IntegerNesting {
                    limit: MAX_INTEGER_NESTING,
                },
            ));
        }
        if self.is_word("NOT") && !matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'.'))
        {
            let at = self.current.at;
            self.advance()?;
            self.scalar_negation(columns, depth + 1, program)?;
            return emit(program, ParsedOp::Bound(GraphIntegerOp::Not), at);
        }
        self.scalar_comparison(columns, depth, program)
    }

    fn scalar_comparison(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        self.scalar_concat(columns, depth, program)?;
        loop {
            let at = self.current.at;
            let op = if self.take_word("STARTS")? {
                self.word("WITH")?;
                Some(GraphIntegerOp::StartsWith)
            } else if self.take_word("ENDS")? {
                self.word("WITH")?;
                Some(GraphIntegerOp::EndsWith)
            } else if self.take_word("CONTAINS")? {
                Some(GraphIntegerOp::Contains)
            } else {
                None
            };
            if let Some(op) = op {
                self.scalar_concat(columns, depth, program)?;
                emit(program, ParsedOp::Bound(op), at)?;
                continue;
            }
            if self.is_word("IN")
                || (self.is_word("NOT")
                    && matches!(self.lexer.clone().next()?.kind,
                TokenKind::Word(word) if word.eq_ignore_ascii_case("IN")))
            {
                let negate = self.take_word("NOT")?;
                self.word("IN")?;
                self.punct(b'[', "[")?;
                let mut members = 0;
                if !self.take(b']')? {
                    loop {
                        self.scalar_concat(columns, depth + 1, program)?;
                        members += 1;
                        if self.take(b']')? {
                            break;
                        }
                        self.punct(b',', ",")?;
                    }
                }
                emit(
                    program,
                    ParsedOp::Bound(GraphIntegerOp::InList { members }),
                    at,
                )?;
                if negate {
                    emit(program, ParsedOp::Bound(GraphIntegerOp::Not), at)?;
                }
                continue;
            }
            if self.take_word("IS")? {
                let negate = self.take_word("NOT")?;
                self.word("NULL")?;
                emit(
                    program,
                    ParsedOp::Bound(GraphIntegerOp::IsNull(!negate)),
                    at,
                )?;
                continue;
            }
            if matches!(
                self.current.kind,
                TokenKind::Punct(b'=' | b'!' | b'<' | b'>')
            ) {
                let comparison = self.comparison()?;
                self.scalar_concat(columns, depth, program)?;
                emit(
                    program,
                    ParsedOp::Bound(GraphIntegerOp::Compare(comparison)),
                    at,
                )?;
                continue;
            }
            break;
        }
        Ok(())
    }

    fn scalar_concat(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        self.integer_sum(columns, depth, program)?;
        while self.take(b'|')? {
            let at = self.current.at;
            self.punct(b'|', "||")?;
            self.integer_sum(columns, depth, program)?;
            emit(program, ParsedOp::Bound(GraphIntegerOp::Concat), at)?;
        }
        Ok(())
    }

    fn integer_sum(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        self.integer_product(columns, depth, program)?;
        loop {
            let at = self.current.at;
            let op = if self.take(b'+')? {
                GraphIntegerBinary::Add
            } else if self.take(b'-')? {
                GraphIntegerBinary::Subtract
            } else {
                break;
            };
            self.integer_product(columns, depth, program)?;
            emit(program, ParsedOp::Binary(op), at)?;
        }
        Ok(())
    }
    fn integer_product(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        self.integer_unary(columns, depth, program)?;
        loop {
            let at = self.current.at;
            let op = if self.take(b'*')? {
                GraphIntegerBinary::Multiply
            } else if self.take(b'/')? {
                GraphIntegerBinary::Divide
            } else if self.take(b'%')? {
                GraphIntegerBinary::Remainder
            } else {
                break;
            };
            self.integer_unary(columns, depth, program)?;
            emit(program, ParsedOp::Binary(op), at)?;
        }
        Ok(())
    }
    fn integer_unary(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        let at = self.current.at;
        if depth > MAX_INTEGER_NESTING {
            return Err(failure(
                at,
                GraphMutationTextErrorKind::IntegerNesting {
                    limit: MAX_INTEGER_NESTING,
                },
            ));
        }
        if self.starts_integer_case()? {
            self.advance()?;
            return self.integer_case(columns, depth + 1, program, at);
        }
        // -9223372036854775808 is one legal signed literal, not negation of
        // an unrepresentable positive integer. Other signs are checked unary IR.
        let signed_literal =
            self.is_punct(b'-') && matches!(self.lexer.clone().next()?.kind, TokenKind::Digits(_));
        if (self.is_punct(b'+') || self.is_punct(b'-')) && !signed_literal {
            let op = if self.is_punct(b'+') {
                GraphIntegerUnary::Plus
            } else {
                GraphIntegerUnary::Negate
            };
            self.advance()?;
            self.integer_unary(columns, depth + 1, program)?;
            return emit(program, ParsedOp::Unary(op), at);
        }
        if self.take(b'(')? {
            self.scalar_boolean(columns, depth + 1, program)?;
            self.punct(b')', ")")?;
            return Ok(());
        }
        let function = if (self.is_word("ABS")
            || self.is_word("COALESCE")
            || self.is_word("NULLIF")
            || self.is_word("UPPER")
            || self.is_word("LOWER")
            || self.is_word("TRIM")
            || self.is_word("CHAR_LENGTH")
            || self.is_word("SUBSTRING"))
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
        {
            let TokenKind::Word(name) = self.current.kind else {
                unreachable!("checked function word")
            };
            Some(name)
        } else {
            None
        };
        if let Some(function) = function {
            self.advance()?;
            self.punct(b'(', "(")?;
            self.scalar_concat(columns, depth + 1, program)?;
            let text_op = if function.eq_ignore_ascii_case("UPPER") {
                Some(GraphIntegerOp::Upper)
            } else if function.eq_ignore_ascii_case("LOWER") {
                Some(GraphIntegerOp::Lower)
            } else if function.eq_ignore_ascii_case("TRIM") {
                Some(GraphIntegerOp::Trim)
            } else if function.eq_ignore_ascii_case("CHAR_LENGTH") {
                Some(GraphIntegerOp::CharLength)
            } else {
                None
            };
            if let Some(op) = text_op {
                self.punct(b')', ")")?;
                return emit(program, ParsedOp::Bound(op), at);
            }
            if function.eq_ignore_ascii_case("SUBSTRING") {
                let sql = self.take_word("FROM")?;
                if !sql {
                    self.punct(b',', ", or FROM")?;
                }
                self.scalar_concat(columns, depth + 1, program)?;
                if sql {
                    self.word("FOR")?;
                } else {
                    self.punct(b',', ",")?;
                }
                self.scalar_concat(columns, depth + 1, program)?;
                self.punct(b')', ")")?;
                return emit(program, ParsedOp::Bound(GraphIntegerOp::Substring), at);
            }
            if function.eq_ignore_ascii_case("ABS") {
                self.punct(b')', ")")?;
                return emit(program, ParsedOp::Unary(GraphIntegerUnary::Abs), at);
            }
            self.punct(b',', ",")?;
            self.scalar_concat(columns, depth + 1, program)?;
            if function.eq_ignore_ascii_case("NULLIF") {
                self.punct(b')', ")")?;
                return emit(program, ParsedOp::Binary(GraphIntegerBinary::NullIf), at);
            }
            emit(program, ParsedOp::Coalesce, at)?;
            while self.take(b',')? {
                self.scalar_concat(columns, depth + 1, program)?;
                emit(program, ParsedOp::Coalesce, at)?;
            }
            self.punct(b')', ")")?;
            return Ok(());
        }
        let operand = match columns {
            ExpressionColumns::Graph(columns) => self.mutation_operand(columns)?,
            ExpressionColumns::Row(schema) => self.row_operand(schema)?,
            ExpressionColumns::Resolved(resolve) => match resolve(self)? {
                Some(column) => Operand::Column(column),
                None => self.row_operand(&[])?,
            },
        };
        emit(program, ParsedOp::Atom(operand, at), at)
    }

    pub(super) fn row_operand(
        &mut self,
        schema: &[(Name<'a>, crate::GraphSetColumnType)],
    ) -> Result<Operand, GraphPatternTextError> {
        // `n.p` of a carried MATCH vertex inside a graph-to-row WITH's scope
        // reads its hidden boundary column (fgdb-1tgko); the private names of
        // those columns never resolve as text.
        if let Some(column) = self.boundary_read(schema.len())? {
            return Ok(Operand::Column(column));
        }
        if let TokenKind::Word(word) = self.current.kind {
            let visible = self.visible_width(schema);
            if let Some(column) = schema[..visible]
                .iter()
                .position(|(name, _)| name.text == word)
            {
                self.advance()?;
                return Ok(Operand::Column(column));
            }
            if !(word.eq_ignore_ascii_case("TRUE")
                || word.eq_ignore_ascii_case("FALSE")
                || word.eq_ignore_ascii_case("NULL"))
                || matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'.'))
            {
                return Err(error(
                    self.current.at,
                    GraphPatternTextErrorKind::UnknownVariable,
                ));
            }
        }
        // Only literals/parameters remain. In particular, no root variable or
        // property lookup can reach the graph operand branch of this helper.
        self.mutation_operand(&mut Vec::new())
    }
}

#[cfg(test)]
mod predicate_boundary_tests {
    use super::*;
    use crate::GqlQueryPolicy;
    use fgdb_types::VId;

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            _ => None,
        }
    }

    fn rows(predicate: &str) -> Vec<VId> {
        let p = PropertyKeyId(1);
        let q = PropertyKeyId(2);
        let values = [
            vec![(p, CanonicalScalar::Int(0)), (q, CanonicalScalar::Int(1))],
            vec![(p, CanonicalScalar::Int(1)), (q, CanonicalScalar::Int(0))],
            vec![(p, CanonicalScalar::Int(2)), (q, CanonicalScalar::Int(2))],
            vec![(p, CanonicalScalar::Null), (q, CanonicalScalar::Int(1))],
            vec![(q, CanonicalScalar::Int(1))],
        ];
        let pattern =
            PreparedGraphText::prepare(&format!("MATCH (n) WHERE {predicate} RETURN n"), symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
        pattern
            .plan()
            .execute_governed_with_properties(
                5,
                (1..=5).map(VId),
                [],
                |vid, predicate| {
                    Ok::<_, ()>(
                        predicate
                            .iter()
                            .all(|p| p.matches(&[], &values[vid.0 as usize - 1])),
                    )
                },
                |vid, key| {
                    Ok(values[vid.0 as usize - 1]
                        .iter()
                        .find(|(property, _)| *property == key)
                        .map(|(_, value)| value))
                },
                GqlQueryPolicy::new(100, 100, 100_000, 100_000),
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value
            .iter()
            .map(|row| row.get(0).unwrap().as_vertex().unwrap())
            .collect()
    }

    #[test]
    fn scalar_where_leaves_do_not_swallow_outer_conjunctions_or_disjunctions() {
        for (predicate, expected) in [
            ("n.p=1 AND n.q+1=1 OR n.p=0", vec![VId(1), VId(2)]),
            ("n.p=1 AND (n.q+1=1 OR n.p=0)", vec![VId(2)]),
            ("NOT n.p=1 AND n.q+1=1 OR n.p=0", vec![VId(1)]),
            ("n.p=1 AND NOT n.q+1=1 OR n.p=0", vec![VId(1)]),
            (
                "n.q+1=1 AND n.p BETWEEN 0 AND 1 OR n.p=2",
                vec![VId(2), VId(3)],
            ),
            (
                "n.p=1 AND n.q+1=1 OR n.p IS NULL",
                vec![VId(2), VId(4), VId(5)],
            ),
        ] {
            assert_eq!(rows(predicate), expected, "{predicate}");
        }
    }

    #[test]
    fn graph_boolean_program_keeps_separate_scalar_leaves_and_operator_order() {
        use crate::graph_text::boolean::SyntaxItem;
        let syntax = Parser::new("MATCH (n) WHERE n.p+1=1 AND n.q+1=1 OR n.p=2 RETURN n")
            .unwrap()
            .parse()
            .unwrap();
        let [Filter::Boolean { program, .. }] = syntax.filters.as_slice() else {
            panic!("one graph Boolean program");
        };
        assert!(matches!(
            program.as_slice(),
            [
                SyntaxItem::Expression { .. },
                SyntaxItem::Expression { .. },
                SyntaxItem::And,
                SyntaxItem::Atom(Filter::Property { .. }),
                SyntaxItem::Or,
            ]
        ));
    }

    #[test]
    fn assignment_and_row_expressions_still_parse_the_whole_boolean_expression() {
        let text = "1=1 AND 2=2 OR 3=4";
        let mut predicate = Parser::new(text).unwrap();
        predicate.boolean_scalar_expression().unwrap();
        assert!(predicate.is_word("AND"));
        for row_scope in [false, true] {
            let mut parser = Parser::new(text).unwrap();
            let operand = if row_scope {
                parser.row_expression(&[]).unwrap()
            } else {
                parser.mutation_expression(&mut Vec::new()).unwrap()
            };
            assert!(matches!(parser.current.kind, TokenKind::End));
            let Operand::Integer { program, .. } = operand else {
                panic!("whole Boolean expression bytecode");
            };
            assert!(matches!(
                program.last(),
                Some(MutationIntegerTemplateOp::Bound(GraphIntegerOp::Or))
            ));
        }
    }
}
