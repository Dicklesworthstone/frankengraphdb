//! Ordered input support for deletion-safe native collection aggregates.
//!
//! A value's first occurrence is determined by the complete INPUT row order,
//! not by value order or arrival time. Retain one counted tuple index shared
//! by every COLLECT in the definition. Prepare copies only touched groups'
//! index nodes (rows are Arc-shared); no accepted state changes until commit.
//! Logical entry/payload accounting follows the ordinary row-window contract,
//! not allocator-byte, comparison-count or spill guarantees.

use super::*;
use crate::algebra::GraphValueOrder;
use std::cmp::Ordering;
use std::collections::BTreeMap;

#[derive(Clone)]
struct OrderedRow {
    row: Arc<GraphValueRow>,
    order: Arc<[GraphValueOrder]>,
}
impl Ord for OrderedRow {
    fn cmp(&self, other: &Self) -> Ordering {
        self.order.cmp(&other.order).then_with(|| {
            self.row
                .compare_incremental_window_order(&other.row, &self.order)
        })
    }
}
impl PartialOrd for OrderedRow {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for OrderedRow {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for OrderedRow {}

#[derive(PartialEq, Eq)]
pub(super) struct State {
    order: Arc<[GraphValueOrder]>,
    groups: BTreeMap<Group, ZSet<OrderedRow>>,
}
impl State {
    pub(super) fn new(order: &[GraphValueOrder]) -> Self {
        Self {
            order: order.into(),
            groups: BTreeMap::new(),
        }
    }

    // The enclosing operator has already checked EVERY raw row, payload and
    // final multiplicity. Recheck integrated support here before exposing the
    // prospective index; the index never trusts a negative result as absence.
    pub(super) fn prepare<E>(
        &mut self,
        changes: &ZSet<GraphValueRow>,
        keys: &[usize],
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<Update<'_>, GroupError<E>> {
        let mut updates: BTreeMap<Group, Vec<(OrderedRow, ZWeight)>> = BTreeMap::new();
        for (row, weight) in changes.iter() {
            charge(control, ZSetEvent::Work)?;
            charge(control, ZSetEvent::ScratchEntry)?;
            let mut values = Vec::new();
            for &column in keys {
                let value = row.values().get(column).ok_or(GroupError::InputSchema)?;
                values.push(copy(value, control)?);
            }
            let group: Group = values.into();
            if !updates.contains_key(&group) {
                charge(control, ZSetEvent::ScratchEntry)?;
            }
            charge(control, ZSetEvent::ScratchEntry)?;
            let mut values = Vec::new();
            for value in row.values() {
                values.push(copy(value, control)?);
            }
            charge(control, ZSetEvent::ScratchEntry)?;
            updates.entry(group).or_default().push((
                OrderedRow {
                    row: Arc::new(GraphValueRow::from_owned_values(values)),
                    order: Arc::clone(&self.order),
                },
                weight.checked_clone(limbs).map_err(ZSetError::Arithmetic)?,
            ));
        }
        let mut staged = BTreeMap::new();
        for (group, updates) in updates {
            charge(control, ZSetEvent::Work)?;
            let mut next = match self.groups.get(&group) {
                Some(rows) => rows.checked_clone(limbs, control)?,
                None => ZSet::new(),
            };
            let changes = ZSet::from_updates(updates, limbs, control)?;
            next.integrate(&changes, limbs, control)?;
            for (_, weight) in next.iter() {
                charge(control, ZSetEvent::Work)?;
                if weight <= &ZWeight::ZERO {
                    return Err(GroupError::NegativeMultiplicity);
                }
            }
            charge(control, ZSetEvent::ScratchEntry)?;
            staged.insert(group, next);
        }
        charge(control, ZSetEvent::Work)?;
        Ok(Update {
            owner: self,
            staged,
        })
    }
}

pub(super) struct Update<'a> {
    owner: &'a mut State,
    // Empty is an explicit group deletion, never an absent overlay entry.
    staged: BTreeMap<Group, ZSet<OrderedRow>>,
}
impl Update<'_> {
    pub(super) fn changed_groups(&self) -> impl Iterator<Item = &Group> {
        self.staged.keys()
    }

