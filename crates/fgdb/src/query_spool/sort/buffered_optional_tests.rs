//! Cold OPTIONAL witnesses feed the ordinary complete external row owner.
use super::*;

fn encoded(rows: Vec<Vec<GraphValue>>) -> Vec<Vec<u8>> {
    rows.into_iter()
        .map(|row| {
            GraphValueRow::from_owned_values(row)
                .canonical_bytes()
                .unwrap()
        })
        .collect()
}

#[test]
fn cold_optional_keeps_historical_fields_null_extension_and_terminal_pages() {
    let ((), report) = run_async_under_lab(0xc01d_3017, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let source_pool = MemoryPool::new(32_000_000, 0).unwrap();
        let spill_pool = MemoryPool::new(1_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &source_pool).await;
        let baseline = source_pool.used();
        for cut in [1, 2] {
            let null = GraphValue::Scalar(CanonicalScalar::Null);
            let expected = if cut == 1 {
                vec![
                    vec![scalar(7), scalar(4)],
                    vec![scalar(8), scalar(4)],
                    vec![scalar(9), scalar(4)],
                    vec![scalar(10), null.clone()],
                ]
            } else {
                vec![
                    vec![scalar(8), scalar(14)],
                    vec![scalar(9), scalar(14)],
                    vec![scalar(10), null.clone()],
                    vec![scalar(17), scalar(14)],
                ]
            };
            let head = format!(
                "MATCH (a)-[r:R]->(b) OPTIONAL MATCH (b)-[s:R]->(c) \
                 WHERE s.p >= 10 FOR SYSTEM_TIME AS OF SEQ {cut}"
            );
            for (tail, rows) in [
                (
                    "RETURN r.p AS left,c.p AS right ORDER BY left,right",
                    expected.clone(),
                ),
                (
                    "RETURN r.p AS left,c.p AS right ORDER BY left,right SKIP 1 LIMIT 2",
                    expected[1..3].to_vec(),
                ),
                (
                    "RETURN DISTINCT c.p AS value ORDER BY value NULLS LAST SKIP 1",
                    vec![vec![null]],
                ),
                (
                    "RETURN r.p AS left,c.p AS right ORDER BY left,right LIMIT 0",
                    Vec::new(),
                ),
            ] {
                let text = format!("{head} {tail}");
                let plan = prepare(&text);
                let (mut scratch, _) = file(&cx, &spill_pool).await;
                let (mut destination, _) = file(&cx, &spill_pool).await;
                let (mut auxiliary, _) = file(&cx, &spill_pool).await;
                let (spool, _) = plan
                    .spool_in_view(
                        &mut view,
                        &cx,
                        policy(rows.len() as u64),
                        &mut scratch,
                        &mut destination,
                        &mut auxiliary,
                        1,
                        32,
                        1,
                        32,
                        31,
                        8192,
                        100,
                        WORK,
                    )
                    .await
                    .unwrap_or_else(|error| panic!("{text}: {error}"));
                assert_eq!(spool.snapshot_seq(), CommitSeq(cut));
                assert_eq!(
                    contents(&spool, &mut destination, &cx).await,
                    encoded(rows),
                    "{text}"
                );
                assert!(
                    spool.row_stats().snapshot_records > 4,
                    "the optional body was executed even under LIMIT 0"
                );
                drop((spool, scratch, destination, auxiliary));
                assert_eq!(source_pool.used(), baseline);
                assert_eq!(spill_pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_optional_body_errors_publish_no_spool_even_under_limit_zero() {
    let ((), report) = run_async_under_lab(0xc01d_3018, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let source_pool = MemoryPool::new(32_000_000, 0).unwrap();
        let spill_pool = MemoryPool::new(1_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &source_pool).await;
        let baseline = source_pool.used();
        for tail in ["", "LIMIT 0"] {
            let plan = prepare(&format!(
                "MATCH (a)-[r:R]->(b) OPTIONAL MATCH (b)-[s:R]->(c) \
                 WHERE 1 / (s.p - s.p) > 0 RETURN r,s,c {tail}"
            ));
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (mut auxiliary, _) = file(&cx, &spill_pool).await;
            let result = plan
                .spool_in_view(
                    &mut view,
                    &cx,
                    policy(100),
                    &mut scratch,
                    &mut destination,
                    &mut auxiliary,
                    1,
                    32,
                    1,
                    32,
                    31,
                    8192,
                    100,
                    WORK,
                )
                .await;
            assert!(result.is_err());
            assert_eq!(destination.stats().published_runs, 0);
            drop((scratch, destination, auxiliary));
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
