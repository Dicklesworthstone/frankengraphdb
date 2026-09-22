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

impl PreparedGraphSet {
    pub(crate) fn has_repeated_factor(&self, columns: &[usize]) -> bool {
        if !self.order.is_empty() || columns.iter().any(|&at| at >= self.types.len()) {
            return false;
        }
        match &self.node {
            SetNode::CrossJoin { left, .. } => columns.iter().all(|&at| at < left.types.len()),
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
