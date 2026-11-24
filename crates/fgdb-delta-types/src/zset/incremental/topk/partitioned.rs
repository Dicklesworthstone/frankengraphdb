//! Independent exact occurrence windows over immutable partition keys.
//!
//! Only changed partitions enter the existing weighted TopK transition. Each
//! partition retains its complete input support so later retractions can
//! promote an older row. Preparation stages every touched partition and the
//! combined output before publication; cancellation changes none of them.

use super::{IncrementalTopK, TopKError, TopKPatch, ZSet, ZSetError, ZSetEvent, event};
use crate::{LimbLimit, ZWeight};
use std::collections::{BTreeMap, btree_map::Entry};
use std::ops::Bound;

/// A stable partition projection of an ordered key. Equal keys must have equal
/// partitions, and a retained key's partition must never change. The caller
/// owns payload/comparison costs; the operator admits each projection or clone
/// before invoking it. Partition and row identity are exact, never hash-only.
pub trait PartitionedKey: Ord + Clone {
    type Partition: Ord + Clone;

    fn partition(&self) -> Self::Partition;
}

#[derive(PartialEq, Eq)]
pub struct IncrementalPartitionedTopK<T: PartitionedKey> {
    groups: BTreeMap<T::Partition, IncrementalTopK<T>>,
    rows: ZSet<T>,
    offset: u64,
    count: u64,
}

impl<T: PartitionedKey> IncrementalPartitionedTopK<T> {
    pub fn new(offset: u64, count: u64) -> Self {
        Self {
            groups: BTreeMap::new(),
            rows: ZSet::new(),
            offset,
            count,
        }
    }

    pub fn rows(&self) -> &ZSet<T> {
        &self.rows
    }

    pub fn partition_count(&self) -> usize {
        self.groups.len()
    }

    pub fn weight(&self, partition: &T::Partition, key: &T) -> Option<&ZWeight> {
        self.groups.get(partition)?.counts().weight(key)
    }

    /// Prepare only partitions named by this consolidated tick. Unchanged
    /// input and output support are neither scanned nor copied. Empty final
    /// partitions retire at commit, including under a zero-size window.
    pub fn prepare<E>(
        &mut self,
        changes: &ZSet<T>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<PartitionedTopKUpdate<'_, T>, TopKError<E>> {
        event(control, ZSetEvent::Work)?;
        let mut changes_by_partition = BTreeMap::<T::Partition, ZSet<T>>::new();
        for (key, change) in changes.iter() {
            event(control, ZSetEvent::Work)?;
            event(control, ZSetEvent::ScratchEntry)?;
            let partition = key.partition();
            let group = match changes_by_partition.entry(partition) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    event(control, ZSetEvent::ScratchEntry)?;
                    entry.insert(ZSet::new())
                }
            };
            event(control, ZSetEvent::ScratchEntry)?;
            group.accumulate(
                key.clone(),
                change.checked_clone(limbs).map_err(ZSetError::Arithmetic)?,
                limbs,
                control,
            )?;
        }
        let mut patches = BTreeMap::new();
        let mut delta = ZSet::new();
        for (partition, changes) in changes_by_partition {
            event(control, ZSetEvent::Work)?;
            let patch = match self.groups.get(&partition) {
                Some(group) => group.prepare_patch(&changes, limbs, control)?,
                None => {
                    // Admission for the retained partition entry, before a
                    // successful commit can install it in the live directory.
                    event(control, ZSetEvent::ScratchEntry)?;
                    IncrementalTopK::new(self.offset, self.count)
                        .prepare_patch(&changes, limbs, control)?
                }
            };
            for (key, change) in patch.delta.iter() {
                event(control, ZSetEvent::Work)?;
                event(control, ZSetEvent::ScratchEntry)?;
                delta.accumulate(
                    key.clone(),
                    change.checked_clone(limbs).map_err(ZSetError::Arithmetic)?,
                    limbs,
                    control,
                )?;
            }
            event(control, ZSetEvent::ScratchEntry)?;
            patches.insert(partition, patch);
        }
        let replacements = self.rows.prepare_integration(&delta, limbs, control)?;
        let mut support = self.rows.len();
        for (key, weight) in &replacements {
            event(control, ZSetEvent::Work)?;
            if weight < &ZWeight::ZERO {
                return Err(TopKError::NegativeMultiplicity);
            }
            match (self.rows.weight(key).is_some(), weight.is_zero()) {
                (true, true) => support -= 1,
                (false, false) => support += 1,
                _ => {}
            }
        }
        event(control, ZSetEvent::Work)?;
        Ok(PartitionedTopKUpdate {
            owner: self,
            patches,
            replacements,
            support,
            delta,
        })
    }

    pub fn apply<E>(
        &mut self,
        changes: &ZSet<T>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<T>, TopKError<E>> {
        Ok(self.prepare(changes, limbs, control)?.commit())
    }
}

