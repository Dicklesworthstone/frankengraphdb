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
        Cell::Value(ValueRef::Scalar(value)) => ExpressionCell::Scalar(Cow::Borrowed(value)),
        Cell::Value(_) => return Err(GraphIntegerErrorKind::NonScalar),
    })
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

fn expression<'g, E, C>(
    value: &'g GraphSetValue,
    input: impl Fn(usize) -> Result<Cell<'g>, GraphIntegerErrorKind> + Copy,
    column: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), QueryError<E, C>>,
) -> Result<OutputValue<'g>, QueryError<E, C>> {
    // The same checked expression program reads batch or maintained cells.
    // Preparation bounds the recursive expression depth and number of nodes.
    control(GlaExecutionEvent::Work)?;
    Ok(match value {
        GraphSetValue::Column(at) => {
            OutputValue::Borrowed(input(*at).map_err(|kind| failure(column, kind))?)
        }
        GraphSetValue::Literal(value) => {
            OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(value.value())))
        }
        GraphSetValue::Value(value) => OutputValue::Borrowed(Cell::Value(value_ref(value))),
        GraphSetValue::Integer(expression) => {
            let value = expression
                // Aggregate outputs bind no element scope (fgdb-20foe), so a
                // width of 0 makes any Local a MissingColumn, never a cell.
                .evaluate_loaded_with_control(0, |at| input(at).and_then(load_cell), control)
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
        // Aggregate outputs bind no element scope (fgdb-20foe): the text
        // compilers refuse a comprehension there before binding, so this is
        // a typed failure for a hand-built plan, never a guessed element.
        GraphSetValue::Local(_)
        | GraphSetValue::Comprehension { .. }
        | GraphSetValue::Quantifier { .. }
        | GraphSetValue::Reduce { .. } => {
            return Err(failure(column, GraphIntegerErrorKind::IncompatibleOperands));
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
            let value = GraphValue::Map {
                keys: keys.clone(),
                values: entries.into_boxed_slice(),
            };
            if !value.validate_bounds() {
                return Err(failure(column, GraphIntegerErrorKind::Overflow));
            }
            OutputValue::Owned(GraphAggregateValue::Value(value))
        }
        GraphSetValue::MapGet { map, key } => {
            let map = expression(map, input, column, control)?;
            match map.cell() {
                cell if cell.is_null() => {
                    OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(&NULL)))
                }
                Cell::Value(ValueRef::Map { keys, values }) => {
                    match keys.binary_search_by(|probe| probe.as_bytes().cmp(key.as_bytes())) {
                        Ok(at) => OutputValue::Owned(GraphAggregateValue::Value(
                            values[at].copy_with_control(control)?,
                        )),
                        Err(_) => OutputValue::Borrowed(Cell::Value(ValueRef::Scalar(&NULL))),
                    }
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
                    let members = crate::set_ops::range_values(start, end, step)
                        .map_err(|kind| failure(column, kind))?;
                    for _ in &members {
                        control(GlaExecutionEvent::ScratchEntry)?;
                    }
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
                    control,
                )?;
                if result != Ordering::Equal {
                    return Ok(result);
                }
            }
        }
        for order in &self.ordering {
            let result = compare_cell(
                left.group.cell(order.column),
                right.group.cell(order.column),
                order.descending,
                order.nulls,
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
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), QueryError<E, C>>,
    ) -> Result<Vec<GraphAggregateRow>, QueryError<E, C>> {
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
            for (column, value) in projection.iter().enumerate() {
                control(GlaExecutionEvent::ScratchEntry)?;
                values.push(expression(
                    value.value(),
                    |at| input_cell(group, at),
                    column,
                    control,
                )?);
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
            let result = a.compare(b);
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
