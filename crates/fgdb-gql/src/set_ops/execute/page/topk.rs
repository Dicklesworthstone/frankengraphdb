//! Fallible bounded ranking over the ordinary complete-row comparator.
//!
//! The heap root is the worst retained occurrence. Its size is at most
//! OFFSET + LIMIT; rows tied on explicit keys use the ordinary whole-row tie
//! break. Equal complete rows remain separate occurrences, never DISTINCT.
//! Input arrays and one current row remain outside this bound. Copy/work
//! charges are cumulative, not peak allocator bytes, and no spill is implied.

use super::*;

struct Ranked<'a> {
    order: &'a [GraphValueOrder],
    bound: u128,
    rows: Vec<GraphValueRow>,
}

impl<'a> Ranked<'a> {
    fn new(order: &'a [GraphValueOrder], offset: u64, count: u64) -> Self {
        Self {
            order,
            // A zero page retains nothing even after a huge OFFSET. Otherwise
            // use a wide bound: a tiny input with offset+count > u64 must not
            // fail or preallocate an advertised, never-observed population.
            bound: if count == 0 {
                0
            } else {
                u128::from(offset) + u128::from(count)
            },
            rows: Vec::new(),
        }
    }

    /// This is private tentative state. A refusal drops the ENTIRE query-owned
    /// heap; a partially sifted heap is never reused or returned to a caller.
    fn offer<E>(
        &mut self,
        row: GraphValueRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        control(GlaExecutionEvent::Work)?;
        if self.bound == 0 {
            return Ok(());
        }
        if (self.rows.len() as u128) < self.bound {
            control(GlaExecutionEvent::ScratchEntry)?;
            self.rows.push(row);
            let mut at = self.rows.len() - 1;
            while at != 0 {
                let parent = (at - 1) / 2;
                if compare_rows(&self.rows[parent], &self.rows[at], self.order, control)?
                    != Ordering::Less
                {
                    break;
                }
                control(GlaExecutionEvent::Work)?;
                self.rows.swap(parent, at);
                at = parent;
            }
        } else if compare_rows(&row, &self.rows[0], self.order, control)? == Ordering::Less {
            // Reuse one retained slot. The upstream producer has already
            // admitted the new row's owned cells and payload copy.
            control(GlaExecutionEvent::Work)?;
            self.rows[0] = row;
            let mut at = 0;
            while at < self.rows.len() / 2 {
                let mut child = 2 * at + 1;
                if child + 1 < self.rows.len()
                    && compare_rows(
                        &self.rows[child],
                        &self.rows[child + 1],
                        self.order,
                        control,
                    )? == Ordering::Less
                {
                    child += 1;
                }
                if compare_rows(&self.rows[at], &self.rows[child], self.order, control)?
                    != Ordering::Less
                {
                    break;
                }
                control(GlaExecutionEvent::Work)?;
                self.rows.swap(at, child);
                at = child;
            }
        }
        Ok(())
    }

    fn finish<E>(
        mut self,
        offset: u64,
        count: u64,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Vec<GraphValueRow>, E> {
        control(GlaExecutionEvent::Work)?;
        let order = self.order;
        merge::sort(&mut self.rows, control, &mut |a, b, control| {
            compare_rows(a, b, order, control)
        })?;
        let mut selection = Selection {
            skip: offset,
            remaining: count,
        };
        let selected = selection.range(self.rows.len());
        let mut output = Vec::new();
        for (at, row) in self.rows.into_iter().enumerate() {
            control(GlaExecutionEvent::Work)?;
            if selected.contains(&at) {
                control(GlaExecutionEvent::ScratchEntry)?;
                output.push(row);
            }
        }
        control(GlaExecutionEvent::Work)?;
        Ok(output)
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
    let count = query.count.expect("finite ranked page admitted");
    let mut ranked = Ranked::new(&query.order, query.offset, count);
    fold::visit_unwindowed(query, source, meter, operand, &mut |row, meter| {
        ranked.offer(row, &mut |event| meter.event(event))
    })?;
    // All upstream sources and expressions have now succeeded. Sort only the
    // retained prefix, then apply OFFSET. The ordinary final sink, not this
    // private heap or an intermediate page, owns public ResultRow admission.
    ranked.finish(query.offset, count, &mut |event| meter.event(event))
}

#[cfg(test)]
mod tests;
