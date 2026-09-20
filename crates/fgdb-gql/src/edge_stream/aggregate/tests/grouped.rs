//! Groups are over joined occurrences, with DISTINCT support local to each key.
use super::*;
use crate::GraphExactAverage;

fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}

fn eager(q: &PreparedGraphAggregate, s: &Source) -> Vec<GraphAggregateRow> {
    q.execute_governed_with_element_properties(
        s.edges.len() as u64,
        s.vertices.keys().copied(),
        s.edges.iter().map(|(&id, (a, r, b, _))| (id, *a, *r, *b)),
        |vid, predicates| {
            Ok::<_, ()>(predicates.iter().all(|p| {
                p.matches_borrowed([], s.vertices[&vid].iter().map(|(k, v)| (*k, v)))
            }))
        },
        |id, key| Ok(s.vertices[&id].iter().find(|(k, _)| *k == key).map(|(_, v)| v)),
        |id, key| Ok(s.edges[&id].3.iter().find(|(k, _)| *k == key).map(|(_, v)| v)),
        wide(),
        || Ok::<_, ()>(()),
    ).unwrap().value
}

#[test]
fn grouped_edge_properties_and_vertex_keys_match_batch_for_all_fixture_subgraphs() {
    for mask in 0..64 {
        for pattern in [
            "(a)-[r:R]->(b)", "(a)<-[r:R]-(b)", "(a)-[r:R]-(b)",
            "(a)-[r:R]->(b)-[s:S]->(c)", "(a)-[r:R]-(b)-[s:S]-(c)",
        ] {
            for (outputs, keys) in [
                ("b AS destination", "b"),
                ("r.p AS bucket", "r.p"),
                ("a AS owner, r.p AS bucket", "a, r.p"),
                ("b.p AS bucket, a AS owner", "b.p, a"),
                ("r AS edge", "r"),
            ] {
                let q = prepare(&format!(
                    "MATCH {pattern} RETURN {outputs}, COUNT(*) AS rows, COUNT(DISTINCT r.p) AS different, SUM(r.p) AS sum, SUM(DISTINCT r.p) AS distinct_sum, AVG(r.p) AS average, AVG(DISTINCT r.p) AS distinct_average, MIN(r.p) AS minimum, MAX(r.p) AS maximum GROUP BY {keys}"
                ));
                let s = source(mask);
                let expected = eager(&q, &s);
                let mut cursor = run(&q, s, wide());
                assert_eq!(cursor.key_columns(), q.key_columns());
                assert_eq!(cursor.row_stats().result_rows, 0);
                assert_eq!(cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(), expected);
                assert_eq!(cursor.row_stats().result_rows, expected.len() as u64);
                assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                assert!(cursor.next().is_none());
            }
        }
    }
}

#[test]
fn captured_path_keys_preserve_real_edge_occurrences_instead_of_only_endpoints() {
    let q = prepare("MATCH p=(a)-[:R]->(b)-[:S]->(c) RETURN p AS path, COUNT(*) AS occurrences, COUNT(DISTINCT b) AS middles GROUP BY p");
    for mask in 0..64 {
        let s = source(mask);
        let expected = eager(&q, &s);
        let mut cursor = run(&q, s, wide());
        let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(rows, expected);
        for row in &rows {
            assert!(matches!(row.keys().first(), Some(GraphValue::Path(_))));
            assert_eq!(row.values()[0].as_count(), Some(1));
            assert_eq!(row.values()[1].as_count(), Some(1));
        }
        assert_eq!(cursor.row_stats().result_rows, rows.len() as u64);
        assert!(cursor.next().is_none());
    }
}

#[test]
fn groups_own_keys_and_local_distinct_state_and_release_the_pin_before_delivery() {
    let q = prepare("MATCH (a)-[r:R]->(b) RETURN b AS destination, COUNT(*) AS rows, COUNT(DISTINCT r.p) AS support, AVG(DISTINCT r.p) AS average GROUP BY b");
    let mut s = source(63);
    s.edges.get_mut(&EId(2)).unwrap().3 = vec![(P, CanonicalScalar::Int(5))];
    let expected = eager(&q, &s);
    let reads = s.reads.clone();
    let dropped = s.drops.clone();
    let mut cursor = run(&q, s, wide());
    assert_eq!(cursor.size_hint(), (0, None));
    let first = cursor.next().unwrap().unwrap();
    assert_eq!(first, expected[0]);
    assert_eq!(first.keys(), &[GraphValue::Vertex(VId(0))]);
    assert_eq!(first.values()[0].as_count(), Some(2));
    assert_eq!(first.values()[1].as_count(), Some(2));
    assert_eq!(first.values()[2].as_average(), GraphExactAverage::new(-3, 2));
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(cursor.input.source.is_none());
    assert!(cursor.input.traversal.is_none());
    assert_eq!(cursor.pending.as_ref().unwrap().len(), 1);
    let count = reads.load(Ordering::SeqCst);
    let second = cursor.next().unwrap().unwrap();
    assert_eq!(second, expected[1]);
    assert_eq!(second.values()[0].as_count(), Some(2));
    assert_eq!(second.values()[1].as_count(), Some(1));
    assert_eq!(second.values()[2].as_average(), GraphExactAverage::new(5, 1));
    assert_eq!(reads.load(Ordering::SeqCst), count);
    assert!(cursor.pending.is_none());
    assert!(cursor.next().is_none());
}

