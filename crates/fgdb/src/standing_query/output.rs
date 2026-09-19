//! Projection and deletion-safe DISTINCT downstream of complete HAVING groups.
//!
//! ALL publishes a bag: equal projected rows add their multiplicities. DISTINCT
//! retains each qualifying group's exact projected value under a semantic output
//! key. Unranked output chooses the least complete group key; ranked output uses
//! the original ORDER BY and then the same canonical key tiebreak. Both retain
//! support outside the visible result. This is session-local derived state,
//! not a delivery cursor or spill store.

mod ranked;

use super::{Meter, StandingQueryFailure, zset_error};
use fgdb_delta_types::zset::ZSetUpdate;
use fgdb_delta_types::{LimbLimit, ZSet, ZSetEvent, ZWeight};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GlaExecutionEvent, GqlQueryError, GraphAggregateError, GraphAggregateRow,
    PreparedGraphAggregate,
};
use std::collections::{BTreeMap, btree_map::Entry};
use std::sync::Arc;

type GroupKey = Arc<[GraphValue]>;
type Members = BTreeMap<GroupKey, Arc<GraphAggregateRow>>;
type Classes = BTreeMap<GraphAggregateRow, Members>;
type Changes = BTreeMap<GraphAggregateRow, BTreeMap<GroupKey, Option<Arc<GraphAggregateRow>>>>;

pub(crate) struct State {
    definition: PreparedGraphAggregate,
    classes: Classes,
    ranked: Option<ranked::State>,
    pub(super) rows: ZSet<GraphAggregateRow>,
    row_count: u128,
}

impl core::fmt::Debug for State {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StandingOutput")
            .field("rows", &self.row_count)
            .field("distinct_classes", &self.classes.len())
            .field("ranked", &self.ranked.is_some())
            .field("data", &"[REDACTED]")
            .finish()
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

fn copy_group(
    row: &GraphAggregateRow,
    meter: &mut Meter<'_>,
) -> Result<GroupKey, StandingQueryFailure> {
    meter.charge(ZSetEvent::ScratchEntry)?;
    let mut keys = Vec::new();
    for key in row.keys() {
        meter.charge(ZSetEvent::Work)?;
        keys.push(key.copy_with_control(&mut |event| govern(meter, event))?);
    }
    meter.charge(ZSetEvent::ScratchEntry)?;
    Ok(Arc::from(keys.into_boxed_slice()))
}

fn reserve_row(row: &GraphAggregateRow, meter: &mut Meter<'_>) -> Result<(), StandingQueryFailure> {
    meter.charge(ZSetEvent::ScratchEntry)?;
    for key in row.keys() {
        meter.charge(ZSetEvent::Work)?;
        meter.units(ZSetEvent::ScratchEntry, 1 + key.payload_units())?;
    }
    for value in row.values() {
        meter.charge(ZSetEvent::Work)?;
        meter.charge(ZSetEvent::ScratchEntry)?;
        if let Some(value) = value.as_value() {
            meter.units(ZSetEvent::ScratchEntry, value.payload_units())?;
        }
    }
    Ok(())
}

impl State {
    pub(super) fn new(definition: PreparedGraphAggregate) -> Self {
        let ranked = definition
            .has_incremental_ranking()
            .then(ranked::State::default);
        Self {
            definition,
            classes: BTreeMap::new(),
            ranked,
            rows: ZSet::new(),
            row_count: 0,
        }
    }

    pub(super) fn definition(&self) -> &PreparedGraphAggregate {
        &self.definition
    }

    pub(super) fn ordered_rows(&self) -> Option<&[Arc<GraphAggregateRow>]> {
        self.ranked.as_ref().map(|state| state.page.as_slice())
    }

