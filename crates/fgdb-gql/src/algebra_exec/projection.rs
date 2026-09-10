//! Terminal DISTINCT and ALL collectors for the one binding-row evaluator.
//!
//! ALL retains each actual occurrence. Its ordinal disambiguates equal keys
//! inside a B-tree; it is neither a public column nor a multiplicity estimate.

use std::borrow::Borrow;
use std::collections::BTreeSet;

/// Private construction selects the logical terminal projection's semantics.
/// Public visibility is required by the public-but-sealed projection trait;
/// this type is not exported from the crate's public surface.
pub struct ProjectedRows<Row> {
    storage: Storage<Row>,
}

enum Storage<Row> {
    Distinct(BTreeSet<Row>),
    All(BTreeSet<(Row, usize)>),
}

impl<Row: Ord> ProjectedRows<Row> {
    pub(crate) fn new(distinct: bool) -> Self {
        Self {
            storage: if distinct {
                Storage::Distinct(BTreeSet::new())
            } else {
                Storage::All(BTreeSet::new())
            },
        }
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
        }
    }

    /// Callers charge the entry and its owned cells before reaching this method.
    pub(crate) fn insert(&mut self, row: Row) {
        match &mut self.storage {
            Storage::Distinct(rows) => {
                rows.insert(row);
            }
            Storage::All(rows) => {
                // No entry is removed during collection. len() is a unique
                // occurrence ordinal and counts physically retained entries,
                // not products of input cardinalities that could overflow.
                let ordinal = rows.len();
                let inserted = rows.insert((row, ordinal));
                debug_assert!(inserted, "each retained occurrence has a fresh ordinal");
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        match &self.storage {
            Storage::Distinct(rows) => rows.len(),
            Storage::All(rows) => rows.len(),
        }
    }

    /// The modes are exclusive, so this chain is one already sorted stream.
    /// Equal rows remain equal after their private ordinals are removed. Every
    /// row moves to the evaluator's existing pagination/output guard unchanged.
    pub(crate) fn into_rows(self) -> impl Iterator<Item = Row> {
        let (distinct, all) = match self.storage {
            Storage::Distinct(rows) => (Some(rows), None),
            Storage::All(rows) => (None, Some(rows)),
        };
        distinct
            .into_iter()
            .flatten()
            .chain(all.into_iter().flatten().map(|(row, _)| row))
    }

    #[cfg(test)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Row> {
        let (distinct, all) = match &self.storage {
            Storage::Distinct(rows) => (Some(rows), None),
            Storage::All(rows) => (None, Some(rows)),
        };
        distinct
            .into_iter()
            .flatten()
            .chain(all.into_iter().flatten().map(|(row, _)| row))
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
        edges: impl IntoIterator<Item = (fgdb_types::VId, fgdb_delta_types::RelationId, fgdb_types::VId)>,
        test_vertex: F,
        mut property: R,
        control: C,
        mut visit: P,
    ) -> Result<(), E>
    where
        F: FnMut(fgdb_types::VId, &[crate::algebra::VertexPredicate]) -> Result<bool, E>,
        R: FnMut(fgdb_types::VId, fgdb_delta_types::PropertyKeyId) -> Result<Option<&'a fgdb_types::CanonicalScalar>, E>,
        C: FnMut(super::GlaExecutionEvent) -> Result<(), E>,
        P: FnMut(&[crate::algebra::ValueProjection], &[Option<fgdb_types::VId>], &mut R, &mut C) -> Result<(), E>,
    {
        let unused = self.execute_projected(
            vertices,
            edges,
            test_vertex,
            control,
            |operator, bindings, _projected, control| {
                if matches!(operator, crate::algebra::GlaOperator::CompareProperties { .. }) {
                    return super::compare_properties(operator, bindings, &mut property, control);
                }
                let crate::algebra::GlaOperator::ProjectValues { columns } = operator else {
                    unreachable!("the checked aggregate child has value projection")
                };
                visit(columns, bindings, &mut property, control)?;
                Ok(false)
            },
        )?;
        debug_assert!(unused.is_empty(), "aggregation never materializes its child bag");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_types::VId;

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
