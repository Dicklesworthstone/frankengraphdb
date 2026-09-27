//! Exercise the real relational evaluator and scalar VM with an independent
//! occurrence-to-identity oracle. No proposal is treated as a durable write.

use super::*;
use crate::algebra::{GraphValue, GraphValueOrder};
use crate::{
    GlaLimitDimension, GqlBudgetDimension, GqlParameters, GraphIntegerBinary,
    GraphIntegerExpression, GraphIntegerOp, GraphSetBuildError, GraphSetQuantifier,
    PreparedGraphText,
};
use std::cell::Cell;

const R: RelationId = RelationId(9);
const P: PropertyKeyId = PropertyKeyId(4);
type ResultOf =
    Result<GraphInsertBatch, GqlQueryError<GraphInsertError<&'static str, &'static str>, usize>>;

fn policy() -> GraphInsertPolicy {
    GraphInsertPolicy::new(GqlQueryPolicy::new(100, 100, 100_000, 100_000), 100, 100)
}
fn scalar(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn unwind(values: Vec<GraphValue>) -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .unwind(
            "value".into(),
            GraphSetValue::Value(GraphValue::List(values.into())),
        )
        .unwrap()
}
fn vertex(value: GraphMutationValue) -> GraphInsertVertex {
    GraphInsertVertex {
        labels: vec![LabelId(3)],
        properties: vec![(P, value)],
    }
}
fn plan(input: PreparedGraphSet) -> PreparedGraphInsert {
    PreparedGraphInsert::prepare_relation(
        input,
        R,
        vec![vertex(GraphMutationValue::Column(0))],
        vec![GraphInsertEdge {
            source: GraphInsertEndpoint::CreatedVertex(0),
            destination: GraphInsertEndpoint::CreatedVertex(0),
            relation: R,
            properties: vec![(P, GraphMutationValue::Column(0))],
        }],
    )
    .unwrap()
}
fn no_source(
    _: &PreparedGraphPattern<GraphValueRow>,
    _: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>> {
    panic!("source-free relational insertion must not observe a graph")
}
fn identity(request: GraphInsertRequest) -> Result<ElementId, &'static str> {
    Ok(match request {
        GraphInsertRequest::Vertex { row, vertex } => {
            ElementId::Vertex(VId(100 + row as u128 * 256 + vertex as u128))
        }
        GraphInsertRequest::Edge { row, edge } => {
            ElementId::Edge(EId(200 + row as u128 * 256 + edge as u128))
        }
    })
}
fn run(plan: &PreparedGraphInsert, policy: GraphInsertPolicy) -> ResultOf {
    plan.execute_governed(policy, no_source, identity, || Ok(()))
}
fn pattern() -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare("MATCH (n) RETURN n", |_, _| None)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn graph_source(
    pattern: &PreparedGraphPattern<GraphValueRow>,
    allowance: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>> {
    pattern.plan().execute_governed_with_properties(
        2,
        [VId(7), VId(2)],
        [],
        |_, _| Ok(true),
        |_, _| Ok(None),
        allowance,
        || Ok(()),
    )
}

#[test]
fn duplicates_null_and_input_order_create_distinct_occurrence_local_structures() {
    let values = [
        CanonicalScalar::Int(3),
        CanonicalScalar::Int(1),
        CanonicalScalar::Int(3),
        CanonicalScalar::Null,
    ];
    let plan = plan(unwind(
        values.iter().cloned().map(GraphValue::Scalar).collect(),
    ));
    assert!(plan.selection().is_none());
    assert!(plan.relational_selection().is_some());
    assert!(!plan.is_standalone());
    assert!(!plan.requires_read());
    let mut requests = Vec::new();
    let result: ResultOf = plan.execute_governed(
        policy(),
        no_source,
        |request| {
            requests.push(request);
            identity(request)
        },
        || Ok(()),
    );
    let batch = result.unwrap();
    let mut expected = Vec::new();
    let mut expected_requests = Vec::new();
    for (row, value) in values.into_iter().enumerate() {
        let vertex = VId(100 + row as u128 * 256);
        expected.push(GraphInsertIntent::Vertex {
            vertex,
            labels: vec![LabelId(3)],
            properties: vec![(P, value.clone())],
        });
        expected.push(GraphInsertIntent::Edge {
            edge: EId(200 + row as u128 * 256),
            relation: R,
            source: vertex,
            destination: vertex,
            properties: vec![(P, value)],
        });
        expected_requests.push(GraphInsertRequest::Vertex { row, vertex: 0 });
        expected_requests.push(GraphInsertRequest::Edge { row, edge: 0 });
    }
    assert_eq!(batch.intents(), expected);
    assert_eq!(requests, expected_requests);
    assert_eq!(batch.stats().selection.snapshot_records, 0);
    assert_eq!(batch.stats().selection.result_rows, 4);
    assert_eq!(batch.stats().created_vertices, 4);
    assert_eq!(batch.stats().created_edges, 4);
    assert_eq!(run(&plan, policy()).unwrap(), batch);
}

#[test]
fn relation_order_and_page_determine_identity_requests_after_unwind() {
    let input = unwind(vec![scalar(3), scalar(1), scalar(3), scalar(2)])
        .with_order_by(&[GraphValueOrder::ascending(0)])
        .unwrap()
        .with_page(1, Some(2));
    let result = run(&plan(input), policy()).unwrap();
    assert_eq!(result.stats().created_vertices, 2);
    for (intent, expected) in result.intents().iter().step_by(2).zip([2, 3]) {
        let GraphInsertIntent::Vertex { properties, .. } = intent else {
            panic!("expected one vertex before its edge")
        };
        assert_eq!(properties, &[(P, CanonicalScalar::Int(expected))]);
    }
}

#[test]
fn empty_and_null_lists_create_nothing_and_request_no_identity() {
    let null_input = PreparedGraphSet::singleton()
        .unwind(
            "value".into(),
            GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
        )
        .unwrap();
    for input in [unwind(vec![]), null_input] {
        let result: ResultOf = plan(input).execute_governed(
            GraphInsertPolicy::new(GqlQueryPolicy::new(0, 0, 1000, 1000), 0, 0),
            no_source,
            |_| panic!("an empty relation cannot allocate identities"),
            || Ok(()),
        );
        let batch = result.unwrap();
        assert!(batch.intents().is_empty());
        assert_eq!(batch.stats().selection.result_rows, 0);
        assert_eq!(batch.stats().created_vertices, 0);
        assert_eq!(batch.stats().created_edges, 0);
    }
}

#[test]
fn dynamic_non_scalar_properties_refuse_before_any_identity_is_requested() {
    for bad in [
        GraphValue::List(vec![scalar(9)].into()),
        GraphValue::Vertex(VId(999)),
        GraphValue::Edge(EId(999)),
    ] {
        let plan = plan(unwind(vec![scalar(7), bad]));
        let result: ResultOf = plan.execute_governed(
            policy(),
            no_source,
            |_| panic!("all rows must validate before the first identity request"),
            || Ok(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphInsertError::InputSchema {
                row: 1,
                column: 0
            }))
        ));
    }
}

