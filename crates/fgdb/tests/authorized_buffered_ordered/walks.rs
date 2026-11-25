//! Authorized routes consume the same bounded root/incidence source as probes.
use super::*;
use fgdb_gql::algebra::GraphPath;
use std::collections::{BTreeMap, BTreeSet};

// Enumerate complete edge sequences on the literal visible graph, then apply
// each path rule. This has no dependency on the database's cursor, pruning,
// source admission, ordering implementation or resident query evaluator.
fn expected_routes(direction: i8, mode: &str, first_score: i64) -> Vec<Vec<GraphValue>> {
    let edges = [(101, 1, 2), (102, 1, 2), (103, 2, 3)];
    let scores = [first_score, 1, 3, -1, 2];
    let mut rows = Vec::new();
    for start in 1..=5 {
        let mut frontier = vec![Vec::<(EId, VId)>::new()];
        for depth in 0..=2 {
            for steps in &frontier {
                let vertices: Vec<_> = std::iter::once(VId(start))
                    .chain(steps.iter().map(|step| step.1))
                    .collect();
                let edge_unique = steps
                    .iter()
                    .map(|step| step.0)
                    .collect::<BTreeSet<_>>()
                    .len()
                    == steps.len();
                let vertex_unique =
                    vertices.iter().copied().collect::<BTreeSet<_>>().len() == vertices.len();
                let simple = vertices.iter().enumerate().all(|(at, vertex)| {
                    !vertices[..at].contains(vertex)
                        || (at + 1 == vertices.len() && *vertex == VId(start))
                });
                if match mode {
                    "TRAIL" => edge_unique,
                    "ACYCLIC" => vertex_unique,
                    "SIMPLE" => simple,
                    _ => true,
                } {
                    let end = steps.last().map_or(start, |step| step.1.0);
                    rows.push(vec![
                        GraphValue::Vertex(VId(start)),
                        GraphValue::Vertex(VId(end)),
                        GraphValue::Path(GraphPath::new(VId(start), steps.clone().into())),
                        scalar(depth),
                        scalar(scores[start as usize - 1]),
                        scalar(scores[end as usize - 1]),
                        null(),
                    ]);
                }
            }
            let mut next = Vec::new();
            if depth < 2 {
                for steps in frontier {
                    let endpoint = steps.last().map_or(start, |step| step.1.0);
                    for (id, from, to) in edges {
                        let target = if direction >= 0 && endpoint == from {
                            Some(to)
                        } else if direction <= 0 && endpoint == to {
                            Some(from)
                        } else {
                            None
                        };
                        if let Some(target) = target {
                            let mut extended = steps.clone();
                            extended.push((EId(id), VId(target)));
                            next.push(extended);
                        }
                    }
                }
            }
            frontier = next;
        }
    }
    if mode.ends_with("SHORTEST") {
        let mut minimum = BTreeMap::new();
        for row in &rows {
            let depth = minimum
                .entry((row[0].clone(), row[1].clone()))
                .or_insert_with(|| row[3].clone());
            *depth = depth.clone().min(row[3].clone());
        }
        rows.retain(|row| row[3] == minimum[&(row[0].clone(), row[1].clone())]);
    }
    rows.sort_by(|a, b| (&a[0], &a[1], &a[3], &a[2]).cmp(&(&b[0], &b[1], &b[3], &b[2])));
    if mode == "ANY SHORTEST" {
        let mut seen = BTreeSet::new();
        rows.retain(|row| seen.insert((row[0].clone(), row[1].clone())));
    }
    rows
}

