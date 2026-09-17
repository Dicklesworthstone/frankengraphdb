//! Row predicates between relational stages. No graph or source is reopened.
//!
//! Comparisons use the same canonical scalar law as GLA WHERE, not the total
//! ordering used by set equality. Evaluation is eager and three-valued; only
//! TRUE retains a row. Project computed operands first, then filter their cells.

use super::{GraphSetBuildError, GraphSetColumnType, PreparedGraphSet, SetNode, check_depth};
use crate::algebra::{
    GraphValue, GraphValueRow, IntegerComparison, MAX_BOOLEAN_INSTRUCTIONS, MAX_PATTERN_PREDICATES,
};
use crate::{GlaExecutionEvent, GqlScalarParameter};
use fgdb_types::CanonicalScalar;

/// Positions refer to the completed input relation, never private graph slots.
/// Literals have already passed the ordinary scalar-parameter admission rules.
#[derive(Clone, PartialEq, Eq)]
pub enum GraphSetOperand {
    Column(usize),
    Literal(GqlScalarParameter),
}
impl core::fmt::Debug for GraphSetOperand {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphSetOperand([REDACTED])")
    }
}

/// Checked postfix Boolean IR. Every comparison and null test pushes one
/// Boolean; NOT consumes one and AND/OR consume two. Truth(None) is UNKNOWN.
/// Aliases and parameter names are resolved during preparation, not execution.
#[derive(Clone, PartialEq, Eq)]
pub enum GraphSetPredicateOp {
    Compare {
        left: GraphSetOperand,
        comparison: IntegerComparison,
        right: GraphSetOperand,
    },
    IsNull {
        operand: GraphSetOperand,
        is_null: bool,
    },
    Truth(Option<bool>),
    Not,
    And,
    Or,
}
impl core::fmt::Debug for GraphSetPredicateOp {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphSetPredicateOp([REDACTED])")
    }
}
impl GraphSetPredicateOp {
    // Native preparation validates even parameterized filters before catalog
    // callbacks. Placeholder scalar values prove structure, never truth.
    pub(crate) fn validate_schema(
        types: &[GraphSetColumnType],
        code: &[Self],
    ) -> Result<(), GraphSetFilterError> {
        RowPredicate::prepare(types, code).map(|_| ())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphSetFilterError {
    Empty,
    TooManyInstructions { limit: usize, observed: usize },
    TooManyPredicates { limit: usize, observed: usize },
    InvalidStack { instruction: usize },
    UnknownInput { instruction: usize, column: usize },
    InvalidVertexComparison { instruction: usize },
    InvalidValueComparison { instruction: usize },
    SetBuild(GraphSetBuildError),
}
impl core::fmt::Display for GraphSetFilterError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "graph row filter definition: {self:?}")
    }
}
impl core::error::Error for GraphSetFilterError {}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct RowPredicate {
    code: Box<[GraphSetPredicateOp]>,
}
impl RowPredicate {
    pub(super) fn prepare(
        types: &[GraphSetColumnType],
        code: &[GraphSetPredicateOp],
    ) -> Result<Self, GraphSetFilterError> {
        use GraphSetFilterError as Error;
        if code.is_empty() {
            return Err(Error::Empty);
        }
        if code.len() > MAX_BOOLEAN_INSTRUCTIONS {
            return Err(Error::TooManyInstructions {
                limit: MAX_BOOLEAN_INSTRUCTIONS,
                observed: code.len(),
            });
        }
        let mut depth = 0;
        let mut predicates = 0;
        for (instruction, op) in code.iter().enumerate() {
            let domain = |operand: &GraphSetOperand| match operand {
                GraphSetOperand::Column(column) => {
                    types.get(*column).copied().ok_or(Error::UnknownInput {
                        instruction,
                        column: *column,
                    })
                }
                GraphSetOperand::Literal(_) => Ok(GraphSetColumnType::Scalar),
            };
            match op {
                GraphSetPredicateOp::Not if depth >= 1 => continue,
                GraphSetPredicateOp::And | GraphSetPredicateOp::Or if depth >= 2 => {
                    depth -= 1;
                    continue;
                }
                GraphSetPredicateOp::Compare {
                    left,
                    comparison,
                    right,
                } => {
                    let left = domain(left)?;
                    let right = domain(right)?;
                    if matches!(
                        left,
                        GraphSetColumnType::Path
                            | GraphSetColumnType::Vertices
                            | GraphSetColumnType::Edges
                    ) || matches!(
                        right,
                        GraphSetColumnType::Path
                            | GraphSetColumnType::Vertices
                            | GraphSetColumnType::Edges
                    ) {
                        return Err(Error::InvalidValueComparison { instruction });
                    }
                    if (left == GraphSetColumnType::Vertex || right == GraphSetColumnType::Vertex)
                        && !(left == right
                            && matches!(
                                comparison,
                                IntegerComparison::Equal | IntegerComparison::NotEqual
                            ))
                    {
                        return Err(Error::InvalidVertexComparison { instruction });
                    }
                }
                GraphSetPredicateOp::IsNull { operand, .. } => {
                    domain(operand)?;
                }
                GraphSetPredicateOp::Truth(_) => {}
                _ => return Err(Error::InvalidStack { instruction }),
            }
            depth += 1;
            predicates += 1;
            if predicates > MAX_PATTERN_PREDICATES {
                return Err(Error::TooManyPredicates {
                    limit: MAX_PATTERN_PREDICATES,
                    observed: predicates,
                });
            }
        }
        if depth != 1 {
            return Err(Error::InvalidStack {
                instruction: code.len(),
            });
        }
        // The complete definition and schema are checked before cloning literals.
        Ok(Self {
            code: code.to_vec().into_boxed_slice(),
        })
    }

