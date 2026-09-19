//! Governed ordering of complete maintained groups, shared with batch results.
//!
//! This is a comparison primitive, not a sort, a graph scan or an admission of
//! ranked standing queries. Consumers retain every candidate needed to refill
//! a page and publish ordering, representatives and result rows atomically.

use super::*;

fn cell(row: &GraphAggregateRow, column: GraphAggregateColumn) -> Option<Cell<'_>> {
    match column {
        GraphAggregateColumn::GroupKey(at) => {
            row.keys.get(at).map(|value| Cell::Value(value_ref(value)))
        }
        GraphAggregateColumn::Aggregate(at) => row.values.get(at).map(result_cell),
    }
}

impl PreparedGraphAggregate {
    /// ORDER BY or an output page needs a ranked result stage, even when the
    /// visible tuple is otherwise the identity projection. The source stage
    /// must never apply this window before completing aggregates and HAVING.
    #[must_use]
    pub fn has_incremental_ranking(&self) -> bool {
        !self.ordering.is_empty() || self.offset != 0 || self.count.is_some()
    }

    /// Original result window in occurrence units, after output DISTINCT.
    /// None is unbounded; Some(0) is empty but does not suppress upstream errors.
    #[must_use]
    pub fn incremental_result_window(&self) -> (u64, Option<u64>) {
        (self.offset, self.count)
    }
}

