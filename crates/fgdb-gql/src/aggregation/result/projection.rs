//! Borrowed post-group expressions and governed projected ranking.
mod incremental;

use super::*;
use crate::integer_expression::ExpressionCell;
use crate::{GraphIntegerError, GraphIntegerErrorKind, GraphIntegerEvaluationError, GraphSetValue};
use std::borrow::Cow;

type QueryError<E, C> = GqlQueryError<GraphAggregateError<E>, C>;

enum OutputValue<'a> {
    Borrowed(Cell<'a>),
    Owned(GraphAggregateValue),
}

/// Group columns keep their exact aggregate domains. Only local element and
/// accumulator cells are owned; entering a list scope never copies the group
/// or narrows its count, wide integer sum or exact average into a graph scalar.
struct ExpressionInput<'s, 'r, F, E> {
    columns: F,
    width: usize,
    locals: Vec<GraphAggregateValue>,
    read: &'s mut crate::set_ops::ElementRead<'r, E>,
}

fn result_cell(value: &GraphAggregateValue) -> Cell<'_> {
    match value {
        GraphAggregateValue::Count(value) => Cell::Count(*value),
        GraphAggregateValue::Integer(value) => Cell::Integer(*value),
        GraphAggregateValue::Average(value) => Cell::Average {
            sum: value.numerator(),
            count: value.denominator(),
        },
        GraphAggregateValue::Value(value) => Cell::Value(value_ref(value)),
    }
}

fn value_ref(value: &GraphValue) -> ValueRef<'_> {
    match value {
        GraphValue::Scalar(value) => ValueRef::Scalar(value),
        GraphValue::Vertex(value) => ValueRef::Vertex(*value),
        GraphValue::Edge(value) => ValueRef::Edge(*value),
        GraphValue::Path(value) => ValueRef::Path(value),
        GraphValue::Vertices(value) => ValueRef::Vertices(value),
        GraphValue::Edges(value) => ValueRef::Edges(value),
        GraphValue::List(value) => ValueRef::List(value),
        GraphValue::Map { keys, values } => ValueRef::Map { keys, values },
    }
}

impl<'a> OutputValue<'a> {
    fn cell(&self) -> Cell<'_> {
        match self {
            Self::Borrowed(value) => *value,
            Self::Owned(GraphAggregateValue::Count(value)) => Cell::Count(*value),
            Self::Owned(GraphAggregateValue::Integer(value)) => Cell::Integer(*value),
            Self::Owned(GraphAggregateValue::Average(value)) => Cell::Average {
                sum: value.numerator(),
                count: value.denominator(),
            },
            Self::Owned(GraphAggregateValue::Value(value)) => Cell::Value(value_ref(value)),
        }
    }

    fn into_owned<E>(
        self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<GraphAggregateValue, E> {
        match self {
            Self::Owned(value) => Ok(value),
            Self::Borrowed(value) => {
                control(GlaExecutionEvent::ScratchEntry)?;
                Ok(match value {
                    Cell::Count(value) => GraphAggregateValue::Count(value),
                    Cell::Integer(value) => GraphAggregateValue::Integer(value),
                    Cell::Average { sum, count } => GraphAggregateValue::Average(
                        GraphExactAverage::new(sum, count).expect("nonnull average"),
                    ),
                    Cell::Float(value) => GraphAggregateValue::Value(GraphValue::Scalar(
                        CanonicalScalar::Float(value),
                    )),
                    Cell::Value(value) => GraphAggregateValue::Value(value.copy_owned(control)?),
                })
            }
        }
    }
}

fn failure<E, C>(column: usize, kind: GraphIntegerErrorKind) -> QueryError<E, C> {
    GqlQueryError::Source(GraphAggregateError::OutputExpression {
        column,
        error: GraphIntegerError {
            instruction: 0,
            kind,
        },
    })
}

fn load_cell(cell: Cell<'_>) -> Result<ExpressionCell<'_>, GraphIntegerErrorKind> {
    Ok(match cell {
        Cell::Count(value) => ExpressionCell::Count(value),
        Cell::Integer(value) => ExpressionCell::Integer(value),
        Cell::Average { sum, count } => {
            ExpressionCell::Average(GraphExactAverage::new(sum, count).expect("nonnull average"))
        }
        Cell::Float(value) => ExpressionCell::Scalar(Cow::Owned(CanonicalScalar::Float(value))),
        Cell::Value(ValueRef::Scalar(value)) => ExpressionCell::Scalar(Cow::Borrowed(value)),
        Cell::Value(_) => return Err(GraphIntegerErrorKind::NonScalar),
    })
}

/// The identity of a grouped vertex or edge for id()/elementId() (fgdb-j687q).
fn load_element(cell: Cell<'_>) -> Result<Option<u128>, GraphIntegerErrorKind> {
    match cell {
        Cell::Value(ValueRef::Vertex(vertex)) => Ok(Some(vertex.0)),
        Cell::Value(ValueRef::Edge(edge)) => Ok(Some(edge.0)),
        cell if cell.is_null() => Ok(None),
        _ => Err(GraphIntegerErrorKind::IncompatibleOperands),
    }
}

fn input_cell<'g, 'a: 'g>(
    group: Group<'g, 'a>,
    input: usize,
) -> Result<Cell<'g>, GraphIntegerErrorKind> {
    if input < group.key.len() {
        Ok(Cell::Value(group.key[input]))
    } else {
        group
            .state
            .get(input - group.key.len())
            .map(Cell::from_state)
            .ok_or(GraphIntegerErrorKind::MissingColumn)
    }
}

fn element_truth<E, C>(cell: Cell<'_>, column: usize) -> Result<Option<bool>, QueryError<E, C>> {
    match cell {
        cell if cell.is_null() => Ok(None),
        Cell::Value(ValueRef::Scalar(CanonicalScalar::Bool(value))) => Ok(Some(*value)),
        _ => Err(failure(column, GraphIntegerErrorKind::NonBoolean)),
    }
}

