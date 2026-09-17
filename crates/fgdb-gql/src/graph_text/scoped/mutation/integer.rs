//! Precedence-aware integer RHS lowering on the shared MATCH lexer.
//! Only preparation sees these temporary nodes. Binding substitutes typed
//! operands into checked bytecode, never into source text or an AST evaluator.

mod conditional;

use super::*;
use crate::{GraphIntegerBinary, GraphIntegerBuildError, GraphIntegerExpression,
    GraphIntegerOp, GraphIntegerUnary, MAX_GRAPH_INTEGER_INSTRUCTIONS};

const MAX_INTEGER_NESTING: usize = 64;
// The precedence/CASE compiler is shared by graph assignments and relational
// stages. A row scope resolves only admitted aliases, never ambient graph names.
enum ExpressionColumns<'columns, 'text> {
    Graph(&'columns mut Vec<Projection<'text>>),
    Row(&'columns [(Name<'text>, crate::GraphSetColumnType)]),
}
enum ParsedOp {
    Atom(Operand, usize), Unary(GraphIntegerUnary), Binary(GraphIntegerBinary), Coalesce,
    Bound(GraphIntegerOp),
}

fn failure(at: usize, kind: GraphMutationTextErrorKind) -> GraphMutationTextError {
    GraphMutationTextError { offset: at, kind }
}
fn emit(program: &mut Vec<ParsedOp>, op: ParsedOp, at: usize) -> Result<(), GraphMutationTextError> {
    if program.len() >= MAX_GRAPH_INTEGER_INSTRUCTIONS {
        return Err(failure(at, GraphMutationTextErrorKind::IntegerExpression(
            GraphIntegerBuildError::TooManyInstructions { limit: MAX_GRAPH_INTEGER_INSTRUCTIONS,
                observed: program.len() + 1 })));
    }
    program.push(op);
    Ok(())
}

pub(in crate::graph_text) fn bind_integer(program: &[MutationIntegerTemplateOp], values: &[GqlParameterValue], at: usize)
    -> Result<GraphIntegerExpression, GraphMutationTextError> {
    let mut ops = Vec::with_capacity(program.len());
    for op in program {
        ops.push(match op {
            MutationIntegerTemplateOp::Bound(op) => op.clone(),
            MutationIntegerTemplateOp::Parameter { index, at } => {
                let value = values.get(*index).ok_or_else(|| failure(*at, GraphMutationTextErrorKind::IntegerOperand))?;
                GraphIntegerOp::Scalar(scalar(value.clone(), *at)?.predicate(IntegerComparison::Equal))
            }
        });
    }
    GraphIntegerExpression::prepare_scalar(&ops)
        .map_err(|error| failure(at, GraphMutationTextErrorKind::IntegerExpression(error)))
}

impl<'a> Parser<'a> {
    pub(in crate::graph_text) fn bind_boolean_scalar(program: &[MutationIntegerTemplateOp], values: &[GqlParameterValue], at: usize)
        -> Result<GraphIntegerExpression, GraphPatternTextError> {
        bind_integer(program, values, at).map_err(|source| error(source.offset, GraphPatternTextErrorKind::BooleanExpression))
    }

    pub(in crate::graph_text) fn boolean_scalar_expression(&mut self)
        -> Result<(Vec<(Name<'a>, Name<'a>)>, Vec<MutationIntegerTemplateOp>), GraphPatternTextError> {
        let at = self.current.at;
        let mut columns = Vec::new();
        let operand = self.mutation_expression(&mut columns)
            .map_err(|source| error(source.offset, GraphPatternTextErrorKind::BooleanExpression))?;
        let program = match operand {
            Operand::Integer { program, .. } => program,
            Operand::Column(column) => vec![MutationIntegerTemplateOp::Bound(GraphIntegerOp::ScalarColumn(column))],
            Operand::Literal(value) => vec![MutationIntegerTemplateOp::Bound(GraphIntegerOp::Scalar(value.predicate(IntegerComparison::Equal)))],
            Operand::Number(Number::Literal(value)) => vec![MutationIntegerTemplateOp::Bound(GraphIntegerOp::Scalar(scalar(value, at)?.predicate(IntegerComparison::Equal)))],
            Operand::Number(Number::Parameter(index)) => vec![MutationIntegerTemplateOp::Parameter { index, at }],
        };
        let columns = columns.into_iter().map(|column| column.property.map(|key| (column.variable, key))
            .ok_or_else(|| error(at, GraphPatternTextErrorKind::BooleanExpression))).collect::<Result<Vec<_>, _>>()?;
        Ok((columns, program))
    }

