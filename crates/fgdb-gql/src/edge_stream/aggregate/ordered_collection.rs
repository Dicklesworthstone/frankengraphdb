//! Collection support keyed by the complete canonical identified input row.
//!
//! Several COLLECT arguments must observe ONE occurrence order, not independent
//! argument sorts. A key is retained once per contributing match and shared by
//! all collection cells in its group. Numeric cells consume input immediately;
//! only collection cells are populated after the key order has been established.
//! No expression, source access, result evaluation or second matcher runs here.
//! This is governed resident aggregate support, not a spill-backed input owner.

use super::*;
use crate::algebra::GraphValueRow;
use core::cmp::Ordering;

/// A compile-time order proof, independent of input cardinality. The ordinary
/// join compiler must still admit every source operator and the canonical ALL
/// tail. Unidentified pair traversal and transformed inputs have other order
/// contracts and must not be inferred from a row's values at runtime.
#[derive(Clone)]
pub(super) struct Layout {
    targets: Vec<(usize, usize)>,
}
impl Layout {
    pub(super) fn prepare(aggregate: &PreparedGraphAggregate) -> Option<Self> {
        if aggregate.input_projection().is_some()
            || aggregate.input_relation().is_some()
            || !aggregate.input_pattern().plan().requires_identified_edges()
        {
            return None;
        }
        let targets: Vec<_> = aggregate
            .aggregates()
            .iter()
            .enumerate()
            .filter(|(_, spec)| NumericState::collects(spec.function()))
            .map(|(at, spec)| Some((at, spec.argument_column()?)))
            .collect::<Option<_>>()?;
        (!targets.is_empty()).then_some(Self { targets })
    }
}

#[derive(Default)]
pub(super) struct State {
    rows: Vec<GraphValueRow>,
}
impl State {
    pub(super) fn push<E>(
        &mut self,
        layout: &Layout,
        row: GraphValueRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut contributes = false;
        for &(_, column) in &layout.targets {
            control(GlaExecutionEvent::Work)?;
            contributes |= !row.values()[column].is_null();
        }
        if contributes {
            // The source has already charged and transferred the complete
            // owned row. Retaining it needs one support entry, not another
            // unmetered payload clone or one key copy per COLLECT argument.
            control(GlaExecutionEvent::ScratchEntry)?;
            self.rows.push(row);
        }
        Ok(())
    }

    pub(super) fn finish<E, C>(
        mut self,
        layout: &Layout,
        states: &mut [NumericState],
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), EdgeAggregateError<E, C>>,
    ) -> Result<(), EdgeAggregateError<E, C>> {
        sort(&mut self.rows, control)?;
        for row in self.rows {
            for &(aggregate, column) in &layout.targets {
                control(GlaExecutionEvent::Work)?;
                // Canonical DISTINCT membership, NULL omission, collection
                // payload ownership and admission remain in the same cells
                // used by direct ordered input. Equal keys have equal values,
                // so their tie order cannot change any collected sequence.
                states[aggregate].update_governed(
                    Input::from_value(&row.values()[column]),
                    aggregate,
                    &mut |event| control(value_event(event)),
                )?;
            }
        }
        Ok(())
    }
}

