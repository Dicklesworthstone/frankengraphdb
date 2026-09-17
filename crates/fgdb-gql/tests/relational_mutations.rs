//! Ranked row pipelines feed the existing simultaneous assignment reducer.
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryError, GqlQueryExecution,
    GqlQueryPolicy, GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp,
    GraphMutationAction as Action, GraphMutationBatch, GraphMutationBuildError,
    GraphMutationError, GraphMutationIntent, GraphMutationPolicy, GraphMutationValue as Value,
    GraphSetExecutionError, GraphSetOperation, GraphSetQuantifier, GraphSymbol, GraphSymbolKind,
    PreparedGraphMutation, PreparedGraphSet, PreparedGraphSetText, PreparedGraphText,
    MAX_GRAPH_SET_DEPTH,
};
use fgdb_types::{CanonicalScalar, VId};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
type Edge = (VId, RelationId, VId);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn relation(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn wide() -> GraphMutationPolicy {
    GraphMutationPolicy::new(GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000), 1_000)
}
fn set(target: usize, value: usize) -> Action {
    Action::SetProperty { target, key: Q, value: Value::Column(value) }
}
fn execute<C>(mutation: &PreparedGraphMutation, values: &[CanonicalScalar], edges: &[Edge],
    policy: GraphMutationPolicy, mut checkpoint: impl FnMut() -> Result<(), C>)
    -> Result<GraphMutationBatch, GqlQueryError<GraphMutationError<()>, C>> {
    let checkpoint = std::cell::RefCell::new(&mut checkpoint);
    mutation.execute_governed(policy, |pattern, remaining| {
        pattern.plan().execute_governed_with_properties((values.len() + edges.len()) as u64,
            (0..values.len()).map(|i| VId(i as u128)), edges.iter().copied(),
            |_, _| Ok::<_, ()>(true),
            |vid, key| Ok(if key == P { values.get(vid.0 as usize) } else { None }),
            remaining, || (*checkpoint.borrow_mut())())
    }, || (*checkpoint.borrow_mut())())
}
fn run(mutation: &PreparedGraphMutation, values: &[CanonicalScalar], edges: &[Edge],
    policy: GraphMutationPolicy) -> GraphMutationBatch {
    execute(mutation, values, edges, policy, || Ok::<_, ()>(())).unwrap()
}

#[test]
fn ranked_computed_columns_are_the_actual_assignment_schema() {
    let input = relation("MATCH (n) WITH n AS target,n.p AS x WHERE x>0 RETURN x+1 AS value,target ORDER BY value DESC LIMIT 2");
    let leaf = input.clone();
    let mutation = PreparedGraphMutation::prepare_relation(input, R, vec![set(1, 0)]).unwrap();
    assert_eq!(mutation.input_relation(), Some(&leaf));
    let values = [-1, 2, 7, 4].map(CanonicalScalar::Int);
    let result = run(&mutation, &values, &[], GraphMutationPolicy::new(
        GqlQueryPolicy::new(4, 2, 100_000, 100_000), 2));
    assert_eq!(result.intents(), &[
        GraphMutationIntent::Property { vertex: VId(2), key: Q, value: Some(CanonicalScalar::Int(8)) },
        GraphMutationIntent::Property { vertex: VId(3), key: Q, value: Some(CanonicalScalar::Int(5)) },
    ]);
    assert_eq!(result.stats().selection.result_rows, 2);
    assert_eq!(result.stats().effects, 2);
}

#[test]
fn generated_filter_rank_pages_match_independent_target_value_oracle() {
    for code in 0..81 {
        let mut digits = code;
        let mut values = Vec::new();
        for _ in 0..4 {
            values.push([-2, 0, 5][digits % 3]); digits /= 3;
        }
        for skip in 0..3 {
            for count in 0..3 {
                let input = relation(&format!("MATCH (n) WITH n AS target,n.p AS x WHERE x>=0 RETURN target,x ORDER BY x DESC SKIP {skip} LIMIT {count}"));
                let mutation = PreparedGraphMutation::prepare_relation(input, R, vec![set(0, 1)]).unwrap();
                let mut expected = values.iter().copied().enumerate().filter(|(_, x)| *x >= 0)
                    .collect::<Vec<_>>();
                expected.sort_by_key(|(target, value)| (std::cmp::Reverse(*value), *target));
                let mut expected = expected.into_iter().skip(skip).take(count).collect::<Vec<_>>();
                expected.sort_by_key(|(target, _)| *target);
                let expected = expected.into_iter().map(|(target, value)| GraphMutationIntent::Property {
                    vertex: VId(target as u128), key: Q, value: Some(CanonicalScalar::Int(value)),
                }).collect::<Vec<_>>();
                let scalars = values.iter().copied().map(CanonicalScalar::Int).collect::<Vec<_>>();
                assert_eq!(run(&mutation, &scalars, &[], wide()).intents(), expected);
            }
        }
    }
}

