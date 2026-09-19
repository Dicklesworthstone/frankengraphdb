//! Boolean WHERE over one affected binding, using the shared GLA evaluator.
//!
//! The lookup below contains only the current binding's borrowed vertex states,
//! not a graph snapshot or another expression interpreter. Prepared dependency
//! discovery in the parent retains every hidden Boolean/scalar input property.

use super::*;
use fgdb_gql::GlaExecutionEvent;
use fgdb_gql::algebra::{BoundBooleanExpression, MAX_PATTERN_BINDINGS};

pub(super) fn keeps(
    expression: &BoundBooleanExpression,
    binding: &[(VId, &VertexState)],
    meter: &mut Meter<'_>,
) -> Result<bool, StandingQueryFailure> {
    if binding.len() > MAX_PATTERN_BINDINGS {
        return Err(StandingQueryFailure::InvalidDelta);
    }
    meter.units(ZSetEvent::ScratchEntry, 1 + binding.len())?;
    let mut identities = Vec::with_capacity(binding.len());
    let mut source = BTreeMap::new();
    for &(vid, state) in binding {
        meter.charge(ZSetEvent::Work)?;
        identities.push(Some(vid));
        if let std::collections::btree_map::Entry::Vacant(entry) = source.entry(vid) {
            meter.charge(ZSetEvent::ScratchEntry)?;
            entry.insert(state);
        }
    }
    // Multiple slots may name one vertex (correlations, cycles, self-loops).
    // They borrow the same canonical old OR final source generation supplied
    // by the caller. Never resolve a staged deletion against the old state.
    expression
        .evaluate_vertex_binding(
            &identities,
            &mut |vid, key| {
                source
                    .get(&vid)
                    .map(|state| state.props.get(&key))
                    .ok_or(StandingQueryFailure::InvalidDelta)
            },
            &mut |event| {
                // GLA charges work for every event. Scratch growth consumes
                // both counters, and every charge remains cancellation-aware.
                meter.charge(ZSetEvent::Work)?;
                match event {
                    GlaExecutionEvent::Work => Ok(()),
                    GlaExecutionEvent::ScratchEntry => meter.charge(ZSetEvent::ScratchEntry),
                    GlaExecutionEvent::ResultRow => Err(StandingQueryFailure::InvalidDelta),
                }
            },
        )?
        .ok_or(StandingQueryFailure::InvalidDelta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DatabaseKeys, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::RelationId;
    use fgdb_gql::algebra::{
        GraphBooleanExpression, GraphBooleanOp as Op, GraphBooleanOperand as Arg, GraphColumn,
        GraphPatternBuilder, IntegerComparison,
    };
    use fgdb_gql::{GraphAggregate, GraphIntegerExpression, GraphIntegerOp};
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    fn definition() -> PreparedGraphAggregate {
        let flag =
            GraphIntegerExpression::prepare_scalar(&[GraphIntegerOp::ScalarColumn(0)]).unwrap();
        let columns = [Arg::Property {
            variable: "n",
            key: PropertyKeyId(4),
        }];
        let literal = CanonicalScalar::Int(7);
        let filter = GraphBooleanExpression::prepare(&[
            Op::Compare {
                left: Arg::Property {
                    variable: "n",
                    key: PropertyKeyId(3),
                },
                comparison: IntegerComparison::Equal,
                right: Arg::Literal(&literal),
            },
            Op::Not,
            Op::Expression {
                expression: &flag,
                columns: &columns,
            },
            Op::Or,
        ])
        .unwrap();
        let mut builder = GraphPatternBuilder::new();
        builder
            .vertex("n")
            .unwrap()
            .filter_boolean(&filter)
            .unwrap();
        let input = builder
            .prepare_values(
                &[
                    GraphColumn::property("group", "n", PropertyKeyId(1)),
                    GraphColumn::property("value", "n", PropertyKeyId(2)),
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
                GraphAggregate::min("minimum", 1),
                GraphAggregate::count_distinct("distinct", 1),
                GraphAggregate::sum_int("sum", 1),
            ],
            0,
            None,
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
        assert!(needs_property(&definition, PropertyKeyId(3)));
        assert!(needs_property(&definition, PropertyKeyId(4)));
        assert!(!needs_property(&definition, PropertyKeyId(99)));
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

    fn unchanged(actual: &StandingQuery, expected: &StandingQuery) {
        assert_eq!(actual.vertices, expected.vertices);
        assert_eq!(actual.edges, expected.edges);
        assert_eq!(actual.aggregate, expected.aggregate);
        assert_eq!(actual.rows, expected.rows);
        assert_eq!(actual.frontier, expected.frontier);
        assert_eq!(actual.failure, expected.failure);
    }

    #[test]
    fn hidden_boolean_changes_are_atomic_at_every_checkpoint_and_budget_boundary() {
        let ((), report) = run_async_under_lab(0x6a40, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let keys = DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32]);
            let mut db = Database::open_memory(&commit, keys).await.unwrap();
            let mut initial = WriteBatch::new(RelationId(1));
            for id in 1..=2 {
                initial.create_vertex(
                    VId(id),
                    vec![],
                    vec![
                        (PropertyKeyId(1), CanonicalScalar::Int(id as i64)),
                        (PropertyKeyId(2), CanonicalScalar::Int(id as i64)),
                        (PropertyKeyId(3), CanonicalScalar::Int(7)),
                        (PropertyKeyId(4), CanonicalScalar::Bool(false)),
                    ],
                );
            }
            let basis = db.write(&commit, initial).await.unwrap();
            let initial = db.delta_index().unwrap().get(basis).unwrap().clone();
            let mut next = WriteBatch::new(RelationId(1));
            // Neither changed property is returned or aggregated. One is an
            // ordinary comparison input; the other occurs only in scalar IR.
            next.set_vertex_property(VId(1), PropertyKeyId(3), Some(CanonicalScalar::Int(8)));
            next.set_vertex_property(VId(2), PropertyKeyId(4), Some(CanonicalScalar::Bool(true)));
            let at = db.write(&commit, next).await.unwrap();
            let delta = db.delta_index().unwrap().get(at).unwrap().clone();
            let before = seeded(&initial);
            assert!(before.rows.is_empty());
            let mut success = seeded(&initial);
            let mut calls = 0;
            let stats = advance(&mut success, &delta, &mut || {
                calls += 1;
                Ok(())
            })
            .unwrap();
            assert_eq!(success.rows.len(), 2);
            assert_eq!(stats.affected_vertices, 2);
            assert_eq!(stats.affected_edges, 0);
            for stop in 1..=calls {
                let mut candidate = seeded(&initial);
                let mut seen = 0;
                assert_eq!(
                    advance(&mut candidate, &delta, &mut || {
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
                unchanged(&candidate, &before);
                advance(&mut candidate, &delta, &mut || Ok(())).unwrap();
                unchanged(&candidate, &success);
            }
            for reason in [
                StandingQueryFailure::WorkBudget,
                StandingQueryFailure::ScratchBudget,
                StandingQueryFailure::ResultBudget,
            ] {
                let mut candidate = seeded(&initial);
                match reason {
                    StandingQueryFailure::WorkBudget => {
                        candidate.policy.evaluator.max_work_units =
                            stats.work_units.checked_sub(1).unwrap();
                    }
                    StandingQueryFailure::ScratchBudget => {
                        candidate.policy.evaluator.max_scratch_entries =
                            stats.scratch_entries.checked_sub(1).unwrap();
                    }
                    _ => candidate.policy = GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000),
                }
                assert_eq!(advance(&mut candidate, &delta, &mut || Ok(())), Err(reason));
                unchanged(&candidate, &before);
                candidate.policy = before.policy;
                advance(&mut candidate, &delta, &mut || Ok(())).unwrap();
                unchanged(&candidate, &success);
            }
            let mut irrelevant = WriteBatch::new(RelationId(1));
            irrelevant.set_vertex_property(
                VId(1),
                PropertyKeyId(99),
                Some(CanonicalScalar::Int(100)),
            );
            let at = db.write(&commit, irrelevant).await.unwrap();
            let delta = db.delta_index().unwrap().get(at).unwrap();
            let rows: Vec<_> = success
                .rows
                .iter()
                .map(|(row, weight)| (row.clone(), weight.to_i128()))
                .collect();
            let stats = advance(&mut success, delta, &mut || Ok(())).unwrap();
            assert_eq!(stats.affected_vertices, 0);
            assert_eq!(
                success
                    .rows
                    .iter()
                    .map(|(row, weight)| (row.clone(), weight.to_i128()))
                    .collect::<Vec<_>>(),
                rows
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
