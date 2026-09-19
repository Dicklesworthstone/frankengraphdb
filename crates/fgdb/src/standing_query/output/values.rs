//! Ordinary MATCH result bags, ordered by the original GLA result contract.
//!
//! The shared graph maintainer supplies complete tuples with positive counts.
//! Retain one candidate per tuple, threshold for DISTINCT and slice occurrences
//! only at final output. Deleted winners refill from retained candidates. No
//! graph is read here and no hidden duplicate count is expanded for OFFSET.

use super::{Meter, StandingQueryFailure, zset_error};
use fgdb_delta_types::{LimbLimit, ZSet, ZSetEvent, ZWeight};
use fgdb_delta_types::zset::ZSetUpdate;
use fgdb_gql::{GlaExecutionEvent, GraphAggregateOrder, GraphAggregateRow};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Clone, Debug)]
struct Rank {
    counted: Arc<GraphAggregateRow>,
    ordering: Arc<[GraphAggregateOrder]>,
}
impl PartialEq for Rank {
    fn eq(&self, other: &Self) -> bool { self.cmp(other) == Ordering::Equal }
}
impl Eq for Rank {}
impl PartialOrd for Rank {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}
impl Ord for Rank {
    fn cmp(&self, other: &Self) -> Ordering {
        // Only this immutable query's schema-checked keys inhabit its maps.
        // As for the existing ZSet/ranked sink, BTree key comparison and
        // allocator costs are outside the logical entry-event accounting.
        self.counted.compare_incremental_order(&other.counted, &self.ordering,
            &mut |_| Ok::<_, core::convert::Infallible>(())).unwrap()
            .expect("row rank schema and ordering were admitted before insertion")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Tuple {
    row: Arc<GraphValueRow>,
    count: u64,
}

type Candidates = BTreeMap<Rank, Tuple>;
type Changes = BTreeMap<Rank, Option<Tuple>>;

pub(crate) struct State {
    definition: PreparedGraphPattern<GraphValueRow>,
    ordering: Arc<[GraphAggregateOrder]>,
    distinct: bool,
    offset: u64,
    count: Option<u64>,
    candidates: Candidates,
    pub(super) rows: ZSet<GraphValueRow>,
    pub(super) ordered: Vec<Arc<GraphValueRow>>,
}
impl core::fmt::Debug for State {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StandingRows")
            .field("candidates", &self.candidates.len())
            .field("selected_occurrences", &self.ordered.len())
            .field("data", &"[REDACTED]").finish()
    }
}

fn govern(meter: &mut Meter<'_>, event: GlaExecutionEvent) -> Result<(), StandingQueryFailure> {
    meter.charge(ZSetEvent::Work)?;
    match event {
        GlaExecutionEvent::Work => Ok(()),
        GlaExecutionEvent::ScratchEntry => meter.charge(ZSetEvent::ScratchEntry),
        GlaExecutionEvent::ResultRow => Err(StandingQueryFailure::InvalidDelta),
    }
}

fn reserve_values(values: &[GraphValue], meter: &mut Meter<'_>) -> Result<(), StandingQueryFailure> {
    meter.charge(ZSetEvent::ScratchEntry)?;
    for value in values {
        meter.charge(ZSetEvent::Work)?;
        meter.units(ZSetEvent::ScratchEntry, 1 + value.payload_units())?;
    }
    Ok(())
}

impl State {
    pub(super) fn new(definition: PreparedGraphPattern<GraphValueRow>) -> Option<Self> {
        let (distinct, offset, count) = definition.incremental_row_window()?;
        let ordering = definition.incremental_row_ordering()?.into();
        Some(Self { definition, ordering, distinct, offset, count,
            candidates: BTreeMap::new(), rows: ZSet::new(), ordered: Vec::new() })
    }

    pub(super) fn definition(&self) -> &PreparedGraphPattern<GraphValueRow> { &self.definition }

