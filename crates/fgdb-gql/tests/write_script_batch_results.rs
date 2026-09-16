//! Batch coordinates augment, rather than replace, ordinary program outcomes.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryError,
    GraphMutationProgramDimension, GraphMutationProgramError, GraphSymbol, GraphSymbolKind,
    GraphWriteProgramError, GraphWriteProgramReceipt, GraphWriteProgramStats,
    GraphWriteScriptExecutionError, GraphWriteStepReceipt, PreparedGraphWriteScript,
};
use fgdb_types::VId;

type ProgramError = GraphWriteProgramError<&'static str, &'static str, &'static str>;
fn batch() -> fgdb_gql::BoundGraphWriteScriptBatch {
    let script = PreparedGraphWriteScript::prepare(
        "CREATE (n);\n\u{2003}MATCH (n) SET n.p=1", RelationId(1), |kind, name| {
            match (kind, name) {
                (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
                _ => None,
            }
        },
    ).unwrap();
    script.bind_parameter_sets_with_limit(&vec![GqlParameters::new(); 80], 160).unwrap()
}

#[test]
fn each_statement_error_family_keeps_exact_record_coordinates_and_original_source() {
    use GraphMutationProgramError as M;
    use GraphWriteProgramError as W;
    let batch = batch();
    let errors: Vec<ProgramError> = vec![
        W::Insert { statement: 137, source: GqlQueryError::Interrupted("insert") },
        W::VertexMerge { statement: 137, source: GqlQueryError::Interrupted("vertex merge") },
        W::VertexUpsert { statement: 137, source: GqlQueryError::Interrupted("vertex upsert") },
        W::EdgeMerge { statement: 137, source: GqlQueryError::Interrupted("edge merge") },
        W::EdgeUpsert { statement: 137, source: GqlQueryError::Interrupted("edge upsert") },
        W::Delete { statement: 137, source: GqlQueryError::Interrupted("delete") },
        W::CreationBudget { statement: 137, dimension: fgdb_gql::insertion::GraphInsertLimitDimension::Vertices,
            limit: 68, observed: 69 },
        W::Program(M::Statement { statement: 137, source: GqlQueryError::Interrupted("mutation") }),
        W::Program(M::Budget { statement: 137, dimension: GraphMutationProgramDimension::Effects,
            limit: 68, observed: 69 }),
        W::Program(M::InvalidStatistics { statement: 137 }),
        W::Program(M::Interrupted { completed_statements: 137, source: "before step" }),
    ];
    for original in errors {
        let expected = original.to_string();
        let kind = core::mem::discriminant(&original);
        let mapped = batch.execution_error(original);
        assert!(mapped.to_string().contains("argument set 68, statement 1"));
        let GraphWriteScriptExecutionError::BatchProgram { location, source } = mapped else {
            panic!("execution error must not become a binding failure")
        };
        assert_eq!(location, batch.location(137));
        assert_eq!(core::mem::discriminant(&source), kind);
        assert_eq!(source.to_string(), expected);
    }
}

#[test]
fn infrastructure_and_final_acceptance_are_not_assigned_a_fabricated_record() {
    use GraphMutationProgramError as M;
    let batch = batch();
    let errors: Vec<ProgramError> = vec![
        M::Preflight("unknown commit outcome").into(),
        M::Interrupted { completed_statements: 160, source: "final checkpoint" }.into(),
        M::Budget { statement: 160, dimension: GraphMutationProgramDimension::WorkUnits,
            limit: 160, observed: 161 }.into(),
        M::InvalidStatistics { statement: usize::MAX }.into(),
    ];
    for original in errors {
        let expected = original.to_string();
        let GraphWriteScriptExecutionError::BatchProgram { location, source } = batch.execution_error(original)
            else { panic!("wrong error family") };
        assert_eq!(location, None);
        assert_eq!(source.to_string(), expected);
    }
}

fn stats(completed_statements: usize) -> GraphWriteProgramStats {
    GraphWriteProgramStats {
        completed_statements,
        selection: GqlExecutionStats { snapshot_records: 0, result_rows: 0 },
        evaluator: GlaExecutionStats::default(),
        target_vertex_visits: 80,
        mutation_effects: 80,
        created_vertices: 80,
        created_edges: 0,
    }
}

#[test]
fn record_receipts_require_a_complete_shape_and_preserve_order_without_copying() {
    let batch = batch();
    let steps = (0..160).map(|index| {
        let vertex = VId(u128::MAX - index);
        if index % 2 == 0 {
            GraphWriteStepReceipt::Insert { vertices: vec![vertex], edges: vec![] }
        } else {
            GraphWriteStepReceipt::Mutation { targets: vec![vertex] }
        }
    }).collect::<Vec<_>>();
    let receipt = GraphWriteProgramReceipt::new(stats(160), steps.clone());
    let last = batch.record_receipts(&receipt, 79).unwrap();
    assert_eq!(last, &receipt.steps()[158..160]);
    assert!(core::ptr::eq(last.as_ptr(), receipt.steps()[158..].as_ptr()));
    assert_eq!(last[0].created_vertices(), Some(&[VId(u128::MAX - 158)][..]));
    assert_eq!(last[1].mutation_targets(), Some(&[VId(u128::MAX - 159)][..]));
    assert_eq!(batch.record_receipts(&receipt, 80), None);
    assert_eq!(batch.record_receipts(&receipt, usize::MAX), None);
    for malformed in [
        GraphWriteProgramReceipt::new(stats(159), steps[..159].to_vec()),
        GraphWriteProgramReceipt::new(stats(160), steps[..159].to_vec()),
        GraphWriteProgramReceipt::new(stats(159), steps),
    ] {
        assert_eq!(batch.record_receipts(&malformed, 0), None);
    }
    assert!(!format!("{receipt:?}").contains(&u128::MAX.to_string()));
}

#[test]
fn source_chain_preserves_the_original_infrastructure_error() {
    use std::error::Error;
    let original: GraphWriteProgramError<std::io::Error, std::io::Error, std::io::Error> =
        GraphMutationProgramError::Preflight(std::io::Error::other("storage outcome unavailable")).into();
    let mapped = batch().execution_error(original);
    let program = mapped.source().unwrap();
    let mutation = program.source().unwrap();
    let storage = mutation.source().unwrap();
    assert_eq!(storage.to_string(), "storage outcome unavailable");
    assert!(storage.source().is_none());
}
