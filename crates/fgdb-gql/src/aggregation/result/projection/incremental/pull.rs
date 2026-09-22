//! Completed-group filtering/projection for physical pull reducers.
//!
//! This adapter neither reads graph data nor reconstructs accumulators. It
//! borrows the exact finalized cells through the existing HAVING interpreter
//! and the same result-projection evaluator as incremental/batch consumers.

use super::*;
use super::super::super::having::GroupCells;

mod distinct;
use distinct::{DistinctIndex, DistinctKey};

#[derive(Clone, Copy)]
struct CompleteGroup<'a>(&'a GraphAggregateRow);
impl<'a> GroupCells<'a> for CompleteGroup<'a> {
    fn cell(self, column: GraphAggregateColumn) -> Cell<'a> {
        match column {
            GraphAggregateColumn::GroupKey(at) => Cell::Value(value_ref(&self.0.keys[at])),
            GraphAggregateColumn::Aggregate(at) => result_cell(&self.0.values[at]),
        }
    }
}

impl PreparedGraphAggregate {
    /// Normalize legacy conjunctive HAVING filters into the SAME bounded
    /// program as textual HAVING, once at physical-plan admission. The original
    /// logical definition/canonical transcript is never changed. All operands,
    /// including hidden keys and summaries, address the full evaluation schema.
    /// This private gate does not widen any incremental-maintenance contract.
    pub(crate) fn prepare_streamed_output(&self) -> Option<Self> {
        if self.supports_row_local_aggregate_stream() { return Some(self.clone()); }
        if self.relational_input.is_some() {
            return None;
        }
        self.prepare_complete_group_output()
    }

    /// Normalize only completed-group output. The caller separately admits its
    /// source and accumulator profile. Relational folding may reuse this result
    /// stage without widening vertex/edge pull-source admission above.
    pub(crate) fn prepare_complete_group_output(&self) -> Option<Self> {
        if self.supports_row_local_aggregate_stream() { return Some(self.clone()); }
        let mut physical = self.clone();
        if !self.having.is_empty() {
            let mut program = Vec::new();
            for (at, filter) in self.having.iter().enumerate() {
                let operand = GraphHavingOperand::Column(filter.column);
                program.push(match filter.test {
                    GraphAggregateTest::IsNull => GraphHavingOp::IsNull { operand, is_null: true },
                    GraphAggregateTest::IsNotNull => GraphHavingOp::IsNull { operand, is_null: false },
                    GraphAggregateTest::Integer { comparison, value } => GraphHavingOp::Compare {
                        left: operand, comparison, right: GraphHavingOperand::Integer(value),
                    },
                });
                if at != 0 { program.push(GraphHavingOp::And); }
            }
            physical.having_expression = Some(GraphHavingExpression::prepare(&program).ok()?);
            physical.having.clear();
        }
        Some(physical)
    }

    /// Plain reductions keep their original event sequence and move-only
    /// delivery. Clauses require a complete pre-delivery validation pass, even
    /// when the result window is empty or earlier groups fill the requested page.
    pub(crate) fn has_streamed_output_stage(&self) -> bool {
        self.having_expression.is_some()
            || self.output_projection.is_some()
            || self.key_output.is_some()
            || self.output_aggregates != self.aggregates.len()
            || self.output_distinct
            || !self.ordering.is_empty()
            || self.offset != 0
            || self.count.is_some()
    }