#[test]
fn dynamic_scalar_expressions_refuse_late_type_errors_before_identity_allocation() {
    let expression = GraphIntegerExpression::prepare(&[
        GraphIntegerOp::Column(0),
        GraphIntegerOp::Literal(Some(1)),
        GraphIntegerOp::Binary(GraphIntegerBinary::Add),
    ])
    .unwrap();
    let plan = PreparedGraphInsert::prepare_relation(
        unwind(vec![scalar(7), GraphValue::List(vec![scalar(9)].into())]),
        R,
        vec![vertex(GraphMutationValue::Expression(expression))],
        vec![],
    )
    .unwrap();
    let result: ResultOf = plan.execute_governed(
        policy(),
        no_source,
        |_| panic!("a late scalar VM refusal must precede allocation"),
        || Ok(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphInsertError::Arithmetic {
            row: 1,
            declaration: 0,
            property: 0,
            ..
        }))
    ));
}

#[test]
fn chained_unwind_can_retain_unused_list_columns_and_create_from_final_scalars() {
    let input = unwind(vec![
        GraphValue::List(vec![scalar(3), scalar(1)].into()),
        GraphValue::List(vec![scalar(2)].into()),
    ])
    .unwind("element".into(), GraphSetValue::Column(0))
    .unwrap();
    let plan = PreparedGraphInsert::prepare_relation(
        input,
        R,
        vec![vertex(GraphMutationValue::Column(1))],
        vec![],
    )
    .unwrap();
    let batch = run(&plan, policy()).unwrap();
    assert_eq!(batch.stats().created_vertices, 3);
    for (row, value) in [3, 1, 2].into_iter().enumerate() {
        assert_eq!(
            batch.intents()[row],
            GraphInsertIntent::Vertex {
                vertex: VId(100 + row as u128 * 256),
                labels: vec![LabelId(3)],
                properties: vec![(P, CanonicalScalar::Int(value))],
            }
        );
    }
}

