//! Checked nullable integer expressions over a frozen, typed value row.
//!
//! Preparation consumes bounded, typed postfix IR and emits private linear
//! bytecode. CASE and COALESCE select branches lazily; already-projected inputs
//! retain the enclosing GLA source/error contract. Arithmetic is signed i64,
//! division truncates toward zero, and overflow/zero division refuse. Boolean
//! conditions are a separate compile-time domain, never integer truthiness.
//! No float, decimal, string, or vertex-to-integer coercion is performed.

mod compile;

use crate::GlaExecutionEvent;
use crate::algebra::{GraphValue, IntegerComparison};
use fgdb_types::CanonicalScalar;

pub const MAX_GRAPH_INTEGER_INSTRUCTIONS: usize = 1_024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphIntegerUnary { Plus, Negate, Abs }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphIntegerBinary { Add, Subtract, Multiply, Divide, Remainder, NullIf }

/// Postfix construction IR. Arithmetic operands/results are nullable integers.
/// Truth, Compare, IsNull, Not, And and Or produce private Boolean conditions.
/// Case consumes (condition, then_integer, else_integer); only the selected
/// result executes. Nest Case in the else operand for ordered WHEN clauses.
/// SimpleCase consumes (selector, when, then, ..., default), evaluates its
/// selector once, and selects the first nonnull equality. Conditions use eager
/// three-valued Boolean evaluation; unselected CASE arms are not executed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GraphIntegerOp {
    Column(usize),
    Literal(Option<i64>),
    Unary(GraphIntegerUnary),
    Binary(GraphIntegerBinary),
    Coalesce,
    Truth(Option<bool>),
    Compare(IntegerComparison),
    IsNull(bool),
    Not,
    And,
    Or,
    Case,
    SimpleCase { alternatives: usize },
}
impl core::fmt::Debug for GraphIntegerOp {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Column(_) => f.write_str("Column([REDACTED])"),
            Self::Literal(_) => f.write_str("Literal([REDACTED])"),
            Self::Unary(op) => op.fmt(f),
            Self::Binary(op) => op.fmt(f),
            Self::Coalesce => f.write_str("Coalesce"),
            Self::Truth(_) => f.write_str("Truth([REDACTED])"),
            Self::Compare(op) => op.fmt(f),
            Self::IsNull(_) => f.write_str("IsNull"),
            Self::Not => f.write_str("Not"),
            Self::And => f.write_str("And"),
            Self::Or => f.write_str("Or"),
            Self::Case => f.write_str("Case"),
            Self::SimpleCase { alternatives } => f.debug_struct("SimpleCase").field("alternatives", alternatives).finish(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphIntegerBuildError {
    Empty,
    TooManyInstructions { limit: usize, observed: usize },
    MissingOperand { instruction: usize },
    ExtraOperands { remaining: usize },
    OperandType { instruction: usize },
    EmptyCase { instruction: usize },
}
impl core::fmt::Display for GraphIntegerBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "integer expression definition: {self:?}")
    }
}
impl core::error::Error for GraphIntegerBuildError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphIntegerErrorKind { MissingColumn, NonInteger, Overflow, DivisionByZero }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphIntegerError {
    pub instruction: usize,
    pub kind: GraphIntegerErrorKind,
}
impl core::fmt::Display for GraphIntegerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "integer expression {:?} at instruction {}", self.kind, self.instruction)
    }
}
impl core::error::Error for GraphIntegerError {}

#[derive(Debug)]
pub enum GraphIntegerEvaluationError<E> { Control(E), Value(GraphIntegerError) }
impl<E: core::fmt::Display> core::fmt::Display for GraphIntegerEvaluationError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Control(error) => error.fmt(f),
            Self::Value(error) => error.fmt(f),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for GraphIntegerEvaluationError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self { Self::Control(error) => Some(error), Self::Value(error) => Some(error) }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum Instruction {
    Column(usize), Literal(Option<i64>), Unary(GraphIntegerUnary),
    Binary(GraphIntegerBinary), JumpIfPresent(usize),
    Truth(Option<bool>), Compare(IntegerComparison), IsNull(bool), Not, And, Or,
    JumpUnlessTrue(usize), JumpUnlessEqual(usize), Jump(usize), Drop,
}

