//! Borrowed post-group expressions and governed projected ranking.
use super::*;
use crate::{GraphIntegerError, GraphIntegerErrorKind, GraphIntegerEvaluationError, GraphSetValue};
use crate::integer_expression::ExpressionCell;
use std::borrow::Cow;

type QueryError<E, C> = GqlQueryError<GraphAggregateError<E>, C>;

enum OutputValue<'a> {
    Borrowed(Cell<'a>),
    Owned(GraphAggregateValue),
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
    }
}

impl<'a> OutputValue<'a> {
    fn cell(&self) -> Cell<'_> {
        match self {
            Self::Borrowed(value) => *value,
            Self::Owned(GraphAggregateValue::Count(value)) => Cell::Count(*value),
            Self::Owned(GraphAggregateValue::Integer(value)) => Cell::Integer(*value),
            Self::Owned(GraphAggregateValue::Average(value)) => Cell::Average {
                sum: value.numerator(), count: value.denominator(),
            },
            Self::Owned(GraphAggregateValue::Value(value)) => Cell::Value(value_ref(value)),
        }
    }

    fn into_owned<E>(self, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>) -> Result<GraphAggregateValue, E> {
        match self {
            Self::Owned(value) => Ok(value),
            Self::Borrowed(value) => {
                control(GlaExecutionEvent::ScratchEntry)?;
                Ok(match value {
                    Cell::Count(value) => GraphAggregateValue::Count(value),
                    Cell::Integer(value) => GraphAggregateValue::Integer(value),
                    Cell::Average { sum, count } => GraphAggregateValue::Average(
                        GraphExactAverage::new(sum, count).expect("nonnull average")),
                    Cell::Value(value) => GraphAggregateValue::Value(value.copy_owned(control)?),
                })
            }
        }
    }
}

fn failure<E, C>(column: usize, kind: GraphIntegerErrorKind) -> QueryError<E, C> {
    GqlQueryError::Source(GraphAggregateError::OutputExpression {
        column, error: GraphIntegerError { instruction: 0, kind },
    })
}