    /// The owning reducer must supply every complete group in canonical key
    /// order. None is only HAVING FALSE/UNKNOWN, never a schema or source error.
    /// Apply pagination after this call: every qualified output expression is
    /// evaluated even for skipped/off-page groups. No ResultRow event is emitted
    /// here; only actual delivery consumes that counter.
    pub(crate) fn evaluate_streamed_output<E, C>(
        &self,
        row: GraphAggregateRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), QueryError<E, C>>,
    ) -> Result<Option<GraphAggregateRow>, QueryError<E, C>> {
        if !self.qualifies_streamed_output(&row, control)? { return Ok(None); }
        if !self.transforms_streamed_columns() {
            return Ok(Some(row));
        }
        self.project_complete_output(&row, control).map(Some)
    }

    fn transforms_streamed_columns(&self) -> bool {
        self.output_projection.is_some() || self.key_output.is_some()
            || self.output_aggregates != self.aggregates.len()
    }

    fn qualifies_streamed_output<E, C>(
        &self,
        row: &GraphAggregateRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), QueryError<E, C>>,
    ) -> Result<bool, QueryError<E, C>> {
        debug_assert_eq!(row.keys.len(), self.keys.len());
        debug_assert_eq!(row.values.len(), self.aggregates.len());
        control(GlaExecutionEvent::Work)?;
        if let Some(having) = &self.having_expression {
            return having.evaluate(CompleteGroup(row), control);
        }
        Ok(true)
    }

    /// Physical ranking over finalized groups, never over matched bindings.
    /// The caller supplies the exact number of complete groups before HAVING;
    /// the finite prefix cannot exceed it. No allocation depends on a huge
    /// numeric LIMIT alone. DISTINCT tracks only the retained prefix's output
    /// classes; the representative of each class is its best-ranked full group.
    /// This does not widen any incremental-maintenance or input-source gate.
    pub(crate) fn streamed_group_ranking(&self, groups: usize) -> StreamedGroupRanking {
        let offset = usize::try_from(self.offset).unwrap_or(usize::MAX);
        let count = self.count.and_then(|n| usize::try_from(n).ok()).unwrap_or(usize::MAX);
        let prefix = if count == 0 || offset >= groups {
            0
        } else {
            offset.saturating_add(count).min(groups)
        };
        StreamedGroupRanking {
            offset, count, prefix, heap: Vec::new(),
            distinct: self.output_distinct.then(DistinctIndex::default),
        }
    }
}

// Hidden evaluation cells own the rank and its canonical key tiebreak. Output
// may hide/repeat those keys or map many different groups to the same tuple.
// Without a column transform the complete row is moved, not cloned twice.
struct RankedGroup {
    complete: GraphAggregateRow,
    projected: Option<GraphAggregateRow>,
    distinct_key: Option<DistinctKey>,
}

/// One-query, fail-stop physical selection state. A failed push/finish must be
/// discarded by its owning cursor; no tentative page has escaped. The heap is
/// worst-first and holds at most min(groups, SKIP + LIMIT) complete candidates,
/// plus one transient candidate being validated. DISTINCT membership is indexed
/// only for those retained candidates, not for every output class ever visited.
/// A previously evicted class may re-enter with a better representative; its
/// discarded representative cannot beat the monotonically improving cutoff.
/// Group accumulation upstream
/// is still separately governed in-memory state, not a spill implementation.
pub(crate) struct StreamedGroupRanking {
    offset: usize,
    count: usize,
    prefix: usize,
    heap: Vec<RankedGroup>,
    distinct: Option<DistinctIndex>,
}

