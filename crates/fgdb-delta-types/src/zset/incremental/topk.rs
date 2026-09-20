//! Exact deletion-aware ordered windows over complete nonnegative bags.
//!
//! The caller supplies a canonical total order on keys, including deterministic
//! tie breakers; `Reverse<T>` selects the opposite order. Offset and count refer
//! to occurrences, not distinct keys. We retain the complete input support so
//! deleting a selected row can promote an older row outside the window.
//!
//! This is an in-process algebra stage, not a durable view or a source-coverage
//! proof. Preparation merges the retained order with only the changed counts,
//! visits at most the prefix through the window, and differences only the
//! bounded output. Multiplicities are never expanded. Callbacks govern logical
//! work and entries, not arbitrary key payload or allocator-byte costs.

use super::super::{ZSet, ZSetError, ZSetEvent, admit, event};
use crate::{LimbLimit, ZWeight};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TopKError<E> {
    Delta(ZSetError<E>),
    /// A complete input bag cannot have negative integrated multiplicity,
    /// including on keys outside the visible window.
    NegativeMultiplicity,
}

impl<E> From<ZSetError<E>> for TopKError<E> {
    fn from(error: ZSetError<E>) -> Self {
        Self::Delta(error)
    }
}

impl<E: core::fmt::Display> core::fmt::Display for TopKError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Delta(error) => error.fmt(f),
            Self::NegativeMultiplicity => f.write_str("negative integrated ordered-window input"),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for TopKError<E> {}

/// A fixed `ORDER BY key OFFSET offset LIMIT count` over an exact bag.
/// Zero count is a legal empty window; input validation still applies.
#[derive(PartialEq, Eq)]
pub struct IncrementalTopK<T: Ord> {
    counts: ZSet<T>,
    rows: ZSet<T>,
    offset: u64,
    count: u64,
}