impl<T: PartitionedKey> core::fmt::Debug for IncrementalPartitionedTopK<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IncrementalPartitionedTopK")
            .field("partitions", &self.groups.len())
            .field("output_support", &self.rows.len())
            .field("offset", &self.offset)
            .field("count", &self.count)
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[must_use = "dropping a partitioned window update preserves every partition"]
pub struct PartitionedTopKUpdate<'a, T: PartitionedKey> {
    owner: &'a mut IncrementalPartitionedTopK<T>,
    patches: BTreeMap<T::Partition, TopKPatch<T>>,
    replacements: BTreeMap<T, ZWeight>,
    support: usize,
    delta: ZSet<T>,
}

impl<T: PartitionedKey> PartitionedTopKUpdate<'_, T> {
    pub fn delta(&self) -> &ZSet<T> {
        &self.delta
    }

    /// Borrow tentative output without constructing a copy of unchanged
    /// partitions. Enumeration is lazy and uses ordered range lookups; merely
    /// preparing or dropping a tick never enumerates these rows.
    pub fn rows(&self) -> impl DoubleEndedIterator<Item = (&T, &ZWeight)> + ExactSizeIterator {
        MergedRows {
            old: &self.owner.rows.entries,
            replacements: &self.replacements,
            front: None,
            back: None,
            remaining: self.support,
        }
    }

    pub fn commit(self) -> ZSet<T> {
        let Self {
            owner,
            patches,
            replacements,
            delta,
            ..
        } = self;
        for (partition, patch) in patches {
            match owner.groups.entry(partition) {
                Entry::Occupied(mut entry) => {
                    let group = entry.get_mut();
                    group.counts.publish(patch.replacements);
                    group.rows = patch.rows;
                    if group.counts.is_empty() {
                        entry.remove();
                    }
                }
                Entry::Vacant(entry) => {
                    let mut group = IncrementalTopK::new(owner.offset, owner.count);
                    group.counts.publish(patch.replacements);
                    group.rows = patch.rows;
                    if !group.counts.is_empty() {
                        entry.insert(group);
                    }
                }
            }
        }
        owner.rows.publish(replacements);
        delta
    }
}

impl<T: PartitionedKey> core::fmt::Debug for PartitionedTopKUpdate<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PartitionedTopKUpdate")
            .field("changed_partitions", &self.patches.len())
            .field("delta_support", &self.delta.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

struct MergedRows<'a, T: Ord> {
    old: &'a BTreeMap<T, ZWeight>,
    replacements: &'a BTreeMap<T, ZWeight>,
    front: Option<&'a T>,
    back: Option<&'a T>,
    remaining: usize,
}

impl<'a, T: Ord> MergedRows<'a, T> {
    fn take(&mut self, reverse: bool) -> Option<(&'a T, &'a ZWeight)> {
        if self.remaining == 0 {
            return None;
        }
        loop {
            let bounds = (
                self.front.map_or(Bound::Unbounded, Bound::Excluded),
                self.back.map_or(Bound::Unbounded, Bound::Excluded),
            );
            let mut old = self.old.range(bounds);
            let mut changed = self.replacements.range(bounds);
            let (old, changed) = if reverse {
                (old.next_back(), changed.next_back())
            } else {
                (old.next(), changed.next())
            };
            let next = match (old, changed) {
                (Some(left), Some(right)) => {
                    let order = left.0.cmp(right.0);
                    if order.is_eq() || (order.is_lt() == reverse) {
                        right
                    } else {
                        left
                    }
                }
                (Some(row), None) | (None, Some(row)) => row,
                (None, None) => return None,
            };
            if reverse {
                self.back = Some(next.0);
            } else {
                self.front = Some(next.0);
            }
            if !next.1.is_zero() {
                self.remaining -= 1;
                return Some(next);
            }
        }
    }
}

impl<'a, T: Ord> Iterator for MergedRows<'a, T> {
    type Item = (&'a T, &'a ZWeight);

    fn next(&mut self) -> Option<Self::Item> {
        self.take(false)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<T: Ord> DoubleEndedIterator for MergedRows<'_, T> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.take(true)
    }
}

impl<T: Ord> ExactSizeIterator for MergedRows<'_, T> {}

#[cfg(test)]
mod tests;