fn load_cell(cell: Cell<'_>) -> Result<ExpressionCell<'_>, GraphIntegerErrorKind> {
    Ok(match cell {
        Cell::Count(value) => ExpressionCell::Count(value),
        Cell::Integer(value) => ExpressionCell::Integer(value),
        Cell::Average { sum, count } => ExpressionCell::Average(
            GraphExactAverage::new(sum, count).expect("nonnull average")),
        Cell::Value(ValueRef::Scalar(value)) => ExpressionCell::Scalar(Cow::Borrowed(value)),
        Cell::Value(_) => return Err(GraphIntegerErrorKind::NonScalar),
    })
}

fn input_cell<'g, 'a: 'g>(group: Group<'g, 'a>, input: usize) -> Result<Cell<'g>, GraphIntegerErrorKind> {
    if input < group.key.len() {
        Ok(Cell::Value(group.key[input]))
    } else {
        group.state.get(input - group.key.len()).map(Cell::from_state)
            .ok_or(GraphIntegerErrorKind::MissingColumn)
    }
}

fn expression<'g, 'a: 'g, E, C>(
    value: &'g GraphSetValue,
    group: Group<'g, 'a>,
    column: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), QueryError<E, C>>,
) -> Result<OutputValue<'g>, QueryError<E, C>> {
    // Preparation bounds the recursive expression depth and number of nodes.
    control(GlaExecutionEvent::Work)?;
    Ok(match value {
        GraphSetValue::Column(input) => OutputValue::Borrowed(
            input_cell(group, *input).map_err(|kind| failure(column, kind))?),
        GraphSetValue::Literal(value) => OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(value.value()))),
        GraphSetValue::Value(value) => OutputValue::Borrowed(Cell::Value(value_ref(value))),
        GraphSetValue::Integer(expression) => {
            let value = expression.evaluate_loaded_with_control(
                |input| input_cell(group, input).and_then(load_cell), control,
            ).map_err(|error| match error {
                GraphIntegerEvaluationError::Control(error) => error,
                GraphIntegerEvaluationError::Value(error) => GqlQueryError::Source(
                    GraphAggregateError::OutputExpression { column, error }),
            })?;
            match value {
                ExpressionCell::Scalar(Cow::Borrowed(value)) => OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(value))),
                ExpressionCell::Scalar(Cow::Owned(value)) => {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    OutputValue::Owned(GraphAggregateValue::Value(GraphValue::Scalar(value)))
                }
                ExpressionCell::Count(value) => OutputValue::Borrowed(Cell::Count(value)),
                ExpressionCell::Integer(value) => OutputValue::Borrowed(Cell::Integer(value)),
                ExpressionCell::Average(value) => OutputValue::Borrowed(Cell::Average {
                    sum: value.numerator(), count: value.denominator(),
                }),
            }
        }
        GraphSetValue::List(values) => {
            control(GlaExecutionEvent::ScratchEntry)?;
            let mut list = Vec::new();
            for value in values {
                let value = expression(value, group, column, control)?.into_owned(control)?;
                // GraphValue lists have the ordinary scalar domain. Preserve
                // exactness or fail, never truncate an aggregate into an i64.
                let value = match value {
                    GraphAggregateValue::Value(value) => value,
                    GraphAggregateValue::Count(value) => GraphValue::Scalar(CanonicalScalar::Int(
                        i64::try_from(value).map_err(|_| failure(column, GraphIntegerErrorKind::Overflow))?)),
                    GraphAggregateValue::Integer(value) => GraphValue::Scalar(CanonicalScalar::Int(
                        i64::try_from(value).map_err(|_| failure(column, GraphIntegerErrorKind::Overflow))?)),
                    GraphAggregateValue::Average(_) => return Err(failure(column, GraphIntegerErrorKind::IncompatibleOperands)),
                };
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
            let list = expression(list, group, column, control)?;
            let value = match list.cell() {
                cell if cell.is_null() => CanonicalScalar::Null,
                Cell::Value(ValueRef::List(values)) => CanonicalScalar::Int(
                    i64::try_from(values.len()).map_err(|_| failure(column, GraphIntegerErrorKind::Overflow))?),
                _ => return Err(failure(column, GraphIntegerErrorKind::IncompatibleOperands)),
            };
            control(GlaExecutionEvent::ScratchEntry)?;
            OutputValue::Owned(GraphAggregateValue::Value(GraphValue::Scalar(value)))
        }
        GraphSetValue::Index { list, index } => {
            let list = expression(list, group, column, control)?;
            let index = expression(index, group, column, control)?;
            let list_cell = list.cell();
            let index = index.cell();
            if list_cell.is_null() || index.is_null() {
                OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(&NULL)))
            } else {
                let Cell::Value(ValueRef::List(values)) = list_cell else {
                    return Err(failure(column, GraphIntegerErrorKind::IncompatibleOperands));
                };
                let index = index.integer().ok_or_else(|| failure(column, GraphIntegerErrorKind::NonInteger))?;
                let at = if index < 0 { (values.len() as i128).checked_add(index) } else { Some(index) };
                match at.and_then(|at| usize::try_from(at).ok()).and_then(|at| values.get(at)) {
                    Some(value) => OutputValue::Owned(GraphAggregateValue::Value(value.copy_with_control(control)?)),
                    None => OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(&NULL))),
                }
            }
        }
    })
}

struct ProjectedGroup<'g, 'a> {
    group: Group<'g, 'a>,
    values: Vec<OutputValue<'g>>,
}


