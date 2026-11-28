//! CLI transport of the same native computed edge/endpoint cursor.

use super::*;

#[test]
fn local_computed_edge_frames_match_eager_rows_at_current_and_bound_historical_cuts() {
    let ((), report) = run_async_under_lab(0x636c_7610, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, resident) = fixture(&commit).await;
        for (text, seq, count, temporal) in [
            (
                "MATCH (a)-[r:R]->(b) RETURN r AS id,r.p+a.p+b.p+$by AS cost ORDER BY id",
                2,
                3,
                false,
            ),
            (
                "MATCH (a)<-[r:R]-(b) RETURN r AS id,a.p-b.p+r.p+$by AS cost ORDER BY id",
                2,
                3,
                false,
            ),
            (
                "MATCH (a)-[r:R]-(b) RETURN r AS id,a AS origin,r.p+a.p+$by AS cost ORDER BY id,origin",
                2,
                5,
                false,
            ),
            (
                "MATCH (a)-[r:R]-(b) RETURN {edge:r,origin:a,cost:r.p+a.p+b.p+$by,missing:b.missing} AS doc ORDER BY r,a",
                2,
                5,
                false,
            ),
            (
                "MATCH (a)-[r:R]-(b) RETURN $by AS repeated ORDER BY r,a",
                2,
                5,
                false,
            ),
            (
                "MATCH (a)-[r:R]-(b) FOR SYSTEM_TIME AS OF SEQ $cut RETURN r AS id,a AS origin,r.p+a.p+$by AS cost ORDER BY id,origin SKIP $skip LIMIT $take",
                1,
                3,
                true,
            ),
        ] {
            let mut options = options(&[text]);
            options.params = fgdb_gql::GqlParameters::new().with_int64("by", 7).unwrap();
            if temporal {
                options.params = options
                    .params
                    .with_uint64("cut", seq)
                    .unwrap()
                    .with_uint64("skip", 1)
                    .unwrap()
                    .with_uint64("take", 3)
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
            assert_eq!(rows(&output), rows(&expected), "{text}");
            assert_eq!(rows(&output).len(), count);
            assert!(std::str::from_utf8(&output).unwrap().ends_with(&format!(
                "{}\n",
                stream::summary_line(seq, count as u64, true),
            )));
            drop(view);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_edge_preflight_proves_the_complete_order_and_preserves_revision_selection() {
    let marker = format!("1@{}", "bc".repeat(32));
    let selected = options(&[
        "--at-revision",
        &marker,
        "MATCH (a)-[r:R]-(b) RETURN r AS id,a AS origin,r.p+a.p AS cost ORDER BY id,origin",
    ]);
    let prepared = okay(prepare(&selected));
    assert_eq!(prepared.as_of, Some(CommitSeq(1)));
    assert!(matches!(&prepared.source, StreamSource::Projection(_)));
    for text in [
        "MATCH (a)-[r:R]->(b) RETURN r.p+a.p AS cost ORDER BY r LIMIT 0",
        "MATCH (a)-[r:R]-(b) RETURN r AS id,r.p+1 AS cost ORDER BY id LIMIT 0",
        "MATCH (a)-[r:R]-(b) RETURN r AS id,a AS origin,r.p+1 AS cost ORDER BY origin,id LIMIT 0",
        "MATCH (a)-[r:R]->(b) RETURN r AS id,r.p+1 AS cost ORDER BY id DESC LIMIT 0",
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r AS id,s AS next_id,r.p+s.p AS cost ORDER BY id,next_id LIMIT 0",
    ] {
        assert!(prepare(&options(&[text])).is_err(), "{text}");
    }
    let temporal = options(&[
        "--at-revision",
        &marker,
        "MATCH (a)-[r:R]->(b) FOR SYSTEM_TIME AS OF SEQ 2 RETURN r AS id,r.p+1 AS cost ORDER BY id",
    ]);
    assert!(prepare(&temporal).is_err());
}

#[test]
fn computed_edge_suffix_budget_and_transport_failures_cannot_publish_success() {
    struct ClosedOutput {
        bytes: Vec<u8>,
        flushes: usize,
    }
    impl Write for ClosedOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if self.flushes == 3 {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "closed second orientation",
                ))
            } else {
                Ok(())
            }
        }
    }
    let ((), report) = run_async_under_lab(0x636c_7611, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = fixture(&commit).await;
        for (text, cap, delivered) in [
            ("MATCH (a)-[r:R]-(b) RETURN r,a,r.p+a.p AS cost", 1, 1),
            (
                "MATCH (a)-[r:R]-(b) RETURN r AS id,a AS origin,12/(r.p-9) AS cost ORDER BY id,origin LIMIT 1",
                1,
                1,
            ),
            (
                "MATCH (a)-[r:R]-(b) RETURN r AS id,a AS origin,12/(r.p-9) AS cost ORDER BY id,origin LIMIT 0",
                0,
                0,
            ),
        ] {
            let mut options = options(&[text]);
            options.budget.rows = Some(cap);
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
                .expect_err("late edge failure");
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

        let options = options(&[
            "MATCH (a)-[r:R]-(b) RETURN {edge:r,origin:a,cost:r.p+a.p} AS doc ORDER BY r,a",
        ]);
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
        let mut output = ClosedOutput {
            bytes: Vec::new(),
            flushes: 0,
        };
        let error = run(&mut view, &cx, &options, &prepared, true, &mut output)
            .await
            .expect_err("broken output");
        assert_eq!(error.class, "io");
        assert_eq!(output.flushes, 3);
        assert!(
            error
                .message
                .starts_with("stream incomplete after 1 fully flushed row(s)")
        );
        assert!(
            !std::str::from_utf8(&output.bytes)
                .unwrap()
                .contains(r#""event":"result""#)
        );
        let mut retry = Vec::new();
        okay(run(&mut view, &cx, &options, &prepared, true, &mut retry).await);
        assert_eq!(rows(&retry).len(), 5);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
