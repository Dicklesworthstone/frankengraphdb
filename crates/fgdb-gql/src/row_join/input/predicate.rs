//! Residual ON predicates make existence row-specific, not key-specific.
//!
//! Matched pairs still use the selected bilinear derivative. Outer/presence
//! changes inspect changed rows and opposite support transitions, not complete
//! before/after products. A nonzero-to-nonzero weight change cannot add or lose
//! a witness. All counts, original row identities and preparation guards remain
//! owned by the existing row join; only temporary support metadata is retained.
//! Losing a last witness can still require a complete opposite-group search.

use super::*;
use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};

type Group = ZSet<Row>;
type Changed = [Vec<(Row, ZWeight)>; 2];

pub(super) fn presence_delta<E>(
    input: &IncrementalJoin<Key, Row, Row>,
    spec: &RowJoinSpec,
    left: &Arranged,
    right: &Arranged,
    limbs: LimbLimit,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<ZSet<GraphValueRow>, ZSetError<E>> {
    if spec.kind == RowJoinKind::Inner {
        return Ok(ZSet::new());
    }
    // Group only this tick's admitted changes; Arc clones do not copy payloads.
    let mut changes: BTreeMap<&Key, Changed> = BTreeMap::new();
    for (side, delta) in [left, right].into_iter().enumerate() {
        for ((key, row), weight) in delta.iter() {
            charge(control, ZSetEvent::Work)?;
            let group = match changes.entry(key) {
                Entry::Vacant(entry) => {
                    charge(control, ZSetEvent::ScratchEntry)?;
                    entry.insert([Vec::new(), Vec::new()])
                }
                Entry::Occupied(entry) => entry.into_mut(),
            };
            let weight = weight.checked_clone(limbs).map_err(ZSetError::Arithmetic)?;
            charge(control, ZSetEvent::ScratchEntry)?;
            group[side].push((row.clone(), weight));
        }
    }
    let empty = Group::new();
    let mut output = Vec::new();
    for (key, [left, right]) in changes {
        charge(control, ZSetEvent::Work)?;
        let old_left = input.left_group(key).unwrap_or(&empty);
        let old_right = input.right_group(key).unwrap_or(&empty);
        let left = Group::from_updates(left, limbs, control)?;
        let right = Group::from_updates(right, limbs, control)?;
        // The host has validated every original row's final nonnegative count.
        // Never clone either old group or materialize its complete successor.
        if matches!(
            spec.kind,
            RowJoinKind::Left | RowJoinKind::Full | RowJoinKind::Semi | RowJoinKind::Anti
        ) {
            let other = Witnesses::new(old_right, &right, limbs, control)?;
            append_changes(spec, old_left, &left, &other, false, &mut output, limbs, control)?;
        }
        if matches!(spec.kind, RowJoinKind::Right | RowJoinKind::Full) {
            let other = Witnesses::new(old_left, &left, limbs, control)?;
            append_changes(spec, old_right, &right, &other, true, &mut output, limbs, control)?;
        }
    }
    // Null extensions from distinct arms can coincide. Consolidate exact signed
    // counts before the single native sink applies its final occurrence bound.
    ZSet::from_updates(output, limbs, control)
}

// Only zero crossings affect existence. Borrow their original rows from this
// tick's delta: the complete old arrangement and its payloads stay in place.
// The metadata is O(changed support), even for a very large common-key group.
struct Witnesses<'a> {
    old: &'a Group,
    added: Vec<&'a Row>,
    removed: BTreeSet<&'a Row>,
}

impl<'a> Witnesses<'a> {
    fn new<E>(
        old: &'a Group,
        delta: &'a Group,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<Self, ZSetError<E>> {
        let mut added = Vec::new();
        let mut removed = BTreeSet::new();
        for (row, change) in delta.iter() {
            charge(control, ZSetEvent::Work)?;
            if let Some(before) = old.weight(row) {
                charge(control, ZSetEvent::Work)?;
                let after = before.checked_add(change, limbs).map_err(ZSetError::Arithmetic)?;
                if after.is_zero() {
                    charge(control, ZSetEvent::ScratchEntry)?;
                    removed.insert(row);
                }
            } else {
                // Admitted absent rows can only become positive support.
                charge(control, ZSetEvent::ScratchEntry)?;
                added.push(row);
            }
        }
        Ok(Self { old, added, removed })
    }

    fn changed(&self) -> bool {
        !self.added.is_empty() || !self.removed.is_empty()
    }

