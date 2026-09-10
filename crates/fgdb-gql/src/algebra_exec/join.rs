//! Physical access for already-bound endpoints and adjacent closing constraints.
//!
//! Logical operators and their transcript are unchanged. Only the exact forms
//! recognized below are eligible: no Select, projection, optional/probe boundary,
//! or other observable source operation is moved or skipped. A reciprocal index
//! is derived from the same admitted topology as the original adjacency index.
//! The original visitor still emits every actual edge occurrence. In particular,
//! intersection is a candidate filter, not a multiplicity estimate or DISTINCT.

use super::{GlaDirection, GlaExecutionEvent, GlaOperator, Index, RelationId, VId};
use crate::algebra::BindingSlot;

#[derive(Clone, Copy)]
struct IntersectionAccess {
    candidate: usize,
    anchor: BindingSlot,
    relation: RelationId,
    direction: GlaDirection,
}

fn reverse(direction: GlaDirection) -> GlaDirection {
    match direction {
        GlaDirection::Forward => GlaDirection::Reverse,
        GlaDirection::Reverse => GlaDirection::Forward,
        GlaDirection::Undirected => GlaDirection::Undirected,
    }
}

/// Recognize two adjacent expansions whose new slots are joined immediately:
///
/// ```text
/// expand(bound, x); expand(x, y); y = bound2
/// expand(bound, x); expand(bound2, y); y = x
/// ```
///
/// No predicate or scope boundary may intervene. Numeric slot relationships are
/// checked again against the actual binding width before using an access path.
fn intersection_access(operators: &[GlaOperator], at: usize) -> Option<IntersectionAccess> {
    let GlaOperator::Expand { source: first, .. } = operators.get(at)? else {
        return None;
    };
    let GlaOperator::Expand { source, relation, direction } = operators.get(at.checked_add(1)?)? else {
        return None;
    };
    let GlaOperator::VertexIdentity { left, right, equal: true } = operators.get(at.checked_add(2)?)? else {
        return None;
    };
    let appended = left.ordinal().max(right.ordinal());
    let other = if left.ordinal() == appended { *right } else { *left };
    let candidate = appended.checked_sub(1)?;
    if first.ordinal() >= candidate {
        return None;
    }
    let (anchor, direction) = if source.ordinal() == candidate && other.ordinal() < candidate {
        (other, reverse(*direction))
    } else if source.ordinal() < candidate && other.ordinal() == candidate {
        (*source, *direction)
    } else {
        return None;
    };
    Some(IntersectionAccess { candidate: candidate as usize, anchor, relation: *relation, direction })
}

/// Called once after registering the logical plan's ordinary index pairs.
/// Only a genuinely new reciprocal pair reserves an extra metadata entry; edge
/// occurrences in it subsequently use build_index's existing scratch controls.
pub(super) fn register_indexes<E>(
    operators: &[GlaOperator],
    index: &mut Index,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    for at in 0..operators.len() {
        if let Some(access) = intersection_access(operators, at) {
            control(GlaExecutionEvent::Work)?;
            let key = (access.relation, access.direction);
            if !index.contains_key(&key) {
                control(GlaExecutionEvent::ScratchEntry)?;
                index.insert(key, Default::default());
            }
        }
    }
    Ok(())
}

/// First index at or after start with value >= target. Exponential bracketing
/// keeps monotone sparse intersections from rescanning long rejected prefixes.
/// The half-open binary-search interval excludes an already-known upper bound.
fn seek_ge<E>(
    values: &[VId],
    target: VId,
    start: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<usize, E> {
    if start >= values.len() {
        return Ok(values.len());
    }
    control(GlaExecutionEvent::Work)?;
    if values[start] >= target {
        return Ok(start);
    }
    let mut low = start + 1;
    let mut stride = 1_usize;
    let mut high;
    loop {
        let probe = start.saturating_add(stride);
        if probe >= values.len() {
            high = values.len();
            break;
        }
        control(GlaExecutionEvent::Work)?;
        if values[probe] >= target {
            high = probe;
            break;
        }
        low = probe + 1;
        stride = stride.saturating_mul(2);
    }
    while low < high {
        let middle = low + (high - low) / 2;
        control(GlaExecutionEvent::Work)?;
        if values[middle] < target { low = middle + 1; } else { high = middle; }
    }
    Ok(low)
}

fn equal_range<'a, E>(
    values: &'a [VId],
    target: VId,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<&'a [VId], E> {
    let start = seek_ge(values, target, 0, control)?;
    if start == values.len() { return Ok(&values[start..start]); }
    control(GlaExecutionEvent::Work)?;
    if values[start] != target { return Ok(&values[start..start]); }
    let mut low = start + 1;
    let mut high = values.len();
    while low < high {
        let middle = low + (high - low) / 2;
        control(GlaExecutionEvent::Work)?;
        if values[middle] <= target { low = middle + 1; } else { high = middle; }
    }
    Ok(&values[start..low])
}

