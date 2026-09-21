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
    DatabaseKeys::new([0x91; 32], DatabaseSecurityNamespaceId([0x92; 32]), [0x93; 32])
}
fn args(text: &str, before: u64, after: u64) -> Vec<String> {
    vec!["--db".into(), "unused".into(), "--key-file".into(), "unused".into(),
        "--label".into(), "L=1".into(), "--property".into(), "score=1".into(),
        "--before".into(), before.to_string(), "--after".into(), after.to_string(), text.into()]
}
fn options(text: &str, before: u64, after: u64) -> Options {
    okay(crate::parse(&args(text, before, after), "diff"))
}
fn changes(output: &str) -> Vec<&str> {
    output.lines().filter(|line| line.contains(r#""event":"change""#)).collect()
}
fn add(batch: &mut WriteBatch, id: u128, score: i64) {
    batch.create_vertex(VId(id), vec![LabelId(1)],
        vec![(PropertyKeyId(1), CanonicalScalar::Int(score))]);
}

#[test]
fn diff_flags_are_exact_required_unique_and_do_not_change_other_commands() {
    for (before, after) in [(0, 0), (u64::MAX, 0), (1, u64::MAX)] {
        let parsed = options("MATCH (n) RETURN n", before, after);
        assert_eq!(okay(parsed.diff.endpoints()), (CommitSeq(before), CommitSeq(after)));
        assert_eq!(parsed.diff.policy(), policy());
    }
    for flag in ["--before", "--after", "--max-snapshot-records", "--max-result-rows",
        "--max-work-units", "--max-scratch-entries"] {
        for bad in ["", "-1", "+1", " 1", "1 ", "1.0", "1e3", "18446744073709551616", "secret"] {
            let mut opts = DiffOptions::default();
            let error = opts.set(flag, bad).err().expect("invalid decimal");
            assert_eq!(error.code, 2);
            assert!(!error.message.contains("secret"));
        }
        let mut opts = DiffOptions::default();
        okay(opts.set(flag, "0"));
        assert!(opts.set(flag, "0").is_err());
    }
    for limits in [0, u64::MAX] {
        let mut argv = args("MATCH (n) RETURN n", 0, 0);
        for flag in ["--max-snapshot-records", "--max-result-rows", "--max-work-units", "--max-scratch-entries"] {
            argv.extend([flag.into(), limits.to_string()]);
        }
        let parsed = okay(crate::parse(&argv, "diff"));
        assert_eq!(parsed.diff.policy(), GqlQueryPolicy::new(limits, limits, limits, limits));
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
    for forbidden in [vec!["--stream"], vec!["--certify-to", "x"], vec!["--certificate", "x"],
        vec!["--write", "CREATE (n)"], vec!["--rollback"], vec!["--write-relation", "1"]] {
        let mut argv = args("MATCH (n) RETURN n", 0, 0);
        argv.extend(forbidden.into_iter().map(str::to_owned));
        assert_eq!(crate::parse(&argv, "diff").err().unwrap().code, 2);
    }
    for command in ["create", "query", "write", "replay", "load", "transaction"] {
        assert_eq!(crate::parse(&args("MATCH (n) RETURN n", 0, 0), command).err().unwrap().code, 2);
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
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        add(&mut seed, 1, 4); add(&mut seed, 2, 4); add(&mut seed, 3, 9);
        let before = db.write(&commit, seed).await.unwrap();
        let mut edit = WriteBatch::new(RelationId(1));
        edit.delete_vertex(VId(2));
        edit.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(7)));
        let after = db.write(&commit, edit).await.unwrap();
        for distinct in [false, true] {
            let query = if distinct { "MATCH (n:L) RETURN DISTINCT n.score AS score" }
                else { "MATCH (n:L) RETURN n.score AS score" };
            let opts = options(query, before.0, after.0);
            let mut bytes = Vec::new();
            okay(run(&db, &cx, &opts, true, &mut bytes));
            let output = String::from_utf8(bytes).unwrap();
            let count = if distinct { 1 } else { 2 };
            let expected = [
                format!(r#"{{"v":1,"event":"change","weight":"-{count}","cells":[{{"type":"int","value":"4"}}]}}"#),
                r#"{"v":1,"event":"change","weight":"1","cells":[{"type":"int","value":"7"}]}"#.into(),
            ];
            assert_eq!(changes(&output), expected.iter().map(String::as_str).collect::<Vec<_>>());
            assert!(output.starts_with(r#"{"v":1,"event":"diff_columns","before":"1","after":"2","semantics":"after_minus_before_bag","columns":["score"]}"#));
            assert!(output.lines().last().unwrap().contains(&format!(r#""changed_rows":"2","inserted":"1","retracted":"{count}""#)));
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
    let diff = GraphResultDiff::execute(before, after, vec!["name\n\"é".into()], policy(), |_, _| {
        Ok::<_, GqlQueryError<std::convert::Infallible, ()>>(GraphDiffInput::values(GqlQueryExecution {
            value: vec![], rows: GqlExecutionStats { snapshot_records: 0, result_rows: 0 },
            evaluator: GlaExecutionStats::default(),
        }))
    }, || Ok::<_, ()>(())).unwrap();
    let mut bytes = Vec::new();
    okay(render(&diff, true, &mut bytes, &mut || Ok(())));
    let output = String::from_utf8(bytes).unwrap();
    assert_eq!(output.lines().count(), 2);
    assert!(changes(&output).is_empty());
    assert!(output.contains(r#""before":"9007199254740993","after":"18446744073709551615""#));
    assert!(output.contains(r#""columns":["name\n\"é"]"#));
    assert!(output.contains(r#""changed_rows":"0","inserted":"0","retracted":"0""#));
    for field in ["diff_columns", "change", "changed_rows", "inserted", "retracted", "after_minus_before_bag"] {
        assert!(crate::ROBOT_SCHEMA.contains(field));
    }
}

#[test]
fn revision_query_and_shared_budget_refusals_emit_no_diff_output_or_writes() {
    let ((), report) = run_async_under_lab(0xd1ff_0102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1)); add(&mut seed, 1, 7);
        let at = db.write(&commit, seed).await.unwrap();
        let query = "MATCH (n:L) RETURN n.score AS score";
        for opts in [options(query, 0, at.0 + 1), options(query, at.0 + 1, 0),
            options("MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ 0 RETURN n", 0, at.0),
            options("CREATE (n:L)", 0, at.0),
            options("MATCH (n:L) WHERE n.score > $missing RETURN n", at.0, at.0)] {
            let mut bytes = Vec::new();
            assert_eq!(run(&db, &cx, &opts, true, &mut bytes).err().unwrap().code, 3);
            assert!(bytes.is_empty());
        }
        let opts = options(query, 0, at.0);
        let baseline = db.query_diff(&cx, query, &opts.params, &opts, CommitSeq(0), at, policy()).unwrap();
        let rows = baseline.row_stats(); let eval = baseline.evaluator_stats();
        for (flag, limit) in [("--max-snapshot-records", rows.snapshot_records - 1),
            ("--max-result-rows", 0), ("--max-work-units", eval.work_units - 1),
            ("--max-scratch-entries", eval.scratch_entries - 1)] {
            let mut opts = options(query, 0, at.0);
            okay(opts.diff.set(flag, &limit.to_string()));
            let mut bytes = Vec::new();
            assert_eq!(run(&db, &cx, &opts, true, &mut bytes).err().unwrap().code, 3);
            assert!(bytes.is_empty());
        }
        let mut exact = options(query, 0, at.0);
        for (flag, value) in [("--max-snapshot-records", rows.snapshot_records),
            ("--max-result-rows", rows.result_rows), ("--max-work-units", eval.work_units),
            ("--max-scratch-entries", eval.scratch_entries)] {
            okay(exact.diff.set(flag, &value.to_string()));
        }
        okay(run(&db, &cx, &exact, true, &mut Vec::new()));
        assert_eq!(db.frontier().unwrap(), at);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