    pub(super) fn mutation_expression(&mut self, columns: &mut Vec<Projection<'a>>)
        -> Result<Operand, GraphMutationTextError> {
        self.checked_expression(&mut ExpressionColumns::Graph(columns))
    }

    pub(super) fn row_expression(&mut self, columns: &[(Name<'a>, crate::GraphSetColumnType)])
        -> Result<Operand, GraphMutationTextError> {
        self.checked_expression(&mut ExpressionColumns::Row(columns))
    }

    fn checked_expression(&mut self, columns: &mut ExpressionColumns<'_, 'a>)
        -> Result<Operand, GraphMutationTextError> {
        let at = self.current.at;
        let mut parsed = Vec::new();
        self.scalar_comparison(columns, 0, &mut parsed)?;
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
                ParsedOp::Binary(op) => MutationIntegerTemplateOp::Bound(GraphIntegerOp::Binary(op)),
                ParsedOp::Coalesce => MutationIntegerTemplateOp::Bound(GraphIntegerOp::Coalesce),
                ParsedOp::Atom(Operand::Column(column), at) => {
                    if let ExpressionColumns::Row(schema) = columns
                        && schema[column].1 != crate::GraphSetColumnType::Scalar {
                        return Err(failure(at, GraphMutationTextErrorKind::IntegerOperand));
                    }
                    MutationIntegerTemplateOp::Bound(GraphIntegerOp::ScalarColumn(column))
                }
                ParsedOp::Atom(Operand::Literal(value), _) =>
                    MutationIntegerTemplateOp::Bound(GraphIntegerOp::Scalar(value.predicate(IntegerComparison::Equal))),
                ParsedOp::Atom(Operand::Number(Number::Literal(value)), at) =>
                    MutationIntegerTemplateOp::Bound(GraphIntegerOp::Scalar(scalar(value, at)?.predicate(IntegerComparison::Equal))),
                ParsedOp::Atom(Operand::Number(Number::Parameter(index)), at) => {
                    if self.syntax.parameters[index].parameter_type == GqlParameterType::UInt64 {
                        return Err(failure(at, GraphMutationTextErrorKind::IntegerOperand));
                    }
                    MutationIntegerTemplateOp::Parameter { index, at }
                }
                ParsedOp::Atom(Operand::Integer { .. }, _) => unreachable!("atoms never recurse into expression preparation"),
            });
        }
        // Null placeholders prove stack/types/control-flow only. Preparation
        // neither evaluates branches nor guesses a parameter's runtime value.
        let shape: Vec<_> = program.iter().map(|op| match op {
            MutationIntegerTemplateOp::Bound(op) => op.clone(),
            MutationIntegerTemplateOp::Parameter { .. } => GraphIntegerOp::Literal(None),
        }).collect();
        GraphIntegerExpression::prepare_scalar(&shape)
            .map_err(|error| failure(at, GraphMutationTextErrorKind::IntegerExpression(error)))?;
        Ok(Operand::Integer { program, at })
    }

    fn scalar_comparison(&mut self, columns: &mut ExpressionColumns<'_, 'a>, depth: usize, program: &mut Vec<ParsedOp>)
        -> Result<(), GraphMutationTextError> {
        self.scalar_concat(columns, depth, program)?;
        loop {
            let at = self.current.at;
            let op = if self.take_word("STARTS")? {
                self.word("WITH")?; Some(GraphIntegerOp::StartsWith)
            } else if self.take_word("ENDS")? {
                self.word("WITH")?; Some(GraphIntegerOp::EndsWith)
            } else if self.take_word("CONTAINS")? { Some(GraphIntegerOp::Contains) }
            else { None };
            if let Some(op) = op {
                self.scalar_concat(columns, depth, program)?;
                emit(program, ParsedOp::Bound(op), at)?;
                continue;
            }
            if self.is_word("IN") || (self.is_word("NOT") && matches!(self.lexer.clone().next()?.kind,
                TokenKind::Word(word) if word.eq_ignore_ascii_case("IN"))) {
                let negate = self.take_word("NOT")?;
                self.word("IN")?;
                self.punct(b'[', "[")?;
                let mut members = 0;
                if !self.take(b']')? {
                    loop {
                        self.scalar_concat(columns, depth + 1, program)?;
                        members += 1;
                        if self.take(b']')? { break; }
                        self.punct(b',', ", or ]")?;
                    }
                }
                emit(program, ParsedOp::Bound(GraphIntegerOp::InList { members }), at)?;
                if negate { emit(program, ParsedOp::Bound(GraphIntegerOp::Not), at)?; }
                continue;
            }
            if self.take_word("IS")? {
                let negate = self.take_word("NOT")?;
                self.word("NULL")?;
                emit(program, ParsedOp::Bound(GraphIntegerOp::IsNull(!negate)), at)?;
                continue;
            }
            if matches!(self.current.kind, TokenKind::Punct(b'=' | b'!' | b'<' | b'>')) {
                let comparison = self.comparison()?;
                self.scalar_concat(columns, depth, program)?;
                emit(program, ParsedOp::Bound(GraphIntegerOp::Compare(comparison)), at)?;
                continue;
            }
            break;
        }
        Ok(())
    }

    fn scalar_concat(&mut self, columns: &mut ExpressionColumns<'_, 'a>, depth: usize, program: &mut Vec<ParsedOp>)
        -> Result<(), GraphMutationTextError> {
        self.integer_sum(columns, depth, program)?;
        while self.take(b'|')? {
            let at = self.current.at;
            self.punct(b'|', "||")?;
            self.integer_sum(columns, depth, program)?;
            emit(program, ParsedOp::Bound(GraphIntegerOp::Concat), at)?;
        }
        Ok(())
    }

    fn integer_sum(&mut self, columns: &mut ExpressionColumns<'_, 'a>, depth: usize, program: &mut Vec<ParsedOp>)
        -> Result<(), GraphMutationTextError> {
        self.integer_product(columns, depth, program)?;
        loop {
            let at = self.current.at;
            let op = if self.take(b'+')? { GraphIntegerBinary::Add }
                else if self.take(b'-')? { GraphIntegerBinary::Subtract }
                else { break; };
            self.integer_product(columns, depth, program)?;
            emit(program, ParsedOp::Binary(op), at)?;
        }
        Ok(())
    }
    fn integer_product(&mut self, columns: &mut ExpressionColumns<'_, 'a>, depth: usize, program: &mut Vec<ParsedOp>)
        -> Result<(), GraphMutationTextError> {
        self.integer_unary(columns, depth, program)?;
        loop {
            let at = self.current.at;
            let op = if self.take(b'*')? { GraphIntegerBinary::Multiply }
                else if self.take(b'/')? { GraphIntegerBinary::Divide }
                else if self.take(b'%')? { GraphIntegerBinary::Remainder }
                else { break; };
            self.integer_unary(columns, depth, program)?;
            emit(program, ParsedOp::Binary(op), at)?;
        }
        Ok(())
    }
    fn integer_unary(&mut self, columns: &mut ExpressionColumns<'_, 'a>, depth: usize, program: &mut Vec<ParsedOp>)
        -> Result<(), GraphMutationTextError> {
        let at = self.current.at;
        if depth > MAX_INTEGER_NESTING {
            return Err(failure(at, GraphMutationTextErrorKind::IntegerNesting { limit: MAX_INTEGER_NESTING }));
        }
        if self.starts_integer_case()? {
            self.advance()?;
            return self.integer_case(columns, depth + 1, program, at);
        }
        // -9223372036854775808 is one legal signed literal, not negation of
        // an unrepresentable positive integer. Other signs are checked unary IR.
        let signed_literal = self.is_punct(b'-')
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Digits(_));
        if (self.is_punct(b'+') || self.is_punct(b'-')) && !signed_literal {
            let op = if self.is_punct(b'+') { GraphIntegerUnary::Plus } else { GraphIntegerUnary::Negate };
            self.advance()?;
            self.integer_unary(columns, depth + 1, program)?;
            return emit(program, ParsedOp::Unary(op), at);
        }
        if self.take(b'(')? {
            self.scalar_comparison(columns, depth + 1, program)?;
            self.punct(b')', ")")?;
            return Ok(());
        }
        let function = if (self.is_word("ABS") || self.is_word("COALESCE") || self.is_word("NULLIF")
            || self.is_word("UPPER") || self.is_word("LOWER") || self.is_word("TRIM")
            || self.is_word("CHAR_LENGTH") || self.is_word("SUBSTRING"))
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'(')) {
            let TokenKind::Word(name) = self.current.kind else { unreachable!("checked function word") };
            Some(name)
        } else { None };
        if let Some(function) = function {
            self.advance()?;
            self.punct(b'(', "(")?;
            self.scalar_concat(columns, depth + 1, program)?;
            let text_op = if function.eq_ignore_ascii_case("UPPER") { Some(GraphIntegerOp::Upper) }
                else if function.eq_ignore_ascii_case("LOWER") { Some(GraphIntegerOp::Lower) }
                else if function.eq_ignore_ascii_case("TRIM") { Some(GraphIntegerOp::Trim) }
                else if function.eq_ignore_ascii_case("CHAR_LENGTH") { Some(GraphIntegerOp::CharLength) }
                else { None };
            if let Some(op) = text_op {
                self.punct(b')', ")")?;
                return emit(program, ParsedOp::Bound(op), at);
            }
            if function.eq_ignore_ascii_case("SUBSTRING") {
                let sql = self.take_word("FROM")?;
                if !sql { self.punct(b',', ", or FROM")?; }
                self.scalar_concat(columns, depth + 1, program)?;
                if sql { self.word("FOR")?; } else { self.punct(b',', ",")?; }
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
        };
        emit(program, ParsedOp::Atom(operand, at), at)
    }

    pub(super) fn row_operand(&mut self, schema: &[(Name<'a>, crate::GraphSetColumnType)])
        -> Result<Operand, GraphPatternTextError> {
        if let TokenKind::Word(word) = self.current.kind {
            if let Some(column) = schema.iter().position(|(name, _)| name.text == word) {
                self.advance()?;
                return Ok(Operand::Column(column));
            }
            if !(word.eq_ignore_ascii_case("TRUE") || word.eq_ignore_ascii_case("FALSE")
                || word.eq_ignore_ascii_case("NULL"))
                || matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'.')) {
                return Err(error(self.current.at, GraphPatternTextErrorKind::UnknownVariable));
            }
        }
        // Only literals/parameters remain. In particular, no root variable or
        // property lookup can reach the graph operand branch of this helper.
        self.mutation_operand(&mut Vec::new())
    }
}