#[test]
fn duplicate_rows_collapse_equal_assignments_but_refuse_conflicting_values() {
    let input = relation("MATCH (a)-[:R]->(b) WITH a AS target,b.p AS x RETURN target,x");
    let mutation = PreparedGraphMutation::prepare_relation(input, R, vec![set(0, 1)]).unwrap();
    let edges = [(VId(0), R, VId(1)), (VId(0), R, VId(1)), (VId(0), R, VId(2))];
    let same = [0, 7, 7].map(CanonicalScalar::Int);
    let result = run(&mutation, &same, &edges, wide());
    assert_eq!(result.stats().selection.result_rows, 3);
    assert_eq!(result.stats().effects, 1);
    let different = [0, 7, 9].map(CanonicalScalar::Int);
    assert!(matches!(execute(&mutation, &different, &edges, wide(), || Ok::<_, ()>(())),
        Err(GqlQueryError::Source(GraphMutationError::ConflictingAssignment { .. }))));
}

#[test]
fn null_targets_do_not_execute_rhs_and_detach_uses_the_selected_vertex() {
    let division = GraphIntegerExpression::prepare(&[
        GraphIntegerOp::Literal(Some(1)), GraphIntegerOp::Literal(Some(0)),
        GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
    ]).unwrap();
    let input = relation("MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) WITH b AS target RETURN target");
    let mutation = PreparedGraphMutation::prepare_relation(input, R, vec![Action::SetProperty {
        target: 0, key: Q, value: Value::Expression(division),
    }]).unwrap();
    assert!(run(&mutation, &[CanonicalScalar::Int(4)], &[], wide()).intents().is_empty());
    let input = relation("MATCH (a) WITH a AS target,a.p AS x ORDER BY x DESC LIMIT 1 RETURN target");
    let deletion = PreparedGraphMutation::prepare_relation(input, R, vec![Action::DetachDelete { target: 0 }]).unwrap();
    assert_eq!(run(&deletion, &[CanonicalScalar::Int(2), CanonicalScalar::Int(9)], &[], wide()).intents(),
        &[GraphMutationIntent::DetachDelete { vertex: VId(1) }]);
}

#[test]
fn all_phases_share_limits_and_every_interruption_returns_no_batch() {
    let mutation = PreparedGraphMutation::prepare_relation(
        relation("MATCH (n) WITH n AS target,n.p+1 AS x WHERE x>0 RETURN target,x"),
        R, vec![set(0, 1), Action::SetLabel { target: 0, label: LabelId(1), present: true }],
    ).unwrap();
    let values = [1, 3, 5].map(CanonicalScalar::Int);
    let checkpoints = Cell::new(0);
    let measured = execute(&mutation, &values, &[], wide(), || {
        checkpoints.set(checkpoints.get() + 1); Ok::<_, usize>(())
    }).unwrap();
    let stats = measured.stats();
    let caps = [stats.selection.snapshot_records, stats.selection.result_rows,
        stats.evaluator.work_units, stats.evaluator.scratch_entries, stats.effects];
    let policy = |caps: [u64; 5]| GraphMutationPolicy::new(
        GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]), caps[4]);
    assert_eq!(run(&mutation, &values, &[], policy(caps)).stats(), stats);
    for dimension in 0..5 {
        let mut less = caps; less[dimension] -= 1;
        assert!(execute(&mutation, &values, &[], policy(less), || Ok::<_, ()>(())).is_err());
    }
    for stop in 1..=checkpoints.get() {
        let mut at = 0;
        let result = execute(&mutation, &values, &[], wide(), || {
            at += 1; if at == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(observed)) if observed == stop));
        assert_eq!(at, stop);
    }
}

