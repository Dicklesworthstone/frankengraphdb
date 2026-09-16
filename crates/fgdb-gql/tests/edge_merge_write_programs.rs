//! Relationship MERGE's full source cost, branch validation and prepared
//! parameter contract inside the existing ordered write-program abstraction.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::GraphInsertLimitDimension;
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphEdgeMergeError, GraphEdgeMergeStats, GraphMutationProgramDimension, GraphMutationProgramError,
    GraphSymbol, GraphSymbolKind, GraphWriteProgramError, GraphWriteProgramPolicy,
    GraphWriteProgramTemplateError, GraphWriteStepError, GraphWriteStepStats,
    PreparedGraphEdgeMerge, PreparedGraphEdgeMergeText, PreparedGraphVertexMergeText,
    PreparedGraphWriteProgram, PreparedGraphWriteProgramTemplate,
};
use std::cell::Cell;

type Error = GraphWriteProgramError<&'static str, &'static str, usize>;
const R: RelationId = RelationId(1);
const EDGE: &str = "MATCH (a:Person),(b:Person) WHERE a.p=$left AND b.p=$right MERGE (a)-[:R]->(b)";
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}
fn args() -> GqlParameters {
    GqlParameters::new().with_int64("left", 7).unwrap().with_int64("right", 8).unwrap()
}
fn merge() -> PreparedGraphEdgeMerge {
    PreparedGraphEdgeMergeText::prepare(EDGE, R, symbols).unwrap().bind_parameters(&args()).unwrap()
}
fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(GqlQueryPolicy::new(100, 100, 100, 100), 0, 0, 1)
}
fn stats(created: bool) -> GraphEdgeMergeStats {
    GraphEdgeMergeStats {
        match_selection: GqlExecutionStats { snapshot_records: 2, result_rows: 1 },
        overlay_edges: if created { 3 } else { 4 },
        evaluator: GlaExecutionStats { work_units: if created { 5 } else { 6 }, scratch_entries: 1 },
        created_edges: u64::from(created),
    }
}

#[test]
fn existence_scans_count_toward_one_cumulative_source_budget() {
    let program = PreparedGraphWriteProgram::prepare(vec![merge().into(), merge().into()]).unwrap();
    let exact = GraphWriteProgramPolicy::new(GqlQueryPolicy::new(11, 2, 14, 2), 0, 0, 1);
    let result: Result<_, Error> = program.execute_governed(exact, |statement, _, remaining| {
        assert_eq!(remaining.max_created_edges, u64::from(statement == 0));
        assert_eq!(remaining.edge_merge_policy().max_created_edges, remaining.max_created_edges);
        assert_eq!(remaining.mutations.query.rows.max_snapshot_records(), Some(if statement == 0 { 11 } else { 6 }));
        Ok(GraphWriteStepStats::EdgeMerge(stats(statement == 0)))
    }, || Ok(()));
    let actual = result.unwrap();
    assert_eq!(actual.selection, GqlExecutionStats { snapshot_records: 11, result_rows: 2 });
    assert_eq!(actual.evaluator, GlaExecutionStats { work_units: 14, scratch_entries: 2 });
    assert_eq!((actual.created_vertices, actual.created_edges), (0, 1));
    let limited = GraphWriteProgramPolicy::new(GqlQueryPolicy::new(10, 2, 14, 2), 0, 0, 1);
    let result: Result<_, Error> = program.execute_governed(limited, |statement, _, _| {
        Ok(GraphWriteStepStats::EdgeMerge(stats(statement == 0)))
    }, || Ok(()));
    assert!(matches!(result, Err(GraphWriteProgramError::Program(GraphMutationProgramError::Budget {
        statement: 1, dimension: GraphMutationProgramDimension::SnapshotRecords, limit: 10, observed: 11,
    }))));
}

#[test]
fn combined_match_and_existence_source_count_cannot_wrap() {
    let program = PreparedGraphWriteProgram::prepare(vec![merge().into()]).unwrap();
    let generous = GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX), 0, 0, 1,
    );
    let result: Result<_, Error> = program.execute_governed(generous, |_, _, _| {
        let mut value = stats(true);
        value.match_selection.snapshot_records = u64::MAX;
        value.overlay_edges = 1;
        Ok(GraphWriteStepStats::EdgeMerge(value))
    }, || Ok(()));
    assert!(matches!(result, Err(GraphWriteProgramError::Program(GraphMutationProgramError::Budget {
        statement: 0, dimension: GraphMutationProgramDimension::SnapshotRecords, limit: u64::MAX, observed,
    })) if observed == u128::from(u64::MAX) + 1));
}

