//! Terminal DISTINCT and ALL collectors for the one binding-row evaluator.
//!
//! A finite logical page retains only its smallest offset + count rows, under
//! the existing canonical order. This is terminal selection, not early query
//! termination: every candidate's fallible predicates/properties still execute.
//! ALL retains occurrences rather than values; private reusable slot IDs keep
//! equal rows distinct without an ever-growing occurrence counter.

mod ranked;

use super::{GlaExecutionEvent, GlaOperator};
use crate::algebra::GraphValueOrder;
use ranked::Ranked;
use std::borrow::Borrow;
use std::collections::BTreeSet;
use std::sync::Arc;

/// Private construction selects the logical terminal projection's semantics.
/// Public visibility is required by the public-but-sealed projection trait;
/// this type is not exported from the crate's public surface.
pub struct ProjectedRows<Row> {
    storage: Storage<Row>,
    capacity: Option<usize>,
}

enum Storage<Row> {
    Distinct(BTreeSet<Row>),
    All(BTreeSet<(Row, usize)>),
    Ranked {
        rows: BTreeSet<Ranked<Row>>,
        order: Arc<[GraphValueOrder]>,
        distinct: bool,
    },
}

impl<Row: Ord> ProjectedRows<Row> {
    pub(crate) fn new(distinct: bool) -> Self {
        Self {
            storage: if distinct {
                Storage::Distinct(BTreeSet::new())
            } else {
                Storage::All(BTreeSet::new())
            },
            capacity: None,
        }
    }

    /// Fuse only the compiler-owned terminal pagination into collection. An
    /// unrepresentable bound must never wrap to a smaller page. LIMIT 0 needs
    /// no retained rows, even with an enormous offset, but does not skip reads.
    pub(crate) fn for_plan(distinct: bool, operators: &[GlaOperator]) -> Self {
        let mut rows = Self::new(distinct);
        rows.capacity = match operators.last() {
            Some(GlaOperator::Limit { count: Some(0), .. }) => Some(0),
            Some(GlaOperator::Limit { offset, count: Some(count) }) => offset
                .checked_add(*count)
                .and_then(|bound| usize::try_from(bound).ok()),
            _ => None,
        };
        if let Some(GlaOperator::OrderByValueColumns { columns }) = operators.iter().rev().nth(1) {
            rows.storage = Storage::Ranked {
                rows: BTreeSet::new(),
                order: Arc::clone(columns),
                distinct,
            };
        }
        rows
    }

