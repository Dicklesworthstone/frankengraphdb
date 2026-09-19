//! Bounded three-valued predicates over a complete positive-pattern binding.
//!
//! The program is checked postfix, not query text or an executable callback.
//! Operands use the same immutable property source and canonical comparison as
//! ordinary selections. Evaluation is eager, left-to-right: Boolean results do
//! not suppress fallible reads. Only a final TRUE survives WHERE. No program,
//! literal value or variable name is exposed by Debug.

use super::GraphValue;
use super::{
    BindingSlot, IntegerComparison, MAX_PATTERN_NAME_BYTES, MAX_PATTERN_PREDICATES,
    PatternBuildError, ScalarPredicate, ScalarPredicateError,
};
use crate::{GlaExecutionEvent, GraphIntegerEvaluationError};
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::sync::Arc;

pub const MAX_BOOLEAN_INSTRUCTIONS: usize = 1024;

#[derive(Clone, Copy)]
pub enum GraphBooleanOperand<'a> {
    Vertex(&'a str),
    Property {
        variable: &'a str,
        key: PropertyKeyId,
    },
    EdgeProperty {
        variable: &'a str,
        key: PropertyKeyId,
    },
    Literal(&'a CanonicalScalar),
    /// Reuse an already admitted operand and its canonical encoding.
    CheckedLiteral(&'a ScalarPredicate),
}

#[derive(Clone, Copy)]
pub enum GraphBooleanOp<'a> {
    Compare {
        left: GraphBooleanOperand<'a>,
        comparison: IntegerComparison,
        right: GraphBooleanOperand<'a>,
    },
    IsNull {
        operand: GraphBooleanOperand<'a>,
        is_null: bool,
    },
    Truth(Option<bool>),
    /// A whole typed scalar program evaluated by the shared expression
    /// engine. Ordered inputs feed its value columns; the result must be a
    /// canonical Boolean or null to select rows.
    Expression {
        expression: &'a crate::GraphIntegerExpression,
        columns: &'a [GraphBooleanOperand<'a>],
    },
    And,
    Or,
    Not,
}

impl core::fmt::Debug for GraphBooleanOperand<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphBooleanOperand([REDACTED])")
    }
}
impl core::fmt::Debug for GraphBooleanOp<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphBooleanOp([REDACTED])")
    }
}

