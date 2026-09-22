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
            Ok::<_, ()>(
                predicates
                    .iter()
                    .all(|p| p.matches_borrowed([], s.vertices[&vid].iter().map(|(k, v)| (*k, v)))),
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

#[test]
fn grouped_edge_properties_and_vertex_keys_match_batch_for_all_fixture_subgraphs() {
    for mask in 0..64 {
        for pattern in [
            "(a)-[r:R]->(b)",
            "(a)<-[r:R]-(b)",
            "(a)-[r:R]-(b)",
            "(a)-[r:R]->(b)-[s:S]->(c)",
            "(a)-[r:R]-(b)-[s:S]-(c)",
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
                assert_eq!(
                    cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                    expected
                );
                assert_eq!(cursor.row_stats().result_rows, expected.len() as u64);
                assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                assert!(cursor.next().is_none());
            }
        }
    }
}

#[test]
fn captured_path_keys_preserve_real_edge_occurrences_instead_of_only_endpoints() {
    let q = prepare(
        "MATCH p=(a)-[:R]->(b)-[:S]->(c) RETURN p AS path, COUNT(*) AS occurrences, COUNT(DISTINCT b) AS middles GROUP BY p",
    );
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
    let q = prepare(
        "MATCH (a)-[r:R]->(b) RETURN b AS destination, COUNT(*) AS rows, COUNT(DISTINCT r.p) AS support, AVG(DISTINCT r.p) AS average GROUP BY b",
    );
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
    assert_eq!(
        first.values()[2].as_average(),
        GraphExactAverage::new(-3, 2)
    );
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(cursor.input.source.is_none());
    assert!(cursor.input.traversal.is_none());
    assert_eq!(cursor.pending.as_ref().unwrap().len(), 1);
    let count = reads.load(Ordering::SeqCst);
    let second = cursor.next().unwrap().unwrap();
    assert_eq!(second, expected[1]);
    assert_eq!(second.values()[0].as_count(), Some(2));
    assert_eq!(second.values()[1].as_count(), Some(1));
    assert_eq!(
        second.values()[2].as_average(),
        GraphExactAverage::new(5, 1)
    );
    assert_eq!(reads.load(Ordering::SeqCst), count);
    assert!(cursor.pending.is_none());
    assert!(cursor.next().is_none());
}

