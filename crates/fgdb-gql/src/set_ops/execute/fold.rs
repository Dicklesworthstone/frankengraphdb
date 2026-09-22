//! Fold order-preserving relational stages without retaining their output bag.
//!
//! Sort/DISTINCT barriers and graph leaves keep the ordinary executor. UNWIND,
//! Cartesian expansion, filters and order-preserving ALL projections forward
//! owned rows directly. Pages drain their input: LIMIT 0 cannot hide failures.
//! This is query-local physical execution, not a new logical algebra or spill.

use super::*;

mod cardinality;
mod repeated;

type Consumer<'a, Checkpoint, E, C> =
    dyn FnMut(GraphValueRow, &mut Meter<Checkpoint>) -> SetResult<(), E, C> + 'a;

impl PreparedGraphSet {
    /// A terminal expansion can be folded even if its children need a sort.
    /// Never elide a projection's implicit canonicalization or explicit order.
    pub(crate) fn has_foldable_expansion(&self) -> bool {
        if !self.order.is_empty() {
            return false;
        }
        match &self.node {
            SetNode::Unwind { .. } | SetNode::CrossJoin { .. } => true,
            SetNode::Scope(input) | SetNode::Filter { input, .. } => input.has_foldable_expansion(),
            SetNode::Project {
                input,
                quantifier: GraphSetQuantifier::All,
                ..
            } if input.preserves_row_order() => input.has_foldable_expansion(),
            _ => false,
        }
    }

    /// Internal terminal fold. The sink's state is tentative until success.
    /// Source admission and every scalable stage share one cumulative meter.
    /// No intermediate occurrence spends the public result-row allowance.
    /// A semantic sink failure is deferred while upstream validation completes;
    /// interruption and resource refusal stop immediately. No partial sink
    /// state is returned by this method on failure.
    pub(crate) fn fold_governed<E, C>(
        &self,
        policy: GqlQueryPolicy,
        mut source: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        checkpoint: impl FnMut() -> Result<(), C>,
        mut consume: impl FnMut(
            GraphValueRow,
            &mut dyn FnMut(GlaExecutionEvent) -> SetResult<(), E, C>,
        ) -> SetResult<(), E, C>,
    ) -> SetResult<(GqlExecutionStats, GlaExecutionStats), E, C> {
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
        visit(
            self,
            &mut source,
            &mut meter,
            &mut operand,
            &mut |row, meter| consume(row, &mut |event| meter.event(event)),
        )?;
        meter.event(GlaExecutionEvent::Work)?;
        Ok((meter.rows, meter.evaluator))
    }
}

/// Selection is downstream of a stage's own validation. Remembering a sink
/// error must NOT suppress a later error in this stage or one of its children.
struct Window<E> {
    skip: u64,
    remaining: Option<u64>,
    downstream: Option<GraphSetExecutionError<E>>,
}
impl<E> Window<E> {
    fn new(skip: u64, remaining: Option<u64>) -> Self {
        Self {
            skip,
            remaining,
            downstream: None,
        }
    }

    fn push<C, Checkpoint>(
        &mut self,
        row: GraphValueRow,
        meter: &mut Meter<Checkpoint>,
        consume: &mut Consumer<'_, Checkpoint, E, C>,
    ) -> SetResult<(), E, C>
    where
        Checkpoint: FnMut() -> Result<(), C>,
    {
        meter.event(GlaExecutionEvent::Work)?;
        if self.skip != 0 {
            self.skip -= 1;
            return Ok(());
        }
        if self.remaining == Some(0) {
            return Ok(());
        }
        if let Some(remaining) = &mut self.remaining {
            *remaining -= 1;
        }
        if self.downstream.is_none() {
            remember(consume(row, meter), &mut self.downstream)?;
        }
        Ok(())
    }

    fn finish<C>(self) -> SetResult<(), E, C> {
        match self.downstream {
            Some(error) => Err(GqlQueryError::Source(error)),
            None => Ok(()),
        }
    }
}

fn remember<E, C>(
    result: SetResult<(), E, C>,
    deferred: &mut Option<GraphSetExecutionError<E>>,
) -> SetResult<(), E, C> {
    match result {
        Err(GqlQueryError::Source(error)) => {
            *deferred = Some(error);
            Ok(())
        }
        result => result,
    }
}

fn projected<E, C>(
    failure: projection::ProjectionFailure<GqlQueryError<GraphSetExecutionError<E>, C>>,
    row: usize,
) -> GqlQueryError<GraphSetExecutionError<E>, C> {
    match failure {
        projection::ProjectionFailure::Control(error) => error,
        projection::ProjectionFailure::Arithmetic { column, error } => {
            GqlQueryError::Source(GraphSetExecutionError::Projection { row, column, error })
        }
    }
}

