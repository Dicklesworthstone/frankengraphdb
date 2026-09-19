//! Retractable, grouped integer aggregates over integrated nonnegative bags.
//!
//! Inputs are `(group, Option<i128>)` Z-set deltas: `None` is SQL-style NULL.
//! Counts and sums use exact ZWeight arithmetic, including bigint promotion.
//! DISTINCT statistics change only when a nonnull value enters or leaves
//! positive support; duplicate insertions/retractions do not recount it.
//! Ordered per-value counts support deleting extrema. Only changed groups and
//! values are patched; finding a replacement extremum visits the invalidated
//! prefix/suffix, not every group or every repeated occurrence.
//!
//! This is an in-memory Ripple algebra operator, not a durable materialized
//! view, subscription protocol, GQL binder, spill store, or global aggregate.
//! A group disappears when its last row is removed. Input deltas may be signed,
//! but a negative integrated per-value multiplicity is refused atomically.

use super::{ZSet, ZSetError, ZSetEvent, admit, event};
use crate::{LimbLimit, ZWeight};
use std::collections::{BTreeMap, btree_map::Entry};
use std::ops::RangeBounds;
use std::sync::Arc;

/// Exact grouped COUNT, SUM, AVG sufficient statistics, and MIN/MAX.
/// Private fields preserve nullable-sum, distinct-support and extrema coherence.
/// Arc sharing lets result Z-sets clone keys without infallibly cloning bigints.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
pub struct AggregateValues {
    rows: ZWeight,
    values: ZWeight,
    sum: Option<ZWeight>,
    distinct: ZWeight,
    distinct_sum: Option<ZWeight>,
    minimum: Option<i128>,
    maximum: Option<i128>,
}

impl AggregateValues {
    pub fn count_rows(&self) -> &ZWeight {
        &self.rows
    }
    pub fn count_values(&self) -> &ZWeight {
        &self.values
    }
    pub fn sum(&self) -> Option<&ZWeight> {
        self.sum.as_ref()
    }
    /// Number of different nonnull values with positive integrated support.
    pub fn count_distinct(&self) -> &ZWeight {
        &self.distinct
    }
    /// Sum of each different nonnull input exactly once; NULL for no values.
    pub fn sum_distinct(&self) -> Option<&ZWeight> {
        self.distinct_sum.as_ref()
    }
    pub fn minimum(&self) -> Option<i128> {
        self.minimum
    }
    pub fn maximum(&self) -> Option<i128> {
        self.maximum
    }
    /// Exact AVG sufficient statistics (numerator, positive denominator).
    /// This is not a rounded floating-point or normalized rational value.
    pub fn average_parts(&self) -> Option<(&ZWeight, &ZWeight)> {
        self.sum.as_ref().map(|sum| (sum, &self.values))
    }
    /// Exact AVG(DISTINCT value) sufficient statistics, without rounding.
    pub fn average_distinct_parts(&self) -> Option<(&ZWeight, &ZWeight)> {
        self.distinct_sum.as_ref().map(|sum| (sum, &self.distinct))
    }
}

impl core::fmt::Debug for AggregateValues {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AggregateValues([REDACTED])")
    }
}

/// Every changed group retracts its old summary and inserts its new summary.
/// Equal summaries cancel, although the group's input counts still advance.
pub type AggregateDelta<K> = ZSet<(K, Arc<AggregateValues>)>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AggregateError<E> {
    ZSet(ZSetError<E>),
    /// No group key, input value, or multiplicity is exported by diagnostics.
    NegativeMultiplicity,
}

impl<E> From<ZSetError<E>> for AggregateError<E> {
    fn from(error: ZSetError<E>) -> Self {
        Self::ZSet(error)
    }
}
impl<E: core::fmt::Display> core::fmt::Display for AggregateError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ZSet(error) => error.fmt(f),
            Self::NegativeMultiplicity => f.write_str("negative integrated aggregate multiplicity"),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for AggregateError<E> {}

#[derive(PartialEq, Eq)]
struct Group {
    counts: ZSet<Option<i128>>,
    summary: Arc<AggregateValues>,
}

#[derive(PartialEq, Eq)]
pub struct IncrementalAggregate<K: Ord> {
    groups: BTreeMap<K, Group>,
}