    pub(super) fn evaluate<E>(
        &self,
        row: &GraphValueRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<bool, E> {
        let mut stack: [Option<bool>; MAX_PATTERN_PREDICATES] = [None; MAX_PATTERN_PREDICATES];
        let mut depth = 0;
        for op in self.code.iter() {
            control(GlaExecutionEvent::Work)?;
            let value = match op {
                GraphSetPredicateOp::Compare {
                    left,
                    comparison,
                    right,
                } => {
                    let left = resolve(left, row, control)?;
                    let right = resolve(right, row, control)?;
                    for value in [left, right] {
                        if let Cell::Scalar(value) = value {
                            crate::algebra_exec::charge_payload(value, control)?;
                        }
                    }
                    compare(left, right, *comparison)
                }
                GraphSetPredicateOp::IsNull { operand, is_null } => {
                    Some(resolve(operand, row, control)?.is_null() == *is_null)
                }
                GraphSetPredicateOp::Truth(value) => *value,
                GraphSetPredicateOp::Not => {
                    stack[depth - 1] = stack[depth - 1].map(|value| !value);
                    continue;
                }
                GraphSetPredicateOp::And | GraphSetPredicateOp::Or => {
                    let right = stack[depth - 1];
                    depth -= 1;
                    let left = stack[depth - 1];
                    stack[depth - 1] = if matches!(op, GraphSetPredicateOp::And) {
                        if left == Some(false) || right == Some(false) {
                            Some(false)
                        } else {
                            left.zip(right).map(|(a, b)| a && b)
                        }
                    } else if left == Some(true) || right == Some(true) {
                        Some(true)
                    } else {
                        left.zip(right).map(|(a, b)| a || b)
                    };
                    continue;
                }
            };
            stack[depth] = value;
            depth += 1;
        }
        debug_assert_eq!(depth, 1);
        Ok(stack[0] == Some(true))
    }

    pub(super) fn append_transcript(&self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(&(self.code.len() as u64).to_be_bytes());
        for op in self.code.iter() {
            match op {
                GraphSetPredicateOp::Compare {
                    left,
                    comparison,
                    right,
                } => {
                    bytes.push(0);
                    append_operand(left, bytes);
                    bytes.push(match comparison {
                        IntegerComparison::Equal => 0,
                        IntegerComparison::NotEqual => 1,
                        IntegerComparison::Greater => 2,
                        IntegerComparison::Less => 3,
                        IntegerComparison::GreaterOrEqual => 4,
                        IntegerComparison::LessOrEqual => 5,
                    });
                    append_operand(right, bytes);
                }
                GraphSetPredicateOp::IsNull { operand, is_null } => {
                    bytes.push(1);
                    append_operand(operand, bytes);
                    bytes.push(u8::from(*is_null));
                }
                GraphSetPredicateOp::Truth(value) => bytes.extend_from_slice(&[
                    2,
                    match value {
                        None => 0,
                        Some(false) => 1,
                        Some(true) => 2,
                    },
                ]),
                GraphSetPredicateOp::Not => bytes.push(3),
                GraphSetPredicateOp::And => bytes.push(4),
                GraphSetPredicateOp::Or => bytes.push(5),
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Cell<'a> {
    Scalar(&'a CanonicalScalar),
    Vertex(fgdb_types::VId),
    Incompatible,
}
impl Cell<'_> {
    fn is_null(self) -> bool {
        matches!(self, Self::Scalar(CanonicalScalar::Null))
    }
}
fn resolve<'a, E>(
    operand: &'a GraphSetOperand,
    row: &'a GraphValueRow,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Cell<'a>, E> {
    control(GlaExecutionEvent::Work)?;
    Ok(match operand {
        GraphSetOperand::Column(column) => match &row.values()[*column] {
            GraphValue::Vertex(value) => Cell::Vertex(*value),
            GraphValue::Scalar(value) => Cell::Scalar(value),
            GraphValue::Path(_) | GraphValue::Vertices(_) | GraphValue::Edges(_) => {
                Cell::Incompatible
            }
        },
        GraphSetOperand::Literal(value) => Cell::Scalar(value.value()),
    })
}
fn compare(left: Cell<'_>, right: Cell<'_>, comparison: IntegerComparison) -> Option<bool> {
    if left.is_null() || right.is_null() {
        return None;
    }
    match (left, right) {
        (Cell::Vertex(left), Cell::Vertex(right)) => Some(match comparison {
            IntegerComparison::Equal => left == right,
            IntegerComparison::NotEqual => left != right,
            _ => unreachable!("vertex ordering is rejected before execution"),
        }),
        (Cell::Scalar(left), Cell::Scalar(right))
            if core::mem::discriminant(left) == core::mem::discriminant(right) =>
        {
            Some(comparison.accepts_scalar_pair(Some(left), Some(right)))
        }
        _ => None,
    }
}
fn append_operand(operand: &GraphSetOperand, bytes: &mut Vec<u8>) {
    match operand {
        GraphSetOperand::Column(column) => {
            bytes.push(0);
            bytes.extend_from_slice(&(*column as u64).to_be_bytes());
        }
        GraphSetOperand::Literal(value) => {
            bytes.push(1);
            bytes.extend_from_slice(&(value.canonical_bytes().len() as u64).to_be_bytes());
            bytes.extend_from_slice(value.canonical_bytes());
        }
    }
}

impl PreparedGraphSet {
    /// Filter the completed input, preserving its duplicates and order. The
    /// child's DISTINCT/order/page runs BEFORE this predicate; this new scope's
    /// own order/page runs AFTER it. Only TRUE survives. NULL and incompatible
    /// scalar kinds produce UNKNOWN, including beneath NOT. Vertex comparisons
    /// require two vertex columns and equality/inequality; IS NULL is explicit.
    ///
    /// No source is reopened and no payload is cloned. The materialized set
    /// executor charges each retained row and every predicate operation under
    /// its one cumulative allowance. A late error or cancellation releases no
    /// result prefix, even for an outer LIMIT 0.
    pub fn filter(self, code: &[GraphSetPredicateOp]) -> Result<Self, GraphSetFilterError> {
        let depth = self.depth + 1;
        check_depth(depth).map_err(GraphSetFilterError::SetBuild)?;
        let predicate = RowPredicate::prepare(&self.types, code)?;
        Ok(Self {
            columns: self.columns.clone(),
            types: self.types.clone(),
            operands: self.operands,
            depth,
            node: SetNode::Filter {
                input: Box::new(self),
                predicate,
            },
            order: Vec::new(),
            offset: 0,
            count: None,
        })
    }
}
