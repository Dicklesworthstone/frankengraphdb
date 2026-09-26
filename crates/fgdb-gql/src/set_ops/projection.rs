//! Typed relational projection over complete GLA/set rows. Expressions see the
//! original row, never another output alias. No graph is traversed a second time.

use super::{
    GraphSetBuildError, GraphSetColumnType, GraphSetQuantifier, PreparedGraphSet, SetNode,
    check_depth,
};
use crate::algebra::{
    GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphValue, GraphValueRow, MAX_PATTERN_NAME_BYTES,
    MAX_PATTERN_VERTICES,
};
use crate::{
    GlaExecutionEvent, GqlScalarParameter, GraphIntegerError, GraphIntegerEvaluationError,
    GraphIntegerExpression,
};
use fgdb_types::CanonicalScalar;
use std::collections::BTreeSet;

/// Checked output expression. Column positions refer to the input relation,
/// including when that relation is itself a projection or compound set.
#[derive(Clone, PartialEq, Eq)]
pub enum GraphSetValue {
    Column(usize),
    Literal(GqlScalarParameter),
    Integer(GraphIntegerExpression),
    List(Vec<GraphSetValue>),
    Index {
        list: Box<GraphSetValue>,
        index: Box<GraphSetValue>,
    },
    Size(Box<GraphSetValue>),
    Value(GraphValue),
}
impl GraphSetValue {
    pub(crate) fn append_canonical_bytes(&self, bytes: &mut Vec<u8>) {
        append_value_transcript(self, bytes);
    }
    #[allow(dead_code)]
    pub(super) fn append_transcript(&self, bytes: &mut Vec<u8>) {
        self.append_canonical_bytes(bytes);
    }
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
    pub(crate) fn admit_output(
        value: &GraphSetValue,
        types: &[GraphSetColumnType],
        column: usize,
    ) -> Result<GraphSetColumnType, GraphSetProjectionError> {
        value_type(value, types, column)
    }

    pub(crate) fn validate_output_name(
        name: &str,
        column: usize,
    ) -> Result<(), GraphSetProjectionError> {
        validate_name(name, column)
    }

    #[must_use]
    pub fn new(name: impl Into<String>, value: GraphSetValue) -> Self {
        Self {
            name: name.into(),
            value,
        }
    }
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
    #[must_use]
    pub fn value(&self) -> &GraphSetValue {
        &self.value
    }

    /// The same row mechanism also supplies computed GroupAggregate inputs.
    /// The owner supplies its arithmetic error vocabulary; source/control
    /// failures pass through without wrapping or losing their original type.
    pub(crate) fn evaluate_row_with_control<E>(
        row: &GraphValueRow,
        projection: &[Self],
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        arithmetic: impl FnOnce(usize, GraphIntegerError) -> E,
    ) -> Result<GraphValueRow, E> {
        evaluate(row, projection, control).map_err(|error| match error {
            ProjectionFailure::Control(error) => error,
            ProjectionFailure::Arithmetic { column, error } => arithmetic(column, error),
        })
    }
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
    ListInput { column: usize },
    ExpressionBounds { column: usize },
    InvalidValue { column: usize },
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
    /// per distinct hidden carrier. This is the bounded materialized profile,
    /// not spill-backed expression evaluation.
    pub fn project(
        self,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
    ) -> Result<Self, GraphSetProjectionError> {
        use GraphSetProjectionError as Error;
        if projection.is_empty() {
            return Err(Error::Empty);
        }
        if projection.len() > MAX_PATTERN_VERTICES {
            return Err(Error::TooManyColumns {
                limit: MAX_PATTERN_VERTICES,
                observed: projection.len(),
            });
        }
        let depth = self.depth + 1;
        check_depth(depth).map_err(Error::SetBuild)?;
        let mut names = BTreeSet::new();
        let mut types = Vec::new();
        for (column, output) in projection.iter().enumerate() {
            validate_name(&output.name, column)?;
            if !names.insert(output.name.as_str()) {
                return Err(Error::DuplicateName { column });
            }
            let mut nodes = 0;
            types.push(admit(&output.value, &self.types, column, 0, &mut nodes)?);
        }
        Ok(Self {
            columns: projection
                .iter()
                .map(|column| column.name.clone())
                .collect(),
            types,
            operands: self.operands,
            depth,
            node: SetNode::Project {
                input: Box::new(self),
                projection,
                quantifier,
            },
            order: Vec::new(),
            offset: 0,
            count: None,
        })
    }
}

