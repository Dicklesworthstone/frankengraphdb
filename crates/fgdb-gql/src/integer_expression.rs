//! Checked nullable scalar expressions over a frozen, typed value row.
//!
//! Bounded typed postfix IR compiles to private linear bytecode. CASE and
//! COALESCE are lazy. Integer arithmetic remains checked, with no coercions.
//! Text operations use Unicode scalar positions and UCS_BASIC result collation.

mod compile;

use crate::GlaExecutionEvent;
use crate::algebra::{GraphValue, IntegerComparison, ScalarPredicate, GRAPH_VALUE_PAYLOAD_UNIT_BYTES};
use fgdb_types::CanonicalScalar;
use std::borrow::Cow;

pub const MAX_GRAPH_INTEGER_INSTRUCTIONS: usize = 1_024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphIntegerUnary { Plus, Negate, Abs }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphIntegerBinary { Add, Subtract, Multiply, Divide, Remainder, NullIf }

/// Postfix construction IR. Arithmetic operands/results are nullable integers.
/// Scalar conditions and results never implicitly coerce between domains.
/// Case consumes (condition, then_value, else_value); only the selected
/// result executes. Nest Case in the else operand for ordered WHEN clauses.
/// SimpleCase consumes (selector, when, then, ..., default), evaluates its
/// selector once, and selects the first nonnull equality. Conditions use eager
/// three-valued Boolean evaluation; unselected CASE arms are not executed.
#[derive(Clone, PartialEq, Eq)]
pub enum GraphIntegerOp {
    Column(usize),
    Literal(Option<i64>),
    Scalar(ScalarPredicate),
    ScalarColumn(usize),
    Upper,
    Lower,
    Trim,
    CharLength,
    Substring,
    Concat,
    StartsWith,
    EndsWith,
    Contains,
    InList { members: usize },
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
            Self::Scalar(_) => f.write_str("Scalar([REDACTED])"),
            Self::ScalarColumn(_) => f.write_str("ScalarColumn([REDACTED])"),
            Self::Upper => f.write_str("Upper"),
            Self::Lower => f.write_str("Lower"),
            Self::Trim => f.write_str("Trim"),
            Self::CharLength => f.write_str("CharLength"),
            Self::Substring => f.write_str("Substring"),
            Self::Concat => f.write_str("Concat"),
            Self::StartsWith => f.write_str("StartsWith"),
            Self::EndsWith => f.write_str("EndsWith"),
            Self::Contains => f.write_str("Contains"),
            Self::InList { members } => f.debug_struct("InList").field("members", members).finish(),
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
pub enum GraphIntegerErrorKind {
    MissingColumn, NonInteger, Overflow, DivisionByZero, NonText, NonBoolean,
    NonScalar, IncompatibleOperands, InvalidSubstring, TextConstruction,
}
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
    Scalar(ScalarPredicate), ScalarColumn(usize),
    Upper, Lower, Trim, CharLength, Substring, Concat, StartsWith, EndsWith, Contains,
    InList { members: usize },
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

    /// Admit an integer, Boolean, text or null root, including dynamic columns.
    pub fn prepare_scalar(ops: &[GraphIntegerOp]) -> Result<Self, GraphIntegerBuildError> {
        compile::prepare_scalar(ops)
    }