    /// Called AFTER reading the complete borrowed candidate, BEFORE cloning
    /// owned cells. A previously discarded DISTINCT value never needs a global
    /// seen set: once full, the largest retained value can only decrease.
    /// Extra equal ALL occurrences cannot improve an already full prefix.
    ///
    /// Reserve the cutoff decision and any eviction before row construction.
    /// If construction later refuses, the previous collector stays unchanged.
    /// Scratch remains cumulative admission work, not a peak-memory counter;
    /// replacing an entry never refunds the already charged payload units.
    pub(crate) fn should_retain<Key: Ord + ?Sized, E>(
        &self,
        key: &Key,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<bool, E>
    where
        Row: Borrow<Key>,
    {
        if self.contains(key) {
            return Ok(false);
        }
        let Some(capacity) = self.capacity else {
            return Ok(true);
        };
        control(GlaExecutionEvent::Work)?;
        if capacity == 0 {
            return Ok(false);
        }
        if self.len() < capacity {
            return Ok(true);
        }
        let largest = match &self.storage {
            Storage::Distinct(rows) => rows.last(),
            Storage::All(rows) => rows.last().map(|(row, _)| row),
            Storage::Ranked { .. } => unreachable!("ordered value rows use their borrowed ranked key"),
        }.expect("a nonzero full prefix has a largest row");
        let largest: &Key = largest.borrow();
        if largest <= key {
            return Ok(false);
        }
        control(GlaExecutionEvent::Work)?;
        Ok(true)
    }

    /// A duplicate can be elided only when the logical plan contains DISTINCT.
    /// The tuple collector can therefore keep its borrowed stack-key lookup.
    pub(crate) fn contains<Key: Ord + ?Sized>(&self, key: &Key) -> bool
    where
        Row: Borrow<Key>,
    {
        match &self.storage {
            Storage::Distinct(rows) => rows.contains(key),
            Storage::All(_) => false,
            Storage::Ranked { .. } => unreachable!("ordered value rows use their borrowed ranked key"),
        }
    }

    /// Private callers first admit the borrowed candidate with should_retain,
    /// then charge/build every owned cell. Only complete rows reach insertion.
    /// At most capacity retained rows plus that one staged candidate coexist.
    pub(crate) fn insert(&mut self, row: Row) {
        let full = self.capacity.is_some_and(|capacity| self.len() == capacity);
        debug_assert_ne!(self.capacity, Some(0), "a zero-size page admits no row");
        match &mut self.storage {
            Storage::Distinct(rows) => {
                if full {
                    debug_assert!(rows.last().is_some_and(|largest| largest > &row));
                    debug_assert!(!rows.contains(&row));
                    let _ = rows.pop_last();
                }
                rows.insert(row);
            }
            Storage::All(rows) => {
                // Every retained slot ID is unique. Growth assigns the next
                // unused ID; replacement reuses exactly the evicted ID. Thus
                // repeated equal rows never collide after arbitrary evictions,
                // and even a long input needs no monotonically growing ordinal.
                let ordinal = if full {
                    let (largest, slot) = rows.pop_last().expect("full nonempty ALL prefix");
                    debug_assert!(largest > row);
                    slot
                } else {
                    rows.len()
                };
                let inserted = rows.insert((row, ordinal));
                debug_assert!(inserted, "each retained occurrence has a unique live slot");
            }
            Storage::Ranked { .. } => unreachable!("ordered value rows use their checked ranked insertion"),
        }
    }

    pub(crate) fn len(&self) -> usize {
        match &self.storage {
            Storage::Distinct(rows) => rows.len(),
            Storage::All(rows) => rows.len(),
            Storage::Ranked { rows, .. } => rows.len(),
        }
    }

    /// The modes are exclusive, so this chain is one already sorted stream.
    /// Equal rows remain equal after their private ordinals are removed. Every
    /// row moves to the evaluator's existing pagination/output guard unchanged.
    pub(crate) fn into_rows(self) -> impl Iterator<Item = Row> {
        let (distinct, all, ranked) = match self.storage {
            Storage::Distinct(rows) => (Some(rows), None, None),
            Storage::All(rows) => (None, Some(rows), None),
            Storage::Ranked { rows, .. } => (None, None, Some(rows)),
        };
        distinct
            .into_iter()
            .flatten()
            .chain(all.into_iter().flatten().map(|(row, _)| row))
            .chain(ranked.into_iter().flatten().map(|entry| entry.row))
    }

    #[cfg(test)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Row> {
        let (distinct, all, ranked) = match &self.storage {
            Storage::Distinct(rows) => (Some(rows), None, None),
            Storage::All(rows) => (None, Some(rows), None),
            Storage::Ranked { rows, .. } => (None, None, Some(rows)),
        };
        distinct
            .into_iter()
            .flatten()
            .chain(all.into_iter().flatten().map(|(row, _)| row))
            .chain(ranked.into_iter().flatten().map(|entry| &entry.row))
    }

    #[cfg(test)]
    pub(crate) fn first(&self) -> Option<&Row> {
        self.iter().next()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<Row: Ord> core::fmt::Debug for ProjectedRows<Row> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ProjectedRows")
            .field("row_count", &self.len())
            .field("values", &"[REDACTED]")
            .finish()
    }
}

impl super::GlaExecutionStats {
    /// The aggregate operator shares the evaluator's transactional, checked
    /// meter instead of reimplementing its limit or overflow arithmetic.
    pub(crate) fn charge_event(
        &mut self,
        limits: super::GlaExecutionLimits,
        event: super::GlaExecutionEvent,
    ) -> Result<(), super::GlaLimitExceeded> {
        super::charge(self, limits, event)
    }
}