fn visit<E, C, S, Checkpoint>(
    query: &PreparedGraphSet,
    source: &mut S,
    meter: &mut Meter<Checkpoint>,
    operand: &mut usize,
    consume: &mut Consumer<'_, Checkpoint, E, C>,
) -> SetResult<(), E, C>
where
    S: FnMut(
        &PreparedGraphPattern<GraphValueRow>,
        GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
    Checkpoint: FnMut() -> Result<(), C>,
{
    // The ordinary path remains the only implementation of a sorting or set
    // barrier. Its complete selected output is already in its declared order.
    if !query.has_foldable_expansion() {
        let rows = run(query, source, meter, operand)?;
        let mut forward = Window::new(0, None);
        for row in rows {
            forward.push(row, meter, consume)?;
        }
        return forward.finish();
    }

    meter.event(GlaExecutionEvent::Work)?;
    let mut window = Window::new(query.offset, query.count);
    if let Some((left, right, code, projection)) = query.filtered_cross_inputs() {
        // The materialized and folded paths share the exact selected-pair walk.
        // Complete both original children first; even LIMIT 0 cannot hide a
        // late source or child-expression failure. Never retain the joined bag.
        let left = run(left, source, meter, operand)?;
        let right = run(right, source, meter, operand)?;
        let columns = selected_cross::columns(projection, &mut |event| meter.event(event))?;
        selected_cross::visit_with_context(
            &left,
            &right,
            code,
            columns.as_deref(),
            meter,
            |meter, event| meter.event(event),
            |left, right, meter| {
                let row =
                    selected_cross::copy_pair(left, right, columns.as_deref(), &mut |event| {
                        meter.event(event)
                    })?;
                // Keep the ordinary window and deferred downstream-error law.
                // The probe walk drains even after the output page is full.
                window.push(row, meter, consume)
            },
        )?;
        return window.finish();
    }
    let mut local = None;
    match &query.node {
        SetNode::CrossJoin { left, right } => {
            // Both children execute once, left-to-right, even if either is
            // empty. Their own sort/page and source-failure order are intact.
            let left = run(left, source, meter, operand)?;
            let right = run(right, source, meter, operand)?;
            for a in &left {
                for b in &right {
                    meter.event(GlaExecutionEvent::Work)?;
                    meter.event(GlaExecutionEvent::ScratchEntry)?;
                    let mut values = Vec::new();
                    for value in a.values().iter().chain(b.values()) {
                        values.push(projection::copy_value(value, &mut |event| {
                            meter.event(event)
                        })?);
                    }
                    window.push(GraphValueRow::from_owned_values(values), meter, consume)?;
                }
            }
        }
        SetNode::Unwind { input, value } => {
            let mut row_at = 0_usize;
            visit(input, source, meter, operand, &mut |row, meter| {
                if local.is_some() {
                    return Ok(());
                }
                let result = (|| {
                    meter.event(GlaExecutionEvent::Work)?;
                    let column = row.len();
                    let list = projection::evaluate_value(value, &row, column, &mut |event| {
                        meter.event(event)
                    })
                    .map_err(|error| projected(error, row_at))?;
                    let values = match list {
                        GraphValue::List(values) => values,
                        value if value.is_null() => return Ok(()),
                        _ => {
                            return Err(GqlQueryError::Source(
                                GraphSetExecutionError::Projection {
                                    row: row_at,
                                    column,
                                    error: crate::GraphIntegerError {
                                        instruction: 0,
                                        kind: crate::GraphIntegerErrorKind::IncompatibleOperands,
                                    },
                                },
                            ));
                        }
                    };
                    for value in values.into_vec() {
                        meter.event(GlaExecutionEvent::Work)?;
                        meter.event(GlaExecutionEvent::ScratchEntry)?;
                        let mut cells = Vec::new();
                        for cell in row.values() {
                            cells.push(projection::copy_value(cell, &mut |event| {
                                meter.event(event)
                            })?);
                        }
                        meter.event(GlaExecutionEvent::ScratchEntry)?;
                        cells.push(value);
                        window.push(GraphValueRow::from_owned_values(cells), meter, consume)?;
                    }
                    Ok(())
                })();
                remember(result, &mut local)?;
                row_at = row_at.checked_add(1).ok_or_else(|| {
                    GqlQueryError::Source(GraphSetExecutionError::AccountingOverflow {
                        dimension: GqlBudgetDimension::ResultRows,
                    })
                })?;
                Ok(())
            })?;
        }
        SetNode::Scope(input) => {
            visit(input, source, meter, operand, &mut |row, meter| {
                window.push(row, meter, consume)
            })?;
        }
        SetNode::Filter { input, predicate } => {
            visit(input, source, meter, operand, &mut |row, meter| {
                meter.event(GlaExecutionEvent::Work)?;
                if predicate.evaluate(&row, &mut |event| meter.event(event))? {
                    window.push(row, meter, consume)?;
                }
                Ok(())
            })?;
        }
        SetNode::Project {
            input, projection, ..
        } => {
            // Admission proves ALL and order preservation. Implicit canonical
            // projection sorting is never bypassed just because the sink sums.
            let mut row_at = 0_usize;
            visit(input, source, meter, operand, &mut |row, meter| {
                if local.is_some() {
                    return Ok(());
                }
                meter.event(GlaExecutionEvent::Work)?;
                let result =
                    projection::evaluate(&row, projection, &mut |event| meter.event(event))
                        .map_err(|error| projected(error, row_at))
                        .and_then(|row| window.push(row, meter, consume));
                remember(result, &mut local)?;
                row_at = row_at.checked_add(1).ok_or_else(|| {
                    GqlQueryError::Source(GraphSetExecutionError::AccountingOverflow {
                        dimension: GqlBudgetDimension::ResultRows,
                    })
                })?;
                Ok(())
            })?;
        }
        _ => unreachable!("fold admission is the closed terminal operator profile"),
    }
    if let Some(error) = local {
        return Err(GqlQueryError::Source(error));
    }
    window.finish()
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod selected_tests;