/// Immutable checked scalar program. It owns no database, parameter map,
/// authority, source callback or mutable execution frame.
#[derive(Clone, PartialEq, Eq)]
pub struct GraphIntegerExpression {
    code: Box<[Instruction]>,
    stack_entries: usize,
}
impl core::fmt::Debug for GraphIntegerExpression {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphIntegerExpression")
            .field("instructions", &self.code.len())
            .field("definition", &"[REDACTED]").finish()
    }
}
impl GraphIntegerExpression {
    /// Validate all stack shapes and operand types, including unreachable
    /// branches, before compiling. Compilation and evaluation are iterative.
    /// The final result must be integer/null; Boolean-to-integer casts are not
    /// implicit. Existing arithmetic-only definitions retain their bytecode.
    pub fn prepare(ops: &[GraphIntegerOp]) -> Result<Self, GraphIntegerBuildError> {
        compile::prepare(ops)
    }

    pub fn referenced_columns(&self) -> impl Iterator<Item = usize> + '_ {
        self.code.iter().filter_map(|op| match op {
            Instruction::Column(column) => Some(*column), _ => None,
        })
    }

    /// Reserve the finite private frame before allocation, then checkpoint
    /// every executed instruction, including branches. Unselected CASE arms
    /// and COALESCE fallbacks do no arithmetic or column lookup here. The
    /// enclosing selection still owns eager storage reads and source failures.
    pub fn evaluate_with_control<E>(
        &self, values: &[GraphValue],
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<i64>, GraphIntegerEvaluationError<E>> {
        for _ in 0..self.stack_entries {
            control(GlaExecutionEvent::ScratchEntry).map_err(GraphIntegerEvaluationError::Control)?;
        }
        // Static admission proves each cell's domain. Conditions use only
        // None/0/1 internally and can never be consumed by integer arithmetic.
        let mut stack: Vec<Option<i64>> = Vec::with_capacity(self.stack_entries);
        let mut at = 0;
        while let Some(op) = self.code.get(at) {
            control(GlaExecutionEvent::Work).map_err(GraphIntegerEvaluationError::Control)?;
            let failure = |kind| GraphIntegerEvaluationError::Value(GraphIntegerError { instruction: at, kind });
            match op {
                Instruction::Column(column) => {
                    let value = match values.get(*column) {
                        Some(GraphValue::Scalar(CanonicalScalar::Int(value))) => Some(*value),
                        Some(GraphValue::Scalar(CanonicalScalar::Null)) => None,
                        Some(_) => return Err(failure(GraphIntegerErrorKind::NonInteger)),
                        None => return Err(failure(GraphIntegerErrorKind::MissingColumn)),
                    };
                    stack.push(value);
                }
                Instruction::Literal(value) => stack.push(*value),
                Instruction::Unary(op) => {
                    let value = stack.last_mut().expect("validated unary stack");
                    if let Some(number) = *value {
                        *value = Some(match op {
                            GraphIntegerUnary::Plus => Some(number),
                            GraphIntegerUnary::Negate => number.checked_neg(),
                            GraphIntegerUnary::Abs => number.checked_abs(),
                        }.ok_or_else(|| failure(GraphIntegerErrorKind::Overflow))?);
                    }
                }
                Instruction::Binary(op) => {
                    let right = stack.pop().expect("validated right operand");
                    let left = stack.last_mut().expect("validated left operand");
                    *left = apply_binary(*op, *left, right).map_err(failure)?;
                }
                Instruction::JumpIfPresent(target) => {
                    if stack.last().expect("validated coalesce operand").is_some() {
                        at = *target; continue;
                    }
                    let _ = stack.pop();
                }
                Instruction::Truth(value) => stack.push(value.map(i64::from)),
                Instruction::Compare(comparison) => {
                    let right = stack.pop().expect("validated comparison right operand");
                    let left = stack.last_mut().expect("validated comparison left operand");
                    *left = (*left).zip(right).map(|(a, b)| i64::from(match comparison {
                        IntegerComparison::Equal => a == b,
                        IntegerComparison::NotEqual => a != b,
                        IntegerComparison::Less => a < b,
                        IntegerComparison::LessOrEqual => a <= b,
                        IntegerComparison::Greater => a > b,
                        IntegerComparison::GreaterOrEqual => a >= b,
                    }));
                }
                Instruction::IsNull(is_null) => {
                    let value = stack.last_mut().expect("validated null operand");
                    *value = Some(i64::from(value.is_none() == *is_null));
                }
                Instruction::Not => {
                    let value = stack.last_mut().expect("validated Boolean operand");
                    *value = (*value).map(|value| 1 - value);
                }
                Instruction::And | Instruction::Or => {
                    let right = stack.pop().expect("validated Boolean right operand");
                    let left = stack.last_mut().expect("validated Boolean left operand");
                    *left = if matches!(op, Instruction::And) {
                        if *left == Some(0) || right == Some(0) { Some(0) }
                        else if left.is_none() || right.is_none() { None } else { Some(1) }
                    } else if *left == Some(1) || right == Some(1) { Some(1) }
                    else if left.is_none() || right.is_none() { None } else { Some(0) };
                }
                Instruction::JumpUnlessTrue(target) => {
                    if stack.pop().expect("validated CASE condition") != Some(1) {
                        at = *target; continue;
                    }
                }
                Instruction::JumpUnlessEqual(target) => {
                    let candidate = stack.pop().expect("validated WHEN operand");
                    let selector = *stack.last().expect("validated CASE selector");
                    if selector.is_none() || selector != candidate { at = *target; continue; }
                    let _ = stack.pop();
                }
                Instruction::Jump(target) => { at = *target; continue; }
                Instruction::Drop => { let _ = stack.pop().expect("validated unmatched selector"); }
            }
            debug_assert!(stack.len() <= self.stack_entries);
            at += 1;
        }
        debug_assert_eq!(stack.len(), 1);
        Ok(stack.pop().expect("one validated scalar result"))
    }

    /// Value-bearing application transcript. Existing instruction tags remain
    /// unchanged; added closed tags encode exact conditional jump targets and
    /// three-valued tests. This is not a durable scalar format or host numeric
    /// environment. No unexecuted literal or input is omitted from identity.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:checked-integer-expression:v1\0".to_vec();
        bytes.extend_from_slice(&(self.code.len() as u64).to_be_bytes());
        for op in &self.code {
            match op {
                Instruction::Column(column) => {
                    bytes.push(0); bytes.extend_from_slice(&(*column as u64).to_be_bytes());
                }
                Instruction::Literal(value) => {
                    bytes.extend_from_slice(&[1, u8::from(value.is_some())]);
                    if let Some(value) = value { bytes.extend_from_slice(&value.to_be_bytes()); }
                }
                Instruction::Unary(op) => bytes.extend_from_slice(&[2, match op {
                    GraphIntegerUnary::Plus => 0, GraphIntegerUnary::Negate => 1, GraphIntegerUnary::Abs => 2,
                }]),
                Instruction::Binary(op) => bytes.extend_from_slice(&[3, match op {
                    GraphIntegerBinary::Add => 0, GraphIntegerBinary::Subtract => 1,
                    GraphIntegerBinary::Multiply => 2, GraphIntegerBinary::Divide => 3,
                    GraphIntegerBinary::Remainder => 4, GraphIntegerBinary::NullIf => 5,
                }]),
                Instruction::JumpIfPresent(target) => {
                    bytes.push(4); bytes.extend_from_slice(&(*target as u64).to_be_bytes());
                }
                Instruction::Truth(value) => bytes.extend_from_slice(&[5, match value {
                    None => 0, Some(false) => 1, Some(true) => 2,
                }]),
                Instruction::Compare(comparison) => bytes.extend_from_slice(&[6, match comparison {
                    IntegerComparison::Equal => 0, IntegerComparison::NotEqual => 1,
                    IntegerComparison::Less => 2, IntegerComparison::LessOrEqual => 3,
                    IntegerComparison::Greater => 4, IntegerComparison::GreaterOrEqual => 5,
                }]),
                Instruction::IsNull(is_null) => bytes.extend_from_slice(&[7, u8::from(*is_null)]),
                Instruction::Not => bytes.push(8),
                Instruction::And => bytes.push(9),
                Instruction::Or => bytes.push(10),
                Instruction::JumpUnlessTrue(target) => {
                    bytes.push(11); bytes.extend_from_slice(&(*target as u64).to_be_bytes());
                }
                Instruction::JumpUnlessEqual(target) => {
                    bytes.push(12); bytes.extend_from_slice(&(*target as u64).to_be_bytes());
                }
                Instruction::Jump(target) => {
                    bytes.push(13); bytes.extend_from_slice(&(*target as u64).to_be_bytes());
                }
                Instruction::Drop => bytes.push(14),
            }
        }
        bytes
    }
}