    pub fn referenced_columns(&self) -> impl Iterator<Item = usize> + '_ {
        self.code.iter().filter_map(|op| match op {
            Instruction::Column(column) | Instruction::ScalarColumn(column) => Some(*column), _ => None,
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
        scalar_integer(&self.evaluate_scalar_with_control(values, control)?).map_err(|kind|
            GraphIntegerEvaluationError::Value(GraphIntegerError { instruction: self.code.len(), kind }))
    }

    /// Evaluate borrowed inputs without copying their payloads. Each payload
    /// scan is charged in work units; new payload storage is reserved through
    /// ScratchEntry checkpoints before allocation, including the owned return.
    pub fn evaluate_scalar_with_control<E>(
        &self, values: &[GraphValue],
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<CanonicalScalar, GraphIntegerEvaluationError<E>> {
        for _ in 0..self.stack_entries {
            control(GlaExecutionEvent::ScratchEntry).map_err(GraphIntegerEvaluationError::Control)?;
        }
        let mut stack: Vec<Cow<'_, CanonicalScalar>> = Vec::with_capacity(self.stack_entries);
        let mut at = 0;
        while let Some(op) = self.code.get(at) {
            control(GlaExecutionEvent::Work).map_err(GraphIntegerEvaluationError::Control)?;
            let failure = |kind| GraphIntegerEvaluationError::Value(GraphIntegerError { instruction: at, kind });
            match op {
                Instruction::Column(column) | Instruction::ScalarColumn(column) => {
                    let value = match values.get(*column) {
                        Some(GraphValue::Scalar(value)) => value,
                        Some(_) => return Err(failure(if matches!(op, Instruction::Column(_)) {
                            GraphIntegerErrorKind::NonInteger
                        } else { GraphIntegerErrorKind::NonScalar })),
                        None => return Err(failure(GraphIntegerErrorKind::MissingColumn)),
                    };
                    if matches!(op, Instruction::Column(_)) { scalar_integer(value).map_err(failure)?; }
                    stack.push(Cow::Borrowed(value));
                }
                Instruction::Scalar(value) => stack.push(Cow::Borrowed(value.value())),
                Instruction::Literal(value) => stack.push(Cow::Owned(integer_scalar(*value))),
                Instruction::Truth(value) => stack.push(Cow::Owned(boolean_scalar(*value))),
                Instruction::Unary(op) => {
                    let value = stack.last_mut().expect("validated unary stack");
                    let number = scalar_integer(value).map_err(failure)?;
                    let result = match number {
                        None => None,
                        Some(number) => Some(match op {
                            GraphIntegerUnary::Plus => Some(number),
                            GraphIntegerUnary::Negate => number.checked_neg(),
                            GraphIntegerUnary::Abs => number.checked_abs(),
                        }.ok_or_else(|| failure(GraphIntegerErrorKind::Overflow))?),
                    };
                    *value = Cow::Owned(integer_scalar(result));
                }
                Instruction::Binary(op) => {
                    let right = stack.pop().expect("validated right operand");
                    let left = stack.last_mut().expect("validated left operand");
                    let a = scalar_integer(left).map_err(failure)?;
                    let b = scalar_integer(&right).map_err(failure)?;
                    *left = Cow::Owned(integer_scalar(apply_binary(*op, a, b).map_err(failure)?));
                }
                Instruction::JumpIfPresent(target) => {
                    if !matches!(stack.last().expect("validated coalesce operand").as_ref(), CanonicalScalar::Null) {
                        at = *target; continue;
                    }
                    let _ = stack.pop();
                }
                Instruction::Compare(comparison) => {
                    let right = stack.pop().expect("validated comparison right operand");
                    let left = stack.last_mut().expect("validated comparison left operand");
                    let result = compare_scalars(*comparison, left, &right, at, control)?;
                    *left = Cow::Owned(boolean_scalar(result));
                }
                Instruction::IsNull(is_null) => {
                    let value = stack.last_mut().expect("validated null operand");
                    *value = Cow::Owned(CanonicalScalar::Bool(matches!(value.as_ref(), CanonicalScalar::Null) == *is_null));
                }
                Instruction::Not => {
                    let value = stack.last_mut().expect("validated Boolean operand");
                    *value = Cow::Owned(boolean_scalar(scalar_boolean(value).map_err(failure)?.map(|value| !value)));
                }
                Instruction::And | Instruction::Or => {
                    let right = stack.pop().expect("validated Boolean right operand");
                    let left = stack.last_mut().expect("validated Boolean left operand");
                    let a = scalar_boolean(left).map_err(failure)?;
                    let b = scalar_boolean(&right).map_err(failure)?;
                    let result = if matches!(op, Instruction::And) {
                        if a == Some(false) || b == Some(false) { Some(false) }
                        else if a.is_none() || b.is_none() { None } else { Some(true) }
                    } else if a == Some(true) || b == Some(true) { Some(true) }
                    else if a.is_none() || b.is_none() { None } else { Some(false) };
                    *left = Cow::Owned(boolean_scalar(result));
                }
                Instruction::Upper | Instruction::Lower | Instruction::Trim | Instruction::CharLength => {
                    let value = stack.last_mut().expect("validated text operand");
                    let result = if let Some(text) = scalar_text(value).map_err(failure)? {
                        charge_payload(text.len(), false, control)?;
                        match op {
                            Instruction::CharLength => CanonicalScalar::Int(i64::try_from(text.chars().count())
                                .map_err(|_| failure(GraphIntegerErrorKind::Overflow))?),
                            Instruction::Trim => make_text(text.trim(), at, control)?,
                            _ => {
                                // Unicode case mapping expands a scalar by at most three
                                // times its UTF-8 bytes. Reserve before str allocates.
                                let bound = text.len().checked_mul(3).ok_or_else(|| failure(GraphIntegerErrorKind::Overflow))?;
                                charge_payload(bound, true, control)?;
                                let mapped = if matches!(op, Instruction::Upper) { text.to_uppercase() }
                                    else { text.to_lowercase() };
                                make_text(&mapped, at, control)?
                            }
                        }
                    } else { CanonicalScalar::Null };
                    *value = Cow::Owned(result);
                }
                Instruction::Concat | Instruction::StartsWith | Instruction::EndsWith | Instruction::Contains => {
                    let right = stack.pop().expect("validated text right operand");
                    let left = stack.last_mut().expect("validated text left operand");
                    let a = scalar_text(left).map_err(failure)?;
                    let b = scalar_text(&right).map_err(failure)?;
                    let result = if let (Some(a), Some(b)) = (a, b) {
                        charge_payload(a.len(), false, control)?;
                        charge_payload(b.len(), false, control)?;
                        match op {
                            Instruction::Concat => {
                                let size = a.len().checked_add(b.len()).ok_or_else(|| failure(GraphIntegerErrorKind::Overflow))?;
                                charge_payload(size, true, control)?;
                                let mut joined = String::with_capacity(size);
                                joined.push_str(a); joined.push_str(b);
                                make_text(&joined, at, control)?
                            }
                            Instruction::StartsWith => CanonicalScalar::Bool(a.starts_with(b)),
                            Instruction::EndsWith => CanonicalScalar::Bool(a.ends_with(b)),
                            _ => CanonicalScalar::Bool(a.contains(b)),
                        }
                    } else { CanonicalScalar::Null };
                    *left = Cow::Owned(result);
                }
                Instruction::Substring => {
                    let length = stack.pop().expect("validated substring length");
                    let start = stack.pop().expect("validated substring start");
                    let value = stack.last_mut().expect("validated substring text");
                    let text = scalar_text(value).map_err(failure)?;
                    let start = scalar_integer(&start).map_err(failure)?;
                    let length = scalar_integer(&length).map_err(failure)?;
                    if length.is_some_and(|length| length < 0) {
                        return Err(failure(GraphIntegerErrorKind::InvalidSubstring));
                    }
                    let result = if let (Some(text), Some(start), Some(length)) = (text, start, length) {
                        charge_payload(text.len(), false, control)?;
                        // Intersect the requested [start, start + length) with
                        // one-based Unicode positions. i128 avoids i64 overflow.
                        let begin = (i128::from(start) - 1).max(0);
                        let end = (i128::from(start) - 1 + i128::from(length)).max(0);
                        let mut first = text.len();
                        let mut last = text.len();
                        for (position, (byte, _)) in text.char_indices().enumerate() {
                            let position = position as i128;
                            if position == begin { first = byte; }
                            if position == end { last = byte; break; }
                        }
                        make_text(&text[first..last], at, control)?
                    } else { CanonicalScalar::Null };
                    *value = Cow::Owned(result);
                }
                Instruction::InList { members } => {
                    let base = stack.len() - *members - 1;
                    let mut result = Some(false);
                    for candidate in &stack[base + 1..] {
                        let equal = compare_scalars(IntegerComparison::Equal, &stack[base], candidate, at, control)?;
                        if equal == Some(true) { result = Some(true); }
                        else if equal.is_none() && result != Some(true) { result = None; }
                    }
                    stack.truncate(base);
                    stack.push(Cow::Owned(boolean_scalar(result)));
                }
                Instruction::JumpUnlessTrue(target) => {
                    let condition = stack.pop().expect("validated CASE condition");
                    if scalar_boolean(&condition).map_err(failure)? != Some(true) {
                        at = *target; continue;
                    }
                }
                Instruction::JumpUnlessEqual(target) => {
                    let candidate = stack.pop().expect("validated WHEN operand");
                    let selector = stack.last().expect("validated CASE selector");
                    if compare_scalars(IntegerComparison::Equal, selector, &candidate, at, control)? != Some(true) {
                        at = *target; continue;
                    }
                    let _ = stack.pop();
                }
                Instruction::Jump(target) => { at = *target; continue; }
                Instruction::Drop => { let _ = stack.pop().expect("validated unmatched selector"); }
            }
            debug_assert!(stack.len() <= self.stack_entries);
            at += 1;
        }
        debug_assert_eq!(stack.len(), 1);
        let value = stack.pop().expect("one validated scalar result");
        if !matches!(value.as_ref(), CanonicalScalar::Null | CanonicalScalar::Int(_)
            | CanonicalScalar::Bool(_) | CanonicalScalar::Text(_)) {
            return Err(GraphIntegerEvaluationError::Value(GraphIntegerError {
                instruction: self.code.len(), kind: GraphIntegerErrorKind::NonScalar,
            }));
        }
        if let Cow::Borrowed(value) = &value { charge_payload(scalar_payload_bytes(value), true, control)?; }
        Ok(value.into_owned())
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
                Instruction::Scalar(value) => {
                    bytes.push(15);
                    let value = value.canonical_value_bytes();
                    bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
                    bytes.extend_from_slice(value);
                }
                Instruction::ScalarColumn(column) => {
                    bytes.push(16); bytes.extend_from_slice(&(*column as u64).to_be_bytes());
                }
                Instruction::Upper => bytes.push(17),
                Instruction::Lower => bytes.push(18),
                Instruction::Trim => bytes.push(19),
                Instruction::CharLength => bytes.push(20),
                Instruction::Substring => bytes.push(21),
                Instruction::Concat => bytes.push(22),
                Instruction::StartsWith => bytes.push(23),
                Instruction::EndsWith => bytes.push(24),
                Instruction::Contains => bytes.push(25),
                Instruction::InList { members } => {
                    bytes.push(26); bytes.extend_from_slice(&(*members as u64).to_be_bytes());
                }
            }
        }
        bytes
    }
}

