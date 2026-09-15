//! Public mixed-program execution uses the ordinary per-statement kernels.
//! These tests prove ordering/admission, not atomicity of arbitrary callbacks;
//! database tests exercise the actual guarded workspace.

use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::insertion::{GraphInsertLimitDimension, GraphInsertRequest};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphMutationProgramBuildError,
    GraphMutationProgramDimension, GraphMutationProgramError, GraphMutationStats,
    GraphSymbol, GraphSymbolKind, GraphWriteIdentityRequest, GraphWriteProgramError,
    GraphWriteProgramPolicy, GraphWriteProgramStats, GraphWriteStatement, GraphWriteStepError,
    GraphWriteStepStats, MAX_GRAPH_MUTATION_STATEMENTS, PreparedGraphInsertText,
    PreparedGraphMutationProgram, PreparedGraphMutationText, PreparedGraphWriteProgram,
};
use fgdb_types::{CanonicalScalar, EId, VId};
use std::cell::{Cell, RefCell};

type Error = GraphWriteProgramError<&'static str, &'static str, usize>;
type StepError = GraphWriteStepError<&'static str, &'static str, usize>;
const R: RelationId = RelationId(1);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}
fn insertion() -> GraphWriteStatement {
    PreparedGraphInsertText::prepare("CREATE (x {p:7})-[:R]->(x)", R, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap().into()
}
fn mutation() -> GraphWriteStatement {
    PreparedGraphMutationText::prepare("MATCH (n) SET n.p=7", R, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap().into()
}
fn program() -> PreparedGraphWriteProgram {
    PreparedGraphWriteProgram::prepare(vec![insertion(), mutation(), insertion()]).unwrap()
}
fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(GqlQueryPolicy::new(100, 100, 1_000_000, 1_000_000), 100, 100, 100)
}
fn source(pattern: &PreparedGraphPattern<GraphValueRow>, allowance: GqlQueryPolicy)
    -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>> {
    pattern.plan().execute_governed_with_properties(2, [VId(1), VId(2)], [],
        |_, _| Ok(true), |_, _| Ok::<Option<&CanonicalScalar>, _>(None), allowance, || Ok(()))
}
fn step(statement: usize, input: &GraphWriteStatement, remaining: GraphWriteProgramPolicy,
    requests: &RefCell<Vec<GraphWriteIdentityRequest>>) -> Result<GraphWriteStepStats, StepError> {
    match input {
        GraphWriteStatement::Mutation(input) => input.execute_governed(remaining.mutations, source, || Ok(()))
            .map(|batch| GraphWriteStepStats::Mutation(batch.stats())).map_err(GraphWriteStepError::Mutation),
        GraphWriteStatement::Insert(input) => input.execute_governed(remaining.insertion_policy(), source,
            |request| {
                requests.borrow_mut().push(GraphWriteIdentityRequest { statement, request });
                Ok(match request {
                    GraphInsertRequest::Vertex { row, vertex } => ElementId::Vertex(VId(
                        1000 + statement as u128 * 100 + row as u128 * 10 + vertex as u128)),
                    GraphInsertRequest::Edge { row, edge } => ElementId::Edge(EId(
                        1000 + statement as u128 * 100 + row as u128 * 10 + edge as u128)),
                })
            }, || Ok(())).map(|batch| GraphWriteStepStats::Insert(batch.stats())).map_err(GraphWriteStepError::Insert),
    }
}
fn run(input: &PreparedGraphWriteProgram, allowance: GraphWriteProgramPolicy) -> Result<GraphWriteProgramStats, Error> {
    let requests = RefCell::new(Vec::new());
    input.execute_governed(allowance, |at, input, remaining| step(at, input, remaining, &requests), || Ok(()))
}

#[test]
fn source_order_scoped_identity_requests_and_cumulative_counts_are_explicit() {
    let requests = RefCell::new(Vec::new());
    let visited = RefCell::new(Vec::new());
    let program = program();
    let frozen = program.canonical_bytes();
    let stats: Result<_, Error> = program.execute_governed(policy(), |at, input, remaining| {
        visited.borrow_mut().push(at);
        step(at, input, remaining, &requests)
    }, || Ok(()));
    let stats = stats.unwrap();
    assert_eq!(*visited.borrow(), vec![0, 1, 2]);
    assert_eq!((stats.completed_statements, stats.selection.snapshot_records, stats.selection.result_rows), (3, 2, 4));
    assert_eq!((stats.target_vertex_visits, stats.mutation_effects, stats.created_vertices, stats.created_edges), (2, 2, 2, 2));
    assert_eq!(stats.proposed_effects(), 6);
    assert_eq!(*requests.borrow(), vec![
        GraphWriteIdentityRequest { statement: 0, request: GraphInsertRequest::Vertex { row: 0, vertex: 0 } },
        GraphWriteIdentityRequest { statement: 0, request: GraphInsertRequest::Edge { row: 0, edge: 0 } },
        GraphWriteIdentityRequest { statement: 2, request: GraphInsertRequest::Vertex { row: 0, vertex: 0 } },
        GraphWriteIdentityRequest { statement: 2, request: GraphInsertRequest::Edge { row: 0, edge: 0 } },
    ]);
    assert_eq!(program.canonical_bytes(), frozen);
    assert!(!format!("{program:?}").contains("CREATE"));
    let reordered = PreparedGraphWriteProgram::prepare(vec![mutation(), insertion(), insertion()]).unwrap();
    assert_ne!(reordered.canonical_bytes(), frozen);
}

#[test]
fn common_accounting_is_identical_to_the_existing_mutation_program() {
    let GraphWriteStatement::Mutation(input) = mutation() else { unreachable!() };
    let old = PreparedGraphMutationProgram::prepare(vec![input.clone(), input.clone()]).unwrap();
    let expected = old.execute_governed(policy().mutations, |input, allowance| {
        input.execute_governed(allowance, source, || Ok(())).map(|batch| batch.stats())
    }, || Ok(())).unwrap();
    let new = PreparedGraphWriteProgram::prepare(vec![input.clone().into(), input.into()]).unwrap();
    let actual = run(&new, policy()).unwrap();
    assert_eq!(actual.selection, expected.selection);
    assert_eq!(actual.evaluator, expected.evaluator);
    assert_eq!(actual.completed_statements, expected.completed_statements);
    assert_eq!(actual.target_vertex_visits, expected.target_vertex_visits);
    assert_eq!(actual.mutation_effects, expected.effects);
    assert_eq!((actual.created_vertices, actual.created_edges), (0, 0));
}

#[test]
fn exact_seven_dimension_limits_and_final_acceptance_refusal_are_cumulative() {
    let program = program();
    let stats = run(&program, policy()).unwrap();
    let exact = GraphWriteProgramPolicy::new(GqlQueryPolicy::new(
        stats.selection.snapshot_records, stats.selection.result_rows,
        stats.evaluator.work_units, stats.evaluator.scratch_entries,
    ), stats.mutation_effects, stats.created_vertices, stats.created_edges);
    assert_eq!(run(&program, exact).unwrap(), stats);
    for dimension in 0..7 {
        let mut caps = [stats.selection.snapshot_records, stats.selection.result_rows,
            stats.evaluator.work_units, stats.evaluator.scratch_entries,
            stats.mutation_effects, stats.created_vertices, stats.created_edges];
        caps[dimension] -= 1;
        let cap = GraphWriteProgramPolicy::new(GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]), caps[4], caps[5], caps[6]);
        assert!(run(&program, cap).is_err(), "dimension {dimension}");
    }
    let cap = GraphWriteProgramPolicy::new(GqlQueryPolicy::new(100, 100, stats.evaluator.work_units - 1, 1_000_000), 100, 100, 100);
    assert!(matches!(run(&program, cap), Err(GraphWriteProgramError::Program(GraphMutationProgramError::Budget {
        statement: 3, dimension: GraphMutationProgramDimension::WorkUnits, ..
    }))));
    let requests = RefCell::new(Vec::new());
    let limited = GraphWriteProgramPolicy { max_created_vertices: 1, ..policy() };
    let result: Result<_, Error> = program.execute_governed(limited,
        |at, input, allowance| step(at, input, allowance, &requests), || Ok(()));
    assert!(matches!(result, Err(GraphWriteProgramError::CreationBudget {
        statement: 2, dimension: GraphInsertLimitDimension::Vertices, limit: 1, observed: 2,
    })));
    assert_eq!(requests.borrow().len(), 2, "second CREATE refuses before allocation");
}

