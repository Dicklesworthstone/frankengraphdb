//! Independent aggregation oracles: enumerate concrete edge pairs, then group.
//! The oracle does not use GLA slots, aggregate state, or projected child rows.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GraphColumn, GraphPatternBuilder, GraphValue, GraphValueRow, PreparedGraphPattern,
};
use fgdb_gql::{
    GqlBudgetDimension, GqlQueryError, GqlQueryPolicy, GraphAggregate, GraphAggregateBuildError,
    GraphAggregateError, GraphAggregateValue, PreparedGraphAggregate,
};
use fgdb_types::{CanonicalScalar, VId};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const VALUE: PropertyKeyId = PropertyKeyId(1);
const BUCKET: PropertyKeyId = PropertyKeyId(2);
type Edge = (VId, RelationId, VId);
type PlainRow = (Vec<GraphValue>, Vec<GraphAggregateValue>);

fn child() -> PreparedGraphPattern<GraphValueRow> {
    let mut builder = GraphPatternBuilder::new();
    for name in ["a", "b", "c"] {
        builder.vertex(name).unwrap();
    }
    builder.edge("a", R, GlaDirection::Forward, "b").unwrap();
    builder.edge("b", S, GlaDirection::Forward, "c").unwrap();
    builder
        .prepare_values(
            &[
                GraphColumn::vertex("owner", "a"),
                GraphColumn::vertex("via", "b"),
                GraphColumn::property("value", "c", VALUE),
                GraphColumn::property("bucket", "a", BUCKET),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates()
}

fn functions() -> [GraphAggregate<'static>; 6] {
    [
        GraphAggregate::count_rows("paths"),
        GraphAggregate::count("present", 2),
        GraphAggregate::count_distinct("unique_values", 2),
        GraphAggregate::sum_int("total", 2),
        GraphAggregate::min("least", 2),
        GraphAggregate::max("greatest", 2),
    ]
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn plain(rows: &[fgdb_gql::GraphAggregateRow]) -> Vec<PlainRow> {
    rows.iter()
        .map(|row| (row.keys().to_vec(), row.values().to_vec()))
        .collect()
}
fn finish(values: &[Option<i64>]) -> Vec<GraphAggregateValue> {
    let present: Vec<_> = values.iter().filter_map(|value| *value).collect();
    let null = GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null));
    vec![
        GraphAggregateValue::Count(values.len() as u64),
        GraphAggregateValue::Count(present.len() as u64),
        GraphAggregateValue::Count(present.iter().copied().collect::<BTreeSet<_>>().len() as u64),
        if present.is_empty() {
            null.clone()
        } else {
            GraphAggregateValue::Integer(present.iter().map(|value| i128::from(*value)).sum())
        },
        present.iter().min().map_or_else(
            || null.clone(),
            |value| GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(*value))),
        ),
        present.iter().max().map_or(null, |value| {
            GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(*value)))
        }),
    ]
}

#[test]
fn grouped_and_global_summaries_match_independent_multigraph_enumeration() {
    let input = child();
    let specs = functions();
    let raw = [Some(-5_i64), None, Some(7)];
    let values = raw.map(|value| value.map(CanonicalScalar::Int));
    let buckets = [
        Some(CanonicalScalar::Int(1)),
        None,
        Some(CanonicalScalar::Int(1)),
    ];
    let universe = [
        (VId(1), R, VId(2)),
        (VId(2), R, VId(2)),
        (VId(3), R, VId(1)),
        (VId(2), S, VId(3)),
        (VId(2), S, VId(1)),
        (VId(1), S, VId(2)),
    ];
    for mut encoded in 0..3_usize.pow(universe.len() as u32) {
        let mut edges = Vec::new();
        for edge in universe {
            for _ in 0..encoded % 3 {
                edges.push(edge);
            }
            encoded /= 3;
        }
        for keys in [vec![], vec![0], vec![3]] {
            let aggregate =
                PreparedGraphAggregate::prepare(input.clone(), &keys, &specs, 0, None).unwrap();
            let actual = aggregate
                .execute_governed(
                    edges.len() as u64,
                    [],
                    edges.iter().copied(),
                    |_, _| Ok::<_, ()>(true),
                    |vid, key| {
                        let at = vid.0 as usize - 1;
                        Ok(if key == VALUE {
                            values[at].as_ref()
                        } else {
                            buckets[at].as_ref()
                        })
                    },
                    wide(),
                    || Ok::<_, ()>(()),
                )
                .unwrap();
            let mut expected: BTreeMap<Vec<GraphValue>, Vec<Option<i64>>> = BTreeMap::new();
            if keys.is_empty() {
                expected.insert(Vec::new(), Vec::new());
            }
            for &(owner, relation, via) in &edges {
                if relation != R {
                    continue;
                }
                for &(source, relation, destination) in &edges {
                    if relation != S || source != via {
                        continue;
                    }
                    let key = match keys.first() {
                        None => vec![],
                        Some(0) => vec![GraphValue::Vertex(owner)],
                        _ => vec![GraphValue::Scalar(
                            buckets[owner.0 as usize - 1]
                                .clone()
                                .unwrap_or(CanonicalScalar::Null),
                        )],
                    };
                    expected
                        .entry(key)
                        .or_default()
                        .push(raw[destination.0 as usize - 1]);
                }
            }
            let expected: Vec<_> = expected
                .into_iter()
                .map(|(key, values)| (key, finish(&values)))
                .collect();
            assert_eq!(plain(&actual.value), expected);
            assert_eq!(actual.rows.result_rows, expected.len() as u64);
        }
    }
}