/// Only an immediate equality with an existing binding permits a point seek.
/// A null anchor is known not to match; it is never replaced by a sentinel ID.
/// Unrecognized/tautological/inequality shapes use the ordinary adjacency slice.
pub(super) fn bound_neighbors<'a, E>(
    next: Option<&GlaOperator>,
    appended: usize,
    bindings: &[Option<VId>],
    neighbors: &'a [VId],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<&'a [VId], E> {
    let Some(GlaOperator::VertexIdentity { left, right, equal: true }) = next else {
        return Ok(neighbors);
    };
    let anchor = if left.ordinal() as usize == appended && (right.ordinal() as usize) < appended {
        right
    } else if right.ordinal() as usize == appended && (left.ordinal() as usize) < appended {
        left
    } else {
        return Ok(neighbors);
    };
    match bindings.get(anchor.ordinal() as usize) {
        Some(Some(target)) => equal_range(neighbors, *target, control),
        Some(None) => Ok(&neighbors[..0]),
        None => Ok(neighbors),
    }
}

/// Borrowed, allocation-free candidate cursor. Every occurrence in primary is
/// yielded once iff its ID occurs in membership. Repetitions in membership are
/// deliberately not expanded here: the following logical Expand does that once,
/// using its complete equal range. This preserves the product of multiplicities.
pub(super) struct Candidates<'a> {
    primary: &'a [VId],
    membership: Option<&'a [VId]>,
    left: usize,
    right: usize,
}
impl<'a> Candidates<'a> {
    fn all(primary: &'a [VId]) -> Self {
        Self { primary, membership: None, left: 0, right: 0 }
    }

    pub(super) fn next<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<VId>, E> {
        let Some(membership) = self.membership else {
            let value = self.primary.get(self.left).copied();
            if value.is_some() { self.left += 1; }
            return Ok(value);
        };
        while self.left < self.primary.len() && self.right < membership.len() {
            let left = self.primary[self.left];
            let right = membership[self.right];
            control(GlaExecutionEvent::Work)?;
            match left.cmp(&right) {
                core::cmp::Ordering::Equal => {
                    self.left += 1;
                    return Ok(Some(left));
                }
                core::cmp::Ordering::Less => {
                    self.left = seek_ge(self.primary, right, self.left + 1, control)?;
                }
                core::cmp::Ordering::Greater => {
                    self.right = seek_ge(membership, left, self.right + 1, control)?;
                }
            }
        }
        Ok(None)
    }
}

