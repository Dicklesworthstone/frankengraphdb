//! Program-meter and native-template laws for relationship branch actions.
//! Synthetic step statistics exercise admission, not arbitrary callback atomicity.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphEdgeMergeError, GraphEdgeMergeStats, GraphEdgeUpsertBranch, GraphEdgeUpsertError,
    GraphEdgeUpsertStats, GraphMutationProgramDimension, GraphMutationProgramError, GraphSymbol,
    GraphSymbolKind, GraphWriteProgramError, GraphWriteProgramPolicy,
    GraphWriteProgramTemplateError, GraphWriteStepError, GraphWriteStepStats,
    PreparedGraphEdgeUpsertText, PreparedGraphVertexMergeText, PreparedGraphWriteProgram,
    PreparedGraphWriteProgramTemplate,
};
use std::cell::Cell;

type Error = GraphWriteProgramError<&'static str, &'static str, &'static str>;
const R: RelationId = RelationId(1);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "w") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        (GraphSymbolKind::Label, "Person") => {
            Some(GraphSymbol::Label(fgdb_delta_types::LabelId(1)))
        }
        _ => None,
    }
}
fn program(count: usize) -> PreparedGraphWriteProgram {
    let step = PreparedGraphEdgeUpsertText::prepare(
        "MATCH (a),(b) MERGE (a)-[e:R]->(b) ON CREATE SET e.w=1 ON MATCH SET e.w=2",
        R,
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    PreparedGraphWriteProgram::prepare(vec![step.into(); count]).unwrap()
}
fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(GqlQueryPolicy::new(100, 100, 1000, 1000), 100, 0, 10)
}
fn stats(branch: GraphEdgeUpsertBranch) -> GraphEdgeUpsertStats {
    let input = branch != GraphEdgeUpsertBranch::NoInput;
    let actions = u64::from(input);
    GraphEdgeUpsertStats {
        merge: GraphEdgeMergeStats {
            match_selection: GqlExecutionStats {
                snapshot_records: 2,
                result_rows: u64::from(input),
            },
            overlay_edges: u64::from(branch == GraphEdgeUpsertBranch::Match),
            evaluator: GlaExecutionStats {
                work_units: 10,
                scratch_entries: 3,
            },
            created_edges: u64::from(branch == GraphEdgeUpsertBranch::Create),
        },
        branch,
        action_effects: actions,
        evaluator: GlaExecutionStats {
            work_units: 11 + actions,
            scratch_entries: 3 + actions,
        },
    }
}

#[test]
fn cumulative_source_work_actions_and_creation_are_charged_exactly_once() {
    let exact = GraphWriteProgramPolicy::new(GqlQueryPolicy::new(5, 2, 27, 8), 2, 0, 1);
    let result: Result<_, Error> = program(2).execute_governed(
        exact,
        |index, _, remaining| {
            assert_eq!(remaining.edge_upsert_policy().max_actions, 2 - index as u64);
            assert_eq!(
                remaining.edge_upsert_policy().merge.max_created_edges,
                u64::from(index == 0)
            );
            Ok(GraphWriteStepStats::EdgeUpsert(stats(if index == 0 {
                GraphEdgeUpsertBranch::Create
            } else {
                GraphEdgeUpsertBranch::Match
            })))
        },
        || Ok(()),
    );
    let result = result.unwrap();
    assert_eq!(result.completed_statements, 2);
    assert_eq!(
        result.selection,
        GqlExecutionStats {
            snapshot_records: 5,
            result_rows: 2
        }
    );
    assert_eq!(
        result.evaluator,
        GlaExecutionStats {
            work_units: 27,
            scratch_entries: 8
        }
    );
    assert_eq!(
        (
            result.mutation_effects,
            result.created_edges,
            result.target_vertex_visits
        ),
        (2, 1, 0)
    );
    assert_eq!(result.proposed_effects(), 3);
}

#[test]
fn impossible_branch_counts_or_wrapped_totals_are_rejected_before_next_step() {
    for corruption in 0..5 {
        let calls = Cell::new(0);
        let result: Result<_, Error> = program(2).execute_governed(
            policy(),
            |_, _, _| {
                calls.set(calls.get() + 1);
                let mut value = stats(GraphEdgeUpsertBranch::Create);
                match corruption {
                    0 => value.branch = GraphEdgeUpsertBranch::NoInput,
                    1 => value.action_effects = 0,
                    2 => value.evaluator.work_units -= 1,
                    3 => value.evaluator.scratch_entries -= 1,
                    _ => {
                        value.merge.evaluator.work_units = u64::MAX;
                        value.evaluator.work_units = 1;
                    }
                }
                Ok(GraphWriteStepStats::EdgeUpsert(value))
            },
            || Ok(()),
        );
        assert!(matches!(
            result,
            Err(GraphWriteProgramError::Program(
                GraphMutationProgramError::InvalidStatistics { statement: 0 }
            ))
        ));
        assert_eq!(calls.get(), 1);
    }
}