#[test]
fn global_empty_input_has_a_count_row_but_empty_keyed_input_does_not() {
    for keys in [vec![], vec![0]] {
        let aggregate =
            PreparedGraphAggregate::prepare(child(), &keys, &functions(), 0, None).unwrap();
        let result = aggregate
            .execute_governed(
                0,
                [],
                [],
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(
            plain(&result.value),
            if keys.is_empty() {
                vec![(vec![], finish(&[]))]
            } else {
                vec![]
            }
        );
    }
    let aggregate =
        PreparedGraphAggregate::prepare(child(), &[], &functions(), 0, Some(0)).unwrap();
    let result = aggregate
        .execute_governed(
            0,
            [],
            [],
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert!(result.value.is_empty());
}

#[test]
fn definition_validation_preserves_occurrences_and_group_identity() {
    let input = child();
    assert_eq!(
        PreparedGraphAggregate::prepare(input.clone(), &[], &[], 0, None).unwrap_err(),
        GraphAggregateBuildError::EmptyAggregates
    );
    assert_eq!(
        PreparedGraphAggregate::prepare(input.clone(), &[4], &functions(), 0, None).unwrap_err(),
        GraphAggregateBuildError::UnknownColumn { column: 4 }
    );
    assert_eq!(
        PreparedGraphAggregate::prepare(input.clone(), &[0, 0], &functions(), 0, None).unwrap_err(),
        GraphAggregateBuildError::DuplicateKey { column: 0 }
    );
    assert_eq!(
        PreparedGraphAggregate::prepare(
            input.clone(),
            &[0],
            &[GraphAggregate::count_rows("owner")],
            0,
            None
        )
        .unwrap_err(),
        GraphAggregateBuildError::DuplicateName
    );
    assert_eq!(
        PreparedGraphAggregate::prepare(
            input.clone(),
            &[],
            &[GraphAggregate::count_rows("bad name")],
            0,
            None
        )
        .unwrap_err(),
        GraphAggregateBuildError::InvalidName
    );
    assert_eq!(
        PreparedGraphAggregate::prepare(
            input.clone(),
            &[],
            &[GraphAggregate::sum_int("sum", 99)],
            0,
            None
        )
        .unwrap_err(),
        GraphAggregateBuildError::UnknownColumn { column: 99 }
    );
    assert!(matches!(
        PreparedGraphAggregate::prepare(
            input.clone(),
            &[],
            &[GraphAggregate::count_rows("n"); 66],
            0,
            None
        ),
        Err(GraphAggregateBuildError::TooManyColumns {
            limit: 65,
            observed: 66
        })
    ));
    let mut b = GraphPatternBuilder::new();
    b.vertex("a").unwrap();
    let columns = [GraphColumn::vertex("a", "a")];
    for invalid in [
        b.prepare_values(&columns, 0, None).unwrap(),
        b.prepare_values(&columns, 1, None)
            .unwrap()
            .with_duplicates(),
        b.prepare_values(&columns, 0, Some(1))
            .unwrap()
            .with_duplicates(),
    ] {
        assert_eq!(
            PreparedGraphAggregate::prepare(
                invalid,
                &[],
                &[GraphAggregate::count_rows("n")],
                0,
                None
            )
            .unwrap_err(),
            GraphAggregateBuildError::RequiresUnpaginatedAll
        );
    }
    let a = PreparedGraphAggregate::prepare(
        input.clone(),
        &[0],
        &[GraphAggregate::count_rows("n")],
        0,
        None,
    )
    .unwrap();
    let renamed = PreparedGraphAggregate::prepare(
        input.clone(),
        &[0],
        &[GraphAggregate::count_rows("alias")],
        0,
        None,
    )
    .unwrap();
    assert_eq!(a.canonical_bytes(), renamed.canonical_bytes());
    let changed =
        PreparedGraphAggregate::prepare(input, &[1], &[GraphAggregate::count_rows("n")], 0, None)
            .unwrap();
    assert_ne!(a.canonical_bytes(), changed.canonical_bytes());
    assert!(!format!("{a:?}").contains("owner"));
}

fn fixture() -> Vec<Edge> {
    vec![
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
        (VId(4), R, VId(2)),
        (VId(2), S, VId(3)),
        (VId(2), S, VId(3)),
    ]
}

#[test]
fn exact_limits_and_group_pagination_do_not_limit_child_occurrences() {
    let input = child();
    let aggregate =
        PreparedGraphAggregate::prepare(input.clone(), &[0], &functions(), 0, None).unwrap();
    let scalar = CanonicalScalar::Int(i64::MAX);
    let edges = fixture();
    let run = |policy| {
        aggregate.execute_governed(
            5,
            [],
            edges.iter().copied(),
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(Some(&scalar)),
            policy,
            || Ok::<_, ()>(()),
        )
    };
    let full = run(wide()).unwrap();
    assert_eq!(full.value.len(), 2);
    assert_eq!(full.value[0].get(0).unwrap().as_count(), Some(4));
    assert_eq!(
        full.value[0].get(3).unwrap().as_integer(),
        Some(i128::from(i64::MAX) * 4)
    );
    let exact = GqlQueryPolicy::new(
        5,
        2,
        full.evaluator.work_units,
        full.evaluator.scratch_entries,
    );
    assert_eq!(run(exact).unwrap(), full);
    for policy in [
        GqlQueryPolicy::new(5, 2, full.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(5, 2, u64::MAX, full.evaluator.scratch_entries - 1),
    ] {
        assert!(matches!(run(policy), Err(GqlQueryError::Evaluator(_))));
    }
    assert!(matches!(run(GqlQueryPolicy::new(5, 1, u64::MAX, u64::MAX)),
        Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::ResultRows && error.observed == 2));
    assert!(matches!(
        run(GqlQueryPolicy::new(4, 2, u64::MAX, u64::MAX)),
        Err(GqlQueryError::Rows(_))
    ));
    let paged = PreparedGraphAggregate::prepare(input, &[0], &functions(), 1, Some(1)).unwrap();
    let page = paged
        .execute_governed(
            5,
            [],
            edges,
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(Some(&scalar)),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(page.value, full.value[1..]);
}

#[test]
fn cancellation_at_every_checkpoint_and_late_source_failures_release_no_rows() {
    let aggregate = PreparedGraphAggregate::prepare(child(), &[0], &functions(), 0, None).unwrap();
    let edges = fixture();
    let scalar = CanonicalScalar::Int(3);
    let mut total = 0;
    aggregate
        .execute_governed(
            5,
            [],
            edges.iter().copied(),
            |_, _| Ok::<_, &str>(true),
            |_, _| Ok(Some(&scalar)),
            wide(),
            || {
                total += 1;
                Ok::<_, usize>(())
            },
        )
        .unwrap();
    for stop in 1..=total {
        let mut calls = 0;
        let result = aggregate.execute_governed(
            5,
            [],
            edges.iter().copied(),
            |_, _| Ok::<_, &str>(true),
            |_, _| Ok(Some(&scalar)),
            wide(),
            || {
                calls += 1;
                if calls == stop { Err(stop) } else { Ok(()) }
            },
        );
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(calls, stop);
    }
    let failed = aggregate.execute_governed(
        5,
        [],
        edges,
        |_, _| Ok::<_, &str>(true),
        |vid, _| {
            if vid == VId(4) {
                Err("late property source")
            } else {
                Ok(Some(&scalar))
            }
        },
        GqlQueryPolicy::new(5, 0, u64::MAX, u64::MAX),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        failed,
        Err(GqlQueryError::Source(GraphAggregateError::Source(
            "late property source"
        )))
    ));
}

#[test]
fn sum_refuses_nonintegers_instead_of_coercing_or_dropping_them() {
    let aggregate = PreparedGraphAggregate::prepare(child(), &[], &functions(), 0, None).unwrap();
    let scalar = CanonicalScalar::Bool(true);
    let result = aggregate.execute_governed(
        5,
        [],
        fixture(),
        |_, _| Ok::<_, ()>(true),
        |_, _| Ok(Some(&scalar)),
        wide(),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum {
            aggregate: 3
        }))
    ));
}

#[test]
fn aggregation_retains_groups_not_the_entire_parallel_path_bag() {
    let input = child();
    let aggregate =
        PreparedGraphAggregate::prepare(input.clone(), &[0], &functions(), 0, None).unwrap();
    let mut edges = vec![(VId(1), R, VId(2)); 64];
    edges.extend(vec![(VId(2), S, VId(3)); 64]);
    let scalar = CanonicalScalar::Int(7);
    let grouped = aggregate
        .execute_governed(
            128,
            [],
            edges.iter().copied(),
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(Some(&scalar)),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(grouped.value[0].get(0).unwrap().as_count(), Some(4096));
    let bag = input
        .plan()
        .execute_governed_with_properties(
            128,
            [],
            edges,
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(Some(&scalar)),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(bag.value.len(), 4096);
    assert!(grouped.evaluator.scratch_entries < bag.evaluator.scratch_entries / 10);
}
