//! Larger native ingestion batches use the existing program, not a chunk loop.
//! Callback tests below verify accounting only; database tests own atomicity.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryPolicy,
    GraphMutationProgramBuildError, GraphMutationProgramDimension, GraphMutationProgramError,
    GraphMutationStats, GraphSymbol, GraphSymbolKind, GraphWriteProgramError,
    GraphWriteProgramPolicy, GraphWriteScriptBatchError, GraphWriteStatement, GraphWriteStepStats,
    MAX_GRAPH_MUTATION_STATEMENTS, PreparedGraphWriteProgram, PreparedGraphWriteScript,
};
use std::cell::Cell;

const R: RelationId = RelationId(1);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn arguments(key: i64) -> GqlParameters {
    GqlParameters::new()
        .with_int64("key", key)
        .unwrap()
        .with_int64("value", key + 10)
        .unwrap()
}
fn insertion() -> PreparedGraphWriteScript {
    PreparedGraphWriteScript::prepare("CREATE (n {p:$key,q:$value})", R, symbols).unwrap()
}
fn mutation_batch(records: usize) -> fgdb_gql::BoundGraphWriteScriptBatch {
    let script = PreparedGraphWriteScript::prepare("MATCH (n) SET n.p=$key", R, symbols).unwrap();
    let args = vec![GqlParameters::new().with_int64("key", 7).unwrap(); records];
    script
        .bind_parameter_sets_with_limit(&args, records)
        .unwrap()
}
fn step_stats() -> GraphWriteStepStats {
    GraphWriteStepStats::Mutation(GraphMutationStats {
        selection: GqlExecutionStats {
            snapshot_records: 2,
            result_rows: 1,
        },
        evaluator: GlaExecutionStats {
            work_units: 3,
            scratch_entries: 2,
        },
        target_vertices: 1,
        target_edges: 0,
        effects: 1,
    })
}
fn exact(records: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(2 * records, records, 4 * records + 1, 2 * records),
        records,
        0,
        0,
    )
}

#[test]
fn explicit_large_batch_preserves_record_order_spans_and_frozen_bindings() {
    let text = "CREATE (n {p:$key});\n\u{2003}MATCH (n) WHERE n.p=$key SET n.q=$value";
    let calls = Cell::new(0);
    let script = PreparedGraphWriteScript::prepare(text, R, |kind, name| {
        calls.set(calls.get() + 1);
        symbols(kind, name)
    })
    .unwrap();
    let resolved = calls.get();
    let mut args = (0..80).map(arguments).collect::<Vec<_>>();
    let batch = script.bind_parameter_sets_with_limit(&args, 160).unwrap();
    assert_eq!(batch.argument_sets(), 80);
    assert_eq!(batch.program().statements().len(), 160);
    for (record, values) in args.iter().enumerate() {
        let range = batch.statement_range(record).unwrap();
        let single = script.bind_parameters(values).unwrap();
        assert_eq!(
            &batch.program().statements()[range.clone()],
            single.statements()
        );
        for index in range {
            let location = batch.location(index).unwrap();
            assert_eq!(
                (location.argument_set, location.statement),
                (record, index % 2)
            );
            assert_eq!(location.span, script.statement_span(index % 2).unwrap());
        }
    }
    assert_eq!(batch.statement_range(80), None);
    assert_eq!(batch.statement_range(usize::MAX), None);
    assert_eq!(batch.location(160), None);
    assert_eq!(batch.location(usize::MAX), None);
    assert_eq!(
        calls.get(),
        resolved,
        "batch binding must not call the catalog"
    );
    let frozen = batch.program().canonical_bytes();
    args[79] = arguments(999);
    let changed = script.bind_parameter_sets_with_limit(&args, 160).unwrap();
    assert_ne!(changed.program().canonical_bytes(), frozen);
    assert_eq!(batch.program().canonical_bytes(), frozen);
    assert!(!format!("{batch:?}").contains("$key"));
}

