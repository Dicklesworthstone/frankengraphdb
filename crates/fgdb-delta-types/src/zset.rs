//! Exact finite-support weighted relations for incremental computation.
//!
//! A Z-set is a value, not a graph store or a committed view generation. Keys
//! use the caller's canonical total order. Weights use the existing checked
//! i128-to-bigint kernel; negative weights are retractions, never discarded.
//! Every operation has explicit arithmetic admission and interruptible logical
//! work/growth events. Events count entries, not allocator bytes: callers own
//! key-size, comparison and callback costs. No durable encoding is introduced.

pub mod aggregate;
pub mod incremental;

use crate::{LimbLimit, ZWeight, ZWeightError};
use std::collections::{BTreeMap, btree_map::Entry};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZSetEvent {
    /// A tuple visit, callback, or exact weight operation.
    Work,
    /// One new output or private staging entry, before insertion.
    ScratchEntry,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZSetError<E> {
    Arithmetic(ZWeightError),
    /// A transferred input already contains a promoted weight above admission.
    WeightAdmission { required_limbs: usize, limit: usize },
    Control(E),
    Callback(E),
}

impl<E: core::fmt::Display> core::fmt::Display for ZSetError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Arithmetic(error) => error.fmt(f),
            Self::WeightAdmission { required_limbs, limit } => {
                write!(f, "Z-set weight requires {required_limbs} limbs, limit {limit}")
            }
            Self::Control(error) => write!(f, "Z-set control: {error}"),
            Self::Callback(error) => write!(f, "Z-set callback: {error}"),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for ZSetError<E> {}

/// Canonical finite support: keys are ordered and unique; zero weights are
/// absent. There is deliberately no infallible Clone for arbitrary weights.
#[derive(PartialEq, Eq)]
pub struct ZSet<T: Ord> {
    entries: BTreeMap<T, ZWeight>,
}

impl<T: Ord> core::fmt::Debug for ZSet<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ZSet")
            .field("support", &self.entries.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}
impl<T: Ord> Default for ZSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Ord> ZSet<T> {
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    /// Explicit data export. Missing support has mathematical weight zero.
    pub fn weight(&self, key: &T) -> Option<&ZWeight> {
        self.entries.get(key)
    }
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (&T, &ZWeight)> + ExactSizeIterator {
        self.entries.iter()
    }
    pub fn into_updates(self) -> impl Iterator<Item = (T, ZWeight)> {
        self.entries.into_iter()
    }

    pub fn from_updates<E>(
        updates: impl IntoIterator<Item = (T, ZWeight)>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<Self, ZSetError<E>> {
        let mut result = Self::new();
        for (key, weight) in updates {
            result.accumulate(key, weight, limbs, control)?;
        }
        Ok(result)
    }

    fn accumulate<E>(
        &mut self,
        key: T,
        weight: ZWeight,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<(), ZSetError<E>> {
        event(control, ZSetEvent::Work)?;
        admit(&weight, limbs)?;
        if weight.is_zero() {
            return Ok(());
        }
        match self.entries.entry(key) {
            Entry::Vacant(entry) => {
                event(control, ZSetEvent::ScratchEntry)?;
                entry.insert(weight);
            }
            Entry::Occupied(mut entry) => {
                event(control, ZSetEvent::Work)?;
                let next = entry
                    .get()
                    .checked_add(&weight, limbs)
                    .map_err(ZSetError::Arithmetic)?;
                if next.is_zero() {
                    entry.remove();
                } else {
                    entry.insert(next);
                }
            }
        }
        Ok(())
    }

    pub fn total_weight<E>(
        &self,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZWeight, ZSetError<E>> {
        let mut result = ZWeight::ZERO;
        for weight in self.entries.values() {
            event(control, ZSetEvent::Work)?;
            admit(weight, limbs)?;
            result = result
                .checked_add(weight, limbs)
                .map_err(ZSetError::Arithmetic)?;
        }
        Ok(result)
    }

    /// Linear projection. Colliding projected keys add their signed weights;
    /// applying DISTINCT to a delta instead would lose deletion multiplicity.
    pub fn map<U: Ord, E>(
        &self,
        mut project: impl FnMut(&T) -> Result<U, E>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<U>, ZSetError<E>> {
        let mut result = ZSet::new();
        for (key, weight) in &self.entries {
            event(control, ZSetEvent::Work)?;
            let projected = project(key).map_err(ZSetError::Callback)?;
            let weight = weight
                .checked_clone(limbs)
                .map_err(ZSetError::Arithmetic)?;
            result.accumulate(projected, weight, limbs, control)?;
        }
        Ok(result)
    }
}

impl<T: Ord + Clone> ZSet<T> {
    pub fn checked_clone<E>(
        &self,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<Self, ZSetError<E>> {
        self.map(|key| Ok(key.clone()), limbs, control)
    }

    pub fn filter<E>(
        &self,
        mut predicate: impl FnMut(&T) -> Result<bool, E>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<Self, ZSetError<E>> {
        let mut result = Self::new();
        for (key, weight) in &self.entries {
            event(control, ZSetEvent::Work)?;
            if predicate(key).map_err(ZSetError::Callback)? {
                let weight = weight
                    .checked_clone(limbs)
                    .map_err(ZSetError::Arithmetic)?;
                result.accumulate(key.clone(), weight, limbs, control)?;
            }
        }
        Ok(result)
    }

    pub fn negated<E>(
        &self,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<Self, ZSetError<E>> {
        let mut result = Self::new();
        for (key, weight) in &self.entries {
            event(control, ZSetEvent::Work)?;
            let inverse = weight.checked_neg(limbs).map_err(ZSetError::Arithmetic)?;
            result.accumulate(key.clone(), inverse, limbs, control)?;
        }
        Ok(result)
    }

    pub fn plus<E>(
        &self,
        other: &Self,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<Self, ZSetError<E>> {
        let mut result = self.checked_clone(limbs, control)?;
        result.integrate(other, limbs, control)?;
        Ok(result)
    }

    /// Differentiate two full values: `new.minus(old)` is the exact delta.
    pub fn minus<E>(
        &self,
        other: &Self,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<Self, ZSetError<E>> {
        let inverse = other.negated(limbs, control)?;
        self.plus(&inverse, limbs, control)
    }

    /// Integrate a delta without cloning or scanning unaffected support.
    /// Recoverable arithmetic/control refusal leaves `self` unchanged. Prepare
    /// one replacement per changed key, then publish with no fallible callback.
    /// Standard-library map allocation and arbitrary user key code are outside
    /// the logical event model; this is not an allocator-byte or spill promise.
    pub fn integrate<E>(
        &mut self,
        delta: &Self,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<(), ZSetError<E>> {
        let replacements = self.prepare_integration(delta, limbs, control)?;
        event(control, ZSetEvent::Work)?;
        self.publish(replacements);
        Ok(())
    }

    pub(crate) fn prepare_integration<E>(
        &self,
        delta: &Self,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<BTreeMap<T, ZWeight>, ZSetError<E>> {
        let mut replacements = BTreeMap::new();
        for (key, change) in &delta.entries {
            event(control, ZSetEvent::Work)?;
            admit(change, limbs)?;
            let next = match self.entries.get(key) {
                Some(old) => old.checked_add(change, limbs),
                None => change.checked_clone(limbs),
            }
            .map_err(ZSetError::Arithmetic)?;
            event(control, ZSetEvent::ScratchEntry)?;
            replacements.insert(key.clone(), next);
        }
        Ok(replacements)
    }

    pub(crate) fn publish(&mut self, replacements: BTreeMap<T, ZWeight>) {
        for (key, weight) in replacements {
            if weight.is_zero() {
                self.entries.remove(&key);
            } else {
                self.entries.insert(key, weight);
            }
        }
    }

    /// Exact bilinear product. This value primitive is deliberately not an
    /// optimizer: arranged equijoins can avoid visiting nonmatching keys.
    pub fn product<U: Ord + Clone, E>(
        &self,
        other: &ZSet<U>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<(T, U)>, ZSetError<E>> {
        let mut result = ZSet::new();
        for (left, left_weight) in &self.entries {
            event(control, ZSetEvent::Work)?;
            for (right, right_weight) in &other.entries {
                event(control, ZSetEvent::Work)?;
                let weight = left_weight
                    .checked_mul(right_weight, limbs)
                    .map_err(ZSetError::Arithmetic)?;
                result.accumulate((left.clone(), right.clone()), weight, limbs, control)?;
            }
        }
        Ok(result)
    }
}

pub(crate) fn event<E>(
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    value: ZSetEvent,
) -> Result<(), ZSetError<E>> {
    control(value).map_err(ZSetError::Control)
}

fn admit<E>(weight: &ZWeight, limbs: LimbLimit) -> Result<(), ZSetError<E>> {
    if weight.is_promoted() && weight.magnitude_limb_count() > limbs.max_limbs() {
        return Err(ZSetError::WeightAdmission {
            required_limbs: weight.magnitude_limb_count(),
            limit: limbs.max_limbs(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMBS: LimbLimit = LimbLimit::new(16);

    fn allow(_: ZSetEvent) -> Result<(), usize> {
        Ok(())
    }

    fn z(updates: &[(i32, i128)]) -> ZSet<i32> {
        ZSet::from_updates(
            updates.iter().map(|&(k, w)| (k, ZWeight::from_i128(w))),
            LIMBS,
            &mut allow,
        )
        .unwrap()
    }

    fn plain(set: &ZSet<i32>) -> Vec<(i32, i128)> {
        set.iter()
            .map(|(k, w)| (*k, w.to_i128().unwrap()))
            .collect()
    }

    #[test]
    fn consolidation_preserves_signed_support_and_cancels_zeros() {
        assert_eq!(
            plain(&z(&[(3, -2), (1, 4), (3, 1), (1, -4), (2, 0)])),
            vec![(3, -1)]
        );
        let input = z(&[(1, 2), (2, -3), (3, 1)]);
        let projected = input.map(|key| Ok(key % 2), LIMBS, &mut allow).unwrap();
        assert_eq!(plain(&projected), vec![(0, -3), (1, 3)]);
        assert_eq!(input.total_weight(LIMBS, &mut allow).unwrap(), ZWeight::ZERO);
        let selected = input.filter(|key| Ok(*key > 1), LIMBS, &mut allow).unwrap();
        assert_eq!(plain(&selected), vec![(2, -3), (3, 1)]);
    }

    #[test]
    fn addition_negation_and_differentiation_are_integer_group_operations() {
        for a in -2..=2 {
            for b in -2..=2 {
                let left = z(&[(1, a), (2, b)]);
                let right = z(&[(1, b), (3, a)]);
                let sum = left.plus(&right, LIMBS, &mut allow).unwrap();
                assert_eq!(sum, right.plus(&left, LIMBS, &mut allow).unwrap());
                assert_eq!(sum.minus(&right, LIMBS, &mut allow).unwrap(), left);
                assert!(
                    left.plus(
                        &left.negated(LIMBS, &mut allow).unwrap(),
                        LIMBS,
                        &mut allow,
                    )
                    .unwrap()
                    .is_empty()
                );
                let delta = right.minus(&left, LIMBS, &mut allow).unwrap();
                let mut integrated = left.checked_clone(LIMBS, &mut allow).unwrap();
                integrated.integrate(&delta, LIMBS, &mut allow).unwrap();
                assert_eq!(integrated, right);
            }
        }
    }

    #[test]
    fn product_derivative_includes_the_same_batch_cross_term() {
        for a in -1..=1 {
            for b in -1..=1 {
                for da in -1..=1 {
                    for db in -1..=1 {
                        let left = z(&[(1, a)]);
                        let right = z(&[(2, b)]);
                        let dl = z(&[(1, da)]);
                        let dr = z(&[(2, db)]);
                        let before = left.product(&right, LIMBS, &mut allow).unwrap();
                        let after = left
                            .plus(&dl, LIMBS, &mut allow)
                            .unwrap()
                            .product(
                                &right.plus(&dr, LIMBS, &mut allow).unwrap(),
                                LIMBS,
                                &mut allow,
                            )
                            .unwrap();
                        let expected = after.minus(&before, LIMBS, &mut allow).unwrap();
                        let actual = dl
                            .product(&right, LIMBS, &mut allow)
                            .unwrap()
                            .plus(
                                &left.product(&dr, LIMBS, &mut allow).unwrap(),
                                LIMBS,
                                &mut allow,
                            )
                            .unwrap()
                            .plus(
                                &dl.product(&dr, LIMBS, &mut allow).unwrap(),
                                LIMBS,
                                &mut allow,
                            )
                            .unwrap();
                        assert_eq!(actual, expected);
                    }
                }
            }
        }
    }

    #[test]
    fn promotion_and_late_arithmetic_refusal_do_not_publish_partial_state() {
        let mut state = z(&[(1, 7), (2, i128::MAX)]);
        let delta = z(&[(1, 1), (2, 1)]);
        let before = state.checked_clone(LIMBS, &mut allow).unwrap();
        assert!(matches!(
            state.integrate(&delta, LimbLimit::new(0), &mut allow),
            Err(ZSetError::Arithmetic(_))
        ));
        assert_eq!(state, before);
        state.integrate(&delta, LIMBS, &mut allow).unwrap();
        assert!(state.weight(&2).unwrap().is_promoted());
        state
            .integrate(
                &delta.negated(LIMBS, &mut allow).unwrap(),
                LIMBS,
                &mut allow,
            )
            .unwrap();
        assert_eq!(state, before);
    }

    #[test]
    fn every_integration_checkpoint_refuses_without_mutating_the_value() {
        let initial = z(&[(1, 1), (2, 2), (3, 3)]);
        let delta = z(&[(1, -1), (2, 2), (4, 9)]);
        let mut calls = 0;
        let mut success = initial.checked_clone(LIMBS, &mut allow).unwrap();
        success
            .integrate(&delta, LIMBS, &mut |_| {
                calls += 1;
                Ok::<_, usize>(())
            })
            .unwrap();
        assert_eq!(plain(&success), vec![(2, 4), (3, 3), (4, 9)]);
        for stop in 1..=calls {
            let mut state = initial.checked_clone(LIMBS, &mut allow).unwrap();
            let mut seen = 0;
            assert_eq!(
                state.integrate(&delta, LIMBS, &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                }),
                Err(ZSetError::Control(stop))
            );
            assert_eq!(seen, stop);
            assert_eq!(state, initial);
            state.integrate(&delta, LIMBS, &mut allow).unwrap();
            assert_eq!(state, success);
        }
    }

    #[test]
    fn callbacks_are_fallible_and_debug_does_not_export_data() {
        let state = z(&[(1, 2), (2, 3)]);
        assert_eq!(
            state.map(
                |key| if *key == 2 { Err(17) } else { Ok(*key) },
                LIMBS,
                &mut allow,
            ),
            Err(ZSetError::Callback(17))
        );
        let secret = ZSet::from_updates(
            [("private-key", ZWeight::from_i128(919191))],
            LIMBS,
            &mut allow,
        )
        .unwrap();
        let debug = format!("{secret:?}");
        assert!(!debug.contains("private-key"));
        assert!(!debug.contains("919191"));
    }
}
