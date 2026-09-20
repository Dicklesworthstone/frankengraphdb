//! Group boundaries, native value support, and the pull lifecycle are checked
//! independently of the shared aggregate cells. Reuse only the indexed source.
use super::*;
use crate::{GraphAggregateFunction as Function, GraphExactAverage};

fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 100_000, 20_000_000, 20_000_000)
}
fn eager(q: &PreparedGraphAggregate, s: &Source) -> Vec<GraphAggregateRow> {
    q.execute_governed_with_element_properties(
        s.edges.len() as u64,
        s.vertices.keys().copied(),
        s.edges.iter().map(|(&id, (a, r, b, _))| (id, *a, *r, *b)),
        |vid, tests| Ok::<_, ()>(tests.iter().all(|test| {
            test.matches_borrowed([], s.vertices[&vid].iter().map(|(k, v)| (*k, v)))
        })),
        |vid, _| Ok(s.vertices[&vid].iter().find(|(key, _)| *key == P).map(|(_, v)| v)),
        |eid, _| Ok(s.edges[&eid].3.iter().find(|(key, _)| *key == P).map(|(_, v)| v)),
        wide(),
        || Ok::<_, ()>(()),
    ).unwrap().value
}
fn exact_average(values: &[i128]) -> GraphAggregateValue {
    if values.is_empty() {
        return sum(None);
    }
    GraphAggregateValue::Average(GraphExactAverage::new(
        values.iter().sum(), values.len() as u64,
    ).unwrap())
}
fn extreme(value: Option<&i128>) -> GraphAggregateValue {
    GraphAggregateValue::Value(GraphValue::Scalar(value.map_or(CanonicalScalar::Null,
        |value| CanonicalScalar::Int(i64::try_from(*value).unwrap()))))
}
fn independent_groups(s: &Source, direction: GlaDirection) -> Vec<(Vec<GraphValue>, Vec<GraphAggregateValue>)> {
    let mut groups: BTreeMap<VId, Vec<Option<i128>>> = BTreeMap::new();
    for (a, relation, b, props) in s.edges.values() {
        if *relation != R { continue; }
        let value = integer(props);
        match direction {
            GlaDirection::Forward => groups.entry(*a).or_default().push(value),
            GlaDirection::Reverse => groups.entry(*b).or_default().push(value),
            GlaDirection::Undirected => {
                groups.entry(*a).or_default().push(value);
                if a != b { groups.entry(*b).or_default().push(value); }
            }
        }
    }
    groups.into_iter().map(|(owner, rows)| {
        let mut present: Vec<i128> = rows.iter().filter_map(|v| *v).collect();
        present.sort_unstable();
        let distinct: Vec<i128> = present.iter().copied().collect::<BTreeSet<_>>().into_iter().collect();
        let values = vec![
            GraphAggregateValue::Count(rows.len() as u64),
            GraphAggregateValue::Count(present.len() as u64),
            GraphAggregateValue::Count(distinct.len() as u64),
            sum((!present.is_empty()).then(|| present.iter().sum())),
            sum((!distinct.is_empty()).then(|| distinct.iter().sum())),
            exact_average(&present), exact_average(&distinct),
            extreme(present.first()), extreme(present.last()),
        ];
        (vec![GraphValue::Vertex(owner)], values)
    }).collect()
}