#[test]
fn nested_action_and_creation_refusals_report_global_limits_and_statement() {
    for creation in [false, true] {
        let mut limits = policy();
        limits.mutations.max_effects = 1;
        limits.max_created_edges = 1;
        let calls = Cell::new(0);
        let result: Result<_, Error> = program(3).execute_governed(
            limits,
            |index, _, remaining| {
                calls.set(calls.get() + 1);
                if index == 0 {
                    return Ok(GraphWriteStepStats::EdgeUpsert(stats(
                        GraphEdgeUpsertBranch::Create,
                    )));
                }
                assert_eq!(remaining.mutations.max_effects, 0);
                assert_eq!(remaining.max_created_edges, 0);
                Err(GraphWriteStepError::EdgeUpsert(GqlQueryError::Source(
                    if creation {
                        GraphEdgeUpsertError::Merge(GraphEdgeMergeError::CreationLimit {
                            limit: 0,
                            observed: 1,
                        })
                    } else {
                        GraphEdgeUpsertError::ActionLimit {
                            limit: 0,
                            observed: 1,
                        }
                    },
                )))
            },
            || Ok(()),
        );
        if creation {
            assert!(matches!(
                result,
                Err(GraphWriteProgramError::CreationBudget {
                    statement: 1,
                    dimension: fgdb_gql::insertion::GraphInsertLimitDimension::Edges,
                    limit: 1,
                    observed: 2,
                })
            ));
        } else {
            assert!(matches!(
                result,
                Err(GraphWriteProgramError::Program(
                    GraphMutationProgramError::Budget {
                        statement: 1,
                        dimension: GraphMutationProgramDimension::Effects,
                        limit: 1,
                        observed: 2,
                    }
                ))
            ));
        }
        assert_eq!(calls.get(), 2);
    }
}

#[test]
fn no_input_needs_no_action_or_creation_quota_and_final_interruption_is_not_success() {
    let zero_effects = GraphWriteProgramPolicy::new(GqlQueryPolicy::new(2, 0, 13, 3), 0, 0, 0);
    let result: Result<_, Error> = program(1).execute_governed(
        zero_effects,
        |_, _, _| {
            Ok(GraphWriteStepStats::EdgeUpsert(stats(
                GraphEdgeUpsertBranch::NoInput,
            )))
        },
        || Ok(()),
    );
    let result = result.unwrap();
    assert_eq!(
        (
            result.mutation_effects,
            result.created_edges,
            result.selection.result_rows
        ),
        (0, 0, 0)
    );
    let checkpoints = Cell::new(0);
    let result: Result<_, Error> = program(1).execute_governed(
        zero_effects,
        |_, _, _| {
            Ok(GraphWriteStepStats::EdgeUpsert(stats(
                GraphEdgeUpsertBranch::NoInput,
            )))
        },
        || {
            checkpoints.set(checkpoints.get() + 1);
            if checkpoints.get() == 2 {
                Err("cancelled at acceptance")
            } else {
                Ok(())
            }
        },
    );
    assert!(matches!(
        result,
        Err(GraphWriteProgramError::Program(
            GraphMutationProgramError::Interrupted {
                completed_statements: 1,
                source: "cancelled at acceptance",
            }
        ))
    ));
}

#[test]
fn native_upsert_templates_share_arguments_and_preserve_indexed_bind_errors() {
    let calls = Cell::new(0);
    let mut resolve = |kind, name: &str| {
        calls.set(calls.get() + 1);
        symbols(kind, name)
    };
    let vertex =
        PreparedGraphVertexMergeText::prepare("MERGE (n:Person {p:$key})", R, &mut resolve)
            .unwrap();
    let text =
        "\u{2003}MATCH (a:Person) WHERE a.p=$key MERGE (a)-[e:R]->(a) ON CREATE SET e.w=$value";
    let edge = PreparedGraphEdgeUpsertText::prepare(text, R, &mut resolve).unwrap();
    let before = calls.get();
    let template =
        PreparedGraphWriteProgramTemplate::prepare(vec![vertex.into(), edge.into()]).unwrap();
    assert_eq!(template.parameter_schema().len(), 2);
    let arguments = GqlParameters::new()
        .with_int64("key", 7)
        .unwrap()
        .with_int64("value", 9)
        .unwrap();
    let bound = template.bind_parameters(&arguments).unwrap();
    assert!(matches!(
        bound.statements()[1],
        fgdb_gql::GraphWriteStatement::EdgeUpsert(_)
    ));
    assert_eq!(template.bind_parameters(&arguments).unwrap(), bound);
    assert_eq!(calls.get(), before);
    let error = template
        .bind_parameters(&GqlParameters::new().with_int64("key", 7).unwrap())
        .unwrap_err();
    assert!(
        matches!(error, GraphWriteProgramTemplateError::EdgeUpsertBind { statement: 1, source }
        if source.offset == text.find("$value").unwrap())
    );
}
