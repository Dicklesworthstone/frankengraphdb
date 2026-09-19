//! Incremental bag semijoin and antijoin, driven by exact witness counts.
//!
//! Right input is the counted projection onto the join key, not a set of
//! changed keys. Losing one of several witnesses must not retract existence.
//! Left multiplicity is preserved, never multiplied by the witness count.
//! Both input bags must remain nonnegative; signed deltas are admitted only as
//! whole ticks. Keys use the caller's canonical equality: GQL null-key filtering
//! and correlated predicate evaluation belong to the producer, not this kernel.
//!
//! This shares the existing arranged-input patches and prepare/commit protocol.
//! It is an in-memory algebra operator, not durable subscription delivery.

use super::{
    Arrangement, Changes, IncrementalJoin, JoinUpdate, LimbLimit, ZSet, ZSetError, ZSetEvent,
    ZWeight, event, grouped, prepare_changes, publish_changes,
};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresenceMode {
    Exists,
    NotExists,
}

impl PresenceMode {
    fn keeps(self, count: Option<&ZWeight>) -> bool {
        let present = count.is_some_and(|count| count > &ZWeight::ZERO);
        match self {
            Self::Exists => present,
            Self::NotExists => !present,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BagInput {
    Left,
    Right,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BagJoinError<E> {
    ZSet(ZSetError<E>),
    /// Diagnostics reveal the input side, never the key, payload or count.
    NegativeMultiplicity {
        input: BagInput,
    },
}

impl<E> From<ZSetError<E>> for BagJoinError<E> {
    fn from(error: ZSetError<E>) -> Self {
        Self::ZSet(error)
    }
}
impl<E: core::fmt::Display> core::fmt::Display for BagJoinError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ZSet(error) => error.fmt(f),
            Self::NegativeMultiplicity { input } => {
                write!(f, "negative integrated {input:?} join multiplicity")
            }
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for BagJoinError<E> {}

/// Semi/antijoin of `(key, left_value)` against a bag of right keys.
/// Mode is fixed at construction so a materialized output cannot silently
/// change its interpretation between ticks. The right projection may be fed
/// by another prepared operator; do not threshold its delta before passing it.
#[derive(PartialEq, Eq)]
pub struct IncrementalPresence<K: Ord, L: Ord> {
    mode: PresenceMode,
    left: Arrangement<K, L>,
    witnesses: ZSet<K>,
}

impl<K: Ord, L: Ord> IncrementalPresence<K, L> {
    pub fn new(mode: PresenceMode) -> Self {
        Self {
            mode,
            left: BTreeMap::new(),
            witnesses: ZSet::new(),
        }
    }
    pub fn mode(&self) -> PresenceMode {
        self.mode
    }
    pub fn left_weight(&self, key: &K, value: &L) -> Option<&ZWeight> {
        self.left.get(key).and_then(|group| group.weight(value))
    }
    pub fn witness_counts(&self) -> &ZSet<K> {
        &self.witnesses
    }
    pub fn left_rows(&self) -> impl Iterator<Item = (&K, &L, &ZWeight)> {
        self.left.iter().flat_map(|(key, group)| {
            group
                .iter()
                .map(move |(value, weight)| (key, value, weight))
        })
    }
}

impl<K: Ord, L: Ord> core::fmt::Debug for IncrementalPresence<K, L> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IncrementalPresence")
            .field("mode", &self.mode)
            .field("left_keys", &self.left.len())
            .field("witness_keys", &self.witnesses.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

impl<K: Ord + Clone, L: Ord + Clone> IncrementalPresence<K, L> {
    pub fn prepare<E>(
        &mut self,
        delta_left: &ZSet<(K, L)>,
        delta_witnesses: &ZSet<K>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<PresenceUpdate<'_, K, L>, BagJoinError<E>> {
        event(control, ZSetEvent::Work)?;
        let grouped = grouped(delta_left, limbs, control)?;
        let left = prepare_changes(&self.left, &grouped, limbs, control)?;
        validate_changes(&left, BagInput::Left, control)?;
        let witnesses = prepare_witnesses(&self.witnesses, delta_witnesses, limbs, control)?;
        let delta = presence_delta(
            &self.left,
            &self.witnesses,
            &witnesses,
            delta_left,
            self.mode,
            limbs,
            control,
        )?;
        event(control, ZSetEvent::Work)?;
        Ok(PresenceUpdate {
            owner: self,
            left,
            witnesses,
            delta,
        })
    }

    pub fn apply<E>(
        &mut self,
        delta_left: &ZSet<(K, L)>,
        delta_witnesses: &ZSet<K>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<(K, L)>, BagJoinError<E>> {
        Ok(self
            .prepare(delta_left, delta_witnesses, limbs, control)?
            .commit())
    }

    /// Explicit recomputation for snapshots/audits, not part of a normal tick.
    pub fn snapshot<E>(
        &self,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<(K, L)>, BagJoinError<E>> {
        let mut output = ZSet::new();
        event(control, ZSetEvent::Work)?;
        for (key, group) in &self.left {
            event(control, ZSetEvent::Work)?;
            if self.mode.keeps(self.witnesses.weight(key)) {
                for (value, weight) in group.iter() {
                    event(control, ZSetEvent::Work)?;
                    let weight = weight.checked_clone(limbs).map_err(ZSetError::Arithmetic)?;
                    output.accumulate((key.clone(), value.clone()), weight, limbs, control)?;
                }
            }
        }
        Ok(output)
    }
}

#[must_use = "dropping a presence update aborts both inputs"]
pub struct PresenceUpdate<'a, K: Ord, L: Ord> {
    owner: &'a mut IncrementalPresence<K, L>,
    left: Changes<K, L>,
    witnesses: BTreeMap<K, ZWeight>,
    delta: ZSet<(K, L)>,
}

impl<K: Ord, L: Ord> PresenceUpdate<'_, K, L> {
    pub fn delta(&self) -> &ZSet<(K, L)> {
        &self.delta
    }
}
impl<K: Ord + Clone, L: Ord + Clone> PresenceUpdate<'_, K, L> {
    /// Publish only after downstream preparation succeeds. No recoverable
    /// arithmetic or callback remains. As in the parent API, panics in generic
    /// key code and standard collection allocation are outside this contract.
    pub fn commit(self) -> ZSet<(K, L)> {
        let Self {
            owner,
            left,
            witnesses,
            delta,
        } = self;
        publish_changes(&mut owner.left, left);
        owner.witnesses.publish(witnesses);
        delta
    }
}
impl<K: Ord, L: Ord> core::fmt::Debug for PresenceUpdate<'_, K, L> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PresenceUpdate")
            .field("delta_support", &self.delta.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

pub(super) fn validate_changes<K: Ord, V: Ord, E>(
    changes: &Changes<K, V>,
    input: BagInput,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<(), BagJoinError<E>> {
    for group in changes.values() {
        event(control, ZSetEvent::Work)?;
        for weight in group.values() {
            event(control, ZSetEvent::Work)?;
            if weight < &ZWeight::ZERO {
                return Err(BagJoinError::NegativeMultiplicity { input });
            }
        }
    }
    Ok(())
}

pub(super) fn prepare_witnesses<K: Ord + Clone, E>(
    before: &ZSet<K>,
    delta: &ZSet<K>,
    limbs: LimbLimit,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<BTreeMap<K, ZWeight>, BagJoinError<E>> {
    let replacements = before.prepare_integration(delta, limbs, control)?;
    for (key, next) in &replacements {
        event(control, ZSetEvent::Work)?;
        if next < &ZWeight::ZERO {
            return Err(BagJoinError::NegativeMultiplicity {
                input: BagInput::Right,
            });
        }
        if before.weight(key).is_none() && !next.is_zero() {
            // Reserve the future retained count as well as the staged entry.
            event(control, ZSetEvent::ScratchEntry)?;
        }
    }
    Ok(replacements)
}

/// For keep indicator h, use delta_left*h(after) + left_before*(h(after)-h(before)).
/// The after indicator includes this tick's right changes. This is the cross
/// term needed when a left update and the first/last witness change together.
/// Never expand the Cartesian join merely to discover that a witness exists.
/// Unchanged indicators do not visit ANY old left rows, even at a changed key.
pub(super) fn presence_delta<K: Ord + Clone, L: Ord + Clone, E>(
    left: &Arrangement<K, L>,
    witnesses: &ZSet<K>,
    replacements: &BTreeMap<K, ZWeight>,
    delta_left: &ZSet<(K, L)>,
    mode: PresenceMode,
    limbs: LimbLimit,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<ZSet<(K, L)>, BagJoinError<E>> {
    let mut output = ZSet::new();
    for ((key, value), change) in delta_left.iter() {
        event(control, ZSetEvent::Work)?;
        // A zero replacement is authoritative, not a missing lookup that may
        // fall back to an old positive witness count.
        let after = replacements.get(key).or_else(|| witnesses.weight(key));
        if mode.keeps(after) {
            let change = change.checked_clone(limbs).map_err(ZSetError::Arithmetic)?;
            output.accumulate((key.clone(), value.clone()), change, limbs, control)?;
        }
    }
    for (key, next) in replacements {
        event(control, ZSetEvent::Work)?;
        let after = mode.keeps(Some(next));
        if mode.keeps(witnesses.weight(key)) == after {
            continue;
        }
        if let Some(group) = left.get(key) {
            for (value, weight) in group.iter() {
                event(control, ZSetEvent::Work)?;
                let change = if after {
                    weight.checked_clone(limbs)
                } else {
                    weight.checked_neg(limbs)
                }
                .map_err(ZSetError::Arithmetic)?;
                output.accumulate((key.clone(), value.clone()), change, limbs, control)?;
            }
        }
    }
    Ok(output)
}

/// The outer Option distinguishes an unmatched row from ANY real right value.
/// For example, R = Option<T> represents a matching null payload as Some(None),
/// while the left-join null extension is None. Neither is a fabricated key.
pub type LeftJoinDelta<K, L, R> = ZSet<(K, L, Option<R>)>;

/// Exact incremental left outer equijoin of nonnegative integrated bags.
/// One ordinary arranged join owns both input relations. The only additional
/// retained state is a right witness count per key; the left arrangement is
/// not duplicated in a second antijoin. Null-key semantics belong to the caller
/// just as for IncrementalPresence. This does not implement arbitrary ON filters.
#[derive(PartialEq, Eq)]
pub struct IncrementalLeftJoin<K: Ord, L: Ord, R: Ord> {
    joined: IncrementalJoin<K, L, R>,
    witnesses: ZSet<K>,
}

impl<K: Ord, L: Ord, R: Ord> Default for IncrementalLeftJoin<K, L, R> {
    fn default() -> Self {
        Self::new()
    }
}
impl<K: Ord, L: Ord, R: Ord> IncrementalLeftJoin<K, L, R> {
    pub fn new() -> Self {
        Self {
            joined: IncrementalJoin::new(),
            witnesses: ZSet::new(),
        }
    }
    pub fn left_weight(&self, key: &K, value: &L) -> Option<&ZWeight> {
        self.joined.left_weight(key, value)
    }
    pub fn right_weight(&self, key: &K, value: &R) -> Option<&ZWeight> {
        self.joined.right_weight(key, value)
    }
    pub fn witness_counts(&self) -> &ZSet<K> {
        &self.witnesses
    }
}
impl<K: Ord, L: Ord, R: Ord> core::fmt::Debug for IncrementalLeftJoin<K, L, R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IncrementalLeftJoin")
            .field("inputs", &self.joined)
            .field("witness_keys", &self.witnesses.len())
            .finish()
    }
}

impl<K: Ord + Clone, L: Ord + Clone, R: Ord + Clone> IncrementalLeftJoin<K, L, R> {
    /// Prepare matched and unmatched changes against the SAME old input state.
    /// Right projection uses signed weights, so replacement of the only witness
    /// in one tick does not briefly publish an unmatched row. Validate each raw
    /// input multiplicity, not only the projected total: compensating invalid
    /// retractions cannot hide behind another right value at the same key.
    pub fn prepare<E>(
        &mut self,
        delta_left: &ZSet<(K, L)>,
        delta_right: &ZSet<(K, R)>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<LeftJoinUpdate<'_, K, L, R>, BagJoinError<E>> {
        event(control, ZSetEvent::Work)?;
        let delta_counts = delta_right.map(|(key, _)| Ok(key.clone()), limbs, control)?;
        let replacements = prepare_witnesses(&self.witnesses, &delta_counts, limbs, control)?;
        let mut joined = self
            .joined
            .prepare(delta_left, delta_right, limbs, control)?;
        validate_changes(&joined.left, BagInput::Left, control)?;
        validate_changes(&joined.right, BagInput::Right, control)?;
        let unmatched = presence_delta(
            &joined.owner.left,
            &self.witnesses,
            &replacements,
            delta_left,
            PresenceMode::NotExists,
            limbs,
            control,
        )?;
        // Transfer the matched delta into its nullable shape rather than
        // retaining another clone of its keys and potentially promoted weights.
        let mut delta = lift_matches(core::mem::take(&mut joined.delta), limbs, control)?;
        for ((key, left), weight) in unmatched.into_updates() {
            event(control, ZSetEvent::Work)?;
            delta.accumulate((key, left, None), weight, limbs, control)?;
        }
        event(control, ZSetEvent::Work)?;
        Ok(LeftJoinUpdate {
            joined,
            witnesses: &mut self.witnesses,
            replacements,
            delta,
        })
    }

    pub fn apply<E>(
        &mut self,
        delta_left: &ZSet<(K, L)>,
        delta_right: &ZSet<(K, R)>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<LeftJoinDelta<K, L, R>, BagJoinError<E>> {
        Ok(self
            .prepare(delta_left, delta_right, limbs, control)?
            .commit())
    }

    /// Explicit output snapshot. Incremental ticks never invoke this full scan.
    pub fn snapshot<E>(
        &self,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<LeftJoinDelta<K, L, R>, BagJoinError<E>> {
        event(control, ZSetEvent::Work)?;
        let matched = self.joined.snapshot(limbs, control)?;
        let mut output = lift_matches(matched, limbs, control)?;
        for (key, group) in &self.joined.left {
            event(control, ZSetEvent::Work)?;
            if self.witnesses.weight(key).is_some() {
                continue;
            }
            for (left, weight) in group.iter() {
                event(control, ZSetEvent::Work)?;
                let weight = weight.checked_clone(limbs).map_err(ZSetError::Arithmetic)?;
                output.accumulate((key.clone(), left.clone(), None), weight, limbs, control)?;
            }
        }
        Ok(output)
    }
}

fn lift_matches<K: Ord, L: Ord, R: Ord, E>(
    matched: ZSet<(K, L, R)>,
    limbs: LimbLimit,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<LeftJoinDelta<K, L, R>, BagJoinError<E>> {
    let mut output = ZSet::new();
    for ((key, left, right), weight) in matched.into_updates() {
        event(control, ZSetEvent::Work)?;
        output.accumulate((key, left, Some(right)), weight, limbs, control)?;
    }
    Ok(output)
}

#[must_use = "dropping a left join update aborts both inputs and witness counts"]
pub struct LeftJoinUpdate<'a, K: Ord, L: Ord, R: Ord> {
    joined: JoinUpdate<'a, K, L, R>,
    witnesses: &'a mut ZSet<K>,
    replacements: BTreeMap<K, ZWeight>,
    delta: LeftJoinDelta<K, L, R>,
}
impl<K: Ord, L: Ord, R: Ord> LeftJoinUpdate<'_, K, L, R> {
    pub fn delta(&self) -> &LeftJoinDelta<K, L, R> {
        &self.delta
    }
}
impl<K: Ord + Clone, L: Ord + Clone, R: Ord + Clone> LeftJoinUpdate<'_, K, L, R> {
    /// All participants must prepare before any commit. The same no-recoverable-
    /// failure boundary as the underlying join/Z-set guards applies here.
    pub fn commit(self) -> LeftJoinDelta<K, L, R> {
        let Self {
            joined,
            witnesses,
            replacements,
            delta,
        } = self;
        let _ = joined.commit();
        witnesses.publish(replacements);
        delta
    }
}
impl<K: Ord, L: Ord, R: Ord> core::fmt::Debug for LeftJoinUpdate<'_, K, L, R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LeftJoinUpdate")
            .field("delta_support", &self.delta.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMBS: LimbLimit = LimbLimit::new(16);
    type Operator = IncrementalPresence<i32, i32>;

    fn allow(_: ZSetEvent) -> Result<(), usize> {
        Ok(())
    }
    fn z<T: Ord + Clone>(rows: &[(T, i128)]) -> ZSet<T> {
        ZSet::from_updates(
            rows.iter()
                .map(|(key, count)| (key.clone(), ZWeight::from_i128(*count))),
            LIMBS,
            &mut allow,
        )
        .unwrap()
    }
    fn seed(mode: PresenceMode) -> Operator {
        let mut state = Operator::new(mode);
        state
            .apply(
                &z(&[((1, 10), 2), ((1, 11), 1), ((2, 20), 3)]),
                &z(&[(1, 2)]),
                LIMBS,
                &mut allow,
            )
            .unwrap();
        state
    }

    #[test]
    fn every_small_bag_transition_matches_full_recomputation_and_partition_law() {
        for old in 0..27 {
            for new in 0..27 {
                let (a, b, r) = (old % 3, old / 3 % 3, old / 9);
                let (na, nb, nr) = (new % 3, new / 3 % 3, new / 9);
                let dl = z(&[((1, 10), na - a), ((1, 11), nb - b)]);
                let mut partition = ZSet::new();
                for mode in [PresenceMode::Exists, PresenceMode::NotExists] {
                    let mut state = Operator::new(mode);
                    state
                        .apply(
                            &z(&[((1, 10), a), ((1, 11), b)]),
                            &z(&[(1, r)]),
                            LIMBS,
                            &mut allow,
                        )
                        .unwrap();
                    let delta = state
                        .apply(&dl, &z(&[(1, nr - r)]), LIMBS, &mut allow)
                        .unwrap();
                    let selected = |w| i128::from((w > 0) == (mode == PresenceMode::Exists));
                    assert_eq!(
                        delta,
                        z(&[
                            ((1, 10), na * selected(nr) - a * selected(r)),
                            ((1, 11), nb * selected(nr) - b * selected(r)),
                        ])
                    );
                    assert_eq!(
                        state.snapshot(LIMBS, &mut allow).unwrap(),
                        z(&[((1, 10), na * selected(nr)), ((1, 11), nb * selected(nr))])
                    );
                    assert_eq!(state.witness_counts(), &z(&[(1, nr)]));
                    partition.integrate(&delta, LIMBS, &mut allow).unwrap();
                }
                assert_eq!(partition, dl);
            }
        }
    }

    #[test]
    fn silent_witness_and_left_updates_are_retained_until_the_last_witness_leaves() {
        for mode in [PresenceMode::Exists, PresenceMode::NotExists] {
            let mut state = seed(mode);
            assert!(
                state
                    .apply(&ZSet::new(), &z(&[(1, -1)]), LIMBS, &mut allow)
                    .unwrap()
                    .is_empty()
            );
            let changed = state
                .apply(&z(&[((1, 10), -1)]), &ZSet::new(), LIMBS, &mut allow)
                .unwrap();
            let sign = if mode == PresenceMode::Exists { -1 } else { 1 };
            assert_eq!(
                changed,
                if mode == PresenceMode::Exists {
                    z(&[((1, 10), -1)])
                } else {
                    ZSet::new()
                }
            );
            assert_eq!(
                state
                    .apply(&ZSet::new(), &z(&[(1, -1)]), LIMBS, &mut allow)
                    .unwrap(),
                z(&[((1, 10), sign), ((1, 11), sign)])
            );
            assert!(state.witness_counts().is_empty());
        }
    }

    #[test]
    fn all_refusals_and_dropped_downstream_preparations_preserve_both_inputs() {
        let dl = z(&[((1, 10), -2), ((2, 21), 4)]);
        let dr = z(&[(1, -2), (2, 1)]);
        for mode in [PresenceMode::Exists, PresenceMode::NotExists] {
            let mut success = seed(mode);
            let mut calls = 0;
            let expected = success
                .apply(&dl, &dr, LIMBS, &mut |_| {
                    calls += 1;
                    Ok::<_, usize>(())
                })
                .unwrap();
            for stop in 1..=calls {
                let mut state = seed(mode);
                let mut seen = 0;
                assert_eq!(
                    state.apply(&dl, &dr, LIMBS, &mut |_| {
                        seen += 1;
                        if seen == stop { Err(stop) } else { Ok(()) }
                    }),
                    Err(BagJoinError::ZSet(ZSetError::Control(stop)))
                );
                assert_eq!(seen, stop);
                assert_eq!(state, seed(mode));
                assert_eq!(state.apply(&dl, &dr, LIMBS, &mut allow).unwrap(), expected);
                assert_eq!(state, success);
            }
            let mut state = seed(mode);
            let mut sink = state.snapshot(LIMBS, &mut allow).unwrap();
            let before = sink.checked_clone(LIMBS, &mut allow).unwrap();
            {
                let pending = state.prepare(&dl, &dr, LIMBS, &mut allow).unwrap();
                assert_eq!(pending.delta(), &expected);
                let _sink_update = sink
                    .prepare_update(pending.delta(), LIMBS, &mut allow)
                    .unwrap();
                // Failure in a later operator drops both preparations.
            }
            assert_eq!(state, seed(mode));
            assert_eq!(sink, before);
        }
    }

    #[test]
    fn invalid_bags_refuse_even_when_the_bad_input_would_not_be_visible() {
        for mode in [PresenceMode::Exists, PresenceMode::NotExists] {
            for (dl, dr, input) in [
                (
                    z(&[((99, 10), -1), ((2, 21), 1)]),
                    z(&[(2, 1)]),
                    BagInput::Left,
                ),
                (z(&[((1, 10), -3)]), ZSet::new(), BagInput::Left),
                (z(&[((2, 21), 1)]), z(&[(1, -3), (2, 1)]), BagInput::Right),
            ] {
                let mut state = seed(mode);
                assert_eq!(
                    state.apply(&dl, &dr, LIMBS, &mut allow),
                    Err(BagJoinError::NegativeMultiplicity { input })
                );
                assert_eq!(state, seed(mode));
            }
        }
    }

    #[test]
    fn stable_presence_skips_same_key_fanout_and_unrelated_groups() {
        for mode in [PresenceMode::Exists, PresenceMode::NotExists] {
            let mut small = seed(mode);
            let mut large = seed(mode);
            let extras: Vec<_> = (100..1100)
                .flat_map(|value| [((1, value), 1), ((value, 10), 1)])
                .collect();
            large
                .apply(&z(&extras), &ZSet::new(), LIMBS, &mut allow)
                .unwrap();
            let mut measured = Vec::new();
            for state in [&mut small, &mut large] {
                let mut events = Vec::new();
                let delta = state
                    .apply(&ZSet::new(), &z(&[(1, 1)]), LIMBS, &mut |event| {
                        events.push(event);
                        Ok::<_, usize>(())
                    })
                    .unwrap();
                measured.push((delta, events));
            }
            assert_eq!(measured[0], measured[1]);
            assert!(measured[0].0.is_empty());
        }
    }

    #[test]
    fn bigint_witness_promotion_refusal_and_demotion_do_not_change_presence_early() {
        let mut state = Operator::new(PresenceMode::Exists);
        state
            .apply(
                &z(&[((1, 10), i128::MAX)]),
                &z(&[(1, i128::MAX)]),
                LIMBS,
                &mut allow,
            )
            .unwrap();
        assert!(matches!(
            state.apply(&ZSet::new(), &z(&[(1, 1)]), LimbLimit::new(0), &mut allow),
            Err(BagJoinError::ZSet(ZSetError::Arithmetic(_)))
        ));
        assert_eq!(state.witness_counts(), &z(&[(1, i128::MAX)]));
        assert!(
            state
                .apply(&ZSet::new(), &z(&[(1, 1)]), LIMBS, &mut allow)
                .unwrap()
                .is_empty()
        );
        assert!(state.witness_counts().weight(&1).unwrap().is_promoted());
        assert!(
            state
                .apply(&ZSet::new(), &z(&[(1, -1)]), LIMBS, &mut allow)
                .unwrap()
                .is_empty()
        );
        assert!(!state.witness_counts().weight(&1).unwrap().is_promoted());
        assert_eq!(
            state
                .apply(&ZSet::new(), &z(&[(1, -i128::MAX)]), LIMBS, &mut allow)
                .unwrap(),
            z(&[((1, 10), -i128::MAX)])
        );
    }

    #[test]
    fn exact_event_limits_pass_and_one_below_refuses_without_publication() {
        let dl = z(&[((2, 21), 1)]);
        let dr = z(&[(1, -2), (2, 1)]);
        let mut measured = seed(PresenceMode::NotExists);
        let (mut work, mut scratch) = (0, 0);
        let expected = measured
            .apply(&dl, &dr, LIMBS, &mut |event| {
                work += 1;
                scratch += usize::from(event == ZSetEvent::ScratchEntry);
                Ok::<_, usize>(())
            })
            .unwrap();
        for (max_work, max_scratch) in [(work, scratch), (work - 1, scratch), (work, scratch - 1)] {
            let mut state = seed(PresenceMode::NotExists);
            let (mut used, mut grown) = (0, 0);
            let result = state.apply(&dl, &dr, LIMBS, &mut |event| {
                used += 1;
                grown += usize::from(event == ZSetEvent::ScratchEntry);
                if used > max_work || grown > max_scratch {
                    Err(17)
                } else {
                    Ok(())
                }
            });
            if max_work == work && max_scratch == scratch {
                assert_eq!(result.unwrap(), expected);
                assert_eq!(state, measured);
            } else {
                assert_eq!(result, Err(BagJoinError::ZSet(ZSetError::Control(17))));
                assert_eq!(state, seed(PresenceMode::NotExists));
            }
        }
    }
}

#[cfg(test)]
mod outer_tests {
    use super::*;

    const LIMBS: LimbLimit = LimbLimit::new(16);
    type Operator = IncrementalLeftJoin<i32, i32, i32>;
    type Input = BTreeMap<(i32, i32), i128>;
    type Output = BTreeMap<(i32, i32, Option<i32>), i128>;
    fn allow(_: ZSetEvent) -> Result<(), usize> {
        Ok(())
    }
    fn z<T: Ord + Clone>(rows: &[(T, i128)]) -> ZSet<T> {
        ZSet::from_updates(
            rows.iter()
                .map(|(k, w)| (k.clone(), ZWeight::from_i128(*w))),
            LIMBS,
            &mut allow,
        )
        .unwrap()
    }
    fn plain<T: Ord + Clone>(rows: &ZSet<T>) -> BTreeMap<T, i128> {
        rows.iter()
            .map(|(key, weight)| (key.clone(), weight.to_i128().unwrap()))
            .collect()
    }
    // Independent whole-bag nested-loop definition, with no witness totals or
    // three-term derivative. None appears exactly when this left row has no match.
    fn oracle(left: &Input, right: &Input) -> Output {
        let mut out = BTreeMap::new();
        for (&(key, l), &lw) in left {
            let mut found = false;
            for (&(other, r), &rw) in right {
                if key == other && rw > 0 {
                    out.insert((key, l, Some(r)), lw * rw);
                    found = true;
                }
            }
            if !found {
                out.insert((key, l, None), lw);
            }
        }
        out
    }
    fn seed() -> Operator {
        let mut state = Operator::new();
        state
            .apply(
                &z(&[((1, 10), 2), ((2, 20), 3)]),
                &z(&[((1, 30), 2)]),
                LIMBS,
                &mut allow,
            )
            .unwrap();
        state
    }

    #[test]
    fn all_small_simultaneous_changes_equal_the_difference_of_full_outer_joins() {
        for old in 0..81_i128 {
            for new in 0..81_i128 {
                let left = z(&[((1, 10), old % 3), ((1, 11), old / 3 % 3)]);
                let right = z(&[((1, 30), old / 9 % 3), ((1, 31), old / 27)]);
                let next_left = z(&[((1, 10), new % 3), ((1, 11), new / 3 % 3)]);
                let next_right = z(&[((1, 30), new / 9 % 3), ((1, 31), new / 27)]);
                let mut state = Operator::new();
                let mut materialized = state.apply(&left, &right, LIMBS, &mut allow).unwrap();
                assert_eq!(plain(&materialized), oracle(&plain(&left), &plain(&right)));
                let delta = state
                    .apply(
                        &next_left.minus(&left, LIMBS, &mut allow).unwrap(),
                        &next_right.minus(&right, LIMBS, &mut allow).unwrap(),
                        LIMBS,
                        &mut allow,
                    )
                    .unwrap();
                materialized.integrate(&delta, LIMBS, &mut allow).unwrap();
                let expected = oracle(&plain(&next_left), &plain(&next_right));
                assert_eq!(plain(&materialized), expected, "old={old}, new={new}");
                assert_eq!(plain(&state.snapshot(LIMBS, &mut allow).unwrap()), expected);
            }
        }
    }

    #[test]
    fn replacing_witnesses_or_deleting_both_inputs_does_not_invent_a_null_row() {
        let mut state = seed();
        let delta = state
            .apply(
                &ZSet::new(),
                &z(&[((1, 30), -2), ((1, 31), 2)]),
                LIMBS,
                &mut allow,
            )
            .unwrap();
        assert_eq!(delta, z(&[((1, 10, Some(30)), -4), ((1, 10, Some(31)), 4)]));
        let removed = state
            .apply(
                &z(&[((1, 10), -2)]),
                &z(&[((1, 31), -2)]),
                LIMBS,
                &mut allow,
            )
            .unwrap();
        assert_eq!(removed, z(&[((1, 10, Some(31)), -4)]));
        assert_eq!(
            state.snapshot(LIMBS, &mut allow).unwrap(),
            z(&[((2, 20, None), 3)])
        );
        state
            .apply(&z(&[((2, 20), -3)]), &ZSet::new(), LIMBS, &mut allow)
            .unwrap();
        assert_eq!(state, Operator::new());
    }

    #[test]
    fn a_matching_null_payload_is_not_an_unmatched_row() {
        let mut state = IncrementalLeftJoin::<i32, i32, Option<i32>>::new();
        let left = z(&[((1, 10), 2)]);
        let right = z(&[((1, None), 3)]);
        assert_eq!(
            state.apply(&left, &right, LIMBS, &mut allow).unwrap(),
            z(&[((1, 10, Some(None)), 6)])
        );
        let delta = state
            .apply(
                &ZSet::new(),
                &right.negated(LIMBS, &mut allow).unwrap(),
                LIMBS,
                &mut allow,
            )
            .unwrap();
        assert_eq!(delta, z(&[((1, 10, Some(None)), -6), ((1, 10, None), 2)]));
    }

    #[test]
    fn invalid_per_tuple_retractions_cannot_hide_behind_a_valid_key_total() {
        for (left, right, input) in [
            (
                ZSet::new(),
                z(&[((1, 30), -3), ((1, 31), 3)]),
                BagInput::Right,
            ),
            (
                z(&[((2, 20), -4), ((2, 21), 4)]),
                ZSet::new(),
                BagInput::Left,
            ),
        ] {
            let mut state = seed();
            assert_eq!(
                state.apply(&left, &right, LIMBS, &mut allow),
                Err(BagJoinError::NegativeMultiplicity { input })
            );
            assert_eq!(state, seed());
        }
    }

    #[test]
    fn every_outer_join_refusal_is_atomic_through_match_and_null_extension_preparation() {
        let left = z(&[((1, 10), -1), ((2, 21), 1)]);
        let right = z(&[((1, 30), -2), ((2, 31), 3)]);
        let mut success = seed();
        let mut calls = 0;
        let expected = success
            .apply(&left, &right, LIMBS, &mut |_| {
                calls += 1;
                Ok::<_, usize>(())
            })
            .unwrap();
        for stop in 1..=calls {
            let mut state = seed();
            let mut seen = 0;
            assert_eq!(
                state.apply(&left, &right, LIMBS, &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                }),
                Err(BagJoinError::ZSet(ZSetError::Control(stop)))
            );
            assert_eq!(seen, stop);
            assert_eq!(state, seed());
            assert_eq!(
                state.apply(&left, &right, LIMBS, &mut allow).unwrap(),
                expected
            );
            assert_eq!(state, success);
        }
        let mut state = seed();
        {
            let pending = state.prepare(&left, &right, LIMBS, &mut allow).unwrap();
            assert_eq!(pending.delta(), &expected);
        }
        assert_eq!(state, seed());
    }

    #[test]
    fn product_promotion_refusal_does_not_remove_the_old_null_extension() {
        let mut state = Operator::new();
        let left = z(&[((1, 10), i128::MAX)]);
        let before = state.apply(&left, &ZSet::new(), LIMBS, &mut allow).unwrap();
        assert!(matches!(
            state.apply(
                &ZSet::new(),
                &z(&[((1, 30), 2)]),
                LimbLimit::new(0),
                &mut allow
            ),
            Err(BagJoinError::ZSet(ZSetError::Arithmetic(_)))
        ));
        assert_eq!(state.snapshot(LIMBS, &mut allow).unwrap(), before);
        assert!(state.witness_counts().is_empty());
        assert!(state.right_weight(&1, &30).is_none());
        let delta = state
            .apply(&ZSet::new(), &z(&[((1, 30), 2)]), LIMBS, &mut allow)
            .unwrap();
        assert_eq!(
            delta.weight(&(1, 10, None)),
            Some(&ZWeight::from_i128(-i128::MAX))
        );
        assert!(delta.weight(&(1, 10, Some(30))).unwrap().is_promoted());
        assert_eq!(
            before.plus(&delta, LIMBS, &mut allow).unwrap(),
            state.snapshot(LIMBS, &mut allow).unwrap()
        );
    }
}
