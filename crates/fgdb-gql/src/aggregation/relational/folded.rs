//! Owned exact aggregation over a folded relational expansion.
//!
//! The existing set executor still owns every source, scope, expression, page,
//! sorting barrier and error position. Its terminal rows feed the SAME owned
//! numeric/DISTINCT/extremum cells as vertex and edge aggregate cursors. There
//! is no expanded input bag and no alternative scalar or aggregate evaluator.

use super::*;
use crate::stream::VertexScanEvent;
use crate::stream::aggregate::{Input, NumericState};
use std::collections::btree_map;

mod repeated;

type Failure<E, C> = GqlQueryError<GraphAggregateError<E>, C>;
type Groups = BTreeMap<Vec<GraphValue>, Vec<NumericState>>;

enum FoldedGroups {
    Rows(Groups),
    Cardinality(Option<u64>),
}

fn input_event(event: VertexScanEvent) -> GlaExecutionEvent {
    match event {
        VertexScanEvent::Work => GlaExecutionEvent::Work,
        VertexScanEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
    }
}

fn new_states<E, C>(
    query: &PreparedGraphAggregate,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), Failure<E, C>>,
) -> Result<Vec<NumericState>, Failure<E, C>> {
    control(GlaExecutionEvent::ScratchEntry)?;
    let mut states = Vec::new();
    for aggregate in query.aggregates() {
        control(GlaExecutionEvent::ScratchEntry)?;
        states.push(NumericState::new_governed(
            aggregate.function(),
            &mut |event| control(input_event(event)),
        )?);
    }
    Ok(states)
}

fn push<E, C>(
    query: &PreparedGraphAggregate,
    groups: &mut Groups,
    largest_key: &mut usize,
    row: GraphValueRow,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), Failure<E, C>>,
) -> Result<(), Failure<E, C>> {
    push_values(
        query,
        groups,
        largest_key,
        |column| &row.values()[column],
        Some(1),
        control,
    )
}

fn push_values<'a, E, C>(
    query: &PreparedGraphAggregate,
    groups: &mut Groups,
    largest_key: &mut usize,
    mut value: impl FnMut(usize) -> &'a GraphValue,
    repetitions: Option<u128>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), Failure<E, C>>,
) -> Result<(), Failure<E, C>> {
    if repetitions == Some(0) {
        return Ok(());
    }
    control(GlaExecutionEvent::Work)?;
    control(GlaExecutionEvent::ScratchEntry)?;
    let mut key = Vec::new();
    let mut units = 0_usize;
    for &column in query.group_key_columns() {
        control(GlaExecutionEvent::Work)?;
        let value = value(column);
        units = units
            .saturating_add(value.payload_units())
            .saturating_add(1);
        key.push(value.copy_with_control(control)?);
    }
    *largest_key = (*largest_key).max(units);
    // Match the existing owned reducers' logical comparison reservation. This
    // accounts for recursive key payloads, not std allocator bytes or exact
    // B-tree comparator invocations. Groups remain governed in-memory state.
    let levels = groups.len().saturating_add(1).ilog2() as usize + 1;
    for _ in 0..levels
        .saturating_mul(24)
        .saturating_mul(largest_key.saturating_add(1))
    {
        control(GlaExecutionEvent::Work)?;
    }
    let states = match groups.entry(key) {
        btree_map::Entry::Occupied(entry) => entry.into_mut(),
        btree_map::Entry::Vacant(entry) => {
            control(GlaExecutionEvent::ScratchEntry)?;
            entry.insert(new_states(query, control)?)
        }
    };
    repeated::update(query, states, &mut value, repetitions, control)
}

impl PreparedGraphAggregate {
    fn uses_factorized_cardinality(&self) -> bool {
        self.group_key_columns().is_empty()
            && !self.aggregates().is_empty()
            && self
                .aggregates()
                .iter()
                .all(|aggregate| aggregate.function() == GraphAggregateFunction::CountRows)
            && self
                .relational_input
                .as_ref()
                .is_some_and(|input| input.has_factorized_cardinality())
    }

    /// Physical admission only. COLLECT retains visitation-ordered lists and
    /// remains on its ordinary implementation; no function is approximated.
    /// A missing fusion opportunity is not retried after runtime failure.
    pub(super) fn folded_definition(&self) -> Option<Self> {
        let input = self.relational_input.as_ref()?;
        // COUNT(*) depends on bag cardinality, not the order or values of
        // Cartesian pairs. Value-sensitive barriers still execute normally.
        if self.uses_factorized_cardinality() {
            return self.prepare_complete_group_output();
        }
        if !(input.has_foldable_expansion() || input.has_repeated_factor(&self.repeated_columns()))
            || !self.aggregates().iter().all(|aggregate| {
                match aggregate.function() {
                    GraphAggregateFunction::Collect | GraphAggregateFunction::CollectDistinct => {
                        false
                    }
                    // The existing owned extremum kernel assumes one checked
                    // GraphValue domain. UNWIND's Any schema may alternate
                    // scalar/vertex/list values; keep that general comparator
                    // on the materialized path rather than reaching its
                    // homogeneous-domain assertion.
                    GraphAggregateFunction::Min | GraphAggregateFunction::Max => {
                        aggregate.argument_column().is_some_and(|column| {
                            input.column_types()[column] != crate::GraphSetColumnType::Any
                        })
                    }
                    function => NumericState::supports(function),
                }
            })
        {
            return None;
        }
        self.prepare_complete_group_output()
    }