#[test]
fn relational_projection_failures_keep_their_typed_error_before_allocation() {
    let input = PreparedGraphSet::singleton()
        .unwind("value".into(), GraphSetValue::Value(scalar(5)))
        .unwrap();
    let result: ResultOf = plan(input).execute_governed(
        policy(),
        no_source,
        |_| panic!("invalid UNWIND must not allocate"),
        || Ok(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphInsertError::InputRelation(
            GraphSetExecutionError::Projection { .. }
        )))
    ));
}

#[test]
fn row_creation_work_and_scratch_limits_cover_relation_and_collection_together() {
    let input = unwind(vec![scalar(7), scalar(8)]);
    let relation_stats = input
        .execute_governed(policy().query, no_source, || Ok(()))
        .unwrap();
    let plan = plan(input);
    let baseline = run(&plan, policy()).unwrap();
    let all = baseline.stats().evaluator;
    assert!(all.work_units > relation_stats.evaluator.work_units);
    assert!(all.scratch_entries > relation_stats.evaluator.scratch_entries);
    for dimension in [
        GlaLimitDimension::WorkUnits,
        GlaLimitDimension::ScratchEntries,
    ] {
        let mut tight = policy();
        let limit = match dimension {
            GlaLimitDimension::WorkUnits => {
                tight.query.evaluator.max_work_units = relation_stats.evaluator.work_units;
                relation_stats.evaluator.work_units
            }
            GlaLimitDimension::ScratchEntries => {
                tight.query.evaluator.max_scratch_entries =
                    relation_stats.evaluator.scratch_entries;
                relation_stats.evaluator.scratch_entries
            }
        };
        let result: ResultOf = plan.execute_governed(
            tight,
            no_source,
            |_| panic!("the relation exhausted the shared allowance before allocation"),
            || Ok(()),
        );
        assert!(matches!(result, Err(GqlQueryError::Evaluator(error))
            if error.dimension == dimension && error.limit == limit && error.observed > u128::from(limit)));
    }
    let exact = GraphInsertPolicy::new(
        GqlQueryPolicy::new(0, 2, all.work_units, all.scratch_entries),
        2,
        2,
    );
    assert_eq!(run(&plan, exact).unwrap(), baseline);
    for (case, mut tight) in [exact; 3].into_iter().enumerate() {
        match case {
            0 => tight.query = GqlQueryPolicy::new(0, 1, all.work_units, all.scratch_entries),
            1 => tight.max_vertices = 1,
            _ => tight.max_edges = 1,
        }
        let result: ResultOf = plan.execute_governed(
            tight,
            no_source,
            |_| panic!("row/creation admission must precede all identity allocation"),
            || Ok(()),
        );
        match case {
            0 => assert!(matches!(result, Err(GqlQueryError::Rows(error))
                if error.dimension == GqlBudgetDimension::ResultRows && error.observed == 2)),
            1 => assert!(matches!(
                result,
                Err(GqlQueryError::Source(GraphInsertError::Limit {
                    dimension: GraphInsertLimitDimension::Vertices,
                    ..
                }))
            )),
            _ => assert!(matches!(
                result,
                Err(GqlQueryError::Source(GraphInsertError::Limit {
                    dimension: GraphInsertLimitDimension::Edges,
                    ..
                }))
            )),
        }
    }
}

