//! Final result clauses must not be pushed into matched rows or partial groups.
use super::*;
use crate::algebra::IntegerComparison as Comparison;
use crate::{
    GraphAggregate, GraphAggregateColumn as Column, GraphAggregateFilter, GraphAggregateTest,
    GraphHavingExpression, GraphHavingOp as Op, GraphHavingOperand as Operand,
    GraphIntegerBinary as Binary, GraphIntegerExpression, GraphIntegerOp as IntOp,
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
        |id, tests| {
            Ok::<_, ()>(
                tests
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
fn definition(
    pattern: &str,
    offset: u64,
    count: Option<u64>,
    minimum: i128,
) -> PreparedGraphAggregate {
    let end = if pattern.contains("(c)") { "c" } else { "b" };
    let raw = prepare(&format!(
        "MATCH {pattern} RETURN {end} AS key,COUNT(*) AS n,SUM(r.p) AS total,AVG(r.p) AS average GROUP BY {end}"
    ));
    let value = raw.aggregates()[1].argument_column().unwrap();
    let q = PreparedGraphAggregate::prepare(
        raw.input_pattern().clone(),
        raw.group_key_columns(),
        &[
            GraphAggregate::count_rows("n"),
            GraphAggregate::sum_int("total", value),
            GraphAggregate::average_int("average", value),
        ],
        offset,
        count,
    )
    .unwrap();
    q.with_having_expression(
        &GraphHavingExpression::prepare(&[
            Op::Compare {
                left: Operand::Column(Column::Aggregate(0)),
                comparison: Comparison::GreaterOrEqual,
                right: Operand::Integer(minimum),
            },
            Op::Compare {
                left: Operand::Column(Column::Aggregate(2)),
                comparison: Comparison::Greater,
                right: Operand::Integer(1),
            },
            Op::IsNull {
                operand: Operand::Column(Column::Aggregate(1)),
                is_null: true,
            },
            Op::Or,
            Op::And,
        ])
        .unwrap(),
    )
    .unwrap()
}
fn plain(rows: &[GraphAggregateRow]) -> Vec<(Vec<GraphValue>, Vec<GraphAggregateValue>)> {
    rows.iter()
        .map(|r| (r.keys().to_vec(), r.values().to_vec()))
        .collect()
}
// Independent occurrence enumeration; arithmetic filtering and the window are
// applied ONLY to completed grouped bags, never a production GLA/VM/cursor.
fn oracle(
    s: &Source,
    dir: usize,
    hops: usize,
    min: i128,
    offset: usize,
    count: usize,
) -> Vec<(Vec<GraphValue>, Vec<GraphAggregateValue>)> {
    let oriented: Vec<_> = s
        .edges
        .values()
        .flat_map(|(a, r, b, p)| {
            let v = integer(p);
            if dir == 1 {
                vec![(*b, *r, *a, v)]
            } else if dir == 2 && a != b {
                vec![(*a, *r, *b, v), (*b, *r, *a, v)]
            } else {
                vec![(*a, *r, *b, v)]
            }
        })
        .collect();
    let mut groups = BTreeMap::<VId, Vec<Option<i128>>>::new();
    for &(_, r, b, v) in &oriented {
        if r != R {
            continue;
        }
        if hops == 1 {
            groups.entry(b).or_default().push(v);
        } else {
            for &(from, rel, to, _) in &oriented {
                if from == b && rel == S {
                    groups.entry(to).or_default().push(v);
                }
            }
        }
    }
    groups
        .into_iter()
        .filter_map(|(key, bag)| {
            let n = bag.len() as u64;
            let nonnull: Vec<_> = bag.into_iter().flatten().collect();
            let total: i128 = nonnull.iter().sum();
            if i128::from(n) < min || (!nonnull.is_empty() && total <= nonnull.len() as i128) {
                return None;
            }
            let sum = if nonnull.is_empty() {
                sum(None)
            } else {
                sum(Some(total))
            };
            let average = if nonnull.is_empty() {
                sum.clone()
            } else {
                GraphAggregateValue::Average(
                    crate::GraphExactAverage::new(total, nonnull.len() as u64).unwrap(),
                )
            };
            Some((
                vec![GraphValue::Vertex(key)],
                vec![GraphAggregateValue::Count(n), sum, average],
            ))
        })
        .skip(offset)
        .take(count)
        .collect()
}

#[test]
fn complete_having_and_windows_match_independent_groups_and_the_batch_engine() {
    for mask in 0..64 {
        for dir in 0..3 {
            let edge = |name, rel, end| match dir {
                0 => format!("-[{name}:{rel}]->({end})"),
                1 => format!("<-[{name}:{rel}]-({end})"),
                _ => format!("-[{name}:{rel}]-({end})"),
            };
            for hops in 1..=2 {
                let mut pattern = format!("(a){}", edge("r", "R", "b"));
                if hops == 2 {
                    pattern += &edge("s", "S", "c");
                }
                for minimum in [0, 2] {
                    for (offset, count) in [(0, 0), (0, 1), (1, 3)] {
                        let q = definition(&pattern, offset, Some(count), minimum);
                        let before = q.canonical_bytes();
                        let s = source(mask);
                        let expected =
                            oracle(&s, dir, hops, minimum, offset as usize, count as usize);
                        let batch = eager(&q, &s);
                        let mut cursor = run(&q, s, wide());
                        let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                        assert_eq!(plain(&rows), expected);
                        assert_eq!(rows, batch);
                        assert_eq!(cursor.row_stats().result_rows, rows.len() as u64);
                        assert_eq!(q.canonical_bytes(), before);
                        assert!(!q.supports_incremental_maintenance());
                    }
                }
            }
        }
    }
}

#[test]
fn hidden_keys_and_aggregates_keep_group_bags_and_computed_outputs_keep_wide_cells() {
    let q = definition("(a)-[r:R]->(b)", 0, None, 0);
    for q in [
        q.clone()
            .with_key_output_columns(&[])
            .unwrap()
            .with_aggregate_output_prefix(1)
            .unwrap(),
        q.clone().with_key_output_columns(&[0, 0]).unwrap(),
        q.with_output_projection(vec![
            GraphSetProjection::new(
                "next",
                GraphSetValue::Integer(
                    GraphIntegerExpression::prepare(&[
                        IntOp::Column(1),
                        IntOp::Literal(Some(1)),
                        IntOp::Binary(Binary::Add),
                    ])
                    .unwrap(),
                ),
            ),
            GraphSetProjection::new("exact", GraphSetValue::Column(3)),
        ])
        .unwrap(),
    ] {
        let s = source(63);
        let expected = eager(&q, &s);
        assert_eq!(
            run(&q, s, wide()).collect::<Result<Vec<_>, _>>().unwrap(),
            expected
        );
    }
    let raw = prepare("MATCH (a)-[r:R]->(b) RETURN b,COUNT(*) AS n GROUP BY b");
    let hidden = raw.with_key_output_columns(&[]).unwrap();
    let rows = run(&hidden, source(63), wide())
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], rows[1]); // Two groups, not DISTINCT output.
}

#[test]
fn later_having_and_projection_errors_are_not_hidden_by_a_full_or_empty_page() {
    for count in [0, 1] {
        let raw = prepare("MATCH (a)-[r:R]->(b) RETURN b,MIN(r.p) AS lowest GROUP BY b");
        let q = PreparedGraphAggregate::prepare(
            raw.input_pattern().clone(),
            raw.group_key_columns(),
            &[GraphAggregate::min(
                "lowest",
                raw.aggregates()[0].argument_column().unwrap(),
            )],
            0,
            Some(count),
        )
        .unwrap()
        .with_having_expression(
            &GraphHavingExpression::prepare(&[
                Op::Compare {
                    left: Operand::Column(Column::Aggregate(0)),
                    comparison: Comparison::Greater,
                    right: Operand::Integer(0),
                },
                Op::Truth(Some(true)),
                Op::Or,
            ])
            .unwrap(),
        )
        .unwrap();
        let mut s = source(0);
        s.edges.insert(
            EId(1),
            (VId(0), R, VId(0), vec![(P, CanonicalScalar::Int(1))]),
        );
        s.edges.insert(
            EId(2),
            (
                VId(0),
                R,
                VId(1),
                vec![(
                    P,
                    CanonicalScalar::ucs_basic_text("private operand").unwrap(),
                )],
            ),
        );
        let mut cursor = run(&q, s, wide());
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Source(
                GraphAggregateError::NonIntegerHaving { predicate: 0 }
            )))
        ));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert!(cursor.next().is_none());
        let raw = prepare("MATCH (a)-[r:R]->(b) RETURN b,COUNT(*) AS n GROUP BY b");
        let q = PreparedGraphAggregate::prepare(
            raw.input_pattern().clone(),
            raw.group_key_columns(),
            &[GraphAggregate::count_rows("n")],
            0,
            Some(count),
        )
        .unwrap()
        .with_output_projection(vec![GraphSetProjection::new(
            "bad",
            GraphSetValue::Integer(
                GraphIntegerExpression::prepare(&[
                    IntOp::Literal(Some(1)),
                    IntOp::Column(1),
                    IntOp::Literal(Some(1)),
                    IntOp::Binary(Binary::Subtract),
                    IntOp::Binary(Binary::Divide),
                ])
                .unwrap(),
            ),
        )])
        .unwrap();
        let mut s = source(0);
        for (id, to) in [(1, 0), (2, 0), (3, 1)] {
            s.edges.insert(EId(id), (VId(0), R, VId(to), vec![]));
        }
        let mut cursor = run(&q, s, wide());
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Source(
                GraphAggregateError::OutputExpression { column: 0, .. }
            )))
        ));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert!(cursor.completed.is_none());
    }
}

