//! The output engine accepts a complete relational owner without extracting a
//! fake first-leaf aggregate. Its existing ranking and DISTINCT laws stay shared.
use super::*;
use crate::standing_query::StandingQueryStats;
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder};
use fgdb_gql::{
    GqlQueryPolicy, GraphAggregate, GraphAggregateColumn, GraphAggregateOrder,
    GraphAggregateValue, GraphNullPlacement, GraphSetOperation, GraphSetQuantifier,
    PreparedGraphSet, PreparedGraphSetAggregate,
};
use fgdb_types::CanonicalScalar;

const LIMBS: LimbLimit = LimbLimit::new(4);
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn definitions(distinct: bool, ranked: bool) -> (PreparedGraphAggregate, PreparedGraphSetAggregate) {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let pattern = builder.prepare_values(&[
        GraphColumn::property("key", "n", PropertyKeyId(1)),
        GraphColumn::property("value", "n", PropertyKeyId(2)),
    ], 0, None).unwrap().with_duplicates();
    let relation = PreparedGraphSet::from(pattern.clone())
        .combine(GraphSetOperation::Union, GraphSetQuantifier::All, pattern.clone().into()).unwrap();
    let specs = [GraphAggregate::count_rows("count"), GraphAggregate::sum_int("sum", 1)];
    let order = if ranked { vec![GraphAggregateOrder {
        column: GraphAggregateColumn::Aggregate(1), descending: true, nulls: GraphNullPlacement::Last,
    }] } else { vec![] };
    let plain = PreparedGraphAggregate::prepare(pattern, &[0], &specs, 0, ranked.then_some(1))
        .unwrap().with_result_clauses(&[], &order).unwrap()
        .with_key_output_columns(&[]).unwrap().with_aggregate_output_prefix(1).unwrap()
        .with_distinct_output(distinct);
    let compound = PreparedGraphSetAggregate::prepare(relation, &[0], &specs, 0, ranked.then_some(1))
        .unwrap().with_result_clauses(&[], &order).unwrap()
        .with_key_output_columns(&[]).unwrap().with_aggregate_output_prefix(1).unwrap()
        .with_distinct_output(distinct);
    (plain, compound)
}
fn delta<D: GroupDefinition>(definition: &D, values: &[(i64, u64, i128, i128)]) -> ZSet<GraphAggregateRow> {
    let complete = definition.complete_groups().unwrap();
    ZSet::from_updates(values.iter().map(|&(key, count, sum, sign)| (
        complete.materialize_incremental_row(
            vec![GraphValue::Scalar(CanonicalScalar::Int(key))],
            vec![GraphAggregateValue::Count(count), GraphAggregateValue::Integer(sum)],
        ).unwrap(), ZWeight::from_i128(sign),
    )), LIMBS, &mut |_| Ok::<_, ()>(())).unwrap()
}
fn apply<D: GroupDefinition>(state: &mut State<D>, change: &ZSet<GraphAggregateRow>) -> ZSet<GraphAggregateRow> {
    let mut checkpoint = || Ok(());
    let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
    state.prepare(change, &mut meter).unwrap().commit()
}
fn copy(rows: &ZSet<GraphAggregateRow>) -> ZSet<GraphAggregateRow> {
    rows.checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap()
}
fn visible<D: GroupDefinition>(state: &State<D>) -> Vec<GraphAggregateRow> {
    state.ordered_rows().map_or_else(Vec::new, |rows| rows.iter().map(|r| r.as_ref().clone()).collect())
}

#[test]
fn compound_and_single_source_owners_share_output_semantics_and_exact_derivatives() {
    for distinct in [false, true] { for ranked in [false, true] {
        let (plain, compound) = definitions(distinct, ranked);
        let frozen = compound.input().canonical_bytes();
        let mut ordinary = State::new(plain);
        let mut relational = State::new(compound);
        for change in [
            vec![(1, 2, 20, 1), (2, 2, 10, 1), (3, 3, 5, 1)],
            vec![(1, 2, 20, -1)],
            vec![(4, 4, 15, 1)],
            vec![(2, 2, 10, -1), (2, 2, 25, 1)],
            vec![],
            vec![(2, 2, 25, -1), (3, 3, 5, -1), (4, 4, 15, -1)],
        ] {
            let change = delta(relational.definition(), &change);
            let before = copy(&relational.rows);
            let a = apply(&mut ordinary, &change);
            let b = apply(&mut relational, &change);
            assert_eq!(a, b);
            let mut integrated = before;
            integrated.integrate(&b, LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
            assert_eq!(integrated, relational.rows);
            assert_eq!(ordinary.rows, relational.rows);
            assert_eq!(visible(&ordinary), visible(&relational));
            assert_eq!(relational.definition().input().canonical_bytes(), frozen);
        }
    }}
}

#[test]
fn hidden_distinct_representative_rank_changes_refill_the_right_final_page() {
    let (_, definition) = definitions(true, true);
    let mut state = State::new(definition);
    let first = delta(state.definition(), &[(1, 2, 20, 1), (2, 2, 10, 1), (3, 3, 5, 1)]);
    apply(&mut state, &first);
    let change = delta(state.definition(), &[(1, 2, 20, -1)]);
    assert!(apply(&mut state, &change).is_empty()); // Same cell, different retained witness.
    let change = delta(state.definition(), &[(4, 4, 15, 1)]);
    assert_eq!(apply(&mut state, &change).len(), 2);
    assert_eq!(visible(&state)[0].values(), &[GraphAggregateValue::Count(4)]);
    let change = delta(state.definition(), &[(2, 2, 10, -1), (2, 2, 25, 1)]);
    apply(&mut state, &change);
    assert_eq!(visible(&state)[0].values(), &[GraphAggregateValue::Count(2)]);
}

#[test]
fn relational_output_refusal_drop_and_retry_publish_neither_page_nor_derivative() {
    for ranked in [false, true] {
        let make = || {
            let (_, definition) = definitions(true, ranked);
            let mut state = State::new(definition);
            let initial = delta(state.definition(), &[(1, 2, 20, 1), (2, 2, 10, 1), (3, 3, 5, 1)]);
            apply(&mut state, &initial);
            state
        };
        let mut success = make();
        let change = delta(success.definition(), &[(1, 2, 20, -1), (4, 4, 15, 1)]);
        let mut calls = 0;
        let expected_delta = {
            let mut checkpoint = || { calls += 1; Ok(()) };
            let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            success.prepare(&change, &mut meter).unwrap().commit()
        };
        for stop in 1..=calls {
            let mut state = make();
            let before = copy(&state.rows);
            let before_order = visible(&state);
            let before_classes = state.classes.clone();
            let before_count = state.row_count;
            let mut seen = 0;
            {
                let mut checkpoint = || {
                    seen += 1;
                    if seen == stop { Err(StandingQueryFailure::Interrupted) } else { Ok(()) }
                };
                let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
                assert!(state.prepare(&change, &mut meter).is_err());
            }
            assert_eq!(seen, stop);
            assert_eq!(state.rows, before);
            assert_eq!(visible(&state), before_order);
            assert_eq!(state.classes, before_classes);
            assert_eq!(state.row_count, before_count);
            let mut checkpoint = || Ok(());
            let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            drop(state.prepare(&change, &mut meter).unwrap());
            assert_eq!(state.rows, before);
            assert_eq!(visible(&state), before_order);
            assert_eq!(state.prepare(&change, &mut meter).unwrap().commit(), expected_delta);
            assert_eq!(state.rows, success.rows);
            assert_eq!(visible(&state), visible(&success));
        }
    }
}
