//! Finite occurrence pages over the existing relational fold.
//!
//! Only this node's page is selected here. Child pages and sorting/DISTINCT
//! barriers remain in their original scopes. Complete source admission and
//! expression validation precede successful delivery, including for LIMIT 0.
//! This bounds retained output rows, not source residency or allocator bytes.

use super::*;

mod topk;

pub(super) fn supports(query: &PreparedGraphSet) -> bool {
    query.count.is_some() && query.has_foldable_node()
}

/// Consume an occurrence interval without ever forming offset + count, which
/// need not fit u64. Each returned range is bounded by its supplied group.
struct Selection {
    skip: u64,
    remaining: u64,
}
impl Selection {
    fn range(&mut self, len: usize) -> core::ops::Range<usize> {
        let skipped = u128::from(self.skip).min(len as u128);
        self.skip -= skipped as u64;
        let start = skipped as usize;
        let taken = u128::from(self.remaining).min((len - start) as u128);
        self.remaining -= taken as u64;
        start..start + taken as usize
    }
}

pub(super) fn collect<E, C, S, Checkpoint>(
    query: &PreparedGraphSet,
    source: &mut S,
    meter: &mut Meter<Checkpoint>,
    operand: &mut usize,
) -> SetResult<Vec<GraphValueRow>, E, C>
where
    S: FnMut(
        &PreparedGraphPattern<GraphValueRow>,
        GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
    Checkpoint: FnMut() -> Result<(), C>,
{
    debug_assert!(supports(query));
    if !query.order.is_empty() {
        return topk::collect(query, source, meter, operand);
    }
    let mut selection = Selection {
        skip: query.offset,
        remaining: query.count.expect("finite page admitted"),
    };
    let mut rows = Vec::new();
    if let SetNode::CrossJoin { left, right } = &query.node {
        let left = run(left, source, meter, operand)?;
        let right = run(right, source, meter, operand)?;
        // Both children have finished, including all their errors and pages.
        // Concatenation itself only copies admitted cells: skipping a product
        // range cannot erase a scalar-expression failure or a source witness.
        // Do not multiply input lengths or visit every skipped duplicate pair.
        for left in &left {
            meter.event(GlaExecutionEvent::Work)?;
            for at in selection.range(right.len()) {
                meter.event(GlaExecutionEvent::Work)?;
                rows.push(selected_cross::copy_pair(left, &right[at], None, &mut |event| {
                    meter.event(event)
                })?);
            }
        }
    } else if let Some((left, right, code, projection)) = query.filtered_cross_inputs() {
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
                meter.event(GlaExecutionEvent::Work)?;
                if !selection.range(1).is_empty() {
                    rows.push(selected_cross::copy_pair(
                        left,
                        right,
                        columns.as_deref(),
                        &mut |event| meter.event(event),
                    )?);
                }
                Ok(())
            },
        )?;
    } else {
        // Reuse the complete fold, including every nested local page and its
        // deferred semantic-error law. The fold applies THIS page exactly once.
        // It drains expressions even after the receiver has enough rows.
        fold::visit(query, source, meter, operand, &mut |row, meter| {
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            rows.push(row);
            Ok(())
        })?;
    }
    meter.event(GlaExecutionEvent::Work)?;
    Ok(rows)
}

#[cfg(test)]
mod tests;