impl PreparedGraphAggregate {
    fn compare_projected<E>(
        &self, left: &ProjectedGroup<'_, '_>, right: &ProjectedGroup<'_, '_>,
        distinct: bool,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Ordering, E> {
        if distinct {
            for (a, b) in left.values.iter().zip(&right.values) {
                let result = compare_cell(a.cell(), b.cell(), false, GraphNullPlacement::First, control)?;
                if result != Ordering::Equal { return Ok(result); }
            }
        }
        for order in &self.ordering {
            let result = compare_cell(left.group.cell(order.column), right.group.cell(order.column), order.descending, order.nulls, control)?;
            if result != Ordering::Equal { return Ok(result); }
        }
        // Hidden evaluation keys, not the possibly noninjective output, remain
        // the canonical final tiebreak and DISTINCT representative selector.
        for (a, b) in left.group.key.iter().zip(right.group.key) {
            control(GlaExecutionEvent::Work)?;
            for _ in 0..a.payload_units().max(b.payload_units()) {
                control(GlaExecutionEvent::Work)?;
            }
            let result = a.cmp(b);
            if result != Ordering::Equal { return Ok(result); }
        }
        Ok(left.group.key.len().cmp(&right.group.key.len()))
    }

    fn projected_equal<E>(
        &self, left: &ProjectedGroup<'_, '_>, right: &ProjectedGroup<'_, '_>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<bool, E> {
        for (a, b) in left.values.iter().zip(&right.values) {
            if compare_cell(a.cell(), b.cell(), false, GraphNullPlacement::First, control)? != Ordering::Equal {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn sift_projected<E>(
        &self, heap: &mut [ProjectedGroup<'_, '_>], mut root: usize, end: usize, distinct: bool,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        while root < end / 2 {
            let mut child = 2 * root + 1;
            if child + 1 < end && self.compare_projected(&heap[child], &heap[child + 1], distinct, control)? == Ordering::Less {
                child += 1;
            }
            if self.compare_projected(&heap[root], &heap[child], distinct, control)? != Ordering::Less { break; }
            control(GlaExecutionEvent::Work)?;
            heap.swap(root, child);
            root = child;
        }
        Ok(())
    }

    fn sort_projected<E>(
        &self, rows: &mut [ProjectedGroup<'_, '_>], distinct: bool,
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
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), QueryError<E, C>>,
    ) -> Result<Vec<GraphAggregateRow>, QueryError<E, C>> {
        let mut rows = Vec::new();
        // Complete every surviving group's expressions before selection, even
        // for an impossible page. No result-row charge or hidden payload copy.
        for (key, state) in groups {
            control(GlaExecutionEvent::Work)?;
            let group = Group { key, state };
            if !self.keep(group, control)? { continue; }
            control(GlaExecutionEvent::ScratchEntry)?;
            let mut values = Vec::new();
            for (column, value) in projection.iter().enumerate() {
                control(GlaExecutionEvent::ScratchEntry)?;
                values.push(expression(value.value(), group, column, control)?);
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
        if self.output_distinct || !self.ordering.is_empty() {
            self.sort_projected(&mut rows, false, control)?;
        }
        let offset = usize::try_from(self.offset).unwrap_or(usize::MAX);
        let count = self.count.and_then(|count| usize::try_from(count).ok()).unwrap_or(usize::MAX);
        let mut output = Vec::new();
        for row in rows.into_iter().skip(offset).take(count) {
            control(GlaExecutionEvent::ResultRow)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            let mut values = Vec::new();
            for value in row.values {
                control(GlaExecutionEvent::ScratchEntry)?;
                values.push(value.into_owned(control)?);
            }
            output.push(GraphAggregateRow { keys: Box::new([]), values: values.into_boxed_slice() });
        }
        Ok(output)
    }
}

fn compare_cell<E>(
    a: Cell<'_>, b: Cell<'_>, descending: bool, nulls: GraphNullPlacement,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Ordering, E> {
    control(GlaExecutionEvent::Work)?;
    let result = match (a.is_null(), b.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => if nulls == GraphNullPlacement::First { Ordering::Less } else { Ordering::Greater },
        (false, true) => if nulls == GraphNullPlacement::First { Ordering::Greater } else { Ordering::Less },
        (false, false) => {
            for _ in 0..a.payload_units().max(b.payload_units()) {
                control(GlaExecutionEvent::Work)?;
            }
            let result = a.compare(b);
            if descending { result.reverse() } else { result }
        }
    };
    Ok(result)
}