#[test]
fn legacy_filters_and_false_having_preserve_eager_numeric_domain_checks() {
    let raw = prepare("MATCH (a)-[r:R]->(b) RETURN b,MIN(r.p) AS lowest GROUP BY b");
    let q = raw
        .with_result_clauses(
            &[
                GraphAggregateFilter {
                    column: Column::Aggregate(0),
                    test: GraphAggregateTest::IsNull,
                },
                GraphAggregateFilter {
                    column: Column::Aggregate(0),
                    test: GraphAggregateTest::Integer {
                        comparison: Comparison::Greater,
                        value: 1,
                    },
                },
            ],
            &[],
        )
        .unwrap();
    let mut s = source(63);
    s.edges.get_mut(&EId(1)).unwrap().3 = vec![(P, CanonicalScalar::Bool(true))];
    let mut cursor = run(&q, s, wide());
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(
            GraphAggregateError::NonIntegerHaving { predicate: 1 }
        )))
    ));
    assert_eq!(cursor.row_stats().result_rows, 0);
}

#[test]
fn all_checkpoints_and_one_less_limits_discard_tentative_results_and_fuse() {
    let q = definition("(a)-[r:R]->(b)", 0, Some(2), 0);
    let mut calls = 0;
    let mut full = EdgeAggregateCursor::new(
        source(63),
        EdgeAggregatePlan::compile(&q).unwrap(),
        wide(),
        || {
            calls += 1;
            Ok::<_, usize>(())
        },
    );
    let expected = full.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    let r = full.row_stats();
    let e = full.evaluator_stats();
    drop(full);
    assert!(!expected.is_empty());
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
    for stop in 1..=calls {
        let s = source(63);
        let drops = s.drops.clone();
        let mut seen = 0;
        let mut cursor =
            EdgeAggregateCursor::new(s, EdgeAggregatePlan::compile(&q).unwrap(), exact, || {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            });
        let mut delivered = Vec::new();
        loop {
            match cursor.next().expect("selected checkpoint must be reached") {
                Ok(row) => delivered.push(row),
                Err(GqlQueryError::Interrupted(at)) => {
                    assert_eq!(at, stop);
                    break;
                }
                Err(error) => panic!("unexpected failure: {error:?}"),
            }
        }
        assert!(expected.starts_with(&delivered));
        assert_eq!(cursor.row_stats().result_rows, delivered.len() as u64);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.pending.is_none() && cursor.completed.is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(cursor.next().is_none());
        drop(cursor);
        assert_eq!(seen, stop);
    }
    for p in [
        GqlQueryPolicy::new(r.snapshot_records - 1, r.result_rows, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, r.result_rows - 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, u64::MAX, e.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, e.scratch_entries - 1),
    ] {
        let mut cursor = run(&q, source(63), p);
        assert!(cursor.by_ref().collect::<Result<Vec<_>, _>>().is_err());
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.next().is_none());
    }
}

