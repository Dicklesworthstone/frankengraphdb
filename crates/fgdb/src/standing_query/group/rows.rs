//! Checked ordinary-row output for a maintained relational group stage.
//!
//! This is a sink for the existing exact group's derivative, not a reducer.
//! Only changed selected groups are converted. The raw group, output and this
//! sink prepare together; conversion failure cannot publish any of them.

use super::*;
use fgdb_delta_types::ZWeight;
use fgdb_delta_types::zset::ZSetUpdate;
use fgdb_gql::spill_set::SpillSetAggregateRowError;
use fgdb_gql::{GlaExecutionEvent, GraphAggregateError, GraphSetColumnType};

pub(super) struct State {
    columns: Vec<String>,
    types: Vec<GraphSetColumnType>,
    keys: usize,
    values: usize,
    rows: ZSet<GraphValueRow>,
    last_delta: Option<ZSet<GraphValueRow>>,
}

fn govern(meter: &mut Meter<'_>, event: GlaExecutionEvent) -> Result<(), StandingQueryFailure> {
    match event {
        GlaExecutionEvent::Work => meter.charge(ZSetEvent::Work),
        GlaExecutionEvent::ScratchEntry => meter.charge(ZSetEvent::ScratchEntry),
        GlaExecutionEvent::ResultRow => Err(StandingQueryFailure::InvalidDelta),
    }
}

fn conversion_error(
    error: SpillSetAggregateRowError<StandingQueryFailure>,
) -> StandingQueryFailure {
    match error {
        SpillSetAggregateRowError::Control(error) => error,
        SpillSetAggregateRowError::Value(GraphAggregateError::OutputExpression {
            column,
            error,
        }) => StandingQueryFailure::OutputExpression { column, error },
        SpillSetAggregateRowError::Value(_) => StandingQueryFailure::InvalidDelta,
    }
}

fn reserve_row(row: &GraphValueRow, meter: &mut Meter<'_>) -> Result<(), StandingQueryFailure> {
    meter.charge(ZSetEvent::ScratchEntry)?;
    for value in row.values() {
        meter.charge(ZSetEvent::Work)?;
        let units = value
            .payload_units()
            .checked_add(1)
            .ok_or(StandingQueryFailure::ScratchBudget)?;
        meter.units(ZSetEvent::ScratchEntry, units)?;
    }
    Ok(())
}

impl State {
    pub(super) fn new(
        columns: &[String],
        types: &[GraphSetColumnType],
        keys: usize,
        values: usize,
        meter: &mut Meter<'_>,
    ) -> Result<Self, StandingQueryFailure> {
        if keys.checked_add(values) != Some(columns.len()) || types.len() != columns.len() {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        meter.charge(ZSetEvent::ScratchEntry)?;
        let mut names = Vec::new();
        for name in columns {
            meter.charge(ZSetEvent::Work)?;
            meter.units(ZSetEvent::ScratchEntry, 1 + name.len().div_ceil(64))?;
            names.push(name.clone());
        }
        meter.units(ZSetEvent::ScratchEntry, 1 + types.len())?;
        Ok(Self {
            columns: names,
            types: types.to_vec(),
            keys,
            values,
            rows: ZSet::new(),
            last_delta: None,
        })
    }

    pub(super) fn schema(&self) -> (&[String], &[GraphSetColumnType]) {
        (&self.columns, &self.types)
    }
    pub(super) fn rows(&self) -> &ZSet<GraphValueRow> {
        &self.rows
    }
    pub(super) fn delta(&self) -> Option<&ZSet<GraphValueRow>> {
        self.last_delta.as_ref()
    }
    pub(super) fn clear_baseline_delta(&mut self) {
        self.last_delta = None;
    }

    pub(super) fn prepare(
        &mut self,
        delta: &ZSet<GraphAggregateRow>,
        meter: &mut Meter<'_>,
    ) -> Result<Update<'_>, StandingQueryFailure> {
        let mut converted = ZSet::new();
        for (row, weight) in delta.iter() {
            meter.charge(ZSetEvent::Work)?;
            if row.keys().len() != self.keys || row.values().len() != self.values {
                return Err(StandingQueryFailure::InvalidDelta);
            }
            // Use precisely the snapshot/spill conversion. Int64 overflow and
            // an exact rational are errors, not lossy scalar approximations.
            let value = row
                .to_value_row_with_control(&mut |event| govern(meter, event))
                .map_err(conversion_error)?;
            let weight = weight
                .checked_clone(LIMBS)
                .map_err(|_| StandingQueryFailure::Arithmetic)?;
            // Conversion can identify formerly distinct representations. Keep
            // their signed support, never DISTINCT a derivative or narrow it.
            converted
                .accumulate(value, weight, LIMBS, &mut |event| meter.charge(event))
                .map_err(zset_error)?;
        }
        // The complete conversion barrier executes even on an unchanged tick.
        govern(meter, GlaExecutionEvent::Work)?;
        for (row, _) in converted.iter() {
            reserve_row(row, meter)?;
        }
        let sink = self
            .rows
            .prepare_update(&converted, LIMBS, &mut |event| meter.charge(event))
            .map_err(zset_error)?;
        for (row, _) in converted.iter() {
            meter.charge(ZSetEvent::Work)?;
            if sink
                .weight(row)
                .is_some_and(|weight| weight < &ZWeight::ZERO)
            {
                return Err(StandingQueryFailure::InvalidDelta);
            }
        }
        (meter.checkpoint)()?;
        Ok(Update {
            sink,
            last_delta: &mut self.last_delta,
            delta: converted,
        })
    }
}

