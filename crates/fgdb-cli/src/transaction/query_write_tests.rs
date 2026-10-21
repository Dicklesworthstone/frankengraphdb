//! Real transaction driver: native reads/writes, savepoints and sole completion.

use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb::{DatabaseKeys, MemVfs};
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_protocol::json::{Json, parse_json};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId};

const MERGE: &str = "MERGE (c:City {p:$key}) ON CREATE SET c.q=1 ON MATCH SET c.q=c.q+1";
const SOURCE: &str = "MATCH (p:Person) RETURN p.p AS key";
const SEED: &str = "CREATE (:Person {p:1}),(:Person {p:2}),(:Person {p:1})";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xd1; 32],
        DatabaseSecurityNamespaceId([0xd2; 32]),
        [0xd3; 32],
    )
}

fn parse(parts: &[&str]) -> Result<Options, Failure> {
    let mut args = vec![
        "--db",
        "unused",
        "--key-file",
        "unused",
        "--label",
        "Person=1",
        "--label",
        "City=2",
        "--label",
        "Copy=3",
        "--property",
        "p=1",
        "--property",
        "q=2",
    ];
    args.extend_from_slice(parts);
    crate::parse(
        &args.into_iter().map(str::to_owned).collect::<Vec<_>>(),
        "transaction",
    )
}

fn options(parts: &[&str]) -> Options {
    parse(parts).unwrap()
}

fn frames(bytes: &[u8]) -> Vec<Json> {
    std::str::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(|line| parse_json(line, 10_000, 65_536).unwrap())
        .collect()
}

fn field<'a>(value: &'a Json, key: &str) -> &'a Json {
    match value {
        Json::Object(fields) => fields.get(key),
        _ => None,
    }
    .expect("required frame field")
}

fn text(value: &Json) -> &str {
    match value {
        Json::String(value) => Some(value.as_str()),
        _ => None,
    }
    .expect("string field required")
}

fn number(value: &Json) -> u64 {
    match value {
        Json::Number(value) => value.parse().ok(),
        _ => None,
    }
    .expect("unsigned numeric field required")
}

fn terminal(bytes: &[u8], kind: &str, statements: u64, count: u64) {
    let frames = frames(bytes);
    assert_eq!(
        frames
            .iter()
            .filter(|f| text(field(f, "event")) == "result")
            .count(),
        1
    );
    let last = frames.last().unwrap();
    assert_eq!(text(field(last, "kind")), kind);
    assert_eq!(number(field(last, "statements")), statements);
    assert_eq!(number(field(last, "count")), count);
}

