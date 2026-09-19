//! Post-group projection for incremental consumers of the ordinary GQL plan.
//!
//! Matching, aggregation and HAVING remain upstream. The projection below uses
//! the same expression evaluator as snapshot results. DISTINCT consumers retain
//! complete groups to select the first ranked representative after deletes.

mod ranking;

use super::*;
use core::convert::Infallible;

fn result_cell(value: &GraphAggregateValue) -> Cell<'_> {
    match value {
        GraphAggregateValue::Count(value) => Cell::Count(*value),
        GraphAggregateValue::Integer(value) => Cell::Integer(*value),
        GraphAggregateValue::Average(value) => Cell::Average {
            sum: value.numerator(),
            count: value.denominator(),
        },
        GraphAggregateValue::Value(value) => Cell::Value(value_ref(value)),
    }
}

impl PreparedGraphAggregate {
    /// Whether the final result differs from the complete evaluation groups.
    /// HAVING is upstream of this transformation, not an output projection.
    #[must_use]
    pub fn has_incremental_output_transform(&self) -> bool {
        self.output_projection.is_some()
            || self.key_output.is_some()
            || self.output_aggregates != self.aggregates.len()
            || self.output_distinct
            || self.has_incremental_ranking()
    }

    #[must_use]
    pub fn incremental_output_is_distinct(&self) -> bool {
        self.output_distinct
    }

    /// Split out the complete-group producer once during circuit preparation.
    /// The original definition MUST remain the downstream output owner. Its
    /// projection, DISTINCT, ordering and window execute there, never on source
    /// bindings or incomplete groups. Source, input expressions, aggregates and
    /// HAVING are unchanged. No runtime source is read by this split.
    #[must_use]
    pub fn incremental_source_definition(&self) -> Option<Self> {
        if !self.supports_incremental_input() {
            return None;
        }
        let mut source = self.clone();
        source.key_output = None;
        source.output_aggregates = source.aggregates.len();
        source.output_projection = None;
        source.output_names.clear();
        source.output_distinct = false;
        source.ordering.clear();
        source.offset = 0;
        source.count = None;
        source
            .supports_incremental_maintenance_with_having()
            .then_some(source)
    }