#[test]
fn cold_walks_preserve_modes_directions_parallel_paths_masking_and_history() {
    let ((), report) = run_async_under_lab(0xa8_0240, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let authority = issuer();
        let token = authority.issue_at(&grant(10_000, 100), 100).unwrap();
        let args = GqlParameters::new();
        for hidden in [false, true] {
            let (vfs, _db, old, current) = fixture(&commit, hidden).await;
            for (at, first_score) in [(old, 3), (current, 30)] {
                for (direction, left, right) in [(1, "-", "->"), (-1, "<-", "-"), (0, "-", "-")] {
                    for mode in [
                        "WALK",
                        "TRAIL",
                        "ACYCLIC",
                        "SIMPLE",
                        "ANY SHORTEST",
                        "ALL SHORTEST",
                    ] {
                        let pool = memory();
                        let (mut ordered, _) = session(
                            &commit,
                            &cx,
                            &vfs,
                            &pool,
                            &authority,
                            &token,
                            at,
                            || 100,
                            policy(100),
                        )
                        .await;
                        let text = format!(
                            "MATCH REPEATABLE ELEMENTS p = {mode} (a){left}[:R*0..2]{right}(b) \
                             RETURN a,b,p,length(p) AS hops,a.score AS from_score, \
                             b.score AS to_score,a.secret AS hidden ORDER BY a,b,hops,p"
                        );
                        let prepared = ordered.prepare(&cx, &text, &args).unwrap();
                        let execution = ordered.execute_ordered(&cx, &prepared, &args).unwrap();
                        assert_send(&execution);
                        let mut cursor = execution.await.unwrap();
                        assert_eq!(cursor.snapshot_seq(), at);
                        assert_eq!(
                            values(&mut cursor).await,
                            expected_routes(direction, mode, first_score),
                            "{text}; hidden={hidden}; at={at:?}",
                        );
                        drop(cursor);
                        assert!(!ordered.is_closed());
                        drop(ordered);
                        assert_eq!(pool.used(), 0);
                    }
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_walk_fields_zero_hops_ordered_pages_and_aggregates_share_one_scope() {
    let ((), report) = run_async_under_lab(0xa8_0241, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let authority = issuer();
        let token = authority.issue_at(&grant(1000, 100), 100).unwrap();
        let args = GqlParameters::new();
        let (vfs, _db, old, current) = fixture(&commit, true).await;
        // The historical root predicate matches vertices 1 and 3. Only
        // vertex 1 still matches after its score changes to 30.
        for (at, first, selected, roots) in [(old, 3, 7, 2), (current, 30, 17, 1)] {
            let pool = memory();
            let (mut ordered, _) = session(
                &commit,
                &cx,
                &vfs,
                &pool,
                &authority,
                &token,
                at,
                || 100,
                policy(100),
            )
            .await;
            for (text, expected) in [
                (
                    format!(
                        "MATCH p = (a {{score:{first}}})-[:R*0..2 {{score:{selected}}}]->(b) \
                             RETURN a.score,b.score,length(p) AS hops ORDER BY hops"
                    ),
                    std::iter::repeat_n(vec![scalar(first), scalar(first), scalar(0)], roots)
                        .chain([vec![scalar(first), scalar(1), scalar(1)]])
                        .collect(),
                ),
                // Every real edge fails a denied property predicate. The
                // empty route has no hop predicate to evaluate and survives.
                (
                    "MATCH p = (a)-[:R*0..2 {secret:999}]->(b) \
                     RETURN a,b,length(p) AS hops ORDER BY a"
                        .into(),
                    (1..=5)
                        .map(|id| {
                            vec![
                                GraphValue::Vertex(VId(id)),
                                GraphValue::Vertex(VId(id)),
                                scalar(0),
                            ]
                        })
                        .collect(),
                ),
                // A denied relation must not remove isolates or zero-hop
                // roots, and may not contribute a positive-hop occurrence.
                (
                    "MATCH p = (a)-[:S*0..2]->(b) RETURN a,b,length(p) AS hops ORDER BY a".into(),
                    (1..=5)
                        .map(|id| {
                            vec![
                                GraphValue::Vertex(VId(id)),
                                GraphValue::Vertex(VId(id)),
                                scalar(0),
                            ]
                        })
                        .collect(),
                ),
                (
                    "MATCH p = (a)-[:R*1..2]->(b) RETURN DISTINCT a.score AS score \
                     ORDER BY score DESC SKIP 1 LIMIT 1"
                        .into(),
                    vec![vec![scalar(1)]],
                ),
            ] {
                let prepared = ordered.prepare(&cx, &text, &args).unwrap();
                let mut cursor = ordered
                    .execute_ordered(&cx, &prepared, &args)
                    .unwrap()
                    .await
                    .unwrap();
                assert_eq!(values(&mut cursor).await, expected, "{text}; at={at:?}");
                drop(cursor);
            }
            let prepared = ordered
                .prepare(
                    &cx,
                    "MATCH p = (a)-[:R*1..2]->(b) \
                 RETURN count(*) AS paths,sum(a.score) AS roots,count(b.secret) AS masked",
                    &args,
                )
                .unwrap();
            let mut cursor = ordered
                .execute_ordered_aggregate(&cx, &prepared, &args)
                .unwrap()
                .await
                .unwrap();
            assert_eq!(
                aggregates(&mut cursor).await,
                vec![vec![
                    GraphAggregateValue::Count(5),
                    GraphAggregateValue::Integer(4 * i128::from(first) + 1),
                    GraphAggregateValue::Count(0),
                ]]
            );
            drop(cursor);
            drop(ordered);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_walk_limit_zero_keeps_exact_source_allowances_and_retryable_ownership() {
    let ((), report) = run_async_under_lab(0xa8_0242, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let authority = issuer();
        let args = GqlParameters::new();
        for hidden in [false, true] {
            let (vfs, _db, _, at) = fixture(&commit, hidden).await;
            // Five visible roots plus five traversed edge occurrences. Each
            // edge owns both endpoint admissions: 5 + 2*5 = 15 signed nodes,
            // and 5 + 5 = 10 native snapshot records. Hidden state adds none.
            for (nodes, records, pass) in [(15, 10, true), (14, 10, false), (15, 9, false)] {
                let token = authority.issue_at(&grant(nodes, 1), 100).unwrap();
                let pool = memory();
                let (mut ordered, files) = session(
                    &commit,
                    &cx,
                    &vfs,
                    &pool,
                    &authority,
                    &token,
                    at,
                    || 100,
                    GqlQueryPolicy::new(records, 100, 1_000_000, 1_000_000),
                )
                .await;
                let prepared = ordered
                    .prepare(
                        &cx,
                        "MATCH p = (a)-[:R*0..2]->(b) RETURN a,b,p LIMIT 0",
                        &args,
                    )
                    .unwrap();
                vfs.reset();
                let before = observations(&files);
                let bytes = pool.used();
                drop(ordered.execute_ordered(&cx, &prepared, &args).unwrap());
                assert_eq!(vfs.reads.load(Ordering::SeqCst), 0);
                assert_eq!(observations(&files), before);
                assert_eq!(pool.used(), bytes);
                let execution = ordered.execute_ordered(&cx, &prepared, &args).unwrap();
                if pass {
                    let mut cursor = execution.await.unwrap();
                    assert!(values(&mut cursor).await.is_empty());
                } else {
                    let error = match execution.await {
                        Ok(_) => panic!("a one-short route allowance must refuse"),
                        Err(error) => error,
                    };
                    if nodes == 14 {
                        assert!(matches!(error, NativeSpoolError::Authorized(error)
                            if matches!(*error, QueryError::Authorization(Error::LimitExceeded(LimitDimension::Nodes)))));
                    } else {
                        assert!(matches!(error, NativeSpoolError::Authorized(error)
                            if matches!(*error, QueryError::EdgeStream(fgdb_gql::GqlQueryError::Rows(error))
                                if error.dimension == fgdb_gql::GqlBudgetDimension::SnapshotRecords
                                    && error.limit == 9 && error.observed == 10)));
                    }
                }
                assert!(!ordered.is_closed());
                // A failed route cannot strand the detached backend or poison
                // the next complete spool. This new execution needs 5 roots.
                let retry = ordered
                    .prepare(&cx, "MATCH p = (a)-[:S*0..0]->(b) RETURN a LIMIT 1", &args)
                    .unwrap();
                let mut cursor = ordered
                    .execute_ordered(&cx, &retry, &args)
                    .unwrap()
                    .await
                    .unwrap();
                assert_eq!(
                    values(&mut cursor).await,
                    vec![vec![GraphValue::Vertex(VId(1))]]
                );
                drop(cursor);
                drop(ordered);
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