pub(super) fn value_type(
    value: &GraphSetValue,
    types: &[GraphSetColumnType],
    column: usize,
) -> Result<GraphSetColumnType, GraphSetProjectionError> {
    admit(value, types, column, 0, &mut 0)
}

pub(super) fn validate_name(name: &str, column: usize) -> Result<(), GraphSetProjectionError> {
    let bytes = name.as_bytes();
    if bytes.is_empty()
        || bytes.len() > MAX_PATTERN_NAME_BYTES
        || !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        return Err(GraphSetProjectionError::InvalidName { column });
    }
    Ok(())
}

fn admit(
    value: &GraphSetValue,
    types: &[GraphSetColumnType],
    column: usize,
    depth: usize,
    nodes: &mut usize,
) -> Result<GraphSetColumnType, GraphSetProjectionError> {
    use GraphSetProjectionError as Error;
    *nodes += 1;
    if depth > GraphValue::MAX_LIST_DEPTH || *nodes > GraphValue::MAX_LIST_NODES {
        return Err(Error::ExpressionBounds { column });
    }
    Ok(match value {
        GraphSetValue::Column(input) => *types.get(*input).ok_or(Error::UnknownInput {
            column,
            input: *input,
        })?,
        GraphSetValue::Literal(_) => GraphSetColumnType::Scalar,
        GraphSetValue::Integer(expression) => {
            for input in expression.referenced_columns() {
                let kind = types
                    .get(input)
                    .ok_or(Error::UnknownInput { column, input })?;
                if !matches!(kind, GraphSetColumnType::Scalar | GraphSetColumnType::Any) {
                    return Err(Error::IntegerInput { column, input });
                }
            }
            GraphSetColumnType::Scalar
        }
        GraphSetValue::Value(value) => {
            if !value.validate_bounds() || value.canonical_bytes().is_err() {
                return Err(Error::InvalidValue { column });
            }
            match value {
                GraphValue::Scalar(_) => GraphSetColumnType::Scalar,
                GraphValue::Vertex(_) => GraphSetColumnType::Vertex,
                GraphValue::Edge(_) => GraphSetColumnType::Edge,
                GraphValue::Path(_) => GraphSetColumnType::Path,
                GraphValue::Vertices(_) => GraphSetColumnType::Vertices,
                GraphValue::Edges(_) => GraphSetColumnType::Edges,
                GraphValue::List(_) => GraphSetColumnType::List,
            }
        }
        GraphSetValue::List(values) => {
            for value in values {
                admit(value, types, column, depth + 1, nodes)?;
            }
            GraphSetColumnType::List
        }
        GraphSetValue::Index { list, index } => {
            admit_list(list, types, column, depth + 1, nodes)?;
            if !matches!(
                admit(index, types, column, depth + 1, nodes)?,
                GraphSetColumnType::Scalar | GraphSetColumnType::Any
            ) {
                return Err(Error::ListInput { column });
            }
            GraphSetColumnType::Any
        }
        // openCypher size(): a list's length or a text's character count
        // (fgdb-xakp1). A literal that is neither refuses here; any other
        // scalar is a typed failure when evaluated.
        GraphSetValue::Size(value) => {
            let literal = matches!(value.as_ref(), GraphSetValue::Literal(scalar)
                if !matches!(scalar.value(), CanonicalScalar::Null | CanonicalScalar::Text(_)));
            if literal
                || !matches!(
                    admit(value, types, column, depth + 1, nodes)?,
                    GraphSetColumnType::List | GraphSetColumnType::Any | GraphSetColumnType::Scalar
                )
            {
                return Err(Error::ListInput { column });
            }
            GraphSetColumnType::Scalar
        }
    })
}

