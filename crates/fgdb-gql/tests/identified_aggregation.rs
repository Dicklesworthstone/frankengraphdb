//! Captured identities reach the ordinary exact aggregate/result engine.
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GraphColumn, GraphPathFunction, GraphPatternBuilder, GraphValueRow,
    IntegerComparison, PreparedGraphPattern, VertexPredicate,
};
use fgdb_gql::{
    GqlQueryError, GqlQueryPolicy, GraphAggregate, GraphAggregateColumn, GraphAggregateError,
    GraphAggregateOrder, GraphAggregateRow, GraphExactAverage, GraphIntegerBinary,
    GraphIntegerErrorKind, GraphIntegerExpression, GraphIntegerOp, GraphSetExecutionError,
    GraphSetProjection, GraphSetQuantifier, GraphSetValue, GraphWalkBounds,
    PreparedGraphAggregate, PreparedGraphSet,
};
use fgdb_types::{CanonicalScalar, EId, VId};
use std::cell::Cell;

const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
const VERTICES: [VId; 3] = [VId(1), VId(2), VId(3)];
const EDGES: [(EId, VId, RelationId, VId); 4] = [
    (EId(14), VId(1), R, VId(3)),
    (EId(12), VId(1), R, VId(2)),
    (EId(13), VId(2), R, VId(3)),
    (EId(11), VId(1), R, VId(2)),
];
static VALUES: [CanonicalScalar; 3] = [CanonicalScalar::Int(1), CanonicalScalar::Int(2), CanonicalScalar::Int(3)];
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(100, 100, 1_000_000, 1_000_000) }
fn matches(vid: VId, predicates: &[VertexPredicate]) -> Result<bool, &'static str> {
    Ok(predicates.iter().all(|p| p.matches(&[], &[(P, VALUES[vid.0 as usize - 1].clone())])))
}
fn input(captured: bool, minimum: u32) -> PreparedGraphPattern<GraphValueRow> {
    let mut b = GraphPatternBuilder::new();
    b.vertex("a").unwrap().vertex("b").unwrap();
    b.walk("a", R, GlaDirection::Forward, "b", GraphWalkBounds::new(minimum, 2).unwrap()).unwrap();
    b.filter("a", VertexPredicate::IntegerProperty { key: P, comparison: IntegerComparison::Equal, value: 1 }).unwrap();
    let mut columns = vec![GraphColumn::vertex("endpoint", "b"), GraphColumn::property("value", "b", P)];
    if captured {
        b.capture_path("route").unwrap();
        columns.extend([
            GraphColumn::path("route", "route", GraphPathFunction::Value),
            GraphColumn::path("hops", "route", GraphPathFunction::Length),
            GraphColumn::path("nodes", "route", GraphPathFunction::Nodes),
            GraphColumn::path("edges", "route", GraphPathFunction::Edges),
        ]);
    }
    b.prepare_values(&columns, 0, None).unwrap().with_duplicates()
}
fn run(query: &PreparedGraphAggregate, policy: GqlQueryPolicy) -> fgdb_gql::GqlQueryExecution<GraphAggregateRow> {
    query.execute_governed_with_identified_properties(7, VERTICES, EDGES, matches,
        |vid, _| Ok(Some(&VALUES[vid.0 as usize - 1])), policy, || Ok::<_, ()>(())).unwrap()
}
fn summary(input: PreparedGraphPattern<GraphValueRow>) -> PreparedGraphAggregate {
    PreparedGraphAggregate::prepare(input, &[], &[
        GraphAggregate::count_rows("occurrences"), GraphAggregate::count("nonnull", 2),
        GraphAggregate::count_distinct("routes", 2), GraphAggregate::sum_int("hops", 3),
        GraphAggregate::sum_int_distinct("lengths", 3), GraphAggregate::average_int("mean", 3),
        GraphAggregate::average_int_distinct("unique_mean", 3), GraphAggregate::min("first", 2),
        GraphAggregate::max("last", 2),
    ], 0, None).unwrap()
}

