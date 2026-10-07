use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::io;

fn okay<T>(value: Result<T, Failure>) -> T {
    value.unwrap_or_else(|error| panic!("{}: {}", error.class, error.message))
}
fn parent() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "fgdb-cli-spill-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&path).unwrap();
    path
}
fn empty(path: &Path) {
    assert_eq!(
        std::fs::read_dir(path).unwrap().count(),
        0,
        "scratch must retire"
    );
}
fn options(parent: &Path, text: &str) -> Options {
    let args = [
        "--db",
        "unused",
        "--key-file",
        "unused",
        "--property",
        "p=1",
        "--property",
        "note=2",
        "--relation",
        "R=1",
        "--spill-dir",
        parent.to_str().unwrap(),
        "--spill-memory-bytes",
        "131072",
        text,
    ]
    .map(str::to_owned);
    okay(crate::parse(&args, "query"))
}
async fn fixture(cx: &fgdb_types::CommitCx) -> Database<MemVfs> {
    let keys = DatabaseKeys::new(
        [0xd1; 32],
        DatabaseSecurityNamespaceId([0xd2; 32]),
        [0xd3; 32],
    );
    let mut db = Database::open_memory(cx, keys).await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    let note = CanonicalScalar::ucs_basic_text(&"é-padding".repeat(512)).unwrap();
    for id in 0..96 {
        batch.create_vertex(
            VId(id),
            vec![],
            vec![
                (
                    PropertyKeyId(1),
                    CanonicalScalar::Int(((id * 17) % 13) as i64),
                ),
                (PropertyKeyId(2), note.clone()),
            ],
        );
    }
    for (id, a, b) in [(1, 0, 1), (2, 0, 1), (3, 1, 1), (4, 1, 2)] {
        batch.add_edge(EId(id), VId(a), VId(b), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    db
}
fn rows(bytes: &[u8]) -> Vec<&str> {
    std::str::from_utf8(bytes)
        .unwrap()
        .lines()
        .filter(|line| line.contains(r#""event":"row""#))
        .collect()
}

#[test]
fn external_grouping_matches_eager_rows_for_vertices_edges_nulls_and_history() {
    let ((), report) = run_async_under_lab(0x5b118, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = fixture(&contexts.commit()).await;
        let mut changes = WriteBatch::new(RelationId(1));
        changes.set_vertex_property(VId(0), PropertyKeyId(1), None);
        changes.set_vertex_property(
            VId(1),
            PropertyKeyId(1),
            Some(CanonicalScalar::Int(i64::MAX)),
        );
        changes.set_vertex_property(
            VId(2),
            PropertyKeyId(1),
            Some(CanonicalScalar::Int(i64::MAX)),
        );
        db.write(&contexts.commit(), changes).await.unwrap();
        let view = db.read_session().unwrap();
        let cx = contexts.query();
        let directory = parent();
        for (text, seq) in [
            (
                "MATCH (n) RETURN n.p AS p, count(*) AS count, sum(n.p) AS total, avg(n.p) AS mean, min(n.p) AS lo, max(n.p) AS hi",
                2,
            ),
            (
                "MATCH (n) RETURN count(*) AS count, count(n.p) AS present, sum(n.p) AS total, avg(n.p) AS mean",
                2,
            ),
            (
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p AS p, count(*) AS count, sum(n.p) AS total, avg(n.p) AS mean",
                1,
            ),
            (
                "MATCH (a)-[e:R]->(b) RETURN a.p AS p, count(*) AS count, sum(b.p) AS total, avg(b.p) AS mean",
                2,
            ),
            (
                "MATCH (a)-[e:R]-(b) RETURN b.p AS p, count(*) AS count, min(a.p) AS lo, max(a.p) AS hi",
                2,
            ),
            (
                "MATCH (n) WHERE n.p < 0 RETURN count(*) AS count, sum(n.p) AS total, avg(n.p) AS mean",
                2,
            ),
            (
                "MATCH (n) WHERE n.p < 0 RETURN n.p AS p, count(*) AS count",
                2,
            ),
        ] {
            let mut options = options(&directory, text);
            // This derives one resident group, forcing repeated partitioning
            // for the multi-group cases instead of keeping the input catalog.
            options.spill.memory = Some(262_144);
            let eager = view
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
            okay(run(&view, &cx, &options, None, true, &mut output).await);
            assert_eq!(rows(&output), rows(&expected), "{text}");
            assert!(
                std::str::from_utf8(&output)
                    .unwrap()
                    .contains(&format!(r#""stream":true,"seq":{seq}"#))
            );
            if text.starts_with("MATCH (n) RETURN count(*)") {
                let text = std::str::from_utf8(&output).unwrap();
                assert!(text.contains(r#""type":"wideint""#));
                assert!(text.contains(r#""type":"average""#));
                assert!(text.contains(r#""type":"count""#));
            }
            empty(&directory);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn external_grouping_delivers_more_result_bytes_than_the_shared_pool() {
    let ((), report) = run_async_under_lab(0x5b119, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let cx = contexts.query();
        let directory = parent();
        let mut options = options(
            &directory,
            "MATCH (n) RETURN n AS id, min(n.note) AS note, count(*) AS count ORDER BY count DESC,id DESC",
        );
        options.spill.memory = Some(262_144);
        let eager = view
            .query(
                &cx,
                &options.text,
                &options.params,
                &options,
                options.budget.policy(),
            )
            .unwrap();
        let mut expected = Vec::new();
        okay(crate::render(eager, 1, "rows", true, &mut expected));
        for robot in [true, false] {
            let mut output = Vec::new();
            okay(run(&view, &cx, &options, None, robot, &mut output).await);
            assert!(output.len() > 262_144);
            if robot {
                assert_eq!(rows(&output), rows(&expected));
                assert_eq!(rows(&output).len(), 96);
            } else {
                let text = std::str::from_utf8(&output).unwrap();
                assert_eq!(text.lines().next(), Some("id\tnote\tcount"));
                assert!(
                    text.lines()
                        .skip(1)
                        .take(96)
                        .all(|line| line.split('\t').count() == 3)
                );
            }
            empty(&directory);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn external_group_result_clauses_keep_hidden_cells_private_and_select_final_rows() {
    let ((), report) = run_async_under_lab(0x5b11d, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let cx = contexts.query();
        let directory = parent();
        for text in [
            "MATCH (n) RETURN n.p AS p,count(*) AS count,avg(n.p) AS mean HAVING count >= 7 ORDER BY mean DESC SKIP 1 LIMIT 3",
            "MATCH (n) RETURN count(*) AS count GROUP BY n.p ORDER BY sum(n.p) DESC LIMIT 4",
            "MATCH (n) RETURN n.p AS first,n.p AS again,count(*) AS count GROUP BY n.p ORDER BY count DESC LIMIT 3",
            "MATCH (a)-[e:R]-(b) RETURN count(*) AS count GROUP BY b.p HAVING count > 0 ORDER BY sum(a.p) DESC LIMIT 2",
            "MATCH (n) RETURN count(*) AS count HAVING count > 0 ORDER BY count LIMIT 0",
            "MATCH (n) RETURN n.p AS p,count(*) AS count HAVING count < 0 LIMIT 1",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN count(*) AS count GROUP BY n.p ORDER BY avg(n.p) DESC SKIP 12 LIMIT 10",
        ] {
            let mut options = options(&directory, text);
            options.spill.memory = Some(262_144);
            let eager = view
                .query(
                    &cx,
                    text,
                    &options.params,
                    &options,
                    options.budget.policy(),
                )
                .unwrap();
            let mut expected = Vec::new();
            okay(crate::render(eager, 1, "rows", true, &mut expected));
            // Only selected result rows may consume this allowance; all 96
            // source vertices and all complete groups still need evaluation.
            options.budget.rows = Some(rows(&expected).len() as u64);
            let mut output = Vec::new();
            okay(run(&view, &cx, &options, None, true, &mut output).await);
            assert_eq!(rows(&output), rows(&expected), "{text}");
            let output = std::str::from_utf8(&output).unwrap();
            assert!(output.contains(r#""event":"result""#), "{text}");
            if text.starts_with("MATCH (n) RETURN count(*) AS count GROUP BY") {
                assert!(
                    output
                        .lines()
                        .next()
                        .unwrap()
                        .contains(r#""columns":["count"]"#)
                );
                assert!(
                    rows(output.as_bytes())
                        .iter()
                        .all(|row| row.matches(r#""type":"count""#).count() == 1)
                );
            }
            empty(&directory);
        }
        let mut invalid = options(
            &directory,
            "MATCH (n) RETURN min(n.note) AS note GROUP BY n.p HAVING note > 0 OR TRUE ORDER BY count(*) DESC LIMIT 0",
        );
        invalid.spill.memory = Some(262_144);
        let mut output = Vec::new();
        let error = run(&view, &cx, &invalid, None, true, &mut output)
            .await
            .expect_err("HAVING must evaluate every numeric operand before an empty page");
        assert_eq!(error.code, 3);
        assert!(output.is_empty());
        empty(&directory);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn external_computed_aggregate_inputs_match_eager_rows_and_retire_failed_scratch() {
    let ((), report) = run_async_under_lab(0x5b11e, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let cx = contexts.query();
        let directory = parent();
        for text in [
            "MATCH (n) RETURN n.p%3 AS bucket,SUM(n.p*2) AS total,AVG(n.p+1) AS average GROUP BY n.p%3 ORDER BY total DESC",
            "MATCH (n) RETURN COUNT(*) AS count GROUP BY n.p%3 HAVING SUM(n.p*2)>0 ORDER BY AVG(n.p+1) DESC SKIP 1 LIMIT 1",
            "MATCH (a)-[e:R]->(b) RETURN b.p%3 AS bucket,SUM(a.p+b.p) AS total GROUP BY b.p%3 ORDER BY bucket",
        ] {
            let mut options = options(&directory, text);
            options.spill.memory = Some(262_144);
            let eager = view
                .query(
                    &cx,
                    text,
                    &options.params,
                    &options,
                    options.budget.policy(),
                )
                .unwrap();
            let mut expected = Vec::new();
            okay(crate::render(eager, 1, "rows", true, &mut expected));
            options.budget.rows = Some(rows(&expected).len() as u64);
            let mut actual = Vec::new();
            okay(run(&view, &cx, &options, None, true, &mut actual).await);
            assert_eq!(rows(&actual), rows(&expected), "{text}");
            empty(&directory);
        }
        for text in [
            "MATCH (n) RETURN SUM(10/(n.p-12)) AS total LIMIT 0",
            "MATCH (n) RETURN n.p AS bucket,SUM(10/(n.p-12)) AS total GROUP BY n.p ORDER BY bucket LIMIT 1",
            "MATCH (a)-[e:R]->(b) RETURN SUM(10/(b.p-4)) AS total LIMIT 0",
        ] {
            let mut options = options(&directory, text);
            options.spill.memory = Some(262_144);
            let mut output = Vec::new();
            let error = run(&view, &cx, &options, None, true, &mut output)
                .await
                .expect_err("all computed occurrences must be checked before delivery");
            assert_eq!(error.code, 3);
            assert!(output.is_empty());
            empty(&directory);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn external_grouping_enforces_full_input_and_final_result_allowances() {
    let ((), report) = run_async_under_lab(0x5b11a, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let cx = contexts.query();
        let directory = parent();
        let mut allowed = options(&directory, "MATCH (n) RETURN count(*) AS count");
        allowed.spill.memory = Some(262_144);
        allowed.spill.rows = Some(96);
        allowed.budget.rows = Some(1);
        let mut output = Vec::new();
        okay(run(&view, &cx, &allowed, None, true, &mut output).await);
        assert_eq!(
            rows(&output),
            [r#"{"v":1,"event":"row","cells":[{"type":"count","value":"96"}]}"#]
        );
        empty(&directory);
        for case in 0..5 {
            let mut options = options(&directory, "MATCH (n) RETURN n.p AS p, count(*) AS count");
            options.spill.memory = Some(262_144);
            match case {
                0 => options.spill.rows = Some(95),
                1 => options.budget.rows = Some(12),
                2 => options.spill.work = Some(0),
                3 => options.spill.disk = Some(3),
                _ => options.spill.memory = Some(1),
            }
            let mut output = Vec::new();
            let error = run(&view, &cx, &options, None, true, &mut output)
                .await
                .expect_err("aggregate refusal");
            assert_eq!(error.code, 3, "{}", error.message);
            assert!(
                output.is_empty(),
                "aggregate failure must precede the header"
            );
            empty(&directory);
        }
        for text in [
            "MATCH (n) RETURN count(DISTINCT n.p) AS count",
            "MATCH (n) RETURN collect(n.p) AS values",
            "MATCH (n) RETURN DISTINCT count(*) AS count GROUP BY n.p",
            "MATCH (n) RETURN count(*) + 1 AS count",
            "MATCH (n) RETURN n.p + 1 AS p, count(*) AS count",
        ] {
            let options = options(&directory, text);
            let mut output = Vec::new();
            assert!(
                run(&view, &cx, &options, None, true, &mut output)
                    .await
                    .is_err(),
                "{text}"
            );
            assert!(output.is_empty(), "{text}");
            empty(&directory);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn external_grouping_flush_failure_retires_every_partition_file() {
    let ((), report) = run_async_under_lab(0x5b11b, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let cx = contexts.query();
        let directory = parent();
        let mut options = options(&directory, "MATCH (n) RETURN n.p AS p, count(*) AS count");
        options.spill.memory = Some(262_144);
        for fail_at in [1, 2, 14, 15] {
            let mut output = FlushFailure {
                bytes: Vec::new(),
                flushes: 0,
                fail_at,
            };
            let error = run(&view, &cx, &options, None, true, &mut output)
                .await
                .expect_err("aggregate flush failure");
            assert_eq!(error.code, 5, "{}", error.message);
            assert_eq!(output.flushes, fail_at);
            if fail_at < 15 {
                assert!(
                    !std::str::from_utf8(&output.bytes)
                        .unwrap()
                        .contains(r#""event":"result""#)
                );
            }
            empty(&directory);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn external_order_matches_native_results_across_runs_distinct_windows_and_history() {
    let ((), report) = run_async_under_lab(0x5b111, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let cx = contexts.query();
        let directory = parent();
        for text in [
            "MATCH (n) RETURN n.p AS p, n.note AS note ORDER BY p DESC",
            "MATCH (n) RETURN DISTINCT n.p AS p ORDER BY p DESC SKIP 2 LIMIT 3",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n AS id, n.p AS p ORDER BY p, id DESC LIMIT 4",
            "MATCH path=(a)-[e:R]->(b) RETURN path AS path, e AS edge, a AS source ORDER BY edge DESC",
            "MATCH (a)-[e:R]-(b) RETURN DISTINCT b.p AS p ORDER BY p DESC NULLS FIRST SKIP 1 LIMIT 2",
            "MATCH (n) RETURN n.p AS p ORDER BY p LIMIT 0",
        ] {
            let options = options(&directory, text);
            let eager = view
                .query(
                    &cx,
                    text,
                    &options.params,
                    &options,
                    options.budget.policy(),
                )
                .unwrap();
            let mut expected = Vec::new();
            okay(crate::render(eager, 1, "rows", true, &mut expected));
            let mut output = Vec::new();
            okay(run(&view, &cx, &options, None, true, &mut output).await);
            assert_eq!(rows(&output), rows(&expected), "{text}");
            assert!(
                std::str::from_utf8(&output)
                    .unwrap()
                    .contains(r#""stream":true,"seq":1"#)
            );
            if text.contains("note") {
                assert!(
                    output.len() > 131_072,
                    "result exceeds the shared spill memory cap"
                );
                assert_eq!(rows(&output).len(), 96);
            }
            empty(&directory);
        }
        // Final-row admission must not accidentally become the input allowance.
        let mut options = options(
            &directory,
            "MATCH (n) RETURN n.p AS p ORDER BY p DESC LIMIT 1",
        );
        options.budget.rows = Some(1);
        options.spill.rows = Some(96);
        let mut output = Vec::new();
        okay(run(&view, &cx, &options, None, true, &mut output).await);
        assert_eq!(
            rows(&output),
            [r#"{"v":1,"event":"row","cells":[{"type":"int","value":"12"}]}"#]
        );
        empty(&directory);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn refusal_cleans_scratch_and_emits_no_accepted_result() {
    let ((), report) = run_async_under_lab(0x5b112, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let cx = contexts.query();
        let directory = parent();
        for case in 0..6 {
            let mut options = options(&directory, "MATCH (n) RETURN n.p AS p ORDER BY p");
            match case {
                0 => options.spill.rows = Some(95),
                1 => options.spill.work = Some(0),
                2 => options.spill.memory = Some(0),
                3 => options.spill.disk = Some(2),
                4 => options.budget.rows = Some(1),
                _ => options.text = "MATCH (n) RETURN collect(n) AS nodes".into(),
            }
            let mut out = Vec::new();
            let error = run(&view, &cx, &options, None, true, &mut out)
                .await
                .expect_err("refusal");
            assert_eq!(error.code, 3, "{}", error.message);
            assert!(
                out.is_empty(),
                "source/sort refusal must precede the header"
            );
            empty(&directory);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

struct FlushFailure {
    bytes: Vec<u8>,
    flushes: usize,
    fail_at: usize,
}
impl Write for FlushFailure {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.flushes += 1;
        if self.flushes == self.fail_at {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "injected flush"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn every_delivery_boundary_surfaces_flush_failure_and_retires_scratch() {
    let ((), report) = run_async_under_lab(0x5b113, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let cx = contexts.query();
        let directory = parent();
        let options = options(&directory, "MATCH (n) RETURN n.p AS p ORDER BY p LIMIT 2");
        for fail_at in 1..=4 {
            let mut out = FlushFailure {
                bytes: Vec::new(),
                flushes: 0,
                fail_at,
            };
            let error = run(&view, &cx, &options, None, true, &mut out)
                .await
                .expect_err("flush failure");
            assert_eq!(error.code, 5);
            assert_eq!(out.flushes, fail_at);
            if fail_at < 4 {
                assert!(
                    !std::str::from_utf8(&out.bytes)
                        .unwrap()
                        .contains(r#""event":"result""#)
                );
            }
            empty(&directory);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn scratch_is_private_and_only_owned_names_are_retired() {
    let ((), report) = run_async_under_lab(0x5b114, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let directory = parent();
        let sentinel = directory.join("foreign");
        std::fs::write(&sentinel, b"leave this alone").unwrap();
        {
            let mut owner = okay(ScratchOwner::new(&cx, &directory));
            let file = okay(owner.file("scratch"));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(&owner.directory)
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o700
                );
                assert_eq!(
                    std::fs::metadata(owner.directory.join("scratch"))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
            }
            drop(file);
            root.set_cancel_requested(true);
            // Cleanup must work after cancellation and on ordinary Drop.
        }
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"leave this alone");
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn spill_flags_are_explicit_unique_query_only_and_incompatible_with_other_delivery_modes() {
    for flags in [
        vec!["--spill-memory-bytes", "100"],
        vec!["--spill-disk-bytes", "100"],
        vec!["--max-spill-rows", "100"],
        vec!["--max-sort-work", "100"],
        vec!["--spill-dir", ""],
        vec!["--spill-dir", "a", "--spill-dir", "b"],
        vec![
            "--spill-dir",
            "a",
            "--spill-memory-bytes",
            "1",
            "--spill-memory-bytes",
            "2",
        ],
        vec!["--spill-dir", "a", "--spill-disk-bytes", "-1"],
        vec![
            "--spill-dir",
            "a",
            "--max-spill-rows",
            "18446744073709551616",
        ],
        vec!["--spill-dir", "a", "--max-sort-work", "1e6"],
        vec!["--spill-dir", "a", "--stream"],
        vec!["--spill-dir", "a", "--certify-to", "certificate"],
    ] {
        let mut args = vec!["--db", "unused", "--key-file", "unused"];
        args.extend(flags);
        args.push("MATCH (n) RETURN n AS id ORDER BY id");
        let args: Vec<_> = args.into_iter().map(str::to_owned).collect();
        let error = match crate::parse(&args, "query") {
            Ok(_) => panic!("accepted incompatible or invalid spill flags: {args:?}"),
            Err(error) => error,
        };
        assert_eq!(error.code, 2);
    }
    for command in ["write", "diff", "transaction", "search"] {
        let args = [
            "--db",
            "unused",
            "--key-file",
            "unused",
            "--spill-dir",
            "a",
            "CREATE ()",
        ]
        .map(str::to_owned);
        assert!(crate::parse(&args, command).is_err(), "{command}");
    }
    let args = [
        "--db",
        "unused",
        "--key-file",
        "unused",
        "--spill-dir",
        "a",
        "CALL fnx.pagerank() YIELD node",
    ]
    .map(str::to_owned);
    assert!(crate::parse(&args, "query").is_err());
}

struct CancelOutput<F> {
    bytes: Vec<u8>,
    flushes: usize,
    cancel: F,
}

impl<F: FnMut()> Write for CancelOutput<F> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.flushes += 1;
        if self.flushes == 2 {
            (self.cancel)();
        }
        Ok(())
    }
}

#[test]
fn cancellation_after_one_flushed_row_retires_scratch_without_a_success_terminal() {
    let ((), report) = run_async_under_lab(0x5b115, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let cx = contexts.query();
        let directory = parent();
        let options = options(&directory, "MATCH (n) RETURN n.p AS p ORDER BY p");
        let mut out = CancelOutput {
            bytes: Vec::new(),
            flushes: 0,
            cancel: || root.set_cancel_requested(true),
        };
        let error = run(&view, &cx, &options, None, true, &mut out)
            .await
            .expect_err("cancelled delivery");
        assert_eq!(error.code, 3);
        assert_eq!(out.flushes, 2);
        assert_eq!(rows(&out.bytes).len(), 1);
        assert!(
            !std::str::from_utf8(&out.bytes)
                .unwrap()
                .contains(r#""event":"result""#)
        );
        empty(&directory);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn aggregate_cancellation_retires_all_three_files_after_one_flushed_group() {
    let ((), report) = run_async_under_lab(0x5b11c, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let cx = contexts.query();
        let directory = parent();
        let mut options = options(&directory, "MATCH (n) RETURN n.p AS p, count(*) AS count");
        options.spill.memory = Some(262_144);
        let mut out = CancelOutput {
            bytes: Vec::new(),
            flushes: 0,
            cancel: || root.set_cancel_requested(true),
        };
        let error = run(&view, &cx, &options, None, true, &mut out)
            .await
            .expect_err("cancelled aggregate delivery");
        assert_eq!(error.code, 3);
        assert_eq!(out.flushes, 2);
        assert_eq!(rows(&out.bytes).len(), 1);
        assert!(
            !std::str::from_utf8(&out.bytes)
                .unwrap()
                .contains(r#""event":"result""#)
        );
        empty(&directory);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ranked_aggregate_cancellation_retires_both_sort_passes_before_any_success_terminal() {
    let ((), report) = run_async_under_lab(0x5b11e, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let cx = contexts.query();
        let directory = parent();
        let mut options = options(
            &directory,
            "MATCH (n) RETURN count(*) AS count GROUP BY n.p HAVING count > 0 ORDER BY sum(n.p) DESC LIMIT 4",
        );
        options.spill.memory = Some(262_144);
        let mut out = CancelOutput {
            bytes: Vec::new(),
            flushes: 0,
            cancel: || root.set_cancel_requested(true),
        };
        let error = run(&view, &cx, &options, None, true, &mut out)
            .await
            .expect_err("cancelled ranked delivery");
        assert_eq!(error.code, 3);
        assert_eq!(out.flushes, 2);
        assert_eq!(rows(&out.bytes).len(), 1);
        assert!(
            !std::str::from_utf8(&out.bytes)
                .unwrap()
                .contains(r#""event":"result""#)
        );
        empty(&directory);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn hidden_sort_keys_rank_spilled_rows_without_entering_the_public_schema() {
    let ((), report) = run_async_under_lab(0x5b116, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = fixture(&contexts.commit()).await;
        let mut update = WriteBatch::new(RelationId(1));
        update.set_vertex_property(VId(0), PropertyKeyId(1), None);
        update.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(1000)));
        db.write(&contexts.commit(), update).await.unwrap();
        let view = db.read_session().unwrap();
        let cx = contexts.query();
        let directory = parent();
        for (text, column, seq) in [
            (
                "MATCH (n) RETURN n AS id ORDER BY n.p DESC NULLS LAST, id SKIP 7 LIMIT 13",
                "id",
                2,
            ),
            (
                "MATCH (n) RETURN n AS id ORDER BY n.p ASC NULLS FIRST, n.note DESC, id LIMIT 3",
                "id",
                2,
            ),
            (
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n AS id ORDER BY n.p DESC, id SKIP 2 LIMIT 4",
                "id",
                1,
            ),
            (
                "MATCH (a)-[e:R]->(b) RETURN e AS edge ORDER BY b.p DESC NULLS FIRST, a.p ASC, edge",
                "edge",
                2,
            ),
            (
                "MATCH path=(a)-[e:R]->(b) RETURN e AS edge ORDER BY LENGTH(path) DESC, b.p DESC, edge",
                "edge",
                2,
            ),
            (
                "MATCH (a)-[e:R]-(b) RETURN b AS target ORDER BY a.p DESC NULLS LAST, b.p ASC NULLS FIRST SKIP 1 LIMIT 5",
                "target",
                2,
            ),
            (
                "MATCH (n) RETURN n AS id ORDER BY n.note DESC LIMIT 0",
                "id",
                2,
            ),
            (
                "MATCH (n) RETURN n AS id ORDER BY n.p DESC NULLS LAST LIMIT 1",
                "id",
                2,
            ),
        ] {
            let mut options = options(&directory, text);
            if text.ends_with("LIMIT 1") {
                // Hidden keys belong to the private input allowance. They do
                // not spend the single-row allowance of the selected page.
                options.budget.rows = Some(1);
                options.spill.rows = Some(96);
            }
            let eager = view
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
            okay(run(&view, &cx, &options, None, true, &mut output).await);
            assert_eq!(rows(&output), rows(&expected), "{text}");
            let rendered = std::str::from_utf8(&output).unwrap();
            assert_eq!(
                rendered.lines().next().unwrap(),
                format!(
                    r#"{{"v":1,"event":"columns","stream":true,"seq":{seq},"columns":[{}]}}"#,
                    quoted(column)
                ),
                "{text}"
            );
            assert!(
                !rendered.contains("padding"),
                "a hidden text property escaped into the robot output"
            );
            empty(&directory);

            let mut human = Vec::new();
            okay(run(&view, &cx, &options, None, false, &mut human).await);
            let human = std::str::from_utf8(&human).unwrap();
            let lines: Vec<_> = human.lines().collect();
            assert_eq!(lines[0], column, "{text}");
            assert_eq!(lines.len(), rows(&output).len() + 2, "{text}");
            assert!(
                lines[1..lines.len() - 1]
                    .iter()
                    .all(|line| !line.contains('\t')),
                "human rows must contain only the one visible column"
            );
            assert!(!human.contains("padding"));
            empty(&directory);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn hidden_distinct_sort_keys_refuse_before_any_delivery_or_scratch_even_at_limit_zero() {
    let ((), report) = run_async_under_lab(0x5b117, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let cx = contexts.query();
        let directory = parent();
        for text in [
            "MATCH (n) RETURN DISTINCT n.note AS note ORDER BY n.p",
            "MATCH (n) RETURN DISTINCT n.note AS note ORDER BY n.p LIMIT 0",
            "MATCH (a)-[e:R]->(b) RETURN DISTINCT a AS source ORDER BY b.p LIMIT 0",
        ] {
            let options = options(&directory, text);
            let mut output = Vec::new();
            let error = run(&view, &cx, &options, None, true, &mut output)
                .await
                .expect_err("DISTINCT cannot deduplicate hidden evaluation cells");
            assert_eq!(error.code, 3, "{}", error.message);
            assert!(output.is_empty(), "{text}");
            empty(&directory);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