fn admit_list(
    value: &GraphSetValue,
    types: &[GraphSetColumnType],
    column: usize,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), GraphSetProjectionError> {
    let kind = admit(value, types, column, depth, nodes)?;
    let null = matches!(value, GraphSetValue::Literal(v) if matches!(v.value(), CanonicalScalar::Null))
        || matches!(value, GraphSetValue::Value(v) if v.is_null());
    if !matches!(kind, GraphSetColumnType::List | GraphSetColumnType::Any) && !null {
        return Err(GraphSetProjectionError::ListInput { column });
    }
    Ok(())
}

pub(super) fn append_value_transcript(value: &GraphSetValue, bytes: &mut Vec<u8>) {
    match value {
        GraphSetValue::Column(input) => {
            bytes.push(0);
            bytes.extend_from_slice(&(*input as u64).to_be_bytes());
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
        GraphSetValue::List(values) => {
            bytes.push(3);
            bytes.extend_from_slice(&(values.len() as u64).to_be_bytes());
            for value in values {
                append_value_transcript(value, bytes);
            }
        }
        GraphSetValue::Index { list, index } => {
            bytes.push(4);
            append_value_transcript(list, bytes);
            append_value_transcript(index, bytes);
        }
        GraphSetValue::Size(list) => {
            bytes.push(5);
            append_value_transcript(list, bytes);
        }
        GraphSetValue::Value(value) => {
            bytes.push(6);
            let value = value
                .canonical_bytes()
                .expect("value encoding admitted during preparation");
            bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
            bytes.extend_from_slice(&value);
        }
    }
}

pub(super) fn append_transcript(projection: &[GraphSetProjection], bytes: &mut Vec<u8>) {
    bytes.extend_from_slice(&(projection.len() as u64).to_be_bytes());
    for column in projection {
        append_value_transcript(&column.value, bytes);
    }
}

pub(super) enum ProjectionFailure<E> {
    Control(E),
    Arithmetic {
        column: usize,
        error: GraphIntegerError,
    },
}

pub(super) fn copy_value<E>(
    value: &GraphValue,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<GraphValue, E> {
    value.copy_with_control(control)
}

/// The input is a private owned row whose complete schema was admitted by the
/// enclosing set executor. Every output cell and variable payload is reserved
/// before growth. A refusal drops the whole private row, never a short result.
/// Arithmetic is not elided for DISTINCT duplicates or a final LIMIT 0.
pub(super) fn evaluate<E>(
    row: &GraphValueRow,
    projection: &[GraphSetProjection],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<GraphValueRow, ProjectionFailure<E>> {
    let mut values = Vec::new();
    for (column, output) in projection.iter().enumerate() {
        let value = evaluate_value(&output.value, row, column, control)?;
        values.push(value);
    }
    Ok(GraphValueRow::from_owned_values(values))
}

pub(super) fn evaluate_value<E>(
    value: &GraphSetValue,
    row: &GraphValueRow,
    column: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<GraphValue, ProjectionFailure<E>> {
    evaluate_value_at(value, row, column, control, 0, &mut 0)
}

fn operand<'a, E>(
    value: &'a GraphSetValue,
    row: &'a GraphValueRow,
    column: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    depth: usize,
    nodes: &mut usize,
) -> Result<std::borrow::Cow<'a, GraphValue>, ProjectionFailure<E>> {
    let borrowed = match value {
        GraphSetValue::Column(input) => row.values().get(*input),
        GraphSetValue::Value(value) => Some(value),
        _ => None,
    };
    if let Some(value) = borrowed {
        control(GlaExecutionEvent::Work).map_err(ProjectionFailure::Control)?;
        Ok(std::borrow::Cow::Borrowed(value))
    } else {
        evaluate_value_at(value, row, column, control, depth, nodes).map(std::borrow::Cow::Owned)
    }
}

fn evaluate_value_at<E>(
    value: &GraphSetValue,
    row: &GraphValueRow,
    column: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    depth: usize,
    nodes: &mut usize,
) -> Result<GraphValue, ProjectionFailure<E>> {
    use crate::GraphIntegerErrorKind;
    let failure = |kind| ProjectionFailure::Arithmetic {
        column,
        error: GraphIntegerError {
            instruction: 0,
            kind,
        },
    };
    control(GlaExecutionEvent::Work).map_err(ProjectionFailure::Control)?;
    *nodes += 1;
    if depth > GraphValue::MAX_LIST_DEPTH || *nodes > GraphValue::MAX_LIST_NODES {
        return Err(failure(GraphIntegerErrorKind::Overflow));
    }
    let result = match value {
        GraphSetValue::Column(input) => {
            let value = row
                .values()
                .get(*input)
                .ok_or_else(|| failure(GraphIntegerErrorKind::MissingColumn))?;
            copy_value(value, control).map_err(ProjectionFailure::Control)?
        }
        GraphSetValue::Literal(value) => {
            // Borrow the checked scalar while reserving its eventual copy.
            // The temporary GraphValue owns a clone, so use the shared row
            // copier only after the payload reservations below instead.
            let scalar = value.value();
            let sizes = match scalar {
                CanonicalScalar::Text(value) => [
                    value.len(),
                    value.canonical_sort_key().map_or(0, <[u8]>::len),
                ],
                CanonicalScalar::Bytes(value) => [value.as_slice().len(), 0],
                CanonicalScalar::Timestamp(value) => {
                    [value.zone().map_or(0, |zone| zone.identifier().len()), 0]
                }
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
            let value = expression
                .evaluate_scalar_with_control(row.values(), control)
                .map_err(|error| match error {
                    GraphIntegerEvaluationError::Control(error) => {
                        ProjectionFailure::Control(error)
                    }
                    GraphIntegerEvaluationError::Value(error) => {
                        ProjectionFailure::Arithmetic { column, error }
                    }
                })?;
            control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
            GraphValue::Scalar(value)
        }
        GraphSetValue::Value(value) => {
            copy_value(value, control).map_err(ProjectionFailure::Control)?
        }
        GraphSetValue::List(expressions) => {
            control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
            let mut values = Vec::new();
            for expression in expressions {
                control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                values.push(evaluate_value_at(
                    expression,
                    row,
                    column,
                    control,
                    depth + 1,
                    nodes,
                )?);
            }
            GraphValue::List(values.into_boxed_slice())
        }
        GraphSetValue::Index { list, index } => {
            let list = operand(list, row, column, control, depth + 1, nodes)?;
            let index = evaluate_value_at(index, row, column, control, depth + 1, nodes)?;
            if list.is_null() || index.is_null() {
                control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                GraphValue::Scalar(CanonicalScalar::Null)
            } else {
                let values = list
                    .as_list()
                    .ok_or_else(|| failure(GraphIntegerErrorKind::IncompatibleOperands))?;
                let GraphValue::Scalar(CanonicalScalar::Int(index)) = index else {
                    return Err(failure(GraphIntegerErrorKind::NonInteger));
                };
                let at = if index < 0 {
                    (values.len() as i128) + i128::from(index)
                } else {
                    i128::from(index)
                };
                match usize::try_from(at).ok().and_then(|at| values.get(at)) {
                    Some(value) => {
                        copy_value(value, control).map_err(ProjectionFailure::Control)?
                    }
                    None => {
                        control(GlaExecutionEvent::ScratchEntry)
                            .map_err(ProjectionFailure::Control)?;
                        GraphValue::Scalar(CanonicalScalar::Null)
                    }
                }
            }
        }
        GraphSetValue::Size(list) => {
            let list = operand(list, row, column, control, depth + 1, nodes)?;
            let size = if list.is_null() {
                None
            } else if let Some(values) = list.as_list() {
                Some(values.len())
            } else if let GraphValue::Scalar(CanonicalScalar::Text(text)) = list.as_ref() {
                // CHAR_LENGTH's charge: one unit of work per payload unit read.
                let text = text.as_str();
                for _ in 0..text.len().div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
                    control(GlaExecutionEvent::Work).map_err(ProjectionFailure::Control)?;
                }
                Some(text.chars().count())
            } else {
                return Err(failure(GraphIntegerErrorKind::IncompatibleOperands));
            };
            let scalar = match size {
                None => CanonicalScalar::Null,
                Some(size) => CanonicalScalar::Int(
                    i64::try_from(size).map_err(|_| failure(GraphIntegerErrorKind::Overflow))?,
                ),
            };
            control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
            GraphValue::Scalar(scalar)
        }
    };
    Ok(result)
}