    /// Retractions validate full before-images, including counts. A new count
    /// for an existing tuple requires its old count's retraction in this tick.
    /// Counts stay compressed until selected occurrences enter the public page.
    pub(super) fn prepare(
        &mut self,
        delta: &ZSet<GraphAggregateRow>,
        meter: &mut Meter<'_>,
    ) -> Result<Update<'_>, StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        let mut changes = Changes::new();
        for wanted in [-1, 1] {
            for (counted, weight) in delta.iter() {
                meter.charge(ZSetEvent::Work)?;
                let sign = weight.to_i128().ok_or(StandingQueryFailure::Arithmetic)?;
                if !matches!(sign, -1 | 1) { return Err(StandingQueryFailure::InvalidDelta); }
                if sign != wanted { continue; }
                let (row, count) = self.definition.materialize_incremental_values(counted,
                    &mut |event| govern(meter, event))?.ok_or(StandingQueryFailure::InvalidDelta)?;
                reserve_values(counted.keys(), meter)?;
                meter.units(ZSetEvent::ScratchEntry, 4)?;
                let rank = Rank { counted: Arc::new(counted.clone()), ordering: Arc::clone(&self.ordering) };
                let retained = self.candidates.get_key_value(&rank);
                if sign < 0 {
                    if retained.is_none_or(|(old, _)| old.counted.as_ref() != counted)
                        || changes.contains_key(&rank)
                    { return Err(StandingQueryFailure::InvalidDelta); }
                    changes.insert(rank, None);
                } else {
                    if changes.get(&rank).is_some_and(Option::is_some)
                        || (retained.is_some() && !matches!(changes.get(&rank), Some(None)))
                    { return Err(StandingQueryFailure::InvalidDelta); }
                    // BTreeMap::insert retains the old key on equality. Replace
                    // it explicitly so future before-image checks see NEW count.
                    changes.remove(&rank);
                    changes.insert(rank, Some(Tuple { row: Arc::new(row), count }));
                }
            }
        }
        let limbs = LimbLimit::new(4);
        let mut updates = Vec::new();
        let next_page = if changes.is_empty() {
            // Empty committed ticks advance the source frontier, not scan a
            // retained page or manufacture result changes.
            if meter.policy.rows.max_result_rows().is_some_and(|limit| self.ordered.len() as u128 > u128::from(limit)) {
                return Err(StandingQueryFailure::ResultBudget);
            }
            None
        } else {
            let (page, selected) = self.select(&changes, meter)?;
            for (row, weight) in self.rows.iter() {
                meter.charge(ZSetEvent::Work)?;
                let count = weight.to_i128().ok_or(StandingQueryFailure::Arithmetic)?;
                if count <= 0 { return Err(StandingQueryFailure::InvalidDelta); }
                reserve_values(row.values(), meter)?;
                updates.push((row.clone(), ZWeight::from_i128(-count)));
            }
            for (row, count) in selected {
                reserve_values(row.values(), meter)?;
                updates.push((row.as_ref().clone(), ZWeight::from_i128(i128::from(count))));
            }
            Some(page)
        };
        let delta = ZSet::from_updates(updates, limbs, &mut |event| meter.charge(event)).map_err(zset_error)?;
        // prepare_update clones changed keys; reserve those payloads separately.
        for (row, _) in delta.iter() { reserve_values(row.values(), meter)?; }
        let sink = self.rows.prepare_update(&delta, limbs, &mut |event| meter.charge(event)).map_err(zset_error)?;
        (meter.checkpoint)()?;
        Ok(Update { candidates: &mut self.candidates, ordered: &mut self.ordered,
            changes, next_page, sink })
    }

    fn select(
        &self,
        changes: &Changes,
        meter: &mut Meter<'_>,
    ) -> Result<Selection, StandingQueryFailure> {
        let mut old = self.candidates.iter().peekable();
        let mut patch = changes.iter().peekable();
        let mut skip = self.offset;
        let mut remaining = self.count.unwrap_or(u64::MAX);
        let mut selected_count = 0u64;
        let mut page = Vec::new();
        let mut selected = Vec::new();
        meter.units(ZSetEvent::ScratchEntry, 2)?;
        // Merge only the visited sorted prefix. A replacement shadows the
        // retained key, including deletion; do not fall through to old support.
        while self.count.is_none() || remaining != 0 {
            meter.charge(ZSetEvent::Work)?;
            let ordering = match (old.peek(), patch.peek()) {
                (None, None) => break,
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (Some((left, _)), Some((right, _))) => left.cmp(right),
            };
            let tuple = match ordering {
                Ordering::Less => old.next().map(|(_, tuple)| tuple),
                Ordering::Greater => patch.next().and_then(|(_, tuple)| tuple.as_ref()),
                Ordering::Equal => {
                    old.next();
                    patch.next().and_then(|(_, tuple)| tuple.as_ref())
                }
            };
            let Some(tuple) = tuple else { continue; };
            let weight = if self.distinct { 1 } else { tuple.count };
            let skipped = skip.min(weight);
            skip -= skipped;
            let take = if self.count.is_some() { (weight - skipped).min(remaining) } else { weight - skipped };
            if take == 0 { continue; }
            selected_count = selected_count.checked_add(take).ok_or(StandingQueryFailure::ResultBudget)?;
            if meter.policy.rows.max_result_rows().is_some_and(|limit| selected_count > limit) {
                return Err(StandingQueryFailure::ResultBudget);
            }
            meter.charge(ZSetEvent::ScratchEntry)?;
            selected.push((Arc::clone(&tuple.row), take));
            // Only selected occurrences expand. Each Arc slot is admitted and
            // interruptible; never preallocate from an unchecked u64 count.
            for _ in 0..take {
                meter.charge(ZSetEvent::Work)?;
                meter.charge(ZSetEvent::ScratchEntry)?;
                page.push(Arc::clone(&tuple.row));
            }
            if self.count.is_some() { remaining -= take; }
        }
        Ok((page, selected))
    }
}

type Selection = (Vec<Arc<GraphValueRow>>, Vec<(Arc<GraphValueRow>, u64)>);