impl<T: Ord> IncrementalTopK<T> {
    pub fn new(offset: u64, count: u64) -> Self {
        Self {
            counts: ZSet::new(),
            rows: ZSet::new(),
            offset,
            count,
        }
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// Complete integrated input, including support outside the window.
    pub fn counts(&self) -> &ZSet<T> {
        &self.counts
    }

    /// Selected occurrences in canonical key order, with compressed weights.
    pub fn rows(&self) -> &ZSet<T> {
        &self.rows
    }
}

impl<T: Ord> core::fmt::Debug for IncrementalTopK<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IncrementalTopK")
            .field("offset", &self.offset)
            .field("count", &self.count)
            .field("input_support", &self.counts.len())
            .field("output_support", &self.rows.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

impl<T: Ord + Clone> IncrementalTopK<T> {
    /// Prepare a whole tick against the final consolidated input counts.
    /// Failure or dropping the guard preserves both input and selected output.
    /// Downstream operators must prepare before any participant commits.
    pub fn prepare<E>(
        &mut self,
        changes: &ZSet<T>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<TopKUpdate<'_, T>, TopKError<E>> {
        event(control, ZSetEvent::Work)?;
        let replacements = self.counts.prepare_integration(changes, limbs, control)?;
        for weight in replacements.values() {
            event(control, ZSetEvent::Work)?;
            if weight < &ZWeight::ZERO {
                return Err(TopKError::NegativeMultiplicity);
            }
        }
        let rows = select_window(
            &self.counts,
            &replacements,
            self.offset,
            self.count,
            limbs,
            control,
        )?;
        let delta = rows.minus(&self.rows, limbs, control)?;
        event(control, ZSetEvent::Work)?;
        Ok(TopKUpdate {
            owner: self,
            replacements,
            rows,
            delta,
        })
    }

    /// Integrate one tick. Use `prepare` when composing multiple stages.
    pub fn apply<E>(
        &mut self,
        changes: &ZSet<T>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<T>, TopKError<E>> {
        Ok(self.prepare(changes, limbs, control)?.commit())
    }
}

fn select_window<T: Ord + Clone, E>(
    counts: &ZSet<T>,
    replacements: &BTreeMap<T, ZWeight>,
    offset: u64,
    count: u64,
    limbs: LimbLimit,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<ZSet<T>, ZSetError<E>> {
    let mut old = counts.iter().peekable();
    let mut changed = replacements.iter().peekable();
    let mut skip = ZWeight::from_i128(i128::from(offset));
    let mut remaining = ZWeight::from_i128(i128::from(count));
    let mut rows = ZSet::new();
    while !remaining.is_zero() {
        event(control, ZSetEvent::Work)?;
        let next = match (old.peek(), changed.peek()) {
            (Some((left, _)), Some((right, _))) => match left.cmp(right) {
                core::cmp::Ordering::Less => old.next(),
                core::cmp::Ordering::Greater => changed.next(),
                core::cmp::Ordering::Equal => {
                    // A zero replacement deletes the old support; never fall
                    // through to its previously positive count.
                    old.next();
                    changed.next()
                }
            },
            (Some(_), None) => old.next(),
            (None, Some(_)) => changed.next(),
            (None, None) => None,
        };
        let Some((key, weight)) = next else {
            break;
        };
        admit(weight, limbs)?;
        if weight <= &skip {
            skip = skip
                .checked_sub(weight, limbs)
                .map_err(ZSetError::Arithmetic)?;
            continue;
        }
        let available = weight
            .checked_sub(&skip, limbs)
            .map_err(ZSetError::Arithmetic)?;
        skip = ZWeight::ZERO;
        let selected = if available < remaining {
            available
        } else {
            remaining
                .checked_clone(limbs)
                .map_err(ZSetError::Arithmetic)?
        };
        event(control, ZSetEvent::Work)?;
        remaining = remaining
            .checked_sub(&selected, limbs)
            .map_err(ZSetError::Arithmetic)?;
        // Reserve before the caller-defined key clone, in addition to the
        // retained entry reservation inside accumulate.
        event(control, ZSetEvent::ScratchEntry)?;
        rows.accumulate(key.clone(), selected, limbs, control)?;
    }
    Ok(rows)
}

#[must_use = "dropping an ordered-window update aborts it"]
pub struct TopKUpdate<'a, T: Ord> {
    owner: &'a mut IncrementalTopK<T>,
    replacements: BTreeMap<T, ZWeight>,
    rows: ZSet<T>,
    delta: ZSet<T>,
}

impl<T: Ord> TopKUpdate<'_, T> {
    pub fn delta(&self) -> &ZSet<T> {
        &self.delta
    }

    /// Tentative selected bag; not published until `commit`.
    pub fn rows(&self) -> &ZSet<T> {
        &self.rows
    }
}

impl<T: Ord + Clone> TopKUpdate<'_, T> {
    pub fn commit(self) -> ZSet<T> {
        let Self {
            owner,
            replacements,
            rows,
            delta,
        } = self;
        owner.counts.publish(replacements);
        owner.rows = rows;
        delta
    }
}

impl<T: Ord> core::fmt::Debug for TopKUpdate<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TopKUpdate")
            .field("delta_support", &self.delta.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMBS: LimbLimit = LimbLimit::new(16);

    fn allow(_: ZSetEvent) -> Result<(), usize> {
        Ok(())
    }

    fn z(rows: &[(i32, i128)]) -> ZSet<i32> {
        ZSet::from_updates(
            rows.iter()
                .map(|&(key, weight)| (key, ZWeight::from_i128(weight))),
            LIMBS,
            &mut allow,
        )
        .unwrap()
    }

    // Deliberately expand only tiny oracle fixtures. Production never expands
    // occurrences and does not use this sorting/slicing implementation.
    fn oracle(counts: &[i128; 3], offset: usize, count: usize) -> ZSet<i32> {
        let mut expanded = Vec::new();
        for (key, &weight) in counts.iter().enumerate() {
            for _ in 0..weight {
                expanded.push(key as i32);
            }
        }
        ZSet::from_updates(
            expanded
                .into_iter()
                .skip(offset)
                .take(count)
                .map(|key| (key, ZWeight::from_i128(1))),
            LIMBS,
            &mut allow,
        )
        .unwrap()
    }

    fn bag(code: i128) -> [i128; 3] {
        [code % 3, code / 3 % 3, code / 9 % 3]
    }

    #[test]
    fn every_small_bag_transition_matches_independent_occurrence_sort_and_slice() {
        for offset in 0..=7 {
            for count in 0..=6 {
                for before_code in 0..27 {
                    for after_code in 0..27 {
                        let before = bag(before_code);
                        let after = bag(after_code);
                        let seed = z(&[(0, before[0]), (1, before[1]), (2, before[2])]);
                        let changes = z(&[
                            (0, after[0] - before[0]),
                            (1, after[1] - before[1]),
                            (2, after[2] - before[2]),
                        ]);
                        let mut stage = IncrementalTopK::new(offset as u64, count as u64);
                        let mut sink = stage.apply(&seed, LIMBS, &mut allow).unwrap();
                        assert_eq!(sink, oracle(&before, offset, count));
                        let delta = stage.apply(&changes, LIMBS, &mut allow).unwrap();
                        sink.integrate(&delta, LIMBS, &mut allow).unwrap();
                        assert_eq!(sink, oracle(&after, offset, count));
                        assert_eq!(stage.rows(), &sink);
                        assert_eq!(
                            stage.counts(),
                            &z(&[(0, after[0]), (1, after[1]), (2, after[2])])
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn deletion_promotes_retained_tail_and_splits_bag_multiplicity_at_both_boundaries() {
        let mut stage = IncrementalTopK::new(2, 4);
        assert_eq!(
            stage
                .apply(&z(&[(1, 3), (2, 2), (3, 4)]), LIMBS, &mut allow)
                .unwrap(),
            z(&[(1, 1), (2, 2), (3, 1)])
        );
        assert_eq!(
            stage.apply(&z(&[(1, -3)]), LIMBS, &mut allow).unwrap(),
            z(&[(1, -1), (2, -2), (3, 3)])
        );
        assert_eq!(stage.rows(), &z(&[(3, 4)]));
        stage
            .apply(&z(&[(2, -2), (3, -4)]), LIMBS, &mut allow)
            .unwrap();
        assert!(stage.counts().is_empty());
        assert!(stage.rows().is_empty());
    }

    fn seeded() -> IncrementalTopK<i32> {
        let mut stage = IncrementalTopK::new(1, 3);
        stage
            .apply(&z(&[(1, 2), (2, 2), (3, 3)]), LIMBS, &mut allow)
            .unwrap();
        stage
    }

    #[test]
    fn every_control_refusal_and_dropped_guard_is_atomic_and_retryable() {
        let changes = z(&[(0, 1), (1, -2), (2, -1), (4, 2)]);
        let mut success = seeded();
        let mut calls = 0;
        let expected = success
            .apply(&changes, LIMBS, &mut |_| {
                calls += 1;
                Ok::<_, usize>(())
            })
            .unwrap();
        for stop in 1..=calls {
            let mut stage = seeded();
            let mut seen = 0;
            assert_eq!(
                stage.apply(&changes, LIMBS, &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                }),
                Err(TopKError::Delta(ZSetError::Control(stop)))
            );
            assert_eq!(stage, seeded());
            assert_eq!(stage.apply(&changes, LIMBS, &mut allow).unwrap(), expected);
            assert_eq!(stage, success);
        }
        let mut stage = seeded();
        {
            let pending = stage.prepare(&changes, LIMBS, &mut allow).unwrap();
            assert_eq!(pending.delta(), &expected);
            assert_eq!(pending.rows(), success.rows());
        }
        assert_eq!(stage, seeded());
    }

    #[test]
    fn invalid_retractions_outside_window_and_zero_limit_fail_closed() {
        for count in [0, 1, 10] {
            let mut stage = IncrementalTopK::new(0, count);
            let seed = z(&[(1, 2), (99, 1)]);
            stage.apply(&seed, LIMBS, &mut allow).unwrap();
            assert_eq!(
                stage.apply(&z(&[(1, 1), (99, -2)]), LIMBS, &mut allow),
                Err(TopKError::NegativeMultiplicity)
            );
            assert_eq!(stage.counts(), &seed);
            assert_eq!(
                stage.apply(&z(&[(100, -1)]), LIMBS, &mut allow),
                Err(TopKError::NegativeMultiplicity)
            );
        }
    }

    #[test]
    fn promoted_counts_and_maximum_offsets_do_not_expand_or_overflow() {
        let promoted = ZWeight::from_i128(i128::MAX)
            .checked_add(&ZWeight::from_i128(1), LIMBS)
            .unwrap();
        let changes = ZSet::from_updates([(7, promoted)], LIMBS, &mut allow).unwrap();
        let mut stage = IncrementalTopK::new(u64::MAX, u64::MAX);
        let mut calls = 0;
        let selected = stage
            .apply(&changes, LIMBS, &mut |_| {
                calls += 1;
                assert!(calls < 100);
                Ok::<_, usize>(())
            })
            .unwrap();
        assert_eq!(selected, z(&[(7, i128::from(u64::MAX))]));
        assert!(stage.counts().weight(&7).unwrap().is_promoted());
        let mut denied = IncrementalTopK::new(0, 1);
        assert!(
            denied
                .apply(&changes, LimbLimit::new(0), &mut allow)
                .is_err()
        );
        assert!(denied.counts().is_empty());
        assert!(denied.rows().is_empty());
    }

    #[test]
    fn reverse_and_tuple_keys_preserve_total_order_without_losing_tied_rows() {
        use core::cmp::Reverse;
        let seed = ZSet::from_updates(
            [
                ((Reverse(10), 1), 1),
                ((Reverse(10), 2), 1),
                ((Reverse(9), 3), 1),
            ]
            .into_iter()
            .map(|(key, count)| (key, ZWeight::from_i128(count))),
            LIMBS,
            &mut allow,
        )
        .unwrap();
        let mut stage = IncrementalTopK::new(1, 1);
        stage.apply(&seed, LIMBS, &mut allow).unwrap();
        assert_eq!(stage.rows().iter().next().unwrap().0, &(Reverse(10), 2));
        assert_eq!(stage.counts().len(), 3);
    }

    #[test]
    fn diagnostics_redact_payloads() {
        let redacted_marker = "do-not-log-this-row";
        let seed = ZSet::from_updates(
            [(redacted_marker, ZWeight::from_i128(1))],
            LIMBS,
            &mut allow,
        )
        .unwrap();
        let mut stage = IncrementalTopK::new(0, 1);
        assert!(!format!("{stage:?}").contains(redacted_marker));
        let pending = stage.prepare(&seed, LIMBS, &mut allow).unwrap();
        assert!(!format!("{pending:?}").contains(redacted_marker));
        pending.commit();
        assert!(!format!("{stage:?}").contains(redacted_marker));
    }
}
