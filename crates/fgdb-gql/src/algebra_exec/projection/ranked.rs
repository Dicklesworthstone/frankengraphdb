//! Bounded ordered value-row collection with heterogeneous borrowed lookup.
//!
//! The tree owns each retained row exactly once. Search keys borrow the full
//! candidate; ordering metadata is shared, and never copied property payloads.
//! DISTINCT compares the complete row, ALL adds a reusable private slot ID.

use super::{GlaExecutionEvent, ProjectedRows, Storage};
use crate::algebra::{GraphValueOrder, GraphValueRow, RowKey, ValueRef};
use fgdb_types::CanonicalScalar;
use std::borrow::Borrow;
use std::cmp::Ordering;
use std::sync::Arc;

pub(super) struct Ranked<Row> {
    pub(super) row: Row,
    pub(super) slot: usize,
    order: Arc<[GraphValueOrder]>,
    view: fn(&Row) -> &dyn RowKey,
}

trait RankedKey: RowKey {
    fn order(&self) -> &[GraphValueOrder];
    fn slot(&self) -> usize;
}

impl<Row> RowKey for Ranked<Row> {
    fn width(&self) -> usize { (self.view)(&self.row).width() }
    fn cell(&self, at: usize) -> ValueRef<'_> { (self.view)(&self.row).cell(at) }
}
impl<Row> RankedKey for Ranked<Row> {
    fn order(&self) -> &[GraphValueOrder] { &self.order }
    fn slot(&self) -> usize { self.slot }
}

struct Lookup<'a> {
    key: &'a dyn RowKey,
    order: &'a [GraphValueOrder],
}
impl RowKey for Lookup<'_> {
    fn width(&self) -> usize { self.key.width() }
    fn cell(&self, at: usize) -> ValueRef<'_> { self.key.cell(at) }
}
impl RankedKey for Lookup<'_> {
    fn order(&self) -> &[GraphValueOrder] { self.order }
    fn slot(&self) -> usize { 0 }
}

fn is_null(value: ValueRef<'_>) -> bool {
    matches!(value, ValueRef::Scalar(CanonicalScalar::Null))
}

fn compare_rows(left: &dyn RankedKey, right: &dyn RankedKey) -> Ordering {
    // This also gives the private key type a total order across different
    // definitions. Every tree and its borrowed searches use one definition.
    let definition = left.order().cmp(right.order());
    if definition != Ordering::Equal { return definition; }
    for column in left.order() {
        let a = left.cell(column.column);
        let b = right.cell(column.column);
        let null_order = match (is_null(a), is_null(b)) {
            (true, false) => Some(if column.nulls_first { Ordering::Less } else { Ordering::Greater }),
            (false, true) => Some(if column.nulls_first { Ordering::Greater } else { Ordering::Less }),
            _ => None,
        };
        // NULL placement does not reverse when direction reverses.
        let order = null_order.unwrap_or_else(|| {
            let order = a.cmp(&b);
            if column.descending { order.reverse() } else { order }
        });
        if order != Ordering::Equal { return order; }
    }
    // A total deterministic tie-break and whole-row DISTINCT, not DISTINCT
    // over only the ORDER BY keys. Equal keys with different payloads survive.
    for at in 0..left.width().min(right.width()) {
        let order = left.cell(at).cmp(&right.cell(at));
        if order != Ordering::Equal { return order; }
    }
    left.width().cmp(&right.width())
}

fn compare(left: &dyn RankedKey, right: &dyn RankedKey) -> Ordering {
    compare_rows(left, right).then_with(|| left.slot().cmp(&right.slot()))
}

impl PartialEq for dyn RankedKey + '_ {
    fn eq(&self, other: &Self) -> bool { compare(self, other) == Ordering::Equal }
}
impl Eq for dyn RankedKey + '_ {}
impl PartialOrd for dyn RankedKey + '_ {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}
impl Ord for dyn RankedKey + '_ {
    fn cmp(&self, other: &Self) -> Ordering { compare(self, other) }
}
impl<Row> PartialEq for Ranked<Row> {
    fn eq(&self, other: &Self) -> bool { compare(self, other) == Ordering::Equal }
}
impl<Row> Eq for Ranked<Row> {}
impl<Row> PartialOrd for Ranked<Row> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}
impl<Row> Ord for Ranked<Row> {
    fn cmp(&self, other: &Self) -> Ordering { compare(self, other) }
}
impl<'a, Row: 'a> Borrow<dyn RankedKey + 'a> for Ranked<Row> {
    fn borrow(&self) -> &(dyn RankedKey + 'a) { self }
}