fn apply_binary(op: GraphIntegerBinary, left: Option<i64>, right: Option<i64>)
    -> Result<Option<i64>, GraphIntegerErrorKind> {
    if op == GraphIntegerBinary::NullIf {
        return Ok(if left.is_some() && left == right { None } else { left });
    }
    let (Some(left), Some(right)) = (left, right) else { return Ok(None); };
    if right == 0 && matches!(op, GraphIntegerBinary::Divide | GraphIntegerBinary::Remainder) {
        return Err(GraphIntegerErrorKind::DivisionByZero);
    }
    let result = match op {
        GraphIntegerBinary::Add => left.checked_add(right),
        GraphIntegerBinary::Subtract => left.checked_sub(right),
        GraphIntegerBinary::Multiply => left.checked_mul(right),
        GraphIntegerBinary::Divide => left.checked_div(right),
        GraphIntegerBinary::Remainder if right == -1 => Some(0),
        GraphIntegerBinary::Remainder => left.checked_rem(right),
        GraphIntegerBinary::NullIf => unreachable!("NULLIF handled before null propagation"),
    };
    result.map(Some).ok_or(GraphIntegerErrorKind::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use GraphIntegerOp::{Binary, Coalesce, Column, Literal, Unary};

    fn evaluate(ops: &[GraphIntegerOp], values: &[GraphValue]) -> Result<Option<i64>, GraphIntegerError> {
        GraphIntegerExpression::prepare(ops).unwrap().evaluate_with_control(values, &mut |_| Ok::<_, ()>(()))
            .map_err(|error| match error { GraphIntegerEvaluationError::Value(error) => error,
                GraphIntegerEvaluationError::Control(()) => unreachable!("infallible test control") })
    }

    #[test]
    fn arithmetic_matches_wide_integer_oracle_without_host_overflow() {
        let values = [i64::MIN, i64::MIN + 1, -4_000_000_000, -7, -1, 0, 1, 7, 4_000_000_000, i64::MAX - 1, i64::MAX];
        for left in values { for right in values {
            for op in [GraphIntegerBinary::Add, GraphIntegerBinary::Subtract, GraphIntegerBinary::Multiply,
                GraphIntegerBinary::Divide, GraphIntegerBinary::Remainder, GraphIntegerBinary::NullIf] {
                let expected = if op == GraphIntegerBinary::NullIf {
                    Ok((left != right).then_some(left))
                } else if right == 0 && matches!(op, GraphIntegerBinary::Divide | GraphIntegerBinary::Remainder) {
                    Err(GraphIntegerErrorKind::DivisionByZero)
                } else {
                    let (a, b) = (i128::from(left), i128::from(right));
                    let result = match op {
                        GraphIntegerBinary::Add => a + b, GraphIntegerBinary::Subtract => a - b,
                        GraphIntegerBinary::Multiply => a * b, GraphIntegerBinary::Divide => a / b,
                        GraphIntegerBinary::Remainder => a % b, GraphIntegerBinary::NullIf => unreachable!(),
                    };
                    i64::try_from(result).map(Some).map_err(|_| GraphIntegerErrorKind::Overflow)
                };
                assert_eq!(apply_binary(op, Some(left), Some(right)), expected, "{op:?}, {left}, {right}");
                assert_eq!(evaluate(&[Literal(Some(left)), Literal(Some(right)), Binary(op)], &[]).map_err(|e| e.kind), expected);
            }
        }}
    }

    #[test]
    fn coalesce_skips_only_its_own_fallback_and_nested_jump_targets_are_exact() {
        let bad = [Literal(Some(1)), Literal(Some(0)), Binary(GraphIntegerBinary::Divide)];
        let mut ops = vec![Literal(Some(9))]; ops.extend(bad); ops.push(Coalesce);
        assert_eq!(evaluate(&ops, &[]), Ok(Some(9)));
        ops[0] = Literal(None);
        assert_eq!(evaluate(&ops, &[]).unwrap_err().kind, GraphIntegerErrorKind::DivisionByZero);
        let ops = [Literal(None), Literal(None), Literal(Some(8)), Coalesce, Coalesce,
            Literal(Some(3)), Binary(GraphIntegerBinary::Subtract)];
        assert_eq!(evaluate(&ops, &[]), Ok(Some(5)));
        let ops = [Literal(Some(8)), Column(usize::MAX), Coalesce,
            Literal(None), Literal(Some(2)), Coalesce, Binary(GraphIntegerBinary::Multiply)];
        assert_eq!(evaluate(&ops, &[]), Ok(Some(16)));
        assert_eq!(evaluate(&[Literal(None), Literal(Some(0)), Binary(GraphIntegerBinary::Divide)], &[]), Ok(None));
        assert_eq!(evaluate(&[Literal(None), Literal(Some(5)), Binary(GraphIntegerBinary::NullIf)], &[]), Ok(None));
        assert_eq!(evaluate(&[Literal(Some(5)), Literal(None), Binary(GraphIntegerBinary::NullIf)], &[]), Ok(Some(5)));
    }

    #[test]
    fn invalid_programs_and_noninteger_rows_are_typed_refusals() {
        assert_eq!(GraphIntegerExpression::prepare(&[]), Err(GraphIntegerBuildError::Empty));
        for ops in [vec![Unary(GraphIntegerUnary::Negate)], vec![Literal(None), Coalesce]] {
            assert!(matches!(GraphIntegerExpression::prepare(&ops), Err(GraphIntegerBuildError::MissingOperand { .. })));
        }
        assert!(matches!(GraphIntegerExpression::prepare(&[Literal(None), Literal(None)]),
            Err(GraphIntegerBuildError::ExtraOperands { remaining: 2 })));
        assert_eq!(evaluate(&[Column(0)], &[]).unwrap_err().kind, GraphIntegerErrorKind::MissingColumn);
        assert_eq!(evaluate(&[Column(0)], &[GraphValue::Scalar(CanonicalScalar::Bool(true))]).unwrap_err().kind,
            GraphIntegerErrorKind::NonInteger);
        for op in [GraphIntegerUnary::Negate, GraphIntegerUnary::Abs] {
            assert_eq!(evaluate(&[Literal(Some(i64::MIN)), Unary(op)], &[]).unwrap_err().kind, GraphIntegerErrorKind::Overflow);
        }
        let mut ops = vec![Literal(Some(3))];
        ops.extend(std::iter::repeat_n(Unary(GraphIntegerUnary::Plus), MAX_GRAPH_INTEGER_INSTRUCTIONS - 1));
        assert_eq!(evaluate(&ops, &[]), Ok(Some(3)));
        ops.push(Unary(GraphIntegerUnary::Plus));
        assert!(matches!(GraphIntegerExpression::prepare(&ops), Err(GraphIntegerBuildError::TooManyInstructions { .. })));
    }

    #[test]
    fn every_frame_and_instruction_checkpoint_refuses_without_reusing_state() {
        let ops = [Column(0), Literal(Some(2)), Coalesce, Literal(Some(3)), Binary(GraphIntegerBinary::Multiply),
            Literal(Some(-4)), Unary(GraphIntegerUnary::Abs), Binary(GraphIntegerBinary::Add)];
        let expression = GraphIntegerExpression::prepare(&ops).unwrap();
        let values = [GraphValue::Scalar(CanonicalScalar::Null)];
        let mut count = 0;
        assert_eq!(expression.evaluate_with_control(&values, &mut |_| { count += 1; Ok::<_, usize>(()) }).unwrap(), Some(10));
        for stop in 1..=count {
            let mut seen = 0;
            assert!(matches!(expression.evaluate_with_control(&values, &mut |_| {
                seen += 1; if seen == stop { Err(stop) } else { Ok(()) }
            }), Err(GraphIntegerEvaluationError::Control(at)) if at == stop));
            assert_eq!(seen, stop);
        }
        assert_eq!(evaluate(&ops, &values), Ok(Some(10)));
        let frozen = expression.canonical_bytes();
        assert_eq!(GraphIntegerExpression::prepare(&ops).unwrap().canonical_bytes(), frozen);
        assert!(!format!("{:?}", GraphIntegerExpression::prepare(&[Literal(Some(123456789))]).unwrap()).contains("123456789"));
    }
}