impl<K: Ord> Default for IncrementalAggregate<K> {
    fn default() -> Self {
        Self::new()
    }
}
impl<K: Ord> IncrementalAggregate<K> {
    pub fn new() -> Self {
        Self {
            groups: BTreeMap::new(),
        }
    }
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }
    pub fn get(&self, key: &K) -> Option<&AggregateValues> {
        self.groups.get(key).map(|group| group.summary.as_ref())
    }
    /// Explicit retained-input inspection; absent support has weight zero.
    pub fn input_weight(&self, key: &K, value: Option<i128>) -> Option<&ZWeight> {
        self.groups
            .get(key)
            .and_then(|group| group.counts.weight(&value))
    }
    pub fn rows(&self) -> impl Iterator<Item = (&K, &AggregateValues)> {
        self.groups
            .iter()
            .map(|(key, group)| (key, group.summary.as_ref()))
    }
    /// Borrow an ordered key interval without scanning unrelated groups or
    /// allocating a result. As with get(), callers govern each visited entry
    /// and their key comparison/payload costs before consuming it.
    pub fn range(
        &self,
        range: impl RangeBounds<K>,
    ) -> impl DoubleEndedIterator<Item = (&K, &AggregateValues)> {
        self.groups
            .range(range)
            .map(|(key, group)| (key, group.summary.as_ref()))
    }
}
impl<K: Ord> core::fmt::Debug for IncrementalAggregate<K> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IncrementalAggregate")
            .field("groups", &self.groups.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

type Replacements = BTreeMap<Option<i128>, ZWeight>;
struct GroupPatch {
    counts: Replacements,
    summary: Option<Arc<AggregateValues>>,
}

impl<K: Ord + Clone> IncrementalAggregate<K> {
    /// Prepare one whole input tick without changing retained state. All
    /// arithmetic and recoverable control failures occur before publication.
    /// The exclusive-borrow guard prevents publishing against a stale basis.
    /// Standard collection allocation and arbitrary K clone/comparison code
    /// retain the parent Z-set API's explicit non-recoverable boundary.
    pub fn prepare<E>(
        &mut self,
        delta: &ZSet<(K, Option<i128>)>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<AggregateUpdate<'_, K>, AggregateError<E>> {
        event(control, ZSetEvent::Work)?;
        let mut patches = BTreeMap::new();
        let mut output = ZSet::new();
        // Input support is canonical (group,value) order. Borrow consecutive
        // groups directly instead of building or cloning another arrangement.
        let mut changes = delta.iter().peekable();
        while let Some(((key, _), _)) = changes.peek().copied() {
            event(control, ZSetEvent::Work)?;
            let old = self.groups.get(key);
            let (mut rows, mut values, mut sum, mut distinct, mut distinct_sum) =
                if let Some(group) = old {
                    event(control, ZSetEvent::Work)?;
                    let row = &group.summary;
                    (
                        row.rows
                            .checked_clone(limbs)
                            .map_err(ZSetError::Arithmetic)?,
                        row.values
                            .checked_clone(limbs)
                            .map_err(ZSetError::Arithmetic)?,
                        match &row.sum {
                            Some(sum) => sum.checked_clone(limbs).map_err(ZSetError::Arithmetic)?,
                            None => ZWeight::ZERO,
                        },
                        row.distinct
                            .checked_clone(limbs)
                            .map_err(ZSetError::Arithmetic)?,
                        match &row.distinct_sum {
                            Some(sum) => sum.checked_clone(limbs).map_err(ZSetError::Arithmetic)?,
                            None => ZWeight::ZERO,
                        },
                    )
                } else {
                    (
                        ZWeight::ZERO,
                        ZWeight::ZERO,
                        ZWeight::ZERO,
                        ZWeight::ZERO,
                        ZWeight::ZERO,
                    )
                };
            let mut counts = BTreeMap::new();
            while let Some(((next_key, value), change)) = changes.peek().copied() {
                if next_key != key {
                    break;
                }
                changes.next();
                event(control, ZSetEvent::Work)?;
                admit(change, limbs)?;
                let previous = old.and_then(|group| group.counts.weight(value));
                let next = match previous {
                    Some(previous) => previous.checked_add(change, limbs),
                    None => change.checked_clone(limbs),
                }
                .map_err(ZSetError::Arithmetic)?;
                if next < ZWeight::ZERO {
                    return Err(AggregateError::NegativeMultiplicity);
                }
                event(control, ZSetEvent::Work)?;
                rows = rows
                    .checked_add(change, limbs)
                    .map_err(ZSetError::Arithmetic)?;
                if let Some(value) = value {
                    event(control, ZSetEvent::Work)?;
                    values = values
                        .checked_add(change, limbs)
                        .map_err(ZSetError::Arithmetic)?;
                    event(control, ZSetEvent::Work)?;
                    let term = change
                        .checked_mul_i128(*value, limbs)
                        .map_err(ZSetError::Arithmetic)?;
                    event(control, ZSetEvent::Work)?;
                    sum = sum
                        .checked_add(&term, limbs)
                        .map_err(ZSetError::Arithmetic)?;
                    let was_present = previous.is_some_and(|weight| !weight.is_zero());
                    let is_present = !next.is_zero();
                    if was_present != is_present {
                        let sign = if is_present { 1 } else { -1 };
                        let crossing = ZWeight::from_i128(sign);
                        event(control, ZSetEvent::Work)?;
                        distinct = distinct
                            .checked_add(&crossing, limbs)
                            .map_err(ZSetError::Arithmetic)?;
                        // Do not negate i128::MIN in the scalar domain.
                        event(control, ZSetEvent::Work)?;
                        let term = crossing
                            .checked_mul_i128(*value, limbs)
                            .map_err(ZSetError::Arithmetic)?;
                        event(control, ZSetEvent::Work)?;
                        distinct_sum = distinct_sum
                            .checked_add(&term, limbs)
                            .map_err(ZSetError::Arithmetic)?;
                    }
                }
                // Reserve staging and any future retained input entry before
                // cloning/inserting. Publication invokes no controller.
                event(control, ZSetEvent::ScratchEntry)?;
                if previous.is_none() && !next.is_zero() {
                    event(control, ZSetEvent::ScratchEntry)?;
                }
                counts.insert(*value, next);
            }
            let summary = if rows.is_zero() {
                debug_assert!(values.is_zero() && sum.is_zero());
                debug_assert!(distinct.is_zero() && distinct_sum.is_zero());
                None
            } else {
                let (minimum, maximum) = if values.is_zero() {
                    (None, None)
                } else {
                    let previous = old.map(|group| &group.counts);
                    (
                        extremum(previous, &counts, false, control)?,
                        extremum(previous, &counts, true, control)?,
                    )
                };
                let row = AggregateValues {
                    rows,
                    sum: (!values.is_zero()).then_some(sum),
                    values,
                    distinct_sum: (!distinct.is_zero()).then_some(distinct_sum),
                    distinct,
                    minimum,
                    maximum,
                };
                if let Some(group) = old.filter(|group| group.summary.as_ref() == &row) {
                    Some(Arc::clone(&group.summary))
                } else {
                    event(control, ZSetEvent::ScratchEntry)?;
                    Some(Arc::new(row))
                }
            };
            if old.map(|group| &group.summary) != summary.as_ref() {
                if let Some(group) = old {
                    output.accumulate(
                        (key.clone(), Arc::clone(&group.summary)),
                        ZWeight::from_i128(-1),
                        limbs,
                        control,
                    )?;
                }
                if let Some(row) = &summary {
                    output.accumulate(
                        (key.clone(), Arc::clone(row)),
                        ZWeight::ONE,
                        limbs,
                        control,
                    )?;
                }
            }
            event(control, ZSetEvent::ScratchEntry)?;
            if old.is_none() && summary.is_some() {
                event(control, ZSetEvent::ScratchEntry)?;
            }
            patches.insert(key.clone(), GroupPatch { counts, summary });
        }
        event(control, ZSetEvent::Work)?;
        Ok(AggregateUpdate {
            owner: self,
            patches,
            delta: output,
        })
    }

    pub fn apply<E>(
        &mut self,
        delta: &ZSet<(K, Option<i128>)>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<AggregateDelta<K>, AggregateError<E>> {
        Ok(self.prepare(delta, limbs, control)?.commit())
    }

    /// Explicit current-result export. Normal updates never recompute this.
    /// Shared immutable summaries do not clone bigint payloads.
    pub fn snapshot<E>(
        &self,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<AggregateDelta<K>, ZSetError<E>> {
        let mut result = ZSet::new();
        for (key, group) in &self.groups {
            event(control, ZSetEvent::Work)?;
            result.accumulate(
                (key.clone(), Arc::clone(&group.summary)),
                ZWeight::ONE,
                limbs,
                control,
            )?;
        }
        Ok(result)
    }
}

/// Find the first surviving OLD value in the requested order, accounting for
/// every replacement. Newly inserted values are considered separately below.
fn surviving<'a, E>(
    entries: impl Iterator<Item = (&'a Option<i128>, &'a ZWeight)>,
    replacements: &Replacements,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<Option<i128>, ZSetError<E>> {
    for (value, weight) in entries {
        event(control, ZSetEvent::Work)?;
        if value.is_some() && !replacements.get(value).unwrap_or(weight).is_zero() {
            return Ok(*value);
        }
    }
    Ok(None)
}

fn extremum<E>(
    old: Option<&ZSet<Option<i128>>>,
    replacements: &Replacements,
    maximum: bool,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<Option<i128>, ZSetError<E>> {
    let previous = old.into_iter().flat_map(|group| group.iter());
    let old_value = if maximum {
        surviving(previous.rev(), replacements, control)?
    } else {
        surviving(previous, replacements, control)?
    };
    // The replacement map contains nonnegative integrated counts, not deltas.
    // An empty overlay is sufficient here: these entries already are final.
    let empty = BTreeMap::new();
    let changed_value = if maximum {
        surviving(replacements.iter().rev(), &empty, control)?
    } else {
        surviving(replacements.iter(), &empty, control)?
    };
    Ok(match (old_value, changed_value) {
        (Some(left), Some(right)) => Some(if maximum {
            left.max(right)
        } else {
            left.min(right)
        }),
        (left, right) => left.or(right),
    })
}

#[must_use = "dropping an aggregate update aborts it"]
pub struct AggregateUpdate<'a, K: Ord> {
    owner: &'a mut IncrementalAggregate<K>,
    patches: BTreeMap<K, GroupPatch>,
    delta: AggregateDelta<K>,
}
impl<K: Ord> AggregateUpdate<'_, K> {
    /// Read the prospective summary without publishing. An explicit removal
    /// shadows the old group; it must not fall back to the retained summary.
    /// This permits downstream row construction while the update owns its
    /// exclusive borrow. Lookup/key-comparison costs belong to the caller.
    pub fn get(&self, key: &K) -> Option<&AggregateValues> {
        match self.patches.get(key) {
            Some(patch) => patch.summary.as_deref(),
            None => self.owner.get(key),
        }
    }

    /// Borrow the pre-update interval while this guard holds the owner.
    /// These are OLD summaries: use get(key) to account for staged removals
    /// or replacements. Pair with changed_range() to include new keys.
    pub fn retained_range(
        &self,
        range: impl RangeBounds<K>,
    ) -> impl DoubleEndedIterator<Item = (&K, &AggregateValues)> {
        self.owner.range(range)
    }

    /// Borrow only staged keys in an interval. None is an explicit removal,
    /// not an instruction to fall back to the old summary. No scan or copy of
    /// unrelated keys occurs. Callers govern iteration as with range().
    pub fn changed_range(
        &self,
        range: impl RangeBounds<K>,
    ) -> impl DoubleEndedIterator<Item = (&K, Option<&AggregateValues>)> {
        self.patches
            .range(range)
            .map(|(key, patch)| (key, patch.summary.as_deref()))
    }

    /// Tentative output. Publish externally only after the whole tick commits.
    pub fn delta(&self) -> &AggregateDelta<K> {
        &self.delta
    }
    /// Infallible under the same collection/key-code boundary as ZSet::integrate.
    pub fn commit(self) -> AggregateDelta<K> {
        let Self {
            owner,
            patches,
            delta,
        } = self;
        for (key, patch) in patches {
            let Some(summary) = patch.summary else {
                owner.groups.remove(&key);
                continue;
            };
            match owner.groups.entry(key) {
                Entry::Occupied(mut entry) => {
                    entry.get_mut().counts.publish(patch.counts);
                    entry.get_mut().summary = summary;
                }
                Entry::Vacant(entry) => {
                    let mut counts = ZSet::new();
                    counts.publish(patch.counts);
                    entry.insert(Group { counts, summary });
                }
            }
        }
        delta
    }
}
impl<K: Ord> core::fmt::Debug for AggregateUpdate<'_, K> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AggregateUpdate")
            .field("changed_groups", &self.patches.len())
            .field("delta_support", &self.delta.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMBS: LimbLimit = LimbLimit::new(16);
    type Input = ZSet<(i32, Option<i128>)>;

    fn allow(_: ZSetEvent) -> Result<(), usize> {
        Ok(())
    }
    fn input(rows: impl IntoIterator<Item = (i32, Option<i128>, i128)>) -> Input {
        ZSet::from_updates(
            rows.into_iter()
                .map(|(key, value, weight)| ((key, value), ZWeight::from_i128(weight))),
            LIMBS,
            &mut allow,
        )
        .unwrap()
    }
    fn seeded(rows: &Input) -> IncrementalAggregate<i32> {
        let mut state = IncrementalAggregate::new();
        state.apply(rows, LIMBS, &mut allow).unwrap();
        state
    }

    // Independent full recomputation on tiny bags. Expand multiplicities into
    // rows; use no retained summary, extremum overlay, or incremental formula.
    fn reference(rows: &Input) -> AggregateDelta<i32> {
        let mut groups = BTreeMap::<i32, Vec<Option<i128>>>::new();
        for ((key, value), weight) in rows.iter() {
            let count = weight.to_i128().unwrap();
            assert!((0..=2).contains(&count));
            for _ in 0..count {
                groups.entry(*key).or_default().push(*value);
            }
        }
        ZSet::from_updates(
            groups.into_iter().map(|(key, rows)| {
                let values: Vec<_> = rows.iter().filter_map(|value| *value).collect();
                let distinct: std::collections::BTreeSet<_> = values.iter().copied().collect();
                let row = AggregateValues {
                    rows: ZWeight::from_i128(rows.len() as i128),
                    values: ZWeight::from_i128(values.len() as i128),
                    sum: (!values.is_empty()).then(|| ZWeight::from_i128(values.iter().sum())),
                    distinct: ZWeight::from_i128(distinct.len() as i128),
                    distinct_sum: (!distinct.is_empty())
                        .then(|| ZWeight::from_i128(distinct.iter().sum())),
                    minimum: values.iter().copied().min(),
                    maximum: values.iter().copied().max(),
                };
                ((key, Arc::new(row)), ZWeight::ONE)
            }),
            LIMBS,
            &mut allow,
        )
        .unwrap()
    }

    #[test]
    fn every_small_bag_transition_matches_full_recomputation_and_output_differentiation() {
        let bag = |mut code: usize| {
            input(
                [(0, None), (0, Some(-2)), (0, Some(3)), (1, Some(7))]
                    .into_iter()
                    .map(|(key, value)| {
                        let weight = (code % 3) as i128;
                        code /= 3;
                        (key, value, weight)
                    }),
            )
        };
        for old in 0..81 {
            for new in 0..81 {
                let before = bag(old);
                let after = bag(new);
                let mut state = seeded(&before);
                let mut sink = reference(&before);
                let expected = reference(&after);
                let expected_delta = expected.minus(&sink, LIMBS, &mut allow).unwrap();
                let changes = after.minus(&before, LIMBS, &mut allow).unwrap();
                let actual_delta = state.apply(&changes, LIMBS, &mut allow).unwrap();
                assert_eq!(actual_delta, expected_delta, "old={old}, new={new}");
                sink.integrate(&actual_delta, LIMBS, &mut allow).unwrap();
                assert_eq!(sink, expected);
                assert_eq!(state.snapshot(LIMBS, &mut allow).unwrap(), expected);
            }
        }
    }

    #[test]
    fn nulls_zero_sum_and_last_row_removal_have_distinct_results() {
        let initial = input([(1, None, 2), (2, Some(-5), 1), (2, Some(5), 1)]);
        let mut state = seeded(&initial);
        let nulls = state.get(&1).unwrap();
        assert_eq!(nulls.count_rows(), &ZWeight::from_i128(2));
        assert_eq!(nulls.count_values(), &ZWeight::ZERO);
        assert!(nulls.sum().is_none());
        assert!(nulls.average_parts().is_none());
        assert_eq!((nulls.minimum(), nulls.maximum()), (None, None));
        let zero = state.get(&2).unwrap();
        assert_eq!(zero.sum(), Some(&ZWeight::ZERO));
        let (numerator, denominator) = zero.average_parts().unwrap();
        assert_eq!(numerator, &ZWeight::ZERO);
        assert_eq!(denominator, &ZWeight::from_i128(2));
        let frozen = state.snapshot(LIMBS, &mut allow).unwrap();
        let delta = state
            .apply(
                &initial.negated(LIMBS, &mut allow).unwrap(),
                LIMBS,
                &mut allow,
            )
            .unwrap();
        assert_eq!(state.group_count(), 0);
        assert_eq!(delta, frozen.negated(LIMBS, &mut allow).unwrap());
        assert_eq!(frozen.len(), 2);
    }

    #[test]
    fn unchanged_summary_still_advances_counts_and_retractions_replace_extrema() {
        let mut state = seeded(&input([
            (0, Some(0), 1),
            (0, Some(2), 1),
            (0, Some(8), 1),
            (0, Some(10), 1),
        ]));
        let unchanged = state
            .apply(
                &input([
                    (0, Some(2), -1),
                    (0, Some(8), -1),
                    (0, Some(4), 1),
                    (0, Some(6), 1),
                ]),
                LIMBS,
                &mut allow,
            )
            .unwrap();
        assert!(unchanged.is_empty());
        assert!(state.input_weight(&0, Some(2)).is_none());
        assert_eq!(state.input_weight(&0, Some(4)), Some(&ZWeight::ONE));
        state
            .apply(
                &input([(0, Some(0), -1), (0, Some(10), -1)]),
                LIMBS,
                &mut allow,
            )
            .unwrap();
        let row = state.get(&0).unwrap();
        assert_eq!((row.minimum(), row.maximum()), (Some(4), Some(6)));
        state
            .apply(
                &input([(0, Some(4), -1), (0, Some(6), -1)]),
                LIMBS,
                &mut allow,
            )
            .unwrap();
        assert_eq!(state.group_count(), 0);
    }

    #[test]
    fn negative_per_value_counts_refuse_even_when_the_group_total_would_be_positive() {
        let before = input([(0, Some(2), 1), (0, Some(9), 10), (1, None, 2)]);
        let mut state = seeded(&before);
        let bad = input([(0, Some(2), -2), (0, Some(4), 5), (1, None, -1)]);
        assert_eq!(
            state.apply(&bad, LIMBS, &mut allow),
            Err(AggregateError::NegativeMultiplicity)
        );
        assert_eq!(state, seeded(&before));
        assert_eq!(
            state.apply(&input([(99, Some(1), -1)]), LIMBS, &mut allow),
            Err(AggregateError::NegativeMultiplicity)
        );
        assert_eq!(state, seeded(&before));
    }

    #[test]
    fn dropped_guards_and_every_control_refusal_preserve_all_groups_and_allow_retry() {
        let before = input([
            (0, None, 1),
            (0, Some(1), 2),
            (0, Some(7), 1),
            (1, Some(9), 1),
        ]);
        let changes = input([
            (0, Some(1), -2),
            (0, Some(3), 1),
            (1, Some(9), -1),
            (2, None, 1),
        ]);
        let mut state = seeded(&before);
        {
            let update = state.prepare(&changes, LIMBS, &mut allow).unwrap();
            assert!(!update.delta().is_empty());
        }
        assert_eq!(state, seeded(&before));
        let mut calls = 0;
        let expected_delta = state
            .apply(&changes, LIMBS, &mut |_| {
                calls += 1;
                Ok::<_, usize>(())
            })
            .unwrap();
        for stop in 1..=calls {
            let mut attempt = seeded(&before);
            let mut seen = 0;
            assert_eq!(
                attempt.apply(&changes, LIMBS, &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                }),
                Err(AggregateError::ZSet(ZSetError::Control(stop)))
            );
            assert_eq!(seen, stop);
            assert_eq!(attempt, seeded(&before));
            assert_eq!(
                attempt.apply(&changes, LIMBS, &mut allow).unwrap(),
                expected_delta
            );
            assert_eq!(attempt, state);
        }
    }

    #[test]
    fn promotion_is_exact_and_arithmetic_or_weight_admission_refuses_before_publication() {
        let large = input([(0, Some(2), i128::MAX)]);
        let mut state = IncrementalAggregate::new();
        assert!(matches!(
            state.apply(&large, LimbLimit::new(0), &mut allow),
            Err(AggregateError::ZSet(ZSetError::Arithmetic(_)))
        ));
        assert_eq!(state.group_count(), 0);
        state.apply(&large, LIMBS, &mut allow).unwrap();
        assert!(state.get(&0).unwrap().sum().unwrap().is_promoted());
        state
            .apply(&input([(0, Some(2), 1 - i128::MAX)]), LIMBS, &mut allow)
            .unwrap();
        assert_eq!(state.get(&0).unwrap().sum(), Some(&ZWeight::from_i128(2)));
        assert_eq!(state.get(&0).unwrap().count_rows(), &ZWeight::ONE);

        let mut state = seeded(&input([(0, Some(0), i128::MAX)]));
        state
            .apply(&input([(0, Some(0), 1)]), LIMBS, &mut allow)
            .unwrap();
        let row = state.get(&0).unwrap();
        assert!(row.count_rows().is_promoted());
        assert!(row.count_values().is_promoted());
        assert_eq!(row.sum(), Some(&ZWeight::ZERO));
        let promoted = ZWeight::from_i128(i128::MAX)
            .checked_add(&ZWeight::ONE, LIMBS)
            .unwrap();
        let transferred =
            ZSet::from_updates([((3, Some(0)), promoted)], LIMBS, &mut allow).unwrap();
        assert!(matches!(
            state.apply(&transferred, LimbLimit::new(0), &mut allow),
            Err(AggregateError::ZSet(ZSetError::WeightAdmission { .. }))
        ));
        assert_eq!(state.group_count(), 1);
    }

    #[test]
    fn sparse_updates_do_not_scan_unrelated_groups_or_unchanged_value_interiors() {
        let small = input([(0, Some(0), 1), (0, Some(5000), 1), (0, Some(9999), 1)]);
        let large = input(
            (0..10000)
                .map(|value| (0, Some(value), 1))
                .chain((1..1000).map(|key| (key, Some(1), 1))),
        );
        let delta = input([(0, Some(5000), 1)]);
        let measure = |initial: &Input| {
            let mut state = seeded(initial);
            let mut events = Vec::new();
            state
                .apply(&delta, LIMBS, &mut |event| {
                    events.push(event);
                    Ok::<_, usize>(())
                })
                .unwrap();
            events
        };
        assert_eq!(measure(&small), measure(&large));
    }

    #[test]
    fn exact_work_and_growth_limits_succeed_and_one_below_refuses_atomically() {
        let initial = input([(0, Some(1), 1), (0, Some(2), 1)]);
        let delta = input([(0, Some(1), -1), (0, Some(3), 1), (1, None, 1)]);
        let mut measured = seeded(&initial);
        let (mut work, mut scratch) = (0, 0);
        measured
            .apply(&delta, LIMBS, &mut |event| {
                work += 1;
                scratch += usize::from(event == ZSetEvent::ScratchEntry);
                Ok::<_, usize>(())
            })
            .unwrap();
        for (max_work, max_scratch, pass) in [
            (work, scratch, true),
            (work - 1, scratch, false),
            (work, scratch - 1, false),
        ] {
            let mut state = seeded(&initial);
            let (mut seen_work, mut seen_scratch) = (0, 0);
            let result = state.apply(&delta, LIMBS, &mut |event| {
                seen_work += 1;
                seen_scratch += usize::from(event == ZSetEvent::ScratchEntry);
                if seen_work > max_work || seen_scratch > max_scratch {
                    Err(1)
                } else {
                    Ok(())
                }
            });
            assert_eq!(result.is_ok(), pass);
            if pass {
                assert_eq!(state, measured);
            } else {
                assert_eq!(state, seeded(&initial));
            }
        }
    }

    #[test]
    fn debug_output_does_not_export_group_keys_or_summary_values() {
        let mut state = IncrementalAggregate::new();
        let rows = ZSet::from_updates(
            [(("private-group", Some(919191)), ZWeight::ONE)],
            LIMBS,
            &mut allow,
        )
        .unwrap();
        let delta = state.apply(&rows, LIMBS, &mut allow).unwrap();
        let text = format!(
            "{state:?} {delta:?} {:?}",
            state.get(&"private-group").unwrap()
        );
        assert!(!text.contains("private-group"));
        assert!(!text.contains("919191"));
        let update = state.prepare(&rows, LIMBS, &mut allow).unwrap();
        assert!(!format!("{update:?}").contains("private-group"));
    }

    #[test]
    fn distinct_statistics_change_only_at_support_zero_crossings() {
        let mut state = seeded(&input([(0, None, 2), (0, Some(-4), 3), (0, Some(10), 1)]));
        let assert_distinct = |state: &IncrementalAggregate<i32>, expected_sum, expected_count| {
            let row = state.get(&0).unwrap();
            assert_eq!(row.count_distinct().to_i128(), Some(expected_count));
            assert_eq!(row.sum_distinct().and_then(ZWeight::to_i128), expected_sum);
            assert_eq!(
                row.average_distinct_parts()
                    .map(|(sum, count)| (sum.to_i128().unwrap(), count.to_i128().unwrap())),
                expected_sum.map(|sum| (sum, expected_count))
            );
        };
        assert_distinct(&state, Some(6), 2);
        state
            .apply(&input([(0, Some(-4), -2)]), LIMBS, &mut allow)
            .unwrap();
        assert_distinct(&state, Some(6), 2);
        state
            .apply(
                &input([(0, Some(-4), -1), (0, Some(10), 5)]),
                LIMBS,
                &mut allow,
            )
            .unwrap();
        assert_distinct(&state, Some(10), 1);
        state
            .apply(&input([(0, Some(10), -6)]), LIMBS, &mut allow)
            .unwrap();
        assert_distinct(&state, None, 0);
        assert_eq!(state.get(&0).unwrap().count_rows(), &ZWeight::from_i128(2));
        state
            .apply(&input([(0, Some(0), 1)]), LIMBS, &mut allow)
            .unwrap();
        assert_distinct(&state, Some(0), 1);
    }

    #[test]
    fn distinct_sum_promotes_and_retracts_full_width_extrema_exactly() {
        let before = input([(0, Some(i128::MIN), 1), (0, Some(-1), 1)]);
        let mut state = seeded(&before);
        assert!(state.get(&0).unwrap().sum_distinct().unwrap().is_promoted());
        state
            .apply(&input([(0, Some(i128::MIN), -1)]), LIMBS, &mut allow)
            .unwrap();
        assert_eq!(
            state.get(&0).unwrap().sum_distinct().unwrap().to_i128(),
            Some(-1)
        );
        let mut refused = IncrementalAggregate::new();
        assert!(
            refused
                .apply(&before, LimbLimit::new(0), &mut allow)
                .is_err()
        );
        assert_eq!(refused.group_count(), 0);
    }
}

#[cfg(test)]
mod prospective_tests {
    use super::*;

    #[test]
    fn prospective_reads_shadow_removals_and_abort_without_changing_the_owner() {
        let limbs = LimbLimit::new(4);
        let mut allow = |_| Ok::<_, ()>(());
        let initial = ZSet::from_updates(
            [
                ((1, Some(7)), ZWeight::ONE),
                ((2, Some(5)), ZWeight::ONE),
                ((3, None), ZWeight::ONE),
            ],
            limbs,
            &mut allow,
        )
        .unwrap();
        let delta = ZSet::from_updates(
            [
                ((1, Some(7)), ZWeight::from_i128(-1)),
                ((2, Some(9)), ZWeight::ONE),
                ((4, Some(-2)), ZWeight::ONE),
            ],
            limbs,
            &mut allow,
        )
        .unwrap();
        let mut aggregate = IncrementalAggregate::new();
        aggregate.apply(&initial, limbs, &mut allow).unwrap();
        let before = aggregate.snapshot(limbs, &mut allow).unwrap();
        {
            let pending = aggregate.prepare(&delta, limbs, &mut allow).unwrap();
            assert!(pending.get(&1).is_none());
            assert_eq!(pending.get(&2).unwrap().sum().unwrap().to_i128(), Some(14));
            assert_eq!(pending.get(&3).unwrap().count_rows(), &ZWeight::ONE);
            assert!(pending.get(&3).unwrap().sum().is_none());
            assert_eq!(pending.get(&4).unwrap().minimum(), Some(-2));
            assert!(pending.get(&5).is_none());
            assert_eq!(
                pending
                    .retained_range(1..=3)
                    .map(|(key, _)| *key)
                    .collect::<Vec<_>>(),
                vec![1, 2, 3]
            );
            assert_eq!(
                pending
                    .changed_range(1..=4)
                    .map(|(key, row)| (*key, row.is_some()))
                    .collect::<Vec<_>>(),
                vec![(1, false), (2, true), (4, true)]
            );
            assert_eq!(
                pending
                    .changed_range(2..=4)
                    .next_back()
                    .map(|(key, _)| *key),
                Some(4)
            );
        }
        assert_eq!(aggregate.snapshot(limbs, &mut allow).unwrap(), before);
        aggregate
            .prepare(&delta, limbs, &mut allow)
            .unwrap()
            .commit();
        assert!(aggregate.get(&1).is_none());
        assert_eq!(
            aggregate.get(&2).unwrap().sum().unwrap().to_i128(),
            Some(14)
        );
        assert_eq!(
            aggregate
                .range(2..=3)
                .map(|(key, _)| *key)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
    }
}
