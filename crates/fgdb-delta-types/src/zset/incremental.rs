//! Stateful derivatives of exact weighted relations.
//!
//! These are in-memory algebra operators, not persisted arrangements or a
//! scheduler. Join probes only changed join keys; DISTINCT retains integrated
//! counts instead of thresholding each delta. Preparation owns every fallible
//! operation. An exclusive-borrow update guard commits infallibly or rolls back
//! on drop, allowing a caller to prepare downstream state before publishing.
//! Key allocation/comparison costs and standard collection allocation failure
//! have the same explicit boundary as the parent Z-set value API.

pub mod presence;

use super::{ZSet, ZSetError, ZSetEvent, admit, event};
use crate::{LimbLimit, ZWeight};
use std::collections::{BTreeMap, btree_map::Entry};

type Arrangement<K, V> = BTreeMap<K, ZSet<V>>;
type Changes<K, V> = BTreeMap<K, BTreeMap<V, ZWeight>>;
type Grouped<'a, K, V> = BTreeMap<&'a K, Vec<(&'a V, &'a ZWeight)>>;

/// Two integrated input relations arranged by their shared key. Input weights
/// are exact signed integers. The output is `(key, left_value, right_value)`
/// with multiplicity equal to the product of the corresponding input weights.
/// Output state is deliberately not duplicated inside the operator.
#[derive(PartialEq, Eq)]
pub struct IncrementalJoin<K: Ord, L: Ord, R: Ord> {
    left: Arrangement<K, L>,
    right: Arrangement<K, R>,
}

impl<K: Ord, L: Ord, R: Ord> core::fmt::Debug for IncrementalJoin<K, L, R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IncrementalJoin")
            .field("left_keys", &self.left.len())
            .field("right_keys", &self.right.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

impl<K: Ord, L: Ord, R: Ord> Default for IncrementalJoin<K, L, R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Ord, L: Ord, R: Ord> IncrementalJoin<K, L, R> {
    pub fn new() -> Self {
        Self {
            left: BTreeMap::new(),
            right: BTreeMap::new(),
        }
    }

    pub fn left_weight(&self, key: &K, value: &L) -> Option<&ZWeight> {
        self.left.get(key).and_then(|group| group.weight(value))
    }

    pub fn right_weight(&self, key: &K, value: &R) -> Option<&ZWeight> {
        self.right.get(key).and_then(|group| group.weight(value))
    }

    /// Explicit input export in canonical key/value order.
    pub fn left_rows(&self) -> impl Iterator<Item = (&K, &L, &ZWeight)> {
        self.left
            .iter()
            .flat_map(|(key, group)| group.iter().map(move |(value, weight)| (key, value, weight)))
    }

    pub fn right_rows(&self) -> impl Iterator<Item = (&K, &R, &ZWeight)> {
        self.right
            .iter()
            .flat_map(|(key, group)| group.iter().map(move |(value, weight)| (key, value, weight)))
    }
}

impl<K: Ord + Clone, L: Ord + Clone, R: Ord + Clone> IncrementalJoin<K, L, R> {
    /// Prepare the exact derivative:
    /// `delta_left ⋈ right + left ⋈ delta_right + delta_left ⋈ delta_right`.
    /// Both old arrangements remain unchanged while all three terms execute.
    /// Omitting the last term loses simultaneous insertions and over-retracts
    /// simultaneous deletions. No unrelated join-key group is scanned or cloned.
    pub fn prepare<E>(
        &mut self,
        delta_left: &ZSet<(K, L)>,
        delta_right: &ZSet<(K, R)>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<JoinUpdate<'_, K, L, R>, ZSetError<E>> {
        event(control, ZSetEvent::Work)?;
        let left = grouped(delta_left, limbs, control)?;
        let right = grouped(delta_right, limbs, control)?;
        let mut output = ZSet::new();
        for (&key, changes) in &left {
            event(control, ZSetEvent::Work)?;
            if let Some(old_right) = self.right.get(key) {
                for &l in changes {
                    for r in old_right.iter() {
                        add_product(&mut output, key, l, r, limbs, control)?;
                    }
                }
            }
            if let Some(changed_right) = right.get(key) {
                for &l in changes {
                    for &r in changed_right {
                        add_product(&mut output, key, l, r, limbs, control)?;
                    }
                }
            }
        }
        for (&key, changes) in &right {
            event(control, ZSetEvent::Work)?;
            if let Some(old_left) = self.left.get(key) {
                for l in old_left.iter() {
                    for &r in changes {
                        add_product(&mut output, key, l, r, limbs, control)?;
                    }
                }
            }
        }
        let left = prepare_changes(&self.left, &left, limbs, control)?;
        let right = prepare_changes(&self.right, &right, limbs, control)?;
        event(control, ZSetEvent::Work)?;
        Ok(JoinUpdate {
            owner: self,
            left,
            right,
            delta: output,
        })
    }

    /// One complete algebra tick. For multi-operator atomic publication use
    /// `prepare`, prepare downstream output, then commit the update guard.
    pub fn apply<E>(
        &mut self,
        delta_left: &ZSet<(K, L)>,
        delta_right: &ZSet<(K, R)>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<(K, L, R)>, ZSetError<E>> {
        Ok(self.prepare(delta_left, delta_right, limbs, control)?.commit())
    }

    /// Recompute the current result, useful for explicit snapshots and audits.
    /// Ordinary incremental updates do not call this method.
    pub fn snapshot<E>(
        &self,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<(K, L, R)>, ZSetError<E>> {
        let mut output = ZSet::new();
        for (key, left) in &self.left {
            event(control, ZSetEvent::Work)?;
            if let Some(right) = self.right.get(key) {
                for l in left.iter() {
                    for r in right.iter() {
                        add_product(&mut output, key, l, r, limbs, control)?;
                    }
                }
            }
        }
        Ok(output)
    }
}

/// An uncommitted operator transition. The exclusive borrow prevents stale
/// publication. Dropping it, including after a downstream refusal, changes no
/// input state. Its delta is tentative until the guard is committed.
#[must_use = "dropping an update aborts it"]
pub struct JoinUpdate<'a, K: Ord, L: Ord, R: Ord> {
    owner: &'a mut IncrementalJoin<K, L, R>,
    left: Changes<K, L>,
    right: Changes<K, R>,
    delta: ZSet<(K, L, R)>,
}

impl<K: Ord, L: Ord, R: Ord> JoinUpdate<'_, K, L, R> {
    pub fn delta(&self) -> &ZSet<(K, L, R)> {
        &self.delta
    }
}

impl<K: Ord + Clone, L: Ord + Clone, R: Ord + Clone> JoinUpdate<'_, K, L, R> {
    pub fn commit(self) -> ZSet<(K, L, R)> {
        let Self { owner, left, right, delta } = self;
        publish_changes(&mut owner.left, left);
        publish_changes(&mut owner.right, right);
        delta
    }
}