    pub(super) fn execute_relational_folded<E, C>(
        &self,
        policy: GqlQueryPolicy,
        source: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphAggregateRow>, Failure<E, C>> {
        let relation = self
            .relational_input
            .as_ref()
            .expect("admitted relational fold");
        if self.uses_factorized_cardinality() {
            let (cardinality, rows, evaluator) = relation
                .count_governed(policy, source, &mut checkpoint)
                .map_err(|error| error.map_source(GraphAggregateError::InputRelation))?;
            return self.finish_relational_folded(
                policy,
                checkpoint,
                rows,
                evaluator,
                FoldedGroups::Cardinality(cardinality),
            );
        }
        let columns = self.repeated_columns();
        if relation.has_repeated_factor(&columns) {
            return self.execute_repeated_groups(&columns, policy, source, checkpoint);
        }
        let mut groups = Groups::new();
        let mut largest_key = 0;
        let mut deferred = None;
        let (rows, evaluator) = relation
            .fold_governed(policy, source, &mut checkpoint, |row, control| {
                if deferred.is_some() {
                    return Ok(());
                }
                let result = push(self, &mut groups, &mut largest_key, row, &mut |event| {
                    control(event)
                        .map_err(|error| error.map_source(GraphAggregateError::InputRelation))
                });
                match result {
                    // Complete upstream row phases before exposing an aggregate
                    // data failure. A later UNWIND/projection/source error has
                    // the same precedence as in the materialized implementation.
                    Err(GqlQueryError::Source(error)) => {
                        deferred = Some(error);
                        Ok(())
                    }
                    Err(error) => {
                        Err(error.map_source(|_| unreachable!("source arm handled above")))
                    }
                    Ok(()) => Ok(()),
                }
            })
            .map_err(|error| error.map_source(GraphAggregateError::InputRelation))?;
        if let Some(error) = deferred {
            return Err(GqlQueryError::Source(error));
        }

        self.finish_relational_folded(
            policy,
            checkpoint,
            rows,
            evaluator,
            FoldedGroups::Rows(groups),
        )
    }

    fn finish_relational_folded<E, C>(
        &self,
        policy: GqlQueryPolicy,
        mut checkpoint: impl FnMut() -> Result<(), C>,
        mut rows: GqlExecutionStats,
        mut evaluator: GlaExecutionStats,
        input: FoldedGroups,
    ) -> Result<GqlQueryExecution<GraphAggregateRow>, Failure<E, C>> {
        // Continue the original allowance, not a fresh quota for the result
        // stage. Private row occurrences did not consume ResultRows above.
        let mut control = |event| {
            checkpoint().map_err(GqlQueryError::Interrupted)?;
            let next = if event == GlaExecutionEvent::ResultRow {
                let next = rows.result_rows.checked_add(1).ok_or_else(|| {
                    GqlQueryError::Source(GraphAggregateError::ResultCountOverflow)
                })?;
                policy
                    .rows
                    .check(GqlBudgetDimension::ResultRows, next)
                    .map_err(GqlQueryError::Rows)?;
                next
            } else {
                rows.result_rows
            };
            evaluator
                .charge_event(policy.evaluator, event)
                .map_err(GqlQueryError::Evaluator)?;
            rows.result_rows = next;
            Ok(())
        };
        let mut groups = match input {
            FoldedGroups::Rows(groups) => groups,
            FoldedGroups::Cardinality(cardinality) => {
                control(GlaExecutionEvent::Work)?;
                // Overflow is a terminal aggregate error, never an early
                // source failure. All factors and local pages completed above,
                // including a zero factor that can annihilate a huge product.
                let count = cardinality.ok_or_else(|| {
                    GqlQueryError::Source(GraphAggregateError::ArithmeticOverflow { aggregate: 0 })
                })?;
                let mut states = new_states(self, &mut control)?;
                for state in &mut states {
                    control(GlaExecutionEvent::Work)?;
                    let NumericState::Count(value) = state else {
                        unreachable!("factorized cardinality admits only COUNT(*)")
                    };
                    *value = count;
                }
                control(GlaExecutionEvent::ScratchEntry)?;
                let mut groups = Groups::new();
                groups.insert(Vec::new(), states);
                groups
            }
        };
        if groups.is_empty() && self.group_key_columns().is_empty() {
            control(GlaExecutionEvent::ScratchEntry)?;
            groups.insert(Vec::new(), new_states(self, &mut control)?);
        }
        let mut ranking = self.streamed_group_ranking(groups.len());
        for (keys, states) in groups {
            let mut values = Vec::new();
            for state in states {
                control(GlaExecutionEvent::ScratchEntry)?;
                values.push(state.finish_governed(&mut |event| control(input_event(event)))?);
            }
            ranking.push(
                self,
                GraphAggregateRow::from_group_values(keys, values),
                &mut control,
            )?;
        }
        let value = ranking.finish(self, &mut control)?;
        for _ in &value {
            control(GlaExecutionEvent::ResultRow)?;
        }
        control(GlaExecutionEvent::Work)?;
        Ok(GqlQueryExecution {
            value,
            rows,
            evaluator,
        })
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod cardinality_tests;
