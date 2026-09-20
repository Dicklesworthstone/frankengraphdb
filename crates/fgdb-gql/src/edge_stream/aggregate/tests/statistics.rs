//! Reuse the real join fixture, with the separate eager engine as the oracle.
use super::*;
use crate::GraphExactAverage;

fn eager(q: &PreparedGraphAggregate, s: &Source) -> GraphAggregateRow {
    q.execute_governed_with_element_properties(s.edges.len() as u64,
        s.vertices.keys().copied(), s.edges.iter().map(|(&id, (a, r, b, _))| (id, *a, *r, *b)),
        |vid, tests| Ok::<_, ()>(tests.iter().all(|test| test.matches_borrowed([], s.vertices[&vid].iter().map(|(k,v)| (*k,v))))),
        |id, _| Ok(s.vertices[&id].iter().find(|(k, _)| *k == P).map(|(_, v)| v)),
        |id, _| Ok(s.edges[&id].3.iter().find(|(k, _)| *k == P).map(|(_, v)| v)),
        policy(), || Ok::<_, ()>(())).unwrap().value.into_iter().next().unwrap()
}
fn summary(pattern: &str) -> PreparedGraphAggregate {
    prepare(&format!("MATCH {pattern} RETURN COUNT(*) AS n,COUNT(r.p) AS present,COUNT(DISTINCT r.p) AS different,\
        SUM(r.p) AS total,SUM(DISTINCT r.p) AS distinct_total,AVG(r.p) AS average,\
        AVG(DISTINCT r.p) AS distinct_average,MIN(r.p) AS lo,MAX(r.p) AS hi,\
        COUNT(DISTINCT r) AS edges,MIN(r) AS first_edge,MAX(r) AS last_edge,COUNT(DISTINCT a) AS roots"))
}

#[test]
fn all_nine_functions_and_full_width_identities_match_eager_for_576_multigraphs() {
    for mask in 0..64 {
        for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
            for shape in 0..3 {
                let edge = |name: &str, rel: &str, end: &str| match direction {
                    GlaDirection::Forward => format!("-[{name}:{rel}]->({end})"),
                    GlaDirection::Reverse => format!("<-[{name}:{rel}]-({end})"),
                    GlaDirection::Undirected => format!("-[{name}:{rel}]-({end})"),
                };
                let mut pattern = format!("(a){}", edge("r", "R", "b"));
                if shape > 0 { pattern.push_str(&edge("s", "S", "c")); }
                if shape > 1 { pattern.push_str(&edge("t", "R", "a")); }
                let q = summary(&pattern); let s = source(mask);
                let expected = eager(&q, &s);
                let mut cursor = run(&q, s, policy());
                assert_eq!(cursor.next().unwrap().unwrap(), expected, "{pattern}, mask {mask}");
                assert_eq!(cursor.row_stats().result_rows, 1);
                assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                assert!(cursor.next().is_none());
            }
        }
    }
    let q = summary("(a)-[r:R]->(b)"); let mut s = source(63);
    s.edges.insert(EId(u128::MAX), (VId(0),R,VId(u128::MAX),vec![(P,CanonicalScalar::Int(5))]));
    let row = run(&q, s, policy()).next().unwrap().unwrap();
    assert_eq!(row.values()[9].as_count(), Some(5));
    assert_eq!(row.values()[11].as_value(), Some(&GraphValue::Edge(EId(u128::MAX))));
}

#[test]
fn captured_path_distinct_and_extrema_do_not_reduce_identity_to_an_integer() {
    for text in [
        "MATCH p=(a)-[r:R]->(b)-[s:S]->(c) RETURN COUNT(DISTINCT p) AS paths,MIN(p) AS first,MAX(p) AS last",
        "MATCH p=(a)-[r:R]->(b)-[s:S]->(c) RETURN COUNT(DISTINCT r) AS roots,COUNT(DISTINCT a) AS starts",
        "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[:S]->(c) } RETURN AVG(DISTINCT r.p) AS average,MIN(r) AS first",
    ] {
        let q = prepare(text); let s = source(63); let expected = eager(&q, &s);
        assert_eq!(run(&q, s, policy()).next().unwrap().unwrap(), expected, "{text}");
    }
}