    /// Consume the tentative complete-group Z-set, after HAVING and before any
    /// upstream publication. Both signs are processed as one change: raw row
    /// sort order must not decide whether an updated group is inserted/deleted.
    pub(super) fn prepare(
        &mut self,
        delta: &ZSet<GraphAggregateRow>,
        meter: &mut Meter<'_>,
    ) -> Result<Update<'_>, StandingQueryFailure> {
        if let Some(ranked) = &mut self.ranked {
            return ranked
                .prepare(
                    &self.definition,
                    delta,
                    &mut self.rows,
                    &mut self.row_count,
                    meter,
                )
                .map(|update| Update {
                    transition: Transition::Ranked(update),
                });
        }
        meter.charge(ZSetEvent::Work)?;
        let distinct = self.definition.incremental_output_is_distinct();
        let mut changes: Changes = BTreeMap::new();
        let mut output = Vec::new();
        let mut row_count = self.row_count;
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
                let projected = self
                    .definition
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
                if !distinct {
                    row_count = if sign < 0 {
                        row_count.checked_sub(1)
                    } else {
                        row_count.checked_add(1)
                    }
                    .ok_or(StandingQueryFailure::InvalidDelta)?;
                    meter.charge(ZSetEvent::ScratchEntry)?;
                    output.push((projected, ZWeight::from_i128(sign)));
                    continue;
                }
                let identity =
                    projected.incremental_distinct_key(&mut |event| govern(meter, event))?;
                let key = copy_group(row, meter)?;
                let retained = self
                    .classes
                    .get(&identity)
                    .and_then(|class| class.get(&key));
                let patch = match changes.entry(identity) {
                    Entry::Occupied(entry) => entry.into_mut(),
                    Entry::Vacant(entry) => {
                        meter.units(ZSetEvent::ScratchEntry, 2)?;
                        entry.insert(BTreeMap::new())
                    }
                };
                if sign < 0 {
                    if retained.map(Arc::as_ref) != Some(&projected) || patch.contains_key(&key) {
                        return Err(StandingQueryFailure::InvalidDelta);
                    }
                    meter.charge(ZSetEvent::ScratchEntry)?;
                    patch.insert(key, None);
                } else {
                    if patch.get(&key).is_some_and(Option::is_some)
                        || (retained.is_some() && !matches!(patch.get(&key), Some(None)))
                    {
                        return Err(StandingQueryFailure::InvalidDelta);
                    }
                    meter.units(ZSetEvent::ScratchEntry, 3)?;
                    patch.insert(key, Some(Arc::new(projected)));
                }
            }
        }
        if distinct {
            for (identity, patch) in &changes {
                meter.charge(ZSetEvent::Work)?;
                let retained = self.classes.get(identity);
                let old = retained
                    .and_then(|class| class.first_key_value())
                    .map(|(_, row)| row);
                let mut next = None;
                // Only a removed/updated prefix can be skipped. This never
                // scans an unchanged equivalence class to rebuild its members.
                if let Some(retained) = retained {
                    for (key, row) in retained {
                        meter.charge(ZSetEvent::Work)?;
                        if !patch.contains_key(key) {
                            next = Some((key, row));
                            break;
                        }
                    }
                }
                for (key, row) in patch {
                    meter.charge(ZSetEvent::Work)?;
                    if let Some(row) = row {
                        if next.is_none_or(|(current, _)| key < current) {
                            next = Some((key, row));
                        }
                        break;
                    }
                }
                let new = next.map(|(_, row)| row);
                row_count = row_count
                    .checked_sub(u128::from(old.is_some()))
                    .and_then(|count| count.checked_add(u128::from(new.is_some())))
                    .ok_or(StandingQueryFailure::InvalidDelta)?;
                if old == new {
                    continue;
                }
                if let Some(row) = old {
                    reserve_row(row, meter)?;
                    output.push((row.as_ref().clone(), ZWeight::from_i128(-1)));
                }
                if let Some(row) = new {
                    reserve_row(row, meter)?;
                    output.push((row.as_ref().clone(), ZWeight::ONE));
                }
            }
        }
        // ALL counts occurrences; DISTINCT counts final classes. Neither is
        // bounded by transient tick peaks or the number of hidden support rows.
        if meter
            .policy
            .rows
            .max_result_rows()
            .is_some_and(|limit| row_count > u128::from(limit))
        {
            return Err(StandingQueryFailure::ResultBudget);
        }
        let limbs = LimbLimit::new(4);
        let output = ZSet::from_updates(output, limbs, &mut |event| meter.charge(event))
            .map_err(zset_error)?;
        for (row, _) in output.iter() {
            reserve_row(row, meter)?;
        }
        let sink = self
            .rows
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
            transition: Transition::Plain(PlainUpdate {
                classes: &mut self.classes,
                count: &mut self.row_count,
                changes,
                row_count,
                sink,
            }),
        })
    }
}

