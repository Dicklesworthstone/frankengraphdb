//! Public cold vertex probes compose through the shared ordered graph program.

use super::*;

#[test]
fn cold_vertex_probes_compose_with_sort_union_and_exact_aggregates() {
    use GraphAggregateValue::{Count, Integer};
    let ((), report) = run_async_under_lab(0xa8_0301, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let authority = issuer();
        let token = authority.issue_at(&grant(100, 10), 100).unwrap();
        let args = GqlParameters::new();
        for hidden in [false, true] {
            let (vfs, _db, old, current) = fixture(&commit, hidden).await;
            for at in [old, current] {
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
                    policy(10),
                )
                .await;
                let first = if at == old { 3 } else { 30 };
                for (text, expected) in [
                    (
                        "MATCH (n) WHERE EXISTS { MATCH (n)-[r:R|S]->(m) \
                         WHERE r.score >= 7 AND m.score = 1 \
                         AND r.secret IS NULL AND m.secret IS NULL } \
                         RETURN n,n.score AS score,n.secret ORDER BY score DESC,n",
                        vec![vec![GraphValue::Vertex(VId(1)), scalar(first), null()]],
                    ),
                    (
                        "MATCH (n) WHERE NOT EXISTS { MATCH (n)-[:R]->(m) } \
                         RETURN n.score AS score ORDER BY score",
                        vec![vec![scalar(-1)], vec![scalar(2)], vec![scalar(3)]],
                    ),
                    (
                        "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) } RETURN n \
                         UNION ALL MATCH (n) WHERE NOT EXISTS { MATCH (n)-[:R]->(m) } \
                         RETURN n ORDER BY n",
                        (1..=5)
                            .map(|id| vec![GraphValue::Vertex(VId(id))])
                            .collect(),
                    ),
                ] {
                    let prepared = ordered.prepare(&cx, text, &args).unwrap();
                    let mut cursor = ordered
                        .execute_ordered(&cx, &prepared, &args)
                        .unwrap()
                        .await
                        .unwrap();
                    assert_eq!(cursor.snapshot_seq(), at);
                    assert_eq!(
                        values(&mut cursor).await,
                        expected,
                        "{text}; hidden={hidden}"
                    );
                    drop(cursor);
                    assert!(!ordered.is_closed());
                }
                let prepared = ordered
                    .prepare(
                        &cx,
                        "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) } \
                         RETURN count(*) AS roots,sum(n.score) AS total,count(n.secret) AS hidden",
                        &args,
                    )
                    .unwrap();
                let mut cursor = ordered
                    .execute_ordered_aggregate(&cx, &prepared, &args)
                    .unwrap()
                    .await
                    .unwrap();
                // Parallel child edges prove one outer root and never
                // multiply its contribution to the count or sum.
                assert_eq!(
                    aggregates(&mut cursor).await,
                    vec![vec![Count(2), Integer(i128::from(first + 1)), Count(0)]],
                );
                drop(cursor);
                drop(ordered);
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_unsupported_vertex_probe_demand_refuses_before_source_or_spill_io() {
    let ((), report) = run_async_under_lab(0xa8_0302, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _db, at, _) = fixture(&commit, false).await;
        let authority = issuer();
        let token = authority.issue_at(&grant(100, 10), 100).unwrap();
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
            policy(10),
        )
        .await;
        let args = GqlParameters::new();
        for text in [
            "MATCH (n) WHERE EXISTS { MATCH (n)-[:R*1..2]->(m) } RETURN n ORDER BY n LIMIT 0",
            "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) } RETURN n,properties(n) ORDER BY n LIMIT 0",
        ] {
            let prepared = ordered.prepare(&cx, text, &args).unwrap();
            vfs.reset();
            let before = observations(&files);
            assert!(
                ordered.execute_ordered(&cx, &prepared, &args).is_err(),
                "{text}"
            );
            assert_eq!(vfs.reads.load(Ordering::SeqCst), 0);
            assert_eq!(observations(&files), before);
            assert!(!ordered.is_closed());
        }
        drop(ordered);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
