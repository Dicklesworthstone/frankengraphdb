//! Exact contiguous-repetition laws over the existing owned aggregate cells.
//!
//! The first occurrence always uses the shared updater (types, DISTINCT,
//! extrema and payload ownership). A homogeneous numeric tail advances by a
//! checked closed form. Find the first failing occurrence across ALL cells
//! before advancing: processing a whole column at a time would change which
//! aggregate reports overflow. No signed product is formed before cancellation
//! against the existing sum; i128::MIN and the full unsigned distance are legal.

use super::*;

fn unit<'a, E, C>(
    query: &PreparedGraphAggregate,
    states: &mut [NumericState],
    value: &mut impl FnMut(usize) -> &'a GraphValue,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), Failure<E, C>>,
) -> Result<(), Failure<E, C>> {
    for (at, (aggregate, state)) in query.aggregates().iter().zip(states).enumerate() {
        control(GlaExecutionEvent::Work)?;
        let input = aggregate
            .argument_column()
            .map_or(Input::Identity, |column| Input::from_value(value(column)));
        state.update_governed(input, at, &mut |event| control(input_event(event)))?;
    }
    Ok(())
}

fn sum_capacity(total: i128, step: i64) -> Option<u128> {
    // The sign-bit bias is an order-preserving map from i128 into u128.
    let position = (total as u128) ^ (1_u128 << 127);
    match step.cmp(&0) {
        core::cmp::Ordering::Greater => Some((u128::MAX - position) / step as u128),
        core::cmp::Ordering::Less => Some(position / u128::from(step.unsigned_abs())),
        core::cmp::Ordering::Equal => None,
    }
}

fn capacity(state: &NumericState, input: Input<'_>) -> Option<u128> {
    if matches!(input, Input::Scalar(None | Some(CanonicalScalar::Null))) {
        return None;
    }
    match (state, input) {
        (NumericState::Count(count), _) => Some(u128::from(u64::MAX - *count)),
        (NumericState::Sum(sum), Input::Scalar(Some(CanonicalScalar::Int(step)))) => {
            sum_capacity(sum.unwrap_or(0), *step)
        }
        (NumericState::Average { sum, count }, Input::Scalar(Some(CanonicalScalar::Int(step)))) => {
            let count = u128::from(u64::MAX - *count);
            Some(sum_capacity(*sum, *step).map_or(count, |sum| sum.min(count)))
        }
        (NumericState::Distinct(_) | NumericState::Extreme { .. }, _) => None,
        // The first ordinary update rejects wrong numeric types. Collections
        // cannot enter this physical profile. Never grant them an unsafe tail.
        _ => Some(0),
    }
}

fn advance_sum(total: i128, step: i64, count: u128) -> i128 {
    let position = (total as u128) ^ (1_u128 << 127);
    let distance = u128::from(step.unsigned_abs())
        .checked_mul(count)
        .expect("the common repetition capacity bounds the unsigned distance");
    let next = if step < 0 {
        position.checked_sub(distance)
    } else {
        position.checked_add(distance)
    }
    .expect("the common repetition capacity bounds the signed result");
    (next ^ (1_u128 << 127)) as i128
}

fn advance(state: &mut NumericState, input: Input<'_>, count: u128) {
    if matches!(input, Input::Scalar(None | Some(CanonicalScalar::Null))) {
        return;
    }
    match (state, input) {
        (NumericState::Count(value), _) => {
            *value += u64::try_from(count).expect("COUNT capacity fits u64");
        }
        (NumericState::Sum(total), Input::Scalar(Some(CanonicalScalar::Int(step)))) => {
            *total = Some(advance_sum(total.unwrap_or(0), *step, count));
        }
        (
            NumericState::Average {
                sum,
                count: denominator,
            },
            Input::Scalar(Some(CanonicalScalar::Int(step))),
        ) => {
            *sum = advance_sum(*sum, *step, count);
            *denominator += u64::try_from(count).expect("AVG capacity fits u64");
        }
        (NumericState::Distinct(_) | NumericState::Extreme { .. }, _) => {}
        _ => unreachable!("only validated numeric/support tails are advanced"),
    }
}