fn integer_scalar(value: Option<i64>) -> CanonicalScalar {
    value.map_or(CanonicalScalar::Null, CanonicalScalar::Int)
}

fn boolean_scalar(value: Option<bool>) -> CanonicalScalar {
    value.map_or(CanonicalScalar::Null, CanonicalScalar::Bool)
}

fn scalar_integer(value: &CanonicalScalar) -> Result<Option<i64>, GraphIntegerErrorKind> {
    match value {
        CanonicalScalar::Null => Ok(None),
        CanonicalScalar::Int(value) => Ok(Some(*value)),
        _ => Err(GraphIntegerErrorKind::NonInteger),
    }
}

fn scalar_boolean(value: &CanonicalScalar) -> Result<Option<bool>, GraphIntegerErrorKind> {
    match value {
        CanonicalScalar::Null => Ok(None),
        CanonicalScalar::Bool(value) => Ok(Some(*value)),
        _ => Err(GraphIntegerErrorKind::NonBoolean),
    }
}

fn scalar_text(value: &CanonicalScalar) -> Result<Option<&str>, GraphIntegerErrorKind> {
    match value {
        CanonicalScalar::Null => Ok(None),
        CanonicalScalar::Text(value) => Ok(Some(value.as_str())),
        _ => Err(GraphIntegerErrorKind::NonText),
    }
}