pub(super) fn candidates<'a, E>(
    operators: &[GlaOperator],
    at: usize,
    bindings: &[Option<VId>],
    neighbors: &'a [VId],
    index: &'a Index,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Candidates<'a>, E> {
    let neighbors = bound_neighbors(operators.get(at + 1), bindings.len(), bindings, neighbors, control)?;
    let Some(access) = intersection_access(operators, at) else { return Ok(Candidates::all(neighbors)); };
    if access.candidate != bindings.len() { return Ok(Candidates::all(neighbors)); }
    let Some(anchor) = bindings.get(access.anchor.ordinal() as usize) else { return Ok(Candidates::all(neighbors)); };
    let Some(anchor) = anchor else { return Ok(Candidates::all(&neighbors[..0])); };
    // A missing derived index is not evidence of absent edges. Keep the ordinary
    // visitor as a correctness-preserving fallback for that internal mismatch.
    let Some(adjacency) = index.get(&(access.relation, access.direction)) else {
        return Ok(Candidates::all(neighbors));
    };
    let membership = adjacency.get(anchor).map_or(&[][..], Vec::as_slice);
    Ok(Candidates { primary: neighbors, membership: Some(membership), left: 0, right: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn multisets() -> Vec<Vec<VId>> {
        let choices = [VId(0), VId(1_u128 << 100), VId(u128::MAX)];
        let mut arrays = BTreeSet::new();
        for length in 0..=6 {
            for mut code in 0..3_usize.pow(length) {
                let mut row = Vec::new();
                for _ in 0..length { row.push(choices[code % 3]); code /= 3; }
                row.sort_unstable();
                arrays.insert(row);
            }
        }
        arrays.into_iter().collect()
    }

    fn closing(fan_in: bool, direction: GlaDirection) -> Vec<GlaOperator> {
        vec![
            GlaOperator::Expand { source: BindingSlot(1), relation: RelationId(2), direction: GlaDirection::Forward },
            GlaOperator::Expand { source: BindingSlot(if fan_in { 0 } else { 2 }), relation: RelationId(3), direction },
            GlaOperator::VertexIdentity { left: BindingSlot(if fan_in { 2 } else { 0 }), right: BindingSlot(3), equal: true },
        ]
    }

    #[test]
    fn seeks_and_ranges_match_linear_search_at_every_start() {
        let targets = [VId(0), VId(1), VId(1_u128 << 100), VId((1_u128 << 100) + 1), VId(u128::MAX)];
        for array in multisets() {
            for target in targets {
                for start in 0..=array.len() {
                    let expected = (start..array.len()).find(|at| array[*at] >= target).unwrap_or(array.len());
                    assert_eq!(seek_ge(&array, target, start, &mut |_| Ok::<_, ()>(())).unwrap(), expected);
                }
                let expected: Vec<_> = array.iter().copied().filter(|value| *value == target).collect();
                assert_eq!(equal_range(&array, target, &mut |_| Ok::<_, ()>(())).unwrap(), expected.as_slice());
            }
        }
    }

    #[test]
    fn intersection_retains_left_occurrences_without_multiplying_right_ones() {
        let arrays = multisets();
        for left in &arrays {
            for right in &arrays {
                let mut cursor = Candidates { primary: left, membership: Some(right), left: 0, right: 0 };
                let mut found = Vec::new();
                let mut work = 0_usize;
                while let Some(value) = cursor.next(&mut |event| {
                    assert_eq!(event, GlaExecutionEvent::Work);
                    work += 1;
                    Ok::<_, ()>(())
                }).unwrap() { found.push(value); }
                let expected: Vec<_> = left.iter().copied().filter(|value| right.contains(value)).collect();
                assert_eq!(found, expected);
                assert!(work <= 8 * (left.len() + right.len() + 1));
            }
        }
    }

    #[test]
    fn every_new_seek_checkpoint_preserves_the_callers_error() {
        let left = [VId(0), VId(0), VId(4), VId(8), VId(9), VId(u128::MAX)];
        let right = [VId(1), VId(4), VId(4), VId(9), VId(12)];
        let run = |stop: usize| {
            let mut events = 0;
            let mut cursor = Candidates { primary: &left, membership: Some(&right), left: 0, right: 0 };
            let mut found = Vec::new();
            let result = (|| {
                while let Some(value) = cursor.next(&mut |_| {
                    events += 1;
                    if events == stop { Err(stop) } else { Ok(()) }
                })? { found.push(value); }
                Ok(found)
            })();
            (events, result)
        };
        let (total, result) = run(usize::MAX);
        assert_eq!(result, Ok(vec![VId(4), VId(9)]));
        for stop in 1..=total {
            let (events, result) = run(stop);
            assert_eq!(events, stop);
            assert_eq!(result, Err(stop));
        }
        let mut events = 0;
        equal_range(&left, VId(0), &mut |_| { events += 1; Ok::<_, ()>(()) }).unwrap();
        for stop in 1..=events {
            let mut at = 0;
            let result = equal_range(&left, VId(0), &mut |_| {
                at += 1;
                if at == stop { Err(stop) } else { Ok(()) }
            });
            assert_eq!(result, Err(stop));
            assert_eq!(at, stop);
        }
    }

    #[test]
    fn static_access_is_direction_correct_and_cannot_cross_a_scope_or_predicate() {
        for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
            for fan_in in [false, true] {
                let operators = closing(fan_in, direction);
                let access = intersection_access(&operators, 0).unwrap();
                assert_eq!(access.candidate, 2);
                assert_eq!(access.anchor.ordinal(), 0);
                assert_eq!(access.direction, if fan_in { direction } else { reverse(direction) });
                for barrier in [
                    GlaOperator::Select { slot: BindingSlot(2), predicates: vec![] },
                    GlaOperator::CompareProperties {
                        left: BindingSlot(2),
                        left_key: fgdb_delta_types::PropertyKeyId(1),
                        right: BindingSlot(0),
                        right_key: fgdb_delta_types::PropertyKeyId(1),
                        comparison: crate::algebra::IntegerComparison::Equal,
                    },
                    GlaOperator::OptionalEnd { group: 0 },
                    GlaOperator::ProbeEnd { group: 0 },
                    GlaOperator::BindVertex { source: BindingSlot(0) },
                ] {
                    let mut blocked = operators.clone(); blocked.insert(1, barrier);
                    assert!(intersection_access(&blocked, 0).is_none());
                }
            }
        }
    }

    #[test]
    fn missing_derived_indexes_fall_back_and_null_anchors_do_not_become_ids() {
        let operators = closing(false, GlaDirection::Forward);
        let neighbors = [VId(3), VId(3), VId(4)];
        let bindings = [Some(VId(0)), Some(VId(1))];
        let index = Index::new();
        let mut cursor = candidates(&operators, 0, &bindings, &neighbors, &index, &mut |_| Ok::<_, ()>(())).unwrap();
        let mut actual = Vec::new();
        while let Some(value) = cursor.next(&mut |_| Ok::<_, ()>(())).unwrap() { actual.push(value); }
        assert_eq!(actual, neighbors);
        let mut cursor = candidates(&operators, 0, &[None, Some(VId(1))], &neighbors, &index, &mut |_| Ok::<_, ()>(())).unwrap();
        assert_eq!(cursor.next(&mut |_| Ok::<_, ()>(())).unwrap(), None);
        let equal = GlaOperator::VertexIdentity { left: BindingSlot(0), right: BindingSlot(2), equal: true };
        assert!(bound_neighbors(Some(&equal), 2, &[None, Some(VId(1))], &neighbors, &mut |_| Ok::<_, ()>(())).unwrap().is_empty());
        let inequality = GlaOperator::VertexIdentity { left: BindingSlot(0), right: BindingSlot(2), equal: false };
        assert_eq!(bound_neighbors(Some(&inequality), 2, &bindings, &neighbors, &mut |_| Ok::<_, ()>(())).unwrap(), neighbors);
    }

    #[test]
    fn reciprocal_index_growth_is_reserved_once_and_never_on_refusal() {
        let operators = closing(false, GlaDirection::Forward);
        let mut index = Index::new();
        let result = register_indexes(&operators, &mut index, &mut |event| {
            if event == GlaExecutionEvent::ScratchEntry { Err("scratch") } else { Ok(()) }
        });
        assert_eq!(result, Err("scratch"));
        assert!(index.is_empty());
        let mut scratch = 0;
        for _ in 0..2 {
            register_indexes(&operators, &mut index, &mut |event| {
                scratch += usize::from(event == GlaExecutionEvent::ScratchEntry);
                Ok::<_, ()>(())
            }).unwrap();
        }
        assert_eq!(scratch, 1);
        assert!(index.contains_key(&(RelationId(3), GlaDirection::Reverse)));
    }

    #[test]
    fn bound_lookup_does_not_visit_a_high_degree_neighborhood_linearly() {
        let neighbors: Vec<_> = (0..65_536_u128).map(VId).collect();
        for target in [VId(0), VId(32_768), VId(65_535), VId(u128::MAX)] {
            let mut comparisons = 0;
            let found = equal_range(&neighbors, target, &mut |_| { comparisons += 1; Ok::<_, ()>(()) }).unwrap();
            assert_eq!(found.len(), usize::from(target.0 < 65_536));
            assert!(comparisons <= 100, "comparison bound violated: {comparisons}");
        }
    }
}