#[test]
fn existing_program_and_batch_defaults_stay_at_sixty_four() {
    let script = insertion();
    let args = vec![arguments(1); 65];
    assert!(matches!(
        script.bind_parameter_sets(&args),
        Err(GraphWriteScriptBatchError::TooManyStatements {
            limit: 64,
            observed: 65
        })
    ));
    let batch = script.bind_parameter_sets_with_limit(&args, 65).unwrap();
    assert_eq!(batch.program().statements().len(), 65);
    assert!(matches!(
        PreparedGraphWriteProgram::prepare(batch.program().statements().to_vec()),
        Err(GraphMutationProgramBuildError::TooManyStatements {
            limit: 64,
            observed: 65
        })
    ));
    let default = script.bind_parameter_sets(&args[..64]).unwrap();
    let explicit = script
        .bind_parameter_sets_with_limit(&args[..64], 65)
        .unwrap();
    assert_eq!(default.program(), explicit.program());
    assert_eq!(
        default.program().canonical_bytes(),
        explicit.program().canonical_bytes()
    );
    assert_eq!(MAX_GRAPH_MUTATION_STATEMENTS, 64);
}

#[test]
fn expanded_count_admission_precedes_binding_and_cannot_bypass_the_hard_ceiling() {
    let script = insertion();
    assert!(matches!(
        script.bind_parameter_sets_with_limit(&[], 0),
        Err(GraphWriteScriptBatchError::Empty)
    ));
    assert!(matches!(
        script.bind_parameter_sets_with_limit(&[GqlParameters::new()], 0),
        Err(GraphWriteScriptBatchError::TooManyStatements {
            limit: 0,
            observed: 1
        })
    ));
    let invalid = vec![GqlParameters::new(); 65];
    assert!(matches!(
        script.bind_parameter_sets_with_limit(&invalid, 64),
        Err(GraphWriteScriptBatchError::TooManyStatements {
            limit: 64,
            observed: 65
        })
    ));
    assert!(matches!(
        script.bind_parameter_sets_with_limit(&invalid, 65),
        Err(GraphWriteScriptBatchError::Arguments {
            argument_set: 0,
            ..
        })
    ));
    let text = vec!["CREATE (n {p:$key})"; 64].join(";");
    let wide_script = PreparedGraphWriteScript::prepare(&text, R, symbols).unwrap();
    let invalid = vec![GqlParameters::new(); 1025];
    assert!(matches!(
        wide_script.bind_parameter_sets_with_limit(&invalid, usize::MAX),
        Err(GraphWriteScriptBatchError::TooManyStatements {
            limit: 65_536,
            observed: 65_600
        })
    ));
    assert_eq!(PreparedGraphWriteScript::MAX_BATCH_STATEMENTS, 65_536);
}

#[test]
fn late_record_binding_failure_keeps_original_utf8_and_statement_coordinates() {
    let text = "CREATE (n {p:$key});\n\u{2003}MATCH (n) WHERE n.p=$key SET n.q=$value";
    let script = PreparedGraphWriteScript::prepare(text, R, symbols).unwrap();
    let mut args = (0..81).map(arguments).collect::<Vec<_>>();
    args[80] = GqlParameters::new().with_int64("key", 80).unwrap();
    let error = script
        .bind_parameter_sets_with_limit(&args, 162)
        .unwrap_err();
    let GraphWriteScriptBatchError::Arguments {
        argument_set,
        source,
    } = error
    else {
        panic!("expected indexed argument failure")
    };
    assert_eq!(argument_set, 80);
    assert_eq!(source.statement, Some(1));
    assert_eq!(source.offset, text.find("$value").unwrap());
    args[80] = arguments(80);
    assert_eq!(
        script
            .bind_parameter_sets_with_limit(&args, 162)
            .unwrap()
            .program()
            .statements()
            .len(),
        162
    );
}