#[test]
fn no_input_is_valid_but_impossible_branch_statistics_are_rejected() {
    let program = PreparedGraphWriteProgram::prepare(vec![merge().into()]).unwrap();
    let empty = GraphEdgeMergeStats {
        match_selection: GqlExecutionStats { snapshot_records: 2, result_rows: 0 },
        overlay_edges: 0,
        evaluator: GlaExecutionStats { work_units: 1, scratch_entries: 0 },
        created_edges: 0,
    };
    let result: Result<_, Error> = program.execute_governed(policy(), |_, _, _| {
        Ok(GraphWriteStepStats::EdgeMerge(empty))
    }, || Ok(()));
    assert_eq!(result.unwrap().created_edges, 0);
    for corruption in 0..4 {
        let result: Result<_, Error> = program.execute_governed(policy(), |_, _, _| {
            let mut value = empty;
            match corruption {
                0 => value.overlay_edges = 1,
                1 => value.created_edges = 1,
                2 => value.match_selection.result_rows = 1,
                _ => { value.match_selection.result_rows = 1; value.created_edges = 2; }
            }
            Ok(GraphWriteStepStats::EdgeMerge(value))
        }, || Ok(()));
        assert!(matches!(result, Err(GraphWriteProgramError::Program(
            GraphMutationProgramError::InvalidStatistics { statement: 0 }
        ))));
    }
}

#[test]
fn relationship_creation_refusals_report_whole_program_counts() {
    let program = PreparedGraphWriteProgram::prepare(vec![merge().into(), merge().into()]).unwrap();
    let result: Result<_, Error> = program.execute_governed(policy(), |statement, _, remaining| {
        if statement == 0 {
            Ok(GraphWriteStepStats::EdgeMerge(stats(true)))
        } else {
            assert_eq!(remaining.max_created_edges, 0);
            Err(GraphWriteStepError::EdgeMerge(GqlQueryError::Source(
                GraphEdgeMergeError::CreationLimit { limit: 0, observed: 1 },
            )))
        }
    }, || Ok(()));
    assert!(matches!(result, Err(GraphWriteProgramError::CreationBudget {
        statement: 1, dimension: GraphInsertLimitDimension::Edges, limit: 1, observed: 2,
    })));
}

#[test]
fn relationship_templates_share_arguments_and_preserve_later_bind_errors() {
    let calls = Cell::new(0);
    let mut resolve = |kind, name: &str| { calls.set(calls.get() + 1); symbols(kind, name) };
    let vertex = PreparedGraphVertexMergeText::prepare("MERGE (n:Person {p:$left})", R, &mut resolve).unwrap();
    let edge = PreparedGraphEdgeMergeText::prepare(EDGE, R, &mut resolve).unwrap();
    let resolved = calls.get();
    let template = PreparedGraphWriteProgramTemplate::prepare(vec![vertex.clone().into(), edge.clone().into()]).unwrap();
    assert_eq!(template.parameter_schema().len(), 2);
    let bound = template.bind_parameters(&args()).unwrap();
    let expected = PreparedGraphWriteProgram::prepare(vec![
        vertex.bind_parameters(&GqlParameters::new().with_int64("left", 7).unwrap()).unwrap().into(),
        edge.bind_parameters(&args()).unwrap().into(),
    ]).unwrap();
    assert_eq!(bound, expected);
    assert_eq!(bound, template.bind_parameters(&args()).unwrap());
    assert_eq!(calls.get(), resolved);
    let error = template.bind_parameters(&GqlParameters::new().with_int64("left", 7).unwrap()).unwrap_err();
    assert!(matches!(error, GraphWriteProgramTemplateError::EdgeMergeBind { statement: 1, source }
        if source.offset == EDGE.find("$right").unwrap()));
    assert!(!format!("{template:?} {bound:?}").contains("Person"));
}