    pub(super) fn render<E>(
        &self,
        group: &Group,
        column: usize,
        distinct: bool,
        after: bool,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<GraphValue, GroupError<E>> {
        let rows = if after {
            self.staged
                .get(group)
                .or_else(|| self.owner.groups.get(group))
        } else {
            self.owner.groups.get(group)
        };
        let mut values = Vec::new();
        let mut seen = BTreeSet::<&GraphValue>::new();
        let mut max_payload = 0_usize;
        let mut remaining = GraphValue::MAX_LIST_NODES - 1; // outer list itself
        charge(control, ZSetEvent::ScratchEntry)?;
        for (row, weight) in rows.into_iter().flat_map(|rows| rows.iter()) {
            charge(control, ZSetEvent::Work)?;
            let value = row
                .row
                .values()
                .get(column)
                .ok_or(GroupError::InputSchema)?;
            if value.is_null() {
                continue;
            }
            if distinct {
                // The first occurrence wins, using the native value equality.
                // Duplicate counts can exceed u64: they are never expanded or
                // narrowed when only presence is needed.
                max_payload = max_payload.max(value.payload_units());
                let levels = (seen.len().saturating_add(1)).ilog2() as usize + 1;
                for _ in 0..levels
                    .saturating_mul(24)
                    .saturating_mul(max_payload.saturating_add(1))
                {
                    charge(control, ZSetEvent::Work)?;
                }
                if seen.contains(value) {
                    continue;
                }
                charge(control, ZSetEvent::ScratchEntry)?;
                seen.insert(value);
            }
            let repetitions = if distinct { 1 } else { count(weight)? };
            let nodes = nested_nodes(value, 1, control)?;
            if repetitions > (remaining / nodes) as u64 {
                return Err(GroupError::Arithmetic);
            }
            // Above proof bounds conversion/multiplication independently of
            // the host word size, before the first occurrence allocation.
            remaining -= nodes * repetitions as usize;
            for _ in 0..repetitions {
                charge(control, ZSetEvent::Work)?;
                values.push(copy(value, control)?);
            }
        }
        charge(control, ZSetEvent::Work)?;
        Ok(GraphValue::List(values.into_boxed_slice()))
    }

    pub(super) fn commit(self) {
        for (group, rows) in self.staged {
            if rows.is_empty() {
                self.owner.groups.remove(&group);
            } else {
                self.owner.groups.insert(group, rows);
            }
        }
    }
}

fn copy<E>(
    value: &GraphValue,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<GraphValue, GroupError<E>> {
    value.copy_with_control(&mut |event| {
        charge(
            control,
            match event {
                GlaExecutionEvent::ScratchEntry => ZSetEvent::ScratchEntry,
                GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => ZSetEvent::Work,
            },
        )
    })
}

// Count a prospective element at its OUTPUT nesting depth before copying it.
// Source admission bounded recursion already, but adding an outer collection
// can exceed the public value depth/node domain. Every visited node is charged.
fn nested_nodes<E>(
    value: &GraphValue,
    depth: usize,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<usize, GroupError<E>> {
    charge(control, ZSetEvent::Work)?;
    if depth > GraphValue::MAX_LIST_DEPTH {
        return Err(GroupError::Arithmetic);
    }
    let mut nodes = 1_usize;
    if let GraphValue::List(values) = value {
        for value in values.iter() {
            nodes = nodes
                .checked_add(nested_nodes(value, depth + 1, control)?)
                .filter(|&nodes| nodes <= GraphValue::MAX_LIST_NODES)
                .ok_or(GroupError::Arithmetic)?;
        }
    }
    Ok(nodes)
}

#[cfg(test)]
mod tests;
