use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb::{DatabaseKeys, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::result_diff::GraphDiffInput;
use fgdb_gql::{GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryExecution};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};

fn okay<T>(result: Result<T, Failure>) -> T {
    result.unwrap_or_else(|error| panic!("{}: {}", error.class, error.message))
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x91; 32],
        DatabaseSecurityNamespaceId([0x92; 32]),
        [0x93; 32],
    )
}
fn args(text: &str, before: u64, after: u64) -> Vec<String> {
    vec![
        "--db".into(),
        "unused".into(),
        "--key-file".into(),
        "unused".into(),
        "--label".into(),
        "L=1".into(),
        "--property".into(),
        "score=1".into(),
        "--before".into(),
        before.to_string(),
        "--after".into(),
        after.to_string(),
        text.into(),
    ]
}
fn options(text: &str, before: u64, after: u64) -> Options {
    okay(crate::parse(&args(text, before, after), "diff"))
}
fn changes(output: &str) -> Vec<&str> {
    output
        .lines()
        .filter(|line| line.contains(r#""event":"change""#))
        .collect()
}
fn add(batch: &mut WriteBatch, id: u128, score: i64) {
    batch.create_vertex(
        VId(id),
        vec![LabelId(1)],
        vec![(PropertyKeyId(1), CanonicalScalar::Int(score))],
    );
}

#[test]
fn diff_flags_are_exact_required_unique_and_do_not_change_other_commands() {
    for (before, after) in [(0, 0), (u64::MAX, 0), (1, u64::MAX)] {
        let parsed = options("MATCH (n) RETURN n", before, after);
        assert_eq!(
            okay(parsed.diff.endpoints()),
            (CommitSeq(before), CommitSeq(after))
        );
        assert_eq!(parsed.diff.policy(), policy());
        assert_eq!(
            parsed.diff.output_bytes.unwrap_or(DEFAULT_OUTPUT_BYTES),
            16 * 1024 * 1024
        );
    }
    for flag in [
        "--before",
        "--after",
        "--max-snapshot-records",
        "--max-result-rows",
        "--max-work-units",
        "--max-scratch-entries",
        "--max-output-bytes",
    ] {
        for bad in [
            "",
            "-1",
            "+1",
            " 1",
            "1 ",
            "1.0",
            "1e3",
            "18446744073709551616",
            "secret",
        ] {
            let mut opts = DiffOptions::default();
            let error = opts.set(flag, bad).expect_err("invalid decimal");
            assert_eq!(error.code, 2);
            assert!(!error.message.contains("secret"));
        }
        let mut opts = DiffOptions::default();
        okay(opts.set(flag, "0"));
        assert!(opts.set(flag, "0").is_err());
    }
    for limits in [0, u64::MAX] {
        let mut argv = args("MATCH (n) RETURN n", 0, 0);
        for flag in [
            "--max-snapshot-records",
            "--max-result-rows",
            "--max-work-units",
            "--max-scratch-entries",
        ] {
            argv.extend([flag.into(), limits.to_string()]);
        }
        let parsed = okay(crate::parse(&argv, "diff"));
        assert_eq!(
            parsed.diff.policy(),
            GqlQueryPolicy::new(limits, limits, limits, limits)
        );
        argv.extend(["--max-output-bytes".into(), limits.to_string()]);
        let parsed = okay(crate::parse(&argv, "diff"));
        assert_eq!(parsed.diff.output_bytes, Some(limits));
    }
    for missing in ["--before", "--after"] {
        let mut argv = args("MATCH (n) RETURN n", 0, 0);
        let at = argv.iter().position(|v| v == missing).unwrap();
        argv.drain(at..at + 2);
        assert_eq!(crate::parse(&argv, "diff").err().unwrap().code, 2);
    }
    for flag in ["--before", "--after"] {
        let mut argv = args("MATCH (n) RETURN n", 0, 0);
        argv.extend([flag.into(), "0".into()]);
        assert_eq!(crate::parse(&argv, "diff").err().unwrap().code, 2);
    }
    for forbidden in [
        vec!["--stream"],
        vec!["--certify-to", "x"],
        vec!["--certificate", "x"],
        vec!["--write", "CREATE (n)"],
        vec!["--rollback"],
        vec!["--write-relation", "1"],
    ] {
        let mut argv = args("MATCH (n) RETURN n", 0, 0);
        argv.extend(forbidden.into_iter().map(str::to_owned));
        assert_eq!(crate::parse(&argv, "diff").err().unwrap().code, 2);
    }
    for command in ["create", "query", "write", "replay", "load", "transaction"] {
        assert_eq!(
            crate::parse(&args("MATCH (n) RETURN n", 0, 0), command)
                .err()
                .unwrap()
                .code,
            2
        );
    }
    let query = "MATCH (n) WHERE n.score=$before RETURN n AS after";
    let mut argv = args(query, 1, 2);
    argv.extend(["--param".into(), "before=int:9".into()]);
    let parsed = okay(crate::parse(&argv, "diff"));
    assert_eq!(parsed.text, query);
    assert_eq!(parsed.raw_params, vec![("before".into(), "int:9".into())]);
}

#[test]
fn robot_and_human_diffs_preserve_bag_multiplicities_and_distinct_zero_net_changes() {
    let ((), report) = run_async_under_lab(0xd1ff_0101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        add(&mut seed, 1, 4);
        add(&mut seed, 2, 4);
        add(&mut seed, 3, 9);
        let before = db.write(&commit, seed).await.unwrap();
        let mut edit = WriteBatch::new(RelationId(1));
        edit.delete_vertex(VId(2));
        edit.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(7)));
        let after = db.write(&commit, edit).await.unwrap();
        for distinct in [false, true] {
            let query = if distinct {
                "MATCH (n:L) RETURN DISTINCT n.score AS score"
            } else {
                "MATCH (n:L) RETURN ALL n.score AS score"
            };
            let opts = options(query, before.0, after.0);
            let mut bytes = Vec::new();
            okay(run(&db, &cx, &opts, true, &mut bytes));
            let output = String::from_utf8(bytes).unwrap();
            let count = if distinct { 1 } else { 2 };
            let expected = [
                format!(
                    r#"{{"v":1,"event":"change","weight":"-{count}","cells":[{{"type":"int","value":"4"}}]}}"#
                ),
                r#"{"v":1,"event":"change","weight":"1","cells":[{"type":"int","value":"7"}]}"#
                    .into(),
            ];
            assert_eq!(
                changes(&output),
                expected.iter().map(String::as_str).collect::<Vec<_>>()
            );
            assert!(output.starts_with(r#"{"v":1,"event":"diff_columns","before":"1","after":"2","semantics":"after_minus_before_bag","columns":["score"]}"#));
            assert!(output.lines().last().unwrap().contains(&format!(
                r#""changed_rows":"2","inserted":"1","retracted":"{count}""#
            )));
            assert_eq!(output.lines().count(), 4);
            let mut human = Vec::new();
            okay(run(&db, &cx, &opts, false, &mut human));
            let human = String::from_utf8(human).unwrap();
            assert!(human.contains(&format!("-{count}\t4\n+1\t7\n")));
            assert!(human.contains(&format!("2 changed tuple(s): +1 / -{count} occurrence(s)")));
        }
        assert_eq!(db.frontier().unwrap(), after);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unchanged_results_keep_schema_and_large_revision_metadata_is_decimal_text() {
    // Encoding-only source seam: it does not claim these cuts exist in a database.
    let before = CommitSeq(9_007_199_254_740_993);
    let after = CommitSeq(u64::MAX);
    let diff = GraphResultDiff::execute(
        before,
        after,
        vec!["name\n\"é".into()],
        policy(),
        |_, _| {
            Ok::<_, GqlQueryError<std::convert::Infallible, ()>>(GraphDiffInput::values(
                GqlQueryExecution {
                    value: vec![],
                    rows: GqlExecutionStats {
                        snapshot_records: 0,
                        result_rows: 0,
                    },
                    evaluator: GlaExecutionStats::default(),
                },
            ))
        },
        || Ok::<_, ()>(()),
    )
    .unwrap();
    let mut bytes = Vec::new();
    okay(render(
        &diff,
        true,
        DEFAULT_OUTPUT_BYTES,
        &mut bytes,
        &mut || Ok(()),
    ));
    let output = String::from_utf8(bytes).unwrap();
    assert_eq!(output.lines().count(), 2);
    assert!(changes(&output).is_empty());
    assert!(output.contains(r#""before":"9007199254740993","after":"18446744073709551615""#));
    assert!(output.contains(r#""columns":["name\n\"é"]"#));
    assert!(output.contains(r#""changed_rows":"0","inserted":"0","retracted":"0""#));
    for field in [
        "diff_columns",
        "change",
        "changed_rows",
        "inserted",
        "retracted",
        "after_minus_before_bag",
    ] {
        assert!(crate::ROBOT_SCHEMA.contains(field));
    }
}

#[test]
fn revision_query_and_shared_budget_refusals_emit_no_diff_output_or_writes() {
    let ((), report) = run_async_under_lab(0xd1ff_0102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        add(&mut seed, 1, 7);
        let at = db.write(&commit, seed).await.unwrap();
        let query = "MATCH (n:L) RETURN n.score AS score";
        for opts in [
            options(query, 0, at.0 + 1),
            options(query, at.0 + 1, 0),
            options("MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ 0 RETURN n", 0, at.0),
            options("CREATE (n:L)", 0, at.0),
            options("MATCH (n:L) WHERE n.score > $missing RETURN n", at.0, at.0),
        ] {
            let mut bytes = Vec::new();
            assert_eq!(
                run(&db, &cx, &opts, true, &mut bytes).err().unwrap().code,
                3
            );
            assert!(bytes.is_empty());
        }
        let opts = options(query, 0, at.0);
        let baseline = db
            .query_diff(&cx, query, &opts.params, &opts, CommitSeq(0), at, policy())
            .unwrap();
        let rows = baseline.row_stats();
        let eval = baseline.evaluator_stats();
        for (flag, limit) in [
            ("--max-snapshot-records", rows.snapshot_records - 1),
            ("--max-result-rows", 0),
            ("--max-work-units", eval.work_units - 1),
            ("--max-scratch-entries", eval.scratch_entries - 1),
        ] {
            let mut opts = options(query, 0, at.0);
            okay(opts.diff.set(flag, &limit.to_string()));
            let mut bytes = Vec::new();
            assert_eq!(
                run(&db, &cx, &opts, true, &mut bytes).err().unwrap().code,
                3
            );
            assert!(bytes.is_empty());
        }
        let mut exact = options(query, 0, at.0);
        for (flag, value) in [
            ("--max-snapshot-records", rows.snapshot_records),
            ("--max-result-rows", rows.result_rows),
            ("--max-work-units", eval.work_units),
            ("--max-scratch-entries", eval.scratch_entries),
        ] {
            okay(exact.diff.set(flag, &value.to_string()));
        }
        okay(run(&db, &cx, &exact, true, &mut Vec::new()));
        assert_eq!(db.frontier().unwrap(), at);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    flushes: usize,
    fail_flush: Option<usize>,
    fail_after_bytes: Option<usize>,
}
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let count = if let Some(limit) = self.fail_after_bytes {
            if self.bytes.len() >= limit {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "receiver closed",
                ));
            }
            bytes.len().min(limit - self.bytes.len())
        } else {
            bytes.len()
        };
        self.bytes.extend_from_slice(&bytes[..count]);
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.flushes += 1;
        if self.fail_flush == Some(self.flushes) {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "receiver closed",
            ))
        } else {
            Ok(())
        }
    }
}

#[test]
fn output_bytes_are_utf8_cumulative_and_admit_whole_frames_before_any_write() {
    assert!(signed(&ZWeight::ZERO).is_err());
    assert_eq!(okay(signed(&ZWeight::from_i128(i128::MIN))), i128::MIN);
    let promoted = ZWeight::from_i128(i128::MAX)
        .checked_mul_i128(2, fgdb_delta_types::LimbLimit::new(4))
        .unwrap();
    assert!(signed(&promoted).is_err()); // Never silently narrow future carriers.
    let mut output = Output::default();
    let mut used = 2;
    let error = frame(&mut output, "é", &mut used, 4).err().unwrap();
    assert_eq!(error.code, 3);
    assert_eq!(used, 2);
    assert!(output.bytes.is_empty());
    assert_eq!(output.flushes, 0);
    okay(frame(&mut output, "é", &mut used, 5));
    assert_eq!(output.bytes, "é\n".as_bytes());
    assert_eq!(used, 5);
    assert_eq!(output.flushes, 1);
    assert!(frame(&mut output, "", &mut used, 5).is_err());
    okay(frame(&mut output, "", &mut used, 6));
    assert_eq!(used, 6);
    let before = output.bytes.clone();
    used = u64::MAX;
    assert!(frame(&mut output, "", &mut used, u64::MAX).is_err());
    assert_eq!(output.bytes, before);
    assert_eq!(used, u64::MAX);
    let mut output = Output {
        fail_flush: Some(1),
        ..Output::default()
    };
    used = 0;
    assert_eq!(
        frame(&mut output, "abc", &mut used, 4).err().unwrap().code,
        5
    );
    assert_eq!(used, 0); // A write is not a completed flush.
    assert_eq!(output.bytes, b"abc\n");
}

#[test]
fn every_delivery_checkpoint_and_broken_output_is_terminal_with_an_honest_prefix() {
    let ((), report) = run_async_under_lab(0xd1ff_0201, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, value) in [(1, 2), (2, 5), (3, 8)] {
            add(&mut seed, id, value);
        }
        db.write(&commit, seed).await.unwrap();
        let opts = options("MATCH (n:L) RETURN ALL n.score AS score", 0, 1);
        let diff = db
            .query_diff(
                &cx,
                &opts.text,
                &opts.params,
                &opts,
                CommitSeq(0),
                CommitSeq(1),
                policy(),
            )
            .unwrap();
        let mut expected = Output::default();
        let mut calls = 0;
        okay(render(&diff, true, u64::MAX, &mut expected, &mut || {
            calls += 1;
            Ok(())
        }));
        assert_eq!(expected.flushes, 5); // Header, three changes, completion.
        let full = String::from_utf8(expected.bytes.clone()).unwrap();
        let wanted: Vec<_> = changes(&full).into_iter().map(str::to_owned).collect();
        let unchanged = format!("{diff:?}");
        for stop in 1..=calls {
            let mut visited = 0;
            let mut out = Output::default();
            let error = render(&diff, true, u64::MAX, &mut out, &mut || {
                visited += 1;
                if visited == stop {
                    Err(Failure::query("cancelled delivery"))
                } else {
                    Ok(())
                }
            })
            .expect_err("injected cancellation");
            assert_eq!(visited, stop);
            assert_eq!(error.code, 3);
            assert!(expected.bytes.starts_with(&out.bytes));
            let text = String::from_utf8(out.bytes).unwrap();
            assert!(!text.contains(r#""event":"result""#));
            let delivered = changes(&text);
            assert_eq!(
                delivered,
                wanted[..delivered.len()]
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
            );
            assert_eq!(format!("{diff:?}"), unchanged);
            let mut retry = Vec::new();
            okay(render(&diff, true, u64::MAX, &mut retry, &mut || Ok(())));
            assert_eq!(retry, expected.bytes);
        }
        let frames: Vec<_> = full.split_inclusive('\n').collect();
        for fail in 1..=expected.flushes {
            let mut out = Output {
                fail_flush: Some(fail),
                ..Output::default()
            };
            let error = render(&diff, true, u64::MAX, &mut out, &mut || Ok(()))
                .err()
                .unwrap();
            assert_eq!(error.code, 5);
            assert_eq!(out.flushes, fail);
            assert_eq!(out.bytes, frames[..fail].concat().as_bytes());
            let sent = fail.saturating_sub(2).min(3);
            assert!(error.message.starts_with(&format!(
                "diff incomplete after {sent} fully flushed change(s)"
            )));
            if fail < expected.flushes {
                assert!(
                    !String::from_utf8(out.bytes)
                        .unwrap()
                        .contains(r#""event":"result""#)
                );
            }
            // At final-flush failure result bytes may exist, but return is Err:
            // consumers must require successful exit, not just a result frame.
        }
        for stop in 0..expected.bytes.len() {
            let mut out = Output {
                fail_after_bytes: Some(stop),
                ..Output::default()
            };
            let error = render(&diff, true, u64::MAX, &mut out, &mut || Ok(()))
                .err()
                .unwrap();
            assert_eq!(error.code, 5);
            assert_eq!(out.bytes, expected.bytes[..stop]);
        }
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cli_delivery_limit_includes_summary_without_masking_endpoint_errors() {
    let ((), report) = run_async_under_lab(0xd1ff_0202, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(
            VId(1),
            vec![LabelId(1)],
            vec![(
                PropertyKeyId(1),
                CanonicalScalar::ucs_basic_text("é\n\"\\\0").unwrap(),
            )],
        );
        db.write(&commit, seed).await.unwrap();
        for robot in [false, true] {
            let query = "MATCH (n:L) RETURN n.score AS score";
            let mut opts = options(query, 0, 1);
            let mut full = Vec::new();
            okay(run(&db, &cx, &opts, robot, &mut full));
            let size = full.len() as u64;
            opts.diff.output_bytes = Some(size);
            let mut exact = Vec::new();
            okay(run(&db, &cx, &opts, robot, &mut exact));
            assert_eq!(exact, full);
            for limit in [0, 1, size - 1] {
                opts.diff.output_bytes = Some(limit);
                let mut out = Output::default();
                let error = run(&db, &cx, &opts, robot, &mut out).err().unwrap();
                assert_eq!(error.code, 3);
                assert!(error.message.contains("encoded output exceeds"));
                assert!(out.bytes.len() as u64 <= limit);
                assert!(full.starts_with(&out.bytes));
                if limit < 2 {
                    assert!(out.bytes.is_empty());
                }
                let text = String::from_utf8(out.bytes).unwrap();
                assert!(!text.contains(r#""event":"result""#));
                assert!(!text.contains("diff complete"));
            }
        }
        let mut opts = options("MATCH (n:L) RETURN SUM(n.score) AS sum", 1, 1);
        opts.diff.output_bytes = Some(0);
        let mut bytes = Vec::new();
        let error = run(&db, &cx, &opts, true, &mut bytes).err().unwrap();
        assert_eq!(error.code, 3);
        assert!(!error.message.contains("encoded output")); // Real data error wins, even equal cuts.
        assert!(bytes.is_empty());
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_diff_delivery_preserves_complete_queries_and_exact_domains_after_reopen() {
    use fgdb::{MemVfs, QueryResult};
    use fgdb_types::EId;
    use std::collections::BTreeMap;
    type Bag = BTreeMap<Vec<QueryValue>, i128>;
    fn bag(result: QueryResult) -> Bag {
        let QueryResult::Rows { rows, .. } = result else {
            panic!("a native read returned writes")
        };
        let mut bag = Bag::new();
        for row in rows {
            *bag.entry(row).or_default() += 1;
        }
        bag
    }
    let ((), report) = run_async_under_lab(0xd1ff_0203, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, score, bucket) in [
            (0, Some(7), Some(1)),
            (1, Some(7), Some(1)),
            (2, None, Some(2)),
            (u128::MAX, Some(-3), None),
        ] {
            let mut props = Vec::new();
            if let Some(value) = score {
                props.push((PropertyKeyId(1), CanonicalScalar::Int(value)));
            }
            if let Some(value) = bucket {
                props.push((PropertyKeyId(2), CanonicalScalar::Int(value)));
            }
            seed.create_vertex(VId(id), vec![LabelId(1)], props);
        }
        for id in [0, u128::MAX] {
            seed.add_edge(
                EId(id),
                VId(0),
                VId(1),
                vec![(PropertyKeyId(1), CanonicalScalar::Int(5))],
            );
        }
        let basis = db.write(&commit, seed).await.unwrap();
        let old = db.read_session().unwrap();
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(9)));
        edit.set_vertex_property(
            VId(u128::MAX),
            PropertyKeyId(2),
            Some(CanonicalScalar::Int(2)),
        );
        edit.delete_edge(EId(0));
        edit.add_edge(
            EId(3),
            VId(1),
            VId(2),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(-4))],
        );
        let at = db.write(&commit, edit).await.unwrap();
        let new = db.read_session().unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        for text in [
            "MATCH (n:L) RETURN n AS id, n.score AS score",
            "MATCH (n:L) RETURN ALL n.score AS score",
            "MATCH (n:L) RETURN DISTINCT n.score AS score",
            "MATCH (n:L) RETURN n.score AS score ORDER BY score DESC NULLS LAST LIMIT 1",
            "MATCH (n:L) RETURN COUNT(*) AS n, SUM(n.score) AS sum, AVG(n.score) AS avg",
            "MATCH (n:L) RETURN n.bucket AS first, n.bucket AS again, SUM(n.score) AS total GROUP BY n.bucket HAVING total > 0 ORDER BY total DESC LIMIT 1",
            "MATCH (n:L) WITH n.score AS score RETURN SUM(score) AS sum, AVG(score) AS avg",
            "MATCH (n:L) RETURN n.score AS score UNION ALL MATCH (m:L) RETURN m.score AS score",
            "MATCH (a)-[r:R]->(b) RETURN r AS edge, a AS src, b AS dst, r.score AS score",
            "MATCH p=(a)-[:R]->(b) RETURN p AS path",
            "MATCH (n:L) RETURN COLLECT(n.score) AS scores",
            "MATCH (n:L) WHERE n.score >= $floor RETURN n.score AS score",
        ] {
            for reverse in [false, true] {
                let (before, after) = if reverse { (at, basis) } else { (basis, at) };
                let mut argv = args(text, before.0, after.0);
                argv.extend([
                    "--relation".into(),
                    "R=1".into(),
                    "--property".into(),
                    "bucket=2".into(),
                ]);
                if text.contains("$floor") {
                    argv.extend(["--param".into(), "floor=int:8".into()]);
                }
                let mut opts = okay(crate::parse(&argv, "diff"));
                // Same deferred parameter decoder used by dispatch; endpoint
                // flags never become statement parameters or vice versa.
                for (name, raw) in &opts.raw_params {
                    opts.params
                        .insert(name, okay(crate::parameter(raw, None)))
                        .unwrap();
                }
                let (left, right) = if reverse { (&new, &old) } else { (&old, &new) };
                let before_rows = bag(left
                    .query(&cx, text, &opts.params, &opts, policy())
                    .unwrap());
                let mut expected = bag(right
                    .query(&cx, text, &opts.params, &opts, policy())
                    .unwrap());
                for (row, count) in before_rows {
                    *expected.entry(row).or_default() -= count;
                }
                expected.retain(|_, count| *count != 0);
                let expected_lines: Vec<_> = expected
                    .iter()
                    .map(|(row, count)| {
                        let cells: Vec<_> =
                            row.iter().map(|value| okay(crate::cell(value))).collect();
                        format!(
                            r#"{{"v":1,"event":"change","weight":"{count}","cells":[{}]}}"#,
                            cells.join(",")
                        )
                    })
                    .collect();
                let mut bytes = Vec::new();
                okay(run(&db, &cx, &opts, true, &mut bytes));
                let result = String::from_utf8(bytes).unwrap();
                assert_eq!(
                    changes(&result),
                    expected_lines
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>(),
                    "{text}"
                );
                assert!(
                    result.contains(&format!(r#""before":"{}","after":"{}""#, before.0, after.0))
                );
                assert!(result.lines().last().unwrap().contains(r#""kind":"diff""#));
            }
        }
        let opts = options("MATCH (n:L) RETURN AVG(n.score) AS average", basis.0, at.0);
        let mut bytes = Vec::new();
        okay(run(&db, &cx, &opts, true, &mut bytes));
        let result = String::from_utf8(bytes).unwrap();
        assert_eq!(
            changes(&result),
            vec![
                r#"{"v":1,"event":"change","weight":"-1","cells":[{"type":"average","value":"11/3"}]}"#,
                r#"{"v":1,"event":"change","weight":"1","cells":[{"type":"average","value":"13/3"}]}"#,
            ]
        );
        assert_eq!(db.frontier().unwrap(), at); // Comparisons never publish a commit.
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
