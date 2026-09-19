//! Computed aggregate contributions from one old/final GLA binding.
//!
//! Source projection is identical to the plain standing path. The GQL owner
//! evaluates all computed columns with its existing scalar VM, then this module
//! feeds the same exact numeric and canonical-value support arrangements. No
//! computed row outlives this call and no graph snapshot is reconstructed.

use super::*;
use fgdb_gql::{GlaExecutionEvent, GqlQueryError, GraphAggregateError};

pub(super) fn contributions<'a>(
    query: &PreparedGraphAggregate,
    mut binding: impl FnMut(u32) -> Result<Option<(VId, &'a VertexState)>, StandingQueryFailure>,
    sign: i128,
    output: &mut Vec<Contribution>,
    meter: &mut Meter<'_>,
) -> Result<(), StandingQueryFailure> {
    let columns = query.input_pattern().value_columns();
    meter.units(ZSetEvent::ScratchEntry, 1 + columns.len())?;
    let mut source = Vec::with_capacity(columns.len());
    for column in columns {
        meter.charge(ZSetEvent::Work)?;
        source.push(projected_value(*column, &mut binding, meter)?);
    }
    // Eager source projection retains all declared dependencies, including
    // operands used only in lazy branches or otherwise unused computed columns.
    // The scalar VM, not this maintainer, decides which CASE/COALESCE arm runs.
    let row = query
        .evaluate_incremental_input(source, &mut |event| {
            meter.charge(ZSetEvent::Work)?;
            match event {
                GlaExecutionEvent::Work => Ok(()),
                GlaExecutionEvent::ScratchEntry => meter.charge(ZSetEvent::ScratchEntry),
                GlaExecutionEvent::ResultRow => Err(StandingQueryFailure::InvalidDelta),
            }
        })
        .map_err(|error| match error {
            GqlQueryError::Interrupted(reason) => reason,
            GqlQueryError::Source(GraphAggregateError::InputExpression {
                column, error, ..
            }) => StandingQueryFailure::InputExpression { column, error },
            _ => StandingQueryFailure::InvalidDelta,
        })?
        .ok_or(StandingQueryFailure::InvalidDelta)?;

    meter.units(ZSetEvent::ScratchEntry, 1 + query.group_key_columns().len())?;
    let mut key = Vec::with_capacity(query.group_key_columns().len());
    for &column in query.group_key_columns() {
        meter.charge(ZSetEvent::Work)?;
        let value = row.get(column).ok_or(StandingQueryFailure::InvalidDelta)?;
        meter.units(ZSetEvent::ScratchEntry, support::value_units(value)?)?;
        key.push(value.clone());
    }
    meter.charge(ZSetEvent::ScratchEntry)?;
    let key: GroupKey = Arc::from(key.into_boxed_slice());
    for (index, aggregate) in query.aggregates().iter().enumerate() {
        meter.charge(ZSetEvent::Work)?;
        let value = match aggregate.argument_column() {
            Some(column) => Some(row.get(column).ok_or(StandingQueryFailure::InvalidDelta)?),
            None => None,
        };
        if support::uses_support(aggregate.function()) {
            let value = value.ok_or(StandingQueryFailure::InvalidDelta)?;
            // Every match keeps the group alive, even when its computed value
            // is NULL. Distinct argument support is tracked independently.
            meter.charge(ZSetEvent::ScratchEntry)?;
            output.push((
                (support::primary(&key, index), None),
                ZWeight::from_i128(sign),
            ));
            if !value.is_null() {
                meter.units(ZSetEvent::ScratchEntry, 2 + support::value_units(value)?)?;
                output.push((
                    (
                        (Arc::clone(&key), index, Some(Arc::new(value.clone()))),
                        Some(0),
                    ),
                    ZWeight::from_i128(sign),
                ));
            }
            continue;
        }
        let value = match value {
            None => Some(0),
            Some(value) if value.is_null() => None,
            Some(_) if aggregate.function() == GraphAggregateFunction::Count => Some(0),
            Some(GraphValue::Scalar(CanonicalScalar::Int(value))) => Some(i128::from(*value)),
            _ => return Err(StandingQueryFailure::NonIntegerSum),
        };
        meter.charge(ZSetEvent::ScratchEntry)?;
        output.push((
            (support::primary(&key, index), value),
            ZWeight::from_i128(sign),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DatabaseKeys, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::RelationId;
    use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, IntegerComparison};
    use fgdb_gql::{
        GraphAggregate, GraphAggregateColumn, GraphAggregateFilter, GraphAggregateTest,
        GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp, GraphSetProjection,
        GraphSetValue,
    };
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    fn definition() -> PreparedGraphAggregate {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        let input = builder
            .prepare_values(
                &[
                    GraphColumn::vertex("id", "n"),
                    GraphColumn::property("group", "n", PropertyKeyId(1)),
                    GraphColumn::property("quantity", "n", PropertyKeyId(2)),
                    GraphColumn::property("price", "n", PropertyKeyId(3)),
                ],
                0,
                None,
            )
            .unwrap()
            .with_duplicates();
        let product = GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Column(2),
            GraphIntegerOp::Column(3),
            GraphIntegerOp::Binary(GraphIntegerBinary::Multiply),
        ])
        .unwrap();
        PreparedGraphAggregate::prepare_projected(
            input,
            vec![
                GraphSetProjection::new("cost", GraphSetValue::Integer(product)),
                GraphSetProjection::new("group", GraphSetValue::Column(1)),
            ],
            &[1],
            &[
                GraphAggregate::min("min", 0),
                GraphAggregate::count_distinct("distinct", 0),
                GraphAggregate::sum_int("sum", 0),
                GraphAggregate::average_int("average", 0),
                GraphAggregate::count_rows("count"),
            ],
            0,
            None,
        )
        .unwrap()
        .with_result_clauses(
            &[GraphAggregateFilter {
                column: GraphAggregateColumn::Aggregate(2),
                test: GraphAggregateTest::Integer {
                    comparison: IntegerComparison::Greater,
                    value: 0,
                },
            }],
            &[],
        )
        .unwrap()
    }

    fn advance(
        query: &mut StandingQuery,
        batch: &LogicalDeltaBatch,
        checkpoint: &mut dyn FnMut() -> Result<(), StandingQueryFailure>,
    ) -> Result<StandingQueryStats, StandingQueryFailure> {
        let mut meter = Meter {
            policy: query.policy,
            stats: StandingQueryStats::default(),
            checkpoint,
        };
        query.maintain(batch, &mut meter)?;
        query.frontier = batch.commit_seq();
        Ok(meter.stats)
    }

    fn seeded(batch: &LogicalDeltaBatch) -> StandingQuery {
        let definition = definition();
        assert!(eligible(&definition));
        let mut query = StandingQuery {
            definition,
            policy: GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000),
            vertices: BTreeMap::new(),
            edges: None,
            aggregate: IncrementalAggregate::new(),
            rows: ZSet::new(),
            frontier: CommitSeq::ORIGIN,
            stats: StandingQueryStats::default(),
            failure: None,
        };
        advance(&mut query, batch, &mut || Ok(())).unwrap();
        query
    }

    fn same(actual: &StandingQuery, expected: &StandingQuery) {
        assert_eq!(actual.vertices, expected.vertices);
        assert_eq!(actual.aggregate, expected.aggregate);
        assert_eq!(actual.rows, expected.rows);
        assert_eq!(actual.frontier, expected.frontier);
        assert_eq!(actual.failure, expected.failure);
    }

    #[test]
    fn every_computed_tick_checkpoint_and_budget_aborts_all_state_then_retries() {
        let ((), report) = run_async_under_lab(0x6a60, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let keys = DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32]);
            let mut db = Database::open_memory(&commit, keys).await.unwrap();
            let mut first = WriteBatch::new(RelationId(1));
            for (id, group, quantity, price) in [(1, 1, 2, 3), (2, 1, 2, 3), (3, 2, 4, 5)] {
                first.create_vertex(
                    VId(id),
                    vec![],
                    vec![
                        (PropertyKeyId(1), CanonicalScalar::Int(group)),
                        (PropertyKeyId(2), CanonicalScalar::Int(quantity)),
                        (PropertyKeyId(3), CanonicalScalar::Int(price)),
                    ],
                );
            }
            let basis = db.write(&commit, first).await.unwrap();
            let initial = db.delta_index().unwrap().get(basis).unwrap().clone();
            let mut next = WriteBatch::new(RelationId(1));
            next.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(2)));
            next.set_vertex_property(VId(1), PropertyKeyId(2), Some(CanonicalScalar::Int(3)));
            next.set_vertex_property(VId(2), PropertyKeyId(2), Some(CanonicalScalar::Int(-2)));
            next.delete_vertex(VId(3));
            let at = db.write(&commit, next).await.unwrap();
            let batch = db.delta_index().unwrap().get(at).unwrap().clone();
            let original = seeded(&initial);
            let mut success = seeded(&initial);
            let mut calls = 0;
            let stats = advance(&mut success, &batch, &mut || {
                calls += 1;
                Ok(())
            })
            .unwrap();
            assert_eq!(success.rows.len(), 1);
            let (row, weight) = success.rows.iter().next().unwrap();
            assert_eq!(weight, &ZWeight::ONE);
            assert_eq!(row.keys(), &[GraphValue::Scalar(CanonicalScalar::Int(2))]);
            assert_eq!(row.get(2).unwrap().as_integer(), Some(9));
            for stop in 1..=calls {
                let mut query = seeded(&initial);
                let mut seen = 0;
                assert_eq!(
                    advance(&mut query, &batch, &mut || {
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
                same(&query, &original);
                advance(&mut query, &batch, &mut || Ok(())).unwrap();
                same(&query, &success);
            }
            for reason in [
                StandingQueryFailure::WorkBudget,
                StandingQueryFailure::ScratchBudget,
                StandingQueryFailure::ResultBudget,
            ] {
                let mut query = seeded(&initial);
                match reason {
                    StandingQueryFailure::WorkBudget => {
                        query.policy.evaluator.max_work_units = stats.work_units - 1
                    }
                    StandingQueryFailure::ScratchBudget => {
                        query.policy.evaluator.max_scratch_entries = stats.scratch_entries - 1
                    }
                    _ => query.policy = GqlQueryPolicy::new(100_000, 0, 10_000_000, 10_000_000),
                }
                assert_eq!(advance(&mut query, &batch, &mut || Ok(())), Err(reason));
                same(&query, &original);
                query.policy = original.policy;
                advance(&mut query, &batch, &mut || Ok(())).unwrap();
                same(&query, &success);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