#[derive(Debug)]
pub enum GraphBooleanError {
    Empty,
    TooManyInstructions { limit: usize, observed: usize },
    TooManyPredicates { limit: usize, observed: usize },
    InvalidStack { instruction: usize },
    InvalidVariableName,
    InvalidVertexComparison,
    InvalidExpressionColumn,
    Scalar(ScalarPredicateError),
}
impl core::fmt::Display for GraphBooleanError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Empty => f.write_str("Boolean expression requires an operand"),
            Self::TooManyInstructions { limit, observed } => write!(
                f,
                "Boolean expression has {observed} instructions, limit {limit}"
            ),
            Self::TooManyPredicates { limit, observed } => write!(
                f,
                "Boolean expression has {observed} predicates, limit {limit}"
            ),
            Self::InvalidStack { instruction } => {
                write!(f, "invalid Boolean stack at instruction {instruction}")
            }
            Self::InvalidVariableName => f.write_str("invalid Boolean variable name"),
            Self::InvalidVertexComparison => {
                f.write_str("vertex operands require vertex equality or inequality")
            }
            Self::InvalidExpressionColumn => {
                f.write_str("Boolean expression references an unknown value column")
            }
            Self::Scalar(error) => core::fmt::Display::fmt(error, f),
        }
    }
}
impl core::error::Error for GraphBooleanError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Scalar(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum Operand<S> {
    Vertex(S),
    Property { variable: S, key: PropertyKeyId },
    EdgeProperty { variable: S, key: PropertyKeyId },
    Literal(ScalarPredicate),
}
#[derive(Clone, PartialEq, Eq)]
enum Instruction<S> {
    Compare {
        left: Operand<S>,
        comparison: IntegerComparison,
        right: Operand<S>,
    },
    IsNull {
        operand: Operand<S>,
        is_null: bool,
    },
    Truth(Option<bool>),
    Expression {
        expression: crate::GraphIntegerExpression,
        columns: Vec<Operand<S>>,
    },
    And,
    Or,
    Not,
}

impl<S> Operand<S> {
    fn try_map<T, E>(
        &self,
        map: &mut impl FnMut(&S, bool) -> Result<T, E>,
    ) -> Result<Operand<T>, E> {
        Ok(match self {
            Self::Vertex(slot) => Operand::Vertex(map(slot, false)?),
            Self::Property { variable, key } => Operand::Property {
                variable: map(variable, false)?,
                key: *key,
            },
            Self::EdgeProperty { variable, key } => Operand::EdgeProperty {
                variable: map(variable, true)?,
                key: *key,
            },
            Self::Literal(value) => Operand::Literal(value.clone()),
        })
    }
}
impl<S> Instruction<S> {
    fn try_map<T, E>(
        &self,
        map: &mut impl FnMut(&S, bool) -> Result<T, E>,
    ) -> Result<Instruction<T>, E> {
        Ok(match self {
            Self::Compare {
                left,
                comparison,
                right,
            } => Instruction::Compare {
                left: left.try_map(map)?,
                comparison: *comparison,
                right: right.try_map(map)?,
            },
            Self::IsNull { operand, is_null } => Instruction::IsNull {
                operand: operand.try_map(map)?,
                is_null: *is_null,
            },
            Self::Expression {
                expression,
                columns,
            } => {
                let mut mapped = Vec::with_capacity(columns.len());
                for column in columns {
                    mapped.push(column.try_map(map)?);
                }
                Instruction::Expression {
                    expression: expression.clone(),
                    columns: mapped,
                }
            }
            Self::Truth(value) => Instruction::Truth(*value),
            Self::And => Instruction::And,
            Self::Or => Instruction::Or,
            Self::Not => Instruction::Not,
        })
    }
}

/// An immutable bounded postfix Boolean expression. Construct it once, then
/// attach it to a builder with filter_boolean. Literal encodings are checked
/// once and shared; binding names are replaced by slots during compilation.
#[derive(Clone, PartialEq, Eq)]
pub struct GraphBooleanExpression {
    program: Arc<[Instruction<String>]>,
    predicates: usize,
}
impl core::fmt::Debug for GraphBooleanExpression {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphBooleanExpression")
            .field("instructions", &self.program.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

fn name(value: &str) -> Result<String, GraphBooleanError> {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > MAX_PATTERN_NAME_BYTES
        || !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        || !bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
    {
        return Err(GraphBooleanError::InvalidVariableName);
    }
    Ok(value.to_owned())
}
fn own(operand: GraphBooleanOperand<'_>) -> Result<Operand<String>, GraphBooleanError> {
    Ok(match operand {
        GraphBooleanOperand::Vertex(variable) => Operand::Vertex(name(variable)?),
        GraphBooleanOperand::Property { variable, key } => Operand::Property {
            variable: name(variable)?,
            key,
        },
        GraphBooleanOperand::EdgeProperty { variable, key } => Operand::EdgeProperty {
            variable: name(variable)?,
            key,
        },
        GraphBooleanOperand::CheckedLiteral(value) => {
            Operand::Literal(value.with_comparison(IntegerComparison::Equal))
        }
        GraphBooleanOperand::Literal(value) => {
            // Check variable payload BEFORE cloning it. ScalarPredicate then
            // checks the complete canonical encoding, including fixed headers.
            let bytes = match value {
                CanonicalScalar::Bytes(value) => value.as_slice().len(),
                CanonicalScalar::Text(value) => value
                    .len()
                    .saturating_add(value.canonical_sort_key().map_or(0, <[u8]>::len)),
                CanonicalScalar::Timestamp(value) => {
                    value.zone().map_or(0, |zone| zone.identifier().len())
                }
                _ => 0,
            };
            if bytes > super::MAX_SCALAR_PREDICATE_BYTES {
                return Err(GraphBooleanError::Scalar(
                    ScalarPredicateError::LiteralTooLarge {
                        limit: super::MAX_SCALAR_PREDICATE_BYTES,
                        observed: bytes,
                    },
                ));
            }
            Operand::Literal(
                ScalarPredicate::new(value.clone(), IntegerComparison::Equal)
                    .map_err(GraphBooleanError::Scalar)?,
            )
        }
    })
}

impl GraphBooleanExpression {
    pub fn prepare(instructions: &[GraphBooleanOp<'_>]) -> Result<Self, GraphBooleanError> {
        if instructions.is_empty() {
            return Err(GraphBooleanError::Empty);
        }
        if instructions.len() > MAX_BOOLEAN_INSTRUCTIONS {
            return Err(GraphBooleanError::TooManyInstructions {
                limit: MAX_BOOLEAN_INSTRUCTIONS,
                observed: instructions.len(),
            });
        }
        let mut depth = 0_usize;
        let mut predicates = 0_usize;
        // Validate the complete stack and limits before owning any operand.
        for (at, instruction) in instructions.iter().enumerate() {
            match instruction {
                GraphBooleanOp::And | GraphBooleanOp::Or if depth >= 2 => {
                    depth -= 1;
                }
                GraphBooleanOp::Not if depth >= 1 => {}
                GraphBooleanOp::Compare { .. }
                | GraphBooleanOp::IsNull { .. }
                | GraphBooleanOp::Truth(_)
                | GraphBooleanOp::Expression { .. } => {
                    depth += 1;
                    predicates += 1;
                    if predicates > MAX_PATTERN_PREDICATES {
                        return Err(GraphBooleanError::TooManyPredicates {
                            limit: MAX_PATTERN_PREDICATES,
                            observed: predicates,
                        });
                    }
                }
                _ => return Err(GraphBooleanError::InvalidStack { instruction: at }),
            }
        }
        if depth != 1 {
            return Err(GraphBooleanError::InvalidStack {
                instruction: instructions.len(),
            });
        }
        let mut program = Vec::new();
        for instruction in instructions {
            program.push(match *instruction {
                GraphBooleanOp::Compare {
                    left,
                    comparison,
                    right,
                } => {
                    let left_vertex = matches!(left, GraphBooleanOperand::Vertex(_));
                    let right_vertex = matches!(right, GraphBooleanOperand::Vertex(_));
                    if (left_vertex || right_vertex)
                        && !(left_vertex
                            && right_vertex
                            && matches!(
                                comparison,
                                IntegerComparison::Equal | IntegerComparison::NotEqual
                            ))
                    {
                        return Err(GraphBooleanError::InvalidVertexComparison);
                    }
                    Instruction::Compare {
                        left: own(left)?,
                        comparison,
                        right: own(right)?,
                    }
                }
                GraphBooleanOp::IsNull { operand, is_null } => Instruction::IsNull {
                    operand: own(operand)?,
                    is_null,
                },
                GraphBooleanOp::Expression {
                    expression,
                    columns,
                } => {
                    if expression
                        .referenced_columns()
                        .any(|column| column >= columns.len())
                    {
                        return Err(GraphBooleanError::InvalidExpressionColumn);
                    }
                    let mut mapped = Vec::with_capacity(columns.len());
                    for column in columns {
                        mapped.push(own(*column)?);
                    }
                    Instruction::Expression {
                        expression: expression.clone(),
                        columns: mapped,
                    }
                }
                GraphBooleanOp::Truth(value) => Instruction::Truth(value),
                GraphBooleanOp::And => Instruction::And,
                GraphBooleanOp::Or => Instruction::Or,
                GraphBooleanOp::Not => Instruction::Not,
            });
        }
        Ok(Self {
            program: program.into(),
            predicates,
        })
    }
    #[must_use]
    pub fn predicate_count(&self) -> usize {
        self.predicates
    }

    pub(crate) fn bind(
        &self,
        mut binding: impl FnMut(&str) -> Result<usize, PatternBuildError>,
        mut capture: impl FnMut(&str) -> Result<usize, PatternBuildError>,
    ) -> Result<BoundBooleanExpression, PatternBuildError> {
        let program = self
            .program
            .iter()
            .map(|instruction| {
                instruction.try_map(&mut |name: &String, edge| {
                    if edge { capture(name) } else { binding(name) }
                        .map(|slot| BindingSlot(slot as u32))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(BoundBooleanExpression {
            program: program.into(),
        })
    }

    /// Whether any operand reads a captured relationship property. Such a
    /// program needs the element-property executor, never a vertex-only one.
    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn contains_edge_property(&self) -> bool {
        fn operand<S>(operand: &Operand<S>) -> bool {
            matches!(operand, Operand::EdgeProperty { .. })
        }
        self.program.iter().any(|instruction| match instruction {
            Instruction::Compare { left, right, .. } => operand(left) || operand(right),
            Instruction::IsNull {
                operand: checked, ..
            } => operand(checked),
            Instruction::Expression { columns, .. } => columns.iter().any(operand),
            Instruction::Truth(_) | Instruction::And | Instruction::Or | Instruction::Not => false,
        })
    }
}

/// Compiler-owned slot program. There is no public unchecked constructor.
#[derive(Clone, PartialEq, Eq)]
pub struct BoundBooleanExpression {
    program: Arc<[Instruction<BindingSlot>]>,
}
impl core::fmt::Debug for BoundBooleanExpression {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BoundBooleanExpression")
            .field("instructions", &self.program.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

impl BoundBooleanExpression {
    /// A direct STARTS WITH conjunct over one vertex slot. The original
    /// expression must still be evaluated on the resolved visible rows.
    /// OR, NOT, nested scalar programs and captured-edge operands are not
    /// eligible. This exposes semantic facts, never unchecked bytecode.
    #[must_use]
    pub fn starts_with_conjunct(&self) -> Option<(PropertyKeyId, &str)> {
        let local = |operand: &Operand<BindingSlot>| match operand {
            Operand::Vertex(variable) | Operand::Property { variable, .. } => {
                variable.ordinal() == 0
            }
            Operand::Literal(_) => true,
            Operand::EdgeProperty { .. } => false,
        };
        let mut selected = None;
        for instruction in self.program.iter() {
            match instruction {
                Instruction::Expression {
                    expression,
                    columns,
                } => {
                    if !columns.iter().all(&local) {
                        return None;
                    }
                    let (column, prefix) = expression.starts_with_literal()?;
                    let Operand::Property { variable, key } = columns.get(column)? else {
                        return None;
                    };
                    if variable.ordinal() != 0 {
                        return None;
                    }
                    if let Some((chosen, _)) = selected {
                        if chosen != *key {
                            return None;
                        }
                    } else {
                        selected = Some((*key, prefix));
                    }
                }
                Instruction::Compare { left, right, .. } if local(left) && local(right) => {}
                Instruction::IsNull { operand, .. } if local(operand) => {}
                Instruction::Truth(_) | Instruction::And => {}
                _ => return None,
            }
        }
        selected
    }
    /// Whether any already-bound operand reads a captured relationship property.
    #[must_use]
    pub(crate) fn contains_edge_property(&self) -> bool {
        fn operand<S>(operand: &Operand<S>) -> bool {
            matches!(operand, Operand::EdgeProperty { .. })
        }
        self.program.iter().any(|instruction| match instruction {
            Instruction::Compare { left, right, .. } => operand(left) || operand(right),
            Instruction::IsNull {
                operand: checked, ..
            } => operand(checked),
            Instruction::Expression { columns, .. } => columns.iter().any(operand),
            Instruction::Truth(_) | Instruction::And | Instruction::Or | Instruction::Not => false,
        })
    }
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Truth {
    False,
    Unknown,
    True,
}
impl Truth {
    fn from(value: Option<bool>) -> Self {
        match value {
            Some(false) => Self::False,
            None => Self::Unknown,
            Some(true) => Self::True,
        }
    }
    fn not(self) -> Self {
        match self {
            Self::False => Self::True,
            Self::True => Self::False,
            Self::Unknown => Self::Unknown,
        }
    }
    fn and(self, other: Self) -> Self {
        if self == Self::False || other == Self::False {
            Self::False
        } else if self == Self::Unknown || other == Self::Unknown {
            Self::Unknown
        } else {
            Self::True
        }
    }
    fn or(self, other: Self) -> Self {
        if self == Self::True || other == Self::True {
            Self::True
        } else if self == Self::Unknown || other == Self::Unknown {
            Self::Unknown
        } else {
            Self::False
        }
    }
}

enum Value<'a> {
    Scalar(Option<&'a CanonicalScalar>),
    Vertex(Option<VId>),
}
impl Value<'_> {
    fn is_null(&self) -> bool {
        match self {
            Self::Vertex(value) => value.is_none(),
            Self::Scalar(value) => value.is_none_or(|v| matches!(v, CanonicalScalar::Null)),
        }
    }
}
fn resolve<'source: 'borrow, 'borrow, E>(
    operand: &'borrow Operand<BindingSlot>,
    bindings: &[Option<VId>],
    paths: &[Option<super::GraphPath>],
    property: &mut impl FnMut(VId, PropertyKeyId) -> Result<Option<&'source CanonicalScalar>, E>,
    edge_property: &mut impl FnMut(EId, PropertyKeyId) -> Result<Option<&'source CanonicalScalar>, E>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Value<'borrow>, E> {
    control(GlaExecutionEvent::Work)?;
    Ok(match operand {
        Operand::Vertex(slot) => {
            Value::Vertex(bindings.get(slot.ordinal() as usize).copied().flatten())
        }
        Operand::Property { variable, key } => Value::Scalar(
            match bindings.get(variable.ordinal() as usize).copied().flatten() {
                Some(vertex) => property(vertex, *key)?,
                None => None,
            },
        ),
        Operand::EdgeProperty { variable, key } => Value::Scalar(
            match paths
                .get(variable.ordinal() as usize)
                .and_then(Option::as_ref)
            {
                Some(path) => {
                    let [(edge, _)] = path.steps() else {
                        unreachable!("edge property captures contain exactly one relationship")
                    };
                    edge_property(*edge, *key)?
                }
                None => None,
            },
        ),
        Operand::Literal(value) => Value::Scalar(Some(value.value())),
    })
}
fn compare(left: &Value<'_>, right: &Value<'_>, comparison: IntegerComparison) -> Truth {
    if left.is_null() || right.is_null() {
        return Truth::Unknown;
    }
    match (left, right) {
        (Value::Vertex(Some(left)), Value::Vertex(Some(right))) => {
            Truth::from(Some(if comparison == IntegerComparison::Equal {
                left == right
            } else {
                left != right
            }))
        }
        (Value::Scalar(Some(left)), Value::Scalar(Some(right))) => {
            if core::mem::discriminant(*left) != core::mem::discriminant(*right) {
                Truth::Unknown
            } else {
                Truth::from(Some(
                    comparison.accepts_scalar_pair(Some(*left), Some(*right)),
                ))
            }
        }
        _ => Truth::Unknown,
    }
}

impl BoundBooleanExpression {
    pub(crate) fn remap(&self, mut map: impl FnMut(BindingSlot) -> BindingSlot) -> Self {
        self.remap_elements(&mut map, |capture| capture)
    }

    pub(crate) fn remap_elements(
        &self,
        mut map: impl FnMut(BindingSlot) -> BindingSlot,
        mut capture: impl FnMut(u32) -> u32,
    ) -> Self {
        let program = self
            .program
            .iter()
            .map(|instruction| {
                let result: Result<_, core::convert::Infallible> =
                    instruction.try_map(&mut |slot, edge| {
                        Ok(if edge {
                            BindingSlot(capture(slot.ordinal()))
                        } else {
                            map(*slot)
                        })
                    });
                match result {
                    Ok(value) => value,
                    Err(never) => match never {},
                }
            })
            .collect::<Vec<_>>();
        Self {
            program: program.into(),
        }
    }

    /// Versioned, value-independent transcript of the compiled boolean
    /// template. Resolved binding slots, property keys, comparisons and
    /// literal constants are encoded; no database values, statement text or
    /// diagnostic offsets appear. The certificate layer hashes this with its
    /// own domain prefix, so the encoding here needs no extra framing.
    #[must_use]
    pub(crate) fn template_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:gql:boolean-template:v1\0".to_vec();
        self.append_transcript(&mut bytes);
        bytes
    }

    pub(crate) fn evaluate<'a, E>(
        &self,
        bindings: &[Option<VId>],
        property: &mut impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<bool, E> {
        self.evaluate_elements(
            bindings,
            &[],
            property,
            &mut |_, _| panic!("edge properties require an explicit edge property source"),
            control,
        )
    }

    pub(crate) fn evaluate_elements<'a, E>(
        &self,
        bindings: &[Option<VId>],
        paths: &[Option<super::GraphPath>],
        property: &mut impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        edge_property: &mut impl FnMut(EId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<bool, E> {
        let mut stack = [Truth::Unknown; MAX_PATTERN_PREDICATES];
        let mut depth = 0_usize;
        for instruction in self.program.iter() {
            control(GlaExecutionEvent::Work)?;
            let value = match instruction {
                Instruction::Compare {
                    left,
                    comparison,
                    right,
                } => {
                    let left = resolve(left, bindings, paths, property, edge_property, control)?;
                    let right = resolve(right, bindings, paths, property, edge_property, control)?;
                    for value in [&left, &right] {
                        if let Value::Scalar(Some(value)) = value {
                            crate::algebra_exec::charge_payload(value, control)?;
                        }
                    }
                    compare(&left, &right, *comparison)
                }
                Instruction::IsNull { operand, is_null } => Truth::from(Some(
                    resolve(operand, bindings, paths, property, edge_property, control)?.is_null()
                        == *is_null,
                )),
                Instruction::Expression {
                    expression,
                    columns,
                } => {
                    for _ in columns {
                        control(GlaExecutionEvent::ScratchEntry)?;
                    }
                    let mut values = Vec::with_capacity(columns.len());
                    for column in columns {
                        let value = match resolve(
                            column,
                            bindings,
                            paths,
                            property,
                            edge_property,
                            control,
                        )? {
                            Value::Vertex(Some(vertex)) => GraphValue::Vertex(vertex),
                            Value::Scalar(Some(value)) => {
                                crate::algebra_exec::charge_payload(value, control)?;
                                GraphValue::Scalar(value.clone())
                            }
                            Value::Vertex(None) | Value::Scalar(None) => {
                                GraphValue::Scalar(CanonicalScalar::Null)
                            }
                        };
                        values.push(value);
                    }
                    match expression.evaluate_scalar_with_control(&values, control) {
                        Ok(CanonicalScalar::Bool(value)) => Truth::from(Some(value)),
                        Ok(_) | Err(GraphIntegerEvaluationError::Value(_)) => Truth::Unknown,
                        Err(GraphIntegerEvaluationError::Control(error)) => return Err(error),
                    }
                }
                Instruction::Truth(value) => Truth::from(*value),
                Instruction::Not => {
                    stack[depth - 1] = stack[depth - 1].not();
                    continue;
                }
                Instruction::And | Instruction::Or => {
                    let right = stack[depth - 1];
                    depth -= 1;
                    stack[depth - 1] = if matches!(instruction, Instruction::And) {
                        stack[depth - 1].and(right)
                    } else {
                        stack[depth - 1].or(right)
                    };
                    continue;
                }
            };
            stack[depth] = value;
            depth += 1;
        }
        debug_assert_eq!(depth, 1);
        Ok(stack[0] == Truth::True)
    }

    pub(crate) fn append_transcript(&self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(&(self.program.len() as u64).to_be_bytes());
        for instruction in self.program.iter() {
            match instruction {
                Instruction::Compare {
                    left,
                    comparison,
                    right,
                } => {
                    bytes.push(0);
                    append_operand(left, bytes);
                    bytes.push(comparison.tag());
                    append_operand(right, bytes);
                }
                Instruction::IsNull { operand, is_null } => {
                    bytes.push(1);
                    append_operand(operand, bytes);
                    bytes.push(u8::from(*is_null));
                }
                Instruction::Truth(value) => {
                    bytes.push(2);
                    bytes.push(match value {
                        None => 0,
                        Some(false) => 1,
                        Some(true) => 2,
                    });
                }
                Instruction::And => bytes.push(3),
                Instruction::Or => bytes.push(4),
                Instruction::Not => bytes.push(5),
                Instruction::Expression {
                    expression,
                    columns,
                } => {
                    bytes.push(6);
                    let encoded = expression.canonical_bytes();
                    bytes.extend_from_slice(&(encoded.len() as u64).to_be_bytes());
                    bytes.extend_from_slice(&encoded);
                    bytes.extend_from_slice(&(columns.len() as u64).to_be_bytes());
                    for column in columns {
                        append_operand(column, bytes);
                    }
                }
            }
        }
    }
}
fn append_operand(operand: &Operand<BindingSlot>, bytes: &mut Vec<u8>) {
    match operand {
        Operand::Vertex(slot) => {
            bytes.push(0);
            bytes.extend_from_slice(&slot.ordinal().to_be_bytes());
        }
        Operand::Property { variable, key } => {
            bytes.push(1);
            bytes.extend_from_slice(&variable.ordinal().to_be_bytes());
            bytes.extend_from_slice(&key.0.to_be_bytes());
        }
        Operand::EdgeProperty { variable, key } => {
            bytes.push(3);
            bytes.extend_from_slice(&variable.ordinal().to_be_bytes());
            bytes.extend_from_slice(&key.0.to_be_bytes());
        }
        Operand::Literal(value) => {
            bytes.push(2);
            let encoded = value.canonical_value_bytes();
            bytes.extend_from_slice(&(encoded.len() as u64).to_be_bytes());
            bytes.extend_from_slice(encoded);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use GraphBooleanOp as Op;
    use GraphBooleanOperand as Arg;

    fn bound(ops: &[Op<'_>]) -> BoundBooleanExpression {
        GraphBooleanExpression::prepare(ops)
            .unwrap()
            .bind(
                |name| match name {
                    "a" => Ok(0),
                    "b" => Ok(1),
                    _ => Err(PatternBuildError::UnknownVariable),
                },
                |name| match name {
                    "a" | "b" => Ok(0),
                    _ => Err(PatternBuildError::UnknownVariable),
                },
            )
            .unwrap()
    }

    #[test]
    fn complete_three_valued_truth_tables_and_de_morgan_laws() {
        let values = [Truth::False, Truth::Unknown, Truth::True];
        let and = [[0, 0, 0], [0, 1, 1], [0, 1, 2]];
        let or = [[0, 1, 2], [1, 1, 2], [2, 2, 2]];
        for (i, left) in values.into_iter().enumerate() {
            assert_eq!(left.not().not(), left);
            for (j, right) in values.into_iter().enumerate() {
                assert_eq!(left.and(right), values[and[i][j]]);
                assert_eq!(left.or(right), values[or[i][j]]);
                assert_eq!(left.and(right).not(), left.not().or(right.not()));
                assert_eq!(left.or(right).not(), left.not().and(right.not()));
                for (op, expected) in [(Op::And, values[and[i][j]]), (Op::Or, values[or[i][j]])] {
                    let literal = |value| match value {
                        Truth::False => Some(false),
                        Truth::Unknown => None,
                        Truth::True => Some(true),
                    };
                    let program = bound(&[Op::Truth(literal(left)), Op::Truth(literal(right)), op]);
                    let actual = program
                        .evaluate(
                            &[],
                            &mut |_, _| Err::<Option<&CanonicalScalar>, _>("unexpected source"),
                            &mut |_| Ok(()),
                        )
                        .unwrap();
                    assert_eq!(actual, expected == Truth::True);
                }
            }
        }
    }

    #[test]
    fn negation_does_not_admit_missing_null_or_incompatible_values() {
        let expected = CanonicalScalar::Int(7);
        let values = [
            None,
            Some(CanonicalScalar::Null),
            Some(CanonicalScalar::Bool(true)),
            Some(CanonicalScalar::Int(7)),
            Some(CanonicalScalar::Int(8)),
        ];
        for comparison in [
            IntegerComparison::Equal,
            IntegerComparison::NotEqual,
            IntegerComparison::Greater,
            IntegerComparison::Less,
            IntegerComparison::GreaterOrEqual,
            IntegerComparison::LessOrEqual,
        ] {
            let program = bound(&[
                Op::Compare {
                    left: Arg::Property {
                        variable: "a",
                        key: PropertyKeyId(3),
                    },
                    comparison,
                    right: Arg::Literal(&expected),
                },
                Op::Not,
            ]);
            for value in &values {
                let actual = program
                    .evaluate(
                        &[Some(VId(0))],
                        &mut |_, _| Ok::<_, ()>(value.as_ref()),
                        &mut |_| Ok(()),
                    )
                    .unwrap();
                let expected = match value {
                    Some(CanonicalScalar::Int(n)) => !comparison.accepts(*n, 7),
                    _ => false,
                };
                assert_eq!(actual, expected);
            }
        }
        let null = bound(&[Op::IsNull {
            operand: Arg::Property {
                variable: "a",
                key: PropertyKeyId(3),
            },
            is_null: true,
        }]);
        assert!(
            null.evaluate(
                &[None],
                &mut |_, _| Err::<Option<&CanonicalScalar>, _>("null read"),
                &mut |_| Ok(())
            )
            .unwrap()
        );
    }

    #[test]
    fn all_leaf_reads_are_fallible_even_after_true_disjunction_or_false_conjunction() {
        let value = CanonicalScalar::Int(1);
        for (left, operator) in [(true, Op::Or), (false, Op::And)] {
            let expression = bound(&[
                Op::Truth(Some(left)),
                Op::Compare {
                    left: Arg::Property {
                        variable: "a",
                        key: PropertyKeyId(1),
                    },
                    comparison: IntegerComparison::Equal,
                    right: Arg::Literal(&value),
                },
                operator,
            ]);
            let mut reads = 0;
            let result = expression.evaluate(
                &[Some(VId(u128::MAX))],
                &mut |vid, _| {
                    assert_eq!(vid, VId(u128::MAX));
                    reads += 1;
                    Err::<Option<&CanonicalScalar>, _>("unreadable")
                },
                &mut |_| Ok(()),
            );
            assert_eq!(result, Err("unreadable"));
            assert_eq!(reads, 1);
        }
    }

    #[test]
    fn admission_validates_stack_limits_and_names_before_binding() {
        assert!(matches!(
            GraphBooleanExpression::prepare(&[]),
            Err(GraphBooleanError::Empty)
        ));
        for program in [
            vec![Op::And],
            vec![Op::Not],
            vec![Op::Truth(None), Op::Truth(None)],
            vec![Op::Truth(None), Op::Or],
        ] {
            assert!(matches!(
                GraphBooleanExpression::prepare(&program),
                Err(GraphBooleanError::InvalidStack { .. })
            ));
        }
        assert!(matches!(
            GraphBooleanExpression::prepare(&vec![Op::Truth(None); MAX_BOOLEAN_INSTRUCTIONS + 1]),
            Err(GraphBooleanError::TooManyInstructions { .. })
        ));
        let mut maximal = vec![Op::Truth(Some(true)); MAX_PATTERN_PREDICATES];
        maximal.extend(std::iter::repeat_n(Op::And, MAX_PATTERN_PREDICATES - 1));
        assert!(
            bound(&maximal)
                .evaluate(&[], &mut |_, _| Ok::<_, ()>(None), &mut |_| Ok(()))
                .unwrap()
        );
        maximal.insert(0, Op::Truth(None));
        assert!(matches!(
            GraphBooleanExpression::prepare(&maximal),
            Err(GraphBooleanError::TooManyPredicates { .. })
        ));
        let bad = [Op::IsNull {
            operand: Arg::Vertex("not valid"),
            is_null: true,
        }];
        assert!(matches!(
            GraphBooleanExpression::prepare(&bad),
            Err(GraphBooleanError::InvalidVariableName)
        ));
        let literal = CanonicalScalar::Int(1);
        assert!(matches!(
            GraphBooleanExpression::prepare(&[Op::Compare {
                left: Arg::Vertex("a"),
                comparison: IntegerComparison::Equal,
                right: Arg::Literal(&literal)
            }]),
            Err(GraphBooleanError::InvalidVertexComparison)
        ));
    }

    #[test]
    fn checked_operands_share_storage_and_transcripts_use_slots_not_names() {
        let scalar = ScalarPredicate::new(
            CanonicalScalar::ucs_basic_text("private").unwrap(),
            IntegerComparison::Greater,
        )
        .unwrap();
        let build = |name| {
            GraphBooleanExpression::prepare(&[Op::Compare {
                left: Arg::Property {
                    variable: name,
                    key: PropertyKeyId(1),
                },
                comparison: IntegerComparison::Equal,
                right: Arg::CheckedLiteral(&scalar),
            }])
            .unwrap()
        };
        let first = build("secret");
        let Instruction::Compare {
            right: Operand::Literal(stored),
            ..
        } = &first.program[0]
        else {
            panic!("literal");
        };
        assert!(std::ptr::eq(
            stored.canonical_value_bytes().as_ptr(),
            scalar.canonical_value_bytes().as_ptr()
        ));
        let mut one = Vec::new();
        let mut two = Vec::new();
        let first = first.bind(|_| Ok(0), |_| Ok(0)).unwrap();
        let renamed = build("renamed").bind(|_| Ok(0), |_| Ok(0)).unwrap();
        first.append_transcript(&mut one);
        renamed.append_transcript(&mut two);
        assert_eq!(one, two);
        let moved = first.remap(|slot| BindingSlot(slot.ordinal() + 7));
        two.clear();
        moved.append_transcript(&mut two);
        assert_ne!(one, two);
        assert!(!format!("{first:?}").contains("private"));
        assert!(!format!("{first:?}").contains("secret"));
    }

    #[test]
    fn every_expression_checkpoint_preserves_interruptions_and_definition() {
        let value = CanonicalScalar::ucs_basic_text(&"q".repeat(129)).unwrap();
        let expression = bound(&[
            Op::Compare {
                left: Arg::Property {
                    variable: "a",
                    key: PropertyKeyId(1),
                },
                comparison: IntegerComparison::Equal,
                right: Arg::Literal(&value),
            },
            Op::Not,
            Op::IsNull {
                operand: Arg::Vertex("b"),
                is_null: true,
            },
            Op::Or,
        ]);
        let mut calls = 0;
        assert!(
            expression
                .evaluate(
                    &[Some(VId(0)), None],
                    &mut |_, _| Ok::<_, usize>(Some(&value)),
                    &mut |_| {
                        calls += 1;
                        Ok(())
                    }
                )
                .unwrap()
        );
        let before = expression.clone();
        for stop in 1..=calls {
            let mut at = 0;
            let result = expression.evaluate(
                &[Some(VId(0)), None],
                &mut |_, _| Ok(Some(&value)),
                &mut |_| {
                    at += 1;
                    if at == stop { Err(stop) } else { Ok(()) }
                },
            );
            assert_eq!(result, Err(stop));
            assert_eq!(at, stop);
            assert_eq!(expression, before);
        }
    }
    #[test]
    fn template_bytes_are_deterministic_and_structure_sensitive() {
        let int = CanonicalScalar::Int(7);
        let build = || {
            bound(&[
                Op::Compare {
                    left: Arg::Property {
                        variable: "a",
                        key: PropertyKeyId(3),
                    },
                    comparison: IntegerComparison::Greater,
                    right: Arg::Literal(&int),
                },
                Op::IsNull {
                    operand: Arg::Vertex("b"),
                    is_null: false,
                },
                Op::And,
            ])
        };
        // Determinism: two independently bound copies of the same template.
        let first = build();
        let second = build();
        assert_eq!(first.template_bytes(), second.template_bytes());

        // Property identity, comparison, slots and program structure are bound.
        let different_key = bound(&[
            Op::Compare {
                left: Arg::Property {
                    variable: "a",
                    key: PropertyKeyId(4),
                },
                comparison: IntegerComparison::Greater,
                right: Arg::Literal(&int),
            },
            Op::IsNull {
                operand: Arg::Vertex("b"),
                is_null: false,
            },
            Op::And,
        ]);
        assert_ne!(first.template_bytes(), different_key.template_bytes());

        let different_comparison = bound(&[
            Op::Compare {
                left: Arg::Property {
                    variable: "a",
                    key: PropertyKeyId(3),
                },
                comparison: IntegerComparison::Less,
                right: Arg::Literal(&int),
            },
            Op::IsNull {
                operand: Arg::Vertex("b"),
                is_null: false,
            },
            Op::And,
        ]);
        assert_ne!(
            first.template_bytes(),
            different_comparison.template_bytes()
        );

        let different_slot = bound(&[
            Op::Compare {
                left: Arg::Property {
                    variable: "a",
                    key: PropertyKeyId(3),
                },
                comparison: IntegerComparison::Greater,
                right: Arg::Literal(&int),
            },
            Op::IsNull {
                operand: Arg::Vertex("a"),
                is_null: false,
            },
            Op::And,
        ]);
        assert_ne!(first.template_bytes(), different_slot.template_bytes());

        let different_structure = bound(&[Op::Truth(Some(true)), Op::Not]);
        assert_ne!(first.template_bytes(), different_structure.template_bytes());

        // Same template, different slot remapping: different bytes.
        let remapped = first.remap(|slot| BindingSlot(slot.ordinal() + 10));
        assert_ne!(first.template_bytes(), remapped.template_bytes());
    }
}