#[test]
fn every_relational_or_collection_checkpoint_can_cancel_without_a_partial_batch() {
    let plan = plan(unwind(vec![scalar(7), scalar(8)]));
    let calls = Cell::new(0);
    let baseline: ResultOf = plan.execute_governed(policy(), no_source, identity, || {
        calls.set(calls.get() + 1);
        Ok(())
    });
    let baseline = baseline.unwrap();
    let total = calls.get();
    assert!(total > 10);
    for stop in 1..=total {
        let mut observed = 0;
        let result: ResultOf = plan.execute_governed(policy(), no_source, identity, || {
            observed += 1;
            if observed == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
    }
    assert_eq!(run(&plan, policy()).unwrap(), baseline);
}

#[test]
fn typed_vertex_columns_remain_valid_endpoints_after_relational_unwind() {
    let input = PreparedGraphSet::from(pattern())
        .unwind(
            "value".into(),
            GraphSetValue::List(vec![
                GraphSetValue::Value(scalar(4)),
                GraphSetValue::Value(scalar(4)),
            ]),
        )
        .unwrap();
    let plan = PreparedGraphInsert::prepare_relation(
        input,
        R,
        vec![vertex(GraphMutationValue::Column(1))],
        vec![GraphInsertEdge {
            source: GraphInsertEndpoint::Column(0),
            destination: GraphInsertEndpoint::CreatedVertex(0),
            relation: R,
            properties: vec![],
        }],
    )
    .unwrap();
    assert!(plan.requires_read());
    let result: ResultOf = plan.execute_governed(policy(), graph_source, identity, || Ok(()));
    let batch = result.unwrap();
    assert_eq!(batch.stats().selection.snapshot_records, 2);
    assert_eq!(batch.stats().selection.result_rows, 4);
    for (row, source) in [VId(2), VId(2), VId(7), VId(7)].into_iter().enumerate() {
        assert_eq!(
            batch.intents()[row * 2 + 1],
            GraphInsertIntent::Edge {
                edge: EId(200 + row as u128 * 256),
                relation: R,
                source,
                destination: VId(100 + row as u128 * 256),
                properties: vec![],
            }
        );
    }
}

#[test]
fn multiple_graph_leaves_share_source_admission_and_late_failure_precedes_allocation() {
    let input = PreparedGraphSet::from(pattern())
        .cross_join(PreparedGraphSet::from(pattern()))
        .unwrap();
    let plan = PreparedGraphInsert::prepare_relation(
        input,
        R,
        vec![],
        vec![GraphInsertEdge {
            source: GraphInsertEndpoint::Column(0),
            destination: GraphInsertEndpoint::Column(1),
            relation: R,
            properties: vec![],
        }],
    )
    .unwrap();
    let mut visits = Vec::new();
    let result: ResultOf = plan.execute_governed(
        policy(),
        |pattern, allowance| {
            visits.push(allowance);
            graph_source(pattern, allowance)
        },
        identity,
        || Ok(()),
    );
    let batch = result.unwrap();
    assert_eq!(visits.len(), 2);
    assert!(visits[1].evaluator.max_work_units < visits[0].evaluator.max_work_units);
    assert_eq!(visits[1].rows.max_snapshot_records(), Some(98));
    assert_eq!(batch.stats().selection.snapshot_records, 4);
    assert_eq!(batch.stats().created_edges, 4);
    let mut calls = 0;
    let result: ResultOf = plan.execute_governed(
        policy(),
        |pattern, allowance| {
            calls += 1;
            if calls == 2 {
                Err(GqlQueryError::Source("second source failed"))
            } else {
                graph_source(pattern, allowance)
            }
        },
        |_| panic!("all graph leaves must succeed before allocation"),
        || Ok(()),
    );
    assert_eq!(calls, 2);
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphInsertError::InputRelation(
            GraphSetExecutionError::Source("second source failed")
        )))
    ));
    let mut tight = policy();
    tight.query = GqlQueryPolicy::new(3, 100, 100_000, 100_000);
    let result: ResultOf = plan.execute_governed(
        tight,
        graph_source,
        |_| panic!("cumulative source admission failed before allocation"),
        || Ok(()),
    );
    assert!(matches!(result, Err(GqlQueryError::Rows(error))
        if error.dimension == GqlBudgetDimension::SnapshotRecords && error.limit == 3 && error.observed == 4));
}

