//! Compile ordinary MATCH rows into counted tuples for incremental consumers.
//!
//! The input is the original GLA, not a new matcher. Grouping every projected
//! column retains an exact occurrence count for each complete tuple. A consumer
//! must expand that count (or threshold it for DISTINCT) BEFORE applying the
//! original result window. Executing the carrier as the user query is incorrect.

use crate::algebra::{GlaOperator, GraphValue, GraphValueRow, PreparedGraphPattern,
    ValueProjection, MAX_PATTERN_VERTICES};
use crate::{GlaExecutionEvent, GraphAggregate, GraphAggregateColumn, GraphAggregateOrder,
    GraphAggregateRow, GraphAggregateValue, GraphNullPlacement, PreparedGraphAggregate};

struct Output<'a> {
    projection: usize,
    columns: &'a [ValueProjection],
    order: &'a [super::GraphValueOrder],
    distinct: bool,
    offset: u64,
    count: Option<u64>,
}

impl PreparedGraphPattern<GraphValueRow> {
    fn incremental_rows_output(&self) -> Option<Output<'_>> {
        let ops = self.plan().operators();
        let order_at = ops.len().checked_sub(2)?;
        let (offset, count) = match ops.last()? {
            GlaOperator::Limit { offset, count } => (*offset, *count),
            _ => return None,
        };
        let order = match ops.get(order_at)? {
            GlaOperator::OrderByValues => &[][..],
            GlaOperator::OrderByValueColumns { columns } => columns.as_ref(),
            _ => return None,
        };
        let previous = order_at.checked_sub(1)?;
        let distinct = matches!(ops.get(previous), Some(GlaOperator::Distinct));
        let projection = previous.checked_sub(usize::from(distinct))?;
        let GlaOperator::ProjectValues { columns } = ops.get(projection)? else { return None; };
        // One private COUNT cell uses the existing aggregate schema bound.
        // Hidden sort-carrier columns and collection/captured-element outputs
        // require a separate admitted derivative, never silent truncation.
        if columns.is_empty() || columns.len() >= MAX_PATTERN_VERTICES
            || columns.len() != self.columns().len()
            || columns.iter().any(|column| !matches!(column,
                ValueProjection::Vertex { .. } | ValueProjection::Property { .. }))
            || order.iter().any(|order| order.column >= columns.len())
            || ops[..projection].iter().any(|op| matches!(op,
                GlaOperator::ProjectValues { .. } | GlaOperator::Distinct
                | GlaOperator::OrderByValues | GlaOperator::OrderByValueColumns { .. }
                | GlaOperator::Limit { .. }))
        { return None; }
        Some(Output { projection, columns, order, distinct, offset, count })
    }

    /// Private counted-tuple producer for an incremental row consumer. Every
    /// key is a projected column in original order; the sole COUNT is tuple
    /// multiplicity. There is no global zero row on empty MATCH input.
    ///
    /// The caller MUST retain this original pattern and apply its quantifier,
    /// ordering and window downstream. Source topology is admitted separately
    /// by the standing-query engine; this method is not an authority grant.
    /// Up to MAX_PATTERN_VERTICES - 1 scalar/vertex columns are admitted.
    #[must_use]
    pub fn incremental_row_source_definition(&self) -> Option<PreparedGraphAggregate> {
        let output = self.incremental_rows_output()?;
        let mut input = self.clone();
        input.logical.operators.truncate(output.projection + 1);
        input.logical.operators.extend([
            GlaOperator::OrderByValues,
            GlaOperator::Limit { offset: 0, count: None },
        ]);
        // Aliases are metadata, not execution. Select a deterministic private
        // name that cannot collide with any user-provided projected alias.
        let name = (0..=self.columns().len())
            .map(|at| format!("_fgdb_tuple_count_{at}"))
            .find(|name| !self.columns().contains(name))?;
        let keys: Vec<_> = (0..output.columns.len()).collect();
        PreparedGraphAggregate::prepare(input, &keys, &[GraphAggregate::count_rows(&name)], 0, None).ok()
    }

    /// DISTINCT flag and occurrence-based output window. DISTINCT thresholds
    /// the integrated count, never the signed change or the current page.
    #[must_use]
    pub fn incremental_row_window(&self) -> Option<(bool, u64, Option<u64>)> {
        let output = self.incremental_rows_output()?;
        Some((output.distinct, output.offset, output.count))
    }

    /// Reuse the existing maintained-group comparator on the tuple's keys.
    /// An empty list means canonical complete-tuple order; explicit NULL
    /// placement and direction have the same semantics as ordinary GLA.
    #[must_use]
    pub fn incremental_row_ordering(&self) -> Option<Vec<GraphAggregateOrder>> {
        Some(self.incremental_rows_output()?.order.iter().map(|order| GraphAggregateOrder {
            column: GraphAggregateColumn::GroupKey(order.column),
            descending: order.descending,
            nulls: if order.nulls_first { GraphNullPlacement::First } else { GraphNullPlacement::Last },
        }).collect())
    }

    /// Decode one positive counted tuple into the ordinary value-row domain.
    /// Counts remain u64, never narrowed into scalars. Schema and zero-count
    /// refusal is None; cancellation/resource failures preserve their type.
    /// The caller owns final quotas, retractions and atomic publication.
    pub fn materialize_incremental_values<E>(
        &self,
        counted: &GraphAggregateRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<(GraphValueRow, u64)>, E> {
        control(GlaExecutionEvent::Work)?;
        let Some(output) = self.incremental_rows_output() else { return Ok(None); };
        if counted.keys().len() != output.columns.len() { return Ok(None); }
        let [GraphAggregateValue::Count(count)] = counted.values() else { return Ok(None); };
        if *count == 0 { return Ok(None); }
        // Validate ALL cells before copying any payload, including columns
        // beyond the first decisive sort key or an eventually empty page.
        for (projection, value) in output.columns.iter().zip(counted.keys()) {
            control(GlaExecutionEvent::Work)?;
            let valid = match projection {
                ValueProjection::Property { .. } => matches!(value, GraphValue::Scalar(_)),
                ValueProjection::Vertex { .. } => matches!(value, GraphValue::Vertex(_)) || value.is_null(),
                _ => false,
            };
            if !valid { return Ok(None); }
        }
        control(GlaExecutionEvent::ScratchEntry)?;
        let mut values = Vec::new();
        for value in counted.keys() { values.push(value.copy_with_control(control)?); }
        control(GlaExecutionEvent::Work)?;
        Ok(Some((GraphValueRow::from_owned_values(values), *count)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algebra::{GlaDirection, GraphColumn, GraphPatternBuilder, GraphValueOrder};
    use crate::GqlQueryPolicy;
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_types::{CanonicalScalar, VId};
    use std::cmp::Ordering;

    fn definition(distinct: bool, offset: u64, count: Option<u64>) -> PreparedGraphPattern<GraphValueRow> {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("a").unwrap().vertex("b").unwrap()
            .edge("a", RelationId(1), GlaDirection::Forward, "b").unwrap();
        let pattern = builder.prepare_values(&[
            GraphColumn::property("_fgdb_tuple_count_0", "b", PropertyKeyId(1)),
        ], offset, count).unwrap();
        if distinct { pattern } else { pattern.with_duplicates() }
    }

    #[test]
    fn counted_lowering_preserves_original_plan_and_matches_all_distinct_pages() {
        let amounts = [CanonicalScalar::Int(7), CanonicalScalar::Int(7), CanonicalScalar::Null,
            CanonicalScalar::Bool(true)];
        let edges = [(VId(9), RelationId(1), VId(0)), (VId(9), RelationId(1), VId(0)),
            (VId(9), RelationId(1), VId(1)), (VId(9), RelationId(1), VId(2)),
            (VId(9), RelationId(1), VId(3))];
        let policy = GqlQueryPolicy::new(100, 100, 100_000, 100_000);
        for distinct in [false, true] {
            for offset in [0, 1, 3, u64::MAX] {
                for count in [None, Some(0), Some(1), Some(3)] {
                    for ordered in [false, true] {
                        let pattern = definition(distinct, offset, count);
                        let pattern = if ordered {
                            pattern.with_order_by(&[GraphValueOrder::descending(0).with_nulls_first(true)]).unwrap()
                        } else { pattern };
                        let bytes = pattern.canonical_bytes();
                        let source = pattern.incremental_row_source_definition().unwrap();
                        assert_eq!(pattern.canonical_bytes(), bytes);
                        assert_eq!(pattern.incremental_row_window(), Some((distinct, offset, count)));
                        assert!(source.input_pattern().preserves_duplicates());
                        let mut actual = source.execute_governed(5, [], edges, |_, _| Ok::<_, ()>(true),
                            |vid, _| Ok(Some(&amounts[vid.0 as usize])), policy, || Ok::<_, ()>(())).unwrap().value;
                        let ordering = pattern.incremental_row_ordering().unwrap();
                        actual.sort_by(|a, b| a.compare_incremental_order(b, &ordering,
                            &mut |_| Ok::<_, ()>(())).unwrap().unwrap());
                        let mut expanded = Vec::new();
                        for row in actual {
                            let (row, n) = pattern.materialize_incremental_values(&row,
                                &mut |_| Ok::<_, ()>(())).unwrap().unwrap();
                            let occurrences = if distinct { 1 } else { n };
                            for _ in 0..occurrences { expanded.push(row.clone()); }
                        }
                        let selected: Vec<_> = expanded.into_iter()
                            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
                            .take(count.and_then(|n| usize::try_from(n).ok()).unwrap_or(usize::MAX)).collect();
                        let expected = pattern.plan().execute_governed_with_properties(5, [], edges,
                            |_, _| Ok::<_, ()>(true), |vid, _| Ok(Some(&amounts[vid.0 as usize])),
                            policy, || Ok::<_, ()>(())).unwrap().value;
                        assert_eq!(selected, expected);
                    }
                }
            }
        }
    }

    #[test]
    fn row_materialization_checks_schema_and_every_checkpoint_is_retryable() {
        let pattern = definition(false, 0, None);
        let source = pattern.incremental_row_source_definition().unwrap();
        let row = source.materialize_incremental_row(vec![GraphValue::Scalar(
            CanonicalScalar::bytes(vec![7; 8192]).unwrap())], vec![GraphAggregateValue::Count(u64::MAX)]).unwrap();
        let mut calls = 0;
        let expected = pattern.materialize_incremental_values(&row, &mut |_| {
            calls += 1; Ok::<_, usize>(())
        }).unwrap().unwrap();
        assert_eq!(expected.1, u64::MAX);
        assert!(calls > 128);
        for stop in 1..=calls {
            let mut seen = 0;
            assert_eq!(pattern.materialize_incremental_values(&row, &mut |_| {
                seen += 1; if seen == stop { Err(stop) } else { Ok(()) }
            }), Err(stop));
            assert_eq!(seen, stop);
            assert_eq!(pattern.materialize_incremental_values(&row, &mut |_| Ok::<_, usize>(())).unwrap(), Some(expected.clone()));
        }
        let zero = source.materialize_incremental_row(vec![GraphValue::Scalar(CanonicalScalar::Null)],
            vec![GraphAggregateValue::Count(0)]).unwrap();
        assert_eq!(pattern.materialize_incremental_values(&zero, &mut |_| Ok::<_, ()>(())).unwrap(), None);
        let mut builder = GraphPatternBuilder::new(); builder.vertex("n").unwrap();
        let other = builder.prepare_values(&[GraphColumn::vertex("id", "n")], 0, None).unwrap();
        assert_eq!(other.materialize_incremental_values(&row, &mut |_| Ok::<_, ()>(())).unwrap(), None);
        let a = source.materialize_incremental_row(vec![GraphValue::Scalar(CanonicalScalar::Int(1))],
            vec![GraphAggregateValue::Count(1)]).unwrap();
        let b = source.materialize_incremental_row(vec![GraphValue::Scalar(CanonicalScalar::Int(1))],
            vec![GraphAggregateValue::Count(9)]).unwrap();
        assert_eq!(a.compare_incremental_order(&b, &pattern.incremental_row_ordering().unwrap(),
            &mut |_| Ok::<_, ()>(())).unwrap(), Some(Ordering::Equal));
    }

    #[test]
    fn unsupported_hidden_outputs_and_maximum_width_fail_closed() {
        let mut builder = GraphPatternBuilder::new(); builder.vertex("n").unwrap();
        let hidden = builder.prepare_values(&[GraphColumn::vertex("a", "n"),
            GraphColumn::property("hidden", "n", PropertyKeyId(1))], 0, None).unwrap()
            .with_duplicates().with_visible_columns(1);
        assert!(hidden.incremental_row_source_definition().is_none());
        let names: Vec<_> = (0..MAX_PATTERN_VERTICES).map(|n| format!("c{n}")).collect();
        let columns: Vec<_> = names.iter().map(|name| GraphColumn::vertex(name, "n")).collect();
        let full = builder.prepare_values(&columns, 0, None).unwrap();
        assert!(full.incremental_row_source_definition().is_none());
        let supported = builder.prepare_values(&columns[..MAX_PATTERN_VERTICES - 1], 0, None).unwrap();
        assert!(supported.incremental_row_source_definition().is_some());
    }
}