#[test]
fn input_failures_remain_typed_and_zero_final_rows_cannot_hide_them() {
    let mutation = PreparedGraphMutation::prepare_relation(
        relation("MATCH (n) WITH n AS target,n.p AS x RETURN target,10/x AS value LIMIT 0"),
        R, vec![set(0, 1)],
    ).unwrap();
    assert!(matches!(execute(&mutation, &[CanonicalScalar::Int(0)], &[], wide(), || Ok::<_, ()>(())),
        Err(GqlQueryError::Source(GraphMutationError::InputRelation(GraphSetExecutionError::Projection { .. })))));
    let failed = mutation.execute_governed(wide(), |_, _| {
        Err::<GqlQueryExecution<GraphValueRow>, _>(GqlQueryError::Source("source unavailable"))
    }, || Ok::<_, ()>(()));
    assert!(matches!(failed, Err(GqlQueryError::Source(GraphMutationError::InputRelation(
        GraphSetExecutionError::Source("source unavailable"))))));
    let invalid = mutation.execute_governed(wide(), |_, _| Ok::<_, GqlQueryError<(), ()>>(
        GqlQueryExecution { value: vec![], rows: GqlExecutionStats { snapshot_records: 0, result_rows: 1 },
            evaluator: GlaExecutionStats::default() }), || Ok::<_, ()>(()));
    assert!(matches!(invalid, Err(GqlQueryError::Source(GraphMutationError::InputRelation(
        GraphSetExecutionError::InvalidSourceStatistics { .. })))));
    let safe = PreparedGraphMutation::prepare_relation(
        relation("MATCH (n) WITH n AS target,n.p AS x WHERE x<>0 RETURN target,10/x AS value"),
        R, vec![set(0, 1)],
    ).unwrap();
    assert!(run(&safe, &[CanonicalScalar::Int(0)], &[], wide()).intents().is_empty());
}

#[test]
fn final_schema_and_parent_depth_are_checked_before_a_source_is_available() {
    let input = relation("MATCH (n) RETURN n AS target,n.p AS x");
    for action in [set(1, 0), set(0, 0), set(usize::MAX, 1),
        Action::DetachDelete { target: 1 }] {
        assert!(PreparedGraphMutation::prepare_relation(input.clone(), R, vec![action]).is_err());
    }
    assert!(matches!(PreparedGraphMutation::prepare_relation(input.clone(), R,
        vec![set(0, 1), Action::DetachDelete { target: 0 }]),
        Err(GraphMutationBuildError::MixedDeletionAndUpdates)));
    let binary = input.clone().combine(GraphSetOperation::Union, GraphSetQuantifier::All, input.clone()).unwrap();
    assert!(matches!(PreparedGraphMutation::prepare_relation(binary, R, vec![set(0, 1)]),
        Err(GraphMutationBuildError::RequiresSingleGraphSource)));
    let pattern: PreparedGraphPattern<GraphValueRow> = PreparedGraphText::prepare("MATCH (n) RETURN n,n.p AS x", symbols)
        .unwrap().bind_parameters(&GqlParameters::new()).unwrap();
    let mut deep: PreparedGraphSet = pattern.into();
    for _ in 1..MAX_GRAPH_SET_DEPTH { deep = deep.nested().unwrap(); }
    assert!(matches!(PreparedGraphMutation::prepare_relation(deep, R, vec![set(0, 1)]),
        Err(GraphMutationBuildError::RelationalInput(_))));
}

#[test]
fn ordinary_definition_is_unchanged_and_pipeline_executes_one_real_leaf() {
    let pattern = PreparedGraphText::prepare("MATCH (n) RETURN n AS target,n.p AS x", symbols)
        .unwrap().bind_parameters(&GqlParameters::new()).unwrap();
    let ordinary = PreparedGraphMutation::prepare(pattern.clone(), R, vec![set(0, 1)]).unwrap();
    let pipeline = PreparedGraphMutation::prepare_relation(pattern.clone().into(), R, vec![set(0, 1)]).unwrap();
    assert!(ordinary.input_relation().is_none());
    assert_eq!(pipeline.selection(), ordinary.selection());
    assert_ne!(pipeline.canonical_bytes(), ordinary.canonical_bytes());
    let values = [CanonicalScalar::Int(7)];
    assert_eq!(run(&ordinary, &values, &[], wide()).intents(), run(&pipeline, &values, &[], wide()).intents());
    let calls = Cell::new(0);
    pipeline.execute_governed(wide(), |source, remaining| {
        calls.set(calls.get() + 1);
        assert_eq!(source, &pattern);
        source.plan().execute_governed_with_properties(1, [VId(0)], [],
            |_, _| Ok::<_, ()>(true), |_, _| Ok(Some(&values[0])), remaining, || Ok::<_, ()>(()))
    }, || Ok::<_, ()>(())).unwrap();
    assert_eq!(calls.get(), 1);
    assert!(!format!("{pipeline:?}").contains("target"));
}
