//! Typed relational projection over complete GLA/set rows. Expressions see the
//! original row, never another output alias. No graph is traversed a second time.

use super::{GraphSetBuildError, GraphSetColumnType, GraphSetQuantifier, PreparedGraphSet, SetNode, check_depth};
use crate::algebra::{GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphValue, GraphValueRow, MAX_PATTERN_NAME_BYTES, MAX_PATTERN_VERTICES};
use crate::{GlaExecutionEvent, GraphIntegerError, GraphIntegerEvaluationError, GraphIntegerExpression, GqlScalarParameter};
use fgdb_types::CanonicalScalar;
use std::collections::BTreeSet;

/// Checked output expression. Column positions refer to the input relation,
/// including when that relation is itself a projection or compound set.
#[derive(Clone, PartialEq, Eq)]
pub enum GraphSetValue {
    Column(usize),
    Literal(GqlScalarParameter),
    Integer(GraphIntegerExpression),
}
impl core::fmt::Debug for GraphSetValue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphSetValue([REDACTED])")
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct GraphSetProjection {
    name: String,
    value: GraphSetValue,
}
impl GraphSetProjection {
    #[must_use]
    pub fn new(name: impl Into<String>, value: GraphSetValue) -> Self {
        Self { name: name.into(), value }
    }
    #[must_use]
    pub fn name(&self) -> &str { &self.name }
    #[must_use]
    pub fn value(&self) -> &GraphSetValue { &self.value }
}
impl core::fmt::Debug for GraphSetProjection {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphSetProjection([REDACTED])")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphSetProjectionError {
    Empty,
    TooManyColumns { limit: usize, observed: usize },
    InvalidName { column: usize },
    DuplicateName { column: usize },
    UnknownInput { column: usize, input: usize },
    IntegerInput { column: usize, input: usize },
    SetBuild(GraphSetBuildError),
}
impl core::fmt::Display for GraphSetProjectionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "graph projection definition: {self:?}")
    }
}
impl core::error::Error for GraphSetProjectionError {}

impl PreparedGraphSet {
    /// Project the completed input before this new scope's DISTINCT/order/page.
    /// The input retains its own page. All columns read the same frozen input;
    /// aliases never become temporary variable bindings. Numeric references are
    /// checked against scalar domains before a source can be executed, including
    /// columns in lazy branches. Individual scalar kinds are checked at runtime.
    /// A constant projection still emits once per input occurrence, not once
    /// per distinct hidden carrier. The bounded materialized profile does not
    /// claim spill-backed expression evaluation or arithmetic aggregate support.
    pub fn project(self, projection: Vec<GraphSetProjection>, quantifier: GraphSetQuantifier)
        -> Result<Self, GraphSetProjectionError> {
        use GraphSetProjectionError as Error;
        if projection.is_empty() { return Err(Error::Empty); }
        if projection.len() > MAX_PATTERN_VERTICES {
            return Err(Error::TooManyColumns { limit: MAX_PATTERN_VERTICES, observed: projection.len() });
        }
        let depth = self.depth + 1;
        check_depth(depth).map_err(Error::SetBuild)?;
        let mut names = BTreeSet::new();
        let mut types = Vec::new();
        for (column, output) in projection.iter().enumerate() {
            let bytes = output.name.as_bytes();
            if bytes.is_empty() || bytes.len() > MAX_PATTERN_NAME_BYTES
                || !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
                || !bytes.iter().all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            { return Err(Error::InvalidName { column }); }
            if !names.insert(output.name.as_str()) { return Err(Error::DuplicateName { column }); }
            types.push(match &output.value {
                GraphSetValue::Column(input) => *self.types.get(*input)
                    .ok_or(Error::UnknownInput { column, input: *input })?,
                GraphSetValue::Literal(_) => GraphSetColumnType::Scalar,
                GraphSetValue::Integer(expression) => {
                    for input in expression.referenced_columns() {
                        let kind = self.types.get(input).ok_or(Error::UnknownInput { column, input })?;
                        if *kind != GraphSetColumnType::Scalar { return Err(Error::IntegerInput { column, input }); }
                    }
                    GraphSetColumnType::Scalar
                }
            });
        }
        Ok(Self {
            columns: projection.iter().map(|column| column.name.clone()).collect(),
            types, operands: self.operands, depth,
            node: SetNode::Project { input: Box::new(self), projection, quantifier },
            order: Vec::new(), offset: 0, count: None,
        })
    }
}

