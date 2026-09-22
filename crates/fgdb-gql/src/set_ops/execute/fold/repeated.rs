//! Observed left-factor rows with contiguous, unobserved right multiplicities.
//!
//! This is the left-major Cartesian preservation law, not a commutativity
//! shortcut: each emitted tuple stands for one contiguous run in the original
//! input order. SUM's checked prefixes and aggregate error positions therefore
//! remain meaningful. Both children complete, left-to-right, before any sink
//! runs. The ordinary executor still owns all value/order-sensitive barriers.

use super::*;
use super::cardinality::{self, Amount};

type RepeatedRows = Vec<(GraphValueRow, Amount)>;

// Definition-only mapping, bounded by the admitted row schema. Literal
// columns need no child value. Duplicate aliases share one retained input.
fn projection_inputs(projection: &[GraphSetProjection], columns: &[usize]) -> Vec<usize> {
    let mut inputs = Vec::new();
    for &column in columns {
        if let GraphSetValue::Column(input) = projection[column].value() {
            inputs.push(*input);
        }
    }
    inputs.sort_unstable();
    inputs.dedup();
    inputs
}

impl PreparedGraphSet {
    pub(crate) fn has_repeated_factor(&self, columns: &[usize]) -> bool {
        if !self.order.is_empty() || columns.iter().any(|&at| at >= self.types.len()) {
            return false;
        }
        match &self.node {
            SetNode::CrossJoin { left, .. } => columns.iter().all(|&at| at < left.types.len()),
            SetNode::Unwind { input, value } => {
                cardinality::constant(value)
                    && columns.iter().all(|&at| at < input.types.len())
            }
            SetNode::Project { input, projection, quantifier: GraphSetQuantifier::All }
                if input.preserves_row_order() && cardinality::total_projection(projection) =>
            {
                input.has_repeated_factor(&projection_inputs(projection, columns))
            }
            SetNode::Scope(input) => input.has_repeated_factor(columns),
            _ => false,
        }
    }

    /// A private compact schema, exactly `columns` in the supplied order.
    /// Multiplicity Some(n) is exact and positive; None proves >u128::MAX.
    /// No hidden-column placeholder, fabricated graph, or partial result is
    /// supplied. Only immutable source adapters may enter this execution seam.
    pub(crate) fn fold_repeated_governed<E, C>(
        &self,
        columns: &[usize],
        policy: GqlQueryPolicy,
        mut source: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        checkpoint: impl FnMut() -> Result<(), C>,
        mut consume: impl FnMut(
            GraphValueRow,
            Option<u128>,
            &mut dyn FnMut(GlaExecutionEvent) -> SetResult<(), E, C>,
        ) -> SetResult<(), E, C>,
    ) -> SetResult<(GqlExecutionStats, GlaExecutionStats), E, C> {
        let mut meter = Meter {
            policy,
            checkpoint,
            rows: GqlExecutionStats { snapshot_records: 0, result_rows: 0 },
            evaluator: GlaExecutionStats::default(),
        };
        let rows = collect(self, columns, &mut source, &mut meter, &mut 0)?;
        for (row, repetitions) in rows {
            meter.event(GlaExecutionEvent::Work)?;
            consume(row, repetitions.to_u128(), &mut |event| meter.event(event))?;
        }
        meter.event(GlaExecutionEvent::Work)?;
        Ok((meter.rows, meter.evaluator))
    }
}

