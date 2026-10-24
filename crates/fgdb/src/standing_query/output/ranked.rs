//! Retained ranked candidates and deletion-safe result windows.
//!
//! Rank changes touch changed groups/classes only. A finite page merges the
//! retained rank index with its private patch through offset + count entries;
//! it does not sort or rescan the graph. Large offsets cost their visited
//! prefix, and unbounded output materializes all result occurrences. Keeping
//! candidates outside the page is mandatory for deletion/refill correctness.
//! Collection comparisons have the same canonical-key cost boundary as ZSet:
//! metering counts probes, visited entries and reserved payloads, not allocator
//! bytes or preemption inside BTreeMap's comparisons. No spill is claimed.

use super::*;
use core::convert::Infallible;
use fgdb_gql::{GraphAggregateColumn, GraphAggregateOrder, GraphNullPlacement};
use std::cmp::Ordering;

#[derive(Clone)]
struct Rank {
    order: Arc<[GraphAggregateOrder]>,
    group: Arc<GraphAggregateRow>,
    // Share the once-evaluated tuple with its class/candidate value. The raw
    // group remains separate for before-image validation and canonical ties.
    projected: Option<Arc<GraphAggregateRow>>,
}

fn order_field(order: &GraphAggregateOrder) -> (u8, usize, bool, bool) {
    let (kind, at) = match order.column {
        GraphAggregateColumn::GroupKey(at) => (0, at),
        GraphAggregateColumn::Aggregate(at) => (1, at),
    };
    (
        kind,
        at,
        order.descending,
        order.nulls == GraphNullPlacement::Last,
    )
}

