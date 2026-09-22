//! Scalar projections run on complete bindings, before grouping and DISTINCT.
use super::*;
use crate::{
    GraphAggregate, GraphExactAverage, GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp,
    GraphSetProjection, GraphSetValue,
};

fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn eager(q: &PreparedGraphAggregate, s: &Source) -> Vec<GraphAggregateRow> {
    q.execute_governed_with_element_properties(
        s.edges.len() as u64,
        s.vertices.keys().copied(),
        s.edges.iter().map(|(&id, (a, r, b, _))| (id, *a, *r, *b)),
        |id, predicates| {
            Ok::<_, ()>(
                predicates
                    .iter()
                    .all(|p| p.matches_borrowed([], s.vertices[&id].iter().map(|(k, v)| (*k, v)))),
            )
        },
        |id, key| {
            Ok(s.vertices[&id]
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v))
        },
        |id, key| {
            Ok(s.edges[&id]
                .3
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v))
        },
        wide(),
        || Ok::<_, ()>(()),
    )
    .unwrap()
    .value
}
fn projected(
    columns: Vec<GraphSetProjection>,
    keys: &[usize],
    specs: &[GraphAggregate<'_>],
) -> PreparedGraphAggregate {
    let input = prepare("MATCH (a)-[r:R]->(b) RETURN SUM(r.p) AS total")
        .input_pattern()
        .clone();
    PreparedGraphAggregate::prepare_projected(input, columns, keys, specs, 0, None).unwrap()
}
fn expression(ops: &[GraphIntegerOp]) -> GraphSetValue {
    GraphSetValue::Integer(GraphIntegerExpression::prepare(ops).unwrap())
}
fn null() -> GraphAggregateValue {
    GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null))
}
fn average(values: &[i128]) -> GraphAggregateValue {
    if values.is_empty() {
        null()
    } else {
        GraphAggregateValue::Average(
            GraphExactAverage::new(values.iter().sum(), values.len() as u64).unwrap(),
        )
    }
}
type Answer = (Vec<GraphValue>, Vec<GraphAggregateValue>);
// Complete vertex assignments, then every edge occurrence combination. This
// does not use production GLA slots, scalar bytecode, grouping or join cursors.
fn oracle(s: &Source, hops: usize, dir: GlaDirection, grouped: bool) -> Vec<Answer> {
    let atoms = [(0, R, 1), (1, S, 2), (2, R, 0)];
    let width = if hops == 1 { 2 } else { 3 };
    let domain: Vec<_> = s.vertices.keys().copied().collect();
    let mut groups = BTreeMap::<Vec<GraphValue>, Vec<Option<i128>>>::new();
    if !grouped {
        groups.insert(vec![], vec![]);
    }
    for mut code in 0..domain.len().pow(width) {
        let mut ids = Vec::new();
        for _ in 0..width {
            ids.push(domain[code % domain.len()]);
            code /= domain.len();
        }
        let mut choices = Vec::new();
        for &(from, relation, to) in &atoms[..hops] {
            let matching: Vec<_> = s
                .edges
                .values()
                .filter(|(a, r, b, _)| {
                    *r == relation
                        && match dir {
                            GlaDirection::Forward => *a == ids[from] && *b == ids[to],
                            GlaDirection::Reverse => *b == ids[from] && *a == ids[to],
                            GlaDirection::Undirected => {
                                (*a == ids[from] && *b == ids[to])
                                    || (*b == ids[from] && *a == ids[to])
                            }
                        }
                })
                .collect();
            choices.push(matching);
        }
        let suffix: usize = choices.iter().skip(1).map(Vec::len).product();
        if suffix == 0 || choices[0].is_empty() {
            continue;
        }
        let endpoint = ids[width as usize - 1];
        let key = if grouped {
            vec![GraphValue::Vertex(endpoint)]
        } else {
            vec![]
        };
        let output = groups.entry(key).or_default();
        let right = integer(&s.vertices[&endpoint]);
        for edge in &choices[0] {
            let value = integer(&edge.3).zip(right).map(|(a, b)| a * b + 2);
            output.extend(std::iter::repeat_n(value, suffix));
        }
    }
    groups
        .into_iter()
        .map(|(key, all)| {
            let values: Vec<_> = all.iter().flatten().copied().collect();
            let distinct: Vec<_> = values
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let total = |v: &[i128]| {
                if v.is_empty() {
                    null()
                } else {
                    GraphAggregateValue::Integer(v.iter().sum())
                }
            };
            (
                key,
                vec![
                    GraphAggregateValue::Count(all.len() as u64),
                    GraphAggregateValue::Count(values.len() as u64),
                    total(&values),
                    average(&values),
                    GraphAggregateValue::Count(distinct.len() as u64),
                    total(&distinct),
                    average(&distinct),
                ],
            )
        })
        .collect()
}