impl GraphAggregateRow {
    /// Compare COMPLETE evaluation groups with the snapshot result comparator.
    /// Columns address full grouping keys/aggregates, never projected output.
    /// Null placement is independent of direction; ascending complete keys
    /// break every explicit sort tie. Equal keys and equal sort cells identify
    /// the same rank even if an unreferenced aggregate value changed.
    ///
    /// Both rows must have the same schema. None refuses a missing column or
    /// incompatible width before any early comparison can hide it. This does
    /// not certify source/aggregate correctness. Payloads are borrowed; each
    /// inspected cell and payload comparison uses the caller's control. A
    /// collection storing these keys owns its key-comparison cost contract.
    pub fn compare_incremental_order<E>(
        &self,
        other: &Self,
        ordering: &[GraphAggregateOrder],
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<Ordering>, E> {
        control(GlaExecutionEvent::Work)?;
        if self.keys.len() != other.keys.len() || self.values.len() != other.values.len() {
            return Ok(None);
        }
        // Validate the entire ordering before a decisive earlier key can stop
        // comparison. No unchecked column access is exposed by this API.
        for order in ordering {
            control(GlaExecutionEvent::Work)?;
            if cell(self, order.column).is_none() || cell(other, order.column).is_none() {
                return Ok(None);
            }
        }
        for order in ordering {
            let result = compare_cell(
                cell(self, order.column).expect("ordering columns checked above"),
                cell(other, order.column).expect("ordering columns checked above"),
                order.descending,
                order.nulls,
                control,
            )?;
            if result != Ordering::Equal {
                return Ok(Some(result));
            }
        }
        for (left, right) in self.keys.iter().zip(&other.keys) {
            control(GlaExecutionEvent::Work)?;
            for _ in 0..value_ref(left)
                .payload_units()
                .max(value_ref(right).payload_units())
            {
                control(GlaExecutionEvent::Work)?;
            }
            let result = left.cmp(right);
            if result != Ordering::Equal {
                return Ok(Some(result));
            }
        }
        Ok(Some(Ordering::Equal))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: i64, value: GraphAggregateValue) -> GraphAggregateRow {
        GraphAggregateRow {
            keys: vec![GraphValue::Scalar(CanonicalScalar::Int(key))].into_boxed_slice(),
            values: vec![value].into_boxed_slice(),
        }
    }

    #[test]
    fn maintained_order_keeps_exact_domains_null_placement_and_ascending_ties() {
        let values = [
            GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
            GraphAggregateValue::Average(GraphExactAverage::new(i128::MIN, u64::MAX).unwrap()),
            GraphAggregateValue::Integer(-1),
            GraphAggregateValue::Count(0),
            GraphAggregateValue::Average(GraphExactAverage::new(3, 2).unwrap()),
            GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(2))),
            GraphAggregateValue::Integer(i128::MAX),
        ];
        for descending in [false, true] {
            for nulls in [GraphNullPlacement::First, GraphNullPlacement::Last] {
                let order = [GraphAggregateOrder {
                    column: GraphAggregateColumn::Aggregate(0),
                    descending,
                    nulls,
                }];
                for (i, a) in values.iter().enumerate() {
                    for (j, b) in values.iter().enumerate() {
                        let expected = match (i == 0, j == 0) {
                            (true, true) => Ordering::Equal,
                            (true, false) => {
                                if nulls == GraphNullPlacement::First {
                                    Ordering::Less
                                } else {
                                    Ordering::Greater
                                }
                            }
                            (false, true) => {
                                if nulls == GraphNullPlacement::First {
                                    Ordering::Greater
                                } else {
                                    Ordering::Less
                                }
                            }
                            _ => {
                                if descending {
                                    j.cmp(&i)
                                } else {
                                    i.cmp(&j)
                                }
                            }
                        };
                        assert_eq!(
                            row(0, a.clone())
                                .compare_incremental_order(
                                    &row(0, b.clone()),
                                    &order,
                                    &mut |_| Ok::<_, ()>(()),
                                )
                                .unwrap(),
                            Some(expected)
                        );
                    }
                }
                // Numeric equality must not fall back to result enum tags;
                // DESC reverses only sort cells, not the hidden key tiebreak.
                assert_eq!(
                    row(1, GraphAggregateValue::Count(2))
                        .compare_incremental_order(
                            &row(
                                2,
                                GraphAggregateValue::Average(GraphExactAverage::new(4, 2).unwrap())
                            ),
                            &order,
                            &mut |_| Ok::<_, ()>(()),
                        )
                        .unwrap(),
                    Some(Ordering::Less)
                );
            }
        }
        assert_eq!(
            row(1, GraphAggregateValue::Count(9))
                .compare_incremental_order(
                    &row(1, GraphAggregateValue::Count(1)),
                    &[],
                    &mut |_| Ok::<_, ()>(()),
                )
                .unwrap(),
            Some(Ordering::Equal)
        );
    }

    #[test]
    fn rank_schema_refuses_before_decisive_prefix_and_every_checkpoint_retries() {
        let a = row(1, GraphAggregateValue::Count(1));
        let b = row(2, GraphAggregateValue::Count(2));
        let bad = [
            GraphAggregateOrder::ascending(GraphAggregateColumn::Aggregate(0)),
            GraphAggregateOrder::descending(GraphAggregateColumn::Aggregate(1)),
        ];
        assert_eq!(
            a.compare_incremental_order(&b, &bad, &mut |_| Ok::<_, ()>(()))
                .unwrap(),
            None
        );
        let malformed = GraphAggregateRow {
            keys: Box::new([]),
            values: Box::new([]),
        };
        assert_eq!(
            a.compare_incremental_order(&malformed, &[], &mut |_| Ok::<_, ()>(()))
                .unwrap(),
            None
        );
        let payload = CanonicalScalar::ucs_basic_text(&"x".repeat(257)).unwrap();
        let mut a = a;
        let mut b = b;
        a.values = vec![GraphAggregateValue::Value(GraphValue::Scalar(
            payload.clone(),
        ))]
        .into_boxed_slice();
        b.values = a.values.clone();
        let order = [GraphAggregateOrder::descending(
            GraphAggregateColumn::Aggregate(0),
        )];
        let before = (a.clone(), b.clone());
        let mut calls = 0;
        assert_eq!(
            a.compare_incremental_order(&b, &order, &mut |event| {
                assert_eq!(event, GlaExecutionEvent::Work);
                calls += 1;
                Ok::<_, usize>(())
            })
            .unwrap(),
            Some(Ordering::Less)
        );
        assert!(calls > 5);
        for stop in 1..=calls {
            let mut seen = 0;
            assert_eq!(
                a.compare_incremental_order(&b, &order, &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                }),
                Err(stop)
            );
            assert_eq!(seen, stop);
            assert_eq!((&a, &b), (&before.0, &before.1));
            assert_eq!(
                a.compare_incremental_order(&b, &order, &mut |_| Ok::<_, ()>(()))
                    .unwrap(),
                Some(Ordering::Less)
            );
        }
    }
}