#[test]
fn all_nine_grouped_functions_match_independent_bags_and_snapshot_execution() {
    for mask in 0..64 {
        for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
            let atom = match direction {
                GlaDirection::Forward => "(a)-[r:R]->(b)",
                GlaDirection::Reverse => "(a)<-[r:R]-(b)",
                GlaDirection::Undirected => "(a)-[r:R]-(b)",
            };
            let q = prepare(&format!("MATCH {atom} RETURN a AS owner,COUNT(*) AS n,COUNT(r.p) AS present,COUNT(DISTINCT r.p) AS d,SUM(r.p) AS total,SUM(DISTINCT r.p) AS dt,AVG(r.p) AS avg,AVG(DISTINCT r.p) AS da,MIN(r.p) AS lo,MAX(r.p) AS hi GROUP BY a"));
            let s = source(mask);
            let expected = eager(&q, &s);
            let independent = independent_groups(&s, direction);
            let mut cursor = run(&q, s, wide());
            assert_eq!(cursor.key_columns(), q.key_columns());
            assert_eq!(cursor.size_hint(), (0, None));
            let actual: Vec<_> = cursor.by_ref().collect::<Result<_, _>>().unwrap();
            assert_eq!(actual, expected, "mask {mask}, {direction:?}");
            let rows: Vec<_> = actual.iter().map(|row| (row.keys().to_vec(), row.values().to_vec())).collect();
            assert_eq!(rows, independent, "mask {mask}, {direction:?}");
            assert_eq!(cursor.row_stats().result_rows, actual.len() as u64);
            assert_eq!(cursor.state(), EdgeScanState::Exhausted);
            assert!(cursor.next().is_none());
        }
    }
}

#[test]
fn joins_probes_composite_null_keys_and_edge_path_domains_keep_full_semantics() {
    for text in [
        "MATCH (a)-[r:R]->(b)-[s:S]->(c) RETURN a AS owner,c.p AS bucket,COUNT(*) AS n,COUNT(DISTINCT b) AS b_count,AVG(r.p) AS avg,MIN(s) AS low,MAX(s) AS high GROUP BY a,c.p",
        "MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (b)-[:S]->(c) WHERE c.p>0 } RETURN b.p AS bucket,COUNT(DISTINCT r) AS n,MIN(r) AS low,MAX(r) AS high GROUP BY b.p",
        "MATCH p=(a)-[:R]->(b)-[:S]->(c) RETURN a AS owner,COUNT(DISTINCT p) AS n,MIN(p) AS low,MAX(p) AS high GROUP BY a",
        "MATCH p=(a)-[:R]->(b) RETURN p AS path,COUNT(*) AS n GROUP BY p",
        "MATCH (a)-[r:R]->(b) RETURN r AS edge,COUNT(*) AS n GROUP BY r",
    ] {
        let q = prepare(text);
        for mask in [0, 3, 7, 47, 63] {
            let s = source(mask);
            let expected = eager(&q, &s);
            let actual: Vec<_> = run(&q, s, wide()).collect::<Result<_, _>>().unwrap();
            assert_eq!(actual, expected, "{text}, mask {mask}");
        }
    }
}

#[test]
fn shared_cells_keep_nested_and_identity_domains_distinct_without_numeric_coercion() {
    let scalar = GraphValue::Scalar(CanonicalScalar::Int(7));
    let vertex = GraphValue::Vertex(VId(7));
    let edge = GraphValue::Edge(EId(7));
    let list = GraphValue::List(vec![scalar.clone(), GraphValue::List(vec![edge.clone()].into())].into());
    let values = [scalar.clone(), vertex, edge, list.clone(), list,
        GraphValue::Scalar(CanonicalScalar::Null)];
    let mut allow = |_| Ok::<_, GqlQueryError<GraphAggregateError<()>, usize>>(());
    let mut count = NumericState::new_governed(Function::CountDistinct, &mut allow).unwrap();
    let mut minimum = NumericState::new_governed(Function::Min, &mut allow).unwrap();
    let mut maximum = NumericState::new_governed(Function::Max, &mut allow).unwrap();
    // A real static column has one declared domain. DISTINCT is nevertheless
    // disjoint across all native forms; extrema here use one list domain.
    for value in &values {
        count.update_governed(Input::from_value(value), 0, &mut allow).unwrap();
    }
    let small = GraphValue::List(vec![scalar].into());
    let large = &values[3];
    for value in [&small, large, &small] {
        minimum.update_governed(Input::from_value(value), 1, &mut allow).unwrap();
        maximum.update_governed(Input::from_value(value), 2, &mut allow).unwrap();
    }
    assert_eq!(count.finish_governed(&mut allow).unwrap(), GraphAggregateValue::Count(4));
    assert_eq!(minimum.finish_governed(&mut allow).unwrap(), GraphAggregateValue::Value(small));
    assert_eq!(maximum.finish_governed(&mut allow).unwrap(), GraphAggregateValue::Value(large.clone()));
    let mut sum = NumericState::new_governed(Function::SumIntDistinct, &mut allow).unwrap();
    assert!(matches!(sum.update_governed(Input::from_value(large), 3, &mut allow),
        Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum { aggregate: 3 }))));
    assert_eq!(sum.finish_governed(&mut allow).unwrap(), super::sum(None));
}

