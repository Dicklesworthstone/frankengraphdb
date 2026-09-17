//! The trusted program adapter's MERGE accounting contract, independently of
//! database staging. Atomic rollback itself is tested in the fgdb crate.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::{GraphInsertError, GraphInsertLimitDimension};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphMutationProgramError, GraphSymbol, GraphSymbolKind, GraphVertexMergeError,
    GraphVertexMergeStats, GraphVertexUpsertBranch, GraphVertexUpsertStats, GraphWriteProgramError,
    GraphWriteProgramPolicy, GraphWriteStepError, GraphWriteStepStats,
    PreparedGraphVertexMergeText, PreparedGraphVertexUpsertText, PreparedGraphWriteProgram,
};

type Error = GraphWriteProgramError<&'static str, &'static str, usize>;
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn merge() -> fgdb_gql::PreparedGraphVertexMerge {
    PreparedGraphVertexMergeText::prepare("MERGE (n:Person {p:7})", RelationId(1), symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn upsert() -> fgdb_gql::PreparedGraphVertexUpsert {
    PreparedGraphVertexUpsertText::prepare(
        "MERGE (n:Person {p:7}) ON MATCH SET n.q=9 ON CREATE SET n.q=5",
        RelationId(1),
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
fn stats(created: bool) -> GraphVertexMergeStats {
    GraphVertexMergeStats {
        match_selection: GqlExecutionStats {
            snapshot_records: 2,
            result_rows: u64::from(!created),
        },
        evaluator: GlaExecutionStats {
            work_units: 3,
            scratch_entries: 2,
        },
        created_vertices: u64::from(created),
    }
}
fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(GqlQueryPolicy::new(100, 100, 100, 100), 100, 100, 100)
}

#[test]
fn exact_merge_program_limits_and_remaining_creation_allowance_are_shared() {
    let program = PreparedGraphWriteProgram::prepare(vec![merge().into(), merge().into()]).unwrap();
    let exact = GraphWriteProgramPolicy::new(GqlQueryPolicy::new(4, 1, 9, 4), 0, 1, 0);
    let result: Result<_, Error> = program.execute_governed(
        exact,
        |statement, _, remaining| {
            assert_eq!(remaining.max_created_vertices, u64::from(statement == 0));
            assert_eq!(
                remaining.vertex_merge_policy().max_created_vertices,
                remaining.max_created_vertices
            );
            Ok(GraphWriteStepStats::VertexMerge(stats(statement == 0)))
        },
        || Ok(()),
    );
    let actual = result.unwrap();
    assert_eq!(actual.completed_statements, 2);
    assert_eq!(actual.created_vertices, 1);
    assert_eq!(
        actual.selection,
        GqlExecutionStats {
            snapshot_records: 4,
            result_rows: 1
        }
    );
    assert_eq!(
        actual.evaluator,
        GlaExecutionStats {
            work_units: 9,
            scratch_entries: 4
        }
    );
    assert_eq!(actual.proposed_effects(), 1);
    for dimension in 0..5 {
        let mut caps = [4, 1, 9, 4, 1];
        caps[dimension] -= 1;
        let limited = GraphWriteProgramPolicy::new(
            GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
            0,
            caps[4],
            0,
        );
        let result: Result<_, Error> = program.execute_governed(
            limited,
            |statement, _, _| Ok(GraphWriteStepStats::VertexMerge(stats(statement == 0))),
            || Ok(()),
        );
        assert!(result.is_err(), "dimension {dimension}");
    }
}

#[test]
fn create_upsert_actions_are_metered_even_with_zero_selected_rows() {
    let program = PreparedGraphWriteProgram::prepare(vec![upsert().into()]).unwrap();
    let result: Result<_, Error> = program.execute_governed(
        policy(),
        |_, _, _| {
            Ok(GraphWriteStepStats::VertexUpsert(GraphVertexUpsertStats {
                merge: stats(true),
                branch: GraphVertexUpsertBranch::Create,
                action_effects: 1,
            }))
        },
        || Ok(()),
    );
    let actual = result.unwrap();
    assert_eq!(actual.selection.result_rows, 0);
    assert_eq!(
        (
            actual.created_vertices,
            actual.mutation_effects,
            actual.target_vertex_visits
        ),
        (1, 1, 1)
    );
    assert_eq!(actual.proposed_effects(), 2);
}

#[test]
fn impossible_merge_and_upsert_statistics_refuse_before_later_steps() {
    let program = PreparedGraphWriteProgram::prepare(vec![merge().into(), merge().into()]).unwrap();
    for corruption in 0..2 {
        let mut calls = 0;
        let result: Result<_, Error> = program.execute_governed(
            policy(),
            |_, _, _| {
                calls += 1;
                let mut invalid = stats(true);
                invalid.created_vertices = if corruption == 0 { 2 } else { 0 };
                Ok(GraphWriteStepStats::VertexMerge(invalid))
            },
            || Ok(()),
        );
        assert!(matches!(
            result,
            Err(GraphWriteProgramError::Program(
                GraphMutationProgramError::InvalidStatistics { statement: 0 }
            ))
        ));
        assert_eq!(calls, 1);
    }
    let program = PreparedGraphWriteProgram::prepare(vec![upsert().into()]).unwrap();
    for corruption in 0..2 {
        let result: Result<_, Error> = program.execute_governed(
            policy(),
            |_, _, _| {
                Ok(GraphWriteStepStats::VertexUpsert(GraphVertexUpsertStats {
                    merge: stats(true),
                    branch: if corruption == 0 {
                        GraphVertexUpsertBranch::Match
                    } else {
                        GraphVertexUpsertBranch::Create
                    },
                    action_effects: if corruption == 0 { 1 } else { 0 },
                }))
            },
            || Ok(()),
        );
        assert!(matches!(
            result,
            Err(GraphWriteProgramError::Program(
                GraphMutationProgramError::InvalidStatistics { statement: 0 }
            ))
        ));
    }
}

#[test]
fn nested_merge_creation_refusal_reports_whole_program_budget() {
    let program = PreparedGraphWriteProgram::prepare(vec![merge().into(), merge().into()]).unwrap();
    let limited = GraphWriteProgramPolicy {
        max_created_vertices: 1,
        ..policy()
    };
    let result: Result<_, Error> = program.execute_governed(
        limited,
        |statement, _, remaining| {
            if statement == 0 {
                Ok(GraphWriteStepStats::VertexMerge(stats(true)))
            } else {
                assert_eq!(remaining.max_created_vertices, 0);
                Err(GraphWriteStepError::VertexMerge(GqlQueryError::Source(
                    GraphVertexMergeError::Creation(GraphInsertError::Limit {
                        dimension: GraphInsertLimitDimension::Vertices,
                        limit: 0,
                        observed: 1,
                    }),
                )))
            }
        },
        || Ok(()),
    );
    assert!(matches!(
        result,
        Err(GraphWriteProgramError::CreationBudget {
            statement: 1,
            dimension: GraphInsertLimitDimension::Vertices,
            limit: 1,
            observed: 2,
        })
    ));
}
