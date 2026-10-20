//! Two endpoint factors from one source admission. The existing ordered probe
//! index and predicate kernel select borrowed triples before any row is copied.

use super::*;
use crate::set_ops::filter::FilteredCrossInputs;

/// A narrow physical alternative to one completed independent-vertex product.
/// Both source factors finish before indexing, including when the leading bag
/// is empty. `None` retains the ordinary logical source without reinterpreting
/// a source refusal, residual expression, scope, projection or page.
pub(in crate::set_ops::execute) fn collect<E, C, S, Checkpoint>(
    left: &[GraphValueRow],
    (leading, right, code, projection): FilteredCrossInputs<'_>,
    source: &mut S,
    meter: &mut Meter<Checkpoint>,
    operand: &mut usize,
) -> SetResult<Option<Vec<GraphValueRow>>, E, C>
where
    S: GraphSetSource<E, C> + ?Sized,
    Checkpoint: FnMut() -> Result<(), C>,
{
    if !source.supports_pattern_factors()
        || projection.is_some()
        || !right.order.is_empty()
        || right.offset != 0
        || right.count.is_some()
    {
        return Ok(None);
    }
    let SetNode::Pattern(pattern) = &right.node else {
        return Ok(None);
    };
    let Some(columns) = pattern.independent_vertex_factor_columns() else {
        return Ok(None);
    };
    let width = leading.types.len();
    let mut keyed = [false; 2];
    for op in code {
        if matches!(op, GraphSetPredicateOp::And) {
            continue;
        }
        let Some((_, column)) = equality(op, width, None) else {
            return Ok(None);
        };
        let Some(ValueProjection::Vertex { slot } | ValueProjection::Property { slot, .. }) =
            columns.get(column)
        else {
            return Ok(None);
        };
        keyed[slot.ordinal() as usize] = true;
    }
    if !keyed.into_iter().all(|keyed| keyed) {
        return Ok(None);
    }

    let request = pattern
        .independent_vertex_factors_with_control(&mut |event| meter.event(event))?
        .expect("the allocation-free shape proof admitted this immutable pattern");
    meter.event(GlaExecutionEvent::Work)?;
    let at = *operand;
    *operand += 1;
    let execution =
        source
            .pattern_factors(&request, meter.remaining())
            .ok_or(GqlQueryError::Source(
                GraphSetExecutionError::PatternFactorsUnavailable { operand: at },
            ))?;
    let execution = execution.map_err(|error| meter.source_error(error))?;
    let retained = execution.value.iter().try_fold(0u64, |total, rows| {
        total.checked_add(u64::try_from(rows.len()).ok()?)
    });
    meter.absorb_statistics(execution.rows, execution.evaluator, retained, at)?;
    let [first, second] = execution.value;
    let mut admitted = [Vec::new(), Vec::new()];
    for ((rows, pattern), output) in [first, second]
        .into_iter()
        .zip(request.factors())
        .zip(&mut admitted)
    {
        let mut types = Vec::new();
        for column in pattern.value_columns() {
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            types.push(GraphSetColumnType::from(column));
        }
        *output = finish_source_rows(rows, &types, at, meter)?;
    }
    // A unique source identity domain is part of the factor owner's contract.
    // Repeated identities would repeat complete B runs within one A identity,
    // which is not the original complete-row canonical product order under ALL.
    // Validate even if the leading bag or the other completed factor is empty.
    for rows in &admitted {
        let mut previous = None;
        for row in rows {
            meter.event(GlaExecutionEvent::Work)?;
            let Some(GraphValue::Vertex(vertex)) = row.values().first() else {
                return Err(GqlQueryError::Source(GraphSetExecutionError::InputSchema {
                    operand: at,
                }));
            };
            if previous.is_some_and(|previous| previous >= *vertex) {
                return Err(GqlQueryError::Source(GraphSetExecutionError::InputSchema {
                    operand: at,
                }));
            }
            previous = Some(*vertex);
        }
    }
    let [first, second] = admitted;
    if left.is_empty() || first.is_empty() || second.is_empty() {
        return Ok(Some(Vec::new()));
    }

    // Partition only total equality instructions. Their comparison semantics
    // remain owned by the existing selected-cross interpreter and key index.
    let mut predicates = [Vec::new(), Vec::new()];
    for op in code {
        meter.event(GlaExecutionEvent::Work)?;
        let Some((left, column)) = equality(op, width, None) else {
            continue; // The proof above admits only an AND at this position.
        };
        let (factor, column) = request.columns()[column];
        let predicate = &mut predicates[factor];
        let conjunction = !predicate.is_empty();
        meter.event(GlaExecutionEvent::ScratchEntry)?;
        predicate.push(GraphSetPredicateOp::Compare {
            left: GraphSetOperand::Column(left),
            comparison: IntegerComparison::Equal,
            right: GraphSetOperand::Column(width + column),
        });
        if conjunction {
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            predicate.push(GraphSetPredicateOp::And);
        }
    }
    let mut columns = Vec::new();
    for column in 0..width {
        meter.event(GlaExecutionEvent::ScratchEntry)?;
        columns.push(column);
    }
    let first_width = request.factors()[0].value_columns().len();
    for &(factor, column) in request.columns() {
        meter.event(GlaExecutionEvent::ScratchEntry)?;
        columns.push(width + usize::from(factor == 1) * first_width + column);
    }

    let mut control = |event| meter.event(event);
    let first_index = Index::build(width, &first, &predicates[0], None, &mut control)?;
    let second_index = Index::build(width, &second, &predicates[1], None, &mut control)?;
    let mut hints = [0; 2];
    let mut output = Vec::new();
    for row in left {
        control(GlaExecutionEvent::Work)?;
        let first_range = first_index.range(row, &first, &mut hints[0], &mut control)?;
        let second_range = second_index.range(row, &second, &mut hints[1], &mut control)?;
        if second_range.is_empty() {
            continue;
        }
        for a in first_range {
            control(GlaExecutionEvent::Work)?;
            let a = first_index.row(&first, a);
            if !GraphSetPredicateOp::evaluate_pair_with_control(
                &predicates[0],
                row,
                a,
                &mut control,
            )? {
                continue;
            }
            for b in second_range.clone() {
                control(GlaExecutionEvent::Work)?;
                let b = second_index.row(&second, b);
                if GraphSetPredicateOp::evaluate_pair_with_control(
                    &predicates[1],
                    row,
                    b,
                    &mut control,
                )? {
                    output.push(copy_cells(
                        columns.iter().map(|&column| {
                            if column < width {
                                &row.values()[column]
                            } else if column < width + first_width {
                                &a.values()[column - width]
                            } else {
                                &b.values()[column - width - first_width]
                            }
                        }),
                        &mut control,
                    )?);
                }
            }
        }
    }
    control(GlaExecutionEvent::Work)?;
    Ok(Some(output))
}

#[cfg(test)]
mod tests;