#[must_use = "dropping row conversion leaves the prior row bag and derivative unchanged"]
pub(super) struct Update<'a> {
    sink: ZSetUpdate<'a, GraphValueRow>,
    last_delta: &'a mut Option<ZSet<GraphValueRow>>,
    delta: ZSet<GraphValueRow>,
}
impl Update<'_> {
    pub(super) fn commit(self) {
        self.sink.commit();
        *self.last_delta = Some(self.delta);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, GraphValue};
    use fgdb_gql::{GraphAggregate, GraphAggregateValue, GraphIntegerErrorKind, PreparedGraphSet};
    use fgdb_types::CanonicalScalar;

    type Group = super::super::State;

    fn scalar(value: i64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Int(value))
    }
    fn policy() -> GqlQueryPolicy {
        GqlQueryPolicy::new(100, 100, 10_000_000, 10_000_000)
    }
    fn input(updates: &[(i64, i64, i128)]) -> ZSet<GraphValueRow> {
        ZSet::from_updates(
            updates.iter().map(|&(key, value, sign)| {
                (
                    GraphValueRow::from_owned_values(vec![scalar(key), scalar(value)]),
                    ZWeight::from_i128(sign),
                )
            }),
            LIMBS,
            &mut |_| Ok::<(), ()>(()),
        )
        .unwrap()
    }
    fn apply(
        state: &mut Group,
        delta: &ZSet<GraphValueRow>,
        policy: GqlQueryPolicy,
        checkpoint: &mut dyn FnMut() -> Result<(), StandingQueryFailure>,
    ) -> Result<StandingQueryStats, StandingQueryFailure> {
        let mut meter = Meter {
            policy,
            stats: StandingQueryStats::default(),
            checkpoint,
        };
        state.apply(delta, &mut meter)?;
        Ok(meter.stats)
    }
    fn empty() -> Group {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        let input = builder
            .prepare_values(
                &[
                    GraphColumn::property("key", "n", PropertyKeyId(1)),
                    GraphColumn::property("value", "n", PropertyKeyId(2)),
                ],
                0,
                None,
            )
            .unwrap()
            .with_duplicates();
        let definition = PreparedGraphSetAggregate::prepare(
            PreparedGraphSet::from(input),
            &[0],
            &[GraphAggregate::sum_int("total", 1)],
            0,
            None,
        )
        .unwrap();
        let operator = IncrementalGroupAggregate::new(
            definition.complete_groups().unwrap(),
            definition.input().column_types(),
        )
        .unwrap();
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: policy(),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        let row_output = State::new(
            &["key".into(), "total".into()],
            &[GraphSetColumnType::Scalar; 2],
            1,
            1,
            &mut meter,
        )
        .unwrap();
        Group {
            input: 0,
            operator,
            output: output::State::new(definition),
            row_output: Some(row_output),
            last_delta: None,
            policy: policy(),
            frontier: CommitSeq(1),
            stats: StandingQueryStats::default(),
            failure: None,
        }
    }
    fn seeded() -> Group {
        let mut state = empty();
        apply(
            &mut state,
            &input(&[(1, i64::MAX, 1), (2, 7, 1)]),
            policy(),
            &mut || Ok(()),
        )
        .unwrap();
        state
    }
    fn same(a: &Group, b: &Group) {
        assert_eq!(a.operator, b.operator);
        assert_eq!(a.rows(), b.rows());
        assert_eq!(a.value_rows(), b.value_rows());
        assert_eq!(a.delta(), b.delta());
        assert_eq!(a.value_delta(), b.value_delta());
        assert_eq!(a.frontier, b.frontier);
        assert_eq!(a.failure, b.failure);
    }

    #[test]
    fn conversion_failure_aborts_exact_group_support_and_both_outputs_then_retries() {
        let before = seeded();
        let mut candidate = seeded();
        assert!(matches!(
            apply(&mut candidate, &input(&[(1, 1, 1)]), policy(), &mut || Ok(())),
            Err(StandingQueryFailure::OutputExpression { column: 1, error })
                if error.kind == GraphIntegerErrorKind::Overflow
        ));
        same(&candidate, &before);
        let change = input(&[(1, i64::MAX, -1), (1, 9, 1), (3, 4, 1)]);
        apply(&mut candidate, &change, policy(), &mut || Ok(())).unwrap();
        assert_eq!(
            candidate.value_rows().unwrap(),
            &input(&[(1, 9, 1), (2, 7, 1), (3, 4, 1)])
        );
        // A leaked +1 in the exact reducer would yield 10 rather than 9.
        assert_eq!(
            candidate.rows().iter().next().unwrap().0.values(),
            &[GraphAggregateValue::Integer(9)]
        );
    }

    #[test]
    fn every_conversion_tick_checkpoint_and_one_short_budget_preserve_retry_state() {
        let change = input(&[(1, i64::MAX, -1), (1, 9, 1), (3, 4, 1)]);
        let before = seeded();
        let mut expected = seeded();
        let mut calls = 0;
        let stats = apply(&mut expected, &change, policy(), &mut || {
            calls += 1;
            Ok(())
        })
        .unwrap();
        assert!(calls > 0 && stats.work_units > 0 && stats.scratch_entries > 0);
        for stop in 1..=calls {
            let mut candidate = seeded();
            let mut seen = 0;
            assert_eq!(
                apply(&mut candidate, &change, policy(), &mut || {
                    seen += 1;
                    if seen == stop {
                        Err(StandingQueryFailure::Interrupted)
                    } else {
                        Ok(())
                    }
                }),
                Err(StandingQueryFailure::Interrupted)
            );
            assert_eq!(seen, stop);
            same(&candidate, &before);
            apply(&mut candidate, &change, policy(), &mut || Ok(())).unwrap();
            same(&candidate, &expected);
        }
        for failure in [
            StandingQueryFailure::WorkBudget,
            StandingQueryFailure::ScratchBudget,
            StandingQueryFailure::ResultBudget,
        ] {
            let mut candidate = seeded();
            let mut bounded = policy();
            match failure {
                StandingQueryFailure::WorkBudget => {
                    bounded.evaluator.max_work_units = stats.work_units - 1;
                }
                StandingQueryFailure::ScratchBudget => {
                    bounded.evaluator.max_scratch_entries = stats.scratch_entries - 1;
                }
                _ => bounded = GqlQueryPolicy::new(100, 2, 10_000_000, 10_000_000),
            }
            assert_eq!(
                apply(&mut candidate, &change, bounded, &mut || Ok(())),
                Err(failure)
            );
            same(&candidate, &before);
            apply(&mut candidate, &change, policy(), &mut || Ok(())).unwrap();
            same(&candidate, &expected);
        }
    }

    #[test]
    fn dropped_row_guard_and_empty_tick_preserve_the_conversion_barrier() {
        let before = seeded();
        let mut candidate = seeded();
        let raw = ZSet::from_updates(
            [(
                GraphAggregateRow::from_group_values(
                    vec![scalar(3)],
                    vec![GraphAggregateValue::Integer(4)],
                ),
                ZWeight::ONE,
            )],
            LIMBS,
            &mut |_| Ok::<(), ()>(()),
        )
        .unwrap();
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: policy(),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        drop(
            candidate
                .row_output
                .as_mut()
                .unwrap()
                .prepare(&raw, &mut meter)
                .unwrap(),
        );
        same(&candidate, &before);
        apply(&mut candidate, &ZSet::new(), policy(), &mut || Ok(())).unwrap();
        assert_eq!(candidate.rows(), before.rows());
        assert_eq!(candidate.value_rows(), before.value_rows());
        assert!(candidate.delta().unwrap().is_empty());
        assert!(candidate.value_delta().unwrap().is_empty());
    }

    #[test]
    fn converted_collisions_keep_signed_multiplicity_and_reject_negative_support() {
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: policy(),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        let mut state = State::new(
            &["count".into()],
            &[GraphSetColumnType::Scalar],
            0,
            1,
            &mut meter,
        )
        .unwrap();
        let row = |value| GraphAggregateRow::from_group_values(vec![], vec![value]);
        let positive = ZSet::from_updates(
            [
                (row(GraphAggregateValue::Count(2)), ZWeight::from_i128(2)),
                (row(GraphAggregateValue::Integer(2)), ZWeight::ONE),
            ],
            LIMBS,
            &mut |_| Ok::<(), ()>(()),
        )
        .unwrap();
        state.prepare(&positive, &mut meter).unwrap().commit();
        let key = GraphValueRow::from_owned_values(vec![scalar(2)]);
        assert_eq!(state.rows().weight(&key), Some(&ZWeight::from_i128(3)));
        let retract = ZSet::from_updates(
            [(row(GraphAggregateValue::Integer(2)), ZWeight::from_i128(-2))],
            LIMBS,
            &mut |_| Ok::<(), ()>(()),
        )
        .unwrap();
        state.prepare(&retract, &mut meter).unwrap().commit();
        assert_eq!(state.rows().weight(&key), Some(&ZWeight::ONE));
        assert!(matches!(
            state.prepare(&retract, &mut meter),
            Err(StandingQueryFailure::InvalidDelta)
        ));
        assert_eq!(state.rows().weight(&key), Some(&ZWeight::ONE));
        assert_eq!(
            state.delta().unwrap().weight(&key),
            Some(&ZWeight::from_i128(-2))
        );
    }
}