impl<K: Ord, L: Ord, R: Ord> core::fmt::Debug for JoinUpdate<'_, K, L, R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("JoinUpdate")
            .field("delta_support", &self.delta.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

fn grouped<'a, K: Ord, V: Ord, E>(
    delta: &'a ZSet<(K, V)>,
    limbs: LimbLimit,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<Grouped<'a, K, V>, ZSetError<E>> {
    let mut groups: Grouped<'a, K, V> = BTreeMap::new();
    for ((key, value), weight) in delta.iter() {
        event(control, ZSetEvent::Work)?;
        admit(weight, limbs)?;
        let rows = match groups.entry(key) {
            Entry::Vacant(entry) => {
                event(control, ZSetEvent::ScratchEntry)?;
                entry.insert(Vec::new())
            }
            Entry::Occupied(entry) => entry.into_mut(),
        };
        event(control, ZSetEvent::ScratchEntry)?;
        rows.push((value, weight));
    }
    Ok(groups)
}

fn add_product<K: Ord + Clone, L: Ord + Clone, R: Ord + Clone, E>(
    output: &mut ZSet<(K, L, R)>,
    key: &K,
    left: (&L, &ZWeight),
    right: (&R, &ZWeight),
    limbs: LimbLimit,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<(), ZSetError<E>> {
    event(control, ZSetEvent::Work)?;
    let weight = left.1.checked_mul(right.1, limbs).map_err(ZSetError::Arithmetic)?;
    output.accumulate((key.clone(), left.0.clone(), right.0.clone()), weight, limbs, control)
}

fn prepare_changes<K: Ord + Clone, V: Ord + Clone, E>(
    base: &Arrangement<K, V>,
    groups: &Grouped<'_, K, V>,
    limbs: LimbLimit,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<Changes<K, V>, ZSetError<E>> {
    let mut prepared = BTreeMap::new();
    for (&key, changes) in groups {
        event(control, ZSetEvent::Work)?;
        let existing = base.get(key);
        event(control, ZSetEvent::ScratchEntry)?;
        if existing.is_none() {
            // Reserve a future retained group as well as this private patch.
            event(control, ZSetEvent::ScratchEntry)?;
        }
        let mut replacement = BTreeMap::new();
        for &(value, change) in changes {
            event(control, ZSetEvent::Work)?;
            let old = existing.and_then(|group| group.weight(value));
            let next = match old {
                Some(old) => old.checked_add(change, limbs),
                None => change.checked_clone(limbs),
            }
            .map_err(ZSetError::Arithmetic)?;
            if old.is_none() && !next.is_zero() {
                event(control, ZSetEvent::ScratchEntry)?;
            }
            event(control, ZSetEvent::ScratchEntry)?;
            replacement.insert(value.clone(), next);
        }
        prepared.insert(key.clone(), replacement);
    }
    Ok(prepared)
}

fn publish_changes<K: Ord, V: Ord + Clone>(base: &mut Arrangement<K, V>, changes: Changes<K, V>) {
    for (key, replacements) in changes {
        match base.entry(key) {
            Entry::Occupied(mut entry) => {
                entry.get_mut().publish(replacements);
                if entry.get().is_empty() {
                    entry.remove();
                }
            }
            Entry::Vacant(entry) => {
                let mut group = ZSet::new();
                group.publish(replacements);
                if !group.is_empty() {
                    entry.insert(group);
                }
            }
        }
    }
}

/// Incremental positive-support threshold: output weight is one exactly when
/// the integrated input weight is positive. This is bag DISTINCT for nonnegative
/// integrated bags, extended to signed Z-sets by the explicit positive test.
#[derive(PartialEq, Eq)]
pub struct IncrementalDistinct<T: Ord> {
    counts: ZSet<T>,
}

impl<T: Ord> Default for IncrementalDistinct<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Ord> IncrementalDistinct<T> {
    pub fn new() -> Self {
        Self { counts: ZSet::new() }
    }
    pub fn counts(&self) -> &ZSet<T> {
        &self.counts
    }
    pub fn contains(&self, key: &T) -> bool {
        self.counts.weight(key).is_some_and(|weight| weight > &ZWeight::ZERO)
    }
}

impl<T: Ord> core::fmt::Debug for IncrementalDistinct<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IncrementalDistinct")
            .field("counts", &self.counts)
            .finish()
    }
}

