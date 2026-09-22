//! Cardinality-only physical reduction of independent relational factors.
//!
//! A product contributes |left| * |right| and UNION ALL contributes their sum.
//! No pair is constructed and no factor is executed twice. Value-sensitive
//! operators retain the existing row executor, including their local pages,
//! expression failures, equality and order. Only a cardinality-only suffix may
//! elide sorting: which rows a parent filter sees is never changed.

use super::*;

impl PreparedGraphSet {
    pub(crate) fn has_factorized_cardinality(&self) -> bool {
        match &self.node {
            SetNode::CrossJoin { .. }
            | SetNode::Binary {
                operation: GraphSetOperation::Union,
                quantifier: GraphSetQuantifier::All,
                ..
            } => true,
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
        Ok((u64::try_from(count).ok(), meter.rows, meter.evaluator))
    }
}

/// Exact through u128::MAX; saturation above it is only an overflow witness,
/// NEVER a returned count. A path has at most MAX_GRAPH_SET_DEPTH pages, each
/// subtracting at most u64::MAX. Even after all such subtractions a saturated
/// value remains above u64::MAX. Positive sums/products cannot lower it, zero
/// factors annihilate it, and a finite LIMIT restores an exact u64 count.
/// Consequently every representable final count remains exact, including
/// (2^64 SKIP u64::MAX) = 1. Saturating at u64 would give the wrong answer.
fn count<E, C, S, Checkpoint>(
    query: &PreparedGraphSet,
    source: &mut S,
    meter: &mut Meter<Checkpoint>,
    operand: &mut usize,
) -> SetResult<u128, E, C>
where
    S: FnMut(
        &PreparedGraphPattern<GraphValueRow>,
        GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
    Checkpoint: FnMut() -> Result<(), C>,
{
    meter.event(GlaExecutionEvent::Work)?;
    let size = match &query.node {
        SetNode::CrossJoin { left, right } => {
            let left = count(left, source, meter, operand)?;
            // Do not short-circuit zero, overflow or a parent LIMIT 0. The
            // right source can fail and its negative-read witnesses matter.
            let right = count(right, source, meter, operand)?;
            left.saturating_mul(right)
        }
        SetNode::Binary {
            operation: GraphSetOperation::Union,
            quantifier: GraphSetQuantifier::All,
            left,
            right,
        } => {
            let left = count(left, source, meter, operand)?;
            let right = count(right, source, meter, operand)?;
            left.saturating_add(right)
        }
        SetNode::Scope(input) => count(input, source, meter, operand)?,
        _ => {
            // Existing visit/run owns every value-sensitive barrier, including
            // DISTINCT, filters, computed projections and dynamic UNWIND.
            // Its page has already been applied; do not apply it twice.
            let mut size = 0_u128;
            visit(query, source, meter, operand, &mut |_, meter| {
                meter.event(GlaExecutionEvent::Work)?;
                size = size.saturating_add(1);
                Ok(())
            })?;
            return Ok(size);
        }
    };
    meter.event(GlaExecutionEvent::Work)?;
    let selected = size.saturating_sub(u128::from(query.offset));
    Ok(query.count.map_or(selected, |limit| selected.min(u128::from(limit))))
}

#[cfg(test)]
mod tests;
