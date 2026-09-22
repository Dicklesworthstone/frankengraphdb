//! Visible-output DISTINCT must choose its original ranked representative.
use super::super::*;
use fgdb_gql::{
    GraphAggregate, GraphAggregateColumn as Column, GraphAggregateOrder as Order,
    GraphIntegerBinary as Binary, GraphIntegerExpression, GraphIntegerOp as Op, GraphSetProjection,
    GraphSetValue,
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
fn query(offset: u64, count: Option<u64>, descending: bool) -> PreparedGraphAggregate {
    let raw = prepare("MATCH (a)-[r:R]->(b) RETURN b,COUNT(*) AS n,SUM(r.p) AS total GROUP BY b");
    let column = raw.aggregates()[1].argument_column().unwrap();
    PreparedGraphAggregate::prepare(
        raw.input_pattern().clone(),
        raw.group_key_columns(),
        &[
            GraphAggregate::count_rows("n"),
            GraphAggregate::sum_int("total", column),
        ],
        offset,
        count,
    )
    .unwrap()
    .with_result_clauses(
        &[],
        &[Order {
            column: Column::Aggregate(1),
            descending,
            nulls: fgdb_gql::GraphNullPlacement::Last,
        }],
    )
    .unwrap()
    .with_key_output_columns(&[])
    .unwrap()
    .with_aggregate_output_prefix(1)
    .unwrap()
    .with_distinct_output(true)
}

#[test]
fn completed_group_distinct_matches_batch_for_hidden_sort_columns_and_all_windows() {
    for mask in 0..64 {
        for descending in [false, true] {
            for (offset, count) in [(0, Some(0)), (0, Some(1)), (1, Some(2)), (0, None)] {
                let q = query(offset, count, descending);
                let before = q.canonical_bytes();
                let s = source(mask);
                let expected = eager(&q, &s);
                let mut cursor = run(&q, s, wide());
                let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                assert_eq!(rows, expected);
                assert_eq!(cursor.row_stats().result_rows, rows.len() as u64);
                assert_eq!(q.canonical_bytes(), before);
            }
        }
    }
    // A later group replaces the earlier representative of the COUNT=1 class.
    // Limiting/deduplicating before ranking would wrongly select COUNT=2 first.
    let q = query(0, Some(2), true);
    let mut s = source(0);
    for (id, to, value) in [(1, 0, 0), (2, 1, 5), (3, 1, 5), (4, u128::MAX, 100)] {
        s.edges.insert(
            EId(id),
            (VId(0), R, VId(to), vec![(P, CanonicalScalar::Int(value))]),
        );
    }
    let result = run(&q, s.clone(), wide())
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        result
            .iter()
            .map(|r| r.values()[0].as_count().unwrap())
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(
        run(&query(1, Some(1), true), s, wide())
            .next()
            .unwrap()
            .unwrap()
            .values()[0]
            .as_count(),
        Some(2)
    );
}

#[test]
fn distinct_after_projection_handles_empty_tuples_nulls_and_computed_keys_without_narrowing() {
    for text in [
        "MATCH (a)-[r:R]->(b) RETURN DISTINCT COUNT(*) AS n GROUP BY b",
        "MATCH (a)-[r:R]->(b) RETURN DISTINCT AVG(r.p) AS mean GROUP BY b ORDER BY SUM(r.p) DESC NULLS FIRST",
        "MATCH (a)-[r:R]->(b)-[s:S]->(c) RETURN DISTINCT SUM(r.p)+1 AS total GROUP BY c HAVING COUNT(*)>0 ORDER BY SUM(r.p) DESC",
        "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[:S*1..3]->(c) } RETURN DISTINCT COUNT(*)%2 AS parity GROUP BY b ORDER BY COUNT(*) DESC",
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
    let q = query(0, None, true)
        .with_aggregate_output_prefix(0)
        .unwrap();
    let rows = run(
        &q,
        source(63),
        GqlQueryPolicy::new(100, 1, u64::MAX, u64::MAX),
    )
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].keys().is_empty() && rows[0].values().is_empty());
}

#[test]
fn distinct_class_storage_is_not_output_quota_and_late_failures_never_publish_a_page() {
    let q = prepare(
        "MATCH (a)-[r:R]->(b) RETURN DISTINCT COUNT(*) AS n GROUP BY b ORDER BY SUM(r.p) DESC LIMIT 1",
    );
    let mut s = source(0);
    for id in 0..1024 {
        s.vertices.insert(VId(id), vec![]);
        s.edges.insert(
            EId(id),
            (
                VId(0),
                R,
                VId(id),
                vec![(P, CanonicalScalar::Int(id as i64))],
            ),
        );
    }
    let drops = s.drops.clone();
    let mut cursor = run(&q, s, GqlQueryPolicy::new(1024, 1, u64::MAX, u64::MAX));
    assert_eq!(
        cursor.next().unwrap().unwrap().values()[0].as_count(),
        Some(1)
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(cursor.next().is_none());
    for count in [0, 1] {
        let q = prepare(&format!(
            "MATCH (a)-[r:R]->(b) RETURN DISTINCT 1/(COUNT(*)-1) AS value GROUP BY b ORDER BY COUNT(*) DESC LIMIT {count}"
        ));
        let mut s = source(0);
        for (id, to) in [(1, 0), (2, 0), (3, 1)] {
            s.edges.insert(EId(id), (VId(0), R, VId(to), vec![]));
        }
        let mut cursor = run(&q, s, wide());
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Source(
                GraphAggregateError::OutputExpression { .. }
            )))
        ));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.next().is_none());
    }
    let q = prepare("MATCH (a)-[r:R]->(b) RETURN DISTINCT COUNT(*) AS n ORDER BY n LIMIT 0");
    let mut bad = source(63);
    bad.fail = Some(EId(6));
    let mut cursor = run(&q, bad, wide());
    assert!(cursor.next().unwrap().is_err());
    assert_eq!(cursor.row_stats().result_rows, 0);
}

#[test]
fn distinct_exact_limits_every_checkpoint_and_close_preserve_one_query_allowance() {
    let q = query(0, Some(2), true)
        .with_output_projection(vec![GraphSetProjection::new(
            "parity",
            GraphSetValue::Integer(
                GraphIntegerExpression::prepare(&[
                    Op::Column(1),
                    Op::Literal(Some(2)),
                    Op::Binary(Binary::Remainder),
                ])
                .unwrap(),
            ),
        )])
        .unwrap();
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
        let dropped = s.drops.clone();
        let mut seen = 0;
        let mut cursor =
            EdgeAggregateCursor::new(s, EdgeAggregatePlan::compile(&q).unwrap(), exact, || {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            });
        let mut delivered = Vec::new();
        loop {
            match cursor.next().expect("injected boundary must execute") {
                Ok(row) => delivered.push(row),
                Err(GqlQueryError::Interrupted(at)) => {
                    assert_eq!(at, stop);
                    break;
                }
                Err(error) => panic!("unexpected: {error:?}"),
            }
        }
        assert!(expected.starts_with(&delivered));
        assert_eq!(cursor.row_stats().result_rows, delivered.len() as u64);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
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
        assert!(cursor.next().is_none());
    }
    let s = source(63);
    let reads = s.reads.clone();
    let mut cursor = run(&q, s, wide());
    cursor.close();
    cursor.close();
    assert!(cursor.next().is_none());
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}