impl<T: Ord + Clone> IncrementalDistinct<T> {
    pub fn prepare<E>(
        &mut self,
        delta: &ZSet<T>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<DistinctUpdate<'_, T>, ZSetError<E>> {
        event(control, ZSetEvent::Work)?;
        let replacements = self.counts.prepare_integration(delta, limbs, control)?;
        let mut output = ZSet::new();
        for (key, next) in &replacements {
            event(control, ZSetEvent::Work)?;
            let before = self.contains(key);
            let after = next > &ZWeight::ZERO;
            if before != after {
                output.accumulate(
                    key.clone(),
                    ZWeight::from_i128(if after { 1 } else { -1 }),
                    limbs,
                    control,
                )?;
            }
        }
        event(control, ZSetEvent::Work)?;
        Ok(DistinctUpdate { owner: self, replacements, delta: output })
    }

    pub fn apply<E>(
        &mut self,
        delta: &ZSet<T>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<T>, ZSetError<E>> {
        Ok(self.prepare(delta, limbs, control)?.commit())
    }
}

#[must_use = "dropping an update aborts it"]
pub struct DistinctUpdate<'a, T: Ord> {
    owner: &'a mut IncrementalDistinct<T>,
    replacements: BTreeMap<T, ZWeight>,
    delta: ZSet<T>,
}

impl<T: Ord> DistinctUpdate<'_, T> {
    pub fn delta(&self) -> &ZSet<T> {
        &self.delta
    }
}

impl<T: Ord + Clone> DistinctUpdate<'_, T> {
    pub fn commit(self) -> ZSet<T> {
        let Self { owner, replacements, delta } = self;
        owner.counts.publish(replacements);
        delta
    }
}

