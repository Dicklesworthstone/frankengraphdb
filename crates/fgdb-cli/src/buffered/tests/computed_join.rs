//! Local CLI fixed-hop projection routing and completion behavior.

use super::*;

#[test]
fn local_computed_join_delivery_preserves_temporal_markers_and_reports_late_failure() {
    let ((), report) = run_async_under_lab(0x636c_7620, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, resident) = fixture(&commit).await;

        let marker = format!("1@{}", "cd".repeat(32));
        let selected = options(&[
            "--at-revision",
            &marker,
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r AS first,s AS second,r.p+s.p AS cost ORDER BY first,second",
        ]);
        let prepared = okay(prepare(&selected));
        assert_eq!(prepared.as_of, Some(CommitSeq(1)));
        assert!(matches!(&prepared.source, StreamSource::Projection(_)));
        let conflicting = options(&[
            "--at-revision",
            &marker,
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) FOR SYSTEM_TIME AS OF SEQ 2 RETURN r,s,r.p+s.p AS cost",
        ]);
        assert!(prepare(&conflicting).is_err());
        for text in [
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r AS first,s AS second,r.p+s.p AS cost ORDER BY first LIMIT 0",
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r AS first,s AS second,r.p+s.p AS cost ORDER BY second,first LIMIT 0",
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r.p+s.p AS cost ORDER BY r,s LIMIT 0",
        ] {
            assert!(prepare(&options(&[text])).is_err(), "{text}");
        }

        for (text, seq, count, temporal) in [
            (
                "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r AS first,s AS second,r.p+s.p+a.p+c.p+$by AS cost ORDER BY first,second",
                2,
                2,
                false,
            ),
            (
                "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN {first:r,second:s,cost:r.p+s.p+$by} AS doc ORDER BY r,s",
                2,
                2,
                false,
            ),
            (
                "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN $by AS repeated ORDER BY r,s",
                2,
                2,
                false,
            ),
            (
                "MATCH (a)-[r:R]->(b)-[s:R]->(c) FOR SYSTEM_TIME AS OF SEQ $cut RETURN r AS first,s AS second,r.p+s.p+$by AS cost ORDER BY first,second SKIP $skip LIMIT $take",
                1,
                1,
                true,
            ),
        ] {
            for by in [7, 20] {
                let mut options = options(&[text]);
                // Match the existing two-hop CLI fixture's retained edge and
                // endpoint working-set allowance.
                options.buffered.memory = Some(32 * 1024 * 1024);
                options.params = fgdb_gql::GqlParameters::new().with_int64("by", by).unwrap();
                if temporal {
                    options.params = options
                        .params
                        .with_uint64("cut", seq)
                        .unwrap()
                        .with_uint64("skip", 1)
                        .unwrap()
                        .with_uint64("take", 1)
                        .unwrap();
                }
                let prepared = okay(prepare(&options));
                assert!(matches!(&prepared.source, StreamSource::Projection(_)));
                let (pool, limits) = okay(options.buffered.admission(options.budget.policy()));
                let mut view = Database::open_buffered_read_view_with_vfs(
                    &commit,
                    vfs.clone(),
                    vfs.database_dir(),
                    keys(),
                    pool.clone(),
                    limits,
                )
                .await
                .unwrap();
                let eager = resident
                    .query(
                        &cx,
                        text,
                        &options.params,
                        &options,
                        options.budget.policy(),
                    )
                    .unwrap();
                let mut expected = Vec::new();
                okay(crate::render(eager, seq, "rows", true, &mut expected));
                let mut output = Vec::new();
                okay(run(&mut view, &cx, &options, &prepared, true, &mut output).await);
                assert_eq!(rows(&output), rows(&expected), "{text}, by {by}");
                assert_eq!(rows(&output).len(), count);
                assert!(std::str::from_utf8(&output).unwrap().ends_with(&format!(
                    "{}\n",
                    stream::summary_line(seq, count as u64, true),
                )));
                drop(view);
                assert_eq!(pool.used(), 0);
            }
        }

        for count in [0, 1] {
            let text = format!(
                "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r AS first,s AS second,12/(r.p-8) AS cost ORDER BY first,second LIMIT {count}",
            );
            let mut options = options(&[&text]);
            options.buffered.memory = Some(32 * 1024 * 1024);
            options.budget.rows = Some(count);
            let prepared = okay(prepare(&options));
            let (pool, limits) = okay(options.buffered.admission(options.budget.policy()));
            let mut view = Database::open_buffered_read_view_with_vfs(
                &commit,
                vfs.clone(),
                vfs.database_dir(),
                keys(),
                pool.clone(),
                limits,
            )
            .await
            .unwrap();
            let mut output = Vec::new();
            let error = run(&mut view, &cx, &options, &prepared, true, &mut output)
                .await
                .expect_err("unselected join suffix must still fail");
            assert_eq!(error.class, "query");
            assert!(error.message.starts_with(&format!(
                "stream incomplete after {count} fully flushed row(s)",
            )));
            assert_eq!(rows(&output).len(), count as usize);
            assert!(
                !std::str::from_utf8(&output)
                    .unwrap()
                    .contains(r#""event":"result""#)
            );

            let mut retry_options =
                super::options(&["MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r,s,r.p+s.p AS cost"]);
            retry_options.budget.rows = Some(2);
            let retry = okay(prepare(&retry_options));
            let mut output = Vec::new();
            okay(run(&mut view, &cx, &retry_options, &retry, true, &mut output).await);
            assert_eq!(rows(&output).len(), 2);
            assert!(
                std::str::from_utf8(&output)
                    .unwrap()
                    .ends_with(&format!("{}\n", stream::summary_line(2, 2, true),))
            );
            drop(view);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