impl Ord for Rank {
    fn cmp(&self, other: &Self) -> Ordering {
        // Collection keys have a total order even across separately prepared
        // schemas. A state itself admits only its one immutable schema.
        let schema = self
            .order
            .iter()
            .map(order_field)
            .cmp(other.order.iter().map(order_field))
            .then_with(|| self.projected.is_some().cmp(&other.projected.is_some()))
            .then_with(|| {
                self.projected
                    .as_ref()
                    .map(|row| (row.keys().len(), row.values().len()))
                    .cmp(
                        &other
                            .projected
                            .as_ref()
                            .map(|row| (row.keys().len(), row.values().len())),
                    )
            })
            .then_with(|| {
                (self.group.keys().len(), self.group.values().len())
                    .cmp(&(other.group.keys().len(), other.group.values().len()))
            });
        if schema != Ordering::Equal {
            return schema;
        }
        // Projected rows have no key prefix. Equal computed sort cells must
        // reach the ORIGINAL complete key, never the other visible columns.
        if let (Some(left), Some(right)) = (&self.projected, &other.projected) {
            match left
                .compare_incremental_order(right, &self.order, &mut |_| Ok::<_, Infallible>(()))
            {
                Ok(Some(Ordering::Equal)) => {}
                Ok(Some(order)) => return order,
                Ok(None) => return left.cmp(right).then_with(|| self.group.cmp(&other.group)),
                Err(never) => match never {},
            }
        }
        let order: &[GraphAggregateOrder] = if self.projected.is_some() {
            &[]
        } else {
            &self.order
        };
        match self
            .group
            .compare_incremental_order(&other.group, order, &mut |_| Ok::<_, Infallible>(()))
        {
            Ok(Some(order)) => order,
            // Unreachable for admitted ranks, but still total for an internal
            // malformed key. Preparation refuses malformed schemas first.
            Ok(None) => self.group.cmp(&other.group),
            Err(never) => match never {},
        }
    }
}
impl PartialOrd for Rank {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for Rank {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Rank {}

type Ranked = BTreeMap<Rank, Arc<GraphAggregateRow>>;
type RankPatch = BTreeMap<Rank, Option<Arc<GraphAggregateRow>>>;
type RankedClasses = BTreeMap<GraphAggregateRow, Ranked>;
type ClassPatch = BTreeMap<GraphAggregateRow, RankPatch>;
type Groups = BTreeMap<GroupKey, Rank>;
type GroupPatch = BTreeMap<GroupKey, Option<Rank>>;

#[derive(Default)]
pub(super) struct State {
    order: Option<Arc<[GraphAggregateOrder]>>,
    projected_order: bool,
    groups: Groups,
    classes: RankedClasses,
    candidates: Ranked,
    pub(super) page: Vec<Arc<GraphAggregateRow>>,
}

// Equal rank keys can contain different unreferenced summaries. Replace the
// key as well as its value, so retained source validation never uses an old
// full row after an update whose sort cells did not change.
fn stage(
    patch: &mut RankPatch,
    rank: Rank,
    value: Option<Arc<GraphAggregateRow>>,
    meter: &mut Meter<'_>,
) -> Result<(), StandingQueryFailure> {
    meter.charge(ZSetEvent::Work)?;
    meter.charge(ZSetEvent::ScratchEntry)?;
    patch.remove(&rank);
    patch.insert(rank, value);
    Ok(())
}

fn first_after<'a>(
    base: Option<&'a Ranked>,
    patch: &'a RankPatch,
    meter: &mut Meter<'_>,
) -> Result<Option<(&'a Rank, &'a Arc<GraphAggregateRow>)>, StandingQueryFailure> {
    let mut first = None;
    if let Some(base) = base {
        for (rank, row) in base {
            meter.charge(ZSetEvent::Work)?;
            if !patch.contains_key(rank) {
                first = Some((rank, row));
                break;
            }
        }
    }
    for (rank, row) in patch {
        meter.charge(ZSetEvent::Work)?;
        if let Some(row) = row {
            if first.is_none_or(|(current, _)| rank < current) {
                first = Some((rank, row));
            }
            break;
        }
    }
    Ok(first)
}

fn select_page(
    base: &Ranked,
    patch: &RankPatch,
    mut offset: u64,
    count: Option<u64>,
    meter: &mut Meter<'_>,
) -> Result<Vec<Arc<GraphAggregateRow>>, StandingQueryFailure> {
    meter.charge(ZSetEvent::ScratchEntry)?;
    let mut selected = Vec::new();
    let mut old = base.iter().peekable();
    let mut changed = patch.iter().peekable();
    while count.is_none_or(|count| (selected.len() as u128) < u128::from(count)) {
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
        let Some(value) = value else {
            continue;
        };
        if offset != 0 {
            offset -= 1;
            continue;
        }
        meter.charge(ZSetEvent::ScratchEntry)?;
        selected.push(Arc::clone(value));
    }
    Ok(selected)
}

impl State {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn prepare<'a>(
        &'a mut self,
        definition: &impl GroupDefinition,
        delta: &ZSet<GraphAggregateRow>,
        rows: &'a mut ZSet<GraphAggregateRow>,
        row_count: &'a mut u128,
        meter: &mut Meter<'_>,
    ) -> Result<Update<'a>, StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        let projected_order = !definition.projected_ordering().is_empty();
        if projected_order && !definition.ordering().is_empty() {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        // Lower result positions once into the existing exact row comparator.
        // No per-probe metadata allocation or expression evaluation is needed.
        let columns =
            || {
                definition.ordering().iter().copied().chain(
                    definition
                        .projected_ordering()
                        .iter()
                        .map(|order| GraphAggregateOrder {
                            column: GraphAggregateColumn::Aggregate(order.column),
                            descending: order.descending,
                            nulls: if order.nulls_first {
                                GraphNullPlacement::First
                            } else {
                                GraphNullPlacement::Last
                            },
                        }),
                )
            };
        let order = match &self.order {
            Some(order) => {
                if self.projected_order != projected_order || !order.iter().copied().eq(columns()) {
                    return Err(StandingQueryFailure::InvalidDelta);
                }
                Arc::clone(order)
            }
            None => {
                meter.units(
                    ZSetEvent::ScratchEntry,
                    1 + definition.ordering().len() + definition.projected_ordering().len(),
                )?;
                columns().collect::<Arc<[GraphAggregateOrder]>>()
            }
        };
        let distinct = definition.incremental_output_is_distinct();
        let mut groups: GroupPatch = BTreeMap::new();
        let mut classes: ClassPatch = BTreeMap::new();
        let mut candidates: RankPatch = BTreeMap::new();
        // All retractions precede all insertions, independent of row sort order.
        for wanted in [-1, 1] {
            for (row, weight) in delta.iter() {
                meter.charge(ZSetEvent::Work)?;
                let sign = weight.to_i128().ok_or(StandingQueryFailure::Arithmetic)?;
                if !matches!(sign, -1 | 1) {
                    return Err(StandingQueryFailure::InvalidDelta);
                }
                if sign != wanted {
                    continue;
                }
                // Evaluate EVERY changed qualifying group's output, even when
                // LIMIT 0 or a low rank means it cannot enter the current page.
                let projected = definition
                    .project_incremental_output(row, &mut |event| govern(meter, event))
                    .map_err(|error| match error {
                        GqlQueryError::Interrupted(reason) => reason,
                        GqlQueryError::Source(GraphAggregateError::OutputExpression {
                            column,
                            error,
                        }) => StandingQueryFailure::OutputExpression { column, error },
                        _ => StandingQueryFailure::InvalidDelta,
                    })?
                    .ok_or(StandingQueryFailure::InvalidDelta)?;
                if projected_order && !projected.keys().is_empty() {
                    return Err(StandingQueryFailure::InvalidDelta);
                }
                let computed_rank = if definition.has_computed_ordering() {
                    Some(
                        definition
                            .project_incremental_order(row, &projected, &mut |event| {
                                govern(meter, event)
                            })
                            .map_err(|error| match error {
                                GqlQueryError::Interrupted(reason) => reason,
                                GqlQueryError::Source(GraphAggregateError::OutputExpression {
                                    column,
                                    error,
                                }) => StandingQueryFailure::OutputExpression { column, error },
                                _ => StandingQueryFailure::InvalidDelta,
                            })?
                            .ok_or(StandingQueryFailure::InvalidDelta)?,
                    )
                } else {
                    None
                };
                let sort_row = computed_rank.as_ref().unwrap_or(if projected_order {
                    &projected
                } else {
                    row
                });
                if sort_row
                    .compare_incremental_order(sort_row, &order, &mut |event| govern(meter, event))?
                    .is_none()
                {
                    return Err(StandingQueryFailure::InvalidDelta);
                }
                let key = copy_group(row, meter)?;
                let old = self.groups.get(&key);
                if sign < 0 {
                    if old.map(|rank| rank.group.as_ref()) != Some(row) || groups.contains_key(&key)
                    {
                        return Err(StandingQueryFailure::InvalidDelta);
                    }
                } else if groups.get(&key).is_some_and(Option::is_some)
                    || (old.is_some() && !matches!(groups.get(&key), Some(None)))
                {
                    return Err(StandingQueryFailure::InvalidDelta);
                }
                reserve_row(row, meter)?;
                meter.charge(ZSetEvent::ScratchEntry)?;
                // Ordinary projected ranks share the visible tuple. Computed
                // ranks retain their private tuple separately; DISTINCT and
                // delivered candidates always own only the visible output.
                meter.charge(ZSetEvent::ScratchEntry)?;
                let projected = Arc::new(projected);
                let rank_output = if let Some(rank) = computed_rank {
                    meter.charge(ZSetEvent::ScratchEntry)?;
                    Some(Arc::new(rank))
                } else {
                    projected_order.then(|| Arc::clone(&projected))
                };
                let rank = Rank {
                    order: Arc::clone(&order),
                    group: Arc::new(row.clone()),
                    projected: rank_output,
                };
                meter.units(ZSetEvent::ScratchEntry, 2)?;
                groups.insert(key, if sign < 0 { None } else { Some(rank.clone()) });
                if distinct {
                    let identity =
                        projected.incremental_distinct_key(&mut |event| govern(meter, event))?;
                    if sign < 0
                        && self
                            .classes
                            .get(&identity)
                            .and_then(|class| class.get(&rank))
                            .map(Arc::as_ref)
                            != Some(projected.as_ref())
                    {
                        return Err(StandingQueryFailure::InvalidDelta);
                    }
                    let patch = match classes.entry(identity) {
                        Entry::Occupied(entry) => entry.into_mut(),
                        Entry::Vacant(entry) => {
                            meter.units(ZSetEvent::ScratchEntry, 2)?;
                            entry.insert(BTreeMap::new())
                        }
                    };
                    stage(
                        patch,
                        rank,
                        if sign < 0 { None } else { Some(projected) },
                        meter,
                    )?;
                } else {
                    if sign < 0
                        && self.candidates.get(&rank).map(Arc::as_ref) != Some(projected.as_ref())
                    {
                        return Err(StandingQueryFailure::InvalidDelta);
                    }
                    stage(
                        &mut candidates,
                        rank,
                        if sign < 0 { None } else { Some(projected) },
                        meter,
                    )?;
                }
            }
        }
        if distinct {
            let mut remove = Vec::new();
            let mut insert = Vec::new();
            for (identity, patch) in &classes {
                meter.charge(ZSetEvent::Work)?;
                let retained = self.classes.get(identity);
                if let Some((rank, _)) = retained.and_then(|class| class.first_key_value()) {
                    meter.charge(ZSetEvent::ScratchEntry)?;
                    remove.push(rank.clone());
                }
                if let Some((rank, row)) = first_after(retained, patch, meter)? {
                    meter.charge(ZSetEvent::ScratchEntry)?;
                    insert.push((rank.clone(), Arc::clone(row)));
                }
            }
            // A group can move between classes in one tick. Class ordering is
            // not an execution schedule: retire all representatives first.
            for rank in remove {
                stage(&mut candidates, rank, None, meter)?;
            }
            for (rank, row) in insert {
                stage(&mut candidates, rank, Some(row), meter)?;
            }
        }
        let page = if candidates.is_empty() {
            None // Empty-output ticks retain the existing sequence without a scan.
        } else {
            let (offset, count) = definition.incremental_result_window();
            Some(select_page(
                &self.candidates,
                &candidates,
                offset,
                count,
                meter,
            )?)
        };
        let count = page.as_ref().map_or(*row_count, |page| page.len() as u128);
        if meter
            .policy
            .rows
            .max_result_rows()
            .is_some_and(|limit| count > u128::from(limit))
        {
            return Err(StandingQueryFailure::ResultBudget);
        }
        let mut output = Vec::new();
        if let Some(page) = &page {
            for row in &self.page {
                reserve_row(row, meter)?;
                output.push((row.as_ref().clone(), ZWeight::from_i128(-1)));
            }
            for row in page {
                reserve_row(row, meter)?;
                output.push((row.as_ref().clone(), ZWeight::ONE));
            }
        }
        let limbs = LimbLimit::new(4);
        let output = ZSet::from_updates(output, limbs, &mut |event| meter.charge(event))
            .map_err(zset_error)?;
        for (row, _) in output.iter() {
            reserve_row(row, meter)?;
        }
        let sink = rows
            .prepare_update(&output, limbs, &mut |event| meter.charge(event))
            .map_err(zset_error)?;
        for (row, _) in output.iter() {
            meter.charge(ZSetEvent::Work)?;
            if let Some(weight) = sink.weight(row) {
                let weight = weight.to_i128().ok_or(StandingQueryFailure::Arithmetic)?;
                if weight <= 0 || (distinct && weight != 1) {
                    return Err(StandingQueryFailure::InvalidDelta);
                }
            }
        }
        (meter.checkpoint)()?;
        Ok(Update {
            owner: self,
            order,
            projected_order,
            groups,
            classes,
            candidates,
            page,
            count,
            row_count,
            sink,
            delta: output,
        })
    }
}