#[test]
fn enlarged_program_uses_one_cumulative_meter_past_record_sixty_four() {
    let records = 129;
    let batch = mutation_batch(records);
    let stats = batch
        .program()
        .execute_governed::<(), (), ()>(
            exact(records as u64),
            |index, input, remaining| {
                assert!(matches!(input, GraphWriteStatement::Mutation(_)));
                let left = (records - index) as u64;
                assert_eq!(remaining.mutations.max_effects, left);
                assert_eq!(
                    remaining.mutations.query,
                    GqlQueryPolicy::new(2 * left, left, 4 * left, 2 * left)
                );
                Ok(step_stats())
            },
            || Ok(()),
        )
        .unwrap();
    assert_eq!(stats.completed_statements, records);
    assert_eq!(
        stats.selection,
        GqlExecutionStats {
            snapshot_records: 258,
            result_rows: 129
        }
    );
    assert_eq!(
        stats.evaluator,
        GlaExecutionStats {
            work_units: 517,
            scratch_entries: 258
        }
    );
    assert_eq!(stats.mutation_effects, 129);
    assert_eq!(stats.target_vertex_visits, 129);
    assert_eq!(stats.created_vertices, 0);
    assert_eq!(stats.created_edges, 0);
}

#[test]
fn enlarged_program_refuses_each_cumulative_dimension_without_resetting_at_record_boundaries() {
    let batch = mutation_batch(129);
    for (policy, dimension, at, expected_limit, expected_observed) in [
        (
            GraphWriteProgramPolicy::new(GqlQueryPolicy::new(257, 129, 517, 258), 129, 0, 0),
            GraphMutationProgramDimension::SnapshotRecords,
            128,
            257,
            258,
        ),
        (
            GraphWriteProgramPolicy::new(GqlQueryPolicy::new(258, 128, 517, 258), 129, 0, 0),
            GraphMutationProgramDimension::SelectedRows,
            128,
            128,
            129,
        ),
        (
            GraphWriteProgramPolicy::new(GqlQueryPolicy::new(258, 129, 516, 258), 129, 0, 0),
            GraphMutationProgramDimension::WorkUnits,
            129,
            516,
            517,
        ),
        (
            GraphWriteProgramPolicy::new(GqlQueryPolicy::new(258, 129, 517, 257), 129, 0, 0),
            GraphMutationProgramDimension::ScratchEntries,
            128,
            257,
            258,
        ),
        (
            GraphWriteProgramPolicy::new(GqlQueryPolicy::new(258, 129, 517, 258), 128, 0, 0),
            GraphMutationProgramDimension::Effects,
            128,
            128,
            129,
        ),
    ] {
        let error = batch
            .program()
            .execute_governed::<(), (), ()>(policy, |_, _, _| Ok(step_stats()), || Ok(()))
            .unwrap_err();
        assert!(
            matches!(error, GraphWriteProgramError::Program(GraphMutationProgramError::Budget {
            statement, dimension: actual, limit, observed,
        }) if statement == at && actual == dimension && limit == expected_limit && observed == expected_observed)
        );
    }
}

#[test]
fn every_large_batch_statement_and_final_boundary_is_interruptible() {
    let batch = mutation_batch(129);
    for stop in 1..=130 {
        let mut checkpoints = 0;
        let mut staged = 0;
        let error = batch
            .program()
            .execute_governed::<(), (), usize>(
                exact(129),
                |index, _, _| {
                    assert_eq!(index, staged);
                    staged += 1;
                    Ok(step_stats())
                },
                || {
                    checkpoints += 1;
                    if checkpoints == stop {
                        Err(stop)
                    } else {
                        Ok(())
                    }
                },
            )
            .unwrap_err();
        assert!(
            matches!(error, GraphWriteProgramError::Program(GraphMutationProgramError::Interrupted {
            completed_statements, source,
        }) if completed_statements == stop - 1 && source == stop)
        );
        assert_eq!(checkpoints, stop);
        assert_eq!(staged, stop - 1);
    }
}