#[test]
fn empty_and_zero_pages_validate_the_source_but_only_selected_rows_consume_the_row_limit() {
    for text in [
        "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n HAVING n>0",
        "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n LIMIT 0",
        "MATCH (a)-[r:R]->(b) RETURN b,COUNT(*) AS n GROUP BY b HAVING n>0",
    ] {
        let q = prepare(text);
        let mut cursor = run(&q, source(0), GqlQueryPolicy::new(0, 0, u64::MAX, u64::MAX));
        assert!(cursor.next().is_none());
        assert_eq!(cursor.row_stats().result_rows, 0);
    }
    let q = prepare("MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n LIMIT 0");
    let mut s = source(63);
    s.fail = Some(EId(6));
    let mut cursor = run(&q, s, wide());
    assert!(cursor.next().unwrap().is_err());
    assert_eq!(cursor.row_stats().result_rows, 0);
    let q = prepare("MATCH (a)-[r:R]->(b) RETURN b,COUNT(*) AS n GROUP BY b SKIP 1023 LIMIT 1");
    let mut s = source(0);
    for id in 0..1024 {
        s.vertices.insert(VId(id), vec![]);
        s.edges.insert(EId(id), (VId(0), R, VId(id), vec![]));
    }
    let drops = s.drops.clone();
    let mut cursor = run(&q, s, GqlQueryPolicy::new(1024, 1, u64::MAX, u64::MAX));
    let row = cursor.next().unwrap().unwrap();
    assert_eq!(row.keys(), &[GraphValue::Vertex(VId(1023))]);
    assert_eq!(cursor.row_stats().result_rows, 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(cursor.next().is_none());
}

#[test]
fn close_discards_validated_pages_without_more_demand_and_other_profiles_remain_rejected() {
    let q = prepare("MATCH (a)-[r:R]->(b) RETURN b,COUNT(*) AS n GROUP BY b LIMIT 2");
    let mut cursor = run(&q, source(63), wide());
    cursor.next().unwrap().unwrap();
    assert!(cursor.completed.is_some());
    let before = (cursor.row_stats(), cursor.evaluator_stats());
    cursor.close();
    cursor.close();
    assert!(cursor.completed.is_none());
    assert!(cursor.next().is_none());
    assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), before);
    let s = source(63);
    let reads = s.reads.clone();
    let mut cursor = run(&q, s, wide());
    cursor.close();
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    assert!(EdgeAggregatePlan::compile(&q.clone().with_distinct_output(true)).is_err());
    assert!(
        EdgeAggregatePlan::compile(&prepare(
            "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n ORDER BY n"
        ))
        .is_err()
    );
}