fn collect<E, C, S, Checkpoint>(
    query: &PreparedGraphSet,
    columns: &[usize],
    source: &mut S,
    meter: &mut Meter<Checkpoint>,
    operand: &mut usize,
) -> SetResult<RepeatedRows, E, C>
where
    S: FnMut(
        &PreparedGraphPattern<GraphValueRow>,
        GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
    Checkpoint: FnMut() -> Result<(), C>,
{
    meter.event(GlaExecutionEvent::Work)?;
    if !query.has_repeated_factor(columns) {
        // Finish the full original relation, including its own page and every
        // expression error, before dropping any unobserved columns. No barrier
        // is executed again after a refusal or speculative physical failure.
        let rows = run(query, source, meter, operand)?;
        let mut output = Vec::new();
        for row in rows {
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            let mut values = Vec::new();
            for &column in columns {
                values.push(projection::copy_value(&row.values()[column], &mut |event| {
                    meter.event(event)
                })?);
            }
            output.push((GraphValueRow::from_owned_values(values), Amount::ONE));
        }
        return Ok(output); // The original executor already selected this page.
    }
    let rows = match &query.node {
        SetNode::CrossJoin { left, right } => {
            let mut rows = collect(left, columns, source, meter, operand)?;
            // Even an empty left input cannot skip right-side failures or
            // negative-read witnesses. Count uses the SAME relational engine.
            let repetitions = cardinality::count(right, source, meter, operand)?;
            for (_, weight) in &mut rows {
                meter.event(GlaExecutionEvent::Work)?;
                *weight = weight.multiply(repetitions);
            }
            rows
        }
        SetNode::Scope(input) => collect(input, columns, source, meter, operand)?,
        SetNode::Unwind { input, value } => {
            let mut rows = collect(input, columns, source, meter, operand)?;
            // The appended value is not observed. Evaluate its complete list
            // once only after every input row succeeds, using the same checked
            // interpreter as COUNT. An empty input does not evaluate it at all.
            if !rows.is_empty() {
                let repetitions = cardinality::constant_unwind_size(value, input.types.len(), meter)?;
                for (_, weight) in &mut rows {
                    meter.event(GlaExecutionEvent::Work)?;
                    *weight = weight.multiply(repetitions);
                }
            }
            rows
        }
        SetNode::Project { input, projection, .. } => {
            // Admission checked ALL, preserved order and EVERY expression's
            // totality, not just observed columns. An unused division, index,
            // list constructor, filter or DISTINCT can never disappear here.
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            for _ in columns { meter.event(GlaExecutionEvent::ScratchEntry)?; }
            let inputs = projection_inputs(projection, columns);
            let rows = collect(input, &inputs, source, meter, operand)?;
            let mut output = Vec::new();
            for (row_at, (row, weight)) in rows.into_iter().enumerate() {
                meter.event(GlaExecutionEvent::ScratchEntry)?;
                let mut values = Vec::new();
                for &column in columns {
                    let value = projection[column].value();
                    // Rebind only the private compact slot. Literal/value
                    // payloads remain borrowed until their metered copy.
                    let rebound;
                    let expression = if let GraphSetValue::Column(input) = value {
                        rebound = GraphSetValue::Column(inputs.binary_search(input)
                            .expect("the projection retained every demanded input"));
                        &rebound
                    } else { value };
                    values.push(projection::evaluate_value(expression, &row, column,
                        &mut |event| meter.event(event))
                        .map_err(|error| projected(error, row_at))?);
                }
                output.push((GraphValueRow::from_owned_values(values), weight));
            }
            output
        }
        _ => unreachable!("the repeated-factor admission profile is closed"),
    };
    // Split occurrence windows through run lengths, not carrier-row indices.
    // All upstream validation is complete even for an impossible/empty window.
    let mut skip = query.offset;
    let mut remaining = query.count;
    let mut output = Vec::new();
    for (row, weight) in rows {
        meter.event(GlaExecutionEvent::Work)?;
        let skipped = weight.to_u64().map_or(skip, |n| n.min(skip));
        skip -= skipped;
        let mut selected = weight.subtract(skipped);
        if let Some(limit) = &mut remaining {
            selected = selected.limit(*limit);
            *limit -= selected.to_u64().expect("a finite page has an exact u64 size");
        }
        if !selected.is_zero() {
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            output.push((row, selected));
        }
    }
    Ok(output)
}
