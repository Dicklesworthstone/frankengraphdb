//! Output-equivalence classes before the existing streamed top-K selector.
//!
//! Argument DISTINCT belongs to the numeric cells upstream. Here two COMPLETE
//! groups may project to one output tuple. Retain its first ranked group, not
//! its first input occurrence; a later group can improve the representative.

use super::*;
use std::collections::{BTreeMap, btree_map::Entry};

pub(super) struct Classes {
    values: BTreeMap<GraphAggregateRow, RankedGroup>,
    largest_key_units: usize,
}
impl Classes {
    pub(super) fn new() -> Self {
        Self { values: BTreeMap::new(), largest_key_units: 0 }
    }

    pub(super) fn insert<E>(
        &mut self,
        query: &PreparedGraphAggregate,
        candidate: RankedGroup,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        let output = candidate.projected.as_ref().unwrap_or(&candidate.complete);
        // Reuse the maintained/batch DISTINCT law: exact top-level numeric
        // cells normalize to rational equality, while nested/native values
        // retain canonical identity. Never replace equality with a digest.
        let key = output.incremental_distinct_key(control)?;
        let mut units = 1_usize;
        for value in &key.keys {
            control(GlaExecutionEvent::Work)?;
            units = units.saturating_add(1).saturating_add(value_ref(value).payload_units());
        }
        for value in &key.values {
            control(GlaExecutionEvent::Work)?;
            units = units.saturating_add(1).saturating_add(result_cell(value).payload_units());
        }
        self.largest_key_units = self.largest_key_units.max(units);
        // Logical B-tree comparison reservation, including variable payloads.
        // This is not an exact comparison count or an allocator-byte bound.
        let levels = self.values.len().saturating_add(1).ilog2() as usize + 1;
        for _ in 0..levels.saturating_mul(24).saturating_mul(self.largest_key_units) {
            control(GlaExecutionEvent::Work)?;
        }
        match self.values.entry(key) {
            Entry::Vacant(entry) => {
                control(GlaExecutionEvent::ScratchEntry)?;
                entry.insert(candidate);
            }
            Entry::Occupied(mut entry) => {
                if StreamedGroupRanking::compare(query, &candidate, entry.get(), control)? == Ordering::Less {
                    control(GlaExecutionEvent::Work)?;
                    entry.insert(candidate);
                }
            }
        }
        Ok(())
    }

    pub(super) fn into_candidates(self) -> impl Iterator<Item = RankedGroup> {
        self.values.into_values()
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize { self.values.len() }
}