#[test]
fn computed_arguments_match_full_assignment_and_eager_oracles_across_join_shapes() {
    for mask in 0..64 {
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            for hops in 1..=3 {
                let atom = |name, relation, target| match direction {
                    GlaDirection::Forward => format!("-[{name}:{relation}]->({target})"),
                    GlaDirection::Reverse => format!("<-[{name}:{relation}]-({target})"),
                    GlaDirection::Undirected => format!("-[{name}:{relation}]-({target})"),
                };
                let mut pattern = format!("(a){}", atom("r", "R", "b"));
                if hops > 1 {
                    pattern += &atom("s", "S", "c");
                }
                if hops > 2 {
                    pattern += &atom("t", "R", "a");
                }
                let endpoint = if hops == 1 { "b" } else { "c" };
                let value = format!("r.p * {endpoint}.p + 2");
                for grouped in [false, true] {
                    let key = if grouped {
                        format!("{endpoint} AS endpoint, ")
                    } else {
                        String::new()
                    };
                    let clause = if grouped {
                        format!(" GROUP BY {endpoint}")
                    } else {
                        String::new()
                    };
                    let q = prepare(&format!(
                        "MATCH {pattern} RETURN {key}COUNT(*) AS rows, COUNT({value}) AS present, SUM({value}) AS total, AVG({value}) AS average, COUNT(DISTINCT {value}) AS support, SUM(DISTINCT {value}) AS distinct_total, AVG(DISTINCT {value}) AS distinct_average{clause}"
                    ));
                    assert!(q.input_projection().is_some());
                    assert!(!q.supports_incremental_maintenance());
                    let s = source(mask);
                    let expected = oracle(&s, hops, direction, grouped);
                    let batch = eager(&q, &s);
                    let mut cursor = run(&q, s, wide());
                    let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                    assert_eq!(rows, batch);
                    assert_eq!(
                        rows.iter()
                            .map(|r| (r.keys().to_vec(), r.values().to_vec()))
                            .collect::<Vec<_>>(),
                        expected
                    );
                    assert_eq!(cursor.row_stats().result_rows, rows.len() as u64);
                    assert!(cursor.next().is_none());
                }
            }
        }
    }
}

#[test]
fn computed_keys_and_reordered_columns_are_not_source_positions_or_preprojection_distinct() {
    let square = expression(&[
        GraphIntegerOp::Column(0),
        GraphIntegerOp::Column(0),
        GraphIntegerOp::Binary(GraphIntegerBinary::Multiply),
    ]);
    let q = projected(
        vec![
            GraphSetProjection::new("bucket", square),
            GraphSetProjection::new("constant", expression(&[GraphIntegerOp::Literal(Some(7))])),
            GraphSetProjection::new("original", GraphSetValue::Column(0)),
        ],
        &[0],
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::count_distinct("support", 0),
            GraphAggregate::sum_int("total", 1),
            GraphAggregate::sum_int("original_total", 2),
        ],
    );
    let mut s = source(3);
    s.edges.get_mut(&EId(2)).unwrap().3 = vec![(P, CanonicalScalar::Int(-5))];
    let batch = eager(&q, &s);
    let mut cursor = run(&q, s, wide());
    let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(rows, batch);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].keys(),
        &[GraphValue::Scalar(CanonicalScalar::Int(25))]
    );
    assert_eq!(
        rows[0].values(),
        &[
            GraphAggregateValue::Count(2),
            GraphAggregateValue::Count(1),
            GraphAggregateValue::Integer(14),
            GraphAggregateValue::Integer(0)
        ]
    );
    for text in [
        "MATCH (a)-[r:R]->(b) RETURN r AS edge, MIN(r.p+1) AS low, MAX(r) AS identity GROUP BY r",
        "MATCH path=(a)-[r:R]->(b) RETURN path AS route, COUNT(DISTINCT path) AS support, SUM(r.p+1) AS total GROUP BY path",
    ] {
        let q = prepare(text);
        let s = source(63);
        let batch = eager(&q, &s);
        assert_eq!(
            run(&q, s, wide()).collect::<Result<Vec<_>, _>>().unwrap(),
            batch
        );
    }
}