#[test]
fn parallel_edge_identity_survives_all_nine_aggregate_functions() {
    let query = summary(input(true, 1));
    let measured = run(&query, GqlQueryPolicy::new(7, 1, 1_000_000, 1_000_000));
    let values = measured.value[0].values();
    // Independent route list: 11, 12, 14, 11/13, 12/13. The first two
    // paths have equal vertex lists but different edge identities.
    assert_eq!(values[0].as_count(), Some(5));
    assert_eq!(values[1].as_count(), Some(5));
    assert_eq!(values[2].as_count(), Some(5));
    assert_eq!(values[3].as_integer(), Some(7));
    assert_eq!(values[4].as_integer(), Some(3));
    assert_eq!(values[5].as_average(), GraphExactAverage::new(7, 5));
    assert_eq!(values[6].as_average(), GraphExactAverage::new(3, 2));
    assert_eq!(values[7].as_value().unwrap().as_path().unwrap().steps(), &[(EId(11), VId(2))]);
    assert_eq!(values[8].as_value().unwrap().as_path().unwrap().steps(), &[(EId(14), VId(3))]);
    assert_eq!(measured.rows.snapshot_records, 7);
    assert_eq!(measured.rows.result_rows, 1);
    let arrays = PreparedGraphAggregate::prepare(input(true, 1), &[], &[
        GraphAggregate::count_distinct("vertices", 4), GraphAggregate::count_distinct("edges", 5),
    ], 0, None).unwrap();
    let result = run(&arrays, wide());
    assert_eq!(result.value[0].values()[0].as_count(), Some(3));
    assert_eq!(result.value[0].values()[1].as_count(), Some(5));
}

#[test]
fn captured_group_keys_hidden_outputs_and_ranking_keep_exact_route_identity() {
    let query = PreparedGraphAggregate::prepare(input(true, 0), &[2], &[
        GraphAggregate::count_rows("n"), GraphAggregate::sum_int("length", 3),
    ], 1, Some(2)).unwrap().with_key_output_columns(&[]).unwrap().with_distinct_output(true)
        .with_result_clauses(&[], &[GraphAggregateOrder::descending(GraphAggregateColumn::Aggregate(1))]).unwrap();
    let result = run(&query, wide());
    // Six path groups collapse to the three visible pairs (1,2),(1,1),(1,0).
    // Ranking and DISTINCT precede the output page, not input aggregation.
    assert_eq!(result.value.len(), 2);
    assert!(result.value.iter().all(|row| row.keys().is_empty()));
    assert_eq!(result.value[0].values()[1].as_integer(), Some(1));
    assert_eq!(result.value[1].values()[1].as_integer(), Some(0));
}

#[test]
fn projected_and_completed_relational_inputs_share_identified_summaries() {
    let projection = vec![
        GraphSetProjection::new("route", GraphSetValue::Column(2)),
        GraphSetProjection::new("hops", GraphSetValue::Column(3)),
    ];
    let projected = PreparedGraphAggregate::prepare_projected(input(true, 1), projection.clone(), &[], &[
        GraphAggregate::count_distinct("routes", 0), GraphAggregate::sum_int("hops", 1),
    ], 0, None).unwrap();
    let relation = PreparedGraphSet::from(input(true, 1)).project(projection, GraphSetQuantifier::All).unwrap();
    let full = PreparedGraphAggregate::prepare_relation(relation.clone(), &[], &[
        GraphAggregate::count_distinct("routes", 0), GraphAggregate::sum_int("hops", 1),
    ], 0, None).unwrap();
    assert_eq!(run(&projected, wide()).value, run(&full, wide()).value);
    let page = PreparedGraphAggregate::prepare_relation(relation.with_page(1, Some(2)), &[], &[
        GraphAggregate::count_distinct("routes", 0), GraphAggregate::sum_int("hops", 1),
    ], 0, None).unwrap();
    let result = run(&page, wide());
    // Canonical path order: 11, 11/13, 12, 12/13, 14; page keeps 11/13,12.
    assert_eq!(result.value[0].values()[0].as_count(), Some(2));
    assert_eq!(result.value[0].values()[1].as_integer(), Some(3));
}

#[test]
fn ordinary_inputs_keep_their_existing_counters_and_transcripts() {
    let query = PreparedGraphAggregate::prepare(input(false, 1), &[0], &[
        GraphAggregate::count_rows("n"), GraphAggregate::sum_int("s", 1),
    ], 0, None).unwrap();
    let frozen = query.canonical_bytes();
    let ordinary = query.execute_governed(7, VERTICES, EDGES.map(|(_, s, r, d)| (s, r, d)),
        matches, |vid, _| Ok(Some(&VALUES[vid.0 as usize - 1])), wide(), || Ok::<_, ()>(())).unwrap();
    assert_eq!(run(&query, wide()), ordinary);
    assert_eq!(query.canonical_bytes(), frozen);
    let missing_ids = summary(input(true, 1)).execute_governed(7, VERTICES,
        EDGES.map(|(_, s, r, d)| (s, r, d)), matches, |_, _| Ok(None), wide(), || Ok::<_, ()>(()));
    assert!(matches!(missing_ids, Err(GqlQueryError::IdentifiedEdgesRequired)));
}