fn compare<E>(
    left: &GraphValueRow,
    right: &GraphValueRow,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Ordering, E> {
    control(GlaExecutionEvent::Work)?;
    for (left, right) in left.values().iter().zip(right.values()) {
        control(GlaExecutionEvent::Work)?;
        for _ in 0..left.payload_units().max(right.payload_units()) {
            control(GlaExecutionEvent::Work)?;
        }
        // The child uses canonical tuple order, NOT language ORDER BY. In
        // particular, numeric representation and nested values remain exact.
        let order = left.cmp(right);
        if order != Ordering::Equal {
            return Ok(order);
        }
    }
    Ok(left.values().len().cmp(&right.values().len()))
}

// In-place fallible heapsort: no uncharged sort workspace and no swallowed
// control failure inside an infallible comparison closure. The owning cursor
// drops this entire private state after any refusal, including during a swap.
fn sort<E>(
    rows: &mut [GraphValueRow],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    fn sift<E>(
        rows: &mut [GraphValueRow],
        mut root: usize,
        end: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        // root < end/2 proves that 2*root+1 exists and cannot overflow.
        while root < end / 2 {
            let mut child = root * 2 + 1;
            if child + 1 < end && compare(&rows[child], &rows[child + 1], control)?.is_lt() {
                child += 1;
            }
            if !compare(&rows[root], &rows[child], control)?.is_lt() {
                break;
            }
            control(GlaExecutionEvent::Work)?;
            rows.swap(root, child);
            root = child;
        }
        Ok(())
    }
    control(GlaExecutionEvent::Work)?;
    let len = rows.len();
    for root in (0..len / 2).rev() {
        sift(rows, root, len, control)?;
    }
    for end in (1..len).rev() {
        control(GlaExecutionEvent::Work)?;
        rows.swap(0, end);
        sift(rows, 0, end, control)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GraphAggregateFunction, GraphAggregateValue};

    fn row(key: i64, value: i64) -> GraphValueRow {
        GraphValueRow::from_owned_values(vec![
            GraphValue::Scalar(CanonicalScalar::Int(key)),
            GraphValue::Scalar(CanonicalScalar::Int(value)),
        ])
    }

    #[test]
    fn fallible_key_sort_matches_lexicographic_order_for_all_small_permutations() {
        let values = [row(1, 9), row(0, 7), row(1, -2), row(0, 7)];
        for length in 0..=6 {
            for mut code in 0..4_usize.pow(length) {
                let mut rows = Vec::new();
                for _ in 0..length {
                    rows.push(values[code % 4].clone());
                    code /= 4;
                }
                let mut expected = rows.clone();
                expected.sort_by(|a, b| a.values().cmp(b.values()));
                sort(&mut rows, &mut |_| Ok::<_, ()>(())).unwrap();
                assert_eq!(rows, expected);
            }
        }
    }

    #[test]
    fn every_key_sort_checkpoint_is_propagated_without_accepting_a_partial_sort() {
        let payload =
            GraphValue::Scalar(CanonicalScalar::ucs_basic_text(&"x".repeat(257)).unwrap());
        let rows: Vec<_> = [9, 3, 8, 1, 7, 2, 6]
            .into_iter()
            .map(|value| {
                GraphValueRow::from_owned_values(vec![
                    payload.clone(),
                    row(value, 0).values()[0].clone(),
                ])
            })
            .collect();
        let mut calls = 0;
        let mut sorted = rows.clone();
        sort(&mut sorted, &mut |_| {
            calls += 1;
            Ok::<_, usize>(())
        })
        .unwrap();
        for stop in 1..=calls {
            let mut seen = 0;
            let mut candidate = rows.clone();
            assert_eq!(
                sort(&mut candidate, &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                }),
                Err(stop)
            );
            assert_eq!(seen, stop);
            // The caller discards failed state; a fresh attempt is unchanged.
            let mut retry = rows.clone();
            sort(&mut retry, &mut |_| Ok::<_, ()>(())).unwrap();
            assert_eq!(retry, sorted);
        }
    }

    #[test]
    fn collection_cells_share_the_key_order_and_keep_distinct_first_occurrences() {
        let layout = Layout {
            targets: vec![(0, 0), (1, 1), (2, 1)],
        };
        let mut support = State::default();
        for row in [row(2, 1), row(0, 9), row(1, 3), row(3, 9)] {
            support
                .push(&layout, row, &mut |_| Ok::<_, ()>(()))
                .unwrap();
        }
        let mut states: Vec<_> = [
            GraphAggregateFunction::Collect,
            GraphAggregateFunction::Collect,
            GraphAggregateFunction::CollectDistinct,
        ]
        .into_iter()
        .map(|function| {
            NumericState::new_governed(function, &mut |_| Ok::<_, EdgeAggregateError<(), ()>>(()))
                .unwrap()
        })
        .collect();
        support
            .finish(&layout, &mut states, &mut |_| {
                Ok::<_, EdgeAggregateError<(), ()>>(())
            })
            .unwrap();
        let actual: Vec<_> = states
            .into_iter()
            .map(|state| {
                state
                    .finish_governed(&mut |_| Ok::<_, EdgeAggregateError<(), ()>>(()))
                    .unwrap()
            })
            .collect();
        let list = |values: &[i64]| {
            GraphAggregateValue::Value(GraphValue::List(
                values
                    .iter()
                    .map(|&x| GraphValue::Scalar(CanonicalScalar::Int(x)))
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            ))
        };
        assert_eq!(
            actual,
            [list(&[0, 1, 2, 3]), list(&[9, 3, 1, 9]), list(&[9, 3, 1])]
        );
    }
}