#[must_use = "dropping an output update aborts it"]
pub(super) struct Update<'a> {
    transition: Transition<'a>,
}

enum Transition<'a> {
    Plain(PlainUpdate<'a>),
    Ranked(ranked::Update<'a>),
}

impl Update<'_> {
    pub(super) fn commit(self) {
        match self.transition {
            Transition::Plain(update) => update.commit(),
            Transition::Ranked(update) => update.commit(),
        }
    }
}

struct PlainUpdate<'a> {
    classes: &'a mut Classes,
    count: &'a mut u128,
    changes: Changes,
    row_count: u128,
    sink: ZSetUpdate<'a, GraphAggregateRow>,
}

fn publish_members(
    members: &mut Members,
    changes: BTreeMap<GroupKey, Option<Arc<GraphAggregateRow>>>,
) {
    for (key, value) in changes {
        match value {
            Some(row) => {
                members.insert(key, row);
            }
            None => {
                members.remove(&key);
            }
        }
    }
}

impl PlainUpdate<'_> {
    fn commit(self) {
        let Self {
            classes,
            count,
            changes,
            row_count,
            sink,
        } = self;
        for (identity, changes) in changes {
            match classes.entry(identity) {
                Entry::Occupied(mut entry) => {
                    publish_members(entry.get_mut(), changes);
                    if entry.get().is_empty() {
                        entry.remove();
                    }
                }
                Entry::Vacant(entry) => {
                    let mut members = BTreeMap::new();
                    publish_members(&mut members, changes);
                    if !members.is_empty() {
                        entry.insert(members);
                    }
                }
            }
        }
        *count = row_count;
        sink.commit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::standing_query::StandingQueryStats;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, IntegerComparison};
    use fgdb_gql::{
        GqlQueryPolicy, GraphAggregate, GraphAggregateValue, GraphIntegerExpression,
        GraphIntegerOp as Op, GraphSetProjection, GraphSetValue,
    };
    use fgdb_types::CanonicalScalar;

    fn definition(distinct: bool) -> PreparedGraphAggregate {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        let input = builder
            .prepare_values(
                &[GraphColumn::property("key", "n", PropertyKeyId(1))],
                0,
                None,
            )
            .unwrap()
            .with_duplicates();
        let expression = GraphIntegerExpression::prepare_scalar(&[
            Op::Column(0),
            Op::Literal(Some(1)),
            Op::Compare(IntegerComparison::Equal),
            Op::Column(1),
            Op::Literal(Some(2)),
            Op::Case,
        ])
        .unwrap();
        PreparedGraphAggregate::prepare(
            input,
            &[0],
            &[GraphAggregate::count_rows("count")],
            0,
            None,
        )
        .unwrap()
        .with_output_projection(vec![GraphSetProjection::new(
            "value",
            GraphSetValue::Integer(expression),
        )])
        .unwrap()
        .with_distinct_output(distinct)
    }

    fn delta(rows: &[(i64, u64, i128)]) -> ZSet<GraphAggregateRow> {
        let definition = definition(false).incremental_source_definition().unwrap();
        ZSet::from_updates(
            rows.iter().map(|&(group, count, sign)| {
                (
                    definition
                        .materialize_incremental_row(
                            vec![GraphValue::Scalar(CanonicalScalar::Int(group))],
                            vec![GraphAggregateValue::Count(count)],
                        )
                        .unwrap(),
                    ZWeight::from_i128(sign),
                )
            }),
            LimbLimit::new(4),
            &mut |_| Ok::<_, ()>(()),
        )
        .unwrap()
    }

    fn apply(
        state: &mut State,
        rows: &[(i64, u64, i128)],
        limit: u64,
    ) -> Result<(), StandingQueryFailure> {
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: GqlQueryPolicy::new(100_000, limit, 10_000_000, 10_000_000),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        state.prepare(&delta(rows), &mut meter)?.commit();
        Ok(())
    }

    fn seeded() -> State {
        let mut state = State::new(definition(true));
        apply(&mut state, &[(1, 2, 1), (2, 1, 1), (3, 1, 1)], 1).unwrap();
        state
    }

    fn same(a: &State, b: &State) {
        assert_eq!(a.rows, b.rows);
        assert_eq!(a.classes, b.classes);
        assert_eq!(a.row_count, b.row_count);
    }

    #[test]
    fn representative_replacement_preserves_exact_variant_and_retires_empty_classes() {
        let mut state = seeded();
        assert_eq!(state.rows.len(), 1);
        assert_eq!(
            state.rows.iter().next().unwrap().0.values(),
            &[GraphAggregateValue::Count(2)]
        );
        assert_eq!(state.classes.values().next().unwrap().len(), 3);
        apply(&mut state, &[(1, 2, -1)], 1).unwrap();
        assert_eq!(
            state.rows.iter().next().unwrap().0.values(),
            &[GraphAggregateValue::Value(GraphValue::Scalar(
                CanonicalScalar::Int(2)
            ))]
        );
        apply(&mut state, &[(2, 1, -1), (3, 1, -1)], 0).unwrap();
        assert!(state.rows.is_empty() && state.classes.is_empty());
        assert_eq!(state.row_count, 0);
    }

    #[test]
    fn all_budget_counts_occurrences_and_distinct_updates_are_whole_tick() {
        let mut all = State::new(definition(false));
        assert_eq!(
            apply(&mut all, &[(2, 1, 1), (3, 1, 1)], 1),
            Err(StandingQueryFailure::ResultBudget)
        );
        assert!(all.rows.is_empty());
        apply(&mut all, &[(2, 1, 1), (3, 1, 1)], 2).unwrap();
        assert_eq!(all.rows.len(), 1);
        assert_eq!(all.rows.iter().next().unwrap().1.to_i128(), Some(2));
        apply(&mut all, &[(2, 1, -1), (4, 1, 1)], 2).unwrap();
        assert_eq!(all.row_count, 2);
        let mut distinct = seeded();
        // The new raw row sorts before the old one: deletion still happens first.
        apply(&mut distinct, &[(1, 2, -1), (1, 1, 1)], 2).unwrap();
        assert_eq!(distinct.row_count, 2);
        apply(&mut distinct, &[(1, 1, -1), (1, 2, 1)], 1).unwrap();
        assert_eq!(distinct.row_count, 1);
    }

    #[test]
    fn every_output_checkpoint_and_dropped_guard_preserves_support_and_retries() {
        let change = delta(&[(1, 2, -1), (2, 1, -1), (4, 1, 1)]);
        let before = seeded();
        let mut success = seeded();
        let mut calls = 0;
        let policy = GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000);
        {
            let mut checkpoint = || {
                calls += 1;
                Ok(())
            };
            let mut meter = Meter {
                policy,
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            success.prepare(&change, &mut meter).unwrap().commit();
        }
        for stop in 1..=calls {
            let mut candidate = seeded();
            let mut seen = 0;
            {
                let mut checkpoint = || {
                    seen += 1;
                    if seen == stop {
                        Err(StandingQueryFailure::Interrupted)
                    } else {
                        Ok(())
                    }
                };
                let mut meter = Meter {
                    policy,
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                assert!(matches!(
                    candidate.prepare(&change, &mut meter),
                    Err(StandingQueryFailure::Interrupted)
                ));
            }
            assert_eq!(seen, stop);
            same(&candidate, &before);
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy,
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            drop(candidate.prepare(&change, &mut meter).unwrap());
            same(&candidate, &before);
            candidate.prepare(&change, &mut meter).unwrap().commit();
            same(&candidate, &success);
        }
    }

    #[test]
    fn whole_maintenance_cancellation_and_budget_refusal_preserve_source_and_output() {
        use crate::{Database, DatabaseKeys, WriteBatch};
        use asupersync::lab::run_async_under_lab;
        use fgdb_delta_types::RelationId;
        use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts, VId};
        let ((), report) = run_async_under_lab(0x6a80, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let query = contexts.query();
            let keys = || DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32]);
            let seed = || {
                let mut rows = WriteBatch::new(RelationId(1));
                for id in 1..=3 {
                    rows.create_vertex(
                        VId(id),
                        vec![],
                        vec![(PropertyKeyId(1), CanonicalScalar::Int(id as i64))],
                    );
                }
                rows
            };
            let mut basis = Database::open_memory(&commit, keys()).await.unwrap();
            basis.write(&commit, seed()).await.unwrap();
            let mut driver = Database::open_memory(&commit, keys()).await.unwrap();
            driver.write(&commit, seed()).await.unwrap();
            let mut changes = WriteBatch::new(RelationId(1));
            changes.create_vertex(
                VId(4),
                vec![],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(1))],
            );
            changes.delete_vertex(VId(2));
            let at = driver.write(&commit, changes).await.unwrap();
            let delta = driver.delta_index().unwrap().get(at).unwrap().clone();
            let policy = GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000);
            let make = || {
                let definition = definition(true);
                let raw = definition.incremental_source_definition().unwrap();
                let mut output = State::new(definition);
                let source = basis
                    .prepare_standing_query_with_output(&query, raw, policy, Some(&mut output))
                    .unwrap();
                (source, output)
            };
            let (original, before) = make();
            let (mut successful, mut after) = make();
            let mut calls = 0;
            let stats = {
                let mut checkpoint = || {
                    calls += 1;
                    Ok(())
                };
                let mut meter = Meter {
                    policy,
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                successful
                    .maintain_with_output(&delta, &mut meter, Some(&mut after))
                    .unwrap();
                meter.stats
            };
            assert_eq!(after.row_count, 1);
            assert_eq!(
                after.rows.iter().next().unwrap().0.values(),
                &[GraphAggregateValue::Count(2)]
            );
            for stop in 1..=calls {
                let (mut candidate, mut output) = make();
                let mut seen = 0;
                {
                    let mut checkpoint = || {
                        seen += 1;
                        if seen == stop {
                            Err(StandingQueryFailure::Interrupted)
                        } else {
                            Ok(())
                        }
                    };
                    let mut meter = Meter {
                        policy,
                        stats: StandingQueryStats::default(),
                        checkpoint: &mut checkpoint,
                    };
                    assert_eq!(
                        candidate.maintain_with_output(&delta, &mut meter, Some(&mut output)),
                        Err(StandingQueryFailure::Interrupted)
                    );
                }
                assert_eq!(seen, stop);
                assert_eq!(candidate.rows, original.rows);
                assert_eq!(candidate.frontier, original.frontier);
                same(&output, &before);
                let mut checkpoint = || Ok(());
                let mut meter = Meter {
                    policy,
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                candidate
                    .maintain_with_output(&delta, &mut meter, Some(&mut output))
                    .unwrap();
                assert_eq!(candidate.rows, successful.rows);
                same(&output, &after);
            }
            for reason in [
                StandingQueryFailure::WorkBudget,
                StandingQueryFailure::ScratchBudget,
                StandingQueryFailure::ResultBudget,
            ] {
                let (mut source, mut output) = make();
                let mut bounded = policy;
                match reason {
                    StandingQueryFailure::WorkBudget => {
                        bounded.evaluator.max_work_units = stats.work_units - 1
                    }
                    StandingQueryFailure::ScratchBudget => {
                        bounded.evaluator.max_scratch_entries = stats.scratch_entries - 1
                    }
                    _ => bounded = GqlQueryPolicy::new(100_000, 0, 10_000_000, 10_000_000),
                }
                let mut checkpoint = || Ok(());
                let mut meter = Meter {
                    policy: bounded,
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                assert_eq!(
                    source.maintain_with_output(&delta, &mut meter, Some(&mut output)),
                    Err(reason)
                );
                assert_eq!(source.rows, original.rows);
                same(&output, &before);
                let mut meter = Meter {
                    policy,
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                source
                    .maintain_with_output(&delta, &mut meter, Some(&mut output))
                    .unwrap();
                assert_eq!(source.rows, successful.rows);
                same(&output, &after);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
