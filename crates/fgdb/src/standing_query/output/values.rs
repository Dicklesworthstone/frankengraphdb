//! Exact occurrence windows over incrementally maintained complete tuples.
//!
//! Only changed carriers are decoded. Candidates outside the selected page
//! remain indexed for deletion/refill; no snapshot evaluator is used here.

use super::{Meter, StandingQueryFailure, govern, reserve_row, zset_error};
use fgdb_delta_types::{LimbLimit, ZSet, ZSetEvent, ZWeight};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{GraphAggregateRow, GraphAggregateOrder, GraphAggregateColumn, GraphNullPlacement};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;

#[derive(Clone)]
struct Rank {
    order: Arc<[GraphAggregateOrder]>,
    source: Arc<GraphAggregateRow>,
}

fn order_field(order: &GraphAggregateOrder) -> (u8, usize, bool, bool) {
    let (kind, column) = match order.column {
        GraphAggregateColumn::GroupKey(column) => (0, column),
        GraphAggregateColumn::Aggregate(column) => (1, column),
    };
    (kind, column, order.descending, order.nulls == GraphNullPlacement::Last)
}

impl Ord for Rank {
    fn cmp(&self, other: &Self) -> Ordering {
        let schema = self.order.iter().map(order_field).cmp(other.order.iter().map(order_field))
            .then_with(|| (self.source.keys().len(), self.source.values().len())
                .cmp(&(other.source.keys().len(), other.source.values().len())));
        if schema != Ordering::Equal { return schema; }
        match self.source.compare_incremental_order(&other.source, &self.order,
            &mut |_| Ok::<_, Infallible>(())) {
            Ok(Some(order)) => order,
            Ok(None) => self.source.cmp(&other.source),
            Err(never) => match never {},
        }
    }
}
impl PartialOrd for Rank {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}
impl PartialEq for Rank {
    fn eq(&self, other: &Self) -> bool { self.cmp(other) == Ordering::Equal }
}
impl Eq for Rank {}

#[derive(Clone)]
struct Candidate {
    row: Arc<GraphValueRow>,
    count: u64,
}

type Candidates = BTreeMap<Rank, Candidate>;
type Patch = BTreeMap<Rank, Option<Candidate>>;

pub(super) struct State {
    pub(super) definition: PreparedGraphPattern<GraphValueRow>,
    order: Arc<[GraphAggregateOrder]>,
    distinct: bool,
    offset: u64,
    count: Option<u64>,
    candidates: Candidates,
    pub(super) rows: ZSet<GraphValueRow>,
    pub(super) page: Vec<Arc<GraphValueRow>>,
}