fn scalar_payload_bytes(value: &CanonicalScalar) -> usize {
    match value {
        CanonicalScalar::Text(value) => value.len().saturating_add(value.canonical_sort_key().map_or(0, <[u8]>::len)),
        _ => 0,
    }
}

fn charge_payload<E>(
    bytes: usize, allocate: bool,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), GraphIntegerEvaluationError<E>> {
    for _ in 0..bytes.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
        control(GlaExecutionEvent::Work).map_err(GraphIntegerEvaluationError::Control)?;
        if allocate {
            control(GlaExecutionEvent::ScratchEntry).map_err(GraphIntegerEvaluationError::Control)?;
        }
    }
    Ok(())
}

fn make_text<E>(
    text: &str, instruction: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<CanonicalScalar, GraphIntegerEvaluationError<E>> {
    charge_payload(text.len(), true, control)?;
    CanonicalScalar::ucs_basic_text(text).map_err(|_| GraphIntegerEvaluationError::Value(
        GraphIntegerError { instruction, kind: GraphIntegerErrorKind::TextConstruction }))
}

fn compare_scalars<E>(
    comparison: IntegerComparison, left: &CanonicalScalar, right: &CanonicalScalar,
    instruction: usize, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<bool>, GraphIntegerEvaluationError<E>> {
    if matches!(left, CanonicalScalar::Null) || matches!(right, CanonicalScalar::Null) {
        return Ok(None);
    }
    if core::mem::discriminant(left) != core::mem::discriminant(right) {
        return Err(GraphIntegerEvaluationError::Value(GraphIntegerError {
            instruction, kind: GraphIntegerErrorKind::IncompatibleOperands,
        }));
    }
    charge_payload(scalar_payload_bytes(left), false, control)?;
    charge_payload(scalar_payload_bytes(right), false, control)?;
    Ok(Some(comparison.accepts_scalar_pair(Some(left), Some(right))))
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
