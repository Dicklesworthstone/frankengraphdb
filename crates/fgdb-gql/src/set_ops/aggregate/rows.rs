//! Checked row-domain output for the native aggregate owner (fgdb-ezgeq).
//! This is an output bridge, not a second grouping engine or a WITH parser.

use super::*;
use crate::algebra::GraphValue;
use crate::{GlaExecutionEvent, GraphAggregateValue, GraphIntegerError, GraphIntegerErrorKind};
use fgdb_types::CanonicalScalar;

type ResultRows<E, C> = Result<Vec<GraphValueRow>, GqlQueryError<GraphAggregateError<E>, C>>;

fn conversion_error<E, C>(
    column: usize,
    kind: GraphIntegerErrorKind,
) -> GqlQueryError<GraphAggregateError<E>, C> {
    GqlQueryError::Source(GraphAggregateError::OutputExpression {
        column,
        error: GraphIntegerError {
            instruction: 0,
            kind,
        },
    })
}

/// Both the public selected-output adapter and a relational stage can use this
/// conversion. The caller owns selection order and the one cumulative meter.
/// No partial output escapes on a late incompatible value or control failure.
pub(super) fn convert_rows<E, C>(
    rows: Vec<GraphAggregateRow>,
    control: &mut impl FnMut(
        GlaExecutionEvent,
    ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
) -> ResultRows<E, C> {
    let mut output = Vec::new();
    for row in rows {
        control(GlaExecutionEvent::Work)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        let mut cells = Vec::new();
        for key in row.keys() {
            cells.push(super::super::projection::copy_value(key, control)?);
        }
        for value in row.values() {
            control(GlaExecutionEvent::Work)?;
            let column = cells.len();
            let value = match value {
                GraphAggregateValue::Value(value) => {
                    super::super::projection::copy_value(value, control)?
                }
                GraphAggregateValue::Count(value) => {
                    let value = i64::try_from(*value)
                        .map_err(|_| conversion_error(column, GraphIntegerErrorKind::Overflow))?;
                    control(GlaExecutionEvent::ScratchEntry)?;
                    GraphValue::Scalar(CanonicalScalar::Int(value))
                }
                GraphAggregateValue::Integer(value) => {
                    let value = i64::try_from(*value)
                        .map_err(|_| conversion_error(column, GraphIntegerErrorKind::Overflow))?;
                    control(GlaExecutionEvent::ScratchEntry)?;
                    GraphValue::Scalar(CanonicalScalar::Int(value))
                }
                // As for native list construction, GraphValue has no exact
                // rational variant. Even denominator one must not silently
                // change the aggregate's declared domain. The original exact
                // aggregate API remains available for every such result.
                GraphAggregateValue::Average(_) => {
                    return Err(conversion_error(
                        column,
                        GraphIntegerErrorKind::IncompatibleOperands,
                    ));
                }
            };
            cells.push(value);
        }
        output.push(GraphValueRow::from_owned_values(cells));
    }
    control(GlaExecutionEvent::Work)?;
    Ok(output)
}

impl PreparedGraphSetAggregate {
    /// Execute the complete aggregate and convert its SELECTED output to the
    /// ordinary row domain. Columns are output keys followed by output values,
    /// matching key_columns() followed by aggregate_columns(). Hidden columns
    /// and any native output projection are respected; no graph leaf is rerun.
    ///
    /// Counts and sums require an exact Int64 representation. Out-of-range
    /// values return OutputExpression(Overflow); exact averages return
    /// OutputExpression(IncompatibleOperands), never a rounded/truncated value.
    /// MIN/MAX, COLLECT and nulls keep their native bounded GraphValue domain.
    /// execute_governed() remains the unrestricted exact-result interface.
    ///
    /// Conversion follows this aggregate's own HAVING, output projection and
    /// page. It does not inspect groups that the aggregate did not select. A
    /// relational aggregation stage must therefore convert an unpaginated
    /// aggregate BEFORE applying its downstream filter/order/page.
    ///
    /// The original source/snapshot contract of execute_governed applies.
    /// Conversion and payload copies continue its work/scratch allowance and
    /// checkpoint, without charging the public row count twice. No rows escape
    /// until both aggregate execution and the entire conversion succeed.
    pub fn execute_values_governed<E, C>(
        &self,
        policy: GqlQueryPolicy,
        source: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphAggregateError<E>, C>>
    {
        let execution = self.execute_governed(policy, source, &mut checkpoint)?;
        let mut evaluator = execution.evaluator;
        let value = convert_rows(execution.value, &mut |event| {
            checkpoint().map_err(GqlQueryError::Interrupted)?;
            evaluator
                .charge_event(policy.evaluator, event)
                .map_err(GqlQueryError::Evaluator)
        })?;
        Ok(GqlQueryExecution {
            value,
            rows: execution.rows,
            evaluator,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GraphSetQuantifier, GraphSetValue};
    use core::convert::Infallible;

    fn policy() -> GqlQueryPolicy {
        GqlQueryPolicy::new(0, 100, 1_000_000, 1_000_000)
    }

    fn source(
        _: &PreparedGraphPattern<GraphValueRow>,
        _: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<Infallible, Infallible>> {
        panic!("a values-only relation must not manufacture a graph source")
    }

    fn scalar(value: i64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Int(value))
    }

    fn input(values: &[Option<i64>]) -> PreparedGraphSet {
        PreparedGraphSet::singleton()
            .unwind(
                "x".to_owned(),
                GraphSetValue::List(
                    values
                        .iter()
                        .map(|value| {
                            GraphSetValue::Value(value.map_or_else(
                                || GraphValue::Scalar(CanonicalScalar::Null),
                                scalar,
                            ))
                        })
                        .collect(),
                ),
            )
            .unwrap()
    }

    fn summary(values: &[Option<i64>]) -> PreparedGraphSetAggregate {
        PreparedGraphSetAggregate::prepare(
            input(values),
            &[],
            &[
                GraphAggregate::count_rows("n"),
                GraphAggregate::count("nonnull", 0),
                GraphAggregate::sum_int("total", 0),
                GraphAggregate::collect("items", 0),
            ],
            0,
            None,
        )
        .unwrap()
    }

    #[test]
    fn checked_rows_preserve_nulls_bags_and_collection_order() {
        let query = summary(&[Some(3), None, Some(1), Some(3)]);
        let frozen = query.canonical_bytes();
        let result = query
            .execute_values_governed(policy(), source, || Ok(()))
            .unwrap();
        assert_eq!(result.rows.snapshot_records, 0);
        assert_eq!(result.rows.result_rows, 1);
        assert_eq!(
            result.value[0].values(),
            &[
                scalar(4),
                scalar(3),
                scalar(7),
                GraphValue::List(vec![scalar(3), scalar(1), scalar(3)].into_boxed_slice()),
            ]
        );
        assert_eq!(query.canonical_bytes(), frozen);
    }

    #[test]
    fn empty_global_group_and_empty_grouped_relation_are_distinct() {
        let result = summary(&[])
            .execute_values_governed(policy(), source, || Ok(()))
            .unwrap();
        assert_eq!(
            result.value[0].values(),
            &[
                scalar(0),
                scalar(0),
                GraphValue::Scalar(CanonicalScalar::Null),
                GraphValue::List(Vec::new().into_boxed_slice()),
            ]
        );
        let grouped = PreparedGraphSetAggregate::prepare(
            input(&[]),
            &[0],
            &[GraphAggregate::count_rows("n")],
            0,
            None,
        )
        .unwrap();
        assert!(grouped
            .execute_values_governed(policy(), source, || Ok(()))
            .unwrap()
            .value
            .is_empty());
    }

    #[test]
    fn group_keys_and_native_output_projection_keep_their_schema() {
        let grouped = PreparedGraphSetAggregate::prepare(
            input(&[Some(2), Some(1), Some(2), Some(2)]),
            &[0],
            &[GraphAggregate::count_rows("n")],
            0,
            None,
        )
        .unwrap();
        let result = grouped
            .execute_values_governed(policy(), source, || Ok(()))
            .unwrap();
        assert_eq!(result.value.len(), 2);
        assert_eq!(result.value[0].values(), &[scalar(1), scalar(1)]);
        assert_eq!(result.value[1].values(), &[scalar(2), scalar(3)]);
        let projected = grouped
            .with_output_projection(vec![
                GraphSetProjection::new("count", GraphSetValue::Column(1)),
                GraphSetProjection::new("key", GraphSetValue::Column(0)),
            ])
            .unwrap();
        let result = projected
            .execute_values_governed(policy(), source, || Ok(()))
            .unwrap();
        assert_eq!(result.value[0].values(), &[scalar(1), scalar(1)]);
        assert_eq!(result.value[1].values(), &[scalar(3), scalar(2)]);
        assert_eq!(projected.aggregate_columns(), &["count", "key"]);
    }

    #[test]
    fn wide_sums_and_rationals_refuse_without_changing_exact_results() {
        for values in [[Some(i64::MAX), Some(1)], [Some(i64::MIN), Some(-1)]] {
            let query = summary(&values);
            let exact = query.execute_governed(policy(), source, || Ok(())).unwrap();
            let expected = i128::from(values[0].unwrap()) + i128::from(values[1].unwrap());
            assert_eq!(exact.value[0].values()[2].as_integer(), Some(expected));
            assert!(matches!(
                query.execute_values_governed(policy(), source, || Ok(())),
                Err(GqlQueryError::Source(GraphAggregateError::OutputExpression {
                    column: 2,
                    error: GraphIntegerError { kind: GraphIntegerErrorKind::Overflow, .. },
                }))
            ));
        }
        for values in [[Some(1), Some(2)], [Some(2), Some(2)]] {
            let average = PreparedGraphSetAggregate::prepare(
                input(&values), &[], &[GraphAggregate::average_int("mean", 0)], 0, None,
            )
            .unwrap();
            assert!(average.execute_governed(policy(), source, || Ok(())).unwrap()
                .value[0].values()[0].as_average().is_some());
            assert!(matches!(
                average.execute_values_governed(policy(), source, || Ok(())),
                Err(GqlQueryError::Source(GraphAggregateError::OutputExpression {
                    error: GraphIntegerError {
                        kind: GraphIntegerErrorKind::IncompatibleOperands, ..
                    }, ..
                }))
            ));
        }
    }

    #[test]
    fn conversion_shares_work_scratch_and_does_not_charge_result_rows_twice() {
        let query = summary(&[Some(1), Some(2)]);
        let exact = query.execute_governed(policy(), source, || Ok(())).unwrap();
        let converted = query.execute_values_governed(
            GqlQueryPolicy::new(0, 1, 1_000_000, 1_000_000), source, || Ok(()),
        ).unwrap();
        assert_eq!(converted.rows.result_rows, 1);
        assert!(converted.evaluator.work_units > exact.evaluator.work_units);
        assert!(converted.evaluator.scratch_entries > exact.evaluator.scratch_entries);
        for limited in [
            GqlQueryPolicy::new(0, 1, exact.evaluator.work_units, 1_000_000),
            GqlQueryPolicy::new(0, 1, 1_000_000, exact.evaluator.scratch_entries),
        ] {
            assert!(matches!(
                query.execute_values_governed(limited, source, || Ok(())),
                Err(GqlQueryError::Evaluator(_))
            ));
        }
    }

    #[test]
    fn conversion_keeps_the_same_checkpoint_after_aggregation() {
        let query = summary(&[Some(1), Some(2)]);
        let mut before = 0;
        query.execute_governed(policy(), source, || {
            before += 1;
            Ok(())
        }).unwrap();
        let mut calls = 0;
        let result = query.execute_values_governed(
            policy(),
            |_, _| -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<Infallible, &'static str>> {
                panic!("unexpected source")
            },
            || {
                calls += 1;
                if calls > before { Err("cancel conversion") } else { Ok(()) }
            },
        );
        assert!(matches!(result, Err(GqlQueryError::Interrupted("cancel conversion"))));
    }

    #[test]
    fn compound_input_keeps_bag_semantics_and_source_independence() {
        let relation = input(&[Some(2), None])
            .combine(
                GraphSetOperation::Union,
                GraphSetQuantifier::All,
                input(&[Some(2), Some(3)]),
            )
            .unwrap();
        let query = PreparedGraphSetAggregate::prepare(
            relation, &[], &[GraphAggregate::count_rows("n"), GraphAggregate::sum_int("s", 0)], 0, None,
        ).unwrap();
        assert_eq!(query.execute_values_governed(policy(), source, || Ok(())).unwrap()
            .value[0].values(), &[scalar(4), scalar(7)]);
    }

    #[test]
    fn late_source_failure_is_not_hidden_by_an_empty_left_relation() {
        let pattern = crate::PreparedGraphText::prepare(
            "MATCH (n) RETURN n AS id", |_, _: &str| None,
        ).unwrap().bind_parameters(&crate::GqlParameters::new()).unwrap();
        let relation = PreparedGraphSet::from(pattern.clone()).combine(
            GraphSetOperation::Union,
            GraphSetQuantifier::All,
            PreparedGraphSet::from(pattern),
        ).unwrap();
        let query = PreparedGraphSetAggregate::prepare(
            relation, &[], &[GraphAggregate::count_rows("n")], 0, None,
        ).unwrap();
        let mut calls = 0;
        let result = query.execute_values_governed(
            GqlQueryPolicy::new(2, 1, 1_000_000, 1_000_000),
            |_, remaining| -> Result<
                GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, Infallible>,
            > {
                calls += 1;
                assert_eq!(remaining.rows.max_snapshot_records(), Some(3 - calls));
                if calls == 2 {
                    return Err(GqlQueryError::Source("late-source"));
                }
                Ok(GqlQueryExecution {
                    value: Vec::new(),
                    rows: crate::GqlExecutionStats { snapshot_records: 1, result_rows: 0 },
                    evaluator: crate::GlaExecutionStats::default(),
                })
            },
            || Ok(()),
        );
        assert_eq!(calls, 2);
        assert!(matches!(&result, Err(GqlQueryError::Source(_))));
        assert!(result.unwrap_err().to_string().contains("late-source"));
    }

}