impl ProjectedRows<GraphValueRow> {
    pub(crate) fn should_retain_value<E>(
        &self,
        key: &dyn RowKey,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<bool, E> {
        let Storage::Ranked { rows, order, distinct } = &self.storage else {
            return self.should_retain(key, control);
        };
        let lookup = Lookup { key, order };
        // As with other B-tree accesses, these are logical operation charges,
        // not a claim that every internal library comparison is preemptible.
        control(GlaExecutionEvent::Work)?;
        if *distinct && rows.contains(&lookup as &dyn RankedKey) { return Ok(false); }
        let Some(capacity) = self.capacity else { return Ok(true); };
        control(GlaExecutionEvent::Work)?;
        if capacity == 0 { return Ok(false); }
        if rows.len() < capacity { return Ok(true); }
        let largest = rows.last().expect("a full nonzero prefix has a largest row");
        if compare_rows(largest, &lookup) != Ordering::Greater { return Ok(false); }
        // Reserve eviction before cloning a replacement. If a later payload
        // copy refuses, the original collector remains completely unchanged.
        control(GlaExecutionEvent::Work)?;
        Ok(true)
    }

    pub(crate) fn insert_value(&mut self, row: GraphValueRow) {
        let Storage::Ranked { rows, order, distinct } = &mut self.storage else {
            self.insert(row);
            return;
        };
        let full = self.capacity.is_some_and(|capacity| rows.len() == capacity);
        debug_assert_ne!(self.capacity, Some(0));
        let slot = if full {
            rows.pop_last().expect("a full nonzero prefix has a largest row").slot
        } else if *distinct { 0 } else { rows.len() };
        let inserted = rows.insert(Ranked {
            row, slot, order: Arc::clone(order), view: |row| row,
        });
        debug_assert!(inserted, "complete admitted rows have unique DISTINCT keys or live ALL slots");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algebra::{GraphColumn, GraphPatternBuilder, GlaOperator};
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_types::VId;

    fn fixture() -> Vec<GraphValueRow> {
        let mut b = GraphPatternBuilder::new(); b.vertex("n").unwrap();
        let p = b.prepare_values(&[GraphColumn::vertex("id", "n"),
            GraphColumn::property("rank", "n", PropertyKeyId(1))], 0, None).unwrap();
        let scores = [CanonicalScalar::Int(3), CanonicalScalar::Int(1),
            CanonicalScalar::Int(3), CanonicalScalar::Null, CanonicalScalar::Int(2)];
        p.plan().execute_with_properties_control((0..5).map(VId), [],
            |_, _| Ok::<_, ()>(true), |id, _| Ok(Some(&scores[id.0 as usize])), |_| Ok(())).unwrap()
    }

    #[test]
    fn private_ranked_prefix_matches_full_sort_with_unique_live_occurrence_slots() {
        let input = fixture();
        let order: Arc<[GraphValueOrder]> = Arc::from([GraphValueOrder::descending(1)]);
        for distinct in [false, true] { for capacity in 0..=4 {
            let mut rows = ProjectedRows::<GraphValueRow>::for_plan(distinct,
                &[GlaOperator::OrderByValueColumns { columns: Arc::clone(&order) },
                    GlaOperator::Limit { offset: 0, count: Some(capacity) }]);
            let mut expected = Vec::new();
            for at in [3,1,4,4,0,0,2,2,1,0,4,2] {
                let row = &input[at];
                expected.push(row.clone());
                if rows.should_retain_value(row, &mut |_| Ok::<_, ()>(())).unwrap() {
                    rows.insert_value(row.clone());
                }
                expected.sort_by(|a,b| compare_rows(&Lookup { key:a,order:&order }, &Lookup { key:b,order:&order }));
                if distinct { expected.dedup(); }
                expected.truncate(capacity as usize);
                assert_eq!(rows.iter().cloned().collect::<Vec<_>>(), expected);
                assert!(rows.len() <= capacity as usize);
                let Storage::Ranked { rows: live, .. } = &rows.storage else { unreachable!() };
                assert!(live.iter().all(|entry| Arc::ptr_eq(&entry.order,&order)));
                if !distinct {
                    let ids: std::collections::BTreeSet<_> = live.iter().map(|entry|entry.slot).collect();
                    assert_eq!(ids.len(), live.len());
                    assert!(ids.iter().all(|slot| *slot < capacity as usize));
                }
            }
        }}
    }

    #[test]
    fn private_ordered_cutoff_is_transactional_at_every_decision_checkpoint() {
        let input = fixture();
        let order: Arc<[GraphValueOrder]> = Arc::from([GraphValueOrder::descending(1)]);
        let mut rows = ProjectedRows::<GraphValueRow>::for_plan(false,
            &[GlaOperator::OrderByValueColumns { columns:order }, GlaOperator::Limit { offset:0,count:Some(1) }]);
        assert!(rows.should_retain_value(&input[1],&mut |_|Ok::<_,usize>(())).unwrap());
        rows.insert_value(input[1].clone());
        let mut total = 0;
        assert!(rows.should_retain_value(&input[0],&mut |_| {total+=1;Ok::<_,usize>(())}).unwrap());
        for stop in 1..=total {
            let mut calls = 0;
            let result = rows.should_retain_value(&input[0],&mut |_| {
                calls+=1; if calls==stop {Err(stop)} else {Ok(())}
            });
            assert_eq!(result,Err(stop));
            assert_eq!(calls,stop);
            assert_eq!(rows.first(),Some(&input[1]));
        }
        assert!(!rows.should_retain_value(&input[3],&mut |_|Ok::<_,usize>(())).unwrap());
    }
}