fn copy_value(row: &GraphValueRow, meter: &mut Meter<'_>)
    -> Result<GraphValueRow, StandingQueryFailure>
{
    meter.charge(ZSetEvent::ScratchEntry)?;
    let mut values = Vec::new();
    for value in row.values() {
        values.push(value.copy_with_control(&mut |event| govern(meter, event))?);
    }
    Ok(GraphValueRow::from_owned_values(values))
}

impl State {
    pub(super) fn new(definition: PreparedGraphPattern<GraphValueRow>) -> Option<Self> {
        let (distinct, offset, count) = definition.incremental_row_window()?;
        let order = Arc::from(definition.incremental_row_ordering()?);
        Some(Self { definition, order, distinct, offset, count, candidates: BTreeMap::new(),
            rows: ZSet::new(), page: Vec::new() })
    }

    pub(super) fn prepare(&mut self, delta: &ZSet<GraphAggregateRow>, meter: &mut Meter<'_>)
        -> Result<Update<'_>, StandingQueryFailure>
    {
        let mut patch = Patch::new();
        // A source update is a retraction of the previous complete carrier and
        // an assertion of the next. Never threshold a signed derivative.
        for wanted in [-1, 1] {
            for (source, weight) in delta.iter() {
                meter.charge(ZSetEvent::Work)?;
                let sign = weight.to_i128().ok_or(StandingQueryFailure::Arithmetic)?;
                if !matches!(sign, -1 | 1) { return Err(StandingQueryFailure::InvalidDelta); }
                if sign != wanted { continue; }
                let (row, count) = self.definition.materialize_incremental_values(source,
                    &mut |event| govern(meter, event))?.ok_or(StandingQueryFailure::InvalidDelta)?;
                source.compare_incremental_order(source, &self.order,
                    &mut |event| govern(meter, event))?.ok_or(StandingQueryFailure::InvalidDelta)?;
                reserve_row(source, meter)?;
                meter.units(ZSetEvent::ScratchEntry, 3)?;
                let rank = Rank { order: Arc::clone(&self.order), source: Arc::new(source.clone()) };
                let retained = self.candidates.get_key_value(&rank);
                if sign < 0 {
                    if retained.is_none_or(|(key, value)| key.source.as_ref() != source
                        || value.count != count || value.row.as_ref() != &row)
                        || patch.contains_key(&rank) {
                        return Err(StandingQueryFailure::InvalidDelta);
                    }
                    patch.insert(rank, None);
                } else {
                    if patch.get(&rank).is_some_and(Option::is_some)
                        || (retained.is_some() && !matches!(patch.get(&rank), Some(None))) {
                        return Err(StandingQueryFailure::InvalidDelta);
                    }
                    // Replace comparator-equal keys, not just their values:
                    // a group's canonical representative can change.
                    patch.remove(&rank);
                    patch.insert(rank, Some(Candidate { row: Arc::new(row), count }));
                }
            }
        }
        let page = select_page(&self.candidates, &patch, self.distinct, self.offset, self.count, meter)?;
        // The selected page is the bounded publication unit. It is rebuilt
        // from the rank-index prefix, never from the graph or all candidates.
        let mut updates = Vec::new();
        for row in &page {
            let row = copy_value(row, meter)?;
            meter.charge(ZSetEvent::ScratchEntry)?;
            updates.push((row, ZWeight::ONE));
        }
        let rows = ZSet::from_updates(updates, LimbLimit::new(4), &mut |event| meter.charge(event))
            .map_err(zset_error)?;
        (meter.checkpoint)()?;
        Ok(Update { state: self, patch, page, rows })
    }
}

fn select_page(base: &Candidates, patch: &Patch, distinct: bool, mut offset: u64,
    count: Option<u64>, meter: &mut Meter<'_>) -> Result<Vec<Arc<GraphValueRow>>, StandingQueryFailure>
{
    meter.charge(ZSetEvent::ScratchEntry)?;
    let mut selected = Vec::new();
    let mut old = base.iter().peekable();
    let mut changed = patch.iter().peekable();
    while count.is_none_or(|limit| (selected.len() as u128) < u128::from(limit)) {
        let next = match (old.peek(), changed.peek()) {
            (None, None) => break,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some((a, _)), Some((b, _))) => a.cmp(b),
        };
        meter.charge(ZSetEvent::Work)?;
        let value = match next {
            Ordering::Less => old.next().map(|(_, value)| value),
            Ordering::Greater => changed.next().and_then(|(_, value)| value.as_ref()),
            Ordering::Equal => {
                old.next();
                changed.next().and_then(|(_, value)| value.as_ref())
            }
        };
        let Some(value) = value else { continue; };
        let occurrences = if distinct { 1 } else { value.count };
        let skipped = offset.min(occurrences);
        offset -= skipped;
        let available = occurrences - skipped;
        let take = match count {
            Some(limit) => available.min(limit - u64::try_from(selected.len())
                .map_err(|_| StandingQueryFailure::ResultBudget)?),
            None => available,
        };
        let next_len = (selected.len() as u128).checked_add(u128::from(take))
            .ok_or(StandingQueryFailure::Arithmetic)?;
        if next_len > usize::MAX as u128 || meter.policy.rows.max_result_rows()
            .is_some_and(|limit| next_len > u128::from(limit)) {
            return Err(StandingQueryFailure::ResultBudget);
        }
        // Skip counts arithmetically, but charge every allocated occurrence.
        // A huge carrier with LIMIT 1 never expands its unselected duplicates.
        for _ in 0..take {
            meter.charge(ZSetEvent::Work)?;
            meter.charge(ZSetEvent::ScratchEntry)?;
            selected.push(Arc::clone(&value.row));
        }
    }
    Ok(selected)
}

#[must_use = "dropping a row output update aborts it"]
pub(super) struct Update<'a> {
    state: &'a mut State,
    patch: Patch,
    page: Vec<Arc<GraphValueRow>>,
    rows: ZSet<GraphValueRow>,
}

impl Update<'_> {
    pub(super) fn commit(self) {
        for (rank, value) in self.patch {
            self.state.candidates.remove(&rank);
            if let Some(value) = value { self.state.candidates.insert(rank, value); }
        }
        self.state.rows = self.rows;
        self.state.page = self.page;
    }
}

#[cfg(test)]
mod tests {
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
            let key = fgdb_gql::algebra::GraphValue::Scalar(CanonicalScalar::Int(key));
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
        assert_eq!(state.page.len(), 3);
        assert_eq!(state.rows.iter().map(|(_, w)| w.to_i128().unwrap()).collect::<Vec<_>>(), [2, 1]);
        apply(&mut state, &[(1, 3, -1), (1, 1, 1)], 3).unwrap();
        assert_eq!(state.page.len(), 3);
        assert_eq!(state.rows.iter().map(|(_, w)| w.to_i128().unwrap()).collect::<Vec<_>>(), [2, 1]);
        assert_eq!(state.candidates.len(), 3);
    }

    #[test]
    fn distinct_thresholds_integrated_count_and_huge_bags_skip_without_expanding() {
        let mut distinct = State::new(definition(true, 0, None)).unwrap();
        apply(&mut distinct, &[(1, u64::MAX, 1), (2, 2, 1)], 2).unwrap();
        assert_eq!(distinct.page.len(), 2);
        apply(&mut distinct, &[(1, u64::MAX, -1), (1, 1, 1)], 2).unwrap();
        assert!(distinct.rows.iter().all(|(_, weight)| weight.to_i128() == Some(1)));
        let mut bag = State::new(definition(false, u64::MAX - 1, Some(1))).unwrap();
        apply(&mut bag, &[(1, u64::MAX, 1)], 1).unwrap();
        assert_eq!(bag.page.len(), 1);
    }

    #[test]
    fn refusal_and_dropped_update_preserve_candidates_and_published_page() {
        let mut state = State::new(definition(false, 0, None)).unwrap();
        apply(&mut state, &[(1, 1, 1)], 1).unwrap();
        let old = state.rows.clone();
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