#[test]
fn dependent_query_fed_steps_read_staged_graph_and_commit_once_with_reopen() {
    let ((), report) = run_async_under_lab(0x7478_6401, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let input = options(&[
            "--write",
            SEED,
            "--write",
            MERGE,
            "--from-query",
            SOURCE,
            "--write",
            "CREATE (:Copy {p:$key})",
            "--from-query",
            "MATCH (c:City) RETURN c.q AS key",
            "--query",
            "MATCH (c:City) RETURN c.p AS p,c.q AS q ORDER BY p",
        ]);
        let mut out = Vec::new();
        run(&mut db, &contexts, &input, None, true, &mut out)
            .await
            .unwrap();
        // Seed + two source reads + three and two expanded writes + final read.
        terminal(&out, "committed", 9, 2);
        let output = std::str::from_utf8(&out).unwrap();
        assert!(output.contains(
            r#""statement":4,"cells":[{"type":"int","value":"1"},{"type":"int","value":"2"}]"#
        ));
        assert!(output.contains(
            r#""statement":4,"cells":[{"type":"int","value":"2"},{"type":"int","value":"1"}]"#
        ));
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        assert_eq!(db.delta_since(CommitSeq(0)).unwrap().count(), 1);
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        let vertices = db.vertices_at(CommitSeq(1)).unwrap();
        assert_eq!(vertices.len(), 7);
        let copies: Vec<_> = vertices
            .iter()
            .filter(|v| v.labels == vec![LabelId(3)])
            .map(|v| v.props.clone())
            .collect();
        assert_eq!(copies.len(), 2);
        for value in [1, 2] {
            assert!(copies.contains(&vec![(
                PropertyKeyId(1),
                CanonicalScalar::Int(value).into()
            )]));
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn savepoint_and_explicit_rollback_discard_feed_effects_and_buffered_previews() {
    let ((), report) = run_async_under_lab(0x7478_6402, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for rollback in [false, true] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let mut input = options(&[
                "--write",
                SEED,
                "--savepoint",
                "before",
                "--write",
                MERGE,
                "--from-query",
                SOURCE,
                "--query",
                "MATCH (c:City) RETURN c.q AS q",
                "--rollback-to",
                "before",
                "--write",
                "MERGE (c:City {p:$key}) ON CREATE SET c.q=9",
                "--from-query",
                SOURCE,
                "--release",
                "before",
                "--query",
                "MATCH (c:City) RETURN c.q AS q",
            ]);
            input.rollback = rollback;
            let mut out = Vec::new();
            run(&mut db, &contexts, &input, None, true, &mut out)
                .await
                .unwrap();
            if rollback {
                terminal(&out, "rolled_back", 11, 0);
                assert_eq!(frames(&out).len(), 1);
                assert_eq!(db.frontier().unwrap(), CommitSeq(0));
                assert!(db.vertices_at(CommitSeq(0)).unwrap().is_empty());
            } else {
                terminal(&out, "committed", 11, 2);
                let output = std::str::from_utf8(&out).unwrap();
                assert!(!output.contains(r#""index":3,"#));
                assert!(!output.contains(r#""statement":4,"#));
                assert!(output.contains(r#""index":5,"kind":"rollback_to"#));
                let cities: Vec<_> = db
                    .vertices_at(CommitSeq(1))
                    .unwrap()
                    .into_iter()
                    .filter(|v| v.labels == vec![LabelId(2)])
                    .collect();
                assert_eq!(cities.len(), 2);
                assert!(cities.iter().all(|v| {
                    v.props
                        .contains(&(PropertyKeyId(2), CanonicalScalar::Int(9).into()))
                }));
            }
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn expanded_statement_cap_reserves_future_sources_and_is_not_refunded_by_rollback() {
    let ((), report) = run_async_under_lab(0x7478_6403, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for last in [31, 32] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let argument = format!("last=int:{last}");
            let input = options(&[
                "--savepoint",
                "before",
                "--write",
                "CREATE (:Copy {p:$key})",
                "--from-query",
                "UNWIND range(1,31) AS key RETURN key",
                "--max-batch-statements",
                "1000",
                "--rollback-to",
                "before",
                "--write",
                "CREATE (:Copy {p:$key})",
                "--from-query",
                "UNWIND range(1,$last) AS key RETURN key",
                "--param",
                &argument,
                "--max-batch-statements",
                "1000",
            ]);
            let mut out = Vec::new();
            let result = run(&mut db, &contexts, &input, None, true, &mut out).await;
            if last == 31 {
                result.unwrap();
                terminal(&out, "committed", 64, 0);
                assert_eq!(db.vertices_at(CommitSeq(1)).unwrap().len(), 31);
            } else {
                let error = result.expect_err("source two exceeds remaining expansion capacity");
                assert_eq!(error.code, 3, "{}", error.message);
                assert!(
                    error.message.contains("transaction step 4"),
                    "{}",
                    error.message
                );
                assert!(error.message.contains("ResultRows"), "{}", error.message);
                assert!(out.is_empty());
                assert_eq!(db.frontier().unwrap(), CommitSeq(0));
                assert!(db.vertices_at(CommitSeq(0)).unwrap().is_empty());
            }
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
        // Fixed later reads are reserved too: a first dynamic batch may not
        // use all 63 available slots and then discover it cannot run a read.
        for last in [62, 63] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let argument = format!("last=int:{last}");
            let input = options(&[
                "--write",
                "CREATE (:Copy {p:$key})",
                "--from-query",
                "UNWIND range(1,$last) AS key RETURN key",
                "--param",
                &argument,
                "--query",
                "RETURN 1 AS ok",
            ]);
            let mut out = Vec::new();
            let result = run(&mut db, &contexts, &input, None, true, &mut out).await;
            if last == 62 {
                result.unwrap();
                terminal(&out, "committed", 64, 1);
                assert_eq!(db.vertices_at(CommitSeq(1)).unwrap().len(), 62);
            } else {
                let error = result.expect_err("the future read retains its reserved slot");
                assert!(
                    error.message.contains("transaction step 1"),
                    "{}",
                    error.message
                );
                assert!(error.message.contains("ResultRows"), "{}", error.message);
                assert!(out.is_empty());
                assert_eq!(db.frontier().unwrap(), CommitSeq(0));
            }
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
        // Every source is a native statement even when it selects no rows.
        let mut parts = Vec::new();
        for _ in 0..MAX_STATEMENTS {
            parts.extend([
                "--write",
                "CREATE (:Copy {p:$key})",
                "--from-query",
                "UNWIND [] AS key RETURN key",
                "--max-batch-statements",
                "0",
            ]);
        }
        let input = options(&parts);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut out = Vec::new();
        run(&mut db, &contexts, &input, None, true, &mut out)
            .await
            .unwrap();
        terminal(&out, "read_closed", 64, 0);
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_binding_and_action_failures_abort_earlier_writes_and_returning_rows() {
    let ((), report) = run_async_under_lab(0x7478_6404, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for (source, script) in [
            (
                "UNWIND [1,'bad'] AS key RETURN key",
                "CREATE (:City {p:$key})",
            ),
            (
                "UNWIND [2,0] AS divisor RETURN 1 AS key,divisor ORDER BY divisor DESC",
                "MERGE (c:City {p:$key}) ON CREATE SET c.q=10 ON MATCH SET c.q=c.q/$divisor",
            ),
        ] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let input = options(&[
                "--write",
                "CREATE (n:Person {p:5}) RETURN n.p AS p",
                "--write",
                script,
                "--from-query",
                source,
            ]);
            let mut out = Vec::new();
            let error = run(&mut db, &contexts, &input, None, true, &mut out)
                .await
                .expect_err("the second step must fail");
            assert_eq!(error.code, 3, "{}", error.message);
            assert!(
                error.message.contains("transaction step 2"),
                "{}",
                error.message
            );
            assert!(out.is_empty());
            assert_eq!(db.frontier().unwrap(), CommitSeq(0));
            assert!(db.vertices_at(CommitSeq(0)).unwrap().is_empty());
            // Positive control also checks that the failed invocation leaves
            // the engine allocator and transaction obligations usable.
            let good = options(&[
                "--write",
                "CREATE (n:Person {p:5}) RETURN n.p AS p",
                "--write",
                "CREATE (:City {p:$key})",
                "--from-query",
                "RETURN 7 AS key",
            ]);
            run(&mut db, &contexts, &good, None, true, &mut out)
                .await
                .unwrap();
            terminal(&out, "committed", 3, 1);
            assert_eq!(db.vertices_at(CommitSeq(1)).unwrap().len(), 2);
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn per_step_parameters_and_declarations_do_not_leak_into_the_next_feed() {
    let ((), report) = run_async_under_lab(0x7478_6405, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let input = options(&[
            "--write",
            "CREATE (:City {p:$key})",
            "--from-query",
            "RETURN $data AS key",
            "--param",
            "data=text:Ada",
            "--feed-type",
            "key=text",
            "--write",
            "CREATE (:Copy {p:$key})",
            "--from-query",
            "RETURN $data AS key",
            "--param",
            "data=int:7",
            "--query",
            "MATCH (c:City) RETURN c.p AS p",
            "--query",
            "MATCH (c:Copy) RETURN c.p AS p",
        ]);
        let mut out = Vec::new();
        run(&mut db, &contexts, &input, None, true, &mut out)
            .await
            .unwrap();
        terminal(&out, "committed", 6, 2);
        let output = std::str::from_utf8(&out).unwrap();
        assert!(output.contains(r#""statement":3,"cells":[{"type":"text","value":"Ada"}]"#));
        assert!(output.contains(r#""statement":4,"cells":[{"type":"int","value":"7"}]"#));
        assert_eq!(db.vertices_at(CommitSeq(1)).unwrap().len(), 2);
        let missing = options(&[
            "--write",
            "CREATE (:Person {p:1})",
            "--write",
            "CREATE (:City {p:$key})",
            "--from-query",
            "RETURN 1 AS other",
        ]);
        out.clear();
        assert!(
            run(&mut db, &contexts, &missing, None, true, &mut out)
                .await
                .is_err()
        );
        assert!(out.is_empty());
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        assert_eq!(db.vertices_at(CommitSeq(1)).unwrap().len(), 2);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn feed_output_is_admitted_before_completion_but_source_rows_are_not_delivery_rows() {
    let ((), report) = run_async_under_lab(0x7478_6406, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let input = options(&[
            "--write",
            "CREATE (:City {p:$key})",
            "--from-query",
            "UNWIND [1,2] AS key RETURN key",
        ]);
        for output_bytes in [16_384, 0] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let mut out = Vec::new();
            let result = run_with_limits(
                &mut db,
                &contexts,
                &input,
                None,
                true,
                &mut out,
                Limits {
                    rows: 0,
                    output_bytes,
                },
                None,
            )
            .await;
            if output_bytes == 0 {
                let error = result.expect_err("record admission must precede commit");
                assert!(error.message.contains("output limit"), "{}", error.message);
                assert!(out.is_empty());
                assert_eq!(db.frontier().unwrap(), CommitSeq(0));
                assert!(db.vertices_at(CommitSeq(0)).unwrap().is_empty());
            } else {
                result.unwrap();
                terminal(&out, "committed", 3, 0);
                assert_eq!(db.vertices_at(CommitSeq(1)).unwrap().len(), 2);
            }
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
        for flag in [
            "--max-result-rows",
            "--max-work-units",
            "--max-scratch-entries",
        ] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let input = options(&[
                flag,
                "0",
                "--write",
                "CREATE (:City {p:$key})",
                "--from-query",
                "UNWIND [1,2] AS key RETURN key",
            ]);
            let mut out = Vec::new();
            assert!(
                run(&mut db, &contexts, &input, None, true, &mut out)
                    .await
                    .is_err()
            );
            assert!(out.is_empty());
            assert_eq!(db.frontier().unwrap(), CommitSeq(0));
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn feed_flags_bind_only_to_the_preceding_write_and_share_whole_input_admission() {
    for parts in [
        vec![
            "--from-query",
            "RETURN 1 AS key",
            "--write",
            "CREATE (:City {p:$key})",
        ],
        vec!["--query", "RETURN 1", "--from-query", "RETURN 1 AS key"],
        vec!["--savepoint", "x", "--from-query", "RETURN 1 AS key"],
        vec![
            "--write",
            "CREATE (:City {p:$key})",
            "--feed-type",
            "key=text",
        ],
        vec![
            "--write",
            "CREATE (:City {p:$key})",
            "--max-batch-statements",
            "1",
        ],
        vec![
            "--write",
            "CREATE (:City {p:$key})",
            "--from-query",
            "RETURN 1 AS key",
            "--from-query",
            "RETURN 2 AS key",
        ],
    ] {
        let error = parse(&parts)
            .err()
            .expect("invalid placement must be refused");
        assert_eq!(error.code, 2, "{}", error.message);
    }
    let mut config = QueryWriteOptions::default();
    config.set("--from-query", "RETURN $x AS key").unwrap();
    config.set("--feed-type", "key=int64").unwrap();
    config.set("--max-batch-statements", "0001").unwrap();
    assert_eq!(
        config.input_bytes(),
        "RETURN $x AS key".len() + "key=int64".len() + 4
    );
    let before = config.input_bytes();
    assert!(config.set("--max-batch-statements", "2").is_err());
    assert_eq!(config.input_bytes(), before);

    // Isolate the whole-input guard: each source is natively bounded, but the
    // aggregate must not omit sources merely because they live in step flags.
    let source = " ".repeat(fgdb_gql::MAX_GRAPH_TEXT_BYTES);
    let mut steps = Vec::new();
    for _ in 0..MAX_INPUT_BYTES / source.len() {
        let mut step = Step::new(StepKind::Write, "CREATE (n)".to_owned());
        step.set_query_write_flag("--from-query", &source).unwrap();
        steps.push(step);
    }
    assert!(steps.len() <= MAX_STATEMENTS);
    let error = validate_input(&steps).expect_err("source bytes must count toward 1 MiB");
    assert!(error.message.contains("1 MiB"), "{}", error.message);
}

#[test]
fn graph_fed_steps_keep_every_execution_budget_and_the_explicit_batch_limit() {
    let ((), report) = run_async_under_lab(0x7478_6407, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let seed = options(&["--write", SEED]);
        run(&mut db, &contexts, &seed, None, true, &mut Vec::new())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        for (flag, value, dimension) in [
            ("--max-snapshot-records", "0", "SnapshotRecords"),
            ("--max-result-rows", "0", "ResultRows"),
            ("--max-work-units", "0", "WorkUnits"),
            ("--max-scratch-entries", "0", "ScratchEntries"),
            ("--max-batch-statements", "2", "ResultRows"),
        ] {
            // These flags remain global for execution budgets and step-local
            // for batch admission, regardless of their position after --write.
            let input = options(&["--write", MERGE, "--from-query", SOURCE, flag, value]);
            let mut out = Vec::new();
            let error = run(&mut db, &contexts, &input, None, true, &mut out)
                .await
                .expect_err("the explicit allowance must reach the native executor");
            assert_eq!(error.code, 3, "{}", error.message);
            assert!(error.message.contains(dimension), "{}", error.message);
            assert!(out.is_empty());
            assert_eq!(db.frontier().unwrap(), basis);
            assert_eq!(db.vertices_at(basis).unwrap().len(), 3);
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
        let good = options(&["--write", MERGE, "--from-query", SOURCE]);
        let mut out = Vec::new();
        run(&mut db, &contexts, &good, None, true, &mut out)
            .await
            .unwrap();
        terminal(&out, "committed", 4, 0);
        assert_eq!(db.frontier().unwrap(), CommitSeq(basis.0 + 1));
        assert_eq!(db.vertices_at(db.frontier().unwrap()).unwrap().len(), 5);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn collection_feeds_read_staged_data_and_remain_step_local_across_savepoints() {
    let ((), report) = run_async_under_lab(0x7478_6408, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let input = options(&[
            "--write",
            "CREATE (:Person {p:2}),(:Person {p:3})",
            "--write",
            "CREATE (:City {p:$doc.key,q:size($items)})",
            "--from-query",
            "MATCH (p:Person) \
                RETURN {key:p.p,nested:[p.p]} AS doc,[p.p,p.p+1] AS items",
            "--feed-type",
            "doc=map",
            "--feed-type",
            "items=list",
            "--savepoint",
            "cities",
            "--write",
            "CREATE (:Copy {p:size($items)})",
            "--from-query",
            "MATCH (c:City) RETURN [c.p] AS items",
            "--feed-type",
            "items=list",
            "--rollback-to",
            "cities",
            // The earlier map declaration must not turn this scalar into a map.
            "--write",
            "CREATE (:Copy {p:$doc})",
            "--from-query",
            "RETURN 7 AS doc",
            "--query",
            "MATCH (c:City) RETURN c.p AS p,c.q AS q ORDER BY p",
            "--query",
            "MATCH (c:Copy) RETURN c.p AS p",
        ]);
        let mut out = Vec::new();
        run(&mut db, &contexts, &input, None, true, &mut out)
            .await
            .unwrap();
        terminal(&out, "committed", 11, 3);
        let output = std::str::from_utf8(&out).unwrap();
        assert!(!output.contains(r#""index":4,"#), "{output}");
        assert!(output.contains(
            r#""statement":7,"cells":[{"type":"int","value":"2"},{"type":"int","value":"2"}]"#
        ));
        assert!(output.contains(
            r#""statement":7,"cells":[{"type":"int","value":"3"},{"type":"int","value":"2"}]"#
        ));
        assert!(output.contains(r#""statement":8,"cells":[{"type":"int","value":"7"}]"#));
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        assert_eq!(db.delta_since(CommitSeq(0)).unwrap().count(), 1);
        let vertices = db.vertices_at(CommitSeq(1)).unwrap();
        assert_eq!(vertices.len(), 5);
        let copies: Vec<_> = vertices
            .iter()
            .filter(|v| v.labels == vec![LabelId(3)])
            .collect();
        assert_eq!(copies.len(), 1);
        assert_eq!(
            copies[0].props,
            vec![(PropertyKeyId(1), CanonicalScalar::Int(7).into())]
        );
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_collection_refusal_aborts_prior_transaction_writes_and_returning_rows() {
    let ((), report) = run_async_under_lab(0x7478_6409, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for (source, declaration, script, good_source) in [
            (
                "UNWIND [{v:[1],i:0},{v:7,i:1}] AS r RETURN r.v AS items,r.i AS idx ORDER BY idx",
                "items=list",
                "CREATE (:City {p:size($items)})",
                "RETURN [9] AS items",
            ),
            (
                "UNWIND [{v:{key:1},i:0},{v:[2],i:1}] AS r RETURN r.v AS doc,r.i AS idx ORDER BY idx",
                "doc=map",
                "CREATE (:City {p:$doc.key})",
                "RETURN {key:9} AS doc",
            ),
        ] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let input = options(&[
                "--write",
                "CREATE (n:Person {p:5}) RETURN n.p AS p",
                "--write",
                script,
                "--from-query",
                source,
                "--feed-type",
                declaration,
            ]);
            let mut out = Vec::new();
            let error = run(&mut db, &contexts, &input, None, true, &mut out)
                .await
                .expect_err("a late collection mismatch cannot publish step one");
            assert_eq!(error.code, 3, "{}", error.message);
            assert!(
                error.message.contains("transaction step 2"),
                "{}",
                error.message
            );
            // The refusal names the declared collection kind the late row lacks.
            let expected = if declaration.starts_with("items") {
                "LIST"
            } else {
                "MAP"
            };
            assert!(
                error
                    .message
                    .contains(&format!("is not {expected} or null")),
                "{}",
                error.message
            );
            assert!(out.is_empty());
            assert_eq!(db.frontier().unwrap(), CommitSeq(0));
            assert!(db.vertices_at(CommitSeq(0)).unwrap().is_empty());
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
            let good = options(&[
                "--write",
                "CREATE (n:Person {p:5}) RETURN n.p AS p",
                "--write",
                script,
                "--from-query",
                good_source,
                "--feed-type",
                declaration,
            ]);
            run(&mut db, &contexts, &good, None, true, &mut out)
                .await
                .unwrap();
            terminal(&out, "committed", 3, 1);
            assert_eq!(db.vertices_at(CommitSeq(1)).unwrap().len(), 2);
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