fn expression<'g, E, C>(
    value: &'g GraphSetValue,
    input: &mut ExpressionInput<
        '_,
        '_,
        impl Fn(usize) -> Result<Cell<'g>, GraphIntegerErrorKind>,
        E,
    >,
    column: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), QueryError<E, C>>,
) -> Result<OutputValue<'g>, QueryError<E, C>> {
    // The same checked expression program reads batch or maintained cells.
    // Preparation bounds the recursive expression depth and number of nodes.
    control(GlaExecutionEvent::Work)?;
    Ok(match value {
        GraphSetValue::Column(at) => {
            OutputValue::Borrowed((input.columns)(*at).map_err(|kind| failure(column, kind))?)
        }
        GraphSetValue::Literal(value) => {
            OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(value.value())))
        }
        GraphSetValue::TypedNull(_) => {
            control(GlaExecutionEvent::ScratchEntry)?;
            OutputValue::Owned(GraphAggregateValue::Value(GraphValue::Scalar(
                CanonicalScalar::Null,
            )))
        }
        GraphSetValue::Value(value) => OutputValue::Borrowed(Cell::Value(value_ref(value))),
        GraphSetValue::Integer(expression) => {
            if !input.locals.is_empty() {
                // The scalar VM addresses locals after the frozen group row.
                // Copy a borrowed scalar result before leaving this scope;
                // exact aggregate numbers remain in their original domains.
                let load = |at| {
                    if at < input.width {
                        (input.columns)(at)
                    } else {
                        input
                            .locals
                            .get(at - input.width)
                            .map(result_cell)
                            .ok_or(GraphIntegerErrorKind::MissingColumn)
                    }
                };
                let value = expression
                    .evaluate_loaded_with_control(
                        input.width + input.locals.len(),
                        |at| load(at).and_then(load_cell),
                        |at| load(at).and_then(load_element),
                        control,
                    )
                    .map_err(|error| match error {
                        GraphIntegerEvaluationError::Control(error) => error,
                        GraphIntegerEvaluationError::Value(error) => {
                            GqlQueryError::Source(GraphAggregateError::OutputExpression {
                                column,
                                error,
                            })
                        }
                    })?;
                let value = match value {
                    ExpressionCell::Scalar(Cow::Borrowed(value)) => {
                        OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(value)))
                            .into_owned(control)?
                    }
                    ExpressionCell::Scalar(Cow::Owned(value)) => {
                        control(GlaExecutionEvent::ScratchEntry)?;
                        GraphAggregateValue::Value(GraphValue::Scalar(value))
                    }
                    ExpressionCell::Count(value) => GraphAggregateValue::Count(value),
                    ExpressionCell::Integer(value) => GraphAggregateValue::Integer(value),
                    ExpressionCell::Average(value) => GraphAggregateValue::Average(value),
                };
                return Ok(OutputValue::Owned(value));
            }
            let value = expression
                // Outside an element scope, Local cannot alias a group column.
                .evaluate_loaded_with_control(
                    0,
                    |at| (input.columns)(at).and_then(load_cell),
                    |at| (input.columns)(at).and_then(load_element),
                    control,
                )
                .map_err(|error| match error {
                    GraphIntegerEvaluationError::Control(error) => error,
                    GraphIntegerEvaluationError::Value(error) => {
                        GqlQueryError::Source(GraphAggregateError::OutputExpression {
                            column,
                            error,
                        })
                    }
                })?;
            match value {
                ExpressionCell::Scalar(Cow::Borrowed(value)) => {
                    OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(value)))
                }
                ExpressionCell::Scalar(Cow::Owned(value)) => {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    OutputValue::Owned(GraphAggregateValue::Value(GraphValue::Scalar(value)))
                }
                ExpressionCell::Count(value) => OutputValue::Borrowed(Cell::Count(value)),
                ExpressionCell::Integer(value) => OutputValue::Borrowed(Cell::Integer(value)),
                ExpressionCell::Average(value) => OutputValue::Borrowed(Cell::Average {
                    sum: value.numerator(),
                    count: value.denominator(),
                }),
            }
        }
        GraphSetValue::List(values) => {
            control(GlaExecutionEvent::ScratchEntry)?;
            let mut list = Vec::new();
            for value in values {
                let value = expression(value, input, column, control)?.into_owned(control)?;
                let value = graph_value(value, column)?;
                control(GlaExecutionEvent::ScratchEntry)?;
                list.push(value);
            }
            let value = GraphValue::List(list.into_boxed_slice());
            if !value.validate_bounds() {
                return Err(failure(column, GraphIntegerErrorKind::Overflow));
            }
            OutputValue::Owned(GraphAggregateValue::Value(value))
        }
        GraphSetValue::Size(list) => {
            let list = expression(list, input, column, control)?;
            let value = match list.cell() {
                cell if cell.is_null() => CanonicalScalar::Null,
                Cell::Value(ValueRef::List(values)) => CanonicalScalar::Int(
                    i64::try_from(values.len())
                        .map_err(|_| failure(column, GraphIntegerErrorKind::Overflow))?,
                ),
                // openCypher size() of text is its character count
                // (fgdb-xakp1), charged as CHAR_LENGTH charges it.
                Cell::Value(ValueRef::Scalar(CanonicalScalar::Text(text))) => {
                    let text = text.as_str();
                    for _ in 0..text
                        .len()
                        .div_ceil(crate::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
                    {
                        control(GlaExecutionEvent::Work)?;
                    }
                    CanonicalScalar::Int(
                        i64::try_from(text.chars().count())
                            .map_err(|_| failure(column, GraphIntegerErrorKind::Overflow))?,
                    )
                }
                _ => return Err(failure(column, GraphIntegerErrorKind::IncompatibleOperands)),
            };
            control(GlaExecutionEvent::ScratchEntry)?;
            OutputValue::Owned(GraphAggregateValue::Value(GraphValue::Scalar(value)))
        }
        // Three-valued membership with the row evaluator's law (fgdb-gql
        // set_ops): both operands execute once, a NULL list is UNKNOWN, and
        // any other non-list is an expression error.
        GraphSetValue::In { value, list } => {
            let value = expression(value, input, column, control)?.into_owned(control)?;
            let value = graph_value(value, column)?;
            let list = expression(list, input, column, control)?;
            let truth = match list.cell() {
                cell if cell.is_null() => None,
                Cell::Value(ValueRef::List(members)) => {
                    crate::set_ops::evaluate_membership(&value, members, control)?
                }
                _ => return Err(failure(column, GraphIntegerErrorKind::IncompatibleOperands)),
            };
            control(GlaExecutionEvent::ScratchEntry)?;
            OutputValue::Owned(GraphAggregateValue::Value(GraphValue::Scalar(
                truth.map_or(CanonicalScalar::Null, CanonicalScalar::Bool),
            )))
        }
        // Temporal constructors with the row evaluator's law (fgdb-txbg9).
        GraphSetValue::Temporal {
            function,
            argument,
            zones,
        } => {
            let argument = expression(argument, input, column, control)?.into_owned(control)?;
            let argument = graph_value(argument, column)?;
            let value = crate::set_ops::construct_temporal(*function, &argument, zones.as_ref())
                .map_err(|kind| failure(column, kind))?;
            control(GlaExecutionEvent::ScratchEntry)?;
            OutputValue::Owned(GraphAggregateValue::Value(GraphValue::Scalar(value)))
        }
        // A composite CASE with the row evaluator's law (fgdb-13l85): the
        // first TRUE condition selects, and no later arm executes.
        GraphSetValue::Case { arms, otherwise } => {
            let mut selected = otherwise.as_deref();
            for (condition, value) in arms {
                let truth = match expression(condition, input, column, control)?.cell() {
                    cell if cell.is_null() => false,
                    Cell::Value(ValueRef::Scalar(CanonicalScalar::Bool(truth))) => *truth,
                    _ => return Err(failure(column, GraphIntegerErrorKind::NonBoolean)),
                };
                if truth {
                    selected = Some(value);
                    break;
                }
            }
            match selected {
                Some(value) => expression(value, input, column, control)?,
                None => {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    OutputValue::Owned(GraphAggregateValue::Value(GraphValue::Scalar(
                        CanonicalScalar::Null,
                    )))
                }
            }
        }
        // A composite coalesce, lazily as the row evaluator reads it.
        GraphSetValue::Function {
            function: crate::GraphValueFunction::Coalesce,
            argument,
        } => {
            let arguments = match argument.as_ref() {
                GraphSetValue::List(arguments) => arguments.as_slice(),
                single => core::slice::from_ref(single),
            };
            let mut first = None;
            for argument in arguments {
                let value = expression(argument, input, column, control)?.into_owned(control)?;
                let value = graph_value(value, column)?;
                if !value.is_null() {
                    first = Some(value);
                    break;
                }
            }
            if first.is_none() {
                control(GlaExecutionEvent::ScratchEntry)?;
            }
            OutputValue::Owned(GraphAggregateValue::Value(
                first.unwrap_or(GraphValue::Scalar(CanonicalScalar::Null)),
            ))
        }
        // reverse() and split() with the row evaluator's law (fgdb-13l85).
        GraphSetValue::Function { function, argument } => {
            let argument = expression(argument, input, column, control)?.into_owned(control)?;
            let argument = graph_value(argument, column)?;
            let value = crate::set_ops::value_function(*function, &argument, column, control)
                .map_err(|error| match error {
                    crate::set_ops::ProjectionFailure::Control(error) => error,
                    crate::set_ops::ProjectionFailure::Arithmetic { column, error } => {
                        GqlQueryError::Source(GraphAggregateError::OutputExpression {
                            column,
                            error,
                        })
                    }
                })?;
            OutputValue::Owned(GraphAggregateValue::Value(value))
        }
        // Three-valued equality with the row evaluator's law (fgdb-9cawv).
        GraphSetValue::Equal { left, right } => {
            let left = expression(left, input, column, control)?.into_owned(control)?;
            let left = graph_value(left, column)?;
            let right = expression(right, input, column, control)?.into_owned(control)?;
            let right = graph_value(right, column)?;
            let truth = crate::set_ops::evaluate_equality(&left, &right, control)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            OutputValue::Owned(GraphAggregateValue::Value(GraphValue::Scalar(
                truth.map_or(CanonicalScalar::Null, CanonicalScalar::Bool),
            )))
        }
        // A list ordering with the row evaluator's law.
        GraphSetValue::Ordered {
            comparison,
            left,
            right,
        } => {
            let left = expression(left, input, column, control)?.into_owned(control)?;
            let left = graph_value(left, column)?;
            let right = expression(right, input, column, control)?.into_owned(control)?;
            let right = graph_value(right, column)?;
            let truth = crate::set_ops::evaluate_order(*comparison, &left, &right, control)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            OutputValue::Owned(GraphAggregateValue::Value(GraphValue::Scalar(
                truth.map_or(CanonicalScalar::Null, CanonicalScalar::Bool),
            )))
        }
        // An element entry inside a scalar expression over an aggregate
        // (`[x IN COLLECT(n) | x.p * 10]`) reads x.p through a let slot
        // (fgdb-694ga). As in the row evaluator, every value reads the
        // enclosing scope; the body then reads them as the innermost locals,
        // the last value nearest.
        GraphSetValue::Let { values, body } => {
            let depth = input.locals.len();
            let mut computed = Vec::with_capacity(values.len());
            for value in values {
                computed.push(expression(value, input, column, control)?.into_owned(control)?);
            }
            // Charge every slot before pushing any, so a refused charge
            // leaves the outer scope as it was.
            for _ in &computed {
                control(GlaExecutionEvent::ScratchEntry)?;
            }
            input.locals.extend(computed);
            let value = expression(body, input, column, control);
            // Restore the outer scope before propagating any failure.
            input.locals.truncate(depth);
            value?
        }
        // Aggregate outputs number no rows, so rand() has no key here: a
        // hand-built plan with it is a typed failure.
        GraphSetValue::Random { .. } => {
            return Err(failure(column, GraphIntegerErrorKind::IncompatibleOperands));
        }
        GraphSetValue::Local(offset) => {
            let cell = input
                .locals
                .len()
                .checked_sub(offset + 1)
                .and_then(|at| input.locals.get(at))
                .map(result_cell)
                .ok_or_else(|| failure(column, GraphIntegerErrorKind::MissingColumn))?;
            OutputValue::Owned(OutputValue::Borrowed(cell).into_owned(control)?)
        }
        GraphSetValue::Comprehension { list, filter, map } => {
            let list = expression(list, input, column, control)?;
            if list.cell().is_null() {
                return Ok(OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(&NULL))));
            }
            let Cell::Value(ValueRef::List(members)) = list.cell() else {
                return Err(failure(column, GraphIntegerErrorKind::IncompatibleOperands));
            };
            control(GlaExecutionEvent::ScratchEntry)?;
            let mut kept = Vec::new();
            let depth = input.locals.len();
            for member in members {
                control(GlaExecutionEvent::Work)?;
                control(GlaExecutionEvent::ScratchEntry)?;
                input.locals.push(GraphAggregateValue::Value(
                    member.copy_with_control(control)?,
                ));
                let projected = (|| -> Result<Option<GraphValue>, QueryError<E, C>> {
                    if let Some(filter) = filter {
                        let predicate = expression(filter, input, column, control)?;
                        if !element_truth(predicate.cell(), column)?.unwrap_or(false) {
                            return Ok(None);
                        }
                    }
                    let value = match map {
                        Some(map) => {
                            let value =
                                expression(map, input, column, control)?.into_owned(control)?;
                            graph_value(value, column)?
                        }
                        None => member.copy_with_control(control)?,
                    };
                    Ok(Some(value))
                })();
                // Restore the outer scope before propagating any failure.
                input.locals.truncate(depth);
                if let Some(value) = projected? {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    kept.push(value);
                }
            }
            let value = GraphValue::List(kept.into_boxed_slice());
            if !value.validate_bounds() {
                return Err(failure(column, GraphIntegerErrorKind::Overflow));
            }
            OutputValue::Owned(GraphAggregateValue::Value(value))
        }
        GraphSetValue::Quantifier {
            kind,
            list,
            predicate,
        } => {
            let list = expression(list, input, column, control)?;
            if list.cell().is_null() {
                return Ok(OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(&NULL))));
            }
            let Cell::Value(ValueRef::List(members)) = list.cell() else {
                return Err(failure(column, GraphIntegerErrorKind::IncompatibleOperands));
            };
            control(GlaExecutionEvent::ScratchEntry)?;
            let mut results = Vec::new();
            let depth = input.locals.len();
            for member in members {
                control(GlaExecutionEvent::Work)?;
                control(GlaExecutionEvent::ScratchEntry)?;
                input.locals.push(GraphAggregateValue::Value(
                    member.copy_with_control(control)?,
                ));
                let predicate = expression(predicate, input, column, control)
                    .and_then(|value| element_truth(value.cell(), column));
                input.locals.truncate(depth);
                let predicate = predicate?;
                control(GlaExecutionEvent::ScratchEntry)?;
                results.push(predicate);
            }
            OutputValue::Owned(GraphAggregateValue::Value(GraphValue::Scalar(
                kind.combine(results)
                    .map_or(CanonicalScalar::Null, CanonicalScalar::Bool),
            )))
        }
        GraphSetValue::Reduce { init, list, expr } => {
            let mut accumulator = expression(init, input, column, control)?.into_owned(control)?;
            let list = expression(list, input, column, control)?;
            if list.cell().is_null() {
                return Ok(OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(&NULL))));
            }
            let Cell::Value(ValueRef::List(members)) = list.cell() else {
                return Err(failure(column, GraphIntegerErrorKind::IncompatibleOperands));
            };
            let depth = input.locals.len();
            for member in members {
                control(GlaExecutionEvent::Work)?;
                let member = member.copy_with_control(control)?;
                control(GlaExecutionEvent::ScratchEntry)?;
                control(GlaExecutionEvent::ScratchEntry)?;
                input.locals.push(accumulator);
                input.locals.push(GraphAggregateValue::Value(member));
                let reduced = expression(expr, input, column, control)
                    .and_then(|value| value.into_owned(control));
                input.locals.truncate(depth);
                accumulator = reduced?;
            }
            OutputValue::Owned(accumulator)
        }
        // Map values over aggregate cells (fgdb-2jw3z), with the row
        // evaluator's laws: keys sorted at preparation, NULL for a NULL map or
        // an absent key, and a typed error for a non-map.
        GraphSetValue::MapLiteral {
            keys,
            values,
            guard,
        } => {
            if let Some(guard) = guard
                && expression(guard, input, column, control)?.cell().is_null()
            {
                return Ok(OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(&NULL))));
            }
            control(GlaExecutionEvent::ScratchEntry)?;
            let mut entries = Vec::new();
            for value in values {
                let value = expression(value, input, column, control)?.into_owned(control)?;
                control(GlaExecutionEvent::ScratchEntry)?;
                entries.push(graph_value(value, column)?);
            }
            for key in keys {
                for _ in 0..=key
                    .len()
                    .div_ceil(crate::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
                {
                    control(GlaExecutionEvent::ScratchEntry)?;
                }
            }
            let value = GraphValue::Map {
                keys: keys.clone(),
                values: entries.into_boxed_slice(),
            };
            if !value.validate_bounds() {
                return Err(failure(column, GraphIntegerErrorKind::Overflow));
            }
            OutputValue::Owned(GraphAggregateValue::Value(value))
        }
        GraphSetValue::MapOverlay { base, keys, values } => {
            let base = expression(base, input, column, control)?;
            match base.cell() {
                cell if cell.is_null() => {
                    return Ok(OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(&NULL))));
                }
                Cell::Value(ValueRef::Map { .. }) => {}
                _ => return Err(failure(column, GraphIntegerErrorKind::NonMap)),
            }
            let base = graph_value(base.into_owned(control)?, column)?;
            let mut entries = Vec::new();
            for value in values {
                let value = expression(value, input, column, control)?.into_owned(control)?;
                control(GlaExecutionEvent::ScratchEntry)?;
                entries.push(graph_value(value, column)?);
            }
            let value = crate::set_ops::overlay_map(&base, keys, entries, column, control)
                .map_err(|error| match error {
                    crate::set_ops::ProjectionFailure::Control(error) => error,
                    crate::set_ops::ProjectionFailure::Arithmetic { column, error } => {
                        GqlQueryError::Source(GraphAggregateError::OutputExpression {
                            column,
                            error,
                        })
                    }
                })?;
            OutputValue::Owned(GraphAggregateValue::Value(value))
        }
        GraphSetValue::MapGet { map, key, property } => {
            let map = expression(map, input, column, control)?;
            match map.cell() {
                cell if cell.is_null() => {
                    OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(&NULL)))
                }
                Cell::Value(element @ (ValueRef::Vertex(_) | ValueRef::Edge(_))) => {
                    let element = match element {
                        ValueRef::Vertex(vertex) => ElementId::Vertex(vertex),
                        ValueRef::Edge(edge) => ElementId::Edge(edge),
                        _ => unreachable!("element arm"),
                    };
                    let value = match property {
                        crate::GraphElementKey::Unbound => None,
                        crate::GraphElementKey::Missing => Some(Ok(None)),
                        crate::GraphElementKey::Property(key) => {
                            control(GlaExecutionEvent::Work)?;
                            (input.read)(element, *key)
                        }
                    }
                    .ok_or_else(|| failure(column, GraphIntegerErrorKind::IncompatibleOperands))?
                    .map_err(|error| GqlQueryError::Source(GraphAggregateError::Source(error)))?;
                    control(GlaExecutionEvent::ScratchEntry)?;
                    match value {
                        Some(fgdb_types::CanonicalPropertyValue::Scalar(value)) => {
                            for _ in 0..ValueRef::Scalar(&value).payload_units() {
                                control(GlaExecutionEvent::ScratchEntry)?;
                            }
                            // The pinned reader transfers this scalar's ownership;
                            // keeping it does not clone a whole element or group.
                            OutputValue::Owned(GraphAggregateValue::Value(GraphValue::Scalar(
                                value,
                            )))
                        }
                        // A stored List or Map (fgdb-k3nub) is admitted node by
                        // node and returned as itself, never as NULL.
                        Some(stored) => OutputValue::Owned(GraphAggregateValue::Value(
                            GraphValue::admit_property_value(&stored, control)?,
                        )),
                        None => OutputValue::Owned(GraphAggregateValue::Value(GraphValue::Scalar(
                            CanonicalScalar::Null,
                        ))),
                    }
                }
                Cell::Value(ValueRef::Map { keys, values }) => {
                    match keys.binary_search_by(|probe| probe.as_bytes().cmp(key.as_bytes())) {
                        Ok(at) => OutputValue::Owned(GraphAggregateValue::Value(
                            values[at].copy_with_control(control)?,
                        )),
                        Err(_) => OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(&NULL))),
                    }
                }
                // A temporal value's fields (fgdb-txbg9), as in row projections.
                Cell::Value(ValueRef::Scalar(scalar)) => {
                    let field = crate::set_ops::temporal_field(scalar, key)
                        .ok_or_else(|| {
                            failure(column, GraphIntegerErrorKind::IncompatibleOperands)
                        })?
                        .map_err(|kind| failure(column, kind))?;
                    for _ in 0..crate::set_ops::temporal_field_scratch_entries(&field) {
                        control(GlaExecutionEvent::ScratchEntry)?;
                    }
                    OutputValue::Owned(GraphAggregateValue::Value(GraphValue::Scalar(field)))
                }
                _ => return Err(failure(column, GraphIntegerErrorKind::IncompatibleOperands)),
            }
        }
        GraphSetValue::Keys(map) => {
            let map = expression(map, input, column, control)?;
            match map.cell() {
                cell if cell.is_null() => {
                    OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(&NULL)))
                }
                Cell::Value(ValueRef::Map { keys, .. }) => {
                    let texts = crate::set_ops::map_keys(keys, control).map_err(|error| {
                        error.unwrap_or_else(|| {
                            failure(column, GraphIntegerErrorKind::TextConstruction)
                        })
                    })?;
                    OutputValue::Owned(GraphAggregateValue::Value(GraphValue::List(texts)))
                }
                _ => return Err(failure(column, GraphIntegerErrorKind::IncompatibleOperands)),
            }
        }
        // The row evaluator's slice and range laws (set_ops slice_bounds and
        // range_values), over the aggregate cells.
        GraphSetValue::Slice { list, from, to } => {
            let list = expression(list, input, column, control)?;
            let mut bounds = [None, None];
            let mut null = list.cell().is_null();
            for (slot, bound) in bounds.iter_mut().zip([from, to]) {
                if let Some(bound) = bound {
                    let bound = expression(bound, input, column, control)?;
                    let cell = bound.cell();
                    if cell.is_null() {
                        null = true;
                    } else {
                        *slot =
                            Some(cell.integer().ok_or_else(|| {
                                failure(column, GraphIntegerErrorKind::NonInteger)
                            })?);
                    }
                }
            }
            if null {
                OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(&NULL)))
            } else {
                let Cell::Value(ValueRef::List(values)) = list.cell() else {
                    return Err(failure(column, GraphIntegerErrorKind::IncompatibleOperands));
                };
                let mut kept = Vec::new();
                for value in
                    &values[crate::set_ops::slice_bounds(values.len(), bounds[0], bounds[1])]
                {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    kept.push(value.copy_with_control(control)?);
                }
                OutputValue::Owned(GraphAggregateValue::Value(GraphValue::List(
                    kept.into_boxed_slice(),
                )))
            }
        }
        GraphSetValue::Range { start, end, step } => {
            let mut parts = [None, None, Some(1_i128)];
            let mut null = false;
            for (slot, part) in parts
                .iter_mut()
                .zip([Some(start), Some(end), step.as_ref()])
            {
                let Some(part) = part else {
                    continue;
                };
                let part = expression(part, input, column, control)?;
                let cell = part.cell();
                if cell.is_null() {
                    null = true;
                } else {
                    *slot = Some(
                        cell.integer()
                            .ok_or_else(|| failure(column, GraphIntegerErrorKind::NonInteger))?,
                    );
                }
            }
            let narrow = |value: Option<i128>| {
                value
                    .map(i64::try_from)
                    .transpose()
                    .map_err(|_| failure(column, GraphIntegerErrorKind::Overflow))
            };
            match (
                null,
                narrow(parts[0])?,
                narrow(parts[1])?,
                narrow(parts[2])?,
            ) {
                (false, Some(start), Some(end), Some(step)) => {
                    let members = crate::set_ops::range_values(start, end, step, control).map_err(
                        |error| match error {
                            GraphIntegerEvaluationError::Control(error) => error,
                            GraphIntegerEvaluationError::Value(error) => {
                                failure(column, error.kind)
                            }
                        },
                    )?;
                    OutputValue::Owned(GraphAggregateValue::Value(GraphValue::List(
                        members.into_boxed_slice(),
                    )))
                }
                _ => OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(&NULL))),
            }
        }
        GraphSetValue::Index { list, index } => {
            let list = expression(list, input, column, control)?;
            let index = expression(index, input, column, control)?;
            let list_cell = list.cell();
            let index = index.cell();
            if list_cell.is_null() || index.is_null() {
                OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(&NULL)))
            } else if let Cell::Value(ValueRef::Map { keys, values }) = list_cell {
                // Dynamic map access, as in the row evaluator.
                let Cell::Value(ValueRef::Scalar(CanonicalScalar::Text(key))) = index else {
                    return Err(failure(column, GraphIntegerErrorKind::IncompatibleOperands));
                };
                let key = key.as_str().as_bytes();
                match keys.binary_search_by(|probe| probe.as_bytes().cmp(key)) {
                    Ok(at) => OutputValue::Owned(GraphAggregateValue::Value(
                        values[at].copy_with_control(control)?,
                    )),
                    Err(_) => OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(&NULL))),
                }
            } else {
                let Cell::Value(ValueRef::List(values)) = list_cell else {
                    return Err(failure(column, GraphIntegerErrorKind::IncompatibleOperands));
                };
                let index = index
                    .integer()
                    .ok_or_else(|| failure(column, GraphIntegerErrorKind::NonInteger))?;
                let at = if index < 0 {
                    (values.len() as i128).checked_add(index)
                } else {
                    Some(index)
                };
                match at
                    .and_then(|at| usize::try_from(at).ok())
                    .and_then(|at| values.get(at))
                {
                    Some(value) => OutputValue::Owned(GraphAggregateValue::Value(
                        value.copy_with_control(control)?,
                    )),
                    None => OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(&NULL))),
                }
            }
        }
    })
}