#[test]
fn schema_and_cross_owner_depth_are_checked_during_preparation() {
    let list = PreparedGraphSet::singleton()
        .project(
            vec![GraphSetProjection::new("list", GraphSetValue::List(vec![]))],
            GraphSetQuantifier::All,
        )
        .unwrap();
    assert!(matches!(
        PreparedGraphInsert::prepare_relation(
            list,
            R,
            vec![vertex(GraphMutationValue::Column(0))],
            vec![]
        ),
        Err(GraphInsertBuildError::ValueColumn {
            declaration: 0,
            column: 0
        })
    ));
    let mut input = PreparedGraphSet::singleton();
    for _ in 1..crate::MAX_GRAPH_SET_DEPTH {
        input = input.nested().unwrap();
    }
    assert!(matches!(
        PreparedGraphInsert::prepare_relation(
            input,
            R,
            vec![GraphInsertVertex {
                labels: vec![],
                properties: vec![]
            }],
            vec![]
        ),
        Err(GraphInsertBuildError::RelationalInput(
            GraphSetBuildError::TooDeep { .. }
        ))
    ));
}

#[test]
fn vertex_merge_rejects_relational_creation_even_without_a_graph_source() {
    use crate::vertex_merge::{GraphVertexMergeBuildError, PreparedGraphVertexMerge};

    // MERGE's creation arm must produce exactly one vertex. A relation can
    // change that cardinality even when it has no graph source and therefore
    // exposes no legacy pattern selection.
    for values in [vec![], vec![scalar(7)], vec![scalar(7), scalar(8)]] {
        let creation = PreparedGraphInsert::prepare_relation(
            unwind(values),
            R,
            vec![vertex(GraphMutationValue::Column(0))],
            vec![],
        )
        .unwrap();
        assert!(creation.selection().is_none());
        assert!(!creation.requires_read());
        assert!(matches!(
            PreparedGraphVertexMerge::prepare(pattern(), R, 0, creation),
            Err(GraphVertexMergeBuildError::CreationMustBeStandalone)
        ));
    }
}

#[test]
fn canonical_identity_preserves_existing_domains_and_binds_the_entire_relation() {
    let bare = GraphInsertVertex {
        labels: vec![LabelId(3)],
        properties: vec![],
    };
    let standalone =
        PreparedGraphInsert::prepare_standalone(R, vec![bare.clone()], vec![]).unwrap();
    let mut expected = b"fgdb:standalone-graph-insert:v1\0".to_vec();
    // RelationId and LabelId are u64, as are all collection lengths.
    for value in [9_u64, 0, 1, 1, 3, 0, 0] {
        expected.extend_from_slice(&value.to_be_bytes());
    }
    assert_eq!(standalone.canonical_bytes(), expected);
    let selected = pattern();
    let matched =
        PreparedGraphInsert::prepare(selected.clone(), R, vec![bare.clone()], vec![]).unwrap();
    let mut expected = b"fgdb:query-graph-insert:v1\0".to_vec();
    expected.extend_from_slice(&9_u64.to_be_bytes());
    let selection = selected.canonical_bytes();
    expected.extend_from_slice(&(selection.len() as u64).to_be_bytes());
    expected.extend_from_slice(&selection);
    for value in [1_u64, 1, 3, 0, 0] {
        expected.extend_from_slice(&value.to_be_bytes());
    }
    assert_eq!(matched.canonical_bytes(), expected);
    let wrapped =
        PreparedGraphInsert::prepare_relation(selected.into(), R, vec![bare], vec![]).unwrap();
    assert_ne!(wrapped.canonical_bytes(), matched.canonical_bytes());
    let input = unwind(vec![scalar(3), scalar(1)]);
    let original = plan(input.clone());
    assert_eq!(
        original.canonical_bytes(),
        plan(input.clone()).canonical_bytes()
    );
    assert_ne!(
        original.canonical_bytes(),
        plan(unwind(vec![scalar(1), scalar(3)])).canonical_bytes()
    );
    assert_ne!(
        original.canonical_bytes(),
        plan(input.with_page(0, Some(1))).canonical_bytes()
    );
}
