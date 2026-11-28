//! End-to-end local CLI coverage of computed native vertex pulls.

use super::*;

#[test]
fn computed_revision_preflight_retains_the_exact_marker_cut_and_refuses_a_second_selector() {
    let marker = format!("1@{}", "ab".repeat(32));
    let options = options(&[
        "--at-revision",
        &marker,
        "MATCH (n) RETURN n AS id,n.p+1 AS shifted ORDER BY id",
    ]);
    let prepared = okay(prepare(&options));
    assert_eq!(prepared.as_of, Some(CommitSeq(1)));
    assert!(matches!(
        &prepared.source,
        StreamSource::VertexProjection(_)
    ));
    let temporal = super::options(&[
        "--at-revision",
        &marker,
        "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 2 RETURN n AS id,n.p+1 AS shifted ORDER BY id",
    ]);
    assert!(prepare(&temporal).is_err());
}

#[test]
fn local_computed_vertex_streams_match_eager_rows_and_bound_historical_cuts() {
    let ((), report) = run_async_under_lab(0x636c_7601, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, resident) = fixture(&commit).await;
        for (text, seq, count, temporal) in [
            (
                "MATCH (n:L) RETURN n AS id,n.p+$by AS shifted,n.missing+$by AS missing ORDER BY id",
                2,
                3,
                false,
            ),
            (
                "MATCH (n) RETURN {node:n,shifted:n.p+$by} AS doc ORDER BY n",
                2,
                3,
                false,
            ),
            ("MATCH (n) RETURN $by AS repeated ORDER BY n", 2, 3, false),
            (
                "MATCH (n) RETURN n AS id,n.p+$by AS shifted ORDER BY id LIMIT 0",
                2,
                0,
                false,
            ),
            (
                "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $cut RETURN n AS id,n.p+$by AS shifted ORDER BY id SKIP $skip LIMIT $take",
                1,
                2,
                true,
            ),
        ] {
            for by in [7, 20] {
                let mut options = options(&[text]);
                options.params = fgdb_gql::GqlParameters::new().with_int64("by", by).unwrap();
                if temporal {
                    options.params = options
                        .params
                        .with_uint64("cut", seq)
                        .unwrap()
                        .with_uint64("skip", 1)
                        .unwrap()
                        .with_uint64("take", 2)
                        .unwrap();
                }
                let prepared = okay(prepare(&options));
                assert!(matches!(
                    &prepared.source,
                    StreamSource::VertexProjection(_)
                ));
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
                assert_eq!(rows(&output).len(), count, "{text}");
                let output = std::str::from_utf8(&output).unwrap();
                assert!(output.starts_with(&format!(
                    r#"{{"v":1,"event":"columns","stream":true,"seq":{seq},"#
                )));
                assert!(output.ends_with(&format!(
                    "{}\n",
                    stream::summary_line(seq, count as u64, true),
                )));
                drop(view);
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn local_computed_streams_refuse_unsupported_plans_and_never_complete_failed_suffixes() {
    let ((), report) = run_async_under_lab(0x636c_7602, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        // Preparation requires no usable database/key paths. LIMIT 0 cannot
        // hide an unsupported definition or turn it into an empty success.
        for text in [
            "MATCH (n) RETURN n.p+1 AS value ORDER BY n LIMIT 0",
            "MATCH (n) RETURN n AS id,n.p+1 AS value ORDER BY id DESC LIMIT 0",
            "MATCH (n) RETURN DISTINCT n AS id,n.p+1 AS value ORDER BY id LIMIT 0",
            "MATCH (a)-[r:R]->(b) RETURN r AS id,r.p+1 AS value ORDER BY id LIMIT 0",
        ] {
            assert!(prepare(&options(&[text])).is_err(), "{text}");
        }
        let (vfs, _) = fixture(&commit).await;
        for (text, cap, delivered) in [
            ("MATCH (n) RETURN n,n.p+1 AS value", Some(1), 1),
            (
                "MATCH (n) RETURN n AS id,12/(n.p-3) AS value ORDER BY id LIMIT 1",
                Some(1),
                1,
            ),
            (
                "MATCH (n) RETURN n AS id,12/(n.p-3) AS value ORDER BY id LIMIT 0",
                Some(0),
                0,
            ),
        ] {
            let mut options = options(&[text]);
            options.budget.rows = cap;
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
                .expect_err("the native failure must reach delivery");
            assert_eq!(error.class, "query");
            assert!(error.message.starts_with(&format!(
                "stream incomplete after {delivered} fully flushed row(s)",
            )));
            assert_eq!(rows(&output).len(), delivered);
            assert!(
                !std::str::from_utf8(&output)
                    .unwrap()
                    .contains(r#""event":"result""#)
            );
            drop(view);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn local_computed_output_failure_drops_the_cursor_and_preserves_view_reuse() {
    struct FailingOutput {
        bytes: Vec<u8>,
        flushes: usize,
        fail_at: usize,
    }
    impl Write for FailingOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if self.flushes == self.fail_at {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed output"))
            } else {
                Ok(())
            }
        }
    }
    let ((), report) = run_async_under_lab(0x636c_7603, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = fixture(&commit).await;
        let options = options(&["MATCH (n) RETURN {node:n,shifted:n.p+1} AS doc ORDER BY n"]);
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
        for fail_at in [1, 2, 3] {
            let mut output = FailingOutput {
                bytes: Vec::new(),
                flushes: 0,
                fail_at,
            };
            let error = run(&mut view, &cx, &options, &prepared, true, &mut output)
                .await
                .expect_err("broken transport");
            assert_eq!(error.class, "io");
            assert!(error.message.starts_with(&format!(
                "stream incomplete after {} fully flushed row(s)",
                fail_at.saturating_sub(2),
            )));
            assert_eq!(output.flushes, fail_at);
            assert!(
                !std::str::from_utf8(&output.bytes)
                    .unwrap()
                    .contains(r#""event":"result""#)
            );
            let mut retry = Vec::new();
            okay(run(&mut view, &cx, &options, &prepared, true, &mut retry).await);
            assert_eq!(rows(&retry).len(), 3);
            assert!(
                std::str::from_utf8(&retry)
                    .unwrap()
                    .ends_with(&format!("{}\n", stream::summary_line(2, 3, true),))
            );
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