    /// Project one already HAVING-qualified, complete evaluation group. All
    /// referenced columns address the full keys followed by the full summaries.
    /// This does not execute HAVING again, deduplicate, rank or publish rows.
    /// The caller must apply the original ordering/window AFTER projection.
    /// None denotes unsupported definition/schema, never SQL NULL. Exact count,
    /// sum and average cells remain exact, including inside scalar expressions.
    /// Every output expression executes, and errors retain their column/cause.
    pub fn project_incremental_output<C>(
        &self,
        row: &GraphAggregateRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<GraphAggregateRow>, QueryError<Infallible, C>> {
        let mut govern = |event| {
            control(event).map_err(GqlQueryError::<GraphAggregateError<Infallible>, C>::Interrupted)
        };
        govern(GlaExecutionEvent::Work)?;
        if !self.supports_incremental_input()
            || row.keys.len() != self.keys.len()
            || row.values.len() != self.aggregates.len()
        {
            return Ok(None);
        }
        for _ in row.keys.iter() {
            govern(GlaExecutionEvent::Work)?;
        }
        for _ in row.values.iter() {
            govern(GlaExecutionEvent::Work)?;
        }
        if !self.accepts_incremental_row(&row.keys, &row.values) {
            return Ok(None);
        }
        let input = |column: usize| {
            if column < row.keys.len() {
                Ok(Cell::Value(value_ref(&row.keys[column])))
            } else {
                row.values
                    .get(column - row.keys.len())
                    .map(result_cell)
                    .ok_or(GraphIntegerErrorKind::MissingColumn)
            }
        };
        govern(GlaExecutionEvent::ScratchEntry)?;
        let mut keys = Vec::new();
        let mut values = Vec::new();
        if let Some(projection) = &self.output_projection {
            for (column, output) in projection.iter().enumerate() {
                govern(GlaExecutionEvent::ScratchEntry)?;
                values.push(
                    expression(output.value(), input, column, &mut govern)?
                        .into_owned(&mut govern)?,
                );
            }
        } else {
            if let Some(projection) = &self.key_output {
                for &column in &projection.columns {
                    govern(GlaExecutionEvent::Work)?;
                    keys.push(row.keys[column].copy_with_control(&mut govern)?);
                }
            } else {
                for key in &row.keys {
                    govern(GlaExecutionEvent::Work)?;
                    keys.push(key.copy_with_control(&mut govern)?);
                }
            }
            for value in &row.values[..self.output_aggregates] {
                govern(GlaExecutionEvent::Work)?;
                values.push(OutputValue::Borrowed(result_cell(value)).into_owned(&mut govern)?);
            }
        }
        govern(GlaExecutionEvent::Work)?;
        Ok(Some(GraphAggregateRow {
            keys: keys.into_boxed_slice(),
            values: values.into_boxed_slice(),
        }))
    }
}

impl GraphAggregateRow {
    /// An equality key for post-aggregate DISTINCT, NOT a row to release or a
    /// canonical result encoding. Top-level numeric cells share the exact
    /// rational domain used by the snapshot result comparator. In particular,
    /// Count(2), Integer(2), scalar Int(2) and Average(2/1) are equivalent there.
    /// Keep the original projected row separately: the first ranked complete
    /// group chooses its concrete representative. Without explicit ordering,
    /// complete ascending keys break ties. Nested GraphValue equality is unchanged.
    pub fn incremental_distinct_key<E>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        control(GlaExecutionEvent::ScratchEntry)?;
        let mut keys = Vec::new();
        for key in &self.keys {
            control(GlaExecutionEvent::Work)?;
            keys.push(key.copy_with_control(control)?);
        }
        let mut values = Vec::new();
        for value in &self.values {
            control(GlaExecutionEvent::Work)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            let cell = result_cell(value);
            values.push(match cell.numeric() {
                Some((sum, count)) => {
                    // Bound Euclidean reduction before invoking it; no source
                    // or payload-dependent loop is hidden in this fixed charge.
                    for _ in 0..128 {
                        control(GlaExecutionEvent::Work)?;
                    }
                    GraphAggregateValue::Average(
                        GraphExactAverage::new(sum, count)
                            .expect("numeric result cells have positive denominators"),
                    )
                }
                None => OutputValue::Borrowed(cell).into_owned(control)?,
            });
        }
        control(GlaExecutionEvent::Work)?;
        Ok(Self {
            keys: keys.into_boxed_slice(),
            values: values.into_boxed_slice(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algebra::{GraphColumn, GraphPatternBuilder};
    use crate::{
        GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp as Op, GraphSetProjection,
    };
    use fgdb_delta_types::PropertyKeyId;

    fn definition() -> PreparedGraphAggregate {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        let input = builder
            .prepare_values(
                &[
                    GraphColumn::property("group", "n", PropertyKeyId(1)),
                    GraphColumn::property("amount", "n", PropertyKeyId(2)),
                ],
                0,
                None,
            )
            .unwrap()
            .with_duplicates();
        PreparedGraphAggregate::prepare(
            input,
            &[0],
            &[
                GraphAggregate::count_rows("count"),
                GraphAggregate::sum_int("sum", 1),
                GraphAggregate::average_int("average", 1),
            ],
            0,
            None,
        )
        .unwrap()
    }

    #[test]
    fn output_split_preserves_having_and_projects_full_exact_cells() {
        let original = definition()
            .with_result_clauses(
                &[GraphAggregateFilter {
                    column: GraphAggregateColumn::Aggregate(0),
                    test: GraphAggregateTest::IsNotNull,
                }],
                &[],
            )
            .unwrap()
            .with_key_output_columns(&[])
            .unwrap()
            .with_aggregate_output_prefix(0)
            .unwrap()
            .with_distinct_output(true);
        let source = original.incremental_source_definition().unwrap();
        assert_eq!(source.having(), original.having());
        assert!(!source.has_incremental_output_transform());
        assert!(original.incremental_output_is_distinct());
        let row = source
            .materialize_incremental_row(
                vec![GraphValue::Scalar(CanonicalScalar::Int(1))],
                vec![
                    GraphAggregateValue::Count(u64::MAX),
                    GraphAggregateValue::Integer(i128::MAX),
                    GraphAggregateValue::Average(GraphExactAverage::new(3, 2).unwrap()),
                ],
            )
            .unwrap();
        let empty = original
            .project_incremental_output(&row, &mut |_| Ok::<_, ()>(()))
            .unwrap()
            .unwrap();
        assert!(empty.keys().is_empty() && empty.values().is_empty());
        let expressions = original
            .with_output_projection(vec![
                GraphSetProjection::new("count", GraphSetValue::Column(1)),
                GraphSetProjection::new("sum", GraphSetValue::Column(2)),
                GraphSetProjection::new("average", GraphSetValue::Column(3)),
            ])
            .unwrap();
        let projected = expressions
            .project_incremental_output(&row, &mut |_| Ok::<_, ()>(()))
            .unwrap()
            .unwrap();
        assert!(projected.keys().is_empty());
        assert_eq!(projected.values(), row.values());
        let reordered = definition()
            .with_key_output_columns(&[0, 0])
            .unwrap()
            .with_aggregate_output_prefix(1)
            .unwrap();
        let reordered = reordered
            .project_incremental_output(&row, &mut |_| Ok::<_, ()>(()))
            .unwrap()
            .unwrap();
        assert_eq!(
            reordered.keys(),
            &[row.keys()[0].clone(), row.keys()[0].clone()]
        );
        assert_eq!(reordered.values(), &row.values()[..1]);
        let ranked = definition()
            .with_result_clauses(
                &[],
                &[GraphAggregateOrder::ascending(
                    GraphAggregateColumn::Aggregate(0),
                )],
            )
            .unwrap();
        let before = ranked.canonical_bytes();
        assert!(ranked.has_incremental_output_transform() && ranked.has_incremental_ranking());
        let raw = ranked.incremental_source_definition().unwrap();
        assert!(!raw.has_incremental_output_transform());
        assert_eq!(raw.incremental_result_window(), (0, None));
        assert_eq!(ranked.canonical_bytes(), before);
        assert!(
            ranked
                .project_incremental_output(&row, &mut |_| Ok::<_, ()>(()))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn distinct_key_matches_numeric_equivalence_but_preserves_original_variants() {
        let choices = [
            GraphAggregateValue::Count(2),
            GraphAggregateValue::Integer(2),
            GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(2))),
            GraphAggregateValue::Average(GraphExactAverage::new(6, 3).unwrap()),
            GraphAggregateValue::Average(GraphExactAverage::new(3, 2).unwrap()),
            GraphAggregateValue::Integer(i128::MIN),
            GraphAggregateValue::Integer(i128::MAX),
            GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
            GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Bool(true))),
        ];
        for a in &choices {
            for b in &choices {
                let row = |value: GraphAggregateValue| GraphAggregateRow {
                    keys: Box::new([]),
                    values: vec![value].into_boxed_slice(),
                };
                let left = row(a.clone());
                let right = row(b.clone());
                let expected = compare_cell(
                    result_cell(a),
                    result_cell(b),
                    false,
                    GraphNullPlacement::First,
                    &mut |_| Ok::<_, ()>(()),
                )
                .unwrap()
                    == Ordering::Equal;
                assert_eq!(
                    left.incremental_distinct_key(&mut |_| Ok::<_, ()>(()))
                        .unwrap()
                        == right
                            .incremental_distinct_key(&mut |_| Ok::<_, ()>(()))
                            .unwrap(),
                    expected
                );
                assert_eq!(left.values(), std::slice::from_ref(a));
            }
        }
    }

    #[test]
    fn output_failures_and_every_projection_checkpoint_are_explicit_and_retryable() {
        let base = definition();
        let row = base
            .materialize_incremental_row(
                vec![GraphValue::Scalar(CanonicalScalar::Int(1))],
                vec![
                    GraphAggregateValue::Count(2),
                    GraphAggregateValue::Integer(7),
                    GraphAggregateValue::Average(GraphExactAverage::new(7, 2).unwrap()),
                ],
            )
            .unwrap();
        let expression = GraphIntegerExpression::prepare(&[
            Op::Column(2),
            Op::Literal(Some(3)),
            Op::Binary(GraphIntegerBinary::Multiply),
        ])
        .unwrap();
        let query = base
            .clone()
            .with_output_projection(vec![GraphSetProjection::new(
                "scaled",
                GraphSetValue::Integer(expression),
            )])
            .unwrap();
        let mut calls = 0;
        let expected = query
            .project_incremental_output(&row, &mut |_| {
                calls += 1;
                Ok::<_, usize>(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(expected.values(), &[GraphAggregateValue::Integer(21)]);
        let before = query.canonical_bytes();
        for stop in 1..=calls {
            let mut seen = 0;
            assert!(matches!(query.project_incremental_output(&row, &mut |_| {
                seen += 1; if seen == stop { Err(stop) } else { Ok(()) }
            }), Err(GqlQueryError::Interrupted(at)) if at == stop));
            assert_eq!(seen, stop);
            assert_eq!(query.canonical_bytes(), before);
            assert_eq!(
                query
                    .project_incremental_output(&row, &mut |_| Ok::<_, usize>(()))
                    .unwrap()
                    .unwrap(),
                expected
            );
        }
        let invalid = GraphIntegerExpression::prepare(&[
            Op::Column(2),
            Op::Literal(Some(0)),
            Op::Binary(GraphIntegerBinary::Divide),
        ])
        .unwrap();
        let query = base
            .with_output_projection(vec![
                GraphSetProjection::new("ok", GraphSetValue::Column(1)),
                GraphSetProjection::new("invalid", GraphSetValue::Integer(invalid)),
            ])
            .unwrap();
        assert!(matches!(
            query.project_incremental_output(&row, &mut |_| Ok::<_, ()>(())),
            Err(GqlQueryError::Source(
                GraphAggregateError::OutputExpression { column: 1, .. }
            ))
        ));
    }
}
