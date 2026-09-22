//! Residual ON predicates make existence row-specific, not key-specific.
//!
//! The ordinary selected bilinear derivative owns matched pairs. This module
//! computes only its missing outer/presence terms, as after-minus-before over
//! affected key groups. No unrelated group is read; a no-key theta join has
//! one group and can require quadratic candidate work. Semi/anti never build
//! matched products, even when occurrence counts would overflow multiplication.
//! All temporary state and callbacks remain inside the parent's prepare guard.

use super::*;
use std::collections::{BTreeMap, btree_map::Entry};

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
        let new_left = old_left.plus(&left, limbs, control)?;
        let new_right = old_right.plus(&right, limbs, control)?;
        // Whole-row nonnegative input validation has already run in the host.
        // Compare complete before/after domains, not the sign of a right delta:
        // deleting one of several accepted witnesses preserves existence.
        append_presence(spec, old_left, old_right, -1, &mut output, limbs, control)?;
        append_presence(spec, &new_left, &new_right, 1, &mut output, limbs, control)?;
    }
    // Null extensions from distinct arms can coincide. Consolidate exact signed
    // counts before the single native sink applies its final occurrence bound.
    ZSet::from_updates(output, limbs, control)
}

fn has_match<E>(
    spec: &RowJoinSpec,
    row: &Row,
    other: &Group,
    reversed: bool,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<bool, ZSetError<E>> {
    for (candidate, _) in other.iter() {
        charge(control, ZSetEvent::Work)?;
        let (left, right) = if reversed {
            (candidate, row)
        } else {
            (row, candidate)
        };
        if spec.matches(left, right, control)? {
            return Ok(true);
        }
    }
    Ok(false)
}

#[allow(clippy::too_many_arguments)]
fn append_presence<E>(
    spec: &RowJoinSpec,
    left: &Group,
    right: &Group,
    sign: i128,
    output: &mut Vec<(GraphValueRow, ZWeight)>,
    limbs: LimbLimit,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<(), ZSetError<E>> {
    let sign = ZWeight::from_i128(sign);
    if matches!(
        spec.kind,
        RowJoinKind::Left | RowJoinKind::Full | RowJoinKind::Semi | RowJoinKind::Anti
    ) {
        for (row, count) in left.iter() {
            charge(control, ZSetEvent::Work)?;
            let matched = has_match(spec, row, right, false, control)?;
            let keep = if spec.kind == RowJoinKind::Semi {
                matched
            } else {
                !matched
            };
            if keep {
                let count = count
                    .checked_mul(&sign, limbs)
                    .map_err(ZSetError::Arithmetic)?;
                append(output, spec, Some(row), None, &count, limbs, control)?;
            }
        }
    }
    if matches!(spec.kind, RowJoinKind::Right | RowJoinKind::Full) {
        for (row, count) in right.iter() {
            charge(control, ZSetEvent::Work)?;
            if !has_match(spec, row, left, true, control)? {
                let count = count
                    .checked_mul(&sign, limbs)
                    .map_err(ZSetError::Arithmetic)?;
                append(output, spec, None, Some(row), &count, limbs, control)?;
            }
        }
    }
    Ok(())
}
