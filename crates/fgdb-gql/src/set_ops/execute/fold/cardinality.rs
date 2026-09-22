//! Cardinality-only physical reduction of independent relational factors.
//!
//! A product contributes |left| * |right| and UNION ALL contributes their sum.
//! No pair is constructed and no factor is executed twice. Value-sensitive
//! operators retain the existing row executor, including their local pages,
//! expression failures, equality and order. Only a cardinality-only suffix may
//! elide sorting: which rows a parent filter sees is never changed.

use super::*;

mod amount;
pub(super) use amount::Amount;

// Admission follows the checked expression, never query text or bound values
// from an earlier execution. A constant can still FAIL, so this is permission
// to evaluate it once after input completion, not permission to skip it.
pub(super) fn constant(value: &GraphSetValue) -> bool {
    match value {
        GraphSetValue::Column(_) => false,
        GraphSetValue::Literal(_) | GraphSetValue::Value(_) => true,
        GraphSetValue::Integer(expression) => {
            expression.referenced_columns().into_iter().next().is_none()
        }
        GraphSetValue::List(values) => values.iter().all(constant),
        GraphSetValue::Index { list, index } => constant(list) && constant(index),
        GraphSetValue::Size(list) => constant(list),
    }
}

pub(super) fn total_projection(projection: &[GraphSetProjection]) -> bool {
    projection.iter().all(|column| matches!(column.value(),
        GraphSetValue::Column(_) | GraphSetValue::Literal(_) | GraphSetValue::Value(_)
    ))
}

impl PreparedGraphSet {
    pub(crate) fn has_factorized_cardinality(&self) -> bool {
        match &self.node {
            SetNode::CrossJoin { .. }
            | SetNode::Binary {
                operation: GraphSetOperation::Union,
                quantifier: GraphSetQuantifier::All,
                ..
            } => true,
            SetNode::Unwind { value, .. } => constant(value),
            SetNode::Project {
                input, projection, quantifier: GraphSetQuantifier::All,
            } => total_projection(projection) && input.has_factorized_cardinality(),
            SetNode::Scope(input) => input.has_factorized_cardinality(),
            _ => false,
        }
    }

    /// Return an exact final u64 count, or None proving final COUNT overflow.
    /// Overflow is not an execution failure here: all sources must finish and
    /// pages/empty factors must be applied before the aggregate can refuse.
    /// Work, source visits and scratch share the original query allowance.
    pub(crate) fn count_governed<E, C>(
        &self,
        policy: GqlQueryPolicy,
        mut source: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> SetResult<(Option<u64>, GqlExecutionStats, GlaExecutionStats), E, C> {
        let mut meter = Meter {
            policy,
            checkpoint,
            rows: GqlExecutionStats {
                snapshot_records: 0,
                result_rows: 0,
            },
            evaluator: GlaExecutionStats::default(),
        };
        let mut operand = 0;
        let count = count(self, &mut source, &mut meter, &mut operand)?;
        meter.event(GlaExecutionEvent::Work)?;
        Ok((count.to_u64(), meter.rows, meter.evaluator))
    }
}

/// One cardinality interpreter for both COUNT(*) and repeated-value factors.
/// Amount retains subtraction headroom above the largest exact numeric domain;
/// saturation can never be mistaken for an exact count or signed sum.
pub(super) fn count<E, C, S, Checkpoint>(
    query: &PreparedGraphSet,
    source: &mut S,
    meter: &mut Meter<Checkpoint>,
    operand: &mut usize,
) -> SetResult<Amount, E, C>
where
    S: FnMut(
        &PreparedGraphPattern<GraphValueRow>,
        GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
    Checkpoint: FnMut() -> Result<(), C>,
{
    meter.event(GlaExecutionEvent::Work)?;
    let size = match &query.node {
        SetNode::Values => Amount::ONE,
        SetNode::CrossJoin { left, right } => {
            let left = count(left, source, meter, operand)?;
            // Do not short-circuit zero, overflow or a parent LIMIT 0. The
            // right source can fail and its negative-read witnesses matter.
            let right = count(right, source, meter, operand)?;
            left.multiply(right)
        }
        SetNode::Binary {
            operation: GraphSetOperation::Union,
            quantifier: GraphSetQuantifier::All,
            left,
            right,
        } => {
            let left = count(left, source, meter, operand)?;
            let right = count(right, source, meter, operand)?;
            left.add(right)
        }
        SetNode::Scope(input) => count(input, source, meter, operand)?,
        SetNode::Unwind { input, value } if constant(value) => {
            let input_size = count(input, source, meter, operand)?;
            if input_size.is_zero() {
                Amount::ZERO // An empty input never evaluates a downstream expression.
            } else {
                let column = input.types.len();
                let value = projection::evaluate_value(
                    value, &GraphValueRow::unit(), column, &mut |event| meter.event(event),
                ).map_err(|error| projected(error, 0))?;
                let elements = match value {
                    GraphValue::List(values) => values.len() as u128,
                    value if value.is_null() => 0,
                    _ => return Err(GqlQueryError::Source(GraphSetExecutionError::Projection {
                        row: 0, column,
                        error: crate::GraphIntegerError {
                            instruction: 0,
                            kind: crate::GraphIntegerErrorKind::IncompatibleOperands,
                        },
                    })),
                };
                input_size.multiply(Amount::from_u128(elements))
            }
        }
        SetNode::Project {
            input, projection, quantifier: GraphSetQuantifier::All,
        } if total_projection(projection) => {
            // Checked aliases and already-admitted literal values cannot fail
            // semantically. Their unused copies and sort may be eliminated.
            // Arithmetic, indexing and list construction remain barriers:
            // even wrapping a column in a list can exceed runtime depth bounds.
            count(input, source, meter, operand)?
        }
        _ => {
            // Existing visit/run owns every value-sensitive barrier, including
            // DISTINCT, filters, computed projections and dynamic UNWIND.
            // Its page has already been applied; do not apply it twice.
            let mut size = Amount::ZERO;
            visit(query, source, meter, operand, &mut |_, meter| {
                meter.event(GlaExecutionEvent::Work)?;
                size = size.add(Amount::ONE);
                Ok(())
            })?;
            return Ok(size);
        }
    };
    meter.event(GlaExecutionEvent::Work)?;
    let selected = size.subtract(query.offset);
    Ok(query.count.map_or(selected, |limit| selected.limit(limit)))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod pipeline_tests;