#[test]
fn grouped_scan_and_delivery_share_exact_quotas_and_every_checkpoint_fuses_on_error() {
    let q = prepare("MATCH (a)-[r:R]->(b) RETURN b AS destination, COUNT(*) AS rows, AVG(DISTINCT r.p) AS average GROUP BY b");
    let expected = eager(&q, &source(63));
    let mut total = 0;
    let mut full = EdgeAggregateCursor::new(source(63), EdgeAggregatePlan::compile(&q).unwrap(), wide(),
        || { total += 1; Ok::<_, usize>(()) });
    assert_eq!(full.by_ref().collect::<Result<Vec<_>, _>>().unwrap(), expected);
    let r = full.row_stats();
    let e = full.evaluator_stats();
    drop(full);
    let exact = GqlQueryPolicy::new(r.snapshot_records, r.result_rows, e.work_units, e.scratch_entries);
    assert_eq!(run(&q, source(63), exact).collect::<Result<Vec<_>, _>>().unwrap(), expected);
    for p in [
        GqlQueryPolicy::new(r.snapshot_records - 1, r.result_rows, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, r.result_rows - 1, u64::MAX, u64::MAX),
    ] {
        let mut cursor = run(&q, source(63), p);
        assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Rows(_)))));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert!(cursor.pending.is_none());
        assert!(cursor.next().is_none());
    }
    for p in [
        GqlQueryPolicy::new(u64::MAX, r.result_rows, e.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, r.result_rows, u64::MAX, e.scratch_entries - 1),
    ] {
        let mut cursor = run(&q, source(63), p);
        assert!(cursor.by_ref().collect::<Result<Vec<_>, _>>().is_err());
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.pending.is_none());
        assert!(cursor.next().is_none());
    }
    for stop in 1..=total {
        let s = source(63);
        let dropped = s.drops.clone();
        let mut calls = 0;
        let mut cursor = EdgeAggregateCursor::new(s, EdgeAggregatePlan::compile(&q).unwrap(), exact,
            || { calls += 1; if calls == stop { Err(stop) } else { Ok(()) } });
        let mut prefix = Vec::new();
        loop {
            match cursor.next().expect("selected checkpoint must be reached") {
                Ok(row) => prefix.push(row),
                Err(GqlQueryError::Interrupted(at)) => { assert_eq!(at, stop); break; }
                Err(error) => panic!("unexpected refusal: {error:?}"),
            }
        }
        assert!(expected.starts_with(&prefix));
        assert_eq!(cursor.row_stats().result_rows, prefix.len() as u64);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.pending.is_none());
        assert!(cursor.input.source.is_none());
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(cursor.next().is_none());
        drop(cursor);
        assert_eq!(calls, stop);
    }
}

#[test]
fn mixed_nullable_keys_empty_input_late_failures_and_close_preserve_group_semantics() {
    let q = prepare("MATCH (a)-[r:R]->(b) RETURN r.p AS bucket, a AS owner, COUNT(*) AS rows, COUNT(DISTINCT b) AS destinations GROUP BY r.p, a");
    let mut s = source(63);
    s.edges.get_mut(&EId(1)).unwrap().3 = vec![(P, CanonicalScalar::Bool(false))];
    s.edges.get_mut(&EId(4)).unwrap().3 = vec![(P, CanonicalScalar::ucs_basic_text(&"x".repeat(1024)).unwrap())];
    let expected = eager(&q, &s);
    let mut cursor = run(&q, s.clone(), wide());
    assert_eq!(cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(), expected);
    let mut closed = run(&q, s, wide());
    closed.next().unwrap().unwrap();
    assert!(closed.pending.is_some());
    let stats = (closed.row_stats(), closed.evaluator_stats());
    closed.close();
    assert!(closed.pending.is_none());
    assert!(closed.next().is_none());
    assert_eq!((closed.row_stats(), closed.evaluator_stats()), stats);
    let mut empty = run(&q, source(0), GqlQueryPolicy::new(0, 0, u64::MAX, u64::MAX));
    assert!(empty.next().is_none());
    assert_eq!(empty.row_stats().result_rows, 0);
    let q = prepare("MATCH (a)-[r:R]->(b) RETURN b AS destination, COUNT(*) AS rows, AVG(r.p) AS average GROUP BY b");
    let mut s = source(63);
    s.edges.get_mut(&EId(6)).unwrap().3 = vec![(P, CanonicalScalar::Bool(true))];
    let mut invalid = run(&q, s, wide());
    assert!(matches!(invalid.next(), Some(Err(GqlQueryError::Source(
        GraphAggregateError::NonIntegerAverage { aggregate: 1 }
    )))));
    assert_eq!(invalid.row_stats().result_rows, 0);
    assert!(invalid.pending.is_none());
    assert!(invalid.next().is_none());
}