#[test]
fn statistics_exact_limits_and_every_callback_refusal_preserve_atomic_output_and_source_release() {
    let q = summary("(a)-[r:R]->(b)-[s:S]->(c)");
    let expected = eager(&q, &source(63));
    let mut calls = 0;
    let mut baseline = EdgeAggregateCursor::new(source(63), EdgeAggregatePlan::compile(&q).unwrap(), policy(),
        || { calls += 1; Ok::<_, usize>(()) });
    assert_eq!(baseline.next().unwrap().unwrap(), expected);
    let rows = baseline.row_stats(); let work = baseline.evaluator_stats(); drop(baseline);
    let exact = GqlQueryPolicy::new(rows.snapshot_records, 1, work.work_units, work.scratch_entries);
    assert_eq!(run(&q, source(63), exact).next().unwrap().unwrap(), expected);
    for stop in 1..=calls {
        let s = source(63); let dropped = s.drops.clone(); let mut at = 0;
        let mut cursor = EdgeAggregateCursor::new(s, EdgeAggregatePlan::compile(&q).unwrap(), exact,
            || { at += 1; if at == stop { Err(stop) } else { Ok(()) } });
        assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Interrupted(n))) if n == stop));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(cursor.next().is_none()); drop(cursor); assert_eq!(at, stop);
    }
    for p in [GqlQueryPolicy::new(rows.snapshot_records-1,1,u64::MAX,u64::MAX),
        GqlQueryPolicy::new(u64::MAX,0,u64::MAX,u64::MAX),
        GqlQueryPolicy::new(u64::MAX,1,work.work_units-1,u64::MAX),
        GqlQueryPolicy::new(u64::MAX,1,u64::MAX,work.scratch_entries-1)] {
        let mut cursor = run(&q, source(63), p);
        assert!(cursor.next().unwrap().is_err()); assert_eq!(cursor.row_stats().result_rows, 0);
        assert!(cursor.next().is_none());
    }
}

#[test]
fn late_nonnumeric_or_unavailable_input_does_not_expose_a_partial_distinct_count() {
    for function in ["AVG(r.p)", "SUM(DISTINCT r.p)", "AVG(DISTINCT r.p)"] {
        let q = prepare(&format!("MATCH (a)-[r:R]->(b) RETURN COUNT(DISTINCT r) AS edges,{function} AS value"));
        let mut s = source(63); s.edges.get_mut(&EId(6)).unwrap().3 = vec![(P, CanonicalScalar::Bool(true))];
        let dropped = s.drops.clone(); let mut cursor = run(&q, s, policy());
        let error = cursor.next().unwrap().unwrap_err();
        assert!(matches!(error, GqlQueryError::Source(GraphAggregateError::NonIntegerSum { aggregate: 1 }
            | GraphAggregateError::NonIntegerAverage { aggregate: 1 })));
        assert_eq!(cursor.row_stats().result_rows, 0); assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(cursor.next().is_none());
    }
    let q = summary("(a)-[r:R]->(b)"); let mut s = source(63); s.fail = Some(EId(6));
    let mut cursor = run(&q, s, policy());
    assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Source(GraphAggregateError::Source(
        EdgeScanError::Source("edge unavailable")
    ))))));
    assert_eq!(cursor.row_stats().result_rows, 0); assert!(cursor.next().is_none());
}

#[test]
fn exact_distinct_average_and_scalar_extrema_keep_their_original_domains() {
    let q = summary("(a)-[r:R]->(b)"); let mut s = source(0);
    for (id, value) in [(0, i64::MAX), (1, i64::MAX-1), (2, i64::MAX)] {
        s.edges.insert(EId(id), (VId(0),R,VId(1),vec![(P,CanonicalScalar::Int(value))]));
    }
    let row = run(&q, s, policy()).next().unwrap().unwrap();
    assert_eq!(row.values()[2].as_count(), Some(2));
    assert_eq!(row.values()[5].as_average(), GraphExactAverage::new(3*i128::from(i64::MAX)-1,3));
    assert_eq!(row.values()[6].as_average(), GraphExactAverage::new(2*i128::from(i64::MAX)-1,2));
    let q = prepare("MATCH (a)-[r:R]->(b) RETURN COUNT(DISTINCT r.p) AS different,MIN(r.p) AS lo,MAX(r.p) AS hi");
    let mut s = source(63);
    s.edges.get_mut(&EId(1)).unwrap().3 = vec![(P, CanonicalScalar::ucs_basic_text(&"é".repeat(64)).unwrap())];
    s.edges.get_mut(&EId(6)).unwrap().3 = vec![(P, CanonicalScalar::bytes(vec![255; 512]).unwrap())];
    let expected = eager(&q, &s);
    assert_eq!(run(&q, s, policy()).next().unwrap().unwrap(), expected);
}