#[test]
fn every_grouped_checkpoint_preserves_only_a_complete_prefix_and_releases_all_state() {
    let q = prepare("MATCH (a)-[r:R]-(b) RETURN a AS owner,COUNT(DISTINCT r) AS n,AVG(r.p) AS avg,MIN(b) AS low GROUP BY a");
    let expected: Vec<_> = run(&q, source(63), wide()).collect::<Result<_, _>>().unwrap();
    assert!(expected.len() > 1);
    let mut total = 0;
    {
        let mut cursor = EdgeAggregateCursor::new(source(63), EdgeAggregatePlan::compile(&q).unwrap(), wide(), || {
            total += 1;
            Ok::<_, usize>(())
        });
        assert_eq!(cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(), expected);
    }
    for stop in 1..=total {
        let s = source(63);
        let dropped = Arc::clone(&s.drops);
        let mut calls = 0;
        let mut cursor = EdgeAggregateCursor::new(s, EdgeAggregatePlan::compile(&q).unwrap(), wide(), || {
            calls += 1;
            if calls == stop { Err(stop) } else { Ok(()) }
        });
        let mut prefix = Vec::new();
        loop {
            match cursor.next() {
                Some(Ok(row)) => prefix.push(row),
                Some(Err(GqlQueryError::Interrupted(at))) => { assert_eq!(at, stop); break; }
                other => panic!("expected injected failure: {other:?}"),
            }
        }
        assert_eq!(prefix, expected[..prefix.len()]);
        assert_eq!(cursor.row_stats().result_rows, prefix.len() as u64);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.next().is_none());
        cursor.close();
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(cursor.pending.is_none());
        drop(cursor);
        assert_eq!(calls, stop);
    }
}