impl StreamedGroupRanking {
    pub(crate) fn push<E, C>(
        &mut self,
        query: &PreparedGraphAggregate,
        row: GraphAggregateRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), QueryError<E, C>>,
    ) -> Result<(), QueryError<E, C>> {
        if !query.qualifies_streamed_output(&row, control)? { return Ok(()); }
        // Evaluate even a losing, skipped, or LIMIT-0 group's expressions.
        // Early rank elimination must never conceal a later typed failure.
        let projected = if query.transforms_streamed_columns() {
            Some(query.project_complete_output(&row, control)?)
        } else {
            None
        };
        if self.prefix == 0 { return Ok(()); }
        let mut candidate = RankedGroup { complete: row, projected, distinct_key: None };
        if let Some(index) = &mut self.distinct {
            // Normalize equality only. The chosen representative keeps its
            // original count/integer/fraction/scalar variants for delivery.
            let key = DistinctIndex::key(candidate.projected.as_ref().unwrap_or(&candidate.complete), control)?;
            if let Some(at) = index.position(&key, control)? {
                if Self::compare(query, &candidate, &self.heap[at], control)? == Ordering::Less {
                    control(GlaExecutionEvent::Work)?;
                    // Share the resident key; do not retain a second equal
                    // payload merely because this representative improved.
                    candidate.distinct_key = self.heap[at].distinct_key.clone();
                    self.heap[at] = candidate;
                    // Rank improved: only the children may violate max-heap
                    // order. The class's slot is repaired on every swap.
                    self.sift_down(query, at, self.heap.len(), control)?;
                }
                return Ok(());
            }
            candidate.distinct_key = Some(key);
        }
        if self.heap.len() < self.prefix {
            control(GlaExecutionEvent::ScratchEntry)?;
            let mut child = self.heap.len();
            if let Some(index) = &mut self.distinct {
                index.insert(candidate.distinct_key.as_ref().expect("distinct candidate key").clone(), child, control)?;
            }
            self.heap.push(candidate);
            while child > 0 {
                let parent = (child - 1) / 2;
                if Self::compare(query, &self.heap[parent], &self.heap[child], control)?
                    != Ordering::Less {
                    break;
                }
                self.swap(parent, child, control)?;
                child = parent;
            }
        } else if Self::compare(query, &candidate, &self.heap[0], control)? == Ordering::Less {
            control(GlaExecutionEvent::Work)?;
            if let Some(index) = &mut self.distinct {
                index.remove(self.heap[0].distinct_key.as_ref().expect("resident distinct key"), control)?;
                index.insert(candidate.distinct_key.as_ref().expect("distinct candidate key").clone(), 0, control)?;
            }
            self.heap[0] = candidate;
            self.sift_down(query, 0, self.heap.len(), control)?;
        }
        Ok(())
    }

    fn swap<E>(
        &mut self,
        left: usize,
        right: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        control(GlaExecutionEvent::Work)?;
        self.heap.swap(left, right);
        if let Some(index) = &mut self.distinct {
            for at in [left, right] {
                index.move_to(self.heap[at].distinct_key.as_ref().expect("resident distinct key"), at, control)?;
            }
        }
        Ok(())
    }

    fn compare<E>(
        query: &PreparedGraphAggregate,
        left: &RankedGroup,
        right: &RankedGroup,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Ordering, E> {
        // Use the canonical exact comparator, not derived enum Ord, floats,
        // subtraction, or a projected output's possibly missing sort column.
        Ok(left.complete.compare_incremental_order(&right.complete, query.ordering(), control)?
            .expect("physical reducer supplied complete groups for its checked definition"))
    }

    fn sift_down<E>(
        &mut self,
        query: &PreparedGraphAggregate,
        mut root: usize,
        end: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        while root < end / 2 {
            let mut child = 2 * root + 1;
            if child + 1 < end
                && Self::compare(query, &self.heap[child], &self.heap[child + 1], control)?
                    == Ordering::Less {
                child += 1;
            }
            if Self::compare(query, &self.heap[root], &self.heap[child], control)?
                != Ordering::Less {
                break;
            }
            self.swap(root, child, control)?;
            root = child;
        }
        Ok(())
    }

    pub(crate) fn finish<E>(
        mut self,
        query: &PreparedGraphAggregate,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Vec<GraphAggregateRow>, E> {
        // Membership serves admission only; sorting no longer needs positions.
        // Heap entries share the key payloads, so this releases just the index.
        self.distinct = None;
        // Fallible, governed in-place heap sort: no comparator can ignore a
        // cancellation/budget refusal, and no second sort buffer is allocated.
        for end in (1..self.heap.len()).rev() {
            control(GlaExecutionEvent::Work)?;
            self.heap.swap(0, end);
            self.sift_down(query, 0, end, control)?;
        }
        let mut output = Vec::new();
        for group in self.heap.into_iter().skip(self.offset).take(self.count) {
            control(GlaExecutionEvent::Work)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            output.push(group.projected.unwrap_or(group.complete));
        }
        control(GlaExecutionEvent::Work)?;
        Ok(output)
    }
}

#[cfg(test)]
mod distinct_tests;

#[cfg(test)]
mod bounded_distinct_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algebra::{GraphColumn, GraphPatternBuilder};
    use crate::{GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp, GraphSetProjection};
    use fgdb_delta_types::PropertyKeyId;

    fn definition(offset: u64, count: Option<u64>, descending: bool, nulls: GraphNullPlacement)
        -> PreparedGraphAggregate {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        let input = builder.prepare_values(&[
            GraphColumn::property("key", "n", PropertyKeyId(1)),
            GraphColumn::property("amount", "n", PropertyKeyId(2)),
        ], 0, None).unwrap().with_duplicates();
        PreparedGraphAggregate::prepare(input, &[0], &[
            GraphAggregate::count_rows("count"), GraphAggregate::average_int("average", 1),
        ], offset, count).unwrap().with_result_clauses(&[], &[
            GraphAggregateOrder { column: GraphAggregateColumn::Aggregate(1), descending, nulls },
            GraphAggregateOrder::descending(GraphAggregateColumn::Aggregate(0)),
        ]).unwrap()
    }

    fn rows() -> Vec<GraphAggregateRow> {
        (0..37).map(|i| GraphAggregateRow::from_group_values(
            vec![GraphValue::Scalar(CanonicalScalar::Int(i))],
            vec![GraphAggregateValue::Count((i as u64 * 7) % 5 + 1),
                if i % 7 == 0 {
                    GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null))
                } else {
                    GraphAggregateValue::Average(GraphExactAverage::new(
                        i128::from((i * 13) % 23 - 11), (i as u64 * 5) % 3 + 1,
                    ).unwrap())
                }],
        )).collect()
    }

    // Independent small-domain rational order; never call the production
    // comparator or use aggregate enum tags as a surrogate numeric order.
    fn oracle_order(a: &GraphAggregateRow, b: &GraphAggregateRow, descending: bool,
        nulls: GraphNullPlacement) -> Ordering {
        let fraction = |row: &GraphAggregateRow| match row.values()[1] {
            GraphAggregateValue::Average(value) => Some((value.numerator(), value.denominator())),
            _ => None,
        };
        let order = match (fraction(a), fraction(b)) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => if nulls == GraphNullPlacement::First { Ordering::Less } else { Ordering::Greater },
            (Some(_), None) => if nulls == GraphNullPlacement::First { Ordering::Greater } else { Ordering::Less },
            (Some((a, da)), Some((b, db))) => {
                let result = (a * i128::from(db)).cmp(&(b * i128::from(da)));
                if descending { result.reverse() } else { result }
            }
        };
        order.then_with(|| b.values()[0].cmp(&a.values()[0])).then_with(|| a.keys().cmp(b.keys()))
    }

    #[test]
    fn ordered_prefix_matches_independent_sort_without_losing_hidden_keys_or_exact_averages() {
        for descending in [false, true] {
            for nulls in [GraphNullPlacement::First, GraphNullPlacement::Last] {
                for offset in [0, 1, 8, 64, u64::MAX] {
                    for count in [None, Some(0), Some(1), Some(4), Some(u64::MAX)] {
                        for projection in 0..3 {
                            let q = definition(offset, count, descending, nulls);
                            let q = match projection {
                                1 => q.with_key_output_columns(&[]).unwrap().with_aggregate_output_prefix(1).unwrap(),
                                2 => q.with_key_output_columns(&[0, 0]).unwrap(),
                                _ => q,
                            };
                            let before = q.canonical_bytes();
                            let physical = q.prepare_streamed_output().unwrap();
                            let rows = rows();
                            let mut expected = rows.clone();
                            expected.sort_by(|a, b| oracle_order(a, b, descending, nulls));
                            let expected: Vec<_> = expected.into_iter()
                                .skip(usize::try_from(offset).unwrap_or(usize::MAX))
                                .take(count.and_then(|n| usize::try_from(n).ok()).unwrap_or(usize::MAX))
                                .map(|row| match projection {
                                    1 => GraphAggregateRow::from_group_values(vec![], vec![row.values()[0].clone()]),
                                    2 => GraphAggregateRow::from_group_values(vec![row.keys()[0].clone(); 2], row.values().to_vec()),
                                    _ => row,
                                }).collect();
                            let mut ranking = physical.streamed_group_ranking(rows.len());
                            let mut control = |event| {
                                assert_ne!(event, GlaExecutionEvent::ResultRow);
                                Ok::<_, QueryError<(), ()>>(())
                            };
                            for row in rows {
                                ranking.push(&physical, row, &mut control).unwrap();
                                assert!(ranking.heap.len() <= ranking.prefix);
                                assert!(ranking.heap.len() <= 37);
                            }
                            assert_eq!(ranking.finish(&physical, &mut control).unwrap(), expected);
                            assert_eq!(q.canonical_bytes(), before);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn ordered_off_page_expression_errors_survive_empty_huge_and_full_windows() {
        for (offset, count) in [(0, Some(0)), (0, Some(1)), (u64::MAX, Some(u64::MAX))] {
            let q = definition(offset, count, true, GraphNullPlacement::Last)
                .with_output_projection(vec![GraphSetProjection::new("reciprocal", GraphSetValue::Integer(
                    GraphIntegerExpression::prepare(&[
                        GraphIntegerOp::Literal(Some(1)), GraphIntegerOp::Column(1),
                        GraphIntegerOp::Literal(Some(1)), GraphIntegerOp::Binary(GraphIntegerBinary::Subtract),
                        GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
                    ]).unwrap(),
                ))]).unwrap().prepare_streamed_output().unwrap();
            let mut control = |_| Ok::<_, QueryError<(), ()>>(());
            let mut ranking = q.streamed_group_ranking(2);
            let make = |key, count| GraphAggregateRow::from_group_values(
                vec![GraphValue::Scalar(CanonicalScalar::Int(key))],
                vec![GraphAggregateValue::Count(count), GraphAggregateValue::Average(GraphExactAverage::new(2, 1).unwrap())],
            );
            ranking.push(&q, make(0, 2), &mut control).unwrap();
            assert!(matches!(ranking.push(&q, make(1, 1), &mut control),
                Err(GqlQueryError::Source(GraphAggregateError::OutputExpression { column: 0, .. }))));
        }
    }

    fn interrupted_run(q: &PreparedGraphAggregate, stop: usize)
        -> (Result<Vec<GraphAggregateRow>, QueryError<(), usize>>, usize) {
        let mut calls = 0;
        let result = (|| {
            let mut control = |_| {
                calls += 1;
                if calls == stop { Err(GqlQueryError::Interrupted(stop)) } else { Ok(()) }
            };
            let mut ranking = q.streamed_group_ranking(9);
            for row in rows().into_iter().take(9) { ranking.push(q, row, &mut control)?; }
            ranking.finish(q, &mut control)
        })();
        (result, calls)
    }

    #[test]
    fn every_rank_projection_and_sort_checkpoint_can_refuse_and_retry_from_scratch() {
        let q = definition(1, Some(3), true, GraphNullPlacement::First)
            .with_key_output_columns(&[0, 0]).unwrap().prepare_streamed_output().unwrap();
        let (expected, calls) = interrupted_run(&q, usize::MAX);
        assert_eq!(expected.as_ref().unwrap().len(), 3);
        for stop in 1..=calls {
            let (result, seen) = interrupted_run(&q, stop);
            assert_eq!(result, Err(GqlQueryError::Interrupted(stop)));
            assert_eq!(seen, stop);
        }
        assert_eq!(interrupted_run(&q, usize::MAX), (expected, calls));
    }
}