#[must_use = "dropping a ranked update aborts every arrangement and the page"]
pub(super) struct Update<'a> {
    owner: &'a mut State,
    order: Arc<[GraphAggregateOrder]>,
    projected_order: bool,
    groups: GroupPatch,
    classes: ClassPatch,
    candidates: RankPatch,
    page: Option<Vec<Arc<GraphAggregateRow>>>,
    count: u128,
    row_count: &'a mut u128,
    sink: ZSetUpdate<'a, GraphAggregateRow>,
    delta: ZSet<GraphAggregateRow>,
}

fn publish_ranked(base: &mut Ranked, patch: RankPatch) {
    for (rank, value) in patch {
        base.remove(&rank);
        if let Some(value) = value {
            base.insert(rank, value);
        }
    }
}

impl Update<'_> {
    pub(super) fn commit(self) -> ZSet<GraphAggregateRow> {
        let Self {
            owner,
            order,
            projected_order,
            groups,
            classes,
            candidates,
            page,
            count,
            row_count,
            sink,
            delta,
        } = self;
        for (key, rank) in groups {
            match rank {
                Some(rank) => {
                    owner.groups.insert(key, rank);
                }
                None => {
                    owner.groups.remove(&key);
                }
            }
        }
        for (identity, patch) in classes {
            match owner.classes.entry(identity) {
                Entry::Occupied(mut entry) => {
                    publish_ranked(entry.get_mut(), patch);
                    if entry.get().is_empty() {
                        entry.remove();
                    }
                }
                Entry::Vacant(entry) => {
                    let mut members = BTreeMap::new();
                    publish_ranked(&mut members, patch);
                    if !members.is_empty() {
                        entry.insert(members);
                    }
                }
            }
        }
        publish_ranked(&mut owner.candidates, candidates);
        owner.order = Some(order);
        owner.projected_order = projected_order;
        if let Some(page) = page {
            owner.page = page;
        }
        *row_count = count;
        sink.commit();
        delta
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod projected_rank_tests;