pub(super) fn append_transcript(projection: &[GraphSetProjection], bytes: &mut Vec<u8>) {
    bytes.extend_from_slice(&(projection.len() as u64).to_be_bytes());
    for column in projection {
        match &column.value {
            GraphSetValue::Column(input) => {
                bytes.push(0); bytes.extend_from_slice(&(*input as u64).to_be_bytes());
            }
            GraphSetValue::Literal(value) => {
                bytes.push(1);
                bytes.extend_from_slice(&(value.canonical_bytes().len() as u64).to_be_bytes());
                bytes.extend_from_slice(value.canonical_bytes());
            }
            GraphSetValue::Integer(expression) => {
                bytes.push(2);
                let value = expression.canonical_bytes();
                bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
                bytes.extend_from_slice(&value);
            }
        }
    }
}

pub(super) enum ProjectionFailure<E> {
    Control(E),
    Arithmetic { column: usize, error: GraphIntegerError },
}

fn copy_value<E>(value: &GraphValue, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>)
    -> Result<GraphValue, E> {
    let sizes = match value {
        GraphValue::Scalar(CanonicalScalar::Text(value)) =>
            [value.len(), value.canonical_sort_key().map_or(0, <[u8]>::len)],
        GraphValue::Scalar(CanonicalScalar::Bytes(value)) => [value.as_slice().len(), 0],
        GraphValue::Scalar(CanonicalScalar::Timestamp(value)) =>
            [value.zone().map_or(0, |zone| zone.identifier().len()), 0],
        _ => [0, 0],
    };
    control(GlaExecutionEvent::ScratchEntry)?;
    for bytes in sizes {
        for _ in 0..bytes.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
            control(GlaExecutionEvent::ScratchEntry)?;
        }
    }
    Ok(value.clone())
}

/// The input is a private owned row whose complete schema was admitted by the
/// enclosing set executor. Every output cell and variable payload is reserved
/// before growth. A refusal drops the whole private row, never a short result.
/// Arithmetic is not elided for DISTINCT duplicates or a final LIMIT 0.
pub(super) fn evaluate<E>(row: &GraphValueRow, projection: &[GraphSetProjection],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>)
    -> Result<GraphValueRow, ProjectionFailure<E>> {
    let mut values = Vec::new();
    for (column, output) in projection.iter().enumerate() {
        control(GlaExecutionEvent::Work).map_err(ProjectionFailure::Control)?;
        let value = match &output.value {
            GraphSetValue::Column(input) => copy_value(&row.values()[*input], control)
                .map_err(ProjectionFailure::Control)?,
            GraphSetValue::Literal(value) => {
                // Borrow the checked scalar while reserving its eventual copy.
                // The temporary GraphValue owns a clone, so use the shared row
                // copier only after the payload reservations below instead.
                let scalar = value.value();
                let sizes = match scalar {
                    CanonicalScalar::Text(value) => [value.len(), value.canonical_sort_key().map_or(0, <[u8]>::len)],
                    CanonicalScalar::Bytes(value) => [value.as_slice().len(), 0],
                    CanonicalScalar::Timestamp(value) => [value.zone().map_or(0, |zone| zone.identifier().len()), 0],
                    _ => [0, 0],
                };
                control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                for bytes in sizes {
                    for _ in 0..bytes.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
                        control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                    }
                }
                GraphValue::Scalar(scalar.clone())
            }
            GraphSetValue::Integer(expression) => {
                let value = expression.evaluate_with_control(row.values(), control).map_err(|error| match error {
                    GraphIntegerEvaluationError::Control(error) => ProjectionFailure::Control(error),
                    GraphIntegerEvaluationError::Value(error) => ProjectionFailure::Arithmetic { column, error },
                })?;
                control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
            }
        };
        values.push(value);
    }
    Ok(GraphValueRow::from_owned_values(values))
}
