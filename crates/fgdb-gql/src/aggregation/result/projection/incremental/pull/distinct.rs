//! Equality-class membership for the bounded completed-group rank prefix.
//!
//! This is not a global seen set. Once full, the prefix cutoff can only improve:
//! a discarded representative can never become competitive later. A new, better
//! representative of that forgotten class is admitted normally. Consequently
//! both the heap and this index have at most SKIP+LIMIT entries (and never more
//! than the raw group count), independent of how many losing classes occur.
//!
//! Keys use the existing output-DISTINCT equivalence, including exact rational
//! equality across top-level numeric variants. Rank still uses complete groups.

use super::*;
use std::collections::BTreeMap;
use std::sync::Arc;

pub(super) type DistinctKey = Arc<GraphAggregateRow>;

#[derive(Default)]
pub(super) struct DistinctIndex {
    positions: BTreeMap<DistinctKey, usize>,
    largest_units: usize,
}

impl DistinctIndex {
    pub(super) fn key<E>(
        row: &GraphAggregateRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<DistinctKey, E> {
        let key = row.incremental_distinct_key(control)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        Ok(Arc::new(key))
    }

    // Reserve logical tree-search work before every map operation, including
    // variable-sized payload comparisons. This follows group-map accounting,
    // not an allocator-byte claim or an exact CPU-comparison count. Arc keys
    // share payloads with heap entries; reindexing swaps never clones a row.
    fn reserve<E>(
        &mut self,
        key: &DistinctKey,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut units = 1_usize;
        for value in key.keys() {
            control(GlaExecutionEvent::Work)?;
            units = units
                .saturating_add(value.payload_units())
                .saturating_add(1);
        }
        for value in key.values() {
            control(GlaExecutionEvent::Work)?;
            units = units.saturating_add(1);
            if let GraphAggregateValue::Value(value) = value {
                units = units.saturating_add(value.payload_units());
            }
        }
        self.largest_units = self.largest_units.max(units);
        let levels = self.positions.len().saturating_add(1).ilog2() as usize + 1;
        for _ in 0..levels.saturating_mul(24).saturating_mul(self.largest_units) {
            control(GlaExecutionEvent::Work)?;
        }
        Ok(())
    }

    pub(super) fn position<E>(
        &mut self,
        key: &DistinctKey,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<usize>, E> {
        self.reserve(key, control)?;
        Ok(self.positions.get(key).copied())
    }

    pub(super) fn insert<E>(
        &mut self,
        key: DistinctKey,
        at: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        self.reserve(&key, control)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        let prior = self.positions.insert(key, at);
        debug_assert!(
            prior.is_none(),
            "new class must not replace a resident index entry"
        );
        Ok(())
    }

    pub(super) fn remove<E>(
        &mut self,
        key: &DistinctKey,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        self.reserve(key, control)?;
        let removed = self.positions.remove(key);
        debug_assert!(
            removed.is_some(),
            "evicted class must have a resident index entry"
        );
        Ok(())
    }

    pub(super) fn move_to<E>(
        &mut self,
        key: &DistinctKey,
        at: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        self.reserve(key, control)?;
        *self
            .positions
            .get_mut(key)
            .expect("heap swap retains its indexed class") = at;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.positions.len()
    }
}