#[test]
fn group_quotas_late_data_errors_empty_input_and_close_obey_the_pull_contract() {
    let q = prepare("MATCH (a)-[r:R]-(b) RETURN a AS owner,COUNT(DISTINCT r.p) AS n,AVG(r.p) AS avg GROUP BY a");
    let mut baseline = run(&q, source(63), wide());
    let expected: Vec<_> = baseline.by_ref().collect::<Result<_, _>>().unwrap();
    let rows = baseline.row_stats(); let work = baseline.evaluator_stats();
    let exact = GqlQueryPolicy::new(rows.snapshot_records, rows.result_rows, work.work_units, work.scratch_entries);
    assert_eq!(run(&q, source(63), exact).collect::<Result<Vec<_>, _>>().unwrap(), expected);
    for p in [
        GqlQueryPolicy::new(rows.snapshot_records - 1, rows.result_rows, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, rows.result_rows - 1, u64::MAX, u64::MAX),
    ] {
        let mut cursor = run(&q, source(63), p);
        assert!(cursor.next().unwrap().is_err());
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert!(cursor.next().is_none());
    }
    for p in [
        GqlQueryPolicy::new(u64::MAX, rows.result_rows, work.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, rows.result_rows, u64::MAX, work.scratch_entries - 1),
    ] {
        let mut cursor = run(&q, source(63), p);
        let mut prefix = Vec::new();
        while let Some(Ok(row)) = cursor.next() { prefix.push(row); }
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert_eq!(prefix, expected[..prefix.len()]);
        assert!(cursor.next().is_none());
    }
    let mut bad = source(63);
    bad.edges.get_mut(&EId(6)).unwrap().3 = vec![(P, CanonicalScalar::Bool(true))];
    let mut cursor = run(&q, bad, wide());
    assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Source(
        GraphAggregateError::NonIntegerAverage { aggregate: 1 })))));
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert!(cursor.next().is_none());
    let mut broken = source(63); broken.fail = Some(EId(6));
    let mut cursor = run(&q, broken, wide());
    assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Source(GraphAggregateError::Source(
        EdgeScanError::Source("edge unavailable")))))));
    assert_eq!(cursor.row_stats().result_rows, 0);
    let zero = GqlQueryPolicy::new(100, 0, 100_000, 100_000);
    let mut empty = run(&q, source(0), zero);
    assert!(empty.next().is_none()); assert_eq!(empty.state(), EdgeScanState::Exhausted);
    let global = prepare("MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n,AVG(r.p) AS avg");
    assert!(run(&global, source(0), zero).next().unwrap().is_err());
    for pull_first in [false, true] {
        let s = source(63); let reads = Arc::clone(&s.reads); let dropped = Arc::clone(&s.drops);
        let mut cursor = run(&q, s, wide());
        if pull_first {
            assert_eq!(cursor.next().unwrap().unwrap(), expected[0]);
            assert_eq!(dropped.load(Ordering::SeqCst), 1, "source released before pending delivery");
            assert!(cursor.pending.is_some());
        } else { assert_eq!(reads.load(Ordering::SeqCst), 0); }
        let prior_reads = reads.load(Ordering::SeqCst);
        cursor.close(); cursor.close();
        assert!(cursor.next().is_none()); assert!(cursor.pending.is_none());
        assert_eq!(cursor.state(), EdgeScanState::Closed);
        assert_eq!(reads.load(Ordering::SeqCst), prior_reads);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn thousands_of_joined_occurrences_share_group_local_distinct_support_and_exact_averages() {
    let q = prepare("MATCH (a)-[r:R]->(b) RETURN a AS owner,COUNT(*) AS n,COUNT(DISTINCT r.p) AS d,SUM(r.p) AS total,AVG(DISTINCT r.p) AS avg GROUP BY a");
    let mut s = source(0);
    for id in 0..4096_u128 {
        let owner = if id % 2 == 0 { VId(0) } else { VId(u128::MAX) };
        let value = i64::MAX - i64::try_from((id / 2) % 2).unwrap();
        s.edges.insert(EId(id), (owner, R, VId(1), vec![(P, CanonicalScalar::Int(value))]));
    }
    let mut cursor = run(&q, s, GqlQueryPolicy::new(4096, 2, 10_000_000, 1_000_000));
    let rows: Vec<_> = cursor.by_ref().collect::<Result<_, _>>().unwrap();
    assert_eq!(rows.len(), 2);
    for (row, owner) in rows.iter().zip([VId(0), VId(u128::MAX)]) {
        assert_eq!(row.keys(), &[GraphValue::Vertex(owner)]);
        assert_eq!(row.values()[0].as_count(), Some(2048));
        assert_eq!(row.values()[1].as_count(), Some(2));
        assert_eq!(row.values()[2].as_integer(), Some(1024 * (2 * i128::from(i64::MAX) - 1)));
        let avg = row.values()[3].as_average().unwrap();
        assert_eq!((avg.numerator(), avg.denominator()), (2 * i128::from(i64::MAX) - 1, 2));
    }
    assert_eq!(cursor.row_stats().snapshot_records, 4096);
    assert_eq!(cursor.row_stats().result_rows, 2);
}