/// An aggregate output as an ordinary graph value. Preserve exactness or
/// fail; never truncate an aggregate into an i64.
fn graph_value<E, C>(
    value: GraphAggregateValue,
    column: usize,
) -> Result<GraphValue, QueryError<E, C>> {
    let int = |value: i128| {
        i64::try_from(value)
            .map(|value| GraphValue::Scalar(CanonicalScalar::Int(value)))
            .map_err(|_| failure(column, GraphIntegerErrorKind::Overflow))
    };
    match value {
        GraphAggregateValue::Value(value) => Ok(value),
        GraphAggregateValue::Count(value) => int(i128::from(value)),
        GraphAggregateValue::Integer(value) => int(value),
        GraphAggregateValue::Average(_) => {
            Err(failure(column, GraphIntegerErrorKind::IncompatibleOperands))
        }
    }
}

struct ProjectedGroup<'g, 'a> {
    group: Group<'g, 'a>,
    values: Vec<OutputValue<'g>>,
}

impl PreparedGraphAggregate {
    /// Construct an untransformed COUNT/SUM/AVG result from a delta maintainer.
    /// Keys use the evaluation grouping order and retain their typed domains.
    /// This validates shape and result domains, not the maintained arithmetic.
    /// Callers admit their owned payloads before construction; no copy occurs.
    pub fn incremental_row(
        &self,
        keys: Vec<GraphValue>,
        values: Vec<GraphAggregateValue>,
    ) -> Option<GraphAggregateRow> {
        if !self.supports_incremental_maintenance()
            || keys.len() != self.keys.len()
            || values.len() != self.aggregates.len()
        {
            return None;
        }
        for (key, column) in keys.iter().zip(&self.keys) {
            let valid = match self.input.value_columns().get(*column)? {
                crate::algebra::ValueProjection::Property { .. } => {
                    matches!(key, GraphValue::Scalar(_))
                }
                crate::algebra::ValueProjection::Vertex { .. } => {
                    matches!(key, GraphValue::Vertex(_)) || key.is_null()
                }
                _ => false,
            };
            if !valid || !key.validate_bounds() {
                return None;
            }
        }
        for (aggregate, value) in self.aggregates.iter().zip(&values) {
            let valid = match aggregate.function {
                GraphAggregateFunction::CountRows | GraphAggregateFunction::Count => {
                    matches!(value, GraphAggregateValue::Count(_))
                }
                GraphAggregateFunction::SumInt => {
                    matches!(value, GraphAggregateValue::Integer(_)) || value.is_null()
                }
                GraphAggregateFunction::AverageInt => {
                    matches!(value, GraphAggregateValue::Average(_)) || value.is_null()
                }
                _ => false,
            };
            if !valid {
                return None;
            }
        }
        Some(GraphAggregateRow {
            keys: keys.into_boxed_slice(),
            values: values.into_boxed_slice(),
        })
    }