#[test]
fn interruption_including_final_boundary_never_runs_a_later_statement() {
    let program = program();
    for stop in 1..=4 {
        let events = Cell::new(0);
        let visited = Cell::new(0);
        let requests = RefCell::new(Vec::new());
        let result: Result<_, Error> = program.execute_governed(policy(), |at, input, allowance| {
            visited.set(visited.get() + 1);
            step(at, input, allowance, &requests)
        }, || { events.set(events.get() + 1); if events.get() == stop { Err(stop) } else { Ok(()) } });
        assert!(matches!(result, Err(GraphWriteProgramError::Program(GraphMutationProgramError::Interrupted {
            completed_statements, source,
        })) if completed_statements == stop - 1 && source == stop));
        assert_eq!(visited.get(), stop - 1);
    }
}

#[test]
fn wrong_kind_or_inconsistent_creation_statistics_cannot_be_accepted() {
    let program = program();
    for corruption in 0..4 {
        let calls = Cell::new(0);
        let requests = RefCell::new(Vec::new());
        let result: Result<_, Error> = program.execute_governed(policy(), |at, input, allowance| {
            calls.set(calls.get() + 1);
            let GraphWriteStepStats::Insert(mut stats) = step(at, input, allowance, &requests)? else { unreachable!() };
            match corruption {
                0 => stats.created_vertices += 1,
                1 => stats.selection.snapshot_records = 1,
                2 => { stats.created_vertices = 0; stats.created_edges = 0; stats.selection.result_rows = 0; }
                _ => return Ok(GraphWriteStepStats::Mutation(GraphMutationStats {
                    selection: stats.selection, evaluator: stats.evaluator, target_vertices: 0, effects: 0,
                })),
            }
            Ok(GraphWriteStepStats::Insert(stats))
        }, || Ok(()));
        assert!(matches!(result, Err(GraphWriteProgramError::Program(GraphMutationProgramError::InvalidStatistics { statement: 0 }))));
        assert_eq!(calls.get(), 1);
    }
}

#[test]
fn definitions_are_bounded_and_mixed_coordinates_refuse_before_execution() {
    assert!(matches!(PreparedGraphWriteProgram::prepare(vec![]), Err(GraphMutationProgramBuildError::Empty)));
    assert!(matches!(PreparedGraphWriteProgram::prepare(vec![insertion(); MAX_GRAPH_MUTATION_STATEMENTS + 1]),
        Err(GraphMutationProgramBuildError::TooManyStatements { .. })));
    let foreign = PreparedGraphInsertText::prepare("CREATE (x)", RelationId(2), symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap();
    assert!(matches!(PreparedGraphWriteProgram::prepare(vec![mutation(), foreign.into()]),
        Err(GraphMutationProgramBuildError::MixedRelation { statement: 1 })));
}
