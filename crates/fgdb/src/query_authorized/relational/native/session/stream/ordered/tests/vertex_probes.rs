//! Vertex-rooted semi/anti probes inside complete external relations.
use super::*;

#[test]
fn probe_relations_preserve_native_bags_order_and_masked_work() {
    let ((), report) = run_async_under_lab(0x8600_00a0, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let db = projected_collection_database(&commit, true).await;
        let slim = projected_collection_database(&commit, false).await;
        let issuer = authority();
        let pool = MemoryPool::new(65_536, 0).unwrap();
        let params = GqlParameters::new();
        for (text, expected_bag) in [
            (
                "MATCH (n:L) WHERE EXISTS { MATCH (n)-[:R]->(m) } \
                 RETURN n.p+1 AS value UNION ALL \
                 MATCH (n:L) WHERE NOT EXISTS { MATCH (n)-[:R]->(m) } \
                 RETURN n.p+101 AS value ORDER BY value",
                vec![
                    vec![scalar(2)],
                    vec![scalar(4)],
                    vec![scalar(100)],
                    vec![scalar(103)],
                    vec![scalar(104)],
                ],
            ),
            (
                "MATCH (n:L) WHERE EXISTS { \
                 MATCH (n)-[:R*1..2]->(m) WHERE m.p=3 } \
                 RETURN n.p+1 AS value UNION ALL \
                 MATCH (n:L) WHERE NOT EXISTS { \
                 MATCH (n)-[:R*1..2]->(m) WHERE m.p=3 } \
                 RETURN n.p+101 AS value ORDER BY value DESC",
                vec![
                    vec![scalar(2)],
                    vec![scalar(4)],
                    vec![scalar(100)],
                    vec![scalar(103)],
                    vec![scalar(104)],
                ],
            ),
            (
                "UNWIND [1,1,2] AS wanted CALL (wanted) { \
                 MATCH (n:L) WHERE n.p=wanted AND EXISTS { MATCH (n)-[:R]->(m) } \
                 RETURN n.p+1 AS value } RETURN value ORDER BY value",
                vec![vec![scalar(2)], vec![scalar(2)]],
            ),
            (
                "MATCH (n:L) WHERE EXISTS { MATCH (n)-[e:R]->(m) \
                 WHERE m.hidden IS NULL AND e.hidden IS NULL } \
                 RETURN n.p+1 AS value UNION ALL \
                 MATCH (n:L) WHERE NOT EXISTS { MATCH (n)-[e:R]->(m) \
                 WHERE m.hidden IS NULL AND e.hidden IS NULL } \
                 RETURN n.p+101 AS value ORDER BY value",
                vec![
                    vec![scalar(2)],
                    vec![scalar(4)],
                    vec![scalar(100)],
                    vec![scalar(103)],
                    vec![scalar(104)],
                ],
            ),
        ] {
            let mut allowed = grant();
            allowed.limits.max_rows = expected_bag.len() as u64;
            let token = issuer.issue_at(&allowed, 100).unwrap();
            let calls = AtomicU64::new(0);
            let clock = || {
                calls.fetch_add(1, Ordering::Relaxed);
                100
            };
            let mut observation = None;
            for graph in [&db, &slim] {
                let mut reference = graph
                    .authorized_read_session(
                        &cx,
                        &issuer,
                        &token,
                        "main",
                        symbols as Resolver,
                        policy(),
                        || 100,
                    )
                    .unwrap();
                let prepared = reference.prepare(&cx, text, &params).unwrap();
                let native = reference.execute(&cx, &prepared, &params).unwrap();
                let (mut ordered, _, _) =
                    session(graph, &cx, &issuer, &token, &pool, clock, limits()).await;
                let prepared = ordered.prepare(&cx, text, &params).unwrap();
                let before = calls.load(Ordering::Relaxed);
                let mut cursor = ordered
                    .execute_ordered(&cx, &prepared, &params)
                    .unwrap_or_else(|error| panic!("{text}: {error:?}"))
                    .await
                    .unwrap();
                let columns = cursor.columns().to_vec();
                let rows = values(&mut cursor).await;
                let work = calls.load(Ordering::Relaxed) - before;
                assert_eq!(
                    native,
                    QueryResult::Rows {
                        columns,
                        rows: rows
                            .iter()
                            .map(|row| row.iter().cloned().map(QueryValue::Value).collect())
                            .collect(),
                    },
                    "{text}",
                );
                let mut bag = rows.clone();
                bag.sort();
                assert_eq!(bag, expected_bag, "{text}");
                if let Some((previous_rows, previous_work)) = &observation {
                    assert_eq!(&rows, previous_rows, "hidden graph changed rows: {text}");
                    assert_eq!(&work, previous_work, "hidden graph changed work: {text}");
                } else {
                    observation = Some((rows, work));
                }
                drop(cursor);
                assert!(!ordered.is_closed());
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn probe_relation_empty_pages_preserve_reached_errors_and_private_limits() {
    let ((), report) = run_async_under_lab(0x8600_00a1, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let mut allowed = grant();
        allowed.limits.max_rows = 0;
        let token = issuer.issue_at(&allowed, 100).unwrap();
        let pool = MemoryPool::new(65_536, 0).unwrap();
        let params = GqlParameters::new();
        let (mut ordered, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let prepared = ordered
            .prepare(
                &cx,
                "MATCH (n:L) WHERE EXISTS { MATCH (n)-[:R]->(m) } \
                 RETURN n.p+1 AS value UNION ALL \
                 MATCH (n:L) WHERE NOT EXISTS { MATCH (n)-[:R]->(m) } \
                 RETURN 10/(n.p-2) AS value LIMIT 0",
                &params,
            )
            .unwrap();
        let error = ordered
            .execute_ordered(&cx, &prepared, &params)
            .unwrap()
            .await
            .unwrap_err();
        assert!(matches!(error, NativeSpoolError::Authorized(error)
            if matches!(*error, QueryError::Set(GqlQueryError::Source(
                GraphSetExecutionError::Projection { error, .. }))
                if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero)));
        assert!(!ordered.is_closed());
        assert_eq!(pool.used(), 0);
        drop(ordered);

        // Both witnesses from the first branch must be admitted before the
        // final empty page. They are private input, not delivered result rows.
        let mut physical = limits();
        physical.max_input_rows = 1;
        let (mut ordered, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, physical).await;
        let prepared = ordered
            .prepare(
                &cx,
                "MATCH (n:L) WHERE EXISTS { MATCH (n)-[:R]->(m) } \
                 RETURN n.p+1 AS value UNION ALL \
                 MATCH (n:L) WHERE NOT EXISTS { MATCH (n)-[:R]->(m) } \
                 RETURN n.p+1 AS value LIMIT 0",
                &params,
            )
            .unwrap();
        let error = ordered
            .execute_ordered(&cx, &prepared, &params)
            .unwrap()
            .await
            .unwrap_err();
        assert!(matches!(error, NativeSpoolError::Authorized(error)
            if matches!(*error, QueryError::Stream(GqlQueryError::Rows(error))
                if error.dimension == GqlBudgetDimension::ResultRows
                    && error.limit == 1 && error.observed == 2)));
        assert!(!ordered.is_closed());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unsupported_walk_siblings_refuse_before_any_probe_or_scratch_is_opened() {
    let ((), report) = run_async_under_lab(0x8600_00a2, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(65_536, 0).unwrap();
        let (mut ordered, scratch, destination) =
            session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let params = GqlParameters::new();
        let prepared = ordered
            .prepare(
                &cx,
                "MATCH (n:L) WHERE EXISTS { MATCH (n)-[:R]->(m) } \
                 RETURN n.p+1 AS value UNION ALL \
                 MATCH (a:L)-[:R*1..2]->(b) RETURN b.p+1 AS value LIMIT 0",
                &params,
            )
            .unwrap();
        assert!(matches!(ordered.execute_ordered(&cx, &prepared, &params),
            Err(NativeSpoolError::Authorized(error))
                if matches!(*error, QueryError::StreamingUnsupported { .. })));
        assert!(!ordered.is_closed());
        assert!(scratch.0.lock().unwrap().bytes.get_ref().is_empty());
        assert!(destination.0.lock().unwrap().bytes.get_ref().is_empty());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