pub(super) fn update<'a, E, C>(
    query: &PreparedGraphAggregate,
    states: &mut [NumericState],
    value: &mut impl FnMut(usize) -> &'a GraphValue,
    repetitions: Option<u128>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), Failure<E, C>>,
) -> Result<(), Failure<E, C>> {
    debug_assert_ne!(repetitions, Some(0));
    unit(query, states, value, control)?;
    if repetitions == Some(1) {
        return Ok(()); // Preserve the existing unit-row event sequence.
    }
    let mut safe = None;
    for (aggregate, state) in query.aggregates().iter().zip(states.iter()) {
        control(GlaExecutionEvent::Work)?;
        let input = aggregate
            .argument_column()
            .map_or(Input::Identity, |at| Input::from_value(value(at)));
        if let Some(capacity) = capacity(state, input) {
            safe = Some(safe.map_or(capacity, |old: u128| old.min(capacity)));
        }
    }
    let remaining = repetitions.map(|count| count - 1);
    let steps = match (remaining, safe) {
        (Some(count), Some(safe)) => count.min(safe),
        (Some(count), None) => count,
        (None, Some(safe)) => safe,
        (None, None) => return Ok(()), // All cells are null/zero or support-only.
    };
    for (aggregate, state) in query.aggregates().iter().zip(states.iter_mut()) {
        control(GlaExecutionEvent::Work)?;
        let input = aggregate
            .argument_column()
            .map_or(Input::Identity, |at| Input::from_value(value(at)));
        advance(state, input, steps);
    }
    if remaining.is_none() || remaining.is_some_and(|count| count > steps) {
        // A nonzero finite-domain cell cannot accept >u128 occurrences. After
        // its first step, its remaining capacity is strictly below u128::MAX.
        // Re-enter the ordinary updater at exactly the first overflowing row;
        // the original column order chooses the reported aggregate.
        unit(query, states, value, control)?;
        unreachable!("the minimum finite capacity must refuse its next occurrence");
    }
    Ok(())
}

impl PreparedGraphAggregate {
    pub(super) fn repeated_columns(&self) -> Vec<usize> {
        let mut columns = self.group_key_columns().to_vec();
        columns.extend(
            self.aggregates()
                .iter()
                .filter_map(|aggregate| aggregate.argument_column()),
        );
        columns.sort_unstable();
        columns.dedup();
        columns
    }

    pub(super) fn execute_repeated_groups<E, C>(
        &self,
        columns: &[usize],
        policy: GqlQueryPolicy,
        source: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphAggregateRow>, Failure<E, C>> {
        let mut groups = Groups::new();
        let mut largest_key = 0;
        let mut deferred = None;
        let (rows, evaluator) = self
            .relational_input
            .as_ref()
            .expect("relational profile")
            .fold_repeated_governed(
                columns,
                policy,
                source,
                &mut checkpoint,
                |row, repetitions, control| {
                    if deferred.is_some() {
                        return Ok(());
                    }
                    let result = push_values(
                        self,
                        &mut groups,
                        &mut largest_key,
                        |column| {
                            &row.values()[columns
                                .binary_search(&column)
                                .expect("every group key and argument is retained")]
                        },
                        repetitions,
                        &mut |event| {
                            control(event).map_err(|error| {
                                error.map_source(GraphAggregateError::InputRelation)
                            })
                        },
                    );
                    match result {
                        Err(GqlQueryError::Source(error)) => {
                            deferred = Some(error);
                            Ok(())
                        }
                        Err(error) => {
                            Err(error.map_source(|_| unreachable!("source handled above")))
                        }
                        Ok(()) => Ok(()),
                    }
                },
            )
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
}

#[cfg(test)]
mod tests;
