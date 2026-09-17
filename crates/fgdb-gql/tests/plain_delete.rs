//! Typed non-detaching DELETE proposal laws. Storage incidence is deliberately
//! tested in fgdb; this crate owns selection, schema, deduplication and budgets.

use fgdb_delta_types::{RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphDeleteBuildError, GraphDeleteError,
    GraphDeletePolicy, GraphPatternTextError, GraphSymbol, GraphSymbolKind, PreparedGraphDelete,
    PreparedGraphText,
};
use fgdb_types::VId;

const R: RelationId = RelationId(1);
type Triple = (VId, RelationId, VId);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn selection(text: &str) -> Result<fgdb_gql::algebra::PreparedGraphPattern<fgdb_gql::algebra::GraphValueRow>, GraphPatternTextError> {
    PreparedGraphText::prepare(text, symbols)?.bind_parameters(&GqlParameters::new())
}
fn deletion(text: &str, targets: Vec<usize>) -> PreparedGraphDelete {
    PreparedGraphDelete::prepare(selection(text).unwrap(), R, targets).unwrap()
}
fn policy(max_targets: u64) -> GraphDeletePolicy {
    GraphDeletePolicy::new(GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000), max_targets)
}
fn execute(
    delete: &PreparedGraphDelete,
    vertices: &[VId],
    edges: &[Triple],
    policy: GraphDeletePolicy,
) -> Result<fgdb_gql::GraphDeleteProposal, GqlQueryError<GraphDeleteError<()>, ()>> {
    delete.execute_governed(
        policy,
        |pattern, allowance| {
            pattern.plan().execute_governed_with_properties(
                (vertices.len() + edges.len()) as u64,
                vertices.iter().copied(),
                edges.iter().copied(),
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                allowance,
                || Ok::<_, ()>(()),
            )
        },
        || Ok::<_, ()>(()),
    )
}

#[test]
fn repeated_match_occurrences_and_target_columns_collapse_to_sorted_vertices() {
    let delete = deletion("MATCH (a)-[:R]->(b) RETURN ALL a,b", vec![0, 1]);
    let edges = [
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
        (VId(2), R, VId(3)),
    ];
    let proposal = execute(&delete, &[VId(1), VId(2), VId(3)], &edges, policy(3)).unwrap();
    assert_eq!(proposal.targets(), &[VId(1), VId(2), VId(3)]);
    assert_eq!(proposal.stats().selection.result_rows, 3);
    assert_eq!(proposal.stats().target_vertices, 3);

    let renamed = deletion("MATCH (x)-[:R]->(y) RETURN ALL x,y", vec![0, 1]);
    assert_eq!(delete.canonical_bytes(), renamed.canonical_bytes());
}

#[test]
fn optional_null_targets_are_ignored_without_inventing_an_identity() {
    let delete = deletion("MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) RETURN ALL b", vec![0]);
    let proposal = execute(
        &delete,
        &[VId(1), VId(2), VId(3)],
        &[(VId(1), R, VId(2))],
        policy(10),
    ).unwrap();
    assert_eq!(proposal.stats().selection.result_rows, 3);
    assert_eq!(proposal.targets(), &[VId(2)]);
}

#[test]
fn target_schema_and_definition_limits_are_checked_before_execution() {
    let scalar = selection("MATCH (a) RETURN a").unwrap();
    assert!(matches!(
        PreparedGraphDelete::prepare(scalar.clone(), R, vec![]),
        Err(GraphDeleteBuildError::EmptyTargets)
    ));
    assert!(matches!(
        PreparedGraphDelete::prepare(scalar.clone(), R, vec![0, 0]),
        Err(GraphDeleteBuildError::TargetColumn { target: 1, column: 0 })
    ));
    let too_many = vec![0; fgdb_gql::MAX_GRAPH_DELETE_TARGETS + 1];
    assert!(matches!(
        PreparedGraphDelete::prepare(scalar, R, too_many),
        Err(GraphDeleteBuildError::TooManyTargets { .. })
    ));
}

#[test]
fn target_quota_is_exact_and_refusal_returns_no_partial_proposal() {
    let delete = deletion("MATCH (a) RETURN ALL a", vec![0]);
    let vertices = [VId(3), VId(1), VId(2)];
    assert!(matches!(
        execute(&delete, &vertices, &[], policy(2)),
        Err(GqlQueryError::Source(GraphDeleteError::TargetLimit { limit: 2, observed: 3 }))
    ));
    assert_eq!(execute(&delete, &vertices, &[], policy(3)).unwrap().targets(), &[VId(1), VId(2), VId(3)]);
}

#[test]
fn exact_work_scratch_and_every_checkpoint_are_enforced() {
    let delete = deletion("MATCH (a)-[:R]->(b) RETURN ALL b", vec![0]);
    let vertices = [VId(1), VId(2), VId(3)];
    let edges = [(VId(1), R, VId(2)), (VId(1), R, VId(2)), (VId(1), R, VId(3))];
    let measured = execute(&delete, &vertices, &edges, policy(10)).unwrap().stats();
    let exact = GraphDeletePolicy::new(
        GqlQueryPolicy::new(
            measured.selection.snapshot_records,
            measured.selection.result_rows,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        ),
        2,
    );
    assert_eq!(execute(&delete, &vertices, &edges, exact).unwrap().stats(), measured);

    for reduced in [
        GraphDeletePolicy::new(GqlQueryPolicy::new(
            measured.selection.snapshot_records, measured.selection.result_rows,
            measured.evaluator.work_units - 1, u64::MAX), 2),
        GraphDeletePolicy::new(GqlQueryPolicy::new(
            measured.selection.snapshot_records, measured.selection.result_rows,
            u64::MAX, measured.evaluator.scratch_entries - 1), 2),
    ] {
        assert!(execute(&delete, &vertices, &edges, reduced).is_err());
    }

    let run = |stop: usize| {
        // One counter observed by both checkpoints: the selection's and the delete's.
        let calls = std::cell::Cell::new(0);
        let result = delete.execute_governed(
            policy(10),
            |pattern, allowance| pattern.plan().execute_governed_with_properties(
                3, vertices, edges, |_, _| Ok::<_, usize>(true), |_, _| Ok(None), allowance,
                || { calls.set(calls.get() + 1); if calls.get() == stop { Err(stop) } else { Ok(()) } },
            ),
            || { calls.set(calls.get() + 1); if calls.get() == stop { Err(stop) } else { Ok(()) } },
        );
        (result, calls.get())
    };
    let (complete, total) = run(0);
    assert_eq!(complete.unwrap().targets(), &[VId(2), VId(3)]);
    for stop in 1..=total {
        let (result, calls) = run(stop);
        assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
        assert_eq!(calls, stop);
    }
}