#[test]
fn unused_expression_errors_are_not_skipped_and_lazy_case_keeps_the_unselected_arm_dormant() {
    let bad = expression(&[
        GraphIntegerOp::Literal(Some(1)),
        GraphIntegerOp::Literal(Some(0)),
        GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
    ]);
    let q = projected(
        vec![GraphSetProjection::new("unused", bad.clone())],
        &[],
        &[GraphAggregate::count_rows("rows")],
    );
    let mut cursor = run(&q, source(1), wide());
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(
            GraphAggregateError::InputExpression {
                row: 0,
                column: 0,
                ..
            }
        )))
    ));
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert!(cursor.next().is_none());
    let mut empty = run(&q, source(0), wide());
    assert_eq!(
        empty.next().unwrap().unwrap().values()[0].as_count(),
        Some(0)
    );
    let lazy = expression(&[
        GraphIntegerOp::Truth(Some(true)),
        GraphIntegerOp::Column(0),
        GraphIntegerOp::Literal(Some(1)),
        GraphIntegerOp::Literal(Some(0)),
        GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
        GraphIntegerOp::Case,
    ]);
    let q = projected(
        vec![GraphSetProjection::new("chosen", lazy)],
        &[],
        &[GraphAggregate::sum_int("total", 0)],
    );
    assert_eq!(
        run(&q, source(63), wide())
            .next()
            .unwrap()
            .unwrap()
            .values()[0]
            .as_integer(),
        Some(2)
    );
    let q = projected(
        vec![
            GraphSetProjection::new("copied", GraphSetValue::Column(0)),
            GraphSetProjection::new("unused", bad),
        ],
        &[0],
        &[GraphAggregate::count_rows("rows")],
    );
    let mut cursor = run(&q, source(1), wide());
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(
            GraphAggregateError::InputExpression { column: 1, .. }
        )))
    ));
    assert!(cursor.pending.is_none());
}

#[test]
fn predicates_and_probes_precede_projection_but_late_expression_errors_abort_all_groups() {
    for text in [
        "MATCH (a)-[r:R]->(b) WHERE r.p<>0 RETURN SUM(10/r.p) AS total",
        "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[:S*1..3]->(x) } RETURN SUM(r.p*2) AS total",
        "MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (b)-[:S]->(x) WHERE x.p>0 } RETURN b, SUM(r.p+1) AS total GROUP BY b",
    ] {
        let q = prepare(text);
        let mut s = source(63);
        s.edges.get_mut(&EId(6)).unwrap().3 = vec![(P, CanonicalScalar::Int(0))];
        let batch = eager(&q, &s);
        assert_eq!(
            run(&q, s, wide()).collect::<Result<Vec<_>, _>>().unwrap(),
            batch
        );
    }
    for value in [CanonicalScalar::Int(i64::MAX), CanonicalScalar::Bool(true)] {
        let q = prepare("MATCH (a)-[r:R]->(b) RETURN b, SUM(r.p+1) AS total GROUP BY b");
        let mut s = source(63);
        s.edges.get_mut(&EId(6)).unwrap().3 = vec![(P, value)];
        let dropped = s.drops.clone();
        let mut cursor = run(&q, s, wide());
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Source(
                GraphAggregateError::InputExpression { .. }
            )))
        ));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(cursor.pending.is_none());
        assert!(cursor.next().is_none());
    }
}