    fn compare_projected<E>(
        &self,
        left: &ProjectedGroup<'_, '_>,
        right: &ProjectedGroup<'_, '_>,
        distinct: bool,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Ordering, E> {
        if distinct {
            for (a, b) in left.values.iter().zip(&right.values) {
                let result = compare_cell(
                    a.cell(),
                    b.cell(),
                    false,
                    GraphNullPlacement::First,
                    false,
                    control,
                )?;
                if result != Ordering::Equal {
                    return Ok(result);
                }
            }
        }
        // These cells were evaluated once for every qualified group before
        // selection. Reuse the exact result cells, not a scalar conversion or
        // a second execution of their expressions inside the comparator.
        for order in &self.projected_ordering {
            let result = compare_cell(
                left.values[order.column].cell(),
                right.values[order.column].cell(),
                order.descending,
                if order.nulls_first {
                    GraphNullPlacement::First
                } else {
                    GraphNullPlacement::Last
                },
                true,
                control,
            )?;
            if result != Ordering::Equal {
                return Ok(result);
            }
        }
        for order in &self.ordering {
            let result = compare_cell(
                left.group.cell(order.column),
                right.group.cell(order.column),
                order.descending,
                order.nulls,
                true,
                control,
            )?;
            if result != Ordering::Equal {
                return Ok(result);
            }
        }
        // Hidden evaluation keys, not the possibly noninjective output, remain
        // the canonical final tiebreak and DISTINCT representative selector.
        for (a, b) in left.group.key.iter().zip(right.group.key) {
            control(GlaExecutionEvent::Work)?;
            for _ in 0..a.payload_units().max(b.payload_units()) {
                control(GlaExecutionEvent::Work)?;
            }
            let result = a.cmp(b);
            if result != Ordering::Equal {
                return Ok(result);
            }
        }
        Ok(left.group.key.len().cmp(&right.group.key.len()))
    }

    fn projected_equal<E>(
        &self,
        left: &ProjectedGroup<'_, '_>,
        right: &ProjectedGroup<'_, '_>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<bool, E> {
        for (a, b) in left.values.iter().zip(&right.values) {
            if compare_cell(
                a.cell(),
                b.cell(),
                false,
                GraphNullPlacement::First,
                false,
                control,
            )? != Ordering::Equal
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn sift_projected<E>(
        &self,
        heap: &mut [ProjectedGroup<'_, '_>],
        mut root: usize,
        end: usize,
        distinct: bool,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        while root < end / 2 {
            let mut child = 2 * root + 1;
            if child + 1 < end
                && self.compare_projected(&heap[child], &heap[child + 1], distinct, control)?
                    == Ordering::Less
            {
                child += 1;
            }
            if self.compare_projected(&heap[root], &heap[child], distinct, control)?
                != Ordering::Less
            {
                break;
            }
            control(GlaExecutionEvent::Work)?;
            heap.swap(root, child);
            root = child;
        }
        Ok(())
    }

    fn sort_projected<E>(
        &self,
        rows: &mut [ProjectedGroup<'_, '_>],
        distinct: bool,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        let len = rows.len();
        for root in (0..len / 2).rev() {
            self.sift_projected(rows, root, len, distinct, control)?;
        }
        for end in (1..len).rev() {
            control(GlaExecutionEvent::Work)?;
            rows.swap(0, end);
            self.sift_projected(rows, 0, end, distinct, control)?;
        }
        Ok(())
    }

    pub(super) fn finish_projected_groups<'g, 'a: 'g, E, C>(
        &'g self,
        groups: &'g BTreeMap<Vec<ValueRef<'a>>, Vec<Accumulator<'a>>>,
        projection: &'g [crate::GraphSetProjection],
        read: &mut crate::set_ops::ElementRead<'_, E>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), QueryError<E, C>>,
    ) -> Result<Vec<GraphAggregateRow>, QueryError<E, C>> {
        let mut control: &mut dyn FnMut(GlaExecutionEvent) -> Result<(), QueryError<E, C>> =
            control;
        let control = &mut control;
        let mut rows = Vec::new();
        // Complete every surviving group's expressions before selection, even
        // for an impossible page. No result-row charge or hidden payload copy.
        for (key, state) in groups {
            control(GlaExecutionEvent::Work)?;
            let group = Group { key, state };
            if !self.keep(group, control)? {
                continue;
            }
            control(GlaExecutionEvent::ScratchEntry)?;
            let mut values = Vec::new();
            let mut input = ExpressionInput {
                columns: |at| input_cell(group, at),
                width: group.key.len() + group.state.len(),
                locals: Vec::new(),
                read,
            };
            for (column, value) in projection.iter().enumerate() {
                control(GlaExecutionEvent::ScratchEntry)?;
                values.push(expression(value.value(), &mut input, column, control)?);
            }
            control(GlaExecutionEvent::ScratchEntry)?;
            rows.push(ProjectedGroup { group, values });
        }
        if self.output_distinct {
            self.sort_projected(&mut rows, true, control)?;
            let mut unique = 0;
            for read in 0..rows.len() {
                if unique == 0 || !self.projected_equal(&rows[unique - 1], &rows[read], control)? {
                    if unique != read {
                        control(GlaExecutionEvent::Work)?;
                        rows.swap(unique, read);
                    }
                    unique += 1;
                }
            }
            rows.truncate(unique);
        }
        if self.output_distinct || !self.ordering.is_empty() || !self.projected_ordering.is_empty()
        {
            self.sort_projected(&mut rows, false, control)?;
        }
        let offset = usize::try_from(self.offset).unwrap_or(usize::MAX);
        let count = self
            .count
            .and_then(|count| usize::try_from(count).ok())
            .unwrap_or(usize::MAX);
        let mut output = Vec::new();
        for row in rows.into_iter().skip(offset).take(count) {
            control(GlaExecutionEvent::ResultRow)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            let mut values = Vec::new();
            for value in row.values {
                control(GlaExecutionEvent::ScratchEntry)?;
                values.push(value.into_owned(control)?);
            }
            output.push(GraphAggregateRow {
                keys: Box::new([]),
                values: values.into_boxed_slice(),
            });
        }
        Ok(output)
    }
}

fn compare_cell<E>(
    a: Cell<'_>,
    b: Cell<'_>,
    descending: bool,
    nulls: GraphNullPlacement,
    orderability: bool,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Ordering, E> {
    control(GlaExecutionEvent::Work)?;
    let result = match (a.is_null(), b.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => {
            if nulls == GraphNullPlacement::First {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (false, true) => {
            if nulls == GraphNullPlacement::First {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (false, false) => {
            for _ in 0..a.payload_units().max(b.payload_units()) {
                control(GlaExecutionEvent::Work)?;
            }
            let result = if orderability {
                a.compare_orderability(b)
            } else {
                a.compare(b)
            };
            if descending { result.reverse() } else { result }
        }
    };
    Ok(result)
}

#[cfg(test)]
mod incremental_row_tests {
    use super::*;
    use crate::algebra::{GraphColumn, GraphPatternBuilder};
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_types::VId;

    fn shape(keys: &[usize], count: Option<u64>) -> PreparedGraphAggregate {
        let mut input = GraphPatternBuilder::new();
        input.vertex("n").unwrap();
        let input = input
            .prepare_values(
                &[
                    GraphColumn::property("group", "n", PropertyKeyId(1)),
                    GraphColumn::property("score", "n", PropertyKeyId(2)),
                ],
                0,
                None,
            )
            .unwrap()
            .with_duplicates();
        PreparedGraphAggregate::prepare(
            input,
            keys,
            &[
                GraphAggregate::count_rows("rows"),
                GraphAggregate::sum_int("sum", 1),
                GraphAggregate::average_int("average", 1),
            ],
            0,
            count,
        )
        .unwrap()
    }
    fn values() -> Vec<GraphAggregateValue> {
        vec![
            GraphAggregateValue::Count(2),
            GraphAggregateValue::Integer(3),
            GraphAggregateValue::Average(GraphExactAverage::new(3, 2).unwrap()),
        ]
    }

    #[test]
    fn maintained_rows_preserve_key_and_exact_result_domains() {
        let query = shape(&[0], None);
        let key = GraphValue::Scalar(CanonicalScalar::ucs_basic_text("group").unwrap());
        let row = query.incremental_row(vec![key.clone()], values()).unwrap();
        assert_eq!(row.keys(), &[key]);
        assert_eq!(row.values(), values());
        assert!(query.incremental_row(Vec::new(), values()).is_none());
        assert!(
            query
                .incremental_row(vec![GraphValue::Vertex(VId(1))], values())
                .is_none()
        );
        let null = GraphValue::Scalar(CanonicalScalar::Null);
        let mut wrong = values();
        wrong[0] = GraphAggregateValue::Integer(2);
        assert!(query.incremental_row(vec![null.clone()], wrong).is_none());
        let mut wrong = values();
        wrong[2] = GraphAggregateValue::Integer(1);
        assert!(query.incremental_row(vec![null.clone()], wrong).is_none());
        assert!(
            query
                .incremental_row(vec![null], vec![GraphAggregateValue::Count(1)])
                .is_none()
        );
    }

    #[test]
    fn empty_global_and_all_null_group_rows_remain_distinct_from_transformed_output() {
        let null = GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null));
        let global = shape(&[], None)
            .incremental_row(
                Vec::new(),
                vec![GraphAggregateValue::Count(0), null.clone(), null.clone()],
            )
            .unwrap();
        assert!(global.keys().is_empty());
        assert_eq!(global.get(0).unwrap().as_count(), Some(0));
        let query = shape(&[0], None);
        let key = GraphValue::Scalar(CanonicalScalar::Null);
        let grouped = query
            .incremental_row(
                vec![key.clone()],
                vec![GraphAggregateValue::Count(2), null.clone(), null],
            )
            .unwrap();
        assert_eq!(grouped.keys(), std::slice::from_ref(&key));
        assert_eq!(grouped.get(0).unwrap().as_count(), Some(2));
        assert!(
            shape(&[0], Some(1))
                .incremental_row(vec![key], values())
                .is_none()
        );
    }
}
