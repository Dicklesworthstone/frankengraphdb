//! Plain DELETE uses the same cumulative program meter as all other writes.
//! These callback tests exercise admission; real workspace tests live in fgdb.
use fgdb_delta_types::RelationId;
use fgdb_gql::{GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryError,
    GqlQueryPolicy, GraphDeleteError, GraphDeleteStats, GraphMutationProgramDimension,
    GraphMutationProgramError, GraphWriteProgramError, GraphWriteProgramPolicy,
    GraphWriteStatement, GraphWriteStepError, GraphWriteStepStats, PreparedGraphDeleteText,
    PreparedGraphMutationText, PreparedGraphWriteProgram};

type Error = GraphWriteProgramError<(), (), ()>;
const R: RelationId = RelationId(1);
fn deletion() -> GraphWriteStatement {
    PreparedGraphDeleteText::prepare("MATCH (n) DELETE n", R, |_, _| None).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap().into()
}
fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(GqlQueryPolicy::new(100, 100, 100, 100), 100, 0, 0)
}
fn stats() -> GraphDeleteStats {
    GraphDeleteStats {
        selection: GqlExecutionStats { snapshot_records: 5, result_rows: 2 },
        evaluator: GlaExecutionStats { work_units: 3, scratch_entries: 2 },
        target_vertices: 1,
    }
}
fn run(policy: GraphWriteProgramPolicy) -> Result<fgdb_gql::GraphWriteProgramStats, Error> {
    PreparedGraphWriteProgram::prepare(vec![deletion(), deletion()]).unwrap()
        .execute_governed(policy, |_, _, _| Ok(GraphWriteStepStats::Delete(stats())), || Ok(()))
}

#[test]
fn delete_totals_and_exact_five_dimension_quotas_are_shared() {
    let total = run(policy()).unwrap();
    assert_eq!(total.completed_statements, 2);
    assert_eq!(total.selection, GqlExecutionStats { snapshot_records: 10, result_rows: 4 });
    assert_eq!(total.evaluator, GlaExecutionStats { work_units: 9, scratch_entries: 4 });
    assert_eq!((total.target_vertex_visits, total.mutation_effects), (2, 2));
    assert_eq!((total.created_vertices, total.created_edges), (0, 0));
    let caps = [10, 4, 9, 4, 2];
    let exact = |caps: [u64; 5]| GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]), caps[4], 0, 0);
    assert_eq!(run(exact(caps)).unwrap(), total);
    for dimension in 0..5 {
        let mut lower = caps;
        lower[dimension] -= 1;
        assert!(matches!(run(exact(lower)), Err(GraphWriteProgramError::Program(
            GraphMutationProgramError::Budget { .. }))));
    }
}

#[test]
fn target_and_incidence_refusals_keep_whole_program_location_and_no_suffix_runs() {
    let program = PreparedGraphWriteProgram::prepare(vec![deletion(); 3]).unwrap();
    for incidence in [false, true] {
        let mut visited = Vec::new();
        let result: Result<_, Error> = program.execute_governed(
            GraphWriteProgramPolicy::new(policy().mutations.query, 1, 0, 0),
            |at, _, remaining| {
                visited.push(at);
                if at == 0 { return Ok(GraphWriteStepStats::Delete(stats())); }
                assert_eq!(remaining.deletion_policy().max_targets, 0);
                Err(GraphWriteStepError::Delete(GqlQueryError::Source(if incidence {
                    GraphDeleteError::IncidentRelationships
                } else { GraphDeleteError::TargetLimit { limit: 0, observed: 1 } })))
            }, || Ok(()));
        assert_eq!(visited, vec![0, 1]);
        if incidence {
            assert!(matches!(result, Err(GraphWriteProgramError::Delete { statement: 1,
                source: GqlQueryError::Source(GraphDeleteError::IncidentRelationships) })));
        } else {
            assert!(matches!(result, Err(GraphWriteProgramError::Program(GraphMutationProgramError::Budget {
                statement: 1, dimension: GraphMutationProgramDimension::Effects, limit: 1, observed: 2,
            }))));
        }
    }
}

#[test]
fn impossible_targets_and_cumulative_source_overflow_are_refused() {
    let program = PreparedGraphWriteProgram::prepare(vec![deletion(); 2]).unwrap();
    let bad: Result<_, Error> = program.execute_governed(policy(), |_, _, _| {
        Ok(GraphWriteStepStats::Delete(GraphDeleteStats { target_vertices: 3, ..stats() }))
    }, || Ok(()));
    assert!(matches!(bad, Err(GraphWriteProgramError::Program(
        GraphMutationProgramError::InvalidStatistics { statement: 0 }))));
    let mut allowance = policy();
    allowance.mutations.query = GqlQueryPolicy::new(u64::MAX, 100, 100, 100);
    let overflow: Result<_, Error> = program.execute_governed(allowance, |at, _, _| {
        let mut item = stats();
        if at == 1 { item.selection.snapshot_records = u64::MAX; }
        Ok(GraphWriteStepStats::Delete(item))
    }, || Ok(()));
    assert!(matches!(overflow, Err(GraphWriteProgramError::Program(GraphMutationProgramError::Budget {
        statement: 1, dimension: GraphMutationProgramDimension::SnapshotRecords, limit: u64::MAX, observed,
    })) if observed == u128::from(u64::MAX) + 5));
}

#[test]
fn empty_delete_continues_and_plain_delete_has_a_distinct_canonical_tag() {
    let program = PreparedGraphWriteProgram::prepare(vec![deletion(); 2]).unwrap();
    let result: Result<_, Error> = program.execute_governed(policy(), |at, _, _| {
        let mut item = stats();
        if at == 0 { item.target_vertices = 0; item.selection.result_rows = 0; }
        Ok(GraphWriteStepStats::Delete(item))
    }, || Ok(()));
    assert_eq!(result.unwrap().mutation_effects, 1);
    let detach = PreparedGraphMutationText::prepare("MATCH (n) DETACH DELETE n", R, |_, _| None)
        .unwrap().bind_parameters(&GqlParameters::new()).unwrap();
    let detach = PreparedGraphWriteProgram::prepare(vec![detach.into()]).unwrap();
    let plain = PreparedGraphWriteProgram::prepare(vec![deletion()]).unwrap();
    assert_ne!(plain.canonical_bytes(), detach.canonical_bytes());
}