#[test]
fn projection_grouping_and_delivery_share_exact_budgets_and_terminal_cancellation() {
    let q = prepare(
        "MATCH (a)-[r:R]->(b) RETURN b, SUM(r.p*2) AS total, COUNT(DISTINCT r.p*2) AS support GROUP BY b",
    );
    let mut total = 0;
    let mut full = EdgeAggregateCursor::new(
        source(63),
        EdgeAggregatePlan::compile(&q).unwrap(),
        wide(),
        || {
            total += 1;
            Ok::<_, usize>(())
        },
    );
    let expected = full.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    let r = full.row_stats();
    let e = full.evaluator_stats();
    drop(full);
    let exact = GqlQueryPolicy::new(
        r.snapshot_records,
        r.result_rows,
        e.work_units,
        e.scratch_entries,
    );
    assert_eq!(
        run(&q, source(63), exact)
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        expected
    );
    for limit in [
        GqlQueryPolicy::new(r.snapshot_records - 1, r.result_rows, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, r.result_rows - 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, r.result_rows, e.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, r.result_rows, u64::MAX, e.scratch_entries - 1),
    ] {
        let mut cursor = run(&q, source(63), limit);
        assert!(cursor.by_ref().collect::<Result<Vec<_>, _>>().is_err());
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.next().is_none());
    }
    for stop in 1..=total {
        let s = source(63);
        let dropped = s.drops.clone();
        let mut calls = 0;
        let mut cursor =
            EdgeAggregateCursor::new(s, EdgeAggregatePlan::compile(&q).unwrap(), exact, || {
                calls += 1;
                if calls == stop { Err(stop) } else { Ok(()) }
            });
        let mut delivered = Vec::new();
        loop {
            match cursor.next().expect("injected checkpoint must be reached") {
                Ok(row) => delivered.push(row),
                Err(GqlQueryError::Interrupted(at)) => {
                    assert_eq!(at, stop);
                    break;
                }
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
        assert!(expected.starts_with(&delivered));
        assert_eq!(cursor.row_stats().result_rows, delivered.len() as u64);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(cursor.pending.is_none());
        assert!(cursor.next().is_none());
        drop(cursor);
        assert_eq!(calls, stop);
    }
}

#[test]
fn transformed_input_keeps_supported_result_modifiers_and_maintenance_and_row_scan_boundaries() {
    for text in [
        "MATCH (a)-[r:R]->(b) RETURN SUM(r.p+1) AS total ORDER BY total LIMIT 0",
        "MATCH (a)-[r:R]->(b) RETURN SUM(r.p+1) AS total HAVING total>0 ORDER BY total",
        "MATCH (a)-[r:R]->(b) RETURN SUM(r.p+1) AS total ORDER BY total",
    ] {
        let q = prepare(text);
        let s = source(63);
        let expected = eager(&q, &s);
        assert_eq!(
            run(&q, s, wide()).collect::<Result<Vec<_>, _>>().unwrap(),
            expected,
            "{text}"
        );
    }
    for text in [
        "MATCH (a)-[r:R]->(b) RETURN COLLECT(r.p+1) AS values",
        "MATCH (a)-[r:R]->(b) OPTIONAL MATCH (b)-[:S]->(c) RETURN SUM(r.p+1) AS total",
    ] {
        assert!(
            EdgeAggregatePlan::compile(&prepare(text)).is_err(),
            "{text}"
        );
    }
    let q = prepare("MATCH (a)-[r:R]->(b) RETURN SUM(r.p+1) AS total");
    assert!(!q.supports_incremental_maintenance());
    assert!(EdgeAggregatePlan::compile(&q).is_ok());
    assert!(EdgeAggregatePlan::compile(&q.clone().with_distinct_output(true)).is_ok());
    assert!(EdgeScanPlan::compile(q.input_pattern().plan()).is_err());
    let s = source(63);
    let reads = s.reads.clone();
    let dropped = s.drops.clone();
    let mut cursor = run(&q, s, wide());
    cursor.close();
    cursor.close();
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(cursor.next().is_none());
}

#[test]
fn thousands_of_projected_occurrences_need_one_output_row_not_an_input_result_bag() {
    let q = prepare(
        "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS rows, SUM(r.p+1) AS total, AVG(r.p+1) AS average, COUNT(DISTINCT r.p+1) AS support",
    );
    let mut s = source(0);
    for id in 0..4096 {
        s.edges.insert(
            EId(id),
            (
                VId(0),
                R,
                VId(1),
                vec![(P, CanonicalScalar::Int(i64::MAX - 1))],
            ),
        );
    }
    let mut cursor = run(&q, s, policy());
    let row = cursor.next().unwrap().unwrap();
    assert_eq!(
        row.values(),
        &[
            GraphAggregateValue::Count(4096),
            GraphAggregateValue::Integer(4096 * i128::from(i64::MAX)),
            GraphAggregateValue::Average(GraphExactAverage::new(i128::from(i64::MAX), 1).unwrap()),
            GraphAggregateValue::Count(1)
        ]
    );
    assert_eq!(cursor.row_stats().result_rows, 1);
    assert!(cursor.next().is_none());
}