impl<T: Ord> core::fmt::Debug for DistinctUpdate<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DistinctUpdate")
            .field("delta_support", &self.delta.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMBS: LimbLimit = LimbLimit::new(16);
    type TestJoin = IncrementalJoin<i32, i32, i32>;
    type Input = BTreeMap<(i32, i32), i128>;
    type Output = BTreeMap<(i32, i32, i32), i128>;

    fn allow(_: ZSetEvent) -> Result<(), usize> { Ok(()) }

    fn z<T: Ord + Clone>(rows: &[(T, i128)]) -> ZSet<T> {
        ZSet::from_updates(
            rows.iter().map(|(key, weight)| (key.clone(), ZWeight::from_i128(*weight))),
            LIMBS,
            &mut allow,
        ).unwrap()
    }

    fn plain<T: Ord + Clone>(value: &ZSet<T>) -> BTreeMap<T, i128> {
        value.iter().map(|(key, weight)| (key.clone(), weight.to_i128().unwrap())).collect()
    }

    // Independent full nested-loop oracle using primitive signed integers,
    // not the operator's grouped access, staging or multiplication helper.
    fn oracle(left: &Input, right: &Input) -> Output {
        let mut out = Output::new();
        for (&(key, l), &lw) in left {
            for (&(other, r), &rw) in right {
                if key == other && lw * rw != 0 {
                    out.insert((key, l, r), lw * rw);
                }
            }
        }
        out
    }

    fn seeded() -> TestJoin {
        let mut join = TestJoin::new();
        join.apply(
            &z(&[((1, 2), 2), ((2, 3), 1)]),
            &z(&[((1, 4), 3), ((3, 5), 1)]),
            LIMBS,
            &mut allow,
        ).unwrap();
        join
    }

    #[test]
    fn simultaneous_insertions_deletions_and_signed_weights_match_full_recomputation() {
        for a in -2..=2 {
            for b in -2..=2 {
                for da in -2..=2 {
                    for db in -2..=2 {
                        let left = z(&[((1, 10), a), ((2, 20), 2)]);
                        let right = z(&[((1, 30), b), ((3, 40), -3)]);
                        let dl = z(&[((1, 10), da), ((2, 21), -1)]);
                        let dr = z(&[((1, 30), db), ((2, 41), 3)]);
                        let mut state = TestJoin::new();
                        let initial = state.apply(&left, &right, LIMBS, &mut allow).unwrap();
                        assert_eq!(plain(&initial), oracle(&plain(&left), &plain(&right)));
                        let delta = state.apply(&dl, &dr, LIMBS, &mut allow).unwrap();
                        let new_left = left.plus(&dl, LIMBS, &mut allow).unwrap();
                        let new_right = right.plus(&dr, LIMBS, &mut allow).unwrap();
                        let expected = oracle(&plain(&new_left), &plain(&new_right));
                        let mut actual = initial;
                        actual.integrate(&delta, LIMBS, &mut allow).unwrap();
                        assert_eq!(plain(&actual), expected);
                        assert_eq!(plain(&state.snapshot(LIMBS, &mut allow).unwrap()), expected);
                    }
                }
            }
        }
    }

    #[test]
    fn removing_both_sides_retracts_each_join_occurrence_once_and_removes_empty_groups() {
        let left = z(&[((1, 2), 2)]);
        let right = z(&[((1, 3), 3)]);
        let mut state = TestJoin::new();
        assert_eq!(plain(&state.apply(&left, &right, LIMBS, &mut allow).unwrap()),
            BTreeMap::from([((1, 2, 3), 6)]));
        let delta = state.apply(
            &left.negated(LIMBS, &mut allow).unwrap(),
            &right.negated(LIMBS, &mut allow).unwrap(),
            LIMBS,
            &mut allow,
        ).unwrap();
        assert_eq!(plain(&delta), BTreeMap::from([((1, 2, 3), -6)]));
        assert_eq!(state, TestJoin::new());
    }

    #[test]
    fn failed_or_dropped_preparations_leave_both_arrangements_unchanged_and_retryable() {
        let dl = z(&[((1, 2), -2), ((4, 6), 1)]);
        let dr = z(&[((1, 4), -3), ((4, 7), 1)]);
        let mut success = seeded();
        let mut calls = 0;
        let wanted = success.apply(&dl, &dr, LIMBS, &mut |_| {
            calls += 1;
            Ok::<_, usize>(())
        }).unwrap();
        for stop in 1..=calls {
            let mut state = seeded();
            let mut seen = 0;
            let result = state.apply(&dl, &dr, LIMBS, &mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            });
            assert_eq!(result, Err(ZSetError::Control(stop)));
            assert_eq!(state, seeded());
            assert_eq!(state.apply(&dl, &dr, LIMBS, &mut allow).unwrap(), wanted);
            assert_eq!(state, success);
        }
        let mut state = seeded();
        {
            let pending = state.prepare(&dl, &dr, LIMBS, &mut allow).unwrap();
            assert_eq!(pending.delta(), &wanted);
            // A downstream failure drops this guard instead of publishing.
        }
        assert_eq!(state, seeded());
    }

    #[test]
    fn join_weight_promotion_and_arithmetic_refusal_are_atomic() {
        let left = z(&[((1, 2), i128::MAX)]);
        let right = z(&[((1, 3), 2)]);
        let mut state = TestJoin::new();
        assert!(matches!(
            state.apply(&left, &right, LimbLimit::new(0), &mut allow),
            Err(ZSetError::Arithmetic(_))
        ));
        assert_eq!(state, TestJoin::new());
        let output = state.apply(&left, &right, LIMBS, &mut allow).unwrap();
        assert!(output.weight(&(1, 2, 3)).unwrap().is_promoted());
        let removed = state.apply(
            &left.negated(LIMBS, &mut allow).unwrap(), &ZSet::new(), LIMBS, &mut allow,
        ).unwrap();
        assert!(output.plus(&removed, LIMBS, &mut allow).unwrap().is_empty());
    }

    #[test]
    fn changing_one_join_key_does_not_visit_unrelated_arrangements() {
        let dl = z(&[((1, 2), 1)]);
        let mut small = seeded();
        let mut large = seeded();
        let unrelated: Vec<_> = (100..1100).map(|key| ((key, 9), 1)).collect();
        large.apply(&z(&unrelated), &z(&unrelated), LIMBS, &mut allow).unwrap();
        let mut measured = Vec::new();
        for state in [&mut small, &mut large] {
            let mut events = Vec::new();
            let output = state.apply(&dl, &ZSet::new(), LIMBS, &mut |event| {
                events.push(event);
                Ok::<_, usize>(())
            }).unwrap();
            measured.push((output, events));
        }
        assert_eq!(measured[0], measured[1]);
    }

    #[test]
    fn distinct_tracks_integrated_multiplicity_not_the_sign_of_each_delta() {
        for old in -3..=3 {
            for change in -3..=3 {
                let mut state = IncrementalDistinct::new();
                state.apply(&z(&[(7, old)]), LIMBS, &mut allow).unwrap();
                let output = state.apply(&z(&[(7, change)]), LIMBS, &mut allow).unwrap();
                let expected = i128::from(old + change > 0) - i128::from(old > 0);
                assert_eq!(output, z(&[(7, expected)]));
                assert_eq!(state.contains(&7), old + change > 0);
                assert_eq!(plain(state.counts()), plain(&z(&[(7, old + change)])));
            }
        }
    }

    #[test]
    fn distinct_failure_does_not_publish_counts_or_support() {
        let seed = z(&[(1, 2), (2, 1)]);
        let delta = z(&[(1, -1), (2, -1), (3, 4)]);
        let mut success = IncrementalDistinct::new();
        success.apply(&seed, LIMBS, &mut allow).unwrap();
        let mut calls = 0;
        let expected = success.apply(&delta, LIMBS, &mut |_| {
            calls += 1;
            Ok::<_, usize>(())
        }).unwrap();
        assert_eq!(expected, z(&[(2, -1), (3, 1)]));
        for stop in 1..=calls {
            let mut state = IncrementalDistinct::new();
            state.apply(&seed, LIMBS, &mut allow).unwrap();
            let mut seen = 0;
            assert_eq!(state.apply(&delta, LIMBS, &mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            }), Err(ZSetError::Control(stop)));
            assert_eq!(state.counts(), &seed);
            assert_eq!(state.apply(&delta, LIMBS, &mut allow).unwrap(), expected);
            assert_eq!(state, success);
        }
    }
}