#[test]
fn one_meter_covers_identified_input_projection_groups_and_every_checkpoint() {
    for relational in [false, true] {
        let query = if relational {
            PreparedGraphAggregate::prepare_relation(input(true, 1).into(), &[], &[
                GraphAggregate::count_distinct("routes", 2), GraphAggregate::sum_int("hops", 3),
            ], 0, None).unwrap()
        } else { summary(input(true, 1)) };
        let events = Cell::new(0);
        let measured = query.execute_governed_with_identified_properties(7, VERTICES, EDGES,
            matches, |vid, _| Ok(Some(&VALUES[vid.0 as usize - 1])), wide(), || {
                events.set(events.get() + 1); Ok::<_, usize>(())
            }).unwrap();
        let caps = [measured.rows.snapshot_records, measured.rows.result_rows,
            measured.evaluator.work_units, measured.evaluator.scratch_entries];
        assert_eq!(run(&query, GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3])), measured);
        for dimension in 0..4 {
            let mut cap = caps; cap[dimension] -= 1;
            assert!(query.execute_governed_with_identified_properties(7, VERTICES, EDGES,
                matches, |vid, _| Ok(Some(&VALUES[vid.0 as usize - 1])),
                GqlQueryPolicy::new(cap[0], cap[1], cap[2], cap[3]), || Ok::<_, ()>(())).is_err());
        }
        for stop in 1..=events.get() {
            let mut seen = 0;
            let failed = query.execute_governed_with_identified_properties(7, VERTICES, EDGES,
                matches, |vid, _| Ok(Some(&VALUES[vid.0 as usize - 1])), wide(), || {
                    seen += 1; if seen == stop { Err(stop) } else { Ok(()) }
                });
            assert!(matches!(failed, Err(GqlQueryError::Interrupted(at)) if at == stop));
            assert_eq!(seen, stop);
        }
    }
}

#[test]
fn zero_output_cannot_hide_late_property_or_projected_arithmetic_failures() {
    let bad = GraphIntegerExpression::prepare(&[
        GraphIntegerOp::Literal(Some(1)), GraphIntegerOp::Column(1), GraphIntegerOp::Literal(Some(3)),
        GraphIntegerOp::Binary(GraphIntegerBinary::Subtract), GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
    ]).unwrap();
    let projection = vec![GraphSetProjection::new("bad", GraphSetValue::Integer(bad))];
    for relational in [false, true] {
        let query = if relational {
            let relation = PreparedGraphSet::from(input(true, 1)).project(projection.clone(), GraphSetQuantifier::All).unwrap();
            PreparedGraphAggregate::prepare_relation(relation, &[], &[GraphAggregate::count_rows("n")], 0, Some(0)).unwrap()
        } else {
            PreparedGraphAggregate::prepare_projected(input(true, 1), projection.clone(), &[],
                &[GraphAggregate::count_rows("n")], 0, Some(0)).unwrap()
        };
        let failed = query.execute_governed_with_identified_properties(7, VERTICES, EDGES,
            matches, |vid, _| Ok(Some(&VALUES[vid.0 as usize - 1])), wide(), || Ok::<_, ()>(()));
        assert!(matches!(failed,
            Err(GqlQueryError::Source(GraphAggregateError::InputExpression { error, .. }))
            | Err(GqlQueryError::Source(GraphAggregateError::InputRelation(
                GraphSetExecutionError::Projection { error, .. })))
            if error.kind == GraphIntegerErrorKind::DivisionByZero));
        let failed = query.execute_governed_with_identified_properties(7, VERTICES, EDGES,
            matches, |vid, _| if vid == VId(3) { Err("unreadable") } else { Ok(Some(&VALUES[0])) },
            wide(), || Ok::<_, ()>(()));
        assert!(matches!(failed,
            Err(GqlQueryError::Source(GraphAggregateError::Source("unreadable")))
            | Err(GqlQueryError::Source(GraphAggregateError::InputRelation(GraphSetExecutionError::Source("unreadable"))))));
    }
}

#[test]
fn empty_identified_source_keeps_keyless_zero_and_keyed_absence() {
    for keys in [vec![], vec![2]] {
        let query = PreparedGraphAggregate::prepare(input(true, 1), &keys, &[
            GraphAggregate::count_rows("n"), GraphAggregate::min("route", 2),
        ], 0, None).unwrap();
        let result = query.execute_governed_with_identified_properties(0, [], [], matches,
            |_, _| Ok(None), wide(), || Ok::<_, ()>(())).unwrap();
        if keys.is_empty() {
            assert_eq!(result.value.len(), 1);
            assert_eq!(result.value[0].values()[0].as_count(), Some(0));
            assert!(result.value[0].values()[1].is_null());
        } else { assert!(result.value.is_empty()); }
    }
}
