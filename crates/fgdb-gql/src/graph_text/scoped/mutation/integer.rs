//! Precedence-aware integer RHS lowering on the shared MATCH lexer.
//! Only preparation sees these temporary nodes. Binding substitutes typed
//! operands into checked bytecode, never into source text or an AST evaluator.

mod conditional;

use super::*;
use crate::{GraphIntegerBinary, GraphIntegerBuildError, GraphIntegerExpression,
    GraphIntegerOp, GraphIntegerUnary, MAX_GRAPH_INTEGER_INSTRUCTIONS};
use fgdb_types::CanonicalScalarKind;

const MAX_INTEGER_NESTING: usize = 64;
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
fn integer_scalar(value: &CanonicalScalar, at: usize) -> Result<Option<i64>, GraphMutationTextError> {
    match value {
        CanonicalScalar::Int(value) => Ok(Some(*value)),
        CanonicalScalar::Null => Ok(None),
        _ => Err(failure(at, GraphMutationTextErrorKind::IntegerOperand)),
    }
}
fn integer_value(value: &GqlParameterValue, at: usize) -> Result<Option<i64>, GraphMutationTextError> {
    match value {
        GqlParameterValue::Int64(value) => Ok(Some(*value)),
        GqlParameterValue::Scalar(value) => integer_scalar(value.value(), at),
        GqlParameterValue::UInt64(_) => Err(failure(at, GraphMutationTextErrorKind::IntegerOperand)),
    }
}

pub(super) fn bind_integer(program: &[MutationIntegerTemplateOp], values: &[GqlParameterValue], at: usize)
    -> Result<GraphIntegerExpression, GraphMutationTextError> {
    let mut ops = Vec::with_capacity(program.len());
    for op in program {
        ops.push(match op {
            MutationIntegerTemplateOp::Bound(op) => *op,
            MutationIntegerTemplateOp::Parameter { index, at } => {
                let value = values.get(*index).ok_or_else(|| failure(*at, GraphMutationTextErrorKind::IntegerOperand))?;
                GraphIntegerOp::Literal(integer_value(value, *at)?)
            }
        });
    }
    GraphIntegerExpression::prepare(&ops)
        .map_err(|error| failure(at, GraphMutationTextErrorKind::IntegerExpression(error)))
}

impl<'a> Parser<'a> {
    pub(super) fn mutation_expression(&mut self, columns: &mut Vec<Projection<'a>>)
        -> Result<Operand, GraphMutationTextError> {
        let at = self.current.at;
        let mut parsed = Vec::new();
        self.integer_sum(columns, 0, &mut parsed)?;
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
                ParsedOp::Atom(Operand::Column(column), _) => MutationIntegerTemplateOp::Bound(GraphIntegerOp::Column(column)),
                ParsedOp::Atom(Operand::Literal(value), at) =>
                    MutationIntegerTemplateOp::Bound(GraphIntegerOp::Literal(integer_scalar(value.value(), at)?)),
                ParsedOp::Atom(Operand::Number(Number::Literal(value)), at) =>
                    MutationIntegerTemplateOp::Bound(GraphIntegerOp::Literal(integer_value(&value, at)?)),
                ParsedOp::Atom(Operand::Number(Number::Parameter(index)), at) => {
                    let kind = self.syntax.parameters[index].parameter_type;
                    let integer = match kind {
                        GqlParameterType::Int64 => true,
                        GqlParameterType::Scalar(kind) => kind == CanonicalScalarKind::of(&CanonicalScalar::Int(0))
                            || kind == CanonicalScalarKind::of(&CanonicalScalar::Null),
                        GqlParameterType::UInt64 => false,
                    };
                    if !integer { return Err(failure(at, GraphMutationTextErrorKind::IntegerOperand)); }
                    MutationIntegerTemplateOp::Parameter { index, at }
                }
                ParsedOp::Atom(Operand::Integer { .. }, _) => unreachable!("atoms never recurse into expression preparation"),
            });
        }
        // Null placeholders prove stack/types/control-flow only. Preparation
        // neither evaluates branches nor guesses a parameter's runtime value.
        let shape: Vec<_> = program.iter().map(|op| match op {
            MutationIntegerTemplateOp::Bound(op) => *op,
            MutationIntegerTemplateOp::Parameter { .. } => GraphIntegerOp::Literal(None),
        }).collect();
        GraphIntegerExpression::prepare(&shape)
            .map_err(|error| failure(at, GraphMutationTextErrorKind::IntegerExpression(error)))?;
        Ok(Operand::Integer { program, at })
    }

    fn integer_sum(&mut self, columns: &mut Vec<Projection<'a>>, depth: usize, program: &mut Vec<ParsedOp>)
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
    fn integer_product(&mut self, columns: &mut Vec<Projection<'a>>, depth: usize, program: &mut Vec<ParsedOp>)
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
    fn integer_unary(&mut self, columns: &mut Vec<Projection<'a>>, depth: usize, program: &mut Vec<ParsedOp>)
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
            self.integer_sum(columns, depth + 1, program)?;
            self.punct(b')', ")")?;
            return Ok(());
        }
        let function = if (self.is_word("ABS") || self.is_word("COALESCE") || self.is_word("NULLIF"))
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'(')) {
            let TokenKind::Word(name) = self.current.kind else { unreachable!("checked function word") };
            Some(name)
        } else { None };
        if let Some(function) = function {
            self.advance()?;
            self.punct(b'(', "(")?;
            self.integer_sum(columns, depth + 1, program)?;
            if function.eq_ignore_ascii_case("ABS") {
                self.punct(b')', ")")?;
                return emit(program, ParsedOp::Unary(GraphIntegerUnary::Abs), at);
            }
            self.punct(b',', ",")?;
            self.integer_sum(columns, depth + 1, program)?;
            if function.eq_ignore_ascii_case("NULLIF") {
                self.punct(b')', ")")?;
                return emit(program, ParsedOp::Binary(GraphIntegerBinary::NullIf), at);
            }
            emit(program, ParsedOp::Coalesce, at)?;
            while self.take(b',')? {
                self.integer_sum(columns, depth + 1, program)?;
                emit(program, ParsedOp::Coalesce, at)?;
            }
            self.punct(b')', ")")?;
            return Ok(());
        }
        let operand = self.mutation_operand(columns)?;
        emit(program, ParsedOp::Atom(operand, at), at)
    }
}
