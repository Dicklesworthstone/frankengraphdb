//! DISTINCT over final visible group cells, before output pagination.
//!
//! Sort borrowed groups by their visible tuple, then by their complete result
//! ordering. Keep the first representative of each equivalence class and rank
//! the representatives. Hidden keys and summaries never become owned rows.
//! All HAVING leaves are evaluated before selection, even for LIMIT 0.

use super::*;

impl PreparedGraphAggregate {
    pub(super) fn needs_output_distinct(&self) -> bool {
        self.output_distinct
            && self.key_output.as_ref().is_some_and(|projection| {
                (0..self.keys.len()).any(|key| !projection.columns.contains(&key))
            })
    }

    // The same prepared column has one nonnull aggregate domain. NULL compares
    // equal to NULL for DISTINCT; averages compare as exact rational values,
    // not by their unreduced accumulator (sum, count) representation.
    fn compare_output<E>(
        &self,
        left: Group<'_, '_>,
        right: Group<'_, '_>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Ordering, E> {
        let keys = self.key_output.as_ref().map(|projection| projection.columns.as_ref());
        for at in 0..keys.map_or(self.keys.len(), <[usize]>::len) {
            control(GlaExecutionEvent::Work)?;
            let column = keys.map_or(at, |keys| keys[at]);
            let (a, b) = (left.key[column], right.key[column]);
            for _ in 0..a.payload_units().max(b.payload_units()) {
                control(GlaExecutionEvent::Work)?;
            }
            let order = a.cmp(&b);
            if order != Ordering::Equal {
                return Ok(order);
            }
        }
        for at in 0..self.output_aggregates {
            control(GlaExecutionEvent::Work)?;
            let a = Cell::from_state(&left.state[at]);
            let b = Cell::from_state(&right.state[at]);
            for _ in 0..a.payload_units().max(b.payload_units()) {
                control(GlaExecutionEvent::Work)?;
            }
            let order = match (a.is_null(), b.is_null()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                (false, false) => a.compare(b),
            };
            if order != Ordering::Equal {
                return Ok(order);
            }
        }
        Ok(Ordering::Equal)
    }

    fn compare_representatives<E>(
        &self,
        left: Group<'_, '_>,
        right: Group<'_, '_>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Ordering, E> {
        let order = self.compare_output(left, right, control)?;
        if order == Ordering::Equal {
            // The first row in the ordinary, fully ordered group stream is
            // the representative, including hidden ORDER BY expressions.
            self.compare(left, right, control)
        } else {
            Ok(order)
        }
    }

    fn sift_representatives<E>(
        &self,
        heap: &mut [Group<'_, '_>],
        mut root: usize,
        end: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        while root < end / 2 {
            let mut child = 2 * root + 1;
            if child + 1 < end
                && self.compare_representatives(heap[child], heap[child + 1], control)?
                    == Ordering::Less
            {
                child += 1;
            }
            if self.compare_representatives(heap[root], heap[child], control)? != Ordering::Less {
                break;
            }
            control(GlaExecutionEvent::Work)?;
            heap.swap(root, child);
            root = child;
        }
        Ok(())
    }

    pub(super) fn finish_distinct_groups<'a, E, C>(
        &self,
        groups: &BTreeMap<Vec<ValueRef<'a>>, Vec<Accumulator<'a>>>,
        control: &mut impl FnMut(
            GlaExecutionEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<Vec<GraphAggregateRow>, GqlQueryError<GraphAggregateError<E>, C>> {
        let offset = usize::try_from(self.offset).unwrap_or(usize::MAX);
        let count = self.count.and_then(|count| usize::try_from(count).ok()).unwrap_or(usize::MAX);
        let mut selected = Vec::new();
        // Raw group count is an upper bound on the number of distinct rows.
        // An impossible page still observes all HAVING errors/checkpoints.
        let needs_rows = count != 0 && offset < groups.len();
        for (key, state) in groups {
            control(GlaExecutionEvent::Work)?;
            let group = Group { key, state };
            if self.keep(group, control)? && needs_rows {
                control(GlaExecutionEvent::ScratchEntry)?;
                selected.push(group);
            }
        }
        let len = selected.len();
        for root in (0..len / 2).rev() {
            self.sift_representatives(&mut selected, root, len, control)?;
        }
        for end in (1..len).rev() {
            control(GlaExecutionEvent::Work)?;
            selected.swap(0, end);
            self.sift_representatives(&mut selected, 0, end, control)?;
        }
        // Equal visible tuples are adjacent and already ordered by the final
        // rank. Compact in place without allocating an owned key or hash set.
        let mut unique = 0;
        for read in 0..len {
            if unique == 0
                || self.compare_output(selected[unique - 1], selected[read], control)?
                    != Ordering::Equal
            {
                if unique != read {
                    control(GlaExecutionEvent::Work)?;
                    selected[unique] = selected[read];
                }
                unique += 1;
            }
        }
        if count == 0 || offset >= unique {
            return Ok(Vec::new());
        }
        let prefix = offset.saturating_add(count).min(unique);
        // Reuse the admitted buffer for a worst-first top-prefix heap. The
        // remaining representatives are scanned once, not sorted wholesale.
        for root in (0..prefix / 2).rev() {
            self.sift_down(&mut selected, root, prefix, control)?;
        }
        for at in prefix..unique {
            let candidate = selected[at];
            if self.compare(candidate, selected[0], control)? == Ordering::Less {
                control(GlaExecutionEvent::Work)?;
                selected[0] = candidate;
                self.sift_down(&mut selected, 0, prefix, control)?;
            }
        }
        for end in (1..prefix).rev() {
            control(GlaExecutionEvent::Work)?;
            selected.swap(0, end);
            self.sift_down(&mut selected, 0, end, control)?;
        }
        let mut output = Vec::new();
        for group in selected[..prefix].iter().copied().skip(offset).take(count) {
            output.push(group.copy_owned(self.key_output.as_ref(), self.output_aggregates, control)?);
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algebra::{GraphColumn, GraphPatternBuilder};

    fn definition(offset: u64, count: Option<u64>) -> PreparedGraphAggregate {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        let input = builder.prepare_values(&[
            GraphColumn::vertex("key", "n"),
            GraphColumn::property("p", "n", PropertyKeyId(1)),
        ], 0, None).unwrap().with_duplicates();
        PreparedGraphAggregate::prepare(input, &[0], &[
            GraphAggregate::count_rows("n"), GraphAggregate::sum_int("sum", 1),
        ], offset, count).unwrap()
    }

    type Groups<'a> = BTreeMap<Vec<ValueRef<'a>>, Vec<Accumulator<'a>>>;
    fn groups(values: &[Option<i128>]) -> Groups<'static> {
        values.iter().enumerate().map(|(at, value)| (
            vec![ValueRef::Vertex(VId(at as u128))],
            vec![Accumulator::Count((at % 3 + 1) as u64),
                Accumulator::Sum { value: value.unwrap_or(0), present: value.is_some() }],
        )).collect()
    }
    fn run(query: &PreparedGraphAggregate, groups: &Groups<'_>) -> Vec<GraphAggregateRow> {
        query.finish_groups(groups, &mut |_| {
            Ok::<_, GqlQueryError<GraphAggregateError<()>, ()>>(())
        }).unwrap()
    }

    #[test]
    fn distinct_projection_matches_ordered_first_occurrence_before_pagination() {
        for code in 0..4_usize.pow(4) {
            let mut encoded = code;
            let values: Vec<_> = (0..4).map(|_| {
                let value = [None, Some(-7), Some(0), Some(7)][encoded % 4];
                encoded /= 4;
                value
            }).collect();
            let input = groups(&values);
            for order in [Vec::new(), vec![GraphAggregateOrder::descending(GraphAggregateColumn::Aggregate(1))]] {
                for keys in [&[][..], &[0][..], &[0, 0][..]] {
                    for summaries in 0..=2 {
                        let full = definition(0, None).with_key_output_columns(keys).unwrap()
                            .with_aggregate_output_prefix(summaries).unwrap()
                            .with_result_clauses(&[], &order).unwrap();
                        let mut expected = Vec::new();
                        for row in run(&full, &input) {
                            if !expected.contains(&row) { expected.push(row); }
                        }
                        for offset in 0..=5 {
                            for count in [None, Some(0), Some(1), Some(3), Some(u64::MAX)] {
                                let query = definition(offset, count).with_key_output_columns(keys).unwrap()
                                    .with_aggregate_output_prefix(summaries).unwrap()
                                    .with_result_clauses(&[], &order).unwrap().with_distinct_output(true);
                                let page: Vec<_> = expected.iter().skip(offset as usize)
                                    .take(count.and_then(|n| usize::try_from(n).ok()).unwrap_or(usize::MAX)).cloned().collect();
                                assert_eq!(run(&query, &input), page);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn distinct_mode_is_reversible_and_survives_projection_changes() {
        let base = definition(0, None);
        let bytes = base.canonical_bytes();
        let distinct = base.clone().with_distinct_output(true);
        assert_eq!(distinct.clone().with_distinct_output(false), base);
        assert_ne!(distinct.canonical_bytes(), bytes);
        assert!(!distinct.needs_output_distinct());
        let hidden = distinct.with_key_output_columns(&[]).unwrap();
        assert!(hidden.needs_output_distinct());
        assert!(!hidden.with_key_output_columns(&[0, 0]).unwrap().needs_output_distinct());
        assert_eq!(base.canonical_bytes(), bytes);
    }

    #[test]
    fn hidden_ranking_selects_the_first_representative_not_a_premature_top_k() {
        let mut input = BTreeMap::new();
        for (id, n, rank) in [(0, 1, 9), (1, 1, 8), (2, 2, 7), (3, 2, 6), (4, 3, 5)] {
            input.insert(vec![ValueRef::Vertex(VId(id))], vec![Accumulator::Count(n),
                Accumulator::Sum { value: rank, present: true }]);
        }
        let query = definition(1, Some(2)).with_key_output_columns(&[]).unwrap()
            .with_aggregate_output_prefix(1).unwrap().with_result_clauses(&[], &[
                GraphAggregateOrder::descending(GraphAggregateColumn::Aggregate(1)),
            ]).unwrap().with_distinct_output(true);
        let rows = run(&query, &input);
        assert_eq!(rows.iter().map(|row| row.get(0).unwrap().as_count().unwrap()).collect::<Vec<_>>(), vec![2, 3]);
        assert!(rows.iter().all(|row| row.keys().is_empty()));
    }

    #[test]
    fn every_distinct_sort_comparison_compaction_and_copy_is_interruptible() {
        let input = groups(&[Some(3), None, Some(-1), Some(3), Some(8)]);
        let query = definition(1, Some(2)).with_key_output_columns(&[]).unwrap()
            .with_aggregate_output_prefix(1).unwrap().with_result_clauses(&[], &[
                GraphAggregateOrder::descending(GraphAggregateColumn::Aggregate(1)),
            ]).unwrap().with_distinct_output(true);
        let mut total = 0;
        let expected = query.finish_groups(&input, &mut |_| {
            total += 1; Ok::<_, GqlQueryError<GraphAggregateError<()>, usize>>(())
        }).unwrap();
        for stop in 1..=total {
            let mut at = 0;
            let result = query.finish_groups(&input, &mut |_| {
                at += 1;
                if at == stop { Err(GqlQueryError::<GraphAggregateError<()>, _>::Interrupted(stop)) }
                else { Ok(()) }
            });
            assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
            assert_eq!(at, stop);
        }
        assert_eq!(run(&query, &input), expected);
    }

    #[test]
    fn zero_distinct_pages_still_evaluate_invalid_hidden_having_operands() {
        let boolean = CanonicalScalar::Bool(true);
        let mut input = Groups::new();
        input.insert(vec![ValueRef::Scalar(&boolean)], vec![Accumulator::Count(1),
            Accumulator::Sum { value: 0, present: false }]);
        for (offset, count) in [(0, Some(0)), (u64::MAX, None)] {
            let query = definition(offset, count).with_key_output_columns(&[]).unwrap()
                .with_aggregate_output_prefix(0).unwrap().with_result_clauses(&[
                    GraphAggregateFilter { column: GraphAggregateColumn::GroupKey(0),
                        test: GraphAggregateTest::Integer { comparison: IntegerComparison::Greater, value: 0 } },
                ], &[]).unwrap().with_distinct_output(true);
            let result = query.finish_groups(&input, &mut |_| {
                Ok::<_, GqlQueryError<GraphAggregateError<()>, ()>>(())
            });
            assert!(matches!(result, Err(GqlQueryError::Source(
                GraphAggregateError::NonIntegerHaving { predicate: 0 }))));
        }
    }

    #[test]
    fn distinct_copies_neither_hidden_payloads_nor_duplicate_output_rows() {
        let payloads: Vec<_> = (0..5).map(|n| CanonicalScalar::bytes(vec![n; 8192]).unwrap()).collect();
        let mut input = Groups::new();
        for payload in &payloads {
            input.insert(vec![ValueRef::Scalar(payload)], vec![Accumulator::Count(1),
                Accumulator::Sum { value: 0, present: false }]);
        }
        let query = definition(0, None).with_key_output_columns(&[]).unwrap()
            .with_aggregate_output_prefix(0).unwrap().with_distinct_output(true);
        let mut scratch = 0;
        let rows = query.finish_groups(&input, &mut |event| {
            scratch += usize::from(event == GlaExecutionEvent::ScratchEntry);
            Ok::<_, GqlQueryError<GraphAggregateError<()>, ()>>(())
        }).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].keys().is_empty() && rows[0].values().is_empty());
        assert_eq!(scratch, 5 + 1);
    }
}
