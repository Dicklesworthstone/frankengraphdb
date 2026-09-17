//! Ordered programs keep bounded definitions and one allowance across steps.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GlaExecutionStats, GlaLimitDimension, GlaLimitExceeded, GqlBudgetDimension, GqlBudgetExceeded,
    GqlExecutionStats, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphIntegerError,
    GraphIntegerErrorKind, GraphMutationError, GraphMutationPolicy, GraphMutationProgramBuildError,
    GraphMutationProgramDimension as D, GraphMutationProgramError as Error, GraphMutationStats,
    GraphSymbol, GraphSymbolKind, MAX_GRAPH_MUTATION_STATEMENTS, PreparedGraphMutation,
    PreparedGraphMutationProgram, PreparedGraphMutationText,
};
use std::cell::Cell;

fn statement(value: i64, relation: u64) -> PreparedGraphMutation {
    PreparedGraphMutationText::prepare(
        &format!("MATCH (n) SET n.p={value}"),
        RelationId(relation),
        |kind, name| match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        },
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
fn program() -> PreparedGraphMutationProgram {
    PreparedGraphMutationProgram::prepare(vec![statement(1, 1), statement(2, 1)]).unwrap()
}
fn policy() -> GraphMutationPolicy {
    GraphMutationPolicy::new(GqlQueryPolicy::new(9, 5, 15, 7), 5)
}
fn first() -> GraphMutationStats {
    GraphMutationStats {
        selection: GqlExecutionStats {
            snapshot_records: 4,
            result_rows: 2,
        },
        evaluator: GlaExecutionStats {
            work_units: 5,
            scratch_entries: 3,
        },
        target_vertices: 2,
        target_edges: 0,
        effects: 2,
    }
}
fn second() -> GraphMutationStats {
    GraphMutationStats {
        selection: GqlExecutionStats {
            snapshot_records: 5,
            result_rows: 3,
        },
        evaluator: GlaExecutionStats {
            work_units: 7,
            scratch_entries: 4,
        },
        target_vertices: 3,
        target_edges: 0,
        effects: 3,
    }
}

#[test]
fn definition_order_limits_coordinate_and_redaction_are_checked_without_execution() {
    assert!(matches!(
        PreparedGraphMutationProgram::prepare(vec![]),
        Err(GraphMutationProgramBuildError::Empty)
    ));
    let maximum = vec![statement(1, 1); MAX_GRAPH_MUTATION_STATEMENTS];
    let admitted = PreparedGraphMutationProgram::prepare(maximum.clone()).unwrap();
    assert_eq!(admitted.statements().len(), MAX_GRAPH_MUTATION_STATEMENTS);
    let mut too_many = maximum;
    too_many.push(statement(1, 1));
    assert!(matches!(
        PreparedGraphMutationProgram::prepare(too_many),
        Err(GraphMutationProgramBuildError::TooManyStatements { .. })
    ));
    assert!(matches!(
        PreparedGraphMutationProgram::prepare(vec![statement(1, 1), statement(2, 2)]),
        Err(GraphMutationProgramBuildError::MixedRelation { statement: 1 })
    ));
    assert_eq!(program().relation(), RelationId(1));
    let backwards =
        PreparedGraphMutationProgram::prepare(vec![statement(2, 1), statement(1, 1)]).unwrap();
    assert_ne!(program().canonical_bytes(), backwards.canonical_bytes());
    assert_eq!(program(), program().clone());
    assert!(!format!("{:?}", program()).contains("MATCH"));
}

#[test]
fn steps_receive_remaining_allowances_and_the_final_boundary_is_charged() {
    let calls = Cell::new(0);
    let checkpoints = Cell::new(0);
    let result = program()
        .execute_governed(
            policy(),
            |input, remaining| {
                let at = calls.get();
                calls.set(at + 1);
                if at == 0 {
                    assert_eq!(
                        remaining,
                        GraphMutationPolicy::new(GqlQueryPolicy::new(9, 5, 14, 7), 5)
                    );
                    assert_eq!(input, &statement(1, 1));
                    Ok::<_, GqlQueryError<GraphMutationError<()>, ()>>(first())
                } else {
                    assert_eq!(
                        remaining,
                        GraphMutationPolicy::new(GqlQueryPolicy::new(5, 3, 8, 4), 3)
                    );
                    assert_eq!(input, &statement(2, 1));
                    Ok(second())
                }
            },
            || {
                checkpoints.set(checkpoints.get() + 1);
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(calls.get(), 2);
    assert_eq!(checkpoints.get(), 3);
    assert_eq!(result.completed_statements, 2);
    assert_eq!(
        result.selection,
        GqlExecutionStats {
            snapshot_records: 9,
            result_rows: 5
        }
    );
    assert_eq!(
        result.evaluator,
        GlaExecutionStats {
            work_units: 15,
            scratch_entries: 7
        }
    );
    assert_eq!((result.target_vertex_visits, result.effects), (5, 5));
}

#[test]
fn every_dimension_refuses_the_cumulative_total_not_just_an_individual_step() {
    for (dimension, budget) in [
        (
            D::SnapshotRecords,
            GraphMutationPolicy::new(GqlQueryPolicy::new(8, 5, 15, 7), 5),
        ),
        (
            D::SelectedRows,
            GraphMutationPolicy::new(GqlQueryPolicy::new(9, 4, 15, 7), 5),
        ),
        (
            D::WorkUnits,
            GraphMutationPolicy::new(GqlQueryPolicy::new(9, 5, 14, 7), 5),
        ),
        (
            D::ScratchEntries,
            GraphMutationPolicy::new(GqlQueryPolicy::new(9, 5, 15, 6), 5),
        ),
        (
            D::Effects,
            GraphMutationPolicy::new(GqlQueryPolicy::new(9, 5, 15, 7), 4),
        ),
    ] {
        let mut at = 0;
        let result = program().execute_governed(
            budget,
            |_, _| {
                at += 1;
                Ok::<_, GqlQueryError<GraphMutationError<()>, ()>>(if at == 1 {
                    first()
                } else {
                    second()
                })
            },
            || Ok(()),
        );
        assert!(
            matches!(result, Err(Error::Budget { dimension: found, limit, observed, .. })
            if found == dimension && observed == u128::from(limit) + 1)
        );
        assert_eq!(
            at, 2,
            "each step fits by itself; the combined program does not"
        );
    }
}

#[test]
fn inner_refusals_report_original_program_limits_and_keep_exact_failure_positions() {
    for dimension in [
        D::SnapshotRecords,
        D::SelectedRows,
        D::WorkUnits,
        D::ScratchEntries,
        D::Effects,
    ] {
        let mut at = 0;
        let result = program().execute_governed(
            policy(),
            |_, remaining| {
                at += 1;
                if at == 1 {
                    return Ok::<_, GqlQueryError<GraphMutationError<()>, ()>>(first());
                }
                Err(match dimension {
                    D::SnapshotRecords | D::SelectedRows => {
                        let (kind, limit) = if dimension == D::SnapshotRecords {
                            (
                                GqlBudgetDimension::SnapshotRecords,
                                remaining.query.rows.max_snapshot_records().unwrap(),
                            )
                        } else {
                            (
                                GqlBudgetDimension::ResultRows,
                                remaining.query.rows.max_result_rows().unwrap(),
                            )
                        };
                        GqlQueryError::Rows(GqlBudgetExceeded {
                            dimension: kind,
                            limit,
                            observed: limit + 1,
                        })
                    }
                    D::WorkUnits | D::ScratchEntries => {
                        let (kind, limit) = if dimension == D::WorkUnits {
                            (
                                GlaLimitDimension::WorkUnits,
                                remaining.query.evaluator.max_work_units,
                            )
                        } else {
                            (
                                GlaLimitDimension::ScratchEntries,
                                remaining.query.evaluator.max_scratch_entries,
                            )
                        };
                        GqlQueryError::Evaluator(GlaLimitExceeded {
                            dimension: kind,
                            limit,
                            observed: u128::from(limit) + 1,
                        })
                    }
                    D::Effects => GqlQueryError::Source(GraphMutationError::EffectLimit {
                        limit: remaining.max_effects,
                        observed: u128::from(remaining.max_effects) + 1,
                    }),
                })
            },
            || Ok(()),
        );
        let limit = match dimension {
            D::SnapshotRecords => 9,
            D::SelectedRows | D::Effects => 5,
            D::WorkUnits => 15,
            D::ScratchEntries => 7,
        };
        assert!(
            matches!(result, Err(Error::Budget { statement: 1, dimension: found, limit: found_limit, observed })
            if found == dimension && found_limit == limit && observed == u128::from(limit) + 1)
        );
    }
    let mut at = 0;
    let result = program().execute_governed(
        policy(),
        |_, _| {
            at += 1;
            if at == 1 {
                return Ok::<_, GqlQueryError<GraphMutationError<()>, ()>>(first());
            }
            Err(GqlQueryError::Source(GraphMutationError::Arithmetic {
                row: 3,
                action: 0,
                error: GraphIntegerError {
                    instruction: 7,
                    kind: GraphIntegerErrorKind::Overflow,
                },
            }))
        },
        || Ok(()),
    );
    assert!(matches!(
        result,
        Err(Error::Statement {
            statement: 1,
            source: GqlQueryError::Source(GraphMutationError::Arithmetic {
                row: 3,
                error: GraphIntegerError { instruction: 7, .. },
                ..
            })
        })
    ));
}

#[test]
fn every_program_checkpoint_can_refuse_including_after_the_last_stage() {
    for stop in 1..=3 {
        let calls = Cell::new(0);
        let checkpoints = Cell::new(0);
        let result = program().execute_governed(
            policy(),
            |_, _| {
                let at = calls.get();
                calls.set(at + 1);
                Ok::<_, GqlQueryError<GraphMutationError<()>, usize>>(if at == 0 {
                    first()
                } else {
                    second()
                })
            },
            || {
                let at = checkpoints.get() + 1;
                checkpoints.set(at);
                if at == stop { Err(stop) } else { Ok(()) }
            },
        );
        assert!(
            matches!(result, Err(Error::Interrupted { completed_statements, source })
            if completed_statements == stop - 1 && source == stop)
        );
        assert_eq!(calls.get(), stop - 1);
        assert_eq!(checkpoints.get(), stop);
    }
}

#[test]
fn no_match_steps_do_not_short_circuit_and_invalid_or_overflowed_statistics_refuse() {
    let mut at = 0;
    let result = program()
        .execute_governed(
            policy(),
            |_, _| {
                at += 1;
                Ok::<_, GqlQueryError<GraphMutationError<()>, ()>>(if at == 1 {
                    GraphMutationStats {
                        selection: GqlExecutionStats {
                            snapshot_records: 4,
                            result_rows: 0,
                        },
                        evaluator: GlaExecutionStats {
                            work_units: 1,
                            scratch_entries: 0,
                        },
                        target_vertices: 0,
                        target_edges: 0,
                        effects: 0,
                    }
                } else {
                    second()
                })
            },
            || Ok(()),
        )
        .unwrap();
    assert_eq!((at, result.completed_statements, result.effects), (2, 2, 3));
    let mut invalid = first();
    invalid.target_vertices = 3;
    assert!(matches!(
        program().execute_governed(
            policy(),
            |_, _| { Ok::<_, GqlQueryError<GraphMutationError<()>, ()>>(invalid) },
            || Ok(())
        ),
        Err(Error::InvalidStatistics { statement: 0 })
    ));
    let mut at = 0;
    let maximum = GraphMutationPolicy::new(
        GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX),
        u64::MAX,
    );
    let result = program().execute_governed(
        maximum,
        |_, _| {
            at += 1;
            Ok::<_, GqlQueryError<GraphMutationError<()>, ()>>(GraphMutationStats {
                selection: GqlExecutionStats {
                    snapshot_records: if at == 1 { u64::MAX } else { 1 },
                    result_rows: 0,
                },
                evaluator: GlaExecutionStats::default(),
                target_vertices: 0,
                target_edges: 0,
                effects: 0,
            })
        },
        || Ok(()),
    );
    assert!(
        matches!(result, Err(Error::Budget { statement: 1, dimension: D::SnapshotRecords, observed, .. })
        if observed == u128::from(u64::MAX) + 1)
    );
}