    fn has_match<E>(
        &self,
        spec: &RowJoinSpec,
        row: &Row,
        reversed: bool,
        after: bool,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<bool, ZSetError<E>> {
        for (candidate, _) in self.old.iter() {
            charge(control, ZSetEvent::Work)?;
            if after && self.removed.contains(&candidate) {
                continue;
            }
            if matches(spec, row, candidate, reversed, control)? {
                return Ok(true);
            }
        }
        // Fixed old-then-new traversal, not a data-dependent strategy retry.
        // The predicate is pure; every visited candidate uses its native kernel.
        if after {
            return any_match(spec, row, self.added.iter().copied(), reversed, control);
        }
        Ok(false)
    }
}

fn matches<E>(
    spec: &RowJoinSpec,
    row: &Row,
    candidate: &Row,
    reversed: bool,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<bool, ZSetError<E>> {
    let (left, right) = if reversed { (candidate, row) } else { (row, candidate) };
    spec.matches(left, right, control)
}

fn any_match<'a, E>(
    spec: &RowJoinSpec,
    row: &Row,
    candidates: impl IntoIterator<Item = &'a Row>,
    reversed: bool,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<bool, ZSetError<E>> {
    for candidate in candidates {
        charge(control, ZSetEvent::Work)?;
        if matches(spec, row, candidate, reversed, control)? {
            return Ok(true);
        }
    }
    Ok(false)
}

#[allow(clippy::too_many_arguments)]
fn append_changes<E>(
    spec: &RowJoinSpec,
    old: &Group,
    delta: &Group,
    other: &Witnesses<'_>,
    reversed: bool,
    output: &mut Vec<(GraphValueRow, ZWeight)>,
    limbs: LimbLimit,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<(), ZSetError<E>> {
    let keep_matched = !reversed && spec.kind == RowJoinKind::Semi;
    let zero = ZWeight::ZERO;
    // Changed own rows include simultaneous input changes. Emit the exact
    // new_count * keep_new - old_count * keep_old, never threshold a delta.
    for (row, change) in delta.iter() {
        charge(control, ZSetEvent::Work)?;
        let before = old.weight(row).unwrap_or(&zero);
        charge(control, ZSetEvent::Work)?;
        let after = before.checked_add(change, limbs).map_err(ZSetError::Arithmetic)?;
        let matched_before = !before.is_zero()
            && other.has_match(spec, row, reversed, false, control)?;
        let matched_after = if after.is_zero() {
            false
        } else if !before.is_zero() && !other.changed() {
            matched_before
        } else {
            other.has_match(spec, row, reversed, true, control)?
        };
        let keep_before = !before.is_zero() && matched_before == keep_matched;
        let keep_after = !after.is_zero() && matched_after == keep_matched;
        match (keep_before, keep_after) {
            (true, true) => append_side(output, spec, row, reversed, change, limbs, control)?,
            (false, true) => append_side(output, spec, row, reversed, &after, limbs, control)?,
            (true, false) => {
                charge(control, ZSetEvent::Work)?;
                let removed = before.checked_mul(&ZWeight::from_i128(-1), limbs)
                    .map_err(ZSetError::Arithmetic)?;
                append_side(output, spec, row, reversed, &removed, limbs, control)?;
            }
            (false, false) => {}
        }
    }
    if !other.changed() {
        // No opposite zero crossing: all untouched own rows cancel exactly.
        return Ok(());
    }
    for (row, count) in old.iter() {
        charge(control, ZSetEvent::Work)?;
        if delta.weight(row).is_some() {
            continue;
        }
        let gained = any_match(spec, row, other.added.iter().copied(), reversed, control)?;
        let lost = any_match(spec, row, other.removed.iter().copied(), reversed, control)?;
        if gained == lost {
            // Neither touched a witness, or a removed witness is replaced by
            // an inserted one. In both cases existence is unchanged.
            continue;
        }
        let (before, after) = if gained {
            (other.has_match(spec, row, reversed, false, control)?, true)
        } else {
            (true, other.has_match(spec, row, reversed, true, control)?)
        };
        if before != after {
            charge(control, ZSetEvent::Work)?;
            let sign = if after == keep_matched { 1 } else { -1 };
            let change = count.checked_mul(&ZWeight::from_i128(sign), limbs)
                .map_err(ZSetError::Arithmetic)?;
            append_side(output, spec, row, reversed, &change, limbs, control)?;
        }
    }
    Ok(())
}

fn append_side<E>(
    output: &mut Vec<(GraphValueRow, ZWeight)>,
    spec: &RowJoinSpec,
    row: &Row,
    reversed: bool,
    weight: &ZWeight,
    limbs: LimbLimit,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<(), ZSetError<E>> {
    let (left, right) = if reversed { (None, Some(row)) } else { (Some(row), None) };
    append(output, spec, left, right, weight, limbs, control)
}

#[cfg(test)]
mod tests;