#[test]
fn grouped_scan_and_delivery_share_exact_quotas_and_every_checkpoint_fuses_on_error() {
    let q = prepare(
        "MATCH (a)-[r:R]->(b) RETURN b AS destination, COUNT(*) AS rows, AVG(DISTINCT r.p) AS average GROUP BY b",
    );
    let expected = eager(&q, &source(63));
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
    assert_eq!(
        full.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
        expected
    );
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
        let mut cursor =
            EdgeAggregateCursor::new(s, EdgeAggregatePlan::compile(&q).unwrap(), exact, || {
                calls += 1;
                if calls == stop { Err(stop) } else { Ok(()) }
            });
        let mut prefix = Vec::new();
        loop {
            match cursor.next().expect("selected checkpoint must be reached") {
                Ok(row) => prefix.push(row),
                Err(GqlQueryError::Interrupted(at)) => {
                    assert_eq!(at, stop);
                    break;
                }
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
    let q = prepare(
        "MATCH (a)-[r:R]->(b) RETURN r.p AS bucket, a AS owner, COUNT(*) AS rows, COUNT(DISTINCT b) AS destinations GROUP BY r.p, a",
    );
    let mut s = source(63);
    s.edges.get_mut(&EId(1)).unwrap().3 = vec![(P, CanonicalScalar::Bool(false))];
    s.edges.get_mut(&EId(4)).unwrap().3 = vec![(
        P,
        CanonicalScalar::ucs_basic_text(&"x".repeat(1024)).unwrap(),
    )];
    let expected = eager(&q, &s);
    let mut cursor = run(&q, s.clone(), wide());
    assert_eq!(
        cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
        expected
    );
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
    let q = prepare(
        "MATCH (a)-[r:R]->(b) RETURN b AS destination, COUNT(*) AS rows, AVG(r.p) AS average GROUP BY b",
    );
    let mut s = source(63);
    s.edges.get_mut(&EId(6)).unwrap().3 = vec![(P, CanonicalScalar::Bool(true))];
    let mut invalid = run(&q, s, wide());
    assert!(matches!(
        invalid.next(),
        Some(Err(GqlQueryError::Source(
            GraphAggregateError::NonIntegerAverage { aggregate: 1 }
        )))
    ));
    assert_eq!(invalid.row_stats().result_rows, 0);
    assert!(invalid.pending.is_none());
    assert!(invalid.next().is_none());
}

mod saved_regressions {
    //! Reuse the existing indexed fixture, but derive groups independently from
    //! complete vertex assignments and edge choices, never from cursor bindings.
    use super::super::*;

    type Answer = (Vec<GraphValue>, Vec<GraphAggregateValue>);
    fn wide() -> GqlQueryPolicy {
        GqlQueryPolicy::new(1_000_000, 1000, 10_000_000, 10_000_000)
    }
    fn plain(rows: &[GraphAggregateRow]) -> Vec<Answer> {
        rows.iter()
            .map(|row| (row.keys().to_vec(), row.values().to_vec()))
            .collect()
    }
    fn eager(q: &PreparedGraphAggregate, s: &Source) -> Vec<GraphAggregateRow> {
        q.execute_governed_with_element_properties(
            s.edges.len() as u64,
            s.vertices.keys().copied(),
            s.edges.iter().map(|(&id, (a, r, b, _))| (id, *a, *r, *b)),
            |vid, tests| {
                Ok::<_, ()>(tests.iter().all(|test| {
                    test.matches_borrowed([], s.vertices[&vid].iter().map(|(k, v)| (*k, v)))
                }))
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
    fn grouped_oracle(s: &Source, shape: usize, dir: GlaDirection, key_kind: usize) -> Vec<Answer> {
        let atoms = [(0, R, 1), (1, S, 2), (2, R, 0)];
        let width = if shape == 0 { 2 } else { 3 };
        let domain: Vec<_> = s.vertices.keys().copied().collect();
        let mut groups = BTreeMap::<Vec<GraphValue>, (u64, u64, Option<i128>)>::new();
        for mut code in 0..domain.len().pow(width) {
            let mut ids = Vec::new();
            for _ in 0..width {
                ids.push(domain[code % domain.len()]);
                code /= domain.len();
            }
            let choices: Vec<Vec<_>> = atoms[..=shape]
                .iter()
                .map(|&(from, rel, to)| {
                    s.edges
                        .iter()
                        .filter(|(_, (a, r, b, _))| {
                            *r == rel
                                && match dir {
                                    GlaDirection::Forward => *a == ids[from] && *b == ids[to],
                                    GlaDirection::Reverse => *b == ids[from] && *a == ids[to],
                                    GlaDirection::Undirected => {
                                        (*a == ids[from] && *b == ids[to])
                                            || (*b == ids[from] && *a == ids[to])
                                    }
                                }
                        })
                        .collect()
                })
                .collect();
            let suffix = choices
                .iter()
                .skip(1)
                .map(|edges| edges.len() as u64)
                .product::<u64>();
            if suffix == 0 {
                continue;
            }
            for &(eid, edge) in &choices[0] {
                let key = match key_kind {
                    0 => vec![GraphValue::Vertex(ids[0])],
                    1 => vec![
                        GraphValue::Scalar(
                            edge.3
                                .iter()
                                .find(|(k, _)| *k == P)
                                .map_or(CanonicalScalar::Null, |(_, v)| v.clone()),
                        ),
                        GraphValue::Vertex(ids[width as usize - 1]),
                    ],
                    _ => vec![GraphValue::Edge(*eid)],
                };
                let state = groups.entry(key).or_insert((0, 0, None));
                state.0 += suffix;
                if let Some(value) = integer(&edge.3) {
                    state.1 += suffix;
                    state.2 = Some(state.2.unwrap_or(0) + value * i128::from(suffix));
                }
            }
        }
        groups
            .into_iter()
            .map(|(key, (count, present, total))| {
                (
                    key,
                    vec![
                        GraphAggregateValue::Count(count),
                        GraphAggregateValue::Count(present),
                        sum(total),
                    ],
                )
            })
            .collect()
    }

    #[test]
    fn grouped_edge_chain_and_cycle_rows_match_independent_assignment_groups() {
        for mask in 0..64 {
            for direction in [
                GlaDirection::Forward,
                GlaDirection::Reverse,
                GlaDirection::Undirected,
            ] {
                let edge = |name: &str, rel: &str, end: &str| match direction {
                    GlaDirection::Forward => format!("-[{name}:{rel}]->({end})"),
                    GlaDirection::Reverse => format!("<-[{name}:{rel}]-({end})"),
                    GlaDirection::Undirected => format!("-[{name}:{rel}]-({end})"),
                };
                for shape in 0..3 {
                    let mut pattern = format!("(a){}", edge("r", "R", "b"));
                    if shape > 0 {
                        pattern.push_str(&edge("s", "S", "c"));
                    }
                    if shape > 1 {
                        pattern.push_str(&edge("t", "R", "a"));
                    }
                    let end = if shape == 0 { "b" } else { "c" };
                    for (key_kind, keys) in ["a".to_owned(), format!("r.p,{end}"), "r".to_owned()]
                        .iter()
                        .enumerate()
                    {
                        let q = prepare(&format!(
                            "MATCH {pattern} RETURN {keys},COUNT(*) AS n,COUNT(r.p) AS present,SUM(r.p) AS total GROUP BY {keys}"
                        ));
                        let s = source(mask);
                        let expected = grouped_oracle(&s, shape, direction, key_kind);
                        let ordinary = eager(&q, &s);
                        let dropped = s.drops.clone();
                        let mut stream = run(&q, s, wide());
                        assert_eq!(stream.size_hint(), (0, None));
                        assert_eq!(stream.key_columns(), q.key_columns());
                        let actual = stream.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                        assert_eq!(plain(&actual), expected);
                        assert_eq!(actual, ordinary);
                        assert_eq!(stream.row_stats().result_rows, expected.len() as u64);
                        assert_eq!(stream.state(), EdgeScanState::Exhausted);
                        assert_eq!(dropped.load(Ordering::SeqCst), 1);
                        assert!(stream.next().is_none());
                    }
                }
            }
        }
    }

    #[test]
    fn probe_and_path_grouping_preserve_the_shared_predicates_and_edge_identities() {
        for text in [
            "MATCH (a)-[r:R]->(b)-[s:S]->(c) WHERE r.p>0 OR b.p IS NULL RETURN a,s.p,COUNT(*) AS n,SUM(r.p) AS total GROUP BY a,s.p",
            "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[:S*1..3]->(c) } RETURN b,COUNT(*) AS n,SUM(r.p) AS total GROUP BY b",
            "MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (b)-[:S]->(c) WHERE c.p>0 } RETURN r.p,COUNT(r) AS n GROUP BY r.p",
            "MATCH p=(a)-[r:R]->(b)-[s:S]->(c) RETURN p,COUNT(*) AS n,SUM(s.p) AS total GROUP BY p",
        ] {
            let q = prepare(text);
            let s = source(63);
            let expected = eager(&q, &s);
            assert!(!expected.is_empty());
            let mut stream = run(&q, s, wide());
            let rows = stream.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(rows, expected, "{text}");
            assert!(stream.pending.is_none());
        }
    }

    #[test]
    fn canonical_null_and_full_width_edge_keys_are_owned_without_domain_coercion() {
        let q = prepare(
            "MATCH (a)-[r:R]->(b) RETURN r.p,COUNT(*) AS n,COUNT(r.p) AS present GROUP BY r.p",
        );
        let mut s = source(0);
        let data = [
            None,
            Some(CanonicalScalar::Null),
            Some(CanonicalScalar::Bool(false)),
            Some(CanonicalScalar::Int(0)),
            Some(CanonicalScalar::ucs_basic_text("secret key").unwrap()),
        ];
        for (id, value) in data.into_iter().enumerate() {
            s.edges.insert(
                EId(id as u128),
                (
                    VId(0),
                    R,
                    VId(1),
                    value.into_iter().map(|v| (P, v)).collect(),
                ),
            );
        }
        let expected = eager(&q, &s);
        let dropped = s.drops.clone();
        let mut stream = run(&q, s, wide());
        let first = stream.next().unwrap().unwrap();
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            1,
            "source retained during group delivery"
        );
        assert!(stream.input.source.is_none());
        assert!(!format!("{stream:?}").contains("secret key"));
        let mut actual = vec![first];
        actual.extend(stream.by_ref().map(Result::unwrap));
        let null = actual.iter().find(|row| row.keys()[0].is_null()).unwrap();
        assert_eq!(
            null.values(),
            &[GraphAggregateValue::Count(2), GraphAggregateValue::Count(0)]
        );
        assert_eq!(actual, expected);
        let q = prepare("MATCH (a)-[r:R]->(b) RETURN r,COUNT(*) AS n GROUP BY r");
        let mut s = source(0);
        for id in [0, 1_u128 << 100, u128::MAX] {
            s.edges.insert(EId(id), (VId(0), R, VId(1), vec![]));
        }
        let rows = run(&q, s, wide()).map(Result::unwrap).collect::<Vec<_>>();
        assert_eq!(
            rows.iter()
                .map(|row| row.keys()[0].clone())
                .collect::<Vec<_>>(),
            [0, 1_u128 << 100, u128::MAX].map(|id| GraphValue::Edge(EId(id)))
        );
    }

    #[test]
    fn late_source_data_and_group_quota_failures_precede_every_group() {
        let q = prepare("MATCH (a)-[r:R]->(b) RETURN a,COUNT(*) AS n,SUM(r.p) AS total GROUP BY a");
        for kind in 0..3 {
            let mut s = source(63);
            let dropped = s.drops.clone();
            let p = if kind == 2 {
                GqlQueryPolicy::new(1000, 1, u64::MAX, u64::MAX)
            } else {
                wide()
            };
            match kind {
                0 => s.fail = Some(EId(6)),
                1 => s.edges.get_mut(&EId(6)).unwrap().3 = vec![(P, CanonicalScalar::Bool(true))],
                _ => {}
            }
            let mut stream = run(&q, s, p);
            match kind {
                0 => assert!(matches!(
                    stream.next(),
                    Some(Err(GqlQueryError::Source(GraphAggregateError::Source(
                        EdgeScanError::Source("edge unavailable")
                    ))))
                )),
                1 => assert!(matches!(
                    stream.next(),
                    Some(Err(GqlQueryError::Source(
                        GraphAggregateError::NonIntegerSum { aggregate: 1 }
                    )))
                )),
                _ => assert!(matches!(stream.next(), Some(Err(GqlQueryError::Rows(_))))),
            }
            assert_eq!(stream.row_stats().result_rows, 0);
            assert_eq!(stream.state(), EdgeScanState::Failed);
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
            assert!(stream.pending.is_none());
            assert!(stream.next().is_none());
        }
        let empty = run(&q, source(0), GqlQueryPolicy::new(0, 0, 100, 100)).next();
        assert!(
            empty.is_none(),
            "empty grouped input must not fabricate a global group"
        );
    }

    #[test]
    fn every_checkpoint_and_exact_limit_preserves_only_completed_delivery_prefixes() {
        let q = prepare(
            "MATCH (a)-[r:R]-(b) RETURN a,r.p,COUNT(*) AS n,SUM(r.p) AS total,AVG(r.p) AS mean,COUNT(DISTINCT r.p) AS distinct_values,MIN(r) AS first_edge GROUP BY a,r.p",
        );
        let mut full = run(&q, source(63), wide());
        let expected = full.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        let r = full.row_stats();
        let e = full.evaluator_stats();
        let exact = GqlQueryPolicy::new(
            r.snapshot_records,
            r.result_rows,
            e.work_units,
            e.scratch_entries,
        );
        assert_eq!(
            run(&q, source(63), exact)
                .map(Result::unwrap)
                .collect::<Vec<_>>(),
            expected
        );
        for limited in [
            GqlQueryPolicy::new(r.snapshot_records - 1, 1000, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1000, r.result_rows - 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1000, 1000, e.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(1000, 1000, u64::MAX, e.scratch_entries - 1),
        ] {
            let mut stream = run(&q, source(63), limited);
            let mut prefix = Vec::new();
            loop {
                match stream.next() {
                    Some(Ok(row)) => prefix.push(row),
                    Some(Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_))) => break,
                    other => panic!("quota refusal disappeared: {other:?}"),
                }
            }
            assert_eq!(prefix, expected[..prefix.len()]);
            assert_eq!(stream.row_stats().result_rows, prefix.len() as u64);
            assert!(stream.pending.is_none());
            assert!(stream.next().is_none());
        }
        let calls = std::cell::Cell::new(0);
        let mut baseline = EdgeAggregateCursor::new(
            source(63),
            EdgeAggregatePlan::compile(&q).unwrap(),
            exact,
            || {
                calls.set(calls.get() + 1);
                Ok::<_, usize>(())
            },
        );
        assert_eq!(
            baseline.by_ref().map(Result::unwrap).collect::<Vec<_>>(),
            expected
        );
        let total = calls.get();
        drop(baseline);
        for stop in 1..=total {
            calls.set(0);
            let s = source(63);
            let dropped = s.drops.clone();
            let mut stream =
                EdgeAggregateCursor::new(s, EdgeAggregatePlan::compile(&q).unwrap(), exact, || {
                    calls.set(calls.get() + 1);
                    if calls.get() == stop {
                        Err(stop)
                    } else {
                        Ok(())
                    }
                });
            let mut prefix = Vec::new();
            loop {
                match stream.next() {
                    Some(Ok(row)) => prefix.push(row),
                    Some(Err(GqlQueryError::Interrupted(at))) => {
                        assert_eq!(at, stop);
                        break;
                    }
                    other => panic!("checkpoint refusal disappeared: {other:?}"),
                }
            }
            assert_eq!(calls.get(), stop);
            assert_eq!(prefix, expected[..prefix.len()]);
            assert_eq!(stream.row_stats().result_rows, prefix.len() as u64);
            assert_eq!(stream.state(), EdgeScanState::Failed);
            assert!(stream.pending.is_none());
            assert!(stream.next().is_none());
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
            assert_eq!(
                run(&q, source(63), exact)
                    .map(Result::unwrap)
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn thousands_of_join_occurrences_retain_only_four_numeric_groups_and_close_does_not_drain() {
        let q = prepare("MATCH (a)-[r:R]->(b) RETURN b,COUNT(*) AS n,SUM(r.p) AS total GROUP BY b");
        let mut s = source(0);
        for id in 0..4 {
            s.vertices.insert(VId(id), vec![]);
        }
        for id in 0..4096 {
            s.edges.insert(
                EId(id),
                (
                    VId(0),
                    R,
                    VId(id % 4),
                    vec![(P, CanonicalScalar::Int(i64::MAX))],
                ),
            );
        }
        let dropped = s.drops.clone();
        let mut cursor = run(&q, s, GqlQueryPolicy::new(4096, 4, 2_000_000, 2_000_000));
        let first = cursor.next().unwrap().unwrap();
        assert_eq!(first.keys(), &[GraphValue::Vertex(VId(0))]);
        assert_eq!(
            first.values(),
            &[
                GraphAggregateValue::Count(1024),
                GraphAggregateValue::Integer(1024 * i128::from(i64::MAX))
            ]
        );
        assert_eq!(cursor.pending.as_ref().unwrap().len(), 3);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(cursor.row_stats().snapshot_records, 4096);
        let usage = (cursor.row_stats(), cursor.evaluator_stats());
        cursor.close();
        cursor.close();
        assert_eq!(cursor.state(), EdgeScanState::Closed);
        assert!(cursor.pending.is_none());
        assert!(cursor.next().is_none());
        assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), usage);
        let s = source(63);
        let reads = s.reads.clone();
        let dropped = s.drops.clone();
        let mut closed = run(&q, s, wide());
        closed.close();
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn all_shared_statistics_keep_independent_per_group_distinct_support_and_extrema() {
        let q = prepare(
            "MATCH (a)-[r:R]-(b) RETURN b,COUNT(*) AS n,COUNT(r.p) AS present,COUNT(DISTINCT r.p) AS unique_values,SUM(r.p) AS total,SUM(DISTINCT r.p) AS distinct_total,AVG(r.p) AS mean,AVG(DISTINCT r.p) AS distinct_mean,MIN(r.p) AS low,MAX(r.p) AS high,MIN(r) AS first_edge,MAX(r) AS last_edge,COUNT(DISTINCT r) AS edges GROUP BY b",
        );
        for mask in 0..64 {
            let s = source(mask);
            let expected = eager(&q, &s);
            assert_eq!(
                run(&q, s, wide()).map(Result::unwrap).collect::<Vec<_>>(),
                expected
            );
        }
        // The same value must be counted distinctly in EACH group, not globally.
        let q = prepare(
            "MATCH (a)-[r:R]->(b) RETURN b,COUNT(DISTINCT r.p) AS unique_values,SUM(DISTINCT r.p) AS total,AVG(DISTINCT r.p) AS mean GROUP BY b",
        );
        let mut s = source(0);
        for (id, target, value) in [
            (1, 0, 2),
            (2, 0, 2),
            (3, 0, 5),
            (4, 1, 2),
            (5, 1, 5),
            (6, 1, 5),
        ] {
            s.edges.insert(
                EId(id),
                (
                    VId(0),
                    R,
                    VId(target),
                    vec![(P, CanonicalScalar::Int(value))],
                ),
            );
        }
        let actual = run(&q, s, wide()).map(Result::unwrap).collect::<Vec<_>>();
        assert_eq!(actual.len(), 2);
        for row in actual {
            assert_eq!(
                row.values(),
                &[
                    GraphAggregateValue::Count(2),
                    GraphAggregateValue::Integer(7),
                    GraphAggregateValue::Average(crate::GraphExactAverage::new(7, 2).unwrap()),
                ]
            );
        }
    }
}
