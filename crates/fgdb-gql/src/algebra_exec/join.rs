//! Physical access for already-bound endpoints and adjacent closing constraints.
//!
//! Logical operators and their transcript are unchanged. Only the exact forms
//! recognized below are eligible: no Select, projection, optional/probe boundary,
//! or other observable source operation is moved or skipped. A reciprocal index
//! is derived from the same admitted topology as the original adjacency index.
//! The original visitor still emits every actual edge occurrence. In particular,
//! intersection is a candidate filter, not a multiplicity estimate or DISTINCT.

use super::{GlaDirection, GlaExecutionEvent, GlaOperator, Index, RelationId, VId};
use crate::algebra::{BindingSlot, MAX_PATTERN_EDGES};

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

/// Continue only through adjacent Expand/equality pairs for the SAME candidate.
/// A closing pair appends a temporary slot, even though its representative was
/// already bound. Check that exact ordinal on every step; a user identity, a
/// new independent variable or a scope boundary cannot masquerade as a close.
/// No graph reads, allocation or value-dependent planning occurs here.
fn additional_accesses(
    operators: &[GlaOperator],
    at: usize,
    first: IntersectionAccess,
) -> impl Iterator<Item = IntersectionAccess> + '_ {
    let mut next = at.checked_add(3);
    let mut appended = first.candidate.checked_add(2);
    let mut remaining = MAX_PATTERN_EDGES - 2;
    std::iter::from_fn(move || {
        if remaining == 0 { return None; }
        // Taking next makes the iterator fused on every failed recognition.
        let at = next.take()?;
        let slot = appended?;
        let GlaOperator::Expand { source, relation, direction } = operators.get(at)? else {
            return None;
        };
        let GlaOperator::VertexIdentity { left, right, equal: true } = operators.get(at.checked_add(1)?)? else {
            return None;
        };
        let other = if left.ordinal() as usize == slot {
            *right
        } else if right.ordinal() as usize == slot {
            *left
        } else {
            return None;
        };
        let candidate = first.candidate;
        let (anchor, direction) = if source.ordinal() as usize == candidate
            && (other.ordinal() as usize) < candidate
        {
            (other, reverse(*direction))
        } else if (source.ordinal() as usize) < candidate
            && other.ordinal() as usize == candidate
        {
            (*source, *direction)
        } else {
            return None;
        };
        next = at.checked_add(2);
        appended = slot.checked_add(1);
        remaining -= 1;
        Some(IntersectionAccess { candidate, anchor, relation: *relation, direction })
    })
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
        if let Some(first) = intersection_access(operators, at) {
            for access in std::iter::once(first).chain(additional_accesses(operators, at, first)) {
                control(GlaExecutionEvent::Work)?;
                let key = (access.relation, access.direction);
                if !index.contains_key(&key) {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    index.insert(key, Default::default());
                }
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

/// Additional cursors contain only borrowed adjacency slices and monotone
/// positions, never candidate/result rows. Each descriptor is charged before
/// its allocation. The ordinary pair path needs no additional allocation.
struct Membership<'a> {
    values: &'a [VId],
    position: usize,
}

/// Yield each primary occurrence only if it is in EVERY membership list.
/// Membership repetitions are deliberately not multiplied here: the original
/// visitor expands each complete equal range in turn. Thus conjunction pruning
/// changes candidate access, not the bag or OPTIONAL/EXISTS success boundary.
pub(super) struct Candidates<'a> {
    primary: &'a [VId],
    membership: Option<&'a [VId]>,
    left: usize,
    right: usize,
    additional: Vec<Membership<'a>>,
    confirmed: Option<VId>,
}
impl<'a> Candidates<'a> {
    fn all(primary: &'a [VId]) -> Self {
        Self { primary, membership: None, left: 0, right: 0, additional: Vec::new(), confirmed: None }
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
        'candidate: while self.left < self.primary.len() && self.right < membership.len() {
            let left = self.primary[self.left];
            let right = membership[self.right];
            control(GlaExecutionEvent::Work)?;
            match left.cmp(&right) {
                core::cmp::Ordering::Equal => {
                    if self.confirmed != Some(left) {
                        for cursor in &mut self.additional {
                            cursor.position = seek_ge(cursor.values, left, cursor.position, control)?;
                            let Some(&candidate) = cursor.values.get(cursor.position) else {
                                self.left = self.primary.len();
                                return Ok(None);
                            };
                            control(GlaExecutionEvent::Work)?;
                            if candidate != left {
                                // All positions advance monotonically. If one
                                // stream jumps ahead, retry the first stream
                                // before a value may be declared common to all.
                                self.left = seek_ge(self.primary, candidate, self.left + 1, control)?;
                                continue 'candidate;
                            }
                        }
                        self.confirmed = Some(left);
                    }
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
    let mut cursor = Candidates::all(neighbors);
    cursor.membership = Some(membership);
    for next in additional_accesses(operators, at, access) {
        control(GlaExecutionEvent::Work)?;
        let Some(anchor) = bindings.get(next.anchor.ordinal() as usize) else { break; };
        let Some(anchor) = anchor else { return Ok(Candidates::all(&neighbors[..0])); };
        // A missing derived index disables further pruning, never the logical
        // constraint. Existing, already admitted membership checks stay valid.
        let Some(adjacency) = index.get(&(next.relation, next.direction)) else { break; };
        let values = adjacency.get(anchor).map_or(&[][..], Vec::as_slice);
        control(GlaExecutionEvent::ScratchEntry)?;
        cursor.additional.push(Membership { values, position: 0 });
    }
    Ok(cursor)
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
                let mut cursor = Candidates::all(left);
                cursor.membership = Some(right);
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
            let mut cursor = Candidates::all(&left);
            cursor.membership = Some(&right);
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

    fn multiple_closings() -> Vec<GlaOperator> {
        let mut ops = closing(true, GlaDirection::Forward);
        ops.extend([
            GlaOperator::Expand { source: BindingSlot(2), relation: RelationId(4), direction: GlaDirection::Forward },
            GlaOperator::VertexIdentity { left: BindingSlot(4), right: BindingSlot(0), equal: true },
            GlaOperator::Expand { source: BindingSlot(1), relation: RelationId(5), direction: GlaDirection::Reverse },
            GlaOperator::VertexIdentity { left: BindingSlot(2), right: BindingSlot(5), equal: true },
        ]);
        ops
    }

    #[test]
    fn four_lists_match_independent_membership_and_preserve_primary_multiplicity() {
        let arrays: Vec<_> = multisets().into_iter().filter(|row| row.len() <= 2).collect();
        for a in &arrays {
            for b in &arrays {
                for c in &arrays {
                    for d in &arrays {
                        let mut cursor = Candidates::all(a);
                        cursor.membership = Some(b);
                        cursor.additional = vec![
                            Membership { values: c, position: 0 },
                            Membership { values: d, position: 0 },
                        ];
                        let expected: Vec<_> = a.iter().copied()
                            .filter(|value| b.contains(value) && c.contains(value) && d.contains(value))
                            .collect();
                        let mut actual = Vec::new();
                        while let Some(value) = cursor.next(&mut |_| Ok::<_, ()>(())).unwrap() {
                            actual.push(value);
                        }
                        assert_eq!(actual, expected);
                    }
                }
            }
        }
    }

    #[test]
    fn pairwise_overlap_is_not_a_multiway_witness_and_all_checkpoints_refuse() {
        let primary = [VId(1), VId(1), VId(2), VId(3), VId(9), VId(9)];
        let a = [VId(1), VId(2), VId(9)];
        let b = [VId(2), VId(3), VId(9)];
        let c = [VId(1), VId(3), VId(9)];
        let run = |stop| {
            let mut cursor = Candidates::all(&primary);
            cursor.membership = Some(&a);
            cursor.additional = vec![Membership { values: &b, position: 0 }, Membership { values: &c, position: 0 }];
            let mut events = 0;
            let result = (|| {
                let mut rows = Vec::new();
                while let Some(value) = cursor.next(&mut |_| {
                    events += 1;
                    if events == stop { Err(stop) } else { Ok(()) }
                })? { rows.push(value); }
                Ok(rows)
            })();
            (events, result)
        };
        let (total, result) = run(usize::MAX);
        assert_eq!(result, Ok(vec![VId(9), VId(9)]));
        for stop in 1..=total {
            assert_eq!(run(stop), (stop, Err(stop)));
        }
    }

    #[test]
    fn closing_chain_registers_every_actual_direction_but_stops_at_barriers() {
        let ops = multiple_closings();
        let first = intersection_access(&ops, 0).unwrap();
        let tail: Vec<_> = additional_accesses(&ops, 0, first)
            .map(|access| (access.candidate, access.anchor.ordinal(), access.relation, access.direction))
            .collect();
        assert_eq!(tail, vec![
            (2, 0, RelationId(4), GlaDirection::Reverse),
            (2, 1, RelationId(5), GlaDirection::Reverse),
        ]);
        let mut index = Index::new();
        let mut reservations = 0;
        register_indexes(&ops, &mut index, &mut |event| {
            reservations += usize::from(event == GlaExecutionEvent::ScratchEntry);
            Ok::<_, ()>(())
        }).unwrap();
        assert_eq!(reservations, 3);
        for (relation, direction) in [(3, GlaDirection::Forward), (4, GlaDirection::Reverse), (5, GlaDirection::Reverse)] {
            assert!(index.contains_key(&(RelationId(relation), direction)));
        }
        for barrier in [
            GlaOperator::Select { slot: BindingSlot(2), predicates: vec![] },
            GlaOperator::CompareProperties {
                left: BindingSlot(2), left_key: fgdb_delta_types::PropertyKeyId(1),
                right: BindingSlot(0), right_key: fgdb_delta_types::PropertyKeyId(2),
                comparison: crate::algebra::IntegerComparison::Equal,
            },
            GlaOperator::ProbeEnd { group: 0 },
            GlaOperator::OptionalEnd { group: 0 },
            GlaOperator::BindVertex { source: BindingSlot(0) },
        ] {
            let mut blocked = ops.clone();
            blocked.insert(3, barrier);
            let first = intersection_access(&blocked, 0).unwrap();
            assert!(additional_accesses(&blocked, 0, first).next().is_none());
        }
        let mut malformed = ops.clone();
        malformed[4] = GlaOperator::VertexIdentity { left: BindingSlot(3), right: BindingSlot(0), equal: true };
        assert!(additional_accesses(&malformed, 0, first).next().is_none());
        malformed[4] = GlaOperator::VertexIdentity { left: BindingSlot(4), right: BindingSlot(0), equal: false };
        assert!(additional_accesses(&malformed, 0, first).next().is_none());
    }

    #[test]
    fn descriptor_refusals_missing_indexes_and_null_later_anchors_stay_fail_closed() {
        let ops = multiple_closings();
        let primary = [VId(2), VId(2), VId(3)];
        let mut index = Index::new();
        for (relation, direction, anchor, values) in [
            (3, GlaDirection::Forward, VId(0), vec![VId(2), VId(3)]),
            (4, GlaDirection::Reverse, VId(0), vec![VId(2), VId(3)]),
            (5, GlaDirection::Reverse, VId(1), vec![VId(2)]),
        ] {
            index.entry((RelationId(relation), direction)).or_default().insert(anchor, values);
        }
        let bindings = [Some(VId(0)), Some(VId(1))];
        for stop in 1..=2 {
            let mut scratch = 0;
            let result = candidates(&ops, 0, &bindings, &primary, &index, &mut |event| {
                scratch += usize::from(event == GlaExecutionEvent::ScratchEntry);
                if scratch == stop { Err(stop) } else { Ok(()) }
            });
            assert!(matches!(result, Err(found) if found == stop));
        }
        let mut cursor = candidates(&ops, 0, &bindings, &primary, &index, &mut |_| Ok::<_, ()>(())).unwrap();
        assert_eq!(cursor.next(&mut |_| Ok::<_, ()>(())).unwrap(), Some(VId(2)));
        assert_eq!(cursor.next(&mut |_| Ok::<_, ()>(())).unwrap(), Some(VId(2)));
        assert_eq!(cursor.next(&mut |_| Ok::<_, ()>(())).unwrap(), None);
        let mut cursor = candidates(&ops, 0, &[Some(VId(0)), None], &primary, &index, &mut |_| Ok::<_, ()>(())).unwrap();
        assert_eq!(cursor.next(&mut |_| Ok::<_, ()>(())).unwrap(), None);
        index.remove(&(RelationId(5), GlaDirection::Reverse));
        let mut cursor = candidates(&ops, 0, &bindings, &primary, &index, &mut |_| Ok::<_, ()>(())).unwrap();
        let mut result = Vec::new();
        while let Some(value) = cursor.next(&mut |_| Ok::<_, ()>(())).unwrap() { result.push(value); }
        assert_eq!(result, primary, "an absent derived index is not an empty relation");
    }
}