#[must_use = "dropping a row-output update aborts it"]
pub(super) struct Update<'a> {
    candidates: &'a mut Candidates,
    ordered: &'a mut Vec<Arc<GraphValueRow>>,
    changes: Changes,
    next_page: Option<Vec<Arc<GraphValueRow>>>,
    sink: ZSetUpdate<'a, GraphValueRow>,
}
impl Update<'_> {
    pub(super) fn commit(self) {
        let Self { candidates, ordered, changes, next_page, sink } = self;
        for (rank, tuple) in changes {
            candidates.remove(&rank);
            if let Some(tuple) = tuple { candidates.insert(rank, tuple); }
        }
        if let Some(page) = next_page { *ordered = page; }
        sink.commit();
    }
}

#[cfg(test)]
#[path = "values/tests.rs"]
mod tests;

// Preserve the focused regressions introduced with the original output stage.
#[cfg(test)]
mod occurrence_regressions {
    use super::*;
    use crate::standing_query::StandingQueryStats;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder};
    use fgdb_gql::{GqlQueryPolicy, GraphAggregateValue};
    use fgdb_types::CanonicalScalar;

    fn definition(distinct: bool, offset: u64, count: Option<u64>) -> PreparedGraphPattern<GraphValueRow> {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        let pattern = builder.prepare_values(&[GraphColumn::property("v", "n", PropertyKeyId(1))],
            offset, count).unwrap();
        if distinct { pattern } else { pattern.with_duplicates() }
    }
    fn delta(definition: &PreparedGraphPattern<GraphValueRow>, rows: &[(i64, u64, i128)]) -> ZSet<GraphAggregateRow> {
        let carrier = definition.incremental_row_source_definition().unwrap();
        ZSet::from_updates(rows.iter().map(|&(key, count, sign)| {
            let key = GraphValue::Scalar(CanonicalScalar::Int(key));
            (carrier.materialize_incremental_row(vec![key], vec![GraphAggregateValue::Count(count)]).unwrap(),
                ZWeight::from_i128(sign))
        }), LimbLimit::new(4), &mut |_| Ok::<_, ()>(())).unwrap()
    }
    fn apply(state: &mut State, changes: &[(i64, u64, i128)], limit: u64) -> Result<(), StandingQueryFailure> {
        let delta = delta(&state.definition, changes);
        let mut checkpoint = || Ok(());
        let mut meter = Meter { policy: GqlQueryPolicy::new(100_000, limit, 1_000_000, 1_000_000),
            stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
        state.prepare(&delta, &mut meter)?.commit();
        Ok(())
    }

    #[test]
    fn bag_windows_count_occurrences_and_refill_from_retained_candidates() {
        let mut state = State::new(definition(false, 1, Some(3))).unwrap();
        apply(&mut state, &[(1, 3, 1), (2, 2, 1), (3, 1, 1)], 3).unwrap();
        assert_eq!(state.ordered.len(), 3);
        assert_eq!(state.rows.iter().map(|(_, w)| w.to_i128().unwrap()).collect::<Vec<_>>(), [2, 1]);
        apply(&mut state, &[(1, 3, -1), (1, 1, 1)], 3).unwrap();
        assert_eq!(state.ordered.len(), 3);
        assert_eq!(state.rows.iter().map(|(_, w)| w.to_i128().unwrap()).collect::<Vec<_>>(), [2, 1]);
        assert_eq!(state.candidates.len(), 3);
    }

    #[test]
    fn distinct_thresholds_integrated_count_and_huge_bags_skip_without_expanding() {
        let mut distinct = State::new(definition(true, 0, None)).unwrap();
        apply(&mut distinct, &[(1, u64::MAX, 1), (2, 2, 1)], 2).unwrap();
        assert_eq!(distinct.ordered.len(), 2);
        apply(&mut distinct, &[(1, u64::MAX, -1), (1, 1, 1)], 2).unwrap();
        assert!(distinct.rows.iter().all(|(_, weight)| weight.to_i128() == Some(1)));
        let mut bag = State::new(definition(false, u64::MAX - 1, Some(1))).unwrap();
        apply(&mut bag, &[(1, u64::MAX, 1)], 1).unwrap();
        assert_eq!(bag.ordered.len(), 1);
    }

    #[test]
    fn refusal_and_dropped_update_preserve_candidates_and_published_page() {
        let mut state = State::new(definition(false, 0, None)).unwrap();
        apply(&mut state, &[(1, 1, 1)], 1).unwrap();
        let old = state.rows.checked_clone(LimbLimit::new(4), &mut |_| Ok::<_, ()>(())).unwrap();
        assert_eq!(apply(&mut state, &[(1, 1, -1), (1, 2, 1)], 1), Err(StandingQueryFailure::ResultBudget));
        assert_eq!(state.rows, old);
        let change = delta(&state.definition, &[(1, 1, -1), (2, 1, 1)]);
        let mut checkpoint = || Ok(());
        let mut meter = Meter { policy: GqlQueryPolicy::new(100, 1, 10_000, 10_000),
            stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
        drop(state.prepare(&change, &mut meter).unwrap());
        assert_eq!(state.rows, old);
        state.prepare(&change, &mut meter).unwrap().commit();
        assert_ne!(state.rows, old);
        assert_eq!(state.candidates.len(), 1);
    }
}