impl crate::algebra::GlaPlan<crate::algebra::GraphValueRow> {
    /// Internal GroupAggregate input seam. The caller owns a checked ALL,
    /// unpaginated child. Complete bindings feed its accumulator directly;
    /// the ordinary projection collector is deliberately left empty. Root
    /// scans, expansion, predicates, caches and cancellation remain the SAME
    /// evaluator, and no public callback can replace query semantics.
    /// Binding selections and aggregate arguments share one property resolver.
    pub(crate) fn visit_value_bindings<'a, E, F, C, P, R>(
        &self,
        vertices: impl IntoIterator<Item = fgdb_types::VId>,
        edges: impl IntoIterator<
            Item = (
                fgdb_types::VId,
                fgdb_delta_types::RelationId,
                fgdb_types::VId,
            ),
        >,
        test_vertex: F,
        mut property: R,
        control: C,
        mut visit: P,
    ) -> Result<(), E>
    where
        F: FnMut(fgdb_types::VId, &[crate::algebra::VertexPredicate]) -> Result<bool, E>,
        R: FnMut(
            fgdb_types::VId,
            fgdb_delta_types::PropertyKeyId,
        ) -> Result<Option<&'a fgdb_types::CanonicalScalar>, E>,
        C: FnMut(super::GlaExecutionEvent) -> Result<(), E>,
        P: FnMut(
            &[crate::algebra::ValueProjection],
            &[Option<fgdb_types::VId>],
            &mut R,
            &mut C,
        ) -> Result<(), E>,
    {
        let unused = self.execute_projected(
            vertices,
            edges,
            test_vertex,
            control,
            |operator, bindings, _projected, control| {
                if matches!(
                    operator,
                    crate::algebra::GlaOperator::CompareProperties { .. }
                ) {
                    return super::compare_properties(operator, bindings, &mut property, control);
                }
                let crate::algebra::GlaOperator::ProjectValues { columns } = operator else {
                    unreachable!("the checked aggregate child has value projection")
                };
                visit(columns, bindings, &mut property, control)?;
                Ok(false)
            },
        )?;
        debug_assert!(
            unused.is_empty(),
            "aggregation never materializes its child bag"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_types::VId;

    fn page<Row: Ord>(distinct: bool, offset: u64, count: Option<u64>) -> ProjectedRows<Row> {
        ProjectedRows::for_plan(distinct, &[GlaOperator::Limit { offset, count }])
    }

    #[test]
    fn every_bounded_prefix_matches_full_sort_and_distinct_before_pagination() {
        let choices = [VId(0), VId(1_u128 << 100), VId(u128::MAX)];
        for len in 0..=6_u32 {
            for mut encoded in 0..3_usize.pow(len) {
                let mut input = Vec::new();
                for _ in 0..len {
                    input.push(choices[encoded % 3]);
                    encoded /= 3;
                }
                for distinct in [false, true] {
                    for offset in 0..=3_u64 {
                        for count in 0..=3_u64 {
                            let mut rows = page(distinct, offset, Some(count));
                            let capacity = if count == 0 { 0 } else { (offset + count) as usize };
                            let mut seen = Vec::new();
                            for &value in &input {
                                seen.push(value);
                                if rows.should_retain(&value, &mut |_| Ok::<_, ()>(())).unwrap() {
                                    rows.insert(value);
                                }
                                let mut expected = seen.clone();
                                expected.sort_unstable();
                                if distinct { expected.dedup(); }
                                expected.truncate(capacity);
                                assert_eq!(rows.iter().copied().collect::<Vec<_>>(), expected);
                                assert!(rows.len() <= capacity);
                            }
                            let mut expected = input.clone();
                            expected.sort_unstable();
                            if distinct { expected.dedup(); }
                            assert_eq!(rows.into_rows().skip(offset as usize).take(count as usize).collect::<Vec<_>>(),
                                expected.into_iter().skip(offset as usize).take(count as usize).collect::<Vec<_>>());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn all_slots_are_unique_when_equal_rows_replace_unequal_cutoffs() {
        // Evicted IDs are not necessarily rows.len() - 1. Reusing len() would
        // merge two equal occurrences when their slot IDs collide.
        let mut rows = page(false, 0, Some(3));
        for value in [8, 10, 9, 7, 7, 6, 6, 6, 5, 6] {
            if rows.should_retain(&value, &mut |_| Ok::<_, ()>(())).unwrap() {
                rows.insert(value);
            }
            let Storage::All(set) = &rows.storage else { unreachable!() };
            let slots: BTreeSet<_> = set.iter().map(|(_, slot)| *slot).collect();
            assert_eq!(slots.len(), set.len());
            assert!(slots.iter().all(|slot| *slot < 3));
        }
        assert_eq!(rows.into_rows().collect::<Vec<_>>(), vec![5, 6, 6]);
    }

    #[test]
    fn borrowed_cutoff_refusal_does_not_mutate_the_collector() {
        let mut rows = page::<String>(false, 0, Some(2));
        for value in ["z", "y"] {
            assert!(rows.should_retain(value, &mut |_| Ok::<_, ()>(())).unwrap());
            rows.insert(value.to_owned());
        }
        for stop in 1..=2 {
            let mut calls = 0;
            let result = rows.should_retain("a", &mut |_| {
                calls += 1;
                if calls == stop { Err(stop) } else { Ok(()) }
            });
            assert_eq!(result, Err(stop));
            assert_eq!(calls, stop);
            assert_eq!(rows.iter().map(String::as_str).collect::<Vec<_>>(), vec!["y", "z"]);
        }
        assert!(!rows.should_retain("z", &mut |_| Ok::<_, ()>(())).unwrap());
        assert!(!rows.should_retain("zz", &mut |_| Ok::<_, ()>(())).unwrap());
        assert!(rows.should_retain("a", &mut |_| Ok::<_, ()>(())).unwrap());
        rows.insert("a".into());
        assert_eq!(rows.into_rows().collect::<Vec<_>>(), vec!["a", "y"]);
    }

    #[test]
    fn finite_capacity_arithmetic_never_wraps_and_zero_needs_no_prefix() {
        assert_eq!(page::<u8>(false, u64::MAX, Some(0)).capacity, Some(0));
        assert_eq!(page::<u8>(true, u64::MAX, Some(1)).capacity, None);
        assert_eq!(page::<u8>(false, 1, Some(u64::MAX)).capacity, None);
        assert_eq!(page::<u8>(true, 7, None).capacity, None);
        assert_eq!(page::<u8>(true, 7, Some(3)).capacity, Some(10));
        assert_eq!(page::<u8>(false, 0, Some(u64::MAX)).capacity,
            usize::try_from(u64::MAX).ok());
        let rows = page::<u8>(false, u64::MAX, Some(0));
        assert!(!rows.should_retain(&1, &mut |_| Ok::<_, ()>(())).unwrap());
        assert!(rows.is_empty());
    }

    #[test]
    fn all_retains_each_occurrence_and_distinct_retains_each_value() {
        let input = [VId(3), VId(1), VId(3), VId(2), VId(1)];
        let mut all = ProjectedRows::new(false);
        let mut distinct = ProjectedRows::new(true);
        for value in input {
            assert!(!all.contains(&value));
            all.insert(value);
            if !distinct.contains(&value) {
                distinct.insert(value);
            }
        }
        assert_eq!(all.len(), 5);
        assert_eq!(distinct.len(), 3);
        assert!(!format!("{all:?}").contains("VId"));
        assert_eq!(
            all.into_rows().collect::<Vec<_>>(),
            vec![VId(1), VId(1), VId(2), VId(3), VId(3)]
        );
        assert_eq!(
            distinct.into_rows().collect::<Vec<_>>(),
            vec![VId(1), VId(2), VId(3)]
        );
    }

    #[test]
    fn every_small_bag_matches_independent_sort_and_preserves_occurrence_pagination() {
        for len in 0..=7_u32 {
            for mut encoded in 0..3_usize.pow(len) {
                let mut expected = Vec::new();
                let mut rows = ProjectedRows::new(false);
                for _ in 0..len {
                    let value = [VId(0), VId(1_u128 << 100), VId(u128::MAX)][encoded % 3];
                    encoded /= 3;
                    expected.push(value);
                    rows.insert(value);
                }
                expected.sort_unstable();
                assert_eq!(rows.into_rows().collect::<Vec<_>>(), expected);
            }
        }
    }
}
